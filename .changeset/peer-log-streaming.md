---
default: minor
---

Add peer log streaming. Clients can opt in via Settings to broadcast their structured tracing output (INFO and above) to other peers in the session via the signaling WebSocket. Peers with the toggle enabled see remote logs in the session log panel with a peer name prefix.
