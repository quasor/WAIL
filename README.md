# WAIL — WebRTC Audio Interchange for Link

WAIL synchronizes [Ableton Link](https://www.ableton.com/link/) sessions across the internet using WebRTC DataChannels. Musicians on different networks can sync tempo, phase, and interval boundaries as if they were on the same LAN. Intervalic audio (NINJAM-style) is captured, Opus-encoded, and transmitted over WebRTC DataChannels. A CLAP/VST3 plugin provides DAW integration.

## Install

Download the latest release from the [Releases page](https://github.com/quasor/WAIL/releases).

**macOS** — Open the DMG and drag WAIL to Applications. For the audio plugins, run the included `.pkg` installer.

**Windows** — Run the NSIS installer. Plugins are bundled as CLAP and VST3 files — copy them to your DAW's plugin directory.

> **Important:** Enable Ableton Link in your DAW before using WAIL. In Ableton Live, go to Preferences > Link, Tempo, MIDI and turn on "Show Link Toggle" then enable Link. Other DAWs have similar settings — check your DAW's documentation for Link support.

## Components

WAIL has three components that work together:

- **WAIL app** — The standalone desktop app that handles networking. It connects to the signaling server, establishes WebRTC peer connections, and bridges audio and sync data between the DAW plugins and remote peers. Launch it before opening your DAW session.

- **WAIL Send** (CLAP/VST3 plugin) — Place this on a track or bus in your DAW to capture audio. At each interval boundary, the recorded audio is Opus-encoded and sent to all connected peers via the WAIL app.

- **WAIL Recv** (CLAP/VST3 plugin) — Place this on a track in your DAW to hear remote peers. It receives and decodes incoming audio intervals, mixing them into the main output with additional per-peer auxiliary outputs.

## Troubleshooting

**No sync / peers not connecting** — Make sure Ableton Link is enabled in your DAW. WAIL relies on Link for tempo and phase sync. In Ableton Live: Preferences > Link, Tempo, MIDI > enable Link. In Bitwig: Settings > Synchronization > enable Link.

**No audio from remote peers** — Verify that both WAIL Send and WAIL Recv plugins are loaded and the WAIL app is running and connected to the same room.

## How it works

Each WAIL peer joins a local Ableton Link session and connects to a lightweight HTTP signaling server to discover other peers. Rooms are password-protected — the first peer to join sets the password; subsequent peers must provide the matching password. Once peers discover each other, they establish direct WebRTC connections with two DataChannels each:

- **sync** — JSON text messages for tempo, beat, phase, and clock synchronization
- **audio** — binary wire-format messages carrying Opus-encoded audio intervals

Audio uses a NINJAM-style double-buffer pattern: the plugin records the current interval from the DAW, and at the interval boundary the completed recording is Opus-encoded and sent to all peers. Remote intervals are decoded, mixed, and played back one interval behind — latency equals exactly one interval by design.

```
DAW A → [CLAP Plugin] → record → Opus encode → DataChannel → remote peer
                       ← play  ← Opus decode ← DataChannel ← remote peer
```

## Project structure

```
crates/
├── wail-core/        Core sync library (no networking)
├── wail-audio/       Audio encoding and intervalic ring buffer
├── wail-net/         WebRTC peer mesh and signaling client
├── wail-plugin/      CLAP/VST3 plugin (nih-plug)
├── wail-app/         CLI binary

val-town/
└── signaling.ts      HTTP signaling server (deployed to Val Town)
```

## Build from source

Requires: **Rust 1.75+**, CMake 3.14+, a C++ compiler, and libopus-dev.

```sh
git submodule update --init --recursive   # fetch Ableton Link SDK
cargo build                               # build workspace
```

### Plugin

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

### Running

```sh
# Two peers in the same room — different IPC ports so they don't collide
cargo xtask run-peer --room jam --password mysecret                   # peer A
cargo xtask run-peer --room jam --password mysecret --ipc-port 9192   # peer B
```

All `run-peer` flags map directly to `wail-app join` options (see `--help`).
Both `--room` and `--password` are required. The first peer to join a room sets the password; others must match it.

## Testing

```sh
cargo test                    # all tests (~104 unit + integration)
cargo test -p wail-core       # core library tests
cargo test -p wail-audio      # audio codec, ring buffer, wire format
cargo test -p wail-net        # networking + WebRTC integration tests
```

## License

MIT
