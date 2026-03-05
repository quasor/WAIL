---
default: patch
---

fix: gate audio sending until beat-locked when joining a room with existing peers

When a peer joins a non-empty room, audio intervals are now held back until the first `StateSnapshot` message establishes beat sync with the room. This prevents unsynchronized audio from reaching remote peers outside interval boundaries. First peer and reconnection-to-empty-room cases remain ungated immediately. Gate state is also exposed in `StatusUpdate` for UI display.
