# Changelog

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
