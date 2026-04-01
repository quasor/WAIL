use crate::codec::{AudioDecoder, AudioEncoder};
use crate::ring::{CompletedInterval, IntervalRing};

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

    #[test]
    fn creates_with_valid_params() {
        let bridge = AudioBridge::new(48000, 2, 4, 4.0, 128);
        assert_eq!(bridge.sample_rate(), 48000);
        assert_eq!(bridge.channels(), 2);
    }
}
