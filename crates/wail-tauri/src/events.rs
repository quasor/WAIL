use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStarted {
    pub peer_id: String,
    pub room: String,
    pub bpm: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEnded {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionError {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerJoinedEvent {
    pub peer_id: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerLeftEvent {
    pub peer_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TempoChangedEvent {
    pub bpm: f64,
    /// "local" or "remote"
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub peer_id: String,
    pub display_name: Option<String>,
    pub rtt_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusUpdate {
    pub bpm: f64,
    pub beat: f64,
    pub phase: f64,
    pub link_peers: u64,
    pub peers: Vec<PeerInfo>,
    pub interval_bars: u32,
    pub audio_sent: u64,
    pub audio_recv: u64,
    pub plugin_connected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub level: String,
    pub message: String,
}
