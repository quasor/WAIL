use crate::codec::{AudioDecoder, AudioEncoder};
use crate::interval::AudioInterval;
use crate::ring::{CompletedInterval, IntervalRing};
use crate::wire::AudioWire;

/// AudioBridge: connects the IntervalRing to Opus encoding and IPC framing.
///
/// This replaces the original AudioBridge from wail-plugin with one backed
/// by the NINJAM-style IntervalRing. It lives in wail-audio so both the
/// plugin and the app can use it.
///
/// Responsibilities:
/// 1. Wrap IntervalRing's process() for the audio thread
/// 2. Opus-encode completed intervals into wire-ready bytes
/// 3. Opus-decode incoming wire bytes and feed them to the ring
pub struct AudioBridge {
    ring: IntervalRing,
    encoder: Option<AudioEncoder>,
    decoder: Option<AudioDecoder>,
    sample_rate: u32,
    channels: u16,
    bitrate_kbps: u32,
    bpm: f64,
    bars: u32,
    quantum: f64,
}

impl AudioBridge {
    pub fn new(sample_rate: u32, channels: u16, bars: u32, quantum: f64, bitrate_kbps: u32) -> Self {
        let encoder = match AudioEncoder::new(sample_rate, channels, bitrate_kbps) {
            Ok(enc) => Some(enc),
            Err(e) => {
                tracing::warn!(error = %e, sample_rate, channels, bitrate_kbps, "Failed to create Opus encoder — audio encoding disabled");
                None
            }
        };
        let decoder = match AudioDecoder::new(sample_rate, channels) {
            Ok(dec) => Some(dec),
            Err(e) => {
                tracing::warn!(error = %e, sample_rate, channels, "Failed to create Opus decoder — audio decoding disabled");
                None
            }
        };

        Self {
            ring: IntervalRing::new(sample_rate, channels, bars, quantum),
            encoder,
            decoder,
            sample_rate,
            channels,
            bitrate_kbps,
            bpm: 120.0,
            bars,
            quantum,
        }
    }

    /// Audio-thread safe: drive ring buffer and return raw completed intervals (no Opus).
    ///
    /// Use this from the real-time audio callback. Opus encoding should be done
    /// on a background thread using `AudioEncoder` directly.
    pub fn process_rt(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        beat_position: f64,
    ) -> Vec<CompletedInterval> {
        self.ring.process(input, output, beat_position);
        self.ring.take_completed()
    }

    /// Audio-thread safe: feed already-decoded PCM to ring for playback.
    ///
    /// Use this from the real-time audio callback after decoding Opus on a
    /// background thread.
    pub fn feed_decoded(&mut self, peer_id: String, stream_id: u16, interval_index: i64, samples: Vec<f32>) {
        self.ring.feed_remote(peer_id, stream_id, interval_index, samples);
    }

    /// Process one audio buffer from the DAW. Records input, outputs playback.
    /// Returns wire-encoded bytes for any interval that just completed.
    ///
    /// Note: This encodes Opus on the calling thread. For real-time audio callbacks,
    /// prefer `process_rt()` + encoding on a background thread.
    pub fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        beat_position: f64,
    ) -> Vec<Vec<u8>> {
        // Drive the ring buffer
        self.ring.process(input, output, beat_position);

        // Check for completed intervals and encode them
        let completed = self.ring.take_completed();
        let mut wire_messages = Vec::new();

        for interval in completed {
            if let Some(ref mut encoder) = self.encoder {
                match encoder.encode_interval(&interval.samples) {
                    Ok(opus_data) => {
                        let num_frames =
                            (interval.samples.len() / self.channels as usize) as u32;
                        let audio_interval = AudioInterval {
                            index: interval.index,
                            stream_id: 0,
                            opus_data,
                            sample_rate: self.sample_rate,
                            channels: self.channels,
                            num_frames,
                            bpm: self.bpm,
                            quantum: self.quantum,
                            bars: self.bars,
                        };
                        wire_messages.push(AudioWire::encode(&audio_interval));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to encode completed interval");
                    }
                }
            }
        }

        wire_messages
    }

    /// Receive a wire-encoded audio interval from a remote peer.
    /// Decodes Opus and feeds to the ring for playback.
    pub fn receive_wire(&mut self, peer_id: &str, wire_data: &[u8]) {
        let interval = match AudioWire::decode(wire_data) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to decode wire data");
                return;
            }
        };

        if let Some(ref mut decoder) = self.decoder {
            match decoder.decode_interval(&interval.opus_data) {
                Ok(samples) => {
                    self.ring.feed_remote(peer_id.to_string(), interval.stream_id, interval.index, samples);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to decode Opus audio");
                }
            }
        }
    }

    /// Set the buffer return channel for zero-allocation spare replenishment.
    /// See [`IntervalRing::set_buffer_return_rx`] for details.
    pub fn set_buffer_return_rx(&mut self, rx: crossbeam_channel::Receiver<Vec<f32>>) {
        self.ring.set_buffer_return_rx(rx);
    }

    /// Update tempo/config from DAW transport.
    pub fn update_config(&mut self, bars: u32, quantum: f64, bpm: f64) {
        self.bars = bars;
        self.quantum = quantum;
        self.bpm = bpm;
        self.ring.set_config(bars, quantum);
    }

    /// Reset all state (on transport stop, etc.)
    pub fn reset(&mut self) {
        self.ring.reset();
        self.encoder = AudioEncoder::new(self.sample_rate, self.channels, self.bitrate_kbps)
            .map_err(|e| { tracing::warn!(error = %e, "Failed to recreate Opus encoder on reset"); e })
            .ok();
        self.decoder = AudioDecoder::new(self.sample_rate, self.channels)
            .map_err(|e| { tracing::warn!(error = %e, "Failed to recreate Opus decoder on reset"); e })
            .ok();
    }

    /// Reset interval tracking and buffer positions without clearing peer state.
    ///
    /// Use this on transport restart. Unlike `reset()`, this preserves peer slot
    /// assignments and does not recreate Opus codecs.
    pub fn reset_transport(&mut self) {
        self.ring.reset_transport();
    }

    /// Read per-peer isolated audio from a specific slot.
    /// The slot index corresponds to `peer_playback_slots()` / `peer_info()`.
    pub fn read_peer_playback(&mut self, slot: usize, output: &mut [f32]) -> usize {
        self.ring.read_peer_playback(slot, output)
    }

    /// Return (slot_index, peer_id, stream_id) for all active remote peer-streams.
    pub fn peer_info(&self) -> Vec<(usize, String, u16)> {
        self.ring.active_peer_slots()
    }

    /// Register a peer's persistent identity for slot affinity.
    pub fn notify_peer_joined(&mut self, peer_id: &str, identity: &str) {
        self.ring.notify_peer_joined(peer_id, identity);
    }

    /// Remove a peer and free their slot, reserving it for affinity reconnect.
    pub fn remove_peer(&mut self, peer_id: &str) {
        self.ring.remove_peer(peer_id);
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn bpm(&self) -> f64 {
        self.bpm
    }

    pub fn quantum(&self) -> f64 {
        self.quantum
    }

    pub fn bars(&self) -> u32 {
        self.bars
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }

    /// Return the current interval index (from the ring buffer's beat tracking).
    /// Returns 0 if no interval has started yet.
    pub fn current_interval_index(&self) -> i64 {
        self.ring.current_interval().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::AudioEncoder;
    use crate::interval::AudioInterval;

    fn make_bridge() -> AudioBridge {
        AudioBridge::new(48000, 2, 4, 4.0, 128)
    }

    // --- Construction ---

    #[test]
    fn creates_with_valid_params() {
        let bridge = make_bridge();
        assert_eq!(bridge.sample_rate(), 48000);
        assert_eq!(bridge.channels(), 2);
    }

    // --- Process: recording and encoding ---

    #[test]
    fn process_returns_empty_within_interval() {
        let mut bridge = make_bridge();
        let input = vec![0.5f32; 256];
        let mut output = vec![0.0f32; 256];

        let outgoing = bridge.process(&input, &mut output, 0.0);
        assert!(outgoing.is_empty());

        let outgoing = bridge.process(&input, &mut output, 8.0);
        assert!(outgoing.is_empty());
    }

    #[test]
    fn process_returns_wire_bytes_at_boundary() {
        let mut bridge = make_bridge();
        let input = vec![0.5f32; 256];
        let mut output = vec![0.0f32; 256];

        bridge.process(&input, &mut output, 0.0);
        bridge.process(&input, &mut output, 8.0);

        let outgoing = bridge.process(&input, &mut output, 16.0);
        assert_eq!(outgoing.len(), 1, "Should produce one encoded interval");

        let decoded = AudioWire::decode(&outgoing[0]).unwrap();
        assert_eq!(decoded.index, 0);
        assert_eq!(decoded.sample_rate, 48000);
        assert_eq!(decoded.channels, 2);
        assert!(!decoded.opus_data.is_empty(), "Opus data should not be empty");
    }

    #[test]
    fn wire_output_carries_correct_metadata() {
        let mut bridge = AudioBridge::new(48000, 2, 2, 3.0, 96);
        bridge.update_config(2, 3.0, 145.0);
        let input = vec![0.1f32; 128];
        let mut output = vec![0.0f32; 128];

        bridge.process(&input, &mut output, 0.0);
        let outgoing = bridge.process(&input, &mut output, 6.0);

        let decoded = AudioWire::decode(&outgoing[0]).unwrap();
        assert_eq!(decoded.bars, 2);
        assert!((decoded.quantum - 3.0).abs() < f64::EPSILON);
        assert!((decoded.bpm - 145.0).abs() < f64::EPSILON);
    }

    // --- Process: playback ---

    #[test]
    fn process_outputs_silence_with_no_remote() {
        let mut bridge = make_bridge();
        let input = vec![0.0f32; 128];
        let mut output = vec![1.0f32; 128];

        bridge.process(&input, &mut output, 0.0);
        assert!(output.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn receive_wire_then_playback() {
        let mut bridge = make_bridge();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        bridge.process(&input, &mut output, 0.0);

        // Simulate receiving a remote interval
        let mut encoder = AudioEncoder::new(48000, 2, 128).unwrap();
        let remote_samples = vec![0.3f32; 1920];
        let opus_data = encoder.encode_interval(&remote_samples).unwrap();
        let wire = AudioWire::encode(&AudioInterval {
            index: 0,
            stream_id: 0,
            opus_data,
            sample_rate: 48000,
            channels: 2,
            num_frames: 960,
            bpm: 120.0,
            quantum: 4.0,
            bars: 4,
        });

        bridge.receive_wire("peer-a", &wire);

        // Cross boundary
        bridge.process(&input, &mut output, 16.0);

        let energy: f32 = output.iter().map(|s| s.abs()).sum();
        assert!(energy > 0.0, "Playback should contain decoded remote audio");
    }

    // --- Config updates ---

    #[test]
    fn update_config_changes_bpm_in_output() {
        let mut bridge = make_bridge();
        bridge.update_config(4, 4.0, 180.0);

        let input = vec![0.1f32; 64];
        let mut output = vec![0.0f32; 64];
        bridge.process(&input, &mut output, 0.0);
        let outgoing = bridge.process(&input, &mut output, 16.0);

        let decoded = AudioWire::decode(&outgoing[0]).unwrap();
        assert!((decoded.bpm - 180.0).abs() < f64::EPSILON);
    }

    // --- Reset ---

    #[test]
    fn reset_clears_state() {
        let mut bridge = make_bridge();
        let input = vec![0.5f32; 128];
        let mut output = vec![0.0f32; 128];

        bridge.process(&input, &mut output, 0.0);
        bridge.reset();

        let outgoing = bridge.process(&input, &mut output, 0.0);
        assert!(outgoing.is_empty());
    }
}
