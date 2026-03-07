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
use wail_audio::{AudioDecoder, AudioFrame, AudioFrameWire, AudioWire, FrameAssembler, IpcFramer, IpcMessage, IpcRecvBuffer, IPC_ROLE_RECV, IPC_ROLE_SEND};
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
) {
    let ice = wail_net::default_ice_servers();
    let (mut mesh, _sync_rx, mut audio_rx) = PeerMesh::connect_with_ice(
        &signaling_url,
        &room,
        &peer_id,
        Some(password.as_str()),
        ice,
    )
    .await
    .expect("Mini app failed to connect to signaling");

    let ipc_listener = TcpListener::bind(("127.0.0.1", ipc_port))
        .await
        .expect("Mini app failed to bind IPC port");

    let (ipc_from_plugin_tx, mut ipc_from_plugin_rx) = mpsc::channel::<Vec<u8>>(1024);
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
                                        if tx.send(frame).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    });
                }
            }

            // IPC from send plugin (tag 0x05 WAIF frame) → broadcast raw WAIF bytes to WebRTC peers
            Some(frame) = ipc_from_plugin_rx.recv() => {
                if let Some(wire_data) = IpcMessage::decode_audio_frame(&frame) {
                    mesh.broadcast_audio(&wire_data).await;
                }
            }

            // Signaling (drives WebRTC negotiation)
            _event = mesh.poll_signaling() => {}

            // WebRTC audio from peers (raw WAIF bytes) → forward to recv plugin via IPC (tag 0x01)
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
/// encode as WAIF streaming frames (tag 0x05), send via TCP.
///
/// Mirrors the encode path in wail-plugin-send/src/lib.rs.
async fn simulated_plugin_send(ipc_port: u16, freq_hz: f32) {
    // Retry connection until mini_app is listening
    let stream = loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", ipc_port)).await {
            Ok(s) => break s,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    };
    let mut writer = stream;

    // Produce WAIF IPC frames (tag 0x05), one per 20ms Opus packet
    let ipc_frames =
        tokio::task::spawn_blocking(move || produce_interval_waif_ipc(freq_hz))
            .await
            .expect("produce_interval_waif_ipc panicked");

    for frame in ipc_frames {
        writer
            .write_all(&frame)
            .await
            .expect("Plugin A: failed to write IPC frame");
    }

    // Keep connection alive so app can read
    tokio::time::sleep(Duration::from_secs(5)).await;
}

/// Simulate plugin's IPC thread inbound: connect to mini_app via TCP,
/// receive WAIF frames (tag 0x01 + WAIF inner payload), assemble, decode Opus.
///
/// Mirrors the decode path in wail-plugin-recv/src/lib.rs.
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
    let mut assembler = FrameAssembler::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        tokio::select! {
            result = reader.read(&mut read_buf) => {
                match result {
                    Ok(0) => panic!("Plugin B: IPC connection closed before receiving audio"),
                    Ok(n) => {
                        recv_buf.push(&read_buf[..n]);
                        while let Some(payload) = recv_buf.next_frame() {
                            if let Some((peer_id, wire_data)) = IpcMessage::decode_audio(&payload) {
                                if let Ok(frame) = AudioFrameWire::decode(&wire_data) {
                                    if frame.is_final {
                                        assembler.evict_stale(frame.interval_index);
                                    }
                                    if let Some(assembled) = assembler.insert(&peer_id, &frame) {
                                        let mut decoder = AudioDecoder::new(
                                            assembled.sample_rate,
                                            assembled.channels,
                                        )
                                        .expect("Plugin B: failed to create Opus decoder");
                                        let samples = decoder
                                            .decode_interval(&assembled.opus_data)
                                            .expect("Plugin B: failed to decode Opus audio");
                                        return (assembled.peer_id, samples);
                                    }
                                }
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

/// Convert an AudioWire (WAIL) blob to a list of WAIF IPC frames (tag 0x05).
fn wail_to_waif_ipc(wail_bytes: &[u8]) -> Vec<Vec<u8>> {
    let interval = AudioWire::decode(wail_bytes).expect("wail_to_waif_ipc: AudioWire decode");
    let data = &interval.opus_data;
    let frame_count = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
    let mut packets: Vec<Vec<u8>> = Vec::with_capacity(frame_count);
    let mut offset = 4;
    while packets.len() < frame_count && offset + 2 <= data.len() {
        let pkt_len = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;
        packets.push(data[offset..offset + pkt_len].to_vec());
        offset += pkt_len;
    }
    let total = packets.len();
    let mut output = Vec::with_capacity(total);
    for (fn_, packet) in packets.into_iter().enumerate() {
        let is_final = fn_ + 1 == total;
        let frame = AudioFrame {
            interval_index: interval.index,
            stream_id: interval.stream_id,
            frame_number: fn_ as u32,
            channels: interval.channels,
            opus_data: packet,
            is_final,
            sample_rate: if is_final { interval.sample_rate } else { 0 },
            total_frames: if is_final { total as u32 } else { 0 },
            bpm: if is_final { interval.bpm } else { 0.0 },
            quantum: if is_final { interval.quantum } else { 0.0 },
            bars: if is_final { interval.bars } else { 0 },
        };
        let wire_bytes = AudioFrameWire::encode(&frame);
        let ipc_msg = IpcMessage::encode_audio_frame(&wire_bytes);
        output.push(IpcFramer::encode_frame(&ipc_msg));
    }
    output
}

/// Send multiple full-size intervals via IPC as WAIF streaming frames (tag 0x05).
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
        let ipc_frames =
            tokio::task::spawn_blocking(move || {
                // produce_full_interval returns a WAIL blob; convert to WAIF IPC frames
                let (wail_bytes, _) = produce_full_interval(freq);
                wail_to_waif_ipc(&wail_bytes)
            })
            .await
            .expect("produce_full_interval panicked");

        for frame in ipc_frames {
            writer
                .write_all(&frame)
                .await
                .expect("Plugin A: failed to write IPC frame");
        }
    }

    // Keep connection alive
    tokio::time::sleep(Duration::from_secs(10)).await;
}

/// Receive multiple intervals via IPC, assemble WAIF frames, decode Opus.
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
    let mut assembler = FrameAssembler::new();
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
                                if let Ok(frame) = AudioFrameWire::decode(&wire_data) {
                                    if frame.is_final {
                                        assembler.evict_stale(frame.interval_index);
                                    }
                                    if let Some(assembled) = assembler.insert(&peer_id, &frame) {
                                        let mut decoder = AudioDecoder::new(
                                            assembled.sample_rate,
                                            assembled.channels,
                                        ).expect("Failed to create Opus decoder");
                                        let samples = decoder
                                            .decode_interval(&assembled.opus_data)
                                            .expect("Failed to decode Opus audio");
                                        results.push((assembled.peer_id, samples));
                                    }
                                }
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
    ));

    tokio::time::sleep(Duration::from_millis(100)).await;

    let url = server_url.clone();
    let app_b = tokio::spawn(mini_app_session(
        ipc_port_b,
        url,
        "e2e-room".into(),
        "peer-b".into(),
        "test".into(),
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
    ));

    tokio::time::sleep(Duration::from_millis(100)).await;

    let url = server_url.clone();
    let app_b = tokio::spawn(mini_app_session(
        ipc_port_b,
        url,
        "multi-room".into(),
        "peer-b".into(),
        "test".into(),
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

// ---------------------------------------------------------------------------
// Mini App V2: role-aware session loop (send vs recv plugin routing)
// ---------------------------------------------------------------------------

/// Session loop that reads a role byte from each IPC connection and only
/// forwards received WebRTC audio to connections that identify as RECV.
async fn mini_app_session_v2(
    ipc_port: u16,
    signaling_url: String,
    room: String,
    peer_id: String,
    password: String,
) {
    let ice = wail_net::default_ice_servers();
    let (mut mesh, _sync_rx, mut audio_rx) = PeerMesh::connect_with_ice(
        &signaling_url,
        &room,
        &peer_id,
        Some(password.as_str()),
        ice,
    )
    .await
    .expect("Mini app V2 failed to connect");

    let ipc_listener = TcpListener::bind(("127.0.0.1", ipc_port))
        .await
        .expect("Mini app V2 failed to bind IPC port");

    let (ipc_from_plugin_tx, mut ipc_from_plugin_rx) = mpsc::channel::<Vec<u8>>(1024);
    let mut ipc_recv_writers: Vec<tokio::net::tcp::OwnedWriteHalf> = Vec::new();

    loop {
        tokio::select! {
            result = ipc_listener.accept() => {
                if let Ok((stream, _addr)) = result {
                    let (mut read_half, write_half) = stream.into_split();

                    // Read role byte to determine plugin type
                    let mut role_buf = [0u8; 1];
                    if read_half.read_exact(&mut role_buf).await.is_err() {
                        continue;
                    }
                    let role = role_buf[0];

                    if role == IPC_ROLE_RECV {
                        ipc_recv_writers.push(write_half);
                    }
                    // For all roles, spawn reader for IPC frames (send plugin writes audio)
                    let tx = ipc_from_plugin_tx.clone();
                    tokio::spawn(async move {
                        let mut recv_buf = IpcRecvBuffer::new();
                        let mut buf = [0u8; 65536];
                        loop {
                            match read_half.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(n) => {
                                    recv_buf.push(&buf[..n]);
                                    while let Some(frame) = recv_buf.next_frame() {
                                        if tx.send(frame).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    });
                }
            }

            // IPC from send plugin (tag 0x05 WAIF frame) → broadcast raw WAIF bytes to WebRTC peers
            Some(frame) = ipc_from_plugin_rx.recv() => {
                if let Some(wire_data) = IpcMessage::decode_audio_frame(&frame) {
                    mesh.broadcast_audio(&wire_data).await;
                }
            }

            _event = mesh.poll_signaling() => {}

            // WebRTC audio from peers (raw WAIF bytes) → forward to recv plugin via IPC (tag 0x01)
            Some((from, data)) = audio_rx.recv() => {
                let msg = IpcMessage::encode_audio(&from, &data);
                let frame = IpcFramer::encode_frame(&msg);
                for writer in &mut ipc_recv_writers {
                    let _ = writer.write_all(&frame).await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test: Dual plugin IPC — only recv plugin gets forwarded audio
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn dual_plugin_ipc_only_recv_gets_audio() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // 1. Start signaling server
    let server_url = start_test_signaling_server().await;

    let ipc_port_a = random_port();
    let ipc_port_b = random_port();

    // 2. Spawn mini_app_a (sender side) with V1 (no role awareness needed)
    let url = server_url.clone();
    let app_a = tokio::spawn(mini_app_session(
        ipc_port_a,
        url,
        "dual-room".into(),
        "peer-a".into(),
        "test".into(),
    ));

    tokio::time::sleep(Duration::from_millis(100)).await;

    // 3. Spawn mini_app_b (receiver side) with V2 (role-aware)
    let url = server_url.clone();
    let app_b = tokio::spawn(mini_app_session_v2(
        ipc_port_b,
        url,
        "dual-room".into(),
        "peer-b".into(),
        "test".into(),
    ));

    // 4. Wait for WebRTC connection
    tokio::time::sleep(Duration::from_secs(6)).await;

    // 5. Connect simulated SEND plugin to app_b (identifies as SEND role)
    let send_conn = tokio::spawn(async move {
        let mut stream = loop {
            match tokio::net::TcpStream::connect(("127.0.0.1", ipc_port_b)).await {
                Ok(s) => break s,
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        };
        // Write role byte
        stream.write_all(&[IPC_ROLE_SEND]).await.expect("send role byte");

        // Try to read — should NOT receive any audio (timeout expected)
        let mut buf = [0u8; 65536];
        match tokio::time::timeout(Duration::from_secs(8), stream.read(&mut buf)).await {
            Ok(Ok(0)) => 0usize, // connection closed — no data, correct
            Ok(Ok(n)) => n,      // got data — this would be a bug
            Ok(Err(_)) => 0,     // error — treat as no data
            Err(_) => 0,         // timeout — correct, no data forwarded
        }
    });

    // 6. Connect simulated RECV plugin to app_b (identifies as RECV role)
    let recv_conn = tokio::spawn(async move {
        let mut stream = loop {
            match tokio::net::TcpStream::connect(("127.0.0.1", ipc_port_b)).await {
                Ok(s) => break s,
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        };
        // Write role byte
        stream.write_all(&[IPC_ROLE_RECV]).await.expect("recv role byte");

        // Read IPC frames — should receive audio from remote peer
        let mut recv_buf = IpcRecvBuffer::new();
        let mut buf = [0u8; 65536];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::select! {
                result = stream.read(&mut buf) => {
                    match result {
                        Ok(0) => panic!("Recv plugin: connection closed before audio arrived"),
                        Ok(n) => {
                            recv_buf.push(&buf[..n]);
                            if let Some(payload) = recv_buf.next_frame() {
                                if let Some((peer_id, _wire_data)) = IpcMessage::decode_audio(&payload) {
                                    return peer_id;
                                }
                            }
                        }
                        Err(e) => panic!("Recv plugin: read error: {e}"),
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    panic!("Recv plugin: timed out waiting for audio");
                }
            }
        }
    });

    // 7. Send audio from app_a's plugin side
    let sender = tokio::spawn(simulated_plugin_send(ipc_port_a, 440.0));

    // 8. Verify recv plugin got the audio
    let peer_id = tokio::time::timeout(Duration::from_secs(15), recv_conn)
        .await
        .expect("Overall timeout")
        .expect("recv_conn task panicked");
    assert_eq!(peer_id, "peer-a", "Recv plugin should get audio from peer-a");

    // 9. Verify send plugin did NOT get audio
    let send_bytes = tokio::time::timeout(Duration::from_secs(2), send_conn)
        .await
        .expect("send_conn timeout")
        .expect("send_conn task panicked");
    assert_eq!(
        send_bytes, 0,
        "Send plugin should NOT receive forwarded audio (got {send_bytes} bytes)"
    );

    sender.abort();
    app_a.abort();
    app_b.abort();
}
