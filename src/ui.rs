//! Desktop UI (eframe/egui): the main window with the live readings, tables
//! and graphs, plus the settings dialog (current limits, alert sounds) and
//! the built-in sound file picker.
//!
//! [`App`] never polls hardware itself: it renders from the shared
//! [`WebData`] (updated by the poller thread) and sends [`Cmd`]s to the
//! poller for power-limit and interval changes.

use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::alerts::{Settings, MAX_LIMIT_A, MIN_LIMIT_A};
use crate::hardware::{Cmd, DEFAULT_POLL_MS};
use crate::web::{WebData, WebServer, WebState, DEFAULT_WEB_PORT};

pub(crate) const HORIZONS_MIN: [u32; 6] = [1, 5, 10, 15, 30, 60];

fn current_color(current: f64, s: &Settings) -> egui::Color32 {
    if current < s.yellow_limit_a as f64 {
        egui::Color32::from_rgb(80, 220, 80)
    } else if current < s.red_limit_a as f64 {
        egui::Color32::from_rgb(240, 210, 40)
    } else {
        egui::Color32::from_rgb(240, 60, 60)
    }
}

pub(crate) struct App {
    cmd_tx: mpsc::Sender<Cmd>,
    pending_limit: Option<u32>,
    /// Sample interval in ms (sent to the poller when the slider changes).
    poll_ms: u64,
    /// Latest sample, histories and NVML ranges, shared with the web server.
    data: Arc<Mutex<WebData>>,
    /// Index into HORIZONS_MIN for the visible time window.
    window_idx: u8,
    /// Toggle for the per-pin graphs (history keeps accumulating either way).
    show_graphs: bool,
    /// Web interface port (text box; applied when the checkbox is toggled).
    web_port: String,
    /// Web server state; `Stopped` by default.
    web: WebState,
    /// One-shot warning (e.g. port in use), shown in a small dialog.
    web_error: Option<String>,
    /// The applied limits and sounds.
    settings: Settings,
    /// While set, the settings panel is open and editing this draft copy
    /// of the settings (Accept commits it, Cancel discards it).
    draft_settings: Option<Settings>,
    /// While set, the sound file picker is open.
    file_dialog: Option<FileDialog>,
}

impl App {
    pub(crate) fn new(cmd_tx: mpsc::Sender<Cmd>, data: Arc<Mutex<WebData>>) -> App {
        App {
            cmd_tx,
            pending_limit: None,
            poll_ms: DEFAULT_POLL_MS,
            data,
            window_idx: 0,
            show_graphs: true,
            web_port: DEFAULT_WEB_PORT.to_string(),
            web: WebState::Stopped,
            web_error: None,
            settings: Settings::default(),
            draft_settings: None,
            file_dialog: None,
        }
    }
}

/// Which sound path the file picker is choosing.
#[derive(Clone, Copy)]
enum SoundField {
    Yellow,
    Red,
}

/// A minimal built-in sound file picker: browses directories and lists the
/// `.wav` files to choose from, keeping the app free of dialog dependencies.
struct FileDialog {
    field: SoundField,
    dir: PathBuf,
    /// Shown when the directory could not be read.
    error: Option<String>,
}

impl FileDialog {
    /// Open at the directory holding the current selection (or the current
    /// directory when the stored path does not exist).
    fn new(field: SoundField, current: &Path) -> Self {
        let parent = current.parent().unwrap_or(Path::new(".")).to_path_buf();
        let dir = parent.canonicalize().unwrap_or(parent);
        FileDialog {
            field,
            dir,
            error: None,
        }
    }
}

/// Path display plus browse button for one of the sound settings. Clicking
/// the button records the request; the picker opens next to the panel.
fn sound_file_row(
    ui: &mut egui::Ui,
    path: &mut PathBuf,
    field: SoundField,
    open_browser: &mut Option<(SoundField, PathBuf)>,
) {
    ui.horizontal(|ui| {
        ui.label("Sound file:");
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        if ui
            .button(label)
            .on_hover_text(path.display().to_string())
            .clicked()
        {
            *open_browser = Some((field, path.clone()));
        }
    });
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Sample ingestion happens on the poller thread (WebData::record);
        // here we only mirror the UI settings and render from the shared
        // data, which keeps updating even while this window is minimized.
        let mut data = self.data.lock().unwrap();
        data.poll_ms = self.poll_ms;
        data.show_graphs = self.show_graphs;
        if data.settings != self.settings {
            // Mirrored so the poller plays alert sounds and the web page
            // draws the same thresholds the GUI shows.
            data.settings = self.settings.clone();
        }
        let now = data.start.elapsed().as_secs_f64();

        let window_secs = HORIZONS_MIN[self.window_idx as usize] as f64 * 60.0;

        // Top bar with the settings button (gear), top right.
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new("\u{2699}").size(18.0))
                                .frame(false),
                        )
                        .on_hover_text("Settings")
                        .clicked()
                        && self.draft_settings.is_none()
                    {
                        self.draft_settings = Some(self.settings.clone());
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("ASUS 5090 ROG Astral");
            ui.separator();

            // Total power: prefer NVML (same source as nvidia-smi),
            // fall back to the sum of pin V*A
            let power_text = match data.sample.power_w {
                Some(w) => format!("Total power: {:.1} W", w),
                None => {
                    let est: f64 = data.sample.pins.iter().map(|p| p.voltage * p.current).sum();
                    format!("Total power: {:.1} W (estimated)", est)
                }
            };
            ui.label(egui::RichText::new(power_text).strong().size(20.0));

            // GPU temperature and VRAM (NVML)
            let mut gpu_texts: Vec<String> = Vec::new();
            if let Some(t) = data.sample.temp_c {
                gpu_texts.push(format!("GPU temp: {:.0} °C", t));
            }
            if let Some(used) = data.sample.mem_used_gb {
                let text = match data.sample.mem_total_gb {
                    Some(total) => format!("VRAM: {:.1} / {:.0} GB", used, total),
                    None => format!("VRAM: {:.1} GB", used),
                };
                gpu_texts.push(text);
            }
            if !gpu_texts.is_empty() {
                ui.horizontal(|ui| {
                    for (i, text) in gpu_texts.into_iter().enumerate() {
                        if i > 0 {
                            ui.add_space(16.0);
                        }
                        ui.label(egui::RichText::new(text).strong().size(16.0));
                    }
                });
            }

            // Power management limit (NVML)
            if let (Some(min), Some(max), Some(current)) = (
                data.sample.limit_min_w,
                data.sample.limit_max_w,
                data.sample.limit_w,
            ) {
                if self.pending_limit.is_none() {
                    self.pending_limit = Some(current);
                }
                let mut pending = self.pending_limit.unwrap_or(current);
                ui.horizontal(|ui| {
                    ui.label("Power limit:");
                    let changed = ui
                        .add(
                            egui::DragValue::new(&mut pending)
                                .range(min..=max)
                                .suffix(" W"),
                        )
                        .changed();
                    if changed {
                        self.pending_limit = Some(pending);
                    }
                    if pending != current && ui.button("Apply").clicked() {
                        let _ = self.cmd_tx.send(Cmd::SetLimit(pending));
                    }
                });
            }

            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Sample interval:");
                let mut ms = self.poll_ms;
                let changed = ui
                    .add(
                        egui::DragValue::new(&mut ms)
                            .range(100..=10_000)
                            .suffix(" ms"),
                    )
                    .changed();
                if changed {
                    self.poll_ms = ms;
                    let _ = self.cmd_tx.send(Cmd::Interval(ms));
                }
            });

            ui.separator();

            // Web interface: the text box holds the port, the checkbox
            // starts/stops the HTTP server that mirrors this UI for other
            // devices. Disabled by default; a busy port is rejected with a
            // warning instead of starting the service.
            ui.horizontal(|ui| {
                ui.label("Web server:");
                let mut enabled = matches!(&self.web, WebState::Running(_));
                let mut port = self.web_port.clone();
                if ui
                    .add(egui::TextEdit::singleline(&mut port).desired_width(70.0))
                    .changed()
                {
                    // Digits only; the value is applied when the checkbox is toggled.
                    self.web_port = port.chars().filter(|c| c.is_ascii_digit()).collect();
                }
                if ui.checkbox(&mut enabled, "Enable").changed() {
                    if enabled {
                        match self.web_port.trim().parse::<u16>() {
                            Ok(port) if port != 0 => match WebServer::start(port, self.data.clone())
                            {
                                Ok(server) => self.web = WebState::Running(server),
                                Err(e) => {
                                    enabled = false;
                                    self.web_error = Some(format!(
                                        "Port {port} is not available: {e}\n\nThe web interface was not started.",
                                    ));
                                }
                            },
                            _ => {
                                enabled = false;
                                self.web_error = Some(
                                    "Port must be a number between 1 and 65535.".to_owned(),
                                );
                            }
                        }
                    } else {
                        self.web = WebState::Stopped;
                    }
                }
                if enabled {
                    if let WebState::Running(server) = &self.web {
                        ui.label(
                            egui::RichText::new(format!("active on port {}", server.port))
                                .weak(),
                        );
                    }
                }
            });

            ui.checkbox(&mut self.show_graphs, "Show graphs");
            if self.show_graphs {
                // Time window for the graphs (discrete steps)
                ui.horizontal(|ui| {
                    ui.label("Time window:");
                    let mut idx = self.window_idx as f64;
                    ui.add(egui::Slider::new(&mut idx, 0.0..=5.0).step_by(1.0));
                    self.window_idx = idx as u8;
                });
                ui.horizontal(|ui| {
                    for (i, &min) in HORIZONS_MIN.iter().enumerate() {
                        let text = egui::RichText::new(format!("{min}m"));
                        let text = if i == self.window_idx as usize {
                            text.strong().color(egui::Color32::from_rgb(120, 190, 255))
                        } else {
                            text.weak()
                        };
                        ui.label(text);
                    }
                });
            }

            ui.separator();

            // Voltage table - 2 rows x 3 columns
            ui.strong("Voltage (V)");
            ui.horizontal(|ui| {
                for i in 0..3 {
                    ui.monospace(format!(
                        "Pin {:>2}: {:>7.3}",
                        i, data.sample.pins[i].voltage
                    ));
                }
            });
            ui.horizontal(|ui| {
                for i in 3..6 {
                    ui.monospace(format!(
                        "Pin {:>2}: {:>7.3}",
                        i, data.sample.pins[i].voltage
                    ));
                }
            });

            ui.separator();

            // Current table - 2 rows x 3 columns with color coding,
            // legend to the right of the numbers
            ui.strong("Current (A)");
            for row in 0..2 {
                ui.horizontal(|ui| {
                    for i in row * 3..row * 3 + 3 {
                        let color = current_color(data.sample.pins[i].current, &self.settings);
                        ui.label(
                            egui::RichText::new(format!(
                                "Pin {:>2}: {:>7.3}",
                                i, data.sample.pins[i].current
                            ))
                            .color(color),
                        );
                    }
                    if row == 0 {
                        let y = self.settings.yellow_limit_a;
                        let r = self.settings.red_limit_a;
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(format!("● ≥ {r:.1}A"))
                                    .color(egui::Color32::from_rgb(240, 60, 60)),
                            );
                            ui.add_space(12.0);
                            ui.label(
                                egui::RichText::new(format!("● {y:.1}–{r:.1}A"))
                                    .color(egui::Color32::from_rgb(240, 210, 40)),
                            );
                            ui.add_space(12.0);
                            ui.label(
                                egui::RichText::new(format!("● < {y:.1}A"))
                                    .color(egui::Color32::from_rgb(80, 220, 80)),
                            );
                        });
                    }
                });
            }

            if self.show_graphs {
                ui.separator();

                // Per-pin current graphs (rows 0-1) plus GPU metric graphs
                // (power, temperature, VRAM) in row 2. All share the same
                // time window. 60 px is the minimum height; extra vertical
                // space is used.
                ui.strong("Current and GPU metrics over time");
                let spacing = 8.0;
                let graph_w = (ui.available_width() - 2.0 * spacing) / 3.0;
                let item_spacing_y = ui.spacing().item_spacing.y;
                let label_h = ui.fonts(|f| f.row_height(&egui::FontId::default()));
                let graph_h = ((ui.available_height() - 3.0 * label_h - 6.0 * item_spacing_y)
                    / 3.0)
                    .max(60.0);
                let graph_size = egui::vec2(graph_w.max(60.0), graph_h);
                for row in 0..3 {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = spacing;
                        for col in 0..3 {
                            if row < 2 {
                                let pin = row * 3 + col;
                                add_pin_graph(
                                    ui,
                                    pin,
                                    &data.history[pin],
                                    now,
                                    window_secs,
                                    graph_size,
                                    &self.settings,
                                );
                            } else {
                                match col {
                                    0 => add_metric_graph(
                                        ui,
                                        "Power (W)",
                                        &data.power_history,
                                        now,
                                        window_secs,
                                        graph_size,
                                        egui::Color32::from_rgb(255, 165, 0),
                                        "W",
                                        data.power_range_max,
                                    ),
                                    1 => add_metric_graph(
                                        ui,
                                        "Temp (°C)",
                                        &data.temp_history,
                                        now,
                                        window_secs,
                                        graph_size,
                                        egui::Color32::from_rgb(255, 90, 70),
                                        "°C",
                                        data.temp_range_max,
                                    ),
                                    _ => add_metric_graph(
                                        ui,
                                        "VRAM (GB)",
                                        &data.mem_history,
                                        now,
                                        window_secs,
                                        graph_size,
                                        egui::Color32::from_rgb(140, 190, 255),
                                        "GB",
                                        data.mem_range_max,
                                    ),
                                }
                            }
                        }
                    });
                }
            }
        });

        // One-shot warning (e.g. the port is already in use) as a small
        // dialog; OK clears it.
        if let Some(msg) = self.web_error.clone() {
            egui::Window::new("Web interface")
                .resizable(false)
                .collapsible(false)
                .default_pos([300.0, 350.0])
                .show(ctx, |ui| {
                    ui.label(msg.as_str());
                    ui.add_space(8.0);
                    if ui.button("OK").clicked() {
                        self.web_error = None;
                    }
                });
        }

        // Settings panel: edits a draft copy of the settings. Accept
        // commits it and closes the panel, Cancel (or the window's X)
        // discards it. Accept is only enabled while the draft actually
        // differs from the applied settings.
        let mut accept = false;
        let mut cancel = false;
        let mut open_browser: Option<(SoundField, PathBuf)> = None;
        if let Some(draft) = &mut self.draft_settings {
            let mut open = true;
            egui::Window::new("Settings")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_pos([150.0, 90.0])
                .show(ctx, |ui| {
                    ui.set_min_width(400.0);

                    ui.strong("Current limits");
                    let mut yellow = draft.yellow_limit_a;
                    ui.add(
                        egui::Slider::new(&mut yellow, MIN_LIMIT_A..=draft.red_limit_a)
                            .step_by(0.1)
                            .suffix(" A")
                            .text("Yellow starts"),
                    );
                    draft.yellow_limit_a = (yellow * 10.0).round() / 10.0;
                    let mut red = draft.red_limit_a;
                    ui.add(
                        egui::Slider::new(&mut red, draft.yellow_limit_a..=MAX_LIMIT_A)
                            .step_by(0.1)
                            .suffix(" A")
                            .text("Red starts"),
                    );
                    draft.red_limit_a = (red * 10.0).round() / 10.0;

                    ui.add_space(6.0);
                    ui.separator();
                    ui.strong("Alert sounds");
                    ui.checkbox(
                        &mut draft.yellow_sound_enabled,
                        "Play sound when the yellow limit is surpassed",
                    );
                    sound_file_row(
                        ui,
                        &mut draft.yellow_sound_path,
                        SoundField::Yellow,
                        &mut open_browser,
                    );
                    ui.checkbox(
                        &mut draft.red_sound_enabled,
                        "Play sound when the red limit is surpassed",
                    );
                    sound_file_row(
                        ui,
                        &mut draft.red_sound_path,
                        SoundField::Red,
                        &mut open_browser,
                    );

                    ui.add_space(6.0);
                    ui.separator();
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Right-to-left: added first = rightmost, so Accept
                        // ends up to the left of Cancel.
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                        accept = ui
                            .add_enabled(
                                *draft != self.settings,
                                egui::Button::new(egui::RichText::new("Accept").strong()),
                            )
                            .clicked();
                    });
                });
            if !open {
                cancel = true;
            }
        }
        if let Some((field, path)) = open_browser {
            self.file_dialog = Some(FileDialog::new(field, &path));
        }
        if accept {
            if let Some(draft) = self.draft_settings.take() {
                self.settings = draft;
            }
        }
        if cancel {
            // Cancel / window close: discard the draft, closing the panel.
            self.draft_settings = None;
        }
        if accept || cancel {
            self.file_dialog = None;
        }

        // Sound file picker, shown on top of the settings panel.
        let mut dialog_close = false;
        if let Some(fd) = &mut self.file_dialog {
            let mut picked: Option<PathBuf> = None;
            let mut open = true;
            let mut dirs: Vec<String> = Vec::new();
            let mut files: Vec<String> = Vec::new();
            egui::Window::new("Select sound file")
                .open(&mut open)
                .collapsible(false)
                .default_pos([260.0, 200.0])
                .default_width(360.0)
                .show(ctx, |ui| {
                    ui.monospace(fd.dir.display().to_string());
                    if let Some(err) = &fd.error {
                        ui.colored_label(egui::Color32::from_rgb(240, 120, 120), err);
                    }
                    if ui.button("Up one level").clicked() {
                        if let Some(parent) = fd.dir.parent() {
                            fd.dir = parent.to_path_buf();
                        }
                    }
                    match std::fs::read_dir(&fd.dir) {
                        Ok(entries) => {
                            fd.error = None;
                            for entry in entries.flatten() {
                                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                                let name = entry.file_name().to_string_lossy().into_owned();
                                if is_dir {
                                    // Skip dot-directories; they only add noise.
                                    if !name.starts_with('.') {
                                        dirs.push(name);
                                    }
                                } else if name.to_lowercase().ends_with(".wav") {
                                    files.push(name);
                                }
                            }
                        }
                        Err(e) => {
                            fd.error = Some(format!("Cannot read {}: {e}", fd.dir.display()));
                        }
                    }
                    dirs.sort();
                    files.sort();
                    if fd.error.is_none() && dirs.is_empty() && files.is_empty() {
                        ui.weak("No folders or .wav files here.");
                    }
                    egui::ScrollArea::vertical()
                        .max_height(260.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for d in &dirs {
                                if ui.selectable_label(false, format!("{d}/")).clicked() {
                                    fd.dir = fd.dir.join(d);
                                }
                            }
                            for f in &files {
                                if ui.selectable_label(false, f).clicked() {
                                    picked = Some(fd.dir.join(f));
                                }
                            }
                        });
                    ui.separator();
                    if ui.button("Cancel").clicked() {
                        dialog_close = true;
                    }
                });
            if let Some(path) = picked {
                if let Some(draft) = &mut self.draft_settings {
                    match fd.field {
                        SoundField::Yellow => draft.yellow_sound_path = path,
                        SoundField::Red => draft.red_sound_path = path,
                    }
                }
                dialog_close = true;
            }
            if !open {
                dialog_close = true;
            }
        }
        if dialog_close {
            self.file_dialog = None;
        }

        // Redraw about as fast as the data arrives when idle.
        // Input events (slider drags, checkbox, window moves) still trigger
        // immediate repaints, so the UI stays fully responsive.
        ctx.request_repaint_after(Duration::from_millis(self.poll_ms));
    }
}

/// Draw one small graph of a pin's current over the visible time window.
/// The line is colored per segment by the current thresholds, and the
/// yellow/red limits are drawn as horizontal reference lines.
/// Hover readout shared by all graphs: while the pointer is inside the
/// graph rect, draw a faint vertical crosshair, a marker on the data line,
/// and a label with the value of the nearest sample.
fn draw_hover_readout(
    ui: &egui::Ui,
    painter: &egui::Painter,
    hovered: bool,
    rect: egui::Rect,
    hist: &[(f64, f64)],
    t0: f64,
    window_secs: f64,
    x_of: impl Fn(f64) -> f64,
    y_of: impl Fn(f64) -> f64,
    dot_color: impl Fn(f64) -> egui::Color32,
    unit: &str,
) {
    if !hovered || hist.is_empty() {
        return;
    }
    let Some(pos) = ui.ctx().pointer_hover_pos() else {
        return;
    };
    if !rect.contains(pos) {
        return;
    }

    // Time under the pointer, then the nearest sample by time.
    let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
    let t = t0 + frac as f64 * window_secs;
    let k = hist.partition_point(|(ts, _)| *ts < t);
    let idx = match k {
        0 => 0,
        k if k >= hist.len() => hist.len() - 1,
        _ => {
            if t - hist[k - 1].0 <= hist[k].0 - t {
                k - 1
            } else {
                k
            }
        }
    };
    let (ts, v) = hist[idx];
    let x = x_of(ts) as f32;
    let y = y_of(v) as f32;

    let dim = ui.visuals().weak_text_color();
    painter.vline(
        x,
        rect.top()..=rect.bottom(),
        egui::Stroke::new(1.0, dim.gamma_multiply(0.5)),
    );
    painter.circle(
        egui::Pos2::new(x, y),
        3.0,
        dot_color(v),
        egui::Stroke::new(1.0, ui.visuals().faint_bg_color),
    );
    // Value label next to the crosshair, flipping sides near the right edge.
    let galley = painter.layout_no_wrap(
        format!("{v:.2} {unit}"),
        egui::FontId::monospace(11.0),
        ui.visuals().strong_text_color(),
    );
    let mut label_x = x + 6.0;
    if label_x + galley.size().x > rect.right() - 2.0 {
        label_x = (x - galley.size().x - 6.0).max(rect.left() + 1.0);
    }
    // Backing so the label stays readable over the lines.
    painter.rect(
        egui::Rect::from_min_size(
            egui::Pos2::new(label_x - 1.0, rect.top() + 1.0),
            galley.size() + egui::vec2(2.0, 2.0),
        ),
        0.0,
        ui.visuals().faint_bg_color,
        egui::Stroke::NONE,
        egui::StrokeKind::Middle,
    );
    painter.galley(
        egui::Pos2::new(label_x, rect.top() + 2.0),
        galley,
        ui.visuals().strong_text_color(),
    );
}

fn add_pin_graph(
    ui: &mut egui::Ui,
    pin: usize,
    hist: &[(f64, f64)],
    now: f64,
    window_secs: f64,
    size: egui::Vec2,
    settings: &Settings,
) {
    ui.vertical(|ui| {
        ui.label(egui::RichText::new(format!("Pin {pin}")).strong());
        let (resp, painter) = ui.allocate_painter(size, egui::Sense::hover());
        let rect = resp.rect;

        // Background and frame
        painter.rect(
            rect,
            0.0,
            ui.visuals().faint_bg_color,
            ui.visuals().widgets.noninteractive.bg_stroke,
            egui::StrokeKind::Middle,
        );

        let t0 = now - window_secs;
        let k = hist.partition_point(|(t, _)| *t < t0);
        let vis = &hist[k..];

        // Y range: always keep both limit lines visible, grow for spikes
        let base_max = settings.red_limit_a as f64 * 1.1;
        let mut y_max: f64 = base_max;
        for (_, v) in vis {
            y_max = y_max.max(*v * 1.05);
        }
        let y_of = |v: f64| rect.top() as f64 + (1.0 - v / y_max) * rect.height() as f64;
        let x_of = |t: f64| rect.left() as f64 + (t - t0) / window_secs * rect.width() as f64;

        // Limit reference lines with numeric labels. Muted and dashed so
        // they recede behind the current line instead of competing with it.
        let small = egui::FontId::monospace(9.0);
        let dim = ui.visuals().weak_text_color();
        for (limit, color) in [
            (
                settings.yellow_limit_a as f64,
                egui::Color32::from_rgb(165, 150, 75),
            ),
            (
                settings.red_limit_a as f64,
                egui::Color32::from_rgb(175, 85, 85),
            ),
        ] {
            let y = y_of(limit) as f32;
            let mut x = rect.left();
            while x < rect.right() {
                let x_end = (x + 4.0).min(rect.right());
                painter.line_segment(
                    [egui::Pos2::new(x, y), egui::Pos2::new(x_end, y)],
                    egui::Stroke::new(1.0, color),
                );
                x = x_end + 3.0;
            }
            painter.text(
                egui::Pos2::new(rect.right() - 18.0, (y - 10.0).max(rect.top() + 1.0)),
                egui::Align2::LEFT_TOP,
                format!("{limit:.1}"),
                small.clone(),
                dim,
            );
        }

        // Top scale label: only when the range has grown beyond the base
        // limit scale; the limit lines otherwise define the scale
        if y_max > base_max {
            painter.text(
                egui::Pos2::new(rect.left() + 2.0, rect.top() + 1.0),
                egui::Align2::LEFT_TOP,
                format!("{y_max:.0}"),
                small.clone(),
                dim,
            );
        }
        painter.text(
            egui::Pos2::new(rect.left() + 2.0, rect.bottom() - 10.0),
            egui::Align2::LEFT_TOP,
            "0",
            small,
            dim,
        );

        // Current line: downsample to ~2 points/px, colored per segment.
        // The final step is shortened so the line always ends on the newest sample.
        let n = vis.len();
        if n >= 2 {
            let max_pts = (size.x as usize).saturating_mul(2).max(2);
            let stride = n.div_ceil(max_pts).clamp(1, n - 1);
            let seg = |i: usize, j: usize| {
                painter.line_segment(
                    [
                        egui::Pos2::new(x_of(vis[i].0) as f32, y_of(vis[i].1) as f32),
                        egui::Pos2::new(x_of(vis[j].0) as f32, y_of(vis[j].1) as f32),
                    ],
                    egui::Stroke::new(1.0, current_color(vis[j].1, settings)),
                );
            };
            let mut i = 0;
            while i + stride < n {
                let j = i + stride;
                seg(i, j);
                i = j;
            }
            if i < n - 1 {
                seg(i, n - 1);
            }
        }

        // Hover readout, drawn last so it sits above the lines.
        draw_hover_readout(
            ui,
            &painter,
            resp.hovered(),
            rect,
            vis,
            t0,
            window_secs,
            &x_of,
            &y_of,
            |v| current_color(v, settings),
            "A",
        );
    });
}

/// Draw one small line graph of a GPU metric (power, temperature, VRAM) over
/// the visible time window. The y scale is permanent: fixed at 0..`y_max`
/// (the range NVML reports, e.g. max power limit or total VRAM), so the
/// scale never jumps as the data changes. When no range is known yet the
/// scale auto-fits to the data instead.
fn add_metric_graph(
    ui: &mut egui::Ui,
    title: &str,
    hist: &[(f64, f64)],
    now: f64,
    window_secs: f64,
    size: egui::Vec2,
    color: egui::Color32,
    unit: &str,
    y_max: Option<f64>,
) {
    ui.vertical(|ui| {
        ui.label(egui::RichText::new(title).strong());
        let (resp, painter) = ui.allocate_painter(size, egui::Sense::hover());
        let rect = resp.rect;

        // Background and frame
        painter.rect(
            rect,
            0.0,
            ui.visuals().faint_bg_color,
            ui.visuals().widgets.noninteractive.bg_stroke,
            egui::StrokeKind::Middle,
        );

        let t0 = now - window_secs;
        let k = hist.partition_point(|(t, _)| *t < t0);
        let vis = &hist[k..];

        // Y range: the permanent scale from NVML (e.g. max power limit, max
        // temperature, total VRAM); auto-fit to the data when unknown.
        let y_max = match y_max {
            Some(m) if m > 0.0 => m,
            _ => {
                let mut m: f64 = 1.0;
                for (_, v) in vis {
                    m = m.max(*v);
                }
                m * 1.08 // headroom above the highest value
            }
        };
        let x_of = |t: f64| rect.left() as f64 + (t - t0) / window_secs * rect.width() as f64;
        // Clamp so spikes beyond the fixed scale stay inside the graph.
        let y_of =
            |v: f64| rect.top() as f64 + (1.0 - v.clamp(0.0, y_max) / y_max) * rect.height() as f64;

        let small = egui::FontId::monospace(9.0);
        let dim = ui.visuals().weak_text_color();

        // Top and bottom scale labels
        painter.text(
            egui::Pos2::new(rect.left() + 2.0, rect.top() + 1.0),
            egui::Align2::LEFT_TOP,
            format!("{y_max:.0}"),
            small.clone(),
            dim,
        );
        painter.text(
            egui::Pos2::new(rect.left() + 2.0, rect.bottom() - 10.0),
            egui::Align2::LEFT_TOP,
            "0",
            small,
            dim,
        );

        // Metric line, downsampled to ~2 points/px. The final step is
        // shortened so the line always ends on the newest sample.
        let n = vis.len();
        if n >= 2 {
            let max_pts = (size.x as usize).saturating_mul(2).max(2);
            let stride = n.div_ceil(max_pts).clamp(1, n - 1);
            let seg = |i: usize, j: usize| {
                painter.line_segment(
                    [
                        egui::Pos2::new(x_of(vis[i].0) as f32, y_of(vis[i].1) as f32),
                        egui::Pos2::new(x_of(vis[j].0) as f32, y_of(vis[j].1) as f32),
                    ],
                    egui::Stroke::new(1.5, color),
                );
            };
            let mut i = 0;
            while i + stride < n {
                let j = i + stride;
                seg(i, j);
                i = j;
            }
            if i < n - 1 {
                seg(i, n - 1);
            }
        }

        // Hover readout, drawn last so it sits above the lines.
        draw_hover_readout(
            ui,
            &painter,
            resp.hovered(),
            rect,
            vis,
            t0,
            window_secs,
            &x_of,
            &y_of,
            |_v| color,
            unit,
        );
    });
}
