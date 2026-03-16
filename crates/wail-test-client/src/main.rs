use std::f64::consts::TAU;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use clap::Parser;
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

    let beat_duration_s = 60.0 / args.bpm;
    let bar_duration_ms = beat_duration_s * args.quantum * 1000.0;
    let frames_per_bar = (bar_duration_ms / FRAME_DURATION_MS as f64).round() as u32;
    let frames_per_interval = frames_per_bar * args.bars;

    println!("=== WAIL Test Client ===");
    println!("Room:       {}", args.room);
    println!("Peer ID:    {peer_id}");
    println!("Server:     {}", args.server);
    println!("BPM:        {}", args.bpm);
    println!("Bars:       {} (quantum {})", args.bars, args.quantum);
    println!("Frames/bar: {frames_per_bar}  Frames/interval: {frames_per_interval}");
    println!(
        "Scale:      {} {} {} {} {} (A minor pentatonic)",
        NOTE_NAMES[0], NOTE_NAMES[1], NOTE_NAMES[2], NOTE_NAMES[3], NOTE_NAMES[4]
    );
    println!("Amplitude:  {}", args.amplitude);
    println!();

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
        1,
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
    let mut interval_index: i64 = 0;
    let mut frame_in_interval: u32 = 0;
    let mut global_bar: u64 = 0;
    let amplitude = args.amplitude.clamp(0.0, 1.0);
    let mut intervals_sent: u64 = 0;
    let start_time = Instant::now();

    // --- 20ms frame timer ---
    let mut frame_timer = tokio::time::interval(Duration::from_millis(FRAME_DURATION_MS));
    frame_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Log the first note.
    println!("Bar 1: {} ({:.0} Hz)  [interval 0]", NOTE_NAMES[0], SCALE[0]);

    loop {
        tokio::select! {
            _ = frame_timer.tick() => {
                // Determine current note from bar position in scale cycle.
                let note_idx = (global_bar % 5) as usize;
                let freq = SCALE[note_idx];

                // Generate PCM and encode to Opus.
                let pcm = generate_sine_frame(freq, &mut phase, amplitude);
                let opus_data = encoder.encode_frame(&pcm)?;

                let is_final = frame_in_interval == frames_per_interval - 1;
                let frame = AudioFrame {
                    interval_index,
                    stream_id: 0,
                    frame_number: frame_in_interval,
                    channels: CHANNELS,
                    opus_data,
                    is_final,
                    sample_rate: if is_final { SAMPLE_RATE } else { 0 },
                    total_frames: if is_final { frames_per_interval } else { 0 },
                    bpm: if is_final { args.bpm } else { 0.0 },
                    quantum: if is_final { args.quantum } else { 0.0 },
                    bars: if is_final { args.bars } else { 0 },
                };

                let wire_bytes = AudioFrameWire::encode(&frame);
                mesh.broadcast_audio(&wire_bytes).await;

                frame_in_interval += 1;

                // Bar boundary — advance note.
                if frame_in_interval % frames_per_bar == 0 && frame_in_interval < frames_per_interval {
                    global_bar += 1;
                    let next_note = (global_bar % 5) as usize;
                    println!(
                        "Bar {}: {} ({:.0} Hz)  [interval {interval_index}]",
                        global_bar + 1,
                        NOTE_NAMES[next_note],
                        SCALE[next_note],
                    );
                }

                // Interval boundary — wrap around.
                if frame_in_interval >= frames_per_interval {
                    global_bar += 1;
                    intervals_sent += 1;
                    let elapsed = start_time.elapsed();
                    println!(
                        "Interval {interval_index} complete ({frames_per_interval} frames, {intervals_sent} total, {elapsed:.0?} elapsed)"
                    );

                    frame_in_interval = 0;
                    interval_index += 1;

                    // Broadcast interval boundary sync.
                    mesh.broadcast(&SyncMessage::IntervalBoundary { index: interval_index }).await;

                    // Log the new bar's note.
                    let next_note = (global_bar % 5) as usize;
                    println!(
                        "Bar {}: {} ({:.0} Hz)  [interval {interval_index}]",
                        global_bar + 1,
                        NOTE_NAMES[next_note],
                        SCALE[next_note],
                    );
                }
            }

            Some((from, msg)) = sync_rx.recv() => {
                handle_sync(&mesh, &from, &msg, &peer_id, &identity, &args.name).await;
            }

            Some((_from, _data)) = audio_rx.recv() => {
                // Discard incoming audio — we're a tone generator, not a receiver.
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
                println!("\nShutting down ({intervals_sent} intervals sent).");
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
            // Respond with our own Hello so the peer knows who we are.
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
        _ => {}
    }
}
