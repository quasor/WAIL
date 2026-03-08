---
default: minor
---

Add test mode to the Tauri app for debugging audio without a DAW. Pass `--test-room <ROOM>` on the command line to auto-join a room that generates synthetic audio at interval boundaries and validates received audio with detailed logging. Also available via the `test_mode` parameter on the `join_room` command.
