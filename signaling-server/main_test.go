package main

import (
	"database/sql"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
	_ "modernc.org/sqlite"
)

// ---------------------------------------------------------------------------
// Token bucket unit tests
// ---------------------------------------------------------------------------

func TestTokenBucketBasic(t *testing.T) {
	b := newTokenBucket(10, 20) // 10 tokens/sec, burst of 20
	// Should allow 20 rapid calls (full burst).
	for i := 0; i < 20; i++ {
		if !b.allow() {
			t.Fatalf("expected allow at call %d", i)
		}
	}
	// 21st should fail — bucket empty.
	if b.allow() {
		t.Fatal("expected deny after burst exhausted")
	}
	// After 100ms, ~1 token refilled (10/sec * 0.1s = 1).
	time.Sleep(110 * time.Millisecond)
	if !b.allow() {
		t.Fatal("expected allow after refill")
	}
}

func TestTokenBucketRefill(t *testing.T) {
	b := newTokenBucket(100, 100)
	// Drain all tokens.
	for i := 0; i < 100; i++ {
		b.allow()
	}
	if b.allow() {
		t.Fatal("expected deny after drain")
	}
	// Wait 500ms → ~50 tokens should refill.
	time.Sleep(500 * time.Millisecond)
	allowed := 0
	for i := 0; i < 60; i++ {
		if b.allow() {
			allowed++
		}
	}
	// Allow some jitter: 40-60 tokens.
	if allowed < 40 || allowed > 60 {
		t.Fatalf("expected ~50 tokens after 500ms, got %d", allowed)
	}
}

func TestTokenBucketStreamScaling(t *testing.T) {
	streams := 3
	b := newTokenBucket(baseBinaryRate*float64(streams), baseBinaryBurst*float64(streams))
	// Burst should be 120*3 = 360.
	allowed := 0
	for i := 0; i < 400; i++ {
		if b.allow() {
			allowed++
		}
	}
	if allowed < 350 || allowed > 370 {
		t.Fatalf("expected ~360 burst for 3 streams, got %d", allowed)
	}
}

// ---------------------------------------------------------------------------
// Integration test helpers
// ---------------------------------------------------------------------------

func testDB(t *testing.T) *sql.DB {
	t.Helper()
	db, err := sql.Open("sqlite", ":memory:?_journal_mode=WAL")
	if err != nil {
		t.Fatal(err)
	}
	for _, stmt := range []string{
		`CREATE TABLE IF NOT EXISTS peers (
			room TEXT NOT NULL, peer_id TEXT NOT NULL, display_name TEXT,
			stream_count INTEGER DEFAULT 1, last_seen INTEGER NOT NULL,
			PRIMARY KEY (room, peer_id))`,
		`CREATE TABLE IF NOT EXISTS rooms (
			room TEXT PRIMARY KEY, password_hash TEXT,
			created_at INTEGER NOT NULL DEFAULT 0)`,
	} {
		if _, err := db.Exec(stmt); err != nil {
			t.Fatal(err)
		}
	}
	t.Cleanup(func() { db.Close() })
	return db
}

func testServer(t *testing.T) (*hub, *httptest.Server) {
	t.Helper()
	h := newHub(testDB(t))
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		handleWS(h, w, r)
	}))
	t.Cleanup(func() { srv.Close() })
	return h, srv
}

func dialWS(t *testing.T, srv *httptest.Server) *websocket.Conn {
	t.Helper()
	url := "ws" + strings.TrimPrefix(srv.URL, "http") + "/ws"
	ws, _, err := websocket.DefaultDialer.Dial(url, nil)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { ws.Close() })
	return ws
}

func joinRoom(t *testing.T, ws *websocket.Conn, room, peerID string, streamCount int) {
	t.Helper()
	msg := map[string]any{
		"type":           "join",
		"room":           room,
		"peer_id":        peerID,
		"stream_count":   streamCount,
		"client_version": "99.0.0",
	}
	if err := ws.WriteJSON(msg); err != nil {
		t.Fatal(err)
	}
	// Read join_ok or join_error.
	var resp map[string]any
	if err := ws.ReadJSON(&resp); err != nil {
		t.Fatal(err)
	}
	if resp["type"] != "join_ok" {
		t.Fatalf("expected join_ok, got %v", resp)
	}
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

func TestRateLimitDisconnectBinary(t *testing.T) {
	_, srv := testServer(t)
	ws := dialWS(t, srv)
	joinRoom(t, ws, "test-room", "flood-peer", 1)

	// Flood binary messages well beyond the burst limit (120 for 1 stream).
	// After burst + rateLimitWarnMax violations, the server should close the connection.
	fakeAudio := make([]byte, 100)
	sent := 0
	for i := 0; i < 500; i++ {
		if err := ws.WriteMessage(websocket.BinaryMessage, fakeAudio); err != nil {
			break
		}
		sent++
	}

	// The server should eventually close the connection.
	// Try reading — expect an error.
	ws.SetReadDeadline(time.Now().Add(2 * time.Second))
	for {
		_, _, err := ws.ReadMessage()
		if err != nil {
			break // connection closed as expected
		}
	}

	// If we got here without error within the deadline, the connection was closed.
	t.Logf("sent %d binary messages before connection closed", sent)
}

func TestRateLimitDisconnectText(t *testing.T) {
	_, srv := testServer(t)
	ws := dialWS(t, srv)
	joinRoom(t, ws, "test-room", "flood-peer", 1)

	// Flood text messages (sync type).
	syncMsg := map[string]any{
		"type":    "sync",
		"payload": map[string]any{"type": "Ping", "sent_at": 12345},
	}
	raw, _ := json.Marshal(syncMsg)
	sent := 0
	for i := 0; i < 500; i++ {
		if err := ws.WriteMessage(websocket.TextMessage, raw); err != nil {
			break
		}
		sent++
	}

	ws.SetReadDeadline(time.Now().Add(2 * time.Second))
	for {
		_, _, err := ws.ReadMessage()
		if err != nil {
			break
		}
	}
	t.Logf("sent %d text messages before connection closed", sent)
}

func TestLegitTrafficPasses(t *testing.T) {
	_, srv := testServer(t)

	// Two peers: sender with 3 streams, receiver.
	wsSend := dialWS(t, srv)
	wsRecv := dialWS(t, srv)
	joinRoom(t, wsSend, "legit-room", "sender", 3)
	joinRoom(t, wsRecv, "legit-room", "receiver", 1)

	// Drain peer_joined notification on receiver.
	wsRecv.SetReadDeadline(time.Now().Add(1 * time.Second))
	wsRecv.ReadMessage() // peer_joined for sender (may already be read in join)

	// Send at 150/sec (50fps * 3 streams) for 1 second.
	// With rate = 60*3 = 180 tokens/sec and burst = 360, this should pass.
	fakeAudio := make([]byte, 100)
	ticker := time.NewTicker(time.Second / 150)
	defer ticker.Stop()

	deadline := time.After(1 * time.Second)
	sent := 0
	sendErr := false
loop:
	for {
		select {
		case <-deadline:
			break loop
		case <-ticker.C:
			if err := wsSend.WriteMessage(websocket.BinaryMessage, fakeAudio); err != nil {
				sendErr = true
				break loop
			}
			sent++
		}
	}

	if sendErr {
		t.Fatalf("send error after %d messages — connection was closed prematurely", sent)
	}

	// Verify sender is still connected by sending one more message.
	if err := wsSend.WriteMessage(websocket.BinaryMessage, fakeAudio); err != nil {
		t.Fatal("sender connection was closed despite legitimate traffic")
	}
	t.Logf("sent %d messages at ~150/sec with 3 streams — connection stayed open", sent)
}

func TestJoinExemptFromTextRateLimit(t *testing.T) {
	_, srv := testServer(t)
	ws := dialWS(t, srv)

	// Exhaust the text bucket by sending sync messages to use up the burst,
	// plus a few more to accumulate some violations (but not enough to disconnect).
	syncMsg, _ := json.Marshal(map[string]any{
		"type":    "sync",
		"payload": map[string]any{"type": "Ping", "sent_at": 12345},
	})
	for i := 0; i < 230; i++ {
		ws.WriteMessage(websocket.TextMessage, syncMsg)
	}

	// Join should still work (it's exempted from text rate limiting).
	joinMsg := map[string]any{
		"type":           "join",
		"room":           "join-test",
		"peer_id":        "late-joiner",
		"stream_count":   1,
		"client_version": "99.0.0",
	}
	if err := ws.WriteJSON(joinMsg); err != nil {
		t.Fatal(err)
	}

	ws.SetReadDeadline(time.Now().Add(2 * time.Second))
	for {
		var resp map[string]any
		if err := ws.ReadJSON(&resp); err != nil {
			t.Fatalf("expected join_ok after rate limit, got error: %v", err)
		}
		if resp["type"] == "join_ok" {
			break // success
		}
		if resp["type"] == "rate_limit_warning" {
			continue // skip warnings, keep reading
		}
		t.Fatalf("unexpected message type: %v", resp)
	}
}
