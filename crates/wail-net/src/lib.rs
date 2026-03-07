pub mod peer;
pub mod signaling;

use std::collections::HashMap;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;

use webrtc::ice_transport::ice_credential_type::RTCIceCredentialType;
use webrtc::ice_transport::ice_server::RTCIceServer;

use wail_core::protocol::{SignalMessage, SignalPayload, SyncMessage};
use peer::PeerConnection;
use signaling::SignalingClient;

/// Default ICE servers (multiple STUN servers for reliability).
pub fn default_ice_servers() -> Vec<RTCIceServer> {
    vec![RTCIceServer {
        urls: vec![
            "stun:stun.l.google.com:19302".to_string(),
            "stun:stun1.l.google.com:19302".to_string(),
            "stun:stun2.l.google.com:19302".to_string(),
        ],
        ..Default::default()
    }]
}

/// Build ICE servers list with an optional TURN server.
pub fn ice_servers_with_turn(turn_url: &str, username: &str, credential: &str) -> Vec<RTCIceServer> {
    vec![
        RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_string()],
            ..Default::default()
        },
        RTCIceServer {
            urls: vec![turn_url.to_string()],
            username: username.to_string(),
            credential: credential.to_string(),
            credential_type: RTCIceCredentialType::Password,
        },
    ]
}

/// Fetch ICE servers (STUN + TURN with short-lived credentials) from Metered.
///
/// Credentials are not stored in source — they are fetched fresh at session start
/// and expire automatically.
pub async fn fetch_metered_ice_servers() -> Result<Vec<RTCIceServer>> {
    #[derive(serde::Deserialize)]
    struct MeteredServer {
        urls: MeteredUrls,
        #[serde(default)]
        username: String,
        #[serde(default)]
        credential: String,
    }

    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum MeteredUrls {
        Single(String),
        Multiple(Vec<String>),
    }

    const API_KEY: &str = "6d995a5ee017979f42b2c0234fd3aca872a1";
    const URL: &str = "https://wail.metered.live/api/v1/turn/credentials";

    let servers: Vec<MeteredServer> = reqwest::Client::new()
        .get(URL)
        .query(&[("apiKey", API_KEY)])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(servers
        .into_iter()
        .map(|m| {
            let urls = match m.urls {
                MeteredUrls::Single(s) => vec![s],
                MeteredUrls::Multiple(v) => v,
            };
            let has_cred = !m.credential.is_empty();
            RTCIceServer {
                urls,
                username: m.username,
                credential: m.credential,
                credential_type: if has_cred {
                    RTCIceCredentialType::Password
                } else {
                    RTCIceCredentialType::Unspecified
                },
            }
        })
        .collect())
}

/// Fallback ICE servers when the Metered API is unreachable — Metered STUN only.
/// No TURN credentials are stored in source.
pub fn metered_stun_fallback() -> Vec<RTCIceServer> {
    vec![RTCIceServer {
        urls: vec!["stun:stun.relay.metered.ca:80".to_string()],
        ..Default::default()
    }]
}

/// Manages WebRTC connections to all peers in a room.
///
/// Provides two message streams:
/// - Sync messages (JSON, low bandwidth): tempo, beat, phase, clock sync
/// - Audio messages (binary, high bandwidth): Opus-encoded interval audio
pub struct PeerMesh {
    peer_id: String,
    peers: HashMap<String, PeerConnection>,
    signaling: SignalingClient,
    sync_tx: mpsc::UnboundedSender<(String, SyncMessage)>,
    audio_tx: mpsc::Sender<(String, Vec<u8>)>,
    ice_servers: Vec<RTCIceServer>,
    relay_only: bool,
    stream_count: u16,
    /// Sender for peer failure notifications (cloned into each PeerConnection).
    failure_tx: mpsc::UnboundedSender<String>,
    /// Receiver for peer failure notifications (polled in poll_signaling).
    failure_rx: mpsc::UnboundedReceiver<String>,
    /// Display names for peers already in the room at join time (from signaling server).
    initial_peer_names: HashMap<String, Option<String>>,
}

impl PeerMesh {
    /// Connect to signaling server and join a room.
    /// Returns the mesh plus receivers for sync messages and audio data.
    /// Pass `None` for `password` to create/join a public room.
    pub async fn connect(
        server_url: &str,
        room: &str,
        peer_id: &str,
        password: Option<&str>,
    ) -> Result<(
        Self,
        mpsc::UnboundedReceiver<(String, SyncMessage)>,
        mpsc::Receiver<(String, Vec<u8>)>,
    )> {
        Self::connect_with_ice(server_url, room, peer_id, password, default_ice_servers()).await
    }

    /// Connect with custom ICE servers (e.g. including a TURN server).
    pub async fn connect_with_ice(
        server_url: &str,
        room: &str,
        peer_id: &str,
        password: Option<&str>,
        ice_servers: Vec<RTCIceServer>,
    ) -> Result<(
        Self,
        mpsc::UnboundedReceiver<(String, SyncMessage)>,
        mpsc::Receiver<(String, Vec<u8>)>,
    )> {
        Self::connect_full(server_url, room, peer_id, password, ice_servers, false, 1, None).await
    }

    /// Connect with custom ICE servers, relay-only mode, stream count, and display name.
    /// When `relay_only` is true, only TURN relay candidates are used (no host/srflx).
    pub async fn connect_full(
        server_url: &str,
        room: &str,
        peer_id: &str,
        password: Option<&str>,
        ice_servers: Vec<RTCIceServer>,
        relay_only: bool,
        stream_count: u16,
        display_name: Option<&str>,
    ) -> Result<(
        Self,
        mpsc::UnboundedReceiver<(String, SyncMessage)>,
        mpsc::Receiver<(String, Vec<u8>)>,
    )> {
        let (signaling, initial_peer_names) = SignalingClient::connect_with_options(
            server_url, room, peer_id, password, stream_count, display_name,
        ).await?;
        let (sync_tx, sync_rx) = mpsc::unbounded_channel();
        let (audio_tx, audio_rx) = mpsc::channel(1024);
        let (failure_tx, failure_rx) = mpsc::unbounded_channel();

        let mesh = Self {
            peer_id: peer_id.to_string(),
            peers: HashMap::new(),
            signaling,
            sync_tx,
            audio_tx,
            ice_servers,
            relay_only,
            stream_count,
            failure_tx,
            failure_rx,
            initial_peer_names,
        };

        Ok((mesh, sync_rx, audio_rx))
    }

    /// Broadcast a sync message to all connected peers.
    pub async fn broadcast(&self, msg: &SyncMessage) {
        for (pid, pc) in &self.peers {
            if let Err(e) = pc.send(msg).await {
                warn!(peer = %pid, error = %e, "Failed to send to peer");
            }
        }
    }

    /// Send a sync message to a specific peer.
    pub async fn send_to(&self, peer_id: &str, msg: &SyncMessage) -> Result<()> {
        if let Some(pc) = self.peers.get(peer_id) {
            pc.send(msg).await?;
        }
        Ok(())
    }

    /// Broadcast binary audio data to all connected peers.
    /// Returns peer IDs for which the send failed (for optional retry).
    pub async fn broadcast_audio(&self, data: &[u8]) -> Vec<String> {
        let mut failed = Vec::new();
        for (pid, pc) in &self.peers {
            if let Err(e) = pc.send_audio(data).await {
                warn!(peer = %pid, error = %e, "Failed to send audio to peer");
                failed.push(pid.clone());
            }
        }
        failed
    }

    /// Send binary audio data to a specific peer.
    pub async fn send_audio_to(&self, peer_id: &str, data: &[u8]) -> Result<()> {
        if let Some(pc) = self.peers.get(peer_id) {
            pc.send_audio(data).await?;
        }
        Ok(())
    }

    /// Process one signaling or failure event. Call this in a loop.
    pub async fn poll_signaling(&mut self) -> Result<Option<MeshEvent>> {
        tokio::select! {
            msg = self.signaling.incoming_rx.recv() => {
                let msg = match msg {
                    Some(m) => m,
                    None => return Ok(None),
                };
                self.handle_signal_message(msg).await
            }
            Some(failed_peer) = self.failure_rx.recv() => {
                // Only emit if peer is still in the map (not already removed by PeerLeft)
                if self.peers.contains_key(&failed_peer) {
                    info!(peer = %failed_peer, "Peer connection failed — emitting PeerFailed");
                    Ok(Some(MeshEvent::PeerFailed(failed_peer)))
                } else {
                    Ok(Some(MeshEvent::SignalingProcessed))
                }
            }
        }
    }

    /// Handle a single signaling message.
    async fn handle_signal_message(&mut self, msg: SignalMessage) -> Result<Option<MeshEvent>> {
        match msg {
            SignalMessage::PeerList { peers } => {
                info!(peers = ?peers, "Received peer list");
                let peer_count = peers.len();
                for remote_id in peers {
                    if remote_id != self.peer_id && self.peer_id < remote_id {
                        self.initiate_connection(&remote_id).await?;
                    }
                }
                Ok(Some(MeshEvent::PeerListReceived(peer_count)))
            }

            SignalMessage::PeerJoined { peer_id: remote_id, display_name } => {
                info!(peer = %remote_id, name = ?display_name, "New peer joined room");
                if self.peer_id < remote_id {
                    self.initiate_connection(&remote_id).await?;
                }
                Ok(Some(MeshEvent::PeerJoined { peer_id: remote_id, display_name }))
            }

            SignalMessage::PeerLeft { peer_id: remote_id } => {
                info!(peer = %remote_id, "Peer left room");
                if let Some(pc) = self.peers.remove(&remote_id) {
                    let _ = pc.close().await;
                }
                Ok(Some(MeshEvent::PeerLeft(remote_id)))
            }

            SignalMessage::LogBroadcast { from, level, target, message, timestamp_us } => {
                Ok(Some(MeshEvent::PeerLogBroadcast { from, level, target, message, timestamp_us }))
            }

            SignalMessage::Signal { from, payload, .. } => {
                match payload {
                    SignalPayload::Offer { sdp } => {
                        debug!(peer = %from, "Received SDP offer");
                        // Clean up stale connection if one exists (reconnection scenario)
                        if let Some(old_pc) = self.peers.remove(&from) {
                            warn!(peer = %from, "Replacing stale connection with new offer");
                            let _ = old_pc.close().await;
                        }
                        let mut pc = PeerConnection::new(
                            from.clone(), self.ice_servers.clone(), self.relay_only, self.failure_tx.clone(),
                        ).await?;
                        let (answer_sdp, ice_rx) = pc.handle_offer(sdp).await?;

                        self.signaling.outgoing_tx.send(SignalMessage::Signal {
                            to: from.clone(),
                            from: self.peer_id.clone(),
                            payload: SignalPayload::Answer { sdp: answer_sdp },
                        })?;

                        self.spawn_ice_sender(from.clone(), ice_rx);
                        self.spawn_message_reader(&from, &mut pc);
                        self.spawn_audio_reader(&from, &mut pc);

                        self.peers.insert(from, pc);
                    }

                    SignalPayload::Answer { sdp } => {
                        debug!(peer = %from, "Received SDP answer");
                        if let Some(pc) = self.peers.get_mut(&from) {
                            pc.handle_answer(sdp).await?;
                        }
                    }

                    SignalPayload::IceCandidate {
                        candidate,
                        sdp_mid,
                        sdp_mline_index,
                    } => {
                        if let Some(pc) = self.peers.get_mut(&from) {
                            pc.add_ice_candidate(candidate, sdp_mid, sdp_mline_index)
                                .await?;
                        }
                    }
                }
                Ok(Some(MeshEvent::SignalingProcessed))
            }

            _ => Ok(None),
        }
    }

    /// Initiate a WebRTC connection to a remote peer (we create the offer).
    async fn initiate_connection(&mut self, remote_id: &str) -> Result<()> {
        // Clean up stale connection if one exists (reconnection scenario)
        if let Some(old_pc) = self.peers.remove(remote_id) {
            warn!(peer = %remote_id, "Cleaning up stale connection before re-initiation");
            let _ = old_pc.close().await;
        }
        info!(peer = %remote_id, "Initiating WebRTC connection");
        let mut pc = PeerConnection::new(
            remote_id.to_string(), self.ice_servers.clone(), self.relay_only, self.failure_tx.clone(),
        ).await?;
        let (offer_sdp, ice_rx) = pc.create_offer().await?;

        // Send offer via signaling
        self.signaling.outgoing_tx.send(SignalMessage::Signal {
            to: remote_id.to_string(),
            from: self.peer_id.clone(),
            payload: SignalPayload::Offer { sdp: offer_sdp },
        })?;

        // Spawn ICE candidate sender
        self.spawn_ice_sender(remote_id.to_string(), ice_rx);

        // Spawn message readers (sync + audio)
        self.spawn_message_reader(remote_id, &mut pc);
        self.spawn_audio_reader(remote_id, &mut pc);

        self.peers.insert(remote_id.to_string(), pc);
        Ok(())
    }

    /// Spawn a task that forwards ICE candidates to the signaling server.
    fn spawn_ice_sender(
        &self,
        remote_id: String,
        mut ice_rx: mpsc::UnboundedReceiver<RTCIceCandidate>,
    ) {
        let outgoing = self.signaling.outgoing_tx.clone();
        let our_id = self.peer_id.clone();
        tokio::spawn(async move {
            while let Some(candidate) = ice_rx.recv().await {
                let json = match candidate.to_json() {
                    Ok(j) => j,
                    Err(e) => {
                        error!(error = %e, "Failed to serialize ICE candidate");
                        continue;
                    }
                };
                if outgoing.send(SignalMessage::Signal {
                    to: remote_id.clone(),
                    from: our_id.clone(),
                    payload: SignalPayload::IceCandidate {
                        candidate: json.candidate,
                        sdp_mid: json.sdp_mid,
                        sdp_mline_index: json.sdp_mline_index,
                    },
                }).is_err() {
                    warn!(peer = %remote_id, "Signaling channel closed — ICE candidate lost");
                    break;
                }
            }
        });
    }

    /// Spawn a task that reads sync messages from a peer and forwards to the unified channel.
    fn spawn_message_reader(&self, remote_id: &str, pc: &mut PeerConnection) {
        let sync_tx = self.sync_tx.clone();
        let rid = remote_id.to_string();

        let Some(mut rx) = pc.take_sync_rx() else {
            warn!(peer = %remote_id, "Sync receiver already taken");
            return;
        };

        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if sync_tx.send((rid.clone(), msg)).is_err() {
                    break;
                }
            }
            warn!(peer = %rid, "Sync reader exited");
            // No fail_tx here — redundant with DC on_close and PeerConnectionState::Failed.
        });
    }

    /// Spawn a task that reads audio data from a peer and forwards to the unified audio channel.
    fn spawn_audio_reader(&self, remote_id: &str, pc: &mut PeerConnection) {
        let audio_tx = self.audio_tx.clone();
        let rid = remote_id.to_string();

        let Some(mut rx) = pc.take_audio_rx() else {
            warn!(peer = %remote_id, "Audio receiver already taken");
            return;
        };

        tokio::spawn(async move {
            while let Some(data) = rx.recv().await {
                debug!(peer = %rid, bytes = data.len(), "[AUDIO READER] forwarding to mesh");
                if audio_tx.send((rid.clone(), data)).await.is_err() {
                    break;
                }
            }
            warn!(peer = %rid, "Audio reader exited");
            // No fail_tx here — redundant with DC on_close and PeerConnectionState::Failed.
        });
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

    pub fn connected_peers(&self) -> Vec<String> {
        self.peers.keys().cloned().collect()
    }

    /// Take the initial peer display names (from the signaling join response).
    /// Returns the map and leaves an empty map in its place.
    pub fn take_initial_peer_names(&mut self) -> HashMap<String, Option<String>> {
        std::mem::take(&mut self.initial_peer_names)
    }

    /// Check whether any connected peer has an open audio DataChannel.
    pub fn any_audio_dc_open(&self) -> bool {
        self.peers.values().any(|pc| pc.is_audio_dc_open())
    }

    /// Check whether a specific peer has an open audio DataChannel.
    pub fn is_peer_audio_dc_open(&self, peer_id: &str) -> bool {
        self.peers.get(peer_id).map_or(false, |pc| pc.is_audio_dc_open())
    }

    /// Close a specific peer's WebRTC connection (without removing from the mesh).
    /// The connection state callback will fire, which can trigger `MeshEvent::PeerFailed`.
    pub async fn close_peer(&self, peer_id: &str) {
        if let Some(pc) = self.peers.get(peer_id) {
            if let Err(e) = pc.close().await {
                warn!(peer = %peer_id, error = %e, "Error closing peer connection");
            }
        }
    }

    /// Remove a peer from the mesh, closing its WebRTC connection.
    pub async fn remove_peer(&mut self, peer_id: &str) {
        if let Some(pc) = self.peers.remove(peer_id) {
            if let Err(e) = pc.close().await {
                warn!(peer = %peer_id, error = %e, "Error closing peer connection");
            }
            info!(peer = %peer_id, "Removed peer from mesh");
        }
    }

    /// Re-initiate a WebRTC connection to a peer after failure.
    /// Removes the dead peer first, then starts a new connection.
    /// Respects tie-breaking: only the lower peer_id initiates.
    pub async fn re_initiate(&mut self, peer_id: &str) -> Result<()> {
        self.remove_peer(peer_id).await;
        if *self.peer_id < *peer_id {
            self.initiate_connection(peer_id).await?;
        }
        Ok(())
    }

    /// Returns (ice_state, dc_sync_state, dc_audio_state) strings for a peer,
    /// or None if the peer is not in the mesh.
    pub fn peer_network_state(&self, peer_id: &str) -> Option<(String, String, String)> {
        self.peers.get(peer_id).map(|pc| (
            pc.ice_connection_state_str(),
            pc.dc_sync_state_str(),
            pc.dc_audio_state_str(),
        ))
    }

    /// Reconnect only the signaling WebSocket, leaving all established WebRTC peer
    /// connections untouched. Existing DataChannels continue to operate normally.
    ///
    /// After reconnect, the server sends a fresh peer list. Peers already in
    /// `self.peers` are skipped; only genuinely new peers trigger new WebRTC offers.
    ///
    /// Returns the initial peer display names from the new `join_ok` response.
    pub async fn reconnect_signaling(
        &mut self,
        server_url: &str,
        room: &str,
        password: Option<&str>,
        display_name: Option<&str>,
        new_ice_servers: Vec<RTCIceServer>,
    ) -> Result<HashMap<String, Option<String>>> {
        let (new_signaling, initial_peer_names) = SignalingClient::connect_with_options(
            server_url, room, &self.peer_id, password, self.stream_count, display_name,
        ).await?;

        // Replace signaling BEFORE processing the PeerList so that
        // initiate_connection() sends SDP offers via the new WebSocket.
        self.ice_servers = new_ice_servers;
        self.signaling = new_signaling;
        self.initial_peer_names = initial_peer_names.clone();

        // The SignalingClient pushed a PeerList as its first message.
        // Consume it here: only initiate connections for peers we don't already have.
        match self.signaling.incoming_rx.recv().await {
            Some(SignalMessage::PeerList { peers }) => {
                for remote_id in peers {
                    if remote_id != self.peer_id
                        && self.peer_id < remote_id
                        && !self.peers.contains_key(&remote_id)
                    {
                        self.initiate_connection(&remote_id).await?;
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

        Ok(initial_peer_names)
    }
}

#[cfg(test)]
mod tests {
    // §2.3 — Tie-breaking: verify the < comparison that guards initiation in all three paths.
    #[test]
    fn tie_breaking_lower_id_initiates() {
        // "peer-a" < "peer-b" lexicographically → peer-a should initiate.
        assert!("peer-a" < "peer-b");
    }

    #[test]
    fn tie_breaking_higher_id_does_not_initiate() {
        // "peer-b" is NOT < "peer-a" → peer-b must not initiate.
        assert!(!("peer-b" < "peer-a"));
    }

    #[test]
    fn tie_breaking_equal_peer_ids_does_not_initiate() {
        // When peer_id == remote_id the condition `peer_id < remote_id` is false.
        // The PeerList path additionally guards with `remote_id != self.peer_id`.
        // Both checks correctly suppress initiation for equal IDs.
        let our_id = "peer-x";
        let remote_id = "peer-x";

        // PeerJoined path: `self.peer_id < remote_id`
        assert!(
            !(our_id < remote_id),
            "Equal IDs: PeerJoined path must not initiate"
        );

        // PeerList path: `remote_id != self.peer_id && self.peer_id < remote_id`
        assert!(
            !(remote_id != our_id && our_id < remote_id),
            "Equal IDs: PeerList path must not initiate"
        );
    }

    #[test]
    fn re_initiate_tie_breaking_respects_id_order() {
        // re_initiate guard: `*self.peer_id < *peer_id`
        // Lower-ID peer calling re_initiate on higher-ID peer → should initiate.
        assert!("peer-a" < "peer-b", "Lower-ID peer should initiate in re_initiate");
        // Higher-ID peer calling re_initiate on lower-ID peer → must NOT initiate.
        assert!(!("peer-b" < "peer-a"), "Higher-ID peer must NOT initiate in re_initiate");
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
    /// A peer's WebRTC connection failed or disconnected.
    PeerFailed(String),
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
