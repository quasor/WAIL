# WAIL System Evaluation

Detailed audit of the WAIL codebase covering correctness, reliability, audio quality, security, and resource management. Each finding includes affected files, line numbers, reproduction conditions, and suggested fixes.

---

## CRITICAL — Crashes or data loss

### 1. Panic in clock median calculation

**File:** `crates/wail-core/src/clock.rs:83-85`

```rust
let mut rtts: Vec<i64> = clock.samples.iter().copied().collect();
rtts.sort();
clock.rtt_us = rtts[rtts.len() / 2]; // panics if rtts is empty
```

**Problem:** `rtts[rtts.len() / 2]` panics on index-out-of-bounds if the Vec is empty. Currently safe because `handle_pong()` always pushes a sample before computing the median (line 77). However, the `entry()` API on line 72 creates a `PeerClock` with an empty `VecDeque` — if a future refactor separates insertion from computation, or if the negative-RTT early return on line 68 races with another code path, this panics.

**Impact:** Process crash (panic in production).

**Fix:** Add `if rtts.is_empty() { return; }` before the sort/index.

---

### 2. Memory leak: decoder HashMap in recv plugin

**File:** `crates/wail-plugin-recv/src/lib.rs:469`

```rust
let mut decoders: HashMap<(String, u16, i64), AudioDecoder> = HashMap::new();
```

**Problem:** The key includes `interval_index` (monotonically increasing). Old decoders are **never evicted**. Each Opus decoder holds ~2KB of state. Over a multi-hour session with 4 peers:

| Duration | Intervals (4 bars @ 120 BPM) | Decoders | Memory |
|----------|------------------------------|----------|--------|
| 1 hour   | ~450                         | ~1800    | ~3.5 MB |
| 8 hours  | ~3600                        | ~14400   | ~28 MB |

**Impact:** Unbounded memory growth. Won't crash quickly, but degrades over long sessions.

**Fix:** Evict decoders for intervals older than `current - 2`. After line 524, add:
```rust
decoders.retain(|&(_, _, idx), _| idx >= frame.interval_index - 2);
```

---

### 3. Panic risk in fallback Opus decoder

**File:** `crates/wail-plugin-recv/src/lib.rs:528-531`

```rust
match AudioDecoder::new(opus_rate, channels) {
    Ok(d) => d,
    Err(e) => {
        AudioDecoder::new(48000, channels).expect("fallback opus decoder at known-good params")
    }
}
```

**Problem:** If the primary decoder fails because `channels` is invalid (e.g., 0 or >2), the fallback uses the same invalid `channels` value and panics on `.expect()`.

**Impact:** Plugin process crash inside the DAW.

**Fix:** Clamp channels before the fallback: `AudioDecoder::new(48000, channels.max(1).min(2))`.

---

## HIGH — Audio quality or reliability

### 4. No mixing overflow protection

**File:** `crates/wail-audio/src/ring.rs:562-566`

```rust
let copy_len = remote.samples.len().min(self.playback_slot.len());
for (i, sample) in remote.samples[..copy_len].iter().enumerate() {
    self.playback_slot[i] += sample; // unprotected addition
}
```

**Problem:** When N peers' audio is summed into `playback_slot`, the amplitude can exceed ±1.0. With 4 peers each at 0.5 amplitude, the sum reaches 2.0 — hard clipping. There is no limiter, normalization, or gain reduction.

The same issue exists in the fallback merge path at line 587:
```rust
slot.samples[i] += s;
```

**Impact:** Audible distortion that worsens with peer count. Users have no control over the mix level.

**Options:**
- **Simple:** Divide each peer's contribution by N (peer count) — reduces loudness
- **Better:** Apply a soft limiter (`tanh` or similar) after summing
- **Best:** Per-peer gain parameter exposed in the recv plugin UI

---

### 5. Audio thread allocations via `permit_alloc()`

**Files:** `crates/wail-plugin-send/src/lib.rs:329-412`, `crates/wail-plugin-recv/src/lib.rs:359-421`

**Problem:** Both plugins wrap their `process()` body in `permit_alloc()`, explicitly allowing heap allocations on the real-time audio thread. Operations inside include:
- `Vec::split_off()` and `std::mem::replace()` (allocate)
- `HashMap` lookups (can rehash)
- `try_send()` on bounded channels
- `Vec::clear()` (may deallocate)

**Impact:** Jitter and latency spikes. The host's audio callback has a hard deadline (typically 1-5ms at 256-sample buffer). Any allocation that triggers a page fault or lock contention causes an audible glitch.

**Mitigation:** This is a known trade-off of the NINJAM-style intervalic design. The heavy work (Opus encode/decode) happens on IPC threads, not the audio thread. The audio thread only shuffles pre-allocated buffers. In practice, glitches are rare because the allocations are small and infrequent (once per interval boundary, not per sample). Documenting as a known limitation rather than a bug.

---

### 6. No timeout on signaling join response

**File:** `crates/wail-net/src/signaling.rs:155-168`

```rust
let join_response = loop {
    match ws_read.next().await { // no timeout — hangs forever
        Some(Ok(Message::Text(text))) => {
            break serde_json::from_str::<ServerMsg>(&text)?;
        }
        Some(Ok(Message::Close(_))) | None => {
            anyhow::bail!("WebSocket closed before join response");
        }
        // ...
    }
};
```

**Problem:** If the signaling server accepts the WebSocket but never sends `join_ok` / `join_error`, this loop blocks forever. The Tauri session has no way to recover.

**Impact:** Session hangs on join. User must force-quit.

**Fix:** Wrap in `tokio::time::timeout(Duration::from_secs(10), ...)`.

---

### 7. Wire format accepts invalid field values

**File:** `crates/wail-audio/src/wire.rs:69-141`

**Problem:** `AudioWire::decode()` validates magic bytes and version but does not validate field values:

| Field | Accepted | Should reject |
|-------|----------|---------------|
| `sample_rate` | Any u32 | 0, or not in {8000, 12000, 16000, 24000, 48000} |
| `bpm` | Any f64 | NaN, Inf, ≤ 0, > 999 |
| `quantum` | Any f64 | NaN, Inf, ≤ 0 |
| `bars` | Any u32 | 0 |
| `opus_len` | Any u32 | > 10 MB (DoS via allocation) |

**Impact:**
- `sample_rate = 0` causes division-by-zero downstream
- `bpm = NaN` propagates through interval calculations
- A crafted `opus_len = 0xFFFFFFFF` allocates 4 GB on line 128: `data[HEADER_SIZE..HEADER_SIZE + opus_len].to_vec()`

**Fix:** Add validation after parsing, before constructing `AudioInterval`. Cap `opus_len` at a reasonable maximum (e.g., 1 MB — a 48kHz stereo interval at 128kbps for 60 seconds is ~960 KB).

---

## MEDIUM — Reliability and resource management

### 8. ICE candidates buffer unbounded

**File:** `crates/wail-net/src/peer.rs:138`

```rust
pending_candidates: Vec<RTCIceCandidateInit>,
```

**Problem:** ICE candidates are buffered until `set_remote_description()` is called. If the remote description never arrives (peer crash, network partition), this Vec grows unbounded. Each ICE candidate is small (~200 bytes), but with TURN servers and IPv6, a single peer can generate 20+ candidates.

**Impact:** Minor memory leak per failed connection attempt.

**Fix:** Cap at 100 candidates (discard oldest) or add a 30-second timeout that clears the buffer.

---

### 9. Frame assembler unbounded growth

**File:** `crates/wail-audio/src/frame_assembler.rs:36, 77-79`

```rust
pub struct FrameAssembler {
    pending: HashMap<(i64, u16, String), FrameCollection>,
}
```

**Problem:** The `pending` HashMap accumulates entries for intervals whose final frame never arrives (peer crash, packet loss). `evict_stale()` (line 125) only removes entries with `interval_index < current - 2`, but it's only called when a final frame arrives — if a peer goes silent, no eviction occurs.

Additionally, out-of-order frames cause `frames.resize(idx + 1, None)` (line 78), which can allocate up to 10,000 `Option<Vec<u8>>` slots if `frame_number` is large.

**Impact:** Memory leak proportional to peer count and packet loss rate.

**Fix:** Add a time-based eviction (e.g., drop collections older than 30 seconds) that runs on every `insert()`, not just on final frames.

---

### 10. Spawned per-peer tasks never explicitly cancelled

**File:** `crates/wail-net/src/lib.rs:382-455`

**Problem:** Three tasks are spawned per peer (`spawn_ice_sender`, `spawn_message_reader`, `spawn_audio_reader`). Their `JoinHandle`s are dropped immediately. Cleanup relies on channel receivers being dropped when the `PeerConnection` is removed from the `HashMap`.

**Impact:** If a channel sender is cloned or leaked, the task runs indefinitely. On session end, tasks linger until the tokio runtime shuts down. Not a functional bug today, but fragile.

**Fix:** Store `JoinHandle`s in `PeerConnection` and `.abort()` them on removal.

---

### 11. Signaling WebSocket tasks never cancelled

**File:** `crates/wail-net/src/signaling.rs:221-336`

**Problem:** The signaling client spawns read and write tasks without storing their `JoinHandle`s. On reconnect (when the signaling connection drops and is re-established), old tasks continue running until the underlying WebSocket stream closes naturally. If the stream is in a half-open state, the old task persists.

**Impact:** Duplicate message processing during the window between reconnect and old task termination.

**Fix:** Store `JoinHandle`s and `.abort()` before reconnecting.

---

### 12. Silent `pc.close()` failures

**File:** `crates/wail-net/src/lib.rs:289, 305, 355`

```rust
let _ = pc.close().await;
```

**Problem:** Peer connection close errors are silently discarded at three call sites. Per the project's trade-off preferences ("Silent `.ok()` that discards errors must log at `warn!` level").

**Impact:** Obscures WebRTC cleanup failures that could explain resource leaks.

**Fix:** Replace with:
```rust
if let Err(e) = pc.close().await {
    warn!(error = %e, "Failed to close peer connection");
}
```

---

### 13. Sample rate mismatch silently produces pitch-shifted audio

**File:** `crates/wail-audio/src/codec.rs:11-17`

```rust
pub fn nearest_opus_rate(rate: u32) -> u32 {
    const VALID: [u32; 5] = [8000, 12000, 16000, 24000, 48000];
    *VALID.iter().min_by_key(|&&r| (r as i64 - rate as i64).unsigned_abs()).unwrap()
}
```

**Problem:** DAW sample rates that aren't native Opus rates are silently mapped to the nearest one:
- 44100 Hz → 48000 Hz (9% rate change → audio plays ~9% faster)
- 96000 Hz → 48000 Hz (50% rate change)

The encoder and decoder are created once in `initialize()`. If the DAW changes sample rate mid-session, the old encoder/decoder persist with the wrong rate. Only a warning is logged (`crates/wail-plugin-recv/src/lib.rs:462`).

**Impact:** Pitch-shifted audio between peers with different DAW sample rates.

**Fix:** Add sample rate conversion (resample 44100→48000 before encoding, 48000→44100 after decoding). The `rubato` crate is commonly used for this in Rust audio projects.

---

### 14. Reconnect timer race condition (low risk)

**File:** `crates/wail-tauri/src/session.rs:522-559`

**Problem:** The `reconnect_pending` guard (line 522) prevents duplicate timers for the same peer. However, there's a theoretical window: if `PeerFailed` arrives on the channel between checking `reconnect_pending` (line 522) and setting it to `true` (line 542), a duplicate timer could spawn.

**Actual risk:** Low. The `select!` loop is single-threaded (no concurrent access to `peers`). Multiple `PeerFailed` events for the same peer are processed sequentially, and the guard works correctly in practice.

**Impact:** Negligible — documented for completeness.

---

## LOW — Polish and minor issues

### 15. IPC peer_id silently truncated at 255 bytes

**File:** `crates/wail-audio/src/ipc.rs:113-119`

`peer_id.len().min(255)` silently truncates long peer IDs. Current peer IDs are short UUIDs (~36 bytes), so this is safe today. Would cause routing errors if IDs exceeded 255 bytes.

---

### 16. `beat_position_fallback` can produce NaN

**File:** `crates/wail-plugin-recv/src/lib.rs:98-99`

If `sample_rate` is 0.0 (e.g., before `initialize()` is called), the fallback calculation divides by zero, producing NaN. The NaN propagates to interval tracking.

---

### 17. Audio reassembly mutex poison logged at `debug` not `warn`

**File:** `crates/wail-net/src/peer.rs:51`

Mutex poison in the audio reassembly path is logged at `debug` level, making it invisible in production. Should be `warn`.

---

### 18. Tauri event emission errors discarded

**File:** `crates/wail-tauri/src/session.rs` (multiple sites)

`let _ = app.emit(...)` discards Tauri event errors at ~15 call sites. Per trade-off preferences, these should log at `warn` level for critical events (peer join/leave) and `debug` for high-frequency events (audio status).

---

### 19. OnceLock DataChannels silently queue messages forever

**File:** `crates/wail-net/src/peer.rs:126-128`

If a DataChannel is never opened (ICE failure), messages queued in `pending_sync` (line 136) accumulate indefinitely. The `OnceLock` never resolves, so the pending buffer is never drained.

---

### 20. Crossfade float precision at boundary

**File:** `crates/wail-audio/src/ring.rs:544-550`

`sin(PI/2)` and `cos(PI/2)` at the crossfade boundary may not be exactly 1.0 and 0.0 due to IEEE 754 floating point. The error is ~1e-16, which is inaudible (below -300 dBFS). No fix needed.

---

## Summary

| Severity | Count | Key themes |
|----------|-------|------------|
| Critical | 3 | Panic paths (#1, #3), memory leak (#2) |
| High | 4 | Mixing overflow (#4), RT allocations (#5), no timeout (#6), wire validation (#7) |
| Medium | 7 | Resource leaks (#8, #9), task cancellation (#10, #11), silent errors (#12), sample rate (#13), reconnect race (#14) |
| Low | 6 | Truncation (#15), NaN (#16), log levels (#17, #18), OnceLock (#19), float precision (#20) |

### Recommended fix order

1. **#1, #3** — Defensive guards against panics (small, safe changes)
2. **#2** — Decoder eviction (small change, prevents multi-hour memory growth)
3. **#7** — Wire format validation (prevents DoS and downstream NaN/div-by-zero)
4. **#6** — Signaling join timeout (prevents permanent hang)
5. **#12** — Silent close failures (one-line fixes, improves observability)
6. **#4** — Mixing overflow (requires design decision on approach)
7. **#13** — Sample rate conversion (larger effort, new dependency)
