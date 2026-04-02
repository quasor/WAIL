package main

import (
	"fmt"
	"sync"
	"time"
)

const maxSlots = 16

// PeerState holds all state tracked per remote peer.
type PeerState struct {
	DisplayName         *string
	Identity            *string
	Slots               map[uint16]int // stream_id → slot index
	HelloSent           bool
	LastSeen            time.Time
	EverReceivedMessage bool
	AudioRecvCount      uint64
	AudioRecvPrev       uint64
	RemoteIntervalsSent uint64
	TotalFramesExpected uint64
	TotalFramesReceived uint64
	IntervalFramesExpected map[int64]uint64
	LateFrames          uint64
	PrevStatus          string
	AddedAt             time.Time
	HelloRetrySent      bool
	StreamNames         map[uint16]string
}

// NewPeerState creates a new peer state.
func NewPeerState(displayName *string) *PeerState {
	now := time.Now()
	return &PeerState{
		DisplayName:            displayName,
		Slots:                  make(map[uint16]int),
		LastSeen:               now,
		AddedAt:                now,
		IntervalFramesExpected: make(map[int64]uint64),
		StreamNames:            make(map[uint16]string),
	}
}

// SlotEntry tracks a slot assignment.
type SlotEntry struct {
	ClientID     string
	ChannelIndex uint16
	Occupied     bool
	// Affinity: when released, client_id is remembered for reclaim
	AffinityClientID string
}

// PeerRegistry is the consolidated registry of all connected peers.
type PeerRegistry struct {
	mu    sync.Mutex
	peers map[string]*PeerState
	slots [maxSlots]SlotEntry
}

// NewPeerRegistry creates a new peer registry.
func NewPeerRegistry() *PeerRegistry {
	return &PeerRegistry{
		peers: make(map[string]*PeerState),
	}
}

// Get returns a peer's state (caller must hold no lock).
func (r *PeerRegistry) Get(peerID string) *PeerState {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.peers[peerID]
}

// GetMut returns a peer's state for mutation.
func (r *PeerRegistry) GetMut(peerID string) *PeerState {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.peers[peerID]
}

// Add adds or updates a peer entry.
func (r *PeerRegistry) Add(peerID string, displayName *string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if p, ok := r.peers[peerID]; ok {
		p.LastSeen = time.Now()
		if displayName != nil {
			p.DisplayName = displayName
		}
	} else {
		r.peers[peerID] = NewPeerState(displayName)
	}
}

// Remove removes a peer and frees its slots with affinity.
func (r *PeerRegistry) Remove(peerID string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	peer, ok := r.peers[peerID]
	if !ok {
		return
	}
	clientID := peerID
	if peer.Identity != nil {
		clientID = *peer.Identity
	}
	// Release slots with affinity
	for i := range r.slots {
		if r.slots[i].Occupied && r.slots[i].ClientID == clientID {
			r.slots[i].Occupied = false
			r.slots[i].AffinityClientID = clientID
		}
	}
	delete(r.peers, peerID)
}

// AssignSlot assigns a slot for (peerID, streamID). Returns slot index or -1 if full/unknown.
func (r *PeerRegistry) AssignSlot(peerID string, streamID uint16) int {
	r.mu.Lock()
	defer r.mu.Unlock()
	peer, ok := r.peers[peerID]
	if !ok {
		return -1
	}
	if existing, ok := peer.Slots[streamID]; ok {
		return existing
	}
	clientID := peerID
	if peer.Identity != nil {
		clientID = *peer.Identity
	}

	// Check affinity first
	for i := range r.slots {
		if !r.slots[i].Occupied && r.slots[i].AffinityClientID == clientID {
			r.slots[i] = SlotEntry{ClientID: clientID, ChannelIndex: streamID, Occupied: true}
			peer.Slots[streamID] = i
			return i
		}
	}
	// Find first free slot
	for i := range r.slots {
		if !r.slots[i].Occupied {
			r.slots[i] = SlotEntry{ClientID: clientID, ChannelIndex: streamID, Occupied: true}
			peer.Slots[streamID] = i
			return i
		}
	}
	return -1
}

// SlotFor returns the slot assigned to (peerID, streamID), or -1.
func (r *PeerRegistry) SlotFor(peerID string, streamID uint16) int {
	r.mu.Lock()
	defer r.mu.Unlock()
	peer, ok := r.peers[peerID]
	if !ok {
		return -1
	}
	if slot, ok := peer.Slots[streamID]; ok {
		return slot
	}
	return -1
}

// MarkHelloSent marks Hello as sent. Returns true if first time.
func (r *PeerRegistry) MarkHelloSent(peerID string) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	peer, ok := r.peers[peerID]
	if !ok || peer.HelloSent {
		return false
	}
	peer.HelloSent = true
	return true
}

// ClearHelloSent clears the hello_sent flag.
func (r *PeerRegistry) ClearHelloSent(peerID string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if peer, ok := r.peers[peerID]; ok {
		peer.HelloSent = false
	}
}

// TimedOutPeers returns peers that have communicated but are now silent.
func (r *PeerRegistry) TimedOutPeers(timeout time.Duration) []string {
	r.mu.Lock()
	defer r.mu.Unlock()
	now := time.Now()
	var result []string
	for id, p := range r.peers {
		if p.EverReceivedMessage && now.Sub(p.LastSeen) > timeout {
			result = append(result, id)
		}
	}
	return result
}

// SeedNames adds peers from join response without overwriting existing entries.
func (r *PeerRegistry) SeedNames(names map[string]*string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	for peerID, displayName := range names {
		if _, ok := r.peers[peerID]; !ok {
			r.peers[peerID] = NewPeerState(displayName)
		}
	}
}

// SeedLastSeen resets last_seen to now for all peers.
func (r *PeerRegistry) SeedLastSeen() {
	r.mu.Lock()
	defer r.mu.Unlock()
	now := time.Now()
	for _, p := range r.peers {
		p.LastSeen = now
	}
}

// FlushAudioRecvPrev updates prev = count for all peers.
func (r *PeerRegistry) FlushAudioRecvPrev() {
	r.mu.Lock()
	defer r.mu.Unlock()
	for _, p := range r.peers {
		p.AudioRecvPrev = p.AudioRecvCount
	}
}

// RekeyPeerSlots migrates slots from peerID key to identity key.
func (r *PeerRegistry) RekeyPeerSlots(peerID, identity string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	for i := range r.slots {
		if r.slots[i].Occupied && r.slots[i].ClientID == peerID {
			r.slots[i].ClientID = identity
		}
		if r.slots[i].AffinityClientID == peerID {
			r.slots[i].AffinityClientID = identity
		}
	}
}

// FindByIdentity finds the peer_id of a peer with the given identity.
func (r *PeerRegistry) FindByIdentity(identity string) (string, bool) {
	r.mu.Lock()
	defer r.mu.Unlock()
	for id, p := range r.peers {
		if p.Identity != nil && *p.Identity == identity {
			return id, true
		}
	}
	return "", false
}

// NoIdentityActivePeers returns peers that are active but whose Hello handshake hasn't completed.
// Returns (soft, hard) buckets.
func (r *PeerRegistry) NoIdentityActivePeers(softTimeout, hardTimeout time.Duration) (soft, hard []string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	now := time.Now()
	for id, p := range r.peers {
		if !p.EverReceivedMessage || p.Identity != nil {
			continue
		}
		elapsed := now.Sub(p.AddedAt)
		if elapsed >= hardTimeout {
			hard = append(hard, id)
		} else if elapsed >= softTimeout && !p.HelloRetrySent {
			soft = append(soft, id)
		}
	}
	return
}

// MarkHelloRetrySent marks that a soft retry was sent.
func (r *PeerRegistry) MarkHelloRetrySent(peerID string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if p, ok := r.peers[peerID]; ok {
		p.HelloRetrySent = true
	}
}

// DeriveStatus derives a display status string for a peer.
func (r *PeerRegistry) DeriveStatus(peerID string) string {
	r.mu.Lock()
	defer r.mu.Unlock()
	peer, ok := r.peers[peerID]
	if !ok || peer.DisplayName == nil {
		return "connecting"
	}
	return "connected"
}

// ActiveMappings returns (clientID, channelIndex, slotIndex) for all occupied slots.
func (r *PeerRegistry) ActiveMappings() []SlotMapping {
	r.mu.Lock()
	defer r.mu.Unlock()
	var result []SlotMapping
	for i := range r.slots {
		if r.slots[i].Occupied {
			result = append(result, SlotMapping{
				SlotIndex:    i,
				ClientID:     r.slots[i].ClientID,
				ChannelIndex: r.slots[i].ChannelIndex,
			})
		}
	}
	return result
}

// SlotMapping represents an active slot assignment.
type SlotMapping struct {
	SlotIndex    int
	ClientID     string
	ChannelIndex uint16
}

// ShortID returns a short display ID for a slot mapping (e.g. "a1b2c3:0").
func (m SlotMapping) ShortID() string {
	id := m.ClientID
	if len(id) > 6 {
		id = id[:6]
	}
	return fmt.Sprintf("%s:%d", id, m.ChannelIndex)
}

// AllPeerIDs returns all known peer IDs.
func (r *PeerRegistry) AllPeerIDs() []string {
	r.mu.Lock()
	defer r.mu.Unlock()
	ids := make([]string, 0, len(r.peers))
	for id := range r.peers {
		ids = append(ids, id)
	}
	return ids
}

// WithPeer calls fn with the peer state while holding the lock. Returns false if peer unknown.
func (r *PeerRegistry) WithPeer(peerID string, fn func(p *PeerState)) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	p, ok := r.peers[peerID]
	if !ok {
		return false
	}
	fn(p)
	return true
}

// IsSlotOccupied returns whether a slot is occupied.
func (r *PeerRegistry) IsSlotOccupied(slot int) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	if slot < 0 || slot >= maxSlots {
		return false
	}
	return r.slots[slot].Occupied
}
