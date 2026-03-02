//! Integration test: full Plugin IPC → App → WebRTC → App → Plugin IPC path.
//!
//! Exercises every encoding/decoding layer in the audio pipeline:
//!   Plugin A: AudioBridge → Opus encode → AudioWire → IpcMessage → IpcFramer → TCP
//!     → Mini App A: TCP read → IpcRecvBuffer → IpcMessage decode → PeerMesh.broadcast_audio()
//!       → WebRTC DataChannel
//!     → Mini App B: audio_rx.recv() → IpcMessage encode → IpcFramer → TCP write
//!   → Plugin B: TCP read → IpcRecvBuffer → IpcMessage decode → AudioWire decode → Opus decode
//!
//! No external services or DAW needed: in-process signaling, simulated plugin IPC clients.

mod common;

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use wail_audio::{AudioDecoder, AudioWire, IpcFramer, IpcMessage, IpcRecvBuffer};
use wail_net::PeerMesh;

use common::*;

// ---------------------------------------------------------------------------
// Mini App: lightweight session loop that bridges Plugin IPC ↔ WebRTC audio
// ---------------------------------------------------------------------------

/// A minimal session loop replicating the audio forwarding logic from
/// session.rs without Tauri, Link, clock sync, or interval tracking.
async fn mini_app_session(
    ipc_port: u16,
    signaling_url: String,
    room: String,
    peer_id: String,
    password: String,
    poll_interval_ms: u64,
) {
    let ice = wail_net::default_ice_servers();
    let (mut mesh, _sync_rx, mut audio_rx) = PeerMesh::connect_with_options(
        &signaling_url,
        &room,
        &peer_id,
        &password,
        ice,
        poll_interval_ms,
    )
    .await
    .expect("Mini app failed to connect to signaling");

    let ipc_listener = TcpListener::bind(("127.0.0.1", ipc_port))
        .await
        .expect("Mini app failed to bind IPC port");

    let (ipc_from_plugin_tx, mut ipc_from_plugin_rx) = mpsc::channel::<Vec<u8>>(64);
    let mut ipc_writer: Option<tokio::net::tcp::OwnedWriteHalf> = None;

    loop {
        tokio::select! {
            // Accept plugin IPC connection
            result = ipc_listener.accept() => {
                if let Ok((stream, _addr)) = result {
                    let (read_half, write_half) = stream.into_split();
                    ipc_writer = Some(write_half);

                    let tx = ipc_from_plugin_tx.clone();
                    tokio::spawn(async move {
                        let mut recv_buf = IpcRecvBuffer::new();
                        let mut buf = [0u8; 65536];
                        let mut reader = read_half;
                        loop {
                            match reader.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(n) => {
                                    recv_buf.push(&buf[..n]);
                                    while let Some(frame) = recv_buf.next_frame() {
                                        let _ = tx.try_send(frame);
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    });
                }
            }

            // IPC from plugin → broadcast to WebRTC peers
            Some(frame) = ipc_from_plugin_rx.recv() => {
                if let Some((_peer_id, wire_data)) = IpcMessage::decode_audio(&frame) {
                    mesh.broadcast_audio(&wire_data).await;
                }
            }

            // Signaling (drives WebRTC negotiation)
            _event = mesh.poll_signaling() => {}

            // WebRTC audio from peers → forward to plugin via IPC
            Some((from, data)) = audio_rx.recv() => {
                if let Some(ref mut writer) = ipc_writer {
                    let msg = IpcMessage::encode_audio(&from, &data);
                    let frame = IpcFramer::encode_frame(&msg);
                    if writer.write_all(&frame).await.is_err() {
                        ipc_writer = None;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Simulated plugin IPC clients
// ---------------------------------------------------------------------------

/// Simulate plugin's IPC thread outbound: produce a sine wave interval,
/// Opus-encode, wrap in AudioWire + IpcMessage + IpcFramer, send via TCP.
///
/// Mirrors the encode path in wail-plugin/src/lib.rs lines 376-399.
async fn simulated_plugin_send(ipc_port: u16, freq_hz: f32) {
    // Retry connection until mini_app is listening
    let stream = loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", ipc_port)).await {
            Ok(s) => break s,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    };
    let mut writer = stream;

    // Produce wire-encoded interval (AudioBridge + Opus encode + AudioWire encode)
    let wire_data =
        tokio::task::spawn_blocking(move || produce_interval(freq_hz))
            .await
            .expect("produce_interval panicked");

    // Wrap in IPC framing: empty peer_id for outgoing from plugin
    let msg = IpcMessage::encode_audio("", &wire_data);
    let frame = IpcFramer::encode_frame(&msg);
    writer
        .write_all(&frame)
        .await
        .expect("Plugin A: failed to write IPC frame");

    // Keep connection alive so app can read
    tokio::time::sleep(Duration::from_secs(5)).await;
}

/// Simulate plugin's IPC thread inbound: connect to mini_app via TCP,
/// receive IPC frames, decode AudioWire + Opus, return decoded audio.
///
/// Mirrors the decode path in wail-plugin/src/lib.rs lines 412-450.
async fn simulated_plugin_receive(
    ipc_port: u16,
    timeout: Duration,
) -> (String, Vec<f32>) {
    // Retry connection until mini_app is listening
    let stream = loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", ipc_port)).await {
            Ok(s) => break s,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    };
    let mut reader = stream;

    let mut recv_buf = IpcRecvBuffer::new();
    let mut read_buf = [0u8; 65536];
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        tokio::select! {
            result = reader.read(&mut read_buf) => {
                match result {
                    Ok(0) => panic!("Plugin B: IPC connection closed before receiving audio"),
                    Ok(n) => {
                        recv_buf.push(&read_buf[..n]);
                        if let Some(payload) = recv_buf.next_frame() {
                            if let Some((peer_id, wire_data)) = IpcMessage::decode_audio(&payload) {
                                let interval = AudioWire::decode(&wire_data)
                                    .expect("Plugin B: failed to decode AudioWire");
                                let mut decoder = AudioDecoder::new(
                                    interval.sample_rate,
                                    interval.channels,
                                )
                                .expect("Plugin B: failed to create Opus decoder");
                                let samples = decoder
                                    .decode_interval(&interval.opus_data)
                                    .expect("Plugin B: failed to decode Opus audio");
                                return (peer_id, samples);
                            }
                        }
                    }
                    Err(e) => panic!("Plugin B: IPC read error: {e}"),
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                panic!("Plugin B: timed out waiting for audio via IPC");
            }
        }
    }
}

/// Send multiple full-size intervals via IPC.
async fn simulated_plugin_send_full(ipc_port: u16, count: usize) {
    let stream = loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", ipc_port)).await {
            Ok(s) => break s,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    };
    let mut writer = stream;

    for i in 0..count {
        let freq = 440.0 + i as f32 * 110.0; // different freq per interval
        let (wire_data, _expected) =
            tokio::task::spawn_blocking(move || produce_full_interval(freq))
                .await
                .expect("produce_full_interval panicked");

        let msg = IpcMessage::encode_audio("", &wire_data);
        let frame = IpcFramer::encode_frame(&msg);
        writer
            .write_all(&frame)
            .await
            .expect("Plugin A: failed to write IPC frame");
    }

    // Keep connection alive
    tokio::time::sleep(Duration::from_secs(10)).await;
}

/// Receive multiple intervals via IPC, return all decoded results.
async fn simulated_plugin_receive_multi(
    ipc_port: u16,
    expected_count: usize,
    timeout: Duration,
) -> Vec<(String, Vec<f32>)> {
    let stream = loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", ipc_port)).await {
            Ok(s) => break s,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    };
    let mut reader = stream;

    let mut recv_buf = IpcRecvBuffer::new();
    let mut read_buf = [0u8; 65536];
    let mut results = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if results.len() >= expected_count {
            return results;
        }
        tokio::select! {
            result = reader.read(&mut read_buf) => {
                match result {
                    Ok(0) => panic!("Plugin B: IPC connection closed"),
                    Ok(n) => {
                        recv_buf.push(&read_buf[..n]);
                        while let Some(payload) = recv_buf.next_frame() {
                            if let Some((peer_id, wire_data)) = IpcMessage::decode_audio(&payload) {
                                let interval = AudioWire::decode(&wire_data)
                                    .expect("Failed to decode AudioWire");
                                let mut decoder = AudioDecoder::new(
                                    interval.sample_rate,
                                    interval.channels,
                                ).expect("Failed to create Opus decoder");
                                let samples = decoder
                                    .decode_interval(&interval.opus_data)
                                    .expect("Failed to decode Opus audio");
                                results.push((peer_id, samples));
                            }
                        }
                    }
                    Err(e) => panic!("Plugin B: IPC read error: {e}"),
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                panic!(
                    "Timed out waiting for intervals: got {}/{expected_count}",
                    results.len()
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test: Full plugin-to-plugin audio path through IPC + WebRTC
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn plugin_ipc_to_webrtc_to_plugin_ipc() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // 1. Start in-process HTTP signaling server
    let server_url = start_test_signaling_server().await;

    // 2. Pick random IPC ports for each mini app
    let ipc_port_a = random_port();
    let ipc_port_b = random_port();

    // 3. Spawn mini_app_a (peer-a) and mini_app_b (peer-b)
    //    "peer-a" < "peer-b" lexicographically → peer-a initiates WebRTC
    let url = server_url.clone();
    let app_a = tokio::spawn(mini_app_session(
        ipc_port_a,
        url,
        "e2e-room".into(),
        "peer-a".into(),
        "test".into(),
        200,
    ));

    tokio::time::sleep(Duration::from_millis(100)).await;

    let url = server_url.clone();
    let app_b = tokio::spawn(mini_app_session(
        ipc_port_b,
        url,
        "e2e-room".into(),
        "peer-b".into(),
        "test".into(),
        200,
    ));

    // 4. Wait for WebRTC connection to establish between the two mini apps.
    //    The mini_apps pump signaling in their select loops, so connection
    //    should establish within a few seconds on localhost.
    tokio::time::sleep(Duration::from_secs(6)).await;

    // 5. Simulated Plugin A: produce 440Hz sine wave, Opus-encode, send via TCP IPC
    let sender = tokio::spawn(simulated_plugin_send(ipc_port_a, 440.0));

    // 6. Simulated Plugin B: receive via TCP IPC, decode, verify energy
    let (peer_id, decoded_samples) = tokio::time::timeout(
        Duration::from_secs(15),
        simulated_plugin_receive(ipc_port_b, Duration::from_secs(10)),
    )
    .await
    .expect("Overall test timed out");

    // 7. Assertions
    assert_eq!(peer_id, "peer-a", "Should identify the sender peer");
    assert!(
        !decoded_samples.is_empty(),
        "Decoded samples should be non-empty"
    );

    let energy = rms(&decoded_samples);
    assert!(
        energy > 0.01,
        "Plugin B should receive audio with signal energy via full IPC+WebRTC path, RMS={energy}"
    );

    eprintln!(
        "[test] IPC E2E passed! peer_id={peer_id}, samples={}, RMS={energy:.4}",
        decoded_samples.len()
    );

    // Clean up
    sender.abort();
    app_a.abort();
    app_b.abort();
}

// ---------------------------------------------------------------------------
// Test: Multiple full-size intervals through IPC + WebRTC
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn multi_interval_full_size_e2e() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    let server_url = start_test_signaling_server().await;

    let ipc_port_a = random_port();
    let ipc_port_b = random_port();

    let url = server_url.clone();
    let app_a = tokio::spawn(mini_app_session(
        ipc_port_a,
        url,
        "multi-room".into(),
        "peer-a".into(),
        "test".into(),
        200,
    ));

    tokio::time::sleep(Duration::from_millis(100)).await;

    let url = server_url.clone();
    let app_b = tokio::spawn(mini_app_session(
        ipc_port_b,
        url,
        "multi-room".into(),
        "peer-b".into(),
        "test".into(),
        200,
    ));

    // Wait for WebRTC connection
    tokio::time::sleep(Duration::from_secs(6)).await;

    // Send 3 full-size intervals (each ~768k samples → ~128KB Opus → chunked)
    let num_intervals = 3;
    let sender = tokio::spawn(simulated_plugin_send_full(ipc_port_a, num_intervals));

    // Receive all 3
    let results = tokio::time::timeout(
        Duration::from_secs(30),
        simulated_plugin_receive_multi(ipc_port_b, num_intervals, Duration::from_secs(25)),
    )
    .await
    .expect("Overall test timed out");

    assert_eq!(
        results.len(),
        num_intervals,
        "Should receive all {num_intervals} intervals"
    );

    // Expected samples per interval: 48000 * 2ch * 8s = 768,000
    let expected_samples = 768_000_usize;
    let tolerance = 1920; // ±1 Opus frame (960 samples * 2 channels)

    for (i, (peer_id, samples)) in results.iter().enumerate() {
        assert_eq!(peer_id, "peer-a", "Interval {i}: wrong peer");

        assert!(
            samples.len() >= expected_samples - tolerance,
            "Interval {i}: too few samples: {} (expected ~{expected_samples})",
            samples.len(),
        );

        let energy = rms(samples);
        assert!(
            energy > 0.01,
            "Interval {i}: should contain real audio, RMS={energy}",
        );

        eprintln!(
            "[test] Interval {i}: peer={peer_id}, samples={}, RMS={energy:.4}",
            samples.len()
        );
    }

    eprintln!("[test] Multi-interval E2E passed! All {num_intervals} intervals received with full audio.");

    sender.abort();
    app_a.abort();
    app_b.abort();
}
