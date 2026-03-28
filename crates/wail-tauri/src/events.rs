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

/// Local send plugin info: one entry per connected WAIL Send plugin instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalSendInfo {
    /// Stream index (0–14) matching the Send plugin's stream_index parameter.
    pub stream_index: u16,
    /// True if audio frames were received from this stream since the last status tick.
    pub is_sending: bool,
    /// User-chosen name for this stream (e.g. "Bass"), if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_name: Option<String>,
}

/// Slot-centric view: one entry per occupied DAW aux output slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotInfo {
    /// 1-based slot number matching DAW aux output.
    pub slot: u32,
    /// Short display ID for logging/UI, e.g. "a1b2c3:0".
    pub short_id: String,
    /// Full persistent client identity (UUID).
    pub client_id: String,
    /// Channel index within the client.
    pub channel_index: u16,
    /// Display name of the peer (if known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Connection status: "connecting", "reconnecting", "connected".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    /// Audio was sent to this peer since the last status tick.
    pub is_sending: bool,
    /// Audio was received from this peer since the last status tick.
    pub is_receiving: bool,
    /// Remote peer's user-chosen name for this stream/channel (e.g. "Bass"), if announced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusUpdate {
    pub bpm: f64,
    pub beat: f64,
    pub phase: f64,
    pub link_peers: u64,
    pub peers: Vec<PeerInfo>,
    pub slots: Vec<SlotInfo>,
    pub local_sends: Vec<LocalSendInfo>,
    pub interval_bars: u32,
    pub audio_sent: u64,
    pub audio_recv: u64,
    pub audio_bytes_sent: u64,
    pub audio_bytes_recv: u64,
    pub audio_dc_open: bool,
    pub plugin_connected: bool,
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
pub struct PeerNetworkInfo {
    pub peer_id: String,
    pub display_name: Option<String>,
    /// 1-based slot number, if assigned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
    /// ICE connection state string (e.g. "connected", "checking", "failed").
    pub ice_state: String,
    /// Sync DataChannel state string (e.g. "open", "closed").
    pub dc_sync_state: String,
    /// Audio DataChannel state string (e.g. "open", "closed").
    pub dc_audio_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    /// Total audio intervals received from this peer.
    pub audio_recv: u64,
    /// How many intervals the remote says it has sent (from their AudioStatus).
    pub intervals_sent_remote: u64,
    /// Delivery percentage: `audio_recv / intervals_sent_remote * 100`.
    pub interval_pct: f64,
    /// Cumulative frames expected across all assembled intervals.
    pub frames_expected: u64,
    /// Cumulative frames actually received (non-gap).
    pub frames_received: u64,
    /// Frame delivery percentage: `frames_received / frames_expected * 100`.
    pub frame_pct: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeersNetwork {
    pub peers: Vec<PeerNetworkInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub level: String,
    pub message: String,
    /// Set for log entries broadcast from a remote peer; None for local entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessageEvent {
    pub sender_name: String,
    pub is_own: bool,
    pub text: String,
}
