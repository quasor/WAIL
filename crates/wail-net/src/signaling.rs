use std::time::Duration;

use anyhow::Result;
use reqwest::Client;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use wail_core::protocol::SignalMessage;

/// HTTP polling signaling client that connects to a Val Town HTTP endpoint,
/// joins a room, and relays WebRTC signaling messages via polling.
pub struct SignalingClient {
    pub incoming_rx: mpsc::UnboundedReceiver<SignalMessage>,
    pub outgoing_tx: mpsc::UnboundedSender<SignalMessage>,
}

/// A public room returned by the signaling server's list endpoint.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PublicRoom {
    pub room: String,
    pub created_at: i64,
    pub peer_count: u32,
    pub display_names: Vec<String>,
    pub bpm: Option<f64>,
}

#[derive(serde::Deserialize)]
struct ListResponse {
    rooms: Vec<PublicRoom>,
}

/// Fetch the list of public rooms from a signaling server.
pub async fn list_public_rooms(base_url: &str) -> Result<Vec<PublicRoom>> {
    let client = Client::new();
    let base = base_url.trim_end_matches('/');
    let resp = client
        .get(format!("{base}/?action=list"))
        .send()
        .await?
        .error_for_status()?;
    let list: ListResponse = resp.json().await?;
    Ok(list.rooms)
}

/// Response from the `?action=join` endpoint.
#[derive(serde::Deserialize)]
struct JoinResponse {
    peers: Vec<String>,
}

/// A single queued message returned by `?action=poll`.
#[derive(serde::Deserialize)]
struct PollMessage {
    seq: i64,
    body: SignalMessage,
}

/// Response from the `?action=poll` endpoint.
#[derive(serde::Deserialize)]
struct PollResponse {
    messages: Vec<PollMessage>,
    /// Set to `true` by the server when this peer has been evicted (stale heartbeat).
    #[serde(default)]
    evicted: bool,
}

impl SignalingClient {
    /// Connect to the HTTP signaling server and join a room.
    ///
    /// Sends a `join` request, then spawns a background polling loop that:
    /// - Drains outgoing signals and POSTs them as `?action=signal`
    /// - Polls `?action=poll` at the configured interval
    ///
    /// Pass `None` for `password` to create/join a public room.
    pub async fn connect(base_url: &str, room: &str, peer_id: &str, password: Option<&str>) -> Result<Self> {
        Self::connect_with_poll_interval(base_url, room, peer_id, password, 5_000).await
    }

    /// Connect with a custom poll interval (milliseconds).
    pub async fn connect_with_poll_interval(
        base_url: &str,
        room: &str,
        peer_id: &str,
        password: Option<&str>,
        poll_interval_ms: u64,
    ) -> Result<Self> {
        Self::connect_with_options(base_url, room, peer_id, password, poll_interval_ms, 1).await
    }

    /// Connect with full options including stream count.
    pub async fn connect_with_options(
        base_url: &str,
        room: &str,
        peer_id: &str,
        password: Option<&str>,
        poll_interval_ms: u64,
        stream_count: u16,
    ) -> Result<Self> {
        let client = Client::new();
        let base = base_url.trim_end_matches('/').to_string();

        // POST ?action=join
        let mut body = serde_json::json!({
            "room": room,
            "peer_id": peer_id,
            "stream_count": stream_count,
            "client_version": env!("CARGO_PKG_VERSION"),
        });
        if let Some(pw) = password {
            body["password"] = serde_json::Value::String(pw.to_string());
        }
        let resp = client
            .post(format!("{base}/?action=join"))
            .json(&body)
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!("Invalid room password — the room exists and the password doesn't match");
        }

        if resp.status() == reqwest::StatusCode::CONFLICT {
            let error_body: serde_json::Value = resp.json().await.unwrap_or_default();
            let slots = error_body["slots_available"].as_u64().unwrap_or(0);
            anyhow::bail!("Room full — only {slots} stream slots available");
        }

        if resp.status() == reqwest::StatusCode::UPGRADE_REQUIRED {
            let error_body: serde_json::Value = resp.json().await.unwrap_or_default();
            let min_version = error_body["min_version"].as_str().unwrap_or("unknown");
            anyhow::bail!(
                "Your WAIL version ({}) is outdated. Please update to at least version {min_version}.",
                env!("CARGO_PKG_VERSION")
            );
        }

        let join_resp: JoinResponse = resp.error_for_status()?.json().await?;

        info!(
            %base_url, %room, %peer_id,
            existing_peers = join_resp.peers.len(),
            "Joined signaling room via HTTP"
        );

        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<SignalMessage>();

        // Push PeerList so PeerMesh sees existing peers
        if incoming_tx
            .send(SignalMessage::PeerList {
                peers: join_resp.peers,
            })
            .is_err()
        {
            anyhow::bail!("incoming channel closed immediately");
        }

        // Spawn polling loop
        let poll_room = room.to_string();
        let poll_peer = peer_id.to_string();
        tokio::spawn(async move {
            let mut last_seq: i64 = 0;
            let base_poll_ms: u64 = poll_interval_ms;
            let mut current_poll_ms: u64 = base_poll_ms;
            let max_backoff_ms: u64 = 30_000;

            loop {
                tokio::time::sleep(Duration::from_millis(current_poll_ms)).await;

                // Drain all pending outgoing signals and POST them (batch up to 5 per tick)
                let mut sent = 0;
                loop {
                    if sent >= 5 {
                        break;
                    }
                    match outgoing_rx.try_recv() {
                        Ok(msg) => {
                            debug!(?msg, "Sending signal via HTTP");
                            let res = client
                                .post(format!("{base}/?action=signal"))
                                .json(&msg)
                                .send()
                                .await;
                            match res {
                                Ok(resp) if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                                    warn!("Rate limited on signal POST, backing off");
                                    current_poll_ms = (current_poll_ms * 2).min(max_backoff_ms);
                                    break;
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    warn!(error = %e, "Failed to POST signal");
                                }
                            }
                            sent += 1;
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            // Outgoing channel closed — send leave and exit
                            info!("Outgoing channel closed, sending leave");
                            let _ = client
                                .post(format!("{base}/?action=leave"))
                                .json(&serde_json::json!({
                                    "room": poll_room,
                                    "peer_id": poll_peer,
                                }))
                                .send()
                                .await;
                            return;
                        }
                    }
                }

                // GET ?action=poll
                let poll_url = format!(
                    "{base}/?action=poll&room={}&peer_id={}&after={}",
                    poll_room, poll_peer, last_seq
                );
                match client.get(&poll_url).send().await {
                    Ok(resp) if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                        current_poll_ms = (current_poll_ms * 2).min(max_backoff_ms);
                        warn!(backoff_ms = current_poll_ms, "Rate limited, backing off");
                    }
                    Ok(resp) => {
                        // Successful response — restore base interval
                        current_poll_ms = base_poll_ms;

                        let text = match resp.text().await {
                            Ok(t) => t,
                            Err(e) => {
                                warn!(error = %e, "Failed to read poll response body");
                                continue;
                            }
                        };
                        match serde_json::from_str::<PollResponse>(&text) {
                            Ok(poll) => {
                                if poll.evicted {
                                    warn!("Server indicates peer was evicted (stale heartbeat) — triggering reconnection");
                                    return; // Drop incoming_tx, closing the channel → session sees Ok(None)
                                }
                                for pm in poll.messages {
                                    if pm.seq > last_seq {
                                        last_seq = pm.seq;
                                    }
                                    debug!(?pm.body, seq = pm.seq, "Poll received");
                                    if incoming_tx.send(pm.body).is_err() {
                                        info!("Incoming channel closed, stopping poll loop");
                                        return;
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, body = %text, "Failed to parse poll response");
                            }
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "Poll request failed");
                        current_poll_ms = (current_poll_ms * 2).min(max_backoff_ms);
                    }
                }
            }
        });

        Ok(Self {
            incoming_rx,
            outgoing_tx,
        })
    }
}
