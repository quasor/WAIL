use nih_plug_egui::egui;
use std::sync::{Arc, Mutex};

/// Number of RMS samples in the sparkline history ring buffer.
/// At ~50 updates/sec (typical 960-sample buffers at 48kHz), this covers ~4 seconds.
const SPARKLINE_LEN: usize = 200;

/// Maximum bytes for a fixed-size peer name (UTF-8).
const NAME_BUF_LEN: usize = 64;

// ── Synthwave palette ──────────────────────────────────────────────────────────

const BG_COLOR: egui::Color32 = egui::Color32::from_rgb(0x12, 0x0f, 0x1e);
const PANEL_COLOR: egui::Color32 = egui::Color32::from_rgb(0x1a, 0x16, 0x2a);
const HEADER_COLOR: egui::Color32 = egui::Color32::from_rgb(0x22, 0x1d, 0x36);
const BORDER_COLOR: egui::Color32 = egui::Color32::from_rgb(0x3a, 0x30, 0x5a);
const TEXT_DIM: egui::Color32 = egui::Color32::from_rgb(0x88, 0x80, 0xa0);
const TEXT_BRIGHT: egui::Color32 = egui::Color32::from_rgb(0xe0, 0xd8, 0xf0);

const TEAL: egui::Color32 = egui::Color32::from_rgb(0x00, 0xe5, 0xcc);
const MAGENTA: egui::Color32 = egui::Color32::from_rgb(0xff, 0x2d, 0x95);
const WHITE_HOT: egui::Color32 = egui::Color32::from_rgb(0xff, 0xfa, 0xf0);

const INTERVAL_MARKER_COLOR: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x88, 0x80, 0xa0, 0x60);
const PROGRESS_BG: egui::Color32 = egui::Color32::from_rgb(0x2a, 0x24, 0x40);
const PROGRESS_FILL: egui::Color32 = egui::Color32::from_rgb(0x00, 0xb8, 0xa6);
const SLOT_BG: egui::Color32 = egui::Color32::from_rgb(0x18, 0x14, 0x26);

/// Per-slot visualization data. Fixed-size to avoid audio-thread allocations.
#[derive(Clone)]
pub struct SlotVisual {
    name_buf: [u8; NAME_BUF_LEN],
    name_len: usize,
    pub active: bool,
    rms_history: [f32; SPARKLINE_LEN],
    write_pos: usize,
    pub peak: f32,
    interval_markers: [bool; SPARKLINE_LEN],
}

impl Default for SlotVisual {
    fn default() -> Self {
        Self {
            name_buf: [0u8; NAME_BUF_LEN],
            name_len: 0,
            active: false,
            rms_history: [0.0; SPARKLINE_LEN],
            write_pos: 0,
            peak: 0.0,
            interval_markers: [false; SPARKLINE_LEN],
        }
    }
}

impl SlotVisual {
    /// Set the display name (copies up to NAME_BUF_LEN bytes, no allocation).
    pub fn set_name(&mut self, name: &str) {
        let bytes = name.as_bytes();
        let len = bytes.len().min(NAME_BUF_LEN);
        self.name_buf[..len].copy_from_slice(&bytes[..len]);
        self.name_len = len;
    }

    /// Read back the display name.
    pub fn name_str(&self) -> &str {
        std::str::from_utf8(&self.name_buf[..self.name_len]).unwrap_or("?")
    }

    /// Push a new RMS value into the circular history buffer.
    pub fn push_rms(&mut self, rms: f32, is_interval_boundary: bool) {
        self.rms_history[self.write_pos] = rms;
        self.interval_markers[self.write_pos] = is_interval_boundary;
        self.write_pos = (self.write_pos + 1) % SPARKLINE_LEN;
    }
}

/// Shared state between audio thread and GUI thread.
pub struct EditorData {
    pub slots: [SlotVisual; wail_audio::MAX_REMOTE_PEERS],
    pub bpm: f64,
    pub interval_progress: f32,
    pub current_interval: i64,
}

impl Default for EditorData {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| SlotVisual::default()),
            bpm: 120.0,
            interval_progress: 0.0,
            current_interval: 0,
        }
    }
}

/// Compute RMS of an interleaved sample buffer (all channels combined).
pub fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / samples.len() as f64).sqrt() as f32
}

/// Compute peak absolute value of an interleaved sample buffer.
pub fn compute_peak(samples: &[f32]) -> f32 {
    samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max)
}

/// Interpolate between two colors by `t` (0.0 = a, 1.0 = b).
fn lerp_color(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let inv = 1.0 - t;
    egui::Color32::from_rgb(
        (a.r() as f32 * inv + b.r() as f32 * t) as u8,
        (a.g() as f32 * inv + b.g() as f32 * t) as u8,
        (a.b() as f32 * inv + b.b() as f32 * t) as u8,
    )
}

/// Map RMS value (0.0–1.0) to synthwave gradient: teal → white → magenta.
fn sparkline_color(rms: f32) -> egui::Color32 {
    // Scale RMS nonlinearly for more visual range (approximate dB-like curve)
    let v = (rms * 4.0).clamp(0.0, 1.0).sqrt();
    if v < 0.5 {
        lerp_color(TEAL, WHITE_HOT, v * 2.0)
    } else {
        lerp_color(WHITE_HOT, MAGENTA, (v - 0.5) * 2.0)
    }
}

/// Main editor drawing function.
pub fn draw_editor(egui_ctx: &egui::Context, data: &Arc<Mutex<EditorData>>) {
    let snapshot = match data.lock() {
        Ok(guard) => EditorSnapshot::from(&*guard),
        Err(_) => return,
    };

    // Request periodic repaint for animation
    egui_ctx.request_repaint_after(std::time::Duration::from_millis(33));

    let frame = egui::Frame::new()
        .fill(BG_COLOR)
        .inner_margin(egui::Margin::same(0));

    egui::CentralPanel::default().frame(frame).show(egui_ctx, |ui| {
        ui.style_mut().visuals.override_text_color = Some(TEXT_BRIGHT);

        draw_header(ui, &snapshot);
        ui.add_space(4.0);
        draw_slots(ui, &snapshot);
        ui.add_space(4.0);
        draw_footer(ui);
    });
}

/// Snapshot of EditorData taken under the lock, so we don't hold the mutex
/// during rendering.
struct EditorSnapshot {
    slots: Vec<SlotSnapshot>,
    bpm: f64,
    interval_progress: f32,
    current_interval: i64,
}

struct SlotSnapshot {
    name: String,
    rms_history: [f32; SPARKLINE_LEN],
    write_pos: usize,
    peak: f32,
    interval_markers: [bool; SPARKLINE_LEN],
}

impl EditorSnapshot {
    fn from(data: &EditorData) -> Self {
        let slots = data
            .slots
            .iter()
            .filter(|s| s.active)
            .map(|s| SlotSnapshot {
                name: s.name_str().to_string(),
                rms_history: s.rms_history,
                write_pos: s.write_pos,
                peak: s.peak,
                interval_markers: s.interval_markers,
            })
            .collect();
        Self {
            slots,
            bpm: data.bpm,
            interval_progress: data.interval_progress,
            current_interval: data.current_interval,
        }
    }
}

fn draw_header(ui: &mut egui::Ui, snap: &EditorSnapshot) {
    let header_frame = egui::Frame::new()
        .fill(HEADER_COLOR)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .stroke(egui::Stroke::new(1.0, BORDER_COLOR));

    header_frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("WAIL Recv")
                    .size(16.0)
                    .color(TEAL)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!("{:.1} BPM", snap.bpm))
                        .size(13.0)
                        .color(TEXT_DIM),
                );
            });
        });

        ui.add_space(4.0);

        // Interval progress bar
        let available_width = ui.available_width();
        let bar_height = 6.0;
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(available_width, bar_height),
            egui::Sense::hover(),
        );

        let painter = ui.painter();
        painter.rect_filled(rect, 3.0, PROGRESS_BG);

        let fill_width = rect.width() * snap.interval_progress.clamp(0.0, 1.0);
        if fill_width > 0.5 {
            let fill_rect = egui::Rect::from_min_size(rect.min, egui::vec2(fill_width, bar_height));
            painter.rect_filled(fill_rect, 3.0, PROGRESS_FILL);
        }

        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("Interval {}", snap.current_interval))
                    .size(10.0)
                    .color(TEXT_DIM),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let n = snap.slots.len();
                let label = if n == 1 {
                    "1 peer".to_string()
                } else {
                    format!("{} peers", n)
                };
                ui.label(egui::RichText::new(label).size(10.0).color(TEXT_DIM));
            });
        });
    });
}

fn draw_slots(ui: &mut egui::Ui, snap: &EditorSnapshot) {
    if snap.slots.is_empty() {
        let available = ui.available_size();
        ui.allocate_new_ui(
            egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(ui.cursor().min, available)),
            |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new("Waiting for peers...")
                            .size(13.0)
                            .color(TEXT_DIM),
                    );
                });
            },
        );
        return;
    }

    let slot_frame = egui::Frame::new()
        .fill(PANEL_COLOR)
        .inner_margin(egui::Margin::symmetric(8, 4));

    slot_frame.show(ui, |ui| {
        for slot in &snap.slots {
            draw_slot_row(ui, slot);
            ui.add_space(2.0);
        }
    });
}

fn draw_slot_row(ui: &mut egui::Ui, slot: &SlotSnapshot) {
    let row_height = 28.0;
    let name_width = 72.0;

    let row_frame = egui::Frame::new()
        .fill(SLOT_BG)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(6, 2));

    row_frame.show(ui, |ui| {
        ui.set_min_height(row_height);
        ui.horizontal(|ui| {
            // Peer name label (fixed width, truncated)
            let name = if slot.name.is_empty() {
                "???"
            } else {
                &slot.name
            };
            ui.allocate_ui(egui::vec2(name_width, row_height), |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new(name)
                            .size(11.0)
                            .color(TEXT_BRIGHT),
                    );
                });
            });

            // Sparkline fills remaining width
            let available = ui.available_size();
            let spark_size = egui::vec2(available.x, row_height);
            draw_sparkline(ui, slot, spark_size);
        });
    });
}

fn draw_sparkline(ui: &mut egui::Ui, slot: &SlotSnapshot, size: egui::Vec2) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    // Background
    painter.rect_filled(rect, 2.0, egui::Color32::from_rgb(0x10, 0x0d, 0x1a));

    let n = SPARKLINE_LEN;
    let w = rect.width();
    let h = rect.height();

    if w < 2.0 || h < 2.0 {
        return;
    }

    let x_step = w / (n as f32 - 1.0);
    let baseline_y = rect.max.y - 1.0;
    let top_y = rect.min.y + 1.0;
    let usable_h = baseline_y - top_y;

    // Draw interval markers first (behind sparkline)
    for i in 0..n {
        let ring_idx = (slot.write_pos + i) % n;
        if slot.interval_markers[ring_idx] {
            let x = rect.min.x + i as f32 * x_step;
            painter.line_segment(
                [egui::pos2(x, top_y), egui::pos2(x, baseline_y)],
                egui::Stroke::new(1.0, INTERVAL_MARKER_COLOR),
            );
        }
    }

    // Draw sparkline as filled area with colored top edge
    // Build points from oldest to newest
    let mut prev_point: Option<egui::Pos2> = None;
    for i in 0..n {
        let ring_idx = (slot.write_pos + i) % n;
        let rms = slot.rms_history[ring_idx];
        let x = rect.min.x + i as f32 * x_step;
        // Scale: sqrt for more visible low-level signals
        let normalized = (rms * 4.0).clamp(0.0, 1.0).sqrt();
        let y = baseline_y - normalized * usable_h;
        let point = egui::pos2(x, y);

        // Filled quad from previous point to current (trapezoid to baseline)
        if let Some(prev) = prev_point {
            let color = sparkline_color(rms);
            // Semi-transparent fill
            let fill_color = egui::Color32::from_rgba_premultiplied(
                color.r() / 3,
                color.g() / 3,
                color.b() / 3,
                80,
            );

            // Fill trapezoid as two triangles
            let bl = egui::pos2(prev.x, baseline_y);
            let br = egui::pos2(x, baseline_y);
            painter.add(egui::Shape::convex_polygon(
                vec![prev, point, br, bl],
                fill_color,
                egui::Stroke::NONE,
            ));

            // Top edge line
            painter.line_segment([prev, point], egui::Stroke::new(1.5, color));
        }

        prev_point = Some(point);
    }

    // Peak glow dot at the newest position
    if let Some(last) = prev_point {
        let peak_color = sparkline_color(slot.peak);
        painter.circle_filled(last, 2.5, peak_color);
        // Subtle glow
        let glow = egui::Color32::from_rgba_premultiplied(
            peak_color.r(),
            peak_color.g(),
            peak_color.b(),
            40,
        );
        painter.circle_filled(last, 5.0, glow);
    }
}

fn draw_footer(ui: &mut egui::Ui) {
    ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
        ui.add_space(4.0);
        ui.hyperlink_to(
            egui::RichText::new("github.com/MostDistant/WAIL")
                .size(9.0)
                .color(TEXT_DIM),
            "https://github.com/MostDistant/WAIL",
        );
        ui.label(
            egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                .size(9.0)
                .color(TEXT_DIM),
        );
    });
}
