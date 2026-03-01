use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use wail_audio::{AudioDecoder, AudioEncoder, AudioInterval, AudioWire, IpcFramer, IpcMessage, IpcRecvBuffer};
use wail_core::{ClockSync, IntervalTracker, LinkBridge, LinkCommand, LinkEvent, SyncMessage};
use wail_net::PeerMesh;

use crate::events::*;
use crate::emit_log;

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
    SetTestTone(bool),
    Disconnect,
}

pub struct SessionConfig {
    pub server: String,
    pub room: String,
    pub password: String,
    pub display_name: Option<String>,
    pub bpm: f64,
    pub bars: u32,
    pub quantum: f64,
    pub ipc_port: u16,
    pub test_tone: bool,
    pub turn_url: Option<String>,
    pub turn_username: Option<String>,
    pub turn_credential: Option<String>,
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

    tauri::async_runtime::spawn(async move {
        if let Err(e) = session_loop(app.clone(), config, peer_id, cmd_rx).await {
            ui_error!(&app, "Session error: {e}");
            let _ = app.emit("session:error", SessionError { message: e.to_string() });
        }
        let _ = app.emit("session:ended", SessionEnded {});
    });

    Ok(handle)
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
        bpm,
        bars,
        quantum,
        ipc_port,
        test_tone,
        turn_url,
        turn_username,
        turn_credential,
    } = config;

    let name_str = display_name.as_deref().unwrap_or("(anonymous)");
    ui_info!(&app, "Starting peer {peer_id} as {name_str} in room {room} (BPM {bpm}, {bars} bars, quantum {quantum})");

    // Initialize Ableton Link
    let link = LinkBridge::new(bpm, quantum);
    link.enable();
    let (link_cmd_tx, mut link_event_rx) = link.spawn_poller();
    ui_info!(&app, "Ableton Link enabled");

    // Build ICE servers (STUN + optional TURN)
    let ice_servers = match (turn_url.as_deref(), turn_username.as_deref(), turn_credential.as_deref()) {
        (Some(url), Some(user), Some(cred)) => {
            ui_info!(&app, "TURN server configured: {url}");
            wail_net::ice_servers_with_turn(url, user, cred)
        }
        _ => wail_net::default_ice_servers(),
    };

    // Connect to signaling server
    let (mut mesh, mut sync_rx, mut audio_rx) =
        PeerMesh::connect_with_ice(&server, &room, &peer_id, &password, ice_servers).await?;
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

    // Track peers' display names
    let mut peer_names: HashMap<String, Option<String>> = HashMap::new();

    // Track last broadcast tempo to avoid echo loops
    let mut last_broadcast_bpm: f64 = bpm;
    let mut beat_synced = false;

    // Audio interval stats
    let mut audio_intervals_sent: u64 = 0;
    let mut audio_intervals_received: u64 = 0;

    // Test tone state
    let mut test_tone_enabled = test_tone;
    let mut test_tone_encoder: Option<AudioEncoder> = if test_tone_enabled {
        match AudioEncoder::new(48000, 1, 64) {
            Ok(enc) => {
                ui_info!(&app, "Test tone encoder ready (48kHz mono 64kbps)");
                Some(enc)
            }
            Err(e) => {
                ui_warn!(&app, "Failed to create test tone encoder: {e}");
                None
            }
        }
    } else {
        None
    };
    let mut audio_decoder: Option<AudioDecoder> = match AudioDecoder::new(48000, 1) {
        Ok(dec) => Some(dec),
        Err(e) => {
            ui_warn!(&app, "Failed to create audio decoder for logging: {e}");
            None
        }
    };
    let mut rng = StdRng::from_entropy();
    if test_tone_enabled {
        ui_info!(&app, "Test tone ENABLED — will generate sine tones at interval boundaries");
    }

    // IPC: listen for plugin connections
    let ipc_listener = tokio::net::TcpListener::bind(("127.0.0.1", ipc_port)).await?;
    ui_info!(&app, "IPC listening on port {ipc_port}");

    let mut ipc_writer: Option<tokio::net::tcp::OwnedWriteHalf> = None;
    let (ipc_from_plugin_tx, mut ipc_from_plugin_rx) = mpsc::channel::<Vec<u8>>(64);

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
                    SessionCommand::SetTestTone(enabled) => {
                        test_tone_enabled = enabled;
                        ui_info!(&app, "Test tone {}", if enabled { "ENABLED" } else { "DISABLED" });
                        if enabled && test_tone_encoder.is_none() {
                            match AudioEncoder::new(48000, 1, 64) {
                                Ok(enc) => {
                                    ui_info!(&app, "Test tone encoder ready (48kHz mono 64kbps)");
                                    test_tone_encoder = Some(enc);
                                }
                                Err(e) => ui_warn!(&app, "Failed to create test tone encoder: {e}"),
                            }
                        }
                    }
                    SessionCommand::Disconnect => {
                        ui_info!(&app, "Disconnecting...");
                        break;
                    }
                }
            }

            // --- Accept plugin IPC connection ---
            result = ipc_listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        ui_info!(&app, "Plugin connected from {addr}");
                        let (read_half, write_half) = stream.into_split();
                        ipc_writer = Some(write_half);
                        let _ = app.emit("plugin:connected", ());

                        let tx = ipc_from_plugin_tx.clone();
                        let app2 = app.clone();
                        tokio::spawn(async move {
                            let mut recv_buf = IpcRecvBuffer::new();
                            let mut buf = [0u8; 65536];
                            let mut reader = read_half;
                            loop {
                                match reader.read(&mut buf).await {
                                    Ok(0) => {
                                        ui_info!(&app2, "Plugin disconnected");
                                        let _ = app2.emit("plugin:disconnected", ());
                                        break;
                                    }
                                    Ok(n) => {
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
                                    Err(e) => {
                                        ui_warn!(&app2, "Plugin IPC read error: {e}");
                                        let _ = app2.emit("plugin:disconnected", ());
                                        break;
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        ui_error!(&app, "IPC accept failed: {e}");
                    }
                }
            }

            // --- Audio from plugin IPC → broadcast to WebRTC peers ---
            Some(frame) = ipc_from_plugin_rx.recv() => {
                if let Some((_peer_id, wire_data)) = IpcMessage::decode_audio(&frame) {
                    mesh.broadcast_audio(&wire_data).await;
                    audio_intervals_sent += 1;
                    let peers = mesh.connected_peers();
                    ui_info!(&app, "[AUDIO SEND] wire={} bytes, peers=[{}], total_sent={}", wire_data.len(), peers.join(", "), audio_intervals_sent);
                }
            }

            // --- Signaling messages ---
            event = mesh.poll_signaling() => {
                match event {
                    Ok(Some(wail_net::MeshEvent::PeerJoined(pid))) => {
                        ui_info!(&app, "Peer {pid} joined room");
                        peer_names.insert(pid.clone(), None);
                        let _ = app.emit("peer:joined", PeerJoinedEvent {
                            peer_id: pid.clone(),
                            display_name: None,
                        });

                        let hello = SyncMessage::Hello { peer_id: peer_id.clone(), display_name: display_name.clone() };
                        mesh.broadcast(&hello).await;

                        let config_msg = SyncMessage::IntervalConfig { bars, quantum };
                        mesh.broadcast(&config_msg).await;

                        let caps = SyncMessage::AudioCapabilities {
                            sample_rates: vec![48000],
                            channel_counts: vec![1, 2],
                            can_send: true,
                            can_receive: true,
                        };
                        mesh.broadcast(&caps).await;
                    }
                    Ok(Some(wail_net::MeshEvent::PeerLeft(pid))) => {
                        let name = peer_names.get(&pid).and_then(|n| n.as_deref()).unwrap_or(&pid);
                        ui_info!(&app, "Peer {name} left");
                        peer_names.remove(&pid);
                        let _ = app.emit("peer:left", PeerLeftEvent { peer_id: pid });
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        ui_warn!(&app, "Signaling connection closed");
                        break;
                    }
                    Err(e) => {
                        ui_error!(&app, "Signaling error: {e}");
                    }
                }
            }

            // --- Incoming sync messages from peers ---
            Some((from, msg)) = sync_rx.recv() => {
                match msg {
                    SyncMessage::Hello { peer_id: pid, display_name: name } => {
                        let name_display = name.as_deref().unwrap_or("(anonymous)");
                        ui_info!(&app, "Hello from {name_display} ({pid})");
                        peer_names.insert(pid.clone(), name.clone());
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
                        let name = peer_names.get(&from).and_then(|n| n.as_deref()).unwrap_or(&from);
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
                            ui_info!(&app, "Beat sync — snapped to beat {remote_beat:.2}");
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

                    SyncMessage::AudioCapabilities { sample_rates, channel_counts, can_send, can_receive } => {
                        ui_info!(&app, "Peer {from} audio: rates={sample_rates:?} ch={channel_counts:?} send={can_send} recv={can_receive}");
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
                }
            }

            // --- Incoming audio data from peers → forward to plugin ---
            Some((from, data)) = audio_rx.recv() => {
                audio_intervals_received += 1;
                let peer_name = peer_names.get(&from).and_then(|n| n.as_deref()).unwrap_or(&from);

                match AudioWire::decode(&data) {
                    Ok(audio_interval) => {
                        let mut rms_info = String::new();
                        if let Some(ref mut decoder) = audio_decoder {
                            match decoder.decode_interval(&audio_interval.opus_data) {
                                Ok(pcm) => {
                                    let rms = if pcm.is_empty() {
                                        0.0
                                    } else {
                                        (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32).sqrt()
                                    };
                                    rms_info = format!(", decoded={} samples, RMS={:.4}", pcm.len(), rms);
                                }
                                Err(e) => {
                                    rms_info = format!(", decode_err={e}");
                                }
                            }
                        }
                        ui_info!(
                            &app,
                            "[AUDIO RECV] peer={peer_name}, interval={}, wire={} bytes, opus={} bytes, sr={}, ch={}, bpm={:.1}{rms_info}",
                            audio_interval.index,
                            data.len(),
                            audio_interval.opus_data.len(),
                            audio_interval.sample_rate,
                            audio_interval.channels,
                            audio_interval.bpm,
                        );
                    }
                    Err(e) => {
                        ui_warn!(&app, "[AUDIO RECV] peer={peer_name}, wire={} bytes, decode_err={e}", data.len());
                    }
                }

                if let Some(ref mut writer) = ipc_writer {
                    let msg = IpcMessage::encode_audio(&from, &data);
                    let frame = IpcFramer::encode_frame(&msg);
                    if let Err(e) = writer.write_all(&frame).await {
                        ui_warn!(&app, "Plugin IPC write failed: {e}");
                        ipc_writer = None;
                    }
                }
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
                            if test_tone_enabled {
                                if let Some(ref mut encoder) = test_tone_encoder {
                                    send_test_tone(&app, &mesh, encoder, &mut rng, idx, last_broadcast_bpm, bars, quantum, &mut audio_intervals_sent).await;
                                }
                            }
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
                            if test_tone_enabled {
                                if let Some(ref mut encoder) = test_tone_encoder {
                                    send_test_tone(&app, &mesh, encoder, &mut rng, idx, last_broadcast_bpm, bars, quantum, &mut audio_intervals_sent).await;
                                }
                            }
                        }
                    }
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
                    let peers: Vec<PeerInfo> = connected
                        .iter()
                        .map(|p| PeerInfo {
                            peer_id: p.clone(),
                            display_name: peer_names.get(p).cloned().flatten(),
                            rtt_ms: clock.rtt_us(p).map(|rtt| rtt as f64 / 1000.0),
                        })
                        .collect();

                    let _ = app.emit("status:update", StatusUpdate {
                        bpm: state.bpm,
                        beat: state.beat,
                        phase: state.phase,
                        link_peers: state.num_peers,
                        peers,
                        interval_bars: interval.bars(),
                        audio_sent: audio_intervals_sent,
                        audio_recv: audio_intervals_received,
                        plugin_connected: ipc_writer.is_some(),
                        test_tone_enabled,
                    });
                }
            }
        }
    }

    Ok(())
}

/// Generate a random sine tone, Opus-encode it, and broadcast as an audio interval.
async fn send_test_tone(
    app: &AppHandle,
    mesh: &PeerMesh,
    encoder: &mut AudioEncoder,
    rng: &mut impl Rng,
    idx: i64,
    bpm: f64,
    bars: u32,
    quantum: f64,
    audio_intervals_sent: &mut u64,
) {
    let freq: f32 = rng.gen_range(220.0..=880.0);
    let sample_rate: u32 = 48000;
    let interval_beats = bars as f64 * quantum;
    let duration_secs = (interval_beats * 60.0) / bpm.max(1.0);
    let duration_secs = duration_secs.clamp(0.5, 30.0);
    let num_samples = (duration_secs * sample_rate as f64).round() as usize;

    let samples: Vec<f32> = (0..num_samples)
        .map(|i| {
            let t = i as f32 / sample_rate as f32;
            (t * freq * 2.0 * std::f32::consts::PI).sin() * 0.5
        })
        .collect();

    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt();

    ui_info!(
        app,
        "[TEST TONE] Interval {idx}: freq={freq:.1}Hz, {num_samples} samples ({duration_secs:.2}s), RMS={rms:.4}, bpm={bpm:.1}"
    );

    match encoder.encode_interval(&samples) {
        Ok(opus_data) => {
            let audio_interval = AudioInterval {
                index: idx,
                opus_data: opus_data.clone(),
                sample_rate,
                channels: 1,
                num_frames: num_samples as u32,
                bpm,
                quantum,
                bars,
            };
            let wire_data = AudioWire::encode(&audio_interval);

            ui_info!(
                app,
                "[TEST TONE] Encoded: opus={} bytes, wire={} bytes",
                opus_data.len(),
                wire_data.len()
            );

            mesh.broadcast_audio(&wire_data).await;
            *audio_intervals_sent += 1;

            let ready_msg = SyncMessage::AudioIntervalReady {
                interval_index: idx,
                wire_size: wire_data.len() as u32,
            };
            mesh.broadcast(&ready_msg).await;

            let peers = mesh.connected_peers();
            ui_info!(app, "[TEST TONE] Broadcast interval {idx} to peers: [{}]", peers.join(", "));
        }
        Err(e) => {
            ui_warn!(app, "[TEST TONE] Encode failed: {e}");
        }
    }
}
