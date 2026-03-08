use std::time::{Duration, Instant};

use anyhow::Result;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn, Instrument};

use wail_audio::{ClientChannelMapping, IpcFramer, IpcMessage, IpcRecvBuffer, IPC_ROLE_RECV};
use wail_core::{ClockSync, IntervalTracker, LinkBridge, LinkCommand, LinkEvent, SyncMessage};
use wail_net::PeerMesh;

use crate::events::*;
use crate::emit_log;
use crate::emit_peer_log;
use crate::peers::{IpcWriterPool, PeerRegistry};
use crate::recorder::{RecordingConfig, SessionRecorder};
use crate::wslog::WsLogHandle;

/// Shorthand: log to tracing + emit to UI
macro_rules! ui_info {
    ($app:expr, $($arg:tt)+) => {{
        let msg = format!($($arg)+);
        info!("{}", msg);
        emit_log($app, "info", msg);
    }};
}

macro_rules! ui_warn {
    ($app:expr, $($arg:tt)+) => {{
        let msg = format!($($arg)+);
        warn!("{}", msg);
        emit_log($app, "warn", msg);
    }};
}

macro_rules! ui_error {
    ($app:expr, $($arg:tt)+) => {{
        let msg = format!($($arg)+);
        error!("{}", msg);
        emit_log($app, "error", msg);
    }};
}

pub struct SessionHandle {
    pub cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    pub peer_id: String,
    pub room: String,
}

pub enum SessionCommand {
    ChangeBpm(f64),
    Disconnect,
}

/// State for non-blocking signaling reconnection.
/// Instead of blocking the select! loop, reconnection is driven as a polled state machine.
struct SignalingReconnect {
    attempt: u32,
    next_try: tokio::time::Instant,
}

pub struct SessionConfig {
    pub server: String,
    pub room: String,
    pub password: Option<String>,
    pub display_name: String,
    pub identity: String,
    pub bpm: f64,
    pub bars: u32,
    pub quantum: f64,
    pub ipc_port: u16,
    pub recording: Option<RecordingConfig>,
    pub stream_count: u16,
}

pub fn spawn_session(app: AppHandle, config: SessionConfig) -> Result<SessionHandle> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let peer_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let room = config.room.clone();

    let handle = SessionHandle {
        cmd_tx,
        peer_id: peer_id.clone(),
        room: room.clone(),
    };

    let display_name = config.display_name.clone();
    let session_span = tracing::info_span!(
        "session",
        peer_id = %peer_id,
        room = %room,
        display_name = %display_name,
    );
    tauri::async_runtime::spawn(
        async move {
            if let Err(e) = session_loop(app.clone(), config, peer_id, cmd_rx).await {
                ui_error!(&app, "Session error: {e}");
                crate::hb::report(&e.to_string()).await;
                let _ = app.emit("session:error", SessionError { message: e.to_string() });
            }
            // Clear session state so join_room can be called again.
            // Without this, sessions that end due to signaling close or errors
            // leave stale state, causing "Already in a session" on next join.
            let state = app.state::<crate::commands::SessionState>();
            if let Ok(mut session) = state.lock() {
                *session = None;
            }
            let _ = app.emit("session:ended", SessionEnded {});
        }
        .instrument(session_span),
    );

    Ok(handle)
}

/// Notify recv plugins that a peer has left and remove the peer from the registry.
async fn remove_peer_fully(peers: &mut PeerRegistry, ipc_pool: &mut IpcWriterPool, peer_id: &str) {
    if !ipc_pool.is_empty() {
        let msg = IpcMessage::encode_peer_left(peer_id);
        let frame = IpcFramer::encode_frame(&msg);
        ipc_pool.broadcast(&frame).await;
    }
    peers.remove(peer_id);
}

async fn session_loop(
    app: AppHandle,
    config: SessionConfig,
    peer_id: String,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
) -> Result<()> {
    let SessionConfig {
        server,
        room,
        password,
        display_name,
        identity,
        bpm,
        bars,
        quantum,
        ipc_port,
        recording: recording_config,
        stream_count,
    } = config;

    ui_info!(&app, "Starting peer {peer_id} as {display_name} in room {room} (BPM {bpm}, {bars} bars, quantum {quantum})");

    // Initialize Ableton Link
    let link = LinkBridge::new(bpm, quantum);
    link.enable();
    let (link_cmd_tx, mut link_event_rx) = link.spawn_poller();
    ui_info!(&app, "Ableton Link enabled");

    // Fetch ICE servers (STUN + TURN) from Metered; fall back to STUN-only on failure.
    let ice_servers = match wail_net::fetch_metered_ice_servers().await {
        Ok(servers) => {
            ui_info!(&app, "Fetched {} ICE servers from Metered", servers.len());
            servers
        }
        Err(e) => {
            ui_warn!(&app, "Metered ICE fetch failed ({e}), using STUN fallback");
            wail_net::metered_stun_fallback()
        }
    };

    // Connect to signaling server
    let (mut mesh, mut sync_rx, mut audio_rx) =
        PeerMesh::connect_full(&server, &room, &peer_id, password.as_deref(), ice_servers, false, stream_count, Some(&display_name)).await?;
    ui_info!(&app, "Connected to signaling server at {server}");

    app.emit(
        "session:started",
        SessionStarted {
            peer_id: peer_id.clone(),
            room: room.clone(),
            bpm,
        },
    )?;

    // Clock sync and interval tracker
    let mut clock = ClockSync::new();
    let mut interval = IntervalTracker::new(bars, quantum);
    let mut ping_interval =
        tokio::time::interval(Duration::from_millis(ClockSync::ping_interval_ms()));
    let mut status_interval = tokio::time::interval(Duration::from_secs(2));

    // Consolidated peer state registry (replaces 11 separate HashMaps)
    let mut peers = PeerRegistry::new();
    peers.seed_names(mesh.take_initial_peer_names());

    // Track last broadcast tempo to avoid echo loops
    let mut last_broadcast_bpm: f64 = bpm;
    let mut beat_synced = false;
    let mut audio_gate = AudioSendGate::new();

    // Link peer count — updated every status tick; used to gate audio when Link is not running

    // Audio stats
    let mut audio_intervals_received: u64 = 0;
    let mut audio_bytes_sent: u64 = 0;
    let mut audio_intervals_sent: u64 = 0;
    let mut audio_bytes_recv: u64 = 0;

    // IPC: listen for plugin connections.
    // Use TcpSocket builder to set SO_REUSEADDR before binding, so that reconnecting
    // quickly after a disconnect doesn't fail with WSAEADDRINUSE (os error 10048) on Windows.
    let tcp_socket = tokio::net::TcpSocket::new_v4()?;
    tcp_socket.set_reuseaddr(true)?;
    tcp_socket.bind(std::net::SocketAddr::from(([127, 0, 0, 1], ipc_port)))?;
    let ipc_listener = tcp_socket.listen(128)?;
    ui_info!(&app, "IPC listening on port {ipc_port}");

    let mut ipc_pool = IpcWriterPool::new();
    let mut next_conn_id: usize = 0;
    let (ipc_from_plugin_tx, mut ipc_from_plugin_rx) = mpsc::channel::<Vec<u8>>(64);
    let (ipc_disconnect_tx, mut ipc_disconnect_rx) = mpsc::channel::<usize>(16);

    // Initialize local recording if configured
    let recorder: Option<SessionRecorder> = match recording_config {
        Some(ref cfg) if cfg.enabled => {
            match SessionRecorder::start(cfg.clone(), &room) {
                Ok(r) => {
                    ui_info!(&app, "Recording enabled: {}", cfg.directory);
                    Some(r)
                }
                Err(e) => {
                    ui_warn!(&app, "Failed to start recording: {e}");
                    None
                }
            }
        }
        _ => None,
    };

    // Peer liveness tracking — detect silent disconnections
    let mut liveness_interval = tokio::time::interval(Duration::from_secs(15));
    const PEER_LIVENESS_TIMEOUT: Duration = Duration::from_secs(30);

    // Peer reconnection channels
    let (reconnect_tx, mut reconnect_rx) = mpsc::channel::<String>(16);
    const MAX_PEER_RECONNECT_ATTEMPTS: u32 = 5;
    const PEER_RECONNECT_BASE_MS: u64 = 2000;
    const PEER_RECONNECT_MAX_MS: u64 = 16000;
    const SIGNALING_RECONNECT_BASE_MS: u64 = 1000;
    const SIGNALING_RECONNECT_MAX_MS: u64 = 30_000;
    const SIGNALING_RECONNECT_STALE_ATTEMPT: u32 = 10;

    // Non-blocking signaling reconnection state (None = connected)
    let mut signaling_reconnect: Option<SignalingReconnect> = None;

    // Peer log streaming: subscribe to the broadcast channel for forwarding to signaling.
    let ws_log_handle = app.state::<WsLogHandle>().inner().clone();
    let mut log_rx = ws_log_handle.subscribe();

    // Pending audio retry: (deadline, failed_peer_ids, wire_data, retries_remaining)
    // Up to 3 retries at 250ms intervals for transiently-failed peers.
    let mut audio_retry: Option<(tokio::time::Instant, Vec<String>, Vec<u8>, u32)> = None;

    ui_info!(&app, "Waiting for peers...");

    loop {
        tokio::select! {
            // --- UI commands ---
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    SessionCommand::ChangeBpm(new_bpm) => {
                        ui_info!(&app, "BPM changed to {new_bpm:.1}");
                        last_broadcast_bpm = new_bpm;
                        if link_cmd_tx.send(LinkCommand::SetTempo(new_bpm)).is_err() {
                            ui_warn!(&app, "Link bridge stopped");
                        }
                    }
                    SessionCommand::Disconnect => {
                        ui_info!(&app, "Disconnecting...");
                        break;
                    }
                }
            }

            // --- Outgoing peer log broadcast ---
            Ok(entry) = log_rx.recv(), if ws_log_handle.is_enabled() && signaling_reconnect.is_none() => {
                mesh.send_log(&entry.level, &entry.target, &entry.message, entry.timestamp_us);
            }

            // --- Accept plugin IPC connection ---
            result = ipc_listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let conn_id = next_conn_id;
                        next_conn_id += 1;
                        ui_info!(&app, "Plugin connected from {addr} (conn {conn_id})");
                        let (mut read_half, write_half) = stream.into_split();

                        // Read role byte to determine plugin type
                        let mut role_buf = [0u8; 1];
                        if read_half.read_exact(&mut role_buf).await.is_err() {
                            ui_warn!(&app, "Plugin (conn {conn_id}): failed to read role byte — dropping");
                            continue;
                        }
                        let role = role_buf[0];

                        // Send plugins send 2 additional bytes: stream_index as u16 LE
                        let mut stream_index: u16 = 0;
                        if role != IPC_ROLE_RECV {
                            let mut si_buf = [0u8; 2];
                            match tokio::time::timeout(
                                Duration::from_millis(200),
                                read_half.read_exact(&mut si_buf),
                            ).await {
                                Ok(Ok(_)) => {
                                    stream_index = u16::from_le_bytes(si_buf);
                                }
                                Ok(Err(e)) => {
                                    tracing::warn!("Plugin (conn {conn_id}): IO error reading stream_index: {e}");
                                }
                                Err(_) => {
                                    // Legacy send plugin — no stream_index, default to 0
                                }
                            }
                        }

                        let role_name = if role == IPC_ROLE_RECV { "recv" } else { "send" };
                        if role != IPC_ROLE_RECV {
                            ui_info!(&app, "Plugin (conn {conn_id}) identified as {role_name}, stream_index={stream_index}");
                        } else {
                            ui_info!(&app, "Plugin (conn {conn_id}) identified as {role_name}");
                        }

                        // Only recv plugins get forwarded audio from remote peers
                        if role == IPC_ROLE_RECV {
                            ipc_pool.push(conn_id, write_half);
                        } else {
                            // Send plugin — we don't need its write half (it only sends audio TO us)
                            drop(write_half);
                        }
                        let _ = app.emit("plugin:connected", ());

                        // Only send plugins push audio data to us via IPC
                        let tx = ipc_from_plugin_tx.clone();
                        let disconnect_tx = ipc_disconnect_tx.clone();
                        let app2 = app.clone();
                        let is_send = role != IPC_ROLE_RECV;
                        tokio::spawn(async move {
                            let mut recv_buf = IpcRecvBuffer::new();
                            let mut buf = [0u8; 65536];
                            let mut reader = read_half;
                            loop {
                                match reader.read(&mut buf).await {
                                    Ok(0) => {
                                        ui_info!(&app2, "Plugin disconnected (conn {conn_id})");
                                        let _ = app2.emit("plugin:disconnected", ());
                                        break;
                                    }
                                    Ok(n) => {
                                        if is_send {
                                            recv_buf.push(&buf[..n]);
                                            while let Some(frame) = recv_buf.next_frame() {
                                                match tx.try_send(frame) {
                                                    Ok(()) => {}
                                                    Err(mpsc::error::TrySendError::Full(_)) => {
                                                        debug!("IPC audio channel full — dropping frame");
                                                    }
                                                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                                                }
                                            }
                                        }
                                        // recv plugin: we only read to detect disconnect
                                    }
                                    Err(e) => {
                                        ui_warn!(&app2, "Plugin IPC read error (conn {conn_id}): {e}");
                                        let _ = app2.emit("plugin:disconnected", ());
                                        break;
                                    }
                                }
                            }
                            let _ = disconnect_tx.send(conn_id).await;
                        });
                    }
                    Err(e) => {
                        ui_error!(&app, "IPC accept failed: {e}");
                    }
                }
            }

            // --- Audio from plugin IPC → broadcast to WebRTC peers ---
            Some(frame) = ipc_from_plugin_rx.recv() => {
                // Streaming audio frames (20ms Opus chunks, tag 0x05)
                if let Some(wire_data) = IpcMessage::decode_audio_frame(&frame) {
                    if interval.current_index() <= Some(0) {
                        debug!("audio dropped — interval not started yet (idx={:?})", interval.current_index());
                        continue;
                    }
                    if audio_gate.is_gated() {
                        debug!("audio dropped — send gate is closed");
                        continue;
                    }
                    let failed_peers = mesh.broadcast_audio(&wire_data).await;
                    audio_bytes_sent += wire_data.len() as u64;
                    audio_intervals_sent += 1;
                    if !failed_peers.is_empty() {
                        // Don't retry individual frames — next frame will arrive in 20ms
                        debug!("Frame send failed for {} peers", failed_peers.len());
                    }
                }
            }

            // --- Signaling messages (disabled during reconnection — old mesh is dead) ---
            event = mesh.poll_signaling(), if signaling_reconnect.is_none() => {
                match event {
                    Ok(Some(wail_net::MeshEvent::PeerJoined { peer_id: pid, display_name: sig_name })) => {
                        let display = sig_name.as_deref().unwrap_or(&pid);
                        ui_info!(&app, "Peer {display} joined room");
                        peers.add(pid.clone(), sig_name.clone());
                        let _ = app.emit("peer:joined", PeerJoinedEvent {
                            peer_id: pid.clone(),
                            display_name: sig_name,
                        });

                        let hello = SyncMessage::Hello { peer_id: peer_id.clone(), display_name: Some(display_name.clone()), identity: Some(identity.clone()) };
                        mesh.broadcast(&hello).await;
                        // Mark all connected peers as having been sent Hello
                        // (messages are queued if DataChannel isn't open yet)
                        for p in mesh.connected_peers() {
                            peers.mark_hello_sent(&p);
                        }

                        let config_msg = SyncMessage::IntervalConfig { bars, quantum };
                        mesh.broadcast(&config_msg).await;

                        // Immediately sync the new peer to our current interval so their
                        // audio-send guard clears without waiting up to one full interval
                        // (~8s at 120 BPM, 4 bars). Without this, the joining peer's
                        // interval tracker starts at Some(0) and all outbound audio is
                        // dropped until the next natural boundary fires.
                        if let Some(idx) = interval.current_index() {
                            mesh.broadcast(&SyncMessage::IntervalBoundary { index: idx }).await;
                        }

                        let caps = SyncMessage::AudioCapabilities {
                            sample_rates: vec![48000],
                            channel_counts: vec![1, 2],
                            can_send: true,
                            can_receive: true,
                            max_streams: None,
                        };
                        mesh.broadcast(&caps).await;
                    }
                    Ok(Some(wail_net::MeshEvent::PeerLeft(pid))) => {
                        let name = peers.get(&pid).and_then(|p| p.display_name.as_deref()).unwrap_or(&pid).to_string();
                        ui_info!(&app, "Peer {name} left");
                        remove_peer_fully(&mut peers, &mut ipc_pool, &pid).await;
                        let _ = app.emit("peer:left", PeerLeftEvent { peer_id: pid });
                    }
                    Ok(Some(wail_net::MeshEvent::PeerFailed(pid))) => {
                        // Skip duplicate failure events while a reconnect timer is pending.
                        // A single connection failure fires multiple fail_tx sends (DC on_close
                        // for each channel + PeerConnectionState::Failed). Without this guard,
                        // each event would spawn its own timer and inflate reconnect_attempts.
                        if peers.get(&pid).is_some_and(|p| p.reconnect_pending) {
                            continue;
                        }

                        let name = peers.get(&pid).and_then(|p| p.display_name.as_deref()).unwrap_or(&pid).to_string();
                        let attempt = if let Some(peer) = peers.get_mut(&pid) {
                            peer.reconnect_attempts += 1;
                            peer.reconnect_attempts
                        } else {
                            1
                        };

                        if attempt > MAX_PEER_RECONNECT_ATTEMPTS {
                            ui_error!(&app, "Peer {name} reconnection failed after {MAX_PEER_RECONNECT_ATTEMPTS} attempts — giving up");
                            remove_peer_fully(&mut peers, &mut ipc_pool, &pid).await;
                            mesh.remove_peer(&pid).await;
                            let _ = app.emit("peer:left", PeerLeftEvent { peer_id: pid });
                        } else {
                            if let Some(peer) = peers.get_mut(&pid) {
                                peer.reconnect_pending = true;
                            }
                            let backoff_ms = (PEER_RECONNECT_BASE_MS * 2u64.pow(attempt - 1)).min(PEER_RECONNECT_MAX_MS);
                            let slot_tag = peers.slot_for(&pid, 0)
                                .map(|s| format!("slot={} ", s + 1))
                                .unwrap_or_default();
                            ui_warn!(&app, "{slot_tag}{name} connection failed — reconnecting in {backoff_ms}ms (attempt {attempt}/{MAX_PEER_RECONNECT_ATTEMPTS})");
                            let _ = app.emit("peer:reconnecting", PeerReconnectingEvent {
                                peer_id: pid.clone(),
                                attempt,
                                max_attempts: MAX_PEER_RECONNECT_ATTEMPTS,
                            });

                            let tx = reconnect_tx.clone();
                            let pid_clone = pid.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                                let _ = tx.send(pid_clone).await;
                            });
                        }
                    }
                    Ok(Some(wail_net::MeshEvent::PeerListReceived(n))) => {
                        audio_gate.on_peer_list(n);
                        // Seed liveness for initial peers so the watchdog can
                        // detect peers that connect but never send any messages.
                        peers.seed_last_seen();
                        if n == 0 {
                            ui_info!(&app, "First peer in room — audio send ungated");
                        } else {
                            ui_info!(&app, "Joined room with {n} peer(s) — audio send gated until beat sync");
                        }
                    }
                    Ok(Some(wail_net::MeshEvent::PeerLogBroadcast { from, level, message, .. })) => {
                        if ws_log_handle.is_enabled() {
                            let peer_name = peers.get(&from).and_then(|p| p.display_name.clone());
                            emit_peer_log(&app, &from, peer_name, &level, message);
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        ui_warn!(&app, "Signaling connection closed — attempting reconnection");
                        let _ = app.emit("session:reconnecting", ());
                        signaling_reconnect = Some(SignalingReconnect {
                            attempt: 1,
                            next_try: tokio::time::Instant::now()
                                + Duration::from_millis(SIGNALING_RECONNECT_BASE_MS),
                        });
                    }
                    Err(e) => {
                        ui_error!(&app, "Signaling error: {e}");
                    }
                }
            }

            // --- Pending peer reconnection ---
            Some(pid) = reconnect_rx.recv() => {
                // Clear the pending flag so failures from the NEW connection are detected.
                if let Some(peer) = peers.get_mut(&pid) {
                    peer.reconnect_pending = false;
                }
                if peers.get(&pid).is_some_and(|p| p.reconnect_attempts > 0) {
                    let name = peers.get(&pid).and_then(|p| p.display_name.as_deref()).unwrap_or(&pid).to_string();
                    ui_info!(&app, "Attempting reconnection to {name}...");
                    match mesh.re_initiate(&pid).await {
                        Ok(()) => {
                            ui_info!(&app, "Reconnection offer sent to {name}");
                            if let Some(peer) = peers.get_mut(&pid) {
                                peer.last_seen = Instant::now();
                                peer.hello_sent = false;
                            }
                            let hello = SyncMessage::Hello {
                                peer_id: peer_id.clone(),
                                display_name: Some(display_name.clone()),
                                identity: Some(identity.clone()),
                            };
                            mesh.broadcast(&hello).await;
                            for p in mesh.connected_peers() {
                                peers.mark_hello_sent(&p);
                            }
                        }
                        Err(e) => {
                            ui_warn!(&app, "Reconnection to {name} failed: {e}");
                        }
                    }
                }
            }

            // --- Signaling reconnection state machine (non-blocking) ---
            _ = async {
                if let Some(ref sr) = signaling_reconnect {
                    tokio::time::sleep_until(sr.next_try).await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if signaling_reconnect.is_some() => {
                let sr = signaling_reconnect.as_mut().unwrap();
                let attempt = sr.attempt;

                if attempt == SIGNALING_RECONNECT_STALE_ATTEMPT {
                    ui_warn!(&app, "Signaling reconnection stale after {attempt} attempts — still retrying");
                    let _ = app.emit("session:stale", SessionStale { attempts: attempt });
                }

                ui_info!(&app, "Signaling reconnect attempt {attempt}...");

                // Re-fetch ICE servers (TURN credentials may have expired)
                let ice = match wail_net::fetch_metered_ice_servers().await {
                    Ok(s) => s,
                    Err(_) => wail_net::metered_stun_fallback(),
                };

                match mesh.reconnect_signaling(
                    &server, &room, password.as_deref(), Some(&display_name), ice,
                ).await {
                    Ok(new_peer_names) => {
                        // Seed any genuinely new peers from the fresh join_ok.
                        // Existing peers remain connected — their WebRTC DataChannels
                        // are unaffected by the signaling reconnect.
                        peers.seed_names(new_peer_names);
                        audio_gate.on_reconnect();
                        signaling_reconnect = None;
                        ui_info!(&app, "Signaling reconnected (attempt {attempt}) — existing WebRTC connections preserved");
                        let _ = app.emit("session:reconnected", ());
                    }
                    Err(e) => {
                        ui_warn!(&app, "Signaling reconnect failed: {e}");
                        let next_attempt = attempt + 1;
                        let backoff_ms = (SIGNALING_RECONNECT_BASE_MS
                            * 2u64.pow(next_attempt.min(5) - 1))
                            .min(SIGNALING_RECONNECT_MAX_MS);
                        sr.attempt = next_attempt;
                        sr.next_try = tokio::time::Instant::now()
                            + Duration::from_millis(backoff_ms);
                    }
                }
            }

            // --- Audio retry (up to 3 attempts, 250ms apart) for transiently-failed peers ---
            _ = async {
                if let Some((deadline, _, _, _)) = &audio_retry {
                    tokio::time::sleep_until(*deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if audio_retry.is_some() => {
                if let Some((_, failed_peers, wire_data, retries_remaining)) = audio_retry.take() {
                    let mut still_failed = Vec::new();
                    for pid in &failed_peers {
                        if let Err(e) = mesh.send_audio_to(pid, &wire_data).await {
                            warn!(peer = %pid, error = %e, retries_remaining, "Audio retry failed");
                            still_failed.push(pid.clone());
                        } else {
                            debug!(peer = %pid, "Audio retry succeeded");
                        }
                    }
                    if !still_failed.is_empty() && retries_remaining > 1 {
                        audio_retry = Some((
                            tokio::time::Instant::now() + Duration::from_millis(250),
                            still_failed,
                            wire_data,
                            retries_remaining - 1,
                        ));
                    }
                }
            }

            // --- Incoming sync messages from peers ---
            Some((from, msg)) = sync_rx.recv() => {
                if let Some(peer) = peers.get_mut(&from) {
                    peer.last_seen = Instant::now();
                    peer.ever_received_message = true;
                }
                match msg {
                    SyncMessage::Hello { peer_id: pid, display_name: name, identity: remote_identity } => {
                        let name_display = name.as_deref().unwrap_or("(anonymous)").to_string();
                        ui_info!(&app, "Hello from {name_display} ({pid})");

                        // Update or add peer entry
                        if let Some(peer) = peers.get_mut(&pid) {
                            peer.display_name = name.clone();
                        } else {
                            peers.add(pid.clone(), name.clone());
                        }

                        if let Some(ref rid) = remote_identity {
                            // Evict any stale peer_id that holds this identity.
                            // This happens when a peer crashes and reconnects with a new peer_id
                            // before the old connection has been cleaned up — without eviction the
                            // old slot would still be occupied, forcing the reconnecting peer onto
                            // a new slot and breaking channel affinity.
                            let old_pid = peers
                                .find_by_identity(rid.as_str())
                                .filter(|old| old.as_str() != pid.as_str());

                            if let Some(ref old) = old_pid {
                                ui_info!(&app, "Peer {name_display} reconnected with new peer_id (old={old}, new={pid}) — evicting stale entry");
                                remove_peer_fully(&mut peers, &mut ipc_pool, old).await;
                                mesh.remove_peer(old).await;
                                let _ = app.emit("peer:left", PeerLeftEvent { peer_id: old.clone() });
                            }

                            if let Some(peer) = peers.get_mut(&pid) {
                                peer.identity = Some(rid.clone());
                            }
                            // Migrate any slots assigned under peer_id (fallback) before Hello
                            // arrived — audio DC and sync DC have independent ordering so audio
                            // can arrive before Hello, causing slots to be keyed by peer_id
                            // instead of the persistent identity UUID.
                            peers.rekey_peer_slots(&pid, rid);

                            // Assign slot for stream 0 (mirror recv plugin's logic for UI labeling)
                            let already_had_slot = peers.slot_for(&pid, 0).is_some();
                            if let Some(slot) = peers.assign_slot(&pid, 0) {
                                if !already_had_slot {
                                    let ccm = ClientChannelMapping::new(rid.as_str(), 0);
                                    ui_info!(&app, "[{}] {name_display} assigned to slot {}", ccm.short_id(), slot + 1);
                                }
                            }

                            // Notify recv plugins about peer identity for slot affinity
                            if !ipc_pool.is_empty() {
                                let msg = IpcMessage::encode_peer_joined(&pid, rid);
                                let frame = IpcFramer::encode_frame(&msg);
                                ipc_pool.broadcast(&frame).await;

                                // Send display name so the plugin can rename DAW aux ports
                                if let Some(ref display_name) = name {
                                    let name_msg = IpcMessage::encode_peer_name(&pid, display_name);
                                    let name_frame = IpcFramer::encode_frame(&name_msg);
                                    ipc_pool.broadcast(&name_frame).await;
                                }
                            }
                        }

                        // Clear reconnect tracking — peer is alive
                        if let Some(peer) = peers.get_mut(&pid) {
                            if peer.reconnect_attempts > 0 {
                                peer.reconnect_attempts = 0;
                                peer.reconnect_pending = false;
                                ui_info!(&app, "Peer {name_display} reconnected successfully");
                            }
                        }

                        // Reply with our Hello if we haven't sent one to this peer.
                        // This handles the case where the peer wasn't in mesh.peers
                        // when we originally broadcast Hello (responder timing).
                        if peers.mark_hello_sent(&from) {
                            let reply = SyncMessage::Hello {
                                peer_id: peer_id.clone(),
                                display_name: Some(display_name.clone()),
                                identity: Some(identity.clone()),
                            };
                            if let Err(e) = mesh.send_to(&from, &reply).await {
                                debug!(peer = %from, error = %e, "Failed to send Hello reply");
                                peers.clear_hello_sent(&from);
                            }
                        }

                        let _ = app.emit("peer:joined", PeerJoinedEvent {
                            peer_id: pid,
                            display_name: name,
                        });
                    }

                    SyncMessage::Ping { id, sent_at_us } => {
                        let pong = clock.handle_ping(id, sent_at_us);
                        if let Err(e) = mesh.send_to(&from, &pong).await {
                            debug!(peer = %from, error = %e, "Failed to send pong");
                        }
                    }

                    SyncMessage::Pong { id: _, ping_sent_at_us, pong_sent_at_us } => {
                        clock.handle_pong(&from, ping_sent_at_us, pong_sent_at_us);
                    }

                    SyncMessage::TempoChange { bpm: remote_bpm, .. } => {
                        let name = peers.get(&from).and_then(|p| p.display_name.as_deref()).unwrap_or(&from);
                        ui_info!(&app, "Tempo change from {name}: {remote_bpm:.1} BPM");
                        last_broadcast_bpm = remote_bpm;
                        if link_cmd_tx.send(LinkCommand::SetTempo(remote_bpm)).is_err() {
                            ui_warn!(&app, "Link bridge stopped — cannot apply remote tempo");
                        }
                        let _ = app.emit("tempo:changed", TempoChangedEvent {
                            bpm: remote_bpm,
                            source: "remote".into(),
                        });
                    }

                    SyncMessage::StateSnapshot { bpm: remote_bpm, beat: remote_beat, .. } => {
                        if !beat_synced {
                            beat_synced = true;
                            audio_gate.on_beat_synced();
                            ui_info!(&app, "Beat sync — snapped to beat {remote_beat:.2} — audio send gate lifted");
                            if link_cmd_tx.send(LinkCommand::ForceBeat(remote_beat)).is_err() {
                                ui_warn!(&app, "Link bridge stopped — cannot force beat");
                            }
                            interval.set_config(bars, quantum);
                        }
                        if (remote_bpm - last_broadcast_bpm).abs() > 0.01 {
                            last_broadcast_bpm = remote_bpm;
                            if link_cmd_tx.send(LinkCommand::SetTempo(remote_bpm)).is_err() {
                                ui_warn!(&app, "Link bridge stopped — cannot apply remote tempo");
                            }
                        }
                    }

                    SyncMessage::IntervalConfig { bars: remote_bars, quantum: remote_q } => {
                        ui_info!(&app, "Remote interval config: {remote_bars} bars, quantum {remote_q}");
                        interval.set_config(remote_bars, remote_q);
                    }

                    SyncMessage::AudioCapabilities { sample_rates, channel_counts, can_send, can_receive, max_streams } => {
                        ui_info!(&app, "Peer {from} audio: rates={sample_rates:?} ch={channel_counts:?} send={can_send} recv={can_receive} max_streams={max_streams:?}");
                    }

                    SyncMessage::AudioIntervalReady { interval_index, wire_size } => {
                        debug!(peer = %from, interval = interval_index, size = wire_size, "Audio interval incoming");
                    }

                    SyncMessage::IntervalBoundary { index } => {
                        // No UI log for interval boundaries — too noisy
                        let local = interval.current_index();
                        let behind = local.map_or(true, |l| index > l);
                        if behind {
                            debug!(local = ?local, remote = index, peer = %from, "Interval index behind — syncing");
                            interval.sync_to(index);
                        }
                    }

                    SyncMessage::AudioStatus { audio_dc_open, intervals_sent, intervals_received, plugin_connected } => {
                        let name = peers.get(&from).and_then(|p| p.display_name.as_deref()).unwrap_or(&from);
                        ui_info!(&app, "[REMOTE {name}] dc_open={audio_dc_open}, sent={intervals_sent}, recv={intervals_received}, plugin={plugin_connected}");
                    }
                }
            }

            // --- Incoming audio data from peers → forward to plugin ---
            Some((from, data)) = audio_rx.recv() => {
                if let Some(peer) = peers.get_mut(&from) {
                    peer.last_seen = Instant::now();
                    peer.ever_received_message = true;
                    peer.audio_recv_count += 1;
                }
                audio_intervals_received += 1;
                audio_bytes_recv += data.len() as u64;

                if let Some(ref rec) = recorder {
                    let name = peers.get(&from).and_then(|p| p.display_name.clone());
                    rec.record_peer(from.clone(), name, data.clone());
                }

                if !ipc_pool.is_empty() {
                    let msg = IpcMessage::encode_audio(&from, &data);
                    let frame = IpcFramer::encode_frame(&msg);
                    ipc_pool.broadcast(&frame).await;
                }
            }

            // --- IPC disconnect notification ---
            Some(conn_id) = ipc_disconnect_rx.recv() => {
                ipc_pool.remove(conn_id);
                ui_info!(&app, "IPC connection {conn_id} removed");
            }

            // --- Local Link events ---
            Some(event) = link_event_rx.recv() => {
                match event {
                    LinkEvent::TempoChanged { bpm: local_bpm, beat, timestamp_us } => {
                        if (local_bpm - last_broadcast_bpm).abs() > 0.01 {
                            ui_info!(&app, "Local tempo changed to {local_bpm:.1} BPM");
                            last_broadcast_bpm = local_bpm;
                            let msg = SyncMessage::TempoChange {
                                bpm: local_bpm,
                                quantum,
                                timestamp_us,
                            };
                            mesh.broadcast(&msg).await;
                            let _ = app.emit("tempo:changed", TempoChangedEvent {
                                bpm: local_bpm,
                                source: "local".into(),
                            });
                        }

                        if let Some(idx) = interval.update(beat) {
                            info!(interval = idx, beat = format!("{:.1}", beat), ">>> INTERVAL BOUNDARY <<<");
                            mesh.broadcast(&SyncMessage::IntervalBoundary { index: idx }).await;

                        }
                    }

                    LinkEvent::StateUpdate { bpm: local_bpm, beat, phase, quantum: q, timestamp_us } => {
                        let msg = SyncMessage::StateSnapshot {
                            bpm: local_bpm,
                            beat,
                            phase,
                            quantum: q,
                            timestamp_us,
                        };
                        mesh.broadcast(&msg).await;

                        if let Some(idx) = interval.update(beat) {
                            info!(interval = idx, beat = format!("{:.1}", beat), ">>> INTERVAL BOUNDARY <<<");
                            mesh.broadcast(&SyncMessage::IntervalBoundary { index: idx }).await;

                        }
                    }
                }
            }

            // --- Peer liveness watchdog ---
            _ = liveness_interval.tick() => {
                // Previously-connected peers that went silent.
                let dead_peers = peers.timed_out_peers(PEER_LIVENESS_TIMEOUT);
                for dead_id in dead_peers {
                    let name = peers.get(&dead_id).and_then(|p| p.display_name.as_deref()).unwrap_or(&dead_id).to_string();
                    ui_warn!(&app, "Peer {name} timed out (no messages for {PEER_LIVENESS_TIMEOUT:?})");
                    remove_peer_fully(&mut peers, &mut ipc_pool, &dead_id).await;
                    mesh.remove_peer(&dead_id).await;
                    let _ = app.emit("peer:left", PeerLeftEvent { peer_id: dead_id });
                }
                // Safety net: peers whose ICE never connected and are stuck beyond 2× the normal
                // timeout. The PeerFailed path handles the common case (~30s ICE timeout); this
                // catches the rare case where WebRTC never fires a Failed state at all.
                const PRE_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
                let stale_peers = peers.stale_preconnect_peers(PRE_CONNECT_TIMEOUT);
                for stale_id in stale_peers {
                    let name = peers.get(&stale_id).and_then(|p| p.display_name.as_deref()).unwrap_or(&stale_id).to_string();
                    ui_warn!(&app, "Peer {name} never connected — removing after {PRE_CONNECT_TIMEOUT:?}");
                    remove_peer_fully(&mut peers, &mut ipc_pool, &stale_id).await;
                    mesh.remove_peer(&stale_id).await;
                    let _ = app.emit("peer:left", PeerLeftEvent { peer_id: stale_id });
                }
            }

            // --- Periodic clock sync pings ---
            _ = ping_interval.tick() => {
                let ping = clock.make_ping();
                mesh.broadcast(&ping).await;
            }

            // --- Periodic status update (every 2s) ---
            _ = status_interval.tick() => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                if link_cmd_tx.send(LinkCommand::GetState(tx)).is_err() {
                    debug!("Link bridge stopped — cannot query state");
                    continue;
                }
                if let Ok(state) = rx.await {
                    let connected = mesh.connected_peers();
                    let dc_open = mesh.any_audio_dc_open();
                    let is_sending = mesh.any_audio_dc_open();
                    let peer_infos: Vec<PeerInfo> = connected
                        .iter()
                        .map(|p| {
                            // Extract all data from the immutable borrow before any mutable borrow
                            let (recv_now, recv_prev, prev_status, display_name) = {
                                let ps = peers.get(p);
                                (
                                    ps.map_or(0, |s| s.audio_recv_count),
                                    ps.map_or(0, |s| s.audio_recv_prev),
                                    ps.map_or(String::new(), |s| s.prev_status.clone()),
                                    ps.and_then(|s| s.display_name.clone()),
                                )
                            };
                            let is_receiving = recv_now > recv_prev;
                            let is_sending_to_peer = is_sending && mesh.is_peer_audio_dc_open(p);
                            let status = peers.derive_status(p);

                            // Log status transitions and update prev_status (requires mutable borrow)
                            if prev_status != status {
                                let name = display_name.as_deref().unwrap_or(p);
                                let slot_tag = peers.slot_for(p, 0)
                                    .map(|s| format!("slot={} ", s + 1))
                                    .unwrap_or_default();
                                if prev_status.is_empty() {
                                    ui_info!(&app, "{slot_tag}{name} status: {status}");
                                } else {
                                    ui_info!(&app, "{slot_tag}{name} status: {prev_status} → {status}");
                                }
                                if let Some(peer) = peers.get_mut(p) {
                                    peer.prev_status = status.to_string();
                                }
                            }

                            PeerInfo {
                                peer_id: p.clone(),
                                display_name,
                                rtt_ms: clock.rtt_us(p).map(|rtt| rtt as f64 / 1000.0),
                                slot: peers.slot_for(p, 0).map(|s| s as u32 + 1),
                                status: status.to_string(),
                                is_sending: is_sending_to_peer,
                                is_receiving,
                            }
                        })
                        .collect();

                    // Build slot-centric view from the SlotTable
                    let slot_infos: Vec<SlotInfo> = peers.slot_table().active_mappings()
                        .iter()
                        .map(|(mapping, slot_idx)| {
                            // Find the peer_id for this identity
                            let peer_id = peers.find_by_identity(&mapping.client_id);
                            let peer_state = peer_id.as_deref().and_then(|pid| peers.get(pid));
                            let is_sending_to = peer_id.as_deref()
                                .map(|pid| is_sending && mesh.is_peer_audio_dc_open(pid))
                                .unwrap_or(false);
                            let is_receiving_from = peer_state
                                .map(|ps| ps.audio_recv_count > ps.audio_recv_prev)
                                .unwrap_or(false);
                            SlotInfo {
                                slot: *slot_idx as u32 + 1,
                                short_id: mapping.short_id(),
                                client_id: mapping.client_id.clone(),
                                channel_index: mapping.channel_index,
                                display_name: peer_state.and_then(|ps| ps.display_name.clone()),
                                status: peer_id.as_deref().map(|pid| peers.derive_status(pid).to_string()),
                                rtt_ms: peer_id.as_deref().and_then(|pid| clock.rtt_us(pid).map(|rtt| rtt as f64 / 1000.0)),
                                is_sending: is_sending_to,
                                is_receiving: is_receiving_from,
                            }
                        })
                        .collect();

                    // Build per-peer WebRTC network state for the Network tab
                    let network_infos: Vec<PeerNetworkInfo> = connected
                        .iter()
                        .filter_map(|p| {
                            let (ice, dc_sync, dc_audio) = mesh.peer_network_state(p)?;
                            let ps = peers.get(p);
                            Some(PeerNetworkInfo {
                                peer_id: p.clone(),
                                display_name: ps.and_then(|s| s.display_name.clone()),
                                slot: peers.slot_for(p, 0).map(|s| s as u32 + 1),
                                ice_state: ice,
                                dc_sync_state: dc_sync,
                                dc_audio_state: dc_audio,
                                rtt_ms: clock.rtt_us(p).map(|rtt| rtt as f64 / 1000.0),
                                audio_recv: ps.map_or(0, |s| s.audio_recv_count),
                            })
                        })
                        .collect();
                    let _ = app.emit("peers:network", PeersNetwork { peers: network_infos });

                    // Update snapshots for next tick
                    peers.flush_audio_recv_prev();
                    let _ = app.emit("status:update", StatusUpdate {
                        bpm: state.bpm,
                        beat: state.beat,
                        phase: state.phase,
                        link_peers: state.num_peers,
                        peers: peer_infos,
                        slots: slot_infos,
                        interval_bars: interval.bars(),
                        audio_sent: audio_intervals_sent,
                        audio_recv: audio_intervals_received,
                        audio_bytes_sent,
                        audio_bytes_recv,
                        audio_dc_open: dc_open,
                        plugin_connected: !ipc_pool.is_empty(),
                        audio_send_gated: audio_gate.is_gated(),
                        recording: recorder.is_some(),
                        recording_size_bytes: recorder.as_ref().map_or(0, |r| r.bytes_written()),
                    });

                    // Broadcast audio pipeline status to remote peers
                    let status_msg = SyncMessage::AudioStatus {
                        audio_dc_open: dc_open,
                        intervals_sent: audio_intervals_sent,
                        intervals_received: audio_intervals_received,
                        plugin_connected: !ipc_pool.is_empty(),
                    };
                    mesh.broadcast(&status_msg).await;
                }
            }
        }
    }

    // Finalize recording if active
    if let Some(ref rec) = recorder {
        rec.finalize();
        ui_info!(&app, "Recording finalized");
    }

    Ok(())
}


struct AudioSendGate {
    gated: bool,
}

impl AudioSendGate {
    fn new() -> Self {
        Self { gated: false }
    }

    /// Gates if joining a room that already has peers.
    fn on_peer_list(&mut self, peer_count: usize) {
        if peer_count > 0 {
            self.gated = true;
        } else {
            self.gated = false;
        }
    }

    /// Lifts gate once beat sync is achieved.
    fn on_beat_synced(&mut self) {
        self.gated = false;
    }

    /// Re-gates on signaling reconnection; beat sync must re-establish.
    fn on_reconnect(&mut self) {
        self.gated = true;
    }

    fn is_gated(&self) -> bool {
        self.gated
    }
}

#[cfg(test)]
mod tests {
    use super::AudioSendGate;

    #[test]
    fn first_peer_not_gated() {
        let mut gate = AudioSendGate::new();
        gate.on_peer_list(0);
        assert!(!gate.is_gated());
    }

    #[test]
    fn second_peer_gated_then_unlocked() {
        let mut gate = AudioSendGate::new();
        gate.on_peer_list(1);
        assert!(gate.is_gated());
        gate.on_beat_synced();
        assert!(!gate.is_gated());
    }

    #[test]
    fn reconnect_regates_until_beat_sync() {
        let mut gate = AudioSendGate::new();
        gate.on_peer_list(1);
        gate.on_beat_synced();
        assert!(!gate.is_gated());
        gate.on_reconnect();
        assert!(gate.is_gated());
        gate.on_peer_list(1);
        assert!(gate.is_gated());
        gate.on_beat_synced();
        assert!(!gate.is_gated());
    }

    #[test]
    fn first_peer_reconnects_to_empty_room() {
        let mut gate = AudioSendGate::new();
        gate.on_reconnect();
        assert!(gate.is_gated());
        gate.on_peer_list(0);
        assert!(!gate.is_gated());
    }
}
