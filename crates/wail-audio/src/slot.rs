use serde::{Deserialize, Serialize};

/// Maximum number of remote peer-channel slots with independent audio channels.
pub const MAX_SLOTS: usize = 31;

/// Uniquely identifies a single audio channel from a specific client.
///
/// Combines the client's persistent identity (UUID, survives reconnects/restarts)
/// with the channel index (stream_id from the WAIL Send plugin, 0–30).
/// Used as the key for stable slot assignment on the receiving side.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientChannelMapping {
    /// Persistent client identity (UUID v4, stored in app data dir).
    pub client_id: String,
    /// Channel index within the client (maps to stream_id / stream_index).
    pub channel_index: u16,
}

impl ClientChannelMapping {
    pub fn new(client_id: impl Into<String>, channel_index: u16) -> Self {
        Self {
            client_id: client_id.into(),
            channel_index,
        }
    }

    /// Short display ID for logging/UI, e.g. "a1b2c3:0".
    pub fn short_id(&self) -> String {
        let prefix: String = self.client_id.chars().take(6).collect();
        format!("{}:{}", prefix, self.channel_index)
    }
}

impl std::fmt::Display for ClientChannelMapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.short_id())
    }
}

/// Stable slot assignment table for `ClientChannelMapping`s.
///
/// Guarantees that the same mapping always resolves to the same slot index
/// within a session. When a mapping disconnects, its slot is reserved
/// (affinity). When it reconnects, it reclaims the reserved slot.
///
/// Uses `Vec`-based storage (no `HashMap`) to be safe on audio threads.
pub struct SlotTable {
    occupied: [bool; MAX_SLOTS],
    /// Currently assigned: (mapping, slot_index)
    active: Vec<(ClientChannelMapping, usize)>,
    /// Reserved for disconnected mappings: (mapping, slot_index)
    reserved: Vec<(ClientChannelMapping, usize)>,
}

impl SlotTable {
    pub fn new() -> Self {
        Self {
            occupied: [false; MAX_SLOTS],
            active: Vec::with_capacity(MAX_SLOTS),
            reserved: Vec::with_capacity(MAX_SLOTS),
        }
    }

    /// Assign a slot for the given mapping. Idempotent — returns existing slot
    /// if already assigned. Checks reserved affinity before allocating a new slot.
    /// Returns `None` if all slots are full.
    pub fn assign(&mut self, mapping: &ClientChannelMapping) -> Option<usize> {
        // 1. Already active — return existing
        if let Some(slot) = self.lookup_active(mapping) {
            return Some(slot);
        }

        // 2. Check reserved affinity
        if let Some(pos) = self.reserved.iter().position(|(m, _)| m == mapping) {
            let (_, slot) = self.reserved.remove(pos);
            if slot < MAX_SLOTS && !self.occupied[slot] {
                self.occupied[slot] = true;
                self.active.push((mapping.clone(), slot));
                return Some(slot);
            }
            // Slot was taken by someone else — fall through to find a new one
        }

        // 3. Find lowest free slot
        let slot = self.occupied.iter().position(|&occ| !occ)?;
        self.occupied[slot] = true;
        self.active.push((mapping.clone(), slot));
        Some(slot)
    }

    /// Release a mapping, moving it from active to reserved (affinity).
    pub fn release(&mut self, mapping: &ClientChannelMapping) {
        if let Some(pos) = self.active.iter().position(|(m, _)| m == mapping) {
            let (m, slot) = self.active.remove(pos);
            if slot < MAX_SLOTS {
                self.occupied[slot] = false;
            }
            // Replace any existing reservation for this mapping
            self.reserved.retain(|(rm, _)| rm != &m);
            self.reserved.push((m, slot));
        }
    }

    /// Release all channels for a given client, creating reservations for each.
    pub fn release_all_for_client(&mut self, client_id: &str) {
        let to_release: Vec<ClientChannelMapping> = self
            .active
            .iter()
            .filter(|(m, _)| m.client_id == client_id)
            .map(|(m, _)| m.clone())
            .collect();
        for mapping in to_release {
            self.release(&mapping);
        }
    }

    /// Read-only slot lookup for a mapping.
    pub fn slot_for(&self, mapping: &ClientChannelMapping) -> Option<usize> {
        self.lookup_active(mapping)
    }

    /// Iterator over active (mapping, slot_index) pairs.
    pub fn active_mappings(&self) -> &[(ClientChannelMapping, usize)] {
        &self.active
    }

    /// Check if a slot is occupied.
    pub fn is_occupied(&self, slot: usize) -> bool {
        slot < MAX_SLOTS && self.occupied[slot]
    }

    /// Full reset — clears active, reserved, and occupied.
    pub fn clear(&mut self) {
        self.active.clear();
        self.reserved.clear();
        self.occupied = [false; MAX_SLOTS];
    }

    /// Move all active mappings to reserved (preserve affinity).
    /// Used during signaling reconnection.
    pub fn clear_active_to_reserved(&mut self) {
        for (mapping, slot) in self.active.drain(..) {
            if slot < MAX_SLOTS {
                self.occupied[slot] = false;
            }
            self.reserved.retain(|(m, _)| m != &mapping);
            self.reserved.push((mapping, slot));
        }
    }

    /// Reclaim all reserved slots for a given client_id, moving them to active.
    /// Returns the list of (channel_index, slot_index) that were reclaimed.
    pub fn reclaim_reserved_for_client(&mut self, client_id: &str) -> Vec<(u16, usize)> {
        let to_reclaim: Vec<(ClientChannelMapping, usize)> = self
            .reserved
            .iter()
            .filter(|(m, _)| m.client_id == client_id)
            .cloned()
            .collect();

        let mut reclaimed = Vec::new();
        for (mapping, slot) in to_reclaim {
            self.reserved.retain(|(m, _)| m != &mapping);
            if slot < MAX_SLOTS && !self.occupied[slot] {
                self.occupied[slot] = true;
                self.active.push((mapping.clone(), slot));
                reclaimed.push((mapping.channel_index, slot));
            }
            // If slot was taken by someone else, the reservation is just dropped
        }
        reclaimed
    }

    /// Re-key all active and reserved mappings from `old_client_id` to
    /// `new_client_id`, preserving slot assignments. Used when a peer's
    /// persistent identity becomes known after slots were already assigned
    /// under a fallback key.
    pub fn rekey_client(&mut self, old_client_id: &str, new_client_id: &str) {
        for (mapping, _) in &mut self.active {
            if mapping.client_id == old_client_id {
                mapping.client_id = new_client_id.to_string();
            }
        }
        for (mapping, _) in &mut self.reserved {
            if mapping.client_id == old_client_id {
                mapping.client_id = new_client_id.to_string();
            }
        }
    }

    fn lookup_active(&self, mapping: &ClientChannelMapping) -> Option<usize> {
        self.active
            .iter()
            .find(|(m, _)| m == mapping)
            .map(|(_, slot)| *slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(id: &str, ch: u16) -> ClientChannelMapping {
        ClientChannelMapping::new(id, ch)
    }

    #[test]
    fn assign_returns_slot_zero_for_first() {
        let mut table = SlotTable::new();
        assert_eq!(table.assign(&mapping("alice", 0)), Some(0));
    }

    #[test]
    fn assign_is_idempotent() {
        let mut table = SlotTable::new();
        let m = mapping("alice", 0);
        assert_eq!(table.assign(&m), Some(0));
        assert_eq!(table.assign(&m), Some(0));
    }

    #[test]
    fn different_mappings_get_different_slots() {
        let mut table = SlotTable::new();
        assert_eq!(table.assign(&mapping("alice", 0)), Some(0));
        assert_eq!(table.assign(&mapping("bob", 0)), Some(1));
    }

    #[test]
    fn two_channels_same_client_get_separate_slots() {
        let mut table = SlotTable::new();
        assert_eq!(table.assign(&mapping("alice", 0)), Some(0));
        assert_eq!(table.assign(&mapping("alice", 1)), Some(1));
    }

    #[test]
    fn release_and_reclaim_via_affinity() {
        let mut table = SlotTable::new();
        let m = mapping("alice", 0);
        assert_eq!(table.assign(&m), Some(0));

        // Another peer takes slot 1
        assert_eq!(table.assign(&mapping("bob", 0)), Some(1));

        // Alice disconnects
        table.release(&m);
        assert!(!table.is_occupied(0));

        // Alice reconnects — should reclaim slot 0
        assert_eq!(table.assign(&m), Some(0));
    }

    #[test]
    fn release_all_for_client() {
        let mut table = SlotTable::new();
        assert_eq!(table.assign(&mapping("alice", 0)), Some(0));
        assert_eq!(table.assign(&mapping("alice", 1)), Some(1));
        assert_eq!(table.assign(&mapping("bob", 0)), Some(2));

        table.release_all_for_client("alice");
        assert!(!table.is_occupied(0));
        assert!(!table.is_occupied(1));
        assert!(table.is_occupied(2));

        // Alice channels reclaim via affinity
        assert_eq!(table.assign(&mapping("alice", 0)), Some(0));
        assert_eq!(table.assign(&mapping("alice", 1)), Some(1));
    }

    #[test]
    fn full_capacity_returns_none() {
        let mut table = SlotTable::new();
        for i in 0..MAX_SLOTS {
            assert!(table.assign(&mapping(&format!("peer-{i}"), 0)).is_some());
        }
        assert_eq!(table.assign(&mapping("overflow", 0)), None);
    }

    #[test]
    fn clear_resets_everything() {
        let mut table = SlotTable::new();
        table.assign(&mapping("alice", 0));
        table.release(&mapping("alice", 0));
        table.clear();

        // No affinity — alice gets whatever is free (slot 0)
        assert_eq!(table.assign(&mapping("alice", 0)), Some(0));
        // But this tests that reserved was cleared (slot 0 was free, not reserved)
        assert!(table.active_mappings().len() == 1);
    }

    #[test]
    fn clear_active_to_reserved_preserves_affinity() {
        let mut table = SlotTable::new();
        assert_eq!(table.assign(&mapping("alice", 0)), Some(0));
        assert_eq!(table.assign(&mapping("bob", 0)), Some(1));

        table.clear_active_to_reserved();
        assert!(!table.is_occupied(0));
        assert!(!table.is_occupied(1));
        assert!(table.active_mappings().is_empty());

        // Both reclaim via affinity
        assert_eq!(table.assign(&mapping("alice", 0)), Some(0));
        assert_eq!(table.assign(&mapping("bob", 0)), Some(1));
    }

    #[test]
    fn slot_for_returns_active_slot() {
        let mut table = SlotTable::new();
        let m = mapping("alice", 0);
        assert_eq!(table.slot_for(&m), None);
        table.assign(&m);
        assert_eq!(table.slot_for(&m), Some(0));
        table.release(&m);
        assert_eq!(table.slot_for(&m), None);
    }

    #[test]
    fn short_id_format() {
        let m = ClientChannelMapping::new("abcdef1234567890", 3);
        assert_eq!(m.short_id(), "abcdef:3");

        let short = ClientChannelMapping::new("ab", 0);
        assert_eq!(short.short_id(), "ab:0");
    }

    #[test]
    fn affinity_survives_slot_taken_by_another() {
        let mut table = SlotTable::new();
        let alice = mapping("alice", 0);
        let bob = mapping("bob", 0);

        // Alice gets slot 0
        assert_eq!(table.assign(&alice), Some(0));
        // Alice leaves — slot 0 reserved
        table.release(&alice);

        // Bob takes slot 0 (no affinity for bob, lowest free)
        assert_eq!(table.assign(&bob), Some(0));

        // Alice reconnects — slot 0 is taken, falls through to next free
        assert_eq!(table.assign(&alice), Some(1));
    }

    #[test]
    fn rekey_client_preserves_slot() {
        let mut table = SlotTable::new();
        // Assign under fallback peer_id key
        let fallback = mapping("peer-abc", 0);
        assert_eq!(table.assign(&fallback), Some(0));

        // Identity becomes known — re-key to persistent UUID
        table.rekey_client("peer-abc", "uuid-alice");

        // The slot is now accessible under the new key
        let real = mapping("uuid-alice", 0);
        assert_eq!(table.slot_for(&real), Some(0));
        // Old key no longer resolves
        assert_eq!(table.slot_for(&fallback), None);
        // Only one slot occupied
        assert_eq!(table.active_mappings().len(), 1);
    }

    #[test]
    fn rekey_client_works_for_reserved() {
        let mut table = SlotTable::new();
        let fallback = mapping("peer-abc", 0);
        assert_eq!(table.assign(&fallback), Some(0));
        table.release(&fallback);

        // Re-key while reserved
        table.rekey_client("peer-abc", "uuid-alice");

        // Reclaim under the new key
        let real = mapping("uuid-alice", 0);
        assert_eq!(table.assign(&real), Some(0));
    }
}
