# WAIL - WebRTC Audio Interchange for Link

## What is this?

WAIL synchronizes Ableton Link sessions across the internet using WebRTC DataChannels. Musicians on different networks can sync tempo, phase, and interval boundaries as if they were on the same LAN. Intervalic audio (NINJAM-style) is captured, Opus-encoded, and transmitted over WebRTC DataChannels. Two CLAP/VST3 plugins provide DAW integration: WAIL Send (capture) and WAIL Recv (playback).

## Project Structure

```
Cargo workspace with 6 crates:

crates/
├── wail-core/           Core sync library (no networking)
│   ├── link.rs           Ableton Link bridge via rusty_link
│   ├── protocol.rs       SyncMessage + SignalMessage types
│   ├── clock.rs          NTP-like peer clock offset estimation
│   └── interval.rs       NINJAM-style interval tracking
├── wail-audio/          Audio encoding and intervalic ring buffer
│   ├── codec.rs          Opus encode/decode (audiopus)
│   ├── ring.rs           NINJAM-style interval ring buffer (record + playback)
│   ├── interval.rs       AudioInterval type, IntervalRecorder, IntervalPlayer
│   └── wire.rs           Binary wire format for audio over DataChannels
├── wail-net/            Networking layer
│   ├── lib.rs            PeerMesh: manages all WebRTC connections
│   ├── signaling.rs      HTTP polling signaling client
│   └── peer.rs           WebRTC peer with "sync" + "audio" DataChannels
├── wail-plugin-send/    CLAP/VST3 send plugin (captures DAW audio)
│   ├── lib.rs            Plugin entry point, send-only IPC thread
│   └── params.rs         Send parameters (bars, timesig, send toggle, bitrate)
├── wail-plugin-recv/    CLAP/VST3 receive plugin (plays remote audio)
│   ├── lib.rs            Plugin entry point, recv-only IPC thread
│   └── params.rs         Recv parameters (bars, timesig, receive toggle, volume)
├── wail-app/            CLI binary
│   └── main.rs           Wires Link + WebRTC + IPC together

val-town/
└── signaling.ts      HTTP signaling server (deployed to Val Town)

vendor/
└── link/             Ableton Link 4.0.0 beta SDK (git submodule)
```

## Build

Requires: Rust 1.75+, CMake 3.14+, C++ compiler (for rusty_link/Ableton Link SDK), libopus-dev

```sh
git submodule update --init --recursive   # fetch Link 4 SDK
cargo build                               # build workspace
cargo test                                # run all tests

# Plugin (install bundler once)
cargo install --git https://github.com/robbert-vdh/nih-plug.git cargo-nih-plug
cargo xtask build-plugin                  # → target/bundled/wail-plugin-{send,recv}.{clap,vst3}
cargo xtask install-plugin                # build + install to system plugin dirs

# Run (signaling via Val Town at https://wail.val.run/)
cargo xtask run-peer --room jam --password secret             # peer A (defaults: bpm=120, ipc=9191)
cargo xtask run-peer --room jam --password secret --bpm 96 --ipc-port 9192  # peer B
```

## Key Dependencies

- `rusty_link` - Ableton Link via C FFI to official SDK
- `webrtc` v0.11 (webrtc-rs) - Pure Rust WebRTC
- `audiopus` - Opus audio codec (libopus bindings)
- `nih_plug` (git) - CLAP/VST3 plugin framework
- `tokio` - Async runtime
- `clap` - CLI parsing

## Architecture

### Sync Flow
Each WAIL peer:
1. Joins local Ableton Link session (LAN multicast)
2. Connects to HTTP signaling server to join a password-protected "room"
3. Establishes WebRTC DataChannels with remote peers (P2P)
4. Polls Link at 50Hz, broadcasts tempo/phase changes
5. Applies remote tempo changes to local Link session
6. Tracks NINJAM-style interval boundaries

### Audio Flow (Intervalic)
NINJAM-style double-buffer pattern with two separate plugins:
1. **WAIL Send** plugin captures DAW audio into record slot for current interval
2. At interval boundary: record slot → Opus encode → IPC → wail-app → WebRTC DataChannel
3. **WAIL Recv** plugin receives remote intervals via IPC, decoded and mixed into playback slot
4. Playback slot feeds audio output to DAW (main bus + 7 per-peer aux outputs)
5. Latency = exactly 1 interval (by design, like NINJAM)

Two WebRTC DataChannels per peer:
- **"sync"**: JSON text messages (tempo, beat, phase, clock sync)
- **"audio"**: Binary wire-format messages (Opus-encoded intervals)

```
DAW A → [WAIL Send] → record → Opus encode → IPC → wail-app → DataChannel → remote peer
        [WAIL Recv] ← play  ← Opus decode  ← IPC ← wail-app ← DataChannel ← remote peer
```

### Wire Format (AudioWire)
Binary header (48 bytes) + Opus data:
- Magic "WAIL", version, flags, interval index, sample rate, BPM, quantum, bars

### NINJAM Ring Buffer (IntervalRing)
- Two slots: record (current interval) and playback (previous interval)
- At boundary: record → completed queue (for encoding), pending remote → playback
- Multiple peers' audio mixed (summed) in playback slot
- Beat-position driven boundaries (from DAW transport / Link)

## Testing

```sh
cargo test                    # run all tests (~104 tests)
cargo test -p wail-core       # core library tests only
cargo test -p wail-audio      # audio tests (codec, ring buffer, wire format)
```

## Code Conventions

- Async with tokio, channels for cross-task communication
- `tracing` for structured logging (set RUST_LOG=debug for verbose)
- Protocol messages are JSON-serialized serde enums (tagged unions)
- Audio messages use binary wire format (AudioWire) over DataChannels
- Echo guard pattern: suppress re-broadcast for 150ms after applying remote changes
- wail-core has no networking dependencies (reusable from plugin)
- wail-audio has no networking dependencies (reusable from plugin)
- TDD: write tests first, especially for ring buffer and codec

## Common Tasks

- **Add a new sync message**: Add variant to `SyncMessage` in `crates/wail-core/src/protocol.rs`, handle in `crates/wail-app/src/main.rs` select loop
- **Change Link polling rate**: `POLL_INTERVAL` in `crates/wail-core/src/link.rs`
- **Add STUN/TURN servers**: `RTCIceServer` list in `crates/wail-net/src/peer.rs`
- **Change Opus bitrate**: Default in send plugin params (`crates/wail-plugin-send/src/params.rs`)
- **Modify wire format**: `crates/wail-audio/src/wire.rs` (bump version byte)
- **Adjust ring buffer crossfade**: `IntervalPlayer::new()` crossfade_ms param

## Trade-off Preferences

When encountering code quality trade-offs, follow these principles (derived from owner decisions):

### Error handling
- **Never panic in production paths.** Replace `unwrap()` with match/`?`/log-and-continue. Mutex poison → handle gracefully (return error to host, not crash).
- **Make failures observable.** Silent `.ok()` that discards errors must log at `warn!` level. If something can fail and the caller won't notice, add a log line.
- **Defensive clamping over error propagation** for internal numeric inputs. Bad values (zero divisors, negative durations) → clamp to safe minimums. Don't bubble `Result` for things that should just work.
- **TDD safety-critical fixes.** Division-by-zero, overflow, NaN — write the failing test first, then fix.

### Scope and priorities
- **Batch obvious fixes, discuss complex ones.** If a fix has no real trade-off (dead code, misleading labels, redundant imports), just do it. Only pause for decisions that involve architectural choices or behavioral changes.
- **Defer infrastructure hardening in early development.** Authentication, rate limiting, TLS, connection limits, reconnection logic — track in `tradeoffs.md` but don't implement until the core product works end-to-end.
- **Fix code, don't add process.** Prefer actual code changes over adding TODOs, lint suppressions, or documentation-only fixes. Exception: `#[allow(dead_code)]` is fine for fields that are structurally needed but not yet read.

### Trade-off log
All deferred decisions and remaining code quality items are tracked in `tradeoffs.md` at the repo root. When making a trade-off decision during development, record it there with the rationale.

## Future: Link Audio Integration

Ableton Link 4.0.0 beta (vendored at `vendor/link`) introduces Link Audio — native audio streaming between Link peers on a LAN. The Link Audio API (LinkAudio.hpp) provides:
- `LinkAudioSink`: publish audio channels to the network
- `LinkAudioSource`: subscribe to remote audio channels
- Channel discovery via `channels()` and `setChannelsChangedCallback()`

This could replace the custom Opus+DataChannel pipeline for LAN scenarios, while WAIL continues to bridge audio over the internet via WebRTC.
