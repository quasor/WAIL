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
}

impl SignalingClient {
    /// Connect to the HTTP signaling server and join a room.
    ///
    /// Sends a `join` request, then spawns a background polling loop that:
    /// - Drains outgoing signals and POSTs them as `?action=signal`
    /// - Polls `?action=poll` every 200ms for incoming messages
    pub async fn connect(base_url: &str, room: &str, peer_id: &str, password: &str) -> Result<Self> {
        let client = Client::new();
        let base = base_url.trim_end_matches('/').to_string();

        // POST ?action=join
        let resp = client
            .post(format!("{base}/?action=join"))
            .json(&serde_json::json!({
                "room": room,
                "peer_id": peer_id,
                "password": password,
            }))
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!("Invalid room password — the room exists and the password doesn't match");
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
            let mut poll_interval = tokio::time::interval(Duration::from_millis(200));

            loop {
                poll_interval.tick().await;

                // Drain all pending outgoing signals and POST them
                loop {
                    match outgoing_rx.try_recv() {
                        Ok(msg) => {
                            debug!(?msg, "Sending signal via HTTP");
                            let res = client
                                .post(format!("{base}/?action=signal"))
                                .json(&msg)
                                .send()
                                .await;
                            if let Err(e) = res {
                                warn!(error = %e, "Failed to POST signal");
                            }
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
                    Ok(resp) => match resp.json::<PollResponse>().await {
                        Ok(poll) => {
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
                            warn!(error = %e, "Failed to parse poll response");
                        }
                    },
                    Err(e) => {
                        error!(error = %e, "Poll request failed");
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
