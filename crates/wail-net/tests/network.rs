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
        PeerMesh::connect_with_options(&server_url, "test-room", "peer-a", "test", ice.clone(), 200)
            .await
            .expect("Peer A failed to connect to signaling");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) =
        PeerMesh::connect_with_options(&server_url, "test-room", "peer-b", "test", ice, 200)
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
#[ignore] // Requires coturn installed: cargo test -p wail-net -- --ignored
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
        PeerMesh::connect_with_options(
            &server_url, "test-room", "peer-a", "test", ice_servers.clone(), 200,
        )
            .await
            .expect("Peer A failed to connect to signaling");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut mesh_b, _sync_rx_b, mut audio_rx_b) =
        PeerMesh::connect_with_options(
            &server_url, "test-room", "peer-b", "test", ice_servers, 200,
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
