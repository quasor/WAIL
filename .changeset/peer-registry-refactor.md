---
default: minor
---

Consolidate peer state into `PeerRegistry` and `IpcWriterPool`, removing dead code (`ClockSync` offset computation, `IntervalPlayer`) and extending signaling integration tests.
