---
default: patch
---

Fix plugins going inactive in Bitwig by returning `ProcessStatus::KeepAlive` instead of `ProcessStatus::Normal`. Both send and recv plugins must stay active at all times to maintain IPC communication with wail-app, even when outputting silence.
