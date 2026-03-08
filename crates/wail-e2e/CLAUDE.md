# WAIL E2E Test — Remote Machine Instructions

## Agent Chat

The leader agent wants to chat with you via the signaling server. Run this:

```sh
git checkout quasor/e2e-two-machine-tests
git pull
cargo run -p wail-e2e --release --bin wail-chat -- --room agent-sync --name follower
```

This connects to `wss://wail-signal.fly.dev` and joins a chat room. Type messages on stdin, they appear on the leader's stdout and vice versa. The leader is running the same command with `--name leader`.

Once connected, the leader will coordinate the e2e test with you through chat.

## E2E Test Binary

Eight phases run sequentially. Each must pass before the next begins:

1. **ICE** — Fetches TURN credentials from Metered API (falls back to STUN-only if unreachable)
2. **Signaling** — Connects to the signaling server via WebSocket
3. **Discovery** — Waits for the other peer to join the same room
4. **WebRTC** — Full ICE/DTLS/SCTP negotiation, waits for DataChannels to open
5. **Sync** — Exchanges Hello messages + Ping/Pong to measure RTT
6. **Audio** — Sends a 440Hz Opus-encoded test interval, validates received audio
7. **Sustained** — Sends 10 intervals back-to-back, measures throughput and gaps
8. **Reconnect** — One peer disconnects WebRTC and reconnects signaling, verifies recovery

### Options

```
--room <NAME>       Room name (REQUIRED — must match the other machine)
--server <URL>      Signaling server [default: wss://wail-signal.fly.dev]
--timeout <SECS>    Global timeout [default: 180]
--intervals <N>     Number of sustained audio intervals [default: 10]
--verbose           Debug-level tracing output
```
