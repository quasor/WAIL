# Changelog

## 1.14.1 (2026-03-09)

### Fixes

- suppress third-party crate logs from peer broadcast (#177)
- Filter out third-party crate log warnings (tao, webrtc) from peer log broadcast so remote peers only see WAIL-specific messages.

#### Auto-retry Hello handshake for peers stuck without slot assignment.

When ICE connects but the Hello exchange on the sync DataChannel fails (e.g. due to TURN server timeouts during ICE gathering), audio can flow while the session tab shows no slots. A new watchdog tier detects active peers with no identity: re-sends Hello after 5 seconds (soft retry) and forces a reconnect after 15 seconds (hard retry). This eliminates the need to manually disconnect and reconnect to recover slot assignments.

## 1.14.0 (2026-03-09)

### Features

- Added a simple editor UI to the WAIL Send and WAIL Recv plugins showing the plugin name, version number, and a clickable link to the GitHub project.

### Fixes

- add missing libx11-xcb-dev dependency for Linux build (#173)
- Fix Linux CI build failure caused by missing `libx11-xcb-dev` system dependency.
- Suppress noisy third-party crate logs (e.g. webrtc-rs ICE messages) from the UI log panel. Only WARN+ events from non-wail crates are forwarded to the frontend.

## 1.13.1 (2026-03-09)

### Fixes

- prevent infinite pre-connect watchdog loop when ICE fails (#171)
- Fix infinite "stuck in pre-connect" reconnection loop when ICE fails. Previously, the liveness watchdog called `close_peer` which transitions WebRTC state to `Closed` (not `Failed`), so the failure callback never fired — `last_seen` was never updated and the watchdog fired every 5 seconds indefinitely. Now `close_peer` always signals failure directly, and the watchdog skips peers already being reconnected.

## 1.13.0 (2026-03-08)

### Features

- add auto-generate button for music-themed room names (#165)
- add plugin installation page to Windows NSIS setup.exe (#167)
- Add "Generate" button to auto-create fun music-themed room names from three word dictionaries (synthesis modifiers, sound elements, jam venues).
- On Windows, plugin (VST3 and CLAP) installation is now handled by the setup.exe installer instead of a runtime auto-install step that required admin privileges. The installer shows a plugin options page with checkboxes to choose which plugins to install and directory inputs to customize install paths (defaulting to `%CommonProgramFiles%\VST3` and `%CommonProgramFiles%\CLAP`). Uninstalling WAIL via Windows Settings > Add & Remove Programs also removes the installed plugins.

### Fixes

- suppress repeated peer status logs in UI (#166)
- prevent crash and log spam on plugin reset and disconnect (#168)
- Suppress repeated peer audio status logs in the UI. Previously, each peer's `AudioStatus` message was logged to the UI every 2 seconds even when nothing changed. Now the UI only shows a new line when `dc_open` or `plugin_connected` actually changes. Debug-level logging continues on every tick for file/console output.

#### Fix crash when Bitwig (or other hosts) calls start_processing: plugin reset() was

dropping heap-allocated data (Strings, encoder/decoder objects) inside assert_no_alloc's
no-alloc zone. Wrapped reset() bodies in permit_alloc, matching the existing pattern
used in process().

## 1.12.2 (2026-03-08)

### Fixes

- improve peer list layout alignment in Tauri UI (#161)
- Fix peer list alignment: separate slot label and peer name into distinct flex items so they are evenly distributed alongside the status badge and RTT display.

## 1.12.1 (2026-03-08)

### Fixes

- add burst audio test and validate buffer headroom (#159)

#### Increase peer audio DataChannel receive buffer from 64 to 256 frames and upgrade

silent drop logging from debug to warn. Add burst (zero-delay) audio phase to e2e
test to validate buffer headroom under high-frequency packet sends.

## 1.12.0 (2026-03-08)

### Features

- Add test mode to the Tauri app for debugging audio without a DAW. Pass `--test-room <ROOM>` on the command line to auto-join a room that generates synthetic audio at interval boundaries and validates received audio with detailed logging. Also available via the `test_mode` parameter on the `join_room` command.

## 1.11.0 (2026-03-08)

### Features

- two-machine e2e tests and signaling reconnection fixes (#155)

## 1.10.1 (2026-03-08)

### Fixes

- remove AudioSendGate that permanently blocked audio after reconnect (#153)
- Remove AudioSendGate that could permanently block audio after signaling reconnect. Add INFO/WARN logging for audio transmission milestones and frame drops.

## 1.10.0 (2026-03-08)

### Features

- add real Send→WebRTC→Recv plugin e2e test (#149)
- Add real plugin-to-plugin WebRTC E2E test that loads both the Send and Recv CLAP plugins and validates audio flowing through the full stack: Send plugin → IPC → WebRTC DataChannel → IPC → Recv plugin.

### Fixes

- exclude private rooms from public server list after restart (#151)
- interval sync and peer liveness watchdog race conditions (#152)
- Fix new peer's outbound audio being silently dropped for up to one full interval (~8 seconds at 120 BPM, 4 bars) after joining a room. When a peer joins, their interval tracker starts at index 0 from a fresh Link session. The audio-send guard (`interval.current_index() <= Some(0)`) blocked all outbound audio until the next natural interval boundary fired. Now the existing peer immediately broadcasts its current interval index to the new peer on join (queued if the sync DataChannel isn't open yet), so the guard clears as soon as the first sync message is delivered rather than waiting up to 8 seconds.
- Fix liveness watchdog incorrectly removing peers whose ICE connection never completed. Previously, the 30s watchdog would fire at the same time as the WebRTC ICE failure event, removing the peer from the mesh before `PeerFailed` could trigger reconnection. The watchdog now only applies to peers that have previously communicated; pre-connect failures are handled by the existing reconnection path (up to 5 attempts with exponential backoff). A 60s safety net handles the rare case where ICE hangs indefinitely without firing a Failed state.
- Private (password-protected) rooms no longer appear in the public server list after a server restart.

## 1.9.2 (2026-03-08)

### Fixes

- remove link_peers audio guard, fix peer timeout spam, track intervals_sent (#148)
- Fix audio frames silently dropped when Ableton Link has no LAN peers, fix timed-out peers reappearing in logs every 30 seconds, and fix `intervals_sent` always showing 0 in status logs.

## 1.9.1 (2026-03-07)

### Fixes

- migrate send/recv plugins and app to WAIF streaming (#144)
- Migrate send/recv plugins and recorder to WAIF streaming format. Send plugin now streams 20ms WAIF frames instead of full WAIL intervals. Recv plugin and recorder use shared FrameAssembler to reassemble frames into complete intervals. Fixed frame loss from undersized channel buffers and non-blocking sends.

## 1.9.0 (2026-03-07)

### Features

- NINJAM-style streaming audio: encode and transmit Opus frames every 20ms during the interval instead of batching the entire interval at the boundary. Receivers buffer frames progressively, improving delivery reliability and enabling shorter interval lengths on high-latency connections.

## 1.8.1 (2026-03-07)

### Fixes

- revert Linux rust-lld linker change (#139)
- add diagnostics and fix interval config reset behavior (#138)
- Fix IntervalTracker::set_config to only reset interval tracking when bars or quantum actually change. Previously, receiving a redundant IntervalConfig message (same values) would reset the tracker, briefly re-activating the warmup guard and potentially dropping outgoing audio. Also adds diagnostic logging at interval boundary swaps to help diagnose audio gap issues.
- Revert Linux rust-lld linker change that broke CI builds on Ubuntu runners.

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
- Fix peers stuck in "CONNECTING" status when audio arrives before Hello (DataChannel ordering race).

## 1.7.1 (2026-03-07)

### Fixes

- discard stale plugin-buffered intervals during session warmup (#126)
- Fix interval-0 audio flood on session start/rejoin: stale plugin-buffered intervals no longer sent to peers.

## 1.7.0 (2026-03-07)

### Features

- Add peer log streaming. Clients can opt in via Settings to broadcast their structured tracing output (INFO and above) to other peers in the session via the signaling WebSocket. Peers with the toggle enabled see remote logs in the session log panel with a peer name prefix.

## 1.6.0 (2026-03-07)

### Features

- peer log streaming via signaling WebSocket (#120)
- Dynamic DAW aux output port names via nih_plug fork. CLAP hosts now show peer display names (e.g. "Ringo") instead of static "Slot 1" labels when peers join a session. Adds PeerName IPC message to forward display names from the Tauri session to the recv plugin. VST3 hosts still show static names (no equivalent API).
- Add peer log streaming. Clients can opt in via Settings to broadcast their structured tracing output (INFO and above) to other peers in the session via the signaling WebSocket. Peers with the toggle enabled see remote logs in the session log panel with a peer name prefix.

## 1.5.0 (2026-03-07)

### Features

- equal-power crossfade at interval boundaries (#118)
- Add `ClientChannelMapping` type and unified `SlotTable` for stable per-room DAW slot assignment. Remote peers now consistently map to the same aux output slot across disconnects and reconnects. The UI shows a slot-centric view instead of peer-centric, and log messages include slot numbers and mapping IDs for easier debugging.
- Replace HTTP polling signaling with WebSocket for instant message delivery. Connection setup drops from ~15s to under 1s. Adds a Go WebSocket signaling server (SQLite-backed, deployed on fly.io) and replaces the Val Town dependency.

### Fixes

- add error logging for buffer return channel failures (#116)
- Eliminate audio-thread buffer allocation via a return channel between the IPC encoding thread and IntervalRing. After warmup (2-3 intervals), the audio thread becomes completely allocation-free.
- Add sine wave round-trip test across interval boundaries to characterize crossfade quality.

#### Replace linear fade-in with equal-power crossfade at interval boundaries.

Applies to all interval transitions (not just peer joins): saves the last 128
samples per channel (256 interleaved, matching NINJAM's MAX_FADE constant) from
each peer's outgoing interval and blends them into the head of the incoming
interval using sin/cos weights (sin²+cos²=1), preserving constant energy
throughout the transition. New peers and reconnecting peers retain fade-from-silence
behaviour since their crossfade tail is zero-initialized.

## 1.4.3 (2026-03-07)

### Fixes

- eliminate audio-thread buffer allocation via return channel (#114)

## 1.4.2 (2026-03-06)

### Fixes

- auto-install plugins from Homebrew lib path, suppress dev-mode dialog (#110)
- Auto-install plugins from Homebrew lib path; suppress false error dialog in dev mode.

## 1.4.1 (2026-03-06)

### Fixes

- prevent duplicate PeerFailed events from cascading reconnects (#108)

#### Fix stale peer list and broken audio after WebRTC peer disconnection.

A single ICE failure was generating up to 6 concurrent failure signals (from
Disconnected state, Failed state, DataChannel closes, and reader exits),
instantly exhausting the 5-attempt reconnect budget and spawning multiple
overlapping reconnect timers. Peers appeared stuck in the list while audio
stopped flowing to reconnected peers.

Now: Disconnected state no longer triggers failure signals (it's transient
and may recover), reader exits no longer duplicate DataChannel close signals,
and a `reconnect_pending` guard ensures only one reconnect timer runs per
peer per failure.

## 1.4.0 (2026-03-06)

### Features

- WAIL now automatically installs the Send and Recv plugins to your DAW's plugin directories on first launch. No manual installation step required.
- Show stable peer status with send/receive arrow indicators and slot number in the peer list. The status badge no longer cycles between CONNECTED/SENDING/RECEIVING — instead, small ↑/↓ arrows light up when audio flows. Each peer now shows their DAW slot ("Peer N") beside their name.

### Fixes

- suppress audio transmission and warn when Link has no peers (#104)
- Suppress audio transmission and show a UI warning when Ableton Link has no peers (Link Peers = 0). Without Link running in a DAW, interval boundaries are unsynchronized and transmitting audio serves no purpose.

## 1.3.0 (2026-03-06)

### Features

- add passthrough parameter to WAIL Send plugin (#98)
- Add first-launch screen for display name prompt and settings gear icon. The display name is now only asked on first launch and can be changed later via the settings panel. Telemetry and remember-settings checkboxes are moved to the settings panel for a cleaner join screen.
- Consolidate peer state into `PeerRegistry` and `IpcWriterPool`, removing dead code (`ClockSync` offset computation, `IntervalPlayer`) and extending signaling integration tests.
- Add passthrough parameter to WAIL Send plugin. When enabled, input audio passes through to the plugin output instead of being silenced.

### Fixes

- homebrew install failing with ENOTDIR during plugin bundling (#96)
- evict stale peer entries on reconnect to restore channel affinity (#99)

#### Fix Homebrew install failing with "Not a directory (os error 20)" during plugin

bundling. Pre-build plugin libraries with --locked before the xtask bundle
assembly step to eliminate nested cargo calls in the Homebrew build sandbox.
Also add --locked to xtask's internal cargo build calls and improve error
context on all filesystem operations in bundle-plugin.

#### Fix reconnecting peers being mapped to a different channel slot.

When a peer crashed and reconnected with a new peer_id before the old connection was cleaned up, the old slot remained occupied, forcing the reconnecting peer onto a new slot. The session now evicts the stale peer_id when a Hello arrives with an identity that already belongs to a different tracked peer, freeing the slot for the reconnecting peer to reclaim via affinity.

## 1.2.0 (2026-03-06)

### Features

- Thread peer display names through the signaling layer so names appear immediately when peers join, instead of waiting for the WebRTC DataChannel Hello exchange. Peer names now render as "Ringo (f43add)" with the truncated peer ID always visible.

## 1.1.0 (2026-03-06)

### Features

- enforce strict conventional commit prefix rules (#88)
- add per-peer status tracking and display (#91)

### Fixes

- add fade-in to prevent audio pops when peers join (#90)
- Fix audio pops/clicks when peers join or reconnect by applying a 10ms fade-in to a peer's first audio interval.

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

#### fix: gate audio sending until beat-locked when joining a room with existing peers

When a peer joins a non-empty room, audio intervals are now held back until the first `StateSnapshot` message establishes beat sync with the room. This prevents unsynchronized audio from reaching remote peers outside interval boundaries. First peer and reconnection-to-empty-room cases remain ungated immediately. Gate state is also exposed in `StatusUpdate` for UI display.

#### Local debug log

The telemetry checkbox now saves logs to a local rotating flat file (`wail.log` in the app data directory, up to 10 × 50 MB archives) instead of streaming to Grafana Loki.

## 0.4.15 (2026-03-04)

### Features

- add peer affinity slots for stable DAW aux routing (#71)

#### Add peer affinity slots — when a peer drops and rejoins, they reclaim their original DAW aux output slot.

Each WAIL installation now generates a persistent identity (UUID stored in app data) that survives restarts. This identity is exchanged in Hello messages and used by the recv plugin's IntervalRing to reserve slot assignments. When a peer disconnects, their slot is freed but an affinity reservation maps their identity to the old slot index. On reconnect (even with a new peer_id), the identity match reclaims the original slot.

Also adds slot number labels to the status update events so the frontend can display "Peer 1 (Ringo)", "Peer 2 (Paul)", etc.

### Fixes

- homebrew double-nested plugins and stale release artifacts (#68)
- detect and recover from silent session disconnections (#72)

#### Fixed

- Fix Homebrew formula installing doubly-nested plugin bundles (e.g. `wail-plugin-send.clap/wail-plugin-send.clap/Contents/...`)
- Fix release workflow uploading stale Tauri installer artifacts from cached `target` directory

#### Fix silent disconnections in long sessions by detecting DataChannel failures and dead reader tasks.

- Add `on_close` and `on_error` handlers to both sync and audio DataChannels (initiator and responder paths) so that DataChannel failures immediately signal peer failure via `failure_tx`
- Signal peer failure when sync/audio reader tasks exit (previously exited silently with no notification)
- Add peer liveness watchdog: track last message time per peer, close peers silent for >30 seconds
- Emit `session:stale` event after 10 failed signaling reconnection attempts so the UI can warn users
- Server-side eviction detection: signaling server now returns `evicted: true` when a deleted peer polls, and the client triggers reconnection instead of silently receiving empty responses

## 0.4.14 (2026-03-04)

### Fixes

- remove post_install from homebrew formula to fix sandbox EPERM (#66)

## 0.4.13 (2026-03-04)

### Features

- add /release slash command to merge release PRs (#62)

### Fixes

- auto-install DAW plugins during brew install (#60)
- Homebrew formula now automatically installs CLAP/VST3 plugins to ~/Library/Audio/Plug-Ins/ during `brew install`, removing the need for a separate `wail-install-plugins` command. The helper script is still available for manual reinstallation.

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

#### Automatic TURN server credentials via Metered

WAIL now fetches TURN relay credentials automatically from Metered's REST API at session start. This replaces the manual TURN URL/username/credential configuration in the join-room UI. Credentials are short-lived (not stored in source), and the app falls back to STUN-only if the fetch fails.

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
