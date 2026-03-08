//! End-to-end test: real WAIL Send plugin → WebRTC → real WAIL Recv plugin.
//!
//! This is the most faithful "client A to client B" test: both ends use actual
//! compiled CLAP plugin binaries. Audio flows through the full stack:
//!
//!   [Real Send .clap]
//!     audio thread → IPC (WAIF frames) → mini_app_a
//!       → WebRTC DataChannel → mini_app_b
//!         → IPC (tag 0x01) → [Real Recv .clap]
//!           → FrameAssembler → Opus decode → ring buffer → DAW output
//!
//! No external services or DAW required: in-process signaling, localhost WebRTC.
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

/// Number of output ports for the recv plugin: 1 main + 31 aux stereo.
const NUM_OUTPUT_PORTS: usize = 32;

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

    // Transport must have IS_PLAYING set; otherwise nih_plug sets playing=false
    // and interleave_channels fills the input with silence instead of the sine wave.
    let transport = TransportEvent {
        header: EventHeader::new_core(0, EventFlags::empty()),
        flags: TransportFlags::IS_PLAYING | TransportFlags::HAS_TEMPO,
        song_pos_beats: BeatTime::from_int(0),
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
    };

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

    processor
        .process(
            &in_bufs,
            &mut out_buf_refs,
            &in_events,
            &mut out_events,
            Some(steady_time),
            None,
        )
        .expect("recv process() failed");

    out_bufs.into_iter().next().unwrap()[0].clone()
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[test]
fn send_and_recv_plugin_webrtc_e2e() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    // 1. Load both .clap binaries (CLAP main-thread affinity: all plugin calls stay here)
    let mut send_host = load_send();
    let mut recv_host = load_recv();

    // 2. Pick random IPC ports for the two mini_app sessions
    let send_ipc_port = common::random_port();
    let recv_ipc_port = common::random_port();

    // 3. Start a tokio runtime in a background thread for all async networking.
    //    Plugin process() calls are synchronous and stay on this thread.
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

    // 4. Start signaling + both mini_apps; wait for WebRTC to establish (~6 s on localhost).
    //    mini_app_a accepts the Send plugin's IPC connection and broadcasts audio over WebRTC.
    //    mini_app_b accepts the Recv plugin's IPC connection and forwards received audio to it.
    rt.block_on(async {
        let signaling_url = common::start_test_signaling_server().await;

        let url_a = signaling_url.clone();
        tokio::spawn(common::mini_app_session(
            send_ipc_port,
            url_a,
            "e2e-room".into(),
            "peer-a".into(),
            "test".into(),
        ));

        // Small delay so peer-a joins first (peer-a < peer-b lexicographically → initiates ICE)
        tokio::time::sleep(Duration::from_millis(100)).await;

        tokio::spawn(common::mini_app_session(
            recv_ipc_port,
            signaling_url,
            "e2e-room".into(),
            "peer-b".into(),
            "test".into(),
        ));

        // Wait for WebRTC DataChannels to open between the two mini_apps.
        // On localhost with no TURN relay this typically takes 1-3 s; 8 s is generous.
        tokio::time::sleep(Duration::from_secs(8)).await;
    });

    // 5. Activate send plugin → its IPC thread reads WAIL_IPC_ADDR and connects to mini_app_a.
    //    SAFETY: test binary is single-threaded at this point; no other thread reads this var.
    unsafe { std::env::set_var("WAIL_IPC_ADDR", format!("127.0.0.1:{send_ipc_port}")) };
    let send_stopped = send_host
        .activate(48000.0, 32, 4096)
        .expect("Failed to activate send plugin");
    let mut send_proc = send_stopped
        .start_processing()
        .expect("Failed to start send processing");

    // Small delay for send plugin's IPC thread to connect before recv plugin changes the var.
    std::thread::sleep(Duration::from_millis(200));

    // 6. Activate recv plugin → its IPC thread connects to mini_app_b.
    unsafe { std::env::set_var("WAIL_IPC_ADDR", format!("127.0.0.1:{recv_ipc_port}")) };
    let recv_stopped = recv_host
        .activate(48000.0, 32, 4096)
        .expect("Failed to activate recv plugin");
    let mut recv_proc = recv_stopped
        .start_processing()
        .expect("Failed to start recv processing");

    // Small delay for recv plugin's IPC thread to connect before driving audio.
    std::thread::sleep(Duration::from_millis(500));

    // 7. Drive both plugins in an interleaved loop for ~1 minute of audio playback.
    //
    //    Parameters:
    //      sample_rate = 48000, buf_size = 4096, BPM = 120, bars = 4, quantum = 4
    //      samples_per_interval = bars × quantum × 60 / BPM × sample_rate
    //                           = 4 × 4 × 60 / 120 × 48000 = 384,000
    //      callbacks_per_interval = 384,000 / 4096 ≈ 94
    //
    //    Because the test drives faster than real-time, the IPC→WebRTC→IPC pipeline
    //    has a 2-interval warmup (vs. 1-interval in production where the DAW paces
    //    callbacks at the audio clock rate). After the warmup, audio flows continuously
    //    with zero gap at each interval boundary — the ring buffer's crossfade swap is
    //    always ready because the previous interval's data arrived during the preceding
    //    94-callback window.
    //
    //    Callback budget for 1 minute of audio playback:
    //      - 2-interval pipeline warmup = 2 × 94 = 188 callbacks
    //      - 60 seconds of output     = 60 × 48000 / 4096 ≈ 703 callbacks
    //      - Total minimum            = 891 callbacks → use 950 for margin
    //
    //    Network I/O runs on tokio background threads concurrently with this loop.

    let buf_size: u32 = 4096;
    let num_callbacks: u64 = 950;
    let mut non_silent_buffers: u32 = 0;

    // Gap tracking: measure the longest run of consecutive silent buffers AFTER
    // the first non-silent buffer appears. Zero gap means seamless interval transitions.
    let mut in_audio_phase = false;
    let mut current_gap: u32 = 0;
    let mut max_gap: u32 = 0;

    // Per-interval stats and ZCR sums for temporal-alignment verification.
    // (index, non_silent, total, zcr_sum)
    let mut interval_stats: Vec<(u64, u32, u32, f64)> = Vec::new();
    let mut cur_interval = u64::MAX;
    let mut cur_interval_non_silent: u32 = 0;
    let mut cur_interval_total: u32 = 0;
    let mut cur_interval_zcr_sum: f64 = 0.0;

    for i in 0..num_callbacks {
        let steady_time = i * buf_size as u64;
        let interval_index = steady_time / 384_000;

        if interval_index != cur_interval {
            if cur_interval != u64::MAX {
                interval_stats.push((
                    cur_interval,
                    cur_interval_non_silent,
                    cur_interval_total,
                    cur_interval_zcr_sum,
                ));
            }
            cur_interval = interval_index;
            cur_interval_non_silent = 0;
            cur_interval_total = 0;
            cur_interval_zcr_sum = 0.0;
        }
        cur_interval_total += 1;

        // Tag each send interval with a distinct frequency so we can verify that
        // the recv side plays the correct interval's content in the correct window.
        let send_freq = interval_freq(interval_index);
        drive_send(&mut send_proc, buf_size, steady_time, send_freq);
        let out_l = drive_recv(&mut recv_proc, buf_size, steady_time);

        let energy = rms(&out_l);
        if energy > 0.001 {
            non_silent_buffers += 1;
            cur_interval_non_silent += 1;
            cur_interval_zcr_sum += zcr(&out_l, 48000) as f64;
            if in_audio_phase {
                max_gap = max_gap.max(current_gap);
                current_gap = 0;
            }
            in_audio_phase = true;
        } else if in_audio_phase {
            current_gap += 1;
        }
    }
    // Finalize last interval and any trailing gap
    if cur_interval != u64::MAX {
        interval_stats.push((
            cur_interval,
            cur_interval_non_silent,
            cur_interval_total,
            cur_interval_zcr_sum,
        ));
    }
    max_gap = max_gap.max(current_gap);

    // Find the pipeline lag (in intervals) by locating the first interval with audio.
    // In production at real-time speed this is 1 interval (NINJAM design); the test
    // drives ~14× faster so the pipeline has an extra interval of backlog → lag = 2.
    let lag = interval_stats
        .iter()
        .find(|(_, ns, _, _)| *ns > 0)
        .map(|(idx, _, _, _)| *idx)
        .unwrap_or(0);

    // Log per-interval breakdown with frequency tag
    for (idx, non_silent, total, zcr_sum) in &interval_stats {
        let pct = *non_silent as f64 / *total as f64 * 100.0;
        let avg_zcr = if *non_silent > 0 { *zcr_sum / *non_silent as f64 } else { 0.0 };
        let detected = if avg_zcr > 550.0 { "~880Hz" } else if avg_zcr > 0.1 { "~220Hz" } else { "(silent)" };
        let send_idx = idx.saturating_sub(lag);
        let expected_freq = interval_freq(send_idx);
        eprintln!(
            "[test]   Recv interval {idx:2}: {non_silent:3}/{total:3} ({pct:.0}%)  \
             ZCR≈{avg_zcr:.0}Hz ({detected})  [from send interval {send_idx}, sent {expected_freq:.0}Hz]"
        );
    }

    let max_gap_ms = max_gap as f64 * buf_size as f64 / 48000.0 * 1000.0;
    eprintln!(
        "[test] E2E summary: non_silent={non_silent_buffers}/{num_callbacks}, \
         lag={lag} intervals, max_gap={max_gap} buffers ({max_gap_ms:.0}ms)"
    );

    // 8a. Assert ≥1 minute of contiguous non-silent output with no audible gaps.
    assert!(
        non_silent_buffers >= 700,
        "Expected ≥700 non-silent buffers (≈60s of audio) via the full \
         Send→WebRTC→Recv path, got {non_silent_buffers}/{num_callbacks}."
    );
    assert!(
        max_gap <= 2,
        "Detected a gap of {max_gap} consecutive silent buffers ({max_gap_ms:.0}ms) — \
         interval-boundary transitions must be seamless (≤ 2 buffers / ~170ms)."
    );

    // 8b. Assert temporal alignment: recv interval N plays send interval N−lag's content.
    //
    //     Each send interval is tagged with a distinct frequency (220Hz = even, 880Hz = odd).
    //     Since lag=2 (even), parity is preserved: recv even intervals should play 220Hz,
    //     recv odd intervals should play 880Hz.
    //
    //     We verify each fully-covered recv interval (≥ 75% non-silent).  The ZCR
    //     threshold of 550 Hz sits midway between 220 Hz and 880 Hz.
    //
    //     This catches bugs where the ring buffer plays the wrong interval (off-by-one
    //     in the swap logic) or where interval content is mixed across boundaries.
    let zcr_threshold = (FREQ_EVEN + FREQ_ODD) / 2.0; // 550 Hz
    for (recv_idx, non_silent, total, zcr_sum) in &interval_stats {
        // Only check intervals that are substantially non-silent and fully inside the run
        if (*non_silent as f64) < (*total as f64 * 0.75) { continue; }
        if *recv_idx < lag { continue; }

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

    eprintln!(
        "[test] PASSED — Send→WebRTC→Recv: {non_silent_buffers} non-silent buffers, \
         max_gap={max_gap} ({max_gap_ms:.0}ms), temporal alignment verified across {} intervals.",
        interval_stats.iter().filter(|(idx, ns, total, _)| {
            *idx >= lag && (*ns as f64) >= (*total as f64 * 0.75)
        }).count()
    );

    // 9. Stop and deactivate (order matters: stop_processing before deactivate)
    let send_stopped = send_proc.stop_processing();
    send_host.deactivate(send_stopped);

    let recv_stopped = recv_proc.stop_processing();
    recv_host.deactivate(recv_stopped);

    // 10. Leak both hosts to prevent the .clap dylibs from unloading while background
    //     IPC threads are still running (same pattern as all other wail-plugin-test tests).
    send_host.leak();
    recv_host.leak();
}
