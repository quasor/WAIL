#[cfg(test)]
mod tests {
    use crate::clock::ClockSync;
    use crate::protocol::SyncMessage;

    #[test]
    fn ping_generates_incrementing_ids() {
        let mut clock = ClockSync::new();
        match clock.make_ping() {
            SyncMessage::Ping { id, .. } => assert_eq!(id, 0),
            _ => panic!("Expected Ping"),
        }
        match clock.make_ping() {
            SyncMessage::Ping { id, .. } => assert_eq!(id, 1),
            _ => panic!("Expected Ping"),
        }
    }

    #[test]
    fn handle_ping_returns_pong_with_correct_fields() {
        let clock = ClockSync::new();
        let pong = clock.handle_ping(42, 1000);
        match pong {
            SyncMessage::Pong {
                id,
                ping_sent_at_us,
                pong_sent_at_us,
            } => {
                assert_eq!(id, 42);
                assert_eq!(ping_sent_at_us, 1000);
                assert!(pong_sent_at_us >= 0);
            }
            _ => panic!("Expected Pong"),
        }
    }

    #[test]
    fn offset_none_for_unknown_peer() {
        let clock = ClockSync::new();
        assert!(clock.offset_us("unknown").is_none());
        assert!(clock.rtt_us("unknown").is_none());
    }

    #[test]
    fn pong_establishes_offset_and_rtt() {
        let mut clock = ClockSync::new();
        let local_send = clock.now_us();
        // Simulate remote clock that is 1000us ahead
        let remote_recv = local_send + 500 + 1000; // half RTT + offset
        clock.handle_pong("peer1", local_send, remote_recv);

        assert!(clock.offset_us("peer1").is_some());
        assert!(clock.rtt_us("peer1").is_some());
        assert!(clock.rtt_us("peer1").unwrap() >= 0);
    }

    #[test]
    fn multiple_pongs_converge() {
        let mut clock = ClockSync::new();
        let offset = 5000i64; // 5ms offset

        for _ in 0..10 {
            let t = clock.now_us();
            // Simulate consistent RTT of ~1000us with the offset
            let remote_time = t + 500 + offset;
            clock.handle_pong("peer1", t, remote_time);
            // Small sleep equivalent: the loop is fast enough
        }

        let estimated_offset = clock.offset_us("peer1").unwrap();
        // Should be close to the real offset (within ~2ms tolerance due to timing)
        assert!(
            (estimated_offset - offset).abs() < 2000,
            "Offset {} too far from expected {}",
            estimated_offset,
            offset
        );
    }

    #[test]
    fn remote_to_local_conversion() {
        let mut clock = ClockSync::new();
        let t = clock.now_us();
        // offset = remote - local = 1000
        let remote_recv = t + 500 + 1000;
        clock.handle_pong("peer1", t, remote_recv);

        let offset = clock.offset_us("peer1").unwrap();
        let remote_ts = 10000i64;
        let local_ts = clock.remote_to_local("peer1", remote_ts).unwrap();
        assert_eq!(local_ts, remote_ts - offset);
    }

    #[test]
    fn remote_to_local_none_for_unknown() {
        let clock = ClockSync::new();
        assert!(clock.remote_to_local("unknown", 100).is_none());
    }
}
