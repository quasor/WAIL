use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::span;
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

// Loki credentials come from environment variables:
//   GRAFANA_LOKI_URL (e.g., https://logs-prod-021.grafana.net/loki/api/v1/push)
//   GRAFANA_LOKI_USER (e.g., user ID)
//   GRAFANA_LOKI_TOKEN (e.g., API token)
// Defaults to no-op if not configured (logs are only sent to console/stdout).

fn get_loki_url() -> Option<String> {
    std::env::var("GRAFANA_LOKI_URL").ok()
}

fn get_loki_user() -> Option<String> {
    std::env::var("GRAFANA_LOKI_USER").ok()
}

fn get_loki_token() -> Option<String> {
    std::env::var("GRAFANA_LOKI_TOKEN").ok()
}

const FLUSH_INTERVAL_SECS: u64 = 5;
const MAX_BUFFER_LINES: usize = 10_000;

// ---------------------------------------------------------------------------
// Span field storage (kept in span extensions)
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct SpanFields(BTreeMap<String, String>);

impl Visit for SpanFields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{:?}", value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0
            .insert(field.name().to_string(), value.to_string());
    }
}

// ---------------------------------------------------------------------------
// Event field visitor
// ---------------------------------------------------------------------------

#[derive(Default)]
struct EventVisitor {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else {
            self.fields
                .push((field.name().to_string(), format!("{:?}", value)));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
    }
}

// ---------------------------------------------------------------------------
// LokiLayer
// ---------------------------------------------------------------------------

/// Shared handle to toggle telemetry on/off at runtime.
#[derive(Clone)]
pub struct TelemetryHandle {
    enabled: Arc<AtomicBool>,
}

impl TelemetryHandle {
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }
}

/// A buffered log line with its level for Loki stream labeling.
struct LogLine {
    level: String,
    timestamp_ns: String,
    message: String,
}

pub struct LokiLayer {
    tx: mpsc::UnboundedSender<LogLine>,
    enabled: Arc<AtomicBool>,
}

impl LokiLayer {
    pub fn new() -> (Self, TelemetryHandle) {
        let (tx, rx) = mpsc::unbounded_channel();
        let enabled = Arc::new(AtomicBool::new(true));

        // Spawn flusher on a dedicated thread with its own tokio runtime so it
        // doesn't depend on the Tauri async runtime lifecycle.
        std::thread::Builder::new()
            .name("loki-flusher".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("loki tokio runtime");
                rt.block_on(flusher_loop(rx));
            })
            .expect("loki thread");

        let handle = TelemetryHandle {
            enabled: enabled.clone(),
        };
        (Self { tx, enabled }, handle)
    }
}

impl<S> Layer<S> for LokiLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut fields = SpanFields::default();
            attrs.record(&mut fields);
            span.extensions_mut().insert(fields);
        }
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut ext = span.extensions_mut();
            if let Some(fields) = ext.get_mut::<SpanFields>() {
                values.record(fields);
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }

        let metadata = event.metadata();
        let level = metadata.level().as_str().to_lowercase();
        let target = metadata.target();

        // Extract event message and fields
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);

        // Collect span fields from parent scope
        let mut span_fields: BTreeMap<String, String> = BTreeMap::new();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope {
                if let Some(fields) = span.extensions().get::<SpanFields>() {
                    span_fields.extend(fields.0.iter().map(|(k, v)| (k.clone(), v.clone())));
                }
            }
        }

        // Append any event-level fields
        for (k, v) in &visitor.fields {
            span_fields.insert(k.clone(), v.clone());
        }

        let span_str = if span_fields.is_empty() {
            String::new()
        } else {
            let pairs: Vec<String> = span_fields
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            format!(" [{}]", pairs.join(" "))
        };

        let timestamp_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_string();

        let message = format!("{target}{span_str} {}", visitor.message);
        let _ = self.tx.send(LogLine {
            level,
            timestamp_ns,
            message,
        });
    }
}

// ---------------------------------------------------------------------------
// Background flusher
// ---------------------------------------------------------------------------

async fn flusher_loop(mut rx: mpsc::UnboundedReceiver<LogLine>) {
    let client = reqwest::Client::new();
    let mut buffer: Vec<LogLine> = Vec::new();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(FLUSH_INTERVAL_SECS));

    loop {
        tokio::select! {
            maybe_line = rx.recv() => {
                match maybe_line {
                    Some(line) => {
                        buffer.push(line);
                        // Cap buffer size — drop oldest if over limit
                        if buffer.len() > MAX_BUFFER_LINES {
                            let excess = buffer.len() - MAX_BUFFER_LINES;
                            buffer.drain(..excess);
                        }
                    }
                    None => {
                        // Channel closed — final flush and exit
                        flush(&client, &mut buffer).await;
                        return;
                    }
                }
            }
            _ = interval.tick() => {
                flush(&client, &mut buffer).await;
            }
        }
    }
}

async fn flush(client: &reqwest::Client, buffer: &mut Vec<LogLine>) {
    if buffer.is_empty() {
        return;
    }

    // If Loki credentials are not configured, skip remote flush (logs only go to console).
    let url = match get_loki_url() {
        Some(u) => u,
        None => {
            return;
        }
    };

    let user = match get_loki_user() {
        Some(u) => u,
        None => {
            return;
        }
    };

    let token = match get_loki_token() {
        Some(t) => t,
        None => {
            return;
        }
    };

    // Group log lines by level for Loki stream semantics
    let mut streams: BTreeMap<&str, Vec<[&str; 2]>> = BTreeMap::new();
    for line in buffer.iter() {
        streams
            .entry(&line.level)
            .or_default()
            .push([&line.timestamp_ns, &line.message]);
    }

    let streams_json: Vec<serde_json::Value> = streams
        .into_iter()
        .map(|(level, values)| {
            serde_json::json!({
                "stream": { "app": "wail-tauri", "level": level },
                "values": values,
            })
        })
        .collect();

    let body = serde_json::json!({ "streams": streams_json });

    match client
        .post(&url)
        .basic_auth(&user, Some(&token))
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            buffer.clear();
        }
        Ok(resp) => {
            eprintln!(
                "[loki] flush failed: HTTP {} — keeping {} lines in buffer",
                resp.status(),
                buffer.len()
            );
        }
        Err(e) => {
            eprintln!(
                "[loki] flush error: {e} — keeping {} lines in buffer",
                buffer.len()
            );
        }
    }
}
