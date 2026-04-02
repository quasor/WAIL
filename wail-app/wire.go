package main

import (
	"encoding/binary"
	"fmt"
	"math"
)

// WAIF wire format constants.
var frameMagic = [4]byte{'W', 'A', 'I', 'F'}

const (
	frameHeaderSize = 21 // 4 magic + 1 flags + 2 stream_id + 8 interval_index + 4 frame_number + 2 opus_len
	frameFinalExtra = 28 // 4 sample_rate + 4 total_frames + 8 bpm + 8 quantum + 4 bars

	frameFlagStereo byte = 0x01
	frameFlagFinal  byte = 0x02
)

// WaifHeaderPeek contains minimal WAIF frame header fields extracted without allocation.
type WaifHeaderPeek struct {
	IntervalIndex int64
	StreamID      uint16
	FrameNumber   uint32
	IsFinal       bool
	TotalFrames   uint32 // only valid when IsFinal is true
}

// PeekWaifHeader extracts header fields from a WAIF frame without full decode.
// Returns nil if the data is too short or doesn't have the WAIF magic.
func PeekWaifHeader(data []byte) *WaifHeaderPeek {
	if len(data) < frameHeaderSize {
		return nil
	}
	if data[0] != frameMagic[0] || data[1] != frameMagic[1] || data[2] != frameMagic[2] || data[3] != frameMagic[3] {
		return nil
	}
	flags := data[4]
	isFinal := flags&frameFlagFinal != 0
	streamID := binary.LittleEndian.Uint16(data[5:7])
	intervalIndex := int64(binary.LittleEndian.Uint64(data[7:15]))
	frameNumber := binary.LittleEndian.Uint32(data[15:19])

	var totalFrames uint32
	if isFinal {
		opusLen := int(binary.LittleEndian.Uint16(data[19:21]))
		metaStart := frameHeaderSize + opusLen
		if len(data) < metaStart+frameFinalExtra {
			return nil
		}
		totalFrames = binary.LittleEndian.Uint32(data[metaStart+4 : metaStart+8])
	}

	return &WaifHeaderPeek{
		IntervalIndex: intervalIndex,
		StreamID:      streamID,
		FrameNumber:   frameNumber,
		IsFinal:       isFinal,
		TotalFrames:   totalFrames,
	}
}

// RewriteWaifIntervalIndex rewrites the interval_index field in a WAIF frame in-place.
// Returns true if the rewrite was performed.
func RewriteWaifIntervalIndex(data []byte, newIndex int64) bool {
	if len(data) >= frameHeaderSize &&
		data[0] == frameMagic[0] && data[1] == frameMagic[1] &&
		data[2] == frameMagic[2] && data[3] == frameMagic[3] {
		binary.LittleEndian.PutUint64(data[7:15], uint64(newIndex))
		return true
	}
	return false
}

// AudioFrame represents a decoded WAIF streaming audio frame.
type AudioFrame struct {
	IntervalIndex int64
	StreamID      uint16
	FrameNumber   uint32
	Channels      uint16
	OpusData      []byte
	IsFinal       bool
	SampleRate    uint32
	TotalFrames   uint32
	BPM           float64
	Quantum       float64
	Bars          uint32
}

// EncodeAudioFrameWire encodes an AudioFrame into WAIF binary wire format.
func EncodeAudioFrameWire(f *AudioFrame) []byte {
	extra := 0
	if f.IsFinal {
		extra = frameFinalExtra
	}
	buf := make([]byte, frameHeaderSize+len(f.OpusData)+extra)

	copy(buf[0:4], frameMagic[:])

	var flags byte
	if f.Channels == 2 {
		flags |= frameFlagStereo
	}
	if f.IsFinal {
		flags |= frameFlagFinal
	}
	buf[4] = flags

	binary.LittleEndian.PutUint16(buf[5:7], f.StreamID)
	binary.LittleEndian.PutUint64(buf[7:15], uint64(f.IntervalIndex))
	binary.LittleEndian.PutUint32(buf[15:19], f.FrameNumber)
	binary.LittleEndian.PutUint16(buf[19:21], uint16(len(f.OpusData)))
	copy(buf[21:], f.OpusData)

	if f.IsFinal {
		off := frameHeaderSize + len(f.OpusData)
		binary.LittleEndian.PutUint32(buf[off:], f.SampleRate)
		binary.LittleEndian.PutUint32(buf[off+4:], f.TotalFrames)
		binary.LittleEndian.PutUint64(buf[off+8:], math.Float64bits(f.BPM))
		binary.LittleEndian.PutUint64(buf[off+16:], math.Float64bits(f.Quantum))
		binary.LittleEndian.PutUint32(buf[off+24:], f.Bars)
	}

	return buf
}

// DecodeAudioFrameWire decodes WAIF binary wire format into an AudioFrame.
func DecodeAudioFrameWire(data []byte) (*AudioFrame, error) {
	if len(data) < frameHeaderSize {
		return nil, fmt.Errorf("WAIF frame too short: %d bytes, need %d", len(data), frameHeaderSize)
	}
	if data[0] != frameMagic[0] || data[1] != frameMagic[1] || data[2] != frameMagic[2] || data[3] != frameMagic[3] {
		return nil, fmt.Errorf("invalid WAIF magic: %v", data[0:4])
	}

	flags := data[4]
	var channels uint16 = 1
	if flags&frameFlagStereo != 0 {
		channels = 2
	}
	isFinal := flags&frameFlagFinal != 0

	streamID := binary.LittleEndian.Uint16(data[5:7])
	intervalIndex := int64(binary.LittleEndian.Uint64(data[7:15]))
	frameNumber := binary.LittleEndian.Uint32(data[15:19])
	opusLen := int(binary.LittleEndian.Uint16(data[19:21]))

	if len(data) < frameHeaderSize+opusLen {
		return nil, fmt.Errorf("WAIF frame truncated: need %d opus bytes, got %d", opusLen, len(data)-frameHeaderSize)
	}

	opusData := make([]byte, opusLen)
	copy(opusData, data[frameHeaderSize:frameHeaderSize+opusLen])

	f := &AudioFrame{
		IntervalIndex: intervalIndex,
		StreamID:      streamID,
		FrameNumber:   frameNumber,
		Channels:      channels,
		OpusData:      opusData,
		IsFinal:       isFinal,
	}

	if isFinal {
		metaStart := frameHeaderSize + opusLen
		if len(data) < metaStart+frameFinalExtra {
			return nil, fmt.Errorf("WAIF final metadata truncated")
		}
		f.SampleRate = binary.LittleEndian.Uint32(data[metaStart:])
		f.TotalFrames = binary.LittleEndian.Uint32(data[metaStart+4:])
		f.BPM = math.Float64frombits(binary.LittleEndian.Uint64(data[metaStart+8:]))
		f.Quantum = math.Float64frombits(binary.LittleEndian.Uint64(data[metaStart+16:]))
		f.Bars = binary.LittleEndian.Uint32(data[metaStart+24:])
	}

	return f, nil
}
