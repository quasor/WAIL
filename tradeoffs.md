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
**File:** `signaling-server/main.go`
**Problem:** No rate limiting on the WebSocket signaling server.
**Fix when ready:** Add rate limiting via middleware or per-connection message throttling.

---

## Deferred — Feature Work

### W15. Clock offset computation removed
**Status:** Done — dead code removed
**File:** `crates/wail-core/src/clock.rs`
**Rationale:** Link timestamps (`link.clock_micros()`) and ClockSync timestamps (`Instant::now()`) are different clock domains. Offset computation was dead code — it was never applied to anything. `ClockSync` now tracks RTT only (`VecDeque<i64>` of RTT samples per peer), using a median over the last 8 samples. RTT is available via `rtt_us(peer_id)` for diagnostics.

---

## Skipped

### C4. Signaling server has no rate-limiting
**Status:** Partially addressed — room passwords + capacity check added
**File:** `signaling-server/main.go`
**Problem:** No rate limiting, no per-connection message throttling.
**Rationale:** Room passwords prevent unauthorized joins. 32-slot capacity check prevents room overflow. Rate limiting to revisit when needed.

---

## Design Decisions

### TempoChangeDetector extraction
**Status:** Done
**File:** `crates/wail-core/src/link.rs`
**Decision:** Extracted tempo-change detection logic (threshold check + echo guard state machine) from `LinkBridge` into a separate `pub(crate) TempoChangeDetector` struct. `LinkBridge` delegates to it. The detector accepts `Instant` as a parameter for deterministic testing without `AblLink` (C FFI + CMake). Integration testing of the full `LinkBridge` → `AblLink` path is deferred to e2e tests.

---

## Polish (low priority)

| ID | Item | File | Status |
|----|------|------|--------|
| I4 | Single STUN server, no TURN (symmetric NATs will fail) | `crates/wail-net/src/peer.rs:60` | Open |
| I5 | `now_us()` cast u128→i64 overflows after 292 years | `crates/wail-core/src/clock.rs:36` | Open |
| I6 | Median uses upper-median for even-length arrays | `crates/wail-core/src/clock.rs:87` | Open |
| I7 | Echo guard 150ms window suppresses legit fast tempo changes | `crates/wail-core/src/link.rs:89-94` | Open |
| I9 | DAW aux output ports show "Slot 1–31" instead of actual peer display names | `crates/wail-plugin-recv/src/lib.rs` | Fixed (CLAP only) |

### I9. Dynamic peer names in DAW aux outputs
**Status:** Fixed for CLAP hosts — VST3 has no equivalent API
**File:** `crates/wail-plugin-recv/src/lib.rs`, nih_plug fork at `MostDistant/nih-plug@feat/dynamic-audio-port-names`
**Solution:** Forked nih_plug to add `ProcessContext::set_aux_output_name()` + `rescan_audio_port_names()` which call CLAP's `host.audio_ports->rescan(CLAP_AUDIO_PORTS_RESCAN_NAMES)`. Added `IPC_TAG_PEER_NAME` message type to forward display names from the Tauri session to the recv plugin. When a peer sends Hello with a display name, the session broadcasts it via IPC, the plugin updates the dynamic port name, and triggers a host rescan.
**Limitation:** VST3 hosts will still show static "Slot 1–31" names — VST3 has no bus rename API.
