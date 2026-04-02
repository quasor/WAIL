//! Shared test helpers for wail-plugin-test integration tests.
//!
//! Contains an in-process WebSocket signaling server and a role-aware mini_app
//! session loop that bridges plugin IPC ↔ WebSocket relay, mirroring the audio forwarding
//! logic in wail-tauri/src/session.rs without Tauri, Link, or clock sync.
//!
//! The signaling server and `mini_app_session_v2` are derived from
//! `wail-net/tests/common/mod.rs` and `wail-net/tests/ipc_e2e.rs` respectively.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::future::IntoFuture;
use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use wail_audio::{IpcFramer, IpcMessage, IpcRecvBuffer, IPC_ROLE_RECV};
use wail_core::protocol::SyncMessage;
use wail_net::PeerMesh;

// ---------------------------------------------------------------------------
// In-process WebSocket signaling server
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SignalingState {
    rooms: HashMap<String, Vec<String>>,
    peer_senders: HashMap<String, HashMap<String, tokio::sync::mpsc::UnboundedSender<WsRelay>>>,
    evicted_peers: HashSet<String>,
}

type SharedState = Arc<Mutex<SignalingState>>;

/// Message type for the per-peer relay channel, avoiding unsafe String↔binary coercion.
#[derive(Clone)]
enum WsRelay {
    Text(String),
    Binary(Vec<u8>),
}

async fn handle_ws(ws: WebSocketUpgrade, State(state): State<SharedState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: SharedState) {
    let join_result = handle_join_phase(&mut socket, &state).await;
    let (room, peer_id, _send_tx, send_rx) = match join_result {
        Some(v) => v,
        None => return,
    };

    let (mut ws_tx, ws_rx) = socket.split();
    let write_handle = tokio::spawn(async move {
        let mut send_rx = send_rx;
        while let Some(msg) = send_rx.recv().await {
            let ws_msg = match msg {
                WsRelay::Text(text) => Message::Text(text),
                WsRelay::Binary(data) => Message::Binary(data),
            };
            if ws_tx.send(ws_msg).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    relay_messages(ws_rx, &state, &room, &peer_id).await;
    cleanup_peer(&state, &room, &peer_id).await;
    write_handle.abort();
}

async fn handle_join_phase(
    socket: &mut WebSocket,
    state: &SharedState,
) -> Option<(
    String,
    String,
    tokio::sync::mpsc::UnboundedSender<WsRelay>,
    tokio::sync::mpsc::UnboundedReceiver<WsRelay>,
)> {
    loop {
        let msg = match socket.recv().await {
            Some(Ok(Message::Text(t))) => t,
            Some(Ok(Message::Close(_))) | None => return None,
            _ => continue,
        };

        let parsed: serde_json::Value = match serde_json::from_str(&msg) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if parsed["type"].as_str() != Some("join") {
            continue;
        }

        let room = parsed["room"].as_str().unwrap_or("").to_string();
        let peer_id = parsed["peer_id"].as_str().unwrap_or("").to_string();
        let display_name = parsed["display_name"].as_str().map(|s| s.to_string());

        let mut s = state.lock().await;
        let peers_in_room = s.rooms.entry(room.clone()).or_default();
        let existing: Vec<String> = peers_in_room
            .iter()
            .filter(|p| *p != &peer_id)
            .cloned()
            .collect();
        if !peers_in_room.contains(&peer_id) {
            peers_in_room.push(peer_id.clone());
        }

        let (send_tx, send_rx) = tokio::sync::mpsc::unbounded_channel::<WsRelay>();
        s.peer_senders
            .entry(room.clone())
            .or_default()
            .insert(peer_id.clone(), send_tx.clone());

        // Notify existing peers
        if let Some(room_senders) = s.peer_senders.get(&room) {
            for (id, tx) in room_senders {
                if id != &peer_id {
                    let _ = tx.send(WsRelay::Text(
                        serde_json::json!({
                            "type": "peer_joined",
                            "peer_id": peer_id,
                            "display_name": display_name,
                        })
                        .to_string(),
                    ));
                }
            }
        }

        let peer_display_names: HashMap<String, Option<String>> =
            existing.iter().map(|id| (id.clone(), None)).collect();

        let _ = socket
            .send(Message::Text(
                serde_json::json!({
                    "type": "join_ok",
                    "peers": existing,
                    "peer_display_names": peer_display_names,
                })
                .to_string(),
            ))
            .await;

        return Some((room, peer_id, send_tx, send_rx));
    }
}

async fn relay_messages(
    mut ws_rx: futures_util::stream::SplitStream<WebSocket>,
    state: &SharedState,
    room: &str,
    peer_id: &str,
) {
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Binary(data) => {
                // Binary audio frame: prepend sender header and broadcast to room peers
                let pid_bytes = peer_id.as_bytes();
                let mut frame = Vec::with_capacity(1 + pid_bytes.len() + data.len());
                frame.push(pid_bytes.len() as u8);
                frame.extend_from_slice(pid_bytes);
                frame.extend_from_slice(&data);

                let relay = WsRelay::Binary(frame);
                let s = state.lock().await;
                if let Some(room_senders) = s.peer_senders.get(room) {
                    for (id, tx) in room_senders {
                        if id != peer_id {
                            let _ = tx.send(relay.clone());
                        }
                    }
                }
            }
            Message::Text(text) => {
                let parsed: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                match parsed["type"].as_str().unwrap_or("") {
                    "signal" => {
                        let to = parsed["to"].as_str().unwrap_or("").to_string();
                        let s = state.lock().await;
                        if let Some(room_senders) = s.peer_senders.get(room) {
                            if let Some(tx) = room_senders.get(&to) {
                                let _ = tx.send(WsRelay::Text(text.clone()));
                            }
                        }
                    }
                    "sync" => {
                        let relay = WsRelay::Text(serde_json::json!({
                            "type": "sync",
                            "from": peer_id,
                            "payload": parsed["payload"],
                        }).to_string());
                        let s = state.lock().await;
                        if let Some(room_senders) = s.peer_senders.get(room) {
                            for (id, tx) in room_senders {
                                if id != peer_id {
                                    let _ = tx.send(relay.clone());
                                }
                            }
                        }
                    }
                    "sync_to" => {
                        let to = parsed["to"].as_str().unwrap_or("").to_string();
                        let relay = WsRelay::Text(serde_json::json!({
                            "type": "sync",
                            "from": peer_id,
                            "payload": parsed["payload"],
                        }).to_string());
                        let s = state.lock().await;
                        if let Some(room_senders) = s.peer_senders.get(room) {
                            if let Some(tx) = room_senders.get(&to) {
                                let _ = tx.send(relay);
                            }
                        }
                    }
                    "leave" => break,
                    _ => {}
                }
            }
            Message::Close(_) => break,
            _ => continue,
        }
    }
}

async fn cleanup_peer(state: &SharedState, room: &str, peer_id: &str) {
    let mut s = state.lock().await;

    if let Some(peers) = s.rooms.get_mut(room) {
        peers.retain(|p| p != peer_id);
        if peers.is_empty() {
            s.rooms.remove(room);
        }
    }

    if let Some(room_senders) = s.peer_senders.get(room) {
        for (id, tx) in room_senders {
            if id != peer_id {
                let _ = tx.send(WsRelay::Text(
                    serde_json::json!({
                        "type": "peer_left",
                        "peer_id": peer_id,
                    })
                    .to_string(),
                ));
            }
        }
    }

    if let Some(room_senders) = s.peer_senders.get_mut(room) {
        room_senders.remove(peer_id);
        if room_senders.is_empty() {
            s.peer_senders.remove(room);
        }
    }
}

/// Start an in-process WebSocket signaling server on a random port.
/// Returns the base WebSocket URL (e.g. `"ws://127.0.0.1:PORT"`).
pub async fn start_test_signaling_server() -> String {
    let state: SharedState = Arc::new(Mutex::new(SignalingState::default()));
    let app = Router::new().route("/ws", get(handle_ws)).with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app).into_future());
    format!("ws://{addr}")
}

// ---------------------------------------------------------------------------
// Role-aware mini_app session loop (mirrors session.rs audio forwarding)
// ---------------------------------------------------------------------------

/// A minimal session loop replicating the audio forwarding logic from session.rs.
///
/// - Forwards incoming WAIF frames (tag 0x05) from send plugins to all remote peers.
/// - Forwards incoming remote audio to RECV-role plugin IPC connections (tag 0x01).
/// - Exchanges Hello sync messages and sends PeerJoined/PeerLeft IPC to recv plugins
///   (for slot affinity — ensures peers get the same aux output after reconnect).
/// - No audio gate; no link_peers guard — audio flows unconditionally.
pub async fn mini_app_session(
    ipc_port: u16,
    signaling_url: String,
    room: String,
    peer_id: String,
    password: String,
) {
    mini_app_session_with_identity(
        ipc_port,
        signaling_url,
        room,
        peer_id.clone(),
        password,
        peer_id, // use peer_id as identity fallback
    ).await
}

/// Like `mini_app_session` but with an explicit persistent identity for slot affinity.
pub async fn mini_app_session_with_identity(
    ipc_port: u16,
    signaling_url: String,
    room: String,
    peer_id: String,
    password: String,
    identity: String,
) {
    let (mut mesh, mut sync_rx, mut audio_rx) = PeerMesh::connect_full(
        &signaling_url,
        &room,
        &peer_id,
        Some(password.as_str()),
        1, // stream_count
        None, // display_name
    )
    .await
    .expect("mini_app: failed to connect to signaling");

    let ipc_listener = TcpListener::bind(("127.0.0.1", ipc_port))
        .await
        .expect("mini_app: failed to bind IPC port");

    let (ipc_from_plugin_tx, mut ipc_from_plugin_rx) = mpsc::channel::<Vec<u8>>(1024);
    let mut ipc_recv_writers: Vec<tokio::net::tcp::OwnedWriteHalf> = Vec::new();

    loop {
        tokio::select! {
            // Accept plugin IPC connection; read role byte
            result = ipc_listener.accept() => {
                if let Ok((stream, _addr)) = result {
                    let (mut read_half, write_half) = stream.into_split();

                    let mut role_buf = [0u8; 1];
                    if read_half.read_exact(&mut role_buf).await.is_err() {
                        continue;
                    }
                    let role = role_buf[0];

                    if role == IPC_ROLE_RECV {
                        ipc_recv_writers.push(write_half);
                    } else {
                        // Send plugin: read and discard stream_index (2 bytes)
                        let mut si_buf = [0u8; 2];
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_millis(200),
                            read_half.read_exact(&mut si_buf),
                        ).await;
                        drop(write_half);
                    }

                    let tx = ipc_from_plugin_tx.clone();
                    tokio::spawn(async move {
                        let mut recv_buf = IpcRecvBuffer::new();
                        let mut buf = [0u8; 65536];
                        loop {
                            match read_half.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(n) => {
                                    recv_buf.push(&buf[..n]);
                                    while let Some(frame) = recv_buf.next_frame() {
                                        if tx.send(frame).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    });
                }
            }

            // WAIF frame from send plugin (tag 0x05) → broadcast raw bytes to peers
            Some(frame) = ipc_from_plugin_rx.recv() => {
                if let Some(wire_data) = IpcMessage::decode_audio_frame(&frame) {
                    mesh.broadcast_audio(&wire_data).await;
                }
            }

            // Pump signaling; handle peer join/leave events
            event = mesh.poll_signaling() => {
                match event {
                    Ok(Some(wail_net::MeshEvent::PeerJoined { peer_id: pid, .. })) => {
                        // Send our Hello so the remote peer learns our identity
                        let hello = SyncMessage::Hello {
                            peer_id: peer_id.clone(),
                            display_name: None,
                            identity: Some(identity.clone()),
                        };
                        mesh.broadcast(&hello).await;
                        let _ = pid; // used above for logging context
                    }
                    Ok(Some(wail_net::MeshEvent::PeerLeft(pid))) => {
                        // Notify recv plugins so they clear the peer's ring buffer slot
                        let msg = IpcMessage::encode_peer_left(&pid);
                        let frame = IpcFramer::encode_frame(&msg);
                        for writer in &mut ipc_recv_writers {
                            let _ = writer.write_all(&frame).await;
                        }
                    }
                    _ => {}
                }
            }

            // Sync messages from remote peers (Hello, TempoChange, etc.)
            Some((from, sync_msg)) = sync_rx.recv() => {
                if let SyncMessage::Hello { identity: Some(remote_identity), .. } = &sync_msg {
                    // Notify recv plugins of the peer's persistent identity (for slot affinity)
                    let msg = IpcMessage::encode_peer_joined(&from, remote_identity);
                    let frame = IpcFramer::encode_frame(&msg);
                    for writer in &mut ipc_recv_writers {
                        let _ = writer.write_all(&frame).await;
                    }
                }
            }

            // Audio from remote peer → forward to all RECV plugin connections (tag 0x01)
            Some((from, data)) = audio_rx.recv() => {
                let msg = IpcMessage::encode_audio(&from, &data);
                let frame = IpcFramer::encode_frame(&msg);
                for writer in &mut ipc_recv_writers {
                    let _ = writer.write_all(&frame).await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Bind a TCP listener on a random OS-assigned port and return the port number.
pub fn random_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}
