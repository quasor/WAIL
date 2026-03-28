# WAIL Architecture

## Overview

WAIL bridges Ableton Link sessions across the internet via WebRTC peer-to-peer DataChannels. Musicians on different networks sync tempo, phase, and interval boundaries as if they were on the same LAN. Audio is captured per interval (NINJAM-style), Opus-encoded, and transmitted over binary DataChannels. Two CLAP/VST3 plugins provide DAW integration: WAIL Send (capture, multiple instances supported) and WAIL Recv (playback, up to 15 per-slot auxiliary outputs).

## System Diagram

```
┌──────────────────────────────────────┐                    ┌──────────────────────────────────────┐
│  Peer A Machine                      │                    │  Peer B Machine                      │
│                                      │                    │                                      │
│  ┌──────────────────────────────┐    │                    │    ┌──────────────────────────────┐  │
│  │  DAW (Ableton, Bitwig, etc.) │    │                    │    │  DAW (Ableton, Bitwig, etc.) │  │
│  │                              │    │                    │    │                              │  │
│  │  Tracks: [WAIL Send ×N]      │    │                    │    │  Tracks: [WAIL Send ×N]      │  │
│  │          [WAIL Recv]         │    │                    │    │          [WAIL Recv]         │  │
│  └──────────┬───────────────────┘    │                    │    └──────────┬───────────────────┘  │
│             │ IPC (TCP :9191)        │                    │               │ IPC (TCP :9191)      │
│  ┌──────────┴───────────────────┐    │  WebRTC P2P        │    ┌──────────┴───────────────────┐  │
│  │  WAIL App                    │◄───┼──── DataChannels ──┼───►│  WAIL App                    │  │
│  │  ├─ Link bridge (50Hz poll)  │    │  "sync" (JSON)     │    │  ├─ Link bridge (50Hz poll)  │  │
│  │  └─ Audio relay              │    │  "audio" (binary)  │    │  └─ Audio relay              │  │
│  └──────────┬───────────────────┘    │                    │    └──────────┬───────────────────┘  │
│             │ Link (LAN multicast)   │                    │               │ Link (LAN multicast) │
│  ┌──────────┴───────────────────┐    │                    │    ┌──────────┴───────────────────┐  │
│  │  Ableton Live / Link app     │    │                    │    │  Ableton Live / Link app     │  │
│  └──────────────────────────────┘    │                    │    └──────────────────────────────┘  │
└──────────────────────────────────────┘                    └──────────────────────────────────────┘
                    │ WebSocket                                             │ WebSocket
                    │                                                        │
                    │              ┌──────────────────┐                      │
                    └─────────────►│ Signaling Server │◄─────────────────────┘
                                   │  (Go + SQLite)    │
                                   └──────────────────┘
```

## Crate Dependency Graph

```
wail-tauri (Tauri desktop app — session orchestration, IPC, recording)
├── wail-core (library — no networking deps)
│   └── rusty_link (Ableton Link C FFI)
├── wail-audio (library — no networking deps)
│   └── audiopus (Opus codec via libopus)
└── wail-net (library)
    ├── wail-core
    └── webrtc (pure Rust WebRTC)

wail-plugin-send (CLAP/VST3, captures DAW audio, stream_index param 0-14)
├── wail-core
└── wail-audio

wail-plugin-recv (CLAP/VST3, plays remote audio, 15 aux outputs)
├── wail-core
└── wail-audio

wail-plugin-test (integration test harness for Send/Recv plugins)
├── wail-audio
└── wail-core

wail-e2e (two-machine end-to-end test binary)
├── wail-core
├── wail-audio
└── wail-net

signaling-server/ (Go WebSocket signaling server, deployed to fly.io)
```

## The NINJAM Model

WAIL uses the NINJAM approach to intervalic audio. The core idea:

1. **Record** one full interval of local audio (e.g., 4 bars of 4/4 at 120 BPM = 8 seconds)
2. At the interval boundary, **transmit** the completed interval to all peers
3. Peers **play back** the received interval during the _next_ interval
4. Everyone hears everyone else delayed by exactly one interval

This means:
- **Latency = 1 interval** (e.g., 8 seconds at 4 bars / 120 BPM). This is by design, not a bug.
- **Sync is perfect** — all audio aligns to the same beat grid
- **Internet latency doesn't matter** as long as delivery completes within 1 interval
- Musicians adapt by playing "ahead" — the same mental model as NINJAM

### Why This Works for WAN

Traditional real-time audio requires <20ms round-trip latency. That's impossible over the internet. NINJAM sidesteps the problem: by accepting 1-interval latency, you can jam with anyone in the world. The music "works" because each interval is beat-aligned — you hear what the other person played last time, and play your response this time.

### The Double-Buffer

`IntervalRing` implements the NINJAM double-buffer with up to 15 remote slots, keyed by `ClientChannelMapping(client_id, channel_index)` — a persistent identity that survives reconnects:

```
Interval N:   [RECORD local audio] ──→ on boundary ──→ encode + transmit
              [PLAY remote audio from interval N-1]

Interval N+1: [RECORD local audio] ──→ on boundary ──→ encode + transmit
              [PLAY remote audio from interval N]
```

At each interval boundary:
- The record slot moves to the completed queue (for Opus encoding + transmission)
- Pending remote intervals are mixed (summed) into the playback slot
- Record and playback positions reset to zero

Late-arriving frames (remote audio for the current playback interval that arrives after the swap) are **live-appended** directly to the active playback slot rather than queued for the next boundary. This eliminates the "2 bars sound, 2 bars silence" dropout that occurs at real-time network pacing.

Each unique `ClientChannelMapping` (persistent `client_id` + `channel_index`) is assigned its own playback slot and Recv plugin auxiliary output via a `SlotTable`. If all 15 slots are exhausted, overflow audio is merged into the peer's channel 0 slot.

Slot assignment uses **affinity**: when a peer disconnects, their `SlotTable` entries move from active to reserved. When the same persistent identity reconnects (possibly with a new session-scoped `peer_id`), they reclaim their original slots, keeping DAW aux routing stable across reconnects.

## Audio Flow

### Full Path (Plugin → Network → Plugin)

```
DAW Track A
  → WAIL Send plugin process() — IntervalRing records input samples
  → Opus encode each 20ms frame (960 samples)
  → AudioFrameWire.encode() — WAIF streaming frame (21-byte header + Opus data)
  → IPC TCP frame (length-prefixed, tag 0x05) to WAIL App A
  → WebRTC binary DataChannel "audio" to Peer B
  → WAIL App B receives
  → IPC TCP frame (tag 0x01 with peer_id) to Recv Plugin B
  → FrameAssembler collects WAIF frames, assembles on final frame
  → AudioDecoder.decode_interval() — Opus decode to f32
  → IntervalRing.feed_remote() — live-append if interval is currently playing,
                                  otherwise queue for next playback slot
  → Next boundary: queued remote audio becomes playback slot
  → WAIL Recv plugin process() — IntervalRing reads playback to output
DAW Track B hears Peer A's previous interval
```

### AudioBridge

`AudioBridge` wraps the full encode/decode pipeline in a single struct:

- `process(input, output, beat_position)` → drives IntervalRing, returns wire bytes for completed intervals
- `receive_wire(peer_id, wire_data)` → decodes Opus, feeds to ring for playback (slot keyed by `ClientChannelMapping`)
- `update_config(bars, quantum, bpm)` → updates interval parameters from DAW transport

### Wire Format (AudioFrameWire / WAIF)

Streaming format: one WAIF frame per 20ms Opus packet. The final frame of an interval carries metadata so the receiver can reconstruct the full interval.

```
[4 bytes]  magic: "WAIF"
[1 byte]   flags: bit 0 = stereo, bit 1 = final (last frame of interval)
[2 bytes]  stream_id: u16 LE
[8 bytes]  interval_index: i64 LE
[4 bytes]  frame_number: u32 LE (0-indexed within interval)
[2 bytes]  opus_len: u16 LE
[N bytes]  opus_data

If final flag set, append:
[4 bytes]  sample_rate: u32 LE
[4 bytes]  total_frames: u32 LE
[8 bytes]  bpm: f64 LE
[8 bytes]  quantum: f64 LE
[4 bytes]  bars: u32 LE
```

On the receiver side, `FrameAssembler` (in `wail-audio`) collects WAIF frames keyed by `(interval_index, stream_id, peer_id)` and assembles them into the length-prefixed Opus blob format that `AudioDecoder::decode_interval` expects.

### IPC Protocol (Plugin ↔ App)

TCP connection to `127.0.0.1:9191`. On connect, the plugin sends a handshake:

```
[1 byte]   role: 0x01 = Send, 0x02 = Recv
[2 bytes]  stream_index: u16 LE  (Send plugins only; identifies which stream this instance captures)
```

Legacy send plugins that omit `stream_index` default to stream 0 (the app uses a 200ms read timeout for backward compatibility).

After the handshake, length-prefixed binary framing:

```
[4 bytes]  payload_length: u32 LE
[N bytes]  payload (tagged message, see below)
```

Message tags:

| Tag | Name | Payload layout |
|-----|------|----------------|
| `0x01` | AudioInterval | `peer_id_len (1B) + peer_id (UTF-8) + AudioWire data` |
| `0x02` | PeerJoined | `peer_id_len (1B) + peer_id + identity_len (1B) + identity` |
| `0x03` | PeerLeft | `peer_id_len (1B) + peer_id` |
| `0x04` | PeerName | `peer_id_len (1B) + peer_id + name_len (1B) + display_name` |
| `0x05` | AudioFrame | `AudioFrameWire data (WAIF streaming frame)` |

Send Plugin→App: AudioFrame (tag 0x05) — WAIF streaming frames with no peer_id (local capture).
App→Recv Plugin: AudioInterval (tag 0x01) with peer_id identifying the remote sender; PeerJoined/PeerLeft/PeerName for peer lifecycle and display name updates.

## Tempo Sync Flow

```
1. User changes tempo in DAW A
2. Link broadcasts on LAN
3. WAIL App A Link bridge detects change (50Hz poll)
4. Echo guard check: was this our own recent remote-applied change?
5. If genuine local change → serialize as SyncMessage::TempoChange
6. Broadcast via PeerMesh to all "sync" DataChannels (JSON)
7. Remote peers receive, parse, apply to their local Link via set_tempo()
8. Echo guard activated on remote to prevent re-broadcast loop
9. Remote DAWs see tempo change via Link
```

## WebRTC Connection Establishment

```
1. Peer A fetches ICE servers (Metered TURN credentials via API)
2. Peer A connects to WebSocket signaling server, sends join (with room password, stream_count, client_version)
   - Server rejects outdated clients with join_error code "version_outdated"
3. Server replies with join_ok containing list of existing peers
4. For each peer: lower peer_id creates SDP Offer (deterministic initiator)
5. Offer relayed through signaling server (WebSocket push — instant delivery)
6. Peer B creates Answer, relayed back
7. ICE candidates exchanged via signaling server
8. Two DataChannels established per peer:
   - "sync": ordered, text mode (JSON SyncMessages)
   - "audio": unordered, binary mode (WAIF streaming frames)
9. Signaling server exits the data path
```

## Interval Boundaries

Each peer computes interval boundaries independently from its local beat position:

```
interval_index = floor(beat_position / (bars × quantum))
```

Example: 4 bars × 4.0 quantum = 16 beats per interval. Beat 15.9 → interval 0. Beat 16.0 → interval 1.

**WAN peers' boundaries are NOT synchronized.** Peer A might cross into interval 1 while Peer B is still in interval 0. This is fine for NINJAM semantics — you always play the _previous_ interval, so drift up to 1 full interval is tolerable. As long as the wire data arrives before the receiver's _next_ boundary, it gets played on time.

## Clock Domains

Two independent time domains exist in the system:

1. **Link clock** (`link.clock_micros()`): Ableton Link's internal monotonic clock, used for beat/phase synchronization. This is the authoritative clock for interval boundaries.

2. **ClockSync epoch** (`std::time::Instant::now()`): Used by WAIL's Ping/Pong protocol to measure peer-to-peer RTT. Clock offset computation was removed — these two clocks are different domains and cannot be combined. ClockSync RTT is useful for diagnostics (displaying latency to peers) but does not participate in interval boundary calculations.

## Sync Protocol Messages

| Message | Channel | Format | Purpose |
|---------|---------|--------|---------|
| `Ping` | sync | JSON | Clock sync request |
| `Pong` | sync | JSON | Clock sync response |
| `TempoChange` | sync | JSON | BPM change from local Link |
| `StateSnapshot` | sync | JSON | Periodic full state (every 200ms) |
| `IntervalConfig` | sync | JSON | Agree on interval bars/quantum |
| `Hello` | sync | JSON | Greeting on connect |
| `AudioCapabilities` | sync | JSON | Announce send/receive support |
| `AudioIntervalReady` | sync | JSON | Metadata before binary audio |
| `StreamNames` | sync | JSON | Human-readable names for sender's audio streams |
| _(binary audio)_ | audio | WAIF (AudioFrameWire) | Opus-encoded streaming frames |

## Signaling Protocol Messages

| Message | Direction | Purpose |
|---------|-----------|---------|
| `Join` | Client → Server | Join a named room (includes `stream_count`, `client_version`) |
| `PeerList` | Server → Client | Current room members |
| `PeerJoined` | Server → Client | New peer notification |
| `PeerLeft` | Server → Client | Peer disconnect notification |
| `Signal` | Client ↔ Server ↔ Client | Relay SDP/ICE between peers |
| `LogBroadcast` (`log`) | Client → Server → Room | Broadcast structured log entry to all room peers (opt-in) |
| `MetricsReport` | Client → Server | Per-peer audio frame counts + pipeline state (consumed server-side, not relayed) |

## Key Design Decisions

1. **NINJAM over real-time**: 1-interval latency makes WAN jams possible without sub-20ms RTT. Musicians adapt to the delay.

2. **Binary DataChannel for audio**: Separate from the JSON "sync" channel. Avoids base64 overhead and JSON parsing for large audio payloads.

3. **Opus codec**: Designed for interactive audio. 48kHz, configurable bitrate (64-128 kbps). Frame size = 960 samples (20ms).

4. **Poll-based Link monitoring** (50Hz): Polling is simpler than cross-thread callbacks, and 20ms is fast enough for tempo changes.

5. **Echo guard** (150ms): Prevents infinite tempo change ping-pong when applying remote changes to local Link.

6. **Deterministic WebRTC initiator**: Lower peer_id always creates the offer, preventing simultaneous offer collision.

7. **wail-core and wail-audio have no network deps**: Reusable from the CLAP/VST3 plugin without pulling in webrtc/tokio-tungstenite.

8. **IPC over TCP** (not shared memory): Simpler, cross-platform, reliable. Latency is negligible compared to the 1-interval NINJAM delay.

9. **JSON sync protocol**: Readable for debugging. Bandwidth is negligible for small sync messages.

10. **Stable slot assignment via `ClientChannelMapping`**: Each remote audio channel is identified by `ClientChannelMapping(client_id, channel_index)` where `client_id` is a persistent UUID. A `SlotTable` manages assignment, affinity reservations, and reclamation. When a peer disconnects, their slot entries move to reserved; on reconnect with the same identity, they reclaim the same slots. This prevents DAW routing from breaking during brief network interruptions.

11. **Local session recording**: Sessions can be recorded to WAV files — either a single mixed file or per-peer stems. Managed by `recorder.rs` in wail-tauri.

12. **Fade-in on peer join**: When a new or reconnecting peer's first audio interval arrives, a 10ms linear ramp-from-silence is applied before mixing into the playback buffer. This prevents audible pops/clicks caused by abrupt sample onset. The fade length is clamped to the interval length for safety. After the first interval, subsequent intervals play at full amplitude with no ramping.

## Session Metrics and Live Dashboard

The signaling server tracks aggregate session metrics to monitor whether clients are establishing DataChannels and whether audio is flowing between peers.

### Session model

A **session** starts when the 2nd peer joins a room (≥2 peers) and ends when the count drops below 2. Sessions have two phases:

1. **Joining** — from session start until all peers report `dc_open` and `plugin_connected`. Captures ICE negotiation, DataChannel establishment, and plugin attachment.
2. **Playing** — steady-state audio flow after all peers are fully connected.

### Per-direction metrics

For each unique direction (e.g., `peer1→peer2`), the server tracks metrics independently per phase (joining vs playing). This distinguishes setup-related issues from steady-state network quality.

**Frame-level metrics:**
- `frames_expected` / `frames_received` / `frames_dropped` — tracked via zero-copy WAIF header parsing (`peek_waif_header`) in session.rs as frames pass through. Each "frame" is a single 20ms WAIF streaming Opus packet.

**Network health metrics (per direction):**
- `rtt_us` — median RTT to the peer (µs), from `ClockSync` Ping/Pong
- `jitter_us` — mean absolute deviation from median RTT (µs), the key signal for intermittent issues
- `dc_drops` — DataChannel backpressure drops (audio receiver channel full in peer.rs)
- `late_frames` — WAIF frames that arrived for already-passed intervals (detected in session.rs)
- `decode_failures` — Opus decode failures reported by the recv plugin via `IPC_TAG_METRICS` (0x06)

**Session-level metrics:**
- `ipc_drops` — cumulative IPC channel-full drops (plugin → app direction)
- `boundary_drift_us` — interval boundary timing drift (actual − expected gap, µs)

Clients report cumulative per-peer metrics every 2 seconds via a `metrics_report` message on the signaling WebSocket. The server computes playing-phase deltas by snapshotting cumulative values at the joining→playing transition. Point-in-time values (RTT, jitter) are overwritten with the latest report.

### Endpoints

| Path | Description |
|------|-------------|
| `GET /metrics` | JSON snapshot of active + completed sessions (`?room=` filter supported) |
| `WS /metrics/ws` | Streaming metrics every 2s (`?room=` filter supported) |
| `GET /metrics/dashboard` | Live HTML dashboard with auto-reconnecting WebSocket |

### CLI tool

`signaling-server/cmd/wail-metrics/` queries the `/metrics` endpoint:

```sh
wail-metrics -server https://signal.wail.live -room my-room
wail-metrics -json   # raw JSON
```

## CI/CD Pipeline

Every push to `main` triggers continuous deployment:

1. `auto-release.yml` → consumes `.changeset/` files and conventional commits, bumps versions, updates CHANGELOG, creates a release PR, auto-merges it, then runs `knope release` (creates GitHub release + git tag) and dispatches artifact builds
2. `release.yml` → builds platform artifacts (macOS, Windows, Linux — plugins + Tauri installers) and uploads them to the GitHub release

The release and artifact dispatch steps run inline in `auto-release.yml` because `GITHUB_TOKEN` merges don't trigger other workflows. `release-on-merge.yml` remains as a fallback for manual merges of release PRs.

> **Note:** The auto-merge step uses `GITHUB_TOKEN` which cannot bypass branch protection rules. If required status checks or PR review requirements are ever added to `main`, the auto-merge will fail and you'll need a GitHub App token with bypass permissions, or exempt the `release` branch from those rules.
