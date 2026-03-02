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
        Self::connect_with_options(server_url, room, peer_id, password, ice_servers, 5_000).await
    }

    /// Connect with custom ICE servers and signaling poll interval.
    pub async fn connect_with_options(
        server_url: &str,
        room: &str,
        peer_id: &str,
        password: Option<&str>,
        ice_servers: Vec<RTCIceServer>,
        poll_interval_ms: u64,
    ) -> Result<(
        Self,
        mpsc::UnboundedReceiver<(String, SyncMessage)>,
        mpsc::Receiver<(String, Vec<u8>)>,
    )> {
        let signaling = SignalingClient::connect_with_poll_interval(
            server_url, room, peer_id, password, poll_interval_ms,
        ).await?;
        let (sync_tx, sync_rx) = mpsc::unbounded_channel();
        let (audio_tx, audio_rx) = mpsc::channel(64);

        let mesh = Self {
            peer_id: peer_id.to_string(),
            peers: HashMap::new(),
            signaling,
            sync_tx,
            audio_tx,
            ice_servers,
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
    pub async fn broadcast_audio(&self, data: &[u8]) {
        for (pid, pc) in &self.peers {
            if let Err(e) = pc.send_audio(data).await {
                warn!(peer = %pid, error = %e, "Failed to send audio to peer");
            }
        }
    }

    /// Send binary audio data to a specific peer.
    pub async fn send_audio_to(&self, peer_id: &str, data: &[u8]) -> Result<()> {
        if let Some(pc) = self.peers.get(peer_id) {
            pc.send_audio(data).await?;
        }
        Ok(())
    }

    /// Process one signaling message. Call this in a loop.
    pub async fn poll_signaling(&mut self) -> Result<Option<MeshEvent>> {
        let msg = match self.signaling.incoming_rx.recv().await {
            Some(m) => m,
            None => return Ok(None),
        };

        match msg {
            SignalMessage::PeerList { peers } => {
                info!(peers = ?peers, "Received peer list");
                for remote_id in peers {
                    // Same tie-breaking as PeerJoined: lower peer_id initiates
                    if remote_id != self.peer_id && self.peer_id < remote_id {
                        self.initiate_connection(&remote_id).await?;
                    }
                }
                Ok(Some(MeshEvent::PeerListReceived))
            }

            SignalMessage::PeerJoined { peer_id: remote_id } => {
                info!(peer = %remote_id, "New peer joined room");
                // Deterministic: lower peer_id initiates
                if self.peer_id < remote_id {
                    self.initiate_connection(&remote_id).await?;
                }
                Ok(Some(MeshEvent::PeerJoined(remote_id)))
            }

            SignalMessage::PeerLeft { peer_id: remote_id } => {
                info!(peer = %remote_id, "Peer left room");
                if let Some(pc) = self.peers.remove(&remote_id) {
                    let _ = pc.close().await;
                }
                Ok(Some(MeshEvent::PeerLeft(remote_id)))
            }

            SignalMessage::Signal { from, payload, .. } => {
                match payload {
                    SignalPayload::Offer { sdp } => {
                        debug!(peer = %from, "Received SDP offer");
                        let mut pc = PeerConnection::new(from.clone(), self.ice_servers.clone()).await?;
                        let (answer_sdp, ice_rx) = pc.handle_offer(sdp).await?;

                        // Send answer
                        self.signaling.outgoing_tx.send(SignalMessage::Signal {
                            to: from.clone(),
                            from: self.peer_id.clone(),
                            payload: SignalPayload::Answer { sdp: answer_sdp },
                        })?;

                        // Spawn ICE candidate sender
                        self.spawn_ice_sender(from.clone(), ice_rx);

                        // Spawn message readers (sync + audio)
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
        info!(peer = %remote_id, "Initiating WebRTC connection");
        let mut pc = PeerConnection::new(remote_id.to_string(), self.ice_servers.clone()).await?;
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
                info!(peer = %rid, bytes = data.len(), "[AUDIO READER] forwarding to mesh");
                match audio_tx.try_send((rid.clone(), data)) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        info!(peer = %rid, "Mesh audio channel full — dropping frame");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
        });
    }

    pub fn connected_peers(&self) -> Vec<String> {
        self.peers.keys().cloned().collect()
    }
}

/// Events from the peer mesh.
#[derive(Debug)]
pub enum MeshEvent {
    PeerListReceived,
    PeerJoined(String),
    PeerLeft(String),
    SignalingProcessed,
}
