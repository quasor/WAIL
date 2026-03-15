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
    fn rtt_none_for_unknown_peer() {
        let clock = ClockSync::new();
        assert!(clock.rtt_us("unknown").is_none());
    }

    #[test]
    fn pong_establishes_rtt() {
        let mut clock = ClockSync::new();
        let local_send = clock.now_us();
        clock.handle_pong("peer1", local_send, 0);

        assert!(clock.rtt_us("peer1").is_some());
        assert!(clock.rtt_us("peer1").unwrap() >= 0);
    }

    #[test]
    fn rtt_converges_with_multiple_pongs() {
        let mut clock = ClockSync::new();
        let target_rtt = 5_000i64; // 5ms RTT

        for _ in 0..10 {
            let t = clock.now_us() - target_rtt;
            clock.handle_pong("peer1", t, 0);
        }

        let estimated_rtt = clock.rtt_us("peer1").unwrap();
        // Should be close to the target RTT (within ~2ms tolerance due to timing)
        assert!(
            (estimated_rtt - target_rtt).abs() < 2_000,
            "RTT {estimated_rtt} too far from expected {target_rtt}"
        );
    }

    // §10 — Sliding window: after 16 samples the window holds only the last 8,
    // so old samples no longer influence the median.
    #[test]
    fn sliding_window_only_uses_last_8_samples() {
        let mut clock = ClockSync::new();

        // First 8 samples: simulated RTT ≈ 1_000 µs.
        for _ in 0..8 {
            let t = clock.now_us() - 1_000;
            clock.handle_pong("peer1", t, 0);
        }
        let rtt_after_8 = clock.rtt_us("peer1").unwrap();
        assert!(
            rtt_after_8 < 5_000,
            "After 8 samples, RTT should be near 1000µs (got {rtt_after_8})"
        );

        // Next 8 samples: simulated RTT ≈ 50_000 µs (very different from the first batch).
        for _ in 0..8 {
            let t = clock.now_us() - 50_000;
            clock.handle_pong("peer1", t, 0);
        }
        let rtt_after_16 = clock.rtt_us("peer1").unwrap();
        // The window now contains only the last 8 (all ≈ 50_000µs).
        // The median should have shifted well away from 1_000µs.
        assert!(
            rtt_after_16 > 40_000,
            "After 16 samples, window should hold only last 8 (expected ~50000µs, got {rtt_after_16})"
        );
        assert!(
            (rtt_after_16 - 1_000).abs() > 5_000,
            "Old 1000µs samples should no longer influence the median"
        );
    }

    // RED TEST — Critical #1: median computation panics on empty samples.
    //
    // `ClockSync::median_of` does `sorted[sorted.len() / 2]` — panics on
    // empty input. Currently unreachable through handle_pong() because it
    // always pushes a sample before computing. But the function has no
    // self-defense: any refactor that separates entry creation from sample
    // insertion, or calls median_of on a drained VecDeque, hits a panic
    // that crashes the session select! loop and the DAW host process.
    //
    // Expected behavior: return 0 (or another safe default) for empty input.
    #[test]
    fn median_of_empty_samples_does_not_panic() {
        // This currently PANICS — index out of bounds on empty slice.
        // The fix should make it return 0 for empty input.
        let result = ClockSync::median_of(&[]);
        assert_eq!(result, 0, "median of empty samples should return 0");
    }

    // §10 — Jitter: one extreme outlier RTT among seven normal samples does not
    // dominate the median RTT estimate.
    #[test]
    fn jitter_outlier_does_not_dominate_median_rtt() {
        let mut clock = ClockSync::new();

        // 7 normal samples: RTT ≈ 1000 µs.
        for _ in 0..7 {
            let t = clock.now_us() - 1_000;
            clock.handle_pong("peer1", t, 0);
        }

        // 1 extreme outlier: pretend the ping was sent 200 ms ago (RTT ≈ 200_000 µs).
        let old_sent = clock.now_us() - 200_000;
        clock.handle_pong("peer1", old_sent, 0);

        // Sorted RTTs in the window (8 samples):
        //   [~1000, ~1000, ~1000, ~1000, ~1000, ~1000, ~1000, ~200000]
        // Median index 4 = ~1000 µs — the outlier should not dominate.
        let rtt = clock.rtt_us("peer1").unwrap();
        assert!(
            rtt < 10_000,
            "Median RTT should not be dominated by outlier (got {rtt}µs, expected ~1000µs)"
        );
    }
}
