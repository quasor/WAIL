use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use assert_no_alloc::permit_alloc;
use crossbeam_channel::Receiver;
use nih_plug::prelude::*;
use nih_plug_egui::{create_egui_editor, EguiState};

mod editor;
mod params;

use editor::EditorData;
use params::WailRecvParams;
use wail_audio::{
    nearest_opus_rate, AudioBridge, AudioDecoder, AudioFrameWire,
    IpcMessage, IpcRecvBuffer, IPC_ROLE_RECV, IPC_TAG_AUDIO_PUB,
    IPC_TAG_PEER_JOINED_PUB, IPC_TAG_PEER_LEFT_PUB, IPC_TAG_PEER_NAME_PUB,
};

/// Peer lifecycle events sent from IPC thread to audio thread.
enum PeerEvent {
    Joined { peer_id: String, identity: String },
    Left { peer_id: String },
    NameChanged { peer_id: String, display_name: String },
    /// IPC connection established — update GUI status.
    Connected,
    /// IPC connection dropped — clear all audio state to stop stale playback.
    Disconnected,
}

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
    bridge: Option<AudioBridge>,
    sample_rate: f32,
    ipc_incoming_rx: Option<Receiver<(String, u16, i64, Vec<f32>)>>,
    /// Peer lifecycle events from IPC thread (for slot affinity)
    peer_event_rx: Option<Receiver<PeerEvent>>,
    /// Shutdown flag for the IPC thread (set on re-initialization)
    ipc_shutdown: Option<Arc<AtomicBool>>,
    /// Pre-allocated playback buffer (reused every process call)
    playback_buf: Vec<f32>,
    /// Pre-allocated per-peer buffers (reused every process call)
    peer_bufs: [Vec<f32>; wail_audio::MAX_REMOTE_PEERS],
    /// Cumulative samples processed — fallback beat source when host
    /// doesn't provide `pos_beats()`.
    cumulative_samples: u64,
    beat_fallback_warned: bool,
    /// Display names received via IPC, keyed by peer_id.
    /// Cached here because NameChanged arrives before slots are active.
    pending_names: HashMap<String, String>,
    /// The name most recently applied to each slot (indexed by slot).
    /// Used to avoid redundant `rescan_audio_port_names()` calls.
    applied_slot_names: Vec<Option<String>>,
    editor_state: Arc<EguiState>,
    /// Shared visualization state for the egui editor.
    editor_data: Arc<Mutex<EditorData>>,
    /// Previous transport playing state for detecting stop→play transitions.
    was_playing: Option<bool>,
    /// Last observed interval index — used to detect boundary crossings for markers.
    last_interval: i64,
    /// Whether the IPC thread has an active connection to wail-app.
    ipc_connected: bool,
    /// Latest interval index from incoming audio frames (Go app's Link timeline).
    /// Used to drive ring buffer boundaries instead of DAW transport position.
    remote_interval: Option<i64>,
}

impl Default for WailRecvPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(WailRecvParams::default()),
            bridge: None,
            sample_rate: 48000.0,
            ipc_incoming_rx: None,
            peer_event_rx: None,
            ipc_shutdown: None,
            playback_buf: Vec::new(),
            peer_bufs: Default::default(),
            cumulative_samples: 0,
            beat_fallback_warned: false,
            pending_names: HashMap::new(),
            applied_slot_names: vec![None; wail_audio::MAX_REMOTE_PEERS],
            was_playing: None,
            editor_state: EguiState::from_size(380, 460),
            editor_data: Arc::new(Mutex::new(EditorData::default())),
            last_interval: 0,
            ipc_connected: false,
            remote_interval: None,
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

/// De-interleave a flat interleaved buffer into per-channel DAW output slices.
///
/// `interleaved`: source buffer in interleaved format (L0 R0 L1 R1 ...)
/// `channels`: mutable slice of per-channel output slices
/// `num_samples`: number of samples per channel
fn deinterleave_to_channels(
    interleaved: &[f32],
    channels: &mut [&mut [f32]],
    num_samples: usize,
) {
    let num_channels = channels.len();
    for sample_idx in 0..num_samples {
        for ch in 0..num_channels {
            channels[ch][sample_idx] = interleaved[sample_idx * num_channels + ch];
        }
    }
}

/// Copy per-peer interleaved playback buffer into a per-channel auxiliary output.
///
/// `peer_buf`: interleaved samples for one peer
/// `aux_channels`: mutable slice of per-channel aux output slices
/// `num_samples`: samples to copy per channel
/// `src_channels`: number of channels in the interleaved source
fn write_peer_to_aux(
    peer_buf: &[f32],
    aux_channels: &mut [&mut [f32]],
    num_samples: usize,
    src_channels: usize,
) {
    for sample_idx in 0..num_samples {
        for ch in 0..aux_channels.len().min(src_channels) {
            aux_channels[ch][sample_idx] = peer_buf[sample_idx * src_channels + ch];
        }
    }
}

impl Plugin for WailRecvPlugin {
    const NAME: &'static str = "WAIL Recv";
    const VENDOR: &'static str = "WAIL Project";
    const URL: &'static str = "https://github.com/MostDistant/WAIL";
    const EMAIL: &'static str = "";

    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        // Stereo in/out with 15 per-peer/stream aux stereo outputs
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            aux_output_ports: &[new_nonzero_u32(2); 15],
            names: PortNames {
                aux_outputs: &[
                    "Slot 1", "Slot 2", "Slot 3", "Slot 4",
                    "Slot 5", "Slot 6", "Slot 7", "Slot 8",
                    "Slot 9", "Slot 10", "Slot 11", "Slot 12",
                    "Slot 13", "Slot 14", "Slot 15",
                ],
                ..PortNames::const_default()
            },
            ..AudioIOLayout::const_default()
        },
        // Stereo fallback (no aux — for hosts that don't support aux ports)
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
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

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        // egui + baseview crashes in debug builds (likely OpenGL context issue
        // in the nih_plug_egui baseview backend). Keep the GUI disabled until
        // we find the root cause or switch to a different windowing backend.
        if cfg!(debug_assertions) {
            return None;
        }
        let data = self.editor_data.clone();
        create_egui_editor(
            self.editor_state.clone(),
            data,
            |_, _| {},
            |egui_ctx, _setter, state| {
                editor::draw_editor(egui_ctx, state);
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

        self.bridge = Some(bridge);

        // Signal the old IPC thread to shut down before spawning a new one
        if let Some(ref flag) = self.ipc_shutdown {
            flag.store(true, Ordering::Relaxed);
        }

        let (in_tx, in_rx) = crossbeam_channel::bounded::<(String, u16, i64, Vec<f32>)>(512);
        self.ipc_incoming_rx = Some(in_rx);

        let (peer_tx, peer_rx) = crossbeam_channel::bounded::<PeerEvent>(32);
        self.peer_event_rx = Some(peer_rx);

        let shutdown = Arc::new(AtomicBool::new(false));
        self.ipc_shutdown = Some(shutdown.clone());

        let addr = std::env::var("WAIL_IPC_ADDR")
            .unwrap_or_else(|_| DEFAULT_IPC_ADDR.to_string());

        let ipc_sample_rate = buffer_config.sample_rate as u32;
        let ipc_channels = channels;

        if let Err(e) = std::thread::Builder::new()
            .name("wail-ipc-recv".into())
            .spawn(move || {
                ipc_thread_recv(in_tx, peer_tx, addr, ipc_sample_rate, ipc_channels, shutdown)
            })
        {
            nih_error!("WAIL Recv: failed to spawn IPC thread: {}", e);
        }

        nih_log!(
            "WAIL Recv initialized: {}Hz, {} channels, {} bars",
            buffer_config.sample_rate,
            channels,
            DEFAULT_BARS
        );

        true
    }

    fn reset(&mut self) {
        // reset() is called inside assert_no_alloc (via start_processing → process_wrapper).
        // Dropping Strings (pending_names, applied_slot_names, RemoteInterval, peer_identity_map)
        // and recreating the Opus encoder/decoder all require allocation — wrap in permit_alloc.
        permit_alloc(|| {
            self.cumulative_samples = 0;
            self.was_playing = None;
            self.last_interval = 0;
            self.remote_interval = None;
            self.pending_names.clear();
            for name in &mut self.applied_slot_names {
                *name = None;
            }
            if let Some(ref mut b) = self.bridge {
                b.reset();
            }
        });
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
        let playing = transport.playing;

        // Detect transport restart (stopped → playing) and reset interval tracking
        // so beat position discontinuities don't leave stale buffer positions.
        let transport_restarted = self.was_playing == Some(false) && playing;
        self.was_playing = Some(playing);
        if transport_restarted {
            permit_alloc(|| {
                self.cumulative_samples = 0;
                if let Some(ref mut b) = self.bridge {
                    b.reset_transport();
                }
            });
        }

        let beat_position = match transport.pos_beats() {
            Some(b) => b,
            None => {
                if !self.beat_fallback_warned {
                    self.beat_fallback_warned = true;
                    nih_log!("WAIL Recv: host does not provide beat position — using sample-count fallback");
                }
                beat_position_fallback(self.cumulative_samples, bpm, self.sample_rate as f64)
            }
        };
        self.cumulative_samples += num_samples as u64;

        // Sampled diagnostic: log beat position every ~2 seconds (96000 samples at 48kHz)
        if self.cumulative_samples % 96000 < num_samples as u64 {
            tracing::debug!(
                beat = format!("{:.2}", beat_position),
                bpm = format!("{:.1}", bpm),
                remote_interval = ?self.remote_interval,
                playing = transport.playing,
                "RECV beat"
            );
        }

        if let Some(ref mut bridge) = self.bridge {
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
                let mut port_names_dirty = false;
                permit_alloc(|| {
                    // Process peer lifecycle events (slot affinity)
                    if let Some(ref rx) = self.peer_event_rx {
                        while let Ok(event) = rx.try_recv() {
                            match event {
                                PeerEvent::Joined { peer_id, identity } => {
                                    bridge.notify_peer_joined(&peer_id, &identity);
                                }
                                PeerEvent::Left { peer_id } => {
                                    // Clear applied names for this peer's slots so they
                                    // get renamed again if the peer reconnects.
                                    for (slot, pid, _) in bridge.peer_info() {
                                        if pid == peer_id {
                                            self.applied_slot_names[slot] = None;
                                        }
                                    }
                                    bridge.remove_peer(&peer_id);
                                }
                                PeerEvent::NameChanged { peer_id, display_name } => {
                                    // Cache the name — slots may not be active yet when this
                                    // arrives, so we apply deferred below after audio is fed.
                                    self.pending_names.insert(peer_id, display_name);
                                }
                                PeerEvent::Connected => {
                                    self.ipc_connected = true;
                                }
                                PeerEvent::Disconnected => {
                                    self.ipc_connected = false;
                                    self.remote_interval = None;
                                    tracing::info!("WAIL Recv: IPC disconnected — clearing audio state");
                                    bridge.reset();
                                    // Drain any buffered frames so stale audio doesn't play
                                    if let Some(ref rx) = self.ipc_incoming_rx {
                                        while rx.try_recv().is_ok() {}
                                    }
                                    self.pending_names.clear();
                                    for name in &mut self.applied_slot_names {
                                        *name = None;
                                    }
                                }
                            }
                        }
                    }

                    if let Some(ref rx) = self.ipc_incoming_rx {
                        while let Ok((peer_id, stream_id, interval_index, samples)) = rx.try_recv() {
                            // Track the latest interval index from the Go app's
                            // Link timeline. This drives ring buffer boundaries
                            // instead of the DAW's transport position.
                            match self.remote_interval {
                                Some(prev) if interval_index > prev => {
                                    self.remote_interval = Some(interval_index);
                                }
                                None => {
                                    self.remote_interval = Some(interval_index);
                                }
                                _ => {}
                            }
                            bridge.feed_decoded(peer_id, stream_id, interval_index, samples);
                        }
                    }
                    // Use a zero-length slice as silent input — IntervalRing
                    // handles zero-length input gracefully (nothing recorded).
                    // Drive ring boundaries from the Go app's interval index
                    // (via incoming audio) instead of the DAW's transport position,
                    // which may not match Link's beat timeline.
                    drop(bridge.process_rt_with_interval(&[], playback, self.remote_interval));

                    // Deferred name apply: slots become active only after audio arrives
                    // at an interval boundary. Apply any cached display names now.
                    if !self.pending_names.is_empty() {
                        let active = bridge.peer_info();
                        let mut counts: HashMap<&str, usize> = HashMap::new();
                        for (_, pid, _) in &active {
                            *counts.entry(pid.as_str()).or_insert(0) += 1;
                        }
                        for (slot, peer_id, stream_id) in &active {
                            if let Some(display_name) = self.pending_names.get(peer_id.as_str()) {
                                let multi = counts.get(peer_id.as_str()).copied().unwrap_or(1) > 1;
                                let name = if multi {
                                    format!("{} {}", display_name, stream_id + 1)
                                } else {
                                    display_name.clone()
                                };
                                if self.applied_slot_names.get(*slot).and_then(|n| n.as_deref()) != Some(name.as_str()) {
                                    context.set_aux_output_name(*slot, Some(name.clone()));
                                    if *slot < self.applied_slot_names.len() {
                                        self.applied_slot_names[*slot] = Some(name);
                                    }
                                    port_names_dirty = true;
                                }
                            }
                        }
                    }
                });
                if port_names_dirty {
                    context.rescan_audio_port_names();
                }

                // Mix playback into DAW main output
                deinterleave_to_channels(playback, buffer.as_slice(), num_samples);

                // Route per-peer audio to auxiliary output buses
                let num_aux = aux.outputs.len().min(wail_audio::MAX_REMOTE_PEERS);
                for slot_idx in 0..num_aux {
                    ensure_buf(&mut self.peer_bufs[slot_idx], buf_size);
                    let peer_buf = &mut self.peer_bufs[slot_idx][..buf_size];
                    peer_buf.fill(0.0);
                    bridge.read_peer_playback(slot_idx, peer_buf);

                    let aux_buf = &mut aux.outputs[slot_idx];
                    let n = aux_buf.samples().min(num_samples);
                    write_peer_to_aux(peer_buf, aux_buf.as_slice(), n, num_channels as usize);
                }

                // Update visualization state for the editor GUI
                let display_interval = self.remote_interval.unwrap_or(0);
                let is_boundary = display_interval != self.last_interval;
                self.last_interval = display_interval;

                permit_alloc(|| {
                    if let Ok(mut vis) = self.editor_data.try_lock() {
                        vis.ipc_connected = self.ipc_connected;
                        vis.bpm = bpm;
                        let interval_len = DEFAULT_BARS as f64 * DEFAULT_QUANTUM;
                        vis.interval_progress = ((beat_position % interval_len) / interval_len) as f32;
                        vis.current_interval = display_interval;

                        // Compute per-slot RMS and push to sparkline history
                        let active_info = bridge.peer_info();
                        for slot_idx in 0..num_aux {
                            let peer_buf = &self.peer_bufs[slot_idx][..buf_size];
                            let rms = editor::compute_rms(peer_buf);
                            let peak = editor::compute_peak(peer_buf);
                            vis.slots[slot_idx].push_rms(rms, is_boundary);
                            vis.slots[slot_idx].peak = peak;
                        }

                        // Sync slot active state and names
                        for slot_idx in 0..wail_audio::MAX_REMOTE_PEERS {
                            let is_active = active_info.iter().any(|(s, _, _)| *s == slot_idx);
                            vis.slots[slot_idx].active = is_active;
                        }
                        for (slot, peer_id, _) in &active_info {
                            if let Some(name) = self.pending_names.get(peer_id.as_str()) {
                                vis.slots[*slot].set_name(name);
                            }
                        }
                    }
                });
        }

        ProcessStatus::KeepAlive
    }
}

/// Assembles streaming audio frames into complete intervals for decoding.
///

/// Receive-only IPC thread: reads from TCP, Opus-decodes, sends PCM to audio thread.
fn ipc_thread_recv(
    incoming_tx: crossbeam_channel::Sender<(String, u16, i64, Vec<f32>)>,
    peer_event_tx: crossbeam_channel::Sender<PeerEvent>,
    addr: String,
    sample_rate: u32,
    channels: u16,
    shutdown: Arc<AtomicBool>,
) {
    let opus_rate = nearest_opus_rate(sample_rate);
    if opus_rate != sample_rate {
        tracing::warn!(
            daw_rate = sample_rate,
            opus_rate,
            "DAW sample rate is not a native Opus rate — decoding at {opus_rate}Hz"
        );
    }

    // Key by (peer_id, stream_id) — reuse decoder across intervals to avoid
    // the ~120ms Opus warm-up ramp that occurs with a fresh decoder.
    let mut decoders: HashMap<(String, u16), AudioDecoder> = HashMap::new();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            tracing::info!("WAIL Recv IPC thread: shutdown requested, exiting");
            return;
        }
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

        // Identify as a recv plugin
        if stream.write_all(&[IPC_ROLE_RECV]).is_err() {
            tracing::warn!("WAIL Recv: failed to write role byte — reconnecting");
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        // Notify audio thread that IPC is connected (for GUI status)
        let _ = peer_event_tx.try_send(PeerEvent::Connected);

        // Use a read timeout so the thread can detect when channels are disconnected
        // (e.g., when the DAW re-initializes the plugin and replaces receivers).
        if let Err(e) = stream.set_read_timeout(Some(Duration::from_secs(5))) {
            tracing::warn!(error = %e, "WAIL Recv: failed to set read timeout");
        }

        let mut recv_buf = IpcRecvBuffer::new();
        let mut read_buf = [0u8; 65536];

        loop {
            match stream.read(&mut read_buf) {
                Ok(0) => {
                    tracing::info!("WAIL Recv IPC connection closed");
                    let _ = peer_event_tx.try_send(PeerEvent::Disconnected);
                    decoders.clear();
                    break;
                }
                Ok(n) => {
                    recv_buf.push(&read_buf[..n]);
                    while let Some(payload) = recv_buf.next_frame() {
                        match IpcMessage::tag(&payload) {
                            Some(IPC_TAG_AUDIO_PUB) => {
                                if let Some((peer_id, wire_data)) = IpcMessage::decode_audio(&payload) {
                                    // Detect inner wire format by magic: WAIF = streaming frame, WAIL = full interval
                                    if wire_data.starts_with(b"WAIF") {
                                        match AudioFrameWire::decode(&wire_data) {
                                            Ok(frame) => {
                                                // Decode each frame incrementally instead of
                                                // buffering all frames and bulk-decoding when
                                                // the final frame arrives. This ensures decoded
                                                // PCM reaches the audio thread continuously,
                                                // avoiding dropout at interval boundaries.
                                                let dec_key = (peer_id.clone(), frame.stream_id);
                                                let dec = decoders.entry(dec_key).or_insert_with(|| {
                                                    match AudioDecoder::new(opus_rate, channels) {
                                                        Ok(d) => d,
                                                        Err(e) => {
                                                            tracing::warn!(error = %e, "IPC recv: failed to create decoder, using fallback");
                                                            AudioDecoder::new(48000, channels).expect("fallback opus decoder at known-good params")
                                                        }
                                                    }
                                                });
                                                if frame.opus_data.is_empty() {
                                                    // Empty final-marker frame: sender uses it to
                                                    // signal interval completion for stats, no PCM
                                                    // to decode. Running PLC here would synthesise
                                                    // 20 ms of silence at the interval tail.
                                                } else {
                                                    match dec.decode_frame(&frame.opus_data) {
                                                    Ok(samples) => {
                                                        if let Err(e) = incoming_tx.try_send((
                                                            peer_id.clone(),
                                                            frame.stream_id,
                                                            frame.interval_index,
                                                            samples,
                                                        )) {
                                                            tracing::warn!(
                                                                error = %e,
                                                                "IPC recv: failed to send decoded frame to audio thread (channel full)"
                                                            );
                                                        }
                                                    }
                                                    Err(e) => {
                                                        tracing::warn!(
                                                            error = %e,
                                                            "IPC recv: Opus decode failed, attempting PLC"
                                                        );
                                                        let samples = match dec.decode_frame(&[]) {
                                                            Ok(plc_samples) => {
                                                                tracing::debug!(
                                                                    len = plc_samples.len(),
                                                                    "IPC recv: PLC generated samples"
                                                                );
                                                                plc_samples
                                                            }
                                                            Err(plc_err) => {
                                                                tracing::warn!(
                                                                    error = %plc_err,
                                                                    "IPC recv: PLC also failed, inserting silence"
                                                                );
                                                                vec![0.0f32; dec.frame_size() * dec.channels() as usize]
                                                            }
                                                        };
                                                        if let Err(e) = incoming_tx.try_send((
                                                            peer_id.clone(),
                                                            frame.stream_id,
                                                            frame.interval_index,
                                                            samples,
                                                        )) {
                                                            tracing::warn!(
                                                                error = %e,
                                                                "IPC recv: failed to send PLC frame to audio thread (channel full)"
                                                            );
                                                        }
                                                    }
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    error = %e,
                                                    "IPC recv: failed to decode audio frame wire"
                                                );
                                            }
                                        }
                                    } else {
                                        tracing::warn!("IPC recv: unexpected non-WAIF audio data, ignoring");
                                    }
                                }
                            }
                            Some(IPC_TAG_PEER_JOINED_PUB) => {
                                if let Some((peer_id, identity)) = IpcMessage::decode_peer_joined(&payload) {
                                    if let Err(e) = peer_event_tx.try_send(PeerEvent::Joined { peer_id, identity }) {
                                        tracing::warn!(error = %e, "IPC recv: failed to send PeerJoined event (channel full)");
                                    }
                                }
                            }
                            Some(IPC_TAG_PEER_LEFT_PUB) => {
                                if let Some(peer_id) = IpcMessage::decode_peer_left(&payload) {
                                    // Clean up Opus decoders for the departing peer
                                    decoders.retain(|(pid, _), _| *pid != peer_id);
                                    if let Err(e) = peer_event_tx.try_send(PeerEvent::Left { peer_id }) {
                                        tracing::warn!(error = %e, "IPC recv: failed to send PeerLeft event (channel full)");
                                    }
                                }
                            }
                            Some(IPC_TAG_PEER_NAME_PUB) => {
                                if let Some((peer_id, display_name)) = IpcMessage::decode_peer_name(&payload) {
                                    if let Err(e) = peer_event_tx.try_send(PeerEvent::NameChanged { peer_id, display_name }) {
                                        tracing::warn!(error = %e, "IPC recv: failed to send PeerName event (channel full)");
                                    }
                                }
                            }
                            _ => {
                                tracing::debug!("IPC recv: unknown message tag");
                            }
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                    // Read timeout — check if shutdown was requested (plugin re-initialized)
                    if shutdown.load(Ordering::Relaxed) {
                        tracing::info!("WAIL Recv IPC thread: shutdown requested, exiting");
                        return;
                    }
                    continue;
                }
                Err(_) => {
                    tracing::warn!("WAIL Recv IPC read error — reconnecting");
                    let _ = peer_event_tx.try_send(PeerEvent::Disconnected);
                    decoders.clear();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beat_fallback_at_120bpm() {
        let sr = 48000.0;
        let bpm = 120.0;
        assert!((beat_position_fallback(0, bpm, sr) - 0.0).abs() < 1e-9);
        assert!((beat_position_fallback(24000, bpm, sr) - 1.0).abs() < 1e-9);
        assert!((beat_position_fallback(384000, bpm, sr) - 16.0).abs() < 1e-9);
    }

    #[test]
    fn deinterleave_stereo() {
        let interleaved = [1.0f32, 5.0, 2.0, 6.0, 3.0, 7.0, 4.0, 8.0];
        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        {
            let channels: &mut [&mut [f32]] = &mut [&mut left, &mut right];
            deinterleave_to_channels(&interleaved, channels, 4);
        }
        assert_eq!(left, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(right, [5.0, 6.0, 7.0, 8.0]);
    }

    #[test]
    fn deinterleave_mono() {
        let interleaved = [0.5f32, 0.25, 0.75];
        let mut mono = [0.0f32; 3];
        {
            let channels: &mut [&mut [f32]] = &mut [&mut mono];
            deinterleave_to_channels(&interleaved, channels, 3);
        }
        assert_eq!(mono, [0.5, 0.25, 0.75]);
    }

    #[test]
    fn deinterleave_roundtrips_with_interleave() {
        // Interleave then de-interleave should give back the original channels
        let left = [1.0f32, 2.0, 3.0, 4.0];
        let right = [5.0f32, 6.0, 7.0, 8.0];
        let num_samples = 4;
        let num_channels = 2;

        // Interleave (same logic as send plugin)
        let mut interleaved = vec![0.0f32; num_samples * num_channels];
        for i in 0..num_samples {
            for ch in 0..num_channels {
                interleaved[i * num_channels + ch] = if ch == 0 { left[i] } else { right[i] };
            }
        }

        // De-interleave
        let mut out_left = [0.0f32; 4];
        let mut out_right = [0.0f32; 4];
        {
            let channels: &mut [&mut [f32]] = &mut [&mut out_left, &mut out_right];
            deinterleave_to_channels(&interleaved, channels, num_samples);
        }
        assert_eq!(out_left, left);
        assert_eq!(out_right, right);
    }

    #[test]
    fn write_peer_to_aux_stereo() {
        // Peer buffer: interleaved stereo [L0 R0 L1 R1 L2 R2]
        let peer_buf = [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6];
        let mut aux_left = [0.0f32; 3];
        let mut aux_right = [0.0f32; 3];
        {
            let aux: &mut [&mut [f32]] = &mut [&mut aux_left, &mut aux_right];
            write_peer_to_aux(&peer_buf, aux, 3, 2);
        }
        assert_eq!(aux_left, [0.1, 0.3, 0.5]);
        assert_eq!(aux_right, [0.2, 0.4, 0.6]);
    }

    #[test]
    fn write_peer_to_aux_mono_source_stereo_aux() {
        // Mono source into stereo aux: only first channel gets data
        let peer_buf = [0.5f32, 0.75, 1.0];
        let mut aux_left = [0.0f32; 3];
        let mut aux_right = [0.0f32; 3];
        {
            let aux: &mut [&mut [f32]] = &mut [&mut aux_left, &mut aux_right];
            write_peer_to_aux(&peer_buf, aux, 3, 1);
        }
        assert_eq!(aux_left, [0.5, 0.75, 1.0]);
        assert_eq!(aux_right, [0.0, 0.0, 0.0]); // untouched
    }

    #[test]
    fn peer_isolation_different_data() {
        // Simulate 3 peers with distinct signals, verify isolation
        let peer0 = [1.0f32, 1.0, 1.0, 1.0]; // stereo: L=1 R=1
        let peer1 = [2.0f32, 2.0, 2.0, 2.0]; // stereo: L=2 R=2
        let peer2 = [3.0f32, 3.0, 3.0, 3.0]; // stereo: L=3 R=3

        let peers = [&peer0[..], &peer1[..], &peer2[..]];
        let mut results = [[0.0f32; 2]; 3]; // 3 peers, 2 samples each (left channel only)

        for (i, peer_buf) in peers.iter().enumerate() {
            let mut left = [0.0f32; 2];
            let mut right = [0.0f32; 2];
            {
                let aux: &mut [&mut [f32]] = &mut [&mut left, &mut right];
                write_peer_to_aux(peer_buf, aux, 2, 2);
            }
            results[i] = left;
        }

        // Each peer should produce its own distinct value
        assert_eq!(results[0], [1.0, 1.0]);
        assert_eq!(results[1], [2.0, 2.0]);
        assert_eq!(results[2], [3.0, 3.0]);
    }

    #[test]
    fn ensure_buf_grows() {
        let mut buf = vec![1.0f32; 4];
        ensure_buf(&mut buf, 8);
        assert_eq!(buf.len(), 8);
        assert_eq!(buf[0], 1.0);
        assert_eq!(buf[4], 0.0);
    }
}
