---
default: minor
---

Thread `stream_count` through the join flow and add client version check. The signaling server now validates a minimum client version (0.4.16) and rejects outdated clients with 426 Upgrade Required, providing an actionable error message directing users to update.
