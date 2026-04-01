package main

import (
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"os"
	"sort"
	"sync"
	"time"

	"github.com/gorilla/websocket"
	_ "modernc.org/sqlite"
)

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const (
	minVersion     = "0.0.0" // bump when a breaking client change ships
	stalePeerSec   = 30
	pingInterval   = 15 * time.Second
	pongWait       = 20 * time.Second
	writeWait      = 10 * time.Second
	maxMessageSize = 256 * 1024
	// Keep at most this many completed sessions per room in memory.
	maxCompletedSessions = 50
)

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

// clientMsg is the envelope for all client→server messages.
type clientMsg struct {
	Type          string          `json:"type"`
	Room          string          `json:"room,omitempty"`
	PeerID        string          `json:"peer_id,omitempty"`
	Password      string          `json:"password,omitempty"`
	StreamCount   int             `json:"stream_count,omitempty"`
	DisplayName   *string         `json:"display_name,omitempty"`
	ClientVersion string          `json:"client_version,omitempty"`
	To            string          `json:"to,omitempty"`
	From          string          `json:"from,omitempty"`
	Payload       json.RawMessage `json:"payload,omitempty"`
	// Log broadcast fields
	Level       string `json:"level,omitempty"`
	Target      string `json:"target,omitempty"`
	Message     string `json:"message,omitempty"`
	TimestampUs int64  `json:"timestamp_us,omitempty"`
	// Metrics report fields
	DcOpen          *bool                      `json:"dc_open,omitempty"`
	PluginConnected *bool                      `json:"plugin_connected,omitempty"`
	PerPeer         map[string]peerFrameReport `json:"per_peer,omitempty"`
	IpcDrops        uint64                     `json:"ipc_drops,omitempty"`
	BoundaryDriftUs *int64                     `json:"boundary_drift_us,omitempty"`
}

// peerFrameReport is what each client reports about a specific remote peer.
type peerFrameReport struct {
	FramesExpected uint64 `json:"frames_expected"`
	FramesReceived uint64 `json:"frames_received"`
	RttUs          *int64 `json:"rtt_us,omitempty"`
	JitterUs       *int64 `json:"jitter_us,omitempty"`
	DcDrops        uint64 `json:"dc_drops"`
	LateFrames     uint64 `json:"late_frames"`
	DecodeFailures uint64 `json:"decode_failures"`
}

// wsMessage carries a typed WebSocket frame (text or binary).
type wsMessage struct {
	msgType int
	data    []byte
}

// conn wraps a single WebSocket connection that has joined a room.
type conn struct {
	ws     *websocket.Conn
	room   string
	peerID string
	send   chan wsMessage
}

// ---------------------------------------------------------------------------
// Session metrics
// ---------------------------------------------------------------------------

// directionKey uniquely identifies one direction of audio flow.
type directionKey struct {
	from string
	to   string
}

// directionMetrics tracks frame drops for one direction in one phase.
type directionMetrics struct {
	FramesExpected uint64 `json:"frames_expected"`
	FramesReceived uint64 `json:"frames_received"`
	FramesDropped  uint64 `json:"frames_dropped"`
	RttUs          *int64 `json:"rtt_us,omitempty"`
	JitterUs       *int64 `json:"jitter_us,omitempty"`
	DcDrops        uint64 `json:"dc_drops"`
	LateFrames     uint64 `json:"late_frames"`
	DecodeFailures uint64 `json:"decode_failures"`
}

// peerStatus tracks the last-known state of a peer in a session.
type peerStatus struct {
	dcOpen          bool
	pluginConnected bool
	// Last cumulative values reported (for computing deltas at phase transition).
	lastPerPeer map[string]peerFrameReport
}

// session tracks aggregate metrics for a multi-peer session in a room.
type session struct {
	ID        string                              `json:"id"`
	Room      string                              `json:"room"`
	StartedAt time.Time                           `json:"started_at"`
	EndedAt   *time.Time                          `json:"ended_at,omitempty"`
	Phase     string                              `json:"phase"` // "joining" or "playing"
	Peers     []string                            `json:"peers"`
	Joining   map[string]*directionMetrics        `json:"joining"`           // key: "from→to"
	Playing   map[string]*directionMetrics        `json:"playing"`           // key: "from→to"
	peerState map[string]*peerStatus              // not serialized
	// Snapshot of cumulative values at the moment we transitioned to "playing".
	transitionSnapshot map[directionKey]peerFrameReport
}

func newSession(room string, peers []string) *session {
	id := fmt.Sprintf("%s-%d", room, time.Now().UnixMilli())
	s := &session{
		ID:                 id,
		Room:               room,
		StartedAt:          time.Now(),
		Phase:              "joining",
		Peers:              peers,
		Joining:            make(map[string]*directionMetrics),
		Playing:            make(map[string]*directionMetrics),
		peerState:          make(map[string]*peerStatus),
		transitionSnapshot: make(map[directionKey]peerFrameReport),
	}
	for _, p := range peers {
		s.peerState[p] = &peerStatus{lastPerPeer: make(map[string]peerFrameReport)}
	}
	return s
}

func dirKey(from, to string) string {
	return from + "→" + to
}

// updateMetrics processes a metrics_report from a peer and updates the session.
func (s *session) updateMetrics(reporter string, dcOpen, pluginConnected bool, perPeer map[string]peerFrameReport) {
	ps, ok := s.peerState[reporter]
	if !ok {
		ps = &peerStatus{lastPerPeer: make(map[string]peerFrameReport)}
		s.peerState[reporter] = ps
	}
	ps.dcOpen = dcOpen
	ps.pluginConnected = pluginConnected

	// Update frame counts for the current phase.
	// perPeer maps remote_peer_id → cumulative {frames_expected, frames_received}.
	// The direction is remote→reporter (reporter is observing frames FROM remote).
	target := s.Joining
	if s.Phase == "playing" {
		target = s.Playing
	}

	for remotePeer, report := range perPeer {
		dk := dirKey(remotePeer, reporter)

		if s.Phase == "playing" {
			// Subtract the snapshot at transition to get playing-phase-only counts.
			// Skip directions that weren't reported before the transition to avoid
			// inflating playing-phase counts with pre-transition cumulative values.
			snap, hasSnap := s.transitionSnapshot[directionKey{from: remotePeer, to: reporter}]
			if !hasSnap {
				// First report for this direction came after the transition.
				// Use current values as the baseline so the first delta is zero.
				s.transitionSnapshot[directionKey{from: remotePeer, to: reporter}] = report
				ps.lastPerPeer[remotePeer] = report
				continue
			}
			expected := report.FramesExpected - snap.FramesExpected
			received := report.FramesReceived - snap.FramesReceived
			dm, exists := target[dk]
			if !exists {
				dm = &directionMetrics{}
				target[dk] = dm
			}
			dm.FramesExpected = expected
			dm.FramesReceived = received
			if expected > received {
				dm.FramesDropped = expected - received
			} else {
				dm.FramesDropped = 0
			}
			// Point-in-time metrics: overwrite with latest values
			dm.RttUs = report.RttUs
			dm.JitterUs = report.JitterUs
			dm.DcDrops = report.DcDrops - snap.DcDrops
			dm.LateFrames = report.LateFrames - snap.LateFrames
			dm.DecodeFailures = report.DecodeFailures - snap.DecodeFailures
		} else {
			// Joining phase: use cumulative values directly.
			dm, exists := target[dk]
			if !exists {
				dm = &directionMetrics{}
				target[dk] = dm
			}
			dm.FramesExpected = report.FramesExpected
			dm.FramesReceived = report.FramesReceived
			if report.FramesExpected > report.FramesReceived {
				dm.FramesDropped = report.FramesExpected - report.FramesReceived
			} else {
				dm.FramesDropped = 0
			}
			dm.RttUs = report.RttUs
			dm.JitterUs = report.JitterUs
			dm.DcDrops = report.DcDrops
			dm.LateFrames = report.LateFrames
			dm.DecodeFailures = report.DecodeFailures
		}

		ps.lastPerPeer[remotePeer] = report
	}

	// Check phase transition: joining → playing when ALL peers have dc_open && plugin_connected.
	if s.Phase == "joining" {
		allReady := true
		for _, peer := range s.Peers {
			st, exists := s.peerState[peer]
			if !exists || !st.dcOpen || !st.pluginConnected {
				allReady = false
				break
			}
		}
		if allReady {
			log.Printf("[metrics] session %s transitioning to playing", s.ID)
			s.Phase = "playing"
			// Snapshot current cumulative values so we can compute playing-only deltas.
			for _, peer := range s.Peers {
				st := s.peerState[peer]
				if st == nil {
					continue
				}
				for remotePeer, report := range st.lastPerPeer {
					s.transitionSnapshot[directionKey{from: remotePeer, to: peer}] = report
				}
			}
		}
	}
}

// addPeer adds a peer to the session (for late joiners).
func (s *session) addPeer(peerID string) {
	for _, p := range s.Peers {
		if p == peerID {
			return
		}
	}
	s.Peers = append(s.Peers, peerID)
	if _, ok := s.peerState[peerID]; !ok {
		s.peerState[peerID] = &peerStatus{lastPerPeer: make(map[string]peerFrameReport)}
	}
}

// hub tracks all active connections, keyed by room → peer_id → conn.
type hub struct {
	mu    sync.Mutex
	rooms map[string]map[string]*conn
	db    *sql.DB
	// Session metrics: one active session per room (nil if <2 peers).
	activeSessions    map[string]*session   // room → session
	completedSessions map[string][]*session // room → recent completed sessions
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

func openDB() *sql.DB {
	path := os.Getenv("DB_PATH")
	if path == "" {
		path = "/data/wail.db"
	}
	db, err := sql.Open("sqlite", path+"?_journal_mode=WAL&_busy_timeout=5000")
	if err != nil {
		log.Fatalf("open db: %v", err)
	}
	for _, stmt := range []string{
		`CREATE TABLE IF NOT EXISTS peers (
			room TEXT NOT NULL,
			peer_id TEXT NOT NULL,
			display_name TEXT,
			stream_count INTEGER DEFAULT 1,
			last_seen INTEGER NOT NULL,
			PRIMARY KEY (room, peer_id)
		)`,
		`CREATE TABLE IF NOT EXISTS rooms (
			room TEXT PRIMARY KEY,
			password_hash TEXT,
			created_at INTEGER NOT NULL DEFAULT 0
		)`,
	} {
		if _, err := db.Exec(stmt); err != nil {
			log.Fatalf("migrate: %v", err)
		}
	}
	// Clean stale peers from previous run
	cutoff := time.Now().Unix() - stalePeerSec
	db.Exec("DELETE FROM peers WHERE last_seen < ?", cutoff)
	// Remove rooms whose peers were all stale/crashed — prevents a public ghost room
	// from persisting across a restart and blocking private re-creation of the same name.
	db.Exec("DELETE FROM rooms WHERE room NOT IN (SELECT DISTINCT room FROM peers)")
	return db
}

func hashPassword(pw string) string {
	h := sha256.Sum256([]byte(pw))
	return hex.EncodeToString(h[:])
}

// ---------------------------------------------------------------------------
// Hub methods
// ---------------------------------------------------------------------------

func newHub(db *sql.DB) *hub {
	return &hub{
		rooms:             make(map[string]map[string]*conn),
		db:                db,
		activeSessions:    make(map[string]*session),
		completedSessions: make(map[string][]*session),
	}
}

func (h *hub) join(c *conn, msg clientMsg) {
	h.mu.Lock()
	defer h.mu.Unlock()

	// Leave previous room if already joined (prevents stale references on double-join)
	if c.room != "" {
		h.leaveUnlocked(c)
	}

	room := msg.Room
	peerID := msg.PeerID
	streamCount := msg.StreamCount
	if streamCount < 1 {
		streamCount = 1
	}

	// Version check
	if semverLess(msg.ClientVersion, minVersion) {
		c.sendJSON(map[string]any{
			"type":        "join_error",
			"code":        "version_outdated",
			"min_version": minVersion,
		})
		return
	}

	// Password check
	var storedHash sql.NullString
	var roomExists bool
	err := h.db.QueryRow("SELECT password_hash FROM rooms WHERE room = ?", room).Scan(&storedHash)
	if err == nil {
		roomExists = true
	}

	if roomExists && storedHash.Valid && storedHash.String != "" {
		if hashPassword(msg.Password) != storedHash.String {
			c.sendJSON(map[string]any{"type": "join_error", "code": "unauthorized"})
			return
		}
	}

	// Capacity check (32 stream slots per room)
	const roomCapacity = 32
	var usedSlots int
	h.db.QueryRow("SELECT COALESCE(SUM(stream_count), 0) FROM peers WHERE room = ?", room).Scan(&usedSlots)
	if usedSlots+streamCount > roomCapacity {
		c.sendJSON(map[string]any{
			"type":            "join_error",
			"code":            "room_full",
			"slots_available": roomCapacity - usedSlots,
		})
		return
	}

	// Create room if needed
	if !roomExists {
		pwHash := ""
		if msg.Password != "" {
			pwHash = hashPassword(msg.Password)
		}
		h.db.Exec("INSERT OR IGNORE INTO rooms (room, password_hash, created_at) VALUES (?, ?, ?)",
			room, pwHash, time.Now().Unix())
	}

	// Upsert peer in DB
	displayName := ""
	if msg.DisplayName != nil {
		displayName = *msg.DisplayName
	}
	h.db.Exec(`INSERT INTO peers (room, peer_id, display_name, stream_count, last_seen)
		VALUES (?, ?, ?, ?, ?)
		ON CONFLICT(room, peer_id) DO UPDATE SET display_name=excluded.display_name, stream_count=excluded.stream_count, last_seen=excluded.last_seen`,
		room, peerID, displayName, streamCount, time.Now().Unix())

	// If this peer_id already has an active connection in the room, evict it.
	// Clear its room/peerID so the old readPump's deferred h.leave(c) becomes
	// a no-op and doesn't accidentally remove the NEW connection from the room.
	if roomConns, ok := h.rooms[room]; ok {
		if old, exists := roomConns[peerID]; exists && old != c {
			log.Printf("peer %s reconnecting in room %s — evicting old connection", peerID, room)
			delete(roomConns, peerID)
			old.room = ""
			old.peerID = ""
			close(old.send) // terminates writePump, which closes the WebSocket
		}
	}

	// Build peer list + display names from in-memory connections
	peers := []string{}
	peerDisplayNames := map[string]*string{}
	if roomConns, ok := h.rooms[room]; ok {
		for id, rc := range roomConns {
			if id != peerID {
				peers = append(peers, id)
				// Look up display name from DB
				var dn sql.NullString
				h.db.QueryRow("SELECT display_name FROM peers WHERE room = ? AND peer_id = ?", room, id).Scan(&dn)
				if dn.Valid && dn.String != "" {
					name := dn.String
					peerDisplayNames[id] = &name
				} else {
					peerDisplayNames[id] = nil
				}
				// Notify existing peer
				rc.sendJSON(map[string]any{
					"type":         "peer_joined",
					"peer_id":      peerID,
					"display_name": msg.DisplayName,
				})
			}
		}
	}

	// Register connection
	if h.rooms[room] == nil {
		h.rooms[room] = make(map[string]*conn)
	}
	h.rooms[room][peerID] = c
	c.room = room
	c.peerID = peerID

	// --- Session metrics: start or extend session ---
	peerCountAfter := len(h.rooms[room])
	if peerCountAfter >= 2 {
		if s, ok := h.activeSessions[room]; ok {
			// Session already active, add this peer.
			s.addPeer(peerID)
			log.Printf("[metrics] peer %s joined active session %s (now %d peers)", peerID, s.ID, peerCountAfter)
		} else {
			// New session: collect all current peer IDs.
			allPeers := make([]string, 0, peerCountAfter)
			for pid := range h.rooms[room] {
				allPeers = append(allPeers, pid)
			}
			s := newSession(room, allPeers)
			h.activeSessions[room] = s
			log.Printf("[metrics] session %s started with %d peers", s.ID, peerCountAfter)
		}
	}

	// Send join_ok
	c.sendJSON(map[string]any{
		"type":               "join_ok",
		"peers":              peers,
		"peer_display_names": peerDisplayNames,
	})
}

func (h *hub) signal(c *conn, msg clientMsg) {
	h.mu.Lock()
	defer h.mu.Unlock()

	if roomConns, ok := h.rooms[c.room]; ok {
		if target, ok := roomConns[msg.To]; ok {
			target.sendJSON(map[string]any{
				"type":    "signal",
				"to":      msg.To,
				"from":    c.peerID,
				"payload": msg.Payload,
			})
		}
	}
}

// broadcastSync relays a sync message (JSON) to all room peers except the sender.
func (h *hub) broadcastSync(c *conn, msg clientMsg) {
	h.mu.Lock()
	defer h.mu.Unlock()

	if c.room == "" {
		return
	}
	roomConns, ok := h.rooms[c.room]
	if !ok {
		return
	}
	for pid, rc := range roomConns {
		if pid != c.peerID {
			rc.sendJSON(map[string]any{
				"type":    "sync",
				"from":    c.peerID,
				"payload": msg.Payload,
			})
		}
	}
}

// syncTo relays a sync message to a specific peer in the room.
func (h *hub) syncTo(c *conn, msg clientMsg) {
	h.mu.Lock()
	defer h.mu.Unlock()

	if c.room == "" {
		return
	}
	roomConns, ok := h.rooms[c.room]
	if !ok {
		return
	}
	if target, ok := roomConns[msg.To]; ok {
		target.sendJSON(map[string]any{
			"type":    "sync",
			"from":    c.peerID,
			"payload": msg.Payload,
		})
	}
}

// broadcastAudioBinary relays a binary audio frame to all room peers except the sender.
// Prepends a sender header: [1 byte: peer_id_len][peer_id UTF-8 bytes][audio payload].
func (h *hub) broadcastAudioBinary(c *conn, data []byte) {
	h.mu.Lock()
	defer h.mu.Unlock()

	if c.room == "" {
		return
	}
	roomConns, ok := h.rooms[c.room]
	if !ok {
		return
	}

	// Prepend sender header
	pidBytes := []byte(c.peerID)
	frame := make([]byte, 1+len(pidBytes)+len(data))
	frame[0] = byte(len(pidBytes))
	copy(frame[1:1+len(pidBytes)], pidBytes)
	copy(frame[1+len(pidBytes):], data)

	for pid, rc := range roomConns {
		if pid != c.peerID {
			rc.sendBinary(frame)
		}
	}
}

func (h *hub) broadcastLog(c *conn, msg clientMsg) {
	h.mu.Lock()
	defer h.mu.Unlock()

	if c.room == "" {
		return
	}
	if roomConns, ok := h.rooms[c.room]; ok {
		for pid, rc := range roomConns {
			if pid != c.peerID {
				rc.sendJSON(map[string]any{
					"type":         "log",
					"from":         c.peerID,
					"level":        msg.Level,
					"target":       msg.Target,
					"message":      msg.Message,
					"timestamp_us": msg.TimestampUs,
				})
			}
		}
	}
}

func (h *hub) metricsReport(c *conn, msg clientMsg) {
	h.mu.Lock()
	defer h.mu.Unlock()

	if c.room == "" {
		return
	}

	s, ok := h.activeSessions[c.room]
	if !ok {
		return
	}

	dcOpen := false
	if msg.DcOpen != nil {
		dcOpen = *msg.DcOpen
	}
	pluginConnected := false
	if msg.PluginConnected != nil {
		pluginConnected = *msg.PluginConnected
	}

	s.updateMetrics(c.peerID, dcOpen, pluginConnected, msg.PerPeer)
}

func (h *hub) leave(c *conn) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.leaveUnlocked(c)
}

// leaveUnlocked removes c from its current room. Caller must hold h.mu.
func (h *hub) leaveUnlocked(c *conn) {
	if c.room == "" {
		return
	}

	room := c.room
	peerID := c.peerID

	// Remove from in-memory map
	if roomConns, ok := h.rooms[room]; ok {
		delete(roomConns, peerID)

		// Notify remaining peers
		for _, rc := range roomConns {
			rc.sendJSON(map[string]any{
				"type":    "peer_left",
				"peer_id": peerID,
			})
		}

		// --- Session metrics: end session if <2 peers ---
		peerCountAfter := len(roomConns)
		if peerCountAfter < 2 {
			if s, ok := h.activeSessions[room]; ok {
				now := time.Now()
				s.EndedAt = &now
				log.Printf("[metrics] session %s ended (peer %s left, %d remaining)", s.ID, peerID, peerCountAfter)
				// Archive to completed
				h.completedSessions[room] = append(h.completedSessions[room], s)
				// Trim old sessions
				if len(h.completedSessions[room]) > maxCompletedSessions {
					h.completedSessions[room] = h.completedSessions[room][len(h.completedSessions[room])-maxCompletedSessions:]
				}
				delete(h.activeSessions, room)
			}
		}

		// If room is empty, clean up
		if peerCountAfter == 0 {
			delete(h.rooms, room)
			delete(h.completedSessions, room)
			h.db.Exec("DELETE FROM rooms WHERE room = ?", room)
		}
	}

	// Remove from DB
	h.db.Exec("DELETE FROM peers WHERE room = ? AND peer_id = ?", room, peerID)

	c.room = ""
	c.peerID = ""
}

// ---------------------------------------------------------------------------
// Connection helpers
// ---------------------------------------------------------------------------

func (c *conn) sendJSON(v any) {
	defer func() { recover() }() // send may be closed if connection was evicted
	raw, err := json.Marshal(v)
	if err != nil {
		log.Printf("warn: sendJSON marshal error for peer %s: %v", c.peerID, err)
		return
	}
	select {
	case c.send <- wsMessage{websocket.TextMessage, raw}:
	default:
		log.Printf("warn: dropped message to peer %s (send buffer full)", c.peerID)
	}
}

func (c *conn) sendBinary(data []byte) {
	defer func() { recover() }()
	select {
	case c.send <- wsMessage{websocket.BinaryMessage, data}:
	default:
		log.Printf("warn: dropped binary message to peer %s (send buffer full)", c.peerID)
	}
}

func (c *conn) writePump() {
	ticker := time.NewTicker(pingInterval)
	defer func() {
		ticker.Stop()
		c.ws.Close()
	}()

	for {
		select {
		case msg, ok := <-c.send:
			c.ws.SetWriteDeadline(time.Now().Add(writeWait))
			if !ok {
				c.ws.WriteMessage(websocket.CloseMessage, []byte{})
				return
			}
			if err := c.ws.WriteMessage(msg.msgType, msg.data); err != nil {
				return
			}
		case <-ticker.C:
			c.ws.SetWriteDeadline(time.Now().Add(writeWait))
			if err := c.ws.WriteMessage(websocket.PingMessage, nil); err != nil {
				return
			}
		}
	}
}

func (c *conn) readPump(h *hub) {
	defer func() {
		h.leave(c)
		// send may already be closed if this connection was evicted by a
		// reconnecting peer. Recover from the double-close panic.
		func() {
			defer func() { recover() }()
			close(c.send)
		}()
		c.ws.Close()
	}()

	c.ws.SetReadLimit(maxMessageSize)
	c.ws.SetReadDeadline(time.Now().Add(pongWait))
	c.ws.SetPongHandler(func(string) error {
		c.ws.SetReadDeadline(time.Now().Add(pongWait))
		// Update last_seen in DB
		if c.room != "" {
			h.db.Exec("UPDATE peers SET last_seen = ? WHERE room = ? AND peer_id = ?",
				time.Now().Unix(), c.room, c.peerID)
		}
		return nil
	})

	for {
		msgType, raw, err := c.ws.ReadMessage()
		if err != nil {
			return
		}

		// Binary frames are audio data — relay to room peers
		if msgType == websocket.BinaryMessage {
			h.broadcastAudioBinary(c, raw)
			continue
		}

		var msg clientMsg
		if err := json.Unmarshal(raw, &msg); err != nil {
			continue
		}

		switch msg.Type {
		case "join":
			h.join(c, msg)
		case "signal":
			h.signal(c, msg)
		case "sync":
			h.broadcastSync(c, msg)
		case "sync_to":
			h.syncTo(c, msg)
		case "log":
			h.broadcastLog(c, msg)
		case "leave":
			h.leave(c)
		case "metrics_report":
			h.metricsReport(c, msg)
		}
	}
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

var upgrader = websocket.Upgrader{
	CheckOrigin: func(r *http.Request) bool { return true },
}

func handleWS(h *hub, w http.ResponseWriter, r *http.Request) {
	ws, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		log.Printf("upgrade: %v", err)
		return
	}

	c := &conn{
		ws:   ws,
		send: make(chan wsMessage, 256),
	}

	go c.writePump()
	c.readPump(h)
}

func handleRooms(h *hub, w http.ResponseWriter, r *http.Request) {
	h.mu.Lock()
	defer h.mu.Unlock()

	type roomInfo struct {
		Room         string   `json:"room"`
		CreatedAt    int64    `json:"created_at"`
		PeerCount    int      `json:"peer_count"`
		DisplayNames []string `json:"display_names"`
	}

	var result []roomInfo
	for roomName, conns := range h.rooms {
		// Skip password-protected rooms (they are private)
		var pwHash sql.NullString
		h.db.QueryRow("SELECT password_hash FROM rooms WHERE room = ?", roomName).Scan(&pwHash)
		if pwHash.Valid && pwHash.String != "" {
			continue
		}

		var createdAt int64
		h.db.QueryRow("SELECT created_at FROM rooms WHERE room = ?", roomName).Scan(&createdAt)

		names := []string{}
		for _, c := range conns {
			var dn sql.NullString
			h.db.QueryRow("SELECT display_name FROM peers WHERE room = ? AND peer_id = ?", roomName, c.peerID).Scan(&dn)
			if dn.Valid && dn.String != "" {
				names = append(names, dn.String)
			}
		}

		result = append(result, roomInfo{
			Room:         roomName,
			CreatedAt:    createdAt,
			PeerCount:    len(conns),
			DisplayNames: names,
		})
	}

	if result == nil {
		result = []roomInfo{}
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(map[string]any{"rooms": result})
}

type sessionJSON struct {
	ID        string                       `json:"id"`
	Room      string                       `json:"room"`
	StartedAt string                       `json:"started_at"`
	EndedAt   *string                      `json:"ended_at,omitempty"`
	Duration  string                       `json:"duration"`
	Phase     string                       `json:"phase"`
	Peers     []string                     `json:"peers"`
	Joining   map[string]*directionMetrics `json:"joining"`
	Playing   map[string]*directionMetrics `json:"playing"`
}

type metricsSnapshot struct {
	Active    []sessionJSON `json:"active"`
	Completed []sessionJSON `json:"completed"`
}

func sessionToJSON(s *session) sessionJSON {
	sj := sessionJSON{
		ID:        s.ID,
		Room:      s.Room,
		StartedAt: s.StartedAt.UTC().Format(time.RFC3339),
		Phase:     s.Phase,
		Peers:     s.Peers,
		Joining:   s.Joining,
		Playing:   s.Playing,
	}
	if s.EndedAt != nil {
		t := s.EndedAt.UTC().Format(time.RFC3339)
		sj.EndedAt = &t
		sj.Duration = s.EndedAt.Sub(s.StartedAt).Round(time.Second).String()
	} else {
		sj.Duration = time.Since(s.StartedAt).Round(time.Second).String()
	}
	return sj
}

// isPrivateRoom checks if a room is password-protected. Caller must hold h.mu.
func (h *hub) isPrivateRoom(room string) bool {
	var pwHash sql.NullString
	h.db.QueryRow("SELECT password_hash FROM rooms WHERE room = ?", room).Scan(&pwHash)
	return pwHash.Valid && pwHash.String != ""
}

// snapshotMetrics returns a point-in-time snapshot. Caller must hold h.mu.
// Sessions for password-protected rooms are excluded (consistent with /rooms).
func (h *hub) snapshotMetrics(roomFilter string) metricsSnapshot {
	var active []sessionJSON
	var completed []sessionJSON

	for room, s := range h.activeSessions {
		if roomFilter != "" && room != roomFilter {
			continue
		}
		if h.isPrivateRoom(room) {
			continue
		}
		active = append(active, sessionToJSON(s))
	}
	for room, sessions := range h.completedSessions {
		if roomFilter != "" && room != roomFilter {
			continue
		}
		if h.isPrivateRoom(room) {
			continue
		}
		for _, s := range sessions {
			completed = append(completed, sessionToJSON(s))
		}
	}

	sort.Slice(completed, func(i, j int) bool {
		return completed[i].StartedAt > completed[j].StartedAt
	})

	if active == nil {
		active = []sessionJSON{}
	}
	if completed == nil {
		completed = []sessionJSON{}
	}

	return metricsSnapshot{Active: active, Completed: completed}
}

func handleMetrics(h *hub, w http.ResponseWriter, r *http.Request) {
	h.mu.Lock()
	snap := h.snapshotMetrics(r.URL.Query().Get("room"))
	h.mu.Unlock()

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(snap)
}

func handleMetricsWS(h *hub, w http.ResponseWriter, r *http.Request) {
	ws, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		log.Printf("metrics ws upgrade: %v", err)
		return
	}
	defer ws.Close()

	roomFilter := r.URL.Query().Get("room")
	ticker := time.NewTicker(2 * time.Second)
	defer ticker.Stop()

	// Send initial snapshot immediately.
	h.mu.Lock()
	snap := h.snapshotMetrics(roomFilter)
	h.mu.Unlock()
	if err := ws.WriteJSON(snap); err != nil {
		return
	}

	// Read goroutine to detect client disconnect.
	done := make(chan struct{})
	go func() {
		defer close(done)
		for {
			if _, _, err := ws.ReadMessage(); err != nil {
				return
			}
		}
	}()

	for {
		select {
		case <-done:
			return
		case <-ticker.C:
			h.mu.Lock()
			snap := h.snapshotMetrics(roomFilter)
			h.mu.Unlock()
			ws.SetWriteDeadline(time.Now().Add(writeWait))
			if err := ws.WriteJSON(snap); err != nil {
				return
			}
		}
	}
}

func handleDashboard(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Write([]byte(dashboardHTML))
}

const dashboardHTML = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>WAIL Session Metrics</title>
<style>
  :root { --bg: #0d1117; --fg: #c9d1d9; --card: #161b22; --border: #30363d; --accent: #58a6ff; --green: #3fb950; --red: #f85149; --yellow: #d29922; }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif; background: var(--bg); color: var(--fg); padding: 20px; }
  h1 { color: var(--accent); margin-bottom: 4px; font-size: 1.4em; }
  .status { font-size: 0.85em; color: #8b949e; margin-bottom: 20px; }
  .status .dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; margin-right: 6px; }
  .dot.ok { background: var(--green); }
  .dot.err { background: var(--red); }
  .section-title { font-size: 1.1em; color: var(--accent); margin: 16px 0 8px; }
  .empty { color: #8b949e; font-style: italic; padding: 12px 0; }
  .session { background: var(--card); border: 1px solid var(--border); border-radius: 8px; padding: 16px; margin-bottom: 12px; }
  .session-header { display: flex; flex-wrap: wrap; gap: 16px; align-items: baseline; margin-bottom: 12px; }
  .session-header .room { font-weight: 600; font-size: 1.1em; }
  .badge { display: inline-block; padding: 2px 8px; border-radius: 12px; font-size: 0.8em; font-weight: 500; }
  .badge.joining { background: rgba(210,153,34,0.2); color: var(--yellow); }
  .badge.playing { background: rgba(63,185,80,0.2); color: var(--green); }
  .meta { font-size: 0.85em; color: #8b949e; }
  .peers { font-size: 0.85em; color: #8b949e; margin-bottom: 8px; }
  table { width: 100%; border-collapse: collapse; font-size: 0.85em; margin-top: 4px; }
  th { text-align: left; color: #8b949e; font-weight: 500; padding: 4px 8px; border-bottom: 1px solid var(--border); }
  td { padding: 4px 8px; border-bottom: 1px solid var(--border); }
  .phase-label { font-weight: 500; color: var(--fg); margin-top: 8px; margin-bottom: 2px; }
  .drop-ok { color: var(--green); }
  .drop-warn { color: var(--yellow); }
  .drop-bad { color: var(--red); }
  .no-data { color: #8b949e; font-size: 0.85em; }
</style>
</head>
<body>
<h1>WAIL Session Metrics</h1>
<div class="status" id="status"><span class="dot err"></span>Connecting...</div>

<div id="active-section">
  <div class="section-title">Active Sessions</div>
  <div id="active" class="empty">No active sessions</div>
</div>

<div id="completed-section">
  <div class="section-title">Completed Sessions</div>
  <div id="completed" class="empty">No completed sessions</div>
</div>

<script>
const statusEl = document.getElementById('status');
const activeEl = document.getElementById('active');
const completedEl = document.getElementById('completed');

function esc(s) { const d = document.createElement('div'); d.textContent = s; return d.innerHTML; }

function dropClass(expected, dropped) {
  if (expected === 0) return 'no-data';
  const pct = dropped / expected * 100;
  if (pct <= 1) return 'drop-ok';
  if (pct <= 5) return 'drop-warn';
  return 'drop-bad';
}

function fmtMs(us) { return us != null ? (us / 1000).toFixed(1) + 'ms' : '—'; }
function jitterClass(us) { if (us == null) return 'no-data'; if (us <= 20000) return 'drop-ok'; if (us <= 50000) return 'drop-warn'; return 'drop-bad'; }
function countClass(n) { return n > 0 ? 'drop-bad' : 'drop-ok'; }

function renderDirections(dirs) {
  if (!dirs || Object.keys(dirs).length === 0) return '<span class="no-data">No data yet</span>';
  let html = '<table><tr><th>Direction</th><th>Expected</th><th>Received</th><th>Dropped</th><th>Drop %</th><th>RTT</th><th>Jitter</th><th>DC Drops</th><th>Late</th><th>Decode Err</th></tr>';
  for (const [dir, m] of Object.entries(dirs)) {
    const pct = m.frames_expected > 0 ? (m.frames_dropped / m.frames_expected * 100).toFixed(1) : '—';
    const cls = dropClass(m.frames_expected, m.frames_dropped);
    html += '<tr><td>' + esc(dir) + '</td><td>' + m.frames_expected + '</td><td>' + m.frames_received +
            '</td><td class="' + cls + '">' + m.frames_dropped + '</td><td class="' + cls + '">' + pct + (m.frames_expected > 0 ? '%' : '') +
            '</td><td>' + fmtMs(m.rtt_us) +
            '</td><td class="' + jitterClass(m.jitter_us) + '">' + fmtMs(m.jitter_us) +
            '</td><td class="' + countClass(m.dc_drops || 0) + '">' + (m.dc_drops || 0) +
            '</td><td class="' + countClass(m.late_frames || 0) + '">' + (m.late_frames || 0) +
            '</td><td class="' + countClass(m.decode_failures || 0) + '">' + (m.decode_failures || 0) +
            '</td></tr>';
  }
  html += '</table>';
  return html;
}

function renderSession(s) {
  const phaseClass = s.phase === 'playing' ? 'playing' : 'joining';
  let html = '<div class="session">';
  html += '<div class="session-header"><span class="room">' + esc(s.room) + '</span>';
  html += '<span class="badge ' + phaseClass + '">' + esc(s.phase) + '</span>';
  html += '<span class="meta">' + esc(s.duration) + '</span>';
  if (s.ended_at) html += '<span class="meta">ended ' + esc(new Date(s.ended_at).toLocaleTimeString()) + '</span>';
  html += '</div>';
  html += '<div class="peers">Peers: ' + s.peers.map(esc).join(', ') + '</div>';
  html += '<div class="phase-label">Joining</div>' + renderDirections(s.joining);
  html += '<div class="phase-label">Playing</div>' + renderDirections(s.playing);
  html += '</div>';
  return html;
}

function render(data) {
  if (data.active && data.active.length > 0) {
    activeEl.innerHTML = data.active.map(renderSession).join('');
  } else {
    activeEl.innerHTML = '<div class="empty">No active sessions</div>';
  }
  if (data.completed && data.completed.length > 0) {
    completedEl.innerHTML = data.completed.map(renderSession).join('');
  } else {
    completedEl.innerHTML = '<div class="empty">No completed sessions</div>';
  }
}

function connect() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const params = new URLSearchParams(location.search);
  const room = params.get('room');
  let wsUrl = proto + '//' + location.host + '/metrics/ws';
  if (room) wsUrl += '?room=' + encodeURIComponent(room);
  const ws = new WebSocket(wsUrl);
  ws.onopen = () => {
    statusEl.innerHTML = '<span class="dot ok"></span>Connected — streaming every 2s';
  };
  ws.onmessage = (e) => {
    try { render(JSON.parse(e.data)); } catch(err) { console.error('parse error', err); }
  };
  ws.onclose = () => {
    statusEl.innerHTML = '<span class="dot err"></span>Disconnected — reconnecting...';
    setTimeout(connect, 3000);
  };
  ws.onerror = () => { ws.close(); };
}

connect();
</script>
</body>
</html>`

// ---------------------------------------------------------------------------
// Semver comparison
// ---------------------------------------------------------------------------

func semverLess(a, b string) bool {
	var a1, a2, a3, b1, b2, b3 int
	fmt.Sscanf(a, "%d.%d.%d", &a1, &a2, &a3)
	fmt.Sscanf(b, "%d.%d.%d", &b1, &b2, &b3)
	if a1 != b1 {
		return a1 < b1
	}
	if a2 != b2 {
		return a2 < b2
	}
	return a3 < b3
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

func main() {
	db := openDB()
	defer db.Close()

	h := newHub(db)

	http.HandleFunc("/ws", func(w http.ResponseWriter, r *http.Request) {
		handleWS(h, w, r)
	})
	http.HandleFunc("/rooms", func(w http.ResponseWriter, r *http.Request) {
		handleRooms(h, w, r)
	})
	http.HandleFunc("/metrics", func(w http.ResponseWriter, r *http.Request) {
		handleMetrics(h, w, r)
	})
	http.HandleFunc("/metrics/ws", func(w http.ResponseWriter, r *http.Request) {
		handleMetricsWS(h, w, r)
	})
	http.HandleFunc("/metrics/dashboard", func(w http.ResponseWriter, r *http.Request) {
		handleDashboard(w, r)
	})
	http.HandleFunc("/health", func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(200)
		w.Write([]byte("ok"))
	})

	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}

	log.Printf("WAIL signaling server listening on :%s", port)
	log.Fatal(http.ListenAndServe(":"+port, nil))
}
