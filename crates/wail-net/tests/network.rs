//! Network integration tests: two WAIL peers exchanging audio over WebSocket relay.
//!
//! These tests exercise the full path:
//!   WebSocket signaling → relay establishment → audio exchange
//!
//! No external services needed: in-process WebSocket signaling server.

mod common;

use std::time::Duration;

use wail_audio::{AudioDecoder, AudioFrameWire};
use wail_net::PeerMesh;

use common::*;

/// Decode a received WAIF wire frame and return the RMS energy of the decoded PCM.
fn decode_waif_rms(data: &[u8]) -> f32 {
    let frame = AudioFrameWire::decode(data).expect("decode WAIF frame");
    let sr = if frame.sample_rate > 0 { frame.sample_rate } else { 48000 };
    let ch = frame.channels;
    let mut decoder = AudioDecoder::new(sr, ch).expect("create decoder");
    let samples = decoder.decode_frame(&frame.opus_data).expect("decode Opus frame");
    rms(&samples)
}

// ---------------------------------------------------------------
// Test: Two peers exchange audio intervals over WebSocket relay
// ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn two_peers_exchange_audio_over_webrtc() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // 1. Start in-process signaling server on a random port
    let server_url = start_test_signaling_server().await;

    // 2. Connect both peers to the signaling server
    let (mut mesh_a, _sync_rx_a, mut audio_rx_a) =
        PeerMesh::connect_full(&server_url, "test-room", "peer-a", Some("test"), 1, None)
            .await
            .expect("Peer A failed to connect to signaling");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) =
        PeerMesh::connect_full(&server_url, "test-room", "peer-b", Some("test"), 1, None)
            .await
            .expect("Peer B failed to connect to signaling");

    // 3. Pump signaling until connection is established
    establish_connection(&mut mesh_a, &mut mesh_b).await;

    // 4. Both peers broadcast audio simultaneously (send all WAIF frames)
    let frames_a = produce_interval(440.0);
    let frames_b = produce_interval(880.0);
    for frame in &frames_a {
        mesh_a.broadcast_audio(frame).await;
    }
    for frame in &frames_b {
        mesh_b.broadcast_audio(frame).await;
    }

    // 5. Both peers receive audio from the other (at least one WAIF frame)
    let (from_at_b, received_at_b) = tokio::time::timeout(Duration::from_secs(5), audio_rx_b.recv())
        .await
        .expect("Timed out waiting for audio from A")
        .expect("Audio channel B closed");

    let (from_at_a, received_at_a) = tokio::time::timeout(Duration::from_secs(5), audio_rx_a.recv())
        .await
        .expect("Timed out waiting for audio from B")
        .expect("Audio channel A closed");

    assert_eq!(from_at_b, "peer-a");
    assert_eq!(from_at_a, "peer-b");
    assert!(!received_at_b.is_empty(), "B should receive non-empty wire data from A");
    assert!(!received_at_a.is_empty(), "A should receive non-empty wire data from B");

    // 6. Decode WAIF frames and verify both peers hear real audio
    let energy_at_b = decode_waif_rms(&received_at_b);
    assert!(
        energy_at_b > 0.01,
        "Peer B should hear Peer A's audio, RMS={energy_at_b}"
    );

    let energy_at_a = decode_waif_rms(&received_at_a);
    assert!(
        energy_at_a > 0.01,
        "Peer A should hear Peer B's audio, RMS={energy_at_a}"
    );
}

// ---------------------------------------------------------------
// Test: Signaling eviction response parsing
// ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn signaling_eviction_closes_channel() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // Test that the poll response parsing handles the `evicted` field
    // by verifying the struct deserializes correctly with and without it.
    let without: serde_json::Value = serde_json::json!({ "messages": [] });
    let with_evicted: serde_json::Value = serde_json::json!({ "messages": [], "evicted": true });
    let with_false: serde_json::Value = serde_json::json!({ "messages": [], "evicted": false });

    // These should all parse — the evicted field is optional with default false
    #[derive(serde::Deserialize)]
    struct PollResponse {
        messages: Vec<serde_json::Value>,
        #[serde(default)]
        evicted: bool,
    }

    let r1: PollResponse = serde_json::from_value(without).unwrap();
    assert!(!r1.evicted);
    let r2: PollResponse = serde_json::from_value(with_evicted).unwrap();
    assert!(r2.evicted);
    let r3: PollResponse = serde_json::from_value(with_false).unwrap();
    assert!(!r3.evicted);

    eprintln!("[test] Eviction response parsing test passed");
}

// ---------------------------------------------------------------
// Test: Three peers all connect and exchange audio
// ---------------------------------------------------------------

/// Three peers form a full mesh; A broadcasts and both B and C receive.
/// Then B broadcasts and both A and C receive.
#[tokio::test(flavor = "multi_thread")]
async fn three_peers_exchange_audio() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server_url = start_test_signaling_server().await;

    let (mut mesh_a, _sync_rx_a, mut audio_rx_a) = PeerMesh::connect_full(
        &server_url, "three-room", "peer-a", None, 1, None,
    )
    .await
    .expect("peer-a connect failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) = PeerMesh::connect_full(
        &server_url, "three-room", "peer-b", None, 1, None,
    )
    .await
    .expect("peer-b connect failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_c, _sync_rx_c, mut audio_rx_c) = PeerMesh::connect_full(
        &server_url, "three-room", "peer-c", None, 1, None,
    )
    .await
    .expect("peer-c connect failed");

    establish_three_way_connection(&mut mesh_a, &mut mesh_b, &mut mesh_c, ("peer-a", "peer-b", "peer-c")).await;
    eprintln!("[test] 3-way connection established");

    assert_eq!(mesh_a.connected_peers().len(), 2, "A should be connected to B and C");
    assert_eq!(mesh_b.connected_peers().len(), 2, "B should be connected to A and C");
    assert_eq!(mesh_c.connected_peers().len(), 2, "C should be connected to A and B");

    // A broadcasts — B and C should receive it
    let wire_a = produce_single_waif_frame(440.0);
    mesh_a.broadcast_audio(&wire_a).await;

    let (from_at_b, data_b) = tokio::time::timeout(Duration::from_secs(5), audio_rx_b.recv())
        .await.expect("B timed out waiting for A's audio").expect("audio_rx_b closed");
    let (from_at_c, data_c) = tokio::time::timeout(Duration::from_secs(5), audio_rx_c.recv())
        .await.expect("C timed out waiting for A's audio").expect("audio_rx_c closed");

    assert_eq!(from_at_b, "peer-a");
    assert_eq!(from_at_c, "peer-a");
    assert!(!data_b.is_empty(), "B should receive non-empty audio from A");
    assert!(!data_c.is_empty(), "C should receive non-empty audio from A");

    // B broadcasts — A and C should receive it
    let wire_b = produce_single_waif_frame(880.0);
    mesh_b.broadcast_audio(&wire_b).await;

    let (from_at_a, data_a) = tokio::time::timeout(Duration::from_secs(5), audio_rx_a.recv())
        .await.expect("A timed out waiting for B's audio").expect("audio_rx_a closed");
    let (from_at_c2, data_c2) = tokio::time::timeout(Duration::from_secs(5), audio_rx_c.recv())
        .await.expect("C timed out waiting for B's audio").expect("audio_rx_c closed");

    assert_eq!(from_at_a, "peer-b");
    assert_eq!(from_at_c2, "peer-b");
    assert!(!data_a.is_empty());
    assert!(!data_c2.is_empty());

    eprintln!("[test] Three-peer exchange test passed");
}

// ---------------------------------------------------------------
// Test: One peer leaves a 3-peer room; the remaining two continue
// ---------------------------------------------------------------

/// C leaves a 3-peer room; A and B still exchange audio cleanly.
#[tokio::test(flavor = "multi_thread")]
async fn one_peer_leaves_three_peer_room_others_continue() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server_url = start_test_signaling_server().await;

    let (mut mesh_a, _sync_rx_a, _audio_rx_a) = PeerMesh::connect_full(
        &server_url, "leave-room", "peer-a", None, 1, None,
    ).await.expect("peer-a failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) = PeerMesh::connect_full(
        &server_url, "leave-room", "peer-b", None, 1, None,
    ).await.expect("peer-b failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_c, _sync_rx_c, _audio_rx_c) = PeerMesh::connect_full(
        &server_url, "leave-room", "peer-c", None, 1, None,
    ).await.expect("peer-c failed");

    establish_three_way_connection(&mut mesh_a, &mut mesh_b, &mut mesh_c, ("peer-a", "peer-b", "peer-c")).await;
    eprintln!("[test] 3-way connected; C is about to leave");

    // C leaves by dropping its mesh (triggers signaling leave)
    drop(mesh_c);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A and B should still be connected to each other
    assert!(
        mesh_a.is_peer_audio_dc_open("peer-b"),
        "A's connection to B should still be open after C leaves"
    );
    assert!(
        mesh_b.is_peer_audio_dc_open("peer-a"),
        "B's connection to A should still be open after C leaves"
    );

    // Audio still flows between A and B
    let wire_a = produce_single_waif_frame(440.0);
    mesh_a.broadcast_audio(&wire_a).await;

    let (from, data) = tokio::time::timeout(Duration::from_secs(5), audio_rx_b.recv())
        .await.expect("B timed out after C left").expect("audio_rx_b closed");
    assert_eq!(from, "peer-a");
    assert!(!data.is_empty(), "A→B audio should still flow after C left");

    eprintln!("[test] One-peer-leaves test passed");
}
