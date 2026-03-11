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
        /// Human-readable name (e.g. "Ringo"). Old peers omit this field.
        #[serde(default)]
        display_name: Option<String>,
        /// Persistent identity that survives reconnects — used for peer affinity
        /// (slot re-assignment). Generated once per app install, stored locally.
        /// Old peers omit this field.
        #[serde(default)]
        identity: Option<String>,
    },
    /// Announce audio capabilities (sent after Hello)
    AudioCapabilities {
        /// Supported sample rates (e.g., [48000])
        sample_rates: Vec<u32>,
        /// Supported channel counts (e.g., [1, 2])
        channel_counts: Vec<u16>,
        /// Whether this peer wants to send audio
        can_send: bool,
        /// Whether this peer wants to receive audio
        can_receive: bool,
        /// Maximum number of streams this peer will send (None = legacy single-stream)
        #[serde(default)]
        max_streams: Option<u16>,
    },
    /// Audio interval metadata (sent on the sync channel right before binary audio)
    AudioIntervalReady {
        /// Interval index
        interval_index: i64,
        /// Size of the upcoming binary audio message in bytes
        wire_size: u32,
    },
    /// Interval boundary announcement for cross-peer index synchronisation.
    /// When a peer crosses a boundary it broadcasts this; receivers adopt the
    /// index if theirs differs, correcting any drift caused by divergent beat
    /// positions at Link session merge.
    IntervalBoundary {
        index: i64,
    },
    /// Periodic audio pipeline health (broadcast every status tick)
    AudioStatus {
        audio_dc_open: bool,
        intervals_sent: u64,
        intervals_received: u64,
        plugin_connected: bool,
    },
    ChatMessage {
        sender_name: String,
        text: String,
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
        #[serde(default)]
        display_name: Option<String>,
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
    /// Peer log broadcast: structured log entry relayed via the signaling server
    LogBroadcast {
        from: String,
        level: String,
        target: String,
        message: String,
        timestamp_us: u64,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_with_display_name_roundtrip() {
        let msg = SyncMessage::Hello {
            peer_id: "abc123".into(),
            display_name: Some("Ringo".into()),
            identity: Some("stable-uuid-1234".into()),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let decoded: SyncMessage = serde_json::from_str(&json).expect("deserialize");
        match decoded {
            SyncMessage::Hello { peer_id, display_name, identity } => {
                assert_eq!(peer_id, "abc123");
                assert_eq!(display_name.as_deref(), Some("Ringo"));
                assert_eq!(identity.as_deref(), Some("stable-uuid-1234"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn hello_without_display_name_backward_compat() {
        // Old-format JSON without display_name or identity fields
        let json = r#"{"type":"Hello","peer_id":"old-peer"}"#;
        let decoded: SyncMessage = serde_json::from_str(json).expect("deserialize");
        match decoded {
            SyncMessage::Hello { peer_id, display_name, identity } => {
                assert_eq!(peer_id, "old-peer");
                assert_eq!(display_name, None);
                assert_eq!(identity, None);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn sync_message_interval_boundary_roundtrip() {
        let msg = SyncMessage::IntervalBoundary { index: 42 };
        let json = serde_json::to_string(&msg).expect("serialize");
        let decoded: SyncMessage = serde_json::from_str(&json).expect("deserialize");
        match decoded {
            SyncMessage::IntervalBoundary { index } => assert_eq!(index, 42),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn audio_status_roundtrip() {
        let msg = SyncMessage::AudioStatus {
            audio_dc_open: true,
            intervals_sent: 5,
            intervals_received: 3,
            plugin_connected: true,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let decoded: SyncMessage = serde_json::from_str(&json).expect("deserialize");
        match decoded {
            SyncMessage::AudioStatus {
                audio_dc_open,
                intervals_sent,
                intervals_received,
                plugin_connected,
            } => {
                assert!(audio_dc_open);
                assert_eq!(intervals_sent, 5);
                assert_eq!(intervals_received, 3);
                assert!(plugin_connected);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn chat_message_roundtrip() {
        let msg = SyncMessage::ChatMessage {
            sender_name: "Ringo".into(),
            text: "Let's change key".into(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let decoded: SyncMessage = serde_json::from_str(&json).expect("deserialize");
        match decoded {
            SyncMessage::ChatMessage { sender_name, text } => {
                assert_eq!(sender_name, "Ringo");
                assert_eq!(text, "Let's change key");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
