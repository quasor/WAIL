mod chaos;
mod note_script;
pub use note_script::NoteScript;

use std::collections::{HashMap, HashSet};
use std::f64::consts::TAU;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use clap::Parser;
use rusty_link::{AblLink, SessionState};
use tokio::time::MissedTickBehavior;
use tracing::info;
use uuid::Uuid;

use wail_audio::codec::AudioEncoder;
use wail_audio::wire::AudioFrameWire;
use wail_audio::{AudioDecoder, AudioFrame, FrameAssembler};
use wail_core::protocol::{PeerFrameReport, SyncMessage};
use wail_core::IntervalTracker;
use wail_net::{MeshEvent, PeerMesh};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 2;
const OPUS_BITRATE_KBPS: u32 = 128;
const FRAME_DURATION_MS: u64 = 20;
/// Samples per channel per 20ms Opus frame at 48 kHz.
const FRAME_SIZE: usize = 960;

/// A minor pentatonic scale rooted at A3 (220 Hz).
#[allow(dead_code)]
const SCALE: [f32; 5] = [
    220.00, // A3
    261.63, // C4
    293.66, // D4
    329.63, // E4
    392.00, // G4
];

const NOTE_NAMES: [&str; 5] = ["A3", "C4", "D4", "E4", "G4"];

/// Per-note amplitude multipliers (relative to --amplitude).
/// Descending from root to fifth so each note is distinguishable by volume.
const NOTE_AMPLITUDES: [f32; 5] = [1.0, 0.85, 0.7, 0.55, 0.4];

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

    /// Enable debug-level logging
    #[arg(long)]
    verbose: bool,

    /// Enable audio validation mode (FFT frequency check, dropout detection, seam quality)
    #[arg(long)]
    validate: bool,

    /// Number of intervals to validate before exiting (default 4)
    #[arg(long, default_value = "4")]
    validate_intervals: u32,

    /// Timeout in seconds for validation mode (default 120)
    #[arg(long, default_value = "120")]
    validate_timeout: u64,

    /// Scripted note pattern: 'freq:bars,...' (e.g. '220:4,440:2,silence:1,330:4').
    /// Replaces the default pentatonic scale. Loops when exhausted.
    #[arg(long)]
    note_script: Option<String>,

    /// Expected note pattern from the sender for validation (same syntax as --note-script).
    /// Defaults to the sender's --note-script or pentatonic if neither is set.
    #[arg(long)]
    expect_notes: Option<String>,

    /// Chaos test script: 'stable:4,leave:5s,rejoin,stable:4,transport-stop:5s,resume,stable:4'
    #[arg(long)]
    chaos_script: Option<String>,
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

/// Derive which bar within the current interval from Link beat position.
fn beat_to_bar_in_interval(beat: f64, bars: u32, quantum: f64) -> u32 {
    let beats_per_interval = bars as f64 * quantum;
    let beat_in_interval = beat - (beat / beats_per_interval).floor() * beats_per_interval;
    (beat_in_interval / quantum).floor() as u32
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let note_script = if let Some(ref script) = args.note_script {
        NoteScript::parse(script)?
    } else {
        NoteScript::default_pentatonic()
    };

    let expect_script = if let Some(ref script) = args.expect_notes {
        NoteScript::parse(script)?
    } else if let Some(ref script) = args.note_script {
        NoteScript::parse(script)?
    } else {
        NoteScript::default_pentatonic()
    };

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
    if args.note_script.is_some() {
        println!(
            "Notes:      custom script ({} steps, {} bars/cycle)",
            note_script.steps().len(),
            note_script.total_bars(),
        );
    } else {
        println!(
            "Scale:      {} ({:.0}%) {} ({:.0}%) {} ({:.0}%) {} ({:.0}%) {} ({:.0}%) (A minor pentatonic)",
            NOTE_NAMES[0], NOTE_AMPLITUDES[0] * 100.0,
            NOTE_NAMES[1], NOTE_AMPLITUDES[1] * 100.0,
            NOTE_NAMES[2], NOTE_AMPLITUDES[2] * 100.0,
            NOTE_NAMES[3], NOTE_AMPLITUDES[3] * 100.0,
            NOTE_NAMES[4], NOTE_AMPLITUDES[4] * 100.0,
        );
    }
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

    // --- Connect to signaling ---
    let password = args.password.as_deref();
    let (mesh, sync_rx, audio_rx) = PeerMesh::connect_full(
        &args.server,
        &args.room,
        &peer_id,
        password,
        if args.echo { 2 } else { 1 }, // stream 0 = tone, stream 1 = echo (if enabled)
        Some(&args.name),
    )
    .await?;
    let mut mesh_opt: Option<PeerMesh> = Some(mesh);
    let mut sync_rx_opt: Option<tokio::sync::mpsc::UnboundedReceiver<(String, SyncMessage)>> = Some(sync_rx);
    let mut audio_rx_opt: Option<tokio::sync::mpsc::Receiver<(String, Vec<u8>)>> = Some(audio_rx);
    println!("Connected to signaling server.");

    // --- Wait for at least one peer ---
    println!("Waiting for a peer to join room \"{}\"...", args.room);
    let remote_peer_id = wait_for_peer(mesh_opt.as_mut().unwrap()).await?;
    println!("Peer joined: {remote_peer_id}");

    // --- Wait for DataChannels to open ---
    wait_for_datachannel(mesh_opt.as_mut().unwrap(), Duration::from_secs(30)).await?;
    println!("DataChannels open. Starting audio stream.");

    // --- Send Hello ---
    if let Some(ref m) = mesh_opt {
        m.broadcast(&SyncMessage::Hello {
            peer_id: peer_id.clone(),
            display_name: Some(args.name.clone()),
            identity: Some(identity.clone()),
        })
        .await;
    }

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

    // Per-peer audio health: frames received from each peer vs what they report sending.
    let mut peer_frames_recv: HashMap<String, u64> = HashMap::new();
    let mut peer_remote_sent: HashMap<String, u64> = HashMap::new();
    let mut peer_names: HashMap<String, String> = HashMap::new();
    let mut last_health_print = Instant::now();

    // Track interval/bar transitions from Link beat position.
    let mut interval_tracker = IntervalTracker::new(args.bars, args.quantum);
    let mut prev_bar: Option<u32> = None;
    let mut frame_in_interval: u32 = 0;

    // --- 20ms frame timer ---
    let mut frame_timer = tokio::time::interval(Duration::from_millis(FRAME_DURATION_MS));
    frame_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // --- Validation state ---
    let mut assembler = if args.validate {
        Some(FrameAssembler::new())
    } else {
        None
    };
    let mut decoder = if args.validate {
        Some(AudioDecoder::new(SAMPLE_RATE, CHANNELS)?)
    } else {
        None
    };
    let mut validation_results: Vec<wail_audio::fft_analysis::IntervalAnalysis> = Vec::new();
    let validate_start = Instant::now();

    // --- Chaos script state ---
    let chaos_actions = if let Some(ref script) = args.chaos_script {
        chaos::parse_chaos_script(script)?
    } else {
        Vec::new()
    };
    let mut chaos_idx = 0usize;
    let mut chaos_done = false;
    let mut chaos_intervals_counted = 0u32;
    let mut chaos_waiting_until: Option<Instant> = None;
    let mut is_transport_playing = true;

    loop {
        // --- Process chaos script actions ---
        if chaos_idx < chaos_actions.len() {
            match &chaos_actions[chaos_idx] {
                chaos::ChaosAction::Stable(n) => {
                    if chaos_intervals_counted >= *n {
                        println!("[chaos] Stable({n}) complete, advancing to action {}", chaos_idx + 1);
                        chaos_intervals_counted = 0;
                        chaos_idx += 1;
                    }
                }
                chaos::ChaosAction::Leave(duration) => {
                    if chaos_waiting_until.is_none() {
                        println!("[chaos] Disconnecting (leave for {duration:?})...");
                        if let Some(m) = mesh_opt.take() {
                            drop(m);
                        }
                        sync_rx_opt = None;
                        audio_rx_opt = None;
                        chaos_waiting_until = Some(Instant::now() + *duration);
                    } else if Instant::now() >= chaos_waiting_until.unwrap() {
                        chaos_waiting_until = None;
                        chaos_idx += 1;
                        println!("[chaos] Leave complete, advancing to action {}", chaos_idx);
                    }
                }
                chaos::ChaosAction::Rejoin => {
                    if mesh_opt.is_none() {
                        println!("[chaos] Reconnecting...");
                        let (new_mesh, new_sync_rx, new_audio_rx) = PeerMesh::connect_full(
                            &args.server, &args.room, &peer_id, password,
                            if args.echo { 2 } else { 1 },
                            Some(&args.name),
                        ).await?;
                        mesh_opt = Some(new_mesh);
                        sync_rx_opt = Some(new_sync_rx);
                        audio_rx_opt = Some(new_audio_rx);

                        // Wait for datachannel to open.
                        if let Some(ref mut m) = mesh_opt {
                            wait_for_datachannel(m, Duration::from_secs(30)).await?;
                        }

                        // Re-send Hello.
                        if let Some(ref m) = mesh_opt {
                            m.broadcast(&SyncMessage::Hello {
                                peer_id: peer_id.clone(),
                                display_name: Some(args.name.clone()),
                                identity: Some(identity.clone()),
                            }).await;
                        }
                        greeted_peers.clear();
                        println!("[chaos] Reconnected successfully");
                    }
                    chaos_idx += 1;
                }
                chaos::ChaosAction::TransportStop(duration) => {
                    if chaos_waiting_until.is_none() {
                        println!("[chaos] Stopping transport for {duration:?}...");
                        is_transport_playing = false;
                        chaos_waiting_until = Some(Instant::now() + *duration);
                    } else if Instant::now() >= chaos_waiting_until.unwrap() {
                        chaos_waiting_until = None;
                        chaos_idx += 1;
                        println!("[chaos] TransportStop complete, advancing to action {}", chaos_idx);
                    }
                }
                chaos::ChaosAction::Resume => {
                    println!("[chaos] Resuming transport");
                    is_transport_playing = true;
                    chaos_idx += 1;
                }
            }
        }

        // When chaos script is exhausted, continue running normally.
        // The validator (peer-b) will exit when it has enough intervals,
        // and --abort-on-container-exit will shut everything down.
        if !chaos_done && !chaos_actions.is_empty() && chaos_idx >= chaos_actions.len() {
            chaos_done = true;
            if args.validate {
                let all_pass = validation_results.iter().all(|r| r.pass);
                let total_dropouts: u32 = validation_results
                    .iter()
                    .flat_map(|r| &r.bars)
                    .map(|b| b.dropout_frames)
                    .sum();
                let total_seam_failures: u32 = validation_results
                    .iter()
                    .flat_map(|r| &r.bars)
                    .filter(|b| !b.seam_ok)
                    .count() as u32;
                let total_freq_mismatches: u32 = validation_results
                    .iter()
                    .flat_map(|r| &r.bars)
                    .filter(|b| !b.freq_match)
                    .count() as u32;
                let silence_expected: u32 = validation_results
                    .iter()
                    .flat_map(|r| &r.bars)
                    .filter(|b| b.is_silence_expected)
                    .count() as u32;
                let silence_confirmed: u32 = validation_results
                    .iter()
                    .flat_map(|r| &r.bars)
                    .filter(|b| b.is_silence_expected && b.freq_match)
                    .count() as u32;

                let report = serde_json::json!({
                    "pass": all_pass,
                    "intervals": validation_results,
                    "summary": {
                        "intervals_validated": validation_results.len(),
                        "intervals_passed": validation_results.iter().filter(|r| r.pass).count(),
                        "total_dropouts": total_dropouts,
                        "total_seam_failures": total_seam_failures,
                        "total_freq_mismatches": total_freq_mismatches,
                        "silence_bars_expected": silence_expected,
                        "silence_bars_confirmed": silence_confirmed,
                    }
                });

                println!("\n{}", serde_json::to_string_pretty(&report)?);
                std::process::exit(if all_pass { 0 } else { 1 });
            } else {
                println!("[chaos] Script complete, continuing to stream");
            }
        }

        tokio::select! {
            _ = frame_timer.tick() => {
                // Read current beat position from Link.
                let time = link.clock_micros();
                link.capture_app_session_state(&mut session_state);
                let beat = session_state.beat_at_time(time, args.quantum);
                let bpm = session_state.tempo();

                let interval_index = interval_tracker.interval_index(beat);
                let bar_in_interval = beat_to_bar_in_interval(beat, args.bars, args.quantum);

                // Detect interval boundary (monotonic guard prevents flapping).
                if let Some(idx) = interval_tracker.update(beat) {
                    if interval_tracker.current_index().unwrap_or(0) > 0 && idx > 0 {
                        intervals_sent += 1;
                        let elapsed = start_time.elapsed();
                        println!(
                            "Interval {} complete ({frame_in_interval} frames, {intervals_sent} total, {elapsed:.0?} elapsed)",
                            idx - 1,
                        );
                    }

                    // Broadcast interval boundary sync.
                    if let Some(ref m) = mesh_opt {
                        m.broadcast(&SyncMessage::IntervalBoundary { index: idx }).await;
                    }
                    frame_in_interval = 0;
                    prev_bar = None;

                    // Increment chaos interval counter only during Stable actions.
                    if !chaos_actions.is_empty()
                        && chaos_idx < chaos_actions.len()
                        && matches!(chaos_actions[chaos_idx], chaos::ChaosAction::Stable(_))
                    {
                        chaos_intervals_counted += 1;
                    }

                    // Log the new interval's first note.
                    let first_global_bar = idx as u64 * args.bars as u64 + bar_in_interval as u64;
                    let first_freq = if args.constant {
                        Some(440.0)
                    } else {
                        note_script.freq_at_bar(first_global_bar)
                    };
                    println!(
                        "Bar 1: {freq_desc}  [interval {idx}, beat {beat:.1}, {bpm:.1} BPM, Link peers: {peers}]",
                        freq_desc = match first_freq {
                            Some(f) => format!("{f:.0} Hz"),
                            None => "silence".to_string(),
                        },
                        peers = link.num_peers(),
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
                    let bar_freq = if args.constant {
                        Some(440.0)
                    } else {
                        note_script.freq_at_bar(global_bar)
                    };
                    println!(
                        "Bar {}: {freq_desc}  [interval {interval_index}]",
                        bar_in_interval + 1,
                        freq_desc = match bar_freq {
                            Some(f) => format!("{f:.0} Hz"),
                            None => "silence".to_string(),
                        },
                    );
                }
                if prev_bar.is_none() {
                    prev_bar = Some(bar_in_interval);
                }

                // Gate audio sending on transport state.
                if is_transport_playing {
                    // Determine current note.
                    let global_bar = interval_index as u64 * args.bars as u64 + bar_in_interval as u64;
                    let current_freq = if args.constant {
                        Some(440.0f32)
                    } else {
                        note_script.freq_at_bar(global_bar)
                    };

                    // Generate PCM and encode to Opus.
                    let note_amplitude = if args.note_script.is_some() || args.constant {
                        // Custom script or constant: uniform amplitude.
                        amplitude
                    } else {
                        // Default pentatonic: per-note amplitude scaling.
                        let note_idx = (global_bar % 5) as usize;
                        amplitude * NOTE_AMPLITUDES[note_idx]
                    };
                    let pcm = match current_freq {
                        Some(freq) => generate_sine_frame(freq, &mut phase, note_amplitude),
                        None => vec![0.0f32; FRAME_SIZE * CHANNELS as usize], // silence
                    };
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
                    if let Some(ref m) = mesh_opt {
                        m.broadcast_audio(&wire_bytes).await;
                    }

                    frame_in_interval += 1;
                }

                // Print per-peer health and send metrics report every 5 seconds.
                if last_health_print.elapsed() >= Duration::from_secs(5) && !peer_remote_sent.is_empty() {
                    last_health_print = Instant::now();
                    let mut per_peer_metrics = HashMap::new();
                    for (pid, &remote_sent) in &peer_remote_sent {
                        let local_recv = peer_frames_recv.get(pid).copied().unwrap_or(0);
                        let pct = if remote_sent > 0 {
                            local_recv as f64 / remote_sent as f64 * 100.0
                        } else {
                            100.0
                        };
                        let name = peer_names.get(pid).map(|s| s.as_str()).unwrap_or(&pid[..pid.len().min(8)]);
                        println!("[{name}] health: {local_recv}/{remote_sent} frames ({pct:.1}%)");
                        per_peer_metrics.insert(pid.clone(), PeerFrameReport {
                            frames_expected: remote_sent,
                            frames_received: local_recv,
                            rtt_us: None,
                            jitter_us: None,
                            dc_drops: 0,
                            late_frames: 0,
                            decode_failures: 0,
                        });
                    }
                    if let Some(ref m) = mesh_opt {
                        let has_audio_dc = m.any_audio_dc_open();
                        m.send_metrics_report(has_audio_dc, true, per_peer_metrics, 0, None);
                    }
                }

                // Validation timeout check.
                if args.validate && validate_start.elapsed() >= Duration::from_secs(args.validate_timeout) {
                    eprintln!(
                        "Validation timed out after {}s ({} of {} intervals validated)",
                        args.validate_timeout,
                        validation_results.len(),
                        args.validate_intervals,
                    );
                    std::process::exit(1);
                }
            }

            Some((from, msg)) = async {
                match sync_rx_opt.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(ref m) = mesh_opt {
                    handle_sync(m, &from, &msg, &peer_id, &identity, &args.name, &mut greeted_peers, &link, &mut session_state, &mut interval_tracker, &mut peer_remote_sent, &mut peer_names).await;
                }
            }

            Some((from, data)) = async {
                match audio_rx_opt.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                *peer_frames_recv.entry(from.clone()).or_insert(0) += 1;
                if args.echo {
                    // Echo received audio back on stream 1.
                    // Only echo stream 0 to avoid infinite loops between test clients.
                    if data.len() >= 7 && &data[0..4] == b"WAIF" {
                        let src_stream = u16::from_le_bytes([data[5], data[6]]);
                        if src_stream == 0 {
                            let mut echo = data.clone();
                            echo[5..7].copy_from_slice(&1u16.to_le_bytes());
                            if let Some(ref m) = mesh_opt {
                                m.broadcast_audio(&echo).await;
                            }
                            echo_frames += 1;
                            if echo_frames == 1 {
                                println!("Echo: first frame from {from} re-sent on stream 1");
                            }
                        }
                    }
                }

                // Validation: assemble WAIF frames, decode, and analyze.
                if let Some(ref mut asm) = assembler {
                    if let Ok(frame) = AudioFrameWire::decode(&data) {
                        if let Some(assembled) = asm.insert(&from, &frame) {
                            if let Some(ref mut dec) = decoder {
                                if let Ok(pcm) = dec.decode_interval(&assembled.opus_data) {
                                    // Build expected notes for this interval.
                                    let mut expected: Vec<Option<f32>> = Vec::new();
                                    for bar in 0..assembled.bars {
                                        let global_bar =
                                            assembled.interval_index as u64 * args.bars as u64
                                                + bar as u64;
                                        expected.push(expect_script.freq_at_bar(global_bar));
                                    }

                                    let analysis = wail_audio::fft_analysis::analyze_interval(
                                        &pcm,
                                        assembled.channels,
                                        assembled.sample_rate,
                                        assembled.bars,
                                        assembled.bpm,
                                        assembled.quantum,
                                        assembled.interval_index,
                                        &expected,
                                    );

                                    println!(
                                        "Validated interval {}: {}",
                                        assembled.interval_index,
                                        if analysis.pass { "PASS" } else { "FAIL" },
                                    );
                                    for bar in &analysis.bars {
                                        println!(
                                            "  Bar {}: expected={:?}Hz detected={:.1}Hz match={} seam_ok={} dropouts={}",
                                            bar.bar_index, bar.expected_freq, bar.detected_freq,
                                            bar.freq_match, bar.seam_ok, bar.dropout_frames,
                                        );
                                    }

                                    validation_results.push(analysis);

                                    // Check if we have enough validated intervals.
                                    if validation_results.len() >= args.validate_intervals as usize {
                                        let all_pass = validation_results.iter().all(|r| r.pass);
                                        let total_dropouts: u32 = validation_results
                                            .iter()
                                            .flat_map(|r| &r.bars)
                                            .map(|b| b.dropout_frames)
                                            .sum();
                                        let total_seam_failures: u32 = validation_results
                                            .iter()
                                            .flat_map(|r| &r.bars)
                                            .filter(|b| !b.seam_ok)
                                            .count() as u32;
                                        let total_freq_mismatches: u32 = validation_results
                                            .iter()
                                            .flat_map(|r| &r.bars)
                                            .filter(|b| !b.freq_match)
                                            .count() as u32;
                                        let silence_expected: u32 = validation_results
                                            .iter()
                                            .flat_map(|r| &r.bars)
                                            .filter(|b| b.is_silence_expected)
                                            .count() as u32;
                                        let silence_confirmed: u32 = validation_results
                                            .iter()
                                            .flat_map(|r| &r.bars)
                                            .filter(|b| b.is_silence_expected && b.freq_match)
                                            .count() as u32;

                                        let report = serde_json::json!({
                                            "pass": all_pass,
                                            "intervals": validation_results,
                                            "summary": {
                                                "intervals_validated": validation_results.len(),
                                                "intervals_passed": validation_results.iter().filter(|r| r.pass).count(),
                                                "total_dropouts": total_dropouts,
                                                "total_seam_failures": total_seam_failures,
                                                "total_freq_mismatches": total_freq_mismatches,
                                                "silence_bars_expected": silence_expected,
                                                "silence_bars_confirmed": silence_confirmed,
                                            }
                                        });

                                        println!("\n{}", serde_json::to_string_pretty(&report)?);
                                        std::process::exit(if all_pass { 0 } else { 1 });
                                    }
                                }
                            }
                        }
                    }
                }
            }

            result = async {
                match mesh_opt.as_mut() {
                    Some(m) => m.poll_signaling().await,
                    None => std::future::pending().await,
                }
            } => {
                match result? {
                    Some(MeshEvent::PeerJoined { peer_id: pid, display_name }) => {
                        println!("Peer joined: {pid} ({})", display_name.as_deref().unwrap_or("?"));
                        // Greet the new peer.
                        if let Some(ref m) = mesh_opt {
                            m.broadcast(&SyncMessage::Hello {
                                peer_id: peer_id.clone(),
                                display_name: Some(args.name.clone()),
                                identity: Some(identity.clone()),
                            }).await;
                        }
                    }
                    Some(MeshEvent::PeerLeft(pid)) => {
                        println!("Peer left: {pid}");
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
    link: &AblLink,
    session_state: &mut SessionState,
    interval_tracker: &mut IntervalTracker,
    peer_remote_sent: &mut HashMap<String, u64>,
    peer_names: &mut HashMap<String, String>,
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
            if let Some(name) = display_name {
                peer_names.insert(from.to_string(), name.clone());
            }
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
        SyncMessage::TempoChange { bpm, .. } => {
            info!(peer = %from, bpm, "Applying remote tempo change");
            let time = link.clock_micros();
            link.capture_app_session_state(session_state);
            session_state.set_tempo(*bpm, time);
            link.commit_app_session_state(session_state);
        }
        SyncMessage::IntervalBoundary { index } => {
            if interval_tracker.current_index().map_or(true, |l| *index > l) {
                info!(local = ?interval_tracker.current_index(), remote = index, peer = %from, "Syncing interval index");
                interval_tracker.sync_to(*index);
            }
        }
        SyncMessage::AudioStatus { intervals_sent, .. } => {
            peer_remote_sent.insert(from.to_string(), *intervals_sent);
        }
        _ => {}
    }
}
