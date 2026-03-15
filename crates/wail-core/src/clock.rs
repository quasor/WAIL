use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use crate::protocol::SyncMessage;

const WINDOW_SIZE: usize = 8;
const PING_INTERVAL_MS: u64 = 2000;

/// Tracks round-trip time to each remote peer using NTP-like Ping/Pong.
pub struct ClockSync {
    epoch: Instant,
    per_peer: HashMap<String, PeerClock>,
    next_ping_id: u64,
}

struct PeerClock {
    /// RTT samples in microseconds
    samples: VecDeque<i64>,
    /// Median RTT in microseconds
    rtt_us: i64,
}

impl Default for ClockSync {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            per_peer: HashMap::new(),
            next_ping_id: 0,
        }
    }
}

impl ClockSync {
    pub fn new() -> Self {
        Self::default()
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

    /// Handle an incoming Pong — update RTT estimate for the peer.
    pub fn handle_pong(&mut self, peer_id: &str, ping_sent_at_us: i64, _pong_sent_at_us: i64) {
        let now = self.now_us();
        let rtt = now - ping_sent_at_us;

        // Discard samples with negative RTT (clock anomaly)
        if rtt < 0 {
            return;
        }

        let clock = self.per_peer.entry(peer_id.to_string()).or_insert(PeerClock {
            samples: VecDeque::with_capacity(WINDOW_SIZE),
            rtt_us: 0,
        });

        clock.samples.push_back(rtt);
        if clock.samples.len() > WINDOW_SIZE {
            clock.samples.pop_front();
        }

        // Use median RTT (robust to outliers)
        let samples: Vec<i64> = clock.samples.iter().copied().collect();
        clock.rtt_us = Self::median_of(&samples);
    }

    /// Compute the median of a slice of RTT samples.
    /// Used internally by `handle_pong`.
    pub(crate) fn median_of(samples: &[i64]) -> i64 {
        let mut sorted: Vec<i64> = samples.to_vec();
        sorted.sort();
        sorted[sorted.len() / 2]
    }

    /// Get the estimated RTT for a peer in microseconds.
    pub fn rtt_us(&self, peer_id: &str) -> Option<i64> {
        self.per_peer.get(peer_id).map(|c| c.rtt_us)
    }

    /// Ping interval in milliseconds.
    pub fn ping_interval_ms() -> u64 {
        PING_INTERVAL_MS
    }
}
