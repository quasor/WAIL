use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use wail_core::protocol::SyncMessage;

/// A single WebRTC peer connection with a DataChannel for sync messages.
pub struct PeerConnection {
    pub remote_peer_id: String,
    pc: Arc<RTCPeerConnection>,
    dc: Option<Arc<RTCDataChannel>>,
    pub incoming_rx: mpsc::UnboundedReceiver<SyncMessage>,
    incoming_tx: mpsc::UnboundedSender<SyncMessage>,
    /// ICE candidates that arrived before remote description was set
    pending_candidates: Vec<RTCIceCandidateInit>,
    remote_desc_set: bool,
}

impl PeerConnection {
    /// Create a new peer connection.
    pub async fn new(remote_peer_id: String) -> Result<Self> {
        let mut m = MediaEngine::default();
        m.register_default_codecs()?;
        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut m)?;

        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();

        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_string()],
                ..Default::default()
            }],
            ..Default::default()
        };

        let pc = Arc::new(api.new_peer_connection(config).await?);
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();

        // Monitor connection state
        let rpid = remote_peer_id.clone();
        pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            info!(peer = %rpid, %state, "Peer connection state changed");
            Box::pin(async {})
        }));

        Ok(Self {
            remote_peer_id,
            pc,
            dc: None,
            incoming_rx,
            incoming_tx,
            pending_candidates: Vec::new(),
            remote_desc_set: false,
        })
    }

    /// Create an SDP offer (we are the initiator).
    pub async fn create_offer(&mut self) -> Result<(String, mpsc::UnboundedReceiver<RTCIceCandidate>)> {
        // Create the data channel before creating the offer
        let dc = self.pc.create_data_channel("sync", None).await?;
        self.setup_data_channel(dc).await;

        let (ice_tx, ice_rx) = mpsc::unbounded_channel();
        self.pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
            let ice_tx = ice_tx.clone();
            Box::pin(async move {
                if let Some(c) = candidate {
                    let _ = ice_tx.send(c);
                }
            })
        }));

        let offer = self.pc.create_offer(None).await?;
        self.pc.set_local_description(offer.clone()).await?;
        debug!(peer = %self.remote_peer_id, "Created SDP offer");

        Ok((offer.sdp, ice_rx))
    }

    /// Handle an incoming SDP offer (we are the responder) and return our answer.
    pub async fn handle_offer(&mut self, sdp: String) -> Result<(String, mpsc::UnboundedReceiver<RTCIceCandidate>)> {
        // Set up handler for incoming data channel
        let incoming_tx = self.incoming_tx.clone();
        let rpid = self.remote_peer_id.clone();
        self.pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let incoming_tx = incoming_tx.clone();
            let rpid = rpid.clone();
            Box::pin(async move {
                info!(peer = %rpid, label = %dc.label(), "Data channel opened by remote");
                let tx = incoming_tx.clone();
                dc.on_message(Box::new(move |msg: DataChannelMessage| {
                    let tx = tx.clone();
                    Box::pin(async move {
                        if let Ok(text) = String::from_utf8(msg.data.to_vec()) {
                            if let Ok(sync_msg) = serde_json::from_str::<SyncMessage>(&text) {
                                let _ = tx.send(sync_msg);
                            }
                        }
                    })
                }));
            })
        }));

        let (ice_tx, ice_rx) = mpsc::unbounded_channel();
        self.pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
            let ice_tx = ice_tx.clone();
            Box::pin(async move {
                if let Some(c) = candidate {
                    let _ = ice_tx.send(c);
                }
            })
        }));

        let offer = RTCSessionDescription::offer(sdp)?;
        self.pc.set_remote_description(offer).await?;
        self.remote_desc_set = true;

        // Apply any pending ICE candidates
        for candidate in self.pending_candidates.drain(..) {
            self.pc.add_ice_candidate(candidate).await?;
        }

        let answer = self.pc.create_answer(None).await?;
        self.pc.set_local_description(answer.clone()).await?;
        debug!(peer = %self.remote_peer_id, "Created SDP answer");

        Ok((answer.sdp, ice_rx))
    }

    /// Handle an incoming SDP answer.
    pub async fn handle_answer(&mut self, sdp: String) -> Result<()> {
        let answer = RTCSessionDescription::answer(sdp)?;
        self.pc.set_remote_description(answer).await?;
        self.remote_desc_set = true;

        // Apply any pending ICE candidates
        for candidate in self.pending_candidates.drain(..) {
            self.pc.add_ice_candidate(candidate).await?;
        }

        debug!(peer = %self.remote_peer_id, "Set remote answer");
        Ok(())
    }

    /// Add a remote ICE candidate.
    pub async fn add_ice_candidate(&mut self, candidate: String, sdp_mid: Option<String>, sdp_mline_index: Option<u16>) -> Result<()> {
        let init = RTCIceCandidateInit {
            candidate,
            sdp_mid,
            sdp_mline_index,
            ..Default::default()
        };

        if self.remote_desc_set {
            self.pc.add_ice_candidate(init).await?;
        } else {
            self.pending_candidates.push(init);
        }
        Ok(())
    }

    /// Send a sync message over the DataChannel.
    pub async fn send(&self, msg: &SyncMessage) -> Result<()> {
        if let Some(dc) = &self.dc {
            let text = serde_json::to_string(msg)?;
            dc.send_text(text).await?;
        }
        Ok(())
    }

    /// Set up message handling on a data channel.
    async fn setup_data_channel(&mut self, dc: Arc<RTCDataChannel>) {
        let incoming_tx = self.incoming_tx.clone();
        let rpid = self.remote_peer_id.clone();

        let dc_clone = dc.clone();
        dc.on_open(Box::new(move || {
            info!(peer = %rpid, label = %dc_clone.label(), "Data channel open");
            Box::pin(async {})
        }));

        let tx = incoming_tx.clone();
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let tx = tx.clone();
            Box::pin(async move {
                if let Ok(text) = String::from_utf8(msg.data.to_vec()) {
                    match serde_json::from_str::<SyncMessage>(&text) {
                        Ok(sync_msg) => {
                            let _ = tx.send(sync_msg);
                        }
                        Err(e) => {
                            error!(error = %e, "Failed to parse sync message");
                        }
                    }
                }
            })
        }));

        self.dc = Some(dc);
    }

    pub async fn close(&self) -> Result<()> {
        self.pc.close().await?;
        Ok(())
    }
}
