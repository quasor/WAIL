/// NINJAM-style interval tracking on top of the Ableton Link beat grid.
///
/// An interval is N bars of a given quantum (time signature numerator).
/// For example, 4 bars of quantum 4 (4/4) = 16 beats per interval.
pub struct IntervalTracker {
    bars: u32,
    quantum: f64,
    last_interval_index: Option<i64>,
}

impl IntervalTracker {
    pub fn new(bars: u32, quantum: f64) -> Self {
        Self {
            bars,
            quantum,
            last_interval_index: None,
        }
    }

    /// Beats per interval (bars * quantum).
    pub fn beats_per_interval(&self) -> f64 {
        self.bars as f64 * self.quantum
    }

    /// Current interval index for a given beat position.
    pub fn interval_index(&self, beat: f64) -> i64 {
        (beat / self.beats_per_interval()).floor() as i64
    }

    /// Call this with the current beat position. Returns `Some(interval_index)`
    /// if we just crossed an interval boundary.
    pub fn update(&mut self, beat: f64) -> Option<i64> {
        let idx = self.interval_index(beat);
        match self.last_interval_index {
            Some(last) if idx != last => {
                self.last_interval_index = Some(idx);
                Some(idx)
            }
            None => {
                self.last_interval_index = Some(idx);
                // First update — report the initial interval
                Some(idx)
            }
            _ => None,
        }
    }

    pub fn bars(&self) -> u32 {
        self.bars
    }

    pub fn quantum(&self) -> f64 {
        self.quantum
    }

    pub fn set_config(&mut self, bars: u32, quantum: f64) {
        self.bars = bars;
        self.quantum = quantum;
        self.last_interval_index = None; // reset
    }
}
