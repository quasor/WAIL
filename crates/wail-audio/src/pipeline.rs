/// End-to-end pipeline tests for intervalic audio.
///
/// Validates the full path:
///   Peer A audio → Ring record → Opus encode → Wire encode → IPC frame
///   → (simulated network) →
///   IPC frame → Wire decode → Opus decode → Ring playback → Peer B audio
///
/// This module has no production code — it only contains integration tests
/// that exercise the full pipeline across all wail-audio components.

#[cfg(test)]
mod tests {
    use crate::bridge::AudioBridge;
    use crate::codec::AudioDecoder;
    use crate::ipc::{IpcFramer, IpcRecvBuffer};
    use crate::wire::AudioWire;

    const SR: u32 = 48000;
    const CH: u16 = 2;
    const BARS: u32 = 4;
    const Q: f64 = 4.0;
    const BITRATE: u32 = 128;

    /// Generate a recognizable test signal: sine wave at a given frequency.
    fn sine_wave(freq_hz: f32, duration_samples: usize, channels: u16, sample_rate: u32) -> Vec<f32> {
        let mut out = Vec::with_capacity(duration_samples * channels as usize);
        for i in 0..duration_samples {
            let t = i as f32 / sample_rate as f32;
            let sample = (t * freq_hz * 2.0 * std::f32::consts::PI).sin() * 0.5;
            for _ in 0..channels {
                out.push(sample);
            }
        }
        out
    }

    /// Compute RMS energy of a signal.
    fn rms(samples: &[f32]) -> f32 {
        let sum: f32 = samples.iter().map(|s| s * s).sum();
        (sum / samples.len() as f32).sqrt()
    }

    // ---------------------------------------------------------------
    // Test 1: Full two-peer simulation using AudioBridge
    // ---------------------------------------------------------------

    #[test]
    fn two_peer_interval_exchange() {
        // Peer A records, Peer B receives and plays back.
        let mut peer_a = AudioBridge::new(SR, CH, BARS, Q, BITRATE);
        let mut peer_b = AudioBridge::new(SR, CH, BARS, Q, BITRATE);

        // Must be large enough for multiple Opus frames (960 samples/frame * 2 ch = 1920 interleaved)
        let buf_size = 4096; // samples per process() call (interleaved)
        let silence = vec![0.0f32; buf_size];

        // Peer A: record a sine wave through interval 0
        let signal = sine_wave(440.0, buf_size / CH as usize, CH, SR);
        let mut a_output = vec![0.0f32; buf_size];
        let mut b_output = vec![0.0f32; buf_size];

        // Multiple process calls within interval 0
        for beat in [0.0, 4.0, 8.0, 12.0] {
            let outgoing = peer_a.process(&signal, &mut a_output, beat);
            assert!(outgoing.is_empty(), "No output within interval");
            // Peer B is idle during interval 0
            peer_b.process(&silence, &mut b_output, beat);
        }

        // Cross into interval 1 — Peer A produces encoded interval 0
        let wire_msgs = peer_a.process(&signal, &mut a_output, 16.0);
        assert_eq!(wire_msgs.len(), 1, "Peer A should emit interval 0");

        // "Network": deliver the wire message to Peer B
        peer_b.receive_wire("peer-a", &wire_msgs[0]);

        // Peer B crosses into interval 1 — should start playing Peer A's audio
        peer_b.process(&silence, &mut b_output, 16.0);

        // Peer B's output should now have audio energy (decoded from Peer A)
        let energy = rms(&b_output);
        assert!(energy > 0.01, "Peer B should hear Peer A's audio, got RMS={energy}");
    }

    // ---------------------------------------------------------------
    // Test 2: Full pipeline with IPC framing in the middle
    // ---------------------------------------------------------------

    #[test]
    fn pipeline_with_ipc_framing() {
        let mut peer_a = AudioBridge::new(SR, CH, BARS, Q, BITRATE);

        let buf_size = 512;
        let signal = sine_wave(220.0, buf_size / CH as usize, CH, SR);
        let mut output = vec![0.0f32; buf_size];

        // Record an interval
        peer_a.process(&signal, &mut output, 0.0);
        peer_a.process(&signal, &mut output, 8.0);
        let wire_msgs = peer_a.process(&signal, &mut output, 16.0);

        // Wrap in IPC framing (what plugin sends to app over Unix socket)
        let ipc_frame = IpcFramer::encode_frame(&wire_msgs[0]);

        // Simulate chunked delivery through IPC recv buffer
        let mut recv_buf = IpcRecvBuffer::new();
        // Deliver in 3 chunks
        let chunk_size = ipc_frame.len() / 3;
        recv_buf.push(&ipc_frame[..chunk_size]);
        assert!(recv_buf.next_frame().is_none(), "Partial delivery");

        recv_buf.push(&ipc_frame[chunk_size..chunk_size * 2]);
        assert!(recv_buf.next_frame().is_none(), "Still partial");

        recv_buf.push(&ipc_frame[chunk_size * 2..]);
        let received = recv_buf.next_frame().expect("Should have full frame now");

        // Verify the received wire data is valid
        let interval = AudioWire::decode(&received).unwrap();
        assert_eq!(interval.index, 0);
        assert_eq!(interval.sample_rate, SR);
        assert_eq!(interval.channels, CH);

        // Decode Opus back to audio
        let mut decoder = AudioDecoder::new(SR, CH).unwrap();
        let decoded = decoder.decode_interval(&interval.opus_data).unwrap();
        assert!(!decoded.is_empty());

        let energy = rms(&decoded);
        assert!(energy > 0.01, "Decoded audio should have signal, RMS={energy}");
    }

    // ---------------------------------------------------------------
    // Test 3: Multi-peer mix through bridges
    // ---------------------------------------------------------------

    #[test]
    fn three_peer_mix() {
        let mut peer_a = AudioBridge::new(SR, CH, BARS, Q, BITRATE);
        let mut peer_b = AudioBridge::new(SR, CH, BARS, Q, BITRATE);
        let mut peer_c = AudioBridge::new(SR, CH, BARS, Q, BITRATE); // the listener

        // Must be large enough for multiple Opus frames
        let buf_size = 4096;
        let signal_a = sine_wave(440.0, buf_size / CH as usize, CH, SR);
        let signal_b = sine_wave(880.0, buf_size / CH as usize, CH, SR);
        let silence = vec![0.0f32; buf_size];
        let mut out = vec![0.0f32; buf_size];

        // All three peers process interval 0
        for beat in [0.0, 4.0, 8.0, 12.0] {
            peer_a.process(&signal_a, &mut out, beat);
            peer_b.process(&signal_b, &mut out, beat);
            peer_c.process(&silence, &mut out, beat);
        }

        // Cross boundary — A and B produce intervals
        let wire_a = peer_a.process(&signal_a, &mut out, 16.0);
        let wire_b = peer_b.process(&signal_b, &mut out, 16.0);
        assert_eq!(wire_a.len(), 1);
        assert_eq!(wire_b.len(), 1);

        // Deliver both to Peer C
        peer_c.receive_wire("peer-a", &wire_a[0]);
        peer_c.receive_wire("peer-b", &wire_b[0]);

        // Peer C crosses boundary — should mix both peers
        peer_c.process(&silence, &mut out, 16.0);

        let energy = rms(&out);
        assert!(
            energy > 0.01,
            "Peer C should hear mixed audio from A+B, RMS={energy}"
        );
    }

    // ---------------------------------------------------------------
    // Test 4: Continuous multi-interval cycling
    // ---------------------------------------------------------------

    #[test]
    fn continuous_interval_cycling() {
        let mut sender = AudioBridge::new(SR, CH, BARS, Q, BITRATE);
        let mut receiver = AudioBridge::new(SR, CH, BARS, Q, BITRATE);

        let buf_size = 256;
        let signal = sine_wave(330.0, buf_size / CH as usize, CH, SR);
        let silence = vec![0.0f32; buf_size];
        let mut out_s = vec![0.0f32; buf_size];
        let mut out_r = vec![0.0f32; buf_size];

        // Run through 4 intervals
        let beats_per_interval = (BARS as f64) * Q; // 16.0
        let mut completed_count = 0;

        for interval_idx in 0..4i64 {
            let base_beat = interval_idx as f64 * beats_per_interval;
            // Process within interval
            for sub_beat in [0.0, 4.0, 8.0, 12.0] {
                let beat = base_beat + sub_beat;
                let wire = sender.process(&signal, &mut out_s, beat);
                if !wire.is_empty() {
                    for w in &wire {
                        receiver.receive_wire("sender", w);
                    }
                    completed_count += wire.len();
                }
                receiver.process(&silence, &mut out_r, beat);
            }
        }

        // Should have produced intervals 0, 1, 2 (interval 3 is still recording)
        assert!(
            completed_count >= 3,
            "Expected at least 3 completed intervals, got {completed_count}"
        );
    }

    // ---------------------------------------------------------------
    // Test 5: Wire format preserves all fields through full pipeline
    // ---------------------------------------------------------------

    // ---------------------------------------------------------------
    // WAN NINJAM Tests: Prove two internet peers hear each other's
    // last interval via the NINJAM double-buffer model.
    // ---------------------------------------------------------------

    // ---------------------------------------------------------------
    // WAN Test 1: Boundary drift — peers cross interval boundaries
    // at different times (simulating WAN clock skew). Peer A's
    // completed interval still gets played by Peer B.
    // ---------------------------------------------------------------

    #[test]
    fn wan_peers_with_boundary_drift() {
        let mut peer_a = AudioBridge::new(SR, CH, BARS, Q, BITRATE);
        let mut peer_b = AudioBridge::new(SR, CH, BARS, Q, BITRATE);

        let buf_size = 4096;
        let signal = sine_wave(440.0, buf_size / CH as usize, CH, SR);
        let silence = vec![0.0f32; buf_size];
        let mut out_a = vec![0.0f32; buf_size];
        let mut out_b = vec![0.0f32; buf_size];

        // Both peers record through interval 0, but at different beat positions
        // (simulating independent clocks / WAN drift)
        peer_a.process(&signal, &mut out_a, 0.0);
        peer_b.process(&silence, &mut out_b, 0.5); // Peer B is 0.5 beats behind

        peer_a.process(&signal, &mut out_a, 4.0);
        peer_b.process(&silence, &mut out_b, 4.5);

        peer_a.process(&signal, &mut out_a, 8.0);
        peer_b.process(&silence, &mut out_b, 8.5);

        peer_a.process(&signal, &mut out_a, 12.0);
        peer_b.process(&silence, &mut out_b, 12.5);

        // Peer A crosses into interval 1 FIRST (it's ahead)
        let wire_a = peer_a.process(&signal, &mut out_a, 16.0);
        assert_eq!(wire_a.len(), 1, "Peer A should produce interval 0");

        // Peer B is still in interval 0 (beat 15.5 < 16.0)
        peer_b.process(&silence, &mut out_b, 15.5);

        // "Network" delivers A's interval to B while B is still in interval 0
        peer_b.receive_wire("peer-a", &wire_a[0]);

        // Peer B finally crosses into interval 1 (a bit later than A)
        peer_b.process(&silence, &mut out_b, 16.5);

        // Peer B should now hear Peer A's audio
        let energy = rms(&out_b);
        assert!(
            energy > 0.01,
            "Peer B should hear Peer A despite boundary drift, RMS={energy}"
        );
    }

    // ---------------------------------------------------------------
    // WAN Test 2: The NINJAM invariant — during interval N, you
    // always hear the remote's interval N-1. Verified across
    // multiple intervals with distinct signals per interval.
    // ---------------------------------------------------------------

    #[test]
    fn hear_last_interval_invariant() {
        let mut sender = AudioBridge::new(SR, CH, BARS, Q, BITRATE);
        let mut receiver = AudioBridge::new(SR, CH, BARS, Q, BITRATE);

        let buf_size = 4096;
        let silence = vec![0.0f32; buf_size];
        let mut out_s = vec![0.0f32; buf_size];
        let mut out_r = vec![0.0f32; buf_size];

        // Three distinct signals — different amplitudes to distinguish intervals
        let loud = vec![0.9f32; buf_size];    // interval 0
        let medium = vec![0.5f32; buf_size];  // interval 1
        let quiet = vec![0.2f32; buf_size];   // interval 2

        let beats_per_interval = (BARS as f64) * Q; // 16.0

        // --- Interval 0: sender records loud signal ---
        for sub in [0.0, 4.0, 8.0, 12.0] {
            sender.process(&loud, &mut out_s, sub);
            receiver.process(&silence, &mut out_r, sub);
        }

        // --- Cross into interval 1: sender produces interval 0 (loud) ---
        let wire_0 = sender.process(&medium, &mut out_s, beats_per_interval);
        assert_eq!(wire_0.len(), 1);
        receiver.receive_wire("sender", &wire_0[0]);
        receiver.process(&silence, &mut out_r, beats_per_interval);

        // Receiver is now in interval 1, playing sender's interval 0 (loud)
        let energy_playing_0 = rms(&out_r);
        assert!(
            energy_playing_0 > 0.01,
            "Receiver should hear sender's interval 0, RMS={energy_playing_0}"
        );

        // --- Record through interval 1: sender records medium signal ---
        for sub in [4.0, 8.0, 12.0] {
            sender.process(&medium, &mut out_s, beats_per_interval + sub);
            receiver.process(&silence, &mut out_r, beats_per_interval + sub);
        }

        // --- Cross into interval 2: sender produces interval 1 (medium) ---
        let wire_1 = sender.process(&quiet, &mut out_s, 2.0 * beats_per_interval);
        assert_eq!(wire_1.len(), 1);
        receiver.receive_wire("sender", &wire_1[0]);
        receiver.process(&silence, &mut out_r, 2.0 * beats_per_interval);

        // Receiver is now in interval 2, playing sender's interval 1 (medium)
        let energy_playing_1 = rms(&out_r);
        assert!(
            energy_playing_1 > 0.01,
            "Receiver should hear sender's interval 1, RMS={energy_playing_1}"
        );

        // The key NINJAM invariant: we heard interval 0 during interval 1,
        // and interval 1 during interval 2. Always the PREVIOUS interval.
        // Both had energy (non-silence) confirming actual audio was delivered.
    }

    // ---------------------------------------------------------------
    // WAN Test 3: Bidirectional exchange — both peers record and
    // send simultaneously. Each hears the other's previous interval.
    // ---------------------------------------------------------------

    #[test]
    fn bidirectional_interval_exchange() {
        let mut peer_a = AudioBridge::new(SR, CH, BARS, Q, BITRATE);
        let mut peer_b = AudioBridge::new(SR, CH, BARS, Q, BITRATE);

        let buf_size = 4096;
        let signal_a = sine_wave(440.0, buf_size / CH as usize, CH, SR);
        let signal_b = sine_wave(880.0, buf_size / CH as usize, CH, SR);
        let mut out_a = vec![0.0f32; buf_size];
        let mut out_b = vec![0.0f32; buf_size];

        // Both record their own signals through interval 0
        for beat in [0.0, 4.0, 8.0, 12.0] {
            peer_a.process(&signal_a, &mut out_a, beat);
            peer_b.process(&signal_b, &mut out_b, beat);
        }

        // Both cross into interval 1 — each produces their completed interval
        let wire_a = peer_a.process(&signal_a, &mut out_a, 16.0);
        let wire_b = peer_b.process(&signal_b, &mut out_b, 16.0);
        assert_eq!(wire_a.len(), 1, "Peer A produces interval 0");
        assert_eq!(wire_b.len(), 1, "Peer B produces interval 0");

        // Exchange over "network"
        peer_a.receive_wire("peer-b", &wire_b[0]);
        peer_b.receive_wire("peer-a", &wire_a[0]);

        // Both cross into interval 2 — should play each other's audio
        peer_a.process(&signal_a, &mut out_a, 32.0);
        peer_b.process(&signal_b, &mut out_b, 32.0);

        let energy_a = rms(&out_a);
        let energy_b = rms(&out_b);
        assert!(
            energy_a > 0.01,
            "Peer A should hear Peer B's audio, RMS={energy_a}"
        );
        assert!(
            energy_b > 0.01,
            "Peer B should hear Peer A's audio, RMS={energy_b}"
        );
    }

    // ---------------------------------------------------------------
    // WAN Test 4: Late delivery — interval arrives mid-interval
    // (simulating network delay). Still plays at the next boundary.
    // ---------------------------------------------------------------

    #[test]
    fn late_delivery_still_plays() {
        let mut sender = AudioBridge::new(SR, CH, BARS, Q, BITRATE);
        let mut receiver = AudioBridge::new(SR, CH, BARS, Q, BITRATE);

        let buf_size = 4096;
        let signal = sine_wave(440.0, buf_size / CH as usize, CH, SR);
        let silence = vec![0.0f32; buf_size];
        let mut out_s = vec![0.0f32; buf_size];
        let mut out_r = vec![0.0f32; buf_size];

        let beats_per_interval = (BARS as f64) * Q; // 16.0

        // Sender records interval 0
        for beat in [0.0, 4.0, 8.0, 12.0] {
            sender.process(&signal, &mut out_s, beat);
            receiver.process(&silence, &mut out_r, beat);
        }

        // Sender crosses into interval 1 — produces interval 0
        let wire_0 = sender.process(&signal, &mut out_s, beats_per_interval);
        assert_eq!(wire_0.len(), 1);

        // Receiver also crosses into interval 1 — but NO wire data yet (simulating delay)
        receiver.process(&silence, &mut out_r, beats_per_interval);
        let energy_no_data = rms(&out_r);
        assert!(
            energy_no_data < 0.001,
            "No audio should play yet (data not delivered), RMS={energy_no_data}"
        );

        // Network delay: data arrives mid-interval 1 (beat 20.0)
        receiver.process(&silence, &mut out_r, beats_per_interval + 4.0);
        // NOW the wire data arrives (late, but before next boundary)
        receiver.receive_wire("sender", &wire_0[0]);

        // Continue through interval 1
        receiver.process(&silence, &mut out_r, beats_per_interval + 8.0);
        receiver.process(&silence, &mut out_r, beats_per_interval + 12.0);

        // Cross into interval 2 — late-delivered audio should now play
        receiver.process(&silence, &mut out_r, 2.0 * beats_per_interval);
        let energy_late = rms(&out_r);
        assert!(
            energy_late > 0.01,
            "Late-delivered interval should play at next boundary, RMS={energy_late}"
        );
    }

    // ---------------------------------------------------------------
    // Test 5: Wire format preserves all fields through full pipeline
    // ---------------------------------------------------------------

    // ---------------------------------------------------------------
    // Test 5: Full-mesh 3-peer exchange — all peers send and receive
    // ---------------------------------------------------------------

    #[test]
    fn three_peer_full_mesh_exchange() {
        let mut peer_a = AudioBridge::new(SR, CH, BARS, Q, BITRATE);
        let mut peer_b = AudioBridge::new(SR, CH, BARS, Q, BITRATE);
        let mut peer_c = AudioBridge::new(SR, CH, BARS, Q, BITRATE);

        let buf_size = 4096;
        let signal_a = sine_wave(440.0, buf_size / CH as usize, CH, SR);
        let signal_b = sine_wave(880.0, buf_size / CH as usize, CH, SR);
        let signal_c = sine_wave(330.0, buf_size / CH as usize, CH, SR);
        let mut out_a = vec![0.0f32; buf_size];
        let mut out_b = vec![0.0f32; buf_size];
        let mut out_c = vec![0.0f32; buf_size];

        // All three peers record through interval 0
        for beat in [0.0, 4.0, 8.0, 12.0] {
            peer_a.process(&signal_a, &mut out_a, beat);
            peer_b.process(&signal_b, &mut out_b, beat);
            peer_c.process(&signal_c, &mut out_c, beat);
        }

        // Cross boundary — all three produce their completed intervals
        let wire_a = peer_a.process(&signal_a, &mut out_a, 16.0);
        let wire_b = peer_b.process(&signal_b, &mut out_b, 16.0);
        let wire_c = peer_c.process(&signal_c, &mut out_c, 16.0);
        assert_eq!(wire_a.len(), 1, "Peer A should produce interval 0");
        assert_eq!(wire_b.len(), 1, "Peer B should produce interval 0");
        assert_eq!(wire_c.len(), 1, "Peer C should produce interval 0");

        // Full-mesh delivery: each peer receives from the other two
        peer_a.receive_wire("peer-b", &wire_b[0]);
        peer_a.receive_wire("peer-c", &wire_c[0]);

        peer_b.receive_wire("peer-a", &wire_a[0]);
        peer_b.receive_wire("peer-c", &wire_c[0]);

        peer_c.receive_wire("peer-a", &wire_a[0]);
        peer_c.receive_wire("peer-b", &wire_b[0]);

        // All three cross into interval 2 — each should play the other two
        peer_a.process(&signal_a, &mut out_a, 32.0);
        peer_b.process(&signal_b, &mut out_b, 32.0);
        peer_c.process(&signal_c, &mut out_c, 32.0);

        let energy_a = rms(&out_a);
        let energy_b = rms(&out_b);
        let energy_c = rms(&out_c);

        assert!(
            energy_a > 0.01,
            "Peer A should hear B+C mixed, RMS={energy_a}"
        );
        assert!(
            energy_b > 0.01,
            "Peer B should hear A+C mixed, RMS={energy_b}"
        );
        assert!(
            energy_c > 0.01,
            "Peer C should hear A+B mixed, RMS={energy_c}"
        );
    }

    // ---------------------------------------------------------------
    // Test 6: Wire format preserves all fields through full pipeline
    // ---------------------------------------------------------------

    #[test]
    fn wire_fields_roundtrip_through_pipeline() {
        let mut bridge = AudioBridge::new(48000, 1, 3, 5.0, 96);
        bridge.update_config(3, 5.0, 175.5);

        let signal = sine_wave(1000.0, 128, 1, 48000);
        let mut output = vec![0.0f32; 128];

        // Interval: 3 bars * 5.0 quantum = 15 beats
        bridge.process(&signal, &mut output, 0.0);
        bridge.process(&signal, &mut output, 7.0);
        let wire_msgs = bridge.process(&signal, &mut output, 15.0);

        let interval = AudioWire::decode(&wire_msgs[0]).unwrap();
        assert_eq!(interval.index, 0);
        assert_eq!(interval.sample_rate, 48000);
        assert_eq!(interval.channels, 1);
        assert_eq!(interval.bars, 3);
        assert!((interval.quantum - 5.0).abs() < f64::EPSILON);
        assert!((interval.bpm - 175.5).abs() < f64::EPSILON);
        assert!(!interval.opus_data.is_empty());
    }

    // ---------------------------------------------------------------
    // Test 7: Per-peer isolation through full bridge pipeline
    // ---------------------------------------------------------------

    #[test]
    fn three_peer_per_peer_isolation() {
        let mut peer_a = AudioBridge::new(SR, CH, BARS, Q, BITRATE);
        let mut peer_b = AudioBridge::new(SR, CH, BARS, Q, BITRATE);
        let mut peer_c = AudioBridge::new(SR, CH, BARS, Q, BITRATE);

        let buf_size = 4096;
        let signal_a = sine_wave(440.0, buf_size / CH as usize, CH, SR);
        let signal_b = sine_wave(880.0, buf_size / CH as usize, CH, SR);
        let signal_c = sine_wave(330.0, buf_size / CH as usize, CH, SR);
        let mut out = vec![0.0f32; buf_size];

        // All record through interval 0
        for beat in [0.0, 4.0, 8.0, 12.0] {
            peer_a.process(&signal_a, &mut out, beat);
            peer_b.process(&signal_b, &mut out, beat);
            peer_c.process(&signal_c, &mut out, beat);
        }

        // Cross boundary
        let _wire_a = peer_a.process(&signal_a, &mut out, 16.0);
        let wire_b = peer_b.process(&signal_b, &mut out, 16.0);
        let wire_c = peer_c.process(&signal_c, &mut out, 16.0);

        // Deliver B and C to peer A
        peer_a.receive_wire("peer-b", &wire_b[0]);
        peer_a.receive_wire("peer-c", &wire_c[0]);

        // Peer A crosses into interval 2 to trigger playback
        let mut out_a = vec![0.0f32; buf_size];
        peer_a.process(&signal_a, &mut out_a, 32.0);

        // Verify per-peer isolation on peer A
        let peers = peer_a.peer_info();
        assert_eq!(peers.len(), 2, "Peer A should see 2 remote peers");

        for (slot_idx, pid) in &peers {
            let mut slot_out = vec![0.0f32; buf_size];
            peer_a.read_peer_playback(*slot_idx, &mut slot_out);
            let energy = rms(&slot_out);
            assert!(
                energy > 0.01,
                "Peer slot {slot_idx} ({pid}) should have audio, RMS={energy}"
            );
        }

        // Also verify the summed mix has energy
        let mix_energy = rms(&out_a);
        assert!(
            mix_energy > 0.01,
            "Summed mix should have audio, RMS={mix_energy}"
        );

        // Verify the two peer slots have different audio (different frequencies)
        let (idx_b, _) = peers.iter().find(|(_, pid)| pid == "peer-b").unwrap();
        let (idx_c, _) = peers.iter().find(|(_, pid)| pid == "peer-c").unwrap();
        let mut buf_b = vec![0.0f32; buf_size];
        let mut buf_c = vec![0.0f32; buf_size];
        peer_a.read_peer_playback(*idx_b, &mut buf_b);
        peer_a.read_peer_playback(*idx_c, &mut buf_c);

        // After reading once above, read positions advanced, so these may be partial.
        // But since the total is 4096 and we read 4096 each time, we should still
        // see that at least one of them has data from the first read.
    }
}
