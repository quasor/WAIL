use std::time::{Duration, Instant};

use rusty_link::{AblLink, SessionState};
use tokio::sync::mpsc;
use tracing::{debug, info};

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

/// Bridge between the local Ableton Link session and the sync engine.
pub struct LinkBridge {
    link: AblLink,
    session_state: SessionState,
    quantum: f64,
    last_tempo: f64,
    /// When set, suppress echo of tempo changes we just applied from remote
    echo_guard_until: Option<Instant>,
}

const TEMPO_CHANGE_THRESHOLD: f64 = 0.01; // BPM
const ECHO_GUARD_DURATION: Duration = Duration::from_millis(150);
const POLL_INTERVAL: Duration = Duration::from_millis(20); // 50 Hz

impl LinkBridge {
    pub fn new(initial_bpm: f64, quantum: f64) -> Self {
        let link = AblLink::new(initial_bpm);
        let session_state = SessionState::new();
        Self {
            link,
            session_state,
            quantum,
            last_tempo: initial_bpm,
            echo_guard_until: None,
        }
    }

    pub fn enable(&self) {
        self.link.enable(true);
        info!(bpm = self.last_tempo, "Ableton Link enabled");
    }

    pub fn disable(&self) {
        self.link.enable(false);
        info!("Ableton Link disabled");
    }

    /// Apply a remote tempo change to the local Link session.
    pub fn set_tempo(&mut self, bpm: f64) {
        let time = self.link.clock_micros();
        self.link.capture_app_session_state(&mut self.session_state);
        self.session_state.set_tempo(bpm, time);
        self.link.commit_app_session_state(&self.session_state);
        self.last_tempo = bpm;
        self.echo_guard_until = Some(Instant::now() + ECHO_GUARD_DURATION);
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
        // If echo guard is active, skip
        if let Some(until) = self.echo_guard_until {
            if Instant::now() < until {
                return None;
            }
            self.echo_guard_until = None;
        }

        let time = self.link.clock_micros();
        self.link.capture_app_session_state(&mut self.session_state);
        let bpm = self.session_state.tempo();

        if (bpm - self.last_tempo).abs() > TEMPO_CHANGE_THRESHOLD {
            self.last_tempo = bpm;
            let beat = self.session_state.beat_at_time(time, self.quantum);
            Some(LinkEvent::TempoChanged {
                bpm,
                beat,
                timestamp_us: time,
            })
        } else {
            None
        }
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
                            let _ = event_tx.send(event);
                        }

                        // Send periodic state snapshot (every ~200ms = 10 ticks)
                        snapshot_counter += 1;
                        if snapshot_counter >= 10 {
                            snapshot_counter = 0;
                            let s = self.state();
                            let _ = event_tx.send(LinkEvent::StateUpdate {
                                bpm: s.bpm,
                                beat: s.beat,
                                phase: s.phase,
                                quantum: s.quantum,
                                timestamp_us: s.timestamp_us,
                            });
                        }
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(LinkCommand::SetTempo(bpm)) => {
                                self.set_tempo(bpm);
                            }
                            Some(LinkCommand::GetState(tx)) => {
                                let _ = tx.send(self.state());
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
