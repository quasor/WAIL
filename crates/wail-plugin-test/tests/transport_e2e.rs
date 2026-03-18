//! End-to-end tests for DAW transport state transitions.
//!
//! These tests verify that audio flows correctly when transport starts/stops
//! at various points relative to room joining and WebRTC connection.
//!
//! Scenarios covered:
//!   1. Join room, then start transport → audio must begin flowing
//!   2. Start transport, then join room → audio must begin flowing
//!   3. Stop transport mid-session, then restart → audio must resume
//!   4. Late join with transport initially stopped → audio begins after play
//!   5. Tempo change mid-session → audio continues at new interval length
//!
//! **Must run with `--test-threads=1`**: all tests mutate the process-global
//! `WAIL_IPC_ADDR` env var and leak plugin instances to prevent dylib unload.
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
const NUM_OUTPUT_PORTS: usize = 16;

// ---------------------------------------------------------------------------
// Transport helpers
// ---------------------------------------------------------------------------

/// Build a transport event with IS_PLAYING set.
fn make_transport_playing(steady_time: u64, bpm: f64) -> TransportEvent {
    let beats = steady_time as f64 / (48000.0 * 60.0 / bpm); // samples → beats
    TransportEvent {
        header: EventHeader::new_core(0, EventFlags::empty()),
        flags: TransportFlags::IS_PLAYING
            | TransportFlags::HAS_TEMPO
            | TransportFlags::HAS_BEATS_TIMELINE,
        song_pos_beats: BeatTime::from_float(beats),
        song_pos_seconds: SecondsTime::from_int(0),
        tempo: bpm,
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

/// Build a transport event with transport STOPPED (no IS_PLAYING).
fn make_transport_stopped(_steady_time: u64, bpm: f64) -> TransportEvent {
    TransportEvent {
        header: EventHeader::new_core(0, EventFlags::empty()),
        flags: TransportFlags::HAS_TEMPO | TransportFlags::HAS_BEATS_TIMELINE,
        // Beat position frozen at 0 when stopped (DAW doesn't advance)
        song_pos_beats: BeatTime::from_float(0.0),
        song_pos_seconds: SecondsTime::from_int(0),
        tempo: bpm,
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

// ---------------------------------------------------------------------------
// Plugin loading
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Process helpers (transport-aware)
// ---------------------------------------------------------------------------

fn drive_send_transport(
    processor: &mut StartedPluginAudioProcessor<wail_plugin_test::TestHost>,
    buf_size: u32,
    steady_time: u64,
    freq: f32,
    transport: &TransportEvent,
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

    processor
        .process(
            &in_bufs,
            &mut out_bufs,
            &in_events,
            &mut out_events,
            Some(steady_time),
            Some(transport),
        )
        .expect("send process() failed");
}

fn drive_recv_transport(
    processor: &mut StartedPluginAudioProcessor<wail_plugin_test::TestHost>,
    buf_size: u32,
    steady_time: u64,
    transport: &TransportEvent,
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
            Some(transport),
        )
        .expect("recv process() failed");

    out_bufs.into_iter().next().unwrap()[0].clone()
}

// ---------------------------------------------------------------------------
// Test 1: Join room, then start transport
// ---------------------------------------------------------------------------

/// Verify audio flows when peers join a room first, then start transport later.
///
/// Timeline:
///   t=0      — Both peers join room, WebRTC established
///   Phase 1  — Drive 100 callbacks with transport STOPPED → send silence
///   Phase 2  — Start transport (IS_PLAYING), drive real-time for ~30s
///              → audio must begin flowing within 3 intervals
///
/// This catches bugs where the plugin's interval boundary detection or
/// ring buffer fails to initialize correctly when transport starts after
/// the IPC/WebRTC connection is already established.
#[test]
fn join_room_then_start_transport_e2e() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    let mut send_host = load_send();
    let mut recv_host = load_recv();

    let send_ipc_port = common::random_port();
    let recv_ipc_port = common::random_port();

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

    // 1. Start signaling + both mini_apps; wait for WebRTC to establish.
    rt.block_on(async {
        let signaling_url = common::start_test_signaling_server().await;

        let url_a = signaling_url.clone();
        tokio::spawn(common::mini_app_session(
            send_ipc_port,
            url_a,
            "join-then-play-room".into(),
            "peer-a".into(),
            "test".into(),
        ));

        tokio::time::sleep(Duration::from_millis(100)).await;

        tokio::spawn(common::mini_app_session(
            recv_ipc_port,
            signaling_url,
            "join-then-play-room".into(),
            "peer-b".into(),
            "test".into(),
        ));

        tokio::time::sleep(Duration::from_secs(8)).await;
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

    // 3. Phase 1: Drive with transport STOPPED for ~8 seconds (100 callbacks).
    //    The send plugin should interleave silence (playing=false).
    let stopped_callbacks: u64 = 100;
    for i in 0..stopped_callbacks {
        let steady = i * buf_size as u64;
        let transport = make_transport_stopped(steady, 120.0);
        drive_send_transport(&mut send_proc, buf_size, steady, 440.0, &transport);
        drive_recv_transport(&mut recv_proc, buf_size, steady, &transport);
    }

    eprintln!("[join-then-play] Phase 1 done: {stopped_callbacks} callbacks with transport stopped");

    // 4. Phase 2: Start transport (IS_PLAYING), drive at real-time speed for ~30s.
    //    The steady_time continues from where we left off, but now beat position
    //    advances (IS_PLAYING set).
    let playing_callbacks: u64 = 400;
    let sleep_per_callback = Duration::from_secs_f64(buf_size as f64 / 48000.0);
    let mut non_silent: u32 = 0;
    let mut first_audio: Option<u64> = None;
    let wall_start = std::time::Instant::now();

    for i in 0..playing_callbacks {
        let steady = (stopped_callbacks + i) * buf_size as u64;
        let transport = make_transport_playing(steady, 120.0);
        drive_send_transport(&mut send_proc, buf_size, steady, 440.0, &transport);
        let out_l = drive_recv_transport(&mut recv_proc, buf_size, steady, &transport);

        if rms(&out_l) > 0.001 {
            non_silent += 1;
            first_audio.get_or_insert(i);
        }

        let expected_elapsed = sleep_per_callback * (i as u32 + 1);
        if let Some(remaining) = expected_elapsed.checked_sub(wall_start.elapsed()) {
            std::thread::sleep(remaining);
        }
    }

    eprintln!(
        "[join-then-play] Phase 2: {non_silent}/{playing_callbacks} non-silent, first audio at cb {:?}",
        first_audio,
    );

    // 5. Assertions
    // Audio must begin within 3 intervals (~282 callbacks) of transport starting.
    const MAX_WARMUP: u64 = 282;
    if let Some(first) = first_audio {
        assert!(
            first <= MAX_WARMUP,
            "Audio took too long to start after transport play: first at callback {first}, \
             expected within {MAX_WARMUP}. Transport start after room join may be broken."
        );
    } else {
        panic!(
            "No audio received after starting transport. \
             The plugin may not handle transport start after room join correctly."
        );
    }

    // Must have substantial audio after warmup
    let expected_min = (playing_callbacks.saturating_sub(MAX_WARMUP) as f64 * 0.70) as u32;
    assert!(
        non_silent >= expected_min,
        "Expected ≥{expected_min} non-silent buffers after warmup, got {non_silent}/{playing_callbacks}"
    );

    eprintln!("[join-then-play] PASSED");

    // 6. Cleanup
    let send_stopped = send_proc.stop_processing();
    send_host.deactivate(send_stopped);
    let recv_stopped = recv_proc.stop_processing();
    recv_host.deactivate(recv_stopped);
    send_host.leak();
    recv_host.leak();
}

// ---------------------------------------------------------------------------
// Test 2: Start transport, then join room
// ---------------------------------------------------------------------------

/// Verify audio flows when transport is already playing before peers join.
///
/// Timeline:
///   t=0      — Plugins activated, transport IS_PLAYING, but no room yet
///   Phase 1  — Drive 200 callbacks with transport playing (no WebRTC peer)
///   Phase 2  — peer-b joins the room, WebRTC establishes (~8s)
///   Phase 3  — Drive both for 750 callbacks → audio must flow bidirectionally
///
/// This catches bugs where the send plugin accumulates stale interval data
/// before WebRTC is connected and the initial interval boundary gets confused.
#[test]
fn start_transport_then_join_room_e2e() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    let mut send_a_host = load_send();
    let mut recv_a_host = load_recv();
    let mut send_b_host = load_send();
    let mut recv_b_host = load_recv();

    let port_a = common::random_port();
    let port_b = common::random_port();

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

    // 1. Start signaling, peer-a joins alone.
    let signaling_url = rt.block_on(async {
        let url = common::start_test_signaling_server().await;
        tokio::spawn(common::mini_app_session(
            port_a,
            url.clone(),
            "play-then-join-room".into(),
            "peer-a".into(),
            "test".into(),
        ));
        tokio::time::sleep(Duration::from_secs(2)).await;
        url
    });

    // 2. Activate peer-a plugins and drive with transport PLAYING (no remote peer yet).
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

    // Phase 1: peer-a plays alone for 200 callbacks (~17s, 2+ intervals).
    let solo_callbacks: u64 = 200;
    for i in 0..solo_callbacks {
        let steady = i * buf_size as u64;
        let transport = make_transport_playing(steady, 120.0);
        drive_send_transport(&mut send_a_proc, buf_size, steady, 440.0, &transport);
        drive_recv_transport(&mut recv_a_proc, buf_size, steady, &transport);
    }

    eprintln!("[play-then-join] Phase 1 done: peer-a played {solo_callbacks} callbacks alone");

    // 3. peer-b joins; wait for WebRTC.
    rt.block_on(async {
        tokio::spawn(common::mini_app_session(
            port_b,
            signaling_url,
            "play-then-join-room".into(),
            "peer-b".into(),
            "test".into(),
        ));
        tokio::time::sleep(Duration::from_secs(8)).await;
    });

    // 4. Activate peer-b plugins.
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

    // 5. Phase 2: Both peers drive audio for 750 callbacks.
    let both_callbacks: u64 = 750;
    let mut recv_a_non_silent: u32 = 0;
    let mut recv_b_non_silent: u32 = 0;
    let mut recv_a_first: Option<u64> = None;
    let mut recv_b_first: Option<u64> = None;

    for i in 0..both_callbacks {
        let steady = (solo_callbacks + i) * buf_size as u64;
        let transport = make_transport_playing(steady, 120.0);

        drive_send_transport(&mut send_a_proc, buf_size, steady, 440.0, &transport);
        drive_send_transport(&mut send_b_proc, buf_size, steady, 440.0, &transport);

        let out_a = drive_recv_transport(&mut recv_a_proc, buf_size, steady, &transport);
        let out_b = drive_recv_transport(&mut recv_b_proc, buf_size, steady, &transport);

        if rms(&out_a) > 0.001 {
            recv_a_non_silent += 1;
            recv_a_first.get_or_insert(i);
        }
        if rms(&out_b) > 0.001 {
            recv_b_non_silent += 1;
            recv_b_first.get_or_insert(i);
        }
    }

    eprintln!(
        "[play-then-join] Phase 2: recv_a={recv_a_non_silent}/{both_callbacks} (first {:?}), \
         recv_b={recv_b_non_silent}/{both_callbacks} (first {:?})",
        recv_a_first, recv_b_first,
    );

    // 6. Assertions: both sides must have received audio.
    const MAX_WARMUP: u64 = 282;
    assert!(
        recv_a_non_silent >= 400,
        "peer-a should receive audio from peer-b: got {recv_a_non_silent}/{both_callbacks}"
    );
    assert!(
        recv_b_non_silent >= 400,
        "peer-b should receive audio from peer-a: got {recv_b_non_silent}/{both_callbacks}"
    );

    if let Some(first) = recv_a_first {
        assert!(first <= MAX_WARMUP, "peer-a waited too long for audio: {first}");
    } else {
        panic!("peer-a never received audio from peer-b");
    }
    if let Some(first) = recv_b_first {
        assert!(first <= MAX_WARMUP, "peer-b waited too long for audio: {first}");
    } else {
        panic!("peer-b never received audio from peer-a");
    }

    eprintln!("[play-then-join] PASSED");

    // 7. Cleanup
    let s = send_a_proc.stop_processing(); send_a_host.deactivate(s);
    let s = recv_a_proc.stop_processing(); recv_a_host.deactivate(s);
    let s = send_b_proc.stop_processing(); send_b_host.deactivate(s);
    let s = recv_b_proc.stop_processing(); recv_b_host.deactivate(s);
    send_a_host.leak(); recv_a_host.leak();
    send_b_host.leak(); recv_b_host.leak();
}

// ---------------------------------------------------------------------------
// Test 3: Stop transport mid-session, then restart
// ---------------------------------------------------------------------------

/// Verify that stopping transport mid-session silences audio, and restarting
/// transport resumes audio flow.
///
/// Timeline:
///   Phase 1 — Both peers playing, audio flowing (~30s real-time)
///   Phase 2 — Send-side stops transport (IS_PLAYING cleared) → sends silence
///   Phase 3 — Send-side restarts transport → audio must resume
///
/// This catches bugs where the ring buffer or IPC pipeline gets into an
/// inconsistent state after a stop/start cycle (e.g., stale interval index,
/// frame_number not reset, encoder state corruption).
#[test]
fn stop_and_restart_transport_e2e() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    let mut send_host = load_send();
    let mut recv_host = load_recv();

    let send_ipc_port = common::random_port();
    let recv_ipc_port = common::random_port();

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

    // 1. Set up room with WebRTC
    rt.block_on(async {
        let signaling_url = common::start_test_signaling_server().await;

        let url_a = signaling_url.clone();
        tokio::spawn(common::mini_app_session(
            send_ipc_port,
            url_a,
            "stop-restart-room".into(),
            "peer-a".into(),
            "test".into(),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;

        tokio::spawn(common::mini_app_session(
            recv_ipc_port,
            signaling_url,
            "stop-restart-room".into(),
            "peer-b".into(),
            "test".into(),
        ));
        tokio::time::sleep(Duration::from_secs(8)).await;
    });

    let buf_size: u32 = 4096;
    unsafe { std::env::set_var("WAIL_IPC_ADDR", format!("127.0.0.1:{send_ipc_port}")) };
    let send_stopped = send_host
        .activate(48000.0, 32, 4096)
        .expect("Failed to activate send");
    let mut send_proc = send_stopped
        .start_processing()
        .expect("Failed to start send");
    std::thread::sleep(Duration::from_millis(200));

    unsafe { std::env::set_var("WAIL_IPC_ADDR", format!("127.0.0.1:{recv_ipc_port}")) };
    let recv_stopped = recv_host
        .activate(48000.0, 32, 4096)
        .expect("Failed to activate recv");
    let mut recv_proc = recv_stopped
        .start_processing()
        .expect("Failed to start recv");
    std::thread::sleep(Duration::from_millis(500));

    let sleep_per = Duration::from_secs_f64(buf_size as f64 / 48000.0);

    // 2. Phase 1: Normal playing for ~30s (450 callbacks).
    let phase1_callbacks: u64 = 450;
    let mut phase1_non_silent: u32 = 0;
    let wall_start = std::time::Instant::now();

    for i in 0..phase1_callbacks {
        let steady = i * buf_size as u64;
        let transport = make_transport_playing(steady, 120.0);
        drive_send_transport(&mut send_proc, buf_size, steady, 440.0, &transport);
        let out_l = drive_recv_transport(&mut recv_proc, buf_size, steady, &transport);

        if rms(&out_l) > 0.001 {
            phase1_non_silent += 1;
        }

        let expected_elapsed = sleep_per * (i as u32 + 1);
        if let Some(remaining) = expected_elapsed.checked_sub(wall_start.elapsed()) {
            std::thread::sleep(remaining);
        }
    }

    eprintln!("[stop-restart] Phase 1: {phase1_non_silent}/{phase1_callbacks} non-silent");
    assert!(
        phase1_non_silent >= 300,
        "Phase 1 failed: expected substantial audio, got {phase1_non_silent}/{phase1_callbacks}"
    );

    // 3. Phase 2: Stop transport for 2 intervals (188 callbacks ~16s).
    //    The send plugin fills input with silence when playing=false.
    //    After the pipeline drains (1 interval), recv output should go silent.
    let phase2_callbacks: u64 = 188;
    let mut phase2_tail_non_silent: u32 = 0;
    let phase2_wall_start = std::time::Instant::now();

    for i in 0..phase2_callbacks {
        let steady = (phase1_callbacks + i) * buf_size as u64;
        let transport = make_transport_stopped(steady, 120.0);
        drive_send_transport(&mut send_proc, buf_size, steady, 440.0, &transport);
        let out_l = drive_recv_transport(&mut recv_proc, buf_size, steady, &transport);

        // Only check the second half (after pipeline drains)
        if i >= 94 && rms(&out_l) > 0.001 {
            phase2_tail_non_silent += 1;
        }

        let expected_elapsed = sleep_per * (i as u32 + 1);
        if let Some(remaining) = expected_elapsed.checked_sub(phase2_wall_start.elapsed()) {
            std::thread::sleep(remaining);
        }
    }

    eprintln!(
        "[stop-restart] Phase 2: tail non-silent={phase2_tail_non_silent}/94 (should be ~0)"
    );
    // Allow some residual audio from the pipeline draining
    assert!(
        phase2_tail_non_silent <= 10,
        "After stopping transport, expected near-silence in tail region, \
         got {phase2_tail_non_silent}/94 non-silent buffers"
    );

    // 4. Phase 3: Restart transport — audio must resume.
    //    Beat position resumes from where it "would have been" if playing continuously.
    //    (In a real DAW, the user hits play and beat position starts advancing again.)
    let phase3_callbacks: u64 = 400;
    let phase3_start_sample = (phase1_callbacks + phase2_callbacks) * buf_size as u64;
    let mut phase3_non_silent: u32 = 0;
    let mut phase3_first_audio: Option<u64> = None;
    let phase3_wall_start = std::time::Instant::now();

    for i in 0..phase3_callbacks {
        let steady = phase3_start_sample + i * buf_size as u64;
        let transport = make_transport_playing(steady, 120.0);
        drive_send_transport(&mut send_proc, buf_size, steady, 440.0, &transport);
        let out_l = drive_recv_transport(&mut recv_proc, buf_size, steady, &transport);

        if rms(&out_l) > 0.001 {
            phase3_non_silent += 1;
            phase3_first_audio.get_or_insert(i);
        }

        let expected_elapsed = sleep_per * (i as u32 + 1);
        if let Some(remaining) = expected_elapsed.checked_sub(phase3_wall_start.elapsed()) {
            std::thread::sleep(remaining);
        }
    }

    eprintln!(
        "[stop-restart] Phase 3: {phase3_non_silent}/{phase3_callbacks} non-silent, \
         first audio at cb {:?}",
        phase3_first_audio,
    );

    // Audio must resume within 3 intervals of transport restart
    const MAX_WARMUP: u64 = 282;
    if let Some(first) = phase3_first_audio {
        assert!(
            first <= MAX_WARMUP,
            "Audio took too long to resume after transport restart: first at {first}, \
             expected within {MAX_WARMUP}. Stop/start cycle may corrupt pipeline state."
        );
    } else {
        panic!(
            "No audio after transport restart. \
             The pipeline does not recover from transport stop/start."
        );
    }

    let expected_min = (phase3_callbacks.saturating_sub(MAX_WARMUP) as f64 * 0.70) as u32;
    assert!(
        phase3_non_silent >= expected_min,
        "Expected ≥{expected_min} non-silent buffers after restart warmup, \
         got {phase3_non_silent}/{phase3_callbacks}"
    );

    eprintln!("[stop-restart] PASSED");

    // 5. Cleanup
    let s = send_proc.stop_processing(); send_host.deactivate(s);
    let s = recv_proc.stop_processing(); recv_host.deactivate(s);
    send_host.leak();
    recv_host.leak();
}

// ---------------------------------------------------------------------------
// Test 4: Late join with transport initially stopped
// ---------------------------------------------------------------------------

/// Verify that a late-joining peer with transport stopped can start playing
/// and exchange audio after starting transport.
///
/// Timeline:
///   t=0      — peer-a joins and plays (transport running)
///   t≈17s    — peer-b joins with transport STOPPED
///   Phase 1  — Both drive callbacks; peer-b has stopped transport (100 callbacks)
///   Phase 2  — peer-b starts transport → both must exchange audio
///
/// This catches the intersection of late-join + transport-stopped: the ring
/// buffer and interval tracking must initialize correctly even when the new
/// peer's DAW hasn't started playing yet.
#[test]
fn late_join_stopped_transport_e2e() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    let mut send_a_host = load_send();
    let mut recv_a_host = load_recv();
    let mut send_b_host = load_send();
    let mut recv_b_host = load_recv();

    let port_a = common::random_port();
    let port_b = common::random_port();

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

    // 1. peer-a joins alone, starts playing
    let signaling_url = rt.block_on(async {
        let url = common::start_test_signaling_server().await;
        tokio::spawn(common::mini_app_session(
            port_a,
            url.clone(),
            "late-join-stopped-room".into(),
            "peer-a".into(),
            "test".into(),
        ));
        tokio::time::sleep(Duration::from_secs(2)).await;
        url
    });

    let buf_size: u32 = 4096;

    // Activate peer-a
    unsafe { std::env::set_var("WAIL_IPC_ADDR", format!("127.0.0.1:{port_a}")) };
    let sa = send_a_host.activate(48000.0, 32, 4096).expect("activate send-a");
    let mut send_a = sa.start_processing().expect("start send-a");
    std::thread::sleep(Duration::from_millis(200));
    let ra = recv_a_host.activate(48000.0, 32, 4096).expect("activate recv-a");
    let mut recv_a = ra.start_processing().expect("start recv-a");
    std::thread::sleep(Duration::from_millis(500));

    // peer-a plays alone for 200 callbacks
    let solo_callbacks: u64 = 200;
    for i in 0..solo_callbacks {
        let steady = i * buf_size as u64;
        let transport = make_transport_playing(steady, 120.0);
        drive_send_transport(&mut send_a, buf_size, steady, 440.0, &transport);
        drive_recv_transport(&mut recv_a, buf_size, steady, &transport);
    }

    // 2. peer-b joins (WebRTC establishes ~8s)
    rt.block_on(async {
        tokio::spawn(common::mini_app_session(
            port_b,
            signaling_url,
            "late-join-stopped-room".into(),
            "peer-b".into(),
            "test".into(),
        ));
        tokio::time::sleep(Duration::from_secs(8)).await;
    });

    // Activate peer-b
    unsafe { std::env::set_var("WAIL_IPC_ADDR", format!("127.0.0.1:{port_b}")) };
    let sb = send_b_host.activate(48000.0, 32, 4096).expect("activate send-b");
    let mut send_b = sb.start_processing().expect("start send-b");
    std::thread::sleep(Duration::from_millis(200));
    let rb = recv_b_host.activate(48000.0, 32, 4096).expect("activate recv-b");
    let mut recv_b = rb.start_processing().expect("start recv-b");
    std::thread::sleep(Duration::from_millis(500));

    // 3. Phase 1: peer-a plays, peer-b has transport STOPPED (100 callbacks).
    //    peer-b sends silence; peer-a may or may not receive peer-b's silence.
    let stopped_phase: u64 = 100;
    for i in 0..stopped_phase {
        let steady = (solo_callbacks + i) * buf_size as u64;
        let transport_a = make_transport_playing(steady, 120.0);
        let transport_b = make_transport_stopped(steady, 120.0);

        drive_send_transport(&mut send_a, buf_size, steady, 440.0, &transport_a);
        drive_send_transport(&mut send_b, buf_size, steady, 440.0, &transport_b);
        drive_recv_transport(&mut recv_a, buf_size, steady, &transport_a);
        drive_recv_transport(&mut recv_b, buf_size, steady, &transport_b);
    }

    eprintln!("[late-join-stopped] Phase 1 done: peer-b drove {stopped_phase} callbacks stopped");

    // 4. Phase 2: peer-b starts transport → both must exchange audio.
    let playing_callbacks: u64 = 750;
    let mut recv_a_ns: u32 = 0;
    let mut recv_b_ns: u32 = 0;
    let mut recv_b_first: Option<u64> = None;

    for i in 0..playing_callbacks {
        let steady = (solo_callbacks + stopped_phase + i) * buf_size as u64;
        let transport = make_transport_playing(steady, 120.0);

        drive_send_transport(&mut send_a, buf_size, steady, 440.0, &transport);
        drive_send_transport(&mut send_b, buf_size, steady, 440.0, &transport);
        let out_a = drive_recv_transport(&mut recv_a, buf_size, steady, &transport);
        let out_b = drive_recv_transport(&mut recv_b, buf_size, steady, &transport);

        if rms(&out_a) > 0.001 { recv_a_ns += 1; }
        if rms(&out_b) > 0.001 {
            recv_b_ns += 1;
            recv_b_first.get_or_insert(i);
        }
    }

    eprintln!(
        "[late-join-stopped] Phase 2: recv_a={recv_a_ns}/{playing_callbacks}, \
         recv_b={recv_b_ns}/{playing_callbacks} (first {:?})",
        recv_b_first,
    );

    // peer-b (the late joiner who was stopped) must now receive audio
    const MAX_WARMUP: u64 = 282;
    assert!(
        recv_b_ns >= 400,
        "peer-b (late join, was stopped) should receive audio: got {recv_b_ns}/{playing_callbacks}"
    );
    if let Some(first) = recv_b_first {
        assert!(
            first <= MAX_WARMUP,
            "peer-b audio took too long: first at {first}, expected within {MAX_WARMUP}"
        );
    } else {
        panic!("peer-b never received audio after starting transport");
    }

    // peer-a should also receive peer-b's audio (bidirectional)
    assert!(
        recv_a_ns >= 400,
        "peer-a should receive audio from peer-b: got {recv_a_ns}/{playing_callbacks}"
    );

    eprintln!("[late-join-stopped] PASSED");

    let s = send_a.stop_processing(); send_a_host.deactivate(s);
    let s = recv_a.stop_processing(); recv_a_host.deactivate(s);
    let s = send_b.stop_processing(); send_b_host.deactivate(s);
    let s = recv_b.stop_processing(); recv_b_host.deactivate(s);
    send_a_host.leak(); recv_a_host.leak();
    send_b_host.leak(); recv_b_host.leak();
}

// ---------------------------------------------------------------------------
// Test 5: Tempo change mid-session
// ---------------------------------------------------------------------------

/// Verify that changing tempo mid-session doesn't break audio flow.
///
/// Timeline:
///   Phase 1 — Both peers at 120 BPM, audio flowing (~30s real-time)
///   Phase 2 — Switch to 90 BPM, audio must continue flowing
///
/// At 90 BPM, interval length = 4 bars × 4 beats × 60/90 × 48000 = 512,000 samples
/// (vs 384,000 at 120 BPM). This tests that `bridge.update_config()` correctly
/// recalculates interval boundaries and the ring buffer doesn't get confused
/// by the different interval length.
#[test]
fn tempo_change_mid_session_e2e() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();

    let mut send_host = load_send();
    let mut recv_host = load_recv();

    let send_ipc_port = common::random_port();
    let recv_ipc_port = common::random_port();

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

    rt.block_on(async {
        let signaling_url = common::start_test_signaling_server().await;

        let url_a = signaling_url.clone();
        tokio::spawn(common::mini_app_session(
            send_ipc_port,
            url_a,
            "tempo-change-room".into(),
            "peer-a".into(),
            "test".into(),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;

        tokio::spawn(common::mini_app_session(
            recv_ipc_port,
            signaling_url,
            "tempo-change-room".into(),
            "peer-b".into(),
            "test".into(),
        ));
        tokio::time::sleep(Duration::from_secs(8)).await;
    });

    let buf_size: u32 = 4096;
    unsafe { std::env::set_var("WAIL_IPC_ADDR", format!("127.0.0.1:{send_ipc_port}")) };
    let send_stopped = send_host.activate(48000.0, 32, 4096).expect("activate send");
    let mut send_proc = send_stopped.start_processing().expect("start send");
    std::thread::sleep(Duration::from_millis(200));

    unsafe { std::env::set_var("WAIL_IPC_ADDR", format!("127.0.0.1:{recv_ipc_port}")) };
    let recv_stopped = recv_host.activate(48000.0, 32, 4096).expect("activate recv");
    let mut recv_proc = recv_stopped.start_processing().expect("start recv");
    std::thread::sleep(Duration::from_millis(500));

    let sleep_per = Duration::from_secs_f64(buf_size as f64 / 48000.0);

    // Phase 1: 120 BPM for ~30s (450 callbacks)
    let phase1_callbacks: u64 = 450;
    let mut phase1_ns: u32 = 0;
    let wall_start = std::time::Instant::now();

    for i in 0..phase1_callbacks {
        let steady = i * buf_size as u64;
        let transport = make_transport_playing(steady, 120.0);
        drive_send_transport(&mut send_proc, buf_size, steady, 440.0, &transport);
        let out_l = drive_recv_transport(&mut recv_proc, buf_size, steady, &transport);

        if rms(&out_l) > 0.001 { phase1_ns += 1; }

        let expected = sleep_per * (i as u32 + 1);
        if let Some(r) = expected.checked_sub(wall_start.elapsed()) {
            std::thread::sleep(r);
        }
    }

    eprintln!("[tempo-change] Phase 1 (120 BPM): {phase1_ns}/{phase1_callbacks} non-silent");
    assert!(
        phase1_ns >= 300,
        "Phase 1 failed: expected substantial audio at 120 BPM, got {phase1_ns}/{phase1_callbacks}"
    );

    // Phase 2: Switch to 90 BPM, run for ~40s (450 callbacks).
    //
    // At 90 BPM, one interval = 512,000 samples ≈ 125 callbacks.
    // 450 callbacks ≈ 3.6 intervals at the new tempo.
    // Pipeline warmup at the new tempo is ~2 intervals = 250 callbacks.
    let phase2_callbacks: u64 = 450;
    let phase2_start = phase1_callbacks * buf_size as u64;
    let mut phase2_ns: u32 = 0;
    let mut phase2_first: Option<u64> = None;
    let phase2_wall = std::time::Instant::now();

    for i in 0..phase2_callbacks {
        let steady = phase2_start + i * buf_size as u64;
        let transport = make_transport_playing(steady, 90.0);
        drive_send_transport(&mut send_proc, buf_size, steady, 440.0, &transport);
        let out_l = drive_recv_transport(&mut recv_proc, buf_size, steady, &transport);

        if rms(&out_l) > 0.001 {
            phase2_ns += 1;
            phase2_first.get_or_insert(i);
        }

        let expected = sleep_per * (i as u32 + 1);
        if let Some(r) = expected.checked_sub(phase2_wall.elapsed()) {
            std::thread::sleep(r);
        }
    }

    eprintln!(
        "[tempo-change] Phase 2 (90 BPM): {phase2_ns}/{phase2_callbacks} non-silent, \
         first at cb {:?}",
        phase2_first,
    );

    // Audio must continue after tempo change. Allow up to 3 intervals of warmup
    // at the new tempo (≈ 375 callbacks at 90 BPM — generous).
    // But we should see audio well before that since the pipeline was already warm.
    if let Some(first) = phase2_first {
        assert!(
            first <= 375,
            "Audio took too long to resume after tempo change: first at {first}. \
             Tempo change may break interval boundary tracking."
        );
    } else {
        panic!(
            "No audio after tempo change to 90 BPM. \
             bridge.update_config() may not handle tempo changes correctly."
        );
    }

    // Should have substantial audio (at least some after warmup)
    assert!(
        phase2_ns >= 50,
        "Expected audio after tempo change, got only {phase2_ns}/{phase2_callbacks}"
    );

    eprintln!("[tempo-change] PASSED");

    let s = send_proc.stop_processing(); send_host.deactivate(s);
    let s = recv_proc.stop_processing(); recv_host.deactivate(s);
    send_host.leak();
    recv_host.leak();
}
