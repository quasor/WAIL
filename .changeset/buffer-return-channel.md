---
default: patch
---

Eliminate audio-thread buffer allocation via a return channel between the IPC encoding thread and IntervalRing. After warmup (2-3 intervals), the audio thread becomes completely allocation-free.
