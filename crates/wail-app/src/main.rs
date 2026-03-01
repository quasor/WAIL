use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use wail_audio::{IpcFramer, IpcMessage, IpcRecvBuffer};
use wail_core::{ClockSync, IntervalTracker, LinkBridge, LinkCommand, LinkEvent, SyncMessage};
use wail_net::PeerMesh;

#[derive(Parser)]
#[command(name = "wail", about = "WAIL - WebRTC Audio Interchange for Link")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Join a sync room
    Join {
        /// Room name to join
        #[arg(short, long)]
        room: String,

        /// Signaling server URL
        #[arg(short, long, default_value = "https://wail.val.run/")]
        server: String,

        /// Initial BPM
        #[arg(short, long, default_value_t = 120.0)]
        bpm: f64,

        /// Bars per interval (NINJAM-style)
        #[arg(long, default_value_t = 4)]
        bars: u32,

        /// Quantum (beats per bar / time signature numerator)
        #[arg(short, long, default_value_t = 4.0)]
        quantum: f64,

        /// IPC port for plugin communication
        #[arg(long, default_value_t = 9191)]
        ipc_port: u16,

        /// Display name for this peer (shown to remote peers)
        #[arg(short, long)]
        name: Option<String>,

        /// Room password (first peer to join sets it; others must match)
        #[arg(short, long)]
        password: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "wail=info,wail_app=info,wail_core=info,wail_net=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Join {
            room,
            server,
            bpm,
            bars,
            quantum,
            ipc_port,
            name,
            password,
        } => {
            run_peer(server, room, bpm, bars, quantum, ipc_port, name, password).await?;
        }
    }

    Ok(())
}

async fn run_peer(
    server: String,
    room: String,
    bpm: f64,
    bars: u32,
    quantum: f64,
    ipc_port: u16,
    display_name: Option<String>,
    password: String,
) -> Result<()> {
    let peer_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let name_str = display_name.as_deref().unwrap_or("(anonymous)");
    info!(%peer_id, name = name_str, %room, bpm, bars, quantum, ipc_port, "Starting WAIL peer");

    // Initialize Ableton Link
    let link = LinkBridge::new(bpm, quantum);
    link.enable();
    let (link_cmd_tx, mut link_event_rx) = link.spawn_poller();

    // Connect to signaling server (now returns sync + audio receivers)
    let (mut mesh, mut sync_rx, mut audio_rx) =
        PeerMesh::connect(&server, &room, &peer_id, &password).await?;
    info!("Connected to signaling server");

    // Clock sync and interval tracker
    let mut clock = ClockSync::new();
    let mut interval = IntervalTracker::new(bars, quantum);
    let mut ping_interval =
        tokio::time::interval(Duration::from_millis(ClockSync::ping_interval_ms()));
    let mut status_interval = tokio::time::interval(Duration::from_secs(5));

    // Track last broadcast tempo to avoid echo loops
    let mut last_broadcast_bpm: f64 = bpm;
    // One-shot join-time beat sync: snap local beat clock to first remote StateSnapshot
    let mut beat_synced = false;

    // Audio interval stats
    let mut audio_intervals_sent: u64 = 0;
    let mut audio_intervals_received: u64 = 0;

    // IPC: listen for plugin connections
    let ipc_listener = tokio::net::TcpListener::bind(("127.0.0.1", ipc_port)).await?;
    info!(port = ipc_port, "IPC listening for plugin connections");

    // IPC state: writer to send remote audio to plugin, reader channel for incoming from plugin
    let mut ipc_writer: Option<tokio::net::tcp::OwnedWriteHalf> = None;
    let (ipc_from_plugin_tx, mut ipc_from_plugin_rx) = mpsc::channel::<Vec<u8>>(64);

    info!("WAIL peer running. Waiting for peers...");

    loop {
        tokio::select! {
            // --- Accept plugin IPC connection ---
            result = ipc_listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        info!(%addr, "Plugin IPC connected");
                        let (read_half, write_half) = stream.into_split();
                        ipc_writer = Some(write_half);

                        // Spawn reader task for this plugin connection
                        let tx = ipc_from_plugin_tx.clone();
                        tokio::spawn(async move {
                            let mut recv_buf = IpcRecvBuffer::new();
                            let mut buf = [0u8; 65536];
                            let mut reader = read_half;
                            loop {
                                match reader.read(&mut buf).await {
                                    Ok(0) => {
                                        info!("Plugin IPC disconnected");
                                        break;
                                    }
                                    Ok(n) => {
                                        recv_buf.push(&buf[..n]);
                                        while let Some(frame) = recv_buf.next_frame() {
                                            match tx.try_send(frame) {
                                                Ok(()) => {}
                                                Err(mpsc::error::TrySendError::Full(_)) => {
                                                    debug!("IPC audio channel full — dropping frame");
                                                }
                                                Err(mpsc::error::TrySendError::Closed(_)) => return,
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        warn!(error = %e, "Plugin IPC read error");
                                        break;
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to accept IPC connection");
                    }
                }
            }

            // --- Audio from plugin IPC → broadcast to WebRTC peers ---
            Some(frame) = ipc_from_plugin_rx.recv() => {
                if let Some((_peer_id, wire_data)) = IpcMessage::decode_audio(&frame) {
                    mesh.broadcast_audio(&wire_data).await;
                    audio_intervals_sent += 1;
                    debug!(wire_bytes = wire_data.len(), "Forwarded plugin audio to peers");
                }
            }

            // --- Signaling messages (peer discovery, WebRTC negotiation) ---
            event = mesh.poll_signaling() => {
                match event {
                    Ok(Some(wail_net::MeshEvent::PeerJoined(pid))) => {
                        info!(peer = %pid, "Peer connected - sending Hello");
                        let hello = SyncMessage::Hello { peer_id: peer_id.clone(), display_name: display_name.clone() };
                        mesh.broadcast(&hello).await;

                        // Send interval config
                        let config = SyncMessage::IntervalConfig { bars, quantum };
                        mesh.broadcast(&config).await;

                        // Announce audio capabilities
                        let caps = SyncMessage::AudioCapabilities {
                            sample_rates: vec![48000],
                            channel_counts: vec![1, 2],
                            can_send: true,
                            can_receive: true,
                        };
                        mesh.broadcast(&caps).await;
                    }
                    Ok(Some(wail_net::MeshEvent::PeerLeft(pid))) => {
                        info!(peer = %pid, "Peer disconnected");
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        warn!("Signaling connection closed");
                        break;
                    }
                    Err(e) => {
                        error!(error = %e, "Signaling error");
                    }
                }
            }

            // --- Incoming sync messages from peers ---
            Some((from, msg)) = sync_rx.recv() => {
                match msg {
                    SyncMessage::Hello { peer_id: pid, display_name } => {
                        let name = display_name.as_deref().unwrap_or("(anonymous)");
                        info!(peer = %pid, name, "Received Hello from peer");
                    }

                    SyncMessage::Ping { id, sent_at_us } => {
                        let pong = clock.handle_ping(id, sent_at_us);
                        if let Err(e) = mesh.send_to(&from, &pong).await {
                            debug!(peer = %from, error = %e, "Failed to send pong");
                        }
                    }

                    SyncMessage::Pong { id: _, ping_sent_at_us, pong_sent_at_us } => {
                        clock.handle_pong(&from, ping_sent_at_us, pong_sent_at_us);
                        if let Some(rtt) = clock.rtt_us(&from) {
                            tracing::debug!(peer = %from, rtt_ms = rtt as f64 / 1000.0, "Clock sync update");
                        }
                    }

                    SyncMessage::TempoChange { bpm: remote_bpm, .. } => {
                        info!(peer = %from, bpm = remote_bpm, "Remote tempo change");
                        last_broadcast_bpm = remote_bpm;
                        if link_cmd_tx.send(LinkCommand::SetTempo(remote_bpm)).is_err() {
                            warn!("Link bridge stopped — cannot apply remote tempo");
                        }
                    }

                    SyncMessage::StateSnapshot { bpm: remote_bpm, beat: remote_beat, .. } => {
                        tracing::debug!(
                            peer = %from,
                            bpm = format!("{:.1}", remote_bpm),
                            beat = format!("{:.2}", remote_beat),
                            "Remote state snapshot"
                        );
                        // One-shot join-time beat sync: snap our beat clock to the
                        // remote's on the very first snapshot we receive, before our
                        // own interval counter has any history. This eliminates the
                        // persistent 1-interval offset caused by different absolute
                        // beat counts when Link sessions merge.
                        if !beat_synced {
                            beat_synced = true;
                            info!(
                                peer = %from,
                                beat = format!("{:.2}", remote_beat),
                                "Join-time beat sync — snapping local beat clock to remote"
                            );
                            if link_cmd_tx.send(LinkCommand::ForceBeat(remote_beat)).is_err() {
                                warn!("Link bridge stopped — cannot force beat");
                            }
                            // Also reset the interval tracker so the next update()
                            // fires a fresh boundary at the correct index.
                            interval.set_config(bars, quantum);
                        }
                        // Apply tempo if significantly different
                        if (remote_bpm - last_broadcast_bpm).abs() > 0.01 {
                            last_broadcast_bpm = remote_bpm;
                            if link_cmd_tx.send(LinkCommand::SetTempo(remote_bpm)).is_err() {
                                warn!("Link bridge stopped — cannot apply remote tempo");
                            }
                        }
                    }

                    SyncMessage::IntervalConfig { bars: remote_bars, quantum: remote_q } => {
                        info!(bars = remote_bars, quantum = remote_q, "Remote interval config");
                        interval.set_config(remote_bars, remote_q);
                    }

                    SyncMessage::AudioCapabilities { sample_rates, channel_counts, can_send, can_receive } => {
                        info!(
                            peer = %from,
                            ?sample_rates,
                            ?channel_counts,
                            can_send,
                            can_receive,
                            "Remote audio capabilities"
                        );
                    }

                    SyncMessage::AudioIntervalReady { interval_index, wire_size } => {
                        tracing::debug!(
                            peer = %from,
                            interval = interval_index,
                            size = wire_size,
                            "Audio interval incoming"
                        );
                    }

                    SyncMessage::IntervalBoundary { index } => {
                        let local = interval.current_index();
                        let behind = local.map_or(true, |l| index > l);
                        if behind {
                            debug!(
                                local = ?local,
                                remote = index,
                                peer = %from,
                                "Interval index behind remote — syncing forward"
                            );
                            interval.sync_to(index);
                        }
                    }
                }
            }

            // --- Incoming audio data from peers (binary DataChannel) → forward to plugin ---
            Some((from, data)) = audio_rx.recv() => {
                audio_intervals_received += 1;
                debug!(
                    peer = %from,
                    wire_bytes = data.len(),
                    "Received audio interval from peer"
                );

                // Forward to plugin via IPC
                if let Some(ref mut writer) = ipc_writer {
                    let msg = IpcMessage::encode_audio(&from, &data);
                    let frame = IpcFramer::encode_frame(&msg);
                    if let Err(e) = writer.write_all(&frame).await {
                        warn!(error = %e, "Failed to write to plugin IPC");
                        ipc_writer = None;
                    }
                }
            }

            // --- Local Link events ---
            Some(event) = link_event_rx.recv() => {
                match event {
                    LinkEvent::TempoChanged { bpm: local_bpm, beat, timestamp_us } => {
                        // Only broadcast if this is a genuinely new tempo (not an echo)
                        if (local_bpm - last_broadcast_bpm).abs() > 0.01 {
                            info!(bpm = format!("{:.1}", local_bpm), "Local tempo changed - broadcasting");
                            last_broadcast_bpm = local_bpm;
                            let msg = SyncMessage::TempoChange {
                                bpm: local_bpm,
                                quantum,
                                timestamp_us,
                            };
                            mesh.broadcast(&msg).await;
                        }

                        // Check interval boundary
                        if let Some(idx) = interval.update(beat) {
                            info!(interval = idx, beat = format!("{:.1}", beat), ">>> INTERVAL BOUNDARY <<<");
                            mesh.broadcast(&SyncMessage::IntervalBoundary { index: idx }).await;
                        }
                    }

                    LinkEvent::StateUpdate { bpm: local_bpm, beat, phase, quantum: q, timestamp_us } => {
                        // Broadcast state snapshot to peers
                        let msg = SyncMessage::StateSnapshot {
                            bpm: local_bpm,
                            beat,
                            phase,
                            quantum: q,
                            timestamp_us,
                        };
                        mesh.broadcast(&msg).await;

                        // Check interval boundary
                        if let Some(idx) = interval.update(beat) {
                            info!(interval = idx, beat = format!("{:.1}", beat), ">>> INTERVAL BOUNDARY <<<");
                            mesh.broadcast(&SyncMessage::IntervalBoundary { index: idx }).await;
                        }
                    }
                }
            }

            // --- Periodic clock sync pings ---
            _ = ping_interval.tick() => {
                let ping = clock.make_ping();
                mesh.broadcast(&ping).await;
            }

            // --- Periodic status display ---
            _ = status_interval.tick() => {
                // Get current Link state
                let (tx, rx) = tokio::sync::oneshot::channel();
                if link_cmd_tx.send(LinkCommand::GetState(tx)).is_err() {
                    debug!("Link bridge stopped — cannot query state");
                    continue;
                }
                if let Ok(state) = rx.await {
                    let peers = mesh.connected_peers();
                    let peer_rtts: Vec<String> = peers
                        .iter()
                        .filter_map(|p| {
                            clock.rtt_us(p).map(|rtt| format!("{}({:.0}ms)", &p[..6.min(p.len())], rtt as f64 / 1000.0))
                        })
                        .collect();

                    info!(
                        bpm = format!("{:.1}", state.bpm),
                        beat = format!("{:.1}", state.beat),
                        phase = format!("{:.2}", state.phase),
                        link_peers = state.num_peers,
                        webrtc_peers = peers.len(),
                        rtts = ?peer_rtts,
                        interval_bars = interval.bars(),
                        audio_sent = audio_intervals_sent,
                        audio_recv = audio_intervals_received,
                        ipc_plugin = ipc_writer.is_some(),
                        "Status"
                    );
                }
            }
        }
    }

    Ok(())
}
