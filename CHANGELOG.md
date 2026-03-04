# Changelog

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
