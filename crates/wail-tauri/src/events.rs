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
    /// 1-based slot number corresponding to the DAW aux output ("Peer N").
    /// Populated when the peer's identity is known for affinity tracking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
    /// Peer connection status: "connecting", "reconnecting", "connected".
    pub status: String,
    /// Audio was sent to this peer since the last status tick.
    pub is_sending: bool,
    /// Audio was received from this peer since the last status tick.
    pub is_receiving: bool,
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
    pub audio_bytes_sent: u64,
    pub audio_bytes_recv: u64,
    pub audio_dc_open: bool,
    pub plugin_connected: bool,
    pub test_tone_enabled: bool,
    pub audio_send_gated: bool,
    pub recording: bool,
    pub recording_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerReconnectingEvent {
    pub peer_id: String,
    pub attempt: u32,
    pub max_attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStale {
    pub attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub level: String,
    pub message: String,
}
