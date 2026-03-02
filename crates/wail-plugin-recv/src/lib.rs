use std::io::Read as _;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use assert_no_alloc::permit_alloc;
use crossbeam_channel::Receiver;
use nih_plug::prelude::*;

mod params;

use params::WailRecvParams;
use wail_audio::{
    nearest_opus_rate, AudioBridge, AudioDecoder, AudioWire, IpcMessage, IpcRecvBuffer,
};

/// Default IPC address (overridable via WAIL_IPC_ADDR env var).
const DEFAULT_IPC_ADDR: &str = "127.0.0.1:9191";

const DEFAULT_BARS: u32 = 4;
const DEFAULT_QUANTUM: f64 = 4.0;

/// WAIL Receive Plugin: receives remote peers' audio from wail-app and plays
/// it back in the DAW with per-peer auxiliary outputs.
///
/// Architecture:
/// - Audio thread: drives IntervalRing (playback only, records silence)
/// - IPC thread: TCP read from wail-app + Opus decode
/// - Communication: crossbeam channel from IPC thread to audio thread
pub struct WailRecvPlugin {
    params: Arc<WailRecvParams>,
    bridge: Arc<Mutex<Option<AudioBridge>>>,
    sample_rate: f32,
    ipc_incoming_rx: Option<Receiver<(String, i64, Vec<f32>)>>,
    /// Pre-allocated playback buffer (reused every process call)
    playback_buf: Vec<f32>,
    /// Pre-allocated per-peer buffers (reused every process call)
    peer_bufs: [Vec<f32>; wail_audio::MAX_REMOTE_PEERS],
    /// Cumulative samples processed — fallback beat source when host
    /// doesn't provide `pos_beats()`.
    cumulative_samples: u64,
    beat_fallback_warned: bool,
}

impl Default for WailRecvPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(WailRecvParams::default()),
            bridge: Arc::new(Mutex::new(None)),
            sample_rate: 48000.0,
            ipc_incoming_rx: None,
            playback_buf: Vec::new(),
            peer_bufs: Default::default(),
            cumulative_samples: 0,
            beat_fallback_warned: false,
        }
    }
}

/// Ensure a buffer is at least `needed` elements long, growing with zeroes if necessary.
#[inline]
fn ensure_buf(buf: &mut Vec<f32>, needed: usize) {
    if buf.len() < needed {
        buf.resize(needed, 0.0);
    }
}

impl Plugin for WailRecvPlugin {
    const NAME: &'static str = "WAIL Recv";
    const VENDOR: &'static str = "WAIL Project";
    const URL: &'static str = "https://github.com/quasor/WAIL";
    const EMAIL: &'static str = "";

    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        // Stereo in/out with 7 per-peer aux stereo outputs
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            aux_output_ports: &[new_nonzero_u32(2); 7],
            names: PortNames {
                aux_outputs: &[
                    "Peer 1", "Peer 2", "Peer 3", "Peer 4",
                    "Peer 5", "Peer 6", "Peer 7",
                ],
                ..PortNames::const_default()
            },
            ..AudioIOLayout::const_default()
        },
        // Mono fallback (no aux outputs)
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
        self.cumulative_samples = 0;
        self.beat_fallback_warned = false;

        let channels = audio_io_layout
            .main_output_channels
            .map(|c| c.get() as u16)
            .unwrap_or(2);
        let bridge = AudioBridge::new(
            buffer_config.sample_rate as u32,
            channels,
            DEFAULT_BARS,
            DEFAULT_QUANTUM,
            128, // bitrate doesn't matter for decode-only
        );

        let max_buf = buffer_config.max_buffer_size as usize * channels as usize;
        self.playback_buf = vec![0.0f32; max_buf];
        for buf in &mut self.peer_bufs {
            *buf = vec![0.0f32; max_buf];
        }

        match self.bridge.lock() {
            Ok(mut guard) => *guard = Some(bridge),
            Err(_) => {
                nih_error!("WAIL Recv: bridge mutex poisoned, initialization failed");
                return false;
            }
        }

        let (in_tx, in_rx) = crossbeam_channel::bounded::<(String, i64, Vec<f32>)>(64);
        self.ipc_incoming_rx = Some(in_rx);

        let addr = std::env::var("WAIL_IPC_ADDR")
            .unwrap_or_else(|_| DEFAULT_IPC_ADDR.to_string());

        let ipc_sample_rate = buffer_config.sample_rate as u32;
        let ipc_channels = channels;

        std::thread::Builder::new()
            .name("wail-ipc-recv".into())
            .spawn(move || {
                ipc_thread_recv(in_tx, addr, ipc_sample_rate, ipc_channels)
            })
            .ok();

        nih_log!(
            "WAIL Recv initialized: {}Hz, {} channels, {} bars",
            buffer_config.sample_rate,
            channels,
            DEFAULT_BARS
        );

        true
    }

    fn reset(&mut self) {
        self.cumulative_samples = 0;
        if let Ok(mut bridge) = self.bridge.lock() {
            if let Some(ref mut b) = *bridge {
                b.reset();
            }
        }
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let transport = context.transport();
        let num_channels = buffer.channels() as u16;
        let num_samples = buffer.samples();

        let bpm = transport.tempo.unwrap_or(120.0);

        let beat_position = match transport.pos_beats() {
            Some(b) => b,
            None => {
                if !self.beat_fallback_warned {
                    self.beat_fallback_warned = true;
                    nih_log!("WAIL Recv: host does not provide beat position — using sample-count fallback");
                }
                self.cumulative_samples as f64 * bpm / (60.0 * self.sample_rate as f64)
            }
        };
        self.cumulative_samples += num_samples as u64;

        if let Ok(mut bridge_guard) = self.bridge.try_lock() {
            if let Some(ref mut bridge) = *bridge_guard {
                bridge.update_config(
                    DEFAULT_BARS,
                    DEFAULT_QUANTUM,
                    bpm,
                );

                let buf_size = num_samples * num_channels as usize;

                ensure_buf(&mut self.playback_buf, buf_size);
                let playback = &mut self.playback_buf[..buf_size];
                playback.fill(0.0);

                // Feed decoded remote audio and drive the ring buffer.
                // Silent input — we're receive-only, so the record slot
                // captures nothing useful (completed intervals are discarded).
                // The Vec<CompletedInterval> must be dropped inside permit_alloc
                // to avoid triggering assert_no_alloc on deallocation.
                permit_alloc(|| {
                    if let Some(ref rx) = self.ipc_incoming_rx {
                        while let Ok((peer_id, interval_index, samples)) = rx.try_recv() {
                            bridge.feed_decoded(&peer_id, interval_index, samples);
                        }
                    }
                    // Use a zero-length slice as silent input — IntervalRing
                    // handles zero-length input gracefully (nothing recorded).
                    drop(bridge.process_rt(&[], playback, beat_position));
                });

                // Mix playback into DAW main output
                for sample_idx in 0..num_samples {
                    for ch in 0..num_channels as usize {
                        let pb_idx = sample_idx * num_channels as usize + ch;
                        buffer.as_slice()[ch][sample_idx] = playback[pb_idx];
                    }
                }

                // Route per-peer audio to auxiliary output buses
                let num_aux = aux.outputs.len().min(wail_audio::MAX_REMOTE_PEERS);
                for slot_idx in 0..num_aux {
                    ensure_buf(&mut self.peer_bufs[slot_idx], buf_size);
                    let peer_buf = &mut self.peer_bufs[slot_idx][..buf_size];
                    peer_buf.fill(0.0);
                    bridge.read_peer_playback(slot_idx, peer_buf);

                    let aux_buf = &mut aux.outputs[slot_idx];
                    let aux_ch = aux_buf.channels();
                    let aux_samples = aux_buf.samples();
                    let n = aux_samples.min(num_samples);

                    for sample_idx in 0..n {
                        for ch in 0..aux_ch.min(num_channels as usize) {
                            let pb_idx = sample_idx * num_channels as usize + ch;
                            aux_buf.as_slice()[ch][sample_idx] = peer_buf[pb_idx];
                        }
                    }
                }
            }
        }

        ProcessStatus::Normal
    }
}

/// Receive-only IPC thread: reads from TCP, Opus-decodes, sends PCM to audio thread.
fn ipc_thread_recv(
    incoming_tx: crossbeam_channel::Sender<(String, i64, Vec<f32>)>,
    addr: String,
    sample_rate: u32,
    channels: u16,
) {
    let opus_rate = nearest_opus_rate(sample_rate);
    if opus_rate != sample_rate {
        tracing::warn!(
            daw_rate = sample_rate,
            opus_rate,
            "DAW sample rate is not a native Opus rate — decoding at {opus_rate}Hz"
        );
    }

    let mut decoder = match AudioDecoder::new(opus_rate, channels) {
        Ok(dec) => Some(dec),
        Err(e) => {
            tracing::warn!(error = %e, "IPC recv thread: failed to create Opus decoder");
            None
        }
    };

    loop {
        let mut stream = match TcpStream::connect(&addr) {
            Ok(s) => {
                tracing::info!(addr = %addr, "WAIL Recv IPC connected to wail-app");
                s
            }
            Err(_) => {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        // No read timeout — we block until data arrives or the connection closes
        stream.set_read_timeout(None).ok();

        let mut recv_buf = IpcRecvBuffer::new();
        let mut read_buf = [0u8; 65536];

        loop {
            match stream.read(&mut read_buf) {
                Ok(0) => {
                    tracing::info!("WAIL Recv IPC connection closed");
                    break;
                }
                Ok(n) => {
                    recv_buf.push(&read_buf[..n]);
                    while let Some(payload) = recv_buf.next_frame() {
                        if let Some((peer_id, wire_data)) = IpcMessage::decode_audio(&payload) {
                            match AudioWire::decode(&wire_data) {
                                Ok(interval) => {
                                    if let Some(ref mut dec) = decoder {
                                        match dec.decode_interval(&interval.opus_data) {
                                            Ok(samples) => {
                                                let _ = incoming_tx.try_send((
                                                    peer_id,
                                                    interval.index,
                                                    samples,
                                                ));
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    error = %e,
                                                    "IPC recv: failed to decode Opus audio"
                                                );
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "IPC recv: failed to decode wire data"
                                    );
                                }
                            }
                        }
                    }
                }
                Err(_) => {
                    tracing::warn!("WAIL Recv IPC read error — reconnecting");
                    break;
                }
            }
        }

        std::thread::sleep(Duration::from_secs(1));
    }
}

impl ClapPlugin for WailRecvPlugin {
    const CLAP_ID: &'static str = "com.wail.recv";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("WAIL Recv - plays back remote peers' audio from the network");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::Utility, ClapFeature::Stereo];
}

impl Vst3Plugin for WailRecvPlugin {
    const VST3_CLASS_ID: [u8; 16] = *b"WAILRecvPlugin\0\0";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Network];
}

nih_export_clap!(WailRecvPlugin);
nih_export_vst3!(WailRecvPlugin);
