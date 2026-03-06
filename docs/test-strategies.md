# WAIL Test Strategies

This document maps known scenarios, edge cases, and failure modes to concrete test strategies. It is organized by subsystem, then by what is already covered, what is missing, and what technique to use.

---

## How to Read This Document

Each section lists test scenarios in a table with four fields:

| Field | Meaning |
|-------|---------|
| **Scenario** | What is being exercised |
| **Status** | `covered` — automated test exists; `partial` — partially covered; `missing` — no test |
| **Technique** | Unit / Integration / E2E / Manual / Load |
| **Notes** | Where the test lives, or what to build |

---

## 1. Signaling Server

### 1.1 Join / Room Management

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| Peer joins empty room, gets empty peer list | covered | Integration | `two_peers_exchange_audio_over_webrtc` (implicitly) |
| Second peer joins, first gets `PeerJoined` | covered | Integration | `establish_connection` helper drives this |
| `client_version` below `MIN_CLIENT_VERSION` → 426 | missing | Integration | POST to real or stub server with old version string; assert 426 + `min_version` body |
| `client_version` exactly equal to minimum → accepted | missing | Integration | Boundary condition on semver comparison |
| Wrong password → 401 | missing | Integration | Use in-process server with password; assert client gets the "Invalid room password" error |
| Correct password → 200 | missing | Integration | Same setup, correct password |
| Room full → 409 with `slots_available` | missing | Integration | Fill room to capacity (8 × `stream_count`) then attempt one more join; assert 409 body |
| `display_name` returned to existing peers on join | missing | Integration | Verify `PeerJoined.display_name` matches what joiner sent |
| Room re-creation after last peer leaves | missing | Integration | Two peers join/leave, verify room row deleted; third peer can re-create with different password |
| Peer joins with `stream_count > 1` | missing | Integration | Assert slot consumption is multiplied; room capacity enforced per stream slot |

### 1.2 Polling and Heartbeat

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| `poll` updates `last_seen` | missing | Integration | Poll once, wait 25s (< 30s), poll again — peer should still be in room |
| Stale peer cleaned after 30s of no poll | missing | Integration | Peer A joins, peer B joins, B stops polling. After 35s, A should receive `PeerLeft{B}` |
| `evicted: true` in poll response triggers signaling reconnect | partial | Unit | `signaling_eviction_closes_channel` only tests JSON deserialization; no test for the full `incoming_rx` close → session reconnect path |
| Messages cleaned up after 60s | missing | Integration | Enqueue message, don't poll for 65s, poll — message should be gone and not re-delivered |
| `after` sequence number prevents duplicate delivery | missing | Integration | Poll twice with the same `after`; assert messages not returned twice |
| Rate-limit (429) causes exponential backoff, then recovery | missing | Integration | Intercept HTTP layer or use a stub server that returns 429 × N times then 200; verify `current_poll_ms` doubles and resets |
| Up to 5 outgoing signals per poll tick | missing | Integration | Queue 7 signals on outgoing_tx, run one poll loop; verify only 5 are sent, remaining 2 held for next tick |
| Outgoing channel closed → `leave` is sent | missing | Integration | Drop `SignalingClient`, verify the server receives a `?action=leave` |

### 1.3 Public Room Listing

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| `?action=list` returns rooms with peer counts and BPM | missing | Integration | Create a room with peers, call `list_public_rooms`; verify room appears |
| Password-protected rooms omitted from public list | missing | Integration | Depends on server implementation; verify no password-protected room leaks |

---

## 2. WebRTC / ICE Connection

### 2.1 Happy Path

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| Two peers connect via localhost (no TURN) | covered | Integration | `two_peers_exchange_audio_over_webrtc` |
| Audio DataChannels open on both sides | covered | Integration | `audio_dc_reports_open_after_connection` |
| Two peers connect via local TURN (coturn) | covered | Integration | `two_peers_exchange_audio_via_turn` (ignored, requires coturn) |
| Two peers connect via live Metered TURN | covered | Integration | `metered_turn_relay_live` (ignored, requires internet) |
| Relay-only mode forces TURN candidates, no host/srflx | covered | Integration | `metered_turn_relay_live` uses `relay_only=true` |
| Metered ICE server fetch returns valid TURN credentials | covered | Integration | `fetch_metered_ice_servers_live` (ignored) |

### 2.2 ICE Edge Cases

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| ICE candidates arrive at responder before answer is sent | partial | Integration | Covered by the flow but not asserted explicitly; add a test that artificially delays the answer and sends candidates first, verifying `pending_candidates` drains correctly |
| ICE candidates arrive before remote description is set | partial | Integration | Same as above — the `pending_candidates` buffer is exercised in normal flow but timing is not stressed |
| Metered API unreachable → fallback to `metered_stun_fallback()` | missing | Integration | Stub the HTTP client to return a network error; assert `fetch_metered_ice_servers` returns the Err path and session falls back |
| TURN credentials expire mid-session; peer failure triggers re_initiate with stale ICE | missing | Integration | Set a very short credential TTL (stub server), wait for expiry, trigger peer failure; assert reconnection fails gracefully (not panic) |
| ICE gathering completes but no viable pair (connection goes to Failed state) | missing | Integration | Use a filter that drops all candidates, verify `RTCPeerConnectionState::Failed` → `failure_tx` fires |

### 2.3 Tie-Breaking

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| Lower peer_id initiates, higher waits | covered | Integration | All tests use `peer-a` < `peer-b` implicitly |
| Simultaneous join: both see each other in PeerJoined | missing | Integration | Join A and B at the exact same tick (no sleep between them); assert exactly one offer is created (not two) and one connection is established |
| Equal peer_ids (shouldn't happen, but) | missing | Unit | `peer_id == remote_id` — the code skips connection (`self.peer_id < remote_id` is false); verify no deadlock |

---

## 3. DataChannel Setup

### 3.1 Message Queuing

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| Sync messages sent before DC opens are queued and flushed | missing | Integration | Send `Hello` immediately after `create_offer`, before the responder has set up DCs; verify message arrives after DC opens. Current tests don't assert `pending_sync` behavior |
| Queue is drained in order (FIFO) | missing | Unit | Push 3 messages to `pending_sync`, trigger flush, verify order |
| Large queue (e.g. 50 messages) flushed cleanly on open | missing | Integration | Queue 50 messages before DC open; verify all 50 arrive in order |

### 3.2 Audio Chunking and Reassembly

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| Message exactly 1200 bytes sent without chunking header | missing | Unit | `data.len() == CHUNK_MAX` → `send` should use single-message path (no WACH header) |
| Message exactly 1201 bytes triggers chunking | missing | Unit | `data.len() == CHUNK_MAX + 1` → two chunks with WACH header |
| Large message (50 KB) correctly chunked and reassembled | covered | Integration | `multi_interval_full_size_e2e` exercises this via real intervals |
| Partial chunk received — reassembly buffer holds state | missing | Unit | Manually construct two chunks, deliver first, verify nothing emitted; deliver second, verify complete message emitted |
| Non-chunked message (no WACH magic) passes through directly | missing | Unit | Send a message without WACH prefix; verify reassembly handler emits it as-is |
| Reassembly buffer correctly reset after complete message | missing | Unit | Send full message → reassemble; then send another message; verify second is not corrupted by state from first |
| Messages from two different logical "messages" do not interleave | missing | Unit | WebRTC guarantees ordering but test that reassembly resets fully between messages |

---

## 4. Sync Protocol

### 4.1 Hello Exchange

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| Initiator sends Hello on `PeerJoined`; responder replies | missing | Integration | After `establish_connection`, pump `sync_rx` to receive Hello from both sides; assert `peer_id` and `display_name` fields correct |
| Hello not sent twice to same peer (`hello_sent` guard) | missing | Integration | Trigger `PeerJoined` twice for the same peer (rejoin scenario); assert Hello sent only once per connection |
| Hello reply sent when we receive Hello from peer not yet in `hello_sent` | missing | Integration | Responder-side: receive Hello, verify reply Hello is sent back |
| After reconnection, Hello is resent on new connection | missing | Integration | `peer_reconnects_after_close` does reconnection but doesn't verify Hello exchange on the new connection |
| Identity (`identity` field) forwarded and stored in `peer_identities` | missing | Integration | Send Hello with identity="alice-machine"; verify `peer_identities` map updated, used for slot affinity |

### 4.2 Tempo Sync and Echo Suppression

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| Remote `TempoChange` applied to Link without echo loop | missing | Integration | Simulate A changing BPM, verify B applies it, verify A does NOT broadcast it back (check `last_broadcast_bpm` threshold); run for several cycles |
| Echo guard (150ms) suppresses re-broadcast | missing | Unit | In `LinkBridge`: call `set_tempo(120.0)`, immediately check tempo — `echo_guard_until` should suppress the TempoChanged event |
| Tempo change below 0.01 BPM threshold not broadcast | missing | Unit | Change BPM by 0.009; verify no `TempoChange` message sent |
| Tempo change of exactly 0.01 BPM IS broadcast | missing | Unit | Boundary condition on the threshold |
| Simultaneous tempo changes from two peers — last write wins | missing | Integration | A sends 140 BPM, B sends 120 BPM near-simultaneously; both should eventually settle on the same value without looping |

### 4.3 Beat Sync and State Snapshots

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| First `StateSnapshot` triggers `ForceBeat` and lifts `AudioSendGate` | missing | Integration | Join room with existing peer, receive one StateSnapshot, assert `beat_synced = true` and gate is open |
| `ForceBeat` snaps local beat position to remote value | missing | Integration | Verify Link state after `ForceBeat(10.0)` reports beat ≈ 10.0 |
| Subsequent StateSnapshots do not re-snap beat | missing | Integration | After initial sync, receive 5 more snapshots; assert `ForceBeat` called only once |
| BPM drift detected from StateSnapshot and corrected | missing | Integration | StateSnapshot arrives with BPM 5.0 BPM different from `last_broadcast_bpm`; assert `SetTempo` called |

### 4.4 Interval Boundaries

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| `IntervalBoundary` from peer behind us is ignored | covered | Unit | `interval_tests.rs`: `sync_to` monotonicity tested |
| `IntervalBoundary` from peer ahead syncs local tracker | missing | Integration | Peer A announces interval 5 while B is at interval 3; B should sync_to(5) and stop firing boundaries until beat catches up |
| `IntervalConfig` from peer updates local bars/quantum | missing | Integration | Send `IntervalConfig { bars: 2, quantum: 4 }` mid-session; verify `interval.beats_per_interval()` changes |

---

## 5. Audio Pipeline

### 5.1 AudioSendGate

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| New peer (first in room) is not gated | covered | Unit | `first_peer_not_gated` |
| Joiner is gated until StateSnapshot received | covered | Unit | `second_peer_gated_then_unlocked` |
| Reconnect re-gates | covered | Unit | `reconnect_regates_until_beat_sync` |
| First-in-room reconnect to empty room clears gate | covered | Unit | `first_peer_reconnects_to_empty_room` |
| Gate actually suppresses `broadcast_audio` | missing | Integration | With gate active, call `broadcast_audio`; verify remote peer receives nothing. This is only unit tested today |
| "All peers join simultaneously" — all receive StateSnapshot, all lift gate | missing | Integration | Two peers join with < 100ms gap, both see `n=1` in PeerList; verify both eventually lift their gate after exchanging StateSnapshots |

### 5.2 Wire Format and Codec

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| `AudioWire::encode` then `decode` round-trips cleanly | missing | Unit | Encode a known interval, decode, assert all header fields match and opus bytes intact |
| Wire format magic mismatch → decode error | missing | Unit | Corrupt the "WAIL" magic bytes; assert `AudioWire::decode` returns `Err` |
| Wire format version mismatch | missing | Unit | Set version byte to an unknown value; assert graceful error, not panic |
| Opus decode of real interval produces non-silent PCM | covered | Integration | `two_peers_exchange_audio_over_webrtc` asserts `RMS > 0.01` |
| Very short interval (< 1 Opus frame) handled without panic | missing | Unit | Encode 0 samples or 1 sample; verify no panic |
| Very long interval (e.g., 60s at low BPM) stays within wire format limits | missing | Unit | Large `num_frames` field — verify encode/decode round-trip without integer overflow |

### 5.3 Slot Assignment

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| New peer gets next free slot | missing | Integration | Three peers join; verify slots 0, 1, 2 assigned in arrival order |
| Peer leaves, slot freed | missing | Integration | Peer 1 leaves; verify `slot_occupied[slot]` is false |
| Peer with same identity gets same slot on rejoin (affinity) | missing | Integration | Peer leaves (slot freed, affinity stored), then rejoins with same identity; verify same slot assigned |
| Slot affinity preserved across signaling reconnection | missing | Integration | Simulate signaling reconnect; verify `slot_affinity` not cleared |
| All slots full → new peer cannot get slot | missing | Integration | Fill 31 slots (MAX_REMOTE_PEERS), add 32nd peer; assert no slot assigned, no panic |
| Multi-stream: stream_id > 0 gets its own slot | missing | Integration | Peer sends audio with `stream_id = 1`; verify separate slot from stream_id 0 |

### 5.4 Channel Backpressure and Drops

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| Audio channel at capacity (64) drops frame, logs debug | missing | Unit | Fill `audio_tx` to 64, send one more; verify `try_send` returns `Full`, frame dropped |
| Dropped frames not counted in `audio_intervals_received` | missing | Integration | Send burst of 70 intervals in rapid succession; verify stat counts ≤ 64 (some may have been queued) |
| IPC channel at capacity drops plugin frame | missing | Integration | Fill `ipc_from_plugin_tx`, send one more; verify drop log and no panic |

---

## 6. Failure Detection and Reconnection

### 6.1 Failure Detection

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| DC `on_close` fires → `PeerFailed` emitted | covered | Integration | `peer_failure_emits_event`, `peer_failure_detected_within_timeout` |
| Reader task exit → `PeerFailed` emitted | covered | Integration | Same tests — reader exits when DC closes |
| `RTCPeerConnectionState::Failed` → `failure_tx` fires | missing | Integration | Simulate ICE failure (no candidates work); verify connection-state callback fires and reaches `poll_signaling` |
| Duplicate `PeerFailed` signals deduplicated | missing | Integration | After `close_peer`, let both DC and reader fire; verify `MeshEvent::PeerFailed` emitted exactly once (second maps to `SignalingProcessed`) |
| Liveness watchdog fires after 30s of silence | missing | Integration | Connect two peers, stop peer A from sending any sync messages; after 30s B's watchdog should close the connection and trigger `PeerFailed` |
| Liveness watchdog uses sync messages, not audio | missing | Integration | Peer sends audio but no sync messages for 30s; verify watchdog still fires. Contrasts with a peer that sends sync messages — should NOT be evicted |

### 6.2 Peer Reconnection

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| `re_initiate` removes dead peer and sends new offer | covered | Integration | `peer_reconnects_after_close` |
| Audio flows after reconnection | covered | Integration | `peer_reconnects_after_close` |
| New offer received replaces stale connection | covered | Integration | `new_offer_replaces_stale_connection` |
| Exponential backoff: 2s, 4s, 8s, 16s, 16s | missing | Unit | Mock `reconnect_tx`/`reconnect_rx` timer; verify delays follow schedule |
| After `MAX_PEER_RECONNECT_ATTEMPTS` failures, `PeerLeft` emitted and state cleared | missing | Integration | Force 5 consecutive failures; verify `peer:left` event fires, slots freed |
| Higher-ID peer waits for lower-ID peer's offer on reconnect | missing | Integration | Force failure from higher-ID peer's side; verify higher-ID peer calls `re_initiate` but does NOT send an offer (lower-ID is responsible) |
| `peer_reconnect_attempts` cleared on successful Hello | missing | Integration | Reconnect successfully; verify `peer_reconnect_attempts.remove(&pid)` clears state |
| `PeerLeft` arrives during reconnection backoff — backoff aborted | missing | Integration | Start reconnect timer, then receive `PeerLeft` from signaling; verify `reconnect_rx` handler skips because key removed from map |

### 6.3 Signaling Reconnection

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| Signaling channel close triggers reconnect loop | partial | Unit | `signaling_eviction_closes_channel` only tests JSON parsing; no test for the full session-loop handling |
| Signaling reconnect succeeds on second attempt (backoff 1000ms, 2000ms, ...) | missing | Integration | Shut down the test signaling server temporarily; restart after 2s; verify session reconnects |
| `session:stale` emitted after 10 failed attempts | missing | Integration | Keep signaling server down for enough time; verify event is emitted at attempt 10 |
| After signaling reconnect, peer state cleared and beat sync re-required | missing | Integration | Reconnect successfully; verify `peer_names` cleared, `beat_synced = false`, gate is re-enabled |
| `slot_affinity` preserved across signaling reconnect | missing | Integration | Verify that peer slots are re-assigned by affinity after reconnect |
| `Disconnect` command during signaling reconnect stops cleanly | missing | Integration | Send `Disconnect` while in the reconnect backoff loop; verify session exits |
| TURN credentials re-fetched on signaling reconnect | missing | Integration | Verify `fetch_metered_ice_servers` called once per session start AND once per signaling reconnect |

---

## 7. Multi-Peer Scenarios

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| Three peers all connect, each hears the other two | missing | Integration | Add a third mesh to existing 2-peer tests; verify A→B, A→C, B→A, B→C, C→A, C→B all work |
| Peer D joins existing 3-peer room | missing | Integration | D should initiate connections only to peers with higher `peer_id` than D (or be the target of offers from lower-ID peers) |
| One peer leaves a 3-peer room; remaining two continue | missing | Integration | Verify `PeerLeft` event, remaining mesh functional, audio still flows |
| Two peers join simultaneously (tie-breaking race) | missing | Integration | Both A and B join within the same poll tick; only one offer should be created |
| Broadcast to all peers: all receive | missing | Integration | 3+ peer room; A broadcasts audio; verify B and C both receive it |
| Room at capacity (8 peers); 9th peer rejected | missing | Integration | Fill room with 8 peers; 9th join returns 409 |

---

## 8. IPC / Plugin Integration

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| Send plugin audio flows through IPC → WebRTC → IPC → Recv plugin | covered | Integration | `plugin_ipc_to_webrtc_to_plugin_ipc` |
| Multiple full-size intervals transmitted | covered | Integration | `multi_interval_full_size_e2e` |
| Only recv-role plugin receives forwarded audio | covered | Integration | `dual_plugin_ipc_only_recv_gets_audio` |
| Recv plugin plays back audio (CLAP E2E) | covered | Integration | `recv_plugin_e2e` (requires plugin bundle) |
| Send plugin connects mid-session (not at startup) | missing | Integration | Start session, send initial intervals without send plugin; connect send plugin after 5s; verify audio starts flowing |
| Send plugin disconnects mid-session | missing | Integration | Drop TCP connection from send plugin side; verify session continues, `plugin:disconnected` event fires, no panic |
| Recv plugin disconnects mid-session; reconnects | missing | Integration | Disconnect recv writer; send audio; reconnect recv plugin; verify IPC writer restored |
| Multiple recv plugins both receive audio | missing | Integration | Connect two recv plugin clients to the same app; verify both receive every interval |
| Legacy send plugin (no `stream_index` byte) handled by 200ms timeout | missing | Integration | Connect as send plugin but don't send the `stream_index` bytes; verify session uses `stream_index = 0` after timeout |
| IPC read error on recv writer removes it from `ipc_recv_writers` | missing | Integration | Force a write error; verify dead writer cleaned from the list |
| `plugin:connected` and `plugin:disconnected` events emitted correctly | missing | Integration | Verify Tauri events fire on connect/disconnect |
| `stream_index` identifies plugin streams separately | missing | Integration | Connect two send plugins with `stream_index` 0 and 1; verify `AudioWire.stream_id` matches |

---

## 9. State Machine and Status Reporting

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| Peer status: `connecting` before Hello received | covered | Unit | `status_connecting_when_no_hello` |
| Peer status: `reconnecting` overrides all | covered | Unit | `status_reconnecting_overrides_other_states` |
| Peer status: `full-duplex` when sending and receiving | covered | Unit | `status_full_duplex` |
| Peer status transitions logged as `old → new` | missing | Integration | Drive a session through `connecting → connected → full-duplex`; verify log messages emitted via `ui_info!` |
| `StatusUpdate` emitted every 2s | missing | Integration | Start session, capture events; verify at least 2 `status:update` events within 5s |
| `StatusUpdate.audio_dc_open` reflects actual DC state | missing | Integration | Verify field is `false` before connection, `true` after |
| `StatusUpdate.audio_send_gated` reflects gate state | missing | Integration | Verify field is `true` when joiner is gated, `false` after beat sync |
| RTT displayed for connected peer | missing | Integration | After Ping/Pong exchange, verify `PeerInfo.rtt_ms` is Some and > 0 |

---

## 10. Clock Synchronization

| Scenario | Status | Technique | Notes |
|----------|--------|-----------|-------|
| Ping IDs increment monotonically | covered | Unit | `ping_generates_incrementing_ids` |
| Pong echoes correct fields | covered | Unit | `handle_ping_returns_pong_with_correct_fields` |
| Unknown peer returns None for RTT | covered | Unit | `rtt_none_for_unknown_peer` |
| Pong establishes RTT | covered | Unit | `pong_establishes_rtt` |
| Multiple pongs converge to correct RTT | covered | Unit | `rtt_converges_with_multiple_pongs` |
| Sliding window: only last 8 samples used | covered | Unit | `sliding_window_only_uses_last_8_samples` |
| Jitter: outlier does not dominate median RTT | covered | Unit | `jitter_outlier_does_not_dominate_median_rtt` |
| Clock offset not computed — only RTT tracked | note | — | Offset computation removed: ClockSync and Link timestamps are different clock domains |

---

## 11. Edge Cases in the Protocol Layer

These scenarios map directly to the "Known Edge Cases" in `docs/networking.md`:

| Scenario | Section | Status | Technique |
|----------|---------|--------|-----------|
| Reassembly buffer has no timeout; partial message leaks across reconnect | §13.A | missing | Unit: build a partial reassembly state, then simulate a new connection arriving with fresh data; verify old partial buffer is not mixed with new message |
| ICE candidate race: candidates arrive before answer (ordering) | §13.B | partial | See section 2.2 above |
| Hello queued for responder before DC open | §13.C | missing | See section 4.1 above |
| Higher-ID peer stuck waiting forever after re_initiate | §13.D | missing | Integration: after 5 failures, verify PeerLeft clears the state correctly |
| Signaling reconnect blocks the event loop | §13.E | missing | Integration: while reconnecting, verify Link events are not dropped (check beat counter advancement post-reconnect) |
| "All-gated deadlock" with simultaneous join | §13.F | missing | See AudioSendGate section |
| TURN credential expiry mid-session | §13.G | missing | See section 2.2 above |
| Outgoing signal queue capped at 5/tick | §13.H | missing | See section 1.2 above |
| Liveness watchdog uses sync not audio | §13.I | missing | See section 6.1 above |
| Audio channel drop invisible to UI | §13.J | missing | See section 5.4 above |

---

## 12. Load and Performance

| Scenario | Technique | Notes |
|----------|-----------|-------|
| Sustained session (1 hour, 2 peers, audio every interval) | Long-running Integration | Use test tone, run in CI nightly; verify no memory growth, no connection drops |
| High-volume audio: 8 peers × 2 streams each | Load Integration | Use in-process signaling + `relay_only=false`; measure throughput, drops |
| Signaling server under concurrent load | Load | Spawn 50 parallel join requests; verify no race conditions in SQLite writes |
| Large backlog on bounded audio channel: receiver too slow | Load | Introduce artificial delay on `audio_rx` consumer; verify frames dropped gracefully, no deadlock |
| Memory: `pending_sync` queue grows without bound if DC never opens | missing | Unit/Load: Never open the DC (no ICE), send 10,000 sync messages; verify queue is bounded or monitored |

---

## 13. Manual / DAW Integration Testing

These scenarios require a real DAW with the CLAP/VST3 plugin installed and cannot easily be automated.

### Happy path sessions

1. **Two machines on the same LAN, no TURN:**
   - Enable WAIL, join same room, observe `full-duplex` status, play audio, verify both sides hear each other with ~1-interval latency.

2. **Two machines over the internet via TURN:**
   - Use Metered TURN. Verify connection establishes. Measure RTT displayed in UI.

3. **Three-peer session:**
   - Three musicians join. Each should hear the other two. Verify slot assignment in recv plugin (each peer's audio routed to a separate bus).

4. **Late join — beat sync:**
   - A and B are already playing. C joins mid-song. Verify C's audio is gated until it snaps to the current beat, then C's audio aligns with the interval.

### Failure and recovery

5. **Network interrupt on one peer:**
   - Unplug ethernet cable (or kill Wi-Fi) for 5s. Reconnect. Verify WAIL reconnects without manual intervention. Verify audio resumes within 1–2 interval lengths.

6. **Signaling server unreachable mid-session:**
   - Drop signaling HTTP traffic (e.g., block in firewall). Audio should continue flowing via WebRTC (no signaling needed after connection). After 30s, verify WAIL detects eviction (stale heartbeat) and attempts reconnection to signaling.

7. **Peer closes DAW mid-session:**
   - Kill DAW process abruptly (not graceful close). Remaining peers should detect the failure and show `reconnecting`, then give up and show the peer as left.

8. **Plugin crashes or unloads:**
   - Force-unload the WAIL Send plugin in the DAW. Verify app detects IPC disconnect, emits `plugin:disconnected`, but session (WebRTC) stays alive.

### Audio quality checks

9. **Test tone fidelity:**
   - Enable test tone. Verify sine wave arrives at remote recv plugin with recognizable pitch. Check for artefacts (clicks at interval boundaries, sample-rate mismatches).

10. **Opus quality at various BPMs:**
    - Very low BPM (60 BPM, 4 bars = 16s intervals): very large Opus payloads, many chunks.
    - Very high BPM (240 BPM, 4 bars = 4s intervals): rapid interval boundaries.

11. **Multichannel / multi-stream:**
    - Connect two send plugins (stream 0 and stream 1). Verify recv plugin routes them to separate aux outputs.

---

## 14. Test Infrastructure Gaps

The following are missing capabilities in the test harness that block automation of several scenarios above:

| Gap | Recommendation |
|----|----------------|
| In-process signaling server does not implement password, version, or capacity checks | Extend `start_test_signaling_server()` to accept configuration flags (capacity, min_version, password_hash) |
| No way to simulate `evicted: true` from the test signaling server | Add an `evict_peer(peer_id)` method to `SignalingState` |
| No way to inject HTTP errors or rate limits into the signaling client | Add a middleware layer to the test server that can return 429 or drop requests |
| No way to control signaling poll timing without the real `poll_interval_ms` parameter | Expose a test hook to force an immediate poll |
| `session_loop` is not directly testable without a full Tauri `AppHandle` | Extract session logic into a struct with a testable interface, or use a mock `AppHandle` |
| Clock offset computation removed — only RTT tracked | No offset code exists to test; RTT tests are fully covered in `clock_tests.rs` |
| No way to observe `peer_reconnect_attempts` count from outside the session loop | Add a status field to `StatusUpdate` for reconnect attempt count, or use log assertion helpers |

---

## 15. Test Execution Guide

```sh
# Run all unit + integration tests (requires plugin bundle for recv_plugin_e2e)
cargo xtask build-plugin
cargo test

# Run only networking integration tests (fast, no external deps)
cargo test -p wail-net

# Run TURN tests (requires coturn: brew install coturn)
cargo test -p wail-net -- --ignored two_peers_exchange_audio_via_turn

# Run live Metered tests (requires internet)
cargo test -p wail-net -- --ignored fetch_metered_ice_servers_live metered_turn_relay_live

# Run with verbose logging
RUST_LOG=debug cargo test -p wail-net -- --nocapture

# Run the IPC E2E test suite
cargo test -p wail-net --test ipc_e2e

# Run the plugin E2E test (requires .clap bundle)
cargo test -p wail-plugin-test
```
