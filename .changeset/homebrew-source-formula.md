---
default: minor
---

Add Homebrew source formula for macOS distribution. Users can `brew tap quasor/wail && brew install wail` to build from source, avoiding Gatekeeper quarantine issues with unsigned DMGs. The formula installs the WAIL desktop app, CLAP plugins, and VST3 plugins, with automatic symlinking to ~/Library/Audio/Plug-Ins/.
