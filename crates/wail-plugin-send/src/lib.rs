use std::io::Write as _;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use assert_no_alloc::permit_alloc;
use crossbeam_channel::Sender;
use nih_plug::prelude::*;
use nih_plug_egui::{create_egui_editor, egui, EguiState};

mod params;

use params::WailSendParams;
use wail_audio::{
    nearest_opus_rate, AudioBridge, AudioEncoder, AudioFrame, AudioFrameWire, IpcFramer,
    IpcMessage, IPC_ROLE_SEND,
};

/// Default IPC address (overridable via WAIL_IPC_ADDR env var).
const DEFAULT_IPC_ADDR: &str = "127.0.0.1:9191";

const DEFAULT_BARS: u32 = 4;
const DEFAULT_QUANTUM: f64 = 4.0;
const DEFAULT_BITRATE_KBPS: u32 = 128;

/// A single 20ms frame of raw PCM, sent from the audio thread to the IPC thread
/// for immediate Opus encoding and streaming transmission.
struct RawFrame {
    samples: Vec<f32>,
    interval_index: i64,
    stream_id: u16,
    frame_number: u32,
    channels: u16,
    is_final: bool,
    // Metadata for the final frame:
    sample_rate: u32,
    total_frames: u32,
    bpm: f64,
    quantum: f64,
    bars: u32,
}

/// WAIL Send Plugin: captures DAW audio per interval and sends it to wail-app
/// for network transmission. Output is silent by default; enable the Passthrough
/// parameter to pass input audio through to the output.
///
/// Architecture:
/// - Audio thread: drives IntervalRing (record only, no playback)
/// - IPC thread: Opus encode + TCP write to wail-app
/// - Communication: crossbeam channel from audio thread to IPC thread
pub struct WailSendPlugin {
    params: Arc<WailSendParams>,
    bridge: Arc<Mutex<Option<AudioBridge>>>,
    sample_rate: f32,
    /// Channel for streaming 20ms frames to the IPC thread
    frame_tx: Option<Sender<RawFrame>>,
    /// Pre-allocated interleaved input buffer (reused every process call)
    interleave_buf: Vec<f32>,
    /// Pre-allocated dummy playback buffer (bridge.process_rt requires it)
    playback_buf: Vec<f32>,
    /// Cumulative samples processed — fallback beat source when host
    /// doesn't provide `pos_beats()`.
    cumulative_samples: u64,
    beat_fallback_warned: bool,
    /// Accumulates interleaved samples until we have a full 20ms frame
    frame_buffer: Vec<f32>,
    /// Current interval index for streaming frame dispatch
    streaming_interval_index: Option<i64>,
    /// Current frame number within the interval (resets at boundary)
    streaming_frame_number: u32,
    /// Opus frame size in samples per channel (set during initialize)
    opus_frame_size: usize,
    /// Sender for returning cleared interval buffers to IntervalRing (zero-alloc recycling)
    buf_return_tx: Option<crossbeam_channel::Sender<Vec<f32>>>,
    /// Previous transport playing state for detecting stop→play transitions.
    was_playing: Option<bool>,
    editor_state: Arc<EguiState>,
}

impl Default for WailSendPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(WailSendParams::default()),
            bridge: Arc::new(Mutex::new(None)),
            sample_rate: 48000.0,
            frame_tx: None,
            interleave_buf: Vec::new(),
            playback_buf: Vec::new(),
            cumulative_samples: 0,
            beat_fallback_warned: false,
            frame_buffer: Vec::new(),
            streaming_interval_index: None,
            streaming_frame_number: 0,
            opus_frame_size: 960, // 20ms at 48kHz, updated in initialize
            buf_return_tx: None,
            was_playing: None,
            editor_state: EguiState::from_size(300, 130),
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

/// Compute beat position from cumulative sample count (fallback when host
/// doesn't provide `pos_beats()`).
fn beat_position_fallback(cumulative_samples: u64, bpm: f64, sample_rate: f64) -> f64 {
    cumulative_samples as f64 * bpm / (60.0 * sample_rate)
}

/// Interleave per-channel DAW buffers into a flat interleaved buffer.
///
/// `channels`: slice of per-channel sample slices (e.g. `buffer.as_slice()`)
/// `output`: destination buffer, must be at least `num_samples * num_channels` long
/// `num_samples`: number of samples per channel
/// `playing`: if false, output is filled with silence instead
fn interleave_channels(
    channels: &[&mut [f32]],
    output: &mut [f32],
    num_samples: usize,
    playing: bool,
) {
    let num_channels = channels.len();
    if !playing {
        output[..num_samples * num_channels].fill(0.0);
        return;
    }
    for sample_idx in 0..num_samples {
        for ch in 0..num_channels {
            output[sample_idx * num_channels + ch] = channels[ch][sample_idx];
        }
    }
}

impl Plugin for WailSendPlugin {
    const NAME: &'static str = "WAIL Send";
    const VENDOR: &'static str = "WAIL Project";
    const URL: &'static str = "https://github.com/MostDistant/WAIL";
    const EMAIL: &'static str = "";

    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        // Stereo in/out (output silent by default; passthrough param controls this)
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

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        // Disable GUI in debug builds to avoid crashes during development.
        if cfg!(debug_assertions) {
            return None;
        }
        create_egui_editor(
            self.editor_state.clone(),
            (),
            |_, _| {},
            |egui_ctx, _setter, _state| {
                egui::CentralPanel::default().show(egui_ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.heading("WAIL Send");
                        ui.label(format!("v{}", env!("CARGO_PKG_VERSION")));
                        ui.add_space(8.0);
                        ui.hyperlink_to(
                            "github.com/MostDistant/WAIL",
                            "https://github.com/MostDistant/WAIL",
                        );
                    });
                });
            },
        )
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
            .main_input_channels
            .map(|c| c.get() as u16)
            .unwrap_or(2);
        let mut bridge = AudioBridge::new(
            buffer_config.sample_rate as u32,
            channels,
            DEFAULT_BARS,
            DEFAULT_QUANTUM,
            DEFAULT_BITRATE_KBPS,
        );

        // Wire buffer return channel: completed interval buffers are recycled
        // back to IntervalRing, eliminating audio-thread allocations after warmup.
        let (buf_return_tx, buf_return_rx) = crossbeam_channel::bounded::<Vec<f32>>(8);
        bridge.set_buffer_return_rx(buf_return_rx);
        self.buf_return_tx = Some(buf_return_tx);

        let max_buf = buffer_config.max_buffer_size as usize * channels as usize;
        self.interleave_buf = vec![0.0f32; max_buf];
        self.playback_buf = vec![0.0f32; max_buf];

        match self.bridge.lock() {
            Ok(mut guard) => *guard = Some(bridge),
            Err(_) => {
                nih_error!("WAIL Send: bridge mutex poisoned, initialization failed");
                return false;
            }
        }

        let (ftx, frx) = crossbeam_channel::bounded::<RawFrame>(512);
        self.frame_tx = Some(ftx);

        let opus_rate = nearest_opus_rate(buffer_config.sample_rate as u32);
        self.opus_frame_size = (opus_rate as usize * 20) / 1000;
        self.frame_buffer = Vec::with_capacity(self.opus_frame_size * channels as usize);
        self.streaming_interval_index = None;
        self.streaming_frame_number = 0;

        let addr = std::env::var("WAIL_IPC_ADDR")
            .unwrap_or_else(|_| DEFAULT_IPC_ADDR.to_string());

        let ipc_sample_rate = buffer_config.sample_rate as u32;
        let ipc_channels = channels;
        let ipc_bitrate = DEFAULT_BITRATE_KBPS;
        let ipc_params = self.params.clone();

        if let Err(e) = std::thread::Builder::new()
            .name("wail-ipc-send".into())
            .spawn(move || {
                ipc_thread_send(frx, addr, ipc_sample_rate, ipc_channels, ipc_bitrate, ipc_params)
            })
        {
            nih_error!("WAIL Send: failed to spawn IPC thread: {}", e);
        }

        nih_log!(
            "WAIL Send initialized: {}Hz, {} channels, {} bars",
            buffer_config.sample_rate,
            channels,
            DEFAULT_BARS
        );

        true
    }

    fn reset(&mut self) {
        // reset() is called inside assert_no_alloc (via start_processing → process_wrapper).
        // AudioBridge::reset() drops RemoteIntervals/peer_identity_map Strings and
        // recreates the Opus encoder/decoder — wrap in permit_alloc.
        permit_alloc(|| {
            self.cumulative_samples = 0;
            self.frame_buffer.clear();
            self.streaming_interval_index = None;
            self.streaming_frame_number = 0;
            self.was_playing = None;
            if let Ok(mut bridge) = self.bridge.lock() {
                if let Some(ref mut b) = *bridge {
                    b.reset();
                }
            }
        });
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

        let bpm = transport.tempo.unwrap_or(120.0);
        let playing = transport.playing;

        // Detect transport restart (stopped → playing) and reset interval tracking
        // so beat position discontinuities don't leave stale buffer positions.
        let transport_restarted = self.was_playing == Some(false) && playing;
        self.was_playing = Some(playing);
        if transport_restarted {
            permit_alloc(|| {
                self.cumulative_samples = 0;
                self.streaming_interval_index = None;
                self.streaming_frame_number = 0;
                self.frame_buffer.clear();
                if let Ok(mut bridge) = self.bridge.lock() {
                    if let Some(ref mut b) = *bridge {
                        b.reset_transport();
                    }
                }
            });
        }

        let beat_position = match transport.pos_beats() {
            Some(b) => b,
            None => {
                if !self.beat_fallback_warned {
                    self.beat_fallback_warned = true;
                    nih_log!("WAIL Send: host does not provide beat position — using sample-count fallback");
                }
                beat_position_fallback(self.cumulative_samples, bpm, self.sample_rate as f64)
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

                // Interleave input into pre-allocated buffer
                let buf_size = num_samples * num_channels as usize;
                ensure_buf(&mut self.interleave_buf, buf_size);
                let interleave = &mut self.interleave_buf[..buf_size];
                interleave_channels(buffer.as_slice(), interleave, num_samples, playing);

                // Dummy playback buffer (we don't use the output)
                ensure_buf(&mut self.playback_buf, buf_size);
                let playback = &mut self.playback_buf[..buf_size];
                playback.fill(0.0);

                permit_alloc(|| {
                    let completed = bridge.process_rt(interleave, playback, beat_position);
                    if let Some(ref tx) = self.buf_return_tx {
                        for mut ci in completed {
                            ci.samples.clear();
                            if tx.try_send(ci.samples).is_err() {
                                nih_warn!("Send plugin: buffer return channel full — zero-alloc guarantee broken (capacity=8)");
                            }
                        }
                    }
                });

                // --- Streaming frame dispatch ---
                // Accumulate interleaved input into frame_buffer, dispatch
                // each full 20ms frame to the IPC thread immediately.
                permit_alloc(|| {
                    if let Some(ref ftx) = self.frame_tx {
                        let sr = bridge.sample_rate();
                        let ch = bridge.channels();
                        let bpm_snap = bridge.bpm();
                        let q = bridge.quantum();
                        let b = bridge.bars();
                        let stream_id = self.params.stream_index.value() as u16;
                        let frame_samples = self.opus_frame_size * ch as usize;
                        let interval_idx = bridge.current_interval_index();

                        // Detect interval boundary: flush partial + mark final
                        if let Some(prev_idx) = self.streaming_interval_index {
                            if prev_idx != interval_idx {
                                // Flush remaining samples as final frame (zero-padded),
                                // or send an empty final frame so the receiver knows
                                // the interval is complete.
                                let samples = std::mem::take(&mut self.frame_buffer);
                                let total_frames = if samples.is_empty() {
                                    self.streaming_frame_number
                                } else {
                                    self.streaming_frame_number + 1
                                };
                                if ftx.try_send(RawFrame {
                                    samples,
                                    interval_index: prev_idx,
                                    stream_id,
                                    frame_number: self.streaming_frame_number,
                                    channels: ch,
                                    is_final: true,
                                    sample_rate: sr,
                                    total_frames,
                                    bpm: bpm_snap,
                                    quantum: q,
                                    bars: b,
                                }).is_err() {
                                    nih_warn!("Send plugin: frame channel full at boundary — dropping audio frame (capacity=512)");
                                }
                                self.streaming_frame_number = 0;
                            }
                        }
                        self.streaming_interval_index = Some(interval_idx);

                        // Feed input into frame buffer
                        self.frame_buffer.extend_from_slice(interleave);

                        // Dispatch full 20ms frames
                        while self.frame_buffer.len() >= frame_samples {
                            let rest = self.frame_buffer.split_off(frame_samples);
                            let samples = std::mem::replace(&mut self.frame_buffer, rest);
                            if ftx.try_send(RawFrame {
                                samples,
                                interval_index: interval_idx,
                                stream_id,
                                frame_number: self.streaming_frame_number,
                                channels: ch,
                                is_final: false,
                                sample_rate: sr,
                                total_frames: 0,
                                bpm: bpm_snap,
                                quantum: q,
                                bars: b,
                            }).is_err() {
                                nih_warn!("Send plugin: frame channel full — dropping audio frame (capacity=512)");
                            }
                            self.streaming_frame_number += 1;
                        }
                    }
                });

            }
        }

        if !self.params.passthrough.value() {
            for ch in buffer.as_slice() {
                for sample in ch.iter_mut() {
                    *sample = 0.0;
                }
            }
        }

        ProcessStatus::KeepAlive
    }
}

/// Send-only IPC thread: Opus-encodes 20ms streaming frames and sends them to wail-app.
fn ipc_thread_send(
    frame_rx: crossbeam_channel::Receiver<RawFrame>,
    addr: String,
    sample_rate: u32,
    channels: u16,
    bitrate_kbps: u32,
    params: Arc<WailSendParams>,
) {
    let opus_rate = nearest_opus_rate(sample_rate);
    if opus_rate != sample_rate {
        tracing::warn!(
            daw_rate = sample_rate,
            opus_rate,
            "DAW sample rate is not a native Opus rate — encoding at {opus_rate}Hz"
        );
    }

    let mut frame_encoder = match AudioEncoder::new(opus_rate, channels, bitrate_kbps) {
        Ok(enc) => Some(enc),
        Err(e) => {
            tracing::warn!(error = %e, "IPC send thread: failed to create streaming Opus encoder");
            None
        }
    };

    loop {
        let mut stream = match TcpStream::connect(&addr) {
            Ok(s) => {
                tracing::info!(addr = %addr, "WAIL Send IPC connected to wail-app");
                s
            }
            Err(_) => {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        // Identify as a send plugin + stream index
        let stream_index = params.stream_index.value() as u16;
        let mut handshake = [0u8; 3];
        handshake[0] = IPC_ROLE_SEND;
        handshake[1..3].copy_from_slice(&stream_index.to_le_bytes());
        if stream.write_all(&handshake).is_err() {
            tracing::warn!("WAIL Send: failed to write handshake — reconnecting");
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        let mut write_failed = false;

        loop {
            match frame_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(raw_frame) => {
                    if let Some(ref mut enc) = frame_encoder {
                        match enc.encode_frame(&raw_frame.samples) {
                            Ok(opus_data) => {
                                let audio_frame = AudioFrame {
                                    interval_index: raw_frame.interval_index,
                                    stream_id: raw_frame.stream_id,
                                    frame_number: raw_frame.frame_number,
                                    channels: raw_frame.channels,
                                    opus_data,
                                    is_final: raw_frame.is_final,
                                    sample_rate: raw_frame.sample_rate,
                                    total_frames: raw_frame.total_frames,
                                    bpm: raw_frame.bpm,
                                    quantum: raw_frame.quantum,
                                    bars: raw_frame.bars,
                                };
                                let wire_data = AudioFrameWire::encode(&audio_frame);
                                let msg = IpcMessage::encode_audio_frame(&wire_data);
                                let frame = IpcFramer::encode_frame(&msg);
                                if stream.write_all(&frame).is_err() {
                                    tracing::warn!("WAIL Send IPC write error — reconnecting");
                                    write_failed = true;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "IPC send: failed to encode frame");
                            }
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    tracing::info!("WAIL Send IPC: frame channel closed, shutting down");
                    return;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    // Timeout — connection check will happen on next write
                }
            }

            if write_failed {
                break;
            }
        }

        std::thread::sleep(Duration::from_secs(1));
    }
}

impl ClapPlugin for WailSendPlugin {
    const CLAP_ID: &'static str = "com.wail.send";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("WAIL Send - captures DAW audio for network transmission");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::Utility, ClapFeature::Stereo];
}

impl Vst3Plugin for WailSendPlugin {
    const VST3_CLASS_ID: [u8; 16] = *b"WAILSendPlugin\0\0";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Network];
}

nih_export_clap!(WailSendPlugin);
nih_export_vst3!(WailSendPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beat_fallback_at_120bpm() {
        let sr = 48000.0;
        let bpm = 120.0;
        // At 120 BPM, 1 beat = 0.5s = 24000 samples
        assert!((beat_position_fallback(0, bpm, sr) - 0.0).abs() < 1e-9);
        assert!((beat_position_fallback(24000, bpm, sr) - 1.0).abs() < 1e-9);
        assert!((beat_position_fallback(48000, bpm, sr) - 2.0).abs() < 1e-9);
        // Full interval at 4 bars * 4 beats = 16 beats = 384000 samples
        assert!((beat_position_fallback(384000, bpm, sr) - 16.0).abs() < 1e-9);
    }

    #[test]
    fn beat_fallback_at_90bpm() {
        let sr = 44100.0;
        let bpm = 90.0;
        // At 90 BPM, 1 beat = 60/90 = 0.6667s = 29400 samples
        let one_beat_samples = (sr * 60.0 / bpm) as u64;
        assert!((beat_position_fallback(one_beat_samples, bpm, sr) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn beat_fallback_accumulates_across_buffers() {
        let sr = 48000.0;
        let bpm = 120.0;
        let buf_size: u64 = 256;
        let mut cumulative: u64 = 0;
        let beats_per_buffer = buf_size as f64 * bpm / (60.0 * sr);

        for i in 0..100 {
            let beat = beat_position_fallback(cumulative, bpm, sr);
            let expected = i as f64 * beats_per_buffer;
            assert!(
                (beat - expected).abs() < 1e-9,
                "buffer {i}: got {beat}, expected {expected}"
            );
            cumulative += buf_size;
        }
    }

    #[test]
    fn interleave_stereo() {
        let mut left = [1.0f32, 2.0, 3.0, 4.0];
        let mut right = [5.0f32, 6.0, 7.0, 8.0];
        let channels: &[&mut [f32]] = &[&mut left, &mut right];
        let mut output = vec![0.0f32; 8];

        interleave_channels(channels, &mut output, 4, true);

        assert_eq!(output, vec![1.0, 5.0, 2.0, 6.0, 3.0, 7.0, 4.0, 8.0]);
    }

    #[test]
    fn interleave_mono() {
        let mut mono = [0.5f32, 0.25, 0.75];
        let channels: &[&mut [f32]] = &[&mut mono];
        let mut output = vec![0.0f32; 3];

        interleave_channels(channels, &mut output, 3, true);

        assert_eq!(output, vec![0.5, 0.25, 0.75]);
    }

    #[test]
    fn interleave_silence_when_not_playing() {
        let mut left = [1.0f32, 2.0, 3.0];
        let mut right = [4.0f32, 5.0, 6.0];
        let channels: &[&mut [f32]] = &[&mut left, &mut right];
        let mut output = vec![99.0f32; 6];

        interleave_channels(channels, &mut output, 3, false);

        assert_eq!(output, vec![0.0; 6]);
    }

    #[test]
    fn ensure_buf_grows() {
        let mut buf = vec![1.0f32; 4];
        ensure_buf(&mut buf, 8);
        assert_eq!(buf.len(), 8);
        // Original values preserved
        assert_eq!(buf[0], 1.0);
        assert_eq!(buf[3], 1.0);
        // New values are zero
        assert_eq!(buf[4], 0.0);
    }

    #[test]
    fn ensure_buf_no_shrink() {
        let mut buf = vec![1.0f32; 8];
        ensure_buf(&mut buf, 4);
        assert_eq!(buf.len(), 8); // should not shrink
    }
}
