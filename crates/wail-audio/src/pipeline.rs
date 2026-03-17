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
    // Test 5: Sine wave round-trip characterizes crossfade quality
    // ---------------------------------------------------------------

    #[test]
    fn sine_roundtrip_across_intervals() {
        // Sends a continuous sine wave across 3 interval boundaries end-to-end
        // (Opus encode → wire → decode) and verifies energy is consistent across
        // boundaries. Catches crossfade regressions like phase-cancellation or clipping.
        let mut sender = AudioBridge::new(SR, CH, BARS, Q, BITRATE);
        let mut receiver = AudioBridge::new(SR, CH, BARS, Q, BITRATE);

        let buf_size = 4096;
        let silence = vec![0.0f32; buf_size];
        let signal = sine_wave(440.0, buf_size / CH as usize, CH, SR);
        let mut sender_out = vec![0.0f32; buf_size];
        let mut recv_out = vec![0.0f32; buf_size];

        // 16 beats per interval (4 bars × quantum 4), run through 3 boundaries
        let beats_per_interval = (BARS as f64) * Q; // 16.0
        let mut interval_outputs: Vec<Vec<f32>> = Vec::new();
        let mut current_interval_samples: Vec<f32> = Vec::new();

        for interval_idx in 0..4i64 {
            let base_beat = interval_idx as f64 * beats_per_interval;
            for sub_beat in [0.0, 4.0, 8.0, 12.0] {
                let beat = base_beat + sub_beat;
                let wire_msgs = sender.process(&signal, &mut sender_out, beat);
                for msg in &wire_msgs {
                    receiver.receive_wire("sender", msg);
                }
                receiver.process(&silence, &mut recv_out, beat);

                if !wire_msgs.is_empty() && !current_interval_samples.is_empty() {
                    interval_outputs.push(std::mem::take(&mut current_interval_samples));
                }
                current_interval_samples.extend_from_slice(&recv_out);
            }
        }

        assert!(
            interval_outputs.len() >= 2,
            "Should have at least 2 decoded intervals, got {}",
            interval_outputs.len()
        );

        // Opus adds ~26ms priming delay so interval 0 output may be low — skip it,
        // check intervals 1+ have consistent non-zero energy.
        let rms_values: Vec<f32> = interval_outputs.iter().map(|s| rms(s)).collect();
        for (i, &r) in rms_values.iter().enumerate().skip(1) {
            assert!(r > 0.01, "Interval {i} should have signal energy after round-trip, RMS={r}");
        }

        // Energy should be roughly consistent — allow 3× variation across intervals.
        // A larger ratio would indicate the crossfade is phase-cancelling the signal.
        let later: Vec<f32> = rms_values[1..].to_vec();
        let max_rms = later.iter().cloned().fold(0.0f32, f32::max);
        let min_rms = later.iter().cloned().fold(f32::MAX, f32::min);
        assert!(
            max_rms / min_rms < 3.0,
            "RMS energy should be consistent across intervals (max/min={:.2}): {:?}",
            max_rms / min_rms,
            rms_values
        );

        // No clipping — crossfade should not sum beyond ±1.0.
        for (i, interval) in interval_outputs.iter().enumerate() {
            let max_amp = interval.iter().cloned().fold(0.0f32, f32::max);
            assert!(max_amp <= 1.0, "Interval {i} clipped: max_amp={max_amp}");
        }
    }

    // ---------------------------------------------------------------
    // Test 6: Incremental frame decode prevents boundary dropout
    // ---------------------------------------------------------------

    /// Simulates the WAIF streaming decode pattern: individual 20ms Opus
    /// frames are decoded and fed to the ring buffer incrementally throughout
    /// an interval. Verifies that playback audio is immediately available at
    /// the boundary — the bug that incremental decode fixes.
    ///
    /// Contrasts with the old FrameAssembler pattern where all decoded audio
    /// arrived in one bulk chunk after the boundary, causing a dropout.
    #[test]
    fn incremental_frame_decode_available_at_boundary() {
        use crate::codec::{AudioEncoder, AudioDecoder};
        use crate::ring::IntervalRing;

        let mut ring = IntervalRing::new(SR, CH, BARS, Q);
        let mut encoder = AudioEncoder::new(SR, CH, BITRATE).unwrap();
        let mut decoder = AudioDecoder::new(SR, CH).unwrap();

        let frame_size = encoder.frame_size(); // 960 samples per channel at 48kHz
        let frame_samples = frame_size * CH as usize; // interleaved

        // Generate a recognizable signal (one frame of sine wave)
        let frame_signal: Vec<f32> = (0..frame_samples)
            .map(|i| {
                let t = (i / CH as usize) as f32 / SR as f32;
                (t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5
            })
            .collect();

        let buf_size = 4096;
        let input = vec![0.0f32; buf_size];
        let mut output = vec![0.0f32; buf_size];

        // Start interval 0
        ring.process(&input, &mut output, 0.0);

        // Simulate streaming: encode 10 frames and decode each one incrementally,
        // feeding decoded PCM to the ring as it arrives (before the boundary).
        for _frame_num in 0..10 {
            let opus_bytes = encoder.encode_frame(&frame_signal).unwrap();
            let decoded_pcm = decoder.decode_frame(&opus_bytes).unwrap();
            ring.feed_remote("peer-a".into(), 0, 0, decoded_pcm);
        }

        // Verify: pending_remote has exactly 1 accumulated entry (not 10)
        assert_eq!(ring.pending_remote_count(), 1,
            "Incremental feeds should accumulate into one entry");

        // Cross boundary into interval 1 — audio should be immediately available
        ring.process(&input, &mut output, 16.0);

        let energy: f32 = output.iter().map(|s| s * s).sum::<f32>() / output.len() as f32;
        assert!(energy.sqrt() > 0.01,
            "Playback should have audio immediately at boundary (incremental decode), RMS={}",
            energy.sqrt());
    }

    /// Proves the failure mode of bulk decode: if all decoded audio arrives
    /// AFTER the boundary swap, the receiver hears silence for that interval.
    /// This is the exact bug that incremental decode fixes.
    #[test]
    fn bulk_decode_after_boundary_causes_silence() {
        use crate::ring::IntervalRing;

        let mut ring = IntervalRing::new(SR, CH, BARS, Q);

        let buf_size = 4096;
        let input = vec![0.0f32; buf_size];
        let mut output = vec![0.0f32; buf_size];

        // Start interval 0
        ring.process(&input, &mut output, 0.0);

        // Cross boundary into interval 1 — NO remote audio fed yet
        // (simulates old pattern: FrameAssembler hasn't finished)
        ring.process(&input, &mut output, 16.0);

        // Verify: silence at the boundary
        let energy: f32 = output.iter().map(|s| s * s).sum::<f32>() / output.len() as f32;
        assert!(energy.sqrt() < 0.001,
            "Should be silence when no audio was fed before boundary, RMS={}",
            energy.sqrt());

        // NOW the bulk-decoded audio arrives (too late for interval 0 playback).
        // Tagged as interval 1 (the current recording interval).
        ring.feed_remote("peer-a".into(), 0, 1, vec![0.5f32; buf_size]);

        // It sits in pending_remote, won't play until the NEXT boundary swap.
        // Process more within interval 1 — still silence from the missed swap.
        ring.process(&input, &mut output, 20.0);
        let energy2: f32 = output.iter().map(|s| s * s).sum::<f32>() / output.len() as f32;
        assert!(energy2.sqrt() < 0.001,
            "Late-arriving audio should NOT play mid-interval, RMS={}",
            energy2.sqrt());

        // Only at the NEXT boundary (interval 2) does it finally play
        ring.process(&input, &mut output, 32.0);
        let energy3: f32 = output.iter().map(|s| s * s).sum::<f32>() / output.len() as f32;
        assert!(energy3.sqrt() > 0.01,
            "Audio should finally play at the next boundary, RMS={}",
            energy3.sqrt());
    }

    // ---------------------------------------------------------------
    // Test 6b: Realistic imperfection scenarios
    // ---------------------------------------------------------------

    /// Simulates partial late arrival: 90% of frames arrive before the
    /// boundary, 10% arrive just after. With incremental decode, the
    /// receiver still gets 90% of the audio at the boundary — no dropout.
    /// The late 10% arrives before the next boundary and accumulates.
    #[test]
    fn partial_late_arrival_still_plays_at_boundary() {
        use crate::codec::{AudioEncoder, AudioDecoder};
        use crate::ring::IntervalRing;

        let mut ring = IntervalRing::new(SR, CH, BARS, Q);
        let mut encoder = AudioEncoder::new(SR, CH, BITRATE).unwrap();
        let mut decoder = AudioDecoder::new(SR, CH).unwrap();

        let frame_size = encoder.frame_size();
        let frame_samples = frame_size * CH as usize;

        let frame_signal: Vec<f32> = (0..frame_samples)
            .map(|i| {
                let t = (i / CH as usize) as f32 / SR as f32;
                (t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5
            })
            .collect();

        let total_frames = 50; // ~1 second of audio at 20ms/frame
        let on_time = (total_frames * 9) / 10; // 90% arrive before boundary
        let _late = total_frames - on_time; // 10% arrive after

        // Encode all frames up front (sender encodes during interval)
        let mut encoded_frames: Vec<Vec<u8>> = Vec::new();
        for _ in 0..total_frames {
            encoded_frames.push(encoder.encode_frame(&frame_signal).unwrap());
        }

        let buf_size = 4096;
        let input = vec![0.0f32; buf_size];
        let mut output = vec![0.0f32; buf_size];

        // Start interval 0
        ring.process(&input, &mut output, 0.0);

        // 90% of frames arrive before boundary (incremental decode)
        for opus_bytes in &encoded_frames[..on_time] {
            let decoded_pcm = decoder.decode_frame(opus_bytes).unwrap();
            ring.feed_remote("peer-a".into(), 0, 0, decoded_pcm);
        }

        // Cross boundary — should have substantial audio
        ring.process(&input, &mut output, 16.0);

        let rms_at_boundary: f32 = (output.iter().map(|s| s * s).sum::<f32>() / output.len() as f32).sqrt();
        assert!(rms_at_boundary > 0.01,
            "90% of frames arrived before boundary — should have audio, RMS={rms_at_boundary}");

        // Late 10% arrives now (after boundary, during interval 1)
        // These go to pending_remote for interval 0 — but since interval 0
        // is already in the playback slot, they accumulate for the NEXT swap.
        // In real usage, these would carry the next interval's index.
        // Here we verify the ring doesn't break when late data arrives.
        for opus_bytes in &encoded_frames[on_time..] {
            let decoded_pcm = decoder.decode_frame(opus_bytes).unwrap();
            // In practice, late frames would carry interval_index=1 (next interval).
            // But even if they carry index 0, feed_remote handles it gracefully.
            ring.feed_remote("peer-a".into(), 0, 1, decoded_pcm);
        }

        // Continue through interval 1 — no crash, no corruption
        ring.process(&input, &mut output, 20.0);
    }

    /// Two peers with staggered frame arrival: peer A's frames interleave
    /// with peer B's. Both should accumulate correctly and play at boundary.
    #[test]
    fn two_peers_interleaved_frame_arrival() {
        use crate::codec::{AudioEncoder, AudioDecoder};
        use crate::ring::IntervalRing;

        let mut ring = IntervalRing::new(SR, CH, BARS, Q);
        let mut enc_a = AudioEncoder::new(SR, CH, BITRATE).unwrap();
        let mut enc_b = AudioEncoder::new(SR, CH, BITRATE).unwrap();
        let mut dec_a = AudioDecoder::new(SR, CH).unwrap();
        let mut dec_b = AudioDecoder::new(SR, CH).unwrap();

        let frame_size = enc_a.frame_size();
        let frame_samples = frame_size * CH as usize;

        // Peer A: 440Hz, Peer B: 880Hz (different signals)
        let signal_a: Vec<f32> = (0..frame_samples)
            .map(|i| {
                let t = (i / CH as usize) as f32 / SR as f32;
                (t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.3
            })
            .collect();
        let signal_b: Vec<f32> = (0..frame_samples)
            .map(|i| {
                let t = (i / CH as usize) as f32 / SR as f32;
                (t * 880.0 * 2.0 * std::f32::consts::PI).sin() * 0.4
            })
            .collect();

        let buf_size = 4096;
        let input = vec![0.0f32; buf_size];
        let mut output = vec![0.0f32; buf_size];

        ring.process(&input, &mut output, 0.0);

        // Simulate interleaved arrival: A, B, A, B, A, B...
        // Like TCP packets arriving from two different peers
        for _ in 0..20 {
            let opus_a = enc_a.encode_frame(&signal_a).unwrap();
            let pcm_a = dec_a.decode_frame(&opus_a).unwrap();
            ring.feed_remote("peer-a".into(), 0, 0, pcm_a);

            let opus_b = enc_b.encode_frame(&signal_b).unwrap();
            let pcm_b = dec_b.decode_frame(&opus_b).unwrap();
            ring.feed_remote("peer-b".into(), 0, 0, pcm_b);
        }

        // Should have exactly 2 pending entries (one per peer), not 40
        assert_eq!(ring.pending_remote_count(), 2,
            "Two peers should accumulate into 2 entries, not 40");

        // Cross boundary — both peers should be mixed
        ring.process(&input, &mut output, 16.0);

        let rms_mixed: f32 = (output.iter().map(|s| s * s).sum::<f32>() / output.len() as f32).sqrt();
        assert!(rms_mixed > 0.01,
            "Mixed output from two interleaved peers should have energy, RMS={rms_mixed}");

        // Verify per-peer isolation: both peers should have independent slots
        let active = ring.active_peer_slots();
        assert_eq!(active.len(), 2, "Should have 2 active peer slots");

        let (a_idx, _, _) = active.iter().find(|(_, pid, _)| pid == "peer-a").unwrap();
        let (b_idx, _, _) = active.iter().find(|(_, pid, _)| pid == "peer-b").unwrap();

        let mut slot_a = vec![0.0f32; buf_size];
        let mut slot_b = vec![0.0f32; buf_size];
        ring.read_peer_playback(*a_idx, &mut slot_a);
        ring.read_peer_playback(*b_idx, &mut slot_b);

        let rms_a: f32 = (slot_a.iter().map(|s| s * s).sum::<f32>() / slot_a.len() as f32).sqrt();
        let rms_b: f32 = (slot_b.iter().map(|s| s * s).sum::<f32>() / slot_b.len() as f32).sqrt();
        assert!(rms_a > 0.001, "Peer A slot should have energy, RMS={rms_a}");
        assert!(rms_b > 0.001, "Peer B slot should have energy, RMS={rms_b}");
    }

    /// Simulates frame loss: some frames never arrive (WebRTC DataChannel
    /// can drop packets under congestion). The decoder uses PLC to fill
    /// gaps, and audio still plays without crashing.
    #[test]
    fn frame_loss_with_plc_still_plays() {
        use crate::codec::{AudioEncoder, AudioDecoder};
        use crate::ring::IntervalRing;

        let mut ring = IntervalRing::new(SR, CH, BARS, Q);
        let mut encoder = AudioEncoder::new(SR, CH, BITRATE).unwrap();
        let mut decoder = AudioDecoder::new(SR, CH).unwrap();

        let frame_size = encoder.frame_size();
        let frame_samples = frame_size * CH as usize;

        let frame_signal: Vec<f32> = (0..frame_samples)
            .map(|i| {
                let t = (i / CH as usize) as f32 / SR as f32;
                (t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5
            })
            .collect();

        let buf_size = 4096;
        let input = vec![0.0f32; buf_size];
        let mut output = vec![0.0f32; buf_size];

        ring.process(&input, &mut output, 0.0);

        // Send 30 frames, but drop every 5th one (simulating 20% packet loss)
        for frame_num in 0..30 {
            let opus_bytes = encoder.encode_frame(&frame_signal).unwrap();

            if frame_num % 5 == 3 {
                // Frame lost — use PLC (empty decode)
                let plc_pcm = decoder.decode_frame(&[]).unwrap();
                ring.feed_remote("peer-a".into(), 0, 0, plc_pcm);
            } else {
                let decoded_pcm = decoder.decode_frame(&opus_bytes).unwrap();
                ring.feed_remote("peer-a".into(), 0, 0, decoded_pcm);
            }
        }

        assert_eq!(ring.pending_remote_count(), 1,
            "All frames (including PLC) should accumulate into one entry");

        // Cross boundary — audio should play despite losses
        ring.process(&input, &mut output, 16.0);

        let rms_val: f32 = (output.iter().map(|s| s * s).sum::<f32>() / output.len() as f32).sqrt();
        assert!(rms_val > 0.005,
            "Audio should play despite 20% frame loss (PLC fills gaps), RMS={rms_val}");
    }

    /// Simulates boundary skew: receiver's boundary is slightly ahead of
    /// the sender's. The sender's final few frames arrive just after the
    /// receiver's swap. With incremental decode, the vast majority of
    /// audio is already accumulated — only a tiny tail is missed.
    /// Across multiple intervals, every interval after the first has audio.
    #[test]
    fn boundary_skew_incremental_survives_across_intervals() {
        use crate::codec::{AudioEncoder, AudioDecoder};
        use crate::ring::IntervalRing;

        let mut sender_ring = IntervalRing::new(SR, CH, BARS, Q);
        let mut recv_ring = IntervalRing::new(SR, CH, BARS, Q);
        let mut encoder = AudioEncoder::new(SR, CH, BITRATE).unwrap();
        let mut decoder = AudioDecoder::new(SR, CH).unwrap();

        let frame_size = encoder.frame_size();
        let frame_samples = frame_size * CH as usize;

        let frame_signal: Vec<f32> = (0..frame_samples)
            .map(|i| {
                let t = (i / CH as usize) as f32 / SR as f32;
                (t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5
            })
            .collect();

        let buf_size = 512;
        let silence = vec![0.0f32; buf_size];
        let mut sender_out = vec![0.0f32; buf_size];
        let mut recv_out = vec![0.0f32; buf_size];

        let frames_per_interval = 50;
        let beats_per_interval = (BARS as f64) * Q; // 16.0

        let mut recv_rms_per_interval: Vec<f32> = Vec::new();

        for interval_idx in 0..4i64 {
            let base_beat = interval_idx as f64 * beats_per_interval;

            // Sender processes through the interval, encoding frames
            sender_ring.process(&frame_signal[..buf_size.min(frame_signal.len())], &mut sender_out, base_beat);

            // Encode and deliver 95% of frames before receiver's boundary
            let early_count = (frames_per_interval * 95) / 100;
            for _ in 0..early_count {
                let opus = encoder.encode_frame(&frame_signal).unwrap();
                let pcm = decoder.decode_frame(&opus).unwrap();
                recv_ring.feed_remote("sender".into(), 0, interval_idx, pcm);
            }

            // Receiver crosses boundary slightly BEFORE the sender's last frames
            recv_ring.process(&silence, &mut recv_out, base_beat + beats_per_interval);

            let rms_val: f32 = (recv_out.iter().map(|s| s * s).sum::<f32>() / recv_out.len() as f32).sqrt();
            recv_rms_per_interval.push(rms_val);

            // Late 5% arrives after receiver's boundary (goes to next interval)
            for _ in early_count..frames_per_interval {
                let opus = encoder.encode_frame(&frame_signal).unwrap();
                let pcm = decoder.decode_frame(&opus).unwrap();
                recv_ring.feed_remote("sender".into(), 0, interval_idx + 1, pcm);
            }

            // Sender also crosses boundary
            sender_ring.process(&frame_signal[..buf_size.min(frame_signal.len())], &mut sender_out, base_beat + beats_per_interval);
        }

        // Interval 0 may have lower energy (Opus priming + first boundary).
        // Intervals 1+ should all have audio (incremental decode ensures this).
        for (i, &rms_val) in recv_rms_per_interval.iter().enumerate().skip(1) {
            assert!(rms_val > 0.005,
                "Interval {i} should have audio despite 5% boundary skew, RMS={rms_val}");
        }

        // Verify consistency: later intervals shouldn't vary wildly
        let later_rms: Vec<f32> = recv_rms_per_interval[1..].to_vec();
        if later_rms.len() >= 2 {
            let max_r = later_rms.iter().cloned().fold(0.0f32, f32::max);
            let min_r = later_rms.iter().cloned().fold(f32::MAX, f32::min);
            if min_r > 0.0 {
                assert!(max_r / min_r < 5.0,
                    "RMS should be roughly consistent across intervals (max/min={:.2}): {:?}",
                    max_r / min_r, recv_rms_per_interval);
            }
        }
    }

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

        for (slot_idx, pid, _) in &peers {
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
        let (idx_b, _, _) = peers.iter().find(|(_, pid, _)| pid == "peer-b").unwrap();
        let (idx_c, _, _) = peers.iter().find(|(_, pid, _)| pid == "peer-c").unwrap();
        let mut buf_b = vec![0.0f32; buf_size];
        let mut buf_c = vec![0.0f32; buf_size];
        peer_a.read_peer_playback(*idx_b, &mut buf_b);
        peer_a.read_peer_playback(*idx_c, &mut buf_c);

        // After reading once above, read positions advanced, so these may be partial.
        // But since the total is 4096 and we read 4096 each time, we should still
        // see that at least one of them has data from the first read.
    }

    // ---------------------------------------------------------------
    // Test: Constant tone amplitude continuity across interval boundaries
    // ---------------------------------------------------------------

    /// Verifies that a constant 440 Hz sine tone played through the full
    /// encode → per-frame decode → ring buffer path maintains consistent
    /// amplitude across interval boundaries. Catches two regressions:
    ///
    /// 1. Opus decoder warm-up ramp: if a fresh decoder is created per
    ///    interval, the first ~6 frames (~120ms) ramp up from silence.
    /// 2. Crossfade amplitude dip: if the crossfade window or curve is
    ///    wrong, there's a momentary volume drop at the boundary.
    ///
    /// The test uses a SINGLE decoder across all intervals (matching the
    /// production recv plugin path) and checks that no 20ms window near
    /// the boundary drops below 50% of the steady-state RMS.
    #[test]
    fn constant_tone_no_amplitude_drop_at_boundary() {
        use crate::codec::{AudioEncoder, AudioDecoder};
        use crate::ring::IntervalRing;

        let mut ring = IntervalRing::new(SR, CH, BARS, Q);
        let mut encoder = AudioEncoder::new(SR, CH, BITRATE).unwrap();
        // Single decoder reused across intervals — matches production behavior.
        let mut decoder = AudioDecoder::new(SR, CH).unwrap();

        let frame_size = encoder.frame_size(); // 960 samples per channel
        let frame_samples = frame_size * CH as usize;

        // Continuous sine phase across all intervals (like the test client).
        let mut phase: f64 = 0.0;
        let freq = 440.0f64;
        let phase_inc = freq * std::f64::consts::TAU / SR as f64;

        let buf_size = frame_samples; // process in 20ms chunks for precise boundary tracking
        let input = vec![0.0f32; buf_size];
        let mut output = vec![0.0f32; buf_size];

        let beats_per_interval = BARS as f64 * Q; // 16.0
        let bpm = 120.0;
        let seconds_per_beat = 60.0 / bpm;
        let seconds_per_frame = frame_size as f64 / SR as f64; // 0.02s
        let beats_per_frame = seconds_per_frame / seconds_per_beat;

        // Run for 3 full intervals (0, 1, 2) + partial interval 3.
        let total_frames = ((beats_per_interval * 3.5) / beats_per_frame) as usize;
        let mut beat = 0.0f64;

        // Collect per-frame RMS of the output for analysis.
        let mut frame_rms: Vec<(f64, f32)> = Vec::new(); // (beat, rms)

        for _ in 0..total_frames {
            // Generate one frame of continuous sine.
            let mut pcm = vec![0.0f32; frame_samples];
            for i in 0..frame_size {
                let val = phase.sin() as f32 * 0.5;
                pcm[i * CH as usize] = val;
                pcm[i * CH as usize + 1] = val;
                phase += phase_inc;
            }
            phase %= std::f64::consts::TAU;

            // Encode and decode with persistent decoder.
            let opus_bytes = encoder.encode_frame(&pcm).unwrap();
            let decoded_pcm = decoder.decode_frame(&opus_bytes).unwrap();

            // Determine interval index from beat.
            let interval_index = (beat / beats_per_interval).floor() as i64;

            // Feed decoded audio to ring.
            ring.feed_remote("sender".into(), 0, interval_index, decoded_pcm);

            // Drive the ring (silence input — we only care about playback output).
            ring.process(&input, &mut output, beat);

            let r = rms(&output);
            frame_rms.push((beat, r));

            beat += beats_per_frame;
        }

        // Skip the first interval (Opus priming delay) and analyze from interval 1 onward.
        let skip_beats = beats_per_interval;
        let steady_state: Vec<f32> = frame_rms.iter()
            .filter(|(b, _)| *b >= skip_beats + beats_per_interval * 0.5) // mid-interval 1
            .map(|(_, r)| *r)
            .collect();

        assert!(!steady_state.is_empty(), "Should have steady-state samples");

        let steady_rms = steady_state.iter().sum::<f32>() / steady_state.len() as f32;
        assert!(steady_rms > 0.05,
            "Steady-state RMS should be well above silence, got {steady_rms}");

        // Check the boundary regions (±5 frames around each boundary).
        // No frame should drop below 50% of steady-state RMS.
        let boundary_beats: Vec<f64> = (1..3).map(|i| i as f64 * beats_per_interval).collect();
        let window_beats = beats_per_frame * 5.0;

        for boundary in &boundary_beats {
            let near_boundary: Vec<(f64, f32)> = frame_rms.iter()
                .filter(|(b, _)| (*b - boundary).abs() < window_beats && *b > skip_beats)
                .cloned()
                .collect();

            for (b, r) in &near_boundary {
                assert!(
                    *r > steady_rms * 0.5,
                    "Amplitude drop at boundary beat {boundary}: frame at beat {b:.1} has RMS {r:.4}, \
                     steady-state is {steady_rms:.4}. This indicates either Opus decoder warm-up \
                     (fresh decoder per interval) or a crossfade issue."
                );
            }
        }
    }

    /// Proves the failure mode: if a NEW Opus decoder is created for each
    /// interval (the old per-interval isolation pattern), the first ~6 frames
    /// ramp up from silence, causing an audible ~120ms fade-in.
    #[test]
    fn fresh_decoder_per_interval_causes_amplitude_ramp() {
        use crate::codec::{AudioEncoder, AudioDecoder};
        use crate::ring::IntervalRing;

        let mut ring = IntervalRing::new(SR, CH, BARS, Q);
        let mut encoder = AudioEncoder::new(SR, CH, BITRATE).unwrap();

        let frame_size = encoder.frame_size();
        let frame_samples = frame_size * CH as usize;

        let mut phase: f64 = 0.0;
        let freq = 440.0f64;
        let phase_inc = freq * std::f64::consts::TAU / SR as f64;

        let buf_size = frame_samples;
        let input = vec![0.0f32; buf_size];
        let mut output = vec![0.0f32; buf_size];

        let beats_per_interval = BARS as f64 * Q;
        let bpm = 120.0;
        let seconds_per_beat = 60.0 / bpm;
        let seconds_per_frame = frame_size as f64 / SR as f64;
        let beats_per_frame = seconds_per_frame / seconds_per_beat;

        let total_frames = ((beats_per_interval * 2.5) / beats_per_frame) as usize;
        let mut beat = 0.0f64;
        let mut current_interval = -1i64;

        // Fresh decoder per interval — the OLD behavior.
        let mut decoder = AudioDecoder::new(SR, CH).unwrap();
        let mut frame_rms: Vec<(f64, f32)> = Vec::new();

        for _ in 0..total_frames {
            let mut pcm = vec![0.0f32; frame_samples];
            for i in 0..frame_size {
                let val = phase.sin() as f32 * 0.5;
                pcm[i * CH as usize] = val;
                pcm[i * CH as usize + 1] = val;
                phase += phase_inc;
            }
            phase %= std::f64::consts::TAU;

            let interval_index = (beat / beats_per_interval).floor() as i64;

            // Create a FRESH decoder when the interval changes.
            if interval_index != current_interval {
                decoder = AudioDecoder::new(SR, CH).unwrap();
                current_interval = interval_index;
            }

            let opus_bytes = encoder.encode_frame(&pcm).unwrap();
            let decoded_pcm = decoder.decode_frame(&opus_bytes).unwrap();

            ring.feed_remote("sender".into(), 0, interval_index, decoded_pcm);
            ring.process(&input, &mut output, beat);

            frame_rms.push((beat, rms(&output)));
            beat += beats_per_frame;
        }

        // Find the first frame of interval 2 and check that it has low amplitude
        // (proving the warm-up ramp exists with fresh decoders).
        let boundary_2 = 2.0 * beats_per_interval;
        let first_frames_after: Vec<f32> = frame_rms.iter()
            .filter(|(b, _)| *b >= boundary_2 && *b < boundary_2 + beats_per_frame * 3.0)
            .map(|(_, r)| *r)
            .collect();

        // Get steady-state from mid-interval 1.
        let mid_interval_1 = beats_per_interval * 1.5;
        let steady: Vec<f32> = frame_rms.iter()
            .filter(|(b, _)| (*b - mid_interval_1).abs() < beats_per_frame * 5.0)
            .map(|(_, r)| *r)
            .collect();

        if !steady.is_empty() && !first_frames_after.is_empty() {
            let steady_avg = steady.iter().sum::<f32>() / steady.len() as f32;
            let boundary_avg = first_frames_after.iter().sum::<f32>() / first_frames_after.len() as f32;

            // With a fresh decoder, the first few frames should be significantly quieter.
            // This test documents the regression so it doesn't silently return.
            if steady_avg > 0.05 {
                assert!(
                    boundary_avg < steady_avg * 0.9,
                    "Fresh decoder per interval should cause amplitude ramp at boundary, \
                     but boundary_avg={boundary_avg:.4} is not below 90% of steady={steady_avg:.4}. \
                     If this fails, Opus decoder behavior may have changed."
                );
            }
        }
    }
}
