use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use wail_core::protocol::SignalMessage;

type PeerSender = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<TcpStream>,
    Message,
>;

/// Room: maps peer_id -> WebSocket sender
type Room = HashMap<String, Arc<Mutex<PeerSender>>>;

/// All rooms: maps room_name -> Room
type Rooms = Arc<RwLock<HashMap<String, Room>>>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "wail_signaling=info".into()),
        )
        .init();

    let port = std::env::args()
        .nth(1)
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(9090);

    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "Signaling server listening");

    let rooms: Rooms = Arc::new(RwLock::new(HashMap::new()));

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let rooms = rooms.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, peer_addr, rooms).await {
                error!(%peer_addr, error = %e, "Connection error");
            }
        });
    }
}

async fn handle_connection(stream: TcpStream, addr: SocketAddr, rooms: Rooms) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (write, mut read) = ws.split();
    let write = Arc::new(Mutex::new(write));

    info!(%addr, "WebSocket connected");

    let mut peer_id: Option<String> = None;
    let mut room_name: Option<String> = None;

    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(e) => {
                warn!(%addr, error = %e, "WebSocket read error");
                break;
            }
        };

        let signal: SignalMessage = match serde_json::from_str(&msg) {
            Ok(s) => s,
            Err(e) => {
                warn!(%addr, error = %e, "Invalid message");
                continue;
            }
        };

        match signal {
            SignalMessage::Join {
                room,
                peer_id: pid,
            } => {
                info!(%addr, %pid, %room, "Peer joining room");
                peer_id = Some(pid.clone());
                room_name = Some(room.clone());

                let mut rooms_lock = rooms.write().await;
                let room_map = rooms_lock.entry(room).or_insert_with(HashMap::new);

                // Send current peer list to the new peer
                let peers: Vec<String> = room_map.keys().cloned().collect();
                let list_msg = serde_json::to_string(&SignalMessage::PeerList { peers })?;
                write.lock().await.send(Message::Text(list_msg.into())).await?;

                // Notify existing peers
                let join_msg =
                    serde_json::to_string(&SignalMessage::PeerJoined { peer_id: pid.clone() })?;
                for (id, sender) in room_map.iter() {
                    if let Err(e) = sender.lock().await.send(Message::Text(join_msg.clone().into())).await {
                        warn!(peer = %id, error = %e, "Failed to notify peer of join");
                    }
                }

                // Add new peer to room
                room_map.insert(pid, write.clone());
            }

            SignalMessage::Signal { to, from, payload } => {
                let rooms_lock = rooms.read().await;
                if let Some(rname) = &room_name {
                    if let Some(room_map) = rooms_lock.get(rname) {
                        if let Some(target) = room_map.get(&to) {
                            let relay = SignalMessage::Signal {
                                to: to.clone(),
                                from,
                                payload,
                            };
                            let msg = serde_json::to_string(&relay)?;
                            if let Err(e) = target.lock().await.send(Message::Text(msg.into())).await {
                                warn!(peer = %to, error = %e, "Failed to relay signal to peer");
                            }
                        }
                    }
                }
            }

            _ => {} // Ignore other message types from clients
        }
    }

    // Cleanup: remove peer from room, notify others
    if let (Some(pid), Some(rname)) = (&peer_id, &room_name) {
        info!(%addr, peer_id = %pid, room = %rname, "Peer disconnected");
        let mut rooms_lock = rooms.write().await;
        if let Some(room_map) = rooms_lock.get_mut(rname) {
            room_map.remove(pid);

            let leave_msg =
                serde_json::to_string(&SignalMessage::PeerLeft { peer_id: pid.clone() })?;
            for (id, sender) in room_map.iter() {
                if let Err(e) = sender.lock().await.send(Message::Text(leave_msg.clone().into())).await {
                    warn!(peer = %id, error = %e, "Failed to notify peer of departure");
                }
            }

            if room_map.is_empty() {
                rooms_lock.remove(rname);
            }
        }
    }

    Ok(())
}
