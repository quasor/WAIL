# WAIL Architecture

## Overview

WAIL bridges Ableton Link sessions across the internet via WebRTC peer-to-peer DataChannels. Musicians on different networks sync tempo, phase, and interval boundaries as if they were on the same LAN. Audio is captured per interval (NINJAM-style), Opus-encoded, and transmitted over binary DataChannels. Two CLAP/VST3 plugins provide DAW integration: WAIL Send (capture, multiple instances supported) and WAIL Recv (playback, up to 31 per-slot auxiliary outputs).

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
                    │ HTTP polling                                           │ HTTP polling
                    │                                                        │
                    │              ┌──────────────────┐                      │
                    └─────────────►│ Signaling Server │◄─────────────────────┘
                                   │  (Val Town HTTP)  │
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

wail-plugin-send (CLAP/VST3, captures DAW audio, stream_index param 0-30)
├── wail-core
└── wail-audio

wail-plugin-recv (CLAP/VST3, plays remote audio, 31 aux outputs)
├── wail-core
└── wail-audio

wail-plugin-test (integration test harness for Send/Recv plugins)
├── wail-audio
└── wail-core

val-town/main.ts (HTTP signaling server, deployed to Val Town)
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

`IntervalRing` implements the NINJAM double-buffer with up to 31 remote slots, keyed by `ClientChannelMapping(client_id, channel_index)` — a persistent identity that survives reconnects:

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

Each unique `ClientChannelMapping` (persistent `client_id` + `channel_index`) is assigned its own playback slot and Recv plugin auxiliary output via a `SlotTable`. If all 31 slots are exhausted, overflow audio is merged into the peer's channel 0 slot.

Slot assignment uses **affinity**: when a peer disconnects, their `SlotTable` entries move from active to reserved. When the same persistent identity reconnects (possibly with a new session-scoped `peer_id`), they reclaim their original slots, keeping DAW aux routing stable across reconnects.

## Audio Flow

### Full Path (Plugin → Network → Plugin)

```
DAW Track A
  → WAIL Plugin A process() — IntervalRing records input samples
  → Interval boundary fires
  → IntervalRing.take_completed() returns raw f32 samples
  → AudioEncoder.encode_interval() — Opus encode (960-sample frames)
  → AudioWire.encode() — binary wire format (48-byte header + Opus data)
  → IPC TCP frame (length-prefixed) to WAIL App A
  → WebRTC binary DataChannel "audio" to Peer B
  → WAIL App B receives
  → IPC TCP frame to Plugin B
  → AudioWire.decode() — parse wire header + Opus payload
  → AudioDecoder.decode_interval() — Opus decode to f32
  → IntervalRing.feed_remote() — queue for next playback slot
  → Next boundary: remote audio becomes playback slot
  → WAIL Plugin B process() — IntervalRing reads playback to output
DAW Track B hears Peer A's previous interval
```

### AudioBridge

`AudioBridge` wraps the full encode/decode pipeline in a single struct:

- `process(input, output, beat_position)` → drives IntervalRing, returns wire bytes for completed intervals
- `receive_wire(peer_id, wire_data)` → decodes Opus, feeds to ring for playback (slot keyed by `ClientChannelMapping`)
- `update_config(bars, quantum, bpm)` → updates interval parameters from DAW transport

### Wire Format (AudioWire)

Binary header (48 bytes) + Opus payload:

```
[4 bytes]  magic: "WAIL"
[1 byte]   version: 2  (v1 also accepted for backward compat, stream_id defaults to 0)
[1 byte]   flags: bit 0 = stereo
[2 bytes]  stream_id: u16 LE  (was reserved in v1)
[8 bytes]  interval_index: i64 LE
[4 bytes]  sample_rate: u32 LE
[4 bytes]  num_frames: u32 LE (source samples per channel)
[8 bytes]  bpm: f64 LE
[8 bytes]  quantum: f64 LE
[4 bytes]  bars: u32 LE
[4 bytes]  opus_data_len: u32 LE
[N bytes]  opus_data
```

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
[N bytes]  payload:
  [1 byte]   tag (0x01 = AudioInterval)
  [1 byte]   peer_id_len
  [M bytes]  peer_id (UTF-8, empty for plugin→app outgoing)
  [K bytes]  AudioWire data (includes stream_id in wire header)
```

Plugin→App: local interval encoded as AudioWire, peer_id empty.
App→Plugin: remote peer's interval, peer_id identifies the sender.

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
2. Peer A POSTs join to HTTP signaling server (with room password, stream_count, client_version)
   - Server rejects outdated clients with 426 Upgrade Required (minimum version enforced server-side)
3. Server replies with list of existing peers
4. For each peer: lower peer_id creates SDP Offer (deterministic initiator)
5. Offer relayed through signaling server (HTTP polling)
6. Peer B creates Answer, relayed back
7. ICE candidates exchanged via signaling server
8. Two DataChannels established per peer:
   - "sync": ordered, text mode (JSON SyncMessages)
   - "audio": unordered, binary mode (AudioWire frames)
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
| _(binary audio)_ | audio | AudioWire | Opus-encoded interval data |

## Signaling Protocol Messages

| Message | Direction | Purpose |
|---------|-----------|---------|
| `Join` | Client → Server | Join a named room (includes `stream_count`, `client_version`) |
| `PeerList` | Server → Client | Current room members |
| `PeerJoined` | Server → Client | New peer notification |
| `PeerLeft` | Server → Client | Peer disconnect notification |
| `Signal` | Client ↔ Server ↔ Client | Relay SDP/ICE between peers |

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
