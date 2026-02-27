use serde::{Deserialize, Serialize};

/// Messages exchanged between peers over WebRTC DataChannels.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SyncMessage {
    /// Clock sync: initiator sends Ping
    Ping {
        id: u64,
        sent_at_us: i64,
    },
    /// Clock sync: responder replies with Pong
    Pong {
        id: u64,
        ping_sent_at_us: i64,
        pong_sent_at_us: i64,
    },
    /// Tempo change detected on the sender's local Link session
    TempoChange {
        bpm: f64,
        quantum: f64,
        timestamp_us: i64,
    },
    /// Full state snapshot (sent periodically and on connect)
    StateSnapshot {
        bpm: f64,
        beat: f64,
        phase: f64,
        quantum: f64,
        timestamp_us: i64,
    },
    /// Interval configuration agreement
    IntervalConfig {
        bars: u32,
        quantum: f64,
    },
    /// Greeting on DataChannel open
    Hello {
        peer_id: String,
    },
}

/// Messages exchanged over the WebSocket signaling channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalMessage {
    /// Client -> Server: join a room
    Join {
        room: String,
        peer_id: String,
    },
    /// Server -> Client: current peer list
    PeerList {
        peers: Vec<String>,
    },
    /// Server -> Client: a new peer joined
    PeerJoined {
        peer_id: String,
    },
    /// Server -> Client: a peer left
    PeerLeft {
        peer_id: String,
    },
    /// Bidirectional: relay WebRTC signaling between peers
    Signal {
        to: String,
        from: String,
        payload: SignalPayload,
    },
}

/// WebRTC signaling payloads relayed through the signaling server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SignalPayload {
    Offer { sdp: String },
    Answer { sdp: String },
    IceCandidate { candidate: String, sdp_mid: Option<String>, sdp_mline_index: Option<u16> },
}
