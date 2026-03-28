use std::collections::HashMap;

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
        /// Monotonically increasing sequence number for heartbeat tracking.
        /// Old peers omit this field.
        #[serde(default)]
        seq: u64,
    },
    ChatMessage {
        sender_name: String,
        text: String,
    },
    /// Human-readable names for the sender's audio streams (e.g. {"0": "Bass"}).
    /// Sent after Hello and whenever names change. Full map each time.
    /// Keys are stringified stream indices (JSON requires string keys).
    StreamNames {
        names: HashMap<String, String>,
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
    /// Metrics report sent to the signaling server for session-level aggregation.
    /// Not relayed to other peers — consumed server-side only.
    MetricsReport {
        dc_open: bool,
        plugin_connected: bool,
        /// Per remote peer: cumulative frames expected/received (direction = remote→self).
        per_peer: HashMap<String, PeerFrameReport>,
        /// Cumulative IPC channel-full drops (plugin → app direction).
        #[serde(default)]
        ipc_drops: u64,
        /// Interval boundary timing drift in microseconds (actual − expected gap).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        boundary_drift_us: Option<i64>,
    },
}

/// Cumulative audio frame counts and network health for one direction (remote → observer).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerFrameReport {
    pub frames_expected: u64,
    pub frames_received: u64,
    /// Median RTT to this peer in microseconds (latest value, not cumulative).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_us: Option<i64>,
    /// Jitter (MAD of RTT) in microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter_us: Option<i64>,
    /// Cumulative DataChannel backpressure drops (audio receiver channel full).
    #[serde(default)]
    pub dc_drops: u64,
    /// Cumulative WAIF frames that arrived for already-passed intervals.
    #[serde(default)]
    pub late_frames: u64,
    /// Cumulative Opus decode failures reported by the recv plugin.
    #[serde(default)]
    pub decode_failures: u64,
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
            seq: 42,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let decoded: SyncMessage = serde_json::from_str(&json).expect("deserialize");
        match decoded {
            SyncMessage::AudioStatus {
                audio_dc_open,
                intervals_sent,
                intervals_received,
                plugin_connected,
                seq,
            } => {
                assert!(audio_dc_open);
                assert_eq!(intervals_sent, 5);
                assert_eq!(intervals_received, 3);
                assert!(plugin_connected);
                assert_eq!(seq, 42);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn audio_status_backward_compat_no_seq() {
        // Old peers don't send seq — should default to 0
        let json = r#"{"type":"AudioStatus","audio_dc_open":true,"intervals_sent":10,"intervals_received":7,"plugin_connected":false}"#;
        let decoded: SyncMessage = serde_json::from_str(json).expect("deserialize");
        match decoded {
            SyncMessage::AudioStatus { seq, .. } => assert_eq!(seq, 0),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn peer_frame_report_backward_compat() {
        // Old-format JSON without new fields — all should default to 0/None
        let json = r#"{"frames_expected":100,"frames_received":95}"#;
        let report: PeerFrameReport = serde_json::from_str(json).expect("deserialize");
        assert_eq!(report.frames_expected, 100);
        assert_eq!(report.frames_received, 95);
        assert_eq!(report.rtt_us, None);
        assert_eq!(report.jitter_us, None);
        assert_eq!(report.dc_drops, 0);
        assert_eq!(report.late_frames, 0);
        assert_eq!(report.decode_failures, 0);
    }

    #[test]
    fn peer_frame_report_full_roundtrip() {
        let report = PeerFrameReport {
            frames_expected: 200,
            frames_received: 190,
            rtt_us: Some(15000),
            jitter_us: Some(3000),
            dc_drops: 5,
            late_frames: 2,
            decode_failures: 1,
        };
        let json = serde_json::to_string(&report).expect("serialize");
        let decoded: PeerFrameReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.rtt_us, Some(15000));
        assert_eq!(decoded.jitter_us, Some(3000));
        assert_eq!(decoded.dc_drops, 5);
        assert_eq!(decoded.late_frames, 2);
        assert_eq!(decoded.decode_failures, 1);
    }

    #[test]
    fn metrics_report_backward_compat() {
        // Old-format without ipc_drops and boundary_drift_us
        let json = r#"{"type":"MetricsReport","dc_open":true,"plugin_connected":false,"per_peer":{}}"#;
        let decoded: SignalMessage = serde_json::from_str(json).expect("deserialize");
        match decoded {
            SignalMessage::MetricsReport { ipc_drops, boundary_drift_us, .. } => {
                assert_eq!(ipc_drops, 0);
                assert_eq!(boundary_drift_us, None);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn stream_names_roundtrip() {
        let mut names = HashMap::new();
        names.insert("0".to_string(), "Bass".to_string());
        names.insert("1".to_string(), "Drums".to_string());
        let msg = SyncMessage::StreamNames { names };
        let json = serde_json::to_string(&msg).expect("serialize");
        let decoded: SyncMessage = serde_json::from_str(&json).expect("deserialize");
        match decoded {
            SyncMessage::StreamNames { names } => {
                assert_eq!(names.len(), 2);
                assert_eq!(names["0"], "Bass");
                assert_eq!(names["1"], "Drums");
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
