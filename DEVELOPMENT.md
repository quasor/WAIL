# Development

## How It Works

Each WAIL peer joins a local Ableton Link session and connects to a lightweight signaling server to discover other peers. Once peers find each other, they establish direct WebRTC connections with two DataChannels each:

- **sync** — JSON text messages for tempo, beat, phase, and clock synchronization
- **audio** — binary messages carrying Opus-encoded audio intervals

Audio uses a NINJAM-style double-buffer pattern: the Send plugin records the current interval from the DAW, and at the interval boundary the completed recording is Opus-encoded and sent to all peers. Remote intervals are decoded, mixed, and played back one interval behind — latency equals exactly one interval by design.

```
DAW A → [WAIL Send] → record → Opus encode → DataChannel → remote peer
         [WAIL Recv] ← play  ← Opus decode  ← DataChannel ← remote peer
```

## Project Structure

```
Cargo workspace with 7 crates:

crates/
├── wail-core/           Core sync library (no networking)
├── wail-audio/          Audio encoding, intervalic ring buffer, IPC framing
├── wail-net/            WebRTC peer mesh and signaling client
├── wail-tauri/          Tauri desktop app (session orchestration)
├── wail-plugin-send/    CLAP/VST3 send plugin (captures DAW audio)
├── wail-plugin-recv/    CLAP/VST3 receive plugin (plays remote audio)

xtask/                   Build tasks (build-plugin, install-plugin, build-tauri, etc.)

signaling-server/
└── main.go              WebSocket signaling server (Go + SQLite, deployed to fly.io)

vendor/
└── link/                Ableton Link 4.0.0 beta SDK (git submodule)
```

## Build from Source

Requires: **Rust 1.75+**, CMake 3.14+, a C++ compiler, and libopus-dev.

**Linux build dependencies (Debian/Ubuntu):**

```sh
sudo apt-get install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
  librsvg2-dev libxdo-dev libssl-dev patchelf libopus-dev cmake g++
```

```sh
git submodule update --init --recursive   # fetch Ableton Link SDK
cargo build                               # build workspace
```

### Plugins

Install the bundler once, then use `cargo xtask`:

```sh
cargo install --git https://github.com/robbert-vdh/nih-plug.git cargo-nih-plug

cargo xtask build-plugin        # build CLAP + VST3 bundles → target/bundled/
cargo xtask install-plugin      # build and install to system plugin directories
cargo xtask install-plugin --no-build  # install already-built bundles
```

Plugin directories:
- **macOS** — `~/Library/Audio/Plug-Ins/{CLAP,VST3}/`
- **Linux** — `~/.clap/` and `~/.vst3/`
- **Windows** — `%COMMONPROGRAMFILES%\{CLAP,VST3}\`

### Tauri App

```sh
cargo tauri dev          # run in development mode
cargo xtask build-tauri  # production build (builds plugins first)
```

## Testing

```sh
cargo test                    # all tests (~114 unit + integration)
cargo test -p wail-core       # core library tests
cargo test -p wail-audio      # audio codec, ring buffer, wire format
cargo test -p wail-net        # networking + WebRTC integration tests
```

Some integration tests are marked `#[ignore]` because they require external resources:

```sh
# Requires internet access — hits the live Metered API and asserts valid TURN credentials are returned
cargo test -p wail-net -- --ignored fetch_metered_ice_servers_live

# Requires coturn installed (brew install coturn) — full WebRTC path through a local TURN relay
cargo test -p wail-net -- --ignored two_peers_exchange_audio_via_turn
```

# TODO
