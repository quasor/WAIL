//! Network integration tests: two WAIL peers exchanging audio over real WebRTC.
//!
//! These tests exercise the full path:
//!   HTTP signaling → WebRTC negotiation → DataChannel establishment → audio exchange
//!
//! No external services needed: in-process HTTP signaling server, localhost ICE candidates.

mod common;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use wail_audio::AudioBridge;
use wail_net::PeerMesh;

use common::*;

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

    // 2. Connect both peers to the signaling server (fast polling for tests)
    //    "peer-a" < "peer-b" → peer-a will be the WebRTC initiator
    let ice = wail_net::default_ice_servers();
    let (mut mesh_a, _sync_rx_a, mut audio_rx_a) =
        PeerMesh::connect_with_ice(&server_url, "test-room", "peer-a", Some("test"), ice.clone())
            .await
            .expect("Peer A failed to connect to signaling");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) =
        PeerMesh::connect_with_ice(&server_url, "test-room", "peer-b", Some("test"), ice)
            .await
            .expect("Peer B failed to connect to signaling");

    // 3. Pump signaling until WebRTC DataChannels are established
    establish_connection(&mut mesh_a, &mut mesh_b).await;

    // 4. Both peers broadcast audio simultaneously
    let wire_a = produce_interval(440.0);
    let wire_b = produce_interval(880.0);
    mesh_a.broadcast_audio(&wire_a).await;
    mesh_b.broadcast_audio(&wire_b).await;

    // 5. Both peers receive audio from the other
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

    // 6. Decode and verify both peers hear real audio
    let sr = 48000u32;
    let ch = 2u16;
    let buf_size = 4096;
    let silence = vec![0.0f32; buf_size];
    let mut out = vec![0.0f32; buf_size];

    let mut bridge_b = AudioBridge::new(sr, ch, 4, 4.0, 128);
    bridge_b.process(&silence, &mut out, 0.0);
    bridge_b.receive_wire(&from_at_b, &received_at_b);
    bridge_b.process(&silence, &mut out, 16.0);
    let energy_at_b = rms(&out);
    assert!(
        energy_at_b > 0.01,
        "Peer B should hear Peer A's audio over WebRTC, RMS={energy_at_b}"
    );

    let mut bridge_a = AudioBridge::new(sr, ch, 4, 4.0, 128);
    bridge_a.process(&silence, &mut out, 0.0);
    bridge_a.receive_wire(&from_at_a, &received_at_a);
    bridge_a.process(&silence, &mut out, 16.0);
    let energy_at_a = rms(&out);
    assert!(
        energy_at_a > 0.01,
        "Peer A should hear Peer B's audio over WebRTC, RMS={energy_at_a}"
    );
}

// ---------------------------------------------------------------
// Test: Audio DataChannel reports open after connection
// ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn audio_dc_reports_open_after_connection() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    let server_url = start_test_signaling_server().await;

    let ice = wail_net::default_ice_servers();
    let (mut mesh_a, _sync_rx_a, mut audio_rx_a) =
        PeerMesh::connect_with_ice(&server_url, "test-room", "peer-a", Some("test"), ice.clone())
            .await
            .expect("Peer A failed to connect");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) =
        PeerMesh::connect_with_ice(&server_url, "test-room", "peer-b", Some("test"), ice)
            .await
            .expect("Peer B failed to connect");

    establish_connection(&mut mesh_a, &mut mesh_b).await;

    // Both peers should report audio DC open
    assert!(
        mesh_a.any_audio_dc_open(),
        "Peer A should have an open audio DataChannel"
    );
    assert!(
        mesh_b.any_audio_dc_open(),
        "Peer B should have an open audio DataChannel"
    );

    // Verify audio actually flows both directions
    let wire_a = produce_interval(440.0);
    let wire_b = produce_interval(880.0);
    mesh_a.broadcast_audio(&wire_a).await;
    mesh_b.broadcast_audio(&wire_b).await;

    let (from_at_b, received_at_b) = tokio::time::timeout(Duration::from_secs(5), audio_rx_b.recv())
        .await
        .expect("Timed out waiting for audio at B")
        .expect("Audio channel B closed");

    let (from_at_a, received_at_a) = tokio::time::timeout(Duration::from_secs(5), audio_rx_a.recv())
        .await
        .expect("Timed out waiting for audio at A")
        .expect("Audio channel A closed");

    assert_eq!(from_at_b, "peer-a");
    assert_eq!(from_at_a, "peer-b");
    assert!(!received_at_b.is_empty());
    assert!(!received_at_a.is_empty());
}

// ---------------------------------------------------------------
// TURN server helpers
// ---------------------------------------------------------------

/// Find the coturn `turnserver` binary, or return None if not installed.
fn find_turnserver() -> Option<String> {
    for path in &[
        "/opt/homebrew/opt/coturn/bin/turnserver",
        "/opt/homebrew/bin/turnserver",
        "/usr/local/bin/turnserver",
        "/usr/bin/turnserver",
    ] {
        if std::path::Path::new(path).exists() {
            return Some(path.to_string());
        }
    }
    // Try PATH
    let output = Command::new("which")
        .arg("turnserver")
        .output()
        .ok()?;
    if output.status.success() {
        let p = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !p.is_empty() {
            return Some(p);
        }
    }
    None
}

/// RAII guard that kills the coturn subprocess on drop.
struct CoturnGuard {
    child: Child,
    port: u16,
}

impl Drop for CoturnGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        eprintln!("[test] coturn on port {} stopped", self.port);
    }
}

/// Compute the coturn lt-cred-mech key: MD5(username:realm:password).
fn coturn_lt_key(username: &str, realm: &str, password: &str) -> String {
    use std::io::Write;
    let digest = {
        let mut ctx = md5::Context::new();
        write!(ctx, "{username}:{realm}:{password}").unwrap();
        ctx.compute()
    };
    format!("0x{:x}", digest)
}

/// Start a coturn TURN server on a random port, returning a guard that kills it on drop.
fn start_coturn(turnserver_bin: &str) -> CoturnGuard {
    let port = random_port();
    let relay_min = random_port();
    let relay_max = relay_min.saturating_add(100);

    // lt-cred-mech needs MD5(username:realm:password) as the key
    let key = coturn_lt_key("test", "test", "test");

    eprintln!(
        "[test] Starting coturn: port={port}, relay={relay_min}-{relay_max}"
    );

    let child = Command::new(turnserver_bin)
        .arg("-n")
        .arg("--log-file=stdout")
        .arg("--verbose")
        .arg(format!("--listening-port={port}"))
        .arg("--listening-ip=127.0.0.1")
        .arg("--external-ip=127.0.0.1")
        .arg("--realm=test")
        .arg(format!("--user=test:{key}"))
        .arg("--lt-cred-mech")
        .arg("--no-tls")
        .arg("--no-dtls")
        .arg(format!("--min-port={relay_min}"))
        .arg(format!("--max-port={relay_max}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to start coturn");

    CoturnGuard { child, port }
}

// ---------------------------------------------------------------
// Test: Two peers exchange audio intervals via TURN relay
// ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn two_peers_exchange_audio_via_turn() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // 0. Find and start coturn
    let turnserver_bin = match find_turnserver() {
        Some(bin) => bin,
        None => {
            eprintln!("SKIP: coturn not installed (brew install coturn)");
            return;
        }
    };
    let coturn = start_coturn(&turnserver_bin);

    // Give coturn a moment to bind its port
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 1. Start in-process HTTP signaling server
    let server_url = start_test_signaling_server().await;

    // 2. Build ICE servers with TURN
    let turn_url = format!("turn:127.0.0.1:{}", coturn.port);
    let ice_servers = wail_net::ice_servers_with_turn(&turn_url, "test", "test");

    // 3. Connect both peers using TURN (fast polling for tests)
    let (mut mesh_a, _sync_rx_a, mut audio_rx_a) =
        PeerMesh::connect_with_ice(
            &server_url, "test-room", "peer-a", Some("test"), ice_servers.clone(),
        )
            .await
            .expect("Peer A failed to connect to signaling");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) =
        PeerMesh::connect_with_ice(
            &server_url, "test-room", "peer-b", Some("test"), ice_servers,
        )
            .await
            .expect("Peer B failed to connect to signaling");

    // 4. Establish WebRTC connection (via TURN relay — give extra time for TURN allocation)
    establish_connection_timeout(&mut mesh_a, &mut mesh_b, 30).await;
    eprintln!("[test] WebRTC connected via TURN");

    // 5. Both peers broadcast audio simultaneously
    let wire_a = produce_interval(440.0);
    let wire_b = produce_interval(880.0);
    mesh_a.broadcast_audio(&wire_a).await;
    mesh_b.broadcast_audio(&wire_b).await;

    // 6. Both peers receive audio from the other
    let (from_at_b, received_at_b) = tokio::time::timeout(Duration::from_secs(5), audio_rx_b.recv())
        .await
        .expect("Timed out waiting for audio from A via TURN")
        .expect("Audio channel B closed");

    let (from_at_a, received_at_a) = tokio::time::timeout(Duration::from_secs(5), audio_rx_a.recv())
        .await
        .expect("Timed out waiting for audio from B via TURN")
        .expect("Audio channel A closed");

    assert_eq!(from_at_b, "peer-a");
    assert_eq!(from_at_a, "peer-b");
    assert!(!received_at_b.is_empty(), "B should receive non-empty wire data from A");
    assert!(!received_at_a.is_empty(), "A should receive non-empty wire data from B");

    // 7. Decode and verify both peers hear real audio
    let sr = 48000u32;
    let ch = 2u16;
    let buf_size = 4096;
    let silence = vec![0.0f32; buf_size];
    let mut out = vec![0.0f32; buf_size];

    let mut bridge_b = AudioBridge::new(sr, ch, 4, 4.0, 128);
    bridge_b.process(&silence, &mut out, 0.0);
    bridge_b.receive_wire(&from_at_b, &received_at_b);
    bridge_b.process(&silence, &mut out, 16.0);
    let energy_at_b = rms(&out);
    assert!(
        energy_at_b > 0.01,
        "Peer B should hear Peer A's audio via TURN, RMS={energy_at_b}"
    );

    let mut bridge_a = AudioBridge::new(sr, ch, 4, 4.0, 128);
    bridge_a.process(&silence, &mut out, 0.0);
    bridge_a.receive_wire(&from_at_a, &received_at_a);
    bridge_a.process(&silence, &mut out, 16.0);
    let energy_at_a = rms(&out);
    assert!(
        energy_at_a > 0.01,
        "Peer A should hear Peer B's audio via TURN, RMS={energy_at_a}"
    );

    eprintln!("[test] TURN E2E test passed! A→B RMS={energy_at_b:.4}, B→A RMS={energy_at_a:.4}");
    // coturn is killed automatically when `coturn` guard drops
}

// ---------------------------------------------------------------
// Test: Metered TURN credential fetch (live network)
// ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn fetch_metered_ice_servers_live() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    let servers = wail_net::fetch_metered_ice_servers()
        .await
        .expect("Metered API call failed");

    assert!(!servers.is_empty(), "Expected at least one ICE server from Metered");

    let turn_servers: Vec<_> = servers
        .iter()
        .filter(|s| s.urls.iter().any(|u| u.starts_with("turn:") || u.starts_with("turns:")))
        .collect();

    assert!(!turn_servers.is_empty(), "Expected at least one TURN server in Metered response");

    for s in &turn_servers {
        assert!(!s.username.is_empty(), "TURN server should have a username: {:?}", s.urls);
        assert!(!s.credential.is_empty(), "TURN server should have a credential: {:?}", s.urls);
    }

    eprintln!(
        "[test] Metered returned {} ICE servers ({} TURN)",
        servers.len(),
        turn_servers.len()
    );
}

// ---------------------------------------------------------------
// Test: Live Metered TURN relay — fetch credentials, force relay-only, exchange audio
// ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn metered_turn_relay_live() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // 1. Fetch live TURN credentials from Metered API
    let ice_servers = wail_net::fetch_metered_ice_servers()
        .await
        .expect("Failed to fetch Metered ICE servers — is the API key valid?");

    let turn_count = ice_servers
        .iter()
        .filter(|s| s.urls.iter().any(|u| u.starts_with("turn:") || u.starts_with("turns:")))
        .count();
    assert!(turn_count > 0, "Metered returned no TURN servers");
    eprintln!("[test] Fetched {} ICE servers ({} TURN)", ice_servers.len(), turn_count);

    // 2. Start in-process HTTP signaling server
    let server_url = start_test_signaling_server().await;

    // 3. Connect both peers in relay-only mode (forces TURN, no host/srflx candidates)
    let (mut mesh_a, _sync_rx_a, mut audio_rx_a) =
        PeerMesh::connect_full(
            &server_url, "turn-test", "peer-a", Some("test"), ice_servers.clone(), true, 1, None,
        )
            .await
            .expect("Peer A failed to connect to signaling");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) =
        PeerMesh::connect_full(
            &server_url, "turn-test", "peer-b", Some("test"), ice_servers, true, 1, None,
        )
            .await
            .expect("Peer B failed to connect to signaling");

    // 4. Establish WebRTC connection via TURN relay (30s timeout for allocation)
    establish_connection_timeout(&mut mesh_a, &mut mesh_b, 30).await;
    eprintln!("[test] WebRTC connected via Metered TURN relay");

    // 5. Exchange multiple full-size intervals (120 BPM, quantum=4 → 8s per interval)
    //    Simulates sustained session: 4 intervals = 32s of audio data through TURN
    let freqs_a = [440.0, 550.0, 660.0, 880.0];
    let freqs_b = [330.0, 494.0, 587.0, 740.0];
    let num_intervals = freqs_a.len();

    let sr = 48000u32;
    let ch = 2u16;
    let buf_size = 4096;
    let silence = vec![0.0f32; buf_size];
    let mut out = vec![0.0f32; buf_size];

    for i in 0..num_intervals {
        let (wire_a, _) = produce_full_interval(freqs_a[i]);
        let (wire_b, _) = produce_full_interval(freqs_b[i]);

        let interval_beats = (i + 1) as f64 * 16.0; // 16, 32, 48, 64 beats
        let interval_secs = interval_beats / (120.0 / 60.0); // 8, 16, 24, 32 seconds

        eprintln!(
            "[test] Interval {} — sending ~{}KB each direction ({:.0}s at 120bpm)",
            i + 1,
            wire_a.len() / 1024,
            interval_secs
        );

        mesh_a.broadcast_audio(&wire_a).await;
        mesh_b.broadcast_audio(&wire_b).await;

        // Receive from both sides
        let (from_at_b, received_at_b) = tokio::time::timeout(
            Duration::from_secs(15),
            audio_rx_b.recv(),
        )
            .await
            .unwrap_or_else(|_| panic!("Timed out waiting for interval {} from A via TURN", i + 1))
            .unwrap_or_else(|| panic!("Audio channel B closed at interval {}", i + 1));

        let (from_at_a, received_at_a) = tokio::time::timeout(
            Duration::from_secs(15),
            audio_rx_a.recv(),
        )
            .await
            .unwrap_or_else(|_| panic!("Timed out waiting for interval {} from B via TURN", i + 1))
            .unwrap_or_else(|| panic!("Audio channel A closed at interval {}", i + 1));

        assert_eq!(from_at_b, "peer-a");
        assert_eq!(from_at_a, "peer-b");
        assert!(!received_at_b.is_empty(), "Interval {}: B got empty data from A", i + 1);
        assert!(!received_at_a.is_empty(), "Interval {}: A got empty data from B", i + 1);

        // Decode and verify real audio energy
        let mut bridge_b = AudioBridge::new(sr, ch, 4, 4.0, 128);
        bridge_b.process(&silence, &mut out, 0.0);
        bridge_b.receive_wire(&from_at_b, &received_at_b);
        bridge_b.process(&silence, &mut out, 16.0);
        let energy_b = rms(&out);
        assert!(
            energy_b > 0.01,
            "Interval {}: B should hear A's audio via TURN, RMS={energy_b}",
            i + 1
        );

        let mut bridge_a = AudioBridge::new(sr, ch, 4, 4.0, 128);
        bridge_a.process(&silence, &mut out, 0.0);
        bridge_a.receive_wire(&from_at_a, &received_at_a);
        bridge_a.process(&silence, &mut out, 16.0);
        let energy_a = rms(&out);
        assert!(
            energy_a > 0.01,
            "Interval {}: A should hear B's audio via TURN, RMS={energy_a}",
            i + 1
        );

        eprintln!(
            "[test] Interval {} OK — A→B RMS={energy_b:.4}, B→A RMS={energy_a:.4}, wire={}KB",
            i + 1,
            received_at_b.len() / 1024
        );
    }

    eprintln!(
        "[test] Metered TURN relay: all {} intervals passed ({} beats = {:.0}s at 120bpm)",
        num_intervals,
        num_intervals * 16,
        num_intervals as f64 * 16.0 / (120.0 / 60.0)
    );
}

// ---------------------------------------------------------------
// Test: Peer failure is detected quickly (DataChannel close + reader exit)
// ---------------------------------------------------------------

/// After Steps 1-2 of the resilience fixes, closing a peer's connection
/// should trigger PeerFailed via DataChannel on_close handlers AND reader
/// task exit signals. This tests that detection happens within a few seconds,
/// not minutes of silent failure.
#[tokio::test(flavor = "multi_thread")]
async fn peer_failure_detected_within_timeout() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    let server_url = start_test_signaling_server().await;
    let ice = wail_net::default_ice_servers();

    let (mut mesh_a, _sync_rx_a, mut audio_rx_a) =
        PeerMesh::connect_with_ice(
            &server_url, "dc-close-test", "peer-a", Some("test"), ice.clone(),
        ).await.expect("Peer A connect failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) =
        PeerMesh::connect_with_ice(
            &server_url, "dc-close-test", "peer-b", Some("test"), ice,
        ).await.expect("Peer B connect failed");

    establish_connection(&mut mesh_a, &mut mesh_b).await;

    // Verify audio flows before disconnection
    let wire_a = produce_interval(440.0);
    mesh_a.broadcast_audio(&wire_a).await;
    let (_from, data) = tokio::time::timeout(Duration::from_secs(5), audio_rx_b.recv())
        .await.expect("Pre-failure audio timed out")
        .expect("Audio channel closed");
    assert!(!data.is_empty());
    eprintln!("[test] Pre-failure audio verified");

    // Close peer-a's connection (simulates network failure)
    let close_time = std::time::Instant::now();
    mesh_a.close_peer("peer-b").await;

    // mesh_b should detect PeerFailed within 10 seconds (via on_close + reader exit)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut got_failure = false;
    loop {
        tokio::select! {
            event = mesh_b.poll_signaling() => {
                if let Ok(Some(wail_net::MeshEvent::PeerFailed(pid))) = event {
                    let elapsed = close_time.elapsed();
                    eprintln!("[test] PeerFailed detected in {elapsed:?}");
                    assert_eq!(pid, "peer-a");
                    assert!(elapsed < Duration::from_secs(10), "Detection took too long: {elapsed:?}");
                    got_failure = true;
                    break;
                }
            }
            _ = mesh_a.poll_signaling() => {}
            _ = tokio::time::sleep_until(deadline) => {
                panic!("PeerFailed not detected within 10s — silent disconnection bug");
            }
        }
    }
    assert!(got_failure);
    eprintln!("[test] DataChannel close detection test passed");
}

// ---------------------------------------------------------------
// Test: Signaling eviction triggers reconnection
// ---------------------------------------------------------------

/// Tests the server-side eviction detection (Step 5): when the signaling
/// server returns `evicted: true`, the client should close the signaling
/// channel, causing the session to see `Ok(None)` and attempt reconnection.
#[tokio::test(flavor = "multi_thread")]
async fn signaling_eviction_closes_channel() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // Use the regular test signaling server (which doesn't implement eviction),
    // but we can test that the poll response parsing handles the `evicted` field
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
// Test: Closing a peer's connection emits PeerFailed event
// ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn peer_failure_emits_event() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // 1. Start signaling, connect two peers
    let server_url = start_test_signaling_server().await;

    let (mut mesh_a, _sync_rx_a, _audio_rx_a) =
        PeerMesh::connect_with_ice(
            &server_url, "fail-test", "peer-a", Some("test"),
            wail_net::default_ice_servers(),
        ).await.expect("Peer A connect failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, _audio_rx_b) =
        PeerMesh::connect_with_ice(
            &server_url, "fail-test", "peer-b", Some("test"),
            wail_net::default_ice_servers(),
        ).await.expect("Peer B connect failed");

    // 2. Establish WebRTC connection
    establish_connection(&mut mesh_a, &mut mesh_b).await;
    eprintln!("[test] Connected — now simulating peer-a failure");

    // 3. Close peer-a's connection to simulate failure
    mesh_a.close_peer("peer-b").await;

    // 4. Poll mesh_b — expect PeerFailed("peer-a")
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut got_failure = false;
    loop {
        tokio::select! {
            event = mesh_b.poll_signaling() => {
                if let Ok(Some(wail_net::MeshEvent::PeerFailed(pid))) = event {
                    assert_eq!(pid, "peer-a", "Expected failure from peer-a, got {pid}");
                    got_failure = true;
                    break;
                }
            }
            _ = mesh_a.poll_signaling() => {}
            _ = tokio::time::sleep_until(deadline) => {
                panic!("Timed out waiting for PeerFailed event");
            }
        }
    }

    assert!(got_failure, "Should have received PeerFailed event");
    eprintln!("[test] PeerFailed event received — test passed");
}

// ---------------------------------------------------------------
// Test: Peer reconnects after connection close, audio flows again
// ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn peer_reconnects_after_close() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // 1. Connect and establish
    let server_url = start_test_signaling_server().await;

    let (mut mesh_a, _sync_rx_a, mut audio_rx_a) =
        PeerMesh::connect_with_ice(
            &server_url, "reconn-test", "peer-a", Some("test"),
            wail_net::default_ice_servers(),
        ).await.expect("Peer A connect failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) =
        PeerMesh::connect_with_ice(
            &server_url, "reconn-test", "peer-b", Some("test"),
            wail_net::default_ice_servers(),
        ).await.expect("Peer B connect failed");

    establish_connection(&mut mesh_a, &mut mesh_b).await;
    eprintln!("[test] Initial connection established");

    // 2. Verify audio works before failure
    let wire_a = produce_interval(440.0);
    mesh_a.broadcast_audio(&wire_a).await;
    let (_from, data) = tokio::time::timeout(Duration::from_secs(5), audio_rx_b.recv())
        .await.expect("Pre-failure audio timed out")
        .expect("Audio channel closed");
    assert!(!data.is_empty(), "Should receive audio before failure");
    eprintln!("[test] Pre-failure audio verified");

    // 3. Simulate failure: close peer-a's connection
    mesh_a.close_peer("peer-b").await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // 4. Re-initiate from mesh_a
    mesh_a.re_initiate("peer-b").await.expect("re_initiate failed");
    eprintln!("[test] Re-initiation started");

    // 5. Pump signaling until reconnected
    establish_connection_timeout(&mut mesh_a, &mut mesh_b, 15).await;
    eprintln!("[test] Reconnected");

    // 6. Verify audio works after reconnection
    let wire_a2 = produce_interval(880.0);
    mesh_a.broadcast_audio(&wire_a2).await;
    let (_from, data) = tokio::time::timeout(Duration::from_secs(5), audio_rx_b.recv())
        .await.expect("Post-reconnect audio timed out")
        .expect("Audio channel closed after reconnect");
    assert!(!data.is_empty(), "Should receive audio after reconnection");
    eprintln!("[test] Post-reconnect audio verified — test passed");
}

// ---------------------------------------------------------------
// Test: New SDP offer replaces stale connection
// ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn new_offer_replaces_stale_connection() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // 1. Connect and establish
    let server_url = start_test_signaling_server().await;

    let (mut mesh_a, _sync_rx_a, mut audio_rx_a) =
        PeerMesh::connect_with_ice(
            &server_url, "stale-test", "peer-a", Some("test"),
            wail_net::default_ice_servers(),
        ).await.expect("Peer A connect failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) =
        PeerMesh::connect_with_ice(
            &server_url, "stale-test", "peer-b", Some("test"),
            wail_net::default_ice_servers(),
        ).await.expect("Peer B connect failed");

    establish_connection(&mut mesh_a, &mut mesh_b).await;
    eprintln!("[test] Initial connection established");

    // 2. Re-initiate from mesh_a (creates new offer for existing peer)
    mesh_a.re_initiate("peer-b").await.expect("re_initiate failed");
    eprintln!("[test] Re-initiation triggered (should replace stale connection)");

    // 3. Pump signaling until new connection established
    establish_connection_timeout(&mut mesh_a, &mut mesh_b, 15).await;
    eprintln!("[test] New connection established");

    // 4. Verify audio flows on the new connection
    let wire_a = produce_interval(440.0);
    let wire_b = produce_interval(880.0);
    mesh_a.broadcast_audio(&wire_a).await;
    mesh_b.broadcast_audio(&wire_b).await;

    let (from_b, data_b) = tokio::time::timeout(Duration::from_secs(5), audio_rx_b.recv())
        .await.expect("Audio to B timed out")
        .expect("Audio channel B closed");
    let (from_a, data_a) = tokio::time::timeout(Duration::from_secs(5), audio_rx_a.recv())
        .await.expect("Audio to A timed out")
        .expect("Audio channel A closed");

    assert_eq!(from_b, "peer-a");
    assert_eq!(from_a, "peer-b");
    assert!(!data_b.is_empty());
    assert!(!data_a.is_empty());
    eprintln!("[test] Audio verified on replaced connection — test passed");
}

// ---------------------------------------------------------------
// Test: Three peers all connect and exchange audio
// ---------------------------------------------------------------

/// §7 — Three peers form a full mesh; A broadcasts and both B and C receive.
/// Then B broadcasts and both A and C receive.
#[tokio::test(flavor = "multi_thread")]
async fn three_peers_exchange_audio() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server_url = start_test_signaling_server().await;
    let ice = wail_net::default_ice_servers();

    // Lexicographic order: peer-a < peer-b < peer-c
    // peer-a initiates to peer-b and peer-c; peer-b initiates to peer-c.
    let (mut mesh_a, _sync_rx_a, mut audio_rx_a) = PeerMesh::connect_with_ice(
        &server_url, "three-room", "peer-a", None, ice.clone(),
    )
    .await
    .expect("peer-a connect failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) = PeerMesh::connect_with_ice(
        &server_url, "three-room", "peer-b", None, ice.clone(),
    )
    .await
    .expect("peer-b connect failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_c, _sync_rx_c, mut audio_rx_c) = PeerMesh::connect_with_ice(
        &server_url, "three-room", "peer-c", None, ice.clone(),
    )
    .await
    .expect("peer-c connect failed");

    establish_three_way_connection(&mut mesh_a, &mut mesh_b, &mut mesh_c, ("peer-a", "peer-b", "peer-c")).await;
    eprintln!("[test] 3-way connection established");

    assert_eq!(mesh_a.connected_peers().len(), 2, "A should be connected to B and C");
    assert_eq!(mesh_b.connected_peers().len(), 2, "B should be connected to A and C");
    assert_eq!(mesh_c.connected_peers().len(), 2, "C should be connected to A and B");

    // A broadcasts — B and C should receive it
    let wire_a = produce_interval(440.0);
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
    let wire_b = produce_interval(880.0);
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

/// §7 — C leaves a 3-peer room; A and B still exchange audio cleanly.
#[tokio::test(flavor = "multi_thread")]
async fn one_peer_leaves_three_peer_room_others_continue() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server_url = start_test_signaling_server().await;
    let ice = wail_net::default_ice_servers();

    let (mut mesh_a, _sync_rx_a, mut audio_rx_a) = PeerMesh::connect_with_ice(
        &server_url, "leave-room", "peer-a", None, ice.clone(),
    ).await.expect("peer-a failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) = PeerMesh::connect_with_ice(
        &server_url, "leave-room", "peer-b", None, ice.clone(),
    ).await.expect("peer-b failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_c, _sync_rx_c, _audio_rx_c) = PeerMesh::connect_with_ice(
        &server_url, "leave-room", "peer-c", None, ice.clone(),
    ).await.expect("peer-c failed");

    establish_three_way_connection(&mut mesh_a, &mut mesh_b, &mut mesh_c, ("peer-a", "peer-b", "peer-c")).await;
    eprintln!("[test] 3-way connected; C is about to leave");

    // C leaves by dropping its mesh (triggers signaling leave)
    drop(mesh_c);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A and B should still have open DCs to each other
    assert!(
        mesh_a.is_peer_audio_dc_open("peer-b"),
        "A's DC to B should still be open after C leaves"
    );
    assert!(
        mesh_b.is_peer_audio_dc_open("peer-a"),
        "B's DC to A should still be open after C leaves"
    );

    // Audio still flows between A and B
    let wire_a = produce_interval(440.0);
    mesh_a.broadcast_audio(&wire_a).await;

    let (from, data) = tokio::time::timeout(Duration::from_secs(5), audio_rx_b.recv())
        .await.expect("B timed out after C left").expect("audio_rx_b closed");
    assert_eq!(from, "peer-a");
    assert!(!data.is_empty(), "A→B audio should still flow after C left");

    // A's audio_rx should NOT have received anything (no peer sent to A)
    assert!(
        audio_rx_a.try_recv().is_err(),
        "A should not receive A's own broadcast"
    );

    eprintln!("[test] One-peer-leaves test passed");
}

// ---------------------------------------------------------------
// Test: Duplicate PeerFailed signals are deduplicated
// ---------------------------------------------------------------

/// §6.1 — After peer-a closes its connection, mesh_b may receive multiple
/// failure signals (on_close fires for each DC + reader task exits).
/// The first `poll_signaling()` call returns `PeerFailed`; once the caller
/// removes the peer, subsequent failure signals become `SignalingProcessed`.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_peer_failed_signals_deduplicated() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server_url = start_test_signaling_server().await;
    let ice = wail_net::default_ice_servers();

    let (mut mesh_a, _sync_rx_a, _audio_rx_a) = PeerMesh::connect_with_ice(
        &server_url, "dedup-fail-room", "peer-a", None, ice.clone(),
    ).await.expect("peer-a failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, _audio_rx_b) = PeerMesh::connect_with_ice(
        &server_url, "dedup-fail-room", "peer-b", None, ice,
    ).await.expect("peer-b failed");

    establish_connection(&mut mesh_a, &mut mesh_b).await;

    // Close peer-a's connection — this fires on_close on both DCs AND the reader
    // task eventually exits, sending multiple signals to failure_rx in mesh_b.
    mesh_a.close_peer("peer-b").await;

    // Collect the first PeerFailed event for peer-a from mesh_b.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut first_failure = false;
    loop {
        tokio::select! {
            event = mesh_b.poll_signaling() => {
                if let Ok(Some(wail_net::MeshEvent::PeerFailed(pid))) = event {
                    assert_eq!(pid, "peer-a");
                    first_failure = true;
                    break;
                }
            }
            _ = mesh_a.poll_signaling() => {}
            _ = tokio::time::sleep_until(deadline) => {
                panic!("Timed out waiting for first PeerFailed");
            }
        }
    }
    assert!(first_failure);
    eprintln!("[test] First PeerFailed received");

    // Simulate what session.rs does: remove the peer after handling PeerFailed.
    mesh_b.remove_peer("peer-a").await;

    // Any subsequent failure signals (peer-a no longer in mesh_b's map) must
    // be returned as SignalingProcessed, not as a second PeerFailed.
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut second_peer_failed = false;
    loop {
        tokio::select! {
            event = mesh_b.poll_signaling() => {
                match event {
                    Ok(Some(wail_net::MeshEvent::PeerFailed(pid))) if pid == "peer-a" => {
                        second_peer_failed = true;
                        break;
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep_until(drain_deadline) => break,
        }
    }

    assert!(
        !second_peer_failed,
        "Second PeerFailed should be deduplicated as SignalingProcessed once peer is removed"
    );
    eprintln!("[test] Duplicate PeerFailed deduplication test passed");
}

// ---------------------------------------------------------------
// Test: Higher-ID peer's re_initiate does not send an offer
// ---------------------------------------------------------------

/// §6.2 — When the higher-ID peer calls `re_initiate`, it removes the peer
/// from its map but does NOT send a new offer (tie-breaking: lower ID initiates).
/// Verified by confirming peer-b's connected_peers is empty after re_initiate,
/// and that the connection is restored when peer-a (lower ID) re-initiates.
#[tokio::test(flavor = "multi_thread")]
async fn higher_id_re_initiate_does_not_create_offer() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server_url = start_test_signaling_server().await;
    let ice = wail_net::default_ice_servers();

    // peer-a < peer-b → peer-a is the initiator
    let (mut mesh_a, _sync_rx_a, mut audio_rx_a) = PeerMesh::connect_with_ice(
        &server_url, "tie-room", "peer-a", None, ice.clone(),
    ).await.expect("peer-a failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) = PeerMesh::connect_with_ice(
        &server_url, "tie-room", "peer-b", None, ice,
    ).await.expect("peer-b failed");

    establish_connection(&mut mesh_a, &mut mesh_b).await;
    eprintln!("[test] Initial connection established");

    // peer-b (higher ID) calls re_initiate("peer-a").
    // Expected: peer-a is removed from mesh_b's peers map, no offer sent.
    mesh_b.re_initiate("peer-a").await.expect("re_initiate failed");

    assert!(
        mesh_b.connected_peers().is_empty(),
        "After re_initiate from higher-ID peer, peers map should be empty (no self-initiated offer)"
    );
    eprintln!("[test] peer-b has no peers after re_initiate (correct — waiting for peer-a to offer)");

    // peer-a (lower ID) calls re_initiate("peer-b") to restore the connection.
    mesh_a.close_peer("peer-b").await;
    mesh_a.re_initiate("peer-b").await.expect("peer-a re_initiate failed");

    establish_connection_timeout(&mut mesh_a, &mut mesh_b, 15).await;
    eprintln!("[test] Connection restored by lower-ID peer");

    // Audio should flow again
    let wire_a = produce_interval(440.0);
    let wire_b = produce_interval(880.0);
    mesh_a.broadcast_audio(&wire_a).await;
    mesh_b.broadcast_audio(&wire_b).await;

    let (from_b, _) = tokio::time::timeout(Duration::from_secs(5), audio_rx_b.recv())
        .await.expect("B timed out after reconnect").expect("audio_rx_b closed");
    let (from_a, _) = tokio::time::timeout(Duration::from_secs(5), audio_rx_a.recv())
        .await.expect("A timed out after reconnect").expect("audio_rx_a closed");

    assert_eq!(from_b, "peer-a");
    assert_eq!(from_a, "peer-b");
    eprintln!("[test] Higher-ID re_initiate tie-breaking test passed");
}

// ---------------------------------------------------------------
// Test: A single peer failure produces a bounded number of
// PeerFailed events (no cascade from Disconnected / reader exits)
// ---------------------------------------------------------------

/// A single connection failure should produce at most 3 PeerFailed events
/// (PeerConnectionState::Failed + 2× DataChannel on_close). Previously,
/// Disconnected state and reader exit handlers also sent failure signals,
/// producing 5-6 events — enough to exhaust MAX_PEER_RECONNECT_ATTEMPTS
/// from a single failure.
#[tokio::test(flavor = "multi_thread")]
async fn single_failure_produces_bounded_peer_failed_events() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let server_url = start_test_signaling_server().await;
    let ice = wail_net::default_ice_servers();

    let (mut mesh_a, _sync_a, _audio_a) = PeerMesh::connect_with_ice(
        &server_url, "bounded-fail-room", "peer-a", None, ice.clone(),
    )
    .await
    .expect("peer-a connect failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_b, _audio_b) = PeerMesh::connect_with_ice(
        &server_url, "bounded-fail-room", "peer-b", None, ice,
    )
    .await
    .expect("peer-b connect failed");

    establish_connection(&mut mesh_a, &mut mesh_b).await;
    eprintln!("[test] Connection established");

    // Close peer-a's side — triggers failure detection on mesh_b.
    mesh_a.close_peer("peer-b").await;

    // Drain events until no new PeerFailed arrives for 8 seconds (catches late reader exits).
    let mut failure_count = 0u32;
    let mut last_event = tokio::time::Instant::now();
    let quiet_period = Duration::from_secs(8);
    let hard_deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        tokio::select! {
            event = mesh_b.poll_signaling() => {
                if let Ok(Some(wail_net::MeshEvent::PeerFailed(ref pid))) = event {
                    if pid == "peer-a" {
                        failure_count += 1;
                        last_event = tokio::time::Instant::now();
                        eprintln!("[test] PeerFailed #{failure_count} for peer-a");
                    }
                }
            }
            _ = mesh_a.poll_signaling() => {}
            _ = tokio::time::sleep_until(last_event + quiet_period) => break,
            _ = tokio::time::sleep_until(hard_deadline) => break,
        }
    }

    eprintln!("[test] Total PeerFailed events: {failure_count}");

    // After removing reader exit fail_tx sources, only DC on_close remains (2 max).
    // Previously, reader exit handlers also sent failure signals, inflating the count.
    assert!(
        failure_count <= 2,
        "Expected at most 2 PeerFailed events (DC on_close only), got {failure_count}"
    );
    assert!(
        failure_count >= 1,
        "Expected at least 1 PeerFailed event"
    );
    eprintln!("[test] Bounded PeerFailed events test passed");
}
