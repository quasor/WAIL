---
default: minor
---

Add public rooms: rooms created without a password are listed in a browsable directory. Both the desktop app and web listener show a "Public Rooms" tab with auto-refreshing room list. Signaling server gains `?action=list` and `?action=update` endpoints.
