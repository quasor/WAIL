---
default: patch
---

Fix liveness watchdog incorrectly removing peers whose ICE connection never completed. Previously, the 30s watchdog would fire at the same time as the WebRTC ICE failure event, removing the peer from the mesh before `PeerFailed` could trigger reconnection. The watchdog now only applies to peers that have previously communicated; pre-connect failures are handled by the existing reconnection path (up to 5 attempts with exponential backoff). A 60s safety net handles the rare case where ICE hangs indefinitely without firing a Failed state.
