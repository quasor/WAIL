---
default: minor
---

Enable multiple WAIL Send plugin instances to send independent audio streams to the same peer. Each unique (peer_id, stream_id) pair gets its own auxiliary output slot in the Recv plugin. Send plugin includes stream_index parameter (0-30). Audio wire format bumped to v2 with stream_id encoding. Server enforces global capacity limit (SUM of stream_counts ≤ 31 per room) with 409 response on overflow.
