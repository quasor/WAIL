//go:build !linkstub

package main

import (
	"context"
	"log"
	"sync"
	"time"

	abletonlink "github.com/DatanoiseTV/abletonlink-go"
)

// LinkBridge wraps the Ableton Link session via abletonlink-go CGo binding.
type LinkBridge struct {
	mu           sync.Mutex
	link         *abletonlink.Link
	sessionState *abletonlink.SessionState
	quantum      float64
	detector     *TempoChangeDetector
}

// NewLinkBridge creates a new Link bridge with the given initial BPM and quantum.
func NewLinkBridge(initialBPM, quantum float64) *LinkBridge {
	link := abletonlink.NewLink(initialBPM)
	ss := abletonlink.NewSessionState()
	return &LinkBridge{
		link:         link,
		sessionState: ss,
		quantum:      quantum,
		detector:     NewTempoChangeDetector(initialBPM),
	}
}

// Enable activates the Link session.
func (lb *LinkBridge) Enable() {
	lb.link.Enable(true)
	log.Printf("[link] Ableton Link enabled at %.1f BPM", lb.detector.LastTempo())
}

// Disable deactivates the Link session.
func (lb *LinkBridge) Disable() {
	lb.link.Enable(false)
	log.Printf("[link] Ableton Link disabled")
}

// SetTempo applies a remote tempo change to the local Link session.
func (lb *LinkBridge) SetTempo(bpm float64) {
	lb.mu.Lock()
	t := lb.link.ClockMicros()
	lb.link.CaptureAppSessionState(lb.sessionState)
	lb.sessionState.SetTempo(bpm, t)
	lb.link.CommitAppSessionState(lb.sessionState)
	lb.mu.Unlock()
	lb.detector.SetLastTempo(bpm)
	lb.detector.ArmEchoGuard(time.Now().Add(echoGuardDuration))
	log.Printf("[link] Applied remote tempo %.1f BPM", bpm)
}

// ForceBeat snaps the local beat clock to the given position.
// rttUs compensates for one-way network transit time.
func (lb *LinkBridge) ForceBeat(beat float64, rttUs *int64) {
	lb.mu.Lock()
	t := lb.link.ClockMicros()
	lb.link.CaptureAppSessionState(lb.sessionState)
	bpm := lb.sessionState.Tempo()
	var compensation float64
	if rttUs != nil {
		compensation = float64(*rttUs) / 2_000_000.0 * bpm / 60.0
	}
	compensated := beat + compensation
	lb.sessionState.ForceBeatAtTime(compensated, t, lb.quantum)
	lb.link.CommitAppSessionState(lb.sessionState)
	lb.mu.Unlock()
	lb.detector.ArmEchoGuard(time.Now().Add(echoGuardDuration))
	log.Printf("[link] Forced beat to %.2f (compensated=%.2f, rtt=%v)", beat, compensated, rttUs)
}

// State returns the current Link state.
func (lb *LinkBridge) State() LinkState {
	lb.mu.Lock()
	defer lb.mu.Unlock()
	t := lb.link.ClockMicros()
	lb.link.CaptureAppSessionState(lb.sessionState)
	return LinkState{
		BPM:         lb.sessionState.Tempo(),
		Beat:        lb.sessionState.BeatAtTime(t, lb.quantum),
		Phase:       lb.sessionState.PhaseAtTime(t, lb.quantum),
		Quantum:     lb.quantum,
		TimestampUs: t,
		NumPeers:    lb.link.NumPeers(),
	}
}

// Detector returns the tempo change detector.
func (lb *LinkBridge) Detector() *TempoChangeDetector {
	return lb.detector
}

// SpawnPoller starts a polling goroutine that monitors the Link session.
func (lb *LinkBridge) SpawnPoller(ctx context.Context) (chan<- LinkCommand, <-chan LinkEvent) {
	return SpawnLinkPoller(ctx, lb)
}
