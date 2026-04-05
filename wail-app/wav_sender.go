package main

import (
	"context"
	"fmt"
	"log"
	"math"
	"os"
	"time"

	"github.com/go-audio/wav"
	"gopkg.in/hraban/opus.v2"
)

const (
	wavTargetSampleRate = 48000
	wavTargetChannels   = 2
	wavBitrateKbps      = 128
)

// loadWAV reads a WAV file and returns interleaved int16 samples
// normalized to 48kHz stereo. The entire file is loaded into memory.
func loadWAV(path string) ([]int16, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("open WAV: %w", err)
	}
	defer f.Close()

	dec := wav.NewDecoder(f)
	if !dec.IsValidFile() {
		return nil, fmt.Errorf("invalid WAV file: %s", path)
	}

	format := dec.Format()
	srcRate := int(format.SampleRate)
	srcCh := int(format.NumChannels)
	bitDepth := int(dec.SampleBitDepth())

	log.Printf("[wav] Loading %s: %dHz, %dch, %d-bit", path, srcRate, srcCh, bitDepth)

	buf, err := dec.FullPCMBuffer()
	if err != nil {
		return nil, fmt.Errorf("read WAV data: %w", err)
	}

	// Convert to int16
	samples16 := toInt16(buf.Data, bitDepth)

	// Convert channels to stereo
	samples16 = toStereo(samples16, srcCh)

	// Resample to 48kHz if needed
	if srcRate != wavTargetSampleRate {
		samples16 = resample(samples16, srcRate, wavTargetSampleRate)
	}

	durationSec := float64(len(samples16)/wavTargetChannels) / float64(wavTargetSampleRate)
	log.Printf("[wav] Loaded %.1fs of audio (%d samples, stereo 48kHz)", durationSec, len(samples16)/wavTargetChannels)

	return samples16, nil
}

// toInt16 converts samples from arbitrary bit depth to int16.
func toInt16(data []int, bitDepth int) []int16 {
	out := make([]int16, len(data))
	switch bitDepth {
	case 8:
		for i, v := range data {
			out[i] = int16((v - 128) << 8)
		}
	case 16:
		for i, v := range data {
			out[i] = int16(v)
		}
	case 24:
		for i, v := range data {
			out[i] = int16(v >> 8)
		}
	case 32:
		for i, v := range data {
			out[i] = int16(v >> 16)
		}
	default:
		// Best-effort: assume 16-bit range
		for i, v := range data {
			out[i] = int16(v)
		}
	}
	return out
}

// toStereo converts samples to stereo interleaved format.
func toStereo(samples []int16, srcChannels int) []int16 {
	if srcChannels == 2 {
		return samples
	}
	srcFrames := len(samples) / srcChannels
	out := make([]int16, srcFrames*2)
	if srcChannels == 1 {
		for i := 0; i < srcFrames; i++ {
			out[i*2] = samples[i]
			out[i*2+1] = samples[i]
		}
	} else {
		// >2 channels: take first two
		for i := 0; i < srcFrames; i++ {
			out[i*2] = samples[i*srcChannels]
			out[i*2+1] = samples[i*srcChannels+1]
		}
	}
	return out
}

// resample performs linear interpolation resampling of interleaved stereo samples.
func resample(samples []int16, srcRate, dstRate int) []int16 {
	srcFrames := len(samples) / 2
	if srcFrames < 2 {
		return samples
	}
	dstFrames := int(math.Round(float64(srcFrames) * float64(dstRate) / float64(srcRate)))
	out := make([]int16, dstFrames*2)
	ratio := float64(srcRate) / float64(dstRate)

	for i := 0; i < dstFrames; i++ {
		srcPos := float64(i) * ratio
		srcIdx := int(srcPos)
		frac := srcPos - float64(srcIdx)

		if srcIdx+1 >= srcFrames {
			srcIdx = srcFrames - 2
			frac = 1.0
		}

		for ch := 0; ch < 2; ch++ {
			a := float64(samples[srcIdx*2+ch])
			b := float64(samples[(srcIdx+1)*2+ch])
			out[i*2+ch] = int16(a + frac*(b-a))
		}
	}
	return out
}

// WavSenderTask reads a WAV file and sends Opus-encoded WAIF frames,
// looping continuously. Structurally parallel to TestToneTask.
func WavSenderTask(
	ctx context.Context,
	streamIndex uint16,
	connID int,
	fromPluginCh chan<- ipcFrame,
	boundaryCh <-chan IntervalBoundaryInfo,
	wavPath string,
) {
	samples, err := loadWAV(wavPath)
	if err != nil {
		log.Printf("[wav-sender] Failed to load WAV: %v", err)
		return
	}
	if len(samples) < wavTargetChannels*toneSamplesPerFrame {
		log.Printf("[wav-sender] WAV file too short (need at least one 20ms frame)")
		return
	}

	enc, err := opus.NewEncoder(wavTargetSampleRate, wavTargetChannels, opus.AppAudio)
	if err != nil {
		log.Printf("[wav-sender] Failed to create encoder: %v", err)
		return
	}
	if err := enc.SetBitrate(wavBitrateKbps * 1000); err != nil {
		log.Printf("[wav-sender] Failed to set bitrate: %v", err)
	}

	readPos := 0 // position in interleaved samples
	var currentIdx int64 = -1
	var currentBPM float64 = 120.0
	var currentBars uint32 = 4
	var currentQuantum float64 = 4.0
	var frameNumber uint32
	var totalFrames uint32
	var intervalStart *time.Time

	opusBuf := make([]byte, 4096)
	samplesPerFrame := toneSamplesPerFrame * wavTargetChannels // 960 * 2 = 1920 interleaved

	for {
		select {
		case <-ctx.Done():
			return
		case boundary := <-boundaryCh:
			// Force-send final frame of previous interval if incomplete
			if currentIdx >= 0 && frameNumber > 0 && frameNumber < totalFrames {
				frame := extractFrame(samples, &readPos, samplesPerFrame)
				if opusData, n, err := encodeFrame(enc, frame, opusBuf); err == nil {
					sendWAIFFrame(fromPluginCh, connID, streamIndex, currentIdx,
						totalFrames-1, opusData[:n], true, currentBPM, currentQuantum, currentBars, totalFrames)
				} else {
					log.Printf("[wav-sender] Encode failed on boundary flush: %v", err)
				}
			}
			currentIdx = boundary.Index
			currentBPM = boundary.BPM
			currentBars = boundary.Bars
			currentQuantum = boundary.Quantum
			frameNumber = 0
			totalFrames = FramesPerInterval(currentBPM, currentBars, currentQuantum)
			now := time.Now()
			intervalStart = &now
		default:
		}

		if currentIdx < 0 || frameNumber >= totalFrames {
			time.Sleep(5 * time.Millisecond)
			continue
		}

		// Wall-clock pacing
		if intervalStart != nil {
			elapsedMs := time.Since(*intervalStart).Milliseconds()
			dueFrame := uint32(elapsedMs / toneFrameMs)
			if dueFrame > totalFrames {
				dueFrame = totalFrames
			}
			if frameNumber >= dueFrame {
				time.Sleep(1 * time.Millisecond)
				continue
			}
		}

		frame := extractFrame(samples, &readPos, samplesPerFrame)

		opusData, n, err := encodeFrame(enc, frame, opusBuf)
		if err != nil {
			log.Printf("[wav-sender] Encode failed: %v", err)
			time.Sleep(20 * time.Millisecond)
			continue
		}

		isFinal := frameNumber == totalFrames-1
		sendWAIFFrame(fromPluginCh, connID, streamIndex, currentIdx,
			frameNumber, opusData[:n], isFinal, currentBPM, currentQuantum, currentBars, totalFrames)
		frameNumber++
	}
}

// extractFrame extracts one frame of interleaved samples from the buffer,
// wrapping around to the beginning for continuous looping.
func extractFrame(samples []int16, readPos *int, samplesPerFrame int) []int16 {
	frame := make([]int16, samplesPerFrame)
	remaining := samplesPerFrame
	dst := 0

	for remaining > 0 {
		available := len(samples) - *readPos
		if available <= 0 {
			*readPos = 0
			available = len(samples)
		}
		n := remaining
		if n > available {
			n = available
		}
		copy(frame[dst:dst+n], samples[*readPos:*readPos+n])
		*readPos += n
		dst += n
		remaining -= n
	}
	return frame
}
