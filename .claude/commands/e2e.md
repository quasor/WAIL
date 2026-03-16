---
description: Run full audio path end-to-end plugin integration tests (Send plugin → IPC → WebRTC → IPC → Recv plugin)
allowed-tools: [Bash]
---

# E2E Audio Test

Run the three wail-plugin-test integration scenarios that exercise the complete audio path using real compiled CLAP plugins.

## Instructions

1. Check if CLAP plugin bundles exist:
   `ls target/bundled/wail-plugin-send.clap target/bundled/wail-plugin-recv.clap 2>/dev/null`

2. If either bundle is missing, build them first:
   `cargo xtask build-plugin`

3. Run the integration tests (must be single-threaded — tests mutate process-global env vars):
   `cargo test -p wail-plugin-test --test send_recv_webrtc_e2e -- --test-threads=1 --nocapture`

4. Report results for the two scenarios:
   - `realtime_paced_no_dropout_e2e` — real-time paced (~30s), ZCR frequency alignment + dropout regression guard
   - `late_join_bidirectional_e2e` — late-join bidirectional (peer joins mid-session)
