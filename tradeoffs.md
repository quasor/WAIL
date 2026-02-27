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
| W5 | `let _ =` send failures → tiered logging: `warn!` for critical paths (signaling, Link events), `debug!` for hot paths. Link poller breaks on receiver drop. | pending |
| W12 | DataChannel send() silent when None → `debug!` log when channel not ready | pending |
| W17 | `mem::replace` receiver swap → `Option<Receiver>` with `.take()` methods | pending |
| W2 | Plugin hardcoded 128kbps bitrate → passes `bitrate_kbps` param through | pending |
| W3 | Plugin hardcoded 2 channels → derives from `audio_io_layout` | pending |
| I1 | No `Default` for `ClockSync` → added `impl Default` | pending |
| I2 | No `Default` for `IpcRecvBuffer` → added `impl Default` | pending |
| I3 | Magic number `10` for snapshot interval → `SNAPSHOT_INTERVAL_TICKS` constant | pending |

---

## Deferred — Infrastructure (revisit when deploying)

### W1. Duplicate AudioBridge implementations
**Status:** Deferred — migrate when wiring audio IPC (W14)
**Files:** `crates/wail-plugin/src/audio_bridge.rs` (old) vs `crates/wail-audio/src/bridge.rs` (new)
**Problem:** Two bridge implementations with different APIs. Plugin uses old one. New one in wail-audio uses IntervalRing.
**Decision:** W2/W3 bugs fixed independently. Full migration deferred until W14 (audio IPC wiring) since the plugin's separate capture/playback API would need restructuring to match the unified `process()` API.

### W6. Unbounded channels everywhere (12 instances)
**Status:** Deferred — infrastructure concern
**Files:** `crates/wail-net/src/signaling.rs:33-34`, `crates/wail-net/src/lib.rs:41-42`, `crates/wail-net/src/peer.rs:67-68,100,162`, `crates/wail-core/src/link.rs:120-121`
**Problem:** All channels are `mpsc::unbounded_channel()`. A slow consumer causes unbounded memory growth.
**Decision:** Leave as-is until memory issues are observed. Messages are small, peers are few. Would matter at scale or with bad network.

### W10. No graceful shutdown
**Status:** Deferred — infrastructure concern
**Files:** `crates/wail-app/src/main.rs`, `crates/wail-signaling/src/main.rs`
**Problem:** Neither binary handles SIGINT/SIGTERM. Process just dies on Ctrl+C.
**Fix when ready:** Add `tokio::signal::ctrl_c()` branch in `select!` loops.

### W11. No reconnection logic
**Status:** Deferred — infrastructure concern
**File:** `crates/wail-net/src/signaling.rs`
**Problem:** Signaling server disconnect kills the session permanently.
**Fix when ready:** Reconnect with exponential backoff (1s, 2s, 4s, … 30s max).

### W16. Signaling server accepts unbounded connections
**Status:** Deferred — infrastructure concern
**File:** `crates/wail-signaling/src/main.rs:46-54`
**Problem:** No connection limit. A flood spawns unlimited tokio tasks.
**Fix when ready:** `tokio::sync::Semaphore` with configurable max connections.

---

## Deferred — Feature Work

### W14. Audio IPC not wired — received audio is dropped
**Status:** Feature work, not a fix
**File:** `crates/wail-app/src/main.rs:218`
**Problem:** Decoded audio intervals are logged but never forwarded to plugin.
**Scope:** Requires IPC protocol design and plugin-side receive logic. Trigger W1 migration at this time.

### W15. Clock offset computed but never applied
**Status:** Feature work, not a fix
**File:** `crates/wail-app/src/main.rs:150-153`
**Problem:** Clock sync computes per-peer offset but never uses it to adjust beat timestamps.
**Scope:** Apply `clock.remote_to_local()` when processing remote sync messages.

---

## Skipped

### C4. Signaling server has no authentication/rate-limiting
**Status:** Explicitly skipped — not a priority for early development
**File:** `crates/wail-signaling/src/main.rs`
**Problem:** Binds to 0.0.0.0, no auth, no room passwords, no TLS, no peer limits.
**Rationale:** Security hardening before the core product works is premature. Revisit when deploying publicly.

---

## Polish (low priority)

| ID | Item | File | Status |
|----|------|------|--------|
| I4 | Single STUN server, no TURN (symmetric NATs will fail) | `crates/wail-net/src/peer.rs:60` | Open |
| I5 | `now_us()` cast u128→i64 overflows after 292 years | `crates/wail-core/src/clock.rs:36` | Open |
| I6 | Median uses upper-median for even-length arrays | `crates/wail-core/src/clock.rs:87` | Open |
| I7 | Echo guard 150ms window suppresses legit fast tempo changes | `crates/wail-core/src/link.rs:89-94` | Open |
