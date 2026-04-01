use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use clap::Parser;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{info, warn};
use uuid::Uuid;

use wail_audio::test_tone::{encode_test_interval, validate_audio};
use wail_core::protocol::SyncMessage;
use wail_net::{MeshEvent, PeerMesh};

#[derive(Parser)]
#[command(name = "wail-e2e", about = "Two-machine end-to-end test for WAIL")]
struct Args {
    /// Signaling server URL
    #[arg(long, default_value = "wss://wail-signal.fly.dev")]
    server: String,

    /// Room name (both machines must use the same room)
    #[arg(long)]
    room: Option<String>,

    /// Max seconds to wait for the full test
    #[arg(long, default_value = "180")]
    timeout: u64,

    /// Number of intervals for the sustained audio test
    #[arg(long, default_value = "10")]
    intervals: u32,

    /// Number of intervals for the burst (zero-delay) audio overflow test
    #[arg(long, default_value = "32")]
    burst_intervals: u32,

    /// Enable debug-level tracing
    #[arg(long)]
    verbose: bool,
}

struct TestResult {
    phase: &'static str,
    passed: bool,
    message: String,
    duration: Duration,
}

impl TestResult {
    fn pass(phase: &'static str, message: String, duration: Duration) -> Self {
        Self { phase, passed: true, message, duration }
    }
    fn fail(phase: &'static str, message: String, duration: Duration) -> Self {
        Self { phase, passed: false, message, duration }
    }
}

fn print_result(r: &TestResult) {
    let tag = if r.passed { "PASS" } else { "FAIL" };
    println!("[{tag}] {}: {} ({:.1?})", r.phase, r.message, r.duration);
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = if args.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let room = args.room.unwrap_or_else(|| format!("e2e-{}", &Uuid::new_v4().to_string()[..8]));
    let peer_id = format!("e2e-{}", &Uuid::new_v4().to_string()[..8]);
    let identity = Uuid::new_v4().to_string();
    let global_timeout = Duration::from_secs(args.timeout);

    println!("=== WAIL E2E Test ===");
    println!("Room:       {room}");
    println!("Peer ID:    {peer_id}");
    println!("Identity:   {identity}");
    println!("Server:     {}", args.server);
    println!("Intervals:  {}", args.intervals);
    println!("Burst:      {}", args.burst_intervals);
    println!("Timeout:    {global_timeout:.0?}");
    println!();

    match timeout(global_timeout, run_test(&args.server, &room, &peer_id, &identity, args.intervals, args.burst_intervals)).await {
        Ok(Ok(())) => {
            println!("\n=== ALL TESTS PASSED ===");
            Ok(())
        }
        Ok(Err(e)) => {
            println!("\n=== TEST FAILED: {e} ===");
            std::process::exit(1);
        }
        Err(_) => {
            println!("\n=== TEST TIMED OUT after {global_timeout:.0?} ===");
            std::process::exit(1);
        }
    }
}

async fn run_test(server_url: &str, room: &str, peer_id: &str, identity: &str, num_intervals: u32, burst_intervals: u32) -> Result<()> {
    let mut results: Vec<TestResult> = Vec::new();

    // --- Phase 1: Signaling connection ---
    let t = Instant::now();
    let (mut mesh, mut sync_rx, mut audio_rx) = match timeout(
        Duration::from_secs(10),
        PeerMesh::connect_full(
            server_url,
            room,
            peer_id,
            None,
            1,
            Some("e2e-test"),
        ),
    )
    .await
    {
        Ok(Ok(v)) => {
            results.push(TestResult::pass(
                "Signaling",
                format!("connected to {server_url}"),
                t.elapsed(),
            ));
            v
        }
        Ok(Err(e)) => {
            results.push(TestResult::fail("Signaling", format!("{e}"), t.elapsed()));
            print_result(results.last().unwrap());
            bail!("Signaling connection failed: {e}");
        }
        Err(_) => {
            results.push(TestResult::fail("Signaling", "timeout (10s)".into(), t.elapsed()));
            print_result(results.last().unwrap());
            bail!("Signaling connection timed out");
        }
    };
    print_result(results.last().unwrap());

    // --- Phase 2: Peer discovery ---
    println!("\nWaiting for peer to join room \"{room}\"...");
    println!("  Run on the other machine:");
    println!("  cargo run -p wail-e2e --release -- --room {room} --server {server_url}");
    println!();

    let t = Instant::now();
    let remote_peer_id = wait_for_peer(&mut mesh).await?;
    results.push(TestResult::pass(
        "Discovery",
        format!("peer {remote_peer_id} found"),
        t.elapsed(),
    ));
    print_result(results.last().unwrap());

    // --- Phase 3: Sync message exchange (Hello + Ping/Pong) ---
    let t = Instant::now();
    let rtt_ms = run_sync_exchange(&mut mesh, &mut sync_rx, peer_id, identity).await?;
    results.push(TestResult::pass(
        "Sync",
        format!("Hello exchanged, RTT={rtt_ms:.1}ms"),
        t.elapsed(),
    ));
    print_result(results.last().unwrap());

    // --- Phase 4: Single audio interval exchange ---
    let t = Instant::now();
    let detail = run_single_audio_exchange(&mut mesh, &mut audio_rx).await?;
    results.push(TestResult::pass("Audio", detail, t.elapsed()));
    print_result(results.last().unwrap());

    // --- Phase 5: Sustained multi-interval audio ---
    let t = Instant::now();
    let detail = run_sustained_audio(
        &mut mesh, &mut audio_rx, &mut sync_rx, num_intervals,
    ).await?;
    results.push(TestResult::pass("Sustained", detail, t.elapsed()));
    print_result(results.last().unwrap());

    // --- Phase 6: Burst audio (zero-delay flood to validate buffer headroom) ---
    let t = Instant::now();
    let detail = run_burst_audio(
        &mut mesh, &mut audio_rx, &mut sync_rx, burst_intervals,
    ).await?;
    results.push(TestResult::pass("Burst", detail, t.elapsed()));
    print_result(results.last().unwrap());

    // --- Phase 7: Reconnection ---
    let we_reconnect = peer_id < remote_peer_id.as_str();
    let role = if we_reconnect { "reconnector" } else { "waiter" };
    println!("\nReconnection test: we are the {role} (us={peer_id}, them={remote_peer_id})");

    let t = Instant::now();
    let detail = if we_reconnect {
        run_reconnect_as_initiator(
            &mut mesh, &mut sync_rx, &mut audio_rx,
            server_url, room, peer_id, identity, &remote_peer_id,
        ).await?
    } else {
        run_reconnect_as_waiter(
            &mut mesh, &mut sync_rx, &mut audio_rx,
            peer_id, identity, &remote_peer_id,
        ).await?
    };
    results.push(TestResult::pass("Reconnect", detail, t.elapsed()));
    print_result(results.last().unwrap());

    // --- Summary ---
    println!("\n--- Summary ---");
    for r in &results {
        print_result(r);
    }

    let all_passed = results.iter().all(|r| r.passed);
    if !all_passed {
        bail!("Some tests failed");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase helpers
// ---------------------------------------------------------------------------

async fn wait_for_peer(mesh: &mut PeerMesh) -> Result<String> {
    loop {
        match timeout(Duration::from_secs(1), mesh.poll_signaling()).await {
            Ok(Ok(Some(MeshEvent::PeerJoined { peer_id: rid, display_name }))) => {
                info!(peer = %rid, name = ?display_name, "Peer joined");
                return Ok(rid);
            }
            Ok(Ok(Some(MeshEvent::PeerListReceived(_)))) => {
                // After join, check if any peers were already in the room
                if let Some(rid) = mesh.connected_peers().into_iter().next() {
                    return Ok(rid);
                }
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) => bail!("Signaling channel closed"),
            Ok(Err(e)) => bail!("Signaling error: {e}"),
            Err(_) => {} // 1s poll timeout
        }
    }
}

async fn run_sync_exchange(
    mesh: &mut PeerMesh,
    sync_rx: &mut mpsc::UnboundedReceiver<(String, SyncMessage)>,
    peer_id: &str,
    identity: &str,
) -> Result<f64> {
    // Send Hello with a real identity (same as Tauri app does)
    mesh.broadcast(&SyncMessage::Hello {
        peer_id: peer_id.to_string(),
        display_name: Some("e2e-test".into()),
        identity: Some(identity.to_string()),
    })
    .await;

    // Send Ping
    let ping_sent = now_us();
    mesh.broadcast(&SyncMessage::Ping { id: 1, sent_at_us: ping_sent }).await;

    let mut got_hello = false;
    let mut hello_had_identity = false;
    let mut rtt_us: Option<i64> = None;

    let result = timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                Some((from, msg)) = sync_rx.recv() => {
                    match msg {
                        SyncMessage::Hello { identity: ref id, .. } => {
                            got_hello = true;
                            hello_had_identity = id.is_some();
                        }
                        SyncMessage::Ping { id, sent_at_us } => {
                            mesh.send_to(&from, &SyncMessage::Pong {
                                id,
                                ping_sent_at_us: sent_at_us,
                                pong_sent_at_us: now_us(),
                            }).await.ok();
                        }
                        SyncMessage::Pong { ping_sent_at_us, .. } => {
                            rtt_us = Some(now_us() - ping_sent_at_us);
                            info!(rtt_us = rtt_us.unwrap(), "Got Pong");
                        }
                        _ => {}
                    }
                    if got_hello && rtt_us.is_some() {
                        return Ok::<(), anyhow::Error>(());
                    }
                }
                result = mesh.poll_signaling() => { result?; }
            }
        }
    })
    .await;

    match result {
        Ok(Ok(())) => {
            if !hello_had_identity {
                warn!("Remote Hello arrived without identity — slot assignment will be skipped on Tauri side");
            }
            Ok(rtt_us.unwrap_or(0) as f64 / 1000.0)
        }
        Ok(Err(e)) => bail!("Sync exchange failed: {e}"),
        Err(_) => {
            let detail = if !got_hello { "no Hello received" } else { "Hello OK but no Pong" };
            bail!("Sync exchange timed out: {detail}");
        }
    }
}

async fn run_single_audio_exchange(
    mesh: &mut PeerMesh,
    audio_rx: &mut mpsc::Receiver<(String, Vec<u8>)>,
) -> Result<String> {
    let waif_frames = encode_test_interval(0, 440.0, 120.0, 4, 4.0)?;
    info!(frames = waif_frames.len(), "Sending test audio interval");
    for frame in &waif_frames {
        mesh.broadcast_audio(frame).await;
    }

    let data = receive_audio(mesh, audio_rx, Duration::from_secs(15)).await?;
    validate_audio_str(&data)
}

/// Send N intervals back-to-back, receive N, measure throughput and gaps.
async fn run_sustained_audio(
    mesh: &mut PeerMesh,
    audio_rx: &mut mpsc::Receiver<(String, Vec<u8>)>,
    sync_rx: &mut mpsc::UnboundedReceiver<(String, SyncMessage)>,
    num_intervals: u32,
) -> Result<String> {
    println!("\nSustained audio: sending {num_intervals} intervals...");

    let send_start = Instant::now();
    let mut total_bytes_sent: usize = 0;

    for i in 0..num_intervals {
        let freq = if i % 2 == 0 { 440.0 } else { 880.0 };
        let waif_frames = encode_test_interval(i as i64, freq, 120.0, 4, 4.0)?;
        for frame in &waif_frames {
            total_bytes_sent += frame.len();
            mesh.broadcast_audio(frame).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let send_elapsed = send_start.elapsed();
    info!(
        intervals = num_intervals,
        bytes = total_bytes_sent,
        elapsed = ?send_elapsed,
        "All intervals sent"
    );

    // Receive intervals
    let recv_start = Instant::now();
    let mut received: u32 = 0;
    let mut total_bytes_recv: usize = 0;
    let mut recv_times: Vec<Duration> = Vec::new();
    let mut first_recv: Option<Instant> = None;

    let recv_timeout = Duration::from_secs(30);
    let result = timeout(recv_timeout, async {
        while received < num_intervals {
            tokio::select! {
                Some((_from, data)) = audio_rx.recv() => {
                    let now = Instant::now();
                    if first_recv.is_none() {
                        first_recv = Some(now);
                    }
                    recv_times.push(now.duration_since(first_recv.unwrap()));
                    total_bytes_recv += data.len();
                    received += 1;

                    if received % 5 == 0 || received == num_intervals {
                        info!(received, total = num_intervals, "Receiving intervals...");
                    }
                }
                Some((from, msg)) = sync_rx.recv() => {
                    handle_sync_passthrough(mesh, &from, &msg).await;
                }
                result = mesh.poll_signaling() => { result?; }
            }
        }
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => bail!("Sustained audio failed: {e}"),
        Err(_) => bail!(
            "Sustained audio timed out: received {received}/{num_intervals} intervals in {recv_timeout:.0?}"
        ),
    }

    let recv_elapsed = recv_start.elapsed();
    let total_duration = recv_times.last().copied().unwrap_or(Duration::ZERO);

    // Compute inter-arrival gaps
    let mut max_gap = Duration::ZERO;
    let mut avg_gap = Duration::ZERO;
    if recv_times.len() > 1 {
        let mut gaps: Vec<Duration> = Vec::new();
        for w in recv_times.windows(2) {
            let gap = w[1] - w[0];
            gaps.push(gap);
            if gap > max_gap {
                max_gap = gap;
            }
        }
        let total_gap: Duration = gaps.iter().sum();
        avg_gap = total_gap / gaps.len() as u32;
    }

    let throughput_kbps = if total_duration.as_secs_f64() > 0.0 {
        (total_bytes_recv as f64 * 8.0 / 1000.0) / total_duration.as_secs_f64()
    } else {
        0.0
    };

    Ok(format!(
        "{received}/{num_intervals} intervals, {total_bytes_recv} bytes, \
         {throughput_kbps:.1} kbps, avg_gap={avg_gap:.1?}, max_gap={max_gap:.1?}, \
         send={send_elapsed:.1?}, recv={recv_elapsed:.1?}"
    ))
}

/// Send N intervals with no delay between sends, validate all arrive.
async fn run_burst_audio(
    mesh: &mut PeerMesh,
    audio_rx: &mut mpsc::Receiver<(String, Vec<u8>)>,
    sync_rx: &mut mpsc::UnboundedReceiver<(String, SyncMessage)>,
    num_intervals: u32,
) -> Result<String> {
    println!("\nBurst audio: sending {num_intervals} intervals with no delay...");

    let send_start = Instant::now();
    let mut total_bytes_sent: usize = 0;

    for i in 0..num_intervals {
        let freq = if i % 2 == 0 { 440.0 } else { 880.0 };
        let waif_frames = encode_test_interval(1000 + i as i64, freq, 120.0, 4, 4.0)?;
        for frame in &waif_frames {
            total_bytes_sent += frame.len();
            mesh.broadcast_audio(frame).await;
        }
        tokio::task::yield_now().await;
    }

    let send_elapsed = send_start.elapsed();
    info!(
        intervals = num_intervals,
        bytes = total_bytes_sent,
        elapsed = ?send_elapsed,
        "Burst send complete"
    );

    let recv_start = Instant::now();
    let mut received: u32 = 0;
    let mut total_bytes_recv: usize = 0;

    let recv_timeout = Duration::from_secs(30);
    let result = timeout(recv_timeout, async {
        while received < num_intervals {
            tokio::select! {
                Some((_from, data)) = audio_rx.recv() => {
                    total_bytes_recv += data.len();
                    received += 1;
                }
                Some((from, msg)) = sync_rx.recv() => {
                    handle_sync_passthrough(mesh, &from, &msg).await;
                }
                result = mesh.poll_signaling() => { result?; }
            }
        }
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => bail!("Burst audio failed: {e}"),
        Err(_) => bail!(
            "Burst audio timed out: received {received}/{num_intervals} in {recv_timeout:.0?}"
        ),
    }

    let recv_elapsed = recv_start.elapsed();
    let throughput_kbps = if recv_elapsed.as_secs_f64() > 0.0 {
        (total_bytes_recv as f64 * 8.0 / 1000.0) / recv_elapsed.as_secs_f64()
    } else {
        0.0
    };

    Ok(format!(
        "{received}/{num_intervals} intervals, {total_bytes_recv} bytes, \
         {throughput_kbps:.1} kbps, send={send_elapsed:.1?}, recv={recv_elapsed:.1?}"
    ))
}

/// Phase 7 (initiator): We disconnect and reconnect signaling. The remote peer waits.
async fn run_reconnect_as_initiator(
    mesh: &mut PeerMesh,
    sync_rx: &mut mpsc::UnboundedReceiver<(String, SyncMessage)>,
    audio_rx: &mut mpsc::Receiver<(String, Vec<u8>)>,
    server_url: &str,
    room: &str,
    peer_id: &str,
    identity: &str,
    _remote_peer_id: &str,
) -> Result<String> {
    println!("Reconnecting signaling...");

    let reconnect_start = Instant::now();
    let (_new_peer_names, new_sync_rx, new_audio_rx) =
        mesh.reconnect_signaling(server_url, room, None, Some("e2e-test")).await?;
    *sync_rx = new_sync_rx;
    *audio_rx = new_audio_rx;
    let reconnect_elapsed = reconnect_start.elapsed();
    info!(elapsed = ?reconnect_elapsed, "Signaling reconnected");

    // Drain stale events
    drain_events(mesh, sync_rx, audio_rx, Duration::from_millis(500)).await;

    // Verify sync
    println!("Verifying sync after reconnect...");
    let rtt_ms = run_sync_exchange(mesh, sync_rx, peer_id, identity).await?;
    info!(rtt_ms, "Post-reconnect sync OK");

    // Verify audio
    println!("Verifying audio after reconnect...");
    let waif_frames = encode_test_interval(99, 440.0, 120.0, 4, 4.0)?;
    for frame in &waif_frames {
        mesh.broadcast_audio(frame).await;
    }
    let data = receive_audio(mesh, audio_rx, Duration::from_secs(15)).await?;
    let audio_detail = validate_audio_str(&data)?;
    info!(detail = %audio_detail, "Post-reconnect audio OK");

    Ok(format!(
        "role=initiator, signaling={reconnect_elapsed:.1?}, RTT={rtt_ms:.1}ms, audio={audio_detail}"
    ))
}

/// Phase 7 (waiter): The remote peer disconnects and reconnects. We stay and wait.
async fn run_reconnect_as_waiter(
    mesh: &mut PeerMesh,
    sync_rx: &mut mpsc::UnboundedReceiver<(String, SyncMessage)>,
    audio_rx: &mut mpsc::Receiver<(String, Vec<u8>)>,
    peer_id: &str,
    identity: &str,
    remote_peer_id: &str,
) -> Result<String> {
    println!("Waiting for remote peer to disconnect and reconnect...");

    // Wait for PeerLeft (the initiator reconnects signaling, server sends PeerLeft then PeerJoined)
    let disconnect_start = Instant::now();
    let got_disconnect = timeout(Duration::from_secs(15), async {
        loop {
            tokio::select! {
                Some((from, msg)) = sync_rx.recv() => {
                    handle_sync_passthrough(mesh, &from, &msg).await;
                }
                _ = audio_rx.recv() => {}
                result = mesh.poll_signaling() => {
                    match result? {
                        Some(MeshEvent::PeerLeft(ref pid)) if pid == remote_peer_id => {
                            info!("Remote peer left (expected — reconnecting)");
                            return Ok::<&str, anyhow::Error>("PeerLeft");
                        }
                        _ => {}
                    }
                }
            }
        }
    })
    .await;

    let disconnect_event = match got_disconnect {
        Ok(Ok(event)) => {
            info!(event, elapsed = ?disconnect_start.elapsed(), "Got disconnect signal");
            event.to_string()
        }
        Ok(Err(e)) => bail!("Error waiting for disconnect: {e}"),
        Err(_) => bail!("Timed out waiting for remote peer to disconnect"),
    };

    // Now wait for the remote peer to rejoin
    println!("Waiting for remote peer to rejoin...");
    let rejoin_start = Instant::now();

    let rejoin_result = timeout(Duration::from_secs(30), async {
        loop {
            match mesh.poll_signaling().await? {
                Some(MeshEvent::PeerJoined { peer_id: rid, .. }) if rid == remote_peer_id => {
                    info!("Remote peer rejoined");
                    return Ok::<(), anyhow::Error>(());
                }
                _ => {}
            }
        }
    })
    .await;

    match rejoin_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => bail!("Error waiting for rejoin: {e}"),
        Err(_) => bail!("Timed out waiting for remote peer to rejoin"),
    }
    let rejoin_elapsed = rejoin_start.elapsed();

    // Verify sync
    println!("Verifying sync after reconnect...");
    let rtt_ms = run_sync_exchange(mesh, sync_rx, peer_id, identity).await?;
    info!(rtt_ms, "Post-reconnect sync OK");

    // Verify audio
    println!("Verifying audio after reconnect...");
    let waif_frames = encode_test_interval(99, 440.0, 120.0, 4, 4.0)?;
    for frame in &waif_frames {
        mesh.broadcast_audio(frame).await;
    }
    let data = receive_audio(mesh, audio_rx, Duration::from_secs(15)).await?;
    let audio_detail = validate_audio_str(&data)?;
    info!(detail = %audio_detail, "Post-reconnect audio OK");

    Ok(format!(
        "role=waiter, disconnect={disconnect_event}, rejoin={rejoin_elapsed:.1?}, \
         RTT={rtt_ms:.1}ms, audio={audio_detail}"
    ))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64
}

async fn receive_audio(
    mesh: &mut PeerMesh,
    audio_rx: &mut mpsc::Receiver<(String, Vec<u8>)>,
    wait: Duration,
) -> Result<Vec<u8>> {
    let result = timeout(wait, async {
        loop {
            tokio::select! {
                Some((from, data)) = audio_rx.recv() => {
                    info!(peer = %from, bytes = data.len(), "Received audio data");
                    return Ok::<Vec<u8>, anyhow::Error>(data);
                }
                result = mesh.poll_signaling() => { result?; }
            }
        }
    })
    .await;

    match result {
        Ok(Ok(data)) => Ok(data),
        Ok(Err(e)) => bail!("Audio receive error: {e}"),
        Err(_) => bail!("Audio receive timed out ({wait:.0?})"),
    }
}

/// Validate audio and return a detail string, failing on silence.
fn validate_audio_str(data: &[u8]) -> Result<String> {
    let v = validate_audio(data)?;
    if v.rms < 0.001 && v.format == "WAIL" {
        bail!("decoded audio is silent (RMS={:.6})", v.rms);
    }
    Ok(v.detail)
}

async fn handle_sync_passthrough(mesh: &PeerMesh, from: &str, msg: &SyncMessage) {
    if let SyncMessage::Ping { id, sent_at_us } = msg {
        mesh.send_to(from, &SyncMessage::Pong {
            id: *id,
            ping_sent_at_us: *sent_at_us,
            pong_sent_at_us: now_us(),
        })
        .await
        .ok();
    }
}

async fn drain_events(
    mesh: &mut PeerMesh,
    sync_rx: &mut mpsc::UnboundedReceiver<(String, SyncMessage)>,
    audio_rx: &mut mpsc::Receiver<(String, Vec<u8>)>,
    duration: Duration,
) {
    let _ = timeout(duration, async {
        loop {
            tokio::select! {
                _ = sync_rx.recv() => {}
                _ = audio_rx.recv() => {}
                _ = mesh.poll_signaling() => {}
            }
        }
    })
    .await;
}
