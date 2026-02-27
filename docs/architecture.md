# WAIL Architecture

## Overview

WAIL bridges Ableton Link sessions across the internet via WebRTC peer-to-peer DataChannels. The system is transparent to DAWs — they see normal Link tempo/phase changes.

## System Diagram

```
┌─────────────────────┐                              ┌─────────────────────┐
│   Peer A Machine    │                              │   Peer B Machine    │
│                     │                              │                     │
│  ┌──────────────┐   │                              │   ┌──────────────┐  │
│  │ Ableton Live │   │                              │   │ Ableton Live │  │
│  │  (or any     │   │                              │   │  (or any     │  │
│  │  Link app)   │   │                              │   │  Link app)   │  │
│  └──────┬───────┘   │                              │   └──────┬───────┘  │
│         │ Link      │                              │          │ Link     │
│         │ (LAN)     │                              │          │ (LAN)    │
│  ┌──────┴───────┐   │    WebRTC DataChannel (P2P)  │   ┌──────┴───────┐  │
│  │  WAIL App   │◄──┼──────────────────────────────┼──►│  WAIL App   │  │
│  └──────┬───────┘   │                              │   └──────┬───────┘  │
│         │           │                              │          │          │
└─────────┼───────────┘                              └──────────┼──────────┘
          │ WebSocket                                           │ WebSocket
          │                                                     │
          │            ┌──────────────────┐                     │
          └───────────►│ Signaling Server │◄────────────────────┘
                       │ (room-based WS)  │
                       └──────────────────┘
```

## Crate Dependency Graph

```
wail-app (binary)
├── wail-core (library)
│   └── rusty_link (Ableton Link C FFI)
└── wail-net (library)
    ├── wail-core
    └── webrtc (pure Rust WebRTC)

wail-signaling (binary, standalone)
└── wail-core (for protocol types only)
```

## Data Flow

### Tempo Change Propagation

```
1. User changes tempo in Ableton Live
2. Link broadcasts on LAN
3. WAIL Link bridge detects change (50Hz poll)
4. Echo guard check: was this our own recent change?
5. If genuine local change → serialize as SyncMessage::TempoChange
6. Broadcast via PeerMesh to all WebRTC DataChannels
7. Remote peers receive, parse, apply to their local Link via set_tempo()
8. Echo guard activated on remote to prevent re-broadcast
9. Remote DAWs see tempo change via Link
```

### WebRTC Connection Establishment

```
1. Peer A connects to signaling server via WebSocket
2. Peer A sends Join { room, peer_id }
3. Server sends PeerList of existing peers
4. For each peer in list, Peer A creates RTCPeerConnection
5. Deterministic initiator: lower peer_id creates SDP Offer
6. Offer relayed through signaling server to Peer B
7. Peer B creates Answer, relayed back
8. ICE candidates exchanged via signaling server
9. DataChannel "sync" established directly between peers
10. Signaling server no longer in the data path
```

### Clock Synchronization

```
Every 2 seconds, each peer:
1. Sends Ping { id, sent_at_us } to all peers
2. Receiver replies with Pong { id, ping_sent_at_us, pong_sent_at_us }
3. Sender computes:
   - RTT = now - ping_sent_at_us
   - offset = pong_sent_at_us - (ping_sent_at_us + RTT/2)
4. Sliding window of 8 samples, take median offset
5. Use offset to translate remote timestamps to local time
```

### Interval System (NINJAM-style)

```
- Interval = bars × quantum beats (e.g., 4 bars × 4 = 16 beats)
- interval_index = floor(beat / beats_per_interval)
- When interval_index changes → fire IntervalBoundary event
- All peers track independently using clock-offset-adjusted timestamps
- Future: audio swap happens at interval boundaries
```

## Sync Protocol Messages

| Message | Direction | Purpose |
|---------|-----------|---------|
| `Ping` | Peer → Peer | Clock sync request |
| `Pong` | Peer → Peer | Clock sync response |
| `TempoChange` | Peer → All | BPM change detected on local Link |
| `StateSnapshot` | Peer → All | Periodic full state (every 200ms) |
| `IntervalConfig` | Peer → All | Agree on interval bars/quantum |
| `Hello` | Peer → All | Greeting on connect |

## Signaling Protocol Messages

| Message | Direction | Purpose |
|---------|-----------|---------|
| `Join` | Client → Server | Join a named room |
| `PeerList` | Server → Client | Current room members |
| `PeerJoined` | Server → Client | New peer notification |
| `PeerLeft` | Server → Client | Peer disconnect notification |
| `Signal` | Client ↔ Server ↔ Client | Relay SDP/ICE between peers |

## Key Design Decisions

1. **Poll-based Link monitoring** (50Hz) vs callbacks: polling is simpler, avoids cross-thread callback complexity, and 20ms is fast enough for tempo changes.

2. **Echo guard** (150ms): prevents infinite tempo change ping-pong when applying remote changes to local Link.

3. **Deterministic WebRTC initiator**: lower peer_id always creates the offer, preventing both peers from creating offers simultaneously.

4. **wail-core has no network deps**: keeps it reusable from a future nih-plug CLAP/VST3 plugin without pulling in webrtc/tokio-tungstenite.

5. **JSON protocol**: readable for debugging; switch to bincode/msgpack later if bandwidth matters.

6. **webrtc-rs v0.11 (not latest)**: v0.17.x is the final tokio-coupled stable release. Master is moving to sans-I/O architecture (v0.20) which isn't production-ready yet.
