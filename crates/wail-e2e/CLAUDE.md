# WAIL E2E Test — Remote Machine Instructions

This binary validates the full WAIL network path between two real machines on separate networks.

## Quick Start

You are the **remote peer** in a two-machine end-to-end test. The other machine will give you a room name.

```sh
cargo run -p wail-e2e --release -- --room <ROOM_NAME>
```

Replace `<ROOM_NAME>` with the room name provided by the other machine.

## What It Tests

Seven phases run sequentially. Each must pass before the next begins:

1. **Signaling** — Connects to `wss://wail-signal.fly.dev` via WebSocket and joins the room
2. **Discovery** — Waits for the other peer to join the same room
3. **Sync** — Exchanges Hello messages + Ping/Pong to measure RTT
4. **Audio** — Sends a 440Hz Opus-encoded test interval, validates received audio (wire format, Opus decode, non-silence check)
5. **Sustained** — Sends multiple intervals back-to-back, measures throughput and inter-arrival gaps
6. **Burst** — Zero-delay flood of intervals to validate buffer headroom
7. **Reconnect** — One peer reconnects signaling, verifies sync and audio resume

## Options

```
--room <NAME>       Room name (REQUIRED — must match the other machine)
--server <URL>      Signaling server [default: wss://wail-signal.fly.dev]
--timeout <SECS>    Global timeout [default: 180]
--verbose           Debug-level tracing output
```

## Troubleshooting

- **Signaling timeout**: Check that `wss://wail-signal.fly.dev` is reachable. Try `curl -I https://wail-signal.fly.dev` — you should get a response.
- **No audio received**: The other machine must also reach the Audio phase. If one side is stuck on Discovery, neither will exchange audio.
- **Build fails**: Requires Rust 1.75+, CMake 3.14+, C++ compiler, libopus-dev. Run `git submodule update --init --recursive` first.
