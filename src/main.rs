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

    // Background thread polls I2C (per-pin) and NVML (total power).
    let (tx, rx) = mpsc::channel::<Sample>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    thread::spawn(move || {
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
            if tx.send(sample).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(poll_ms));
        }
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 860.0])
            .with_resizable(true),
        ..Default::default()
    };

    // Pre-reserve history for the default 1 Hz sample rate so the common
    // case never reallocates; a faster slider setting just grows the vecs.
    let mut history: Vec<Vec<(f64, f64)>> = vec![Vec::new(); NUM_PINS];
    let mut power_history: Vec<(f64, f64)> = Vec::new();
    let mut temp_history: Vec<(f64, f64)> = Vec::new();
    let mut mem_history: Vec<(f64, f64)> = Vec::new();
    let reserve = (HISTORY_SECS * 1000.0 / DEFAULT_POLL_MS as f64) as usize + 1024;
    for h in &mut history {
        h.reserve(reserve);
    }
    power_history.reserve(reserve);
    temp_history.reserve(reserve);
    mem_history.reserve(reserve);

    let app = App {
        rx,
        cmd_tx,
        sample: Sample::default(),
        pending_limit: None,
        poll_ms: DEFAULT_POLL_MS,
        history,
        power_history,
        temp_history,
        mem_history,
        power_range_max: None,
        temp_range_max: None,
        mem_range_max: None,
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
    cmd_tx: mpsc::Sender<Cmd>,
    sample: Sample,
    pending_limit: Option<u32>,
    /// Sample interval in ms (sent to the poller when the slider changes).
    poll_ms: u64,
    /// Per-pin history of (elapsed seconds, current A), oldest first.
    history: Vec<Vec<(f64, f64)>>,
    /// GPU metric histories of (elapsed seconds, value), oldest first.
    power_history: Vec<(f64, f64)>, // W
    temp_history: Vec<(f64, f64)>, // °C
    mem_history: Vec<(f64, f64)>,  // VRAM used, GB
    /// Permanent y ranges (max value) for the GPU metric graphs, learned
    /// from NVML: max power limit (W), max supported temp (°C), total VRAM
    /// (GB).
    power_range_max: Option<f64>,
    temp_range_max: Option<f64>,
    mem_range_max: Option<f64>,
    start: std::time::Instant,
    /// Index into HORIZONS_MIN for the visible time window.
    window_idx: u8,
    /// Toggle for the per-pin graphs (history keeps accumulating either way).
    show_graphs: bool,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = self.start.elapsed().as_secs_f64();

        // Append new samples to the history and prune what's expired.
        let mut got_sample = false;
        while let Ok(sample) = self.rx.try_recv() {
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
            self.sample = sample;
            got_sample = true;
        }
        if got_sample {
            let t_min = now - HISTORY_SECS;
            for hist in &mut self.history {
                let k = hist.partition_point(|(t, _)| *t < t_min);
                hist.drain(..k);
            }
            for hist in [
                &mut self.power_history,
                &mut self.temp_history,
                &mut self.mem_history,
            ] {
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

            // GPU temperature and VRAM (NVML)
            let mut gpu_texts: Vec<String> = Vec::new();
            if let Some(t) = self.sample.temp_c {
                gpu_texts.push(format!("GPU temp: {:.0} °C", t));
            }
            if let Some(used) = self.sample.mem_used_gb {
                let text = match self.sample.mem_total_gb {
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
                                    &self.history[pin],
                                    now,
                                    window_secs,
                                    graph_size,
                                );
                            } else {
                                match col {
                                    0 => add_metric_graph(
                                        ui,
                                        "Power (W)",
                                        &self.power_history,
                                        now,
                                        window_secs,
                                        graph_size,
                                        egui::Color32::from_rgb(255, 165, 0),
                                        "W",
                                        self.power_range_max,
                                    ),
                                    1 => add_metric_graph(
                                        ui,
                                        "Temp (°C)",
                                        &self.temp_history,
                                        now,
                                        window_secs,
                                        graph_size,
                                        egui::Color32::from_rgb(255, 90, 70),
                                        "°C",
                                        self.temp_range_max,
                                    ),
                                    _ => add_metric_graph(
                                        ui,
                                        "VRAM (GB)",
                                        &self.mem_history,
                                        now,
                                        window_secs,
                                        graph_size,
                                        egui::Color32::from_rgb(140, 190, 255),
                                        "GB",
                                        self.mem_range_max,
                                    ),
                                }
                            }
                        }
                    });
                }
            }
        });

        // Redraw about as fast as the data arrives when idle.
        // Input events (slider drags, checkbox, window moves) still trigger
        // immediate repaints, so the UI stays fully responsive.
        ctx.request_repaint_after(Duration::from_millis(self.poll_ms));
    }
}

/// Draw one small graph of a pin's current over the visible time window.
/// The line is colored per segment by the current thresholds, and the
/// 9.2 A / 10 A limits are drawn as horizontal reference lines.
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

        // Limit reference lines with numeric labels. Muted and dashed so
        // they recede behind the current line instead of competing with it.
        let small = egui::FontId::monospace(9.0);
        let dim = ui.visuals().weak_text_color();
        for (limit, color) in [
            (9.2, egui::Color32::from_rgb(165, 150, 75)),
            (10.0, egui::Color32::from_rgb(175, 85, 85)),
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
                format!("{limit}"),
                small.clone(),
                dim,
            );
        }

        // Top scale label: only when the range has grown beyond the base
        // 11 A; the limit lines otherwise define the scale
        if y_max > 11.0 {
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
            |v| current_color(v),
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
