---
default: patch
---

# Local debug log

The telemetry checkbox now saves logs to a local rotating flat file (`wail.log` in the app data directory, up to 10 × 50 MB archives) instead of streaming to Grafana Loki.
