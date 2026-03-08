---
default: patch
---

Fix new peer's outbound audio being silently dropped for up to one full interval (~8 seconds at 120 BPM, 4 bars) after joining a room. When a peer joins, their interval tracker starts at index 0 from a fresh Link session. The audio-send guard (`interval.current_index() <= Some(0)`) blocked all outbound audio until the next natural interval boundary fired. Now the existing peer immediately broadcasts its current interval index to the new peer on join (queued if the sync DataChannel isn't open yet), so the guard clears as soon as the first sync message is delivered rather than waiting up to 8 seconds.
