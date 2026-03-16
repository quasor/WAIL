---
description: Launch the test tone client — streams a pentatonic minor sine scale + echo into a WAIL room
allowed-tools: [Bash]
---

# Test Client

Launch `wail-test-client` which joins a room, streams a pentatonic minor sine scale (A3 root, one note per bar) on stream 0, and echoes any received audio back on stream 1.

## Arguments

The user may pass arguments after the slash command (e.g. `/test-client --room jam --bpm 90`). Forward all arguments directly to the binary.

If no arguments are provided, use the defaults (room `test-tone`, 120 BPM, 4 bars, quantum 4).

## Instructions

1. Build in release mode (first run compiles dependencies):
   `cargo build -p wail-test-client --release`

2. Run the binary, forwarding any user-provided arguments:
   `cargo run -p wail-test-client --release -- $ARGS`

   The binary runs until Ctrl+C. Let the user know it's running and what room it joined.

3. If the build fails, show the error and suggest checking that `libopus-dev` is installed.
