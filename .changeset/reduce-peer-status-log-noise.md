---
default: patch
---

Suppress repeated peer audio status logs in the UI. Previously, each peer's `AudioStatus` message was logged to the UI every 2 seconds even when nothing changed. Now the UI only shows a new line when `dc_open` or `plugin_connected` actually changes. Debug-level logging continues on every tick for file/console output.
