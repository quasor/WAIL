---
default: patch
---

Eliminate audio-thread allocation in wail-plugin-send by wiring the buffer return channel. After 2-3 interval warmup, `spare_record` is replenished from returned buffers instead of `Vec::with_capacity()`.
