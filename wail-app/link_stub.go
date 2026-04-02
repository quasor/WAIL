//go:build linkstub

package main

import (
	"context"
	"log"
	"math"
	"sync"
	"time"
)

// LinkBridge stub implementation that simulates Link behavior without the C++ SDK.
// Build with -tags=linkstub to use this instead of the real abletonlink-go bridge.
type LinkBridge struct {
	mu        sync.Mutex
	bpm       float64
	quantum   float64
	beat      float64
	enabled   bool
	startTime time.Time
	detector  *TempoChangeDetector
}

func NewLinkBridge(initialBPM, quantum float64) *LinkBridge {
	return &LinkBridge{
		bpm:       initialBPM,
		quantum:   quantum,
		startTime: time.Now(),
		detector:  NewTempoChangeDetector(initialBPM),
	}
}

func (lb *LinkBridge) Enable() {
	lb.mu.Lock()
	defer lb.mu.Unlock()
	lb.enabled = true
	log.Printf("[link-stub] Ableton Link enabled at %.1f BPM", lb.bpm)
}

func (lb *LinkBridge) Disable() {
	lb.mu.Lock()
	defer lb.mu.Unlock()
	lb.enabled = false
	log.Printf("[link-stub] Ableton Link disabled")
}

func (lb *LinkBridge) SetTempo(bpm float64) {
	lb.mu.Lock()
	lb.bpm = bpm
	lb.mu.Unlock()
	lb.detector.SetLastTempo(bpm)
	lb.detector.ArmEchoGuard(time.Now().Add(echoGuardDuration))
}

func (lb *LinkBridge) ForceBeat(beat float64, rttUs *int64) {
	lb.mu.Lock()
	defer lb.mu.Unlock()
	var compensation float64
	if rttUs != nil {
		compensation = float64(*rttUs) / 2_000_000.0 * lb.bpm / 60.0
	}
	lb.beat = beat + compensation
	lb.detector.ArmEchoGuard(time.Now().Add(echoGuardDuration))
	log.Printf("[link-stub] Forced beat to %.2f (compensated=%.2f)", beat, lb.beat)
}

func (lb *LinkBridge) State() LinkState {
	lb.mu.Lock()
	defer lb.mu.Unlock()
	elapsed := time.Since(lb.startTime).Seconds()
	beatsElapsed := elapsed * lb.bpm / 60.0
	beat := lb.beat + beatsElapsed
	phase := math.Mod(beat, lb.quantum)
	if phase < 0 {
		phase += lb.quantum
	}
	return LinkState{
		BPM:         lb.bpm,
		Beat:        beat,
		Phase:       phase,
		Quantum:     lb.quantum,
		TimestampUs: time.Since(lb.startTime).Microseconds(),
		NumPeers:    0,
	}
}

func (lb *LinkBridge) Detector() *TempoChangeDetector {
	return lb.detector
}

func (lb *LinkBridge) SpawnPoller(ctx context.Context) (chan<- LinkCommand, <-chan LinkEvent) {
	return SpawnLinkPoller(ctx, lb)
}
