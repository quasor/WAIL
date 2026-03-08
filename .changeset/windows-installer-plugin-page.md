---
default: minor
---

On Windows, plugin (VST3 and CLAP) installation is now handled by the setup.exe installer instead of a runtime auto-install step that required admin privileges. The installer shows a plugin options page with checkboxes to choose which plugins to install and directory inputs to customize install paths (defaulting to `%CommonProgramFiles%\VST3` and `%CommonProgramFiles%\CLAP`). Uninstalling WAIL via Windows Settings > Add & Remove Programs also removes the installed plugins.
