use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{error, info, warn};

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

        /// Signaling server WebSocket URL
        #[arg(short, long, default_value = "ws://localhost:9090")]
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
        } => {
            run_peer(server, room, bpm, bars, quantum).await?;
        }
    }

    Ok(())
}

async fn run_peer(server: String, room: String, bpm: f64, bars: u32, quantum: f64) -> Result<()> {
    let peer_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    info!(%peer_id, %room, bpm, bars, quantum, "Starting WAIL peer");

    // Initialize Ableton Link
    let link = LinkBridge::new(bpm, quantum);
    link.enable();
    let (link_cmd_tx, mut link_event_rx) = link.spawn_poller();

    // Connect to signaling server
    let (mut mesh, mut sync_rx) = PeerMesh::connect(&server, &room, &peer_id).await?;
    info!("Connected to signaling server");

    // Clock sync and interval tracker
    let mut clock = ClockSync::new();
    let mut interval = IntervalTracker::new(bars, quantum);
    let mut ping_interval = tokio::time::interval(Duration::from_millis(ClockSync::ping_interval_ms()));
    let mut status_interval = tokio::time::interval(Duration::from_secs(5));

    // Track last broadcast tempo to avoid echo loops
    let mut last_broadcast_bpm: f64 = bpm;

    info!("WAIL peer running. Waiting for peers...");

    loop {
        tokio::select! {
            // --- Signaling messages (peer discovery, WebRTC negotiation) ---
            event = mesh.poll_signaling() => {
                match event {
                    Ok(Some(wail_net::MeshEvent::PeerJoined(pid))) => {
                        info!(peer = %pid, "Peer connected - sending Hello");
                        let hello = SyncMessage::Hello { peer_id: peer_id.clone() };
                        mesh.broadcast(&hello).await;

                        // Send interval config
                        let config = SyncMessage::IntervalConfig { bars, quantum };
                        mesh.broadcast(&config).await;
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
                    SyncMessage::Hello { peer_id: pid } => {
                        info!(peer = %pid, "Received Hello from peer");
                    }

                    SyncMessage::Ping { id, sent_at_us } => {
                        let pong = clock.handle_ping(id, sent_at_us);
                        let _ = mesh.send_to(&from, &pong).await;
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
                        let _ = link_cmd_tx.send(LinkCommand::SetTempo(remote_bpm));
                    }

                    SyncMessage::StateSnapshot { bpm: remote_bpm, beat, .. } => {
                        tracing::debug!(
                            peer = %from,
                            bpm = format!("{:.1}", remote_bpm),
                            beat = format!("{:.2}", beat),
                            "Remote state snapshot"
                        );
                        // Apply tempo if significantly different
                        if (remote_bpm - last_broadcast_bpm).abs() > 0.01 {
                            last_broadcast_bpm = remote_bpm;
                            let _ = link_cmd_tx.send(LinkCommand::SetTempo(remote_bpm));
                        }
                    }

                    SyncMessage::IntervalConfig { bars: remote_bars, quantum: remote_q } => {
                        info!(bars = remote_bars, quantum = remote_q, "Remote interval config");
                        interval.set_config(remote_bars, remote_q);
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
                let _ = link_cmd_tx.send(LinkCommand::GetState(tx));
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
                        "Status"
                    );
                }
            }
        }
    }

    Ok(())
}
