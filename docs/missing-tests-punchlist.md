# Missing Tests Punch List

Cross-reference of `docs/test-strategies.md` against the actual test functions found in the codebase (~160 tests across wail-core, wail-audio, wail-net, wail-tauri, wail-plugin-test). Every scenario marked `missing` or `partial` in test-strategies.md is listed here, grouped by what infrastructure is needed to write it.

---

## Part 1: Infrastructure Gaps (Fix First)

These are missing capabilities in the test harness documented in §14 of test-strategies.md. Each blocks multiple downstream tests.

| Gap | Where to fix | Unblocks |
|-----|-------------|---------|
| Test signaling server ignores password, version, and capacity | `crates/wail-net/tests/common/mod.rs` — extend `start_test_signaling_server()` | All §1.1 join scenarios |
| No `evict_peer(peer_id)` method on test server | Add to `SignalingState` in same file | §1.2 eviction, §6.3 signaling reconnect |
| No way to inject 429 or HTTP errors into signaling client | Add middleware flag to test server | §1.2 rate-limit backoff |
| No way to force an immediate poll (bypass timer) | Expose a test hook on `SignalingClient` | §1.2 poll timing tests |
| `session_loop` untestable without a full Tauri `AppHandle` | Extract session logic into a testable struct, or mock `AppHandle` | §4, §5.1, §6, §8, §9 |
| `peer_reconnect_attempts` not observable from outside | Add field to `StatusUpdate` or a log assertion helper | §6.2 reconnection tests |

---

## Part 2: Quick Wins (No Infrastructure Changes Needed)

Unit tests that can be written against existing code today.

### §3.2 Audio Chunking and Reassembly
File: `crates/wail-net/src/peer.rs` (or a new unit module)

- [ ] Message of exactly 1200 bytes is sent without a WACH header
- [ ] Message of 1201 bytes is chunked (WACH header present, two chunks emitted)
- [ ] First chunk arrives → reassembly buffer holds state, nothing emitted downstream
- [ ] Second chunk arrives → complete message emitted
- [ ] Non-chunked message (no WACH magic) passes through reassembly as-is
- [ ] Reassembly buffer fully reset after a complete message (second message is not corrupted by first)

### §5.2 Wire Format Edge Cases
File: `crates/wail-audio/src/wire.rs`
*(Note: `wire_roundtrip` and `wire_rejects_bad_magic` in `pipeline.rs` already cover the roundtrip and magic mismatch cases.)*

- [ ] Very short interval (0 or 1 sample) — `encode` does not panic
- [ ] Very long interval (60s at 60 BPM) — encode/decode round-trip, no integer overflow
- [ ] Unknown version byte — `decode` returns graceful `Err`, not a panic

### §3.1 DataChannel Message Queuing (Unit)
File: unit test for `pending_sync` in `crates/wail-net/src/peer.rs`

- [ ] `pending_sync` queue drained in FIFO order on DC open (push 3 messages, trigger flush, assert order)

### §2.3 Tie-Breaking
File: `crates/wail-net`

- [ ] `peer_id == remote_id` — verify `initiate_connection` is NOT called and there is no deadlock

### §12 Load
File: `crates/wail-net` or `crates/wail-audio`

- [ ] DC never opens: send 10,000 sync messages to `pending_sync`; verify queue is bounded or does not OOM

---

## Part 3: Integration Tests (Existing Infrastructure Sufficient)

These use the existing `start_test_signaling_server` + `establish_connection` helpers in `crates/wail-net/tests/`.

### §2.2 ICE Edge Cases
File: `crates/wail-net/tests/network.rs`

- [ ] Metered API unreachable → falls back to `metered_stun_fallback()` (stub HTTP client to return a network error)
- [ ] ICE gathering completes but no viable pair → `RTCPeerConnectionState::Failed` fires `failure_tx`

### §3.1 Message Queuing Integration
File: `crates/wail-net/tests/network.rs`

- [ ] Hello sent before DC open is queued and flushed — assert Hello arrives after DC transitions to Open
- [ ] Large queue (50 messages before DC open) — all 50 arrive in order after DC opens

### §6.1 Failure Detection
File: `crates/wail-net/tests/network.rs`

- [ ] `RTCPeerConnectionState::Failed` → `failure_tx` fires (use a candidate filter that drops everything)
- [ ] Duplicate `PeerFailed` signals → `MeshEvent::PeerFailed` emitted exactly once; second becomes `SignalingProcessed`

### §6.2 Peer Reconnection
File: `crates/wail-net/tests/network.rs`

- [ ] Exponential backoff delays follow schedule: 2 s, 4 s, 8 s, 16 s, 16 s (mock timer)
- [ ] After 5 failures (`MAX_PEER_RECONNECT_ATTEMPTS`) → `PeerLeft` emitted, slots freed, state cleared
- [ ] Higher-ID peer calls `re_initiate` → does NOT create an offer (waits for lower-ID peer)
- [ ] `peer_reconnect_attempts` cleared after successful Hello exchange on the new connection
- [ ] `PeerLeft` arrives during backoff → reconnect timer skips because key has been removed from map

### §7 Multi-Peer
File: `crates/wail-net/tests/network.rs`

- [ ] Three peers connect; audio flows on all six directed paths (A→B, A→C, B→A, B→C, C→A, C→B)
- [ ] One peer leaves a 3-peer room; remaining two continue exchanging audio
- [ ] Broadcast: A sends audio, B and C both receive it

---

## Part 4: Tests Requiring New Infrastructure

These are blocked by one or more items in Part 1. Build the relevant gap first.

### §1.1 Join / Room Management
*Requires: extended test signaling server*

- [ ] `client_version` below `MIN_CLIENT_VERSION` → 426 + `min_version` in body
- [ ] `client_version` equal to minimum → 200 (boundary condition)
- [ ] Wrong password → 401
- [ ] Correct password → 200
- [ ] Room at capacity (`8 × stream_count` slots filled) → 409
- [ ] `display_name` is present in the `PeerJoined` message received by existing peers
- [ ] Room row deleted when last peer leaves; third peer can recreate with a different password
- [ ] `stream_count > 1` consumes multiple slots; capacity enforced per stream slot

### §1.2 Polling and Heartbeat
*Requires: extended test server + poll timing hook*

- [ ] `poll` updates `last_seen`; peer still present after 25 s
- [ ] Stale peer removed after 30 s; remaining peer receives `PeerLeft{B}`
- [ ] `evicted: true` in poll response → `incoming_rx` closes → session reconnect triggered
- [ ] Messages older than 60 s deleted; not re-delivered on late poll
- [ ] `after` sequence number prevents duplicate delivery across two consecutive polls
- [ ] 429 response → `current_poll_ms` doubles; resets to base interval on next 200
- [ ] At most 5 outgoing signals sent per poll tick; remainder held for next tick
- [ ] Outgoing channel close → `?action=leave` sent to signaling server

### §1.3 Public Room Listing
*Requires: extended test server*

- [ ] `?action=list` returns rooms with peer counts and BPM
- [ ] Password-protected rooms are absent from the public list

### §4.1 Hello Exchange
*Requires: session-level test harness*

- [ ] Initiator sends Hello on `PeerJoined`; responder receives it and replies
- [ ] `hello_sent` guard: `PeerJoined` for the same peer twice → Hello sent only once
- [ ] After reconnection, Hello is resent on the new connection
- [ ] `identity` field stored in `peer_identities` map and used for slot affinity

### §4.2 Tempo Sync and Echo Suppression
*Requires: session-level test harness*

- [ ] Remote `TempoChange` applied to Link without looping back as an outgoing `TempoChange`
- [ ] Echo guard (150 ms) suppresses `TempoChanged` rebroadcast
- [ ] BPM delta < 0.01 → no `TempoChange` message sent
- [ ] BPM delta = 0.01 exactly → IS broadcast (boundary condition)
- [ ] Simultaneous tempo change from two peers → both settle on the same value, no infinite loop

### §4.3 Beat Sync and StateSnapshot
*Requires: session-level test harness*

- [ ] First `StateSnapshot` → `beat_synced = true`, gate lifted, `ForceBeat` called
- [ ] `ForceBeat(10.0)` → Link reports beat ≈ 10.0
- [ ] Subsequent StateSnapshots do not re-snap beat (ForceBeat called exactly once total)
- [ ] StateSnapshot BPM differs > 0.01 from `last_broadcast_bpm` → `SetTempo` called

### §4.4 Interval Boundaries
*Requires: session-level test harness*

- [ ] `IntervalBoundary` from peer ahead → `interval.sync_to(index)` called; local tracker catches up
- [ ] `IntervalConfig` mid-session → `interval.beats_per_interval()` changes to reflect new bars/quantum

### §5.1 AudioSendGate Integration
*Requires: session-level test harness*

- [ ] Gate active → `broadcast_audio` call reaches no remote peer
- [ ] Simultaneous join (both peers see `n = 1`) → both exchange StateSnapshots and both lift gate

### §5.3 Slot Assignment (Session Level)
*Requires: session-level test harness*
*(Note: ring.rs already covers slot logic at the ring buffer level; these verify session-level wiring.)*

- [ ] Three peers join → assigned slots 0, 1, 2 in arrival order
- [ ] Peer leaves → slot freed in `slot_occupied`
- [ ] Peer rejoins with same identity → same slot assigned via affinity
- [ ] 32nd peer when all slots full → no slot assigned, no panic
- [ ] `slot_affinity` preserved across a signaling reconnect

### §5.4 Channel Backpressure
*Requires: session-level test harness*

- [ ] `audio_tx` at capacity 64 → next frame dropped, debug log emitted
- [ ] Dropped frames are not counted in `audio_intervals_received`
- [ ] `ipc_from_plugin_tx` at capacity → drop logged, no panic

### §6.3 Signaling Reconnection
*Requires: `evict_peer` + session-level test harness*

- [ ] Signaling channel close (eviction) → session enters reconnect loop
- [ ] Reconnect succeeds on second attempt after transient server outage
- [ ] `session:stale` event emitted after 10 failed reconnect attempts
- [ ] After successful reconnect: `peer_names` cleared, `beat_synced = false`, gate re-enabled
- [ ] `Disconnect` during reconnect backoff → session exits cleanly
- [ ] TURN credentials re-fetched on signaling reconnect (`fetch_metered_ice_servers` called again)

### §8 IPC / Plugin Integration
*Requires: session-level test harness*

- [ ] Send plugin connects mid-session → audio starts flowing (not only at startup)
- [ ] Send plugin disconnects mid-session → `plugin:disconnected` event fired, session continues
- [ ] Recv plugin disconnects then reconnects → IPC writer restored, audio resumes
- [ ] Two recv plugins connected simultaneously → both receive every interval
- [ ] Legacy send plugin (no `stream_index` byte) → `stream_index = 0` used after 200 ms timeout
- [ ] IPC write error on recv writer → dead writer removed from `ipc_recv_writers`
- [ ] `plugin:connected` and `plugin:disconnected` Tauri events fired on connect/disconnect
- [ ] Two send plugins with `stream_index` 0 and 1 → `AudioWire.stream_id` matches each

### §9 State Machine and Status Reporting
*Requires: session-level test harness*

- [ ] Status transitions logged as `"old → new"` via `ui_info!`
- [ ] At least two `status:update` events emitted within 5 s
- [ ] `StatusUpdate.audio_dc_open` is `false` before connection, `true` after
- [ ] `StatusUpdate.audio_send_gated` is `true` while gated, `false` after beat sync
- [ ] `PeerInfo.rtt_ms` is `Some` and `> 0` after a Ping/Pong exchange

---

## Summary

| Category | Count |
|----------|------:|
| Infrastructure gaps to fix | 6 |
| Quick-win unit tests | ~10 |
| Integration (existing infra) | ~13 |
| Requires new/extended infra | ~55 |
| **Total missing** | **~84** |

## Recommended Order

1. **Quick-win unit tests** — chunking/reassembly, wire edge cases, clock sliding window. Zero infra work.
2. **Extend the test signaling server** — unlocks all §1 signaling tests and several §6.3 tests.
3. **Add ICE failure and multi-peer integration tests** — wail-net level, existing infra.
4. **Extract `session_loop` into a testable struct** — the highest-leverage unlock; covers §4, §5, §6, §8, §9.
