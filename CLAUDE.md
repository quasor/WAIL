# WAIL - WebRTC Audio Interchange for Link

## What is this?

WAIL synchronizes Ableton Link sessions across the internet using WebRTC DataChannels. Musicians on different networks can sync tempo, phase, and interval boundaries as if they were on the same LAN. Intervalic audio (NINJAM-style) is captured, Opus-encoded, and transmitted over WebRTC DataChannels. Two CLAP/VST3 plugins provide DAW integration: WAIL Send (capture) and WAIL Recv (playback).

## Project Structure

```
Cargo workspace with 7 crates:

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
│   ├── wire.rs           Binary wire format for audio over DataChannels
│   ├── bridge.rs         AudioBridge: wraps ring + Opus codec for send/recv
│   ├── ipc.rs            IPC framing protocol (length-prefixed messages)
│   └── pipeline.rs       Encode/decode pipeline (interval → wire → DataChannel)
├── wail-net/            Networking layer
│   ├── lib.rs            PeerMesh + ICE server config (Metered TURN)
│   ├── signaling.rs      HTTP polling signaling client
│   └── peer.rs           WebRTC peer with "sync" + "audio" DataChannels
├── wail-tauri/          Tauri desktop app (session orchestration)
│   ├── main.rs           App entry point
│   ├── lib.rs            Tauri setup and plugin registration
│   ├── commands.rs       Tauri IPC commands (join/leave room, etc.)
│   ├── events.rs         Tauri event types for frontend
│   ├── session.rs        Session state machine (Link + WebRTC + audio)
│   └── recorder.rs       Local session recording
├── wail-plugin-send/    CLAP/VST3 send plugin (captures DAW audio)
│   ├── lib.rs            Plugin entry point, send-only IPC thread
│   └── params.rs         Plugin parameters (empty — defaults hardcoded)
├── wail-plugin-recv/    CLAP/VST3 receive plugin (plays remote audio)
│   ├── lib.rs            Plugin entry point, recv-only IPC thread
│   └── params.rs         Plugin parameters (empty — defaults hardcoded)

xtask/                   Build tasks (build-plugin, install-plugin, build-tauri, etc.)

val-town/
└── main.ts           HTTP signaling server (deployed to Val Town)

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

# Tauri app (handles Link + WebRTC + IPC)
cargo tauri dev
```

## Key Dependencies

- `rusty_link` - Ableton Link via C FFI to official SDK
- `webrtc` v0.11 (webrtc-rs) - Pure Rust WebRTC
- `audiopus` - Opus audio codec (libopus bindings)
- `nih_plug` (git) - CLAP/VST3 plugin framework
- `tokio` - Async runtime

## Architecture

### Sync Flow
Each WAIL peer:
1. Joins local Ableton Link session (LAN multicast)
2. Connects to HTTP signaling server to join a room (public or password-protected)
3. Establishes WebRTC DataChannels with remote peers (P2P)
4. Polls Link at 50Hz, broadcasts tempo/phase changes
5. Applies remote tempo changes to local Link session
6. Tracks NINJAM-style interval boundaries

### Audio Flow (Intervalic)
NINJAM-style double-buffer pattern with two separate plugins:
1. **WAIL Send** plugin captures DAW audio into record slot for current interval
2. At interval boundary: record slot → Opus encode → IPC → wail-app → WebRTC DataChannel
3. **WAIL Recv** plugin receives remote intervals via IPC, decoded and mixed into playback slot
4. Playback slot feeds audio output to DAW (main bus + up to 31 per-slot aux outputs)
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
cargo test                    # run all tests (~114 tests)
cargo test -p wail-core       # core library tests only
cargo test -p wail-audio      # audio tests (codec, ring buffer, wire format)
```

Some integration tests are marked `#[ignore]` because they require external resources. Run these during local development to verify end-to-end behaviour:

```sh
# Requires internet access — hits the live Metered API and asserts valid TURN credentials are returned
cargo test -p wail-net -- --ignored fetch_metered_ice_servers_live

# Requires coturn installed (brew install coturn) — full WebRTC path through a local TURN relay
cargo test -p wail-net -- --ignored two_peers_exchange_audio_via_turn
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

## Versioning and Releases

Managed by [knope](https://github.com/knope-dev/knope) via `knope.toml`. All crates share one version (workspace-level).

**Versioned files** (kept in sync automatically): `Cargo.toml` (workspace), `crates/wail-tauri/tauri.conf.json`

### Recording changes

When making a user-facing change (feature, fix, breaking change), create a changeset file:

```sh
knope document-change
```

This creates a markdown file in `.changeset/` describing what changed and the bump type (major/minor/patch). Commit the changeset file with your PR. Conventional commit messages (`feat:`, `fix:`, `feat!:`) also work and are picked up automatically.

**Changeset frontmatter format:** The YAML frontmatter must use `default: <type>` (e.g., `default: minor`). Do NOT use `type: <type>` or package names — knope silently ignores unrecognized package keys. Example:
```markdown
---
default: minor
---

Description of the change.
```

### Release pipeline (automated via GitHub Actions)

Releases are fully automated — no manual `knope` commands needed:

1. **Push to `main`** → `auto-release.yml` runs `knope prepare-release`, which consumes `.changeset/` files + conventional commits, bumps versions, updates `CHANGELOG.md`, and opens/updates a PR from the `release` branch → `main`.
2. **Merge the release PR** → `release-on-merge.yml` runs `knope release` (creates GitHub release + git tag) and dispatches artifact builds.
3. **`release.yml`** builds platform artifacts (macOS, Windows, Linux — plugins + Tauri app installers) and uploads them to the GitHub release.

### Rules for agents

- **Always create a changeset** for user-facing work. Run `knope document-change` or manually create a `.changeset/<short-name>.md` file.
- **Never manually edit version numbers** in `Cargo.toml` or `tauri.conf.json` — knope handles this.
- **Never manually create git tags** for releases — GitHub Actions handles tagging.
- **Never run `knope release` or `knope prepare-release` locally** — GitHub Actions runs both automatically.
- Use conventional commit prefixes: `feat:`, `fix:`, `chore:`, `feat!:` (breaking).
- **Keep docs in sync.** For each PR, check whether `README.md` and `docs/architecture.md` need updates to reflect the changes. User-facing features should update README; architectural changes (wire format, IPC protocol, crate structure, new design decisions) should update `docs/architecture.md`.

## Common Tasks

- **Add a new sync message**: Add variant to `SyncMessage` in `crates/wail-core/src/protocol.rs`, handle in `crates/wail-tauri/src/session.rs` select loop
- **Change Link polling rate**: `POLL_INTERVAL` in `crates/wail-core/src/link.rs`
- **Add STUN/TURN servers**: ICE server config in `crates/wail-net/src/lib.rs` (includes dynamic Metered TURN credentials)
- **Change Opus bitrate**: `AudioBridge::new()` bitrate_kbps param in `crates/wail-audio/src/bridge.rs`
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
