use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Max payload per DataChannel message.  Keep each chunk small enough
/// to fit in a single DTLS record / UDP datagram (~1200 bytes MTU on
/// typical internet paths).  webrtc-rs SCTP fragmentation of large
/// messages is unreliable over real networks, so we chunk at the
/// application level instead.
const CHUNK_MAX: usize = 1200;
/// Magic bytes for chunked audio messages.
const CHUNK_MAGIC: &[u8; 4] = b"WACH";
const CHUNK_HEADER_SIZE: usize = 8; // 4 magic + 4 total_len
const CHUNK_MAX_PAYLOAD: usize = CHUNK_MAX - CHUNK_HEADER_SIZE;

/// Reassembly buffer for chunked audio messages arriving on a DataChannel.
struct AudioReassembly {
    buffer: Vec<u8>,
    expected_len: usize,
}

/// Create a shared audio message handler that reassembles chunked messages.
/// Returns a closure suitable for `dc.on_message()`.
fn make_audio_handler(
    tx: mpsc::Sender<Vec<u8>>,
) -> (
    Arc<Mutex<Option<AudioReassembly>>>,
    impl Fn(DataChannelMessage) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync + 'static,
) {
    let reassembly = Arc::new(Mutex::new(None::<AudioReassembly>));
    let reassembly_clone = reassembly.clone();

    let handler = move |msg: DataChannelMessage| {
        let tx = tx.clone();
        let reassembly = reassembly_clone.clone();
        Box::pin(async move {
            let data = msg.data.to_vec();

            // Check if this is a chunked message
            if data.len() >= CHUNK_HEADER_SIZE && &data[0..4] == CHUNK_MAGIC {
                let total_len = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
                let payload = &data[CHUNK_HEADER_SIZE..];

                let mut guard = match reassembly.lock() {
                    Ok(g) => g,
                    Err(e) => {
                        debug!("Audio reassembly mutex poisoned, resetting");
                        e.into_inner()
                    }
                };
                let state = guard.get_or_insert_with(|| AudioReassembly {
                    buffer: Vec::with_capacity(total_len),
                    expected_len: total_len,
                });
                state.buffer.extend_from_slice(payload);

                if state.buffer.len() >= state.expected_len {
                    let complete = std::mem::take(&mut state.buffer);
                    *guard = None;
                    debug!("[DC AUDIO IN] reassembled chunked {} bytes", complete.len());
                    if tx.try_send(complete).is_err() {
                        debug!("[DC AUDIO IN] channel full — dropping reassembled frame");
                    }
                }
            } else {
                // Non-chunked message (small enough to fit in one DC message)
                debug!("[DC AUDIO IN] non-chunked {} bytes", data.len());
                if tx.try_send(data).is_err() {
                    debug!("[DC AUDIO IN] channel full — dropping frame");
                }
            }
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    };

    (reassembly, handler)
}
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
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use wail_core::protocol::SyncMessage;

/// Drain all pending sync messages from the queue.
fn take_pending(pending: &Mutex<Vec<String>>) -> Vec<String> {
    match pending.lock() {
        Ok(mut guard) => std::mem::take(&mut *guard),
        Err(e) => std::mem::take(&mut *e.into_inner()),
    }
}

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
    /// Sync messages queued before the DataChannel is open.
    pending_sync: Arc<Mutex<Vec<String>>>,
    /// ICE candidates that arrived before remote description was set
    pending_candidates: Vec<RTCIceCandidateInit>,
    remote_desc_set: bool,
}

impl PeerConnection {
    /// Create a new peer connection with the given ICE servers.
    /// `failure_tx` receives the remote peer ID when the connection fails or disconnects.
    pub async fn new(
        remote_peer_id: String,
        ice_servers: Vec<RTCIceServer>,
        relay_only: bool,
        failure_tx: mpsc::UnboundedSender<String>,
    ) -> Result<Self> {
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
            ice_servers,
            ice_transport_policy: if relay_only {
                RTCIceTransportPolicy::Relay
            } else {
                RTCIceTransportPolicy::Unspecified
            },
            ..Default::default()
        };

        let pc = Arc::new(api.new_peer_connection(config).await?);
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (audio_tx, audio_rx) = mpsc::channel(64);

        // Monitor connection state — notify failure channel on Failed/Disconnected
        let rpid = remote_peer_id.clone();
        let fail_tx = failure_tx;
        pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            match state {
                RTCPeerConnectionState::Failed => {
                    warn!(peer = %rpid, "WebRTC connection FAILED — ICE negotiation could not establish a path");
                    let _ = fail_tx.send(rpid.clone());
                }
                RTCPeerConnectionState::Disconnected => {
                    warn!(peer = %rpid, "WebRTC connection disconnected");
                    let _ = fail_tx.send(rpid.clone());
                }
                _ => {
                    info!(peer = %rpid, %state, "Peer connection state changed");
                }
            }
            Box::pin(async {})
        }));

        // Monitor ICE connection state (checking → connected → completed → failed)
        let rpid = remote_peer_id.clone();
        pc.on_ice_connection_state_change(Box::new(move |state: RTCIceConnectionState| {
            match state {
                RTCIceConnectionState::Failed => {
                    warn!(peer = %rpid, "ICE connection FAILED — no viable candidate pair found (may need TURN)");
                }
                RTCIceConnectionState::Disconnected => {
                    warn!(peer = %rpid, "ICE connection disconnected");
                }
                _ => {
                    info!(peer = %rpid, state = %state, "ICE connection state changed");
                }
            }
            Box::pin(async {})
        }));

        // Monitor ICE gathering state (new → gathering → complete)
        let rpid = remote_peer_id.clone();
        pc.on_ice_gathering_state_change(Box::new(move |state: RTCIceGathererState| {
            info!(peer = %rpid, state = %state, "ICE gathering state changed");
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
            pending_sync: Arc::new(Mutex::new(Vec::new())),
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
        let rpid = self.remote_peer_id.clone();
        self.pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
            let ice_tx = ice_tx.clone();
            let rpid = rpid.clone();
            Box::pin(async move {
                if let Some(c) = candidate {
                    info!(peer = %rpid, typ = %c.typ, address = %c.address, port = c.port, "ICE candidate discovered");
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
        let pending_sync = self.pending_sync.clone();

        self.pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let incoming_tx = incoming_tx.clone();
            let audio_tx = audio_tx.clone();
            let rpid = rpid.clone();
            let dc_sync_slot = dc_sync_slot.clone();
            let dc_audio_slot = dc_audio_slot.clone();
            let pending_sync = pending_sync.clone();
            Box::pin(async move {
                let label = dc.label().to_string();
                info!(peer = %rpid, label = %label, "Data channel opened by remote");

                match label.as_str() {
                    "sync" => {
                        let _ = dc_sync_slot.set(dc.clone());

                        // Flush pending messages when channel opens
                        let pending2 = pending_sync.clone();
                        let rpid2 = rpid.clone();
                        let dc_for_flush = dc.clone();
                        dc.on_open(Box::new(move || {
                            info!(peer = %rpid2, "Sync channel open (responder)");
                            Box::pin(async move {
                                let messages = take_pending(&pending2);
                                if !messages.is_empty() {
                                    info!(count = messages.len(), "Flushing queued sync messages (responder)");
                                }
                                for text in messages {
                                    if let Err(e) = dc_for_flush.send_text(text).await {
                                        warn!("Failed to send queued sync message: {e}");
                                    }
                                }
                            })
                        }));

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
                        let rpid_audio = rpid.clone();
                        let dc_audio_clone = dc.clone();
                        dc.on_open(Box::new(move || {
                            info!(peer = %rpid_audio, label = %dc_audio_clone.label(), "Audio channel open (responder)");
                            Box::pin(async {})
                        }));
                        let (_reassembly, handler) = make_audio_handler(audio_tx.clone());
                        dc.on_message(Box::new(handler));
                    }
                    other => {
                        debug!(peer = %rpid, label = %other, "Ignoring unknown data channel");
                    }
                }
            })
        }));

        let (ice_tx, ice_rx) = mpsc::unbounded_channel();
        let rpid2 = self.remote_peer_id.clone();
        self.pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
            let ice_tx = ice_tx.clone();
            let rpid2 = rpid2.clone();
            Box::pin(async move {
                if let Some(c) = candidate {
                    info!(peer = %rpid2, typ = %c.typ, address = %c.address, port = c.port, "ICE candidate discovered");
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
    /// If the channel isn't open yet, the message is queued and will be
    /// flushed automatically when the channel opens.
    pub async fn send(&self, msg: &SyncMessage) -> Result<()> {
        let text = serde_json::to_string(msg)?;
        match self.dc_sync.get() {
            Some(dc) if dc.ready_state() == RTCDataChannelState::Open => {
                dc.send_text(text).await?;
            }
            _ => {
                let mut guard = self.pending_sync.lock().unwrap_or_else(|e| e.into_inner());
                debug!(peer = %self.remote_peer_id, pending = guard.len() + 1, "Sync DC not open — message queued");
                guard.push(text);
            }
        }
        Ok(())
    }

    /// Send binary audio data over the "audio" DataChannel.
    /// Large messages are chunked to stay under the SCTP max message size.
    pub async fn send_audio(&self, data: &[u8]) -> Result<()> {
        match self.dc_audio.get() {
            Some(dc) if dc.ready_state() == RTCDataChannelState::Open => {
                if data.len() <= CHUNK_MAX {
                    // Fits in a single message — send as-is (no header)
                    info!(peer = %self.remote_peer_id, bytes = data.len(), "[DC AUDIO OUT] sending single message");
                    dc.send(&Bytes::copy_from_slice(data)).await?;
                } else {
                    // Chunk it: each chunk = [WACH][total_len u32 LE][payload]
                    let total_len = data.len() as u32;
                    let mut offset = 0;
                    while offset < data.len() {
                        let end = (offset + CHUNK_MAX_PAYLOAD).min(data.len());
                        let mut chunk = Vec::with_capacity(CHUNK_HEADER_SIZE + (end - offset));
                        chunk.extend_from_slice(CHUNK_MAGIC);
                        chunk.extend_from_slice(&total_len.to_le_bytes());
                        chunk.extend_from_slice(&data[offset..end]);
                        dc.send(&Bytes::from(chunk)).await?;
                        offset = end;
                    }
                    debug!(
                        peer = %self.remote_peer_id,
                        total = data.len(),
                        chunks = (data.len() + CHUNK_MAX_PAYLOAD - 1) / CHUNK_MAX_PAYLOAD,
                        "Sent chunked audio"
                    );
                }
            }
            Some(dc) => {
                debug!(peer = %self.remote_peer_id, state = ?dc.ready_state(), "Audio DataChannel not open — data dropped");
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
        let pending = self.pending_sync.clone();

        let dc_clone = dc.clone();
        dc.on_open(Box::new(move || {
            info!(peer = %rpid, label = %dc_clone.label(), "Sync channel open");
            let dc2 = dc_clone.clone();
            Box::pin(async move {
                let messages = take_pending(&pending);
                if !messages.is_empty() {
                    info!(count = messages.len(), "Flushing queued sync messages");
                }
                for text in messages {
                    if let Err(e) = dc2.send_text(text).await {
                        warn!("Failed to send queued sync message: {e}");
                    }
                }
            })
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
        let rpid = self.remote_peer_id.clone();

        let dc_clone = dc.clone();
        dc.on_open(Box::new(move || {
            info!(peer = %rpid, label = %dc_clone.label(), "Audio channel open");
            Box::pin(async {})
        }));

        let (_reassembly, handler) = make_audio_handler(self.audio_tx.clone());
        dc.on_message(Box::new(handler));

        let _ = self.dc_audio.set(dc);
    }

    /// Check whether the audio DataChannel is in the Open state.
    pub fn is_audio_dc_open(&self) -> bool {
        self.dc_audio
            .get()
            .map_or(false, |dc| dc.ready_state() == RTCDataChannelState::Open)
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
