/// Maximum number of remote peers with independent audio channels.
pub const MAX_REMOTE_PEERS: usize = 7;

/// Per-peer isolated playback slot.
pub struct PeerSlot {
    pub peer_id: String,
    pub samples: Vec<f32>,
    pub active: bool,
    read_pos: usize,
}

impl PeerSlot {
    fn new() -> Self {
        Self {
            peer_id: String::new(),
            samples: Vec::new(),
            active: false,
            read_pos: 0,
        }
    }

    fn clear(&mut self) {
        self.samples.clear();
        self.active = false;
        self.read_pos = 0;
    }
}

/// NINJAM-style interval ring buffer for simultaneous record and playback.
///
/// The core concept from NINJAM:
/// - Two slots: one is being recorded into, the other is being played back
/// - At each interval boundary, slots swap: the just-recorded interval becomes
///   available for transmission, and received remote audio starts playing back
/// - Multiple remote peers' audio is mixed together in the playback slot
///
/// This implementation is designed for use on an audio thread:
/// - `process()` is the main method called per audio buffer
/// - It writes input to the record slot and reads from the playback slot
/// - At interval boundaries (driven by beat position), it swaps slots
///
/// In addition to the summed playback slot, per-peer isolated audio is kept
/// in up to [`MAX_REMOTE_PEERS`] slots for independent DAW routing.
///
/// # Interval Timing
///
/// Intervals are defined by: `bars * quantum` beats.
/// Beat position comes from the DAW transport (Ableton Link beat grid).
/// Example: 4 bars of 4/4 = 16 beats per interval.
pub struct IntervalRing {
    /// The slot currently being recorded into (local audio capture).
    /// Pre-allocated to `slot_capacity` — never grows on the audio thread.
    record_slot: Vec<f32>,
    /// The slot currently being played back (mixed remote audio).
    /// Pre-allocated to `slot_capacity`.
    playback_slot: Vec<f32>,
    /// Actual number of playback samples in `playback_slot` (may be less than capacity)
    playback_len: usize,
    /// Write position in the record slot (in interleaved samples)
    record_pos: usize,
    /// Read position in the playback slot (in interleaved samples)
    playback_pos: usize,
    /// Pre-allocated capacity for record/playback slots
    #[allow(dead_code)]
    slot_capacity: usize,
    /// Current interval index
    current_interval: Option<i64>,
    /// Completed intervals ready for encoding and transmission
    completed: Vec<CompletedInterval>,
    /// Remote peer intervals waiting to be mixed into next playback slot
    pending_remote: Vec<RemoteInterval>,
    /// Audio parameters (retained for future use in resampling/diagnostics)
    #[allow(dead_code)]
    sample_rate: u32,
    #[allow(dead_code)]
    channels: u16,
    /// Interval parameters
    bars: u32,
    quantum: f64,
    /// Per-peer isolated playback slots (up to MAX_REMOTE_PEERS)
    peer_slots: Vec<PeerSlot>,
    /// Maps peer_id → index into peer_slots for stable assignment
    peer_slot_map: Vec<(String, usize)>,
}

/// A completed local recording ready for encoding.
pub struct CompletedInterval {
    pub index: i64,
    pub samples: Vec<f32>,
}

/// A received remote interval to mix into playback.
#[allow(dead_code)] // index/peer_id retained for future per-peer mixing control
struct RemoteInterval {
    index: i64,
    peer_id: String,
    pub samples: Vec<f32>,
}

impl IntervalRing {
    /// Create a new interval ring buffer.
    ///
    /// All buffers are pre-allocated so that `process()` never allocates on the
    /// audio thread (except for one `to_vec()` at each interval boundary, which
    /// should be wrapped in `permit_alloc` by the caller).
    pub fn new(sample_rate: u32, channels: u16, bars: u32, quantum: f64) -> Self {
        let bars = bars.max(1);
        let quantum = quantum.max(f64::EPSILON);
        let beats_per_interval = bars as f64 * quantum;
        // Pre-allocate for slowest expected tempo (20 BPM) so we never grow
        // on the audio thread. At 20 BPM, 16 beats = 48 seconds.
        let max_seconds = beats_per_interval / 20.0_f64.max(1.0);
        let slot_capacity = (sample_rate as f64 * max_seconds * channels as f64) as usize;

        let mut peer_slots = Vec::with_capacity(MAX_REMOTE_PEERS);
        for _ in 0..MAX_REMOTE_PEERS {
            peer_slots.push(PeerSlot::new());
        }

        Self {
            record_slot: Vec::with_capacity(slot_capacity),
            playback_slot: vec![0.0f32; slot_capacity],
            playback_len: 0,
            record_pos: 0,
            playback_pos: 0,
            slot_capacity,
            current_interval: None,
            completed: Vec::with_capacity(2),
            pending_remote: Vec::with_capacity(MAX_REMOTE_PEERS),
            sample_rate,
            channels,
            bars,
            quantum,
            peer_slots,
            peer_slot_map: Vec::with_capacity(MAX_REMOTE_PEERS),
        }
    }

    /// Process one audio buffer: record input and produce output.
    ///
    /// Called once per audio callback from the DAW/plugin.
    ///
    /// - `input`: interleaved f32 samples from DAW (captured audio)
    /// - `output`: interleaved f32 buffer to fill with playback audio
    /// - `beat_position`: current beat position from DAW transport / Link
    ///
    /// Returns `Some(interval_index)` if an interval boundary was crossed.
    pub fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        beat_position: f64,
    ) -> Option<i64> {
        let interval_index = self.beat_to_interval(beat_position);
        let mut boundary_crossed = None;

        // Check for interval boundary
        match self.current_interval {
            Some(prev) if prev != interval_index => {
                boundary_crossed = Some(prev);
                self.swap_intervals(prev);
            }
            None => {
                // First process call — start recording
            }
            _ => {}
        }
        self.current_interval = Some(interval_index);

        // Record: write input into pre-allocated record slot (no allocation)
        let remaining_capacity = self.record_slot.capacity() - self.record_pos;
        let to_write = input.len().min(remaining_capacity);
        if to_write > 0 {
            // Grow length within existing capacity — no heap allocation
            let new_len = self.record_pos + to_write;
            if self.record_slot.len() < new_len {
                self.record_slot.resize(new_len, 0.0);
            }
            self.record_slot[self.record_pos..new_len].copy_from_slice(&input[..to_write]);
            self.record_pos = new_len;
        }

        // Playback: read from playback slot
        let available = self.playback_len.saturating_sub(self.playback_pos);
        let to_read = available.min(output.len());

        if to_read > 0 {
            output[..to_read]
                .copy_from_slice(&self.playback_slot[self.playback_pos..self.playback_pos + to_read]);
            self.playback_pos += to_read;
        }

        // Fill remainder with silence
        for sample in &mut output[to_read..] {
            *sample = 0.0;
        }

        boundary_crossed
    }

    /// Feed a remote peer's decoded interval audio for playback.
    ///
    /// This will be mixed into the playback slot at the next interval boundary.
    /// Multiple peers' audio is summed together.
    pub fn feed_remote(&mut self, peer_id: &str, interval_index: i64, samples: Vec<f32>) {
        self.pending_remote.push(RemoteInterval {
            index: interval_index,
            peer_id: peer_id.to_string(),
            samples,
        });
    }

    /// Take completed intervals that are ready for encoding and transmission.
    pub fn take_completed(&mut self) -> Vec<CompletedInterval> {
        std::mem::take(&mut self.completed)
    }

    /// Update interval configuration (bars, quantum).
    pub fn set_config(&mut self, bars: u32, quantum: f64) {
        self.bars = bars.max(1);
        self.quantum = quantum.max(f64::EPSILON);
    }

    /// Reset all state (preserves pre-allocated capacity).
    pub fn reset(&mut self) {
        self.record_slot.clear();
        self.record_pos = 0;
        self.playback_pos = 0;
        self.playback_len = 0;
        self.current_interval = None;
        self.completed.clear();
        self.pending_remote.clear();
        for slot in &mut self.peer_slots {
            slot.clear();
        }
        self.peer_slot_map.clear();
    }

    /// Current interval index, if any.
    pub fn current_interval(&self) -> Option<i64> {
        self.current_interval
    }

    /// Number of samples currently recorded in the record slot.
    pub fn record_position(&self) -> usize {
        self.record_pos
    }

    /// Number of samples remaining in the playback slot.
    pub fn playback_remaining(&self) -> usize {
        self.playback_len.saturating_sub(self.playback_pos)
    }

    /// Number of remote intervals pending for next playback.
    pub fn pending_remote_count(&self) -> usize {
        self.pending_remote.len()
    }

    /// Convert a beat position to an interval index.
    fn beat_to_interval(&self, beat: f64) -> i64 {
        let beats_per_interval = self.bars as f64 * self.quantum;
        (beat / beats_per_interval).floor() as i64
    }

    /// Get the per-peer playback slots (up to MAX_REMOTE_PEERS).
    pub fn peer_playback_slots(&self) -> &[PeerSlot] {
        &self.peer_slots
    }

    /// Read audio from a specific peer slot into the output buffer.
    /// Advances that slot's read position. Returns the number of samples written.
    pub fn read_peer_playback(&mut self, slot: usize, output: &mut [f32]) -> usize {
        if slot >= self.peer_slots.len() || !self.peer_slots[slot].active {
            for s in output.iter_mut() {
                *s = 0.0;
            }
            return 0;
        }

        let peer = &mut self.peer_slots[slot];
        let available = peer.samples.len().saturating_sub(peer.read_pos);
        let to_read = available.min(output.len());

        if to_read > 0 {
            output[..to_read]
                .copy_from_slice(&peer.samples[peer.read_pos..peer.read_pos + to_read]);
            peer.read_pos += to_read;
        }

        for s in &mut output[to_read..] {
            *s = 0.0;
        }

        to_read
    }

    /// Return (slot_index, peer_id) for all active peer slots.
    pub fn active_peer_slots(&self) -> Vec<(usize, String)> {
        self.peer_slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.active)
            .map(|(i, s)| (i, s.peer_id.clone()))
            .collect()
    }

    /// Look up or assign a slot index for a peer_id. Returns None if all slots are full.
    fn assign_peer_slot(&mut self, peer_id: &str) -> Option<usize> {
        // Check existing assignment
        for &(ref pid, idx) in &self.peer_slot_map {
            if pid == peer_id {
                return Some(idx);
            }
        }
        // Find first inactive slot
        for (i, slot) in self.peer_slots.iter_mut().enumerate() {
            if !slot.active {
                slot.peer_id = peer_id.to_string();
                slot.active = true;
                self.peer_slot_map.push((peer_id.to_string(), i));
                return Some(i);
            }
        }
        None // all slots full
    }

    /// Swap intervals: move record → completed, mix pending remote → playback.
    ///
    /// NOTE: The `to_vec()` call below is the ONE allocation in the audio-thread
    /// path. It copies recorded samples so they can be owned by `CompletedInterval`
    /// and sent to the IPC thread. The caller should wrap this in `permit_alloc`.
    fn swap_intervals(&mut self, completed_index: i64) {
        // Copy recorded audio to completed queue, then clear record slot
        // (clear preserves the pre-allocated capacity — no future alloc)
        if self.record_pos > 0 {
            let samples = self.record_slot[..self.record_pos].to_vec();
            self.completed.push(CompletedInterval {
                index: completed_index,
                samples,
            });
        }
        self.record_slot.clear();
        self.record_pos = 0;

        // Clear per-peer slots (but keep assignments)
        for slot in &mut self.peer_slots {
            slot.samples.clear();
            slot.read_pos = 0;
        }

        // Mix pending remote intervals into pre-allocated playback slot
        self.playback_pos = 0;
        self.playback_len = 0;

        let pending = std::mem::take(&mut self.pending_remote);
        for remote in pending {
            let mix_len = self.playback_len.max(remote.samples.len());
            // Grow playback within pre-allocated capacity
            let mix_len = mix_len.min(self.playback_slot.len());

            // Zero-fill the extension range
            for s in &mut self.playback_slot[self.playback_len..mix_len] {
                *s = 0.0;
            }
            self.playback_len = mix_len;

            // Sum remote audio into playback
            let copy_len = remote.samples.len().min(self.playback_slot.len());
            for (i, sample) in remote.samples[..copy_len].iter().enumerate() {
                self.playback_slot[i] += sample;
            }

            // Move samples to per-peer slot
            if let Some(slot_idx) = self.assign_peer_slot(&remote.peer_id) {
                self.peer_slots[slot_idx].samples = remote.samples;
                self.peer_slots[slot_idx].read_pos = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48000;
    const CH: u16 = 2;
    const BARS: u32 = 4;
    const QUANTUM: f64 = 4.0;
    // 4 bars * 4 beats = 16 beats per interval

    fn make_ring() -> IntervalRing {
        IntervalRing::new(SR, CH, BARS, QUANTUM)
    }

    // --- Test: Basic record and playback ---

    #[test]
    fn new_ring_starts_empty() {
        let ring = make_ring();
        assert_eq!(ring.current_interval(), None);
        assert_eq!(ring.record_position(), 0);
        assert_eq!(ring.playback_remaining(), 0);
        assert_eq!(ring.pending_remote_count(), 0);
    }

    #[test]
    fn process_records_input() {
        let mut ring = make_ring();
        let input = vec![0.5f32; 256];
        let mut output = vec![0.0f32; 256];

        // Beat 0.0 = interval 0
        let boundary = ring.process(&input, &mut output, 0.0);
        assert!(boundary.is_none()); // first call, no boundary
        assert_eq!(ring.current_interval(), Some(0));
        assert_eq!(ring.record_position(), 256);
    }

    #[test]
    fn process_outputs_silence_when_no_remote_audio() {
        let mut ring = make_ring();
        let input = vec![0.5f32; 256];
        let mut output = vec![1.0f32; 256]; // pre-fill with non-zero

        ring.process(&input, &mut output, 0.0);
        assert!(output.iter().all(|&s| s == 0.0), "Expected silence");
    }

    // --- Test: Interval boundary detection ---

    #[test]
    fn detects_interval_boundary() {
        let mut ring = make_ring();
        let input = vec![0.1f32; 128];
        let mut output = vec![0.0f32; 128];

        // Record in interval 0 (beat 0 to beat 15.9)
        ring.process(&input, &mut output, 0.0);
        ring.process(&input, &mut output, 8.0);
        ring.process(&input, &mut output, 15.0);

        // Cross into interval 1 (beat 16.0)
        let boundary = ring.process(&input, &mut output, 16.0);
        assert_eq!(boundary, Some(0), "Should detect boundary of interval 0");
        assert_eq!(ring.current_interval(), Some(1));
    }

    #[test]
    fn completed_interval_available_after_boundary() {
        let mut ring = make_ring();
        let input = vec![0.3f32; 128];
        let mut output = vec![0.0f32; 128];

        // Record in interval 0
        ring.process(&input, &mut output, 0.0);
        ring.process(&input, &mut output, 8.0);

        // No completed intervals yet
        assert!(ring.take_completed().is_empty());

        // Cross boundary
        ring.process(&input, &mut output, 16.0);

        // Now interval 0 should be completed
        let completed = ring.take_completed();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].index, 0);
        assert_eq!(completed[0].samples.len(), 256); // 2 calls * 128 samples
    }

    // --- Test: Remote audio playback ---

    #[test]
    fn plays_remote_audio_after_boundary() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        // Start in interval 0
        ring.process(&input, &mut output, 0.0);

        // Feed remote audio for next playback
        let remote_audio = vec![0.7f32; 128];
        ring.feed_remote("peer-a", 0, remote_audio);

        // Cross into interval 1 — remote audio should become playback
        ring.process(&input, &mut output, 16.0);

        // Output should now contain the remote audio
        assert!(output.iter().all(|&s| (s - 0.7).abs() < f32::EPSILON),
            "Output should be remote audio, got: {:?}", &output[..8]);
    }

    #[test]
    fn mixes_multiple_remote_peers() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        // Start in interval 0
        ring.process(&input, &mut output, 0.0);

        // Feed from two peers
        ring.feed_remote("peer-a", 0, vec![0.3f32; 128]);
        ring.feed_remote("peer-b", 0, vec![0.5f32; 128]);

        // Cross boundary — both should be mixed (summed)
        ring.process(&input, &mut output, 16.0);

        assert!(output.iter().all(|&s| (s - 0.8).abs() < 0.001),
            "Expected 0.3 + 0.5 = 0.8, got: {:?}", &output[..8]);
    }

    #[test]
    fn remote_audio_longer_than_buffer_spans_calls() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 64];
        let mut output = vec![0.0f32; 64];

        // Start interval 0
        ring.process(&input, &mut output, 0.0);

        // Feed 256 samples of remote audio
        ring.feed_remote("peer-a", 0, vec![0.4f32; 256]);

        // Cross into interval 1
        ring.process(&input, &mut output, 16.0);
        assert!(output.iter().all(|&s| (s - 0.4).abs() < f32::EPSILON));
        assert_eq!(ring.playback_remaining(), 192); // 256 - 64

        // Second call still reads from the same playback slot
        ring.process(&input, &mut output, 16.5);
        assert!(output.iter().all(|&s| (s - 0.4).abs() < f32::EPSILON));
        assert_eq!(ring.playback_remaining(), 128); // 256 - 128
    }

    #[test]
    fn silence_after_playback_exhausted() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 64];
        let mut output = vec![0.0f32; 64];

        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a", 0, vec![0.5f32; 32]); // only 32 samples

        // Cross boundary
        ring.process(&input, &mut output, 16.0);

        // First 32 samples = remote audio, rest = silence
        assert!((output[0] - 0.5).abs() < f32::EPSILON);
        assert!((output[31] - 0.5).abs() < f32::EPSILON);
        assert_eq!(output[32], 0.0);
        assert_eq!(output[63], 0.0);
    }

    // --- Test: Multiple intervals ---

    #[test]
    fn multiple_interval_cycle() {
        let mut ring = make_ring();
        let ones = vec![1.0f32; 100];
        let twos = vec![2.0f32; 100];
        let mut output = vec![0.0f32; 100];

        // Interval 0: record ones
        ring.process(&ones, &mut output, 0.0);
        ring.process(&ones, &mut output, 8.0);

        // Feed remote for playback in interval 1
        ring.feed_remote("peer-a", 0, vec![0.9f32; 100]);

        // Interval 1: record twos, play remote
        ring.process(&twos, &mut output, 16.0);
        let completed = ring.take_completed();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].index, 0);
        assert!((output[0] - 0.9).abs() < f32::EPSILON);

        // Feed new remote for interval 2
        ring.feed_remote("peer-a", 1, vec![0.6f32; 100]);

        // Interval 2: record ones, play new remote
        ring.process(&ones, &mut output, 32.0);
        let completed = ring.take_completed();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].index, 1);
        // Completed interval 1 should contain twos
        assert!((completed[0].samples[0] - 2.0).abs() < f32::EPSILON);
        // Playback should be the new remote
        assert!((output[0] - 0.6).abs() < f32::EPSILON);
    }

    // --- Test: Configuration ---

    #[test]
    fn config_change_affects_interval_index() {
        let mut ring = make_ring(); // 4 bars * 4 quantum = 16 beats
        let input = vec![0.0f32; 64];
        let mut output = vec![0.0f32; 64];

        ring.process(&input, &mut output, 0.0);
        assert_eq!(ring.current_interval(), Some(0));

        // Beat 10 is still interval 0 (< 16)
        ring.process(&input, &mut output, 10.0);
        assert_eq!(ring.current_interval(), Some(0));

        // Change to 2 bars * 4 quantum = 8 beats per interval
        ring.set_config(2, 4.0);

        // Beat 10 is now interval 1 (10/8 = 1.25, floor = 1)
        let boundary = ring.process(&input, &mut output, 10.0);
        assert_eq!(boundary, Some(0)); // crossed from 0 to 1
        assert_eq!(ring.current_interval(), Some(1));
    }

    #[test]
    fn reset_clears_all_state() {
        let mut ring = make_ring();
        let input = vec![0.5f32; 128];
        let mut output = vec![0.0f32; 128];

        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a", 0, vec![0.3f32; 64]);

        ring.reset();

        assert_eq!(ring.current_interval(), None);
        assert_eq!(ring.record_position(), 0);
        assert_eq!(ring.playback_remaining(), 0);
        assert_eq!(ring.pending_remote_count(), 0);
        assert!(ring.take_completed().is_empty());
    }

    // --- Test: Beat position edge cases ---

    #[test]
    fn negative_beat_position() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 64];
        let mut output = vec![0.0f32; 64];

        // Negative beats (pre-roll)
        ring.process(&input, &mut output, -4.0);
        assert_eq!(ring.current_interval(), Some(-1));
    }

    #[test]
    fn fractional_beat_position() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 64];
        let mut output = vec![0.0f32; 64];

        // Beat 15.999 is still interval 0
        ring.process(&input, &mut output, 15.999);
        assert_eq!(ring.current_interval(), Some(0));

        // Beat 16.001 is interval 1
        let boundary = ring.process(&input, &mut output, 16.001);
        assert_eq!(boundary, Some(0));
        assert_eq!(ring.current_interval(), Some(1));
    }

    #[test]
    fn zero_bars_clamped_to_one() {
        let mut ring = IntervalRing::new(SR, CH, 0, QUANTUM);
        let input = vec![0.0f32; 64];
        let mut output = vec![0.0f32; 64];
        // Must not panic — bars=0 is clamped to 1, so interval = 1*4 = 4 beats
        ring.process(&input, &mut output, 0.0);
        assert_eq!(ring.current_interval(), Some(0));
    }

    #[test]
    fn zero_quantum_clamped() {
        let mut ring = IntervalRing::new(SR, CH, BARS, 0.0);
        let input = vec![0.0f32; 64];
        let mut output = vec![0.0f32; 64];
        // Must not panic or produce NaN interval index
        ring.process(&input, &mut output, 10.0);
        assert!(ring.current_interval().is_some());
    }

    #[test]
    fn set_config_clamps_zero_values() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 64];
        let mut output = vec![0.0f32; 64];

        ring.process(&input, &mut output, 0.0);
        ring.set_config(0, 0.0);
        // Must not panic
        ring.process(&input, &mut output, 10.0);
        assert!(ring.current_interval().is_some());
    }

    // --- Test: Per-peer playback slots ---

    #[test]
    fn per_peer_playback_slots() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        ring.process(&input, &mut output, 0.0);

        // Feed from two distinct peers
        ring.feed_remote("peer-a", 0, vec![0.3f32; 128]);
        ring.feed_remote("peer-b", 0, vec![0.7f32; 128]);

        // Cross boundary to activate playback
        ring.process(&input, &mut output, 16.0);

        // Read per-peer slots independently
        let mut slot_a_out = vec![0.0f32; 128];
        let mut slot_b_out = vec![0.0f32; 128];

        let active = ring.active_peer_slots();
        assert_eq!(active.len(), 2);

        // Find which slot is which
        let (a_idx, _) = active.iter().find(|(_, pid)| pid == "peer-a").unwrap();
        let (b_idx, _) = active.iter().find(|(_, pid)| pid == "peer-b").unwrap();

        ring.read_peer_playback(*a_idx, &mut slot_a_out);
        ring.read_peer_playback(*b_idx, &mut slot_b_out);

        // Peer A's slot should have 0.3
        assert!(
            slot_a_out.iter().all(|&s| (s - 0.3).abs() < f32::EPSILON),
            "Peer A slot should be 0.3, got: {:?}", &slot_a_out[..4]
        );
        // Peer B's slot should have 0.7
        assert!(
            slot_b_out.iter().all(|&s| (s - 0.7).abs() < f32::EPSILON),
            "Peer B slot should be 0.7, got: {:?}", &slot_b_out[..4]
        );
    }

    #[test]
    fn per_peer_and_summed_mix_consistent() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 64];
        let mut output = vec![0.0f32; 64];

        ring.process(&input, &mut output, 0.0);

        ring.feed_remote("peer-x", 0, vec![0.2f32; 64]);
        ring.feed_remote("peer-y", 0, vec![0.5f32; 64]);

        // Cross boundary
        ring.process(&input, &mut output, 16.0);

        // Summed mix should be 0.2 + 0.5 = 0.7
        assert!(
            output.iter().all(|&s| (s - 0.7).abs() < 0.001),
            "Summed mix should be 0.7, got: {:?}", &output[..4]
        );

        // Per-peer should sum to the same thing
        let active = ring.active_peer_slots();
        let mut sum = vec![0.0f32; 64];
        for (idx, _) in &active {
            let mut buf = vec![0.0f32; 64];
            ring.read_peer_playback(*idx, &mut buf);
            for (i, s) in buf.iter().enumerate() {
                sum[i] += s;
            }
        }

        for (i, &s) in sum.iter().enumerate() {
            assert!(
                (s - 0.7).abs() < 0.001,
                "Sum of per-peer slots at {i} = {s}, expected 0.7"
            );
        }
    }

    #[test]
    fn inactive_peer_slot_returns_silence() {
        let mut ring = make_ring();
        let mut output = vec![1.0f32; 64];

        // Slot 6 has never been assigned
        ring.read_peer_playback(6, &mut output);
        assert!(output.iter().all(|&s| s == 0.0));
    }
}
