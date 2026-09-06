//! Hardware polling: the ITE IT8915FN power-monitoring chip on the GPU's
//! I2C bus (per-rail voltage and current) plus NVML (total GPU power, power
//! limit, temperature, VRAM).
//!
//! [`spawn_poller`] starts the background poller thread: it reads the pins
//! and NVML at the configured interval, folds each sample into the shared
//! [`WebData`], and drives the limit checking / alert sounds through the
//! [`alerts`] module.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::raw::c_void;
use std::process::{Child, Command, Stdio};
use std::ptr;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::alerts::{auto_power_action, over_limit_sound, AlertState, AutoPowerState, SoundPlayer};
use crate::web::WebData;

// ITE IT8915FN constants
const I2C_SLAVE_ADDR: u16 = 0x2B;
const REG_POWER: u8 = 0x80;
pub(crate) const NUM_PINS: usize = 6;
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
pub(crate) const DEFAULT_POLL_MS: u64 = 1000;

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

/// One poll sample: per-pin I2C readings plus NVML data.
#[derive(Clone, Default)]
pub(crate) struct Sample {
    pub(crate) pins: Vec<PinReading>,
    pub(crate) power_w: Option<f64>, // from NVML, same source as nvidia-smi
    pub(crate) limit_w: Option<u32>, // current power management limit
    pub(crate) limit_min_w: Option<u32>,
    pub(crate) limit_max_w: Option<u32>,
    pub(crate) temp_c: Option<f64>,       // GPU temperature from NVML
    pub(crate) max_temp_c: Option<f64>,   // max supported temperature from NVML
    pub(crate) mem_used_gb: Option<f64>,  // VRAM used (GB) from NVML
    pub(crate) mem_total_gb: Option<f64>, // VRAM total (GB) from NVML
}

/// Commands from the UI to the poller thread.
pub(crate) enum Cmd {
    SetLimit(u32),
    Interval(u64),
    /// Spawn/verify the privileged power helper (prompts for the password).
    GrantPowerAccess,
}

/// Command-line flag that turns this binary into the privileged power-limit
/// helper instead of the app itself (see [`PowerHelper`], [`run_power_helper`]).
pub(crate) const POWER_HELPER_ARG: &str = "--power-helper";

/// State of the privileged power helper as the app sees it. Mirrored into
/// [`WebData::power_auth`](crate::web::WebData::power_auth) by the poller so
/// the GUI can confirm authentication up front (or revert options that need
/// it when authentication fails).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum AuthState {
    /// The helper has never run, or was (re)spawned and has not replied yet.
    #[default]
    Idle,
    /// The helper is running as root: privileged changes can be applied.
    Active,
    /// The last attempt failed (authentication denied, polkit missing or
    /// the helper died); the next request spawns and prompts again.
    Failed,
}

/// Applies GPU power limits with elevated privileges.
///
/// NVML's power-limit setter requires root, while the app runs as a normal
/// user. When a limit change is needed (manually or by the automatic
/// minimum-power feature), the app runs *itself* through polkit's `pkexec`,
/// which prompts for a password via the desktop's polkit agent and execs the
/// helper mode ([`run_power_helper`]) as root. The helper stays alive for the
/// app's lifetime, so the password is asked once; it speaks a one-line
/// protocol over stdin/stdout and nothing else.
pub(crate) struct PowerHelper {
    child: Option<Child>,
    /// Short human-readable status (latest helper result or failure), kept by
    /// the reader thread and mirrored into [`WebData::power_note`] by the
    /// poller.
    status: Arc<Mutex<Option<String>>>,
    /// Authentication state, written by the reader thread (and on spawn
    /// failures) and mirrored into [`WebData::power_auth`] by the poller.
    auth: Arc<Mutex<AuthState>>,
}

impl PowerHelper {
    pub(crate) fn new() -> PowerHelper {
        PowerHelper {
            child: None,
            status: Arc::new(Mutex::new(None)),
            auth: Arc::new(Mutex::new(AuthState::Idle)),
        }
    }

    fn set_status(&self, msg: String) {
        *self.status.lock().unwrap() = Some(msg);
    }

    /// Whether privileged changes can currently be applied (helper replied).
    pub(crate) fn auth_state(&self) -> AuthState {
        *self.auth.lock().unwrap()
    }

    /// Spawn the helper through pkexec unless it is still running. Returns
    /// false when it could not be started (pkexec missing, auth denied).
    fn ensure_running(&mut self) -> bool {
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                // Still running (or between polls): reuse it.
                Ok(None) => return true,
                // Exited (auth denied earlier, crash): respawn on next use.
                _ => self.child = None,
            }
        }

        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                *self.auth.lock().unwrap() = AuthState::Failed;
                self.set_status(format!("Cannot locate own executable: {e}"));
                return false;
            }
        };
        // A fresh attempt: any earlier failure is no longer the last word
        // until this helper replies or dies.
        *self.auth.lock().unwrap() = AuthState::Idle;
        let spawned = Command::new("pkexec")
            .arg(&exe)
            .arg(POWER_HELPER_ARG)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                *self.auth.lock().unwrap() = AuthState::Failed;
                self.set_status(format!(
                    "Could not start pkexec ({e}). Is polkit installed?"
                ));
                return false;
            }
        };

        // Relay helper replies into the status cell; helper diagnostics
        // (pkexec's own errors) go to our stderr.
        if let Some(out) = child.stdout.take() {
            let status = self.status.clone();
            let auth = self.auth.clone();
            thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    // Any reply proves the helper is up as root (an ERR is a
                    // helper/NVML failure, not an authentication one).
                    *auth.lock().unwrap() = AuthState::Active;
                    match line.strip_prefix("ERR ") {
                        Some(msg) => *status.lock().unwrap() = Some(format!("Power helper: {msg}")),
                        None => {
                            *status.lock().unwrap() = Some("Privileged power helper active.".into())
                        }
                    }
                }
                // The helper is gone: without it privileged changes fail.
                *auth.lock().unwrap() = AuthState::Failed;
                *status.lock().unwrap() = Some(
                    "Privileged power helper not running (authentication denied or unavailable)."
                        .into(),
                );
            });
        }
        if let Some(err) = child.stderr.take() {
            thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    eprintln!("power helper: {line}");
                }
            });
        }
        self.child = Some(child);
        true
    }

    /// Send one command to the helper, starting it if needed.
    fn request(&mut self, line: &str) {
        if !self.ensure_running() {
            return;
        }
        let child = self.child.as_mut().unwrap();
        let sent = child
            .stdin
            .as_mut()
            .map(|stdin| writeln!(stdin, "{line}").and_then(|()| stdin.flush()));
        if !matches!(sent, Some(Ok(()))) {
            // The helper died between the wait check and now: drop it so the
            // next request respawns (and re-prompts) it.
            self.child = None;
            self.set_status("Privileged power helper exited.".into());
        }
    }

    /// Set the power management limit to `watts` (asks for the password on
    /// first use). Results are reported through the status cell.
    pub(crate) fn set(&mut self, watts: u32) {
        self.request(&format!("SET {watts}"));
    }

    /// Authenticate without changing anything (lets the user grant access up
    /// front instead of at the first limit excursion).
    pub(crate) fn ping(&mut self) {
        self.request("PING");
    }
}

impl Drop for PowerHelper {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Privileged helper mode (`--power-helper`, normally only started through
/// pkexec by [`PowerHelper`]): read one-line commands from stdin and apply
/// power limits via NVML with the elevated privileges.
///
/// Protocol: `SET <watts>` answers `OK` or `ERR <msg>`, `PING` answers `OK`,
/// `EXIT` ends. Only wattages in a sane range are accepted; the helper never
/// touches I2C, sounds or any configuration.
pub(crate) fn run_power_helper() -> i32 {
    let stdin = io::stdin();
    let mut out = io::stdout();
    let mut nvml: Option<nvml::Nvml> = None;

    for line in stdin.lock().lines().map_while(Result::ok) {
        let line = line.trim().to_owned();
        if line.is_empty() {
            continue;
        }
        let reply = if line == "PING" {
            "OK".to_owned()
        } else if line == "EXIT" {
            break;
        } else if let Some(rest) = line.strip_prefix("SET ") {
            match rest.trim().parse::<u32>() {
                Ok(watts) if (100..=1000).contains(&watts) => {
                    if nvml.is_none() {
                        nvml = nvml::Nvml::new();
                    }
                    match nvml.as_ref().map(|n| n.set_power_limit(watts)) {
                        Some(Ok(())) => format!("Power limit set to {watts} W"),
                        Some(Err(e)) => format!("ERR setting {watts} W failed: {e}"),
                        None => "ERR NVML unavailable (libnvidia-ml.so.1?)".to_owned(),
                    }
                }
                Ok(_) => "ERR watts out of range".to_owned(),
                Err(_) => "ERR watts is not a number".to_owned(),
            }
        } else {
            "ERR unknown command".to_owned()
        };
        if writeln!(out, "{reply}").and_then(|()| out.flush()).is_err() {
            break;
        }
    }
    0
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PinReading {
    pub(crate) voltage: f64,
    pub(crate) current: f64,
}

/// Start the background poller thread. It polls I2C (per-pin) and NVML
/// (total power), records each sample straight into the shared data (so
/// monitoring keeps working while the window is minimized and the web
/// server never depends on the GUI drawing frames), and plays alert sounds
/// when the current limits are exceeded.
pub(crate) fn spawn_poller(
    bus_number: u32,
    data: Arc<Mutex<WebData>>,
    player: SoundPlayer,
) -> mpsc::Sender<Cmd> {
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
        let mut alert_state = AlertState::default();
        let mut power_helper = PowerHelper::new();
        let mut auto_power = AutoPowerState::default();

        loop {
            // Handle UI commands
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    Cmd::SetLimit(watts) => {
                        // A manual set takes precedence over the automatic
                        // lowering: never restore over it later.
                        auto_power.overridden();
                        match nvml.as_ref().map(|n| n.set_power_limit(watts)) {
                            Some(Ok(())) => println!("Power limit set to {watts} W"),
                            // Running unprivileged: NVML rejects the setter,
                            // so route it through the pkexec helper (asks for
                            // the password on first use).
                            Some(Err(e)) if e.is_permission() => {
                                println!(
                                    "Setting the power limit needs root; using the pkexec helper."
                                );
                                power_helper.set(watts);
                            }
                            Some(Err(e)) => {
                                eprintln!("Failed to set power limit to {watts} W: {e}")
                            }
                            None => eprintln!("NVML unavailable: cannot set the power limit."),
                        }
                    }
                    Cmd::GrantPowerAccess => power_helper.ping(),
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

            // Limit checking runs here, on the poller's cadence: the newest
            // settings are mirrored into the shared data by the UI.
            let max_current = sample
                .pins
                .iter()
                .map(|p| p.current)
                .fold(f64::NEG_INFINITY, f64::max);
            // Mirror the helper's status so the GUI can show what happened
            // with privileged limit changes (and revert auto power if the
            // authentication it needs was not granted).
            let power_note = power_helper.status.lock().unwrap().clone();
            let power_auth = power_helper.auth_state();
            let settings = {
                let mut d = data.lock().unwrap();
                let settings = d.settings.clone();
                d.record(sample);
                d.power_note = power_note;
                d.power_auth = power_auth;
                settings
            };
            if let Some(sound) = over_limit_sound(&mut alert_state, max_current, &settings) {
                player.play(sound);
            }
            // Automatic minimum power limit (see alerts::auto_power_action);
            // applying it goes through the privileged helper.
            if let Some(watts) = auto_power_action(
                &mut auto_power,
                max_current,
                limit_w,
                limit_min_w,
                &settings,
            ) {
                println!("Automatic power limit: applying {watts} W");
                power_helper.set(watts);
            }
            thread::sleep(Duration::from_millis(poll_ms));
        }
    });
    cmd_tx
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
pub(crate) fn find_i2c_bus() -> Option<u32> {
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
    const NVML_ERROR_INVALID_ARGUMENT: u32 = 2;
    const NVML_ERROR_NOT_SUPPORTED: u32 = 3;
    const NVML_ERROR_NO_PERMISSION: u32 = 4;
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

    impl Error {
        /// Whether the call was rejected for lack of privileges (the setter
        /// needs root; fall back to the pkexec helper in that case).
        pub fn is_permission(&self) -> bool {
            self.0 == NVML_ERROR_NO_PERMISSION
        }
    }

    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.0 {
                NVML_ERROR_INVALID_ARGUMENT => {
                    write!(f, "invalid argument (outside the allowed range?)")
                }
                NVML_ERROR_NOT_SUPPORTED => write!(f, "not supported by the driver"),
                NVML_ERROR_NO_PERMISSION => write!(f, "insufficient permissions (run as root)"),
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

        /// Set the power management limit (given in watts). Like the getters,
        /// the v1 set function expects milliwatts on current drivers, so
        /// convert first: sending plain watts gets rejected with
        /// NVML_ERROR_INVALID_ARGUMENT (error 2).
        pub fn set_power_limit(&self, watts: u32) -> Result<(), Error> {
            let code = unsafe { (self.set_limit)(self.device, watts.saturating_mul(1000)) };
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
