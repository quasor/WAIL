# Trade-off Log

Deferred decisions and remaining code quality items. Each entry has enough context to review and adjust without re-reading the code.

## Completed

| ID | Fix | Commit |
|----|-----|--------|
| C1 | `unwrap()` in signaling client → match + log + continue | 470c1fa |
| C2 | Mutex poison in plugin `initialize()` → return false to DAW | 470c1fa |
| C3 | Silent Opus init failure → `warn!` log when encoder/decoder is None | 470c1fa |
| C5 | Division by zero in interval tracking → clamp bars≥1, quantum≥ε | 470c1fa |
| W4 | Volume param labeled "dB" but is linear 0–1 → removed misleading unit | f4c1f83 |
| W7 | Integer overflow in clock offset → saturating arithmetic + negative RTT guard | f4c1f83 |
| W8 | `Vec::remove(0)` in clock sync → `VecDeque` + `pop_front()` | f4c1f83 |
| W9 | Dead code warnings → `#[allow(dead_code)]` with comments, removed unused imports/mut | f4c1f83 |
| W13 | Unused `WailTask` enum → removed, `type BackgroundTask = ()` | f4c1f83 |
| I8 | Redundant `use serde_json;` in signaling server → removed | f4c1f83 |
| W5 | `let _ =` send failures → tiered logging: `warn!` for critical paths, `debug!` for hot paths | 67a02c2 |
| W12 | DataChannel send() silent when None → `debug!` log when channel not ready | 67a02c2 |
| W17 | `mem::replace` receiver swap → `Option<Receiver>` with `.take()` methods | 67a02c2 |
| W2 | Plugin hardcoded 128kbps bitrate → passes `bitrate_kbps` param through | 67a02c2 |
| W3 | Plugin hardcoded 2 channels → derives from `audio_io_layout` | 67a02c2 |
| I1 | No `Default` for `ClockSync` → added `impl Default` | 67a02c2 |
| I2 | No `Default` for `IpcRecvBuffer` → added `impl Default` | 67a02c2 |
| I3 | Magic number `10` for snapshot interval → `SNAPSHOT_INTERVAL_TICKS` constant | 67a02c2 |
| W1 | Duplicate AudioBridge → deleted old bridge, plugin uses `wail_audio::AudioBridge` | 085a16e |
| W14 | Audio IPC not wired → TCP IPC between plugin and app, bidirectional audio intervals | 085a16e |
| W6 | Unbounded audio channels → bounded(64) with drop-on-full for 3 audio channels; sync/signaling/ICE left unbounded | 36272e4 |

---

## Deferred — Infrastructure (revisit when deploying)

### W6. Unbounded channels (sync/signaling/ICE — 6 remaining instances)
**Status:** Partially fixed — audio channels bounded, sync/signaling left unbounded
**Files:** `crates/wail-net/src/signaling.rs:33-34`, `crates/wail-net/src/peer.rs:67,100,162`, `crates/wail-core/src/link.rs:120-121`
**Problem:** Remaining sync/signaling channels are unbounded. Messages are tiny JSON structs at low frequency.
**Decision:** Low risk — leave until scale demands it. Audio channels (the real risk) are now bounded(64) with `try_send` + drop-on-full.

### W10. No graceful shutdown
**Status:** Deferred — infrastructure concern
**File:** `crates/wail-app/src/main.rs`
**Problem:** Binary doesn't handle SIGINT/SIGTERM. Process just dies on Ctrl+C.
**Fix when ready:** Add `tokio::signal::ctrl_c()` branch in `select!` loop.

### W11. No reconnection logic
**Status:** Completed
**File:** `crates/wail-net/src/lib.rs`, `crates/wail-tauri/src/session.rs`
**Problem:** Signaling server disconnect and WebRTC peer failures killed the session permanently.
**Resolution:** Implemented automatic reconnection for both:
- **WebRTC peers:** `MeshEvent::PeerFailed` detection via connection state callbacks, `re_initiate()` with exponential backoff (2s–16s, max 5 attempts), UI events (`peer:reconnecting`).
- **Signaling server:** Reconnect loop with exponential backoff (1s–30s, unlimited attempts), re-fetches ICE servers, replaces PeerMesh.
- **Tests:** `peer_failure_emits_event`, `peer_reconnects_after_close`, `new_offer_replaces_stale_connection` in `crates/wail-net/tests/network.rs`.

### W16. Signaling server has no rate limiting
**Status:** Deferred — infrastructure concern
**File:** `val-town/signaling.ts`
**Problem:** No rate limiting on the HTTP signaling endpoint.
**Fix when ready:** Add rate limiting at the Val Town level or via middleware.

---

## Deferred — Feature Work

### W18. Interval index drifts by 1 when Link sessions have different absolute beat counts
**Status:** Fixed for join-time via one-shot `ForceBeat`; residual warn suppressed by monotonic `update()`. Residual drift under high-latency still possible.
**File:** `crates/wail-app/src/main.rs`, `crates/wail-core/src/interval.rs`
**Problem:** Link syncs tempo and phase but not absolute beat count. On the first `StateSnapshot` received, WAIL calls `forceBeatAtTime` to snap the local beat clock to the remote's. This resolves the join-time offset. Residual `IntervalBoundary` mismatches are handled by monotonic `sync_to` without flapping.
**Known limitation — same-LAN disruption:** `forceBeatAtTime` propagates to all LAN Link peers. If the joining peer (C) and the existing peers (A, B) share a LAN, C's snap will jolt A and B by approximately `RTT/2 * BPM/60` beats. At 100ms RTT, 120 BPM this is ~0.1 beats — imperceptible. At 500ms RTT it's ~0.5 beats — audible. In the typical WAIL use case (musicians on separate LANs), there is no cross-LAN Link interaction so no disruption occurs.
**Known limitation — latency compensation:** ForceBeat uses `link.clock_micros()` (local time, now) and `remote_beat` (remote time, past). The beat value is slightly stale by `RTT/2`. A future improvement could add `RTT/2 * BPM/60` beats to compensate, once ClockSync is wired to the Link clock domain.

### W15. Clock offset computed but never applied
**Status:** Won't fix — clock domain mismatch
**File:** `crates/wail-app/src/main.rs:150-153`
**Problem:** Clock sync computes per-peer offset but never uses it to adjust beat timestamps.
**Rationale:** Link timestamps (`link.clock_micros()`) and ClockSync timestamps (`Instant::now()`) are different clock domains. Applying ClockSync offsets to Link timestamps would be incorrect. ClockSync RTT remains useful for diagnostics (displaying latency to peers). Interval boundaries are computed independently per peer from local beat position — this is intentional for NINJAM semantics where drift up to 1 interval is tolerable.

---

## Skipped

### C4. Signaling server has no rate-limiting
**Status:** Partially addressed — room passwords added
**File:** `val-town/signaling.ts`
**Problem:** No rate limiting, no per-room peer limits.
**Rationale:** Room passwords prevent unauthorized joins. Rate limiting and peer caps to revisit when needed.

---

## Polish (low priority)

| ID | Item | File | Status |
|----|------|------|--------|
| I4 | Single STUN server, no TURN (symmetric NATs will fail) | `crates/wail-net/src/peer.rs:60` | Open |
| I5 | `now_us()` cast u128→i64 overflows after 292 years | `crates/wail-core/src/clock.rs:36` | Open |
| I6 | Median uses upper-median for even-length arrays | `crates/wail-core/src/clock.rs:87` | Open |
| I7 | Echo guard 150ms window suppresses legit fast tempo changes | `crates/wail-core/src/link.rs:89-94` | Open |
| I9 | DAW aux output ports show "Peer 1–15" instead of actual peer display names | `crates/wail-plugin-recv/src/lib.rs:83-87` | Open |

### I9. Static peer names in DAW aux outputs
**Status:** Open — plugin API limitation
**File:** `crates/wail-plugin-recv/src/lib.rs:83-87`
**Problem:** DAW shows "Peer 1", "Peer 2", etc. instead of the peer's chosen display name (e.g. "Ringo"). Peer display names already flow through the protocol (`SyncMessage::Hello.display_name`) and are tracked in the Tauri session, but cannot be surfaced in DAW port labels.
**Root cause:** nih_plug `PortNames` are `&'static str` (compile-time only). VST3 has no bus rename API. CLAP has `host.audio_ports->rescan(RESCAN_NAMES)` but nih_plug doesn't expose it.
**Workaround:** Show "Peer 1 = Ringo" mapping in wail-app UI so users know which aux output corresponds to which musician.
**Fix when ready:** Upstream nih_plug enhancement to expose CLAP's `rescan(RESCAN_NAMES)`, or use CLAP's `extensible_audio_ports` draft extension for dynamic port creation with correct names.
