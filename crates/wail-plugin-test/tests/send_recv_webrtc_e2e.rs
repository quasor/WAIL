//! End-to-end test: real WAIL Send plugin → WebSocket relay → real WAIL Recv plugin.
//!
//! This is the most faithful "client A to client B" test: both ends use actual
//! compiled CLAP plugin binaries. Audio flows through the full stack:
//!
//!   [Real Send .clap]
//!     audio thread → IPC (WAIF frames) → mini_app_a
//!       → WebSocket relay → mini_app_b
//!         → IPC (tag 0x01) → [Real Recv .clap]
//!           → FrameAssembler → Opus decode → ring buffer → DAW output
//!
//! No external services or DAW required: in-process signaling, localhost relay.
//!
//! **Must run with `--test-threads=1`**: all tests mutate the process-global
//! `WAIL_IPC_ADDR` env var and leak plugin instances to prevent dylib unload.
//! Parallel execution causes IPC port cross-contamination between tests.
//!
//! Requires: `cargo xtask build-plugin` before running.

mod common;

use std::time::Duration;

use clack_host::events::EventFlags;
use clack_host::events::event_types::{TransportEvent, TransportFlags};
use clack_host::prelude::*;
use clack_host::utils::{BeatTime, SecondsTime};
use wail_plugin_test::{find_plugin_bundle, rms, sine_wave, ClapTestHost};

const SEND_CLAP_ID: &str = "com.wail.send";
const RECV_CLAP_ID: &str = "com.wail.recv";

/// Number of output ports for the recv plugin: 1 main + 15 aux stereo.
const NUM_OUTPUT_PORTS: usize = 16;

fn load_send() -> ClapTestHost {
    let path = find_plugin_bundle("wail-plugin-send");
    assert!(
        path.exists(),
        "Send plugin bundle not found at {}. Run `cargo xtask build-plugin` first.",
        path.display()
    );
    unsafe { ClapTestHost::load(&path, SEND_CLAP_ID).expect("Failed to load send plugin") }
}

fn load_recv() -> ClapTestHost {
    let path = find_plugin_bundle("wail-plugin-recv");
    assert!(
        path.exists(),
        "Recv plugin bundle not found at {}. Run `cargo xtask build-plugin` first.",
        path.display()
    );
    unsafe { ClapTestHost::load(&path, RECV_CLAP_ID).expect("Failed to load recv plugin") }
}

/// Frequencies used to tag even/odd send intervals so we can verify temporal alignment
/// on the receive side.  4:1 ratio gives unambiguous ZCR separation.
const FREQ_EVEN: f32 = 220.0; // tagging even-indexed send intervals
const FREQ_ODD: f32 = 880.0;  // tagging odd-indexed send intervals

/// Return the interval-tag frequency for a given interval index.
fn interval_freq(interval_index: u64) -> f32 {
    if interval_index % 2 == 0 { FREQ_EVEN } else { FREQ_ODD }
}

/// Estimate frequency via zero-crossing rate.
/// Returns crossings-per-second (≈ 2× the dominant frequency for a pure sine).
fn zcr(samples: &[f32], sample_rate: u32) -> f32 {
    let crossings = samples
        .windows(2)
        .filter(|w| w[0].signum() != w[1].signum())
        .count();
    // Each sinusoidal cycle has 2 zero crossings → divide by 2 for Hz estimate
    crossings as f32 / 2.0 / samples.len() as f32 * sample_rate as f32
}

/// Build a transport event with correct beat position for the given sample offset.
///
/// At 120 BPM / 48 kHz: beat = steady_time / 24000.
/// Setting `HAS_BEATS_TIMELINE` makes the plugin use this beat position instead of
/// its internal `cumulative_samples` fallback. This simulates Ableton Link providing
/// a shared beat grid to all peers.
fn make_transport(steady_time: u64) -> TransportEvent {
    let beats = steady_time as f64 / 24000.0; // 48000 Hz, 120 BPM → 24000 samples/beat
    TransportEvent {
        header: EventHeader::new_core(0, EventFlags::empty()),
        flags: TransportFlags::IS_PLAYING
            | TransportFlags::HAS_TEMPO
            | TransportFlags::HAS_BEATS_TIMELINE,
        song_pos_beats: BeatTime::from_float(beats),
        song_pos_seconds: SecondsTime::from_int(0),
        tempo: 120.0,
        tempo_inc: 0.0,
        loop_start_beats: BeatTime::from_int(0),
        loop_end_beats: BeatTime::from_int(0),
        loop_start_seconds: SecondsTime::from_int(0),
        loop_end_seconds: SecondsTime::from_int(0),
        bar_start: BeatTime::from_int(0),
        bar_number: 0,
        time_signature_numerator: 4,
        time_signature_denominator: 4,
    }
}

fn drive_send(
    processor: &mut StartedPluginAudioProcessor<wail_plugin_test::TestHost>,
    buf_size: u32,
    steady_time: u64,
    freq: f32,
) {
    let n = buf_size as usize;
    let mut input_left = sine_wave(freq, n, 1, 48000);
    let mut input_right = input_left.clone();
    let mut output_left = vec![0.0f32; n];
    let mut output_right = vec![0.0f32; n];

    let mut in_ports = AudioPorts::with_capacity(2, 1);
    let in_bufs = in_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(
            [&mut input_left[..], &mut input_right[..]]
                .into_iter()
                .map(|b| InputChannel { buffer: b, is_constant: false }),
        ),
    }]);

    let mut out_ports = AudioPorts::with_capacity(2, 1);
    let mut out_bufs = out_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(
            [&mut output_left[..], &mut output_right[..]].into_iter(),
        ),
    }]);

    let in_events = InputEvents::empty();
    let mut out_events = OutputEvents::void();

    let transport = make_transport(steady_time);

    processor
        .process(
            &in_bufs,
            &mut out_bufs,
            &in_events,
            &mut out_events,
            Some(steady_time),
            Some(&transport),
        )
        .expect("send process() failed");
}

fn drive_recv(
    processor: &mut StartedPluginAudioProcessor<wail_plugin_test::TestHost>,
    buf_size: u32,
    steady_time: u64,
) -> Vec<f32> {
    let n = buf_size as usize;
    let mut input_left = vec![0.0f32; n];
    let mut input_right = vec![0.0f32; n];

    let mut out_bufs: Vec<[Vec<f32>; 2]> = (0..NUM_OUTPUT_PORTS)
        .map(|_| [vec![0.0f32; n], vec![0.0f32; n]])
        .collect();

    let mut in_ports = AudioPorts::with_capacity(2, 1);
    let in_bufs = in_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(
            [&mut input_left[..], &mut input_right[..]]
                .into_iter()
                .map(|b| InputChannel { buffer: b, is_constant: false }),
        ),
    }]);

    let mut out_ports = AudioPorts::with_capacity(NUM_OUTPUT_PORTS * 2, NUM_OUTPUT_PORTS);
    let mut out_buf_refs = out_ports.with_output_buffers(
        out_bufs.iter_mut().map(|[left, right]| AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(
                [left.as_mut_slice(), right.as_mut_slice()].into_iter(),
            ),
        }),
    );

    let in_events = InputEvents::empty();
    let mut out_events = OutputEvents::void();

    let transport = make_transport(steady_time);

    processor
        .process(
            &in_bufs,
            &mut out_buf_refs,
            &in_events,
            &mut out_events,
            Some(steady_time),
            Some(&transport),
        )
        .expect("recv process() failed");

    out_bufs.into_iter().next().unwrap()[0].clone()
}

// ---------------------------------------------------------------------------
// Real-time paced test
// ---------------------------------------------------------------------------

/// Drive send + recv plugins at wall-clock real-time speed with ZCR temporal alignment.
///
/// Paces callbacks at the actual DAW audio clock rate (via `thread::sleep`),
/// reproducing the "2 bars sound, 2 bars silence" dropout that occurs when
/// live-append is broken. Each send interval is tagged with an alternating
/// frequency (220Hz / 880Hz); ZCR analysis on the recv side verifies that the
/// correct interval's content is playing at the right time.
///
/// Duration: ~30s of real-time audio, faithful to production timing.
#[test]
fn realtime_paced_no_dropout_e2e() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    // 1. Load both .clap binaries
    let mut send_host = load_send();
    let mut recv_host = load_recv();

    let send_ipc_port = common::random_port();
    let recv_ipc_port = common::random_port();

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

    // 2. Start signaling + both mini_apps; wait for relay to establish.
    rt.block_on(async {
        let signaling_url = common::start_test_signaling_server().await;

        let url_a = signaling_url.clone();
        tokio::spawn(common::mini_app_session(
            send_ipc_port,
            url_a,
            "realtime-room".into(),
            "peer-a".into(),
            "test".into(),
        ));

        tokio::time::sleep(Duration::from_millis(100)).await;

        tokio::spawn(common::mini_app_session(
            recv_ipc_port,
            signaling_url,
            "realtime-room".into(),
            "peer-b".into(),
            "test".into(),
        ));

        tokio::time::sleep(Duration::from_secs(8)).await;
    });

    // 3. Activate plugins
    unsafe { std::env::set_var("WAIL_IPC_ADDR", format!("127.0.0.1:{send_ipc_port}")) };
    let send_stopped = send_host
        .activate(48000.0, 32, 4096)
        .expect("Failed to activate send plugin");
    let mut send_proc = send_stopped
        .start_processing()
        .expect("Failed to start send processing");

    std::thread::sleep(Duration::from_millis(200));

    unsafe { std::env::set_var("WAIL_IPC_ADDR", format!("127.0.0.1:{recv_ipc_port}")) };
    let recv_stopped = recv_host
        .activate(48000.0, 32, 4096)
        .expect("Failed to activate recv plugin");
    let mut recv_proc = recv_stopped
        .start_processing()
        .expect("Failed to start recv processing");

    std::thread::sleep(Duration::from_millis(500));

    // 4. Drive both plugins at REAL-TIME speed.
    //
    //    buf_size = 4096 samples per channel at 48 kHz = ~85.3 ms per callback.
    //    We sleep for this duration between callbacks to match the DAW clock.
    //
    //    Run for ~30 seconds of real-time audio:
    //      callbacks = 30s / 85.3ms ≈ 352
    //    Plus ~2-interval pipeline warmup (188 callbacks in fast mode, but at
    //    real-time speed the warmup is 1 interval = 94 callbacks).
    //    Total: 94 warmup + 352 steady ≈ 450 callbacks.

    let buf_size: u32 = 4096;
    let num_callbacks: u64 = 450;
    let sleep_per_callback = Duration::from_secs_f64(buf_size as f64 / 48000.0);

    let mut non_silent_buffers: u32 = 0;
    let mut in_audio_phase = false;
    let mut current_gap: u32 = 0;
    let mut max_gap: u32 = 0;

    // Per-interval stats: (index, non_silent, total, zcr_sum, rms_by_position)
    // rms_by_position[i] is the RMS of the i-th callback in this interval (0.0 if silent).
    let mut interval_stats: Vec<(u64, u32, u32, f64, Vec<f64>)> = Vec::new();
    let mut cur_interval = u64::MAX;
    let mut cur_ns: u32 = 0;
    let mut cur_total: u32 = 0;
    let mut cur_zcr_sum: f64 = 0.0;
    let mut cur_rms_by_pos: Vec<f64> = Vec::new();

    let wall_start = std::time::Instant::now();

    for i in 0..num_callbacks {
        let steady_time = i * buf_size as u64;
        let interval_index = steady_time / 384_000;

        if interval_index != cur_interval {
            if cur_interval != u64::MAX {
                interval_stats.push((cur_interval, cur_ns, cur_total, cur_zcr_sum, std::mem::take(&mut cur_rms_by_pos)));
            }
            cur_interval = interval_index;
            cur_ns = 0;
            cur_total = 0;
            cur_zcr_sum = 0.0;
            cur_rms_by_pos.clear();
        }
        cur_total += 1;

        let send_freq = interval_freq(interval_index);
        drive_send(&mut send_proc, buf_size, steady_time, send_freq);
        let out_l = drive_recv(&mut recv_proc, buf_size, steady_time);

        let energy = rms(&out_l);
        cur_rms_by_pos.push(energy as f64);
        if energy > 0.001 {
            non_silent_buffers += 1;
            cur_ns += 1;
            cur_zcr_sum += zcr(&out_l, 48000) as f64;
            if in_audio_phase {
                max_gap = max_gap.max(current_gap);
                current_gap = 0;
            }
            in_audio_phase = true;
        } else if in_audio_phase {
            current_gap += 1;
        }

        // Real-time pacing: sleep to match the DAW audio clock.
        // Subtract the time already spent in process() so we don't drift.
        let expected_elapsed = sleep_per_callback * (i as u32 + 1);
        let actual_elapsed = wall_start.elapsed();
        if let Some(remaining) = expected_elapsed.checked_sub(actual_elapsed) {
            std::thread::sleep(remaining);
        }
    }

    // Finalize
    if cur_interval != u64::MAX {
        interval_stats.push((cur_interval, cur_ns, cur_total, cur_zcr_sum, cur_rms_by_pos));
    }
    max_gap = max_gap.max(current_gap);

    let wall_elapsed = wall_start.elapsed();
    let max_gap_ms = max_gap as f64 * buf_size as f64 / 48000.0 * 1000.0;

    // Find pipeline lag
    let lag = interval_stats
        .iter()
        .find(|(_, ns, _, _, _)| *ns > 0)
        .map(|(idx, _, _, _, _)| *idx)
        .unwrap_or(0);

    // Log per-interval breakdown with ZCR frequency tag and RMS amplitude
    for (idx, non_silent, total, zcr_sum, rms_vals) in &interval_stats {
        let pct = if *total > 0 { *non_silent as f64 / *total as f64 * 100.0 } else { 0.0 };
        let avg_zcr = if *non_silent > 0 { *zcr_sum / *non_silent as f64 } else { 0.0 };
        let detected = if avg_zcr > 550.0 { "~880Hz" } else if avg_zcr > 0.1 { "~220Hz" } else { "(silent)" };
        let send_idx = idx.saturating_sub(lag);
        let expected_freq = interval_freq(send_idx);
        let avg_rms = if rms_vals.is_empty() { 0.0 } else { rms_vals.iter().sum::<f64>() / rms_vals.len() as f64 };
        eprintln!(
            "[realtime]   Interval {idx:2}: {non_silent:3}/{total:3} ({pct:.0}%)  \
             ZCR≈{avg_zcr:.0}Hz ({detected})  RMS={avg_rms:.4}  \
             [from send interval {send_idx}, sent {expected_freq:.0}Hz]"
        );
    }

    eprintln!(
        "[realtime] Summary: non_silent={non_silent_buffers}/{num_callbacks}, \
         lag={lag}, max_gap={max_gap} ({max_gap_ms:.0}ms), wall_time={:.1}s",
        wall_elapsed.as_secs_f64()
    );

    // 5. Assertions
    //
    //    At real-time speed, pipeline lag should be 1 interval (not 2 like the fast test).
    //    After warmup, every interval should have >85% audio coverage and the max gap
    //    between non-silent buffers must be small (≤ 3 buffers / ~256ms).

    // Must have substantial audio output
    let warmup_callbacks = (lag + 1) * 94; // callbacks consumed by pipeline warmup
    let steady_callbacks = num_callbacks.saturating_sub(warmup_callbacks);
    let expected_min = (steady_callbacks as f64 * 0.80) as u32;
    assert!(
        non_silent_buffers >= expected_min,
        "Expected ≥{expected_min} non-silent buffers in steady state, \
         got {non_silent_buffers}/{num_callbacks} (lag={lag}, warmup={warmup_callbacks} callbacks)."
    );

    // No multi-bar silence gaps after audio starts
    assert!(
        max_gap <= 3,
        "Detected a gap of {max_gap} consecutive silent buffers ({max_gap_ms:.0}ms) — \
         at real-time speed, interval transitions must be seamless. \
         A gap > 3 buffers (~256ms) indicates the live-append path is not working."
    );

    // Per-interval coverage: every post-warmup interval must have >75% audio
    for (idx, non_silent, total, _, _) in &interval_stats {
        if *idx <= lag { continue; } // skip warmup intervals
        let pct = *non_silent as f64 / *total as f64 * 100.0;
        assert!(
            pct > 75.0,
            "Interval {idx} has only {pct:.0}% audio coverage ({non_silent}/{total}). \
             This is the 'bars of silence' bug — each interval must have >75% audio at real-time speed."
        );
    }

    // Temporal alignment: verify recv interval N plays send interval N−lag's content.
    //
    // Each send interval is tagged with a distinct frequency (220Hz = even, 880Hz = odd).
    // ZCR threshold of 550Hz sits midway between the two; detected frequency must match
    // the expected frequency for the corresponding send interval.
    //
    // This catches ring buffer swap ordering bugs (off-by-one in interval boundaries).
    let zcr_threshold = (FREQ_EVEN + FREQ_ODD) / 2.0; // 550 Hz
    for (recv_idx, non_silent, total, zcr_sum, _) in &interval_stats {
        if (*non_silent as f64) < (*total as f64 * 0.75) { continue; } // skip sparse intervals
        if *recv_idx <= lag { continue; } // skip warmup

        let send_idx = recv_idx - lag;
        let expected_freq = interval_freq(send_idx);
        let avg_zcr = zcr_sum / *non_silent as f64;
        let detected_high = avg_zcr > zcr_threshold as f64;
        let expected_high = expected_freq > zcr_threshold;
        assert_eq!(
            detected_high, expected_high,
            "Temporal alignment failure at recv interval {recv_idx}: \
             expected {expected_freq:.0}Hz (from send interval {send_idx}), \
             but ZCR≈{avg_zcr:.0}Hz indicates {}. \
             Recv interval N must play send interval N−{lag} content.",
            if detected_high { "880Hz (odd)" } else { "220Hz (even)" },
        );
    }

    let zcr_verified = interval_stats.iter().filter(|(idx, ns, total, _, _)| {
        *idx > lag && (*ns as f64) >= (*total as f64 * 0.75)
    }).count();

    // Amplitude consistency: verify RMS is stable across the pipeline.
    //
    // Send sine amplitude = 0.5 → theoretical RMS = 0.5/√2 ≈ 0.354.
    // After Opus encode/decode there's some loss, so we accept a range.
    // Within each interval, RMS should be stable (no unexpected dips/spikes).
    // Across intervals, mean RMS should be consistent (no volume drift).
    let mut interval_mean_rms: Vec<(u64, f64)> = Vec::new();
    for (idx, _non_silent, _total, _, rms_by_pos) in &interval_stats {
        let non_silent_rms: Vec<f64> = rms_by_pos.iter().copied().filter(|r| *r > 0.001).collect();
        if *idx <= lag || non_silent_rms.len() < 5 { continue; }

        let mean = non_silent_rms.iter().sum::<f64>() / non_silent_rms.len() as f64;
        let variance = non_silent_rms.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / non_silent_rms.len() as f64;
        let stddev = variance.sqrt();
        let cv = if mean > 0.0 { stddev / mean } else { 0.0 };

        // Absolute range: RMS should be between 0.05 and 0.6
        // (generous bounds accounting for Opus codec loss and crossfade regions)
        assert!(
            mean > 0.05 && mean < 0.6,
            "Interval {idx}: mean RMS {mean:.4} outside expected range [0.05, 0.6]. \
             Possible gain staging issue in the pipeline."
        );

        // Stability within interval: coefficient of variation < 30%
        // (allows for crossfade transients at interval boundaries)
        assert!(
            cv < 0.30,
            "Interval {idx}: RMS coefficient of variation {cv:.3} ({stddev:.4}/{mean:.4}) exceeds 30%. \
             Amplitude is unstable within the interval — possible crossfade or ring buffer issue."
        );

        interval_mean_rms.push((*idx, mean));
    }

    // Cross-interval consistency: mean RMS should not vary more than 40% between intervals.
    // (generous to allow for codec artifacts at different frequencies)
    if interval_mean_rms.len() >= 2 {
        let global_mean = interval_mean_rms.iter().map(|(_, r)| r).sum::<f64>() / interval_mean_rms.len() as f64;
        for (idx, mean) in &interval_mean_rms {
            let deviation = (mean - global_mean).abs() / global_mean;
            assert!(
                deviation < 0.40,
                "Interval {idx}: mean RMS {mean:.4} deviates {:.0}% from global mean {global_mean:.4}. \
                 Volume should be consistent across intervals.",
                deviation * 100.0,
            );
        }
        eprintln!(
            "[realtime] Amplitude: global mean RMS={global_mean:.4}, {n} intervals verified (CV<30%, cross-interval <40%)",
            n = interval_mean_rms.len(),
        );
    }

    // Boundary amplitude check: the first 2 callbacks of each interval must not
    // have a significant volume dip compared to steady-state. This catches codec
    // warmup bugs (e.g., re-initializing the Opus decoder each interval, which
    // causes ~120ms of degraded output during the crossfade-in region).
    const BOUNDARY_CALLBACKS: usize = 2; // ~170ms at 4096/48kHz
    let mut boundary_verified = 0usize;
    for (idx, _non_silent, _total, _, rms_by_pos) in &interval_stats {
        if *idx <= lag || rms_by_pos.len() < 10 { continue; }

        // Steady-state: mean RMS of callbacks after the boundary region
        let steady_rms: Vec<f64> = rms_by_pos[BOUNDARY_CALLBACKS..]
            .iter().copied().filter(|r| *r > 0.001).collect();
        if steady_rms.len() < 5 { continue; }
        let steady_mean = steady_rms.iter().sum::<f64>() / steady_rms.len() as f64;

        // Boundary: RMS of the first BOUNDARY_CALLBACKS callbacks
        for (pos, &boundary_rms) in rms_by_pos[..BOUNDARY_CALLBACKS].iter().enumerate() {
            if boundary_rms < 0.001 { continue; } // skip if silent (no audio yet)
            let ratio = boundary_rms / steady_mean;
            assert!(
                ratio >= 0.70,
                "Interval {idx} callback {pos}: boundary RMS {boundary_rms:.4} is only {:.0}% of \
                 steady-state mean {steady_mean:.4}. Codec warmup or crossfade amplitude bug — \
                 first callbacks of each interval must be ≥70% of steady-state volume.",
                ratio * 100.0,
            );
        }
        boundary_verified += 1;
    }
    if boundary_verified > 0 {
        eprintln!(
            "[realtime] Boundary amplitude: {boundary_verified} intervals verified \
             (first {BOUNDARY_CALLBACKS} callbacks ≥70% of steady-state)"
        );
    }

    eprintln!(
        "[realtime] PASSED — {non_silent_buffers} non-silent buffers, \
         max_gap={max_gap} ({max_gap_ms:.0}ms), all post-warmup intervals >75% audio, \
         temporal alignment verified across {zcr_verified} intervals."
    );

    // 6. Cleanup
    let send_stopped = send_proc.stop_processing();
    send_host.deactivate(send_stopped);
    let recv_stopped = recv_proc.stop_processing();
    recv_host.deactivate(recv_stopped);
    send_host.leak();
    recv_host.leak();
}

// ---------------------------------------------------------------------------
// Late-join bidirectional test
// ---------------------------------------------------------------------------

/// Verify that a late-joining peer can exchange audio bidirectionally.
///
/// Timeline:
///   t=0      — peer-a joins, send-a + recv-a plugins active
///   t≈0..15s — peer-a sends ~200 callbacks of audio (≥2 full intervals)
///   t≈15s    — peer-b joins the room, relay establishes (~8s)
///   t≈23s    — send-b + recv-b plugins active, both sides driving audio
///   t≈23..83s — 750 callbacks driven; both recv plugins must produce non-silent output
///
/// This specifically guards against the interval-guard regression where a joining
/// peer's audio-send was silently blocked for up to one full interval (~8s) until
/// the next natural IntervalBoundary fired.  The fix broadcasts the current
/// interval index to the new peer on PeerJoined so the guard clears immediately.
#[test]
fn late_join_bidirectional_e2e() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    // 1. Load four .clap binaries (send-a, recv-a, send-b, recv-b)
    let mut send_a_host = load_send();
    let mut recv_a_host = load_recv();
    let mut send_b_host = load_send();
    let mut recv_b_host = load_recv();

    // Each peer's mini_app gets its own IPC port; both send and recv plugins
    // for that peer connect to the same port.
    let port_a = common::random_port();
    let port_b = common::random_port();

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

    // 2. Start signaling server and peer-a's mini_app.  peer-b joins later.
    let signaling_url = rt.block_on(async {
        let url = common::start_test_signaling_server().await;
        tokio::spawn(common::mini_app_session(
            port_a,
            url.clone(),
            "late-join-room".into(),
            "peer-a".into(),
            "test".into(),
        ));
        // Give peer-a time to connect to the signaling server before we start driving audio.
        tokio::time::sleep(Duration::from_secs(2)).await;
        url
    });

    // 3. Activate peer-a's send + recv plugins (both connect to port_a).
    let buf_size: u32 = 4096;
    unsafe { std::env::set_var("WAIL_IPC_ADDR", format!("127.0.0.1:{port_a}")) };
    let send_a_stopped = send_a_host
        .activate(48000.0, 32, 4096)
        .expect("Failed to activate send-a");
    let mut send_a_proc = send_a_stopped
        .start_processing()
        .expect("Failed to start send-a processing");
    std::thread::sleep(Duration::from_millis(200));

    let recv_a_stopped = recv_a_host
        .activate(48000.0, 32, 4096)
        .expect("Failed to activate recv-a");
    let mut recv_a_proc = recv_a_stopped
        .start_processing()
        .expect("Failed to start recv-a processing");
    std::thread::sleep(Duration::from_millis(500));

    // 4. Drive peer-a alone for ~200 callbacks (≈17s of audio, ≥2 full intervals at 120 BPM).
    //    This simulates peer-a jamming alone for ~15s before peer-b arrives.
    //    recv-a produces silence here — no remote peer has connected yet.
    //
    //    samples_per_interval = 4 bars × 4 quantum × 60 / 120 BPM × 48000 = 384,000
    //    callbacks_per_interval ≈ 384,000 / 4096 ≈ 94
    //    200 callbacks ≈ 2.1 intervals → peer-a will be at interval index 2 when peer-b joins
    let a_only_callbacks: u64 = 200;
    for i in 0..a_only_callbacks {
        let steady_time = i * buf_size as u64;
        drive_send(&mut send_a_proc, buf_size, steady_time, FREQ_EVEN);
        drive_recv(&mut recv_a_proc, buf_size, steady_time); // silent — no remote peer yet
    }

    // 5. peer-b joins the room; wait ~8s for relay to establish.
    rt.block_on(async {
        tokio::spawn(common::mini_app_session(
            port_b,
            signaling_url,
            "late-join-room".into(),
            "peer-b".into(),
            "test".into(),
        ));
        tokio::time::sleep(Duration::from_secs(8)).await;
    });

    // 6. Activate peer-b's send + recv plugins (both connect to port_b).
    unsafe { std::env::set_var("WAIL_IPC_ADDR", format!("127.0.0.1:{port_b}")) };
    let send_b_stopped = send_b_host
        .activate(48000.0, 32, 4096)
        .expect("Failed to activate send-b");
    let mut send_b_proc = send_b_stopped
        .start_processing()
        .expect("Failed to start send-b processing");
    std::thread::sleep(Duration::from_millis(200));

    let recv_b_stopped = recv_b_host
        .activate(48000.0, 32, 4096)
        .expect("Failed to activate recv-b");
    let mut recv_b_proc = recv_b_stopped
        .start_processing()
        .expect("Failed to start recv-b processing");
    std::thread::sleep(Duration::from_millis(500));

    // 7. Drive both peers for 750 callbacks (≈63s of audio) and measure bidirectional receipt.
    //
    //    Both peers share the same steady_time (simulating Ableton Link beat grid sync).
    //    The transport provides HAS_BEATS_TIMELINE so plugins use the host beat position
    //    instead of their internal cumulative_samples fallback. This ensures both peers
    //    produce matching interval indices — just like production with Link.
    //
    //    Tag each send interval with alternating frequencies so we can confirm
    //    the correct audio is flowing in each direction.
    let both_callbacks: u64 = 750;
    let mut recv_a_non_silent: u32 = 0; // peer-a receiving audio FROM peer-b
    let mut recv_b_non_silent: u32 = 0; // peer-b receiving audio FROM peer-a
    let mut recv_a_first_audio: Option<u64> = None;
    let mut recv_b_first_audio: Option<u64> = None;

    for i in 0..both_callbacks {
        // Both peers on the same timeline (Link-synced beat grid)
        let steady = (a_only_callbacks + i) * buf_size as u64;
        let interval_idx = steady / 384_000;

        drive_send(&mut send_a_proc, buf_size, steady, interval_freq(interval_idx));
        drive_send(&mut send_b_proc, buf_size, steady, interval_freq(interval_idx));

        let out_a = drive_recv(&mut recv_a_proc, buf_size, steady);
        let out_b = drive_recv(&mut recv_b_proc, buf_size, steady);

        if rms(&out_a) > 0.001 {
            recv_a_non_silent += 1;
            recv_a_first_audio.get_or_insert(i);
        }
        if rms(&out_b) > 0.001 {
            recv_b_non_silent += 1;
            recv_b_first_audio.get_or_insert(i);
        }
    }

    eprintln!(
        "[test] late-join: recv_a={recv_a_non_silent}/{both_callbacks} (first at cb {:?}), \
         recv_b={recv_b_non_silent}/{both_callbacks} (first at cb {:?})",
        recv_a_first_audio, recv_b_first_audio,
    );

    // 8. Both sides must have received substantial audio.
    //    Pipeline warmup ≈ 2 intervals = 188 callbacks; from 750 callbacks: ≥ 560 expected.
    //    Use 400 as conservative threshold.
    assert!(
        recv_a_non_silent >= 400,
        "peer-a (early joiner) should receive audio from peer-b (late joiner): \
         got {recv_a_non_silent}/{both_callbacks} non-silent buffers"
    );
    assert!(
        recv_b_non_silent >= 400,
        "peer-b (late joiner) should receive audio from peer-a (early joiner): \
         got {recv_b_non_silent}/{both_callbacks} non-silent buffers"
    );

    // Audio must begin flowing within 3 intervals (≈ 282 callbacks) of the
    // bidirectional phase starting.  This guards against the interval-guard
    // regression where audio was silently dropped for up to ~8s after joining.
    const MAX_WARMUP_CALLBACKS: u64 = 282;
    if let Some(first) = recv_a_first_audio {
        assert!(
            first <= MAX_WARMUP_CALLBACKS,
            "peer-a waited too long to receive from peer-b: \
             first audio at callback {first}, expected within {MAX_WARMUP_CALLBACKS}"
        );
    } else {
        panic!("peer-a never received any audio from peer-b");
    }
    if let Some(first) = recv_b_first_audio {
        assert!(
            first <= MAX_WARMUP_CALLBACKS,
            "peer-b waited too long to receive from peer-a: \
             first audio at callback {first}, expected within {MAX_WARMUP_CALLBACKS}"
        );
    } else {
        panic!("peer-b never received any audio from peer-a");
    }

    eprintln!(
        "[test] PASSED — late-join bidirectional: recv_a={recv_a_non_silent}, \
         recv_b={recv_b_non_silent}, both within {MAX_WARMUP_CALLBACKS}-callback warmup window."
    );

    // 9. Stop and deactivate (stop_processing must precede deactivate)
    let send_a_stopped = send_a_proc.stop_processing();
    send_a_host.deactivate(send_a_stopped);
    let recv_a_stopped = recv_a_proc.stop_processing();
    recv_a_host.deactivate(recv_a_stopped);
    let send_b_stopped = send_b_proc.stop_processing();
    send_b_host.deactivate(send_b_stopped);
    let recv_b_stopped = recv_b_proc.stop_processing();
    recv_b_host.deactivate(recv_b_stopped);

    // 10. Leak all hosts to prevent dylib unload while IPC threads are still running.
    send_a_host.leak();
    recv_a_host.leak();
    send_b_host.leak();
    recv_b_host.leak();
}

// ---------------------------------------------------------------------------
// Peer disconnect → silence test
// ---------------------------------------------------------------------------

/// Verify that the recv plugin returns to silence after the remote peer disconnects.
///
/// Timeline:
///   Phase 1 — Normal audio flow: send-a → relay → recv-b (confirm audio is playing)
///   Phase 2 — Kill send-side mini_app (simulates peer crash / app quit)
///   Phase 3 — Continue driving recv-b; verify output goes silent within 2 intervals
///
/// This catches stale ring buffer bugs where the playback slot replays old audio
/// indefinitely after the peer is gone. The mini_app sends PeerLeft IPC on disconnect,
/// which should trigger the recv plugin to clear its audio state.
///
/// Duration: ~50s (30s audio + 8s setup + 15s disconnect verification).
#[test]
fn peer_disconnect_silence_e2e() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    let mut send_host = load_send();
    let mut recv_host = load_recv();

    let send_ipc_port = common::random_port();
    let recv_ipc_port = common::random_port();

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

    // 1. Start signaling + both mini_apps; capture the send-side handle for later abort.
    let send_app_handle = rt.block_on(async {
        let signaling_url = common::start_test_signaling_server().await;

        let url_a = signaling_url.clone();
        let handle = tokio::spawn(common::mini_app_session(
            send_ipc_port,
            url_a,
            "disconnect-room".into(),
            "peer-a".into(),
            "test".into(),
        ));

        tokio::time::sleep(Duration::from_millis(100)).await;

        tokio::spawn(common::mini_app_session(
            recv_ipc_port,
            signaling_url,
            "disconnect-room".into(),
            "peer-b".into(),
            "test".into(),
        ));

        tokio::time::sleep(Duration::from_secs(8)).await;
        handle
    });

    // 2. Activate plugins
    let buf_size: u32 = 4096;
    unsafe { std::env::set_var("WAIL_IPC_ADDR", format!("127.0.0.1:{send_ipc_port}")) };
    let send_stopped = send_host
        .activate(48000.0, 32, 4096)
        .expect("Failed to activate send plugin");
    let mut send_proc = send_stopped
        .start_processing()
        .expect("Failed to start send processing");

    std::thread::sleep(Duration::from_millis(200));

    unsafe { std::env::set_var("WAIL_IPC_ADDR", format!("127.0.0.1:{recv_ipc_port}")) };
    let recv_stopped = recv_host
        .activate(48000.0, 32, 4096)
        .expect("Failed to activate recv plugin");
    let mut recv_proc = recv_stopped
        .start_processing()
        .expect("Failed to start recv processing");

    std::thread::sleep(Duration::from_millis(500));

    // 3. Phase 1: Drive audio at real-time speed for ~30s (same as the main test).
    //    Confirm audio is flowing before we disconnect.
    let phase1_callbacks: u64 = 450;
    let sleep_per_callback = Duration::from_secs_f64(buf_size as f64 / 48000.0);
    let mut phase1_non_silent: u32 = 0;
    let wall_start = std::time::Instant::now();

    for i in 0..phase1_callbacks {
        let steady_time = i * buf_size as u64;
        let interval_index = steady_time / 384_000;
        let send_freq = interval_freq(interval_index);
        drive_send(&mut send_proc, buf_size, steady_time, send_freq);
        let out_l = drive_recv(&mut recv_proc, buf_size, steady_time);

        if rms(&out_l) > 0.001 {
            phase1_non_silent += 1;
        }

        let expected_elapsed = sleep_per_callback * (i as u32 + 1);
        let actual_elapsed = wall_start.elapsed();
        if let Some(remaining) = expected_elapsed.checked_sub(actual_elapsed) {
            std::thread::sleep(remaining);
        }
    }

    eprintln!(
        "[disconnect] Phase 1: {phase1_non_silent}/{phase1_callbacks} non-silent buffers"
    );
    assert!(
        phase1_non_silent >= 300,
        "Phase 1 failed: expected substantial audio before disconnect, \
         got only {phase1_non_silent}/{phase1_callbacks} non-silent buffers"
    );

    // 4. Kill the send-side mini_app. This drops connections, causing
    //    the recv-side mini_app to detect PeerLeft and send PeerLeft IPC.
    rt.block_on(async {
        send_app_handle.abort();
        // Give the recv-side mini_app time to detect the disconnection and
        // forward PeerLeft IPC to the recv plugin.
        tokio::time::sleep(Duration::from_secs(3)).await;
    });

    // Also stop sending audio from the send plugin (it has no peer to send to).
    let send_stopped = send_proc.stop_processing();
    send_host.deactivate(send_stopped);

    // 5. Phase 2: Drive recv-only at real-time speed for ~2 intervals (188 callbacks).
    //    The recv plugin should output silence after the ring buffer's current interval
    //    finishes playing.
    let phase2_callbacks: u64 = 188;
    let mut phase2_non_silent: u32 = 0;
    let mut last_non_silent_cb: Option<u64> = None;
    let phase2_wall_start = std::time::Instant::now();

    for i in 0..phase2_callbacks {
        let steady_time = (phase1_callbacks + i) * buf_size as u64;
        let out_l = drive_recv(&mut recv_proc, buf_size, steady_time);

        if rms(&out_l) > 0.001 {
            phase2_non_silent += 1;
            last_non_silent_cb = Some(i);
        }

        let expected_elapsed = sleep_per_callback * (i as u32 + 1);
        let actual_elapsed = phase2_wall_start.elapsed();
        if let Some(remaining) = expected_elapsed.checked_sub(actual_elapsed) {
            std::thread::sleep(remaining);
        }
    }

    eprintln!(
        "[disconnect] Phase 2: {phase2_non_silent}/{phase2_callbacks} non-silent buffers \
         (last non-silent at cb {:?})",
        last_non_silent_cb,
    );

    // Allow up to 1 interval of residual audio (the currently-playing interval
    // may have buffered data). After that, output must be silence.
    // 1 interval = 94 callbacks at 4096/48kHz/120BPM.
    let max_residual_callbacks: u64 = 100; // slightly generous
    if let Some(last) = last_non_silent_cb {
        assert!(
            last < max_residual_callbacks,
            "Recv plugin still producing audio {last} callbacks after peer disconnect. \
             Expected silence within ~1 interval ({max_residual_callbacks} callbacks). \
             The ring buffer may be replaying stale data."
        );
    }

    // The second half of phase 2 (callbacks 94-188) must be fully silent.
    // We check by looking at the last interval's worth of callbacks.
    let tail_start = max_residual_callbacks.min(phase2_callbacks);
    let tail_non_silent: u32 = if last_non_silent_cb.map_or(true, |l| l < tail_start) {
        0
    } else {
        // Recount — last_non_silent_cb already checked above, but be defensive
        1
    };
    assert_eq!(
        tail_non_silent, 0,
        "Recv plugin produced audio in the tail region (callbacks {tail_start}..{phase2_callbacks}) \
         after peer disconnect. Output must be fully silent after the residual interval drains."
    );

    eprintln!(
        "[disconnect] PASSED — audio stopped within {:?} callbacks of peer disconnect, \
         tail region ({tail_start}..{phase2_callbacks}) fully silent.",
        last_non_silent_cb,
    );

    // 6. Cleanup
    let recv_stopped = recv_proc.stop_processing();
    recv_host.deactivate(recv_stopped);
    send_host.leak();
    recv_host.leak();
}
