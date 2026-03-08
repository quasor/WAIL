use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use clap::Parser;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{info, warn};
use uuid::Uuid;

use wail_audio::codec::{AudioDecoder, AudioEncoder};
use wail_audio::wire::AudioWire;
use wail_audio::AudioInterval;
use wail_core::protocol::SyncMessage;
use wail_net::{fetch_metered_ice_servers, metered_stun_fallback, MeshEvent, PeerMesh};

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

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 2;
const OPUS_BITRATE: u32 = 128;

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
    let global_timeout = Duration::from_secs(args.timeout);

    println!("=== WAIL E2E Test ===");
    println!("Room:       {room}");
    println!("Peer ID:    {peer_id}");
    println!("Server:     {}", args.server);
    println!("Intervals:  {}", args.intervals);
    println!("Timeout:    {global_timeout:.0?}");
    println!();

    match timeout(global_timeout, run_test(&args.server, &room, &peer_id, args.intervals)).await {
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

async fn run_test(server_url: &str, room: &str, peer_id: &str, num_intervals: u32) -> Result<()> {
    let mut results: Vec<TestResult> = Vec::new();

    // --- Phase 1: ICE servers ---
    let t = Instant::now();
    let ice_servers = match fetch_metered_ice_servers().await {
        Ok(servers) => {
            results.push(TestResult::pass(
                "ICE",
                format!("fetched {} servers from Metered", servers.len()),
                t.elapsed(),
            ));
            servers
        }
        Err(e) => {
            warn!("Metered API failed ({e}), using STUN fallback");
            results.push(TestResult::pass(
                "ICE",
                "Metered unreachable, using STUN fallback".into(),
                t.elapsed(),
            ));
            metered_stun_fallback()
        }
    };
    print_result(results.last().unwrap());

    // --- Phase 2: Signaling connection ---
    let t = Instant::now();
    let (mut mesh, mut sync_rx, mut audio_rx) = match timeout(
        Duration::from_secs(10),
        PeerMesh::connect_full(
            server_url,
            room,
            peer_id,
            None,
            ice_servers,
            false,
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

    // --- Phase 3: Peer discovery ---
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

    // --- Phase 4: Wait for DataChannels to open ---
    let t = Instant::now();
    wait_for_datachannel(&mut mesh, &remote_peer_id, Duration::from_secs(30)).await?;
    results.push(TestResult::pass("WebRTC", "DataChannels open".into(), t.elapsed()));
    print_result(results.last().unwrap());

    // --- Phase 5: Sync message exchange (Hello + Ping/Pong) ---
    let t = Instant::now();
    let rtt_ms = run_sync_exchange(&mut mesh, &mut sync_rx, peer_id).await?;
    results.push(TestResult::pass(
        "Sync",
        format!("Hello exchanged, RTT={rtt_ms:.1}ms"),
        t.elapsed(),
    ));
    print_result(results.last().unwrap());

    // --- Phase 6: Single audio interval exchange ---
    let t = Instant::now();
    let detail = run_single_audio_exchange(&mut mesh, &mut audio_rx).await?;
    results.push(TestResult::pass("Audio", detail, t.elapsed()));
    print_result(results.last().unwrap());

    // --- Phase 7: Sustained multi-interval audio ---
    let t = Instant::now();
    let detail = run_sustained_audio(
        &mut mesh, &mut audio_rx, &mut sync_rx, num_intervals,
    ).await?;
    results.push(TestResult::pass("Sustained", detail, t.elapsed()));
    print_result(results.last().unwrap());

    // --- Phase 8: Reconnection ---
    // Lower peer ID reconnects, higher peer ID waits.
    let we_reconnect = peer_id < remote_peer_id.as_str();
    let role = if we_reconnect { "reconnector" } else { "waiter" };
    println!("\nReconnection test: we are the {role} (us={peer_id}, them={remote_peer_id})");

    let t = Instant::now();
    let detail = if we_reconnect {
        run_reconnect_as_initiator(
            &mut mesh, &mut sync_rx, &mut audio_rx,
            server_url, room, peer_id, &remote_peer_id,
        ).await?
    } else {
        run_reconnect_as_waiter(
            &mut mesh, &mut sync_rx, &mut audio_rx,
            peer_id, &remote_peer_id,
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
            Ok(Ok(Some(MeshEvent::PeerListReceived(count)))) => {
                if count > 0 {
                    let peers = mesh.connected_peers();
                    if let Some(rid) = peers.into_iter().next() {
                        return Ok(rid);
                    }
                }
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) => bail!("Signaling channel closed"),
            Ok(Err(e)) => bail!("Signaling error: {e}"),
            Err(_) => {} // 1s poll timeout
        }
    }
}

async fn wait_for_datachannel(mesh: &mut PeerMesh, remote_peer_id: &str, dc_timeout: Duration) -> Result<()> {
    let result = timeout(dc_timeout, async {
        loop {
            if mesh.any_audio_dc_open() {
                return Ok::<(), anyhow::Error>(());
            }
            match timeout(Duration::from_secs(1), mesh.poll_signaling()).await {
                Ok(Ok(Some(_))) => {}
                Ok(Ok(None)) => bail!("Signaling closed during negotiation"),
                Ok(Err(e)) => bail!("Signaling error during negotiation: {e}"),
                Err(_) => {}
            }
        }
    })
    .await;

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => bail!("WebRTC negotiation failed: {e}"),
        Err(_) => {
            let state = mesh.peer_network_state(remote_peer_id);
            let msg = match state {
                Some((ice, sync_dc, audio_dc)) => {
                    format!("timeout ({dc_timeout:.0?}) -- ICE={ice}, sync_dc={sync_dc}, audio_dc={audio_dc}")
                }
                None => format!("timeout ({dc_timeout:.0?}) -- peer not in mesh"),
            };
            bail!("WebRTC negotiation failed: {msg}");
        }
    }
}

async fn run_sync_exchange(
    mesh: &mut PeerMesh,
    sync_rx: &mut mpsc::UnboundedReceiver<(String, SyncMessage)>,
    peer_id: &str,
) -> Result<f64> {
    // Send Hello
    mesh.broadcast(&SyncMessage::Hello {
        peer_id: peer_id.to_string(),
        display_name: Some("e2e-test".into()),
        identity: None,
    })
    .await;

    // Send Ping
    let ping_sent = now_us();
    mesh.broadcast(&SyncMessage::Ping { id: 1, sent_at_us: ping_sent }).await;

    let mut got_hello = false;
    let mut rtt_us: Option<i64> = None;

    let result = timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                Some((from, msg)) = sync_rx.recv() => {
                    match msg {
                        SyncMessage::Hello { .. } => { got_hello = true; }
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
        Ok(Ok(())) => Ok(rtt_us.unwrap_or(0) as f64 / 1000.0),
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
    let wire_bytes = encode_test_interval(0, 440.0)?;
    info!(bytes = wire_bytes.len(), "Sending test audio interval");
    mesh.broadcast_audio(&wire_bytes).await;

    let data = receive_audio(mesh, audio_rx, Duration::from_secs(15)).await?;
    validate_audio(&data)
}

/// Phase 7: Send N intervals back-to-back, receive N, measure throughput and gaps.
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
        let wire_bytes = encode_test_interval(i as i64, freq)?;
        total_bytes_sent += wire_bytes.len();
        mesh.broadcast_audio(&wire_bytes).await;
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

/// Phase 8 (initiator): We disconnect and reconnect. The remote peer waits.
async fn run_reconnect_as_initiator(
    mesh: &mut PeerMesh,
    sync_rx: &mut mpsc::UnboundedReceiver<(String, SyncMessage)>,
    audio_rx: &mut mpsc::Receiver<(String, Vec<u8>)>,
    server_url: &str,
    room: &str,
    peer_id: &str,
    remote_peer_id: &str,
) -> Result<String> {
    println!("Closing WebRTC connection...");

    // Close the WebRTC peer connection (but the remote peer stays in the room)
    mesh.close_peer(remote_peer_id).await;
    // Remove the dead peer from our mesh so re_initiate starts fresh
    mesh.remove_peer(remote_peer_id).await;

    // Brief pause for the close to propagate
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Drain stale events
    drain_events(mesh, sync_rx, audio_rx, Duration::from_millis(500)).await;

    println!("Reconnecting signaling...");

    // Fetch fresh ICE servers
    let fresh_ice = fetch_metered_ice_servers().await.unwrap_or_else(|_| metered_stun_fallback());

    // Reconnect signaling
    let reconnect_start = Instant::now();
    mesh.reconnect_signaling(
        server_url,
        room,
        None,
        Some("e2e-test"),
        fresh_ice,
    )
    .await?;
    let reconnect_elapsed = reconnect_start.elapsed();
    info!(elapsed = ?reconnect_elapsed, "Signaling reconnected");

    // The remote peer should still be in the room. re_initiate will create a new offer.
    println!("Re-initiating WebRTC to {remote_peer_id}...");
    mesh.re_initiate(remote_peer_id).await?;

    // Wait for DataChannels to reopen
    println!("Waiting for DataChannels to reopen...");
    let dc_start = Instant::now();
    wait_for_datachannel(mesh, remote_peer_id, Duration::from_secs(30)).await?;
    let dc_elapsed = dc_start.elapsed();
    info!(elapsed = ?dc_elapsed, "DataChannels reopened");

    // Verify sync
    println!("Verifying sync after reconnect...");
    let rtt_ms = run_sync_exchange(mesh, sync_rx, peer_id).await?;
    info!(rtt_ms, "Post-reconnect sync OK");

    // Verify audio
    println!("Verifying audio after reconnect...");
    let wire_bytes = encode_test_interval(99, 440.0)?;
    mesh.broadcast_audio(&wire_bytes).await;
    let data = receive_audio(mesh, audio_rx, Duration::from_secs(15)).await?;
    let audio_detail = validate_audio(&data)?;
    info!(detail = %audio_detail, "Post-reconnect audio OK");

    Ok(format!(
        "role=initiator, signaling={reconnect_elapsed:.1?}, datachannel={dc_elapsed:.1?}, \
         RTT={rtt_ms:.1}ms, audio={audio_detail}"
    ))
}

/// Phase 8 (waiter): The remote peer disconnects and reconnects. We stay and wait.
async fn run_reconnect_as_waiter(
    mesh: &mut PeerMesh,
    sync_rx: &mut mpsc::UnboundedReceiver<(String, SyncMessage)>,
    audio_rx: &mut mpsc::Receiver<(String, Vec<u8>)>,
    peer_id: &str,
    remote_peer_id: &str,
) -> Result<String> {
    println!("Waiting for remote peer to disconnect and reconnect...");

    // Wait for PeerFailed or PeerLeft (the initiator is closing their connection)
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
                        Some(MeshEvent::PeerFailed(ref pid)) if pid == remote_peer_id => {
                            info!("Remote peer connection failed (expected)");
                            return Ok::<&str, anyhow::Error>("PeerFailed");
                        }
                        Some(MeshEvent::PeerLeft(ref pid)) if pid == remote_peer_id => {
                            info!("Remote peer left (expected)");
                            return Ok("PeerLeft");
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

    // Clean up the dead peer connection
    mesh.remove_peer(remote_peer_id).await;

    // Now wait for the remote peer to rejoin and re-establish WebRTC
    println!("Waiting for remote peer to rejoin...");
    let rejoin_start = Instant::now();

    // Wait for PeerJoined (the initiator reconnects signaling and re-joins)
    let rejoin_result = timeout(Duration::from_secs(30), async {
        loop {
            match mesh.poll_signaling().await? {
                Some(MeshEvent::PeerJoined { peer_id: rid, .. }) if rid == remote_peer_id => {
                    info!("Remote peer rejoined");
                    return Ok::<(), anyhow::Error>(());
                }
                Some(MeshEvent::PeerListReceived(_)) => {
                    // After their reconnect_signaling, they get a peer list and may
                    // initiate a connection to us. Just keep polling.
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

    // Wait for DataChannels to reopen
    println!("Waiting for DataChannels to reopen...");
    let dc_start = Instant::now();
    wait_for_datachannel(mesh, remote_peer_id, Duration::from_secs(30)).await?;
    let dc_elapsed = dc_start.elapsed();
    info!(elapsed = ?dc_elapsed, "DataChannels reopened");

    // Verify sync
    println!("Verifying sync after reconnect...");
    let rtt_ms = run_sync_exchange(mesh, sync_rx, peer_id).await?;
    info!(rtt_ms, "Post-reconnect sync OK");

    // Verify audio
    println!("Verifying audio after reconnect...");
    let wire_bytes = encode_test_interval(99, 440.0)?;
    mesh.broadcast_audio(&wire_bytes).await;
    let data = receive_audio(mesh, audio_rx, Duration::from_secs(15)).await?;
    let audio_detail = validate_audio(&data)?;
    info!(detail = %audio_detail, "Post-reconnect audio OK");

    Ok(format!(
        "role=waiter, disconnect={disconnect_event}, rejoin={rejoin_elapsed:.1?}, \
         datachannel={dc_elapsed:.1?}, RTT={rtt_ms:.1}ms, audio={audio_detail}"
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

fn encode_test_interval(index: i64, freq: f32) -> Result<Vec<u8>> {
    let num_frames = 960u32; // 20ms at 48kHz
    let num_samples = num_frames as usize * CHANNELS as usize;
    let mut samples = vec![0.0f32; num_samples];
    for i in 0..num_frames as usize {
        let val = (2.0 * std::f32::consts::PI * freq * i as f32 / SAMPLE_RATE as f32).sin() * 0.5;
        samples[i * 2] = val;
        samples[i * 2 + 1] = val;
    }

    let mut encoder = AudioEncoder::new(SAMPLE_RATE, CHANNELS, OPUS_BITRATE)?;
    let opus_data = encoder.encode_interval(&samples)?;

    let interval = AudioInterval {
        index,
        stream_id: 0,
        opus_data,
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS,
        num_frames,
        bpm: 120.0,
        quantum: 4.0,
        bars: 4,
    };
    Ok(AudioWire::encode(&interval))
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

fn validate_audio(data: &[u8]) -> Result<String> {
    if data.len() >= 4 && &data[0..4] == b"WAIL" {
        let decoded = AudioWire::decode(data)?;
        if decoded.opus_data.is_empty() {
            bail!("opus_data is empty");
        }

        let mut decoder = AudioDecoder::new(decoded.sample_rate, decoded.channels)?;
        let pcm = decoder.decode_interval(&decoded.opus_data)?;
        let rms = rms(&pcm);

        if rms < 0.001 {
            bail!("decoded audio is silent (RMS={rms:.6})");
        }

        Ok(format!(
            "WAIL interval: {} bytes, {}/{} frames, RMS={rms:.4}, idx={}",
            data.len(),
            decoded.num_frames,
            decoded.channels,
            decoded.index,
        ))
    } else if data.len() >= 4 && &data[0..4] == b"WAIF" {
        let frame = wail_audio::AudioFrameWire::decode(data)?;
        Ok(format!(
            "WAIF frame: {} bytes, frame #{}, interval {}, final={}",
            data.len(),
            frame.frame_number,
            frame.interval_index,
            frame.is_final,
        ))
    } else {
        bail!(
            "unknown wire format: magic={:?}",
            &data[..data.len().min(4)]
        );
    }
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / samples.len() as f64).sqrt() as f32
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
