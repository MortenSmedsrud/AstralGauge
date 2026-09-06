//! Desktop UI (eframe/egui): the main window with the live readings, tables
//! and graphs, plus the settings dialog (current limits, alert sounds,
//! sample interval, web server) and the built-in sound file picker.
//!
//! [`App`] never polls hardware itself: it renders from the shared
//! [`WebData`] (updated by the poller thread) and sends [`Cmd`]s to the
//! poller for power-limit and interval changes.

use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::alerts::{AutoMinPower, EmbeddedSound, Settings, Sound, MAX_LIMIT_A, MIN_LIMIT_A};
use crate::hardware::{AuthState, Cmd, DEFAULT_POLL_MS};
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
    /// Last NVML-reported power limit. A change here that the UI did not
    /// produce (automatic minimum power, a helper-set Apply landing, another
    /// tool like nvidia-smi) re-syncs [`App::pending_limit`] so the power
    /// limit control never displays a stale value.
    shown_limit: Option<u32>,
    /// Latest sample, histories and NVML ranges, shared with the web server.
    data: Arc<Mutex<WebData>>,
    /// Index into HORIZONS_MIN for the visible time window.
    window_idx: u8,
    /// Toggle for the per-pin graphs (history keeps accumulating either way).
    show_graphs: bool,
    /// The applied settings (limits, sounds, interval, web server).
    config: Config,
    /// Web server state; `Stopped` by default. Runtime state, deliberately
    /// kept out of [`Config`] (which only holds the applied intent).
    web: WebState,
    /// One-shot warning (port in use, denied power authentication, ...),
    /// shown in a small dialog.
    warning: Option<String>,
    /// Set between accepting an enabled automatic power limit and the power
    /// helper's authentication result: while set, a failed authentication
    /// reverts the option, so it can never stay on without the privileges it
    /// needs (the prompt must be answered when enabling, not at the first
    /// limit excursion).
    auto_power_pending_auth: bool,
    /// While set, the settings panel is open and editing this draft copy
    /// of [`Config`] (Accept commits it, Cancel discards it).
    draft: Option<Config>,
    /// While set, the sound file picker is open.
    file_dialog: Option<FileDialog>,
}

/// Everything the settings panel edits, in one struct. [`App`] keeps the
/// applied copy while the panel edits a draft of it: Accept commits the
/// draft, Cancel discards it. New app-wide settings belong here — add the
/// field, a control in the settings panel, and (when the poller or the web
/// page needs it) mirror it to [`WebData`] at the top of `update`.
#[derive(Clone, PartialEq)]
struct Config {
    /// Per-pin current limits and alert sounds.
    alerts: Settings,
    /// Poller sample interval in ms (sent to the poller when it changes).
    poll_ms: u64,
    /// Whether the web server should run. The running handle itself lives
    /// in [`App::web`]; this is the applied intent, kept in sync with what
    /// actually runs.
    web_enabled: bool,
    /// Web server port.
    web_port: String,
}

impl App {
    pub(crate) fn new(cmd_tx: mpsc::Sender<Cmd>, data: Arc<Mutex<WebData>>) -> App {
        App {
            cmd_tx,
            pending_limit: None,
            shown_limit: None,
            data,
            window_idx: 0,
            show_graphs: true,
            config: Config {
                alerts: Settings::default(),
                poll_ms: DEFAULT_POLL_MS,
                web_enabled: false,
                web_port: DEFAULT_WEB_PORT.to_string(),
            },
            web: WebState::Stopped,
            warning: None,
            auto_power_pending_auth: false,
            draft: None,
            file_dialog: None,
        }
    }
}

/// Which sound the file picker is choosing.
#[derive(Clone, Copy)]
enum SoundField {
    Yellow,
    Red,
}

impl SoundField {
    /// The sound embedded in the executable for this field: the default and
    /// what "Reset" restores.
    fn default_sound(self) -> Sound {
        match self {
            SoundField::Yellow => Sound::Embedded(EmbeddedSound::Attention),
            SoundField::Red => Sound::Embedded(EmbeddedSound::Critical),
        }
    }
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
    /// Open at the directory holding the currently selected file, or — while
    /// a sound embedded in the executable is selected — at the user's home
    /// directory (falling back to the current one).
    fn new(field: SoundField, current: Option<&Path>) -> Self {
        let start = current
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = start.canonicalize().unwrap_or(start);
        FileDialog {
            field,
            dir,
            error: None,
        }
    }
}

/// Sound display plus browse button for one of the sound settings. Clicking
/// the button records the request; the picker opens next to the panel. A
/// user-selected file can be reset back to the embedded sound.
fn sound_file_row(
    ui: &mut egui::Ui,
    sound: &mut Sound,
    field: SoundField,
    open_browser: &mut Option<(SoundField, Option<PathBuf>)>,
) {
    ui.horizontal(|ui| {
        ui.label("Sound:");
        match sound {
            Sound::Embedded(embedded) => {
                if ui
                    .button(format!("Built-in ({})", embedded.display_name()))
                    .on_hover_text(
                        "Sound embedded in the executable.\
Click to choose your own .wav file instead.",
                    )
                    .clicked()
                {
                    *open_browser = Some((field, None));
                }
            }
            Sound::File(path) => {
                let label = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                if ui
                    .button(label)
                    .on_hover_text(path.display().to_string())
                    .clicked()
                {
                    *open_browser = Some((field, Some(path.clone())));
                }
                if ui
                    .button("Reset")
                    .on_hover_text("Use the built-in sound again")
                    .clicked()
                {
                    *sound = field.default_sound();
                }
            }
        }
    });
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Sample ingestion happens on the poller thread (WebData::record);
        // here we only mirror the UI settings and render from the shared
        // data, which keeps updating even while this window is minimized.
        let mut data = self.data.lock().unwrap();
        // Automatic power limit waiting for its authentication prompt, ahead
        // of the settings mirror below: the option is only good to have while
        // the privileged helper is up, so a denied authentication turns it
        // back off before it is ever mirrored to the poller.
        if self.auto_power_pending_auth {
            match data.power_auth {
                AuthState::Active => self.auto_power_pending_auth = false,
                AuthState::Failed => {
                    self.auto_power_pending_auth = false;
                    if self.config.alerts.auto_min_power != AutoMinPower::Off {
                        self.config.alerts.auto_min_power = AutoMinPower::Off;
                        self.warning = Some(
                            "Authentication was not granted, so the automatic power limit was turned back off.\n\nEnable it again and accept the authentication prompt to use this feature."
                                .to_owned(),
                        );
                    }
                }
                // Prompt still open (or helper just spawned): keep waiting.
                AuthState::Idle => {}
            }
        }
        data.poll_ms = self.config.poll_ms;
        data.show_graphs = self.show_graphs;
        if data.settings != self.config.alerts {
            // Mirrored so the poller plays alert sounds and the web page
            // draws the same thresholds the GUI shows.
            data.settings = self.config.alerts.clone();
        }
        let now = data.start.elapsed().as_secs_f64();

        let window_secs = HORIZONS_MIN[self.window_idx as usize] as f64 * 60.0;

        egui::CentralPanel::default().show(ctx, |ui| {
            // GPU name on the left, settings button (gear) on the right.
            ui.horizontal(|ui| {
                ui.heading("ASUS 5090 ROG Astral");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new("\u{2699}").size(18.0))
                                .frame(false),
                        )
                        .on_hover_text("Settings")
                        .clicked()
                        && self.draft.is_none()
                    {
                        self.draft = Some(self.config.clone());
                    }
                });
            });
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
                // Follow limit changes made outside this control; keep an
                // unapplied drag edit as long as the real limit is unchanged.
                if self.shown_limit != Some(current) || self.pending_limit.is_none() {
                    self.pending_limit = Some(current);
                }
                self.shown_limit = Some(current);
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
            // Privileged power helper status (only while it has something to
            // report: activation, or why a limit change failed).
            if let Some(note) = data.power_note.clone() {
                ui.label(egui::RichText::new(note).weak().size(12.0));
            }

            ui.separator();

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
                        let color = current_color(data.sample.pins[i].current, &self.config.alerts);
                        ui.label(
                            egui::RichText::new(format!(
                                "Pin {:>2}: {:>7.3}",
                                i, data.sample.pins[i].current
                            ))
                            .color(color),
                        );
                    }
                    if row == 0 {
                        let y = self.config.alerts.yellow_limit_a;
                        let r = self.config.alerts.red_limit_a;
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
                                    &self.config.alerts,
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

        // One-shot warning (port in use, denied power authentication, ...) as
        // a small dialog; OK clears it.
        if let Some(msg) = self.warning.clone() {
            egui::Window::new("Warning")
                .resizable(false)
                .collapsible(false)
                .default_pos([300.0, 350.0])
                .show(ctx, |ui| {
                    ui.label(msg.as_str());
                    ui.add_space(8.0);
                    if ui.button("OK").clicked() {
                        self.warning = None;
                    }
                });
        }

        // Settings panel: edits a draft copy of the Config (limits, sounds,
        // sample interval, web server). Accept commits it and closes the
        // panel, Cancel (or the window's X) discards it. Accept is only
        // enabled while the draft actually differs from the applied config.
        let mut accept = false;
        let mut cancel = false;
        let mut open_browser: Option<(SoundField, Option<PathBuf>)> = None;
        let current = self.config.clone();
        if let Some(draft) = &mut self.draft {
            let mut open = true;
            egui::Window::new("Settings")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_pos([150.0, 90.0])
                .show(ctx, |ui| {
                    ui.set_min_width(400.0);

                    ui.strong("Current limits");
                    let mut yellow = draft.alerts.yellow_limit_a;
                    ui.add(
                        egui::Slider::new(&mut yellow, MIN_LIMIT_A..=draft.alerts.red_limit_a)
                            .step_by(0.1)
                            .suffix(" A")
                            .text("Yellow starts"),
                    );
                    draft.alerts.yellow_limit_a = (yellow * 10.0).round() / 10.0;
                    let mut red = draft.alerts.red_limit_a;
                    ui.add(
                        egui::Slider::new(&mut red, draft.alerts.yellow_limit_a..=MAX_LIMIT_A)
                            .step_by(0.1)
                            .suffix(" A")
                            .text("Red starts"),
                    );
                    draft.alerts.red_limit_a = (red * 10.0).round() / 10.0;

                    ui.add_space(6.0);
                    ui.separator();
                    ui.strong("Alert sounds");
                    ui.checkbox(
                        &mut draft.alerts.yellow_sound_enabled,
                        "Play sound when the yellow limit is surpassed",
                    );
                    sound_file_row(
                        ui,
                        &mut draft.alerts.yellow_sound,
                        SoundField::Yellow,
                        &mut open_browser,
                    );
                    ui.checkbox(
                        &mut draft.alerts.red_sound_enabled,
                        "Play sound when the red limit is surpassed",
                    );
                    sound_file_row(
                        ui,
                        &mut draft.alerts.red_sound,
                        SoundField::Red,
                        &mut open_browser,
                    );

                    ui.add_space(6.0);
                    ui.separator();
                    // Lower the GPU power limit to its minimum when a pin
                    // reaches the selected limit. Applying it needs root, so
                    // the first change asks polkit (pkexec) for a password.
                    ui.strong("Automatic power limit");
                    let auto_labels = ["Never", "On yellow limit", "On red limit"];
                    let mut auto_idx = draft.alerts.auto_min_power.index();
                    ui.horizontal(|ui| {
                        ui.label("Lower the power limit to its minimum:");
                        egui::ComboBox::from_label("")
                            .selected_text(auto_labels[auto_idx])
                            .show_ui(ui, |ui| {
                                for (i, label) in auto_labels.iter().enumerate() {
                                    ui.selectable_value(&mut auto_idx, i, *label);
                                }
                            });
                    });
                    draft.alerts.auto_min_power = AutoMinPower::from_index(auto_idx);
                    ui.label(
                        egui::RichText::new(
                            "When a pin reaches the selected limit, the power management limit is \
set to the card's minimum and restored once the current has been back below it for 30 \
seconds. Applying limits needs administrator rights: enabling this opens an \
authentication dialog right away, and turns the option back off if it is not \
accepted. While enabled, the built-in alert sounds play their powering-down \
variants instead.",
                        )
                        .weak()
                        .size(12.0),
                    );
                    if ui.button("Test privileged access").clicked() {
                        // Spawn the helper now, so the password prompt shows
                        // here instead of on the first limit excursion.
                        let _ = self.cmd_tx.send(Cmd::GrantPowerAccess);
                    }

                    ui.add_space(6.0);
                    ui.separator();
                    ui.strong("Sampling");
                    ui.horizontal(|ui| {
                        ui.label("Sample interval:");
                        ui.add(
                            egui::DragValue::new(&mut draft.poll_ms)
                                .range(100..=10_000)
                                .suffix(" ms"),
                        );
                    });

                    ui.add_space(6.0);
                    ui.separator();
                    // The HTTP server mirroring this UI for other devices.
                    // Disabled by default; applied on Accept, where a busy or
                    // invalid port is rejected with a warning (a server that
                    // is already running keeps its port).
                    ui.strong("Web server");
                    ui.horizontal(|ui| {
                        ui.label("Port:");
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut draft.web_port).desired_width(70.0),
                            )
                            .changed()
                        {
                            // Digits only; applied on Accept.
                            draft.web_port = draft
                                .web_port
                                .chars()
                                .filter(|c| c.is_ascii_digit())
                                .collect();
                        }
                        ui.checkbox(&mut draft.web_enabled, "Enable");
                        if draft.web_enabled {
                            if let WebState::Running(server) = &self.web {
                                ui.label(
                                    egui::RichText::new(format!("active on port {}", server.port))
                                        .weak(),
                                );
                            }
                        }
                    });

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
                                *draft != current,
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
            self.file_dialog = Some(FileDialog::new(field, path.as_deref()));
        }
        if accept {
            if let Some(mut draft) = self.draft.take() {
                if draft.poll_ms != self.config.poll_ms {
                    let _ = self.cmd_tx.send(Cmd::Interval(draft.poll_ms));
                }
                // Reconcile the web server with the accepted draft: start,
                // stop, or restart it on a new port. A failed start never
                // tears down a server that is already running.
                let running_port = match &self.web {
                    WebState::Running(server) => Some(server.port),
                    WebState::Stopped => None,
                };
                match (draft.web_enabled, draft.web_port.trim().parse::<u16>()) {
                    (false, _) => {
                        if running_port.is_some() {
                            self.web = WebState::Stopped;
                        }
                    }
                    (true, Ok(port)) if port != 0 => {
                        if running_port != Some(port) {
                            match WebServer::start(port, self.data.clone()) {
                                Ok(server) => self.web = WebState::Running(server),
                                Err(e) => {
                                    self.warning = Some(format!(
                                        "Port {port} is not available: {e}\n\nThe web interface was not started.",
                                    ));
                                }
                            }
                        }
                    }
                    (true, _) => {
                        self.warning =
                            Some("Port must be a number between 1 and 65535.".to_owned());
                    }
                }
                // The applied config mirrors what actually runs: a failed
                // start is not "enabled", and the port is the live one.
                if matches!(self.web, WebState::Stopped) {
                    draft.web_enabled = false;
                } else if let WebState::Running(server) = &self.web {
                    draft.web_port = server.port.to_string();
                }
                self.config = draft;
                // Newly enabled automatic power limit: authenticate the
                // privileged helper right now. The prompt appears while the
                // user is here (instead of possibly going unnoticed at the
                // first excursion), and a denied authentication turns the
                // option back off via the pending-auth check in `update`.
                let auto = self.config.alerts.auto_min_power;
                if auto != AutoMinPower::Off && auto != current.alerts.auto_min_power {
                    self.auto_power_pending_auth =
                        current.alerts.auto_min_power == AutoMinPower::Off;
                    let _ = self.cmd_tx.send(Cmd::GrantPowerAccess);
                }
            }
        }
        if cancel {
            // Cancel / window close: discard the draft, closing the panel.
            self.draft = None;
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
                if let Some(draft) = &mut self.draft {
                    let sound = Sound::File(path);
                    match fd.field {
                        SoundField::Yellow => draft.alerts.yellow_sound = sound,
                        SoundField::Red => draft.alerts.red_sound = sound,
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
        ctx.request_repaint_after(Duration::from_millis(self.config.poll_ms));
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
