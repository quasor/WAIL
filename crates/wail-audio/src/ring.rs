use crate::slot::{ClientChannelMapping, SlotTable, MAX_SLOTS};

/// Maximum number of remote peer-stream slots with independent audio channels.
pub const MAX_REMOTE_PEERS: usize = MAX_SLOTS;

/// Crossfade overlap window in interleaved samples (both channels).
/// 128 per channel × 2 = 256 interleaved — matches NINJAM's MAX_FADE=128 per channel.
/// At 48 kHz stereo this is ~2.7 ms, just above Opus's 2.5 ms algorithmic delay.
const XFADE_SAMPLES: usize = 256;

/// Per-peer-stream isolated playback slot.
pub struct PeerSlot {
    pub peer_id: String,
    pub stream_id: u16,
    pub samples: Vec<f32>,
    pub active: bool,
    read_pos: usize,
    /// Tail of the previous interval's audio for equal-power crossfade blending.
    /// All zeros on a new or reconnected peer, which produces a fade-in from silence.
    crossfade_tail: [f32; XFADE_SAMPLES],
}

impl PeerSlot {
    fn new() -> Self {
        Self {
            peer_id: String::new(),
            stream_id: 0,
            samples: Vec::new(),
            active: false,
            read_pos: 0,
            crossfade_tail: [0.0; XFADE_SAMPLES],
        }
    }

    fn clear(&mut self) {
        self.samples.clear();
        self.active = false;
        self.read_pos = 0;
        self.crossfade_tail = [0.0; XFADE_SAMPLES];
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
    /// The interval index whose audio is currently in the playback slot.
    /// Set during swap_intervals() so that late-arriving frames for this
    /// index can be appended directly instead of waiting for the next swap.
    playback_interval: Option<i64>,
    /// Audio parameters (retained for future use in resampling/diagnostics)
    #[allow(dead_code)]
    sample_rate: u32,
    #[allow(dead_code)]
    channels: u16,
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
            playback_interval: None,
            sample_rate,
            channels,
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
                // First process call — start recording.
                // Set playback_interval so feed_remote() can live-append
                // frames for this interval immediately, instead of queuing
                // them in pending_remote until the first boundary swap.
                self.playback_interval = Some(interval_index);
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
    /// If the audio matches the currently playing interval, it is appended
    /// directly to the active playback slot (live append) so that streaming
    /// frames arriving after the swap don't cause silence gaps. Otherwise,
    /// it is queued in `pending_remote` for the next interval boundary.
    ///
    /// Multiple peers' audio is summed together. Each unique `(peer_id, stream_id)`
    /// pair gets its own isolated slot for per-stream DAW routing.
    pub fn feed_remote(&mut self, peer_id: String, stream_id: u16, interval_index: i64, samples: Vec<f32>) {
        // Live append: if this frame is for the interval currently being played
        // back, append directly to the playback slot instead of waiting for the
        // next swap. This eliminates the "2 bars sound, 2 bars silence" dropout
        // caused by streaming frames arriving after the boundary swap.
        if self.playback_interval == Some(interval_index) {
            // Live append: use the per-peer slot's length as the write cursor
            // so multiple peers sum correctly at the same playback position.
            if let Some(slot_idx) = self.find_peer_slot(&peer_id, stream_id) {
                let peer_pos = self.peer_slots[slot_idx].samples.len();
                let max_write = self.playback_slot.len().saturating_sub(peer_pos);
                let write_len = samples.len().min(max_write);
                if write_len > 0 {
                    let new_end = peer_pos + write_len;

                    // Extend playback_len if this peer writes beyond current end.
                    // Zero-clear the extension so the first peer to reach a region
                    // doesn't sum with stale data.
                    if new_end > self.playback_len {
                        for s in &mut self.playback_slot[self.playback_len..new_end] {
                            *s = 0.0;
                        }
                        self.playback_len = new_end;
                    }

                    // Sum into the shared playback slot
                    for (i, &s) in samples[..write_len].iter().enumerate() {
                        self.playback_slot[peer_pos + i] += s;
                    }

                    // Extend the per-peer slot for isolated routing
                    self.peer_slots[slot_idx].samples.extend_from_slice(&samples[..write_len]);
                }
                return;
            }
            // Peer slot not yet assigned — fall through to pending_remote.
            // This happens for the very first frame before the peer is known.
        }

        // Accumulate into existing entry for the same (peer_id, stream_id, interval_index)
        // to support incremental per-frame decode without creating hundreds of entries.
        if let Some(existing) = self.pending_remote.iter_mut().find(|r| {
            r.peer_id == peer_id && r.stream_id == stream_id && r.index == interval_index
        }) {
            existing.samples.extend_from_slice(&samples);
            return;
        }
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
        self.playback_interval = None;
        self.completed.clear();
        self.pending_remote.clear();
        for slot in &mut self.peer_slots {
            slot.clear();
        }
        self.slot_table.clear();
        self.peer_identity_map.clear();
    }

    /// Reset interval tracking and buffer positions without clearing peer state.
    ///
    /// Use this on transport restart: beat position is about to jump, so interval
    /// tracking and read/write positions must start fresh. Peer slot assignments,
    /// identity mappings, and pending remote audio are preserved.
    pub fn reset_transport(&mut self) {
        self.record_slot.clear();
        self.record_pos = 0;
        self.playback_pos = 0;
        self.playback_len = 0;
        self.current_interval = None;
        self.playback_interval = None;
        self.completed.clear();
        for slot in &mut self.peer_slots {
            slot.read_pos = 0;
        }
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
                self.peer_slots[slot_idx].crossfade_tail = [0.0; XFADE_SAMPLES];
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

    /// Find an existing active peer slot (for live-append). Does not allocate.
    fn find_peer_slot(&self, peer_id: &str, stream_id: u16) -> Option<usize> {
        self.peer_slots.iter().enumerate().find(|(_, s)| {
            s.active && s.peer_id == peer_id && s.stream_id == stream_id
        }).map(|(i, _)| i)
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
                slot.crossfade_tail = [0.0; XFADE_SAMPLES];
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

        // Capture crossfade tails from outgoing audio before clearing slots.
        // The tail of each active peer's previous interval will be blended with
        // the head of the new interval to prevent clicks at interval boundaries.
        for slot in &mut self.peer_slots {
            if slot.active && !slot.samples.is_empty() {
                // Capture the last XFADE_SAMPLES of the outgoing interval, left-aligned.
                // The crossfade loop reads tail[0..fade_len], so the captured audio must
                // start at index 0.
                let src_len = slot.samples.len().min(XFADE_SAMPLES);
                let src_start = slot.samples.len() - src_len;
                for j in 0..src_len {
                    slot.crossfade_tail[j] = slot.samples[src_start + j];
                }
                // Zero the remainder (if the interval was shorter than XFADE_SAMPLES)
                slot.crossfade_tail[src_len..].fill(0.0);
            }
            // Inactive or empty slots: crossfade_tail stays zero → fade from silence
        }

        // Clear per-peer slots (but keep assignments and crossfade tails)
        for slot in &mut self.peer_slots {
            slot.samples.clear();
            slot.read_pos = 0;
        }

        // Mix pending remote intervals into pre-allocated playback slot
        self.playback_pos = 0;
        self.playback_len = 0;

        // Capture previous playback interval BEFORE updating — entries for
        // the outgoing interval may be in pending_remote if the peer slot
        // wasn't assigned during live-append (first audio from a new peer).
        let prev_playback = self.playback_interval;

        // Track the interval being played so late-arriving frames can append live.
        self.playback_interval = Some(completed_index);
        let mut pending = std::mem::take(&mut self.pending_remote);
        let mut keep = Vec::new();
        let pending_count = pending.len();
        let mut mixed_count = 0usize;
        for mut remote in pending.drain(..) {
            if remote.index != completed_index && Some(remote.index) != prev_playback {
                keep.push(remote);
                continue;
            }
            mixed_count += 1;
            // Assign slot FIRST so we can check needs_fade_in before summing
            let slot_assignment = self.assign_peer_slot(&remote.peer_id, remote.stream_id);

            // Apply equal-power crossfade at interval boundary.
            // Blends the tail of the previous interval (fading out) with the head of
            // the new interval (fading in). When crossfade_tail is all zeros (new peer
            // or reconnect), this naturally produces a clean fade-in from silence.
            if let Some(slot_idx) = slot_assignment {
                let fade_len = XFADE_SAMPLES.min(remote.samples.len());
                let tail = self.peer_slots[slot_idx].crossfade_tail;
                for i in 0..fade_len {
                    let t = (i + 1) as f32 / fade_len as f32;
                    let new_w = (t * std::f32::consts::FRAC_PI_2).sin();
                    let old_w = (t * std::f32::consts::FRAC_PI_2).cos();
                    remote.samples[i] = remote.samples[i] * new_w + tail[i] * old_w;
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
        // Put back entries for future intervals, plus the drained vec
        keep.extend(pending.drain(..));
        self.pending_remote = keep;

        // Diagnostic: log boundary swap details to identify gap root cause
        let kept_count = self.pending_remote.len();
        let active_peers: Vec<_> = self.peer_slots.iter()
            .filter(|s| s.active)
            .map(|s| {
                let tail_nonzero = s.crossfade_tail.iter().any(|&v| v != 0.0);
                format!("{}:{} len={} tail={}", s.peer_id, s.stream_id, s.samples.len(), if tail_nonzero { "audio" } else { "zero" })
            })
            .collect();
        tracing::info!(
            completed_index = completed_index,
            pending_count = pending_count,
            mixed_count = mixed_count,
            kept_for_future = kept_count,
            playback_len = self.playback_len,
            peers = ?active_peers,
            "INTERVAL SWAP"
        );
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

    /// Crossfade window length — mirrors the XFADE_SAMPLES constant for test assertions.
    const XFADE_LEN: usize = XFADE_SAMPLES;

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
        let buf = XFADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        // Start in interval 0
        ring.process(&input, &mut output, 0.0);

        // Feed remote audio for next playback
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.7f32; buf]);

        // Cross into interval 1 — remote audio should become playback
        ring.process(&input, &mut output, 16.0);

        // First sample near-zero: equal-power crossfade from silence
        assert!(output[0] < 0.02, "First sample should be near zero, got: {}", output[0]);
        // Post-fade region should contain the remote audio at full amplitude
        assert!(output[XFADE_LEN..].iter().all(|&s| (s - 0.7).abs() < f32::EPSILON),
            "Output should be remote audio after fade-in, got: {:?}", &output[XFADE_LEN..XFADE_LEN+4]);
    }

    #[test]
    fn mixes_multiple_remote_peers() {
        let mut ring = make_ring();
        let buf = XFADE_LEN + 64;
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
        assert!(output[XFADE_LEN..].iter().all(|&s| (s - 0.8).abs() < 0.001),
            "Expected 0.3 + 0.5 = 0.8 after fade, got: {:?}", &output[XFADE_LEN..XFADE_LEN+4]);
    }

    #[test]
    fn remote_audio_longer_than_buffer_spans_calls() {
        let mut ring = make_ring();
        let buf = XFADE_LEN / 2;
        let remote_len = XFADE_LEN + buf * 2;
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

        // First sample near-zero: equal-power crossfade from silence, sin(1/32·π/2) ≈ 0.049
        assert!(output[0] < 0.1, "First sample should be near zero (faded), got: {}", output[0]);
        // Last sample (i=31, t=1.0): new_w = sin(π/2) = 1.0 → output = 0.5 * 1.0 = 0.5
        assert!((output[31] - 0.5).abs() < 0.01,
            "Last audio sample should be ~0.5, got: {}", output[31]);
        // Rest = silence
        assert_eq!(output[32], 0.0);
        assert_eq!(output[63], 0.0);
    }

    // --- Test: Multiple intervals ---

    #[test]
    fn multiple_interval_cycle() {
        let mut ring = make_ring();
        let buf = XFADE_LEN + 64;
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
        assert!((output[XFADE_LEN] - 0.9).abs() < f32::EPSILON);

        // Feed new remote for interval 2
        ring.feed_remote("peer-a".into(), 0, 1, vec![0.6f32; buf]);

        // Interval 2: record ones, play new remote
        ring.process(&ones, &mut output, 32.0);
        let completed = ring.take_completed();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].index, 1);
        // Completed interval 1 should contain twos
        assert!((completed[0].samples[0] - 2.0).abs() < f32::EPSILON);
        // Second interval crossfades from old tail (0.9) to new (0.6).
        // Post-crossfade should be pure new audio.
        assert!((output[XFADE_LEN] - 0.6).abs() < f32::EPSILON);
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

    // --- Test: Partial feed before swap causes truncated playback ---
    //
    // Simulates real-time streaming where the sender's audio arrives as
    // incremental 20ms decoded chunks. If only half the interval's audio
    // has arrived when the receiver crosses the boundary, only that half
    // plays — the rest is silence ("2 bars sound, 2 bars silence").

    #[test]
    fn partial_feed_before_swap_causes_silence_in_second_half() {
        let mut ring = make_ring();
        let buf = 256; // small process buffer
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        // Start interval 0
        ring.process(&input, &mut output, 0.0);

        // Simulate: only HALF an interval's audio arrives before the swap.
        // Full interval at 48kHz stereo, 4 bars, 120 BPM = 768,000 samples.
        // Feed only the first half.
        let full_interval_samples = 768_000;
        let half = full_interval_samples / 2;
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.5f32; half]);

        // Cross into interval 1 — swap_intervals() mixes pending_remote
        ring.process(&input, &mut output, 16.0);

        // Playback should have audio (first buffer is in crossfade region)
        let energy: f32 = output.iter().map(|s| s * s).sum::<f32>().sqrt();
        assert!(energy > 0.0, "First buffer after swap should have audio");

        // Read through the first half — should have audio
        let mut audio_samples_read = buf;
        let beats_per_sample = 120.0 / 60.0 / 48000.0 * 0.5; // stereo: 2 samples per frame
        let mut beat = 16.0 + (buf as f64 * beats_per_sample);

        while audio_samples_read < half {
            ring.process(&input, &mut output, beat);
            let energy: f32 = output.iter().map(|s| s * s).sum::<f32>().sqrt();
            assert!(energy > 0.01, "Should have audio at sample offset {audio_samples_read}");
            audio_samples_read += buf;
            beat += buf as f64 * beats_per_sample;
        }

        // Now read past the half-way point — should be SILENCE
        // (this is the bug: playback_len was only half the interval)
        ring.process(&input, &mut output, beat);
        let silence_energy: f32 = output.iter().map(|s| s * s).sum::<f32>().sqrt();
        assert!(
            silence_energy < f32::EPSILON,
            "Expected silence after partial feed exhausted, got energy={silence_energy}"
        );
    }

    #[test]
    fn incremental_feed_with_network_latency() {
        // Simulates real-time streaming with network latency. The sender
        // streams 20ms Opus frames during interval N, but they arrive at
        // the receiver with a delay. This means some of interval N's frames
        // arrive after the receiver has already crossed the N→N+1 boundary.
        //
        // Without live-append, those late frames are queued for the N+1→N+2
        // swap, causing ~50% silence per interval ("2 bars sound, 2 bars
        // silence"). With live-append, late frames extend the active playback
        // slot, providing continuous audio.
        //
        // Setup: 48kHz stereo, buffer=256, 4 bars @ 120 BPM = 8s intervals.
        // Network latency: ~200ms → ~75 callbacks worth of delay.

        let mut ring = make_ring();
        let buf = 256;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];
        let frame_size = 1920; // 20ms Opus frame, stereo

        let callbacks_per_interval = 768_000 / buf; // 3000
        let callbacks_per_frame = 8; // ~one frame per 8 callbacks (20ms / 2.67ms)
        // Network latency: frames arrive ~75 callbacks late (~200ms).
        // This pushes ~10% of each interval's frames past the swap boundary.
        let latency_callbacks: usize = 75;

        // Use a queue to simulate delayed frame delivery.
        // Each entry: (delivery_callback, interval_index, samples)
        let mut frame_queue: std::collections::VecDeque<(usize, i64, Vec<f32>)> =
            std::collections::VecDeque::new();

        let total_intervals = 4;
        let mut interval_audio_pcts: Vec<f64> = Vec::new();
        let mut global_cb: usize = 0;

        ring.process(&input, &mut output, 0.0);
        global_cb += 1;

        for interval in 0..total_intervals {
            let base_beat = interval as f64 * 16.0;
            let mut audio_buffers = 0u32;
            let mut silent_buffers = 0u32;
            let mut frames_fed = 0u32;

            for cb in 1..=callbacks_per_interval {
                let beat = base_beat + cb as f64 * buf as f64 * (120.0 / 60.0 / 48000.0 * 0.5);

                // Schedule a frame with network latency
                if cb % callbacks_per_frame == 0 {
                    frame_queue.push_back((
                        global_cb + latency_callbacks,
                        interval as i64,
                        vec![0.5f32; frame_size],
                    ));
                }

                // Deliver any frames whose latency has elapsed
                while let Some(&(delivery_cb, _, _)) = frame_queue.front() {
                    if delivery_cb <= global_cb {
                        let (_, idx, samples) = frame_queue.pop_front().unwrap();
                        ring.feed_remote("peer-a".into(), 0, idx, samples);
                        frames_fed += 1;
                    } else {
                        break;
                    }
                }

                ring.process(&input, &mut output, beat);
                global_cb += 1;

                if output.iter().any(|&s| s.abs() > 0.001) {
                    audio_buffers += 1;
                } else {
                    silent_buffers += 1;
                }
            }

            // Cross into next interval
            let next_beat = (interval as f64 + 1.0) * 16.0;

            // Deliver queued frames before the swap callback
            while let Some(&(delivery_cb, _, _)) = frame_queue.front() {
                if delivery_cb <= global_cb {
                    let (_, idx, samples) = frame_queue.pop_front().unwrap();
                    ring.feed_remote("peer-a".into(), 0, idx, samples);
                    frames_fed += 1;
                } else {
                    break;
                }
            }

            ring.process(&input, &mut output, next_beat);
            global_cb += 1;
            if output.iter().any(|&s| s.abs() > 0.001) {
                audio_buffers += 1;
            } else {
                silent_buffers += 1;
            }

            let total = audio_buffers + silent_buffers;
            let pct = audio_buffers as f64 / total as f64 * 100.0;
            interval_audio_pcts.push(pct);
            eprintln!(
                "[test]   Interval {interval}: {audio_buffers}/{total} audio ({pct:.0}%), fed {frames_fed} frames"
            );
        }

        // Interval 0 has no playback (pipeline warmup). Interval 1 may have
        // reduced coverage. By interval 2, live-append should provide >90%.
        let steady_state_pct = interval_audio_pcts[2..].iter().copied()
            .min_by(|a, b| a.partial_cmp(b).unwrap())
            .unwrap_or(0.0);

        eprintln!("[test] Steady-state min coverage (intervals 2+): {steady_state_pct:.0}%");

        assert!(
            steady_state_pct > 90.0,
            "Expected >90% audio coverage in steady state (intervals 2+), \
             got {steady_state_pct:.0}%. Late-arriving frames should be \
             live-appended to the active playback slot."
        );
    }

    #[test]
    fn late_arriving_audio_after_swap_is_live_appended() {
        // Verifies that audio fed AFTER a swap for the currently playing
        // interval is live-appended to the playback slot immediately,
        // rather than waiting for the next swap (which caused silence gaps).
        let mut ring = make_ring();
        let buf = XFADE_LEN + 128;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        // Interval 0
        ring.process(&input, &mut output, 0.0);

        // Feed first half before swap
        let half = 384_000;
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.5f32; half]);

        // Cross into interval 1 — first half mixed into playback
        ring.process(&input, &mut output, 16.0);
        let initial_remaining = ring.playback_remaining();
        assert!(initial_remaining > 0, "Should have partial playback");

        // Feed second half AFTER the swap (tagged as interval 0 = currently playing).
        // With live-append, this should extend the playback slot directly.
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.5f32; half]);

        // Playback should have grown — NOT queued in pending_remote
        assert_eq!(
            ring.pending_remote_count(), 0,
            "Late audio should be live-appended, not queued in pending_remote"
        );
        assert!(
            ring.playback_remaining() > initial_remaining,
            "Playback should have grown after live append"
        );

        // Read through — second half should produce audio, not silence
        let mut found_audio_after_half = false;
        let mut beat = 16.5;
        for _ in 0..(half / buf + 10) {
            ring.process(&input, &mut output, beat);
            beat += 0.01;
            if output.iter().any(|&s| s.abs() > 0.001) {
                found_audio_after_half = true;
            }
        }
        assert!(found_audio_after_half, "Live-appended audio should be audible");
    }

    // --- Test: Per-peer playback slots ---

    #[test]
    fn per_peer_playback_slots() {
        let mut ring = make_ring();
        let buf = XFADE_LEN + 64;
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
            slot_a_out[XFADE_LEN..].iter().all(|&s| (s - 0.3).abs() < f32::EPSILON),
            "Peer A slot should be 0.3 after fade, got: {:?}", &slot_a_out[XFADE_LEN..XFADE_LEN+4]
        );
        // Post-fade: Peer B's slot should have 0.7
        assert!(
            slot_b_out[XFADE_LEN..].iter().all(|&s| (s - 0.7).abs() < f32::EPSILON),
            "Peer B slot should be 0.7 after fade, got: {:?}", &slot_b_out[XFADE_LEN..XFADE_LEN+4]
        );
    }

    #[test]
    fn per_peer_and_summed_mix_consistent() {
        let mut ring = make_ring();
        let buf = XFADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        ring.process(&input, &mut output, 0.0);

        ring.feed_remote("peer-x".into(), 0, 0, vec![0.2f32; buf]);
        ring.feed_remote("peer-y".into(), 0, 0, vec![0.5f32; buf]);

        // Cross boundary
        ring.process(&input, &mut output, 16.0);

        // Post-fade: summed mix should be 0.2 + 0.5 = 0.7
        assert!(
            output[XFADE_LEN..].iter().all(|&s| (s - 0.7).abs() < 0.001),
            "Summed mix should be 0.7 after fade, got: {:?}", &output[XFADE_LEN..XFADE_LEN+4]
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

        for (i, &s) in sum.iter().enumerate().skip(XFADE_LEN) {
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
        ring.feed_remote("peer-a-new".into(), 0, 2, vec![0.5f32; 128]);
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
        let buf = XFADE_LEN + 64;
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
        assert!(s0_out[XFADE_LEN..].iter().all(|&s| (s - 0.3).abs() < f32::EPSILON));
        assert!(s1_out[XFADE_LEN..].iter().all(|&s| (s - 0.7).abs() < f32::EPSILON));

        // Summed playback should be 0.3 + 0.7 = 1.0 (post-fade)
        assert!(output[XFADE_LEN..].iter().all(|&s| (s - 1.0).abs() < 0.001),
            "Summed mix should be 1.0 after fade, got: {:?}", &output[XFADE_LEN..XFADE_LEN+4]);
    }

    #[test]
    fn slot_exhaustion_merges_to_stream_0() {
        let mut ring = make_ring();
        let buf = XFADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        ring.process(&input, &mut output, 0.0);

        // Fill all 15 slots with distinct peer-streams
        // Peer-a stream 0 is at slot 0 (this is the merge target)
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.1f32; buf]);
        for i in 1..MAX_REMOTE_PEERS {
            let peer = format!("peer-fill-{i}");
            ring.feed_remote(peer, 0, 0, vec![0.01f32; buf]);
        }

        // 16th stream should overflow — merge into peer-a's stream 0
        ring.feed_remote("peer-a".into(), 5, 0, vec![0.5f32; buf]);

        ring.process(&input, &mut output, 16.0);

        // Should still have exactly 15 active slots (no new slot for overflow)
        let active = ring.active_peer_slots();
        assert_eq!(active.len(), MAX_REMOTE_PEERS);

        // peer-a stream 0 should contain merged audio post-fade
        // stream 0 (0.1) is faded, overflow stream 5 (0.5) is unfaded (no slot assigned)
        // After fade region: faded(0.1) converges to 0.1, so total = 0.1 + 0.5 = 0.6
        let (s0_idx, _, _) = active.iter().find(|(_, pid, sid)| pid == "peer-a" && *sid == 0).unwrap();
        let mut s0_out = vec![0.0f32; buf];
        ring.read_peer_playback(*s0_idx, &mut s0_out);
        assert!(
            s0_out[XFADE_LEN..].iter().all(|&s| (s - 0.6).abs() < 0.01),
            "Overflowed stream should merge into stream 0 (post-fade), got: {:?}",
            &s0_out[XFADE_LEN..XFADE_LEN+4]
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
        let buf = XFADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        ring.process(&input, &mut output, 0.0);

        // Feed constant-amplitude audio from a new peer
        ring.feed_remote("peer-a".into(), 0, 0, vec![1.0f32; buf]);
        ring.process(&input, &mut output, 16.0);

        // First sample near-zero: equal-power from silence, sin(1/XFADE_LEN·π/2) is small
        assert!(output[0] < 0.02,
            "First sample should be near 0.0 (faded), got: {}", output[0]);

        // Mid-fade: equal-power formula, not linear
        let mid = XFADE_LEN / 2;
        let expected_mid = ((mid + 1) as f32 / XFADE_LEN as f32 * std::f32::consts::FRAC_PI_2).sin();
        assert!((output[mid] - expected_mid).abs() < 0.01,
            "Mid-fade sample should be ~{expected_mid:.3} (sin curve), got: {}", output[mid]);

        // Post-fade should be full amplitude
        assert!((output[XFADE_LEN] - 1.0).abs() < f32::EPSILON,
            "Post-fade sample should be 1.0, got: {}", output[XFADE_LEN]);

        // Per-peer slot should match the summed output
        let active = ring.active_peer_slots();
        let (slot_idx, _, _) = active.iter().find(|(_, pid, _)| pid == "peer-a").unwrap();
        let mut peer_out = vec![0.0f32; buf];
        ring.read_peer_playback(*slot_idx, &mut peer_out);

        assert!(peer_out[0] < 0.02,
            "Per-peer first sample should be near 0.0, got: {}", peer_out[0]);
        assert!((peer_out[XFADE_LEN] - 1.0).abs() < f32::EPSILON,
            "Per-peer post-fade sample should be 1.0, got: {}", peer_out[XFADE_LEN]);
    }

    #[test]
    fn crossfades_between_successive_intervals() {
        let mut ring = make_ring();
        let buf = XFADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        ring.process(&input, &mut output, 0.0);

        // First interval from new peer — fades in from silence
        ring.feed_remote("peer-a".into(), 0, 0, vec![1.0f32; buf]);
        ring.process(&input, &mut output, 16.0);
        assert!(output[0] < 0.02, "First interval should start near zero, got: {}", output[0]);

        // Feed second interval from same peer (old tail = 1.0)
        ring.feed_remote("peer-a".into(), 0, 1, vec![0.8f32; buf]);
        ring.process(&input, &mut output, 32.0);

        // At the boundary: old tail blends into new audio (0.8).
        // First sample should differ from pure new audio (crossfade is active).
        // Post-crossfade sample is pure new audio.
        assert!((output[0] - 0.8).abs() > 0.001, "Start of crossfade should blend, not pass through");
        assert!((output[XFADE_LEN] - 0.8).abs() < f32::EPSILON,
            "Post-crossfade should be pure new audio (0.8), got: {}", output[XFADE_LEN]);
    }

    #[test]
    fn fade_in_applied_on_reconnect_via_affinity() {
        let mut ring = make_ring();
        let buf = XFADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        ring.notify_peer_joined("peer-a", "identity-alice");
        ring.process(&input, &mut output, 0.0);

        // First interval — faded
        ring.feed_remote("peer-a".into(), 0, 0, vec![1.0f32; buf]);
        ring.process(&input, &mut output, 16.0);

        // Second interval — crossfades from 1.0 to 1.0 (same amplitude, effectively seamless)
        ring.feed_remote("peer-a".into(), 0, 1, vec![1.0f32; buf]);
        ring.process(&input, &mut output, 32.0);
        assert!((output[XFADE_LEN] - 1.0).abs() < f32::EPSILON,
            "Post-crossfade should be 1.0, got: {}", output[XFADE_LEN]);

        // Disconnect
        ring.remove_peer("peer-a");

        // Reconnect with new peer_id, same identity
        ring.notify_peer_joined("peer-a-new", "identity-alice");

        // First interval after reconnect — should be faded again
        ring.feed_remote("peer-a-new".into(), 0, 2, vec![1.0f32; buf]);
        ring.process(&input, &mut output, 48.0);

        assert!(output[0] < 0.02,
            "First sample after reconnect should be near 0.0 (faded), got: {}", output[0]);
        assert!((output[XFADE_LEN] - 1.0).abs() < f32::EPSILON,
            "Post-fade should be 1.0, got: {}", output[XFADE_LEN]);
    }

    #[test]
    fn fade_in_summed_mix_consistency() {
        let mut ring = make_ring();
        let buf = XFADE_LEN + 64;
        let input = vec![0.0f32; buf];
        let mut output = vec![0.0f32; buf];

        ring.process(&input, &mut output, 0.0);

        // Two new peers, both should be faded
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.5f32; buf]);
        ring.feed_remote("peer-b".into(), 0, 0, vec![0.5f32; buf]);

        ring.process(&input, &mut output, 16.0);

        // First sample: both peers crossfade from silence → sum near zero
        assert!(output[0] < 0.02,
            "Summed first sample should be near 0.0, got: {}", output[0]);

        // After fade: 0.5 + 0.5 = 1.0
        assert!((output[XFADE_LEN] - 1.0).abs() < f32::EPSILON,
            "Post-fade summed should be 1.0, got: {}", output[XFADE_LEN]);

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

        // Feed only 32 samples (much shorter than XFADE_LEN=960)
        ring.feed_remote("peer-a".into(), 0, 0, vec![1.0f32; 32]);
        ring.process(&input, &mut output, 16.0);

        // Should not panic; first sample near zero (sin(1/32·π/2) ≈ 0.098)
        assert!(output[0] < 0.15, "First sample should be near zero, got: {}", output[0]);
        // Last audio sample (i=31, t=1.0): sin(π/2) = 1.0, so output = 1.0 * 1.0 = 1.0
        assert!((output[31] - 1.0).abs() < 0.01,
            "Last sample should be ~1.0, got: {}", output[31]);
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

    // --- Test: feed_remote accumulation ---

    #[test]
    fn feed_remote_accumulates_same_peer_stream_interval() {
        let mut ring = make_ring();

        // Feed three chunks for the same (peer, stream, interval)
        ring.feed_remote("peer-a".into(), 0, 1, vec![0.1f32; 100]);
        ring.feed_remote("peer-a".into(), 0, 1, vec![0.2f32; 200]);
        ring.feed_remote("peer-a".into(), 0, 1, vec![0.3f32; 50]);

        // Should produce exactly 1 pending entry, not 3
        assert_eq!(ring.pending_remote_count(), 1);
    }

    #[test]
    fn feed_remote_different_keys_stay_separate() {
        let mut ring = make_ring();

        // Different peer
        ring.feed_remote("peer-a".into(), 0, 1, vec![0.1f32; 100]);
        ring.feed_remote("peer-b".into(), 0, 1, vec![0.2f32; 100]);

        // Different stream_id
        ring.feed_remote("peer-a".into(), 1, 1, vec![0.3f32; 100]);

        // Different interval_index
        ring.feed_remote("peer-a".into(), 0, 2, vec![0.4f32; 100]);

        assert_eq!(ring.pending_remote_count(), 4);
    }

    #[test]
    fn reset_produces_silence_after_active_playback() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 128];
        let mut output = vec![0.0f32; 128];

        // Start interval 0
        ring.process(&input, &mut output, 0.0);

        // Feed remote audio for interval 0
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.5f32; 1920]);

        // Cross into interval 1 — remote audio mixes into playback
        ring.process(&input, &mut output, 16.0);
        let energy: f32 = output.iter().map(|s| s.abs()).sum();
        assert!(energy > 0.0, "Should have audio before reset");

        // Reset clears all state
        ring.reset();

        // Process again — should be silence
        let mut output2 = vec![1.0f32; 128];
        ring.process(&input, &mut output2, 16.5);
        assert!(
            output2.iter().all(|&s| s == 0.0),
            "Expected silence after reset, but got audio"
        );
    }

    // --- Transport restart ---

    #[test]
    fn loop_playback_does_not_reset_positions() {
        let mut ring = make_ring();
        let input = vec![0.5f32; 256];
        let mut output = vec![0.0f32; 256];

        // Advance through a full interval
        ring.process(&input, &mut output, 0.0);
        ring.process(&input, &mut output, 4.0);
        ring.process(&input, &mut output, 8.0);
        ring.process(&input, &mut output, 12.0);

        assert_eq!(ring.current_interval(), Some(0));
        let pos_before = ring.record_position();
        assert!(pos_before > 0, "Should have recorded samples");

        // Simulate DAW loop: beat jumps back to 0 while transport stays playing.
        // This must NOT reset positions — only explicit reset_transport() should.
        ring.process(&input, &mut output, 0.0);

        // Record position should continue accumulating (loop did not reset)
        assert!(
            ring.record_position() > pos_before,
            "Loop boundary should not reset record position"
        );
    }

    #[test]
    fn reset_transport_preserves_peer_slots() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 256];
        let mut output = vec![0.0f32; 256];

        // Start an interval and feed remote audio
        ring.process(&input, &mut output, 0.0);
        ring.feed_remote("peer-a".into(), 0, 0, vec![0.3f32; 512]);

        // Cross boundary so peer slot gets assigned
        ring.process(&input, &mut output, 16.0);

        let peers_before = ring.active_peer_slots();
        assert!(!peers_before.is_empty(), "Should have active peer slots");

        // Reset transport
        ring.reset_transport();

        // Peer slots should still be active
        let peers_after = ring.active_peer_slots();
        assert_eq!(peers_before.len(), peers_after.len());
        assert_eq!(peers_before[0].1, peers_after[0].1, "Peer ID should be preserved");
    }

    #[test]
    fn reset_transport_resets_positions() {
        let mut ring = make_ring();
        let input = vec![0.5f32; 256];
        let mut output = vec![0.0f32; 256];

        ring.process(&input, &mut output, 0.0);
        ring.process(&input, &mut output, 8.0);

        assert!(ring.record_position() > 0);
        assert!(ring.current_interval().is_some());

        ring.reset_transport();

        assert_eq!(ring.record_position(), 0);
        assert_eq!(ring.current_interval(), None);
        assert_eq!(ring.playback_remaining(), 0);
    }

    #[test]
    fn transport_restart_full_playback() {
        let mut ring = make_ring();
        let input = vec![0.0f32; 256];
        let mut output = vec![0.0f32; 256];

        // Process first interval
        ring.process(&input, &mut output, 0.0);

        // Feed remote audio for interval 0
        let remote = vec![0.5f32; 1024];
        ring.feed_remote("peer-a".into(), 0, 0, remote.clone());

        // Cross boundary to interval 1 — remote audio becomes playback
        ring.process(&input, &mut output, 16.0);

        // Read some playback (simulate partial playback before transport stop)
        ring.process(&input, &mut output, 17.0);
        let played_before_stop = output.iter().filter(|&&s| s != 0.0).count();
        assert!(played_before_stop > 0, "Should have played some audio");

        // Simulate transport restart at beat 0
        ring.reset_transport();

        // Feed the same remote audio again for the new interval 0
        ring.feed_remote("peer-a".into(), 0, 0, remote.clone());

        // Start fresh interval
        ring.process(&input, &mut output, 0.0);

        // Cross boundary again
        ring.process(&input, &mut output, 16.0);

        // Now playback should start from the beginning (full audio)
        let mut full_output = vec![0.0f32; 1024];
        ring.process(&input, &mut full_output, 17.0);
        let played_after_restart = full_output.iter().filter(|&&s| s != 0.0).count();
        assert!(
            played_after_restart > 0,
            "Should have full playback after transport restart"
        );
    }
}
