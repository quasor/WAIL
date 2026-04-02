//! Shared test helpers for wail-net integration tests.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::future::IntoFuture;
use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use wail_audio::{AudioBridge, AudioEncoder, AudioFrame, AudioFrameWire, IpcFramer, IpcMessage};
use wail_net::PeerMesh;

// ---------------------------------------------------------------------------
// Configurable in-process WebSocket signaling server (mirrors Go server)
// ---------------------------------------------------------------------------

/// Server-side configuration injected at startup for signaling tests.
#[derive(Default, Clone)]
pub struct TestServerConfig {
    /// Minimum acceptable `client_version` (semver string). `None` = no check.
    pub min_version: Option<String>,
    /// Required room password. `None` = all rooms are public (no password required).
    pub password: Option<String>,
    /// Maximum stream slots per room. `None` = unlimited.
    pub room_capacity: Option<usize>,
}

#[derive(Default)]
struct SignalingState {
    /// room → peer_ids
    rooms: HashMap<String, Vec<String>>,
    /// "room:peer_id" → stream_count, for capacity accounting
    peer_slots: HashMap<String, usize>,
    /// room → peer_id → tx for sending messages to that peer's WebSocket
    peer_senders: HashMap<String, HashMap<String, tokio::sync::mpsc::UnboundedSender<WsRelay>>>,
    /// "room:peer_id" keys scheduled to receive `evicted` on next message
    evicted_peers: HashSet<String>,
    /// Per-room passwords (plaintext) set when the room was first created with a password.
    /// A room in this map is considered private and excluded from the public /rooms list.
    room_passwords: HashMap<String, String>,
    config: TestServerConfig,
}

type SharedState = Arc<Mutex<SignalingState>>;

/// Message type for the per-peer relay channel, avoiding unsafe String↔binary coercion.
#[derive(Clone)]
enum WsRelay {
    Text(String),
    Binary(Vec<u8>),
}

// ---------------------------------------------------------------------------
// Public test handle
// ---------------------------------------------------------------------------

/// A handle to the running test server that allows in-test control.
#[derive(Clone)]
pub struct TestServerHandle {
    /// Base URL of the signaling server (e.g. `"ws://127.0.0.1:PORT"`).
    pub url: String,
    state: SharedState,
}

impl TestServerHandle {
    /// Schedule a peer to receive eviction on its next interaction.
    pub async fn evict_peer(&self, room: &str, peer_id: &str) {
        let mut s = self.state.lock().await;
        s.evicted_peers.insert(format!("{room}:{peer_id}"));

        // Send eviction message immediately if peer has an active sender
        if let Some(room_senders) = s.peer_senders.get(room) {
            if let Some(tx) = room_senders.get(peer_id) {
                let _ = tx.send(WsRelay::Text(serde_json::json!({"type": "evicted"}).to_string()));
            }
        }
    }

    /// Return the IDs of all peers currently in a room.
    pub async fn peers_in_room(&self, room: &str) -> Vec<String> {
        self.state
            .lock()
            .await
            .rooms
            .get(room)
            .cloned()
            .unwrap_or_default()
    }

    /// Total stream slots used in a room.
    pub async fn slots_used(&self, room: &str) -> usize {
        let s = self.state.lock().await;
        s.rooms
            .get(room)
            .map(|peers| {
                peers
                    .iter()
                    .map(|p| {
                        s.peer_slots
                            .get(&format!("{room}:{p}"))
                            .copied()
                            .unwrap_or(1)
                    })
                    .sum()
            })
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Simple semver comparison
// ---------------------------------------------------------------------------

fn semver_less_than(a: &str, b: &str) -> bool {
    fn parse(s: &str) -> (u64, u64, u64) {
        let mut parts = s.split('.').filter_map(|p| p.parse::<u64>().ok());
        (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        )
    }
    parse(a) < parse(b)
}

// ---------------------------------------------------------------------------
// WebSocket handler
// ---------------------------------------------------------------------------

async fn handle_ws(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: SharedState) {
    // Phase 1: Wait for the join message and validate it.
    // We keep the socket unsplit so we can send errors directly and close cleanly.
    let join_result = handle_join_phase(&mut socket, &state).await;

    let (room, peer_id, _send_tx, send_rx) = match join_result {
        Some(v) => v,
        None => return, // join failed or connection closed
    };

    // Spawn writer from send_rx
    let (mut ws_tx, ws_rx) = socket.split();
    let write_handle = tokio::spawn(async move {
        let mut send_rx = send_rx;
        while let Some(msg) = send_rx.recv().await {
            let ws_msg = match msg {
                WsRelay::Text(text) => Message::Text(text),
                WsRelay::Binary(data) => Message::Binary(data),
            };
            if ws_tx.send(ws_msg).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    // Phase 2: relay messages
    relay_messages(ws_rx, &state, &room, &peer_id).await;

    // Clean up
    cleanup_peer(&state, &room, &peer_id).await;
    write_handle.abort();
}

/// Handle the join phase. Returns (room, peer_id, send_tx, send_rx) on success, None on failure.
async fn handle_join_phase(
    socket: &mut WebSocket,
    state: &SharedState,
) -> Option<(
    String,
    String,
    tokio::sync::mpsc::UnboundedSender<WsRelay>,
    tokio::sync::mpsc::UnboundedReceiver<WsRelay>,
)> {
    loop {
        let msg = match socket.recv().await {
            Some(Ok(Message::Text(t))) => t,
            Some(Ok(Message::Close(_))) | None => return None,
            _ => continue,
        };

        let parsed: serde_json::Value = match serde_json::from_str(&msg) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if parsed["type"].as_str() != Some("join") {
            continue;
        }

        let room = parsed["room"].as_str().unwrap_or("").to_string();
        let peer_id = parsed["peer_id"].as_str().unwrap_or("").to_string();
        let display_name = parsed["display_name"].as_str().map(|s| s.to_string());
        let stream_count = parsed["stream_count"].as_u64().unwrap_or(1) as usize;
        let client_version = parsed["client_version"]
            .as_str()
            .unwrap_or("0.0.0")
            .to_string();
        let password = parsed["password"].as_str().map(|s| s.to_string());

        let mut s = state.lock().await;

        // Version check
        if let Some(min) = &s.config.min_version.clone() {
            if semver_less_than(&client_version, min) {
                let _ = socket
                    .send(Message::Text(
                        serde_json::json!({
                            "type": "join_error",
                            "code": "version_outdated",
                            "min_version": min
                        })
                        .to_string(),
                    ))
                    .await;
                let _ = socket.close().await;
                return None;
            }
        }

        // Global password check (applies to all rooms when config.password is set)
        if let Some(required) = &s.config.password.clone() {
            let sent = password.as_deref().unwrap_or("");
            if sent != required {
                let _ = socket
                    .send(Message::Text(
                        serde_json::json!({
                            "type": "join_error",
                            "code": "unauthorized"
                        })
                        .to_string(),
                    ))
                    .await;
                let _ = socket.close().await;
                return None;
            }
        }

        // Per-room password check (mirrors Go server behaviour: room is private if
        // the first peer created it with a password; subsequent joiners must match it).
        if s.config.password.is_none() {
            let room_is_new = !s.rooms.contains_key(&room);
            if let Some(stored) = s.room_passwords.get(&room) {
                // Room exists and is private — enforce the password.
                let sent = password.as_deref().unwrap_or("");
                if sent != stored.as_str() {
                    let _ = socket
                        .send(Message::Text(
                            serde_json::json!({
                                "type": "join_error",
                                "code": "unauthorized"
                            })
                            .to_string(),
                        ))
                        .await;
                    let _ = socket.close().await;
                    return None;
                }
            } else if room_is_new {
                // Room is brand new — if the joining peer provides a password, mark it private.
                if let Some(pw) = &password {
                    if !pw.is_empty() {
                        s.room_passwords.insert(room.clone(), pw.clone());
                    }
                }
            }
        }

        // Capacity check
        if let Some(capacity) = s.config.room_capacity {
            let used: usize = s
                .rooms
                .get(&room)
                .map(|peers| {
                    peers
                        .iter()
                        .map(|p| {
                            s.peer_slots
                                .get(&format!("{room}:{p}"))
                                .copied()
                                .unwrap_or(1)
                        })
                        .sum()
                })
                .unwrap_or(0);
            if used + stream_count > capacity {
                let _ = socket
                    .send(Message::Text(
                        serde_json::json!({
                            "type": "join_error",
                            "code": "room_full",
                            "slots_available": capacity.saturating_sub(used)
                        })
                        .to_string(),
                    ))
                    .await;
                let _ = socket.close().await;
                return None;
            }
        }

        // Register peer
        let peers_in_room = s.rooms.entry(room.clone()).or_default();
        let existing: Vec<String> = peers_in_room
            .iter()
            .filter(|p| *p != &peer_id)
            .cloned()
            .collect();
        if !peers_in_room.contains(&peer_id) {
            peers_in_room.push(peer_id.clone());
        }
        s.peer_slots
            .insert(format!("{room}:{peer_id}"), stream_count);

        // Create channel for this peer
        let (send_tx, send_rx) = tokio::sync::mpsc::unbounded_channel::<WsRelay>();

        // Register sender
        s.peer_senders
            .entry(room.clone())
            .or_default()
            .insert(peer_id.clone(), send_tx.clone());

        // Notify existing peers
        if let Some(room_senders) = s.peer_senders.get(&room) {
            for (id, tx) in room_senders {
                if id != &peer_id {
                    let _ = tx.send(WsRelay::Text(
                        serde_json::json!({
                            "type": "peer_joined",
                            "peer_id": peer_id,
                            "display_name": display_name,
                        })
                        .to_string(),
                    ));
                }
            }
        }

        // Build peer display names
        let peer_display_names: HashMap<String, Option<String>> = existing
            .iter()
            .map(|id| (id.clone(), None))
            .collect();

        // Send join_ok
        let _ = socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "join_ok",
                    "peers": existing,
                    "peer_display_names": peer_display_names
                })
                .to_string(),
            ))
            .await;

        return Some((room, peer_id, send_tx, send_rx));
    }
}

async fn relay_messages(
    mut ws_rx: futures_util::stream::SplitStream<WebSocket>,
    state: &SharedState,
    room: &str,
    peer_id: &str,
) {
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Binary(data) => {
                // Binary audio frame: prepend sender header and broadcast to room peers
                let pid_bytes = peer_id.as_bytes();
                let mut frame = Vec::with_capacity(1 + pid_bytes.len() + data.len());
                frame.push(pid_bytes.len() as u8);
                frame.extend_from_slice(pid_bytes);
                frame.extend_from_slice(&data);

                let relay = WsRelay::Binary(frame);
                let s = state.lock().await;
                if let Some(room_senders) = s.peer_senders.get(room) {
                    for (id, tx) in room_senders {
                        if id != peer_id {
                            let _ = tx.send(relay.clone());
                        }
                    }
                }
            }
            Message::Text(text) => {
                let parsed: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let msg_type = parsed["type"].as_str().unwrap_or("");

                match msg_type {
                    "signal" => {
                        let to = parsed["to"].as_str().unwrap_or("").to_string();
                        let s = state.lock().await;
                        if let Some(room_senders) = s.peer_senders.get(room) {
                            if let Some(tx) = room_senders.get(&to) {
                                let _ = tx.send(WsRelay::Text(text.clone()));
                            }
                        }
                    }
                    "sync" => {
                        // Broadcast sync to all room peers except sender
                        let relay = WsRelay::Text(serde_json::json!({
                            "type": "sync",
                            "from": peer_id,
                            "payload": parsed["payload"],
                        }).to_string());
                        let s = state.lock().await;
                        if let Some(room_senders) = s.peer_senders.get(room) {
                            for (id, tx) in room_senders {
                                if id != peer_id {
                                    let _ = tx.send(relay.clone());
                                }
                            }
                        }
                    }
                    "sync_to" => {
                        // Targeted sync to a specific peer
                        let to = parsed["to"].as_str().unwrap_or("").to_string();
                        let relay = WsRelay::Text(serde_json::json!({
                            "type": "sync",
                            "from": peer_id,
                            "payload": parsed["payload"],
                        }).to_string());
                        let s = state.lock().await;
                        if let Some(room_senders) = s.peer_senders.get(room) {
                            if let Some(tx) = room_senders.get(&to) {
                                let _ = tx.send(relay);
                            }
                        }
                    }
                    "leave" => break,
                    _ => {}
                }
            }
            Message::Close(_) => break,
            _ => continue,
        }
    }
}

async fn cleanup_peer(state: &SharedState, room: &str, peer_id: &str) {
    let mut s = state.lock().await;

    if let Some(peers) = s.rooms.get_mut(room) {
        peers.retain(|p| p != peer_id);
        if peers.is_empty() {
            s.rooms.remove(room);
            s.room_passwords.remove(room);
        }
    }
    s.peer_slots.remove(&format!("{room}:{peer_id}"));

    // Notify remaining peers
    if let Some(room_senders) = s.peer_senders.get(room) {
        for (id, tx) in room_senders {
            if id != peer_id {
                let _ = tx.send(WsRelay::Text(
                    serde_json::json!({
                        "type": "peer_left",
                        "peer_id": peer_id,
                    })
                    .to_string(),
                ));
            }
        }
    }

    // Remove sender
    if let Some(room_senders) = s.peer_senders.get_mut(room) {
        room_senders.remove(peer_id);
        if room_senders.is_empty() {
            s.peer_senders.remove(room);
        }
    }
}

// ---------------------------------------------------------------------------
// Server startup
// ---------------------------------------------------------------------------

async fn handle_rooms(State(state): State<SharedState>) -> impl IntoResponse {
    let s = state.lock().await;
    let rooms: Vec<serde_json::Value> = s
        .rooms
        .iter()
        .filter(|(name, _)| !s.room_passwords.contains_key(*name))
        .map(|(name, peers)| {
            serde_json::json!({
                "room": name,
                "peer_count": peers.len(),
                "display_names": [],
                "created_at": 0_i64,
            })
        })
        .collect();
    Json(serde_json::json!({ "rooms": rooms }))
}

fn build_app(state: SharedState) -> Router {
    Router::new()
        .route("/ws", get(handle_ws))
        .route("/rooms", get(handle_rooms))
        .with_state(state)
}

/// Start a plain test signaling server. Returns the base URL.
pub async fn start_test_signaling_server() -> String {
    start_configured_signaling_server(TestServerConfig::default())
        .await
        .url
}

/// Start a configurable test signaling server. Returns a handle with admin methods.
pub async fn start_configured_signaling_server(config: TestServerConfig) -> TestServerHandle {
    let state: SharedState = Arc::new(Mutex::new(SignalingState {
        config,
        ..Default::default()
    }));

    let app = build_app(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app).into_future());

    TestServerHandle {
        url: format!("ws://{}", addr),
        state,
    }
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Generate a recognizable test signal: sine wave at a given frequency.
pub fn sine_wave(freq_hz: f32, duration_samples: usize, channels: u16, sample_rate: u32) -> Vec<f32> {
    let mut out = Vec::with_capacity(duration_samples * channels as usize);
    for i in 0..duration_samples {
        let t = i as f32 / sample_rate as f32;
        let sample = (t * freq_hz * 2.0 * std::f32::consts::PI).sin() * 0.5;
        for _ in 0..channels {
            out.push(sample);
        }
    }
    out
}

/// Compute RMS energy of a signal.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

/// Produce encoded WAIF audio frames from an AudioBridge (one interval).
///
/// Returns a list of WAIF-encoded frames (one per 20ms Opus packet).
/// Callers that need a single blob for `broadcast_audio` should send each frame
/// individually, or use `produce_interval_single_frame` for a final-only frame.
pub fn produce_interval(freq_hz: f32) -> Vec<Vec<u8>> {
    let sr = 48000u32;
    let ch = 2u16;
    let buf_size = 4096;
    let mut bridge = AudioBridge::new(sr, ch, 4, 4.0, 128);
    let signal = sine_wave(freq_hz, buf_size / ch as usize, ch, sr);
    let mut out = vec![0.0f32; buf_size];

    for beat in [0.0, 4.0, 8.0, 12.0] {
        bridge.process_rt(&signal, &mut out, beat);
    }
    let completed = bridge.process_rt(&signal, &mut out, 16.0);
    assert_eq!(completed.len(), 1, "Should produce exactly 1 interval");

    let interval = completed.into_iter().next().unwrap();
    encode_completed_to_waif(interval.index, &interval.samples, sr, ch)
}

/// Encode raw PCM samples into WAIF wire frames (Opus-encoded, 20ms chunks).
fn encode_completed_to_waif(index: i64, samples: &[f32], sample_rate: u32, channels: u16) -> Vec<Vec<u8>> {
    let mut encoder = AudioEncoder::new(sample_rate, channels, 128).expect("create encoder");
    let frame_size = 960 * channels as usize; // 20ms at 48kHz
    let chunks: Vec<&[f32]> = samples.chunks(frame_size).collect();
    let total_frames = chunks.len() as u32;
    let mut waif_frames = Vec::with_capacity(chunks.len());

    for (i, chunk) in chunks.iter().enumerate() {
        // Pad last chunk if needed
        let mut padded;
        let input = if chunk.len() < frame_size {
            padded = vec![0.0f32; frame_size];
            padded[..chunk.len()].copy_from_slice(chunk);
            &padded
        } else {
            *chunk
        };

        let opus_data = encoder.encode_frame(input).expect("encode frame");
        let is_final = i as u32 == total_frames - 1;

        let frame = AudioFrame {
            interval_index: index,
            stream_id: 0,
            frame_number: i as u32,
            channels,
            opus_data,
            is_final,
            sample_rate: if is_final { sample_rate } else { 0 },
            total_frames: if is_final { total_frames } else { 0 },
            bpm: if is_final { 120.0 } else { 0.0 },
            quantum: if is_final { 4.0 } else { 0.0 },
            bars: if is_final { 4 } else { 0 },
        };
        waif_frames.push(AudioFrameWire::encode(&frame));
    }
    waif_frames
}

/// Produce a single WAIF frame containing real audio (for simple send/receive tests).
///
/// This is a convenience wrapper that produces a single final WAIF frame with
/// one 20ms Opus packet. Useful for tests that just need to verify data flows
/// through WebRTC without sending hundreds of frames.
pub fn produce_single_waif_frame(freq_hz: f32) -> Vec<u8> {
    let sr = 48000u32;
    let ch = 2u16;
    let mut encoder = AudioEncoder::new(sr, ch, 128).expect("create encoder");
    let samples_per_frame = 960usize;
    let mut samples = vec![0.0f32; samples_per_frame * ch as usize];
    for i in 0..samples_per_frame {
        let val = (2.0 * std::f32::consts::PI * freq_hz * i as f32 / sr as f32).sin() * 0.5;
        samples[i * 2] = val;
        samples[i * 2 + 1] = val;
    }
    let opus_data = encoder.encode_frame(&samples).expect("encode frame");
    let frame = AudioFrame {
        interval_index: 0,
        stream_id: 0,
        frame_number: 0,
        channels: ch,
        opus_data,
        is_final: true,
        sample_rate: sr,
        total_frames: 1,
        bpm: 120.0,
        quantum: 4.0,
        bars: 4,
    };
    AudioFrameWire::encode(&frame)
}

/// Encode an interval as WAIF IPC frames (tag 0x05), matching the real send plugin output.
///
/// Returns a list of complete IPC frames ready to write to a TCP stream.
/// This mirrors how `wail-plugin-send` streams 20ms Opus packets to wail-app.
pub fn produce_interval_waif_ipc(freq_hz: f32) -> Vec<Vec<u8>> {
    let waif_frames = produce_interval(freq_hz);

    let mut output = Vec::with_capacity(waif_frames.len());
    for wire_bytes in waif_frames {
        let ipc_msg = IpcMessage::encode_audio_frame(&wire_bytes);
        output.push(IpcFramer::encode_frame(&ipc_msg));
    }
    output
}

/// Pump signaling for both meshes until they see each other.
/// With WebSocket relay, connection is immediate once both peers join.
pub async fn establish_connection(mesh_a: &mut PeerMesh, mesh_b: &mut PeerMesh) {
    establish_connection_timeout(mesh_a, mesh_b, 15).await;
}

pub async fn establish_connection_timeout(
    mesh_a: &mut PeerMesh,
    mesh_b: &mut PeerMesh,
    timeout_secs: u64,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);

    loop {
        tokio::select! {
            result = mesh_a.poll_signaling() => {
                if let Err(e) = result {
                    eprintln!("[test] mesh_a poll error: {e}");
                }
            }
            result = mesh_b.poll_signaling() => {
                if let Err(e) = result {
                    eprintln!("[test] mesh_b poll error: {e}");
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if !mesh_a.connected_peers().is_empty() && !mesh_b.connected_peers().is_empty() {
                    return;
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                panic!(
                    "Connection timed out. Peers: A={:?}, B={:?}",
                    mesh_a.connected_peers(),
                    mesh_b.connected_peers(),
                );
            }
        }
    }
}

/// Try to process one signaling message within a short timeout.
/// Returns true if a message was processed, false if nothing was pending.
async fn poll_one(mesh: &mut PeerMesh) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_millis(5), mesh.poll_signaling()).await,
        Ok(Ok(_))
    )
}

/// Pump signaling for three meshes until all see each other.
pub async fn establish_three_way_connection(
    mesh_a: &mut PeerMesh,
    mesh_b: &mut PeerMesh,
    mesh_c: &mut PeerMesh,
    ids: (&str, &str, &str),
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);

    loop {
        let mut any_progress = true;
        while any_progress {
            any_progress = false;
            any_progress |= poll_one(mesh_a).await;
            any_progress |= poll_one(mesh_b).await;
            any_progress |= poll_one(mesh_c).await;
        }

        let all_connected =
            mesh_a.is_peer_audio_dc_open(ids.1) && mesh_a.is_peer_audio_dc_open(ids.2)
            && mesh_b.is_peer_audio_dc_open(ids.0) && mesh_b.is_peer_audio_dc_open(ids.2)
            && mesh_c.is_peer_audio_dc_open(ids.0) && mesh_c.is_peer_audio_dc_open(ids.1);
        if all_connected {
            return;
        }

        if tokio::time::Instant::now() >= deadline {
            panic!(
                "3-way connection timed out. \
                 A→B={} A→C={} B→A={} B→C={} C→A={} C→B={}",
                mesh_a.is_peer_audio_dc_open(ids.1),
                mesh_a.is_peer_audio_dc_open(ids.2),
                mesh_b.is_peer_audio_dc_open(ids.0),
                mesh_b.is_peer_audio_dc_open(ids.2),
                mesh_c.is_peer_audio_dc_open(ids.0),
                mesh_c.is_peer_audio_dc_open(ids.1),
            );
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Produce realistically-sized encoded WAIF frames from an AudioBridge.
///
/// Returns (list of WAIF frames, expected sample count).
pub fn produce_full_interval(freq_hz: f32) -> (Vec<Vec<u8>>, usize) {
    let sr = 48000u32;
    let ch = 2u16;
    let bpm = 120.0_f64;
    let buf_frames: usize = 256;
    let buf_size = buf_frames * ch as usize;

    let mut bridge = AudioBridge::new(sr, ch, 4, 4.0, 128);

    let signal = sine_wave(freq_hz, buf_frames, ch, sr);
    let mut out = vec![0.0f32; buf_size];

    let beats_per_callback = buf_frames as f64 / sr as f64 * bpm / 60.0;
    let mut beat = 0.0_f64;

    while beat < 16.0 {
        bridge.process_rt(&signal, &mut out, beat);
        beat += beats_per_callback;
    }

    let completed = bridge.process_rt(&signal, &mut out, beat);
    assert_eq!(completed.len(), 1, "Should produce exactly 1 interval");

    let interval = completed.into_iter().next().unwrap();
    let expected_samples = (sr as f64 * ch as f64 * 16.0 / (bpm / 60.0)) as usize;
    let waif_frames = encode_completed_to_waif(interval.index, &interval.samples, sr, ch);
    (waif_frames, expected_samples)
}

/// Find a random available port by binding to :0.
pub fn random_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

// ---------------------------------------------------------------------------
// semver_less_than unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_comparison_basic() {
        assert!(semver_less_than("1.0.0", "2.0.0"));
        assert!(semver_less_than("1.2.3", "1.10.0"));
        assert!(!semver_less_than("2.0.0", "1.9.9"));
        assert!(!semver_less_than("1.2.3", "1.2.3"));
    }
}
