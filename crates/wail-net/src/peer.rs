use std::sync::{Arc, OnceLock};

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc_ice::mdns::MulticastDnsMode;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use wail_core::protocol::SyncMessage;

/// A single WebRTC peer connection with DataChannels for sync and audio.
///
/// Two DataChannels:
/// - "sync": JSON-serialized SyncMessage (text mode, low bandwidth)
/// - "audio": Binary AudioWire frames (binary mode, high bandwidth)
///
/// DataChannel references are stored in `Arc<OnceLock<_>>` so that both
/// the initiator (via `setup_*_channel`) and the responder (via the
/// `on_data_channel` callback) can store them. OnceLock is set-once with
/// no locking overhead on subsequent reads.
pub struct PeerConnection {
    pub remote_peer_id: String,
    pc: Arc<RTCPeerConnection>,
    /// "sync" DataChannel for JSON sync messages (set by initiator or responder)
    dc_sync: Arc<OnceLock<Arc<RTCDataChannel>>>,
    /// "audio" DataChannel for binary interval audio (set by initiator or responder)
    dc_audio: Arc<OnceLock<Arc<RTCDataChannel>>>,
    /// Incoming sync messages (JSON) — taken via `take_sync_rx()` for forwarding
    pub incoming_rx: Option<mpsc::UnboundedReceiver<SyncMessage>>,
    incoming_tx: mpsc::UnboundedSender<SyncMessage>,
    /// Incoming audio data (binary, bounded) — taken via `take_audio_rx()` for forwarding
    pub audio_rx: Option<mpsc::Receiver<Vec<u8>>>,
    audio_tx: mpsc::Sender<Vec<u8>>,
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

        let mut s = SettingEngine::default();
        s.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);

        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .with_setting_engine(s)
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
        let (audio_tx, audio_rx) = mpsc::channel(64);

        // Monitor connection state
        let rpid = remote_peer_id.clone();
        pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            info!(peer = %rpid, %state, "Peer connection state changed");
            Box::pin(async {})
        }));

        Ok(Self {
            remote_peer_id,
            pc,
            dc_sync: Arc::new(OnceLock::new()),
            dc_audio: Arc::new(OnceLock::new()),
            incoming_rx: Some(incoming_rx),
            incoming_tx,
            audio_rx: Some(audio_rx),
            audio_tx,
            pending_candidates: Vec::new(),
            remote_desc_set: false,
        })
    }

    /// Create an SDP offer (we are the initiator).
    pub async fn create_offer(&mut self) -> Result<(String, mpsc::UnboundedReceiver<RTCIceCandidate>)> {
        // Create both data channels before creating the offer
        let dc_sync = self.pc.create_data_channel("sync", None).await?;
        self.setup_sync_channel(dc_sync).await;

        let dc_audio = self.pc.create_data_channel("audio", None).await?;
        self.setup_audio_channel(dc_audio).await;

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
        // Set up handler for incoming data channels (both sync and audio).
        // The OnceLock slots allow the callback to store DC refs so that
        // send() and send_audio() work for the responder too.
        let incoming_tx = self.incoming_tx.clone();
        let audio_tx = self.audio_tx.clone();
        let rpid = self.remote_peer_id.clone();
        let dc_sync_slot = self.dc_sync.clone();
        let dc_audio_slot = self.dc_audio.clone();

        self.pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let incoming_tx = incoming_tx.clone();
            let audio_tx = audio_tx.clone();
            let rpid = rpid.clone();
            let dc_sync_slot = dc_sync_slot.clone();
            let dc_audio_slot = dc_audio_slot.clone();
            Box::pin(async move {
                let label = dc.label().to_string();
                info!(peer = %rpid, label = %label, "Data channel opened by remote");

                match label.as_str() {
                    "sync" => {
                        let _ = dc_sync_slot.set(dc.clone());
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
                    }
                    "audio" => {
                        let _ = dc_audio_slot.set(dc.clone());
                        let tx = audio_tx.clone();
                        dc.on_message(Box::new(move |msg: DataChannelMessage| {
                            let tx = tx.clone();
                            Box::pin(async move {
                                if tx.try_send(msg.data.to_vec()).is_err() {
                                    debug!("Audio channel full — dropping frame");
                                }
                            })
                        }));
                    }
                    other => {
                        debug!(peer = %rpid, label = %other, "Ignoring unknown data channel");
                    }
                }
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

    /// Send a sync message over the "sync" DataChannel (JSON text).
    pub async fn send(&self, msg: &SyncMessage) -> Result<()> {
        match self.dc_sync.get() {
            Some(dc) if dc.ready_state() == RTCDataChannelState::Open => {
                let text = serde_json::to_string(msg)?;
                dc.send_text(text).await?;
            }
            Some(_) => {
                debug!(peer = %self.remote_peer_id, "Sync DataChannel not open yet — message dropped");
            }
            None => {
                debug!(peer = %self.remote_peer_id, "Sync DataChannel not ready — message dropped");
            }
        }
        Ok(())
    }

    /// Send binary audio data over the "audio" DataChannel.
    pub async fn send_audio(&self, data: &[u8]) -> Result<()> {
        match self.dc_audio.get() {
            Some(dc) if dc.ready_state() == RTCDataChannelState::Open => {
                dc.send(&Bytes::copy_from_slice(data)).await?;
            }
            Some(_) => {
                debug!(peer = %self.remote_peer_id, "Audio DataChannel not open yet — data dropped");
            }
            None => {
                debug!(peer = %self.remote_peer_id, "Audio DataChannel not ready — data dropped");
            }
        }
        Ok(())
    }

    /// Set up message handling on the "sync" data channel (initiator path).
    async fn setup_sync_channel(&mut self, dc: Arc<RTCDataChannel>) {
        let incoming_tx = self.incoming_tx.clone();
        let rpid = self.remote_peer_id.clone();

        let dc_clone = dc.clone();
        dc.on_open(Box::new(move || {
            info!(peer = %rpid, label = %dc_clone.label(), "Sync channel open");
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

        let _ = self.dc_sync.set(dc);
    }

    /// Set up message handling on the "audio" data channel (initiator path).
    async fn setup_audio_channel(&mut self, dc: Arc<RTCDataChannel>) {
        let audio_tx = self.audio_tx.clone();
        let rpid = self.remote_peer_id.clone();

        let dc_clone = dc.clone();
        dc.on_open(Box::new(move || {
            info!(peer = %rpid, label = %dc_clone.label(), "Audio channel open");
            Box::pin(async {})
        }));

        let tx = audio_tx.clone();
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let tx = tx.clone();
            Box::pin(async move {
                if tx.try_send(msg.data.to_vec()).is_err() {
                    debug!("Audio channel full — dropping frame");
                }
            })
        }));

        let _ = self.dc_audio.set(dc);
    }

    /// Take the sync message receiver for forwarding to a unified channel.
    /// Can only be called once — returns None on subsequent calls.
    pub fn take_sync_rx(&mut self) -> Option<mpsc::UnboundedReceiver<SyncMessage>> {
        self.incoming_rx.take()
    }

    /// Take the audio data receiver for forwarding to a unified channel.
    /// Can only be called once — returns None on subsequent calls.
    pub fn take_audio_rx(&mut self) -> Option<mpsc::Receiver<Vec<u8>>> {
        self.audio_rx.take()
    }

    pub async fn close(&self) -> Result<()> {
        self.pc.close().await?;
        Ok(())
    }
}
