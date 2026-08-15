use eframe::egui;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::raw::c_void;
use std::ptr;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

// ITE IT8915FN constants
const I2C_SLAVE_ADDR: u16 = 0x2B;
const REG_POWER: u8 = 0x80;
const NUM_PINS: usize = 6;
const BYTES_PER_PIN: usize = 4;
const TOTAL_BYTES: usize = NUM_PINS * BYTES_PER_PIN;

// Voltage sanity range for IT8915 detection (millivolts)
const VOLTAGE_SANITY_MIN: u32 = 10_000;
const VOLTAGE_SANITY_MAX: u32 = 14_000;

// Linux i2c-dev ioctl constants
const I2C_SLAVE: libc::c_ulong = 0x0703;
const I2C_SMBUS: libc::c_ulong = 0x0720;
const I2C_SMBUS_READ: u8 = 1;
const I2C_SMBUS_BYTE_DATA: u32 = 2;

// Matches kernel's union i2c_smbus_data { __u8 byte; __u16 word; __u8 block[34]; }.
// Must keep the full 34 bytes: the driver may write more than the byte we use.
#[repr(C)]
union I2cSmbusData {
    byte: u8,
    word: u16,
    block: [u8; 34],
}

// Matches kernel's struct i2c_smbus_ioctl_data
#[repr(C)]
struct I2cSmbusIoctlData {
    read_write: u8,
    command: u8,
    size: u32,
    data: *mut I2cSmbusData,
}

const CURRENT_GREEN: f64 = 9.2;
const CURRENT_YELLOW: f64 = 10.0;

/// One poll sample: per-pin I2C readings plus NVML data.
#[derive(Clone, Default)]
struct Sample {
    pins: Vec<PinReading>,
    power_w: Option<f64>, // from NVML, same source as nvidia-smi
    limit_w: Option<u32>, // current power management limit
    limit_min_w: Option<u32>,
    limit_max_w: Option<u32>,
}

fn main() {
    let bus_number = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .or_else(|| match find_i2c_bus() {
            Some(bus) => Some(bus),
            None => {
                eprintln!("Warning: auto-detect failed. Defaulting to bus 0.");
                eprintln!("Pass the bus number explicitly, e.g.: power-monitor 15");
                Some(0)
            }
        })
        .unwrap_or(0);

    println!("Using I2C bus: {bus_number}");

    // Background thread polls I2C (per-pin) and NVML (total power).
    let (tx, rx) = mpsc::channel::<Sample>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Option<u32>>();
    thread::spawn(move || {
        let i2c_path = format!("/dev/i2c-{}", bus_number);
        let file = match OpenOptions::new().read(true).write(true).open(&i2c_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Cannot open {i2c_path}: {e}. Run as root or add user to i2c group.");
                return;
            }
        };

        // Set slave address
        if unsafe { libc::ioctl(file.as_raw_fd(), I2C_SLAVE, I2C_SLAVE_ADDR as libc::c_uint) < 0 } {
            eprintln!("I2C_SLAVE failed: {}", io::Error::last_os_error());
            return;
        }

        let nvml = nvml::Nvml::new();
        if nvml.is_none() {
            eprintln!(
                "Warning: NVML not available; total power will be estimated from pin readings."
            );
            eprintln!("Power limit control is disabled.");
        }

        loop {
            // Apply any pending power limit change
            while let Ok(Some(watts)) = cmd_rx.try_recv() {
                if let Some(nvml) = nvml.as_ref() {
                    match nvml.set_power_limit(watts) {
                        Ok(()) => println!("Power limit set to {watts} W"),
                        Err(e) => eprintln!("Failed to set power limit to {watts} W: {e}"),
                    }
                }
            }

            let (power_w, limit_w, limit_min_w, limit_max_w) = nvml
                .as_ref()
                .map_or((None, None, None, None), |n| n.power_info());

            match read_pins(&file) {
                Some(pins) => {
                    let sample = Sample {
                        pins,
                        power_w,
                        limit_w,
                        limit_min_w,
                        limit_max_w,
                    };
                    if tx.send(sample).is_err() {
                        break;
                    }
                }
                None => {
                    eprintln!("Read error");
                }
            }
            thread::sleep(Duration::from_millis(200));
        }
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([640.0, 640.0])
            .with_resizable(true),
        ..Default::default()
    };

    let app = App {
        rx,
        cmd_tx,
        sample: Sample::default(),
        pending_limit: None,
        history: vec![Vec::new(); NUM_PINS],
        start: std::time::Instant::now(),
        window_idx: 0,
        show_graphs: true,
    };

    if let Err(e) = eframe::run_native("Power Monitor", options, Box::new(|_cc| Ok(Box::new(app))))
    {
        eprintln!("eframe error: {e}");
        std::process::exit(1);
    }
}

#[derive(Debug, Clone, Default)]
struct PinReading {
    voltage: f64,
    current: f64,
}

/// Read a single register byte via the SMBus byte-data protocol.
/// The IT8915 does not support block reads or combined write+read
/// transfers, so every register must be read individually.
fn smbus_read_byte(fd: RawFd, reg: u8) -> io::Result<u8> {
    let mut data = I2cSmbusData { byte: 0 };
    let ioctl_data = I2cSmbusIoctlData {
        read_write: I2C_SMBUS_READ,
        command: reg,
        size: I2C_SMBUS_BYTE_DATA,
        data: &mut data,
    };
    let ret = unsafe { libc::ioctl(fd, I2C_SMBUS, &ioctl_data as *const _) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    // `byte` is initialized above and (on success) written by the kernel
    Ok(unsafe { data.byte })
}

/// Read all 6 pins. Returns readings in pin order 1-6 (index 0 = pin 1).
/// Register layout: rail `r` lives at offset `r * 4` and maps to pin
/// `NUM_PINS - r` (rail 0 = pin 6, rail 5 = pin 1 — reversed order).
fn read_pins(file: &File) -> Option<Vec<PinReading>> {
    let fd = file.as_raw_fd();

    let mut raw = [0u8; TOTAL_BYTES];
    for i in 0..TOTAL_BYTES {
        raw[i] = smbus_read_byte(fd, REG_POWER + i as u8).ok()?;
    }

    let mut pins = vec![PinReading::default(); NUM_PINS];
    for rail in 0..NUM_PINS {
        let pin = NUM_PINS - 1 - rail;
        let offset = rail * BYTES_PER_PIN;
        let voltage_mv = u16::from_be_bytes([raw[offset], raw[offset + 1]]) as f64;
        let current_ma = u16::from_be_bytes([raw[offset + 2], raw[offset + 3]]) as f64;
        pins[pin] = PinReading {
            voltage: voltage_mv / 1000.0,
            current: current_ma / 1000.0,
        };
    }
    Some(pins)
}

/// Find the i2c bus for the GPU's IT8915.
/// Scans /sys/bus/i2c/devices/i2c-*/name for "NVIDIA i2c adapter 1 at <PCI>",
/// then probes each candidate bus for a valid voltage reading.
fn find_i2c_bus() -> Option<u32> {
    let Ok(entries) = std::fs::read_dir("/sys/bus/i2c/devices") else {
        return None;
    };

    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let name = match std::fs::read_to_string(entry.path().join("name")) {
            Ok(n) => n,
            Err(_) => continue,
        };
        if !name.starts_with("NVIDIA i2c adapter 1 at ") {
            continue;
        }
        let dir = entry.file_name();
        let dir = dir.to_string_lossy();
        if let Some(num) = dir.strip_prefix("i2c-").and_then(|s| s.parse::<u32>().ok()) {
            candidates.push(num);
        }
    }

    candidates.sort();
    for bus in candidates {
        if probe_it8915(bus) {
            return Some(bus);
        }
    }
    None
}

/// Probe a bus for an IT8915 by reading register 0x80 and checking that
/// the voltage is in a sane 10-14V range.
fn probe_it8915(bus: u32) -> bool {
    let path = format!("/dev/i2c-{bus}");
    let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
        return false;
    };
    let fd = file.as_raw_fd();
    if unsafe { libc::ioctl(fd, I2C_SLAVE, I2C_SLAVE_ADDR as libc::c_uint) } < 0 {
        return false;
    }
    let Ok(b0) = smbus_read_byte(fd, REG_POWER) else {
        return false;
    };
    let Ok(b1) = smbus_read_byte(fd, REG_POWER + 1) else {
        return false;
    };
    let voltage_mv = u16::from_be_bytes([b0, b1]) as u32;
    (VOLTAGE_SANITY_MIN..=VOLTAGE_SANITY_MAX).contains(&voltage_mv)
}

/// Minimal FFI bindings to libnvidia-ml.so.1 (NVML).
/// Provides total power draw (same source nvidia-smi uses) and the
/// power management limit. Loaded via dlopen to avoid an extra crate.
mod nvml {
    use super::*;

    pub const NVML_SUCCESS: u32 = 0;
    const NVML_ERROR_NOT_SUPPORTED: u32 = 3;
    const NVML_ERROR_INSUFFICIENT_PERMISSIONS: u32 = 9;

    type FnInit = unsafe extern "C" fn() -> u32;
    type FnGetHandle = unsafe extern "C" fn(index: u32, device: *mut *mut c_void) -> u32;
    type FnGetPowerUsage = unsafe extern "C" fn(device: *mut c_void, power: *mut u32) -> u32;
    type FnGetLimit = unsafe extern "C" fn(device: *mut c_void, limit: *mut u32) -> u32;
    type FnGetLimitConstraints =
        unsafe extern "C" fn(device: *mut c_void, min: *mut u32, max: *mut u32) -> u32;
    type FnSetLimit = unsafe extern "C" fn(device: *mut c_void, limit: u32) -> u32;
    type FnShutdown = unsafe extern "C" fn() -> u32;

    #[derive(Clone, Copy, Debug)]
    pub struct Error(pub u32);

    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.0 {
                NVML_ERROR_NOT_SUPPORTED => write!(f, "not supported by the driver"),
                NVML_ERROR_INSUFFICIENT_PERMISSIONS => {
                    write!(f, "insufficient permissions (run as root)")
                }
                code => write!(f, "NVML error {code}"),
            }
        }
    }

    pub struct Nvml {
        lib: *mut c_void,
        device: *mut c_void,
        get_power_usage: FnGetPowerUsage,
        get_limit: FnGetLimit,
        get_limit_constraints: FnGetLimitConstraints,
        set_limit: FnSetLimit,
        shutdown: FnShutdown,
    }

    // The dlopen handle is shared between threads; NVML is thread-safe.
    unsafe impl Send for Nvml {}

    fn to_watts(value: u32) -> u32 {
        if value >= 1000 {
            value / 1000
        } else {
            value
        }
    }

    // dlsym returns the function's own address, so reinterpret it as the
    // target fn pointer. Transmuting per call site (concrete types) lets the
    // compiler verify both sides are pointer-sized.
    macro_rules! load_fn {
        ($lib:expr, $ty:ty, $name:expr) => {{
            let c_name = CString::new($name).ok()?;
            let sym = unsafe { libc::dlsym($lib, c_name.as_ptr()) };
            if sym.is_null() {
                return None;
            }
            unsafe { std::mem::transmute::<*mut c_void, $ty>(sym) }
        }};
    }

    impl Nvml {
        /// Initialize NVML and get a handle to the first GPU (index 0).
        pub fn new() -> Option<Nvml> {
            let lib_name = CString::new("libnvidia-ml.so.1").ok()?;
            let lib = unsafe { libc::dlopen(lib_name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if lib.is_null() {
                return None;
            }

            let init = load_fn!(lib, FnInit, "nvmlInit_v2");
            let get_handle = load_fn!(lib, FnGetHandle, "nvmlDeviceGetHandleByIndex_v2");
            let get_power_usage = load_fn!(lib, FnGetPowerUsage, "nvmlDeviceGetPowerUsage");
            let get_limit = load_fn!(lib, FnGetLimit, "nvmlDeviceGetPowerManagementLimit");
            let get_limit_constraints = load_fn!(
                lib,
                FnGetLimitConstraints,
                "nvmlDeviceGetPowerManagementLimitConstraints"
            );
            let set_limit = load_fn!(lib, FnSetLimit, "nvmlDeviceSetPowerManagementLimit");
            let shutdown = load_fn!(lib, FnShutdown, "nvmlShutdown");

            if unsafe { init() } != NVML_SUCCESS {
                unsafe { libc::dlclose(lib) };
                return None;
            }

            let mut device: *mut c_void = ptr::null_mut();
            if unsafe { get_handle(0, &mut device) } != NVML_SUCCESS || device.is_null() {
                unsafe {
                    shutdown();
                    libc::dlclose(lib);
                }
                return None;
            }

            Some(Nvml {
                lib,
                device,
                get_power_usage,
                get_limit,
                get_limit_constraints,
                set_limit,
                shutdown,
            })
        }

        /// (total power W, current limit W, min limit W, max limit W)
        pub fn power_info(&self) -> (Option<f64>, Option<u32>, Option<u32>, Option<u32>) {
            let mut mw: u32 = 0;
            let power_w = if unsafe { (self.get_power_usage)(self.device, &mut mw) } == NVML_SUCCESS
            {
                Some(mw as f64 / 1000.0)
            } else {
                None
            };

            // The v1 get functions report the limit in milliwatts on modern
            // drivers (older ones used watts; limits are well under 1000 W).
            let mut limit: u32 = 0;
            let limit_w = if unsafe { (self.get_limit)(self.device, &mut limit) } == NVML_SUCCESS {
                Some(to_watts(limit))
            } else {
                None
            };

            let (min_w, max_w) = {
                let mut min: u32 = 0;
                let mut max: u32 = 0;
                if unsafe { (self.get_limit_constraints)(self.device, &mut min, &mut max) }
                    == NVML_SUCCESS
                {
                    (Some(to_watts(min)), Some(to_watts(max)))
                } else {
                    (None, None)
                }
            };

            (power_w, limit_w, min_w, max_w)
        }

        /// Set the power management limit. The v1 set function takes watts
        /// (the same unit `nvidia-smi -pl` uses), unlike the get functions.
        pub fn set_power_limit(&self, watts: u32) -> Result<(), Error> {
            let code = unsafe { (self.set_limit)(self.device, watts) };
            if code == NVML_SUCCESS {
                Ok(())
            } else {
                Err(Error(code))
            }
        }
    }

    impl Drop for Nvml {
        fn drop(&mut self) {
            unsafe {
                (self.shutdown)();
                libc::dlclose(self.lib);
            }
        }
    }
}

const HORIZONS_MIN: [u32; 6] = [1, 5, 10, 15, 30, 60];
/// Keep a little more history than the largest horizon
const HISTORY_SECS: f64 = 60.0 * 60.0 + 120.0;

fn current_color(current: f64) -> egui::Color32 {
    if current < CURRENT_GREEN {
        egui::Color32::from_rgb(80, 220, 80)
    } else if current < CURRENT_YELLOW {
        egui::Color32::from_rgb(240, 210, 40)
    } else {
        egui::Color32::from_rgb(240, 60, 60)
    }
}

struct App {
    rx: mpsc::Receiver<Sample>,
    cmd_tx: mpsc::Sender<Option<u32>>,
    sample: Sample,
    pending_limit: Option<u32>,
    /// Per-pin history of (elapsed seconds, current A), oldest first.
    history: Vec<Vec<(f64, f64)>>,
    start: std::time::Instant,
    /// Index into HORIZONS_MIN for the visible time window.
    window_idx: u8,
    /// Toggle for the per-pin graphs (history keeps accumulating either way).
    show_graphs: bool,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = self.start.elapsed().as_secs_f64();

        // Append new samples to the history and prune what's expired
        let mut got_sample = false;
        while let Ok(sample) = self.rx.try_recv() {
            for i in 0..NUM_PINS {
                self.history[i].push((now, sample.pins[i].current));
            }
            self.sample = sample;
            got_sample = true;
        }
        if got_sample {
            let t_min = now - HISTORY_SECS;
            for hist in &mut self.history {
                let k = hist.partition_point(|(t, _)| *t < t_min);
                hist.drain(..k);
            }
        }

        let window_secs = HORIZONS_MIN[self.window_idx as usize] as f64 * 60.0;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("ASUS 5090 ROG Astral");
            ui.separator();

            // Total power: prefer NVML (same source as nvidia-smi),
            // fall back to the sum of pin V*A
            let power_text = match self.sample.power_w {
                Some(w) => format!("Total power: {:.1} W", w),
                None => {
                    let est: f64 = self.sample.pins.iter().map(|p| p.voltage * p.current).sum();
                    format!("Total power: {:.1} W (estimated)", est)
                }
            };
            ui.label(egui::RichText::new(power_text).strong().size(20.0));

            // Power management limit (NVML)
            if let (Some(min), Some(max), Some(current)) = (
                self.sample.limit_min_w,
                self.sample.limit_max_w,
                self.sample.limit_w,
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
                        let _ = self.cmd_tx.send(Some(pending));
                    }
                });
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
                        i, self.sample.pins[i].voltage
                    ));
                }
            });
            ui.horizontal(|ui| {
                for i in 3..6 {
                    ui.monospace(format!(
                        "Pin {:>2}: {:>7.3}",
                        i, self.sample.pins[i].voltage
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
                        let color = current_color(self.sample.pins[i].current);
                        ui.label(
                            egui::RichText::new(format!(
                                "Pin {:>2}: {:>7.3}",
                                i, self.sample.pins[i].current
                            ))
                            .color(color),
                        );
                    }
                    if row == 0 {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new("● > 10A")
                                    .color(egui::Color32::from_rgb(240, 60, 60)),
                            );
                            ui.add_space(12.0);
                            ui.label(
                                egui::RichText::new("● 9.2–10A")
                                    .color(egui::Color32::from_rgb(240, 210, 40)),
                            );
                            ui.add_space(12.0);
                            ui.label(
                                egui::RichText::new("● < 9.2A")
                                    .color(egui::Color32::from_rgb(80, 220, 80)),
                            );
                        });
                    }
                });
            }

            if self.show_graphs {
                ui.separator();

                // Per-pin current graphs - 2 rows x 3, like the tables
                ui.strong("Current (A) over time");
                let spacing = 8.0;
                let graph_w = (ui.available_width() - 2.0 * spacing) / 3.0;
                let graph_size = egui::vec2(graph_w.max(60.0), 70.0);
                for row in 0..2 {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = spacing;
                        for col in 0..3 {
                            let pin = row * 3 + col;
                            add_pin_graph(
                                ui,
                                pin,
                                &self.history[pin],
                                now,
                                window_secs,
                                graph_size,
                            );
                        }
                    });
                }
            }
        });

        ctx.request_repaint();
    }
}

/// Draw one small graph of a pin's current over the visible time window.
/// The line is colored per segment by the current thresholds, and the
/// 9.2 A / 10 A limits are drawn as horizontal reference lines.
fn add_pin_graph(
    ui: &mut egui::Ui,
    pin: usize,
    hist: &[(f64, f64)],
    now: f64,
    window_secs: f64,
    size: egui::Vec2,
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
        let mut y_max: f64 = 11.0;
        for (_, v) in vis {
            y_max = y_max.max(*v * 1.05);
        }
        let y_of = |v: f64| rect.top() as f64 + (1.0 - v / y_max) * rect.height() as f64;
        let x_of = |t: f64| rect.left() as f64 + (t - t0) / window_secs * rect.width() as f64;

        // Limit reference lines with numeric labels (colors match the legend)
        let small = egui::FontId::monospace(9.0);
        let dim = ui.visuals().weak_text_color();
        for (limit, color) in [
            (9.2, egui::Color32::from_rgb(240, 210, 40)),
            (10.0, egui::Color32::from_rgb(240, 60, 60)),
        ] {
            let y = y_of(limit) as f32;
            painter.line_segment(
                [
                    egui::Pos2::new(rect.left(), y),
                    egui::Pos2::new(rect.right(), y),
                ],
                egui::Stroke::new(1.0, color),
            );
            painter.text(
                egui::Pos2::new(rect.right() - 18.0, (y - 10.0).max(rect.top() + 1.0)),
                egui::Align2::LEFT_TOP,
                format!("{limit}"),
                small.clone(),
                dim,
            );
        }

        // Y-scale labels
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
                    egui::Stroke::new(1.0, current_color(vis[j].1)),
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
    });
}
