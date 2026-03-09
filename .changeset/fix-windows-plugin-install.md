---
default: patch
---

Fix Windows plugin installation: NSIS installer now correctly bundles and copies both VST3 and CLAP plugins, and opus.dll is placed alongside plugin binaries so DAW hosts can load them.
