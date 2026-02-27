#[cfg(test)]
mod tests {
    use crate::interval::IntervalTracker;

    #[test]
    fn beats_per_interval() {
        let tracker = IntervalTracker::new(4, 4.0);
        assert_eq!(tracker.beats_per_interval(), 16.0);

        let tracker = IntervalTracker::new(2, 3.0);
        assert_eq!(tracker.beats_per_interval(), 6.0);
    }

    #[test]
    fn interval_index_calculation() {
        let tracker = IntervalTracker::new(4, 4.0); // 16 beats per interval
        assert_eq!(tracker.interval_index(0.0), 0);
        assert_eq!(tracker.interval_index(15.9), 0);
        assert_eq!(tracker.interval_index(16.0), 1);
        assert_eq!(tracker.interval_index(31.9), 1);
        assert_eq!(tracker.interval_index(32.0), 2);
    }

    #[test]
    fn first_update_fires_boundary() {
        let mut tracker = IntervalTracker::new(4, 4.0);
        let result = tracker.update(5.0);
        assert_eq!(result, Some(0));
    }

    #[test]
    fn no_boundary_within_same_interval() {
        let mut tracker = IntervalTracker::new(4, 4.0);
        tracker.update(0.0); // initial
        assert_eq!(tracker.update(1.0), None);
        assert_eq!(tracker.update(5.0), None);
        assert_eq!(tracker.update(15.9), None);
    }

    #[test]
    fn fires_at_interval_boundary() {
        let mut tracker = IntervalTracker::new(4, 4.0); // 16 beats
        tracker.update(0.0); // initial, fires for interval 0
        assert_eq!(tracker.update(16.0), Some(1));
        assert_eq!(tracker.update(32.0), Some(2));
        assert_eq!(tracker.update(48.0), Some(3));
    }

    #[test]
    fn fires_once_per_boundary() {
        let mut tracker = IntervalTracker::new(4, 4.0);
        tracker.update(0.0);
        assert_eq!(tracker.update(16.0), Some(1));
        assert_eq!(tracker.update(16.5), None);
        assert_eq!(tracker.update(17.0), None);
    }

    #[test]
    fn set_config_resets_tracking() {
        let mut tracker = IntervalTracker::new(4, 4.0);
        tracker.update(0.0);
        tracker.update(16.0);

        // Change config
        tracker.set_config(2, 4.0); // now 8 beats per interval

        // Should fire again as if fresh
        let result = tracker.update(20.0);
        assert_eq!(result, Some(2)); // beat 20 / 8 = interval 2
    }

    #[test]
    fn waltz_time_signature() {
        let mut tracker = IntervalTracker::new(4, 3.0); // 4 bars of 3/4 = 12 beats
        assert_eq!(tracker.beats_per_interval(), 12.0);
        tracker.update(0.0);
        assert_eq!(tracker.update(11.9), None);
        assert_eq!(tracker.update(12.0), Some(1));
    }

    #[test]
    fn getters_work() {
        let tracker = IntervalTracker::new(4, 4.0);
        assert_eq!(tracker.bars(), 4);
        assert_eq!(tracker.quantum(), 4.0);
    }
}
