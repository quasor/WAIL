use std::collections::HashMap;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use wail_core::protocol::{SignalMessage, SyncMessage};

/// WebSocket signaling client that connects to the WAIL signaling server,
/// joins a room, and relays sync messages and audio data.
///
/// Owns the outgoing channels; incoming channels are returned separately
/// from `connect_with_options` so they can be consumed independently.
pub struct SignalingClient {
    /// Outgoing control-plane messages (log, metrics).
    pub outgoing_tx: mpsc::UnboundedSender<SignalMessage>,
    /// Outgoing sync messages (broadcast or targeted).
    sync_outgoing_tx: mpsc::UnboundedSender<SyncOutgoing>,
    /// Outgoing binary audio frames.
    audio_outgoing_tx: mpsc::Sender<Vec<u8>>,
    /// When set, the write task suppresses the automatic `leave` message on close.
    suppress_leave_tx: Option<tokio::sync::watch::Sender<bool>>,
}

/// Internal enum for outgoing sync messages.
enum SyncOutgoing {
    Broadcast(SyncMessage),
    To { peer_id: String, msg: SyncMessage },
}

impl SignalingClient {
    /// Suppress the automatic `leave` message when this client is dropped.
    /// Call this before replacing the client during signaling reconnection.
    pub fn suppress_leave_on_close(&self) {
        if let Some(ref tx) = self.suppress_leave_tx {
            let _ = tx.send(true);
        }
    }

    /// Broadcast a sync message to all peers in the room via the server.
    pub fn broadcast_sync(&self, msg: &SyncMessage) {
        let _ = self.sync_outgoing_tx.send(SyncOutgoing::Broadcast(msg.clone()));
    }

    /// Send a sync message to a specific peer via the server.
    pub fn send_sync_to(&self, peer_id: &str, msg: &SyncMessage) {
        let _ = self.sync_outgoing_tx.send(SyncOutgoing::To {
            peer_id: peer_id.to_string(),
            msg: msg.clone(),
        });
    }

    /// Send a binary audio frame to all peers in the room via the server.
    /// Returns `true` if the frame was queued, `false` if the channel was full (frame dropped).
    pub fn send_audio(&self, data: &[u8]) -> bool {
        match self.audio_outgoing_tx.try_send(data.to_vec()) {
            Ok(()) => true,
            Err(_) => {
                warn!("Audio outgoing channel full — frame dropped");
                false
            }
        }
    }
}

/// A public room returned by the signaling server's list endpoint.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PublicRoom {
    pub room: String,
    pub created_at: i64,
    pub peer_count: u32,
    pub display_names: Vec<String>,
    #[serde(default)]
    pub bpm: Option<f64>,
}

#[derive(serde::Deserialize)]
struct ListResponse {
    rooms: Vec<PublicRoom>,
}

/// Fetch the list of public rooms from a signaling server.
///
/// Uses the HTTP `/rooms` endpoint (not WebSocket).
pub async fn list_public_rooms(base_url: &str) -> Result<Vec<PublicRoom>> {
    let http_url = base_url
        .replace("wss://", "https://")
        .replace("ws://", "http://");
    let base = http_url.trim_end_matches('/');
    let resp = reqwest::Client::new()
        .get(format!("{base}/rooms"))
        .send()
        .await?
        .error_for_status()?;
    let list: ListResponse = resp.json().await?;
    Ok(list.rooms)
}

/// Server response messages (tagged JSON).
#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum ServerMsg {
    #[serde(rename = "join_ok")]
    JoinOk {
        peers: Vec<String>,
        #[serde(default)]
        peer_display_names: HashMap<String, Option<String>>,
    },
    #[serde(rename = "join_error")]
    JoinError {
        code: String,
        #[serde(default)]
        min_version: Option<String>,
        #[serde(default)]
        slots_available: Option<u64>,
    },
    #[serde(rename = "peer_joined")]
    PeerJoined {
        peer_id: String,
        display_name: Option<String>,
    },
    #[serde(rename = "peer_left")]
    PeerLeft {
        peer_id: String,
    },
    /// Legacy WebRTC signaling — kept for backward compatibility but ignored.
    #[serde(rename = "signal")]
    #[allow(dead_code)]
    Signal {
        #[serde(default)]
        to: String,
        #[serde(default)]
        from: String,
        #[serde(default)]
        payload: serde_json::Value,
    },
    /// Sync message relayed by the server from another peer.
    #[serde(rename = "sync")]
    Sync {
        from: String,
        payload: serde_json::Value,
    },
    #[serde(rename = "evicted")]
    Evicted,
    #[serde(rename = "log")]
    Log {
        from: String,
        level: String,
        target: String,
        message: String,
        timestamp_us: u64,
    },
}

/// Channels returned by `SignalingClient::connect_with_options` for incoming data.
pub struct SignalingChannels {
    /// Control-plane messages from the server (PeerJoined, PeerLeft, LogBroadcast, etc.)
    pub incoming_rx: mpsc::UnboundedReceiver<SignalMessage>,
    /// Sync messages relayed from remote peers via the server.
    pub sync_rx: mpsc::UnboundedReceiver<(String, SyncMessage)>,
    /// Binary audio frames relayed from remote peers via the server.
    pub audio_rx: mpsc::Receiver<(String, Vec<u8>)>,
}

impl SignalingClient {
    /// Connect to the WebSocket signaling server and join a room.
    pub async fn connect(
        server_url: &str,
        room: &str,
        peer_id: &str,
        password: Option<&str>,
    ) -> Result<(Self, SignalingChannels, HashMap<String, Option<String>>)> {
        Self::connect_with_options(server_url, room, peer_id, password, 1, None).await
    }

    /// Connect with full options including stream count and display name.
    ///
    /// Returns the client (for sending), incoming channels (for receiving),
    /// and initial peer display names from the join response.
    pub async fn connect_with_options(
        server_url: &str,
        room: &str,
        peer_id: &str,
        password: Option<&str>,
        stream_count: u16,
        display_name: Option<&str>,
    ) -> Result<(Self, SignalingChannels, HashMap<String, Option<String>>)> {
        let ws_url = format!("{}/ws", server_url.trim_end_matches('/'));

        let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url).await?;
        let (mut ws_write, mut ws_read) = ws_stream.split();

        // Send join message
        let mut join_msg = serde_json::json!({
            "type": "join",
            "room": room,
            "peer_id": peer_id,
            "stream_count": stream_count,
            "client_version": env!("CARGO_PKG_VERSION"),
        });
        if let Some(pw) = password {
            join_msg["password"] = serde_json::Value::String(pw.to_string());
        }
        if let Some(name) = display_name {
            join_msg["display_name"] = serde_json::Value::String(name.to_string());
        }
        ws_write
            .send(Message::Text(join_msg.to_string()))
            .await?;

        // Wait for join_ok or join_error
        let join_response = loop {
            match ws_read.next().await {
                Some(Ok(Message::Text(text))) => {
                    break serde_json::from_str::<ServerMsg>(&text)?;
                }
                Some(Ok(Message::Close(_))) | None => {
                    anyhow::bail!("WebSocket closed before join response");
                }
                Some(Err(e)) => {
                    anyhow::bail!("WebSocket error waiting for join response: {e}");
                }
                _ => continue,
            }
        };

        let (peers, initial_peer_names) = match join_response {
            ServerMsg::JoinOk {
                peers,
                peer_display_names,
            } => (peers, peer_display_names),
            ServerMsg::JoinError {
                code,
                min_version,
                slots_available,
            } => match code.as_str() {
                "unauthorized" => {
                    anyhow::bail!(
                        "Invalid room password — the room exists and the password doesn't match"
                    );
                }
                "room_full" => {
                    let slots = slots_available.unwrap_or(0);
                    anyhow::bail!("Room full — only {slots} stream slots available");
                }
                "version_outdated" => {
                    let min = min_version.as_deref().unwrap_or("unknown");
                    anyhow::bail!(
                        "Your WAIL version ({}) is outdated. Please update to at least version {min}.",
                        env!("CARGO_PKG_VERSION")
                    );
                }
                other => anyhow::bail!("Join failed: {other}"),
            },
            _ => anyhow::bail!("Unexpected server message before join_ok"),
        };

        info!(
            %server_url, %room, %peer_id,
            existing_peers = peers.len(),
            "Joined signaling room via WebSocket"
        );

        // Control-plane channel
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        // Sync messages from remote peers
        let (sync_incoming_tx, sync_rx) = mpsc::unbounded_channel();
        // Audio frames from remote peers
        let (audio_incoming_tx, audio_rx) = mpsc::channel(1024);
        // Outgoing control-plane
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<SignalMessage>();
        // Outgoing sync
        let (sync_outgoing_tx, mut sync_outgoing_rx) = mpsc::unbounded_channel::<SyncOutgoing>();
        // Outgoing audio
        let (audio_outgoing_tx, mut audio_outgoing_rx) = mpsc::channel::<Vec<u8>>(256);

        let (suppress_leave_tx, suppress_leave_rx) = tokio::sync::watch::channel(false);

        // Push PeerList so PeerMesh sees existing peers
        if incoming_tx
            .send(SignalMessage::PeerList { peers })
            .is_err()
        {
            anyhow::bail!("incoming channel closed immediately");
        }

        // Spawn read task: server → incoming channels
        tokio::spawn(async move {
            while let Some(msg_result) = ws_read.next().await {
                match msg_result {
                    Ok(Message::Binary(data)) => {
                        // Binary frame: [1 byte: peer_id_len][peer_id][audio_payload]
                        if data.is_empty() {
                            continue;
                        }
                        let pid_len = data[0] as usize;
                        if data.len() < 1 + pid_len {
                            warn!("Binary frame too short for sender header");
                            continue;
                        }
                        let peer_id = match std::str::from_utf8(&data[1..1 + pid_len]) {
                            Ok(s) => s.to_string(),
                            Err(_) => {
                                warn!("Invalid UTF-8 in binary sender header");
                                continue;
                            }
                        };
                        let audio_data = data[1 + pid_len..].to_vec();
                        if audio_incoming_tx.send((peer_id, audio_data)).await.is_err() {
                            info!("Audio incoming channel closed, stopping WS read");
                            return;
                        }
                    }
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<ServerMsg>(&text) {
                            Ok(server_msg) => {
                                match server_msg {
                                    ServerMsg::PeerJoined {
                                        peer_id,
                                        display_name,
                                    } => {
                                        let _ = incoming_tx.send(SignalMessage::PeerJoined {
                                            peer_id,
                                            display_name,
                                        });
                                    }
                                    ServerMsg::PeerLeft { peer_id } => {
                                        let _ = incoming_tx.send(SignalMessage::PeerLeft { peer_id });
                                    }
                                    ServerMsg::Sync { from, payload } => {
                                        match serde_json::from_value::<SyncMessage>(payload) {
                                            Ok(msg) => {
                                                if sync_incoming_tx.send((from, msg)).is_err() {
                                                    info!("Sync incoming channel closed, stopping WS read");
                                                    return;
                                                }
                                            }
                                            Err(e) => {
                                                warn!(error = %e, "Failed to parse relayed sync payload");
                                            }
                                        }
                                    }
                                    ServerMsg::Evicted => {
                                        warn!("Server evicted us — closing signaling");
                                        return;
                                    }
                                    ServerMsg::Log { from, level, target, message, timestamp_us } => {
                                        let _ = incoming_tx.send(SignalMessage::LogBroadcast { from, level, target, message, timestamp_us });
                                    }
                                    ServerMsg::Signal { .. } => {
                                        debug!("Ignoring legacy signal message");
                                    }
                                    _ => {}
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, body = %text, "Failed to parse server message");
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        info!("WebSocket closed by server");
                        return;
                    }
                    Err(e) => {
                        error!(error = %e, "WebSocket read error");
                        return;
                    }
                    _ => {} // ping/pong handled by tungstenite
                }
            }
            info!("WebSocket stream ended");
        });

        // Spawn write task: outgoing channels → server
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    msg = outgoing_rx.recv() => {
                        let Some(msg) = msg else { break };
                        let raw = match &msg {
                            SignalMessage::LogBroadcast { level, target, message, timestamp_us, .. } => {
                                serde_json::json!({
                                    "type": "log",
                                    "level": level,
                                    "target": target,
                                    "message": message,
                                    "timestamp_us": timestamp_us,
                                })
                            }
                            SignalMessage::MetricsReport { dc_open, plugin_connected, per_peer, ipc_drops, boundary_drift_us } => {
                                serde_json::json!({
                                    "type": "metrics_report",
                                    "dc_open": dc_open,
                                    "plugin_connected": plugin_connected,
                                    "per_peer": per_peer,
                                    "ipc_drops": ipc_drops,
                                    "boundary_drift_us": boundary_drift_us,
                                })
                            }
                            _ => continue,
                        };
                        if ws_write.send(Message::Text(raw.to_string())).await.is_err() {
                            warn!("WebSocket write failed — connection lost");
                            return;
                        }
                    }
                    sync_msg = sync_outgoing_rx.recv() => {
                        let Some(sync_msg) = sync_msg else { break };
                        let raw = match sync_msg {
                            SyncOutgoing::Broadcast(msg) => {
                                serde_json::json!({
                                    "type": "sync",
                                    "payload": serde_json::to_value(&msg).unwrap_or_default(),
                                })
                            }
                            SyncOutgoing::To { peer_id, msg } => {
                                serde_json::json!({
                                    "type": "sync_to",
                                    "to": peer_id,
                                    "payload": serde_json::to_value(&msg).unwrap_or_default(),
                                })
                            }
                        };
                        if ws_write.send(Message::Text(raw.to_string())).await.is_err() {
                            warn!("WebSocket write failed — connection lost");
                            return;
                        }
                    }
                    audio_data = audio_outgoing_rx.recv() => {
                        let Some(data) = audio_data else { break };
                        if ws_write.send(Message::Binary(data)).await.is_err() {
                            warn!("WebSocket write failed (audio) — connection lost");
                            return;
                        }
                    }
                }
            }
            // All outgoing channels closed — only send leave if not suppressed
            if *suppress_leave_rx.borrow() {
                info!("Outgoing channel closed, leave suppressed (reconnecting)");
            } else {
                info!("Outgoing channel closed, sending leave");
                let _ = ws_write
                    .send(Message::Text(
                        serde_json::json!({"type": "leave"}).to_string(),
                    ))
                    .await;
            }
            let _ = ws_write.close().await;
        });

        let channels = SignalingChannels {
            incoming_rx,
            sync_rx,
            audio_rx,
        };

        Ok((
            Self {
                outgoing_tx,
                sync_outgoing_tx,
                audio_outgoing_tx,
                suppress_leave_tx: Some(suppress_leave_tx),
            },
            channels,
            initial_peer_names,
        ))
    }
}
