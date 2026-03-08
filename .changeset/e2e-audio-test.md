---
default: minor
---

Add real plugin-to-plugin WebRTC E2E test that loads both the Send and Recv CLAP plugins and validates audio flowing through the full stack: Send plugin → IPC → WebRTC DataChannel → IPC → Recv plugin.
