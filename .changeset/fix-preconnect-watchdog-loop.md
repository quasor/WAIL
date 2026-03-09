---
default: patch
---

Fix infinite "stuck in pre-connect" reconnection loop when ICE fails. Previously, the liveness watchdog called `close_peer` which transitions WebRTC state to `Closed` (not `Failed`), so the failure callback never fired — `last_seen` was never updated and the watchdog fired every 5 seconds indefinitely. Now `close_peer` always signals failure directly, and the watchdog skips peers already being reconnected.
