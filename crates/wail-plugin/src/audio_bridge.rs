use wail_audio::{AudioDecoder, AudioEncoder, AudioInterval, AudioWire, IntervalPlayer, IntervalRecorder};

/// Bridge between the audio thread and the background encoding/IPC layer.
///
/// The audio thread calls `capture_audio()` and `read_playback()`.
/// Completed intervals are queued for encoding and transmission.
/// Received intervals are decoded and queued for playback.
pub struct AudioBridge {
    recorder: IntervalRecorder,
    player: IntervalPlayer,
    encoder: Option<AudioEncoder>,
    decoder: Option<AudioDecoder>,
    /// Completed intervals waiting to be sent (encoded wire format)
    outgoing: Vec<Vec<u8>>,
    /// Current interval tracking
    current_interval_index: Option<i64>,
    bars: u32,
    quantum: f64,
    bpm: f64,
    sample_rate: u32,
    channels: u16,
    bitrate_kbps: u32,
}

impl AudioBridge {
    pub fn new(sample_rate: u32, channels: u16, bars: u32, quantum: f64, bitrate_kbps: u32) -> Self {
        let encoder = match AudioEncoder::new(sample_rate, channels, bitrate_kbps) {
            Ok(enc) => Some(enc),
            Err(e) => {
                tracing::warn!(error = %e, sample_rate, channels, "Failed to create Opus encoder — audio encoding disabled");
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
            recorder: IntervalRecorder::new(sample_rate, channels),
            player: IntervalPlayer::new(sample_rate, channels, 10),
            encoder,
            decoder,
            outgoing: Vec::new(),
            current_interval_index: None,
            bars,
            quantum,
            bpm: 120.0,
            sample_rate,
            channels,
            bitrate_kbps,
        }
    }

    /// Update interval configuration from plugin parameters.
    pub fn update_config(&mut self, bars: u32, quantum: f64, bpm: f64) {
        self.bars = bars;
        self.quantum = quantum;
        self.bpm = bpm;
    }

    /// Capture interleaved audio samples from the DAW.
    /// Tracks interval boundaries and triggers encoding at boundaries.
    pub fn capture_audio(&mut self, interleaved_samples: &[f32], beat_position: f64) {
        let beats_per_interval = self.bars as f64 * self.quantum;
        let interval_index = (beat_position / beats_per_interval).floor() as i64;

        // Check for interval boundary
        if let Some(prev_idx) = self.current_interval_index {
            if interval_index != prev_idx {
                // Interval boundary crossed — finish and encode the previous interval
                self.finish_current_interval();
            }
        }
        self.current_interval_index = Some(interval_index);

        // Push samples into the recorder
        self.recorder.push_samples(interleaved_samples, interval_index);
    }

    /// Read interleaved playback samples (remote peer audio).
    pub fn read_playback(&mut self, output: &mut [f32]) {
        self.player.read_samples(output);
    }

    /// Take all outgoing wire-encoded audio intervals (for IPC/network send).
    pub fn take_outgoing(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outgoing)
    }

    /// Feed a received wire-encoded audio interval for playback.
    pub fn receive_audio(&mut self, wire_data: &[u8]) {
        let interval = match AudioWire::decode(wire_data) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to decode incoming audio interval");
                return;
            }
        };

        // Decode Opus to PCM
        if let Some(ref mut decoder) = self.decoder {
            match decoder.decode_interval(&interval.opus_data) {
                Ok(samples) => {
                    self.player.enqueue(interval.index, samples);
                    tracing::debug!(
                        interval = interval.index,
                        frames = interval.num_frames,
                        "Enqueued remote audio interval for playback"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to decode Opus audio");
                }
            }
        }
    }

    /// Reset playback and recording state.
    pub fn reset(&mut self) {
        self.player.clear();
        self.current_interval_index = None;
        // Re-create encoder/decoder in case params changed
        self.encoder = AudioEncoder::new(self.sample_rate, self.channels, self.bitrate_kbps)
            .map_err(|e| { tracing::warn!(error = %e, "Failed to recreate Opus encoder on reset"); e })
            .ok();
        self.decoder = AudioDecoder::new(self.sample_rate, self.channels)
            .map_err(|e| { tracing::warn!(error = %e, "Failed to recreate Opus decoder on reset"); e })
            .ok();
    }

    /// Finish the current interval, encode with Opus, and queue for transmission.
    fn finish_current_interval(&mut self) {
        if let Some((index, samples)) = self.recorder.finish_interval() {
            if let Some(ref mut encoder) = self.encoder {
                match encoder.encode_interval(&samples) {
                    Ok(opus_data) => {
                        let num_frames = (samples.len() / self.channels as usize) as u32;
                        let interval = AudioInterval {
                            index,
                            opus_data,
                            sample_rate: self.sample_rate,
                            channels: self.channels,
                            num_frames,
                            bpm: self.bpm,
                            quantum: self.quantum,
                            bars: self.bars,
                        };
                        let wire = AudioWire::encode(&interval);
                        tracing::info!(
                            interval = index,
                            frames = num_frames,
                            wire_bytes = wire.len(),
                            "Encoded audio interval"
                        );
                        self.outgoing.push(wire);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to encode audio interval");
                    }
                }
            }
        }
    }
}
