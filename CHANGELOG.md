# Changelog

## 1.16.1 (2026-03-11)

### Fixes

- remove duplicate changelog entries and prevent future accumulation (#214)

## 1.16.0 (2026-03-11)

### Features

- add peer chat to session view (#209)
- show own stereo pair sends in session view (#212)
- Windows release now ships as a plain zip file (WAIL.exe + plugins + opus.dll) instead of a Chocolatey installer.

### Fixes

- prevent audio dropout at interval boundaries and add channel overflow logging (#207)
- isolate Opus decoder per interval to prevent state leak at boundaries (#210)
- align connected badges in peer list (#211)

## 1.15.3 (2026-03-10)

### Fixes

- update nuspec schema namespace for Chocolatey v2 compatibility (#204)

## 1.15.2 (2026-03-10)

### Fixes

- correct Windows exe filename in release workflow (#202)

## 1.15.1 (2026-03-10)

### Fixes

- replace --bundles none with --no-bundle for tauri-cli v2 (#199)
- audio playback during NINJAM intervals (#201)

## 1.15.0 (2026-03-09)

### Features

- visual refresh of Tauri UI — clean minimal design (Linear/Raycast inspired) (#198)
- Replace Windows NSIS installer with a Chocolatey package. Users install via `choco install wail --source <release-url>` with optional `--params "'/VST3Dir:path /CLAPDir:path'"` for custom plugin directories.

### Fixes

- create opus.dll placeholder on non-Windows to prevent tauri_build panic (#197)

## 1.14.7 (2026-03-09)

### Fixes

- revert auto-merge of release PRs, restore manual gate (#192)

## 1.14.6 (2026-03-09)

### Fixes

- compensate for network latency when syncing beat at join time (#189)
- wire buffer return channel in wail-plugin-send (#190)

## 1.14.5 (2026-03-09)

### Fixes

- add libx11-xcb-dev to Linux CI system dependencies (#186)
- remove incompatible --delete-branch flag from merge queue workflow (#187)
- Windows VST3 plugin discovery and installer setup failures (#183)

## 1.14.4 (2026-03-09)

### Fixes

- add libx11-xcb-dev to Linux CI system dependencies (#186)
- remove incompatible --delete-branch flag from merge queue workflow (#187)
- Automatically merge the release PR so every push to main flows straight through to a release without manual intervention.

## 1.14.3 (2026-03-09)

### Fixes

- log panel scrolls independently; top interface always visible (#181)

## 1.14.2 (2026-03-09)

### Fixes

- return KeepAlive from plugin process() to prevent DAW sleep (#179)

## 1.14.1 (2026-03-09)

### Fixes

- suppress third-party crate logs from peer broadcast (#177)

#### Auto-retry Hello handshake for peers stuck without slot assignment.

When ICE connects but the Hello exchange on the sync DataChannel fails (e.g. due to TURN server timeouts during ICE gathering), audio can flow while the session tab shows no slots. A new watchdog tier detects active peers with no identity: re-sends Hello after 5 seconds (soft retry) and forces a reconnect after 15 seconds (hard retry). This eliminates the need to manually disconnect and reconnect to recover slot assignments.

## 1.14.0 (2026-03-09)

### Features

- Added a simple editor UI to the WAIL Send and WAIL Recv plugins showing the plugin name, version number, and a clickable link to the GitHub project.

### Fixes

- add missing libx11-xcb-dev dependency for Linux build (#173)
- Suppress noisy third-party crate logs (e.g. webrtc-rs ICE messages) from the UI log panel. Only WARN+ events from non-wail crates are forwarded to the frontend.

## 1.13.1 (2026-03-09)

### Fixes

- prevent infinite pre-connect watchdog loop when ICE fails (#171)

## 1.13.0 (2026-03-08)

### Features

- add auto-generate button for music-themed room names (#165)
- add plugin installation page to Windows NSIS setup.exe (#167)

### Fixes

- suppress repeated peer status logs in UI (#166)
- prevent crash and log spam on plugin reset and disconnect (#168)

#### Fix crash when Bitwig (or other hosts) calls start_processing: plugin reset() was

dropping heap-allocated data (Strings, encoder/decoder objects) inside assert_no_alloc's
no-alloc zone. Wrapped reset() bodies in permit_alloc, matching the existing pattern
used in process().

## 1.12.2 (2026-03-08)

### Fixes

- improve peer list layout alignment in Tauri UI (#161)

## 1.12.1 (2026-03-08)

### Fixes

- add burst audio test and validate buffer headroom (#159)

## 1.12.0 (2026-03-08)

### Features

- Add test mode to the Tauri app for debugging audio without a DAW. Pass `--test-room <ROOM>` on the command line to auto-join a room that generates synthetic audio at interval boundaries and validates received audio with detailed logging. Also available via the `test_mode` parameter on the `join_room` command.

## 1.11.0 (2026-03-08)

### Features

- two-machine e2e tests and signaling reconnection fixes (#155)

## 1.10.1 (2026-03-08)

### Fixes

- remove AudioSendGate that permanently blocked audio after reconnect (#153)

## 1.10.0 (2026-03-08)

### Features

- add real Send→WebRTC→Recv plugin e2e test (#149)

### Fixes

- exclude private rooms from public server list after restart (#151)
- interval sync and peer liveness watchdog race conditions (#152)

## 1.9.2 (2026-03-08)

### Fixes

- remove link_peers audio guard, fix peer timeout spam, track intervals_sent (#148)

## 1.9.1 (2026-03-07)

### Fixes

- migrate send/recv plugins and app to WAIF streaming (#144)

## 1.9.0 (2026-03-07)

### Features

- NINJAM-style streaming audio: encode and transmit Opus frames every 20ms during the interval instead of batching the entire interval at the boundary. Receivers buffer frames progressively, improving delivery reliability and enabling shorter interval lengths on high-latency connections.

## 1.8.1 (2026-03-07)

### Fixes

- revert Linux rust-lld linker change (#139)
- add diagnostics and fix interval config reset behavior (#138)

## 1.8.0 (2026-03-07)

### Features

#### Add WebRTC peer network visibility tab and fix signaling reconnect to preserve live connections.

- **Network tab**: New "Network" tab in the session screen shows per-peer ICE state, sync/audio DataChannel states, RTT, and audio recv count, updated every 2 seconds.
- **Signaling reconnect fix**: Reconnecting to the signaling server no longer tears down established WebRTC peer connections. `PeerMesh::reconnect_signaling()` replaces only the WebSocket while leaving `self.peers` intact — audio and sync DataChannels continue uninterrupted.
- **Audio retry**: If broadcasting an audio interval to a peer fails transiently, up to 3 retries are attempted at 250ms intervals.

### Fixes

- Fix Windows reconnect error "Only one usage of each socket address" (os error 10048) by setting SO_REUSEADDR on the IPC TCP listener before binding.
- Speed up Windows CI builds by caching the vcpkg opus installation, stripping PDB debug symbol files before saving the Cargo cache (reducing cache size by ~500MB–2GB), and switching to the rust-lld linker for faster linking on Windows.

## 1.7.2 (2026-03-07)

### Fixes

- resolve peers stuck "CONNECTING" when audio arrives before Hello (#130)
- defer recv plugin slot name assignment until slots are active (#132)

## 1.7.1 (2026-03-07)

### Fixes

- discard stale plugin-buffered intervals during session warmup (#126)

## 1.7.0 (2026-03-07)

### Features

- Add peer log streaming. Clients can opt in via Settings to broadcast their structured tracing output (INFO and above) to other peers in the session via the signaling WebSocket. Peers with the toggle enabled see remote logs in the session log panel with a peer name prefix.

## 1.6.0 (2026-03-07)

### Features

- peer log streaming via signaling WebSocket (#120)
- Dynamic DAW aux output port names via nih_plug fork. CLAP hosts now show peer display names (e.g. "Ringo") instead of static "Slot 1" labels when peers join a session. Adds PeerName IPC message to forward display names from the Tauri session to the recv plugin. VST3 hosts still show static names (no equivalent API).

## 1.5.0 (2026-03-07)

### Features

- equal-power crossfade at interval boundaries (#118)
- Add `ClientChannelMapping` type and unified `SlotTable` for stable per-room DAW slot assignment. Remote peers now consistently map to the same aux output slot across disconnects and reconnects. The UI shows a slot-centric view instead of peer-centric, and log messages include slot numbers and mapping IDs for easier debugging.
- Replace HTTP polling signaling with WebSocket for instant message delivery. Connection setup drops from ~15s to under 1s. Adds a Go WebSocket signaling server (SQLite-backed, deployed on fly.io) and replaces the Val Town dependency.

### Fixes

- add error logging for buffer return channel failures (#116)
- Add sine wave round-trip test across interval boundaries to characterize crossfade quality.

## 1.4.3 (2026-03-07)

### Fixes

- eliminate audio-thread buffer allocation via return channel (#114)

## 1.4.2 (2026-03-06)

### Fixes

- auto-install plugins from Homebrew lib path, suppress dev-mode dialog (#110)

## 1.4.1 (2026-03-06)

### Fixes

- prevent duplicate PeerFailed events from cascading reconnects (#108)

## 1.4.0 (2026-03-06)

### Features

- WAIL now automatically installs the Send and Recv plugins to your DAW's plugin directories on first launch. No manual installation step required.
- Show stable peer status with send/receive arrow indicators and slot number in the peer list. The status badge no longer cycles between CONNECTED/SENDING/RECEIVING — instead, small ↑/↓ arrows light up when audio flows. Each peer now shows their DAW slot ("Peer N") beside their name.

### Fixes

- suppress audio transmission and warn when Link has no peers (#104)

## 1.3.0 (2026-03-06)

### Features

- add passthrough parameter to WAIL Send plugin (#98)
- Add first-launch screen for display name prompt and settings gear icon. The display name is now only asked on first launch and can be changed later via the settings panel. Telemetry and remember-settings checkboxes are moved to the settings panel for a cleaner join screen.
- Consolidate peer state into `PeerRegistry` and `IpcWriterPool`, removing dead code (`ClockSync` offset computation, `IntervalPlayer`) and extending signaling integration tests.

### Fixes

- homebrew install failing with ENOTDIR during plugin bundling (#96)
- evict stale peer entries on reconnect to restore channel affinity (#99)

## 1.2.0 (2026-03-06)

### Features

- Thread peer display names through the signaling layer so names appear immediately when peers join, instead of waiting for the WebRTC DataChannel Hello exchange. Peer names now render as "Ringo (f43add)" with the truncated peer ID always visible.

## 1.1.0 (2026-03-06)

### Features

- enforce strict conventional commit prefix rules (#88)
- add per-peer status tracking and display (#91)

### Fixes

- add fade-in to prevent audio pops when peers join (#90)

## 1.0.0 (2026-03-05)

### Features

- Promote to stable 1.0.0 release. Normal semver now applies — `feat:` commits produce minor bumps, `fix:` commits produce patch bumps.
- Enforce strict conventional commit prefix rules in CLAUDE.md.
- Add CI test job gating builds, bundle CLAP/VST3 plugins into installers, and remove manual plugin install button from UI.

## 0.4.17 (2026-03-05)

### Features

- Thread `stream_count` through the join flow and add client version check. The signaling server now validates a minimum client version (0.4.16) and rejects outdated clients with 426 Upgrade Required, providing an actionable error message directing users to update.

## 0.4.16 (2026-03-05)

### Features

- implement multi-send-streams feature (#77)
- Add local session recording to the WAIL app. New recording options in the Advanced settings panel let you capture jam sessions as WAV files to disk. Supports recording per-peer stems (separate WAV per participant) or a single mixed WAV. Includes configurable recording directory, retention policy (auto-delete old sessions after N days), and a live recording indicator with file size display in the session view.
- Enable multiple WAIL Send plugin instances to send independent audio streams to the same peer. Each unique (peer_id, stream_id) pair gets its own auxiliary output slot in the Recv plugin. Send plugin includes stream_index parameter (0-30). Audio wire format bumped to v2 with stream_id encoding. Server enforces global capacity limit (SUM of stream_counts ≤ 31 per room) with 409 response on overflow.
- Add public rooms: rooms created without a password are listed in a browsable directory. Both the desktop app and web listener show a "Public Rooms" tab with auto-refreshing room list. Signaling server gains `?action=list` and `?action=update` endpoints.

### Fixes

- simplify release skill search command (#76)
- gate audio sending until beat-locked when joining a room (#78)
- replace Grafana Loki telemetry with local rotating logs (#81)
- call audio_gate.on_peer_list() for all peer counts, not just n > 0 (#82)
- correct changeset frontmatter format for knope version bumping (#83)

## 0.4.15 (2026-03-04)

### Features

- add peer affinity slots for stable DAW aux routing (#71)

### Fixes

- homebrew double-nested plugins and stale release artifacts (#68)
- detect and recover from silent session disconnections (#72)

## 0.4.14 (2026-03-04)

### Fixes

- remove post_install from homebrew formula to fix sandbox EPERM (#66)

## 0.4.13 (2026-03-04)

### Features

- add /release slash command to merge release PRs (#62)

### Fixes

- auto-install DAW plugins during brew install (#60)

## 0.4.12 (2026-03-04)

### Fixes

- clear session state when session loop ends (#58)

## 0.4.11 (2026-03-04)

### Fixes

- point pkg-config to pkgconf binary and deploy formula to Homebrew tap (#56)

## 0.4.10 (2026-03-04)

### Fixes

- add pkg-config build dep to Homebrew formula for Opus discovery (#54)

## 0.4.9 (2026-03-04)

### Fixes

- set CMAKE_POLICY_VERSION_MINIMUM for CMake 4.x compat in Homebrew (#51)

## 0.4.8 (2026-03-03)

### Fixes

- sync Cargo.lock during release to fix Homebrew --locked build (#49)

## 0.4.7 (2026-03-03)

### Features

- add Homebrew from-source installation support (#47)

## 0.4.6 (2026-03-03)

### Features

- show bytes sent/received for Opus audio in session stats UI (#44)
- add automatic reconnection for WebRTC peers and signaling server (#46)

## 0.4.5 (2026-03-03)

### Features

- add e2e tests for CLAP plugins with IPC integration (#40)
- add Honeybadger error monitoring (#41)
- add Papertrail log forwarding with opt-in telemetry (#43)

## 0.4.4 (2026-03-03)

### Features

- add local session recording with stems/mixed modes (#23) (#34)
- integrate Metered TURN server credentials (#38)

### Fixes

- fix audio receive and TCP buffer bloat after plugin split (#36)

## 0.4.3 (2026-03-02)

### Features

- add automatic releases on PR merge to main (#30)
- add public rooms with discovery UI (#31)

### Fixes

- strip nested directory from knope tarball in CI (#32)
- split release CI into two phases for branch protection (#33)

## 0.4.2 (2026-03-02)

### Features

- add Linux support (#27)

## 0.4.1 (2026-03-02)

### Fixes

- Set up knope for release management and populate initial CHANGELOG

## 0.4.0

### Breaking Changes

- Removed standalone CLI app (`wail-app` crate) — use the Tauri desktop app instead
- Split single plugin into separate WAIL Send and WAIL Recv plugins

### Features

- Split plugin into separate send and receive plugins for clearer DAW routing
- Add web listener client for mobile listening
- Increase MAX_REMOTE_PEERS from 7 to 15

### Fixes

- Ensure display names are always exchanged between peers
- Add multiple STUN servers for ICE reliability
- Hide IPC port field (hardcoded to match plugins)
- Remove BPM input — let DAW handle tempo via Link
- Simplify TURN server configuration with sensible defaults
- Enable bundle generation in Tauri config
- Correct Windows artifact path in release workflow
