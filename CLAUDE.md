# WAIL - WebRTC Audio Interchange for Link

## What is this?

WAIL synchronizes Ableton Link sessions across the internet using WebRTC DataChannels. Musicians on different networks can sync tempo, phase, and interval boundaries as if they were on the same LAN. Future: real-time audio via CLAP/VST3 plugin.

## Project Structure

```
Cargo workspace with 4 crates:

crates/
├── wail-core/        Core sync library (no networking)
│   ├── link.rs        Ableton Link bridge via rusty_link
│   ├── protocol.rs    SyncMessage + SignalMessage types
│   ├── clock.rs       NTP-like peer clock offset estimation
│   └── interval.rs    NINJAM-style interval tracking
├── wail-net/         Networking layer
│   ├── lib.rs         PeerMesh: manages all WebRTC connections
│   ├── signaling.rs   WebSocket signaling client
│   └── peer.rs        Single WebRTC peer connection
├── wail-app/         CLI binary
│   └── main.rs        Wires Link + WebRTC + sync together
└── wail-signaling/   Signaling server binary
    └── main.rs        WebSocket room-based relay
```

## Build

Requires: Rust 1.75+, CMake 3.14+, C++ compiler (for rusty_link/Ableton Link SDK)

```sh
cargo build                          # build all
cargo run -p wail-signaling          # start signaling server on :9090
cargo run -p wail-app -- join --room test --server ws://localhost:9090
```

## Key Dependencies

- `rusty_link` - Ableton Link via C FFI to official SDK
- `webrtc` v0.11 (webrtc-rs) - Pure Rust WebRTC
- `tokio` - Async runtime
- `tokio-tungstenite` - WebSocket
- `clap` - CLI parsing

## Architecture

Each WAIL peer:
1. Joins local Ableton Link session (LAN multicast)
2. Connects to signaling server (WebSocket) to join a "room"
3. Establishes WebRTC DataChannels with remote peers (P2P)
4. Polls Link at 50Hz, broadcasts tempo/phase changes
5. Applies remote tempo changes to local Link session
6. Tracks NINJAM-style interval boundaries

Flow: `DAW A <-> Link <-> WAIL A <-> WebRTC P2P <-> WAIL B <-> Link <-> DAW B`

## Testing

```sh
cargo test                    # run all tests
cargo test -p wail-core       # core library tests only
```

## Code Conventions

- Async with tokio, channels for cross-task communication
- `tracing` for structured logging (set RUST_LOG=debug for verbose)
- Protocol messages are JSON-serialized serde enums (tagged unions)
- Echo guard pattern: suppress re-broadcast for 150ms after applying remote changes
- wail-core has no networking dependencies (reusable from future plugin)

## Common Tasks

- **Add a new sync message**: Add variant to `SyncMessage` in `crates/wail-core/src/protocol.rs`, handle in `crates/wail-app/src/main.rs` select loop
- **Change Link polling rate**: `POLL_INTERVAL` in `crates/wail-core/src/link.rs`
- **Add STUN/TURN servers**: `RTCIceServer` list in `crates/wail-net/src/peer.rs`
