# WAIL - WebSocket Audio Interchange for Link

## What is this?

WAIL synchronizes Ableton Link sessions across the internet using a WebSocket relay server. Musicians on different networks can sync tempo, phase, and interval boundaries as if they were on the same LAN. Intervalic audio (NINJAM-style) is captured, Opus-encoded, and transmitted via the signaling server. Two CLAP/VST3 plugins provide DAW integration: WAIL Send (capture) and WAIL Recv (playback).

## Project Structure

```
wail-app/                Go/Wails desktop app (session orchestration)
├── main.go               Entry point, Wails window setup, CLI flags
├── app.go                Frontend-callable methods (JoinRoom, Disconnect, etc.)
├── session.go            Session state machine (goroutine-based select loop)
├── signaling.go          WebSocket signaling client + PeerMesh
├── peers.go              PeerRegistry + IPCWriterPool
├── ipc.go                TCP IPC protocol (plugin ↔ app framing)
├── link_real.go          Ableton Link bridge via abletonlink-go (CGo)
├── link_stub.go          Link stub for testing (-tags=linkstub)
├── link_types.go         Link types, poller, echo guard, tempo detector
├── clock.go              NTP-style RTT/clock sync
├── protocol.go           SyncMessage + SignalMessage types
├── wire.go               WAIF binary wire format
├── test_tone.go          Test tone generator (Opus sine wave)
├── recorder.go           Local session recording (WAIF frames to disk)
├── plugin_install.go     Auto-install CLAP/VST3 plugins on startup
├── events.go             Frontend event types
├── stream_names.go       Persistent per-stream name storage
├── filelog.go            Rotating file logger
├── wslog.go              WebSocket log broadcaster
├── honeybadger.go        Honeybadger crash reporting
└── frontend/             Bundled web UI (HTML/JS/CSS)

Cargo workspace with Rust crates (plugins + shared libraries):

crates/
├── wail-core/           Core sync library (no networking)
│   ├── link.rs           Ableton Link bridge via rusty_link
│   ├── protocol.rs       SyncMessage + SignalMessage types
│   ├── clock.rs          NTP-like peer RTT estimation (Ping/Pong)
│   └── interval.rs       NINJAM-style interval tracking
├── wail-audio/          Audio encoding and intervalic ring buffer
│   ├── codec.rs          Opus encode/decode (audiopus)
│   ├── ring.rs           NINJAM-style interval ring buffer (record + playback)
│   ├── interval.rs       AudioInterval, AudioFrame, IntervalRecorder
│   ├── wire.rs           Binary wire formats (AudioWire + WAIF AudioFrameWire)
│   ├── frame_assembler.rs FrameAssembler: collects WAIF frames into intervals
│   ├── bridge.rs         AudioBridge: wraps ring + Opus codec for send/recv
│   ├── ipc.rs            IPC framing protocol (length-prefixed messages)
│   └── pipeline.rs       Encode/decode pipeline (interval → wire → WebSocket relay)
├── wail-net/            Networking layer
│   ├── lib.rs            PeerMesh (WebSocket relay wrapper)
│   └── signaling.rs      WebSocket signaling + data relay client
├── wail-e2e/            Two-machine end-to-end test binary
│   └── main.rs           7-phase test: Signaling → Discovery → Sync → Audio → Sustained → Burst → Reconnect
├── wail-tauri/          Tauri desktop app (legacy, being replaced by wail-app)
│   ├── main.rs           App entry point
│   ├── lib.rs            Tauri setup and plugin registration
│   ├── commands.rs       Tauri IPC commands (join/leave room, etc.)
│   ├── events.rs         Tauri event types for frontend
│   ├── peers.rs          PeerRegistry + IpcWriterPool (consolidated peer state)
│   ├── session.rs        Session state machine (Link + WebSocket relay + audio)
│   └── recorder.rs       Local session recording
├── wail-plugin-send/    CLAP/VST3 send plugin (captures DAW audio)
│   ├── lib.rs            Plugin entry point, send-only IPC thread
│   └── params.rs         Plugin parameters (empty — defaults hardcoded)
├── wail-plugin-recv/    CLAP/VST3 receive plugin (plays remote audio)
│   ├── lib.rs            Plugin entry point, recv-only IPC thread
│   └── params.rs         Plugin parameters (empty — defaults hardcoded)

xtask/                   Build tasks (build-plugin, install-plugin, build-tauri, etc.)

signaling-server/
└── main.go           WebSocket signaling server (Go + SQLite, deployed to fly.io)

vendor/
└── link/             Ableton Link 4.0.0 beta SDK (git submodule)
```

## Build

Requires: Go 1.22+, Rust 1.75+, CMake 3.14+, C++ compiler (for abletonlink-go/Ableton Link SDK), libopus-dev

```sh
git submodule update --init --recursive   # fetch Link 4 SDK
cargo build                               # build Rust workspace (plugins + libraries)
cargo xtask test                          # run all tests (builds plugins if missing)

# Plugins (install bundler once)
cargo install --git https://github.com/robbert-vdh/nih-plug.git cargo-nih-plug
cargo xtask build-plugin                  # → target/bundled/wail-plugin-{send,recv}.{clap,vst3}
cargo xtask install-plugin                # build + install to system plugin dirs

# Go/Wails desktop app (handles Link + WebSocket relay + IPC)
cd wail-app && go build                   # build the app binary
cd wail-app && go test ./...              # run Go tests
```

## Key Dependencies

### Go (wail-app)
- `wails/v3` - Desktop app framework (webview)
- `gorilla/websocket` - WebSocket client (signaling + data relay)
- `abletonlink-go` - Ableton Link via CGo
- `pion/opus` - Opus codec bindings

### Rust (plugins + libraries)
- `rusty_link` - Ableton Link via C FFI to official SDK
- `tokio-tungstenite` - WebSocket client (signaling + data relay)
- `audiopus` - Opus audio codec (libopus bindings)
- `nih_plug` (git) - CLAP/VST3 plugin framework
- `tokio` - Async runtime

## Architecture

### Sync Flow
Each WAIL peer:
1. Joins local Ableton Link session (LAN multicast)
2. Connects to WebSocket signaling server to join a room (public or password-protected)
3. Sync messages (tempo, phase, clock) are relayed through the server to all room peers
4. Polls Link at 50Hz, broadcasts tempo/phase changes
5. Applies remote tempo changes to local Link session
6. Tracks NINJAM-style interval boundaries

### Audio Flow (Intervalic)
NINJAM-style double-buffer pattern with two separate plugins:
1. **WAIL Send** plugin captures DAW audio into record slot for current interval
2. At interval boundary: record slot → Opus encode → IPC → wail-app → WebSocket (binary) → server → all room peers
3. **WAIL Recv** plugin receives remote intervals via IPC, decoded and mixed into playback slot
4. Playback slot feeds audio output to DAW (main bus + up to 15 per-slot aux outputs)
5. Latency = exactly 1 interval (by design, like NINJAM)

Two WebSocket message types via the signaling server:
- **sync** (text): JSON messages relayed to all room peers (tempo, beat, phase, clock sync)
- **audio** (binary): WAIF wire-format frames broadcast to all room peers (Opus-encoded intervals)

```
DAW A → [WAIL Send] → record → Opus encode → IPC → wail-app → WS binary → server → all peers
        [WAIL Recv] ← play  ← Opus decode  ← IPC ← wail-app ← WS binary ← server ← remote peer
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

**Use `cargo xtask test` instead of `cargo test`.** The `wail-plugin-test` crate requires pre-built CLAP plugin bundles. Running `cargo test` directly will deadlock if the bundles are missing, because `build.rs` cannot spawn a nested `cargo` while the outer process holds the workspace lock. `cargo xtask test` handles this automatically — it builds the plugins first if missing, then runs `cargo test`.

```sh
cargo xtask test                          # build plugins if needed, run all tests
cargo xtask test -- -p wail-core          # core library tests only
cargo xtask test -- -p wail-audio         # audio tests (codec, ring buffer, wire format)
```

## Code Conventions

- Async with tokio, channels for cross-task communication
- `tracing` for structured logging (set RUST_LOG=debug for verbose)
- Protocol messages are JSON-serialized serde enums (tagged unions)
- Audio messages use WAIF streaming wire format (AudioFrameWire) over WebSocket relay
- Echo guard pattern: suppress re-broadcast for 150ms after applying remote changes
- wail-core has no networking dependencies (reusable from plugin)
- wail-audio has no networking dependencies (reusable from plugin)
- TDD: write tests first, especially for ring buffer and codec

## Versioning and Releases

Managed by [knope](https://github.com/knope-dev/knope) via `knope.toml`. All crates share one version (workspace-level).

**Versioned files** (kept in sync automatically): `Cargo.toml` (workspace), `crates/wail-tauri/tauri.conf.json`

### Recording changes

Conventional commit messages (`feat:`, `fix:`, `feat!:`) are the **sole** mechanism for changelog entries. Knope's `PrepareRelease` step processes **both** conventional commits and changeset files independently — using both for the same change produces duplicate changelog entries.

**Do NOT create a changeset file for changes that already use a conventional commit prefix.** Changeset files are a fallback only for `chore:` commits (infrastructure, CI, docs) that need a changelog entry but won't be picked up by conventional commit parsing.

If you need a changeset for a `chore:` commit:

```sh
knope document-change
```

**Changeset frontmatter format:** The YAML frontmatter must use `default: <type>` (e.g., `default: patch`). Do NOT use `type: <type>` or package names — knope silently ignores unrecognized package keys. Example:
```markdown
---
default: patch
---

Description of the change.
```

### Release pipeline (automated via GitHub Actions)

Releases are fully automated — no manual `knope` commands needed:

1. **Push to `main`** → `auto-release.yml` runs `knope prepare-release`, which consumes conventional commits (and `.changeset/` files if present as a fallback), bumps versions, updates `CHANGELOG.md`, and opens/updates a PR from the `release` branch → `main`.
2. **Merge the release PR** → `release-on-merge.yml` runs `knope release` (creates GitHub release + git tag) and dispatches artifact builds.
3. **`release.yml`** builds platform artifacts (macOS, Windows, Linux — plugins + Tauri app installers) and uploads them to the GitHub release.

### Rules for agents

- **Use conventional commits for user-facing work** (`feat:`, `fix:`, `feat!:`). Do NOT also create a changeset file — knope processes both sources and this creates duplicate changelog entries.
- **Never manually edit version numbers** in `Cargo.toml` or `tauri.conf.json` — knope handles this.
- **Never manually create git tags** for releases — GitHub Actions handles tagging.
- **Never run `knope release` or `knope prepare-release` locally** — GitHub Actions runs both automatically.
- **Use the correct conventional commit prefix.** New features MUST use `feat:`, bug fixes MUST use `fix:`, breaking changes MUST use `feat!:` or `fix!:`. Never use `fix:` for a new feature — this causes knope to bump only the patch version instead of minor. Similarly, never use unprefixed or `chore:` commits for user-facing changes — knope ignores them entirely. Get the prefix right; it directly controls the version bump.
- **Semver is now standard (post-1.0).** `feat:` / `default: minor` → minor bump, `fix:` / `default: patch` → patch bump, `feat!:` / `default: major` → major bump. No pre-1.0 shifting applies.
- **Keep docs in sync.** For each PR, check whether `README.md` and `docs/architecture.md` need updates to reflect the changes. User-facing features should update README; architectural changes (wire format, IPC protocol, crate structure, new design decisions) should update `docs/architecture.md`.

## Common Tasks

- **Add a new sync message**: Add variant to `SyncMessage` in `crates/wail-core/src/protocol.rs`, handle in `crates/wail-tauri/src/session.rs` select loop
- **Change Link polling rate**: `POLL_INTERVAL` in `crates/wail-core/src/link.rs`
- **Change Opus bitrate**: `AudioBridge::new()` bitrate_kbps param in `crates/wail-audio/src/bridge.rs`
- **Modify wire format**: `crates/wail-audio/src/wire.rs` (bump version byte)


## Trade-off Preferences

When encountering code quality trade-offs, follow these principles (derived from owner decisions):

### Error handling
- **Never panic in production paths.** Replace `unwrap()` with match/`?`/log-and-continue. Mutex poison → handle gracefully (return error to host, not crash).
- **Make failures observable.** Silent `.ok()` that discards errors must log at `warn!` level. If something can fail and the caller won't notice, add a log line.
- **Defensive clamping over error propagation** for internal numeric inputs. Bad values (zero divisors, negative durations) → clamp to safe minimums. Don't bubble `Result` for things that should just work.
- **TDD safety-critical fixes.** Division-by-zero, overflow, NaN — write the failing test first, then fix.

### Scope and priorities
- **Batch obvious fixes, discuss complex ones.** If a fix has no real trade-off (dead code, misleading labels, redundant imports), just do it. Only pause for decisions that involve architectural choices or behavioral changes.
- **Fix code, don't add process.** Prefer actual code changes over adding TODOs, lint suppressions, or documentation-only fixes. Exception: `#[allow(dead_code)]` is fine for fields that are structurally needed but not yet read.

### Trade-off log
All deferred decisions and remaining code quality items are tracked in `tradeoffs.md` at the repo root. When making a trade-off decision during development, record it there with the rationale.

## Known Limitations

### Wails v3 Window Icon (alpha.74)
The `application.WebviewWindowOptions` struct does not expose an `Icon` field in Wails v3.0.0-alpha.74. macOS shows a generic terminal icon in the dock instead of the WAIL logo. Check regularly if this is added in stable releases. Workaround: embed the icon in the platform-specific app bundle or use native platform hooks during the build.

## Future: Link Audio Integration

Ableton Link 4.0.0 beta (vendored at `vendor/link`) introduces Link Audio — native audio streaming between Link peers on a LAN. The Link Audio API (LinkAudio.hpp) provides:
- `LinkAudioSink`: publish audio channels to the network
- `LinkAudioSource`: subscribe to remote audio channels
- Channel discovery via `channels()` and `setChannelsChangedCallback()`

This could replace the custom Opus pipeline for LAN scenarios, while WAIL continues to bridge audio over the internet via the WebSocket relay server.
