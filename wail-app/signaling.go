package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"strings"
	"sync"

	"github.com/gorilla/websocket"
)

const appVersion = "2.0.0-go"

// PublicRoom represents a public room from the signaling server.
type PublicRoom struct {
	Room         string   `json:"room"`
	CreatedAt    int64    `json:"created_at"`
	PeerCount    uint32   `json:"peer_count"`
	DisplayNames []string `json:"display_names"`
	BPM          *float64 `json:"bpm,omitempty"`
}

// ListPublicRooms fetches the list of public rooms from a signaling server.
func ListPublicRooms(baseURL string) ([]PublicRoom, error) {
	httpURL := strings.Replace(baseURL, "wss://", "https://", 1)
	httpURL = strings.Replace(httpURL, "ws://", "http://", 1)
	httpURL = strings.TrimRight(httpURL, "/")

	resp, err := http.Get(httpURL + "/rooms")
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("rooms endpoint returned %d", resp.StatusCode)
	}

	var result struct {
		Rooms []PublicRoom `json:"rooms"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
		return nil, err
	}
	return result.Rooms, nil
}

// SignalingClient manages a WebSocket connection to the signaling server.
type SignalingClient struct {
	mu   sync.Mutex
	conn *websocket.Conn

	// Outgoing channels
	syncOutCh  chan outgoingSync
	audioOutCh chan []byte
	ctrlOutCh  chan SignalMessage

	// Suppress leave on close (for reconnection)
	suppressLeave bool

	cancel context.CancelFunc
}

type outgoingSync struct {
	broadcast bool
	peerID    string
	msg       SyncMessage
}

// SignalingChannels holds the incoming channels from the signaling connection.
type SignalingChannels struct {
	IncomingCh chan SignalMessage                // control plane (PeerJoined, PeerLeft, etc.)
	SyncCh     chan FromPeerSync                // sync messages from peers
	AudioCh    chan FromPeerAudio               // binary audio from peers
}

// FromPeerSync wraps a sync message with the sender's peer ID.
type FromPeerSync struct {
	From string
	Msg  SyncMessage
}

// FromPeerAudio wraps binary audio data with the sender's peer ID.
type FromPeerAudio struct {
	From string
	Data []byte
}

// ConnectSignaling connects to the signaling server and joins a room.
// Returns the client, channels, and initial peer display names.
func ConnectSignaling(
	ctx context.Context,
	serverURL, room, peerID string,
	password *string,
	streamCount uint16,
	displayName *string,
) (*SignalingClient, *SignalingChannels, map[string]*string, error) {
	wsURL := strings.TrimRight(serverURL, "/") + "/ws"

	conn, _, err := websocket.DefaultDialer.DialContext(ctx, wsURL, nil)
	if err != nil {
		return nil, nil, nil, fmt.Errorf("websocket connect: %w", err)
	}

	// Send join message
	joinMsg := map[string]any{
		"type":           "join",
		"room":           room,
		"peer_id":        peerID,
		"stream_count":   streamCount,
		"client_version": appVersion,
	}
	if password != nil {
		joinMsg["password"] = *password
	}
	if displayName != nil {
		joinMsg["display_name"] = *displayName
	}
	if err := conn.WriteJSON(joinMsg); err != nil {
		conn.Close()
		return nil, nil, nil, fmt.Errorf("send join: %w", err)
	}

	// Wait for join_ok or join_error
	var serverMsg ServerMsg
	for {
		_, data, err := conn.ReadMessage()
		if err != nil {
			conn.Close()
			return nil, nil, nil, fmt.Errorf("read join response: %w", err)
		}
		if err := json.Unmarshal(data, &serverMsg); err != nil {
			continue
		}
		if serverMsg.Type == "join_ok" || serverMsg.Type == "join_error" {
			break
		}
	}

	if serverMsg.Type == "join_error" {
		conn.Close()
		switch serverMsg.Code {
		case "unauthorized":
			return nil, nil, nil, fmt.Errorf("invalid room password")
		case "room_full":
			slots := uint64(0)
			if serverMsg.SlotsAvailable != nil {
				slots = *serverMsg.SlotsAvailable
			}
			return nil, nil, nil, fmt.Errorf("room full — only %d slots available", slots)
		case "version_outdated":
			min := "unknown"
			if serverMsg.MinVersion != nil {
				min = *serverMsg.MinVersion
			}
			return nil, nil, nil, fmt.Errorf("version %s outdated, need %s", appVersion, min)
		default:
			return nil, nil, nil, fmt.Errorf("join failed: %s", serverMsg.Code)
		}
	}

	log.Printf("[signaling] Joined room %s as %s (%d existing peers)", room, peerID, len(serverMsg.Peers))

	// Set up channels
	incomingCh := make(chan SignalMessage, 64)
	syncCh := make(chan FromPeerSync, 256)
	audioCh := make(chan FromPeerAudio, 1024)
	syncOutCh := make(chan outgoingSync, 256)
	audioOutCh := make(chan []byte, 256)
	ctrlOutCh := make(chan SignalMessage, 64)

	// Push initial PeerList
	incomingCh <- SignalMessage{Type: "PeerList", Peers: serverMsg.Peers}

	childCtx, cancel := context.WithCancel(ctx)

	client := &SignalingClient{
		conn:       conn,
		syncOutCh:  syncOutCh,
		audioOutCh: audioOutCh,
		ctrlOutCh:  ctrlOutCh,
		cancel:     cancel,
	}

	channels := &SignalingChannels{
		IncomingCh: incomingCh,
		SyncCh:     syncCh,
		AudioCh:    audioCh,
	}

	// Spawn read goroutine
	go func() {
		defer close(incomingCh)
		for {
			msgType, data, err := conn.ReadMessage()
			if err != nil {
				select {
				case <-childCtx.Done():
				default:
					log.Printf("[signaling] WebSocket read error: %v", err)
				}
				return
			}

			if msgType == websocket.BinaryMessage {
				// Binary: [1 byte peer_id_len][peer_id][audio_payload]
				if len(data) < 1 {
					continue
				}
				pidLen := int(data[0])
				if len(data) < 1+pidLen {
					continue
				}
				from := string(data[1 : 1+pidLen])
				audioData := make([]byte, len(data)-(1+pidLen))
				copy(audioData, data[1+pidLen:])
				select {
				case audioCh <- FromPeerAudio{From: from, Data: audioData}:
				default:
					// Drop if channel full
				}
				continue
			}

			// Text message
			var msg ServerMsg
			if err := json.Unmarshal(data, &msg); err != nil {
				log.Printf("[signaling] Failed to parse server message: %v", err)
				continue
			}

			switch msg.Type {
			case "peer_joined":
				incomingCh <- SignalMessage{
					Type:        "PeerJoined",
					PeerID:      msg.PeerID,
					DisplayName: msg.DisplayName,
				}
			case "peer_left":
				incomingCh <- SignalMessage{Type: "PeerLeft", PeerID: msg.PeerID}
			case "sync":
				var syncMsg SyncMessage
				if err := json.Unmarshal(msg.Payload, &syncMsg); err != nil {
					log.Printf("[signaling] Failed to parse sync payload: %v", err)
					continue
				}
				select {
				case syncCh <- FromPeerSync{From: msg.From, Msg: syncMsg}:
				default:
				}
			case "evicted":
				log.Printf("[signaling] Evicted by server")
				return
			case "log":
				incomingCh <- SignalMessage{
					Type:        "LogBroadcast",
					From:        msg.From,
					Level:       msg.Level,
					Target:      msg.Target,
					Message:     msg.Message,
					TimestampUs: msg.TimestampUs,
				}
			}
		}
	}()

	// Spawn write goroutine
	go func() {
		for {
			select {
			case <-childCtx.Done():
				if !client.suppressLeave {
					conn.WriteJSON(map[string]string{"type": "leave"})
				}
				conn.Close()
				return
			case out := <-syncOutCh:
				var raw map[string]any
				payload, _ := json.Marshal(out.msg)
				if out.broadcast {
					raw = map[string]any{
						"type":    "sync",
						"payload": json.RawMessage(payload),
					}
				} else {
					raw = map[string]any{
						"type":    "sync_to",
						"to":      out.peerID,
						"payload": json.RawMessage(payload),
					}
				}
				if err := conn.WriteJSON(raw); err != nil {
					log.Printf("[signaling] Write failed: %v", err)
					return
				}
			case data := <-audioOutCh:
				if err := conn.WriteMessage(websocket.BinaryMessage, data); err != nil {
					log.Printf("[signaling] Audio write failed: %v", err)
					return
				}
			case msg := <-ctrlOutCh:
				var raw map[string]any
				switch msg.Type {
				case "LogBroadcast":
					raw = map[string]any{
						"type":         "log",
						"level":        msg.Level,
						"target":       msg.Target,
						"message":      msg.Message,
						"timestamp_us": msg.TimestampUs,
					}
				case "MetricsReport":
					raw = map[string]any{
						"type":              "metrics_report",
						"dc_open":           msg.DCOpen,
						"plugin_connected":  msg.PluginConnected,
						"per_peer":          msg.PerPeer,
						"ipc_drops":         msg.IPCDrops,
						"boundary_drift_us": msg.BoundaryDriftUs,
					}
				default:
					continue
				}
				if err := conn.WriteJSON(raw); err != nil {
					log.Printf("[signaling] Control write failed: %v", err)
					return
				}
			}
		}
	}()

	return client, channels, serverMsg.PeerDisplayNames, nil
}

// BroadcastSync broadcasts a sync message to all peers.
func (sc *SignalingClient) BroadcastSync(msg SyncMessage) {
	select {
	case sc.syncOutCh <- outgoingSync{broadcast: true, msg: msg}:
	default:
	}
}

// SendSyncTo sends a sync message to a specific peer.
func (sc *SignalingClient) SendSyncTo(peerID string, msg SyncMessage) {
	select {
	case sc.syncOutCh <- outgoingSync{broadcast: false, peerID: peerID, msg: msg}:
	default:
	}
}

// SendAudio broadcasts binary audio data.
func (sc *SignalingClient) SendAudio(data []byte) {
	select {
	case sc.audioOutCh <- data:
	default:
	}
}

// SendControl sends a control-plane message (log, metrics).
func (sc *SignalingClient) SendControl(msg SignalMessage) {
	select {
	case sc.ctrlOutCh <- msg:
	default:
	}
}

// SuppressLeaveOnClose prevents the automatic leave message on disconnect (for reconnection).
func (sc *SignalingClient) SuppressLeaveOnClose() {
	sc.mu.Lock()
	defer sc.mu.Unlock()
	sc.suppressLeave = true
}

// Close shuts down the signaling client.
func (sc *SignalingClient) Close() {
	if sc.cancel != nil {
		sc.cancel()
	}
}

// PeerMesh manages communication with all peers via the signaling server.
type PeerMesh struct {
	peerID           string
	signaling        *SignalingClient
	channels         *SignalingChannels
	peers            map[string]bool
	streamCount      uint16
	initialPeerNames map[string]*string
	mu               sync.Mutex
}

// NewPeerMesh creates a mesh from a signaling client.
func NewPeerMesh(peerID string, signaling *SignalingClient, channels *SignalingChannels, streamCount uint16, initialPeerNames map[string]*string) *PeerMesh {
	return &PeerMesh{
		peerID:           peerID,
		signaling:        signaling,
		channels:         channels,
		peers:            make(map[string]bool),
		streamCount:      streamCount,
		initialPeerNames: initialPeerNames,
	}
}

// Broadcast sends a sync message to all peers.
func (m *PeerMesh) Broadcast(msg SyncMessage) {
	m.signaling.BroadcastSync(msg)
}

// SendTo sends a sync message to a specific peer.
func (m *PeerMesh) SendTo(peerID string, msg SyncMessage) error {
	m.signaling.SendSyncTo(peerID, msg)
	return nil
}

// BroadcastAudio sends binary audio to all peers.
func (m *PeerMesh) BroadcastAudio(data []byte) {
	m.signaling.SendAudio(data)
}

// PollSignaling processes one signaling event and returns a MeshEvent.
func (m *PeerMesh) PollSignaling() (*MeshEvent, bool) {
	msg, ok := <-m.channels.IncomingCh
	if !ok {
		return nil, false
	}
	return m.handleSignalMessage(msg), true
}

func (m *PeerMesh) handleSignalMessage(msg SignalMessage) *MeshEvent {
	m.mu.Lock()
	defer m.mu.Unlock()

	switch msg.Type {
	case "PeerList":
		for _, pid := range msg.Peers {
			if pid != m.peerID {
				m.peers[pid] = true
			}
		}
		return &MeshEvent{Type: "PeerListReceived", PeerCount: len(msg.Peers)}
	case "PeerJoined":
		m.peers[msg.PeerID] = true
		return &MeshEvent{Type: "PeerJoined", PeerID: msg.PeerID, DisplayName: msg.DisplayName}
	case "PeerLeft":
		delete(m.peers, msg.PeerID)
		return &MeshEvent{Type: "PeerLeft", PeerID: msg.PeerID}
	case "LogBroadcast":
		return &MeshEvent{
			Type: "PeerLogBroadcast", From: msg.From,
			Level: msg.Level, Target: msg.Target,
			Message: msg.Message, TimestampUs: msg.TimestampUs,
		}
	}
	return nil
}

// ConnectedPeers returns a list of known peer IDs.
func (m *PeerMesh) ConnectedPeers() []string {
	m.mu.Lock()
	defer m.mu.Unlock()
	peers := make([]string, 0, len(m.peers))
	for pid := range m.peers {
		peers = append(peers, pid)
	}
	return peers
}

// TakeInitialPeerNames returns and clears the initial peer names.
func (m *PeerMesh) TakeInitialPeerNames() map[string]*string {
	m.mu.Lock()
	defer m.mu.Unlock()
	names := m.initialPeerNames
	m.initialPeerNames = nil
	return names
}

// AnyPeersConnected returns true if any peers are known.
func (m *PeerMesh) AnyPeersConnected() bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	return len(m.peers) > 0
}

// IsPeerConnected checks if a specific peer is known.
func (m *PeerMesh) IsPeerConnected(peerID string) bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.peers[peerID]
}

// RemovePeer removes a peer from tracking.
func (m *PeerMesh) RemovePeer(peerID string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	delete(m.peers, peerID)
}

// SendLog sends a log broadcast to the signaling server.
func (m *PeerMesh) SendLog(level, target, message string, timestampUs uint64) {
	m.signaling.SendControl(SignalMessage{
		Type: "LogBroadcast", Level: level, Target: target,
		Message: message, TimestampUs: timestampUs,
	})
}

// SendMetricsReport sends a metrics report to the signaling server.
func (m *PeerMesh) SendMetricsReport(dcOpen, pluginConnected bool, perPeer map[string]PeerFrameReport, ipcDrops uint64, boundaryDriftUs *int64) {
	m.signaling.SendControl(SignalMessage{
		Type: "MetricsReport", DCOpen: dcOpen, PluginConnected: pluginConnected,
		PerPeer: perPeer, IPCDrops: ipcDrops, BoundaryDriftUs: boundaryDriftUs,
	})
}

// ReconnectSignaling reconnects the WebSocket and returns new channels.
func (m *PeerMesh) ReconnectSignaling(
	ctx context.Context,
	serverURL, room string,
	password *string,
	displayName *string,
) (*SignalingChannels, map[string]*string, error) {
	// Suppress leave on old connection
	m.signaling.SuppressLeaveOnClose()
	m.signaling.Close()

	newClient, newChannels, peerNames, err := ConnectSignaling(
		ctx, serverURL, room, m.peerID, password, m.streamCount, displayName,
	)
	if err != nil {
		return nil, nil, err
	}

	m.mu.Lock()
	m.signaling = newClient
	m.channels = newChannels
	m.initialPeerNames = peerNames
	m.mu.Unlock()

	// Consume initial PeerList
	if msg, ok := <-newChannels.IncomingCh; ok && msg.Type == "PeerList" {
		m.mu.Lock()
		for _, pid := range msg.Peers {
			if pid != m.peerID {
				m.peers[pid] = true
			}
		}
		m.mu.Unlock()
	}

	return newChannels, peerNames, nil
}
