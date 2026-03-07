//! CLAP plugin test harness for WAIL plugins.
//!
//! Loads real `.clap` binaries and drives them with synthetic audio and
//! transport, enabling end-to-end testing without a DAW.

use std::io::Read as _;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clack_host::prelude::*;
use wail_audio::{
    AudioEncoder, AudioFrame, AudioFrameWire, IpcFramer, IpcMessage, IpcRecvBuffer, IPC_ROLE_SEND,
};

// ---------------------------------------------------------------------------
// Minimal host handler types (no-op — we just need to drive process)
// ---------------------------------------------------------------------------

pub struct TestHost;

pub struct TestHostShared;

impl<'a> SharedHandler<'a> for TestHostShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

pub struct TestHostMainThread;

impl<'a> MainThreadHandler<'a> for TestHostMainThread {}

pub struct TestHostAudioProcessor;

impl<'a> AudioProcessorHandler<'a> for TestHostAudioProcessor {}

impl HostHandlers for TestHost {
    type Shared<'a> = TestHostShared;
    type MainThread<'a> = TestHostMainThread;
    type AudioProcessor<'a> = TestHostAudioProcessor;
}

// ---------------------------------------------------------------------------
// ClapTestHost: load, activate, process
// ---------------------------------------------------------------------------

/// A minimal CLAP host for testing WAIL plugins.
///
/// Loads a `.clap` binary, instantiates a plugin by CLAP ID, activates it,
/// and provides helpers for driving audio processing.
pub struct ClapTestHost {
    instance: PluginInstance<TestHost>,
    _entry: PluginEntry,
    pub sample_rate: f64,
    pub max_frames: u32,
}

impl ClapTestHost {
    /// Load a `.clap` plugin bundle and instantiate the plugin with the given CLAP ID.
    ///
    /// # Safety
    /// Loading external dynamic libraries is inherently unsafe.
    pub unsafe fn load(path: &Path, clap_id: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let entry = PluginEntry::load(path)?;

        let plugin_factory = entry
            .get_plugin_factory()
            .ok_or("Plugin has no factory")?;

        let plugin_id = plugin_factory
            .plugin_descriptors()
            .find(|d| d.id().unwrap().to_bytes() == clap_id.as_bytes())
            .ok_or_else(|| format!("Plugin ID '{}' not found in bundle", clap_id))?
            .id()
            .unwrap();

        let host_info =
            HostInfo::new("WAIL Test Host", "WAIL Project", "", "0.1.0")?;

        let instance = PluginInstance::<TestHost>::new(
            |_| TestHostShared,
            |_| TestHostMainThread,
            &entry,
            &plugin_id,
            &host_info,
        )?;

        Ok(Self {
            instance,
            _entry: entry,
            sample_rate: 48000.0,
            max_frames: 4096,
        })
    }

    /// Activate the plugin for audio processing.
    /// Returns a `StoppedPluginAudioProcessor` that must be started before processing.
    pub fn activate(
        &mut self,
        sample_rate: f64,
        min_frames: u32,
        max_frames: u32,
    ) -> Result<StoppedPluginAudioProcessor<TestHost>, Box<dyn std::error::Error>> {
        self.sample_rate = sample_rate;
        self.max_frames = max_frames;

        let config = PluginAudioConfiguration {
            sample_rate,
            min_frames_count: min_frames,
            max_frames_count: max_frames,
        };

        let processor = self
            .instance
            .activate(|_, _| TestHostAudioProcessor, config)?;

        Ok(processor)
    }

    /// Deactivate the audio processor.
    pub fn deactivate(
        &mut self,
        processor: StoppedPluginAudioProcessor<TestHost>,
    ) {
        self.instance.deactivate(processor);
    }

    /// Leak the host to prevent the `.clap` dynamic library from being unloaded.
    ///
    /// WAIL plugins spawn background IPC threads that outlive the plugin instance.
    /// If the dynamic library is unloaded while those threads are still running,
    /// the process crashes (SIGSEGV). Leaking the host keeps the library loaded
    /// for the remainder of the test process.
    pub fn leak(self) {
        std::mem::forget(self);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the path to a built `.clap` bundle in `target/bundled/`.
pub fn find_plugin_bundle(plugin_name: &str) -> PathBuf {
    // Walk up from this crate's manifest dir to find workspace root
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.pop(); // crates/
    dir.pop(); // workspace root
    dir.join(format!("target/bundled/{plugin_name}.clap"))
}

/// Generate a sine wave test signal (interleaved stereo).
pub fn sine_wave(freq_hz: f32, num_samples: usize, channels: u16, sample_rate: u32) -> Vec<f32> {
    let mut out = Vec::with_capacity(num_samples * channels as usize);
    for i in 0..num_samples {
        let t = i as f32 / sample_rate as f32;
        let sample = (t * freq_hz * 2.0 * std::f32::consts::PI).sin() * 0.5;
        for _ in 0..channels {
            out.push(sample);
        }
    }
    out
}

/// Compute RMS energy of a signal.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

// ---------------------------------------------------------------------------
// IPC test helpers
// ---------------------------------------------------------------------------

/// Bind a TCP listener on a random available port. Returns the listener and address.
pub fn random_listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind random port");
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

/// Accept one IPC connection with a timeout. Returns the stream, role byte, and stream_index.
///
/// WAIL send plugins write 3 bytes on connect: role byte + stream_index (u16 LE).
/// WAIL recv plugins write 1 byte: role byte only.
/// stream_index is 0 for recv plugins.
pub fn accept_ipc_connection(listener: &TcpListener, timeout: Duration) -> (TcpStream, u8, u16) {
    listener
        .set_nonblocking(false)
        .expect("Failed to set blocking mode");
    // Use SO_RCVTIMEO via the underlying socket for accept timeout
    // by polling in a loop with short sleeps
    let deadline = std::time::Instant::now() + timeout;
    listener.set_nonblocking(true).unwrap();

    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() > deadline {
                    panic!("Timed out waiting for IPC connection ({timeout:?})");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("Accept failed: {e}"),
        }
    };

    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let mut role_buf = [0u8; 1];
    stream
        .try_clone()
        .unwrap()
        .read_exact(&mut role_buf)
        .expect("Failed to read role byte from plugin");
    let role = role_buf[0];

    // Send plugins send 2 additional bytes: stream_index as u16 LE
    let stream_index = if role == IPC_ROLE_SEND {
        let mut si_buf = [0u8; 2];
        stream
            .try_clone()
            .unwrap()
            .read_exact(&mut si_buf)
            .expect("Failed to read stream_index from send plugin");
        u16::from_le_bytes(si_buf)
    } else {
        0
    };

    (stream, role, stream_index)
}

/// Read one complete IPC frame from a stream with a timeout.
///
/// `recv_buf` must be kept alive across calls so that bytes read in one call
/// but belonging to a later frame are not discarded.  A single TCP `read()`
/// may return data for multiple IPC frames; without a persistent buffer the
/// extra bytes would be silently lost and the next call would block forever.
pub fn read_ipc_frame(
    stream: &mut TcpStream,
    recv_buf: &mut IpcRecvBuffer,
    timeout: Duration,
) -> Vec<u8> {
    stream.set_read_timeout(Some(timeout)).unwrap();
    let mut read_buf = [0u8; 65536];

    loop {
        // Return any frame already buffered from a previous read.
        if let Some(payload) = recv_buf.next_frame() {
            return payload;
        }
        match stream.read(&mut read_buf) {
            Ok(0) => panic!("IPC connection closed before a complete frame was received"),
            Ok(n) => {
                recv_buf.push(&read_buf[..n]);
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                panic!("Timed out waiting for IPC frame ({timeout:?})");
            }
            Err(e) => panic!("IPC read error: {e}"),
        }
    }
}

/// Build a complete IPC byte stream containing a test audio interval, encoded as
/// WAIF streaming frames (the format the recv plugin's FrameAssembler handles).
///
/// Returns a buffer with all WAIF IPC frames concatenated — ready to write
/// directly to a TCP stream for a recv plugin to consume.
pub fn make_test_interval_frame(peer_id: &str, interval_index: i64) -> Vec<u8> {
    let sr = 48000u32;
    let channels = 2u16;
    let bpm = 120.0;
    let quantum = 4.0;
    let bars = 4u32;

    // Generate one full interval of audio (16 beats at 120 BPM = 384,000 samples per channel)
    let samples_per_channel = (bars as f64 * quantum * 60.0 / bpm * sr as f64) as usize;
    let samples = sine_wave(440.0, samples_per_channel, channels, sr);

    let mut encoder =
        AudioEncoder::new(sr, channels, 128).expect("Failed to create Opus encoder");
    let opus_data = encoder
        .encode_interval(&samples)
        .expect("Failed to encode interval");

    // Parse the interval blob into individual Opus packets.
    // Format: [u32 frame_count][u16 len][opus bytes]...
    let num_frames = u32::from_le_bytes(opus_data[0..4].try_into().unwrap()) as usize;
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut offset = 4;
    while packets.len() < num_frames && offset + 2 <= opus_data.len() {
        let pkt_len =
            u16::from_le_bytes(opus_data[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;
        packets.push(opus_data[offset..offset + pkt_len].to_vec());
        offset += pkt_len;
    }

    // Encode each Opus packet as a WAIF streaming frame.
    // The plugin's FrameAssembler reassembles these into a complete interval.
    let total_frames = packets.len();
    let mut output = Vec::new();
    for (frame_number, packet) in packets.into_iter().enumerate() {
        let is_final = frame_number + 1 == total_frames;
        let frame = AudioFrame {
            interval_index,
            stream_id: 0,
            frame_number: frame_number as u32,
            channels,
            opus_data: packet,
            is_final,
            sample_rate: if is_final { sr } else { 0 },
            total_frames: if is_final { total_frames as u32 } else { 0 },
            bpm: if is_final { bpm } else { 0.0 },
            quantum: if is_final { quantum } else { 0.0 },
            bars: if is_final { bars } else { 0 },
        };
        let wire_data = AudioFrameWire::encode(&frame);
        let ipc_msg = IpcMessage::encode_audio(peer_id, &wire_data);
        output.extend(IpcFramer::encode_frame(&ipc_msg));
    }
    output
}
