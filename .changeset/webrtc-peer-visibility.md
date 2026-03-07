---
default: minor
---

Add WebRTC peer network visibility tab and fix signaling reconnect to preserve live connections.

- **Network tab**: New "Network" tab in the session screen shows per-peer ICE state, sync/audio DataChannel states, RTT, and audio recv count, updated every 2 seconds.
- **Signaling reconnect fix**: Reconnecting to the signaling server no longer tears down established WebRTC peer connections. `PeerMesh::reconnect_signaling()` replaces only the WebSocket while leaving `self.peers` intact — audio and sync DataChannels continue uninterrupted.
- **Audio retry**: If broadcasting an audio interval to a peer fails transiently, up to 3 retries are attempted at 250ms intervals.
