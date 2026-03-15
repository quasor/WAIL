use std::time::{Duration, Instant};

use rusty_link::{AblLink, SessionState};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Events emitted by the Link bridge when local session state changes.
#[derive(Debug, Clone)]
pub enum LinkEvent {
    TempoChanged {
        bpm: f64,
        beat: f64,
        timestamp_us: i64,
    },
    StateUpdate {
        bpm: f64,
        beat: f64,
        phase: f64,
        quantum: f64,
        timestamp_us: i64,
    },
}

/// Pure-logic tempo change detector with echo guard.
///
/// Extracted from `LinkBridge` so the threshold + echo-guard state machine
/// can be tested without the AblLink C FFI.
pub(crate) struct TempoChangeDetector {
    last_tempo: f64,
    echo_guard_until: Option<Instant>,
}

impl TempoChangeDetector {
    pub(crate) fn new(initial_tempo: f64) -> Self {
        Self {
            last_tempo: initial_tempo,
            echo_guard_until: None,
        }
    }

    /// Arm the echo guard (called after applying a remote tempo change).
    pub(crate) fn arm_echo_guard(&mut self, until: Instant) {
        self.echo_guard_until = Some(until);
    }

    /// Check whether a tempo reading constitutes a reportable change.
    /// `now` is passed explicitly for deterministic testing.
    /// Returns `Some(bpm)` if the change exceeds the threshold and the echo guard is not active.
    pub(crate) fn check(&mut self, bpm: f64, now: Instant) -> Option<f64> {
        if let Some(until) = self.echo_guard_until {
            if now < until {
                return None;
            }
            self.echo_guard_until = None;
        }

        if (bpm - self.last_tempo).abs() > TEMPO_CHANGE_THRESHOLD {
            self.last_tempo = bpm;
            Some(bpm)
        } else {
            None
        }
    }

    pub(crate) fn last_tempo(&self) -> f64 {
        self.last_tempo
    }

    pub(crate) fn set_last_tempo(&mut self, bpm: f64) {
        self.last_tempo = bpm;
    }
}

/// Bridge between the local Ableton Link session and the sync engine.
pub struct LinkBridge {
    link: AblLink,
    session_state: SessionState,
    quantum: f64,
    detector: TempoChangeDetector,
}

const TEMPO_CHANGE_THRESHOLD: f64 = 0.01; // BPM
const ECHO_GUARD_DURATION: Duration = Duration::from_millis(150);
const POLL_INTERVAL: Duration = Duration::from_millis(20); // 50 Hz
const SNAPSHOT_INTERVAL_TICKS: u32 = 10; // ~200ms at 50Hz polling

impl LinkBridge {
    pub fn new(initial_bpm: f64, quantum: f64) -> Self {
        let link = AblLink::new(initial_bpm);
        let session_state = SessionState::new();
        Self {
            link,
            session_state,
            quantum,
            detector: TempoChangeDetector::new(initial_bpm),
        }
    }

    pub fn enable(&self) {
        self.link.enable(true);
        info!(bpm = self.detector.last_tempo(), "Ableton Link enabled");
    }

    pub fn disable(&self) {
        self.link.enable(false);
        info!("Ableton Link disabled");
    }

    /// Snap the local beat clock to `beat` at the current Link time.
    ///
    /// Uses `forceBeatAtTime` from the Link SDK — an immediate, non-negotiated
    /// timeline edit. Intended for one-shot join-time sync only; calling it
    /// repeatedly is disruptive to LAN Link peers.
    ///
    /// `rtt_us` is the round-trip time to the sender in microseconds. When
    /// provided, the beat value is advanced by `RTT/2 * BPM/60` to compensate
    /// for one-way transit time (the remote beat was sampled ~RTT/2 ago).
    pub fn force_beat(&mut self, beat: f64, rtt_us: Option<i64>) {
        let time = self.link.clock_micros();
        self.link.capture_app_session_state(&mut self.session_state);
        let bpm = self.session_state.tempo();
        let compensation = rtt_us.unwrap_or(0) as f64 / 2_000_000.0 * bpm / 60.0;
        let compensated = beat + compensation;
        self.session_state.force_beat_at_time(compensated, time, self.quantum);
        self.link.commit_app_session_state(&self.session_state);
        self.detector.arm_echo_guard(Instant::now() + ECHO_GUARD_DURATION);
        info!(beat, compensated, rtt_us, "Forced beat position for join-time sync");
    }

    /// Apply a remote tempo change to the local Link session.
    pub fn set_tempo(&mut self, bpm: f64) {
        let time = self.link.clock_micros();
        self.link.capture_app_session_state(&mut self.session_state);
        self.session_state.set_tempo(bpm, time);
        self.link.commit_app_session_state(&self.session_state);
        self.detector.set_last_tempo(bpm);
        self.detector.arm_echo_guard(Instant::now() + ECHO_GUARD_DURATION);
        debug!(bpm, "Applied remote tempo to Link");
    }

    /// Get current Link state.
    pub fn state(&mut self) -> LinkState {
        let time = self.link.clock_micros();
        self.link.capture_app_session_state(&mut self.session_state);
        LinkState {
            bpm: self.session_state.tempo(),
            beat: self.session_state.beat_at_time(time, self.quantum),
            phase: self.session_state.phase_at_time(time, self.quantum),
            quantum: self.quantum,
            timestamp_us: time,
            num_peers: self.link.num_peers(),
        }
    }

    /// Check if the tempo has changed since last check (respecting echo guard).
    fn check_tempo_change(&mut self) -> Option<LinkEvent> {
        let time = self.link.clock_micros();
        self.link.capture_app_session_state(&mut self.session_state);
        let bpm = self.session_state.tempo();

        self.detector.check(bpm, Instant::now()).map(|bpm| {
            let beat = self.session_state.beat_at_time(time, self.quantum);
            LinkEvent::TempoChanged {
                bpm,
                beat,
                timestamp_us: time,
            }
        })
    }

    /// Spawn a polling task that monitors the local Link session and sends events.
    pub fn spawn_poller(
        mut self,
    ) -> (
        mpsc::UnboundedSender<LinkCommand>,
        mpsc::UnboundedReceiver<LinkEvent>,
    ) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<LinkCommand>();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(POLL_INTERVAL);
            let mut snapshot_counter: u32 = 0;

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Check for tempo change
                        if let Some(event) = self.check_tempo_change() {
                            if event_tx.send(event).is_err() {
                                warn!("Link event receiver dropped — stopping poller");
                                break;
                            }
                        }

                        // Send periodic state snapshot (every ~200ms = 10 ticks)
                        snapshot_counter += 1;
                        if snapshot_counter >= SNAPSHOT_INTERVAL_TICKS {
                            snapshot_counter = 0;
                            let s = self.state();
                            if event_tx.send(LinkEvent::StateUpdate {
                                bpm: s.bpm,
                                beat: s.beat,
                                phase: s.phase,
                                quantum: s.quantum,
                                timestamp_us: s.timestamp_us,
                            }).is_err() {
                                warn!("Link event receiver dropped — stopping poller");
                                break;
                            }
                        }
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(LinkCommand::SetTempo(bpm)) => {
                                self.set_tempo(bpm);
                            }
                            Some(LinkCommand::ForceBeat { beat, rtt_us }) => {
                                self.force_beat(beat, rtt_us);
                            }
                            Some(LinkCommand::GetState(tx)) => {
                                if tx.send(self.state()).is_err() {
                                    debug!("GetState caller dropped receiver");
                                }
                            }
                            None => break, // channel closed
                        }
                    }
                }
            }
        });

        (cmd_tx, event_rx)
    }
}

/// Commands sent to the Link bridge polling task.
pub enum LinkCommand {
    SetTempo(f64),
    /// Snap the local beat clock to the given position (join-time sync only).
    /// `rtt_us` is used to compensate for one-way network transit time.
    ForceBeat { beat: f64, rtt_us: Option<i64> },
    GetState(tokio::sync::oneshot::Sender<LinkState>),
}

/// Snapshot of the current Link session state.
#[derive(Debug, Clone)]
pub struct LinkState {
    pub bpm: f64,
    pub beat: f64,
    pub phase: f64,
    pub quantum: f64,
    pub timestamp_us: i64,
    pub num_peers: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn detector(bpm: f64) -> TempoChangeDetector {
        TempoChangeDetector::new(bpm)
    }

    #[test]
    fn above_threshold_emits_change() {
        let mut d = detector(120.0);
        let now = Instant::now();
        let result = d.check(120.02, now);
        assert_eq!(result, Some(120.02));
    }

    #[test]
    fn below_threshold_ignored() {
        let mut d = detector(120.0);
        let now = Instant::now();
        assert!(d.check(120.005, now).is_none());
    }

    #[test]
    fn at_threshold_boundary() {
        let mut d = detector(120.0);
        let now = Instant::now();
        // Use a value whose f64 difference from 120.0 lands exactly at the threshold.
        // Due to IEEE 754, 120.01 - 120.0 > 0.01 by ~1e-14, so it *does* trigger.
        // Verify the boundary: 0.009 is below threshold, 0.011 is above.
        assert!(d.check(120.009, now).is_none());
        assert!(d.check(120.02, now).is_some()); // reset baseline
        let mut d2 = detector(120.0);
        assert!(d2.check(120.011, now).is_some());
    }

    #[test]
    fn echo_guard_suppresses_detection() {
        let mut d = detector(120.0);
        let now = Instant::now();
        d.arm_echo_guard(now + Duration::from_millis(150));

        // Large change while guard is active → suppressed
        assert!(d.check(130.0, now).is_none());
        // Still suppressed 100ms later
        assert!(d.check(130.0, now + Duration::from_millis(100)).is_none());
    }

    #[test]
    fn echo_guard_expires_allows_detection() {
        let mut d = detector(120.0);
        let now = Instant::now();
        d.arm_echo_guard(now + Duration::from_millis(150));

        // After guard expires, change is detected
        let after = now + Duration::from_millis(151);
        assert_eq!(d.check(130.0, after), Some(130.0));
    }

    #[test]
    fn echo_guard_clears_after_expiry() {
        let mut d = detector(120.0);
        let now = Instant::now();
        d.arm_echo_guard(now + Duration::from_millis(150));

        // First check after expiry clears the guard
        let t1 = now + Duration::from_millis(200);
        d.check(130.0, t1);

        // Second change should also work (guard is cleared, not re-armed)
        let t2 = t1 + Duration::from_millis(10);
        assert_eq!(d.check(140.0, t2), Some(140.0));
    }

    #[test]
    fn last_tempo_tracks_across_changes() {
        let mut d = detector(120.0);
        let now = Instant::now();

        d.check(125.0, now);
        assert_eq!(d.last_tempo(), 125.0);

        d.check(130.0, now);
        assert_eq!(d.last_tempo(), 130.0);

        // Below-threshold from 130: baseline unchanged
        d.check(130.005, now);
        assert_eq!(d.last_tempo(), 130.0);
    }

    #[test]
    fn negative_tempo_change_detected() {
        let mut d = detector(120.0);
        let now = Instant::now();
        assert_eq!(d.check(119.0, now), Some(119.0));
    }

    #[test]
    fn set_last_tempo_updates_baseline() {
        let mut d = detector(120.0);
        let now = Instant::now();
        // Simulate set_tempo() updating baseline to 130
        d.set_last_tempo(130.0);

        // 130.005 is within threshold → no change
        assert!(d.check(130.005, now).is_none());
        // 130.02 exceeds threshold → change
        assert_eq!(d.check(130.02, now), Some(130.02));
    }

    // RED TEST — Critical #2: NaN BPM poisons the tempo change detector.
    //
    // A remote peer can send `TempoChange { bpm: NaN }` (malicious or buggy
    // JSON). `set_last_tempo(NaN)` stores NaN as the baseline. From that
    // point on, EVERY `check()` computes `(new_bpm - NaN).abs()` = NaN,
    // and `NaN > 0.01` is false, so NO future tempo change is ever
    // detected. The detector is permanently broken for this session.
    //
    // Real-world impact: If any peer in a jam session has a bug that sends
    // NaN BPM, all other peers' tempo sync stops working permanently.
    // They'd have to leave and rejoin the room to recover. The NaN also
    // propagates to Link SDK via set_tempo(NaN), which is undefined
    // behavior in the C++ SDK.
    //
    // Expected: NaN BPM should be rejected (check returns None, baseline
    // remains unchanged).
    #[test]
    fn nan_bpm_does_not_poison_detector() {
        let mut d = detector(120.0);
        let now = Instant::now();

        // A remote peer sends NaN BPM
        d.set_last_tempo(f64::NAN);

        // The detector should still work — NaN must not have been stored
        // as the baseline. A legitimate tempo change should be detected.
        let result = d.check(130.0, now);
        assert_eq!(
            result,
            Some(130.0),
            "After NaN BPM, detector must still detect legitimate changes. \
             Got None because NaN poisoned the baseline: (130.0 - NaN).abs() > 0.01 is false"
        );
    }

    // RED TEST — Critical #2b: zero BPM is passed to Link SDK unchecked.
    //
    // `set_last_tempo(0.0)` stores 0 as the baseline, which isn't harmful
    // to the detector itself. But the real issue is upstream: session.rs
    // calls `LinkCommand::SetTempo(0.0)` → `LinkBridge::set_tempo(0.0)` →
    // `session_state.set_tempo(0.0, time)` on the C FFI. Ableton Link's
    // SDK behavior with 0 BPM is implementation-defined and could freeze
    // or corrupt the timeline.
    //
    // This test verifies the detector rejects zero BPM at the check() level.
    #[test]
    fn zero_bpm_rejected_by_detector() {
        let mut d = detector(120.0);
        let now = Instant::now();

        // Remote peer sends 0 BPM (bug or malicious)
        let result = d.check(0.0, now);
        assert!(
            result.is_none(),
            "Zero BPM should be rejected by the detector, but got Some(0.0). \
             A 0 BPM value would be forwarded to the Ableton Link C FFI, \
             which could freeze the session timeline."
        );
        // Baseline must remain at 120.0
        assert_eq!(d.last_tempo(), 120.0, "Baseline must not change after zero BPM");
    }

    // RED TEST — Critical #2c: negative BPM is passed to Link SDK unchecked.
    #[test]
    fn negative_bpm_rejected_by_detector() {
        let mut d = detector(120.0);
        let now = Instant::now();

        let result = d.check(-120.0, now);
        assert!(
            result.is_none(),
            "Negative BPM should be rejected by the detector, but got Some(-120.0). \
             A negative BPM value would corrupt the Ableton Link session."
        );
        assert_eq!(d.last_tempo(), 120.0, "Baseline must not change after negative BPM");
    }
}
