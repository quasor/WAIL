use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use wail_core::protocol::SignalMessage;

/// WebSocket signaling client that connects to the signaling server,
/// joins a room, and relays WebRTC signaling messages.
pub struct SignalingClient {
    pub incoming_rx: mpsc::UnboundedReceiver<SignalMessage>,
    pub outgoing_tx: mpsc::UnboundedSender<SignalMessage>,
}

impl SignalingClient {
    /// Connect to the signaling server and join a room.
    pub async fn connect(server_url: &str, room: &str, peer_id: &str) -> Result<Self> {
        let (ws, _resp) = tokio_tungstenite::connect_async(server_url).await?;
        let (mut write, mut read) = ws.split();

        info!(%server_url, %room, %peer_id, "Connected to signaling server");

        // Send Join message
        let join = SignalMessage::Join {
            room: room.to_string(),
            peer_id: peer_id.to_string(),
        };
        write
            .send(Message::Text(serde_json::to_string(&join)?.into()))
            .await?;

        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<SignalMessage>();

        // Read task: WS -> incoming channel
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    msg = read.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                match serde_json::from_str::<SignalMessage>(&text) {
                                    Ok(signal) => {
                                        debug!(?signal, "Signaling received");
                                        if incoming_tx.send(signal).is_err() {
                                            break;
                                        }
                                    }
                                    Err(e) => warn!(error = %e, "Invalid signaling message"),
                                }
                            }
                            Some(Ok(Message::Close(_))) | None => {
                                info!("Signaling connection closed");
                                break;
                            }
                            Some(Ok(_)) => {} // ignore binary/ping/pong
                            Some(Err(e)) => {
                                error!(error = %e, "Signaling read error");
                                break;
                            }
                        }
                    }
                    outgoing = outgoing_rx.recv() => {
                        match outgoing {
                            Some(msg) => {
                                let text = serde_json::to_string(&msg).unwrap();
                                if let Err(e) = write.send(Message::Text(text.into())).await {
                                    error!(error = %e, "Signaling write error");
                                    break;
                                }
                            }
                            None => break,
                        }
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
