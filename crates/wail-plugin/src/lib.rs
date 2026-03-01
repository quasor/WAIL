use std::io::{ErrorKind, Read as _, Write as _};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use nih_plug::prelude::*;

mod params;

use params::WailParams;
use wail_audio::{AudioBridge, IpcFramer, IpcMessage, IpcRecvBuffer};

/// Default IPC address (overridable via WAIL_IPC_ADDR env var).
const DEFAULT_IPC_ADDR: &str = "127.0.0.1:9191";

/// WAIL Plugin: captures DAW audio per interval, encodes with Opus,
/// and plays back remote peers' audio intervals.
///
/// Architecture:
/// - Audio thread: drives AudioBridge (IntervalRing + Opus encode/decode)
/// - IPC thread: TCP connection to wail-app for audio interval exchange
/// - Communication: crossbeam channels between audio thread and IPC thread
pub struct WailPlugin {
    params: Arc<WailParams>,
    /// Bridge between audio thread and Opus encode/decode (NINJAM ring buffer)
    bridge: Arc<Mutex<Option<AudioBridge>>>,
    /// Current sample rate (set on initialize)
    sample_rate: f32,
    /// Send completed intervals to IPC thread (audio thread → IPC)
    ipc_outgoing_tx: Option<Sender<Vec<u8>>>,
    /// Receive remote intervals from IPC thread (IPC → audio thread)
    ipc_incoming_rx: Option<Receiver<(String, Vec<u8>)>>,
}

impl Default for WailPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(WailParams::default()),
            bridge: Arc::new(Mutex::new(None)),
            sample_rate: 48000.0,
            ipc_outgoing_tx: None,
            ipc_incoming_rx: None,
        }
    }
}

impl Plugin for WailPlugin {
    const NAME: &'static str = "WAIL";
    const VENDOR: &'static str = "WAIL Project";
    const URL: &'static str = "https://github.com/user/AWAIL";
    const EMAIL: &'static str = "";

    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        // Stereo in/out
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            ..AudioIOLayout::const_default()
        },
        // Mono fallback
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(1),
            main_output_channels: NonZeroU32::new(1),
            ..AudioIOLayout::const_default()
        },
    ];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::None;
    const SAMPLE_ACCURATE_AUTOMATION: bool = false;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn initialize(
        &mut self,
        audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.sample_rate = buffer_config.sample_rate;

        let channels = audio_io_layout
            .main_input_channels
            .map(|c| c.get() as u16)
            .unwrap_or(2);
        let bridge = AudioBridge::new(
            buffer_config.sample_rate as u32,
            channels,
            self.params.bars.value() as u32,
            self.params.quantum(),
            self.params.bitrate_kbps.value() as u32,
        );

        match self.bridge.lock() {
            Ok(mut guard) => *guard = Some(bridge),
            Err(_) => {
                nih_error!("WAIL plugin bridge mutex poisoned, initialization failed");
                return false;
            }
        }

        // Set up IPC channels and spawn IPC thread
        let (out_tx, out_rx) = crossbeam_channel::bounded::<Vec<u8>>(64);
        let (in_tx, in_rx) = crossbeam_channel::bounded::<(String, Vec<u8>)>(64);
        self.ipc_outgoing_tx = Some(out_tx);
        self.ipc_incoming_rx = Some(in_rx);

        let addr = std::env::var("WAIL_IPC_ADDR")
            .unwrap_or_else(|_| DEFAULT_IPC_ADDR.to_string());

        std::thread::Builder::new()
            .name("wail-ipc".into())
            .spawn(move || ipc_thread(out_rx, in_tx, addr))
            .ok();

        nih_log!(
            "WAIL plugin initialized: {}Hz, {} channels, {} bars",
            buffer_config.sample_rate,
            channels,
            self.params.bars.value()
        );

        true
    }

    fn reset(&mut self) {
        if let Ok(mut bridge) = self.bridge.lock() {
            if let Some(ref mut b) = *bridge {
                b.reset();
            }
        }
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let transport = context.transport();
        let num_channels = buffer.channels() as u16;
        let num_samples = buffer.samples();

        // Get beat position from DAW transport
        let beat_position = transport.pos_beats().unwrap_or(0.0);
        let bpm = transport.tempo.unwrap_or(120.0);
        let playing = transport.playing;

        let send_enabled = self.params.send_enabled.value();
        let receive_enabled = self.params.receive_enabled.value();
        let volume = self.params.volume.value();

        if let Ok(mut bridge_guard) = self.bridge.try_lock() {
            if let Some(ref mut bridge) = *bridge_guard {
                // Update interval config if params changed
                bridge.update_config(
                    self.params.bars.value() as u32,
                    self.params.quantum(),
                    bpm,
                );

                // Feed any incoming remote audio before processing
                if let Some(ref rx) = self.ipc_incoming_rx {
                    while let Ok((peer_id, wire_data)) = rx.try_recv() {
                        bridge.receive_wire(&peer_id, &wire_data);
                    }
                }

                // Interleave input (zeros if not recording)
                let buf_size = num_samples * num_channels as usize;
                let input = if send_enabled && playing {
                    let mut interleaved = Vec::with_capacity(buf_size);
                    for sample_idx in 0..num_samples {
                        for ch in 0..num_channels as usize {
                            interleaved.push(buffer.as_slice()[ch][sample_idx]);
                        }
                    }
                    interleaved
                } else {
                    vec![0.0f32; buf_size]
                };

                // Run the ring buffer: record input, produce playback output
                let mut playback = vec![0.0f32; buf_size];
                let wire_msgs = bridge.process(&input, &mut playback, beat_position);

                // Mix playback into DAW output
                if receive_enabled {
                    for sample_idx in 0..num_samples {
                        for ch in 0..num_channels as usize {
                            let pb_idx = sample_idx * num_channels as usize + ch;
                            buffer.as_slice()[ch][sample_idx] += playback[pb_idx] * volume;
                        }
                    }
                }

                // Send completed intervals to IPC thread
                if let Some(ref tx) = self.ipc_outgoing_tx {
                    for wire in wire_msgs {
                        let _ = tx.try_send(wire);
                    }
                }
            }
        }

        ProcessStatus::Normal
    }
}

/// IPC background thread: connects to wail-app via TCP, exchanges audio intervals.
/// Reconnects automatically on disconnect.
fn ipc_thread(
    outgoing_rx: Receiver<Vec<u8>>,
    incoming_tx: Sender<(String, Vec<u8>)>,
    addr: String,
) {
    loop {
        // Check if channels are still alive
        if outgoing_rx.is_empty() && incoming_tx.is_full() {
            // Heuristic: if both are in bad shape, plugin may be gone
        }

        let mut stream = match TcpStream::connect(&addr) {
            Ok(s) => {
                tracing::info!(addr = %addr, "Plugin IPC connected to wail-app");
                s
            }
            Err(_) => {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        // 10ms read timeout: allows checking outgoing channel between reads
        stream
            .set_read_timeout(Some(Duration::from_millis(10)))
            .ok();

        let mut recv_buf = IpcRecvBuffer::new();
        let mut read_buf = [0u8; 65536];

        loop {
            // Send any pending outgoing intervals
            let mut send_error = false;
            while let Ok(wire_data) = outgoing_rx.try_recv() {
                let msg = IpcMessage::encode_audio("", &wire_data);
                let frame = IpcFramer::encode_frame(&msg);
                if stream.write_all(&frame).is_err() {
                    send_error = true;
                    break;
                }
            }
            if send_error {
                tracing::warn!("Plugin IPC write error — reconnecting");
                break;
            }

            // Read incoming remote intervals (with short timeout)
            match stream.read(&mut read_buf) {
                Ok(0) => {
                    tracing::info!("Plugin IPC connection closed");
                    break;
                }
                Ok(n) => {
                    recv_buf.push(&read_buf[..n]);
                    while let Some(payload) = recv_buf.next_frame() {
                        if let Some((peer_id, wire_data)) = IpcMessage::decode_audio(&payload) {
                            let _ = incoming_tx.try_send((peer_id, wire_data));
                        }
                    }
                }
                Err(ref e)
                    if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
                {
                    // No data available within timeout — normal
                }
                Err(_) => {
                    tracing::warn!("Plugin IPC read error — reconnecting");
                    break;
                }
            }
        }

        // Brief pause before reconnect
        std::thread::sleep(Duration::from_secs(1));
    }
}

impl ClapPlugin for WailPlugin {
    const CLAP_ID: &'static str = "com.wail.intervalic-audio";
    const CLAP_DESCRIPTION: Option<&'static str> = Some(
        "WAIL - WebRTC Audio Interchange for Link. Intervalic audio sync over the internet.",
    );
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::Utility, ClapFeature::Stereo];
}

impl Vst3Plugin for WailPlugin {
    const VST3_CLASS_ID: [u8; 16] = *b"WAILIntervalic0\0";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Network];
}

nih_export_clap!(WailPlugin);
nih_export_vst3!(WailPlugin);
