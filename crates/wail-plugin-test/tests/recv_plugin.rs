//! End-to-end tests for the WAIL Recv CLAP plugin.
//!
//! Loads the real `.clap` binary, verifies lifecycle and output behavior.
//!
//! All scenarios run in a single test to avoid loading the `.clap` dylib
//! on multiple threads — CLAP plugins have main-thread affinity for
//! `clap_entry.init()`.
//!
//! Requires: `cargo xtask build-plugin` before running.

use std::io::Write as _;
use std::time::Duration;

use clack_host::prelude::*;
use wail_audio::{AudioDecoder, AudioEncoder, IPC_ROLE_RECV};
use wail_plugin_test::*;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("debug")
        .with_test_writer()
        .try_init();
}

const RECV_CLAP_ID: &str = "com.wail.recv";

fn load_recv_plugin() -> ClapTestHost {
    let path = find_plugin_bundle("wail-plugin-recv");
    assert!(
        path.exists(),
        "Plugin bundle not found at {}. Run `cargo xtask build-plugin` first.",
        path.display()
    );
    unsafe { ClapTestHost::load(&path, RECV_CLAP_ID).expect("Failed to load recv plugin") }
}

/// Number of output ports matching the recv plugin's default layout:
/// 1 main stereo + 31 aux stereo (per-peer/stream) = 32 total.
const NUM_OUTPUT_PORTS: usize = 32;

fn process_one_buffer(
    processor: &mut StartedPluginAudioProcessor<TestHost>,
    num_frames: u32,
    steady_time: u64,
) -> (ProcessStatus, Vec<f32>, Vec<f32>) {
    let n = num_frames as usize;
    let mut input_left = vec![0.0f32; n];
    let mut input_right = vec![0.0f32; n];

    // Pre-allocate all output channel buffers: [port_index] -> [left, right]
    // Port 0 is main output, ports 1-15 are aux (per-peer routing).
    // nih_plug requires the host to provide all ports declared by the
    // active audio layout, otherwise it silently skips process().
    let mut out_bufs: Vec<[Vec<f32>; 2]> = (0..NUM_OUTPUT_PORTS)
        .map(|_| [vec![0.0f32; n], vec![0.0f32; n]])
        .collect();

    let mut ports = AudioPorts::with_capacity(2, 1);
    let input_buffers = ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(
            [&mut input_left[..], &mut input_right[..]]
                .into_iter()
                .map(|b| InputChannel {
                    buffer: b,
                    is_constant: false,
                }),
        ),
    }]);

    let mut output_ports = AudioPorts::with_capacity(NUM_OUTPUT_PORTS * 2, NUM_OUTPUT_PORTS);
    let mut output_buffers = output_ports.with_output_buffers(
        out_bufs.iter_mut().map(|[left, right]| AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(
                [left.as_mut_slice(), right.as_mut_slice()].into_iter(),
            ),
        }),
    );

    let input_events = InputEvents::empty();
    let mut output_events = OutputEvents::void();

    let status = processor
        .process(
            &input_buffers,
            &mut output_buffers,
            &input_events,
            &mut output_events,
            Some(steady_time),
            None,
        )
        .expect("process() failed");

    // Return main output (port 0) channels
    let output_left = out_bufs[0][0].clone();
    let output_right = out_bufs[0][1].clone();
    (status, output_left, output_right)
}

#[test]
fn recv_plugin_e2e() {
    init_tracing();
    let mut host = load_recv_plugin();

    // --- Scenario 1: plays back audio received via IPC ---
    {
        // Start TCP listener before activating (so the IPC thread can connect)
        let (listener, addr) = random_listener();
        unsafe {
            std::env::set_var("WAIL_IPC_ADDR", addr.to_string());
        }

        let stopped = host
            .activate(48000.0, 32, 4096)
            .expect("Failed to activate for IPC test");

        let mut processor = stopped
            .start_processing()
            .expect("Failed to start processing");

        // Accept the IPC connection from the plugin's background thread
        let (mut stream, role, _stream_index) = accept_ipc_connection(&listener, Duration::from_secs(5));
        assert_eq!(
            role, IPC_ROLE_RECV,
            "Expected RECV role byte (0x01), got 0x{role:02x}"
        );

        let buf_size: u32 = 4096;

        // Process one buffer to establish interval 0 in the ring
        process_one_buffer(&mut processor, buf_size, 0);

        // Send a pre-encoded test interval to the plugin via TCP.
        // The IPC thread will Opus-decode it and push to the audio thread's channel.
        let frame = make_test_interval_frame("test-peer", 0);

        // Self-test: verify the Opus encode→decode pipeline produces non-silent audio.
        // make_test_interval_frame sends WAIF streaming frames; we verify the underlying
        // codec independently here.
        {
            let sr = 48000u32;
            let channels = 2u16;
            let samples_per_channel = (4usize * 4 * 60 * sr as usize) / 120;
            let test_samples = sine_wave(440.0, samples_per_channel, channels, sr);
            let mut enc = AudioEncoder::new(sr, channels, 128).unwrap();
            let mut dec = AudioDecoder::new(sr, channels).unwrap();
            let opus = enc.encode_interval(&test_samples).unwrap();
            let decoded = dec.decode_interval(&opus).unwrap();
            let decoded_rms = rms(&decoded);
            eprintln!(
                "Self-test: decoded {} samples, RMS={decoded_rms}, index=0",
                decoded.len()
            );
            assert!(decoded_rms > 0.001, "Decoded audio should be non-silent");
        }

        stream.write_all(&frame).expect("Failed to write IPC frame");

        // Give the IPC thread time to read, decode, and send to channel
        std::thread::sleep(Duration::from_secs(1));

        // Drive enough process() calls to cross the interval boundary.
        // At 120 BPM, 4 bars × quantum 4 = 16 beats = 384,000 samples.
        // With 4096-sample buffers: ceil(384000/4096) = 94 callbacks.
        // The first few calls consume the decoded audio via try_recv() and
        // feed it to the ring's pending_remote. When beat >= 16, the ring
        // swaps pending_remote into the playback slot.
        let num_callbacks: u64 = 100; // extra margin to guarantee boundary crossing

        let mut found_audio = false;
        for i in 1..=num_callbacks {
            let (_, out_l, _) =
                process_one_buffer(&mut processor, buf_size, i * buf_size as u64);
            let r = rms(&out_l);
            if r > 0.001 {
                found_audio = true;
            }
        }

        // Also check the final buffer
        let (_, output_left, _) = process_one_buffer(
            &mut processor,
            buf_size,
            (num_callbacks + 1) * buf_size as u64,
        );
        if rms(&output_left) > 0.001 {
            found_audio = true;
        }

        assert!(
            found_audio,
            "Recv plugin should output non-silent audio after receiving an interval via IPC \
             (checked {} buffers after boundary)",
            num_callbacks + 1
        );

        let stopped = processor.stop_processing();
        host.deactivate(stopped);
    }

    host.leak();
}
