use std::io::Write as _;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use assert_no_alloc::permit_alloc;
use crossbeam_channel::Sender;
use nih_plug::prelude::*;

mod params;

use params::WailSendParams;
use wail_audio::{
    nearest_opus_rate, AudioBridge, AudioEncoder, AudioInterval, AudioWire, CompletedInterval,
    IpcFramer, IpcMessage, IPC_ROLE_SEND,
};

/// Default IPC address (overridable via WAIL_IPC_ADDR env var).
const DEFAULT_IPC_ADDR: &str = "127.0.0.1:9191";

const DEFAULT_BARS: u32 = 4;
const DEFAULT_QUANTUM: f64 = 4.0;
const DEFAULT_BITRATE_KBPS: u32 = 128;

/// Raw completed interval with config snapshot, sent from audio thread to IPC
/// thread for Opus encoding (keeps Opus off the real-time audio callback).
struct RawInterval {
    completed: CompletedInterval,
    stream_id: u16,
    sample_rate: u32,
    channels: u16,
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
    ipc_outgoing_tx: Option<Sender<RawInterval>>,
    /// Pre-allocated interleaved input buffer (reused every process call)
    interleave_buf: Vec<f32>,
    /// Pre-allocated dummy playback buffer (bridge.process_rt requires it)
    playback_buf: Vec<f32>,
    /// Cumulative samples processed — fallback beat source when host
    /// doesn't provide `pos_beats()`.
    cumulative_samples: u64,
    beat_fallback_warned: bool,
}

impl Default for WailSendPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(WailSendParams::default()),
            bridge: Arc::new(Mutex::new(None)),
            sample_rate: 48000.0,
            ipc_outgoing_tx: None,
            interleave_buf: Vec::new(),
            playback_buf: Vec::new(),
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
        let bridge = AudioBridge::new(
            buffer_config.sample_rate as u32,
            channels,
            DEFAULT_BARS,
            DEFAULT_QUANTUM,
            DEFAULT_BITRATE_KBPS,
        );

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

        let (out_tx, out_rx) = crossbeam_channel::bounded::<RawInterval>(64);
        self.ipc_outgoing_tx = Some(out_tx);

        let addr = std::env::var("WAIL_IPC_ADDR")
            .unwrap_or_else(|_| DEFAULT_IPC_ADDR.to_string());

        let ipc_sample_rate = buffer_config.sample_rate as u32;
        let ipc_channels = channels;
        let ipc_bitrate = DEFAULT_BITRATE_KBPS;
        let ipc_params = self.params.clone();

        std::thread::Builder::new()
            .name("wail-ipc-send".into())
            .spawn(move || {
                ipc_thread_send(out_rx, addr, ipc_sample_rate, ipc_channels, ipc_bitrate, ipc_params)
            })
            .ok();

        nih_log!(
            "WAIL Send initialized: {}Hz, {} channels, {} bars",
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
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let transport = context.transport();
        let num_channels = buffer.channels() as u16;
        let num_samples = buffer.samples();

        let bpm = transport.tempo.unwrap_or(120.0);
        let playing = transport.playing;

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

                let completed = permit_alloc(|| {
                    bridge.process_rt(interleave, playback, beat_position)
                });

                // Send completed intervals to IPC thread for encoding
                permit_alloc(|| {
                    if let Some(ref tx) = self.ipc_outgoing_tx {
                        let sr = bridge.sample_rate();
                        let ch = bridge.channels();
                        let bpm_snap = bridge.bpm();
                        let q = bridge.quantum();
                        let b = bridge.bars();
                        let stream_id = self.params.stream_index.value() as u16;
                        for c in completed {
                            let _ = tx.try_send(RawInterval {
                                completed: c,
                                stream_id,
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

        if !self.params.passthrough.value() {
            for ch in buffer.as_slice() {
                for sample in ch.iter_mut() {
                    *sample = 0.0;
                }
            }
        }

        ProcessStatus::Normal
    }
}

/// Send-only IPC thread: Opus-encodes completed intervals and writes to TCP.
fn ipc_thread_send(
    outgoing_rx: crossbeam_channel::Receiver<RawInterval>,
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

    let mut encoder = match AudioEncoder::new(opus_rate, channels, bitrate_kbps) {
        Ok(enc) => Some(enc),
        Err(e) => {
            tracing::warn!(error = %e, "IPC send thread: failed to create Opus encoder");
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

        loop {
            // Block waiting for the next interval (with timeout so we detect disconnects)
            match outgoing_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(raw) => {
                    if let Some(ref mut enc) = encoder {
                        match enc.encode_interval(&raw.completed.samples) {
                            Ok(opus_data) => {
                                let num_frames = (raw.completed.samples.len()
                                    / raw.channels as usize)
                                    as u32;
                                let interval = AudioInterval {
                                    index: raw.completed.index,
                                    stream_id: raw.stream_id,
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
                                    tracing::warn!("WAIL Send IPC write error — reconnecting");
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "IPC send: failed to encode interval");
                            }
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    // No data — check if TCP is still alive by writing nothing
                    // (the next real write will detect if it's broken)
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    tracing::info!("WAIL Send IPC: audio channel closed, shutting down");
                    return;
                }
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
