//! Network integration tests: two WAIL peers exchanging audio over real WebRTC.
//!
//! These tests exercise the full path:
//!   HTTP signaling → WebRTC negotiation → DataChannel establishment → audio exchange
//!
//! No external services needed: in-process HTTP signaling server, localhost ICE candidates.

use std::collections::HashMap;
use std::future::IntoFuture;
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

    // Collect existing peers before adding new one
    let existing: Vec<String> = peers_in_room
        .iter()
        .filter(|p| *p != &peer_id)
        .cloned()
        .collect();

    // Add peer if not already present
    if !peers_in_room.contains(&peer_id) {
        peers_in_room.push(peer_id.clone());
    }

    // Enqueue PeerJoined for existing peers
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
    // Find room for sender
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

/// Route dispatcher: ?action=join|signal|poll
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

async fn start_test_signaling_server() -> String {
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
fn sine_wave(freq_hz: f32, duration_samples: usize, channels: u16, sample_rate: u32) -> Vec<f32> {
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
fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

/// Produce an encoded audio interval from an AudioBridge.
/// Records a sine wave through one full interval, crosses the boundary, returns wire bytes.
fn produce_interval(freq_hz: f32) -> Vec<u8> {
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
async fn establish_connection(mesh_a: &mut PeerMesh, mesh_b: &mut PeerMesh) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let min_settle = tokio::time::Instant::now() + Duration::from_secs(2);

    loop {
        tokio::select! {
            _ = mesh_a.poll_signaling() => {}
            _ = mesh_b.poll_signaling() => {}
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                let both_connected = !mesh_a.connected_peers().is_empty()
                    && !mesh_b.connected_peers().is_empty();
                if both_connected && tokio::time::Instant::now() > min_settle {
                    // Extra settle time for SCTP/DataChannels to open
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

// ---------------------------------------------------------------
// Test: Two peers exchange audio intervals over real WebRTC
// ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn two_peers_exchange_audio_over_webrtc() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // 1. Start in-process HTTP signaling server on a random port
    let server_url = start_test_signaling_server().await;

    // 2. Connect both peers to the signaling server
    //    "peer-a" < "peer-b" → peer-a will be the WebRTC initiator
    let (mut mesh_a, _sync_rx_a, mut audio_rx_a) =
        PeerMesh::connect(&server_url, "test-room", "peer-a", "test")
            .await
            .expect("Peer A failed to connect to signaling");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) =
        PeerMesh::connect(&server_url, "test-room", "peer-b", "test")
            .await
            .expect("Peer B failed to connect to signaling");

    // 3. Pump signaling until WebRTC DataChannels are established
    establish_connection(&mut mesh_a, &mut mesh_b).await;

    // 4. Peer A → Peer B: send audio interval over WebRTC
    let wire_a = produce_interval(440.0);
    mesh_a.broadcast_audio(&wire_a).await;

    let (from, received) = tokio::time::timeout(Duration::from_secs(5), audio_rx_b.recv())
        .await
        .expect("Timed out waiting for audio from A")
        .expect("Audio channel B closed");

    assert_eq!(from, "peer-a");
    assert!(!received.is_empty(), "Wire data should be non-empty");

    // Decode and verify it's real audio
    let sr = 48000u32;
    let ch = 2u16;
    let buf_size = 4096;
    let mut bridge_b = AudioBridge::new(sr, ch, 4, 4.0, 128);
    let silence = vec![0.0f32; buf_size];
    let mut out = vec![0.0f32; buf_size];

    bridge_b.process(&silence, &mut out, 0.0); // start interval 0
    bridge_b.receive_wire(&from, &received);
    bridge_b.process(&silence, &mut out, 16.0); // cross boundary — play remote

    let energy = rms(&out);
    assert!(
        energy > 0.01,
        "Peer B should hear Peer A's audio over WebRTC, RMS={energy}"
    );

    // 5. Peer B → Peer A: send audio interval (bidirectional test)
    let wire_b = produce_interval(880.0);
    mesh_b.broadcast_audio(&wire_b).await;

    let (from_b, received_b) = tokio::time::timeout(Duration::from_secs(5), audio_rx_a.recv())
        .await
        .expect("Timed out waiting for audio from B")
        .expect("Audio channel A closed");

    assert_eq!(from_b, "peer-b");
    assert!(!received_b.is_empty(), "Wire data should be non-empty");

    // Decode and verify
    let mut bridge_a = AudioBridge::new(sr, ch, 4, 4.0, 128);
    bridge_a.process(&silence, &mut out, 0.0);
    bridge_a.receive_wire(&from_b, &received_b);
    bridge_a.process(&silence, &mut out, 16.0);

    let energy_b = rms(&out);
    assert!(
        energy_b > 0.01,
        "Peer A should hear Peer B's audio over WebRTC, RMS={energy_b}"
    );
}
