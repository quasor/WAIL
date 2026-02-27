use std::sync::{Arc, Mutex};

use nih_plug::prelude::*;

mod audio_bridge;
mod params;

use audio_bridge::AudioBridge;
use params::WailParams;

/// WAIL Plugin: captures DAW audio per interval, encodes with Opus,
/// and plays back remote peers' audio intervals.
///
/// Architecture:
/// - Audio thread: writes to IntervalRecorder, reads from IntervalPlayer
/// - Background thread: Opus encode/decode, IPC with wail-app
/// - Communication: lock-free channels between threads
pub struct WailPlugin {
    params: Arc<WailParams>,
    /// Bridge between audio thread and background (Opus + IPC)
    bridge: Arc<Mutex<Option<AudioBridge>>>,
    /// Current sample rate (set on initialize)
    sample_rate: f32,
}

impl Default for WailPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(WailParams::default()),
            bridge: Arc::new(Mutex::new(None)),
            sample_rate: 48000.0,
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
                nih_log::error!("WAIL plugin bridge mutex poisoned, initialization failed");
                return false;
            }
        }

        nih_log::info!(
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
        let beat_position = transport.pos_beats.unwrap_or(0.0);
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

                // Capture input audio into interval recorder
                if send_enabled && playing {
                    // Interleave the channel buffers for the recorder
                    let mut interleaved = Vec::with_capacity(num_samples * num_channels as usize);
                    for sample_idx in 0..num_samples {
                        for ch in 0..num_channels as usize {
                            interleaved.push(buffer.as_slice()[ch][sample_idx]);
                        }
                    }
                    bridge.capture_audio(&interleaved, beat_position);
                }

                // Mix received remote audio into the output
                if receive_enabled {
                    let mut playback_buf = vec![0.0f32; num_samples * num_channels as usize];
                    bridge.read_playback(&mut playback_buf);

                    // Mix into output buffer
                    for sample_idx in 0..num_samples {
                        for ch in 0..num_channels as usize {
                            let pb_idx = sample_idx * num_channels as usize + ch;
                            buffer.as_slice()[ch][sample_idx] += playback_buf[pb_idx] * volume;
                        }
                    }
                }
            }
        }

        ProcessStatus::Normal
    }
}


impl ClapPlugin for WailPlugin {
    const CLAP_ID: &'static str = "com.wail.intervalic-audio";
    const CLAP_DESCRIPTION: Option<&'static str> = Some(
        "WAIL - WebRTC Audio Interchange for Link. Intervalic audio sync over the internet."
    );
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::Utility,
        ClapFeature::Stereo,
    ];
}

impl Vst3Plugin for WailPlugin {
    const VST3_CLASS_ID: [u8; 16] = *b"WAILIntervalic0\0";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Fx,
        Vst3SubCategory::Network,
    ];
}

nih_export_clap!(WailPlugin);
nih_export_vst3!(WailPlugin);
