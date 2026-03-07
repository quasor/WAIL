//! End-to-end tests for the WAIL Send CLAP plugin.
//!
//! Loads the real `.clap` binary, feeds synthetic audio with transport info,
//! and verifies the plugin behaves correctly.
//!
//! All scenarios run in a single test to avoid loading the `.clap` dylib
//! on multiple threads — CLAP plugins have main-thread affinity for
//! `clap_entry.init()`.
//!
//! Requires: `cargo xtask build-plugin` before running.

use std::time::Duration;

use clack_host::prelude::*;
use wail_audio::{AudioFrameWire, IpcMessage, IPC_ROLE_SEND};
use wail_plugin_test::*;

const SEND_CLAP_ID: &str = "com.wail.send";

fn load_send_plugin() -> ClapTestHost {
    let path = find_plugin_bundle("wail-plugin-send");
    assert!(
        path.exists(),
        "Plugin bundle not found at {}. Run `cargo xtask build-plugin` first.",
        path.display()
    );
    unsafe { ClapTestHost::load(&path, SEND_CLAP_ID).expect("Failed to load send plugin") }
}

fn process_one_buffer_with_input(
    processor: &mut StartedPluginAudioProcessor<TestHost>,
    input_left: &mut [f32],
    input_right: &mut [f32],
    num_frames: u32,
    steady_time: u64,
) -> (ProcessStatus, Vec<f32>, Vec<f32>) {
    let mut output_left = vec![99.0f32; num_frames as usize];
    let mut output_right = vec![99.0f32; num_frames as usize];

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

    let mut output_ports = AudioPorts::with_capacity(2, 1);
    let mut output_buffers = output_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(
            [&mut output_left[..], &mut output_right[..]].into_iter(),
        ),
    }]);

    let input_events = InputEvents::empty();
    let mut output_events = OutputEvents::void();

    let status = processor
        .process(
            &input_buffers,
            &mut output_buffers,
            &input_events,
            &mut output_events,
            Some(steady_time),
            None, // no transport — tests beat fallback path
        )
        .expect("process() failed");

    (status, output_left, output_right)
}

#[test]
fn send_plugin_e2e() {
    let mut host = load_send_plugin();

    // --- Scenario 1: loads and activates ---
    let stopped = host
        .activate(48000.0, 32, 4096)
        .expect("Failed to activate");
    host.deactivate(stopped);

    // --- Scenario 2: outputs silence (send is capture-only) ---
    let stopped = host
        .activate(48000.0, 32, 512)
        .expect("Failed to activate");

    let mut processor = stopped
        .start_processing()
        .expect("Failed to start processing");

    let num_frames: u32 = 256;

    // Create stereo input: 440Hz sine wave
    let mut input_left: Vec<f32> = Vec::with_capacity(num_frames as usize);
    let mut input_right: Vec<f32> = Vec::with_capacity(num_frames as usize);
    for i in 0..num_frames as usize {
        let t = i as f32 / 48000.0;
        let sample = (t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5;
        input_left.push(sample);
        input_right.push(sample);
    }

    let (status, output_left, _) = process_one_buffer_with_input(
        &mut processor,
        &mut input_left,
        &mut input_right,
        num_frames,
        0,
    );

    assert!(
        matches!(status, ProcessStatus::Continue | ProcessStatus::ContinueIfNotQuiet),
        "Unexpected process status: {:?}",
        status
    );

    // Send plugin should output silence (it's capture-only)
    let output_rms = rms(&output_left);
    assert!(
        output_rms < 0.001,
        "Send plugin should output silence, but got RMS={output_rms}"
    );

    // --- Scenario 3: processes 100 buffers without crashing ---
    for i in 1..100u64 {
        let mut il = vec![0.1f32; num_frames as usize];
        let mut ir = vec![0.1f32; num_frames as usize];
        let (status, _, _) = process_one_buffer_with_input(
            &mut processor,
            &mut il,
            &mut ir,
            num_frames,
            i * num_frames as u64,
        );

        assert!(
            matches!(status, ProcessStatus::Continue | ProcessStatus::ContinueIfNotQuiet),
            "Unexpected status at buffer {i}: {:?}",
            status
        );
    }

    let stopped = processor.stop_processing();
    host.deactivate(stopped);

    // --- Scenario 4: sends wire-encoded audio over IPC ---
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
        let (mut stream, role, stream_index) = accept_ipc_connection(&listener, Duration::from_secs(5));
        assert_eq!(
            role, IPC_ROLE_SEND,
            "Expected SEND role byte (0x00), got 0x{role:02x}"
        );
        assert_eq!(stream_index, 0, "Default stream_index should be 0");

        // Drive enough process() calls to complete one interval.
        // At 120 BPM, 4 bars × quantum 4 = 16 beats = 384,000 samples at 48kHz.
        // With 4096-sample buffers: ceil(384000/4096) = 94 callbacks.
        let buf_size: u32 = 4096;
        let num_callbacks: u64 = 100; // a few extra to guarantee boundary crossing

        for i in 0..num_callbacks {
            let mut il = sine_wave(440.0, buf_size as usize, 1, 48000);
            let mut ir = il.clone();
            process_one_buffer_with_input(
                &mut processor,
                &mut il,
                &mut ir,
                buf_size,
                i * buf_size as u64,
            );
        }

        // The IPC thread streams the completed interval as WAIF frames
        // (IPC_TAG_AUDIO_FRAME, tag 0x05). Read until we receive the final frame.
        let mut recv_buf = wail_audio::IpcRecvBuffer::new();
        let mut final_frame = None;
        let mut received_frames: u32 = 0;
        while final_frame.is_none() {
            let payload = read_ipc_frame(&mut stream, &mut recv_buf, Duration::from_secs(15));
            if let Some(wire_data) = IpcMessage::decode_audio_frame(&payload) {
                let frame =
                    AudioFrameWire::decode(&wire_data).expect("Failed to decode AudioFrameWire");
                received_frames += 1;
                if frame.is_final {
                    final_frame = Some(frame);
                }
            }
        }
        let final_frame = final_frame.unwrap();
        assert_eq!(final_frame.sample_rate, 48000, "sample_rate mismatch");
        assert_eq!(final_frame.channels, 2, "channels mismatch");
        assert!(
            final_frame.total_frames > 0,
            "Interval should have recorded frames"
        );
        assert!(
            !final_frame.opus_data.is_empty(),
            "Final frame Opus data should not be empty"
        );
        assert_eq!(
            received_frames, final_frame.total_frames,
            "Received frame count should match total_frames in final frame"
        );

        let stopped = processor.stop_processing();
        host.deactivate(stopped);
    }

    host.leak();
}
