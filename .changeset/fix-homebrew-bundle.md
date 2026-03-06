---
default: patch
---

Fix Homebrew install failing with "Not a directory (os error 20)" during plugin
bundling. Pre-build plugin libraries with --locked before the xtask bundle
assembly step to eliminate nested cargo calls in the Homebrew build sandbox.
Also add --locked to xtask's internal cargo build calls and improve error
context on all filesystem operations in bundle-plugin.
