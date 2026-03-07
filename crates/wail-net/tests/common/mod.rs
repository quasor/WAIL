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
use wail_audio::{AudioBridge, AudioFrame, AudioFrameWire, AudioWire, IpcFramer, IpcMessage};
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
    peer_senders: HashMap<String, HashMap<String, tokio::sync::mpsc::UnboundedSender<String>>>,
    /// "room:peer_id" keys scheduled to receive `evicted` on next message
    evicted_peers: HashSet<String>,
    config: TestServerConfig,
}

type SharedState = Arc<Mutex<SignalingState>>;

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
                let _ = tx.send(serde_json::json!({"type": "evicted"}).to_string());
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

    let (room, peer_id, send_tx, send_rx) = match join_result {
        Some(v) => v,
        None => return, // join failed or connection closed
    };

    // Spawn writer from send_rx
    let (mut ws_tx, ws_rx) = socket.split();
    let write_handle = tokio::spawn(async move {
        let mut send_rx = send_rx;
        while let Some(msg) = send_rx.recv().await {
            if ws_tx.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    // Phase 2: relay messages
    relay_messages(ws_rx, &state, &room, &peer_id, &send_tx).await;

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
    tokio::sync::mpsc::UnboundedSender<String>,
    tokio::sync::mpsc::UnboundedReceiver<String>,
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

        // Password check
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
        let (send_tx, send_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        // Register sender
        s.peer_senders
            .entry(room.clone())
            .or_default()
            .insert(peer_id.clone(), send_tx.clone());

        // Notify existing peers
        if let Some(room_senders) = s.peer_senders.get(&room) {
            for (id, tx) in room_senders {
                if id != &peer_id {
                    let _ = tx.send(
                        serde_json::json!({
                            "type": "peer_joined",
                            "peer_id": peer_id,
                            "display_name": display_name,
                        })
                        .to_string(),
                    );
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
    send_tx: &tokio::sync::mpsc::UnboundedSender<String>,
) {
    while let Some(Ok(msg)) = ws_rx.next().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };

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
                        let _ = tx.send(text.clone());
                    }
                }
            }
            "leave" => break,
            _ => {}
        }
    }
}

async fn cleanup_peer(state: &SharedState, room: &str, peer_id: &str) {
    let mut s = state.lock().await;

    if let Some(peers) = s.rooms.get_mut(room) {
        peers.retain(|p| p != peer_id);
        if peers.is_empty() {
            s.rooms.remove(room);
        }
    }
    s.peer_slots.remove(&format!("{room}:{peer_id}"));

    // Notify remaining peers
    if let Some(room_senders) = s.peer_senders.get(room) {
        for (id, tx) in room_senders {
            if id != peer_id {
                let _ = tx.send(
                    serde_json::json!({
                        "type": "peer_left",
                        "peer_id": peer_id,
                    })
                    .to_string(),
                );
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

fn build_app(state: SharedState) -> Router {
    Router::new()
        .route("/ws", get(handle_ws))
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

/// Produce an encoded audio interval from an AudioBridge.
pub fn produce_interval(freq_hz: f32) -> Vec<u8> {
    let sr = 48000u32;
    let ch = 2u16;
    let buf_size = 4096;
    let mut bridge = AudioBridge::new(sr, ch, 4, 4.0, 128);
    let signal = sine_wave(freq_hz, buf_size / ch as usize, ch, sr);
    let mut out = vec![0.0f32; buf_size];

    for beat in [0.0, 4.0, 8.0, 12.0] {
        bridge.process(&signal, &mut out, beat);
    }
    let wire_msgs = bridge.process(&signal, &mut out, 16.0);
    assert_eq!(wire_msgs.len(), 1, "Should produce exactly 1 interval");
    wire_msgs.into_iter().next().unwrap()
}

/// Encode an interval as WAIF IPC frames (tag 0x05), matching the real send plugin output.
///
/// Returns a list of complete IPC frames ready to write to a TCP stream.
/// This mirrors how `wail-plugin-send` streams 20ms Opus packets to wail-app.
pub fn produce_interval_waif_ipc(freq_hz: f32) -> Vec<Vec<u8>> {
    let wail_bytes = produce_interval(freq_hz);
    let interval = AudioWire::decode(&wail_bytes).expect("produce_interval_waif_ipc: AudioWire decode");

    // Parse the length-prefixed opus blob: [u32 count][u16 len][bytes]...
    let data = &interval.opus_data;
    let frame_count = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
    let mut packets: Vec<Vec<u8>> = Vec::with_capacity(frame_count);
    let mut offset = 4;
    while packets.len() < frame_count && offset + 2 <= data.len() {
        let pkt_len = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;
        packets.push(data[offset..offset + pkt_len].to_vec());
        offset += pkt_len;
    }

    let total = packets.len();
    let mut output = Vec::with_capacity(total);
    for (fn_, packet) in packets.into_iter().enumerate() {
        let is_final = fn_ + 1 == total;
        let frame = AudioFrame {
            interval_index: interval.index,
            stream_id: interval.stream_id,
            frame_number: fn_ as u32,
            channels: interval.channels,
            opus_data: packet,
            is_final,
            sample_rate: if is_final { interval.sample_rate } else { 0 },
            total_frames: if is_final { total as u32 } else { 0 },
            bpm: if is_final { interval.bpm } else { 0.0 },
            quantum: if is_final { interval.quantum } else { 0.0 },
            bars: if is_final { interval.bars } else { 0 },
        };
        let wire_bytes = AudioFrameWire::encode(&frame);
        let ipc_msg = IpcMessage::encode_audio_frame(&wire_bytes);
        output.push(IpcFramer::encode_frame(&ipc_msg));
    }
    output
}

/// Pump signaling for both meshes until they see each other, then wait for DataChannels.
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
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                let both_open = mesh_a.any_audio_dc_open() && mesh_b.any_audio_dc_open();
                if both_open {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    return;
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                panic!(
                    "WebRTC connection timed out. Peers: A={:?}, B={:?}, DCs: A={}, B={}",
                    mesh_a.connected_peers(),
                    mesh_b.connected_peers(),
                    mesh_a.any_audio_dc_open(),
                    mesh_b.any_audio_dc_open(),
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

/// Pump signaling for three meshes until all six directed DataChannel paths are open.
///
/// Uses explicit round-robin polling to ensure all meshes get fair scheduling.
/// With WebSocket signaling, messages arrive instantly and tokio::select! can
/// starve lower-priority meshes when one mesh has a flood of ICE candidates.
pub async fn establish_three_way_connection(
    mesh_a: &mut PeerMesh,
    mesh_b: &mut PeerMesh,
    mesh_c: &mut PeerMesh,
    ids: (&str, &str, &str),
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);

    loop {
        // Process one message from each mesh in strict round-robin.
        // Repeat while any mesh had work, to drain queued messages quickly.
        let mut any_progress = true;
        while any_progress {
            any_progress = false;
            any_progress |= poll_one(mesh_a).await;
            any_progress |= poll_one(mesh_b).await;
            any_progress |= poll_one(mesh_c).await;
        }

        let all_open =
            mesh_a.is_peer_audio_dc_open(ids.1) && mesh_a.is_peer_audio_dc_open(ids.2)
            && mesh_b.is_peer_audio_dc_open(ids.0) && mesh_b.is_peer_audio_dc_open(ids.2)
            && mesh_c.is_peer_audio_dc_open(ids.0) && mesh_c.is_peer_audio_dc_open(ids.1);
        if all_open {
            tokio::time::sleep(Duration::from_millis(500)).await;
            return;
        }

        if tokio::time::Instant::now() >= deadline {
            panic!(
                "3-way WebRTC connection timed out. \
                 A→B={} A→C={} B→A={} B→C={} C→A={} C→B={}",
                mesh_a.is_peer_audio_dc_open(ids.1),
                mesh_a.is_peer_audio_dc_open(ids.2),
                mesh_b.is_peer_audio_dc_open(ids.0),
                mesh_b.is_peer_audio_dc_open(ids.2),
                mesh_c.is_peer_audio_dc_open(ids.0),
                mesh_c.is_peer_audio_dc_open(ids.1),
            );
        }

        // Yield to let WebSocket I/O and ICE background tasks make progress
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Produce a realistically-sized encoded audio interval from an AudioBridge.
pub fn produce_full_interval(freq_hz: f32) -> (Vec<u8>, usize) {
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
        bridge.process(&signal, &mut out, beat);
        beat += beats_per_callback;
    }

    let wire_msgs = bridge.process(&signal, &mut out, beat);
    assert_eq!(wire_msgs.len(), 1, "Should produce exactly 1 interval");

    let expected_samples = (sr as f64 * ch as f64 * 16.0 / (bpm / 60.0)) as usize;
    (wire_msgs.into_iter().next().unwrap(), expected_samples)
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
