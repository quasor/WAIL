# WAIL Networking Design Document

This document describes the complete networking stack in WAIL — from the moment a user clicks Join to the moment audio flows between peers. It covers the signaling server, WebRTC lifecycle, sync protocol, audio pipeline, failure detection, and reconnection logic.

---

## Table of Contents

1. [Architecture Overview](#1-architecture-overview)
2. [Layer Stack](#2-layer-stack)
3. [Signaling Server](#3-signaling-server)
4. [Connection Lifecycle](#4-connection-lifecycle)
5. [ICE Negotiation and TURN](#5-ice-negotiation-and-turn)
6. [DataChannel Setup](#6-datachannel-setup)
7. [Sync Protocol](#7-sync-protocol)
8. [Audio Pipeline](#8-audio-pipeline)
9. [Failure Detection and Reconnection](#9-failure-detection-and-reconnection)
10. [Peer Status State Machine](#10-peer-status-state-machine)
11. [Clock Synchronization](#11-clock-synchronization)
12. [Known Edge Cases and Issues](#12-known-edge-cases-and-issues)

---

## 1. Architecture Overview

WAIL connects musicians over the internet using WebRTC DataChannels for both sync (JSON) and audio (binary). The networking stack has three layers:

```
┌─────────────────────────────────────────────────────────┐
│                   wail-tauri / session.rs               │
│   Session orchestrator: Link + WebRTC + IPC + UI        │
├─────────────────────────────────────────────────────────┤
│                   wail-net / PeerMesh                   │
│   WebRTC peer management + signaling                    │
│   ┌──────────────┐  ┌────────────────────────────────┐  │
│   │ SignalingClient│  │ PeerConnection (one per peer)  │  │
│   │ WebSocket     │  │ - "sync" DataChannel (JSON)    │  │
│   └──────────────┘  │ - "audio" DataChannel (binary) │  │
│                     └────────────────────────────────┘  │
├─────────────────────────────────────────────────────────┤
│            signaling-server/main.go (Go + SQLite)       │
│   WebSocket server: join / signal / leave               │
│   SQLite: peers, rooms                                  │
│   Deployed to fly.io (wail-signal.fly.dev)              │
└─────────────────────────────────────────────────────────┘
```

Peers communicate directly (P2P) once WebRTC connects. The signaling server is only used during connection setup — it relays SDP offers/answers and ICE candidates over a persistent WebSocket connection. Once DataChannels open, the WebSocket stays connected for peer join/leave notifications and heartbeat (ping/pong).

---

## 2. Layer Stack

| Layer | Crate/File | Responsibility |
|-------|-----------|----------------|
| Session orchestration | `wail-tauri/session.rs` | Drives the `tokio::select!` loop, owns all state, routes messages between Link, mesh, plugins, and frontend |
| Peer mesh | `wail-net/lib.rs` | Manages the `HashMap<peer_id, PeerConnection>` and the signaling polling loop |
| Single peer | `wail-net/peer.rs` | One WebRTC peer connection with two DataChannels |
| Signaling | `wail-net/signaling.rs` | WebSocket client; sends/receives signaling messages in real time |
| Signaling server | `signaling-server/main.go` | Go WebSocket server on fly.io; stores peers/rooms in SQLite |
| Core protocol | `wail-core/protocol.rs` | `SyncMessage` and `SignalMessage` type definitions |
| Ableton Link | `wail-core/link.rs` | FFI bridge to the Link SDK; 50 Hz poller |
| Clock sync | `wail-core/clock.rs` | NTP-style RTT estimation |
| Interval tracker | `wail-core/interval.rs` | NINJAM-style interval boundary tracking |

---

## 3. Signaling Server

### Architecture

The signaling server is a Go WebSocket server (`signaling-server/main.go`) using gorilla/websocket and modernc.org/sqlite (pure-Go SQLite). It is deployed to fly.io at `wss://wail-signal.fly.dev`.

### Storage

SQLite with two tables:

- `peers(room, peer_id, display_name, stream_count, last_seen)` — one row per live peer
- `rooms(room, password_hash, created_at)` — room metadata and optional password

### Protocol

All signaling happens over a single WebSocket connection per client. Messages are JSON with a `type` field:

| Type (client→server) | Description |
|----------------------|-------------|
| `join` | Join a room with peer_id, password, stream_count, display_name |
| `signal` | Relay a signaling message (offer/answer/ICE) to a specific peer |
| `leave` | Leave the current room |
| `metrics_report` | Report audio frame counts and pipeline state for session metrics (not relayed to peers) |

| Type (server→client) | Description |
|----------------------|-------------|
| `join_ok` | Room joined; includes peer list and display names |
| `join_error` | Join failed (version_outdated, unauthorized, room_full) |
| `peer_joined` | A new peer entered the room |
| `peer_left` | A peer left the room |
| `signal` | Forwarded signaling message from another peer |

### Join flow

On `join`, the server:
1. Checks `client_version >= minVersion`. Older clients get `join_error` with code `version_outdated`.
2. Checks/creates the room, verifies password (SHA-256 hash comparison).
3. Checks room capacity: 32 total stream slots. Full rooms get `join_error` with code `room_full`.
4. Upserts peer into `peers` table.
5. Sends `peer_joined` to all existing peers in the room (instant push).
6. Sends `join_ok` with the list of existing `peer_id`s and their display names.

### Heartbeat and stale peer cleanup

The server sends WebSocket pings every 15 seconds. The client's pong response updates `last_seen` in the DB. Stale peers (not seen in 30+ seconds) are cleaned on server startup. During runtime, peers are cleaned when the WebSocket connection closes (the `readPump` defer calls `leave()`).

### HTTP endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/rooms` | List public rooms (excludes password-protected rooms) |
| `GET` | `/metrics` | JSON snapshot of active + completed session metrics (supports `?room=` filter) |
| `GET` | `/metrics/dashboard` | Live HTML dashboard with WebSocket auto-refresh |
| `WS` | `/metrics/ws` | Streaming metrics JSON every 2 seconds (supports `?room=` filter) |
| `GET` | `/health` | Health check |

### Session metrics

The signaling server tracks aggregate session metrics to answer: are clients establishing DataChannels and is audio flowing?

**Session lifecycle.** A session starts when the 2nd peer joins a room (peer count reaches 2+) and ends when the count drops below 2. Each session tracks which peers participated, the session duration, and per-direction audio frame drop counts.

**Phases.** Each session has two phases:

- **Joining** — from session start until ALL peers report `dc_open=true` AND `plugin_connected=true`. This captures the setup period (ICE negotiation, DataChannel open, plugin attachment).
- **Playing** — after the joining→playing transition, when all peers have established DataChannels and have transport playing. This is the steady-state audio flow period.

Frame drops are tracked independently per phase so you can distinguish setup-related drops from network-quality drops.

**Per-direction tracking.** For each unique direction (e.g., Peer1→Peer2 and Peer2→Peer1), the server tracks:

- `frames_expected` — total WAIF frames (20ms Opus packets) the receiver expected from the sender, as determined by `FrameAssembler` during interval assembly
- `frames_received` — total WAIF frames actually received (non-gap)
- `frames_dropped` — `expected - received`

Note: these counts come from `FrameAssembler` in `wail-audio`, which tracks gaps within assembled intervals. A "frame" here is a single 20ms WAIF streaming frame. Frames dropped at the DataChannel/backpressure level before reaching `FrameAssembler` are not counted (see networking.md §8 "Audio channel drop with no feedback").

**Client reporting.** Clients send a `metrics_report` message to the signaling server every 2 seconds (on the existing status tick). This message includes:

| Field | Type | Description |
|-------|------|-------------|
| `dc_open` | `bool` | Whether the audio DataChannel is open |
| `plugin_connected` | `bool` | Whether a send/recv plugin is connected via IPC |
| `per_peer` | `map<peer_id, {frames_expected, frames_received}>` | Cumulative frame counts per remote peer |

The `per_peer` values are cumulative. The server computes playing-phase-only deltas by snapshotting values at the joining→playing transition.

**CLI tool.** `signaling-server/cmd/wail-metrics/` is a standalone Go binary that queries the `/metrics` endpoint:

```sh
# Table-formatted output
wail-metrics -server https://signal.wail.live
wail-metrics -server https://signal.wail.live -room my-room

# Raw JSON
wail-metrics -json
```

**Live dashboard.** Visit `/metrics/dashboard` on the signaling server for a real-time HTML dashboard that streams metrics via WebSocket every 2 seconds. Supports a `?room=` query parameter to filter by room. The dashboard auto-reconnects on disconnect.

---

## 4. Connection Lifecycle

### Full sequence for two peers joining

```
Peer A (lower peer_id)                  Signaling Server               Peer B (higher peer_id)
───────────────────────                 ────────────────               ───────────────────────
WS connect ─────────────────────────────────►│
send: {type: "join", room, peer_id: "A"} ──►│
◄──────── {type: "join_ok", peers: []} ──────│
(room empty, no connections needed)          │
                                             │
                    WS connect ──────────────────────────────────────►│
                    send: {type: "join", room, peer_id: "B"} ───────►│
                                             │                        │
◄──── {type: "peer_joined", peer_id: "B"} ──│  (instant push to A)   │
                                             │                        │
                    ◄──── {type: "join_ok", peers: ["A"]} ───────────│
                                             │
A: lower peer_id → initiates WebRTC to B     │
A creates offer SDP                          │
A sets local description                     │
A: send {type: "signal", to: "B",            │
         payload: Offer{sdp}} ──────────────►│──── push to B ────────►│
                                             │                         │
A: ICE candidates discovered (async)         │                         │
A: send {type: "signal", to: "B",            │                         │
         payload: IceCandidate} ────────────►│──── push to B ────────►│
                                             │                         │
                  B: creates PeerConnection, handle_offer()            │
                  B: sets remote description (offer)                   │
                  B: applies pending ICE candidates                    │
                  B: creates answer SDP                                │
                  B: sets local description                            │
                  B: send {type: "signal", to: "A",                    │
                           payload: Answer{sdp}} ─────────────────────►│
◄──── push to A ─────────────────────────────│                         │
                  B: ICE candidates discovered                         │
                  B: send {type: "signal", to: "A",                    │
                           payload: IceCandidate} ────────────────────►│
◄──── push to A ─────────────────────────────│
                                             │
A: handle_answer() → sets remote description │
A: adds ICE candidates                       │
                                             │
                     [ICE connectivity checks run P2P]
                                             │
                     [WebRTC connection established]
                     [DataChannels open]
                                             │
A → B: Hello{peer_id, display_name, identity}
B → A: Hello{peer_id, display_name, identity}  (reply)
A → B: IntervalConfig{bars, quantum}
A → B: AudioCapabilities{...}
A → B: StateSnapshot{bpm, beat, phase, ...}  (every ~200ms via Link poller)
```

### Tie-breaking rule

Only the peer with the **lexicographically lower** `peer_id` initiates offers. Peer IDs are 8-character UUID prefixes (e.g., `"3f8a1b2c"`). This rule applies both at join time (from the PeerList) and when a new peer joins mid-session (PeerJoined event). It prevents both peers from simultaneously creating offers to each other.

Concretely, in `handle_signal_message`:
```rust
// PeerList: initiate only if our ID < theirs
if remote_id != self.peer_id && self.peer_id < remote_id {
    self.initiate_connection(&remote_id).await?;
}

// PeerJoined: same rule
if self.peer_id < remote_id {
    self.initiate_connection(&remote_id).await?;
}
```

The higher-peer-ID side waits for an incoming offer via the `on_data_channel` callback.

### What happens if both sides think they should initiate

This can't happen under normal conditions given the strict `<` comparison. However, there is one edge case: if peer B joins, A initiates to B; then A disconnects and reconnects with a new peer_id that is now higher than B's. In that case B will initiate to A on the PeerJoined signal. The server sends PeerJoined to both sides when a new peer joins, but only the lower-ID side does anything with it (the higher-ID side waits). The responder's `on_data_channel` callback handles the incoming channels.

---

## 5. ICE Negotiation and TURN

### ICE server setup

At session start (`session_loop`), WAIL fetches fresh TURN credentials from the Metered API:

```
GET https://wail.metered.live/api/v1/turn/credentials?apiKey=...
```

The response includes STUN and TURN URLs with short-lived credentials. On API failure, WAIL falls back to `stun:stun.relay.metered.ca:80` (STUN only, no relay).

The Metered fetch happens **before** `PeerMesh::connect_full()`, so all peer connections in the session share the same ICE server list. TURN credentials are not refreshed mid-session; if the session runs longer than the credential TTL, ICE reconnection attempts may fail for NAT traversal even though the WebRTC connection itself is alive.

### ICE candidate flow

ICE candidates are discovered asynchronously after `set_local_description`. Each candidate is sent via the `on_ice_candidate` callback into an `mpsc::UnboundedSender<RTCIceCandidate>`, which `spawn_ice_sender` drains and forwards to the signaling server.

ICE candidates may arrive at the responder **before** the remote description is set (because the signaling poll batches messages). The `PeerConnection` stores early candidates in `pending_candidates: Vec<RTCIceCandidateInit>` and applies them immediately after `set_remote_description`:

```rust
if self.remote_desc_set {
    self.pc.add_ice_candidate(init).await?;
} else {
    self.pending_candidates.push(init);
}
```

### Relay-only mode

`relay_only: bool` (default `false`) sets `RTCIceTransportPolicy::Relay`, forcing all traffic through TURN. Used in tests to simulate constrained NAT and in debug scenarios.

### mDNS

mDNS is explicitly disabled (`MulticastDnsMode::Disabled`) so that LAN peers don't leak hostnames through the signaling server.

---

## 6. DataChannel Setup

### Two channels per peer

| Channel | Label | Mode | Buffer | Purpose |
|---------|-------|------|--------|---------|
| sync | `"sync"` | Text (UTF-8 JSON) | Unbounded | Tempo, beat, phase, Hello, Ping/Pong, interval config |
| audio | `"audio"` | Binary | Bounded (64) | Opus-encoded audio intervals |

### Initiator vs. responder paths

**Initiator** (`create_offer`):
- Calls `pc.create_data_channel("sync", None)` and `create_data_channel("audio", None)` **before** creating the SDP offer. This embeds the DC negotiation in the offer SDP.
- Calls `setup_sync_channel()` / `setup_audio_channel()` which register all callbacks and store the `Arc<RTCDataChannel>` in `OnceLock`.

**Responder** (`handle_offer`):
- Registers `pc.on_data_channel()` callback **before** setting the remote description. The callback fires when each DC is received.
- Inside the callback, labels are checked: `"sync"` → `setup_sync_channel`-equivalent logic; `"audio"` → `setup_audio_channel`-equivalent logic. The `OnceLock` slots are set here.
- Unknown DC labels are silently ignored.

Both paths use `OnceLock<Arc<RTCDataChannel>>` so that `send()` and `send_audio()` can check `dc_sync.get()` from any context without holding a lock.

### Message queuing before DC open

Sync messages sent before the DataChannel transitions to `Open` are queued in `pending_sync: Arc<Mutex<Vec<String>>>`. On `on_open`, the queue is drained and sent in order. This ensures `Hello`, `IntervalConfig`, and `AudioCapabilities` reach the peer even if broadcast immediately after initiating.

The audio DC has no such queue — audio sent before `Open` is silently dropped. This is intentional since audio is interval-based and there's no meaningful "replay" of old intervals.

### Audio chunking

SCTP fragmentation in `webrtc-rs` is unreliable on real internet paths. Audio intervals can be large (5–50 KB), so WAIL chunks them at the application layer:

```
┌─────────────────────────────────────────────────────────────┐
│  Each chunk (≤ 1200 bytes total):                           │
│  [WACH][total_len u32 LE][payload up to 1192 bytes]         │
└─────────────────────────────────────────────────────────────┘
```

The receiver's `AudioReassembly` struct (inside a `Mutex`) accumulates chunks until `buffer.len() >= expected_len`, then delivers the complete message downstream.

Small messages (≤ 1200 bytes) bypass chunking entirely — they are sent without the `WACH` header. The reassembly handler checks for the magic prefix to distinguish chunked from non-chunked messages.

**Potential issue:** The reassembly buffer has no timeout. If a chunk is lost, the buffer grows indefinitely until the connection closes. There is also no sequence number or message boundary — if chunks from two concurrent large messages interleave, reassembly would produce garbage. In practice, WebRTC DataChannels guarantee ordered, reliable delivery (SCTP), so this shouldn't happen; but the code has no defensive check.

---

## 7. Sync Protocol

### SyncMessage types

| Type | Direction | Frequency | Purpose |
|------|-----------|-----------|---------|
| `Hello` | bidirectional | Once on connect, once on reconnect | Exchange peer_id, display_name, identity |
| `Ping` | broadcast | Every 2s | Clock sync RTT measurement |
| `Pong` | reply | Per Ping | Clock sync RTT response |
| `TempoChange` | broadcast | On tempo change (threshold: 0.01 BPM) | Propagate Link tempo changes |
| `StateSnapshot` | broadcast | Every ~200ms | Full Link state (bpm, beat, phase, quantum) |
| `IntervalConfig` | broadcast | On join | Tell peers our bars/quantum settings |
| `AudioCapabilities` | broadcast | On join | Announce what we can send/receive |
| `AudioIntervalReady` | broadcast | Per test tone interval | Announce incoming audio (test tone path only) |
| `IntervalBoundary` | broadcast | Per interval | Notify peers of our interval index |
| `AudioStatus` | broadcast | Every 2s | Diagnostic: DC open, intervals sent/received |

### Hello exchange

Hello is sent:
1. When we receive a `PeerJoined` event from signaling — broadcast to all connected peers.
2. When we receive a `Hello` from a peer we haven't replied to yet — unicast reply.

`PeerRegistry::mark_hello_sent(peer_id)` tracks which peers have received our Hello, preventing infinite Hello loops. It returns `false` if Hello was already sent, which guards the reply path.

When a peer reconnects after `PeerFailed`, `PeerRegistry::clear_hello_sent(peer_id)` is called before `re_initiate()`, so the Hello handshake runs again on the new connection.

### Tempo sync

Tempo propagation uses **echo suppression** at two levels:

1. **Link bridge echo guard** (`link.rs`): After applying a remote `SetTempo` or `ForceBeat` command, an `echo_guard_until` timestamp is set 150ms in the future. During this window, the Link poller suppresses `TempoChanged` events (even if Link reports the change locally).

2. **Session-level last_broadcast_bpm**: `last_broadcast_bpm` tracks the last BPM we either set or received. Both `TempoChanged` (outgoing) and `TempoChange`/`StateSnapshot` (incoming) update this value. Changes are only sent if `abs(new - last) > 0.01 BPM`.

Without both guards, the sequence `A changes tempo → broadcasts → B applies → B's Link fires TempoChanged → B broadcasts back → A applies → A's Link fires → ...` would loop indefinitely.

### Beat sync and StateSnapshot

The first `StateSnapshot` received triggers **beat sync**:
1. `beat_synced = true`
2. `audio_gate.on_beat_synced()` — lifts the audio send gate
3. `LinkCommand::ForceBeat(remote_beat)` — snaps local Link to peer's beat position
4. `interval.set_config(bars, quantum)` — adopts remote interval configuration

Subsequent StateSnapshots are used only to track tempo drift (not to re-snap beat position).

### IntervalBoundary sync

When a peer's `IntervalBoundary { index }` arrives:
- If our local `interval.current_index()` is behind (or None), we call `interval.sync_to(index)`.
- `sync_to` sets the internal `last_interval_index` to the remote index, suppressing local boundaries until our beat clock naturally advances past it.
- This is monotonic: we never sync backward.

---

## 8. Audio Pipeline

### End-to-end path

```
DAW → [WAIL Send plugin] → TCP IPC → session_loop
                                         │
                              ipc_from_plugin_rx
                                         │
                              mesh.broadcast_audio(wire_data)
                                         │
                              PeerConnection.send_audio()
                                         │
                              chunk into ≤1200 byte messages
                                         │
                              WebRTC "audio" DataChannel
                                         │ (internet / TURN)
                              remote peer's DataChannel
                                         │
                              AudioReassembly → complete message
                                         │
                              audio_tx (bounded, capacity=64)
                                         │
                              audio_rx in session_loop
                                         │
                              IpcMessage::encode_audio()
                                         │
                              TCP IPC → [WAIL Recv plugin] → DAW
```

### Channel capacities and backpressure

| Channel | Capacity | Drop behavior |
|---------|----------|---------------|
| `PeerConnection.audio_tx` | 64 | `try_send` → log debug, drop |
| `PeerMesh.audio_tx` | 64 | `try_send` → log debug, drop |
| `ipc_from_plugin_tx` | 64 | `try_send` → log debug, drop |

Both audio channels are bounded at 64. Under congestion, frames are **silently dropped** (debug-logged but not counted or reported to the UI). The audio stats (`audio_intervals_received`) count frames that reach `audio_rx.recv()` in the session loop — frames dropped before that point are invisible to the UI.

### Wire format (AudioWire)

Binary header (48 bytes) followed by Opus data:

```
Magic: "WAIL" (4 bytes)
Version: 2 (1 byte)
Flags: (1 byte)
Stream ID: u16 LE (2 bytes)
Interval index: i64 LE (8 bytes)
Sample rate: u32 LE (4 bytes)
BPM: f64 LE (8 bytes)
Quantum: f64 LE (8 bytes)
Bars: u32 LE (4 bytes)
Num frames: u32 LE (4 bytes)
Channels: u8 (1 byte)
Opus data length: u32 LE (4 bytes) [not present in wire — inferred]
... Opus data ...
```

### Slot assignment

When audio arrives from a peer with a new `(peer_id, stream_id)` pair, the session assigns it a **slot** (0–14, matching `MAX_REMOTE_PEERS`). The recv plugin uses this slot to route audio to the correct output bus.

Slot assignment logic (mirrored in both the session and the recv plugin):
1. Check `SlotAllocator::affinity` for `(identity, stream_id)` — if the peer has connected before with the same persistent identity, reuse their old slot.
2. If no affinity, find the first unoccupied slot in the `SlotAllocator::occupied` bitmap.
3. Record the slot in `PeerState::slots` keyed by `stream_id`.

When a peer leaves (`PeerLeft` or `PeerFailed` after max attempts):
1. All slots for that peer are freed.
2. Affinity entries `(identity, stream_id) → slot` are created so the peer gets the same slot if they rejoin.

---

## 9. Failure Detection and Reconnection

### Failure detection mechanisms

There are **three independent** mechanisms that can trigger a `PeerFailed` event:

#### 1. DataChannel `on_close` callback
Both the sync and audio DataChannels have `on_close` callbacks that immediately send `failure_tx.send(peer_id)`. This fires when the remote peer closes the connection cleanly or when the underlying SCTP association closes.

#### 2. Reader task exit
The `spawn_message_reader` and `spawn_audio_reader` tasks loop on `rx.recv()`. When the channel closes (because the PeerConnection was dropped), the loop exits and sends `failure_tx.send(peer_id)`.

Both DC `on_close` and the reader task exit will fire for the same peer failure. The second signal is deduplicated in `poll_signaling`:
```rust
if self.peers.contains_key(&failed_peer) {
    Ok(Some(MeshEvent::PeerFailed(failed_peer)))
} else {
    Ok(Some(MeshEvent::SignalingProcessed))  // already removed
}
```

#### 3. WebRTC connection state callback
`on_peer_connection_state_change` fires `failure_tx` on `Failed` or `Disconnected`. This catches ICE failure (no candidate pair) before DataChannels even open.

#### 4. Liveness watchdog
A separate `liveness_interval` fires every 15 seconds. It checks `peer_last_seen` — if a peer hasn't sent any message in `PEER_LIVENESS_TIMEOUT` (30 seconds), it calls `mesh.close_peer(peer_id)`. Closing triggers the DC `on_close` callbacks, which feed back into mechanism 1 above.

`peer_last_seen` is updated on both sync and audio messages, and is seeded when a peer first appears (via `PeerJoined`, `PeerListReceived`, or `re_initiate`). This ensures peers that connect but never send a message are still timed out.

### Peer reconnection state machine

```
                        PeerFailed received
                               │
                    increment PeerState::reconnect_attempts
                               │
              ┌────────────────▼───────────────────┐
              │  attempts > MAX_PEER_RECONNECT (5)?  │
              └──────────────────────────────────────┘
                   yes │                    no │
                       │                       │
                       ▼                       ▼
            emit PeerLeftEvent       emit PeerReconnectingEvent
            free slots               calculate backoff:
            remove all state           min(BASE_MS * 2^(attempt-1), MAX_MS)
                                       = min(2000 * 2^(n-1), 16000)
                                     spawn timer task → reconnect_rx
                                               │
                              ┌────────────────▼────────────────┐
                              │  reconnect_rx fires (after sleep) │
                              └────────────────────────────────────┘
                                               │
                                 peers.get(pid).reconnect_attempts > 0?
                                    no → skip (PeerLeft arrived in meantime)
                                   yes ↓
                                 mesh.re_initiate(pid)
                                  └── remove_peer(pid)
                                  └── if our_id < pid: initiate_connection(pid)
                                               │
                               send Hello broadcast
```

Backoff schedule:
- Attempt 1: 2 000 ms
- Attempt 2: 4 000 ms
- Attempt 3: 8 000 ms
- Attempt 4: 16 000 ms
- Attempt 5: 16 000 ms (capped)
- Attempt 6+: emit PeerLeftEvent, give up

After `re_initiate()`, the new connection must go through the full ICE + DataChannel handshake. Hello is resent so the peer learns our display name on the new connection.

**Known issue:** If the higher-ID peer is the one calling `re_initiate()`, the function only removes the dead peer and does nothing (`our_id >= pid` → no `initiate_connection`). The higher-ID peer is waiting for the lower-ID peer to send a new offer. If the lower-ID peer is also calling `re_initiate()`, they will send a new offer. But there is no timeout on how long the higher-ID peer waits — it will just hang in a "reconnecting" state until either a new offer arrives or the signaling server sends a `PeerLeft` for the lower-ID peer.

### Signaling reconnection

When `poll_signaling()` returns `Ok(None)` (the signaling WebSocket closed), the session enters a **signaling reconnection loop** that runs *inside* the `tokio::select!` arm (blocking the other arms):

```
Ok(None) from poll_signaling
          │
emit session:reconnecting
          │
          ├── attempt 1: backoff 1000ms, re-fetch ICE, PeerMesh::connect_full()
          ├── attempt 2: backoff 2000ms, ...
          ├── attempt 3: backoff 4000ms, ...
          ├── ...
          ├── attempt 10: emit session:stale, keep retrying
          └── on success: replace mesh/sync_rx/audio_rx, clear peer state, re-gate audio
```

On success (`PeerMesh::reconnect_signaling()`):
- Only the `SignalingClient` (WebSocket) is replaced — `self.peers` (all WebRTC `PeerConnection` objects) are **untouched**.
- Established P2P DataChannels remain open; audio/sync flow continues without interruption.
- Any genuinely new peers from the fresh `join_ok` PeerList get new WebRTC offers initiated.
- `audio_gate.on_reconnect()` re-gates audio briefly until beat sync is re-established.
- `peers.seed_names(new_peer_names)` adds any new peer names without clearing existing state.

See §13.E for the fixed-non-blocking implementation.

---

## 10. Peer Status State Machine

The UI displays a status string per peer. Priority order (highest wins):

```
reconnecting  ←  peer_reconnect_attempts[pid] > 0
connecting    ←  display_name not yet known (Hello not received)
full-duplex   ←  sending to AND receiving from this peer
receiving     ←  receiving audio from this peer
sending       ←  sending audio to this peer (and DC is open)
connected     ←  Hello received, no audio flowing
```

Status derivation is centralized in `PeerRegistry::derive_status()`, which takes `is_receiving` and `is_sending` booleans and returns the highest-priority label. Status transitions are tracked in `PeerState::prev_status` and logged as `"old → new"` on each status tick (every 2 seconds). This means status transitions have up to a 2-second delay in the UI.

---

## 11. Clock Synchronization

WAIL uses an NTP-style algorithm for peer RTT measurement:

```
A → B: Ping { id, sent_at_us: T1 }
B → A: Pong { id, ping_sent_at_us: T1, pong_sent_at_us: T2 }

On A receiving Pong at time T3:
  RTT = T3 - T1
```

Each peer maintains a `VecDeque<i64>` of the last 8 RTT samples per remote peer. The median is used as the working estimate, making it robust to jitter and outliers.

Clock offset computation was intentionally removed: `ClockSync` timestamps (`Instant::now()`) and Link timestamps (`link.clock_micros()`) are different clock domains and cannot be combined. RTT is available via `ClockSync::rtt_us(peer_id)` and displayed in the UI as the peer's round-trip time.

Pings are broadcast to all peers every 2 seconds (`PING_INTERVAL_MS = 2000`). Pong is sent unicast to the ping sender. Both go over the sync DataChannel.

---

## 12. Known Edge Cases and Issues

### A. Duplicate PeerFailed signals

The `on_close` callback, `spawn_message_reader` exit, and `spawn_audio_reader` exit all independently send to `failure_tx`. All three can fire for the same connection failure. The deduplication in `poll_signaling` (checking `self.peers.contains_key`) handles this correctly **only if** `PeerFailed` is processed before the peer is removed from the map. However, once `PeerFailed` reaches session.rs and `re_initiate()` is called (which calls `remove_peer()`), any subsequent `failure_tx` messages from the old connection's tasks will be silently discarded as `SignalingProcessed`. This is correct behavior.

### B. ICE candidate race on fast peers

If A sends an offer and ICE candidates immediately (before B polls), B's `?action=poll` response may contain the offer, ICE candidates, and possibly the answer all in the same batch (in sequence-numbered order). The current code handles candidates before `remote_desc_set` correctly using `pending_candidates`. But if the **answer** arrives before the initiator has processed all its own ICE candidates, this is also fine — the initiator calls `add_ice_candidate` after receiving the answer.

### C. Missing Hello for responder

When peer B receives an offer from A and sends an answer, B inserts A into `self.peers`. Session.rs then broadcasts `Hello` and gets `connected_peers()` → inserts all current peers into `hello_sent`. But A hasn't yet received the answer, so A's DataChannel isn't open. The Hello is **queued** in `pending_sync` and flushed on `on_open` — this is correct and handled.

### D. Re-initiate for the higher-ID peer

As noted in section 9, when the higher-ID peer calls `re_initiate()`, it only removes the dead peer and waits for a new offer. There is no mechanism to ensure the lower-ID peer is actually alive and will send a new offer. If the lower-ID peer is also gone (e.g., both lost internet), the higher-ID peer is stuck in `reconnecting` state with no way out except:
- A new `PeerLeft` arriving from the signaling server (if the lower-ID peer's heartbeat expires).
- The session itself being disconnected.

After `MAX_PEER_RECONNECT_ATTEMPTS` failures (5), the peer is treated as permanently left, which correctly unblocks the slot. But each failure requires waiting for the full reconnection attempt with backoff.

### E. Signaling reconnect (fixed)

Signaling reconnection is implemented as a non-blocking state machine (`SignalingReconnect`) polled by the main `select!` loop. While reconnecting, all other arms continue running: Link events are processed, status updates are emitted, audio from plugins is drained, the liveness watchdog ticks, and `Disconnect` commands are handled immediately.

The `mesh.poll_signaling()` arm is guarded with `if signaling_reconnect.is_none()` so the dead mesh is not polled during reconnection.

### F. TURN credential expiry mid-session

TURN credentials are fetched once at session start. If the session outlasts the credential TTL and a peer fails mid-session, `re_initiate()` will use the stale credentials. The credentials are not refreshed on reconnect.

The signaling reconnect path **does** re-fetch ICE servers:
```rust
let ice = match wail_net::fetch_metered_ice_servers().await {
    Ok(s) => s,
    Err(_) => wail_net::metered_stun_fallback(),
};
```
But individual peer reconnections (PeerFailed → re_initiate) do not. The mesh holds its `ice_servers` from construction time.

### G. WebSocket signaling throughput

With the WebSocket signaling server, signals are delivered instantly (no polling delay). This eliminates the previous bottleneck where at most 5 outgoing signals were processed per 5-second poll tick.

### H. Liveness watchdog peer seeding (fixed)

`peer_last_seen` is updated on both sync and audio messages. Previously, peers that appeared in the mesh (via `PeerJoined` or `PeerListReceived`) but never sent a single message were invisible to the watchdog and could sit in "connecting" state forever.

Now fixed: `peer_last_seen` is seeded with `Instant::now()` when a peer first appears — on `PeerJoined`, on `PeerListReceived` (for initial peers), and after `re_initiate` (for reconnecting peers). A peer that connects but stalls will be timed out by the watchdog after 30 seconds.

### I. Audio channel drop with no feedback

When the audio bounded channel (capacity 64) is full, frames are dropped with a `debug!` log. This is invisible to the UI — `audio_intervals_received` is only incremented when a frame reaches the `audio_rx.recv()` call in the session loop, which happens after the drop point. The UI cannot distinguish "received 100 intervals" from "received 100, dropped 20."
