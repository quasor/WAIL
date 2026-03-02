//! Shared test helpers for wail-net integration tests.

#![allow(dead_code)]

use std::collections::HashMap;
use std::future::IntoFuture;
use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use wail_audio::AudioBridge;
use wail_net::PeerMesh;

// ---------------------------------------------------------------------------
// Minimal in-process HTTP signaling server (mirrors Val Town endpoint)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SignalingState {
    /// room -> set of peer_ids
    rooms: HashMap<String, Vec<String>>,
    /// (room, to_peer) -> queued messages with seq ids
    messages: Vec<StoredMessage>,
    next_seq: i64,
}

struct StoredMessage {
    seq: i64,
    room: String,
    to_peer: String,
    body: serde_json::Value,
}

type SharedState = Arc<Mutex<SignalingState>>;

async fn handle_join(
    State(state): State<SharedState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let room = body["room"].as_str().unwrap().to_string();
    let peer_id = body["peer_id"].as_str().unwrap().to_string();

    let mut s = state.lock().await;
    let peers_in_room = s.rooms.entry(room.clone()).or_default();

    let existing: Vec<String> = peers_in_room
        .iter()
        .filter(|p| *p != &peer_id)
        .cloned()
        .collect();

    if !peers_in_room.contains(&peer_id) {
        peers_in_room.push(peer_id.clone());
    }

    for p in &existing {
        s.next_seq += 1;
        let seq = s.next_seq;
        s.messages.push(StoredMessage {
            seq,
            room: room.clone(),
            to_peer: p.clone(),
            body: serde_json::json!({ "type": "PeerJoined", "peer_id": peer_id }),
        });
    }

    Json(serde_json::json!({ "peers": existing }))
}

async fn handle_signal(
    State(state): State<SharedState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let to = body["to"].as_str().unwrap().to_string();
    let from = body["from"].as_str().unwrap().to_string();

    let mut s = state.lock().await;
    let room = s
        .rooms
        .iter()
        .find(|(_, peers)| peers.contains(&from))
        .map(|(r, _)| r.clone())
        .unwrap_or_default();

    s.next_seq += 1;
    let seq = s.next_seq;
    s.messages.push(StoredMessage {
        seq,
        room,
        to_peer: to,
        body,
    });

    Json(serde_json::json!({ "ok": true }))
}

#[derive(serde::Deserialize)]
struct PollQuery {
    room: String,
    peer_id: String,
    after: Option<i64>,
}

async fn handle_poll(
    State(state): State<SharedState>,
    Query(q): Query<PollQuery>,
) -> Json<serde_json::Value> {
    let after = q.after.unwrap_or(0);
    let s = state.lock().await;

    let messages: Vec<serde_json::Value> = s
        .messages
        .iter()
        .filter(|m| m.room == q.room && m.to_peer == q.peer_id && m.seq > after)
        .map(|m| serde_json::json!({ "seq": m.seq, "body": m.body }))
        .collect();

    Json(serde_json::json!({ "messages": messages }))
}

async fn handle_post(
    Query(params): Query<HashMap<String, String>>,
    state: State<SharedState>,
    body: Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    match params.get("action").map(|s| s.as_str()) {
        Some("join") => handle_join(state, body).await,
        Some("signal") => handle_signal(state, body).await,
        Some("leave") => Json(serde_json::json!({ "ok": true })),
        _ => Json(serde_json::json!({ "error": "unknown action" })),
    }
}

async fn handle_get(
    Query(params): Query<HashMap<String, String>>,
    state: State<SharedState>,
) -> Json<serde_json::Value> {
    if params.get("action").map(|s| s.as_str()) == Some("poll") {
        let q = PollQuery {
            room: params.get("room").cloned().unwrap_or_default(),
            peer_id: params.get("peer_id").cloned().unwrap_or_default(),
            after: params.get("after").and_then(|s| s.parse().ok()),
        };
        handle_poll(state, Query(q)).await
    } else {
        Json(serde_json::json!({ "error": "unknown action" }))
    }
}

pub async fn start_test_signaling_server() -> String {
    let state: SharedState = Arc::new(Mutex::new(SignalingState::default()));

    let app = Router::new()
        .route("/", post(handle_post))
        .route("/", get(handle_get))
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app).into_future());

    format!("http://{}", addr)
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
/// Records a sine wave through one full interval, crosses the boundary, returns wire bytes.
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

/// Pump signaling for both meshes until they see each other, then wait for DataChannels.
pub async fn establish_connection(mesh_a: &mut PeerMesh, mesh_b: &mut PeerMesh) {
    establish_connection_timeout(mesh_a, mesh_b, 15).await;
}

pub async fn establish_connection_timeout(mesh_a: &mut PeerMesh, mesh_b: &mut PeerMesh, timeout_secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    let min_settle = tokio::time::Instant::now() + Duration::from_secs(2);

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
                let both_connected = !mesh_a.connected_peers().is_empty()
                    && !mesh_b.connected_peers().is_empty();
                if both_connected && tokio::time::Instant::now() > min_settle {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    return;
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                panic!(
                    "WebRTC connection timed out. Peers: A={:?}, B={:?}",
                    mesh_a.connected_peers(),
                    mesh_b.connected_peers()
                );
            }
        }
    }
}

/// Produce a realistically-sized encoded audio interval from an AudioBridge.
///
/// Unlike `produce_interval()` which only records a handful of buffers,
/// this simulates a real DAW callback loop: 256-frame buffers at 120 BPM,
/// advancing beat position proportionally, filling the full 8-second interval.
///
/// Returns `(wire_bytes, expected_interleaved_samples)`.
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

    // Fill interval 0 (beats 0..16)
    while beat < 16.0 {
        bridge.process(&signal, &mut out, beat);
        beat += beats_per_callback;
    }

    // Cross boundary — this triggers encode and returns wire bytes
    let wire_msgs = bridge.process(&signal, &mut out, beat);
    assert_eq!(wire_msgs.len(), 1, "Should produce exactly 1 interval");

    // Expected interleaved sample count for a full interval
    let expected_samples = (sr as f64 * ch as f64 * 16.0 / (bpm / 60.0)) as usize;
    (wire_msgs.into_iter().next().unwrap(), expected_samples)
}

/// Find a random available port by binding to :0.
pub fn random_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}
