use eframe::egui;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
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
const I2C_SMBUS_I2C_BLOCK_DATA: u32 = 8;
const I2C_RETRIES: libc::c_ulong = 0x0701;

/// Default poll interval in ms (adjustable in the UI, 100 ms..=10 s).
const DEFAULT_POLL_MS: u64 = 1000;

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

/// Allowed range and defaults for the current limits (amperes).
const MIN_LIMIT_A: f32 = 1.0;
const MAX_LIMIT_A: f32 = 12.0;
const DEFAULT_YELLOW_LIMIT_A: f32 = 9.2;
const DEFAULT_RED_LIMIT_A: f32 = 10.0;

/// Directory with the shipped alert sounds (relative to the working dir).
const SOUND_DIR: &str = "sound";

/// The alarm thresholds and sounds of the app. The green/yellow/red color
/// ranges in the tables and graphs, and the warning sounds, follow the
/// limits: below `yellow_limit_a` is green, from there up to (but not
/// including) `red_limit_a` is yellow, and at `red_limit_a` or above is red.
#[derive(Clone, Debug, PartialEq)]
struct Settings {
    /// Current (A) at which the yellow range starts.
    yellow_limit_a: f32,
    /// Current (A) at which the red range starts.
    red_limit_a: f32,
    /// Play `yellow_sound_path` when a pin's current enters the yellow range.
    yellow_sound_enabled: bool,
    yellow_sound_path: PathBuf,
    /// Play `red_sound_path` when a pin's current enters the red range.
    red_sound_enabled: bool,
    red_sound_path: PathBuf,
}

/// Locate one of the shipped alert sounds by name prefix. The files in
/// `sound/` carry generated hash suffixes, so match the prefix instead of
/// hard-coding the full file names; falls back to `<prefix>.wav`.
fn default_sound_path(prefix: &str) -> PathBuf {
    if let Ok(entries) = std::fs::read_dir(SOUND_DIR) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with(prefix) && n.to_lowercase().ends_with(".wav"))
            .collect();
        names.sort();
        if let Some(name) = names.into_iter().next() {
            return PathBuf::from(SOUND_DIR).join(name);
        }
    }
    PathBuf::from(SOUND_DIR).join(format!("{prefix}.wav"))
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            yellow_limit_a: DEFAULT_YELLOW_LIMIT_A,
            red_limit_a: DEFAULT_RED_LIMIT_A,
            yellow_sound_enabled: false,
            yellow_sound_path: default_sound_path("attention"),
            red_sound_enabled: false,
            red_sound_path: default_sound_path("critical"),
        }
    }
}

/// Play an alert sound with the first system player that starts; runs on a
/// helper thread so the caller never blocks. Sound is best-effort: missing
/// players or files are ignored.
fn play_sound(path: &Path) {
    let path = path.to_owned();
    thread::spawn(move || {
        for player in ["aplay", "pw-play", "paplay"] {
            let mut cmd = Command::new(player);
            if player == "aplay" {
                cmd.arg("-q");
            }
            match cmd
                .arg(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    let _ = child.wait();
                    return;
                }
                Err(_) => continue,
            }
        }
    });
}

/// Fold one sample's maximum pin current into the latched over-limit state
/// and return the alert sound triggered by the rising edge, if any. Red
/// takes priority over yellow, and no sound repeats while a limit stays
/// surpassed.
fn over_limit_sound(
    latched_yellow: &mut bool,
    latched_red: &mut bool,
    max_current: f64,
    s: &Settings,
) -> Option<PathBuf> {
    let over_yellow = max_current >= s.yellow_limit_a as f64;
    let over_red = max_current >= s.red_limit_a as f64;
    let sound = if over_red && !*latched_red && s.red_sound_enabled {
        Some(s.red_sound_path.clone())
    } else if over_yellow && !over_red && !*latched_yellow && s.yellow_sound_enabled {
        Some(s.yellow_sound_path.clone())
    } else {
        None
    };
    *latched_yellow = over_yellow;
    *latched_red = over_red;
    sound
}

/// One poll sample: per-pin I2C readings plus NVML data.
#[derive(Clone, Default)]
struct Sample {
    pins: Vec<PinReading>,
    power_w: Option<f64>, // from NVML, same source as nvidia-smi
    limit_w: Option<u32>, // current power management limit
    limit_min_w: Option<u32>,
    limit_max_w: Option<u32>,
    temp_c: Option<f64>,       // GPU temperature from NVML
    max_temp_c: Option<f64>,   // max supported temperature from NVML
    mem_used_gb: Option<f64>,  // VRAM used (GB) from NVML
    mem_total_gb: Option<f64>, // VRAM total (GB) from NVML
}

/// Commands from the UI to the poller thread.
enum Cmd {
    SetLimit(u32),
    Interval(u64),
}

fn main() {
    let bus_arg: Option<String> = std::env::args().nth(1);

    let bus_number = match bus_arg
        .as_ref()
        .and_then(|s| s.parse::<u32>().ok())
        .or_else(find_i2c_bus)
    {
        Some(bus) => bus,
        None => {
            eprintln!("Warning: auto-detect failed. Defaulting to bus 0.");
            eprintln!("Pass the bus number explicitly, e.g.: power-monitor 15");
            0
        }
    };

    println!("Using I2C bus: {bus_number}");

    // Pre-reserve history for the default 1 Hz sample rate so the common
    // case never reallocates; a faster slider setting just grows the vecs.
    let mut web_data = WebData::new();
    let reserve = (HISTORY_SECS * 1000.0 / DEFAULT_POLL_MS as f64) as usize + 1024;
    for h in &mut web_data.history {
        h.reserve(reserve);
    }
    web_data.power_history.reserve(reserve);
    web_data.temp_history.reserve(reserve);
    web_data.mem_history.reserve(reserve);
    let data = Arc::new(Mutex::new(web_data));

    // Background thread polls I2C (per-pin) and NVML (total power). It
    // records each sample straight into the shared data, so monitoring
    // keeps working while the window is minimized (the web server never
    // depends on the GUI drawing frames).
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let poller_data = data.clone();
    thread::spawn(move || {
        let data = poller_data;
        // I2C is best-effort: if the device is unavailable, the app keeps
        // running with zero pin readings.
        let mut file: Option<File> = None;
        let i2c_path = format!("/dev/i2c-{}", bus_number);
        match OpenOptions::new().read(true).write(true).open(&i2c_path) {
            Ok(f) => {
                // Set slave address
                if unsafe {
                    libc::ioctl(f.as_raw_fd(), I2C_SLAVE, I2C_SLAVE_ADDR as libc::c_uint) < 0
                } {
                    eprintln!("I2C_SLAVE failed: {}", io::Error::last_os_error());
                } else {
                    // Fail fast on a bad transfer instead of letting the
                    // kernel retry (retries multiply worst-case latency).
                    let _ =
                        unsafe { libc::ioctl(f.as_raw_fd(), I2C_RETRIES, 0usize as libc::c_ulong) };
                    file = Some(f);
                }
            }
            Err(e) => {
                eprintln!("Cannot open {i2c_path}: {e}. Run as root or add user to i2c group.");
            }
        }
        if file.is_none() {
            eprintln!("Continuing without I2C (pin readings will be zero).");
        }

        let nvml = nvml::Nvml::new();
        if nvml.is_none() {
            eprintln!(
                "Warning: NVML not available; total power will be estimated from pin readings."
            );
            eprintln!("Power limit control is disabled.");
        }

        let mut polls: u64 = 0;
        let mut poll_sum_ms: u64 = 0;
        let mut poll_max_ms: u64 = 0;
        let mut poll_ms = DEFAULT_POLL_MS;

        loop {
            // Handle UI commands
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    Cmd::SetLimit(watts) => {
                        if let Some(nvml) = nvml.as_ref() {
                            match nvml.set_power_limit(watts) {
                                Ok(()) => println!("Power limit set to {watts} W"),
                                Err(e) => {
                                    eprintln!("Failed to set power limit to {watts} W: {e}")
                                }
                            }
                        }
                    }
                    Cmd::Interval(ms) => poll_ms = ms,
                }
            }

            let (
                power_w,
                limit_w,
                limit_min_w,
                limit_max_w,
                temp_c,
                max_temp_c,
                mem_used_gb,
                mem_total_gb,
            ) = nvml
                .as_ref()
                .map_or((None, None, None, None, None, None, None, None), |n| {
                    let (p, l, lmin, lmax) = n.power_info();
                    let (mu, mt) = n.memory_info();
                    (
                        p,
                        l,
                        lmin,
                        lmax,
                        n.temperature(),
                        n.max_temperature(),
                        mu,
                        mt,
                    )
                });

            let pins =
                match file.as_ref() {
                    Some(f) => {
                        let t = std::time::Instant::now();
                        match read_pins(f) {
                            Some(pins) => {
                                let el = t.elapsed().as_millis() as u64;
                                polls += 1;
                                poll_sum_ms += el;
                                poll_max_ms = poll_max_ms.max(el);
                                if polls == 50 || polls % 500 == 0 {
                                    eprintln!(
                                    "I2C read batch: avg {} ms per poll, worst {} ms (polls 1..{})",
                                    poll_sum_ms / polls, poll_max_ms, polls
                                );
                                }
                                pins
                            }
                            None => {
                                eprintln!("Read error");
                                thread::sleep(Duration::from_millis(poll_ms));
                                continue;
                            }
                        }
                    }
                    None => vec![PinReading::default(); NUM_PINS],
                };

            let sample = Sample {
                pins,
                power_w,
                limit_w,
                limit_min_w,
                limit_max_w,
                temp_c,
                max_temp_c,
                mem_used_gb,
                mem_total_gb,
            };
            data.lock().unwrap().record(sample);
            thread::sleep(Duration::from_millis(poll_ms));
        }
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 860.0])
            .with_resizable(true),
        ..Default::default()
    };

    let app = App {
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
/// Used by bus auto-detection and as a fallback when the I2C block
/// read protocol is unavailable.
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

/// Read all 24 power registers in a single I2C block read (host-specified
/// length; the chip streams consecutive register bytes without SMBus
/// framing). Returns the number of bytes read. Verified ~5x cheaper than
/// 24 byte reads, which matters because each transaction takes a lock on
/// the GPU's internal I2C bus and can stutter games.
fn smbus_read_block(fd: RawFd, reg: u8, data: &mut I2cSmbusData) -> io::Result<usize> {
    unsafe {
        data.block[0] = TOTAL_BYTES as u8;
    }
    let ioctl_data = I2cSmbusIoctlData {
        read_write: I2C_SMBUS_READ,
        command: reg,
        size: I2C_SMBUS_I2C_BLOCK_DATA,
        data,
    };
    let ret = unsafe { libc::ioctl(fd, I2C_SMBUS, &ioctl_data as *const _) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { data.block[0] } as usize)
}

static BLOCK_FALLBACK_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Read all 6 pins. Returns readings in pin order 1-6 (index 0 = pin 1).
/// Register layout: rail `r` lives at offset `r * 4` and maps to pin
/// `NUM_PINS - r` (rail 0 = pin 6, rail 5 = pin 1 — reversed order).
fn read_pins(file: &File) -> Option<Vec<PinReading>> {
    let fd = file.as_raw_fd();

    let mut raw = [0u8; TOTAL_BYTES];
    let mut data = I2cSmbusData { block: [0u8; 34] };
    if smbus_read_block(fd, REG_POWER, &mut data).is_ok_and(|n| n == TOTAL_BYTES) {
        unsafe {
            raw.copy_from_slice(&data.block[1..=TOTAL_BYTES]);
        }
    } else {
        if BLOCK_FALLBACK_WARNED
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            eprintln!("I2C block read unavailable; using 24 byte reads per poll.");
        }
        for i in 0..TOTAL_BYTES {
            raw[i] = smbus_read_byte(fd, REG_POWER + i as u8).ok()?;
        }
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
    /// Sensor type for the main GPU temperature (NVML_TEMPERATURE_GPU).
    const NVML_TEMPERATURE_GPU: u32 = 0;
    /// Temperature thresholds (NVML_TEMPERATURE_THRESHOLD_t).
    const NVML_TEMPERATURE_THRESHOLD_SHUTDOWN: u32 = 0;
    const NVML_TEMPERATURE_THRESHOLD_SLOWDOWN: u32 = 1;
    /// "Maximum temperature that is supported by the GPU".
    const NVML_TEMPERATURE_THRESHOLD_MAX: u32 = 4;

    type FnInit = unsafe extern "C" fn() -> u32;
    type FnGetHandle = unsafe extern "C" fn(index: u32, device: *mut *mut c_void) -> u32;
    type FnGetPowerUsage = unsafe extern "C" fn(device: *mut c_void, power: *mut u32) -> u32;
    type FnGetLimit = unsafe extern "C" fn(device: *mut c_void, limit: *mut u32) -> u32;
    type FnGetLimitConstraints =
        unsafe extern "C" fn(device: *mut c_void, min: *mut u32, max: *mut u32) -> u32;
    type FnSetLimit = unsafe extern "C" fn(device: *mut c_void, limit: u32) -> u32;
    type FnShutdown = unsafe extern "C" fn() -> u32;
    type FnGetTemperature =
        unsafe extern "C" fn(device: *mut c_void, sensor: u32, temp: *mut u32) -> u32;
    type FnGetTemperatureThreshold =
        unsafe extern "C" fn(device: *mut c_void, threshold: u32, temp: *mut i32) -> u32;
    type FnGetMemoryInfo =
        unsafe extern "C" fn(device: *mut c_void, memory: *mut NvmlMemory) -> u32;

    /// Matches NVML's `nvml_memory_t` (all fields in bytes). Field order
    /// matters: it must stay in sync with the C ABI (total, free, used).
    #[repr(C)]
    pub struct NvmlMemory {
        pub total: u64,
        pub free: u64,
        pub used: u64,
    }

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
        get_temperature: FnGetTemperature,
        get_temperature_threshold: FnGetTemperatureThreshold,
        get_memory_info: FnGetMemoryInfo,
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
            let get_temperature = load_fn!(lib, FnGetTemperature, "nvmlDeviceGetTemperature");
            let get_temperature_threshold = load_fn!(
                lib,
                FnGetTemperatureThreshold,
                "nvmlDeviceGetTemperatureThreshold"
            );
            let get_memory_info = load_fn!(lib, FnGetMemoryInfo, "nvmlDeviceGetMemoryInfo");

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
                get_temperature,
                get_temperature_threshold,
                get_memory_info,
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

        /// GPU temperature in degrees Celsius (NVML_TEMPERATURE_GPU sensor).
        pub fn temperature(&self) -> Option<f64> {
            let mut temp: u32 = 0;
            if unsafe { (self.get_temperature)(self.device, NVML_TEMPERATURE_GPU, &mut temp) }
                == NVML_SUCCESS
            {
                Some(temp as f64)
            } else {
                None
            }
        }

        /// Maximum supported GPU temperature in degrees Celsius, if the
        /// driver reports it. Prefers NVML_TEMPERATURE_THRESHOLD_MAX, but
        /// some drivers don't expose it, so fall back to the shutdown and
        /// then slowdown thresholds (the real hardware limit either way).
        pub fn max_temperature(&self) -> Option<f64> {
            for threshold in [
                NVML_TEMPERATURE_THRESHOLD_MAX,
                NVML_TEMPERATURE_THRESHOLD_SHUTDOWN,
                NVML_TEMPERATURE_THRESHOLD_SLOWDOWN,
            ] {
                let mut temp: i32 = 0;
                if unsafe { (self.get_temperature_threshold)(self.device, threshold, &mut temp) }
                    == NVML_SUCCESS
                    && temp > 0
                {
                    return Some(temp as f64);
                }
            }
            None
        }

        /// (VRAM used GB, VRAM total GB) in GiB, like nvidia-smi (a 32 GB
        /// card reads as 32, not 34.4).
        pub fn memory_info(&self) -> (Option<f64>, Option<f64>) {
            const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
            let mut mem = NvmlMemory {
                total: 0,
                free: 0,
                used: 0,
            };
            if unsafe { (self.get_memory_info)(self.device, &mut mem) } == NVML_SUCCESS {
                (Some(mem.used as f64 / GIB), Some(mem.total as f64 / GIB))
            } else {
                (None, None)
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

// ---------------------------------------------------------------------------
// Web interface
// ---------------------------------------------------------------------------

/// Default port for the web interface (editable in the UI).
const DEFAULT_WEB_PORT: u16 = 8666;

/// Max points per series sent to a web client; the canvas can't show more
/// anyway, and it keeps the payload small at the 60-minute horizon.
const WEB_MAX_PTS: usize = 600;

/// The data shared between the GUI and the web server: the latest sample,
/// the history buffers, the permanent y ranges and a few mirrored UI
/// settings. Both sides take one mutex; the critical sections are short.
struct WebData {
    /// Latest sample from the poller thread.
    sample: Sample,
    /// App start; all histories use elapsed seconds from this instant.
    start: std::time::Instant,
    /// Per-pin history of (elapsed seconds, current A), oldest first.
    history: Vec<Vec<(f64, f64)>>,
    /// GPU metric histories of (elapsed seconds, value), oldest first.
    power_history: Vec<(f64, f64)>, // W
    temp_history: Vec<(f64, f64)>, // °C
    mem_history: Vec<(f64, f64)>,  // VRAM used, GB
    /// Permanent y ranges (max value) for the metric graphs, learned from
    /// NVML: max power limit (W), max supported temp (°C), total VRAM (GB).
    power_range_max: Option<f64>,
    temp_range_max: Option<f64>,
    mem_range_max: Option<f64>,
    /// Sample interval, mirrored so the web UI can display it.
    poll_ms: u64,
    /// The "Show graphs" toggle, mirrored so the web UI matches the GUI.
    show_graphs: bool,
    /// The applied limits and sounds, mirrored so the poller (alert sounds)
    /// and the web page (thresholds) follow the settings dialog.
    settings: Settings,
    /// Rising-edge latches for the alert sounds.
    yellow_latched: bool,
    red_latched: bool,
}

impl WebData {
    fn new() -> Self {
        WebData {
            sample: Sample::default(),
            start: std::time::Instant::now(),
            history: vec![Vec::new(); NUM_PINS],
            power_history: Vec::new(),
            temp_history: Vec::new(),
            mem_history: Vec::new(),
            power_range_max: None,
            temp_range_max: None,
            mem_range_max: None,
            poll_ms: DEFAULT_POLL_MS,
            show_graphs: true,
            settings: Settings::default(),
            yellow_latched: false,
            red_latched: false,
        }
    }

    /// Fold one poller sample into the shared state: replace the latest
    /// sample, append it to the histories (pruning what expired) and cache
    /// the permanent NVML ranges. Called from the poller thread so data
    /// keeps flowing while the GUI window is minimized or not drawing.
    fn record(&mut self, sample: Sample) {
        let now = self.start.elapsed().as_secs_f64();
        for i in 0..NUM_PINS {
            self.history[i].push((now, sample.pins[i].current));
        }
        // Power: prefer NVML, fall back to the estimated pin sum so the
        // graph still has data when NVML is unavailable.
        let power_val = sample.power_w.or_else(|| {
            Some(
                sample
                    .pins
                    .iter()
                    .map(|p| p.voltage * p.current)
                    .sum::<f64>(),
            )
        });
        if let Some(p) = power_val {
            self.power_history.push((now, p));
        }
        if let Some(t) = sample.temp_c {
            self.temp_history.push((now, t));
        }
        if let Some(m) = sample.mem_used_gb {
            self.mem_history.push((now, m));
        }
        // Cache the permanent y ranges reported by NVML. They are static
        // per GPU, so the first successful poll fixes the graph scales.
        if let Some(m) = sample.limit_max_w {
            self.power_range_max = Some(m as f64);
        }
        if let Some(m) = sample.max_temp_c {
            self.temp_range_max = Some(m);
        }
        if let Some(m) = sample.mem_total_gb {
            self.mem_range_max = Some(m);
        }

        // Alert sounds: play on the rising edge of the yellow/red limits.
        let max_current = sample
            .pins
            .iter()
            .map(|p| p.current)
            .fold(f64::NEG_INFINITY, f64::max);
        if let Some(path) = over_limit_sound(
            &mut self.yellow_latched,
            &mut self.red_latched,
            max_current,
            &self.settings,
        ) {
            play_sound(&path);
        }
        self.sample = sample;

        // Prune what fell out of the largest horizon.
        let t_min = now - HISTORY_SECS;
        let prune = |hist: &mut Vec<(f64, f64)>| {
            let k = hist.partition_point(|(t, _)| *t < t_min);
            hist.drain(..k);
        };
        for hist in &mut self.history {
            prune(hist);
        }
        prune(&mut self.power_history);
        prune(&mut self.temp_history);
        prune(&mut self.mem_history);
    }
}

/// Web server lifecycle. Dropping the `Running` state (or replacing it with
/// `Stopped`) stops the server and joins its thread.
enum WebState {
    Stopped,
    Running(WebServer),
}

struct WebServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl WebServer {
    /// Bind the port synchronously so a busy port is reported to the UI
    /// before the checkbox is accepted, then spawn the serve thread.
    fn start(port: u16, data: Arc<Mutex<WebData>>) -> io::Result<WebServer> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = thread::spawn(move || web_serve(listener, data, thread_stop));
        println!("Web interface listening on http://{local}/");
        Ok(WebServer {
            port,
            stop,
            handle: Some(handle),
        })
    }

    /// Ask the serve loop to exit and wait for it. Bounded: the loop polls
    /// the flag every 50 ms and never blocks on the data mutex.
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        println!("Web interface stopped (port {})", self.port);
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn web_serve(listener: TcpListener, data: Arc<Mutex<WebData>>, stop: Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                // The accepted socket inherits the listener's non-blocking
                // mode; put it back to blocking for the request.
                let _ = stream.set_nonblocking(false);
                let data = data.clone();
                thread::spawn(move || web_handle(stream, data));
            }
            // No connection yet: poll the stop flag again shortly.
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Serve one HTTP connection: read the request head, answer with the page,
/// the JSON data, or an error, then close the connection.
fn web_handle(mut stream: TcpStream, data: Arc<Mutex<WebData>>) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

    let mut buf = [0u8; 4096];
    let mut head: Vec<u8> = Vec::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if head.len() > 8192 {
                    return;
                }
            }
            Err(_) => return,
        }
    }

    let request = String::from_utf8_lossy(&head);
    let request_line = request.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    match (method, path) {
        ("GET", "/" | "/index.html") => {
            web_respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                INDEX_HTML,
            );
        }
        ("GET", "/data") => {
            let window_secs = query_param(query, "window")
                .and_then(|w| w.parse::<u64>().ok())
                .filter(|w| (60..=3600).contains(w))
                .unwrap_or(60);
            // Try (re-)locking instead of blocking so a busy GUI frame can
            // never deadlock against the shutdown join; clients retry on 503.
            let mut body = None;
            for _ in 0..10 {
                if let Ok(d) = data.try_lock() {
                    body = Some(web_json(&d, window_secs as f64));
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            match body {
                Some(body) => web_respond(
                    &mut stream,
                    "200 OK",
                    "application/json; charset=utf-8",
                    &body,
                ),
                None => web_respond(&mut stream, "503 Service Unavailable", "text/plain", "busy"),
            }
        }
        ("GET", _) => web_respond(&mut stream, "404 Not Found", "text/plain", "Not found"),
        _ => web_respond(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            "Method not allowed",
        ),
    }
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .find_map(|pair| pair.split_once('='))
        .and_then(|(k, v)| (k == key).then_some(v))
}

/// Write a complete HTTP response; the connection is then closed.
fn web_respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Serialize the shared state for one web client: the latest sample plus
/// the history for the requested time window (seconds). Hand-rolled to keep
/// the dependency list short.
fn web_json(data: &WebData, window_secs: f64) -> String {
    let now = data.start.elapsed().as_secs_f64();
    let t_min = now - window_secs;
    let s = &data.sample;

    let mut out = String::with_capacity(64 * 1024);
    out.push_str("{\"now\":");
    json_f(&mut out, now, 1);
    out.push_str(",\"window\":");
    out.push_str(&(window_secs as u64).to_string());
    out.push_str(",\"poll_ms\":");
    out.push_str(&data.poll_ms.to_string());
    out.push_str(",\"limits\":[");
    out.push_str(&format!(
        "{:.1},{:.1}",
        data.settings.yellow_limit_a, data.settings.red_limit_a
    ));
    out.push_str("]");
    out.push_str(",\"show_graphs\":");
    out.push_str(if data.show_graphs { "true" } else { "false" });
    out.push_str(",\"sample\":{\"pins\":[");
    for (i, p) in s.pins.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("[{:.3},{:.3}]", p.voltage, p.current));
    }
    out.push_str("],\"power_w\":");
    json_opt_f(&mut out, s.power_w, 1);
    out.push_str(",\"limit_w\":");
    json_opt_u32(&mut out, s.limit_w);
    out.push_str(",\"limit_min_w\":");
    json_opt_u32(&mut out, s.limit_min_w);
    out.push_str(",\"limit_max_w\":");
    json_opt_u32(&mut out, s.limit_max_w);
    out.push_str(",\"temp_c\":");
    json_opt_f(&mut out, s.temp_c, 1);
    out.push_str(",\"max_temp_c\":");
    json_opt_f(&mut out, s.max_temp_c, 1);
    out.push_str(",\"mem_used_gb\":");
    json_opt_f(&mut out, s.mem_used_gb, 2);
    out.push_str(",\"mem_total_gb\":");
    json_opt_f(&mut out, s.mem_total_gb, 1);
    out.push_str("},\"range_max\":{\"power\":");
    json_opt_f(&mut out, data.power_range_max, 1);
    out.push_str(",\"temp\":");
    json_opt_f(&mut out, data.temp_range_max, 1);
    out.push_str(",\"mem\":");
    json_opt_f(&mut out, data.mem_range_max, 1);
    out.push_str("},\"data\":{\"pins\":[");
    for (i, hist) in data.history.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_series(&mut out, hist, t_min);
    }
    out.push_str("],\"power\":");
    push_series(&mut out, &data.power_history, t_min);
    out.push_str(",\"temp\":");
    push_series(&mut out, &data.temp_history, t_min);
    out.push_str(",\"mem\":");
    push_series(&mut out, &data.mem_history, t_min);
    out.push_str("}}");
    out
}

/// Append one history as a JSON array of [t, v] pairs, restricted to the
/// visible window and downsampled to at most WEB_MAX_PTS points (always
/// keeping the newest sample).
fn push_series(out: &mut String, hist: &[(f64, f64)], t_min: f64) {
    let k = hist.partition_point(|(t, _)| *t < t_min);
    let vis = &hist[k..];
    if vis.is_empty() {
        out.push_str("[]");
        return;
    }
    let n = vis.len();
    let stride = n.div_ceil(WEB_MAX_PTS).max(1);
    let mut idx: Vec<usize> = (0..n).step_by(stride).collect();
    if idx.last() != Some(&(n - 1)) {
        idx.push(n - 1);
    }
    out.push('[');
    for (i, &j) in idx.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("[{:.1},{:.2}]", vis[j].0, vis[j].1));
    }
    out.push(']');
}

fn json_f(out: &mut String, v: f64, decimals: usize) {
    out.push_str(&format!("{v:.prec$}", prec = decimals));
}

fn json_opt_f(out: &mut String, v: Option<f64>, decimals: usize) {
    match v {
        Some(v) => json_f(out, v, decimals),
        None => out.push_str("null"),
    }
}

fn json_opt_u32(out: &mut String, v: Option<u32>) {
    match v {
        Some(v) => out.push_str(&v.to_string()),
        None => out.push_str("null"),
    }
}

/// The single-page web UI. It polls /data and mirrors the graphical UI:
/// the same metrics, tables, color coding and graphs, with a variable
/// 1..60 minute time window.
const INDEX_HTML: &str = r###"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Power Monitor</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { margin: 0; background: #1e1e1e; color: #d4d4d4; font-family: system-ui, -apple-system, sans-serif; }
  .wrap { max-width: 1100px; margin: 0 auto; padding: 10px 16px 48px; }
  h1 { font-size: 22px; margin: 10px 0 6px; }
  .stats { display: flex; flex-wrap: wrap; gap: 20px; font-size: 17px; font-weight: 600; }
  #power { font-size: 21px; }
  .line { margin: 8px 0; font-size: 14px; }
  .dim { color: #999; }
  .tables { display: flex; gap: 48px; margin: 10px 0 4px; }
  .tables h2 { font-size: 14px; font-weight: 700; margin: 0 0 4px; }
  table { border-collapse: collapse; }
  td { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 13px; padding: 1px 16px 1px 0; white-space: pre; }
  .g { color: #50dc50; }
  .y { color: #f0d228; }
  .r { color: #f03c3c; }
  .legend { font-size: 12px; margin: 2px 0 10px; }
  .legend span { margin-right: 14px; }
  .graph-title { font-weight: 700; margin: 14px 0 6px; }
  .windows { margin: 0 0 8px; }
  .windows button { background: #2b2b2b; color: #8a8a8a; border: 1px solid #3d3d3d; border-radius: 3px; padding: 3px 12px; margin-right: 4px; font-size: 13px; cursor: pointer; }
  .windows button:hover { background: #343434; }
  .windows button.on { color: #78beff; font-weight: 700; border-color: #78beff; }
  .grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: 8px; }
  .cell { min-width: 0; }
  .name { font-weight: 700; font-size: 13px; margin-bottom: 2px; }
  canvas { display: block; width: 100%; height: 150px; background: #232323; border: 1px solid #3a3a3a; }
  #err { display: none; color: #f03c3c; margin-top: 12px; }
  @media (max-width: 800px) { .grid { grid-template-columns: repeat(2, 1fr); } .tables { gap: 24px; } }
</style>
</head>
<body>
<div class="wrap">
  <h1>ASUS 5090 ROG Astral</h1>
  <div class="stats">
    <span id="power">Total power: …</span>
    <span id="temp"></span>
    <span id="vram"></span>
  </div>
  <div id="limit" class="line"></div>
  <div id="interval" class="line dim"></div>
  <div class="tables">
    <div>
      <h2>Voltage (V)</h2>
      <table id="volt"></table>
    </div>
    <div>
      <h2>Current (A)</h2>
      <table id="curr"></table>
    </div>
  </div>
  <div class="legend" id="legend">
    <span class="r">● &gt; 10A</span><span class="y">● 9.2–10A</span><span class="g">● &lt; 9.2A</span>
  </div>
  <div id="graphs">
    <div class="graph-title">Current and GPU metrics over time</div>
    <div class="windows" id="windows"></div>
    <div class="grid" id="grid"></div>
  </div>
  <div id="err"></div>
</div>
<script>
'use strict';

const HORIZONS = [1, 5, 10, 15, 30, 60];
let windowMin = 1;
// [yellow, red] current limits (A); updated from the settings dialog.
let limits = [9.2, 10.0];

const winsEl = document.getElementById('windows');
HORIZONS.forEach(function (m) {
  const b = document.createElement('button');
  b.textContent = m + 'm';
  b.addEventListener('click', function () { windowMin = m; syncWindowButtons(); });
  winsEl.appendChild(b);
});
function syncWindowButtons() {
  Array.from(winsEl.children).forEach(function (b, i) {
    b.classList.toggle('on', HORIZONS[i] === windowMin);
  });
}
syncWindowButtons();

const NAMES = ['Pin 0', 'Pin 1', 'Pin 2', 'Pin 3', 'Pin 4', 'Pin 5', 'Power (W)', 'Temp (°C)', 'VRAM (GB)'];
const METRIC_COLORS = { 6: '#ffa500', 7: '#ff5a46', 8: '#8cbefe' };
const grid = document.getElementById('grid');
const canvases = [];
// Last mouse position per canvas (CSS px, null when the pointer is away).
const hoverPos = new Array(NAMES.length).fill(null);
// Latest drawGraph arguments, so mouse moves can redraw between polls.
const drawArgs = new Array(NAMES.length).fill(null);
NAMES.forEach(function (n, i) {
  const cell = document.createElement('div');
  cell.className = 'cell';
  const label = document.createElement('div');
  label.className = 'name';
  label.textContent = n;
  const cv = document.createElement('canvas');
  cell.appendChild(label);
  cell.appendChild(cv);
  grid.appendChild(cell);
  canvases.push(cv);
  cv.addEventListener('mousemove', function (e) {
    const r = cv.getBoundingClientRect();
    hoverPos[i] = { x: e.clientX - r.left, y: e.clientY - r.top };
    redrawGraph(i);
  });
  cv.addEventListener('mouseleave', function () {
    hoverPos[i] = null;
    redrawGraph(i);
  });
});
function redrawGraph(i) {
  const a = drawArgs[i];
  if (a) drawGraph(canvases[i], a[0], a[1], a[2], a[3], a[4]);
}

function currentColor(v) {
  if (v < limits[0]) return '#50dc50';
  if (v < limits[1]) return '#f0d228';
  return '#f03c3c';
}

function drawGraph(cv, pts, now, win, fixedMax, idx) {
  drawArgs[idx] = [pts, now, win, fixedMax, idx];
  const dpr = window.devicePixelRatio || 1;
  const w = cv.clientWidth, h = cv.clientHeight;
  if (w === 0 || h === 0) return;
  const pw = Math.round(w * dpr), ph = Math.round(h * dpr);
  if (cv.width !== pw || cv.height !== ph) { cv.width = pw; cv.height = ph; }
  const ctx = cv.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);

  const isPin = idx < 6;
  const t0 = now - win;
  let k = 0;
  while (k < pts.length && pts[k][0] < t0) k++;
  const vis = pts.slice(k);
  const n = vis.length;

  let yMax;
  if (isPin) {
    // keep the limit lines visible, grow the scale for spikes
    yMax = limits[1] * 1.1;
    for (let i = 0; i < n; i++) yMax = Math.max(yMax, vis[i][1] * 1.05);
  } else if (fixedMax && fixedMax > 0) {
    yMax = fixedMax;
  } else {
    yMax = 1.0;
    for (let i = 0; i < n; i++) yMax = Math.max(yMax, vis[i][1]);
    yMax *= 1.08;
  }

  const xOf = function (t) { return ((t - t0) / win) * w; };
  const yOf = function (v) {
    if (isPin) return (1 - v / yMax) * h;
    return (1 - Math.min(Math.max(v, 0), yMax) / yMax) * h;
  };

  ctx.font = '9px monospace';
  ctx.fillStyle = '#8a8a8a';
  if (!isPin || yMax > limits[1] * 1.1) ctx.fillText(yMax.toFixed(0), 2, 10);
  ctx.fillText('0', 2, h - 3);

  if (isPin) {
    const lims = [[limits[0], '#a5964b'], [limits[1], '#af5555']];
    for (let i = 0; i < lims.length; i++) {
      const lim = lims[i][0], col = lims[i][1];
      const y = yOf(lim);
      ctx.strokeStyle = col;
      ctx.setLineDash([4, 3]);
      ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(w, y); ctx.stroke();
      ctx.setLineDash([]);
      ctx.fillStyle = col;
      ctx.fillText(String(lim), w - 20, Math.max(y - 3, 9));
    }
  }

  if (n >= 2) {
    const maxPts = Math.max(2, Math.floor(w) * 2);
    const stride = Math.max(1, Math.ceil(n / maxPts));
    ctx.lineWidth = isPin ? 1 : 1.5;
    if (!isPin) ctx.strokeStyle = METRIC_COLORS[idx];
    let i = 0;
    while (i + stride < n) {
      const j = i + stride;
      ctx.beginPath();
      ctx.moveTo(xOf(vis[i][0]), yOf(vis[i][1]));
      ctx.lineTo(xOf(vis[j][0]), yOf(vis[j][1]));
      if (isPin) ctx.strokeStyle = currentColor(vis[j][1]);
      ctx.stroke();
      i = j;
    }
    if (i < n - 1) {
      const j = n - 1;
      ctx.beginPath();
      ctx.moveTo(xOf(vis[i][0]), yOf(vis[i][1]));
      ctx.lineTo(xOf(vis[j][0]), yOf(vis[j][1]));
      if (isPin) ctx.strokeStyle = currentColor(vis[j][1]);
      ctx.stroke();
    }
  }

  // Hover readout, drawn last so it sits above the lines.
  drawHover(ctx, w, h, vis, t0, win, xOf, yOf, idx);
}

// Crosshair readout under the pointer, mirroring the desktop UI:
// nearest sample by time, a dim vertical line, a value-colored dot,
// and a value label along the top that flips sides near the right edge.
function drawHover(ctx, w, h, vis, t0, win, xOf, yOf, idx) {
  const hv = hoverPos[idx];
  if (!hv || vis.length === 0) return;

  const frac = Math.min(Math.max(hv.x / w, 0), 1);
  const t = t0 + frac * win;
  let k = 0;
  while (k < vis.length && vis[k][0] < t) k++;
  let p;
  if (k === 0) p = vis[0];
  else if (k >= vis.length) p = vis[vis.length - 1];
  else p = (t - vis[k - 1][0] <= vis[k][0] - t) ? vis[k - 1] : vis[k];
  const x = xOf(p[0]);
  const y = yOf(p[1]);

  ctx.strokeStyle = '#565656';
  ctx.lineWidth = 1;
  ctx.beginPath(); ctx.moveTo(x, 0); ctx.lineTo(x, h); ctx.stroke();

  ctx.fillStyle = idx < 6 ? currentColor(p[1]) : METRIC_COLORS[idx];
  ctx.beginPath(); ctx.arc(x, y, 3, 0, Math.PI * 2); ctx.fill();
  ctx.strokeStyle = '#232323';
  ctx.stroke();

  const unit = idx < 6 ? 'A' : ['W', '°C', 'GB'][idx - 6];
  const label = p[1].toFixed(2) + ' ' + unit;
  ctx.font = '11px monospace';
  const lw = ctx.measureText(label).width;
  let lx = x + 6;
  if (lx + lw > w - 2) lx = Math.max(x - lw - 6, 1);
  ctx.fillStyle = '#232323';
  ctx.fillRect(lx - 1, 1, lw + 2, 14);
  ctx.fillStyle = '#f0f0f0';
  ctx.fillText(label, lx, 12);
}

function pinLabel(pin, value) {
  return 'Pin ' + String(pin).padStart(2) + ': ' + value.toFixed(3).padStart(7);
}

function render(d) {
  const s = d.sample;

  if (s.power_w != null) {
    document.getElementById('power').textContent = 'Total power: ' + s.power_w.toFixed(1) + ' W';
  } else {
    let sum = 0;
    for (const p of s.pins) sum += p[0] * p[1];
    document.getElementById('power').textContent = 'Total power: ' + sum.toFixed(1) + ' W (estimated)';
  }
  document.getElementById('temp').textContent = s.temp_c != null ? 'GPU temp: ' + s.temp_c.toFixed(0) + ' °C' : '';
  const vramEl = document.getElementById('vram');
  if (s.mem_used_gb != null) {
    vramEl.textContent = s.mem_total_gb != null
      ? 'VRAM: ' + s.mem_used_gb.toFixed(1) + ' / ' + s.mem_total_gb.toFixed(0) + ' GB'
      : 'VRAM: ' + s.mem_used_gb.toFixed(1) + ' GB';
  } else {
    vramEl.textContent = '';
  }

  const limitEl = document.getElementById('limit');
  if (s.limit_w != null) {
    let t = 'Power limit: ' + s.limit_w + ' W';
    if (s.limit_min_w != null && s.limit_max_w != null) {
      t += '    (range ' + s.limit_min_w + '–' + s.limit_max_w + ' W)';
    }
    limitEl.textContent = t;
  } else {
    limitEl.textContent = '';
  }
  document.getElementById('interval').textContent = 'Sample interval: ' + d.poll_ms + ' ms';

  // current limits (from the settings dialog) and matching legend
  if (d.limits) {
    limits = d.limits;
    document.getElementById('legend').innerHTML =
      '<span class="r">● ≥ ' + limits[1] + 'A</span>' +
      '<span class="y">● ' + limits[0] + '–' + limits[1] + 'A</span>' +
      '<span class="g">● < ' + limits[0] + 'A</span>';
  }

  // tables (skip until the first sample with all pins has arrived)
  if (s.pins.length === 6) {
    const volt = document.getElementById('volt');
    const curr = document.getElementById('curr');
    volt.innerHTML = '';
    curr.innerHTML = '';
    for (let row = 0; row < 2; row++) {
      const vr = volt.insertRow();
      const cr = curr.insertRow();
      for (let c = 0; c < 3; c++) {
        const pin = row * 3 + c;
        vr.insertCell().textContent = pinLabel(pin, s.pins[pin][0]);
        const cell = cr.insertCell();
        const cur = s.pins[pin][1];
        cell.textContent = pinLabel(pin, cur);
        cell.className = cur < limits[0] ? 'g' : (cur < limits[1] ? 'y' : 'r');
      }
    }
  }

  const graphsEl = document.getElementById('graphs');
  graphsEl.style.display = d.show_graphs ? '' : 'none';
  if (!d.show_graphs) return;
  const series = [
    d.data.pins[0], d.data.pins[1], d.data.pins[2],
    d.data.pins[3], d.data.pins[4], d.data.pins[5],
    d.data.power, d.data.temp, d.data.mem
  ];
  const fixedMax = [null, null, null, null, null, null, d.range_max.power, d.range_max.temp, d.range_max.mem];
  for (let i = 0; i < series.length; i++) {
    drawGraph(canvases[i], series[i], d.now, d.window, fixedMax[i], i);
  }
}

let fails = 0;
function showErr(msg) {
  const el = document.getElementById('err');
  el.style.display = msg ? 'block' : 'none';
  if (msg) el.textContent = msg;
}
async function tick() {
  try {
    const r = await fetch('/data?window=' + windowMin * 60, { cache: 'no-store' });
    if (!r.ok) throw new Error('HTTP ' + r.status);
    render(await r.json());
    fails = 0;
    showErr('');
  } catch (e) {
    if (++fails >= 5) showErr('Cannot reach the power monitor. Is the web interface enabled?');
  }
}
setInterval(tick, 1000);
tick();
</script>
</body>
</html>
"###;

const HORIZONS_MIN: [u32; 6] = [1, 5, 10, 15, 30, 60];
/// Keep a little more history than the largest horizon
const HISTORY_SECS: f64 = 60.0 * 60.0 + 120.0;

fn current_color(current: f64, s: &Settings) -> egui::Color32 {
    if current < s.yellow_limit_a as f64 {
        egui::Color32::from_rgb(80, 220, 80)
    } else if current < s.red_limit_a as f64 {
        egui::Color32::from_rgb(240, 210, 40)
    } else {
        egui::Color32::from_rgb(240, 60, 60)
    }
}

struct App {
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
                        accept = ui
                            .add_enabled(
                                *draft != self.settings,
                                egui::Button::new(egui::RichText::new("Accept").strong()),
                            )
                            .clicked();
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_data() -> Arc<Mutex<WebData>> {
        let mut d = WebData::new();
        d.sample = Sample {
            pins: vec![
                PinReading {
                    voltage: 12.0,
                    current: 5.0,
                },
                PinReading {
                    voltage: 12.1,
                    current: 6.0,
                },
                PinReading {
                    voltage: 12.2,
                    current: 7.0,
                },
                PinReading {
                    voltage: 12.3,
                    current: 8.0,
                },
                PinReading {
                    voltage: 12.4,
                    current: 9.5,
                },
                PinReading {
                    voltage: 12.5,
                    current: 10.5,
                },
            ],
            power_w: Some(300.5),
            limit_w: Some(575),
            limit_min_w: Some(150),
            limit_max_w: Some(600),
            temp_c: Some(55.0),
            max_temp_c: Some(108.0),
            mem_used_gb: Some(20.5),
            mem_total_gb: Some(32.0),
        };
        d.power_range_max = Some(600.0);
        d.temp_range_max = Some(108.0);
        d.mem_range_max = Some(32.0);
        let now = d.start.elapsed().as_secs_f64();
        for i in 0..NUM_PINS {
            for j in 0..10 {
                d.history[i].push((now - (10 - j) as f64, 5.0 + i as f64));
            }
        }
        for j in 0..10 {
            d.power_history.push((now - (10 - j) as f64, 300.0));
        }
        Arc::new(Mutex::new(d))
    }

    fn free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    }

    fn http_get(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let req = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn web_json_contains_expected_fields() {
        let data = test_data();
        let d = data.lock().unwrap();
        let json = web_json(&d, 60.0);
        assert!(json.contains("\"window\":60"));
        assert!(json.contains("\"power_w\":300.5"));
        assert!(json.contains("\"limit_w\":575"));
        assert!(json.contains("\"temp_c\":55.0"));
        assert!(json.contains("\"mem_total_gb\":32.0"));
        assert!(json.contains("\"show_graphs\":true"));
        assert!(json.contains("\"poll_ms\":1000"));
        assert!(json.contains("\"limits\":[9.2,10.0]"));
        // Two \"pins\" arrays: the latest sample and the history data.
        assert_eq!(json.matches("\"pins\":[").count(), 2);
        // All 10 per-pin history points fit inside the 60 s window. History
        // values use 2 decimals, so pin 0's ten 5.0 A samples read "5.00]"
        // (the sample pins use 3 decimals and cannot collide).
        assert_eq!(json.matches("5.00]").count(), 10);
    }

    #[test]
    fn web_data_record_appends_and_caches_ranges() {
        let mut d = WebData::new();
        let s = Sample {
            pins: vec![
                PinReading {
                    voltage: 12.0,
                    current: 5.0,
                };
                NUM_PINS
            ],
            power_w: None,
            limit_w: Some(575),
            limit_min_w: Some(150),
            limit_max_w: Some(600),
            temp_c: Some(55.0),
            max_temp_c: Some(108.0),
            mem_used_gb: Some(20.5),
            mem_total_gb: Some(32.0),
        };
        d.record(s.clone());
        d.record(s);
        assert_eq!(d.history[0].len(), 2);
        assert_eq!(d.history[NUM_PINS - 1].len(), 2);
        // power_w is None, so the estimated pin sum (6 x 12.0 x 5.0) is used
        assert_eq!(d.power_history.last().map(|(_, v)| *v), Some(360.0));
        assert_eq!(d.temp_history.len(), 2);
        assert_eq!(d.mem_history.len(), 2);
        assert_eq!(d.power_range_max, Some(600.0));
        assert_eq!(d.temp_range_max, Some(108.0));
        assert_eq!(d.mem_range_max, Some(32.0));
        assert_eq!(d.sample.limit_w, Some(575));
    }

    #[test]
    fn settings_defaults_are_valid() {
        let s = Settings::default();
        // The defaults match the thresholds this app shipped with.
        assert_eq!(s.yellow_limit_a, DEFAULT_YELLOW_LIMIT_A);
        assert_eq!(s.red_limit_a, DEFAULT_RED_LIMIT_A);
        assert!(!s.yellow_sound_enabled && !s.red_sound_enabled);
        assert!(s.yellow_limit_a >= MIN_LIMIT_A);
        assert!(s.yellow_limit_a <= s.red_limit_a);
        assert!(s.red_limit_a <= MAX_LIMIT_A);
        // The shipped sounds are found by prefix inside the sound dir.
        let a = default_sound_path("attention");
        let c = default_sound_path("critical");
        assert!(
            a.to_string_lossy().starts_with("sound/attention")
                && a.extension() == Some("wav".as_ref())
        );
        assert!(
            c.to_string_lossy().starts_with("sound/critical")
                && c.extension() == Some("wav".as_ref())
        );
        assert!(a != c);
    }

    #[test]
    fn over_limit_sound_edge_triggers() {
        let mut s = Settings::default();
        s.yellow_sound_enabled = true;
        s.red_sound_enabled = true;
        let (mut ly, mut lr) = (false, false);
        // Below yellow: nothing.
        assert_eq!(over_limit_sound(&mut ly, &mut lr, 5.0, &s), None);
        // Enter yellow: the sound fires once, then stays silent.
        assert_eq!(
            over_limit_sound(&mut ly, &mut lr, 9.5, &s),
            Some(s.yellow_sound_path.clone())
        );
        assert_eq!(over_limit_sound(&mut ly, &mut lr, 9.8, &s), None);
        // Enter red: the red sound fires once (yellow is suppressed).
        assert_eq!(
            over_limit_sound(&mut ly, &mut lr, 10.5, &s),
            Some(s.red_sound_path.clone())
        );
        assert_eq!(over_limit_sound(&mut ly, &mut lr, 11.0, &s), None);
        // Falling back into yellow: no repeat, yellow was already latched.
        assert_eq!(over_limit_sound(&mut ly, &mut lr, 9.5, &s), None);
        // Dropping below yellow resets the latches, so it can fire again.
        assert_eq!(over_limit_sound(&mut ly, &mut lr, 1.0, &s), None);
        assert_eq!(
            over_limit_sound(&mut ly, &mut lr, 9.5, &s),
            Some(s.yellow_sound_path.clone())
        );
        // A disabled sound never fires.
        s.yellow_sound_enabled = false;
        let (mut ly2, mut lr2) = (false, false);
        assert_eq!(over_limit_sound(&mut ly2, &mut lr2, 9.5, &s), None);
    }

    #[test]
    fn query_param_parses() {
        assert_eq!(query_param("window=300&x=1", "window"), Some("300"));
        assert_eq!(query_param("x=1", "window"), None);
        assert_eq!(query_param("", "window"), None);
    }

    #[test]
    fn web_server_serves_and_rejects_busy_port() {
        let port = free_port();
        let data = test_data();

        // A port already taken must be reported as an error, not started.
        let blocker = TcpListener::bind(("127.0.0.1", port)).unwrap();
        assert!(WebServer::start(port, data.clone()).is_err());
        drop(blocker);

        let server = WebServer::start(port, data.clone()).unwrap();
        let page = http_get(port, "/");
        assert!(page.starts_with("HTTP/1.1 200 OK"));
        assert!(page.contains("Power Monitor"));

        let json_resp = http_get(port, "/data?window=60");
        assert!(json_resp.starts_with("HTTP/1.1 200 OK"));
        let body = json_resp.split("\r\n\r\n").nth(1).unwrap();
        assert!(body.contains("\"power_w\":300.5"));

        let not_found = http_get(port, "/nope");
        assert!(not_found.starts_with("HTTP/1.1 404"));

        // Stopping the server must join the thread and free the port.
        drop(server);
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    }
}
