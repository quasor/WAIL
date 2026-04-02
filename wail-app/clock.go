package main

import (
	"sort"
	"sync"
	"time"
)

const (
	clockWindowSize   = 8
	PingIntervalMs    = 2000
)

// PeerClock tracks RTT samples for one peer.
type PeerClock struct {
	samples []int64 // RTT samples in microseconds (sliding window)
	RTTUs   int64   // Median RTT in microseconds
}

// ClockSync tracks round-trip time to each remote peer using NTP-like Ping/Pong.
type ClockSync struct {
	mu         sync.Mutex
	epoch      time.Time
	perPeer    map[string]*PeerClock
	nextPingID uint64
}

// NewClockSync creates a new clock sync tracker.
func NewClockSync() *ClockSync {
	return &ClockSync{
		epoch:   time.Now(),
		perPeer: make(map[string]*PeerClock),
	}
}

// NowUs returns the current time in microseconds since epoch.
func (c *ClockSync) NowUs() int64 {
	return time.Since(c.epoch).Microseconds()
}

// MakePing generates a Ping message to send to peers.
func (c *ClockSync) MakePing() SyncMessage {
	c.mu.Lock()
	id := c.nextPingID
	c.nextPingID++
	c.mu.Unlock()
	return NewPing(id, c.NowUs())
}

// HandlePing handles an incoming Ping and returns a Pong to send back.
func (c *ClockSync) HandlePing(id uint64, sentAtUs int64) SyncMessage {
	return NewPong(id, sentAtUs, c.NowUs())
}

// HandlePong handles an incoming Pong and updates RTT estimate for the peer.
func (c *ClockSync) HandlePong(peerID string, pingSentAtUs, pongSentAtUs int64) {
	now := c.NowUs()
	rtt := now - pingSentAtUs
	if rtt < 0 {
		return // clock anomaly
	}

	c.mu.Lock()
	defer c.mu.Unlock()

	clock, ok := c.perPeer[peerID]
	if !ok {
		clock = &PeerClock{samples: make([]int64, 0, clockWindowSize)}
		c.perPeer[peerID] = clock
	}

	clock.samples = append(clock.samples, rtt)
	if len(clock.samples) > clockWindowSize {
		clock.samples = clock.samples[1:]
	}

	clock.RTTUs = medianOf(clock.samples)
}

// RTTUs returns the estimated RTT for a peer in microseconds, or nil if unknown.
func (c *ClockSync) RTTUs(peerID string) *int64 {
	c.mu.Lock()
	defer c.mu.Unlock()
	clock, ok := c.perPeer[peerID]
	if !ok {
		return nil
	}
	rtt := clock.RTTUs
	return &rtt
}

// JitterUs returns the jitter (MAD of RTT) in microseconds, or nil if < 2 samples.
func (c *ClockSync) JitterUs(peerID string) *int64 {
	c.mu.Lock()
	defer c.mu.Unlock()
	clock, ok := c.perPeer[peerID]
	if !ok || len(clock.samples) < 2 {
		return nil
	}
	median := clock.RTTUs
	var sum int64
	for _, s := range clock.samples {
		d := s - median
		if d < 0 {
			d = -d
		}
		sum += d
	}
	mad := sum / int64(len(clock.samples))
	return &mad
}

func medianOf(samples []int64) int64 {
	if len(samples) == 0 {
		return 0
	}
	sorted := make([]int64, len(samples))
	copy(sorted, samples)
	sort.Slice(sorted, func(i, j int) bool { return sorted[i] < sorted[j] })
	return sorted[len(sorted)/2]
}
