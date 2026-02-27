use std::collections::HashMap;
use std::time::Instant;

use crate::protocol::SyncMessage;

const WINDOW_SIZE: usize = 8;
const PING_INTERVAL_MS: u64 = 2000;

/// Tracks clock offset relative to each remote peer using NTP-like Ping/Pong.
pub struct ClockSync {
    epoch: Instant,
    per_peer: HashMap<String, PeerClock>,
    next_ping_id: u64,
}

struct PeerClock {
    /// (rtt_us, offset_us) samples
    samples: Vec<(i64, i64)>,
    /// Median offset in microseconds (remote - local)
    offset_us: i64,
    /// Median RTT in microseconds
    rtt_us: i64,
}

impl ClockSync {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            per_peer: HashMap::new(),
            next_ping_id: 0,
        }
    }

    /// Current local time in microseconds since epoch.
    pub fn now_us(&self) -> i64 {
        self.epoch.elapsed().as_micros() as i64
    }

    /// Generate a Ping message to send to a peer.
    pub fn make_ping(&mut self) -> SyncMessage {
        let id = self.next_ping_id;
        self.next_ping_id += 1;
        SyncMessage::Ping {
            id,
            sent_at_us: self.now_us(),
        }
    }

    /// Handle an incoming Ping — return a Pong to send back.
    pub fn handle_ping(&self, id: u64, sent_at_us: i64) -> SyncMessage {
        SyncMessage::Pong {
            id,
            ping_sent_at_us: sent_at_us,
            pong_sent_at_us: self.now_us(),
        }
    }

    /// Handle an incoming Pong — update clock offset estimate for the peer.
    pub fn handle_pong(&mut self, peer_id: &str, ping_sent_at_us: i64, pong_sent_at_us: i64) {
        let now = self.now_us();
        let rtt = now - ping_sent_at_us;
        // offset = remote_time - local_time (at the moment the pong was sent)
        // pong_sent_at_us ≈ local_midpoint + offset
        // local_midpoint = ping_sent_at_us + rtt/2
        let offset = pong_sent_at_us - (ping_sent_at_us + rtt / 2);

        let clock = self.per_peer.entry(peer_id.to_string()).or_insert(PeerClock {
            samples: Vec::new(),
            offset_us: 0,
            rtt_us: 0,
        });

        clock.samples.push((rtt, offset));
        if clock.samples.len() > WINDOW_SIZE {
            clock.samples.remove(0);
        }

        // Use median offset (robust to outliers)
        let mut offsets: Vec<i64> = clock.samples.iter().map(|s| s.1).collect();
        offsets.sort();
        clock.offset_us = offsets[offsets.len() / 2];

        let mut rtts: Vec<i64> = clock.samples.iter().map(|s| s.0).collect();
        rtts.sort();
        clock.rtt_us = rtts[rtts.len() / 2];
    }

    /// Get the estimated clock offset for a peer (remote - local) in microseconds.
    pub fn offset_us(&self, peer_id: &str) -> Option<i64> {
        self.per_peer.get(peer_id).map(|c| c.offset_us)
    }

    /// Get the estimated RTT for a peer in microseconds.
    pub fn rtt_us(&self, peer_id: &str) -> Option<i64> {
        self.per_peer.get(peer_id).map(|c| c.rtt_us)
    }

    /// Convert a remote peer's timestamp to local time.
    pub fn remote_to_local(&self, peer_id: &str, remote_us: i64) -> Option<i64> {
        self.offset_us(peer_id).map(|off| remote_us - off)
    }

    /// Ping interval in milliseconds.
    pub fn ping_interval_ms() -> u64 {
        PING_INTERVAL_MS
    }
}
