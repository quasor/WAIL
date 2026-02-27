#[cfg(test)]
mod tests {
    use crate::protocol::{SignalMessage, SignalPayload, SyncMessage};

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
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: SyncMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            SyncMessage::Hello { peer_id } => assert_eq!(peer_id, "abc123"),
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
    fn signal_payload_offer_roundtrip() {
        let msg = SignalMessage::Signal {
            to: "peer2".to_string(),
            from: "peer1".to_string(),
            payload: SignalPayload::Offer {
                sdp: "v=0\r\n...".to_string(),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: SignalMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            SignalMessage::Signal { to, from, payload } => {
                assert_eq!(to, "peer2");
                assert_eq!(from, "peer1");
                match payload {
                    SignalPayload::Offer { sdp } => assert_eq!(sdp, "v=0\r\n..."),
                    _ => panic!("Wrong payload variant"),
                }
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn signal_payload_ice_candidate_roundtrip() {
        let msg = SignalMessage::Signal {
            to: "peer2".to_string(),
            from: "peer1".to_string(),
            payload: SignalPayload::IceCandidate {
                candidate: "candidate:1 1 UDP 2122252543 192.168.1.1 12345 typ host".to_string(),
                sdp_mid: Some("0".to_string()),
                sdp_mline_index: Some(0),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: SignalMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            SignalMessage::Signal { payload, .. } => match payload {
                SignalPayload::IceCandidate {
                    candidate,
                    sdp_mid,
                    sdp_mline_index,
                } => {
                    assert!(candidate.contains("candidate:1"));
                    assert_eq!(sdp_mid, Some("0".to_string()));
                    assert_eq!(sdp_mline_index, Some(0));
                }
                _ => panic!("Wrong payload"),
            },
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
