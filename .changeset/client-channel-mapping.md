---
default: minor
---

Add `ClientChannelMapping` type and unified `SlotTable` for stable per-room DAW slot assignment. Remote peers now consistently map to the same aux output slot across disconnects and reconnects. The UI shows a slot-centric view instead of peer-centric, and log messages include slot numbers and mapping IDs for easier debugging.
