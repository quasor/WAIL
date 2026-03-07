use std::collections::HashMap;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

use wail_audio::{AudioDecoder, AudioFrameWire, FrameAssembler};

/// Configuration for local session recording.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingConfig {
    pub enabled: bool,
    pub directory: String,
    pub stems: bool,
    pub retention_days: u32,
}

/// Commands sent from the session loop to the writer task.
enum RecordCommand {
    PeerInterval { peer_id: String, display_name: Option<String>, wire_data: Vec<u8> },
    Finalize,
}

/// Peer metadata for session.json.
#[derive(Serialize)]
struct PeerEntry {
    peer_id: String,
    display_name: Option<String>,
}

/// Session metadata written to session.json on finalize.
#[derive(Serialize)]
struct SessionMetadata {
    version: u32,
    room: String,
    started_at: String,
    ended_at: String,
    sample_rate: u32,
    channels: u16,
    stems: bool,
    peers: Vec<PeerEntry>,
    files: Vec<String>,
}

/// Manages recording for a single session.
/// Audio is sent via channel and written on a blocking task.
pub struct SessionRecorder {
    tx: mpsc::UnboundedSender<RecordCommand>,
    bytes_written: Arc<AtomicU64>,
}

impl SessionRecorder {
    /// Start a new recording session.
    pub fn start(config: RecordingConfig, room: &str) -> Result<Self> {
        // Run cleanup before starting
        if config.retention_days > 0 {
            if let Err(e) = cleanup_old_sessions(Path::new(&config.directory), config.retention_days) {
                warn!("Recording cleanup failed: {e}");
            }
        }

        let session_dir = create_session_dir(&config.directory, room)?;
        let (tx, rx) = mpsc::unbounded_channel();
        let bytes_written = Arc::new(AtomicU64::new(0));
        let bytes_clone = bytes_written.clone();

        let room_owned = room.to_string();
        info!(dir = %session_dir.display(), "Recording session to disk");

        tokio::task::spawn_blocking(move || {
            let mut writer = RecorderWriter::new(config, session_dir, room_owned, bytes_clone);
            writer.run(rx);
        });

        Ok(Self { tx, bytes_written })
    }

    pub fn record_peer(&self, peer_id: String, display_name: Option<String>, wire_data: Vec<u8>) {
        if let Err(e) = self.tx.send(RecordCommand::PeerInterval { peer_id, display_name, wire_data }) {
            warn!("Recording: failed to send peer interval: {e}");
        }
    }

    pub fn finalize(&self) {
        if let Err(e) = self.tx.send(RecordCommand::Finalize) {
            warn!("Recording: failed to send finalize command: {e}");
        }
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }
}

/// Internal writer state running on a blocking task.
struct RecorderWriter {
    config: RecordingConfig,
    session_dir: PathBuf,
    room: String,
    bytes_written: Arc<AtomicU64>,
    started_at: chrono::DateTime<chrono::Utc>,
    /// Per-source WAV writers keyed by source id ("self" or peer_id)
    writers: HashMap<String, StemWriter>,
    /// Per-source Opus decoders keyed by (peer_id, stream_id)
    decoders: HashMap<(String, u16), AudioDecoder>,
    /// Assembles incoming WAIF streaming frames into complete intervals
    assembler: FrameAssembler,
    /// Peer metadata for session.json
    peers: HashMap<String, Option<String>>,
    /// For mixed mode: buffer decoded PCM per interval index, keyed by index
    mix_buffer: HashMap<i64, Vec<f32>>,
    /// Highest interval index seen so far
    max_interval: Option<i64>,
    /// The mix writer (used in mixed mode only)
    mix_writer: Option<StemWriter>,
    /// Detected sample rate and channels from first interval
    sample_rate: Option<u32>,
    channels: Option<u16>,
}

struct StemWriter {
    writer: hound::WavWriter<BufWriter<std::fs::File>>,
    path: PathBuf,
    samples_written: u64,
}

impl StemWriter {
    fn new(path: &Path, sample_rate: u32, channels: u16) -> Result<Self> {
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let file = std::fs::File::create(path)?;
        let buf = BufWriter::new(file);
        let writer = hound::WavWriter::new(buf, spec)?;
        Ok(Self {
            writer,
            path: path.to_path_buf(),
            samples_written: 0,
        })
    }

    fn write_samples(&mut self, samples: &[f32]) -> Result<()> {
        for &s in samples {
            self.writer.write_sample(s)?;
        }
        self.samples_written += samples.len() as u64;
        Ok(())
    }

    fn finalize(self) -> Result<()> {
        self.writer.finalize()?;
        Ok(())
    }
}

impl RecorderWriter {
    fn new(config: RecordingConfig, session_dir: PathBuf, room: String, bytes_written: Arc<AtomicU64>) -> Self {
        Self {
            config,
            session_dir,
            room,
            bytes_written,
            started_at: chrono::Utc::now(),
            writers: HashMap::new(),
            decoders: HashMap::new(),
            assembler: FrameAssembler::new(),
            peers: HashMap::new(),
            mix_buffer: HashMap::new(),
            max_interval: None,
            mix_writer: None,
            sample_rate: None,
            channels: None,
        }
    }

    fn run(&mut self, rx: mpsc::UnboundedReceiver<RecordCommand>) {
        // Use blocking_recv in the spawn_blocking context
        let mut rx = rx;
        loop {
            match rx.blocking_recv() {
                Some(RecordCommand::PeerInterval { peer_id, display_name, wire_data }) => {
                    self.peers.entry(peer_id.clone()).or_insert(display_name.clone());
                    self.handle_waif_frame(&peer_id, display_name.as_deref(), &wire_data);
                }
                Some(RecordCommand::Finalize) => {
                    self.finalize();
                    return;
                }
                None => {
                    // Channel closed — session ended without explicit finalize
                    self.finalize();
                    return;
                }
            }
        }
    }

    fn handle_waif_frame(&mut self, source_id: &str, display_name: Option<&str>, wire_data: &[u8]) {
        let frame = match AudioFrameWire::decode(wire_data) {
            Ok(f) => f,
            Err(e) => {
                warn!(source = source_id, "Recording: failed to decode WAIF frame: {e}");
                return;
            }
        };

        if frame.is_final {
            self.assembler.evict_stale(frame.interval_index);
        }

        let assembled = match self.assembler.insert(source_id, &frame) {
            Some(a) => a,
            None => return, // interval not yet complete
        };

        let sr = assembled.sample_rate;
        let ch = assembled.channels;

        // Detect format from first completed interval
        if self.sample_rate.is_none() {
            self.sample_rate = Some(sr);
            self.channels = Some(ch);
        }

        let dec_key = (source_id.to_string(), assembled.stream_id);
        if !self.decoders.contains_key(&dec_key) {
            let decoder = match AudioDecoder::new(sr, ch) {
                Ok(d) => d,
                Err(e) => {
                    warn!(source = source_id, "Recording: failed to create decoder: {e}");
                    match AudioDecoder::new(48000, 1) {
                        Ok(d) => d,
                        Err(e2) => {
                            warn!(source = source_id, "Recording: fallback decoder also failed: {e2}");
                            return;
                        }
                    }
                }
            };
            self.decoders.insert(dec_key.clone(), decoder);
        }
        let decoder = self.decoders.get_mut(&dec_key).unwrap();

        let pcm = match decoder.decode_interval(&assembled.opus_data) {
            Ok(pcm) => pcm,
            Err(e) => {
                warn!(source = source_id, "Recording: failed to decode opus: {e}");
                return;
            }
        };

        if self.config.stems {
            self.write_stems(source_id, display_name, sr, ch, &pcm);
        } else {
            self.write_mixed(source_id, display_name, sr, ch, assembled.interval_index, &pcm);
        }

        self.update_bytes_written();
    }

    fn write_stems(&mut self, source_id: &str, display_name: Option<&str>, sr: u32, ch: u16, pcm: &[f32]) {
        // Create writer lazily
        if !self.writers.contains_key(source_id) {
            let filename = stem_filename(source_id, display_name);
            let path = self.session_dir.join(&filename);
            match StemWriter::new(&path, sr, ch) {
                Ok(w) => {
                    info!(file = %path.display(), "Recording: opened stem file");
                    self.writers.insert(source_id.to_string(), w);
                }
                Err(e) => {
                    warn!(source = source_id, "Recording: failed to create WAV file: {e}");
                    return;
                }
            }
        }

        if let Some(writer) = self.writers.get_mut(source_id) {
            if let Err(e) = writer.write_samples(pcm) {
                warn!(source = source_id, "Recording: WAV write failed: {e}");
            }
        }
    }

    fn write_mixed(&mut self, _source_id: &str, _display_name: Option<&str>, sr: u32, ch: u16, interval_index: i64, pcm: &[f32]) {
        // Flush any earlier intervals before processing the new one
        if let Some(max) = self.max_interval {
            if interval_index > max {
                // Flush all intervals up to (but not including) the current one
                self.flush_mix_buffer(sr, ch);
            }
        }
        self.max_interval = Some(self.max_interval.map_or(interval_index, |m| m.max(interval_index)));

        // Sum this source's PCM into the mix buffer for this interval
        let buf = self.mix_buffer.entry(interval_index).or_default();
        if buf.is_empty() {
            buf.extend_from_slice(pcm);
        } else {
            // Sum: extend if this source is longer, add sample-by-sample
            if pcm.len() > buf.len() {
                buf.resize(pcm.len(), 0.0);
            }
            for (i, &s) in pcm.iter().enumerate() {
                buf[i] += s;
            }
        }
    }

    fn flush_mix_buffer(&mut self, sr: u32, ch: u16) {
        // Collect all interval indices except the current max (which may still be accumulating)
        let max = match self.max_interval {
            Some(m) => m,
            None => return,
        };

        let to_flush: Vec<i64> = self.mix_buffer.keys()
            .filter(|&&idx| idx < max)
            .copied()
            .collect();

        // Sort to write in order
        let mut to_flush = to_flush;
        to_flush.sort();

        for idx in to_flush {
            if let Some(pcm) = self.mix_buffer.remove(&idx) {
                // Ensure mix writer exists
                if self.mix_writer.is_none() {
                    let path = self.session_dir.join("mix.wav");
                    match StemWriter::new(&path, sr, ch) {
                        Ok(w) => {
                            info!(file = %path.display(), "Recording: opened mix file");
                            self.mix_writer = Some(w);
                        }
                        Err(e) => {
                            warn!("Recording: failed to create mix WAV: {e}");
                            return;
                        }
                    }
                }

                if let Some(ref mut writer) = self.mix_writer {
                    if let Err(e) = writer.write_samples(&pcm) {
                        warn!("Recording: mix WAV write failed: {e}");
                    }
                }
            }
        }
    }

    fn update_bytes_written(&self) {
        let mut total = 0u64;
        for w in self.writers.values() {
            // 4 bytes per f32 sample
            total += w.samples_written * 4;
        }
        if let Some(ref w) = self.mix_writer {
            total += w.samples_written * 4;
        }
        self.bytes_written.store(total, Ordering::Relaxed);
    }

    fn finalize(&mut self) {
        let sr = self.sample_rate.unwrap_or(48000);
        let ch = self.channels.unwrap_or(1);

        // Flush remaining mix buffer
        if !self.config.stems {
            // Flush everything including the current max
            if let Some(max) = self.max_interval {
                // Temporarily bump max so flush_mix_buffer flushes everything
                self.max_interval = Some(max + 1);
                self.flush_mix_buffer(sr, ch);
            }
        }

        // Collect file names for metadata
        let mut files = Vec::new();

        // Finalize all stem writers
        let writers = std::mem::take(&mut self.writers);
        for (source_id, writer) in writers {
            let filename = writer.path.file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("{source_id}.wav"));
            files.push(filename);
            if let Err(e) = writer.finalize() {
                warn!(source = source_id, "Recording: failed to finalize WAV: {e}");
            }
        }

        // Finalize mix writer
        if let Some(writer) = self.mix_writer.take() {
            files.push("mix.wav".to_string());
            if let Err(e) = writer.finalize() {
                warn!("Recording: failed to finalize mix WAV: {e}");
            }
        }

        // Write session.json
        let peers: Vec<PeerEntry> = self.peers.iter()
            .map(|(id, name)| PeerEntry {
                peer_id: id.clone(),
                display_name: name.clone(),
            })
            .collect();

        let metadata = SessionMetadata {
            version: 1,
            room: self.room.clone(),
            started_at: self.started_at.to_rfc3339(),
            ended_at: chrono::Utc::now().to_rfc3339(),
            sample_rate: sr,
            channels: ch,
            stems: self.config.stems,
            peers,
            files,
        };

        let meta_path = self.session_dir.join("session.json");
        match serde_json::to_string_pretty(&metadata) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&meta_path, json) {
                    warn!("Recording: failed to write session.json: {e}");
                }
            }
            Err(e) => {
                warn!("Recording: failed to serialize session.json: {e}");
            }
        }

        self.update_bytes_written();
        info!(dir = %self.session_dir.display(), "Recording finalized");
    }
}

/// Generate a filename for a stem WAV file.
fn stem_filename(source_id: &str, display_name: Option<&str>) -> String {
    if source_id == "self" {
        return "self.wav".to_string();
    }
    let name_part = display_name
        .map(|n| sanitize_filename(n))
        .unwrap_or_else(|| "anonymous".to_string());
    format!("peer_{name_part}_{source_id}.wav")
}

/// Sanitize a string for use in filenames.
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(32)
        .collect()
}

/// Create the session recording directory.
fn create_session_dir(base: &str, room: &str) -> Result<PathBuf> {
    let now = chrono::Local::now();
    let timestamp = now.format("%Y-%m-%d_%H-%M-%S").to_string();
    let safe_room = sanitize_filename(room);
    let dir_name = format!("{timestamp}_{safe_room}");
    let dir = Path::new(base).join(dir_name);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Delete recording sessions older than retention_days.
pub fn cleanup_old_sessions(base_dir: &Path, retention_days: u32) -> Result<(u32, u64)> {
    if retention_days == 0 {
        return Ok((0, 0));
    }

    let cutoff = chrono::Utc::now() - chrono::Duration::days(retention_days as i64);
    let mut deleted = 0u32;
    let mut freed = 0u64;

    if !base_dir.exists() {
        return Ok((0, 0));
    }

    let entries = match std::fs::read_dir(base_dir) {
        Ok(e) => e,
        Err(_) => return Ok((0, 0)),
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();
        // Parse date from directory name: YYYY-MM-DD_HH-MM-SS_room
        if let Some(date_str) = name.get(..19) {
            if let Ok(date) = chrono::NaiveDateTime::parse_from_str(date_str, "%Y-%m-%d_%H-%M-%S") {
                let dt = date.and_utc();
                if dt < cutoff {
                    let size = dir_size(&entry.path());
                    if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                        warn!(dir = %entry.path().display(), "Failed to delete old recording: {e}");
                        continue;
                    }
                    deleted += 1;
                    freed += size;
                    info!(dir = %entry.path().display(), "Deleted old recording session");
                }
            }
        }
    }

    Ok((deleted, freed))
}

/// Calculate the total size of a directory recursively.
fn dir_size(path: &Path) -> u64 {
    let mut size = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                size += dir_size(&entry.path());
            } else {
                size += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    size
}

/// Returns the platform-appropriate default recording directory.
pub fn default_recording_dir() -> Result<String> {
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("HOME environment variable not set"))?;
    let dir = Path::new(&home).join("Music").join("WAIL Sessions");
    Ok(dir.to_string_lossy().to_string())
}
