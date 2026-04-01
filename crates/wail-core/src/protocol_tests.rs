#[cfg(test)]
mod tests {
    use crate::protocol::{SignalMessage, SyncMessage};

    #[test]
    fn sync_message_ping_roundtrip() {
        let msg = SyncMessage::Ping {
            id: 42,
            sent_at_us: 123456,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: SyncMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            SyncMessage::Ping { id, sent_at_us } => {
                assert_eq!(id, 42);
                assert_eq!(sent_at_us, 123456);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn sync_message_pong_roundtrip() {
        let msg = SyncMessage::Pong {
            id: 7,
            ping_sent_at_us: 100,
            pong_sent_at_us: 200,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: SyncMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            SyncMessage::Pong {
                id,
                ping_sent_at_us,
                pong_sent_at_us,
            } => {
                assert_eq!(id, 7);
                assert_eq!(ping_sent_at_us, 100);
                assert_eq!(pong_sent_at_us, 200);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn sync_message_tempo_change_roundtrip() {
        let msg = SyncMessage::TempoChange {
            bpm: 140.5,
            quantum: 4.0,
            timestamp_us: 999999,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: SyncMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            SyncMessage::TempoChange {
                bpm,
                quantum,
                timestamp_us,
            } => {
                assert!((bpm - 140.5).abs() < f64::EPSILON);
                assert!((quantum - 4.0).abs() < f64::EPSILON);
                assert_eq!(timestamp_us, 999999);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn sync_message_state_snapshot_roundtrip() {
        let msg = SyncMessage::StateSnapshot {
            bpm: 120.0,
            beat: 4.5,
            phase: 0.5,
            quantum: 4.0,
            timestamp_us: 500000,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: SyncMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            SyncMessage::StateSnapshot {
                bpm,
                beat,
                phase,
                quantum,
                timestamp_us,
            } => {
                assert!((bpm - 120.0).abs() < f64::EPSILON);
                assert!((beat - 4.5).abs() < f64::EPSILON);
                assert!((phase - 0.5).abs() < f64::EPSILON);
                assert!((quantum - 4.0).abs() < f64::EPSILON);
                assert_eq!(timestamp_us, 500000);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn sync_message_hello_roundtrip() {
        let msg = SyncMessage::Hello {
            peer_id: "abc123".to_string(),
            display_name: Some("TestUser".to_string()),
            identity: Some("stable-uuid".to_string()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: SyncMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            SyncMessage::Hello { peer_id, display_name, identity } => {
                assert_eq!(peer_id, "abc123");
                assert_eq!(display_name.as_deref(), Some("TestUser"));
                assert_eq!(identity.as_deref(), Some("stable-uuid"));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn sync_message_interval_config_roundtrip() {
        let msg = SyncMessage::IntervalConfig {
            bars: 4,
            quantum: 4.0,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: SyncMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            SyncMessage::IntervalConfig { bars, quantum } => {
                assert_eq!(bars, 4);
                assert!((quantum - 4.0).abs() < f64::EPSILON);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn signal_message_join_roundtrip() {
        let msg = SignalMessage::Join {
            room: "jam-session".to_string(),
            peer_id: "peer1".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: SignalMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            SignalMessage::Join { room, peer_id } => {
                assert_eq!(room, "jam-session");
                assert_eq!(peer_id, "peer1");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn sync_message_tagged_json() {
        // Verify the "type" tag is present in serialized JSON
        let msg = SyncMessage::Ping {
            id: 0,
            sent_at_us: 0,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"Ping\""));
    }
}
