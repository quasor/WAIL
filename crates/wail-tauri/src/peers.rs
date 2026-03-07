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
    pub reconnect_attempts: u32,
    /// True while a reconnect timer is scheduled — prevents duplicate PeerFailed events
    /// from spawning multiple concurrent timers and inflating the attempt counter.
    pub reconnect_pending: bool,
    pub audio_recv_count: u64,
    pub audio_recv_prev: u64,
    pub prev_status: String,
}

impl PeerState {
    fn new(display_name: Option<String>) -> Self {
        Self {
            display_name,
            identity: None,
            slots: HashMap::new(),
            hello_sent: false,
            last_seen: Instant::now(),
            reconnect_attempts: 0,
            reconnect_pending: false,
            audio_recv_count: 0,
            audio_recv_prev: 0,
            prev_status: String::new(),
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

    /// Return peer IDs whose last_seen is older than `timeout`.
    pub fn timed_out_peers(&self, timeout: Duration) -> Vec<String> {
        let now = Instant::now();
        self.peers
            .iter()
            .filter(|(_, p)| now.duration_since(p.last_seen) > timeout)
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

    /// Full reset for signaling reconnection. Preserves slot affinity so
    /// returning peers reclaim their slots. Moves all active slots to
    /// reserved, clears all peer state, then seeds fresh peers from `new_names`.
    pub fn reset_for_reconnect(&mut self, new_names: HashMap<String, Option<String>>) {
        self.peers.clear();
        self.slots.clear_active_to_reserved();

        // Seed new peers with last_seen = now
        let now = Instant::now();
        for (peer_id, display_name) in new_names {
            let mut state = PeerState::new(display_name);
            state.last_seen = now;
            self.peers.insert(peer_id, state);
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

    /// Find the peer_id of a peer with the given identity, if any.
    pub fn find_by_identity(&self, identity: &str) -> Option<String> {
        self.peers
            .iter()
            .find(|(_, p)| p.identity.as_deref() == Some(identity))
            .map(|(id, _)| id.clone())
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
        if peer.reconnect_attempts > 0 {
            "reconnecting"
        } else if peer.display_name.is_none() {
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
        assert_eq!(peer.reconnect_attempts, 0);
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

        reg.add("peer2".to_string(), None); // fresh

        let timed_out = reg.timed_out_peers(Duration::from_secs(30));
        assert!(timed_out.contains(&"peer1".to_string()));
        assert!(!timed_out.contains(&"peer2".to_string()));
    }

    #[test]
    fn reset_for_reconnect_clears_peers_preserves_affinity() {
        let mut reg = PeerRegistry::new();

        reg.add("old-peer".to_string(), Some("Bob".to_string()));
        reg.get_mut("old-peer").unwrap().identity = Some("bob-id".to_string());
        reg.assign_slot("old-peer", 0).unwrap();

        let mut new_names = HashMap::new();
        new_names.insert("new-peer".to_string(), Some("Carol".to_string()));
        reg.reset_for_reconnect(new_names);

        assert!(reg.get("old-peer").is_none());
        assert!(!reg.slots.is_occupied(0));
        // Affinity preserved — bob reclaims slot 0
        reg.add("bob-new".to_string(), Some("Bob".to_string()));
        reg.get_mut("bob-new").unwrap().identity = Some("bob-id".to_string());
        assert_eq!(reg.assign_slot("bob-new", 0), Some(0), "Affinity reclaims slot 0");
        reg.remove("bob-new");
        // New peer seeded
        assert_eq!(
            reg.get("new-peer").unwrap().display_name.as_deref(),
            Some("Carol")
        );
    }

    #[test]
    fn derive_status_priority() {
        let mut reg = PeerRegistry::new();
        reg.add("peer1".to_string(), None);

        assert_eq!(reg.derive_status("peer1"), "connecting");

        reg.get_mut("peer1").unwrap().display_name = Some("Alice".to_string());
        assert_eq!(reg.derive_status("peer1"), "connected");

        reg.get_mut("peer1").unwrap().reconnect_attempts = 1;
        assert_eq!(reg.derive_status("peer1"), "reconnecting");

        reg.get_mut("peer1").unwrap().reconnect_attempts = 0;
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
    fn reconnect_pending_prevents_counter_inflation() {
        let mut reg = PeerRegistry::new();
        reg.add("peer-x".to_string(), Some("Alice".to_string()));

        // Initial state: not pending, zero attempts
        let peer = reg.get("peer-x").unwrap();
        assert!(!peer.reconnect_pending);
        assert_eq!(peer.reconnect_attempts, 0);

        // First failure: increment + set pending (simulates session PeerFailed handler)
        let peer = reg.get_mut("peer-x").unwrap();
        peer.reconnect_attempts += 1;
        peer.reconnect_pending = true;

        // While pending: session should skip duplicate events — counter stays at 1
        let peer = reg.get("peer-x").unwrap();
        assert!(peer.reconnect_pending);
        assert_eq!(peer.reconnect_attempts, 1);

        // Timer fires: clear pending
        let peer = reg.get_mut("peer-x").unwrap();
        peer.reconnect_pending = false;

        // Next real failure (from new connection): processed normally
        let peer = reg.get("peer-x").unwrap();
        assert!(!peer.reconnect_pending);
        let peer = reg.get_mut("peer-x").unwrap();
        peer.reconnect_attempts += 1;
        peer.reconnect_pending = true;
        assert_eq!(peer.reconnect_attempts, 2);

        // Successful reconnection: both cleared
        let peer = reg.get_mut("peer-x").unwrap();
        peer.reconnect_attempts = 0;
        peer.reconnect_pending = false;
        assert_eq!(peer.reconnect_attempts, 0);
        assert!(!peer.reconnect_pending);
    }
}
