---
default: patch
---

Fix Windows reconnect error "Only one usage of each socket address" (os error 10048) by setting SO_REUSEADDR on the IPC TCP listener before binding.
