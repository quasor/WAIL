//! Simple chat over the WAIL signaling server.
//!
//! Two agents (or humans) join the same room and exchange text messages
//! relayed through the production signaling WebSocket. No WebRTC needed —
//! messages travel as `signal` payloads through the server.
//!
//! Usage:
//!   cargo run -p wail-e2e --release --bin wail-chat -- --room <ROOM> [--name <NAME>]
//!
//! Then type messages on stdin. They appear on the other peer's stdout.

use std::collections::HashMap;

use anyhow::{bail, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use tokio::io::AsyncBufReadExt;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser)]
#[command(name = "wail-chat", about = "Chat over WAIL signaling server")]
struct Args {
    /// Room name (both peers must use the same room)
    #[arg(long)]
    room: Option<String>,

    /// Display name
    #[arg(long, default_value = "agent")]
    name: String,

    /// Signaling server URL
    #[arg(long, default_value = "wss://wail-signal.fly.dev")]
    server: String,
}

#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum ServerMsg {
    #[serde(rename = "join_ok")]
    JoinOk { peers: Vec<String> },
    #[serde(rename = "join_error")]
    JoinError { code: String },
    #[serde(rename = "peer_joined")]
    PeerJoined { peer_id: String },
    #[serde(rename = "peer_left")]
    PeerLeft { peer_id: String },
    #[serde(rename = "signal")]
    Signal {
        from: String,
        payload: serde_json::Value,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let room = args.room.unwrap_or_else(|| {
        format!("chat-{}", &uuid::Uuid::new_v4().to_string()[..8])
    });
    let peer_id = format!("chat-{}", &uuid::Uuid::new_v4().to_string()[..8]);

    let ws_url = format!("{}/ws", args.server.trim_end_matches('/'));
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url).await?;
    let (mut ws_write, mut ws_read) = ws.split();

    // Join room
    let join_msg = serde_json::json!({
        "type": "join",
        "room": room,
        "peer_id": peer_id,
        "stream_count": 1,
        "display_name": args.name,
        "client_version": env!("CARGO_PKG_VERSION"),
    });
    ws_write.send(Message::Text(join_msg.to_string())).await?;

    // Wait for join_ok
    let mut peers: Vec<String> = Vec::new();
    loop {
        match ws_read.next().await {
            Some(Ok(Message::Text(text))) => {
                match serde_json::from_str::<ServerMsg>(&text)? {
                    ServerMsg::JoinOk { peers: p } => {
                        peers = p;
                        break;
                    }
                    ServerMsg::JoinError { code } => bail!("Join failed: {code}"),
                    _ => {}
                }
            }
            Some(Ok(_)) => continue,
            _ => bail!("WebSocket closed before join"),
        }
    }

    eprintln!("=== wail-chat ===");
    eprintln!("Room:    {room}");
    eprintln!("Peer ID: {peer_id}");
    eprintln!("Server:  {}", args.server);
    if peers.is_empty() {
        eprintln!("\nWaiting for peer... Run on the other machine:");
        eprintln!("  cargo run -p wail-e2e --release --bin wail-chat -- --room {room}");
    } else {
        eprintln!("Peers already in room: {}", peers.join(", "));
    }
    eprintln!("---");

    // Track known peers for broadcasting
    let mut known_peers: HashMap<String, ()> = peers.iter().map(|p| (p.clone(), ())).collect();

    // Read stdin in a separate task
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let reader = tokio::io::BufReader::new(stdin);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if stdin_tx.send(line).is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            Some(line) = stdin_rx.recv() => {
                // Send to all known peers
                for target in known_peers.keys() {
                    let msg = serde_json::json!({
                        "type": "signal",
                        "to": target,
                        "from": peer_id,
                        "payload": {
                            "type": "chat",
                            "name": args.name,
                            "text": line,
                        },
                    });
                    ws_write.send(Message::Text(msg.to_string())).await?;
                }
            }
            Some(ws_msg) = ws_read.next() => {
                match ws_msg {
                    Ok(Message::Text(text)) => {
                        if let Ok(server_msg) = serde_json::from_str::<ServerMsg>(&text) {
                            match server_msg {
                                ServerMsg::PeerJoined { peer_id: rid } => {
                                    eprintln!("[{rid} joined]");
                                    known_peers.insert(rid, ());
                                }
                                ServerMsg::PeerLeft { peer_id: rid } => {
                                    eprintln!("[{rid} left]");
                                    known_peers.remove(&rid);
                                }
                                ServerMsg::Signal { payload, .. } => {
                                    if let (Some(name), Some(text)) = (
                                        payload.get("name").and_then(|v| v.as_str()),
                                        payload.get("text").and_then(|v| v.as_str()),
                                    ) {
                                        println!("{name}: {text}");
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => {
                        eprintln!("[connection closed]");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}
