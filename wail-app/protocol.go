package main

import (
	"encoding/json"
	"fmt"
)

// SyncMessage represents messages exchanged between peers via the signaling server.
// Uses tagged JSON: {"type": "TempoChange", "bpm": 120.0, ...}
type SyncMessage struct {
	Type string `json:"type"`

	// Ping
	ID       uint64 `json:"id,omitempty"`
	SentAtUs int64  `json:"sent_at_us,omitempty"`

	// Pong
	PingSentAtUs int64 `json:"ping_sent_at_us,omitempty"`
	PongSentAtUs int64 `json:"pong_sent_at_us,omitempty"`

	// TempoChange / StateSnapshot
	BPM         float64 `json:"bpm,omitempty"`
	Quantum     float64 `json:"quantum,omitempty"`
	TimestampUs int64   `json:"timestamp_us,omitempty"`
	Beat        float64 `json:"beat,omitempty"`
	Phase       float64 `json:"phase,omitempty"`

	// IntervalConfig
	Bars uint32 `json:"bars,omitempty"`

	// Hello
	PeerID      string  `json:"peer_id,omitempty"`
	DisplayName *string `json:"display_name,omitempty"`
	Identity    *string `json:"identity,omitempty"`

	// AudioCapabilities
	SampleRates   []uint32 `json:"sample_rates,omitempty"`
	ChannelCounts []uint16 `json:"channel_counts,omitempty"`
	CanSend       bool     `json:"can_send,omitempty"`
	CanReceive    bool     `json:"can_receive,omitempty"`
	MaxStreams    *uint16  `json:"max_streams,omitempty"`

	// AudioIntervalReady
	IntervalIndex int64  `json:"interval_index,omitempty"`
	WireSize      uint32 `json:"wire_size,omitempty"`

	// IntervalBoundary
	Index int64 `json:"index,omitempty"`

	// AudioStatus
	AudioDCOpen       bool   `json:"audio_dc_open,omitempty"`
	IntervalsSent     uint64 `json:"intervals_sent,omitempty"`
	IntervalsReceived uint64 `json:"intervals_received,omitempty"`
	PluginConnected   bool   `json:"plugin_connected,omitempty"`
	Seq               uint64 `json:"seq,omitempty"`

	// ChatMessage
	SenderName string `json:"sender_name,omitempty"`
	Text       string `json:"text,omitempty"`

	// StreamNames
	Names map[string]string `json:"names,omitempty"`
}

// NewPing creates a Ping sync message.
func NewPing(id uint64, sentAtUs int64) SyncMessage {
	return SyncMessage{Type: "Ping", ID: id, SentAtUs: sentAtUs}
}

// NewPong creates a Pong sync message.
func NewPong(id uint64, pingSentAtUs, pongSentAtUs int64) SyncMessage {
	return SyncMessage{Type: "Pong", ID: id, PingSentAtUs: pingSentAtUs, PongSentAtUs: pongSentAtUs}
}

// NewTempoChange creates a TempoChange sync message.
func NewTempoChange(bpm, quantum float64, timestampUs int64) SyncMessage {
	return SyncMessage{Type: "TempoChange", BPM: bpm, Quantum: quantum, TimestampUs: timestampUs}
}

// NewStateSnapshot creates a StateSnapshot sync message.
func NewStateSnapshot(bpm, beat, phase, quantum float64, timestampUs int64) SyncMessage {
	return SyncMessage{Type: "StateSnapshot", BPM: bpm, Beat: beat, Phase: phase, Quantum: quantum, TimestampUs: timestampUs}
}

// NewIntervalConfig creates an IntervalConfig sync message.
func NewIntervalConfig(bars uint32, quantum float64) SyncMessage {
	return SyncMessage{Type: "IntervalConfig", Bars: bars, Quantum: quantum}
}

// NewHello creates a Hello sync message.
func NewHello(peerID string, displayName, identity *string) SyncMessage {
	return SyncMessage{Type: "Hello", PeerID: peerID, DisplayName: displayName, Identity: identity}
}

// NewAudioCapabilities creates an AudioCapabilities sync message.
func NewAudioCapabilities(sampleRates []uint32, channelCounts []uint16, canSend, canReceive bool) SyncMessage {
	return SyncMessage{Type: "AudioCapabilities", SampleRates: sampleRates, ChannelCounts: channelCounts, CanSend: canSend, CanReceive: canReceive}
}

// NewIntervalBoundary creates an IntervalBoundary sync message.
func NewIntervalBoundary(index int64) SyncMessage {
	return SyncMessage{Type: "IntervalBoundary", Index: index}
}

// NewAudioStatus creates an AudioStatus sync message.
func NewAudioStatus(dcOpen bool, sent, recv uint64, pluginConn bool, seq uint64) SyncMessage {
	return SyncMessage{Type: "AudioStatus", AudioDCOpen: dcOpen, IntervalsSent: sent, IntervalsReceived: recv, PluginConnected: pluginConn, Seq: seq}
}

// NewChatMessage creates a ChatMessage sync message.
func NewChatMessage(senderName, text string) SyncMessage {
	return SyncMessage{Type: "ChatMessage", SenderName: senderName, Text: text}
}

// NewStreamNames creates a StreamNames sync message.
func NewStreamNames(names map[string]string) SyncMessage {
	return SyncMessage{Type: "StreamNames", Names: names}
}

// SignalMessage represents messages on the WebSocket signaling channel.
type SignalMessage struct {
	Type string `json:"type"`

	// PeerList
	Peers []string `json:"peers,omitempty"`

	// PeerJoined / PeerLeft
	PeerID      string  `json:"peer_id,omitempty"`
	DisplayName *string `json:"display_name,omitempty"`

	// LogBroadcast
	From        string `json:"from,omitempty"`
	Level       string `json:"level,omitempty"`
	Target      string `json:"target,omitempty"`
	Message     string `json:"message,omitempty"`
	TimestampUs uint64 `json:"timestamp_us,omitempty"`

	// MetricsReport
	DCOpen          bool                       `json:"dc_open,omitempty"`
	PluginConnected bool                       `json:"plugin_connected,omitempty"`
	PerPeer         map[string]PeerFrameReport `json:"per_peer,omitempty"`
	IPCDrops        uint64                     `json:"ipc_drops,omitempty"`
	BoundaryDriftUs *int64                     `json:"boundary_drift_us,omitempty"`
}

// PeerFrameReport contains cumulative audio frame counts and network health for one peer.
type PeerFrameReport struct {
	FramesExpected uint64 `json:"frames_expected"`
	FramesReceived uint64 `json:"frames_received"`
	RTTUs          *int64 `json:"rtt_us,omitempty"`
	JitterUs       *int64 `json:"jitter_us,omitempty"`
	DCDrops        uint64 `json:"dc_drops"`
	LateFrames     uint64 `json:"late_frames"`
	DecodeFailures uint64 `json:"decode_failures"`
}

// ServerMsg represents messages from the signaling server (internal deserialization).
type ServerMsg struct {
	Type             string                     `json:"type"`
	Peers            []string                   `json:"peers,omitempty"`
	PeerDisplayNames map[string]*string         `json:"peer_display_names,omitempty"`
	PeerID           string                     `json:"peer_id,omitempty"`
	DisplayName      *string                    `json:"display_name,omitempty"`
	Code             string                     `json:"code,omitempty"`
	MinVersion       *string                    `json:"min_version,omitempty"`
	SlotsAvailable   *uint64                    `json:"slots_available,omitempty"`
	From             string                     `json:"from,omitempty"`
	Payload          json.RawMessage            `json:"payload,omitempty"`
	Level            string                     `json:"level,omitempty"`
	Target           string                     `json:"target,omitempty"`
	Message          string                     `json:"message,omitempty"`
	TimestampUs      uint64                     `json:"timestamp_us,omitempty"`
	To               string                     `json:"to,omitempty"`
	PerPeer          map[string]PeerFrameReport `json:"per_peer,omitempty"`
}

// MeshEvent represents events from the peer mesh.
type MeshEvent struct {
	Type        string
	PeerID      string
	DisplayName *string
	PeerCount   int
	From        string
	Level       string
	Target      string
	Message     string
	TimestampUs uint64
}

// StreamNamesToWire converts internal u16-keyed stream names to string-keyed for wire format.
func StreamNamesToWire(names map[uint16]string) map[string]string {
	wire := make(map[string]string, len(names))
	for k, v := range names {
		wire[fmt.Sprintf("%d", k)] = v
	}
	return wire
}

// StreamNamesFromWire converts wire-format string-keyed stream names to internal u16-keyed.
func StreamNamesFromWire(wire map[string]string) map[uint16]string {
	names := make(map[uint16]string, len(wire))
	for k, v := range wire {
		var idx uint16
		if _, err := fmt.Sscanf(k, "%d", &idx); err == nil {
			names[idx] = v
		}
	}
	return names
}
