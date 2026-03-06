/// A complete audio interval ready for transmission or playback.
#[derive(Debug, Clone)]
pub struct AudioInterval {
    /// Interval index (monotonically increasing per the Link beat grid)
    pub index: i64,
    /// Stream index within a peer (0 = default, supports multi-stream send)
    pub stream_id: u16,
    /// Opus-encoded audio data (length-prefixed frames)
    pub opus_data: Vec<u8>,
    /// Sample rate of the source audio
    pub sample_rate: u32,
    /// Number of channels (1=mono, 2=stereo)
    pub channels: u16,
    /// Total number of source samples per channel before encoding
    pub num_frames: u32,
    /// BPM at the time of recording
    pub bpm: f64,
    /// Quantum (beats per bar) at the time of recording
    pub quantum: f64,
    /// Bars per interval
    pub bars: u32,
}

/// Records audio samples into intervals, triggering encoding at interval boundaries.
///
/// Usage in an audio processing callback:
/// 1. Call `push_samples()` with each audio buffer from the DAW
/// 2. Call `finish_interval()` at interval boundaries to get the encoded interval
pub struct IntervalRecorder {
    /// Accumulated interleaved f32 samples for the current interval
    buffer: Vec<f32>,
    /// Current interval index being recorded
    current_index: Option<i64>,
    /// Audio parameters
    sample_rate: u32,
    channels: u16,
}

impl IntervalRecorder {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        // Pre-allocate for ~8 bars at 120bpm, 4/4 time = 16 beats = 8 seconds
        let estimated_capacity = sample_rate as usize * channels as usize * 8;
        Self {
            buffer: Vec::with_capacity(estimated_capacity),
            current_index: None,
            sample_rate,
            channels,
        }
    }

    /// Push interleaved f32 samples into the current interval buffer.
    pub fn push_samples(&mut self, samples: &[f32], interval_index: i64) {
        // If the interval changed, discard the buffer (caller should have called finish_interval)
        if self.current_index.is_some() && self.current_index != Some(interval_index) {
            tracing::warn!(
                old = ?self.current_index,
                new = interval_index,
                "Interval changed without finish_interval — discarding buffer"
            );
            self.buffer.clear();
        }
        self.current_index = Some(interval_index);
        self.buffer.extend_from_slice(samples);
    }

    /// Finish the current interval and return the raw samples for encoding.
    /// Resets the buffer for the next interval.
    pub fn finish_interval(&mut self) -> Option<(i64, Vec<f32>)> {
        let index = self.current_index.take()?;
        if self.buffer.is_empty() {
            return None;
        }
        let samples = std::mem::take(&mut self.buffer);
        Some((index, samples))
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn is_recording(&self) -> bool {
        self.current_index.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorder_basic_flow() {
        let mut rec = IntervalRecorder::new(48000, 2);

        // Push some samples for interval 0
        rec.push_samples(&[0.1, 0.2, 0.3, 0.4], 0);
        rec.push_samples(&[0.5, 0.6], 0);
        assert!(rec.is_recording());

        // Finish interval
        let (idx, samples) = rec.finish_interval().unwrap();
        assert_eq!(idx, 0);
        assert_eq!(samples.len(), 6);
        assert!(!rec.is_recording());
    }

}
