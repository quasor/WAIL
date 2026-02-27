# Audio Next Steps

## Current State

WAIL currently syncs Ableton Link tempo/phase/interval across the internet via WebRTC DataChannels. No audio is transmitted yet.

## Phase 1: Audio Capture/Playback via CLAP/VST3 Plugin

### Goal
A CLAP/VST3 plugin (via `nih-plug`) inserted on a DAW track that captures audio and sends it to the WAIL standalone app, which forwards it to remote peers. Remote peers' plugin instances receive and play back the audio.

### Architecture

```
┌─────────────────────────────────────────┐
│  DAW (Ableton, Bitwig, Reaper, etc.)    │
│                                         │
│  Track 1: [WAIL Plugin] ──────────┐    │
│  Track 2: [WAIL Plugin] ────────┐ │    │
│                                  │ │    │
└──────────────────────────────────┼─┼────┘
                                   │ │
                     IPC (shared memory or localhost UDP)
                                   │ │
                    ┌──────────────┴─┴──────────────┐
                    │  WAIL Standalone App          │
                    │  ┌─────────────────────────┐   │
                    │  │ Audio mixing / routing  │   │
                    │  │ Opus encoding/decoding  │   │
                    │  │ Interval-aligned buffer │   │
                    │  └──────────┬──────────────┘   │
                    │             │ WebRTC            │
                    └─────────────┼──────────────────┘
                                  │
                          (to remote peers)
```

### New Crate: `wail-plugin`

```toml
[dependencies]
nih_plug = "0.1"     # CLAP/VST3 plugin framework
wail-core = { path = "../wail-core" }
```

- Implements `nih_plug::Plugin` trait
- Audio processing callback captures samples from DAW track
- Sends audio buffers to standalone app via IPC
- Receives remote audio and outputs to DAW

### Plugin ↔ App Communication

Options (in order of preference):
1. **Shared memory ring buffer** — lowest latency, cross-platform via `shared_memory` crate
2. **localhost UDP** — simple, slightly higher latency
3. **Unix domain socket / named pipe** — reliable, moderate latency

### Audio Codec

- **Opus** via `opus` or `audiopus` crate — designed for low-latency interactive audio
- Encode at 48kHz, configurable bitrate (64-128 kbps per channel)
- Frame sizes: 2.5ms, 5ms, 10ms, 20ms (trade latency vs quality)

### NINJAM-style Interval Audio

The key insight: audio is NOT real-time in the traditional sense. Like NINJAM:

1. Record one full interval of audio locally (e.g., 4 bars)
2. At the interval boundary, send the completed interval to all peers
3. Peers play back the received interval during the NEXT interval
4. Everyone hears everyone else delayed by exactly one interval

This means:
- Latency = 1 interval (e.g., 4 bars at 120 BPM = 8 seconds)
- But sync is perfect — all audio aligns to the same beat grid
- Internet latency doesn't matter (as long as < 1 interval)
- Musicians adapt by playing "ahead" — the same mental model as NINJAM

### Audio Protocol Extension

```rust
enum SyncMessage {
    // ... existing messages ...

    /// Audio data for one interval
    AudioInterval {
        interval_id: u64,
        channel: u8,        // which track/stream
        sample_rate: u32,
        codec: AudioCodec,
        data: Vec<u8>,      // Opus-encoded audio
    },
}

enum AudioCodec {
    Opus,
    Raw,  // for debugging
}
```

For large audio payloads, switch from DataChannel to WebRTC media tracks or chunk the data.

## Phase 2: Low-Latency Audio (Optional, Advanced)

For musicians who want tighter sync than NINJAM-style:

### Approach: Adaptive Latency

- Start with interval-based sync (safe, always works)
- Measure RTT between peers
- If RTT is low enough (< 30ms), switch to direct streaming
- Use jitter buffer to smooth out network variance
- Fall back to interval mode if conditions degrade

### WebRTC Media Tracks

Instead of DataChannels for audio, use actual WebRTC audio tracks:
- Built-in congestion control (REMB, transport-cc)
- Jitter buffer handling
- Opus codec support
- `webrtc-rs` supports media tracks already

### New Dependencies

```toml
cpal = "0.15"           # Cross-platform audio I/O (for standalone mode)
opus = "0.3"            # Opus codec bindings
rubato = "0.16"         # Sample rate conversion
ringbuf = "0.4"         # Lock-free ring buffer for audio thread
```

## Phase 3: Multi-Track Routing

### Goal
Each participant can have multiple tracks, with per-track volume control and routing.

```
Peer A sends:  Track 1 (guitar), Track 2 (drums)
Peer B sends:  Track 1 (bass)
Peer C sends:  Track 1 (vocals), Track 2 (keys)

Each peer's plugin instances render their own mix of remote tracks.
```

### Design
- Each plugin instance registers as a numbered channel
- Standalone app maintains a routing matrix
- Remote peers receive all channels, mix locally
- Plugin UI shows per-peer, per-channel volume faders

## Implementation Order

1. **Plugin skeleton** — nih-plug CLAP/VST3 that passes audio through
2. **IPC bridge** — shared memory ring buffer between plugin and app
3. **Opus encoding** — encode audio intervals with Opus
4. **Interval audio exchange** — send/receive audio at interval boundaries over DataChannel
5. **Playback alignment** — buffer and play received audio aligned to beat grid
6. **Multi-track** — multiple plugin instances, channel routing
7. **Low-latency mode** — optional direct streaming for low-RTT peers

## Platform Notes

### macOS
- CLAP/VST3 bundles, codesigning required for distribution
- Audio Units (AU) also possible via nih-plug
- `cpal` uses CoreAudio

### Windows
- CLAP/VST3 DLLs
- `cpal` uses WASAPI
- May need ASIO support for pro audio (via `cpal` feature flag)

### Cross-Platform Build
- nih-plug handles all plugin format packaging
- `cargo xtask bundle wail-plugin --release` produces platform-specific bundles
