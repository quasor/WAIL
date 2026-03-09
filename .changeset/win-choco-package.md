---
default: minor
---

Replace Windows NSIS installer with a Chocolatey package. Users install via `choco install wail --source <release-url>` with optional `--params "'/VST3Dir:path /CLAPDir:path'"` for custom plugin directories.
