use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_ARCHIVE_FILES: u32 = 9; // wail.log.1 .. wail.log.9

// ---------------------------------------------------------------------------
// Event field visitor
// ---------------------------------------------------------------------------

#[derive(Default)]
struct EventVisitor {
    message: String,
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        }
    }
}

// ---------------------------------------------------------------------------
// TelemetryHandle
// ---------------------------------------------------------------------------

/// Shared handle to toggle file logging on/off at runtime and set the log directory.
#[derive(Clone)]
pub struct TelemetryHandle {
    enabled: Arc<AtomicBool>,
    state: Arc<Mutex<FileLogState>>,
}

impl TelemetryHandle {
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Open (or create) the log file in `dir`. Called once from Tauri setup.
    pub fn set_log_dir(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        let path = dir.join("wail.log");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let current_size = file.metadata()?.len();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.writer = Some(BufWriter::new(file));
        state.path = path;
        state.current_size = current_size;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

struct FileLogState {
    writer: Option<BufWriter<File>>,
    path: PathBuf,
    current_size: u64,
}

impl Default for FileLogState {
    fn default() -> Self {
        Self {
            writer: None,
            path: PathBuf::new(),
            current_size: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// FileLogLayer
// ---------------------------------------------------------------------------

pub struct FileLogLayer {
    state: Arc<Mutex<FileLogState>>,
    enabled: Arc<AtomicBool>,
}

impl FileLogLayer {
    pub fn new() -> (Self, TelemetryHandle) {
        let enabled = Arc::new(AtomicBool::new(true));
        let state = Arc::new(Mutex::new(FileLogState::default()));
        let handle = TelemetryHandle {
            enabled: enabled.clone(),
            state: state.clone(),
        };
        (Self { state, enabled }, handle)
    }
}

impl<S> Layer<S> for FileLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }

        let metadata = event.metadata();
        let level = metadata.level().as_str();
        let target = metadata.target();

        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);

        // Simple ISO-8601 UTC timestamp from SystemTime
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let timestamp = format_timestamp(secs);

        let line = format!("{timestamp} {level} {target} {}\n", visitor.message);
        let line_bytes = line.len() as u64;

        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };

        let writer = match state.writer.as_mut() {
            Some(w) => w,
            None => return, // log dir not set yet; event goes to fmt_layer only
        };

        if writer.write_all(line.as_bytes()).is_ok() {
            let _ = writer.flush();
            state.current_size += line_bytes;
        }

        if state.current_size >= MAX_FILE_BYTES {
            // Close current writer before rotating
            state.writer = None;
            rotate(&state.path);
            // Re-open fresh log file
            if let Ok(file) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&state.path)
            {
                state.current_size = 0;
                state.writer = Some(BufWriter::new(file));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rotation: wail.log.8 → .9, ..., wail.log → .1  (keeps ≤10 files total)
// ---------------------------------------------------------------------------

fn rotate(log_path: &Path) {
    let dir = match log_path.parent() {
        Some(d) => d,
        None => return,
    };
    let stem = match log_path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return,
    };

    // Delete the oldest archive to keep at most MAX_ARCHIVE_FILES
    let oldest = dir.join(format!("{stem}.{MAX_ARCHIVE_FILES}"));
    if oldest.exists() {
        let _ = fs::remove_file(&oldest);
    }

    // Shift .1 through .(MAX_ARCHIVE_FILES-1) up by one
    for i in (1..MAX_ARCHIVE_FILES).rev() {
        let src = dir.join(format!("{stem}.{i}"));
        let dst = dir.join(format!("{stem}.{}", i + 1));
        if src.exists() {
            let _ = fs::rename(&src, &dst);
        }
    }

    // Rename current log to .1
    let archive1 = dir.join(format!("{stem}.1"));
    let _ = fs::rename(log_path, &archive1);
}

// ---------------------------------------------------------------------------
// Minimal timestamp formatter (no external deps)
// ---------------------------------------------------------------------------

fn format_timestamp(secs: u64) -> String {
    // Days since Unix epoch → calendar date
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let h = time_of_day / 3600;
    let m = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;

    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_to_ymd(mut days: u64) -> (u32, u32, u32) {
    // Gregorian calendar computation
    let mut year = 1970u32;
    loop {
        let leap = is_leap(year);
        let days_in_year = if leap { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let leap = is_leap(year);
    let month_days: [u64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u32;
    for &md in &month_days {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }
    (year, month, days as u32 + 1)
}

fn is_leap(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}
