use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use wail_audio::{ClientChannelMapping, SlotTable};

/// All state tracked per remote peer, consolidating 11 separate HashMaps.
pub struct PeerState {
    pub display_name: Option<String>,
    pub identity: Option<String>,
    /// stream_id → slot index in the recv ring buffer
    pub slots: HashMap<u16, usize>,
    pub hello_sent: bool,
    pub last_seen: Instant,
    /// True once any sync or audio message has been received from this peer.
    pub ever_received_message: bool,
    pub audio_recv_count: u64,
    pub audio_recv_prev: u64,
    /// Last `intervals_sent` value from this peer's AudioStatus message.
    pub remote_intervals_sent: u64,
    /// Cumulative frames expected across all assembled intervals from this peer.
    pub total_frames_expected: u64,
    /// Cumulative frames actually received (non-gap) across all assembled intervals.
    pub total_frames_received: u64,
    /// Per-interval high-water mark for expected frame count, so we can incrementally
    /// update `total_frames_expected` as non-final frames arrive (not just on the final).
    pub interval_frames_expected: HashMap<i64, u64>,
    /// Cumulative WAIF frames that arrived for already-passed intervals.
    pub late_frames: u64,
    pub prev_status: String,
    /// Timestamp of the most recent connection attempt for this peer. Used by the
    /// Hello-completion watchdog to detect peers that are active (receiving audio)
    /// but whose Hello handshake never finished. Reset on each reconnect attempt.
    pub added_at: Instant,
    /// True after the Hello-completion watchdog has already sent a soft retry for
    /// this connection attempt. Prevents duplicate Hello re-sends on every tick.
    pub hello_retry_sent: bool,
    /// Remote peer's user-chosen stream names (stream_index → label).
    pub stream_names: HashMap<u16, String>,
}

impl PeerState {
    fn new(display_name: Option<String>) -> Self {
        Self {
            display_name,
            identity: None,
            slots: HashMap::new(),
            hello_sent: false,
            last_seen: Instant::now(),
            ever_received_message: false,
            audio_recv_count: 0,
            audio_recv_prev: 0,
            remote_intervals_sent: 0,
            total_frames_expected: 0,
            total_frames_received: 0,
            interval_frames_expected: HashMap::new(),
            late_frames: 0,
            prev_status: String::new(),
            added_at: Instant::now(),
            hello_retry_sent: false,
            stream_names: HashMap::new(),
        }
    }
}

/// Consolidated registry of all connected peers.
pub struct PeerRegistry {
    peers: HashMap<String, PeerState>,
    slots: SlotTable,
}

impl PeerRegistry {
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
            slots: SlotTable::new(),
        }
    }

    /// Get a peer's state.
    pub fn get(&self, peer_id: &str) -> Option<&PeerState> {
        self.peers.get(peer_id)
    }

    /// Get a peer's state mutably.
    pub fn get_mut(&mut self, peer_id: &str) -> Option<&mut PeerState> {
        self.peers.get_mut(peer_id)
    }

    /// Add a peer entry. If the peer already exists, updates display_name and
    /// refreshes last_seen.
    pub fn add(&mut self, peer_id: String, display_name: Option<String>) {
        self.peers
            .entry(peer_id)
            .and_modify(|p| {
                p.last_seen = Instant::now();
                if display_name.is_some() {
                    p.display_name = display_name.clone();
                }
            })
            .or_insert_with(|| PeerState::new(display_name));
    }

    /// Remove a peer, freeing its slots and creating affinity reservations so
    /// the same peer can reclaim the same slot on rejoin.
    pub fn remove(&mut self, peer_id: &str) {
        if let Some(peer) = self.peers.remove(peer_id) {
            let client_id = peer.identity.as_deref().unwrap_or(peer_id);
            self.slots.release_all_for_client(client_id);
        }
    }

    /// Assign a slot for (peer_id, stream_id). No-op and returns the existing
    /// slot if already assigned. Returns None if all slots are full or the
    /// peer is unknown.
    pub fn assign_slot(&mut self, peer_id: &str, stream_id: u16) -> Option<usize> {
        let peer = self.peers.get(peer_id)?;
        if let Some(&existing) = peer.slots.get(&stream_id) {
            return Some(existing);
        }
        let identity = peer.identity.clone();
        let client_id = identity.as_deref().unwrap_or(peer_id);
        let mapping = ClientChannelMapping::new(client_id, stream_id);
        let slot = self.slots.assign(&mapping)?;
        self.peers.get_mut(peer_id)?.slots.insert(stream_id, slot);
        Some(slot)
    }

    /// Get the slot assigned to (peer_id, stream_id), if any.
    pub fn slot_for(&self, peer_id: &str, stream_id: u16) -> Option<usize> {
        self.peers.get(peer_id)?.slots.get(&stream_id).copied()
    }

    /// Mark Hello as sent to peer_id. Returns true if this is the first time
    /// (i.e. the reply should be sent). Returns false if Hello was already sent.
    pub fn mark_hello_sent(&mut self, peer_id: &str) -> bool {
        match self.peers.get_mut(peer_id) {
            Some(peer) if !peer.hello_sent => {
                peer.hello_sent = true;
                true
            }
            _ => false,
        }
    }

    /// Clear the hello_sent flag, e.g. when the Hello reply failed to send.
    pub fn clear_hello_sent(&mut self, peer_id: &str) {
        if let Some(peer) = self.peers.get_mut(peer_id) {
            peer.hello_sent = false;
        }
    }

    /// Return peer IDs that have previously communicated and whose last_seen is older than `timeout`.
    /// Peers that have never sent a message are excluded.
    pub fn timed_out_peers(&self, timeout: Duration) -> Vec<String> {
        let now = Instant::now();
        self.peers
            .iter()
            .filter(|(_, p)| p.ever_received_message && now.duration_since(p.last_seen) > timeout)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Seed peer names from the signaling join response, adding any new peers
    /// without overwriting existing entries.
    pub fn seed_names(&mut self, names: HashMap<String, Option<String>>) {
        for (peer_id, display_name) in names {
            self.peers
                .entry(peer_id)
                .or_insert_with(|| PeerState::new(display_name));
        }
    }

    /// Seed last_seen = now for all currently known peers. Called on
    /// PeerListReceived to start the liveness watchdog for initial peers.
    pub fn seed_last_seen(&mut self) {
        let now = Instant::now();
        for peer in self.peers.values_mut() {
            peer.last_seen = now;
        }
    }

    /// Access the underlying slot table (for building StatusUpdate).
    pub fn slot_table(&self) -> &SlotTable {
        &self.slots
    }

    /// Update audio_recv_prev = audio_recv_count for all peers. Call once per
    /// status tick after reading audio_recv_count/prev to compute is_receiving.
    pub fn flush_audio_recv_prev(&mut self) {
        for peer in self.peers.values_mut() {
            peer.audio_recv_prev = peer.audio_recv_count;
        }
    }

    /// Re-key all slots assigned under `peer_id` (fallback) to use `identity` (persistent UUID).
    /// Call after setting peer.identity in the Hello handler to fix slots that were assigned
    /// via the audio DataChannel before Hello arrived on the sync DataChannel.
    pub fn rekey_peer_slots(&mut self, peer_id: &str, identity: &str) {
        self.slots.rekey_client(peer_id, identity);
    }

    /// Find the peer_id of a peer with the given identity, if any.
    pub fn find_by_identity(&self, identity: &str) -> Option<String> {
        self.peers
            .iter()
            .find(|(_, p)| p.identity.as_deref() == Some(identity))
            .map(|(id, _)| id.clone())
    }

    /// Return peer IDs that are active (have received messages) but whose Hello
    /// handshake has not completed (identity still unknown).
    ///
    /// Split into two buckets:
    /// - `soft`: elapsed since `added_at` is between `soft_timeout` and `hard_timeout` →
    ///   re-send Hello to prompt the remote to reply
    /// - `hard`: elapsed since `added_at` >= `hard_timeout` → force reconnect
    ///
    pub fn no_identity_active_peers(
        &self,
        soft_timeout: Duration,
        hard_timeout: Duration,
    ) -> (Vec<String>, Vec<String>) {
        let now = Instant::now();
        let mut soft = Vec::new();
        let mut hard = Vec::new();
        for (id, p) in &self.peers {
            if !p.ever_received_message || p.identity.is_some() {
                continue;
            }
            let elapsed = now.duration_since(p.added_at);
            if elapsed >= hard_timeout {
                hard.push(id.clone());
            } else if elapsed >= soft_timeout && !p.hello_retry_sent {
                soft.push(id.clone());
            }
        }
        (soft, hard)
    }

    /// Mark that the Hello-completion watchdog has already sent a soft retry
    /// for this peer, so it won't fire again on subsequent ticks.
    pub fn mark_hello_retry_sent(&mut self, peer_id: &str) {
        if let Some(peer) = self.peers.get_mut(peer_id) {
            peer.hello_retry_sent = true;
        }
    }

    /// Derive a display status string for a peer.
    ///
    /// Returns only stable connection states: connecting, reconnecting, connected.
    /// Audio flow direction is exposed separately via PeerInfo::is_sending/is_receiving.
    pub fn derive_status(&self, peer_id: &str) -> &'static str {
        let peer = match self.peers.get(peer_id) {
            Some(p) => p,
            None => return "connecting",
        };
        if peer.display_name.is_none() {
            "connecting"
        } else {
            "connected"
        }
    }

}

/// Pool of TCP write halves for active recv plugin connections.
/// Handles broadcast with automatic cleanup of dead connections.
pub struct IpcWriterPool {
    writers: Vec<(usize, OwnedWriteHalf)>,
}

impl IpcWriterPool {
    pub fn new() -> Self {
        Self {
            writers: Vec::new(),
        }
    }

    /// Add a new recv plugin connection.
    pub fn push(&mut self, id: usize, writer: OwnedWriteHalf) {
        self.writers.push((id, writer));
    }

    /// Remove a connection by ID (e.g., on clean disconnect notification).
    pub fn remove(&mut self, conn_id: usize) {
        self.writers.retain(|(id, _)| *id != conn_id);
    }

    /// Returns true if no recv plugins are connected.
    pub fn is_empty(&self) -> bool {
        self.writers.is_empty()
    }

    /// Number of active recv plugin connections.
    pub fn len(&self) -> usize {
        self.writers.len()
    }

    /// Broadcast a frame to all recv plugins. Dead connections are silently
    /// removed with a `warn!` log.
    pub async fn broadcast(&mut self, frame: &[u8]) {
        if self.writers.is_empty() {
            return;
        }
        let mut dead = Vec::new();
        for (id, writer) in &mut self.writers {
            if writer.write_all(frame).await.is_err() {
                dead.push(*id);
            }
        }
        for id in &dead {
            tracing::warn!("Removing failed IPC writer (conn {id})");
        }
        if !dead.is_empty() {
            self.writers.retain(|(id, _)| !dead.contains(id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_get_peer() {
        let mut reg = PeerRegistry::new();
        reg.add("peer1".to_string(), Some("Alice".to_string()));
        let peer = reg.get("peer1").unwrap();
        assert_eq!(peer.display_name.as_deref(), Some("Alice"));
        assert!(!peer.hello_sent);
    }

    #[test]
    fn remove_peer_frees_slot() {
        let mut reg = PeerRegistry::new();
        reg.add("peer1".to_string(), Some("Alice".to_string()));
        reg.get_mut("peer1").unwrap().identity = Some("alice-id".to_string());
        let slot = reg.assign_slot("peer1", 0).unwrap();
        assert_eq!(slot, 0);
        assert!(reg.slots.is_occupied(0));

        reg.remove("peer1");
        assert!(!reg.slots.is_occupied(0));
        assert!(reg.get("peer1").is_none());
    }

    #[test]
    fn remove_peer_without_identity_frees_slot() {
        let mut reg = PeerRegistry::new();
        reg.add("peer1".to_string(), Some("Anon".to_string()));
        // No identity set — peer.identity remains None
        let slot = reg.assign_slot("peer1", 0).unwrap();
        assert_eq!(slot, 0);
        assert!(reg.slots.is_occupied(0));

        reg.remove("peer1");
        assert!(!reg.slots.is_occupied(0), "Slot must be freed even without identity");
    }

    #[test]
    fn slot_affinity_preserved_on_rejoin() {
        let mut reg = PeerRegistry::new();

        // Peer joins with identity, gets slot 0
        reg.add("peer1".to_string(), Some("Alice".to_string()));
        reg.get_mut("peer1").unwrap().identity = Some("alice-id".to_string());
        let slot = reg.assign_slot("peer1", 0).unwrap();
        assert_eq!(slot, 0);

        // Peer leaves — slot freed, affinity created
        reg.remove("peer1");
        assert!(!reg.slots.is_occupied(0));

        // Peer rejoins (new peer_id, same identity) — should reclaim slot 0 via affinity
        reg.add("peer2".to_string(), Some("Alice".to_string()));
        reg.get_mut("peer2").unwrap().identity = Some("alice-id".to_string());
        let slot2 = reg.assign_slot("peer2", 0).unwrap();
        assert_eq!(slot2, 0, "Should reuse slot 0 via identity affinity");
    }

    #[test]
    fn timed_out_peers_returns_stale_entries() {
        let mut reg = PeerRegistry::new();
        reg.add("peer1".to_string(), None);
        // Backdate last_seen to 60 seconds ago
        reg.get_mut("peer1").unwrap().last_seen =
            Instant::now() - Duration::from_secs(60);
        reg.get_mut("peer1").unwrap().ever_received_message = true;

        reg.add("peer2".to_string(), None); // fresh

        let timed_out = reg.timed_out_peers(Duration::from_secs(30));
        assert!(timed_out.contains(&"peer1".to_string()));
        assert!(!timed_out.contains(&"peer2".to_string()));
    }

    #[test]
    fn timed_out_skips_never_connected_peers() {
        let mut reg = PeerRegistry::new();
        reg.add("peer1".to_string(), None);
        // Simulate 31s of ICE "checking" with no messages received
        reg.get_mut("peer1").unwrap().last_seen = Instant::now() - Duration::from_secs(31);
        // ever_received_message is false (default) — watchdog must not fire
        assert!(reg.timed_out_peers(Duration::from_secs(30)).is_empty());
    }

    #[test]
    fn timed_out_fires_for_connected_then_silent_peer() {
        let mut reg = PeerRegistry::new();
        reg.add("peer1".to_string(), None);
        reg.get_mut("peer1").unwrap().ever_received_message = true;
        reg.get_mut("peer1").unwrap().last_seen = Instant::now() - Duration::from_secs(31);
        assert!(reg.timed_out_peers(Duration::from_secs(30)).contains(&"peer1".to_string()));
    }

    #[test]
    fn derive_status_priority() {
        let mut reg = PeerRegistry::new();
        reg.add("peer1".to_string(), None);

        assert_eq!(reg.derive_status("peer1"), "connecting");

        reg.get_mut("peer1").unwrap().display_name = Some("Alice".to_string());
        assert_eq!(reg.derive_status("peer1"), "connected");

        assert_eq!(reg.derive_status("unknown"), "connecting");
    }

    #[test]
    fn mark_hello_sent_returns_true_once() {
        let mut reg = PeerRegistry::new();
        reg.add("peer1".to_string(), None);

        assert!(reg.mark_hello_sent("peer1")); // first time → true
        assert!(!reg.mark_hello_sent("peer1")); // already sent → false

        reg.clear_hello_sent("peer1");
        assert!(reg.mark_hello_sent("peer1")); // after clear → true again
    }

    #[test]
    fn rekey_peer_slots_fixes_audio_before_hello_race() {
        let mut reg = PeerRegistry::new();
        // Peer joins; no identity yet (Hello not received)
        reg.add("peer1".to_string(), None);

        // Audio arrives before Hello — slot assigned under peer_id as client_id
        let slot = reg.assign_slot("peer1", 0).unwrap();
        assert_eq!(slot, 0);

        // Hello arrives — identity now known; rekey fixes the slot
        reg.get_mut("peer1").unwrap().identity = Some("uuid-alice".to_string());
        reg.rekey_peer_slots("peer1", "uuid-alice");

        // find_by_identity should now resolve the peer
        assert_eq!(reg.find_by_identity("uuid-alice"), Some("peer1".to_string()));
    }

    #[test]
    fn no_identity_active_peers_soft_bucket() {
        let mut reg = PeerRegistry::new();
        reg.add("peer1".to_string(), Some("Alice".to_string()));
        let peer = reg.get_mut("peer1").unwrap();
        peer.ever_received_message = true;
        // identity remains None
        peer.added_at = Instant::now() - Duration::from_secs(6);

        let (soft, hard) = reg.no_identity_active_peers(
            Duration::from_secs(5),
            Duration::from_secs(15),
        );
        assert!(soft.contains(&"peer1".to_string()), "should be in soft bucket at 6s");
        assert!(hard.is_empty());

        // After marking retry sent, peer should no longer appear in soft bucket
        reg.mark_hello_retry_sent("peer1");
        let (soft, _) = reg.no_identity_active_peers(
            Duration::from_secs(5),
            Duration::from_secs(15),
        );
        assert!(soft.is_empty(), "should not repeat soft retry after mark");
    }

    #[test]
    fn no_identity_active_peers_hard_bucket() {
        let mut reg = PeerRegistry::new();
        reg.add("peer1".to_string(), Some("Alice".to_string()));
        let peer = reg.get_mut("peer1").unwrap();
        peer.ever_received_message = true;
        peer.added_at = Instant::now() - Duration::from_secs(16);

        let (soft, hard) = reg.no_identity_active_peers(
            Duration::from_secs(5),
            Duration::from_secs(15),
        );
        assert!(soft.is_empty(), "should not be in soft bucket at 16s");
        assert!(hard.contains(&"peer1".to_string()), "should be in hard bucket at 16s");
    }

    #[test]
    fn no_identity_active_peers_excludes_identified() {
        let mut reg = PeerRegistry::new();
        reg.add("peer1".to_string(), Some("Alice".to_string()));
        let peer = reg.get_mut("peer1").unwrap();
        peer.ever_received_message = true;
        peer.identity = Some("alice-uuid".to_string());
        peer.added_at = Instant::now() - Duration::from_secs(20);

        let (soft, hard) = reg.no_identity_active_peers(
            Duration::from_secs(5),
            Duration::from_secs(15),
        );
        assert!(soft.is_empty(), "identified peer must be excluded");
        assert!(hard.is_empty(), "identified peer must be excluded");
    }

}
