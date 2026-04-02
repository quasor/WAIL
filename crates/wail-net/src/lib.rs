pub mod signaling;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{info, warn};

use wail_core::protocol::{PeerFrameReport, SignalMessage, SyncMessage};
use signaling::SignalingClient;

/// Manages communication with all peers in a room via the signaling server.
///
/// All data (sync messages and audio) is relayed through the WebSocket
/// signaling server. There are no direct peer-to-peer connections.
pub struct PeerMesh {
    peer_id: String,
    signaling: SignalingClient,
    /// Control-plane messages from the server (PeerJoined, PeerLeft, etc.)
    incoming_rx: mpsc::UnboundedReceiver<SignalMessage>,
    /// Known peers in the room (presence tracking only).
    peers: HashSet<String>,
    stream_count: u16,
    /// Display names for peers already in the room at join time (from signaling server).
    initial_peer_names: HashMap<String, Option<String>>,
    /// Cumulative count of audio frames dropped due to outgoing channel backpressure.
    audio_send_drops: Arc<AtomicU64>,
}

impl PeerMesh {
    /// Connect to signaling server and join a room.
    /// Returns the mesh plus receivers for sync messages and audio data.
    /// Pass `None` for `password` to create/join a public room.
    pub async fn connect_full(
        server_url: &str,
        room: &str,
        peer_id: &str,
        password: Option<&str>,
        stream_count: u16,
        display_name: Option<&str>,
    ) -> Result<(
        Self,
        mpsc::UnboundedReceiver<(String, SyncMessage)>,
        mpsc::Receiver<(String, Vec<u8>)>,
    )> {
        let (signaling, channels, initial_peer_names) = SignalingClient::connect_with_options(
            server_url, room, peer_id, password, stream_count, display_name,
        ).await?;

        let mesh = Self {
            peer_id: peer_id.to_string(),
            signaling,
            incoming_rx: channels.incoming_rx,
            peers: HashSet::new(),
            stream_count,
            initial_peer_names,
            audio_send_drops: Arc::new(AtomicU64::new(0)),
        };

        Ok((mesh, channels.sync_rx, channels.audio_rx))
    }

    /// Broadcast a sync message to all peers in the room.
    pub async fn broadcast(&self, msg: &SyncMessage) {
        self.signaling.broadcast_sync(msg);
    }

    /// Send a sync message to a specific peer.
    pub async fn send_to(&self, peer_id: &str, msg: &SyncMessage) -> Result<()> {
        self.signaling.send_sync_to(peer_id, msg);
        Ok(())
    }

    /// Broadcast binary audio data to all peers in the room.
    /// Returns an empty vec (no per-peer failures with server relay).
    pub async fn broadcast_audio(&self, data: &[u8]) -> Vec<String> {
        if !self.signaling.send_audio(data) {
            self.audio_send_drops.fetch_add(1, Ordering::Relaxed);
        }
        Vec::new()
    }

    /// Send binary audio data to a specific peer.
    /// Note: the server broadcasts to all room peers; targeted audio is not supported.
    pub async fn send_audio_to(&self, _peer_id: &str, data: &[u8]) -> Result<()> {
        if !self.signaling.send_audio(data) {
            self.audio_send_drops.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Process one signaling event. Call this in a loop.
    pub async fn poll_signaling(&mut self) -> Result<Option<MeshEvent>> {
        let msg = match self.incoming_rx.recv().await {
            Some(m) => m,
            None => return Ok(None),
        };
        self.handle_signal_message(msg)
    }

    /// Handle a single signaling message.
    fn handle_signal_message(&mut self, msg: SignalMessage) -> Result<Option<MeshEvent>> {
        match msg {
            SignalMessage::PeerList { peers } => {
                info!(peers = ?peers, "Received peer list");
                let peer_count = peers.len();
                for remote_id in peers {
                    if remote_id != self.peer_id {
                        self.peers.insert(remote_id);
                    }
                }
                Ok(Some(MeshEvent::PeerListReceived(peer_count)))
            }

            SignalMessage::PeerJoined { peer_id: remote_id, display_name } => {
                info!(peer = %remote_id, name = ?display_name, "New peer joined room");
                self.peers.insert(remote_id.clone());
                Ok(Some(MeshEvent::PeerJoined { peer_id: remote_id, display_name }))
            }

            SignalMessage::PeerLeft { peer_id: remote_id } => {
                info!(peer = %remote_id, "Peer left room");
                self.peers.remove(&remote_id);
                Ok(Some(MeshEvent::PeerLeft(remote_id)))
            }

            SignalMessage::LogBroadcast { from, level, target, message, timestamp_us } => {
                Ok(Some(MeshEvent::PeerLogBroadcast { from, level, target, message, timestamp_us }))
            }

            _ => Ok(None),
        }
    }

    /// Send a structured log entry to the signaling server for broadcast to room peers.
    pub fn send_log(&self, level: &str, target: &str, message: &str, timestamp_us: u64) {
        if let Err(e) = self.signaling.outgoing_tx.send(SignalMessage::LogBroadcast {
            from: String::new(), // server sets `from` on broadcast
            level: level.to_string(),
            target: target.to_string(),
            message: message.to_string(),
            timestamp_us,
        }) {
            tracing::warn!("failed to send log broadcast: {e}");
        }
    }

    /// Send a metrics report to the signaling server (not relayed to peers).
    pub fn send_metrics_report(
        &self,
        dc_open: bool,
        plugin_connected: bool,
        per_peer: HashMap<String, PeerFrameReport>,
        ipc_drops: u64,
        boundary_drift_us: Option<i64>,
    ) {
        if let Err(e) = self.signaling.outgoing_tx.send(SignalMessage::MetricsReport {
            dc_open,
            plugin_connected,
            per_peer,
            ipc_drops,
            boundary_drift_us,
        }) {
            tracing::warn!("failed to send metrics report: {e}");
        }
    }

    /// Get the cumulative audio send drop count (channel backpressure).
    /// Not per-peer — the server relay means all peers share one outgoing channel.
    pub fn dc_audio_drops(&self, _peer_id: &str) -> u64 {
        self.audio_send_drops.load(Ordering::Relaxed)
    }

    pub fn connected_peers(&self) -> Vec<String> {
        self.peers.iter().cloned().collect()
    }

    /// Take the initial peer display names (from the signaling join response).
    pub fn take_initial_peer_names(&mut self) -> HashMap<String, Option<String>> {
        std::mem::take(&mut self.initial_peer_names)
    }

    /// Check whether any peer is connected. Always true if we have peers.
    pub fn any_audio_dc_open(&self) -> bool {
        !self.peers.is_empty()
    }

    /// Check whether a specific peer is connected. Always true for known peers.
    pub fn is_peer_audio_dc_open(&self, peer_id: &str) -> bool {
        self.peers.contains(peer_id)
    }

    /// Remove a peer from tracking.
    pub async fn remove_peer(&mut self, peer_id: &str) {
        self.peers.remove(peer_id);
        info!(peer = %peer_id, "Removed peer from mesh");
    }

    /// Returns (connection_state, sync_state, audio_state) strings for a peer.
    pub fn peer_network_state(&self, peer_id: &str) -> Option<(String, String, String)> {
        if self.peers.contains(peer_id) {
            Some(("connected".to_string(), "open".to_string(), "open".to_string()))
        } else {
            None
        }
    }

    /// Reconnect the signaling WebSocket.
    ///
    /// Returns the initial peer display names plus new sync and audio receivers.
    /// The caller **must** replace its existing `sync_rx` and `audio_rx` with the
    /// returned ones — the old receivers are dead after reconnection.
    pub async fn reconnect_signaling(
        &mut self,
        server_url: &str,
        room: &str,
        password: Option<&str>,
        display_name: Option<&str>,
    ) -> Result<(
        HashMap<String, Option<String>>,
        mpsc::UnboundedReceiver<(String, SyncMessage)>,
        mpsc::Receiver<(String, Vec<u8>)>,
    )> {
        // Suppress the automatic `leave` on the old WebSocket
        self.signaling.suppress_leave_on_close();

        let (new_signaling, channels, initial_peer_names) = SignalingClient::connect_with_options(
            server_url, room, &self.peer_id, password, self.stream_count, display_name,
        ).await?;

        self.signaling = new_signaling;
        self.incoming_rx = channels.incoming_rx;
        self.initial_peer_names = initial_peer_names.clone();

        // The SignalingClient pushed a PeerList as its first message.
        // Consume it here to update our peers set.
        match self.incoming_rx.recv().await {
            Some(SignalMessage::PeerList { peers }) => {
                for remote_id in peers {
                    if remote_id != self.peer_id {
                        self.peers.insert(remote_id);
                    }
                }
            }
            other => {
                warn!(
                    "reconnect_signaling: expected PeerList as first message, got {:?}",
                    other.as_ref().map(std::mem::discriminant)
                );
            }
        }

        Ok((initial_peer_names, channels.sync_rx, channels.audio_rx))
    }
}

/// Events from the peer mesh.
#[derive(Debug)]
pub enum MeshEvent {
    PeerListReceived(usize),
    PeerJoined {
        peer_id: String,
        display_name: Option<String>,
    },
    PeerLeft(String),
    SignalingProcessed,
    /// A structured log entry broadcast by a remote peer.
    PeerLogBroadcast {
        from: String,
        level: String,
        target: String,
        message: String,
        timestamp_us: u64,
    },
}
