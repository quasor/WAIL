use std::io::{ErrorKind, Read as _, Write as _};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use assert_no_alloc::permit_alloc;
use crossbeam_channel::{Receiver, Sender};
use nih_plug::prelude::*;

mod params;

use params::WailParams;
use wail_audio::{
    AudioBridge, AudioDecoder, AudioEncoder, AudioInterval, AudioWire, CompletedInterval,
    IpcFramer, IpcMessage, IpcRecvBuffer,
};

/// Default IPC address (overridable via WAIL_IPC_ADDR env var).
const DEFAULT_IPC_ADDR: &str = "127.0.0.1:9191";

/// Raw completed interval with config snapshot, sent from audio thread to IPC
/// thread for Opus encoding (keeps Opus off the real-time audio callback).
struct RawInterval {
    completed: CompletedInterval,
    sample_rate: u32,
    channels: u16,
    bpm: f64,
    quantum: f64,
    bars: u32,
}

/// WAIL Plugin: captures DAW audio per interval, encodes with Opus,
/// and plays back remote peers' audio intervals.
///
/// Architecture:
/// - Audio thread: drives IntervalRing (record/playback only, no Opus)
/// - IPC thread: Opus encode/decode + TCP connection to wail-app
/// - Communication: crossbeam channels between audio thread and IPC thread
pub struct WailPlugin {
    params: Arc<WailParams>,
    /// Bridge on audio thread (ring buffer only — no Opus in the audio callback)
    bridge: Arc<Mutex<Option<AudioBridge>>>,
    /// Current sample rate (set on initialize)
    sample_rate: f32,
    /// Send raw completed intervals to IPC thread for encoding (audio → IPC)
    ipc_outgoing_tx: Option<Sender<RawInterval>>,
    /// Receive decoded PCM from IPC thread for playback (IPC → audio)
    ipc_incoming_rx: Option<Receiver<(String, i64, Vec<f32>)>>,
    /// Pre-allocated interleaved input buffer (reused every process call)
    interleave_buf: Vec<f32>,
    /// Pre-allocated playback buffer (reused every process call)
    playback_buf: Vec<f32>,
    /// Pre-allocated per-peer buffers (reused every process call)
    peer_bufs: [Vec<f32>; wail_audio::MAX_REMOTE_PEERS],
}

impl Default for WailPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(WailParams::default()),
            bridge: Arc::new(Mutex::new(None)),
            sample_rate: 48000.0,
            ipc_outgoing_tx: None,
            ipc_incoming_rx: None,
            interleave_buf: Vec::new(),
            playback_buf: Vec::new(),
            peer_bufs: Default::default(),
        }
    }
}

/// Ensure a buffer is at least `needed` elements long, growing with zeroes if necessary.
/// This is NOT real-time safe (allocates), but prevents panics if the DAW exceeds
/// max_buffer_size. In practice this only fires on the first oversized buffer.
#[inline]
fn ensure_buf(buf: &mut Vec<f32>, needed: usize) {
    if buf.len() < needed {
        buf.resize(needed, 0.0);
    }
}

impl Plugin for WailPlugin {
    const NAME: &'static str = "WAIL";
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

        // Pre-allocate reusable audio buffers (max_buffer_size * channels)
        let max_buf = buffer_config.max_buffer_size as usize * channels as usize;
        self.interleave_buf = vec![0.0f32; max_buf];
        self.playback_buf = vec![0.0f32; max_buf];
        for buf in &mut self.peer_bufs {
            *buf = vec![0.0f32; max_buf];
        }

        match self.bridge.lock() {
            Ok(mut guard) => *guard = Some(bridge),
            Err(_) => {
                nih_error!("WAIL plugin bridge mutex poisoned, initialization failed");
                return false;
            }
        }

        // Set up IPC channels and spawn IPC thread
        // Audio thread sends raw PCM; IPC thread handles Opus encode/decode
        let (out_tx, out_rx) = crossbeam_channel::bounded::<RawInterval>(64);
        let (in_tx, in_rx) = crossbeam_channel::bounded::<(String, i64, Vec<f32>)>(64);
        self.ipc_outgoing_tx = Some(out_tx);
        self.ipc_incoming_rx = Some(in_rx);

        let addr = std::env::var("WAIL_IPC_ADDR")
            .unwrap_or_else(|_| DEFAULT_IPC_ADDR.to_string());

        let ipc_sample_rate = buffer_config.sample_rate as u32;
        let ipc_channels = channels;
        let ipc_bitrate = self.params.bitrate_kbps.value() as u32;

        std::thread::Builder::new()
            .name("wail-ipc".into())
            .spawn(move || {
                ipc_thread(out_rx, in_tx, addr, ipc_sample_rate, ipc_channels, ipc_bitrate)
            })
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
        aux: &mut AuxiliaryBuffers,
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

                // Interleave input into pre-allocated buffer (zeros if not recording)
                let buf_size = num_samples * num_channels as usize;
                ensure_buf(&mut self.interleave_buf, buf_size);
                let interleave = &mut self.interleave_buf[..buf_size];
                if send_enabled && playing {
                    for sample_idx in 0..num_samples {
                        for ch in 0..num_channels as usize {
                            interleave[sample_idx * num_channels as usize + ch] =
                                buffer.as_slice()[ch][sample_idx];
                        }
                    }
                } else {
                    interleave.fill(0.0);
                }

                ensure_buf(&mut self.playback_buf, buf_size);
                let playback = &mut self.playback_buf[..buf_size];
                playback.fill(0.0);

                // permit_alloc: feed_decoded may push to pending_remote, and
                // process_rt may allocate once at interval boundaries (copying
                // recorded samples to CompletedInterval for IPC transfer).
                // Normal per-buffer operation is allocation-free.
                let completed = permit_alloc(|| {
                    if let Some(ref rx) = self.ipc_incoming_rx {
                        while let Ok((peer_id, interval_index, samples)) = rx.try_recv() {
                            bridge.feed_decoded(&peer_id, interval_index, samples);
                        }
                    }
                    bridge.process_rt(interleave, playback, beat_position)
                });

                // Mix playback into DAW main output
                if receive_enabled {
                    for sample_idx in 0..num_samples {
                        for ch in 0..num_channels as usize {
                            let pb_idx = sample_idx * num_channels as usize + ch;
                            buffer.as_slice()[ch][sample_idx] += playback[pb_idx] * volume;
                        }
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
                            aux_buf.as_slice()[ch][sample_idx] = peer_buf[pb_idx] * volume;
                        }
                    }
                }

                // Send raw completed intervals to IPC thread for encoding.
                // permit_alloc: the Vec<CompletedInterval> is deallocated when
                // consumed/dropped, which counts as an alloc event.
                permit_alloc(|| {
                    if let Some(ref tx) = self.ipc_outgoing_tx {
                        let sr = bridge.sample_rate();
                        let ch = bridge.channels();
                        let bpm_snap = bridge.bpm();
                        let q = bridge.quantum();
                        let b = bridge.bars();
                        for c in completed {
                            let _ = tx.try_send(RawInterval {
                                completed: c,
                                sample_rate: sr,
                                channels: ch,
                                bpm: bpm_snap,
                                quantum: q,
                                bars: b,
                            });
                        }
                    }
                });
            }
        }

        ProcessStatus::Normal
    }
}

/// IPC background thread: owns Opus encoder/decoder, connects to wail-app via TCP.
///
/// - Receives raw PCM from audio thread, Opus-encodes, sends over TCP
/// - Receives wire bytes from TCP, Opus-decodes, sends PCM to audio thread
///
/// This keeps all Opus DSP off the real-time audio callback.
fn ipc_thread(
    outgoing_rx: Receiver<RawInterval>,
    incoming_tx: Sender<(String, i64, Vec<f32>)>,
    addr: String,
    sample_rate: u32,
    channels: u16,
    bitrate_kbps: u32,
) {
    // Create encoder/decoder on the IPC thread (not the audio thread)
    let mut encoder = match AudioEncoder::new(sample_rate, channels, bitrate_kbps) {
        Ok(enc) => Some(enc),
        Err(e) => {
            tracing::warn!(error = %e, "IPC thread: failed to create Opus encoder");
            None
        }
    };
    let mut decoder = match AudioDecoder::new(sample_rate, channels) {
        Ok(dec) => Some(dec),
        Err(e) => {
            tracing::warn!(error = %e, "IPC thread: failed to create Opus decoder");
            None
        }
    };

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
            // Opus-encode and send any pending outgoing intervals
            let mut send_error = false;
            while let Ok(raw) = outgoing_rx.try_recv() {
                if let Some(ref mut enc) = encoder {
                    match enc.encode_interval(&raw.completed.samples) {
                        Ok(opus_data) => {
                            let num_frames = (raw.completed.samples.len()
                                / raw.channels as usize) as u32;
                            let interval = AudioInterval {
                                index: raw.completed.index,
                                opus_data,
                                sample_rate: raw.sample_rate,
                                channels: raw.channels,
                                num_frames,
                                bpm: raw.bpm,
                                quantum: raw.quantum,
                                bars: raw.bars,
                            };
                            let wire_data = AudioWire::encode(&interval);
                            let msg = IpcMessage::encode_audio("", &wire_data);
                            let frame = IpcFramer::encode_frame(&msg);
                            if stream.write_all(&frame).is_err() {
                                send_error = true;
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "IPC thread: failed to encode interval");
                        }
                    }
                }
            }
            if send_error {
                tracing::warn!("Plugin IPC write error — reconnecting");
                break;
            }

            // Read incoming remote intervals, Opus-decode, send PCM to audio thread
            match stream.read(&mut read_buf) {
                Ok(0) => {
                    tracing::info!("Plugin IPC connection closed");
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
                                                    "IPC thread: failed to decode Opus audio"
                                                );
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "IPC thread: failed to decode wire data"
                                    );
                                }
                            }
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
