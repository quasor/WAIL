package main

import (
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
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

// clientMsg is the envelope for all client->server messages.
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

// connEntry is one element in the copy-on-write connection slice.
type connEntry struct {
	peerID string
	c      *conn
}

// conn wraps a single WebSocket connection that has joined a room.
type conn struct {
	ws       *websocket.Conn
	room     string
	peerID   string
	send     chan wsMessage
	publicIP string // client's public IP for LAN detection
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
	lastPerPeer map[string]peerFrameReport
}

// session tracks aggregate metrics for a multi-peer session in a room.
type session struct {
	ID        string                              `json:"id"`
	Room      string                              `json:"room"`
	StartedAt time.Time                           `json:"started_at"`
	EndedAt   *time.Time                          `json:"ended_at,omitempty"`
	Phase     string                              `json:"phase"`
	Peers     []string                            `json:"peers"`
	Joining   map[string]*directionMetrics        `json:"joining"`
	Playing   map[string]*directionMetrics        `json:"playing"`
	peerState map[string]*peerStatus
	transitionSnapshot map[directionKey]peerFrameReport
}

func newSession(roomName string, peers []string) *session {
	id := fmt.Sprintf("%s-%d", roomName, time.Now().UnixMilli())
	s := &session{
		ID: id, Room: roomName, StartedAt: time.Now(), Phase: "joining",
		Peers: peers,
		Joining: make(map[string]*directionMetrics),
		Playing: make(map[string]*directionMetrics),
		peerState: make(map[string]*peerStatus),
		transitionSnapshot: make(map[directionKey]peerFrameReport),
	}
	for _, p := range peers {
		s.peerState[p] = &peerStatus{lastPerPeer: make(map[string]peerFrameReport)}
	}
	return s
}

func dirKey(from, to string) string { return from + "\xe2\x86\x92" + to }

func (s *session) updateMetrics(reporter string, dcOpen, pluginConnected bool, perPeer map[string]peerFrameReport) {
	ps, ok := s.peerState[reporter]
	if !ok {
		ps = &peerStatus{lastPerPeer: make(map[string]peerFrameReport)}
		s.peerState[reporter] = ps
	}
	ps.dcOpen = dcOpen
	ps.pluginConnected = pluginConnected

	target := s.Joining
	if s.Phase == "playing" {
		target = s.Playing
	}

	for remotePeer, report := range perPeer {
		dk := dirKey(remotePeer, reporter)

		if s.Phase == "playing" {
			snap, hasSnap := s.transitionSnapshot[directionKey{from: remotePeer, to: reporter}]
			if !hasSnap {
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
			dm.RttUs = report.RttUs
			dm.JitterUs = report.JitterUs
			dm.DcDrops = report.DcDrops - snap.DcDrops
			dm.LateFrames = report.LateFrames - snap.LateFrames
			dm.DecodeFailures = report.DecodeFailures - snap.DecodeFailures
		} else {
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
			for _, peer := range s.Peers {
				st := s.peerState[peer]
				if st == nil { continue }
				for remotePeer, report := range st.lastPerPeer {
					s.transitionSnapshot[directionKey{from: remotePeer, to: peer}] = report
				}
			}
		}
	}
}

func (s *session) addPeer(peerID string) {
	for _, p := range s.Peers {
		if p == peerID { return }
	}
	s.Peers = append(s.Peers, peerID)
	if _, ok := s.peerState[peerID]; !ok {
		s.peerState[peerID] = &peerStatus{lastPerPeer: make(map[string]peerFrameReport)}
	}
}

// ---------------------------------------------------------------------------
// Room - per-room state with copy-on-write connection list
// ---------------------------------------------------------------------------

type room struct {
	mu      sync.Mutex
	conns   atomic.Pointer[[]connEntry]
	connMap map[string]*conn
	activeSession     *session
	completedSessions []*session
}

func newRoom() *room {
	r := &room{connMap: make(map[string]*conn)}
	empty := make([]connEntry, 0)
	r.conns.Store(&empty)
	return r
}

func (r *room) rebuildConns() {
	snap := make([]connEntry, 0, len(r.connMap))
	for pid, c := range r.connMap {
		snap = append(snap, connEntry{peerID: pid, c: c})
	}
	r.conns.Store(&snap)
}

func (r *room) loadConns() []connEntry {
	p := r.conns.Load()
	if p == nil { return nil }
	return *p
}

// hub tracks all rooms.
type hub struct {
	mu    sync.RWMutex
	rooms map[string]*room
	db    *sql.DB
	completedMu       sync.Mutex
	completedSessions map[string][]*session
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

func openDB() *sql.DB {
	path := os.Getenv("DB_PATH")
	if path == "" { path = "/data/wail.db" }
	db, err := sql.Open("sqlite", path+"?_journal_mode=WAL&_busy_timeout=5000")
	if err != nil { log.Fatalf("open db: %v", err) }
	for _, stmt := range []string{
		`CREATE TABLE IF NOT EXISTS peers (
			room TEXT NOT NULL, peer_id TEXT NOT NULL, display_name TEXT,
			stream_count INTEGER DEFAULT 1, last_seen INTEGER NOT NULL,
			PRIMARY KEY (room, peer_id))`,
		`CREATE TABLE IF NOT EXISTS rooms (
			room TEXT PRIMARY KEY, password_hash TEXT,
			created_at INTEGER NOT NULL DEFAULT 0)`,
	} {
		if _, err := db.Exec(stmt); err != nil { log.Fatalf("migrate: %v", err) }
	}
	cutoff := time.Now().Unix() - stalePeerSec
	db.Exec("DELETE FROM peers WHERE last_seen < ?", cutoff)
	db.Exec("DELETE FROM rooms WHERE room NOT IN (SELECT DISTINCT room FROM peers)")
	return db
}

func hashPassword(pw string) string {
	h := sha256.Sum256([]byte(pw))
	return hex.EncodeToString(h[:])
}

// ---------------------------------------------------------------------------
// Hub helpers
// ---------------------------------------------------------------------------

func newHub(db *sql.DB) *hub {
	return &hub{
		rooms: make(map[string]*room), db: db,
		completedSessions: make(map[string][]*session),
	}
}

func (h *hub) getOrCreateRoom(name string) *room {
	h.mu.RLock()
	r, ok := h.rooms[name]
	h.mu.RUnlock()
	if ok { return r }
	h.mu.Lock()
	defer h.mu.Unlock()
	r, ok = h.rooms[name]
	if ok { return r }
	r = newRoom()
	h.rooms[name] = r
	return r
}

func (h *hub) getRoom(name string) *room {
	h.mu.RLock()
	r := h.rooms[name]
	h.mu.RUnlock()
	return r
}

func (h *hub) deleteRoom(name string) {
	h.mu.Lock()
	defer h.mu.Unlock()
	r, ok := h.rooms[name]
	if !ok { return }
	r.mu.Lock()
	empty := len(r.connMap) == 0
	r.mu.Unlock()
	if empty { delete(h.rooms, name) }
}

func (h *hub) archiveSession(roomName string, s *session) {
	h.completedMu.Lock()
	defer h.completedMu.Unlock()
	h.completedSessions[roomName] = append(h.completedSessions[roomName], s)
	if len(h.completedSessions[roomName]) > maxCompletedSessions {
		h.completedSessions[roomName] = h.completedSessions[roomName][len(h.completedSessions[roomName])-maxCompletedSessions:]
	}
}

// ---------------------------------------------------------------------------
// Hub methods
// ---------------------------------------------------------------------------

func (h *hub) join(c *conn, msg clientMsg) (string, string) {

	roomName := msg.Room
	peerID := msg.PeerID
	streamCount := msg.StreamCount
	if streamCount < 1 { streamCount = 1 }

	if semverLess(msg.ClientVersion, minVersion) {
		c.sendJSON(map[string]any{"type": "join_error", "code": "version_outdated", "min_version": minVersion})
		return "", ""
	}

	// DB queries BEFORE any room lock
	var storedHash sql.NullString
	var roomExists bool
	if err := h.db.QueryRow("SELECT password_hash FROM rooms WHERE room = ?", roomName).Scan(&storedHash); err == nil {
		roomExists = true
	}
	if roomExists && storedHash.Valid && storedHash.String != "" {
		if hashPassword(msg.Password) != storedHash.String {
			c.sendJSON(map[string]any{"type": "join_error", "code": "unauthorized"})
			return "", ""
		}
	}

	const roomCapacity = 32
	var usedSlots int
	h.db.QueryRow("SELECT COALESCE(SUM(stream_count), 0) FROM peers WHERE room = ?", roomName).Scan(&usedSlots)
	if usedSlots+streamCount > roomCapacity {
		c.sendJSON(map[string]any{"type": "join_error", "code": "room_full", "slots_available": roomCapacity - usedSlots})
		return "", ""
	}

	if !roomExists {
		pwHash := ""
		if msg.Password != "" { pwHash = hashPassword(msg.Password) }
		h.db.Exec("INSERT OR IGNORE INTO rooms (room, password_hash, created_at) VALUES (?, ?, ?)", roomName, pwHash, time.Now().Unix())
	}

	displayName := ""
	if msg.DisplayName != nil { displayName = *msg.DisplayName }
	h.db.Exec(`INSERT INTO peers (room, peer_id, display_name, stream_count, last_seen) VALUES (?, ?, ?, ?, ?)
		ON CONFLICT(room, peer_id) DO UPDATE SET display_name=excluded.display_name, stream_count=excluded.stream_count, last_seen=excluded.last_seen`,
		roomName, peerID, displayName, streamCount, time.Now().Unix())

	// Per-room lock for mutation
	r := h.getOrCreateRoom(roomName)
	r.mu.Lock()

	if old, exists := r.connMap[peerID]; exists && old != c {
		log.Printf("peer %s reconnecting in room %s - evicting old connection", peerID, roomName)
		delete(r.connMap, peerID)
		old.room = ""
		old.peerID = ""
		close(old.send)
	}

	peers := []string{}
	peerDisplayNames := map[string]*string{}
	for id, rc := range r.connMap {
		if id != peerID {
			peers = append(peers, id)
			var dn sql.NullString
			h.db.QueryRow("SELECT display_name FROM peers WHERE room = ? AND peer_id = ?", roomName, id).Scan(&dn)
			if dn.Valid && dn.String != "" {
				name := dn.String
				peerDisplayNames[id] = &name
			} else {
				peerDisplayNames[id] = nil
			}
			rc.sendJSON(map[string]any{"type": "peer_joined", "peer_id": peerID, "display_name": msg.DisplayName})
		}
	}

	r.connMap[peerID] = c
	c.room = roomName
	c.peerID = peerID
	r.rebuildConns()

	peerCountAfter := len(r.connMap)
	if peerCountAfter >= 2 {
		if s := r.activeSession; s != nil {
			s.addPeer(peerID)
			log.Printf("[metrics] peer %s joined active session %s (now %d peers)", peerID, s.ID, peerCountAfter)
		} else {
			allPeers := make([]string, 0, peerCountAfter)
			for pid := range r.connMap { allPeers = append(allPeers, pid) }
			s := newSession(roomName, allPeers)
			r.activeSession = s
			log.Printf("[metrics] session %s started with %d peers", s.ID, peerCountAfter)
		}
	}

	// LAN peer detection via public IP
	lanPeerPresent := false
	for id, rc := range r.connMap {
		if id != peerID && rc.publicIP == c.publicIP {
			lanPeerPresent = true
			ipPrefix := c.publicIP
			if len(ipPrefix) > 8 { ipPrefix = ipPrefix[:8] }
			log.Printf("peer %s shares LAN with existing peer %s (IP prefix: %s...)", peerID, id, ipPrefix)
			break
		}
	}

	r.mu.Unlock()

	c.sendJSON(map[string]any{
		"type": "join_ok", "peers": peers, "peer_display_names": peerDisplayNames,
		"lan_peer_present": lanPeerPresent,
	})
	return roomName, peerID
}

func (h *hub) signal(room, peerID string, c *conn, msg clientMsg) {
	if room == "" { return }
	r := h.getRoom(room)
	if r == nil { return }
	for _, e := range r.loadConns() {
		if e.peerID == msg.To {
			e.c.sendJSON(map[string]any{"type": "signal", "to": msg.To, "from": peerID, "payload": msg.Payload})
			return
		}
	}
}

func (h *hub) broadcastSync(room, peerID string, c *conn, msg clientMsg) {
	if room == "" { return }
	r := h.getRoom(room)
	if r == nil { return }
	raw, err := json.Marshal(map[string]any{"type": "sync", "from": peerID, "payload": msg.Payload})
	if err != nil { return }
	wsMsg := wsMessage{websocket.TextMessage, raw}
	for _, e := range r.loadConns() {
		if e.peerID != peerID { e.c.sendWS(wsMsg) }
	}
}

func (h *hub) syncTo(room, peerID string, c *conn, msg clientMsg) {
	if room == "" { return }
	r := h.getRoom(room)
	if r == nil { return }
	for _, e := range r.loadConns() {
		if e.peerID == msg.To {
			e.c.sendJSON(map[string]any{"type": "sync", "from": peerID, "payload": msg.Payload})
			return
		}
	}
}

// broadcastAudioBinary is the hot path (~50 calls/sec/peer). No locks held during iteration.
// room and peerID are passed as value parameters captured by readPump at join time,
// avoiding unsynchronized reads of c.room/c.peerID (which may be cleared by eviction/leave).
func (h *hub) broadcastAudioBinary(room, peerID string, c *conn, data []byte) {
	if room == "" { return }
	r := h.getRoom(room)
	if r == nil { return }

	pidBytes := []byte(peerID)
	frame := make([]byte, 1+len(pidBytes)+len(data))
	frame[0] = byte(len(pidBytes))
	copy(frame[1:1+len(pidBytes)], pidBytes)
	copy(frame[1+len(pidBytes):], data)

	wsMsg := wsMessage{websocket.BinaryMessage, frame}
	for _, e := range r.loadConns() {
		if e.peerID != peerID { e.c.sendWS(wsMsg) }
	}
}

func (h *hub) broadcastLog(room, peerID string, c *conn, msg clientMsg) {
	if room == "" { return }
	r := h.getRoom(room)
	if r == nil { return }
	raw, err := json.Marshal(map[string]any{
		"type": "log", "from": peerID, "level": msg.Level,
		"target": msg.Target, "message": msg.Message, "timestamp_us": msg.TimestampUs,
	})
	if err != nil { return }
	wsMsg := wsMessage{websocket.TextMessage, raw}
	for _, e := range r.loadConns() {
		if e.peerID != peerID { e.c.sendWS(wsMsg) }
	}
}

func (h *hub) metricsReport(room, peerID string, c *conn, msg clientMsg) {
	if room == "" { return }
	r := h.getRoom(room)
	if r == nil { return }
	r.mu.Lock()
	defer r.mu.Unlock()
	s := r.activeSession
	if s == nil { return }
	dcOpen := false
	if msg.DcOpen != nil { dcOpen = *msg.DcOpen }
	pluginConnected := false
	if msg.PluginConnected != nil { pluginConnected = *msg.PluginConnected }
	s.updateMetrics(peerID, dcOpen, pluginConnected, msg.PerPeer)
}

func (h *hub) leave(room, peerID string, c *conn) {
	if room == "" { return }
	r := h.getRoom(room)
	if r == nil {
		c.room = ""
		c.peerID = ""
		return
	}

	r.mu.Lock()
	if cur, ok := r.connMap[peerID]; ok && cur == c {
		delete(r.connMap, peerID)
		r.rebuildConns()
		for _, e := range r.loadConns() {
			e.c.sendJSON(map[string]any{"type": "peer_left", "peer_id": peerID})
		}
		peerCountAfter := len(r.connMap)
		if peerCountAfter < 2 {
			if s := r.activeSession; s != nil {
				now := time.Now()
				s.EndedAt = &now
				log.Printf("[metrics] session %s ended (peer %s left, %d remaining)", s.ID, peerID, peerCountAfter)
				r.activeSession = nil
				r.mu.Unlock()
				h.archiveSession(room, s)
			} else {
				r.mu.Unlock()
			}
		} else {
			r.mu.Unlock()
		}
	} else {
		r.mu.Unlock()
	}

	h.db.Exec("DELETE FROM peers WHERE room = ? AND peer_id = ?", room, peerID)
	c.room = ""
	c.peerID = ""

	r.mu.Lock()
	empty := len(r.connMap) == 0
	r.mu.Unlock()
	if empty {
		h.deleteRoom(room)
		h.db.Exec("DELETE FROM rooms WHERE room = ?", room)
	}
}

// ---------------------------------------------------------------------------
// Connection helpers
// ---------------------------------------------------------------------------

func (c *conn) sendJSON(v any) {
	raw, err := json.Marshal(v)
	if err != nil {
		log.Printf("warn: sendJSON marshal error for peer %s: %v", c.peerID, err)
		return
	}
	c.sendWS(wsMessage{websocket.TextMessage, raw})
}

func (c *conn) sendWS(msg wsMessage) {
	defer func() { recover() }()
	select {
	case c.send <- msg:
	default:
		log.Printf("warn: dropped message to peer %s (send buffer full)", c.peerID)
	}
}

func (c *conn) sendBinary(data []byte) {
	c.sendWS(wsMessage{websocket.BinaryMessage, data})
}

func (c *conn) writePump() {
	ticker := time.NewTicker(pingInterval)
	defer func() { ticker.Stop(); c.ws.Close() }()
	for {
		select {
		case msg, ok := <-c.send:
			c.ws.SetWriteDeadline(time.Now().Add(writeWait))
			if !ok {
				c.ws.WriteMessage(websocket.CloseMessage, []byte{})
				return
			}
			if err := c.ws.WriteMessage(msg.msgType, msg.data); err != nil { return }
		case <-ticker.C:
			c.ws.SetWriteDeadline(time.Now().Add(writeWait))
			if err := c.ws.WriteMessage(websocket.PingMessage, nil); err != nil { return }
		}
	}
}

func (c *conn) readPump(h *hub) {
	// room and peerID are cached locally after join so that broadcast functions
	// never read c.room/c.peerID directly — avoiding a data race with the
	// eviction path in join() and the cleanup in leave().
	var room, peerID string
	defer func() {
		h.leave(room, peerID, c)
		func() { defer func() { recover() }(); close(c.send) }()
		c.ws.Close()
	}()
	c.ws.SetReadLimit(maxMessageSize)
	c.ws.SetReadDeadline(time.Now().Add(pongWait))
	c.ws.SetPongHandler(func(string) error {
		c.ws.SetReadDeadline(time.Now().Add(pongWait))
		if room != "" {
			h.db.Exec("UPDATE peers SET last_seen = ? WHERE room = ? AND peer_id = ?", time.Now().Unix(), room, peerID)
		}
		return nil
	})
	for {
		msgType, raw, err := c.ws.ReadMessage()
		if err != nil { return }
		if msgType == websocket.BinaryMessage {
			h.broadcastAudioBinary(room, peerID, c, raw)
			continue
		}
		var msg clientMsg
		if err := json.Unmarshal(raw, &msg); err != nil { continue }
		switch msg.Type {
		case "join":
			if room != "" { h.leave(room, peerID, c) }
			room, peerID = h.join(c, msg)
		case "signal":
			h.signal(room, peerID, c, msg)
		case "sync":
			h.broadcastSync(room, peerID, c, msg)
		case "sync_to":
			h.syncTo(room, peerID, c, msg)
		case "log":
			h.broadcastLog(room, peerID, c, msg)
		case "leave":
			h.leave(room, peerID, c)
			room, peerID = "", ""
		case "metrics_report":
			h.metricsReport(room, peerID, c, msg)
		}
	}
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

var upgrader = websocket.Upgrader{CheckOrigin: func(r *http.Request) bool { return true }}

func clientIP(r *http.Request) string {
	if ip := r.Header.Get("Fly-Client-IP"); ip != "" { return ip }
	if xff := r.Header.Get("X-Forwarded-For"); xff != "" {
		return strings.TrimSpace(strings.Split(xff, ",")[0])
	}
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err != nil { return r.RemoteAddr }
	return host
}

func handleWS(h *hub, w http.ResponseWriter, r *http.Request) {
	ws, err := upgrader.Upgrade(w, r, nil)
	if err != nil { log.Printf("upgrade: %v", err); return }
	c := &conn{ws: ws, send: make(chan wsMessage, 256), publicIP: clientIP(r)}
	go c.writePump()
	c.readPump(h)
}

func handleRooms(h *hub, w http.ResponseWriter, r *http.Request) {
	type roomInfo struct {
		Room string `json:"room"`; CreatedAt int64 `json:"created_at"`
		PeerCount int `json:"peer_count"`; DisplayNames []string `json:"display_names"`
	}
	h.mu.RLock()
	roomNames := make([]string, 0, len(h.rooms))
	roomSnaps := make(map[string][]connEntry, len(h.rooms))
	for name, r := range h.rooms {
		roomNames = append(roomNames, name)
		roomSnaps[name] = r.loadConns()
	}
	h.mu.RUnlock()

	var result []roomInfo
	for _, roomName := range roomNames {
		var pwHash sql.NullString
		h.db.QueryRow("SELECT password_hash FROM rooms WHERE room = ?", roomName).Scan(&pwHash)
		if pwHash.Valid && pwHash.String != "" { continue }
		var createdAt int64
		h.db.QueryRow("SELECT created_at FROM rooms WHERE room = ?", roomName).Scan(&createdAt)
		conns := roomSnaps[roomName]
		names := []string{}
		for _, e := range conns {
			var dn sql.NullString
			h.db.QueryRow("SELECT display_name FROM peers WHERE room = ? AND peer_id = ?", roomName, e.peerID).Scan(&dn)
			if dn.Valid && dn.String != "" { names = append(names, dn.String) }
		}
		result = append(result, roomInfo{Room: roomName, CreatedAt: createdAt, PeerCount: len(conns), DisplayNames: names})
	}
	if result == nil { result = []roomInfo{} }
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(map[string]any{"rooms": result})
}

type sessionJSON struct {
	ID string `json:"id"`; Room string `json:"room"`; StartedAt string `json:"started_at"`
	EndedAt *string `json:"ended_at,omitempty"`; Duration string `json:"duration"`
	Phase string `json:"phase"`; Peers []string `json:"peers"`
	PeerDisplayNames map[string]string `json:"peer_display_names,omitempty"`
	Joining map[string]*directionMetrics `json:"joining"`
	Playing map[string]*directionMetrics `json:"playing"`
}
type metricsSnapshot struct {
	Active []sessionJSON `json:"active"`; Completed []sessionJSON `json:"completed"`
}

func sessionToJSON(s *session) sessionJSON {
	sj := sessionJSON{ID: s.ID, Room: s.Room, StartedAt: s.StartedAt.UTC().Format(time.RFC3339),
		Phase: s.Phase, Peers: s.Peers, Joining: s.Joining, Playing: s.Playing}
	if s.EndedAt != nil {
		t := s.EndedAt.UTC().Format(time.RFC3339); sj.EndedAt = &t
		sj.Duration = s.EndedAt.Sub(s.StartedAt).Round(time.Second).String()
	} else {
		sj.Duration = time.Since(s.StartedAt).Round(time.Second).String()
	}
	return sj
}

func (h *hub) lookupDisplayNames(sj *sessionJSON) {
	names := make(map[string]string)
	for _, pid := range sj.Peers {
		var dn sql.NullString
		h.db.QueryRow("SELECT display_name FROM peers WHERE room = ? AND peer_id = ?", sj.Room, pid).Scan(&dn)
		if dn.Valid && dn.String != "" {
			names[pid] = dn.String
		}
	}
	if len(names) > 0 {
		sj.PeerDisplayNames = names
	}
}

func (h *hub) snapshotMetrics(roomFilter string) metricsSnapshot {
	var active, completed []sessionJSON
	h.mu.RLock()
	roomNames := make([]string, 0, len(h.rooms))
	roomPtrs := make([]*room, 0, len(h.rooms))
	for name, r := range h.rooms { roomNames = append(roomNames, name); roomPtrs = append(roomPtrs, r) }
	h.mu.RUnlock()
	for i, name := range roomNames {
		if roomFilter != "" && name != roomFilter { continue }
		r := roomPtrs[i]
		r.mu.Lock()
		if s := r.activeSession; s != nil {
			sj := sessionToJSON(s)
			h.lookupDisplayNames(&sj)
			active = append(active, sj)
		}
		r.mu.Unlock()
	}
	h.completedMu.Lock()
	for rn, sessions := range h.completedSessions {
		if roomFilter != "" && rn != roomFilter { continue }
		for _, s := range sessions {
			sj := sessionToJSON(s)
			h.lookupDisplayNames(&sj)
			completed = append(completed, sj)
		}
	}
	h.completedMu.Unlock()
	sort.Slice(completed, func(i, j int) bool { return completed[i].StartedAt > completed[j].StartedAt })
	if active == nil { active = []sessionJSON{} }
	if completed == nil { completed = []sessionJSON{} }
	return metricsSnapshot{Active: active, Completed: completed}
}

func handleMetrics(h *hub, w http.ResponseWriter, r *http.Request) {
	snap := h.snapshotMetrics(r.URL.Query().Get("room"))
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(snap)
}

func handleMetricsWS(h *hub, w http.ResponseWriter, r *http.Request) {
	ws, err := upgrader.Upgrade(w, r, nil)
	if err != nil { log.Printf("metrics ws upgrade: %v", err); return }
	defer ws.Close()
	roomFilter := r.URL.Query().Get("room")
	ticker := time.NewTicker(2 * time.Second)
	defer ticker.Stop()
	if err := ws.WriteJSON(h.snapshotMetrics(roomFilter)); err != nil { return }
	done := make(chan struct{})
	go func() { defer close(done); for { if _, _, err := ws.ReadMessage(); err != nil { return } } }()
	for {
		select {
		case <-done: return
		case <-ticker.C:
			ws.SetWriteDeadline(time.Now().Add(writeWait))
			if err := ws.WriteJSON(h.snapshotMetrics(roomFilter)); err != nil { return }
		}
	}
}

func handleDashboard(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Write([]byte(dashboardHTML))
}

const dashboardHTML = `<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>WAIL Session Metrics</title>
<style>
  :root { --bg: #0d1117; --fg: #c9d1d9; --card: #161b22; --border: #30363d; --accent: #58a6ff; --green: #3fb950; --red: #f85149; --yellow: #d29922; }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif; background: var(--bg); color: var(--fg); padding: 20px; }
  h1 { color: var(--accent); margin-bottom: 4px; font-size: 1.4em; }
  .status { font-size: 0.85em; color: #8b949e; margin-bottom: 20px; }
  .status .dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; margin-right: 6px; }
  .dot.ok { background: var(--green); } .dot.err { background: var(--red); }
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
  .drop-ok { color: var(--green); } .drop-warn { color: var(--yellow); } .drop-bad { color: var(--red); }
  .no-data { color: #8b949e; font-size: 0.85em; }
  .header-row { display: flex; align-items: baseline; gap: 16px; margin-bottom: 4px; }
  .toggle { background: var(--card); border: 1px solid var(--border); color: var(--fg); padding: 4px 10px; border-radius: 6px; font-size: 0.8em; cursor: pointer; }
  .toggle:hover { border-color: var(--accent); }
  .toggle.active { background: rgba(88,166,255,0.15); border-color: var(--accent); color: var(--accent); }
</style></head><body>
<div class="header-row"><h1>WAIL Session Metrics</h1><button class="toggle" id="name-toggle" onclick="toggleNames()">Show Names</button></div>
<div class="status" id="status"><span class="dot err"></span>Connecting...</div>
<div id="active-section"><div class="section-title">Active Sessions</div><div id="active" class="empty">No active sessions</div></div>
<div id="completed-section"><div class="section-title">Completed Sessions</div><div id="completed" class="empty">No completed sessions</div></div>
<script>
const statusEl=document.getElementById('status'),activeEl=document.getElementById('active'),completedEl=document.getElementById('completed');
let showNames=false;let lastData=null;
function toggleNames(){showNames=!showNames;document.getElementById('name-toggle').className='toggle'+(showNames?' active':'');document.getElementById('name-toggle').textContent=showNames?'Show IDs':'Show Names';if(lastData)render(lastData)}
function esc(s){const d=document.createElement('div');d.textContent=s;return d.innerHTML}
function peerLabel(id,names){if(showNames&&names&&names[id])return names[id];return id}
function dirLabel(dir,names){if(!showNames||!names)return dir;return dir.split('\u2192').map(function(id){return names[id.trim()]||id.trim()}).join('\u2192')}
function dropClass(e,d){if(e===0)return'no-data';const p=d/e*100;return p<=1?'drop-ok':p<=5?'drop-warn':'drop-bad'}
function fmtMs(us){return us!=null?(us/1000).toFixed(1)+'ms':'\u2014'}
function jitterClass(us){if(us==null)return'no-data';return us<=20000?'drop-ok':us<=50000?'drop-warn':'drop-bad'}
function countClass(n){return n>0?'drop-bad':'drop-ok'}
function renderDirections(dirs,names){
  if(!dirs||Object.keys(dirs).length===0)return'<span class="no-data">No data yet</span>';
  let h='<table><tr><th>Direction</th><th>Expected</th><th>Received</th><th>Dropped</th><th>Drop %</th><th>RTT</th><th>Jitter</th><th>DC Drops</th><th>Late</th><th>Decode Err</th></tr>';
  for(const[dir,m]of Object.entries(dirs)){const p=m.frames_expected>0?(m.frames_dropped/m.frames_expected*100).toFixed(1):'\u2014';const c=dropClass(m.frames_expected,m.frames_dropped);
    h+='<tr><td>'+esc(dirLabel(dir,names))+'</td><td>'+m.frames_expected+'</td><td>'+m.frames_received+'</td><td class="'+c+'">'+m.frames_dropped+'</td><td class="'+c+'">'+p+(m.frames_expected>0?'%':'')+'</td><td>'+fmtMs(m.rtt_us)+'</td><td class="'+jitterClass(m.jitter_us)+'">'+fmtMs(m.jitter_us)+'</td><td class="'+countClass(m.dc_drops||0)+'">'+(m.dc_drops||0)+'</td><td class="'+countClass(m.late_frames||0)+'">'+(m.late_frames||0)+'</td><td class="'+countClass(m.decode_failures||0)+'">'+(m.decode_failures||0)+'</td></tr>'}
  return h+'</table>'}
function renderSession(s){const pc=s.phase==='playing'?'playing':'joining';const names=s.peer_display_names;let h='<div class="session"><div class="session-header"><span class="room">'+esc(s.room)+'</span><span class="badge '+pc+'">'+esc(s.phase)+'</span><span class="meta">'+esc(s.duration)+'</span>';
  if(s.ended_at)h+='<span class="meta">ended '+esc(new Date(s.ended_at).toLocaleTimeString())+'</span>';
  h+='</div><div class="peers">Peers: '+s.peers.map(function(p){return esc(peerLabel(p,names))}).join(', ')+'</div><div class="phase-label">Joining</div>'+renderDirections(s.joining,names)+'<div class="phase-label">Playing</div>'+renderDirections(s.playing,names)+'</div>';return h}
function render(data){lastData=data;
  activeEl.innerHTML=data.active&&data.active.length>0?data.active.map(renderSession).join(''):'<div class="empty">No active sessions</div>';
  completedEl.innerHTML=data.completed&&data.completed.length>0?data.completed.map(renderSession).join(''):'<div class="empty">No completed sessions</div>'}
function connect(){const proto=location.protocol==='https:'?'wss:':'ws:';const params=new URLSearchParams(location.search);const room=params.get('room');
  let wsUrl=proto+'//'+location.host+'/metrics/ws';if(room)wsUrl+='?room='+encodeURIComponent(room);
  const ws=new WebSocket(wsUrl);
  ws.onopen=()=>{statusEl.innerHTML='<span class="dot ok"></span>Connected \u2014 streaming every 2s'};
  ws.onmessage=(e)=>{try{render(JSON.parse(e.data))}catch(err){console.error('parse error',err)}};
  ws.onclose=()=>{statusEl.innerHTML='<span class="dot err"></span>Disconnected \u2014 reconnecting...';setTimeout(connect,3000)};
  ws.onerror=()=>{ws.close()}}
connect();
</script></body></html>`

// ---------------------------------------------------------------------------
// Semver comparison
// ---------------------------------------------------------------------------

func semverLess(a, b string) bool {
	var a1, a2, a3, b1, b2, b3 int
	fmt.Sscanf(a, "%d.%d.%d", &a1, &a2, &a3)
	fmt.Sscanf(b, "%d.%d.%d", &b1, &b2, &b3)
	if a1 != b1 { return a1 < b1 }
	if a2 != b2 { return a2 < b2 }
	return a3 < b3
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

func main() {
	db := openDB()
	defer db.Close()
	h := newHub(db)
	http.HandleFunc("/ws", func(w http.ResponseWriter, r *http.Request) { handleWS(h, w, r) })
	http.HandleFunc("/rooms", func(w http.ResponseWriter, r *http.Request) { handleRooms(h, w, r) })
	http.HandleFunc("/metrics", func(w http.ResponseWriter, r *http.Request) { handleMetrics(h, w, r) })
	http.HandleFunc("/metrics/ws", func(w http.ResponseWriter, r *http.Request) { handleMetricsWS(h, w, r) })
	http.HandleFunc("/metrics/dashboard", func(w http.ResponseWriter, r *http.Request) { handleDashboard(w, r) })
	http.HandleFunc("/health", func(w http.ResponseWriter, r *http.Request) { w.WriteHeader(200); w.Write([]byte("ok")) })
	port := os.Getenv("PORT")
	if port == "" { port = "8080" }
	log.Printf("WAIL signaling server listening on :%s", port)
	log.Fatal(http.ListenAndServe(":"+port, nil))
}
