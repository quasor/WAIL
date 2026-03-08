# WAIL E2E Test — Remote Machine Instructions

This binary validates the full WAIL network path between two real machines on separate networks.

## Current Test Run

The leader machine is waiting. Run this NOW:

```sh
git checkout quasor/e2e-two-machine-tests
git pull
cargo run -p wail-e2e --release -- --room e2e-34dc7c33 --server ws://192.168.7.141:8080 --verbose 2>&1 | tee e2e-follower.log
```

## What It Tests

Eight phases run sequentially. Each must pass before the next begins:

1. **ICE** — Fetches TURN credentials from Metered API (falls back to STUN-only if unreachable)
2. **Signaling** — Connects to the signaling server via WebSocket
3. **Discovery** — Waits for the other peer to join the same room
4. **WebRTC** — Full ICE/DTLS/SCTP negotiation, waits for DataChannels to open
5. **Sync** — Exchanges Hello messages + Ping/Pong to measure RTT
6. **Audio** — Sends a 440Hz Opus-encoded test interval, validates received audio (wire format, Opus decode, non-silence check)
7. **Sustained** — Sends 10 intervals back-to-back, measures throughput and gaps
8. **Reconnect** — One peer disconnects WebRTC and reconnects signaling, verifies recovery

## Options

```
--room <NAME>       Room name (REQUIRED — must match the other machine)
--server <URL>      Signaling server [default: wss://wail-signal.fly.dev]
--timeout <SECS>    Global timeout [default: 180]
--intervals <N>     Number of sustained audio intervals [default: 10]
--verbose           Debug-level tracing output
```

## Troubleshooting

- **WebRTC timeout**: Both machines may be behind symmetric NATs. Add `--verbose` to see ICE candidate types. If only `host` candidates appear, TURN relay is needed (the binary fetches TURN credentials automatically, but corporate firewalls may block TURN ports).
- **Signaling timeout**: Check that the signaling server is reachable. Try `curl -I http://192.168.7.141:8080/health` — you should get a response.
- **No audio received**: The other machine must also reach the Audio phase. If one side is stuck on WebRTC, neither will exchange audio.
- **Build fails**: Requires Rust 1.75+, CMake 3.14+, C++ compiler, libopus-dev. Run `git submodule update --init --recursive` first.
