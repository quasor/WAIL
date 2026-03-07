use crate::slot::{ClientChannelMapping, SlotTable, MAX_SLOTS};

/// Maximum number of remote peer-stream slots with independent audio channels.
pub const MAX_REMOTE_PEERS: usize = MAX_SLOTS;

/// Per-peer-stream isolated playback slot.
pub struct PeerSlot {
    pub peer_id: String,
    pub stream_id: u16,
    pub samples: Vec<f32>,
    pub active: bool,
    read_pos: usize,
    /// When true, the next interval from this peer will be faded in
    /// from silence to prevent pops/clicks on join or reconnect.
    needs_fade_in: bool,
}

impl PeerSlot {
    fn new() -> Self {
        Self {
            peer_id: String::new(),
            stream_id: 0,
            samples: Vec::new(),
            active: false,
            read_pos: 0,
            needs_fade_in: true,
        }
    }

    fn clear(&mut self) {
        self.samples.clear();
        self.active = false;
        self.read_pos = 0;
        self.needs_fade_in = true;
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
/// In addition to the summed playback slot, per-peer-stream isolated audio is
/// kept in up to [`MAX_REMOTE_PEERS`] slots for independent DAW routing.
/// Each unique `(peer_id, stream_id)` pair gets its own slot.
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
    slot_capacity: usize,
    /// Spare pre-allocated record buffer — swapped in at interval boundaries
    /// so we can move the filled record_slot to CompletedInterval without copying.
    spare_record: Vec<f32>,
    /// Optional channel for receiving returned buffers from the encoding thread.
    /// After the IPC thread finishes Opus-encoding a CompletedInterval, it sends
    /// the now-empty Vec<f32> back so we can reuse it as the spare — zero alloc.
    buffer_return_rx: Option<crossbeam_channel::Receiver<Vec<f32>>>,
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
    /// Number of interleaved samples to fade in for a new peer's first interval (10ms).
    fade_in_samples: usize,
    /// Interval parameters
    bars: u32,
    quantum: f64,
    /// Per-peer-stream isolated playback slots (up to MAX_REMOTE_PEERS)
    peer_slots: Vec<PeerSlot>,
    /// Stable slot assignment table (handles active + affinity reservations)
    slot_table: SlotTable,
    /// Maps peer_id → persistent identity (for slot affinity across reconnects).
    /// Needed because audio arrives keyed by session-scoped peer_id, but SlotTable
    /// uses persistent client_id.
    peer_identity_map: Vec<(String, String)>,
}

/// A completed local recording ready for encoding.
pub struct CompletedInterval {
    pub index: i64,
    pub samples: Vec<f32>,
}

/// A received remote interval to mix into playback.
struct RemoteInterval {
    #[allow(dead_code)]
    index: i64,
    peer_id: String,
    stream_id: u16,
    pub samples: Vec<f32>,
}

impl IntervalRing {
    /// Create a new interval ring buffer.
    ///
    /// All buffers are pre-allocated so that `process()` never allocates on the
    /// audio thread during normal operation. At interval boundaries, a spare
    /// buffer is swapped in (zero-copy). The spare is replenished lazily.
    pub fn new(sample_rate: u32, channels: u16, bars: u32, quantum: f64) -> Self {
        let bars = bars.max(1);
        let quantum = quantum.max(f64::EPSILON);
        let beats_per_interval = bars as f64 * quantum;
        // Pre-allocate for slowest expected tempo (20 BPM) so we never grow
        // on the audio thread. At 20 BPM, 16 beats = 48 seconds.
        let min_bps = 20.0_f64 / 60.0; // 20 BPM in beats-per-second
        let max_seconds = beats_per_interval / min_bps;
        let slot_capacity = (sample_rate as f64 * max_seconds * channels as f64) as usize;

        let fade_in_samples = (sample_rate as usize * 10 / 1000) * channels as usize;

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
            spare_record: Vec::with_capacity(slot_capacity),
            buffer_return_rx: None,
            current_interval: None,
            completed: Vec::with_capacity(2),
            pending_remote: Vec::with_capacity(MAX_REMOTE_PEERS),
            sample_rate,
            channels,
            fade_in_samples,
            bars,
            quantum,
            peer_slots,
            slot_table: SlotTable::new(),
            peer_identity_map: Vec::with_capacity(MAX_REMOTE_PEERS),
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
        // Replenish spare record buffer between boundaries so that
        // swap_intervals() can do a zero-copy move instead of to_vec().
        // Prefer reclaiming a buffer from the encoding thread (zero alloc)
        // over a fresh allocation (only needed during warmup).
        if self.spare_record.capacity() == 0 {
            if let Some(ref rx) = self.buffer_return_rx {
                if let Ok(buf) = rx.try_recv() {
                    self.spare_record = buf;
                }
            }
            if self.spare_record.capacity() == 0 {
                self.spare_record = Vec::with_capacity(self.slot_capacity);
            }
        }

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
    /// Multiple peers' audio is summed together. Each unique `(peer_id, stream_id)`
    /// pair gets its own isolated slot for per-stream DAW routing.
    pub fn feed_remote(&mut self, peer_id: String, stream_id: u16, interval_index: i64, samples: Vec<f32>) {
        self.pending_remote.push(RemoteInterval {
            index: interval_index,
            peer_id,
            stream_id,
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

    /// Set the buffer return channel for zero-allocation spare replenishment.
    ///
    /// After the encoding thread finishes with a CompletedInterval's sample buffer,
    /// it sends the empty Vec back through this channel. `process()` reclaims it
    /// via `try_recv()` — after warmup (2-3 intervals), no allocations occur on
    /// the audio thread.
    pub fn set_buffer_return_rx(&mut self, rx: crossbeam_channel::Receiver<Vec<f32>>) {
        self.buffer_return_rx = Some(rx);
    }

    /// Reset all state (preserves pre-allocated capacity).
    pub fn reset(&mut self) {
        self.record_slot.clear();
        self.spare_record.clear();
        // Ensure record_slot has capacity (spare may have been swapped in empty)
        if self.record_slot.capacity() == 0 {
            self.record_slot = Vec::with_capacity(self.slot_capacity);
        }
        if self.spare_record.capacity() == 0 {
            self.spare_record = Vec::with_capacity(self.slot_capacity);
        }
        self.record_pos = 0;
        self.playback_pos = 0;
        self.playback_len = 0;
        self.current_interval = None;
        self.completed.clear();
        self.pending_remote.clear();
        for slot in &mut self.peer_slots {
            slot.clear();
        }
        self.slot_table.clear();
        self.peer_identity_map.clear();
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

    /// Get the per-peer-stream playback slots (up to MAX_REMOTE_PEERS).
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

    /// Return (slot_index, peer_id, stream_id) for all active peer slots.
    pub fn active_peer_slots(&self) -> Vec<(usize, String, u16)> {
        self.peer_slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.active)
            .map(|(i, s)| (i, s.peer_id.clone(), s.stream_id))
            .collect()
    }

    /// Access the underlying slot table (for diagnostics / UI).
    pub fn slot_table(&self) -> &SlotTable {
        &self.slot_table
    }

    /// Register a peer's persistent identity for slot affinity.
    ///
    /// Call this when a Hello is received with an identity. If this identity
    /// has reserved slots from a previous connection, they are reclaimed
    /// immediately so audio arriving for this peer lands in the same DAW slots.
    pub fn notify_peer_joined(&mut self, peer_id: &str, identity: &str) {
        // Remove any stale mapping for this peer_id
        self.peer_identity_map.retain(|(pid, _)| pid != peer_id);
        self.peer_identity_map.push((peer_id.to_string(), identity.to_string()));

        // Re-key any active slots assigned under the fallback peer_id key
        // (happens when audio arrived before identity was known)
        self.slot_table.rekey_client(peer_id, identity);

        // Reclaim any reserved slots for this identity
        let reclaimed = self.slot_table.reclaim_reserved_for_client(identity);
        for (stream_id, slot_idx) in reclaimed {
            if slot_idx < self.peer_slots.len() {
                self.peer_slots[slot_idx].peer_id = peer_id.to_string();
                self.peer_slots[slot_idx].stream_id = stream_id;
                self.peer_slots[slot_idx].active = true;
                self.peer_slots[slot_idx].needs_fade_in = true;
                let mapping = ClientChannelMapping::new(identity, stream_id);
                tracing::info!(
                    peer_id, identity, stream_id, slot = slot_idx,
                    "[{}] Peer reclaimed affinity slot", mapping.short_id()
                );
            }
        }
    }

    /// Remove a peer and free ALL their stream slots, creating affinity
    /// reservations so the same identity can reclaim slots on reconnect.
    pub fn remove_peer(&mut self, peer_id: &str) {
        // Look up identity for affinity. If no identity, use peer_id as the
        // client_id (matching the fallback in assign_peer_slot).
        let client_id = self.peer_identity_map.iter()
            .find(|(pid, _)| pid == peer_id)
            .map(|(_, ident)| ident.clone())
            .unwrap_or_else(|| peer_id.to_string());

        // Release all slots for this client via SlotTable (creates reservations)
        self.slot_table.release_all_for_client(&client_id);

        if client_id != peer_id {
            tracing::info!(
                peer_id, identity = %client_id,
                "Peer left — slots reserved for affinity"
            );
        }

        // Clear the PeerSlot audio data for all slots that belonged to this peer
        for slot in &mut self.peer_slots {
            if slot.peer_id == peer_id {
                slot.active = false;
                slot.samples.clear();
                slot.read_pos = 0;
            }
        }

        self.peer_identity_map.retain(|(pid, _)| pid != peer_id);
    }

    /// Look up or assign a slot index for a (peer_id, stream_id) pair.
    /// Returns None if all slots are full or the peer's identity is unknown.
    fn assign_peer_slot(&mut self, peer_id: &str, stream_id: u16) -> Option<usize> {
        // Resolve peer_id to persistent identity
        let identity = self.peer_identity_map.iter()
            .find(|(pid, _)| pid == peer_id)
            .map(|(_, ident)| ident.clone());

        let mapping = match identity {
            Some(ref ident) => ClientChannelMapping::new(ident.as_str(), stream_id),
            None => {
                // No identity known — use peer_id as client_id fallback
                ClientChannelMapping::new(peer_id, stream_id)
            }
        };

        let slot_idx = self.slot_table.assign(&mapping)?;

        if slot_idx < self.peer_slots.len() {
            let slot = &mut self.peer_slots[slot_idx];
            if !slot.active || slot.peer_id != peer_id || slot.stream_id != stream_id {
                slot.peer_id = peer_id.to_string();
                slot.stream_id = stream_id;
                slot.active = true;
                slot.needs_fade_in = true;
            }
        }

        Some(slot_idx)
    }

    /// Swap intervals: move record → completed, mix pending remote → playback.
    ///
    /// Zero-copy: the filled record_slot is moved into CompletedInterval and the
    /// pre-allocated spare_record becomes the new record_slot.  The spare is
    /// replenished lazily in `process()` before the next boundary.
    fn swap_intervals(&mut self, completed_index: i64) {
        if self.record_pos > 0 {
            // Move the record buffer into completed (zero-copy — no alloc, no memcpy)
            let mut samples = std::mem::take(&mut self.record_slot);
            samples.truncate(self.record_pos);
            self.completed.push(CompletedInterval {
                index: completed_index,
                samples,
            });
            // Swap in the spare as the new record slot
            self.record_slot = std::mem::take(&mut self.spare_record);
        } else {
            self.record_slot.clear();
        }
        self.record_pos = 0;

        // Clear per-peer slots (but keep assignments)
        for slot in &mut self.peer_slots {
            slot.samples.clear();
            slot.read_pos = 0;
        }

        // Mix pending remote intervals into pre-allocated playback slot
        self.playback_pos = 0;
        self.playback_len = 0;

        // Take pending_remote, drain it (preserving capacity), then put it back.
        let mut pending = std::mem::take(&mut self.pending_remote);
        for mut remote in pending.drain(..) {
            // Assign slot FIRST so we can check needs_fade_in before summing
            let slot_assignment = self.assign_peer_slot(&remote.peer_id, remote.stream_id);

            // Apply fade-in for the peer's first interval (prevents pop/click on join)
            if let Some(slot_idx) = slot_assignment {
                if self.peer_slots[slot_idx].needs_fade_in {
                    let fade_len = self.fade_in_samples.min(remote.samples.len());
                    for i in 0..fade_len {
                        remote.samples[i] *= i as f32 / fade_len as f32;
                    }
                    self.peer_slots[slot_idx].needs_fade_in = false;
                }
            }

            let mix_len = self.playback_len.max(remote.samples.len());
            // Grow playback within pre-allocated capacity
            let mix_len = mix_len.min(self.playback_slot.len());

            // Zero-fill the extension range
            for s in &mut self.playback_slot[self.playback_len..mix_len] {
                *s = 0.0;
            }
            self.playback_len = mix_len;

            // Sum remote audio into playback (with fade already applied if needed)
            let copy_len = remote.samples.len().min(self.playback_slot.len());
            for (i, sample) in remote.samples[..copy_len].iter().enumerate() {
                self.playback_slot[i] += sample;
            }

            // Move samples to per-peer-stream slot
            match slot_assignment {
                Some(slot_idx) => {
                    self.peer_slots[slot_idx].samples = remote.samples;
                    self.peer_slots[slot_idx].read_pos = 0;
                }
                None => {
                    // All slots full — merge into this peer's stream 0 slot
                    let fallback_identity = self.peer_identity_map.iter()
                        .find(|(pid, _)| pid == &remote.peer_id)
                        .map(|(_, ident)| ident.clone())
                        .unwrap_or_else(|| remote.peer_id.clone());
                    let fallback_mapping = ClientChannelMapping::new(&fallback_identity, 0);
                    if let Some(slot_idx) = self.slot_table.slot_for(&fallback_mapping) {
                        let slot = &mut self.peer_slots[slot_idx];
                        let merge_len = slot.samples.len().max(remote.samples.len());
                        slot.samples.resize(merge_len, 0.0);
                        for (i, &s) in remote.samples.iter().enumerate() {
                            if i < slot.samples.len() {
                                slot.samples[i] += s;
                            }
                        }
                        slot.read_pos = 0;
                        tracing::warn!(
                            peer = %remote.peer_id,
                            stream = remote.stream_id,
                            "All slots full — merged into stream 0"
                        );
                    } else {
                        tracing::warn!(
                            peer = %remote.peer_id,
                            stream = remote.stream_id,
                            "All slots full and no stream 0 slot — audio dropped"
                        );
                    }
                }
            }
        }
        // Put the drained (empty but with capacity) Vec back
        self.pending_remote = pending;
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

    /// Fade-in length in interleaved samples: 10ms at 48kHz stereo = 960
    const FADE_LEN: usize = (SR as usize * 10 / 1000) * CH as usize;

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
        let buf = FADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        // Start in interval 0
        ring.process(&input, &mut output, 0.0);

        // Feed remote audio for next playback
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.7f32; buf]);

        // Cross into interval 1 — remote audio should become playback
        ring.process(&input, &mut output, 16.0);

        // First sample is faded from silence
        assert!(output[0].abs() < f32::EPSILON);
        // Post-fade region should contain the remote audio at full amplitude
        assert!(output[FADE_LEN..].iter().all(|&s| (s - 0.7).abs() < f32::EPSILON),
            "Output should be remote audio after fade-in, got: {:?}", &output[FADE_LEN..FADE_LEN+4]);
    }

    #[test]
    fn mixes_multiple_remote_peers() {
        let mut ring = make_ring();
        let buf = FADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        // Start in interval 0
        ring.process(&input, &mut output, 0.0);

        // Feed from two peers
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.3f32; buf]);
        ring.feed_remote("peer-b".into(), 0, 0, vec![0.5f32; buf]);

        // Cross boundary — both should be mixed (summed)
        ring.process(&input, &mut output, 16.0);

        // Post-fade region: both peers at full amplitude
        assert!(output[FADE_LEN..].iter().all(|&s| (s - 0.8).abs() < 0.001),
            "Expected 0.3 + 0.5 = 0.8 after fade, got: {:?}", &output[FADE_LEN..FADE_LEN+4]);
    }

    #[test]
    fn remote_audio_longer_than_buffer_spans_calls() {
        let mut ring = make_ring();
        let buf = FADE_LEN / 2;
        let remote_len = FADE_LEN + buf * 2;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        // Start interval 0
        ring.process(&input, &mut output, 0.0);

        // Feed remote audio spanning multiple buffers
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.4f32; remote_len]);

        // Cross into interval 1 — first buffer is in the fade region
        ring.process(&input, &mut output, 16.0);
        assert_eq!(ring.playback_remaining(), remote_len - buf);

        // Keep reading until we're past the fade region
        ring.process(&input, &mut output, 16.5);
        assert_eq!(ring.playback_remaining(), remote_len - buf * 2);

        // Third call: now past fade — should be full amplitude
        ring.process(&input, &mut output, 17.0);
        assert!(output.iter().all(|&s| (s - 0.4).abs() < f32::EPSILON),
            "Post-fade samples should be 0.4, got: {:?}", &output[..4]);
    }

    #[test]
    fn silence_after_playback_exhausted() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 64];
        let mut output = vec![0.0f32; 64];

        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.5f32; 32]); // only 32 samples

        // Cross boundary
        ring.process(&input, &mut output, 16.0);

        // First sample faded from silence
        assert!(output[0].abs() < f32::EPSILON, "First sample should be ~0 (faded)");
        // Sample 31 should be faded (32 samples < fade_len, so entire buffer is ramped)
        let expected_last = 0.5 * 31.0 / 32.0;
        assert!((output[31] - expected_last).abs() < 0.01,
            "Last audio sample should be ~{expected_last}, got: {}", output[31]);
        // Rest = silence
        assert_eq!(output[32], 0.0);
        assert_eq!(output[63], 0.0);
    }

    // --- Test: Multiple intervals ---

    #[test]
    fn multiple_interval_cycle() {
        let mut ring = make_ring();
        let buf = FADE_LEN + 64;
        let ones = vec![1.0f32; buf];
        let twos = vec![2.0f32; buf];
        let mut output = vec![0.0f32; buf];

        // Interval 0: record ones
        ring.process(&ones, &mut output, 0.0);
        ring.process(&ones, &mut output, 8.0);

        // Feed remote for playback in interval 1
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.9f32; buf]);

        // Interval 1: record twos, play remote (first interval = faded)
        ring.process(&twos, &mut output, 16.0);
        let completed = ring.take_completed();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].index, 0);
        // First interval from peer-a is faded — check post-fade
        assert!((output[FADE_LEN] - 0.9).abs() < f32::EPSILON);

        // Feed new remote for interval 2
        ring.feed_remote("peer-a".into(), 0, 1, vec![0.6f32; buf]);

        // Interval 2: record ones, play new remote
        ring.process(&ones, &mut output, 32.0);
        let completed = ring.take_completed();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].index, 1);
        // Completed interval 1 should contain twos
        assert!((completed[0].samples[0] - 2.0).abs() < f32::EPSILON);
        // Second interval from same peer should NOT be faded
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
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.3f32; 64]);

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
        let buf = FADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        ring.process(&input, &mut output, 0.0);

        // Feed from two distinct peers
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.3f32; buf]);
        ring.feed_remote("peer-b".into(), 0, 0, vec![0.7f32; buf]);

        // Cross boundary to activate playback
        ring.process(&input, &mut output, 16.0);

        // Read per-peer slots independently
        let mut slot_a_out = vec![0.0f32; buf];
        let mut slot_b_out = vec![0.0f32; buf];

        let active = ring.active_peer_slots();
        assert_eq!(active.len(), 2);

        // Find which slot is which
        let (a_idx, _, _) = active.iter().find(|(_, pid, _)| pid == "peer-a").unwrap();
        let (b_idx, _, _) = active.iter().find(|(_, pid, _)| pid == "peer-b").unwrap();

        ring.read_peer_playback(*a_idx, &mut slot_a_out);
        ring.read_peer_playback(*b_idx, &mut slot_b_out);

        // Post-fade: Peer A's slot should have 0.3
        assert!(
            slot_a_out[FADE_LEN..].iter().all(|&s| (s - 0.3).abs() < f32::EPSILON),
            "Peer A slot should be 0.3 after fade, got: {:?}", &slot_a_out[FADE_LEN..FADE_LEN+4]
        );
        // Post-fade: Peer B's slot should have 0.7
        assert!(
            slot_b_out[FADE_LEN..].iter().all(|&s| (s - 0.7).abs() < f32::EPSILON),
            "Peer B slot should be 0.7 after fade, got: {:?}", &slot_b_out[FADE_LEN..FADE_LEN+4]
        );
    }

    #[test]
    fn per_peer_and_summed_mix_consistent() {
        let mut ring = make_ring();
        let buf = FADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        ring.process(&input, &mut output, 0.0);

        ring.feed_remote("peer-x".into(), 0, 0, vec![0.2f32; buf]);
        ring.feed_remote("peer-y".into(), 0, 0, vec![0.5f32; buf]);

        // Cross boundary
        ring.process(&input, &mut output, 16.0);

        // Post-fade: summed mix should be 0.2 + 0.5 = 0.7
        assert!(
            output[FADE_LEN..].iter().all(|&s| (s - 0.7).abs() < 0.001),
            "Summed mix should be 0.7 after fade, got: {:?}", &output[FADE_LEN..FADE_LEN+4]
        );

        // Per-peer should sum to the same thing
        let active = ring.active_peer_slots();
        let mut sum = vec![0.0f32; buf];
        for (idx, _, _) in &active {
            let mut peer_buf = vec![0.0f32; buf];
            ring.read_peer_playback(*idx, &mut peer_buf);
            for (i, s) in peer_buf.iter().enumerate() {
                sum[i] += s;
            }
        }

        for (i, &s) in sum.iter().enumerate().skip(FADE_LEN) {
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

    // --- Test: Realistic DAW timing fills the full interval ---

    #[test]
    fn interval_fill_ratio_realistic_daw() {
        let mut ring = make_ring();
        let bpm = 120.0_f64;
        let buf_frames: usize = 256;
        let buf_size = buf_frames * CH as usize;

        let input: Vec<f32> = (0..buf_size)
            .map(|i| {
                let t = (i / CH as usize) as f32 / SR as f32;
                (t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5
            })
            .collect();
        let mut output = vec![0.0f32; buf_size];

        let beats_per_callback = buf_frames as f64 / SR as f64 * bpm / 60.0;
        let mut beat = 0.0_f64;
        let mut callbacks = 0_u32;

        while beat < 16.0 {
            ring.process(&input, &mut output, beat);
            beat += beats_per_callback;
            callbacks += 1;
        }

        ring.process(&input, &mut output, beat);
        callbacks += 1;

        let completed = ring.take_completed();
        assert_eq!(completed.len(), 1, "Should produce exactly 1 completed interval");

        let interval = &completed[0];
        assert_eq!(interval.index, 0);

        let expected_samples = (SR as f64 * 8.0 * CH as f64) as usize;
        let tolerance = buf_size * 2;

        assert!(
            interval.samples.len() > expected_samples - tolerance,
            "Interval should contain near-full recording. Got {} samples, expected ~{} (callbacks={})",
            interval.samples.len(), expected_samples, callbacks,
        );
        assert!(
            interval.samples.len() <= expected_samples + tolerance,
            "Interval should not exceed expected size. Got {} samples, expected ~{}",
            interval.samples.len(), expected_samples,
        );

        let rms: f32 = (interval.samples.iter().map(|s| s * s).sum::<f32>()
            / interval.samples.len() as f32)
            .sqrt();
        assert!(rms > 0.1, "Completed interval should contain real audio, RMS={rms}");

        eprintln!(
            "[test] interval_fill_ratio: callbacks={}, samples={}, expected≈{}, RMS={:.4}",
            callbacks, interval.samples.len(), expected_samples, rms,
        );
    }

    // --- Test: Peer affinity slots ---

    #[test]
    fn remove_peer_frees_slot() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.3f32; 128]);
        ring.process(&input, &mut output, 16.0);

        assert_eq!(ring.active_peer_slots().len(), 1);
        let (slot_a, _, _) = ring.active_peer_slots()[0].clone();

        ring.remove_peer("peer-a");
        assert_eq!(ring.active_peer_slots().len(), 0);

        // The freed slot should be reusable by a new peer
        ring.feed_remote("peer-b".into(), 0, 1, vec![0.5f32; 128]);
        ring.process(&input, &mut output, 32.0);

        let active = ring.active_peer_slots();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, slot_a, "New peer should get the freed slot");
        assert_eq!(active[0].1, "peer-b");
    }

    #[test]
    fn affinity_reclaims_same_slot_on_reconnect() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        // peer-a gets slot 0, peer-b gets slot 1
        ring.notify_peer_joined("peer-a", "identity-alice");
        ring.notify_peer_joined("peer-b", "identity-bob");

        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.3f32; 128]);
        ring.feed_remote("peer-b".into(), 0, 0, vec![0.7f32; 128]);
        ring.process(&input, &mut output, 16.0);

        let active = ring.active_peer_slots();
        let slot_a = active.iter().find(|(_, pid, _)| pid == "peer-a").unwrap().0;
        let slot_b = active.iter().find(|(_, pid, _)| pid == "peer-b").unwrap().0;
        assert_ne!(slot_a, slot_b);

        // peer-a disconnects
        ring.remove_peer("peer-a");
        assert_eq!(ring.active_peer_slots().len(), 1);

        // peer-a reconnects with a NEW peer_id but same identity
        ring.notify_peer_joined("peer-a-new", "identity-alice");

        // Feed audio from the new peer_id
        ring.feed_remote("peer-a-new".into(), 0, 2, vec![0.9f32; 128]);
        ring.process(&input, &mut output, 32.0);

        // peer-a-new should have reclaimed peer-a's original slot
        let active = ring.active_peer_slots();
        let new_slot = active.iter().find(|(_, pid, _)| pid == "peer-a-new").unwrap().0;
        assert_eq!(new_slot, slot_a, "Reconnected peer should reclaim original slot via affinity");

        // peer-b should still have their original slot
        let bob_slot = active.iter().find(|(_, pid, _)| pid == "peer-b").unwrap().0;
        assert_eq!(bob_slot, slot_b);
    }

    #[test]
    fn affinity_slot_taken_falls_back_to_first_fit() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        // Set up: peer-a at slot 0
        ring.notify_peer_joined("peer-a", "identity-alice");
        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.3f32; 128]);
        ring.process(&input, &mut output, 16.0);
        let slot_a = ring.active_peer_slots()[0].0;

        // peer-a leaves — affinity reserved
        ring.remove_peer("peer-a");

        // A different peer takes slot 0
        ring.feed_remote("peer-c".into(), 0, 1, vec![0.1f32; 128]);
        ring.process(&input, &mut output, 32.0);
        let slot_c = ring.active_peer_slots().iter().find(|(_, pid, _)| pid == "peer-c").unwrap().0;
        assert_eq!(slot_c, slot_a, "peer-c should take the freed slot");

        // Now peer-a reconnects — their affinity slot is taken
        ring.notify_peer_joined("peer-a-new", "identity-alice");
        ring.feed_remote("peer-a-new".into(), 0, 3, vec![0.5f32; 128]);
        ring.process(&input, &mut output, 48.0);

        // Should get a different slot (first-fit fallback)
        let active = ring.active_peer_slots();
        let new_slot = active.iter().find(|(_, pid, _)| pid == "peer-a-new").unwrap().0;
        assert_ne!(new_slot, slot_a, "Affinity slot is occupied — should get a different one");
    }

    #[test]
    fn remove_peer_without_identity_frees_without_affinity() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        // peer-a joins WITHOUT identity (old client)
        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.3f32; 128]);
        ring.process(&input, &mut output, 16.0);

        assert_eq!(ring.active_peer_slots().len(), 1);
        ring.remove_peer("peer-a");
        assert_eq!(ring.active_peer_slots().len(), 0);

        // Slot is freed with no affinity reservation (no identity)
        ring.feed_remote("peer-b".into(), 0, 1, vec![0.5f32; 128]);
        ring.process(&input, &mut output, 32.0);
        assert_eq!(ring.active_peer_slots().len(), 1);
    }

    #[test]
    fn reset_clears_affinity() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        ring.notify_peer_joined("peer-a", "identity-alice");
        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.3f32; 128]);
        ring.process(&input, &mut output, 16.0);
        ring.remove_peer("peer-a");

        // Reset clears everything including affinity
        ring.reset();

        // Reconnect — should get first-fit (slot 0), not necessarily from affinity
        ring.notify_peer_joined("peer-a-new", "identity-alice");
        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a-new".into(), 0, 0, vec![0.5f32; 128]);
        ring.process(&input, &mut output, 16.0);

        let active = ring.active_peer_slots();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, 0, "After reset, slot 0 should be assigned first-fit");
    }

    // --- Test: Multi-stream support ---

    #[test]
    fn multi_stream_same_peer_separate_slots() {
        let mut ring = make_ring();
        let buf = FADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        ring.process(&input, &mut output, 0.0);

        // Same peer, two different streams
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.3f32; buf]);
        ring.feed_remote("peer-a".into(), 1, 0, vec![0.7f32; buf]);

        ring.process(&input, &mut output, 16.0);

        let active = ring.active_peer_slots();
        assert_eq!(active.len(), 2, "Two streams from same peer should get separate slots");

        // Find slots by stream_id
        let (s0_idx, _, _) = active.iter().find(|(_, pid, sid)| pid == "peer-a" && *sid == 0).unwrap();
        let (s1_idx, _, _) = active.iter().find(|(_, pid, sid)| pid == "peer-a" && *sid == 1).unwrap();
        assert_ne!(s0_idx, s1_idx);

        let mut s0_out = vec![0.0f32; buf];
        let mut s1_out = vec![0.0f32; buf];
        ring.read_peer_playback(*s0_idx, &mut s0_out);
        ring.read_peer_playback(*s1_idx, &mut s1_out);

        // Post-fade: each stream at full amplitude
        assert!(s0_out[FADE_LEN..].iter().all(|&s| (s - 0.3).abs() < f32::EPSILON));
        assert!(s1_out[FADE_LEN..].iter().all(|&s| (s - 0.7).abs() < f32::EPSILON));

        // Summed playback should be 0.3 + 0.7 = 1.0 (post-fade)
        assert!(output[FADE_LEN..].iter().all(|&s| (s - 1.0).abs() < 0.001),
            "Summed mix should be 1.0 after fade, got: {:?}", &output[FADE_LEN..FADE_LEN+4]);
    }

    #[test]
    fn slot_exhaustion_merges_to_stream_0() {
        let mut ring = make_ring();
        let buf = FADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        ring.process(&input, &mut output, 0.0);

        // Fill all 31 slots with distinct peer-streams
        // Peer-a stream 0 is at slot 0 (this is the merge target)
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.1f32; buf]);
        for i in 1..MAX_REMOTE_PEERS {
            let peer = format!("peer-fill-{i}");
            ring.feed_remote(peer, 0, 0, vec![0.01f32; buf]);
        }

        // 32nd stream should overflow — merge into peer-a's stream 0
        ring.feed_remote("peer-a".into(), 5, 0, vec![0.5f32; buf]);

        ring.process(&input, &mut output, 16.0);

        // Should still have exactly 31 active slots (no new slot for overflow)
        let active = ring.active_peer_slots();
        assert_eq!(active.len(), MAX_REMOTE_PEERS);

        // peer-a stream 0 should contain merged audio post-fade
        // stream 0 (0.1) is faded, overflow stream 5 (0.5) is unfaded (no slot assigned)
        // After fade region: faded(0.1) converges to 0.1, so total = 0.1 + 0.5 = 0.6
        let (s0_idx, _, _) = active.iter().find(|(_, pid, sid)| pid == "peer-a" && *sid == 0).unwrap();
        let mut s0_out = vec![0.0f32; buf];
        ring.read_peer_playback(*s0_idx, &mut s0_out);
        assert!(
            s0_out[FADE_LEN..].iter().all(|&s| (s - 0.6).abs() < 0.01),
            "Overflowed stream should merge into stream 0 (post-fade), got: {:?}",
            &s0_out[FADE_LEN..FADE_LEN+4]
        );
    }

    #[test]
    fn remove_peer_frees_all_streams() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        ring.process(&input, &mut output, 0.0);

        // Peer-a sends 3 streams
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.1f32; 128]);
        ring.feed_remote("peer-a".into(), 1, 0, vec![0.2f32; 128]);
        ring.feed_remote("peer-a".into(), 2, 0, vec![0.3f32; 128]);

        ring.process(&input, &mut output, 16.0);
        assert_eq!(ring.active_peer_slots().len(), 3);

        // Remove peer-a — all 3 streams should be freed
        ring.remove_peer("peer-a");
        assert_eq!(ring.active_peer_slots().len(), 0);
    }

    #[test]
    fn affinity_multi_stream_reclaims_all_slots() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        ring.notify_peer_joined("peer-a", "identity-alice");
        ring.process(&input, &mut output, 0.0);

        // Peer-a sends 2 streams
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.1f32; 128]);
        ring.feed_remote("peer-a".into(), 1, 0, vec![0.2f32; 128]);
        ring.process(&input, &mut output, 16.0);

        let active = ring.active_peer_slots();
        let slot_s0 = active.iter().find(|(_, _, sid)| *sid == 0).unwrap().0;
        let slot_s1 = active.iter().find(|(_, _, sid)| *sid == 1).unwrap().0;

        // Disconnect
        ring.remove_peer("peer-a");

        // Reconnect with new peer_id, same identity
        ring.notify_peer_joined("peer-a-new", "identity-alice");
        ring.feed_remote("peer-a-new".into(), 0, 1, vec![0.3f32; 128]);
        ring.feed_remote("peer-a-new".into(), 1, 1, vec![0.4f32; 128]);
        ring.process(&input, &mut output, 32.0);

        let active = ring.active_peer_slots();
        let new_s0 = active.iter().find(|(_, _, sid)| *sid == 0).unwrap().0;
        let new_s1 = active.iter().find(|(_, _, sid)| *sid == 1).unwrap().0;

        assert_eq!(new_s0, slot_s0, "Stream 0 should reclaim original slot");
        assert_eq!(new_s1, slot_s1, "Stream 1 should reclaim original slot");
    }

    // --- Test: Fade-in on peer join/reconnect ---

    #[test]
    fn fade_in_applied_to_first_interval_from_new_peer() {
        let mut ring = make_ring();
        let buf = FADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        ring.process(&input, &mut output, 0.0);

        // Feed constant-amplitude audio from a new peer
        ring.feed_remote("peer-a".into(), 0, 0, vec![1.0f32; buf]);
        ring.process(&input, &mut output, 16.0);

        // First sample should be 0.0 (faded from silence)
        assert!(output[0].abs() < f32::EPSILON,
            "First sample should be ~0.0 (faded), got: {}", output[0]);

        // Mid-fade should be ~0.5
        let mid = FADE_LEN / 2;
        let expected_mid = mid as f32 / FADE_LEN as f32;
        assert!((output[mid] - expected_mid).abs() < 0.01,
            "Mid-fade sample should be ~{expected_mid}, got: {}", output[mid]);

        // Post-fade should be full amplitude
        assert!((output[FADE_LEN] - 1.0).abs() < f32::EPSILON,
            "Post-fade sample should be 1.0, got: {}", output[FADE_LEN]);

        // Per-peer slot should also be faded
        let active = ring.active_peer_slots();
        let (slot_idx, _, _) = active.iter().find(|(_, pid, _)| pid == "peer-a").unwrap();
        let mut peer_out = vec![0.0f32; buf];
        ring.read_peer_playback(*slot_idx, &mut peer_out);

        assert!(peer_out[0].abs() < f32::EPSILON,
            "Per-peer first sample should be ~0.0, got: {}", peer_out[0]);
        assert!((peer_out[FADE_LEN] - 1.0).abs() < f32::EPSILON,
            "Per-peer post-fade sample should be 1.0, got: {}", peer_out[FADE_LEN]);
    }

    #[test]
    fn no_fade_on_second_interval_from_same_peer() {
        let mut ring = make_ring();
        let buf = FADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        ring.process(&input, &mut output, 0.0);

        // First interval from peer — will be faded
        ring.feed_remote("peer-a".into(), 0, 0, vec![1.0f32; buf]);
        ring.process(&input, &mut output, 16.0);
        assert!(output[0].abs() < f32::EPSILON, "First interval should be faded");

        // Feed second interval from same peer
        ring.feed_remote("peer-a".into(), 0, 1, vec![0.8f32; buf]);
        ring.process(&input, &mut output, 32.0);

        // Second interval should NOT be faded — first sample at full amplitude
        assert!((output[0] - 0.8).abs() < f32::EPSILON,
            "Second interval should NOT be faded, got: {}", output[0]);
    }

    #[test]
    fn fade_in_applied_on_reconnect_via_affinity() {
        let mut ring = make_ring();
        let buf = FADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        ring.notify_peer_joined("peer-a", "identity-alice");
        ring.process(&input, &mut output, 0.0);

        // First interval — faded
        ring.feed_remote("peer-a".into(), 0, 0, vec![1.0f32; buf]);
        ring.process(&input, &mut output, 16.0);

        // Second interval — not faded (steady state)
        ring.feed_remote("peer-a".into(), 0, 1, vec![1.0f32; buf]);
        ring.process(&input, &mut output, 32.0);
        assert!((output[0] - 1.0).abs() < f32::EPSILON, "Steady state should not be faded");

        // Disconnect
        ring.remove_peer("peer-a");

        // Reconnect with new peer_id, same identity
        ring.notify_peer_joined("peer-a-new", "identity-alice");

        // First interval after reconnect — should be faded again
        ring.feed_remote("peer-a-new".into(), 0, 2, vec![1.0f32; buf]);
        ring.process(&input, &mut output, 48.0);

        assert!(output[0].abs() < f32::EPSILON,
            "First sample after reconnect should be ~0.0 (faded), got: {}", output[0]);
        assert!((output[FADE_LEN] - 1.0).abs() < f32::EPSILON,
            "Post-fade should be 1.0, got: {}", output[FADE_LEN]);
    }

    #[test]
    fn fade_in_summed_mix_consistency() {
        let mut ring = make_ring();
        let buf = FADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        ring.process(&input, &mut output, 0.0);

        // Two new peers, both should be faded
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.5f32; buf]);
        ring.feed_remote("peer-b".into(), 0, 0, vec![0.5f32; buf]);

        ring.process(&input, &mut output, 16.0);

        // First sample: both peers faded from 0 → sum should be 0
        assert!(output[0].abs() < f32::EPSILON,
            "Summed first sample should be ~0.0, got: {}", output[0]);

        // After fade: 0.5 + 0.5 = 1.0
        assert!((output[FADE_LEN] - 1.0).abs() < f32::EPSILON,
            "Post-fade summed should be 1.0, got: {}", output[FADE_LEN]);

        // Per-peer slots should sum to the same as the main output
        let active = ring.active_peer_slots();
        let mut sum = vec![0.0f32; buf];
        for (idx, _, _) in &active {
            let mut peer_buf = vec![0.0f32; buf];
            ring.read_peer_playback(*idx, &mut peer_buf);
            for (i, s) in peer_buf.iter().enumerate() {
                sum[i] += s;
            }
        }

        for i in 0..buf {
            assert!((sum[i] - output[i]).abs() < 0.001,
                "Per-peer sum at {i} = {}, expected {}", sum[i], output[i]);
        }
    }

    #[test]
    fn fade_in_short_interval_clamped() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 64];
        let mut output = vec![0.0f32; 64];

        ring.process(&input, &mut output, 0.0);

        // Feed only 32 samples (much shorter than FADE_LEN=960)
        ring.feed_remote("peer-a".into(), 0, 0, vec![1.0f32; 32]);
        ring.process(&input, &mut output, 16.0);

        // Should not panic; first sample should be 0
        assert!(output[0].abs() < f32::EPSILON);
        // Last audio sample: fade_len clamped to 32, so sample 31 = 31/32
        let expected_last = 31.0 / 32.0;
        assert!((output[31] - expected_last).abs() < 0.01,
            "Last sample should be ~{expected_last}, got: {}", output[31]);
        // Silence after audio
        assert_eq!(output[32], 0.0);
    }

    // --- Tests: reconnect with same peer_id (WebRTC-level reconnect, no new peer_id) ---

    /// When the WebRTC connection drops and reconnects without a new peer_id
    /// (session retries the same connection), no PeerLeft IPC is sent.
    /// The session sends PeerJoined again with the same peer_id and identity.
    /// The ring must keep the peer on the same slot — the slot was never freed.
    #[test]
    fn same_peer_id_reconnect_keeps_same_slot() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        // Initial connection: peer-a joins and is assigned slot 0
        ring.notify_peer_joined("peer-a", "identity-alice");
        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.3f32; 128]);
        ring.process(&input, &mut output, 16.0);

        let slot_a = ring.active_peer_slots()
            .iter()
            .find(|(_, pid, _)| pid == "peer-a")
            .unwrap()
            .0;

        // WebRTC reconnect: PeerJoined IPC re-sent with same peer_id/identity,
        // but NO PeerLeft was sent (slot is still active)
        ring.notify_peer_joined("peer-a", "identity-alice");

        // Audio resumes from the same peer_id
        ring.feed_remote("peer-a".into(), 0, 1, vec![0.5f32; 128]);
        ring.process(&input, &mut output, 32.0);

        let active = ring.active_peer_slots();
        let slot_after = active.iter()
            .find(|(_, pid, _)| pid == "peer-a")
            .unwrap()
            .0;

        assert_eq!(slot_after, slot_a,
            "Same peer_id reconnect must stay on the same slot");
        assert_eq!(active.len(), 1, "Only one peer should be tracked");
    }

    // --- Tests: reconnect with stale peer (regression for session eviction fix) ---

    /// Without session-side eviction the old peer's slot stays active, blocking
    /// the reconnecting peer from reclaiming it via affinity.
    ///
    /// This documents the bug at the ring level: if `remove_peer` is NOT called
    /// before the new peer_id arrives, the reconnecting peer ends up on a
    /// different slot.  The fix in session.rs detects this via identity matching
    /// and calls `remove_peer` (sending PeerLeft IPC) before `notify_peer_joined`.
    #[test]
    fn reconnect_without_eviction_inherits_same_slot() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        // peer-a joins and is assigned slot 0 via audio
        ring.notify_peer_joined("peer-a", "identity-alice");
        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.3f32; 128]);
        ring.process(&input, &mut output, 16.0);

        let slot_a = ring.active_peer_slots()
            .iter()
            .find(|(_, pid, _)| pid == "peer-a")
            .unwrap()
            .0;

        // peer-a-new arrives with the same identity — even without explicit
        // remove_peer("peer-a"), the ClientChannelMapping is the same so it
        // maps to the same slot. The new peer_id takes over the slot.
        ring.notify_peer_joined("peer-a-new", "identity-alice");
        ring.feed_remote("peer-a-new".into(), 0, 1, vec![0.5f32; 128]);
        ring.process(&input, &mut output, 32.0);

        let active = ring.active_peer_slots();
        let slot_new = active.iter()
            .find(|(_, pid, _)| pid == "peer-a-new")
            .unwrap()
            .0;

        // Same ClientChannelMapping → same slot, new peer_id takes over
        assert_eq!(slot_new, slot_a,
            "Same identity maps to same slot via ClientChannelMapping");
    }

    /// With session-side eviction the sequence is: remove_peer(old) →
    /// notify_peer_joined(new, same identity).  The ring restores the original
    /// slot via affinity.  This is the behaviour guaranteed by the fix in
    /// session.rs that calls remove_peer when it detects two peer_ids sharing
    /// the same identity.
    #[test]
    fn eviction_before_reconnect_reclaims_slot() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        // peer-a joins and is assigned slot 0
        ring.notify_peer_joined("peer-a", "identity-alice");
        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.3f32; 128]);
        ring.process(&input, &mut output, 16.0);

        let slot_a = ring.active_peer_slots()
            .iter()
            .find(|(_, pid, _)| pid == "peer-a")
            .unwrap()
            .0;

        // Session eviction: PeerLeft IPC → remove_peer; PeerJoined IPC → notify_peer_joined
        ring.remove_peer("peer-a");
        ring.notify_peer_joined("peer-a-new", "identity-alice");

        ring.feed_remote("peer-a-new".into(), 0, 1, vec![0.5f32; 128]);
        ring.process(&input, &mut output, 32.0);

        let active = ring.active_peer_slots();
        let slot_new = active.iter()
            .find(|(_, pid, _)| pid == "peer-a-new")
            .unwrap()
            .0;

        assert_eq!(slot_new, slot_a,
            "After eviction the reconnecting peer reclaims its original slot");
        assert_eq!(active.len(), 1);
    }

    /// Two peers are active. One peer reconnects with a new peer_id (old slot
    /// occupied).  The session evicts the stale entry.  The reconnecting peer
    /// reclaims their original slot; the other peer is unaffected.
    #[test]
    fn eviction_with_bystander_peer_unaffected() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        ring.notify_peer_joined("peer-a", "identity-alice");
        ring.notify_peer_joined("peer-b", "identity-bob");
        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.3f32; 128]);
        ring.feed_remote("peer-b".into(), 0, 0, vec![0.7f32; 128]);
        ring.process(&input, &mut output, 16.0);

        let slot_a = ring.active_peer_slots()
            .iter()
            .find(|(_, pid, _)| pid == "peer-a")
            .unwrap()
            .0;
        let slot_b = ring.active_peer_slots()
            .iter()
            .find(|(_, pid, _)| pid == "peer-b")
            .unwrap()
            .0;
        assert_ne!(slot_a, slot_b);

        // Session evicts peer-a (reconnect detected) and registers new peer_id
        ring.remove_peer("peer-a");
        ring.notify_peer_joined("peer-a-new", "identity-alice");

        ring.feed_remote("peer-a-new".into(), 0, 1, vec![0.5f32; 128]);
        ring.feed_remote("peer-b".into(), 0, 1, vec![0.7f32; 128]);
        ring.process(&input, &mut output, 32.0);

        let active = ring.active_peer_slots();
        let new_a = active.iter().find(|(_, pid, _)| pid == "peer-a-new").unwrap().0;
        let new_b = active.iter().find(|(_, pid, _)| pid == "peer-b").unwrap().0;

        assert_eq!(new_a, slot_a, "peer-a-new reclaims peer-a's original slot");
        assert_eq!(new_b, slot_b, "peer-b is unaffected by peer-a's reconnect");
    }

    /// Audio arriving before identity is known should not leak a slot when
    /// notify_peer_joined later provides the real identity.
    #[test]
    fn audio_before_identity_rekeys_slot() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        // Audio arrives before Hello — identity unknown, slot assigned under peer_id
        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a".to_string(), 0, 0, vec![0.3f32; 128]);
        ring.process(&input, &mut output, 16.0);

        let slot_before = ring.active_peer_slots()
            .iter()
            .find(|(_, pid, _)| pid == "peer-a")
            .unwrap()
            .0;

        // Hello arrives — identity becomes known
        ring.notify_peer_joined("peer-a", "identity-alice");

        // Next audio uses the real identity key
        ring.feed_remote("peer-a".to_string(), 0, 1, vec![0.5f32; 128]);
        ring.process(&input, &mut output, 32.0);

        let active = ring.active_peer_slots();
        let slot_after = active.iter()
            .find(|(_, pid, _)| pid == "peer-a")
            .unwrap()
            .0;

        // Same slot — no double-allocation
        assert_eq!(slot_before, slot_after,
            "Slot should be re-keyed, not duplicated");
        assert_eq!(active.len(), 1, "Only one active slot should exist");
    }
}
