use std::collections::HashSet;
use std::f64::consts::TAU;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use clap::Parser;
use rusty_link::{AblLink, SessionState};
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};
use uuid::Uuid;

use wail_audio::codec::AudioEncoder;
use wail_audio::wire::AudioFrameWire;
use wail_audio::AudioFrame;
use wail_core::protocol::SyncMessage;
use wail_net::{fetch_metered_ice_servers, metered_stun_fallback, MeshEvent, PeerMesh};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 2;
const OPUS_BITRATE_KBPS: u32 = 128;
const FRAME_DURATION_MS: u64 = 20;
/// Samples per channel per 20ms Opus frame at 48 kHz.
const FRAME_SIZE: usize = 960;

/// A minor pentatonic scale rooted at A3 (220 Hz).
const SCALE: [f32; 5] = [
    220.00, // A3
    261.63, // C4
    293.66, // D4
    329.63, // E4
    392.00, // G4
];

const NOTE_NAMES: [&str; 5] = ["A3", "C4", "D4", "E4", "G4"];

#[derive(Parser)]
#[command(
    name = "wail-test-client",
    about = "Standalone audio test client — streams a pentatonic minor sine scale into a WAIL room"
)]
struct Args {
    /// Signaling server URL
    #[arg(long, default_value = "wss://wail-signal.fly.dev")]
    server: String,

    /// Room name to join
    #[arg(long, default_value = "test-tone")]
    room: String,

    /// Room password (optional)
    #[arg(long)]
    password: Option<String>,

    /// BPM (beats per minute)
    #[arg(long, default_value = "120")]
    bpm: f64,

    /// Bars per interval
    #[arg(long, default_value = "4")]
    bars: u32,

    /// Quantum (beats per bar)
    #[arg(long, default_value = "4.0")]
    quantum: f64,

    /// Display name shown to other peers
    #[arg(long, default_value = "test-tone")]
    name: String,

    /// Sine wave amplitude (0.0–1.0)
    #[arg(long, default_value = "0.5")]
    amplitude: f32,

    /// Send a constant 440 Hz tone (no scale changes) to isolate crossfade artifacts
    #[arg(long)]
    constant: bool,

    /// Echo received audio back on stream 1 (disabled by default to avoid backpressure)
    #[arg(long)]
    echo: bool,

    /// Force relay-only (TURN) mode
    #[arg(long)]
    relay_only: bool,

    /// Enable debug-level logging
    #[arg(long)]
    verbose: bool,
}

/// Generate one 20ms stereo-interleaved sine frame, advancing `phase` continuously.
fn generate_sine_frame(freq: f32, phase: &mut f64, amplitude: f32) -> Vec<f32> {
    let mut samples = vec![0.0f32; FRAME_SIZE * CHANNELS as usize];
    let phase_inc = freq as f64 * TAU / SAMPLE_RATE as f64;
    for i in 0..FRAME_SIZE {
        let val = (*phase).sin() as f32 * amplitude;
        samples[i * 2] = val;
        samples[i * 2 + 1] = val;
        *phase += phase_inc;
    }
    // Keep phase bounded to avoid f64 precision loss over long runs.
    *phase %= TAU;
    samples
}

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64
}

/// Derive interval index from Link beat position.
fn beat_to_interval(beat: f64, bars: u32, quantum: f64) -> i64 {
    let beats_per_interval = bars as f64 * quantum;
    (beat / beats_per_interval).floor() as i64
}

/// Derive which bar within the current interval from Link beat position.
fn beat_to_bar_in_interval(beat: f64, bars: u32, quantum: f64) -> u32 {
    let beats_per_interval = bars as f64 * quantum;
    let beat_in_interval = beat - (beat / beats_per_interval).floor() * beats_per_interval;
    (beat_in_interval / quantum).floor() as u32
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = if args.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let peer_id = format!("tone-{}", &Uuid::new_v4().to_string()[..8]);
    let identity = Uuid::new_v4().to_string();

    println!("=== WAIL Test Client ===");
    println!("Room:       {}", args.room);
    println!("Peer ID:    {peer_id}");
    println!("Server:     {}", args.server);
    println!("BPM:        {}", args.bpm);
    println!("Bars:       {} (quantum {})", args.bars, args.quantum);
    println!(
        "Scale:      {} {} {} {} {} (A minor pentatonic)",
        NOTE_NAMES[0], NOTE_NAMES[1], NOTE_NAMES[2], NOTE_NAMES[3], NOTE_NAMES[4]
    );
    println!("Amplitude:  {}", args.amplitude);
    if args.echo {
        println!("Streams:    0 = tone, 1 = echo (re-sends received audio)");
    } else {
        println!("Streams:    0 = tone (echo disabled, use --echo to enable)");
    }
    println!("Link:       enabled (intervals sync with DAW)");
    println!();

    // --- Ableton Link ---
    let link = AblLink::new(args.bpm);
    link.enable(true);
    let mut session_state = SessionState::new();
    println!("Ableton Link enabled at {} BPM", args.bpm);

    // --- ICE servers ---
    let ice_servers = match fetch_metered_ice_servers().await {
        Ok(servers) => {
            info!(count = servers.len(), "Fetched ICE servers from Metered");
            servers
        }
        Err(e) => {
            warn!("Metered API failed ({e}), using STUN fallback");
            metered_stun_fallback()
        }
    };

    // --- Connect to signaling + WebRTC ---
    let password = args.password.as_deref();
    let (mut mesh, mut sync_rx, mut audio_rx) = PeerMesh::connect_full(
        &args.server,
        &args.room,
        &peer_id,
        password,
        ice_servers,
        args.relay_only,
        if args.echo { 2 } else { 1 }, // stream 0 = tone, stream 1 = echo (if enabled)
        Some(&args.name),
    )
    .await?;
    println!("Connected to signaling server.");

    // --- Wait for at least one peer ---
    println!("Waiting for a peer to join room \"{}\"...", args.room);
    let remote_peer_id = wait_for_peer(&mut mesh).await?;
    println!("Peer joined: {remote_peer_id}");

    // --- Wait for DataChannels to open ---
    wait_for_datachannel(&mut mesh, Duration::from_secs(30)).await?;
    println!("DataChannels open. Starting audio stream.");

    // --- Send Hello ---
    mesh.broadcast(&SyncMessage::Hello {
        peer_id: peer_id.clone(),
        display_name: Some(args.name.clone()),
        identity: Some(identity.clone()),
    })
    .await;

    // --- Opus encoder ---
    let mut encoder = AudioEncoder::new(SAMPLE_RATE, CHANNELS, OPUS_BITRATE_KBPS)?;

    // --- Tone state ---
    let mut phase: f64 = 0.0;
    let amplitude = args.amplitude.clamp(0.0, 1.0);
    let mut intervals_sent: u64 = 0;
    let mut echo_frames: u64 = 0;
    let start_time = Instant::now();

    // Track which peers we've already greeted to avoid Hello ping-pong storms.
    let mut greeted_peers: HashSet<String> = HashSet::new();

    // Track interval/bar transitions from Link beat position.
    let mut prev_interval: Option<i64> = None;
    let mut prev_bar: Option<u32> = None;
    let mut frame_in_interval: u32 = 0;

    // --- 20ms frame timer ---
    let mut frame_timer = tokio::time::interval(Duration::from_millis(FRAME_DURATION_MS));
    frame_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = frame_timer.tick() => {
                // Read current beat position from Link.
                let time = link.clock_micros();
                link.capture_app_session_state(&mut session_state);
                let beat = session_state.beat_at_time(time, args.quantum);
                let bpm = session_state.tempo();

                let interval_index = beat_to_interval(beat, args.bars, args.quantum);
                let bar_in_interval = beat_to_bar_in_interval(beat, args.bars, args.quantum);

                // Detect interval boundary.
                let is_new_interval = match prev_interval {
                    Some(prev) => prev != interval_index,
                    None => true,
                };

                if is_new_interval {
                    if let Some(_prev_idx) = prev_interval {
                        intervals_sent += 1;
                        let elapsed = start_time.elapsed();
                        println!(
                            "Interval {} complete ({frame_in_interval} frames, {intervals_sent} total, {elapsed:.0?} elapsed)",
                            _prev_idx,
                        );

                        // Broadcast interval boundary sync.
                        mesh.broadcast(&SyncMessage::IntervalBoundary { index: interval_index }).await;
                    }
                    frame_in_interval = 0;
                    prev_interval = Some(interval_index);
                    prev_bar = None;

                    // Log the new interval's first note.
                    let note_idx = (bar_in_interval as usize) % 5;
                    println!(
                        "Bar 1: {} ({:.0} Hz)  [interval {interval_index}, beat {beat:.1}, {bpm:.1} BPM, Link peers: {}]",
                        NOTE_NAMES[note_idx],
                        SCALE[note_idx],
                        link.num_peers(),
                    );
                }

                // Detect bar boundary within interval.
                let is_new_bar = match prev_bar {
                    Some(prev) => prev != bar_in_interval,
                    None => false, // already logged on interval start
                };
                if is_new_bar {
                    prev_bar = Some(bar_in_interval);
                    let global_bar = interval_index as u64 * args.bars as u64 + bar_in_interval as u64;
                    let note_idx = (global_bar % 5) as usize;
                    println!(
                        "Bar {}: {} ({:.0} Hz)  [interval {interval_index}]",
                        bar_in_interval + 1,
                        NOTE_NAMES[note_idx],
                        SCALE[note_idx],
                    );
                }
                if prev_bar.is_none() {
                    prev_bar = Some(bar_in_interval);
                }

                // Determine current note.
                let global_bar = interval_index as u64 * args.bars as u64 + bar_in_interval as u64;
                let note_idx = (global_bar % 5) as usize;
                let freq = if args.constant { 440.0 } else { SCALE[note_idx] };

                // Generate PCM and encode to Opus.
                let pcm = generate_sine_frame(freq, &mut phase, amplitude);
                let opus_data = encoder.encode_frame(&pcm)?;

                // Compute frames_per_interval for the is_final header.
                let beat_duration_s = 60.0 / bpm;
                let bar_duration_ms = beat_duration_s * args.quantum * 1000.0;
                let frames_per_bar = (bar_duration_ms / FRAME_DURATION_MS as f64).round() as u32;
                let frames_per_interval = frames_per_bar * args.bars;

                let is_final = frame_in_interval == frames_per_interval.saturating_sub(1);
                let frame = AudioFrame {
                    interval_index,
                    stream_id: 0,
                    frame_number: frame_in_interval,
                    channels: CHANNELS,
                    opus_data,
                    is_final,
                    sample_rate: if is_final { SAMPLE_RATE } else { 0 },
                    total_frames: if is_final { frames_per_interval } else { 0 },
                    bpm: if is_final { bpm } else { 0.0 },
                    quantum: if is_final { args.quantum } else { 0.0 },
                    bars: if is_final { args.bars } else { 0 },
                };

                let wire_bytes = AudioFrameWire::encode(&frame);
                mesh.broadcast_audio(&wire_bytes).await;

                frame_in_interval += 1;
            }

            Some((from, msg)) = sync_rx.recv() => {
                handle_sync(&mesh, &from, &msg, &peer_id, &identity, &args.name, &mut greeted_peers).await;
            }

            Some((from, data)) = audio_rx.recv() => {
                if args.echo {
                    // Echo received audio back on stream 1.
                    // Only echo stream 0 to avoid infinite loops between test clients.
                    if data.len() >= 7 && &data[0..4] == b"WAIF" {
                        let src_stream = u16::from_le_bytes([data[5], data[6]]);
                        if src_stream == 0 {
                            let mut echo = data;
                            echo[5..7].copy_from_slice(&1u16.to_le_bytes());
                            mesh.broadcast_audio(&echo).await;
                            echo_frames += 1;
                            if echo_frames == 1 {
                                println!("Echo: first frame from {from} re-sent on stream 1");
                            }
                        }
                    }
                }
            }

            result = mesh.poll_signaling() => {
                match result? {
                    Some(MeshEvent::PeerJoined { peer_id: pid, display_name }) => {
                        println!("Peer joined: {pid} ({})", display_name.as_deref().unwrap_or("?"));
                        // Greet the new peer.
                        mesh.broadcast(&SyncMessage::Hello {
                            peer_id: peer_id.clone(),
                            display_name: Some(args.name.clone()),
                            identity: Some(identity.clone()),
                        }).await;
                    }
                    Some(MeshEvent::PeerLeft(pid)) => {
                        println!("Peer left: {pid}");
                    }
                    Some(MeshEvent::PeerFailed(pid)) => {
                        println!("Peer connection failed: {pid}");
                    }
                    _ => {}
                }
            }

            _ = tokio::signal::ctrl_c() => {
                println!("\nShutting down ({intervals_sent} intervals sent, {echo_frames} frames echoed).");
                link.enable(false);
                break;
            }
        }
    }

    Ok(())
}

async fn wait_for_peer(mesh: &mut PeerMesh) -> Result<String> {
    loop {
        match tokio::time::timeout(Duration::from_secs(1), mesh.poll_signaling()).await {
            Ok(Ok(Some(MeshEvent::PeerJoined { peer_id: rid, .. }))) => {
                return Ok(rid);
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) => bail!("Signaling channel closed"),
            Ok(Err(e)) => bail!("Signaling error: {e}"),
            Err(_) => {}
        }
        // Responder case: the peer may already be in the mesh from an offer.
        if let Some(rid) = mesh.connected_peers().into_iter().next() {
            return Ok(rid);
        }
    }
}

async fn wait_for_datachannel(mesh: &mut PeerMesh, dc_timeout: Duration) -> Result<()> {
    let result = tokio::time::timeout(dc_timeout, async {
        loop {
            if mesh.any_audio_dc_open() {
                return Ok::<(), anyhow::Error>(());
            }
            match tokio::time::timeout(Duration::from_secs(1), mesh.poll_signaling()).await {
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
        Err(_) => bail!("WebRTC negotiation timed out ({dc_timeout:.0?})"),
    }
}

async fn handle_sync(
    mesh: &PeerMesh,
    from: &str,
    msg: &SyncMessage,
    our_peer_id: &str,
    our_identity: &str,
    our_name: &str,
    greeted_peers: &mut HashSet<String>,
) {
    match msg {
        SyncMessage::Ping { id, sent_at_us } => {
            mesh.send_to(
                from,
                &SyncMessage::Pong {
                    id: *id,
                    ping_sent_at_us: *sent_at_us,
                    pong_sent_at_us: now_us(),
                },
            )
            .await
            .ok();
        }
        SyncMessage::Hello { peer_id, display_name, .. } => {
            info!(
                peer = %peer_id,
                name = ?display_name,
                "Received Hello"
            );
            // Only respond once per peer to avoid Hello ping-pong storms.
            if greeted_peers.insert(peer_id.clone()) {
                mesh.send_to(
                    from,
                    &SyncMessage::Hello {
                        peer_id: our_peer_id.to_string(),
                        display_name: Some(our_name.to_string()),
                        identity: Some(our_identity.to_string()),
                    },
                )
                .await
                .ok();
            }
        }
        _ => {}
    }
}
