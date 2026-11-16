//! Hardware polling: the ITE IT8915FN power-monitoring chip on the GPU's
//! I2C bus (per-rail voltage and current) plus NVML (total GPU power, power
//! limit, temperature, VRAM).
//!
//! [`spawn_poller`] starts the background poller thread: it reads the pins
//! and NVML at the configured interval, folds each sample into the shared
//! [`WebData`], and drives the limit checking / alert sounds through the
//! [`alerts`] module.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::raw::c_void;
use std::process::{Child, Command, Stdio};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::alerts::{
    auto_power_action, critical_alert_edge, kill_guard_action, over_limit_sound,
    sensor_fault_sound, AlertState, AutoPowerState, CriticalAlertState, KillGuardState,
    SensorFaultState, Sound, SoundPlayer,
};
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

// Plausibility bounds for every polled pin reading. Beyond them the reading
// is treated as a sensor fault, never as a real current: a glitched or torn
// transfer (an all-ones register reads 65.535) must not trigger the red
// alert, the automatic power limit or the kill safeguard. Both sit far above
// anything a 12 V-2x6 pin can carry for long (a melting pin has been measured
// at ~20-25 A); the ceiling is not a lower threshold for real faults.
const MAX_PLAUSIBLE_CURRENT_A: f64 = 50.0;
const MAX_PLAUSIBLE_VOLTAGE_V: f64 = 20.0;

// Linux i2c-dev ioctl constants
const I2C_SLAVE: libc::c_ulong = 0x0703;
const I2C_SMBUS: libc::c_ulong = 0x0720;
const I2C_SMBUS_READ: u8 = 1;
const I2C_SMBUS_BYTE_DATA: u32 = 2;
const I2C_SMBUS_I2C_BLOCK_DATA: u32 = 8;
const I2C_RETRIES: libc::c_ulong = 0x0701;

/// Default poll interval in ms (adjustable in the UI, 100 ms..=10 s).
pub(crate) const DEFAULT_POLL_MS: u64 = 1000;

/// How many top GPU-using processes to keep per sample (main window list,
/// web page, and the critical-alert kill safeguard's victim).
pub(crate) const TOP_PROCESSES: usize = 3;

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
    /// Either [`NUM_PINS`] readings (index 0 = pin 0) or empty when there is
    /// no valid reading: before the first poll, or while the IT8915 cannot
    /// be read (see `WebData::sensor_error`). Never zeros standing in for a
    /// missing reading — those would look like a healthy idle card.
    pub(crate) pins: Vec<PinReading>,
    pub(crate) power_w: Option<f64>, // from NVML, same source as nvidia-smi
    pub(crate) limit_w: Option<u32>, // current power management limit
    pub(crate) limit_min_w: Option<u32>,
    pub(crate) limit_max_w: Option<u32>,
    pub(crate) temp_c: Option<f64>,       // GPU temperature from NVML
    pub(crate) max_temp_c: Option<f64>,   // max supported temperature from NVML
    pub(crate) mem_used_gb: Option<f64>,  // VRAM used (GB) from NVML
    pub(crate) mem_total_gb: Option<f64>, // VRAM total (GB) from NVML
    /// Top GPU-using processes (NVML), highest first; empty when NVML is
    /// unavailable or nothing uses the GPU. See [`TOP_PROCESSES`].
    pub(crate) processes: Vec<GpuProcess>,
}

/// One process using the GPU, as reported by NVML.
#[derive(Clone, Debug, Default)]
pub(crate) struct GpuProcess {
    pub(crate) pid: u32,
    /// Process name (from `/proc/<pid>/comm`), `"pid N"` as a fallback.
    pub(crate) name: String,
    /// GPU memory used, in bytes (0 when the driver does not report it).
    pub(crate) mem_bytes: u64,
    /// SM utilization percent over the last poll interval (0 when the
    /// driver reported no utilization sample for this process).
    pub(crate) util_percent: u32,
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

/// The outcome of a privileged kill, relayed by the helper's reader thread
/// and consumed by the poller on its next poll.
pub(crate) struct KillOutcome {
    pub(crate) pid: u32,
    pub(crate) success: bool,
    /// The helper's error text when the kill failed (empty on success).
    pub(crate) message: String,
}

/// Applies GPU power limits — and, when the safeguard asks for it, SIGKILLs
/// processes — with elevated privileges.
///
/// NVML's power-limit setter (and killing other users' processes) requires
/// root, while the app runs as a normal user. When a privileged change is
/// needed (a limit set manually or by the automatic minimum-power feature,
/// or the cross-user kill of the critical-alert safeguard), the app runs
/// *itself* through polkit's `pkexec`, which prompts for a password via the
/// desktop's polkit agent and execs the helper mode ([`run_power_helper`])
/// as root. The helper stays alive for the app's lifetime, so the password
/// is asked once and serves both purposes; it speaks a one-line protocol
/// over stdin/stdout and nothing else.
pub(crate) struct PowerHelper {
    child: Option<Child>,
    /// Short human-readable status (latest helper result or failure), kept by
    /// the reader thread and mirrored into [`WebData::power_note`] by the
    /// poller.
    status: Arc<Mutex<Option<String>>>,
    /// Authentication state, written by the reader thread (and on spawn
    /// failures) and mirrored into [`WebData::power_auth`] by the poller.
    auth: Arc<Mutex<AuthState>>,
    /// Outcome of the last `KILL` command, set by the reader thread and
    /// consumed by the poller (`None` while no reply is outstanding).
    kill_outcome: Arc<Mutex<Option<KillOutcome>>>,
}

impl PowerHelper {
    pub(crate) fn new() -> PowerHelper {
        PowerHelper {
            child: None,
            status: Arc::new(Mutex::new(None)),
            auth: Arc::new(Mutex::new(AuthState::Idle)),
            kill_outcome: Arc::new(Mutex::new(None)),
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
        // This is the single place an authentication prompt can appear, so
        // announce here (in the user's own UI, not only in polkit's dialog)
        // what the root credentials are for: the GPU power limiting.
        self.set_status(
            "Administrator authentication for the GPU power limiting and the privileged \
process kill requested — please confirm in the authentication dialog."
                .into(),
        );
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
            let kill_outcome = self.kill_outcome.clone();
            thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    // Any reply proves the helper is up as root (an ERR is a
                    // helper/NVML failure, not an authentication one).
                    *auth.lock().unwrap() = AuthState::Active;
                    if line.starts_with("KILL ") {
                        // A safeguard-kill outcome: consumed by the poller,
                        // not part of the limit-centric status note.
                        *kill_outcome.lock().unwrap() = Some(parse_kill_line(&line));
                        continue;
                    }
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
                    "Privileged power helper not running (the administrator authentication for \
                     the GPU power limiting was denied or unavailable.)"
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

    /// Send one command to the helper, starting it if needed. Returns
    /// whether the command reached the helper (it died in the meantime if
    /// not; the next request respawns and re-prompts it).
    fn request(&mut self, line: &str) -> bool {
        if !self.ensure_running() {
            return false;
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
            false
        } else {
            true
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

    /// SIGKILL `pid` through the helper (asks for the password on first
    /// use). Returns whether the command was sent; the outcome arrives via
    /// [`PowerHelper::take_kill_outcome`], usually on the next poll.
    pub(crate) fn kill(&mut self, pid: u32) -> bool {
        self.request(&format!("KILL {pid}"))
    }

    /// Consume the helper's reply to the last `KILL`, if it answered yet.
    pub(crate) fn take_kill_outcome(&self) -> Option<KillOutcome> {
        self.kill_outcome.lock().unwrap().take()
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
/// power limits via NVML — or SIGKILL a process, for the cross-user kill of
/// the critical-alert safeguard — with the elevated privileges.
///
/// Protocol: `SET <watts>` answers `OK` or `ERR <msg>`, `PING` answers
/// `OK`, `KILL <pid>` answers `KILL OK <pid>` or `KILL ERR <pid> <msg>`,
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
        } else if let Some(rest) = line.strip_prefix("KILL ") {
            match rest.trim().parse::<u32>() {
                Ok(pid) if pid != 0 => match kill_process(pid) {
                    Ok(()) => format!("KILL OK {pid}"),
                    Err(e) => format!("KILL ERR {pid} {e}"),
                },
                Ok(_) => "KILL ERR 0 pid must be at least 1".to_owned(),
                Err(_) => "KILL ERR 0 pid is not a number".to_owned(),
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
///
/// `bus_number` is `None` when no IT8915 bus was given or found: the app
/// then runs without pin readings and reports the sensor as unavailable.
pub(crate) fn spawn_poller(
    bus_number: Option<u32>,
    data: Arc<Mutex<WebData>>,
    player: SoundPlayer,
) -> mpsc::Sender<Cmd> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    thread::spawn(move || {
        // I2C is best-effort: if the device is unavailable, the app keeps
        // running without pin readings, and the reason is shown as a sensor
        // fault in both UIs (never as zero, i.e. green, currents).
        let i2c = open_i2c(bus_number);
        if let Err(reason) = &i2c {
            eprintln!("{reason}. Pin currents are NOT monitored.");
        }
        // Whether the previous poll had no valid pin readings (log only the
        // transitions, not every failing poll).
        let mut sensor_failing = false;
        let mut sensor_fault = SensorFaultState::default();

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
        let mut kill_guard = KillGuardState::default();
        // A cross-user kill dispatched through the helper, awaiting its
        // reply (resolved on the next poll); and the alert sound the current
        // excursion earned, held back until that reply is known (a
        // termination replaces it with the critical-terminating sound).
        let mut pending_kill: Option<GpuProcess> = None;
        let mut held_sound: Option<Sound> = None;
        let mut critical_state = CriticalAlertState::default();

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

            // Top GPU-using processes (NVML): drives the main window list,
            // the web page and the critical-alert kill safeguard's target.
            let processes = nvml
                .as_ref()
                .map_or_else(Vec::new, |n| n.processes(TOP_PROCESSES));

            // Pin readings, or why there are none this poll.
            let pins: Result<Vec<PinReading>, String> = match &i2c {
                Ok(f) => {
                    let t = std::time::Instant::now();
                    let read = read_pins(f);
                    if read.is_ok() {
                        let el = t.elapsed().as_millis() as u64;
                        polls += 1;
                        poll_sum_ms += el;
                        poll_max_ms = poll_max_ms.max(el);
                        if polls == 50 || polls.is_multiple_of(500) {
                            eprintln!(
                                "I2C read batch: avg {} ms per poll, worst {} ms (polls 1..{})",
                                poll_sum_ms / polls,
                                poll_max_ms,
                                polls
                            );
                        }
                    }
                    read.map_err(|e| format!("I2C read failed: {e}"))
                        .and_then(|pins| check_plausible(&pins).map(|()| pins))
                }
                Err(reason) => Err(reason.clone()),
            };
            match (&pins, sensor_failing) {
                (Err(e), false) if i2c.is_ok() => {
                    eprintln!("{e}. Pin currents are NOT monitored until reads succeed again.")
                }
                (Ok(_), true) => eprintln!("I2C reads recovered."),
                _ => {}
            }
            sensor_failing = pins.is_err();
            let (pins, sensor_error) = match pins {
                Ok(pins) => (pins, None),
                Err(e) => (Vec::new(), Some(e)),
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
                processes,
            };
            // The kill safeguard's victim is always the top-ranked process of
            // the sample just recorded; grab it before `record` consumes it.
            let top_process = sample.processes.first().cloned();

            // Limit checking runs here, on the poller's cadence: the newest
            // settings are mirrored into the shared data by the UI. Without
            // pin readings there is no current to judge: the alert, kill and
            // automatic-power state machines are skipped (frozen) rather
            // than fed a made-up value, and the sensor fault alerts instead.
            let max_current = (!sample.pins.is_empty()).then(|| {
                sample
                    .pins
                    .iter()
                    .map(|p| p.current)
                    .fold(f64::NEG_INFINITY, f64::max)
            });
            // Mirror the helper's status so the GUI can show what happened
            // with privileged limit changes (and revert auto power if the
            // authentication it needs was not granted).
            let power_note = power_helper.status.lock().unwrap().clone();
            let power_auth = power_helper.auth_state();
            let settings = {
                let mut d = data.lock().unwrap();
                let settings = d.settings.clone();
                d.record(sample);
                d.sensor_error = sensor_error;
                d.power_note = power_note;
                d.power_auth = power_auth;
                settings
            };
            // Limit checking decides which alert sound this sample earns, but
            // it is only played below: a termination on this same red edge
            // replaces the critical alert (see the priority sound below).
            let alert_sound = match max_current {
                Some(current) => over_limit_sound(&mut alert_state, current, &settings),
                None => None,
            };
            // A sensor that stays unreadable alerts once (see
            // alerts::sensor_fault_sound); readings coming back re-arm it.
            let alert_sound = alert_sound.or(sensor_fault_sound(
                &mut sensor_fault,
                max_current.is_some(),
                &settings,
            ));

            // The red rising edge also arms the latching critical-current
            // notification (flashing banner in both UIs). It only ever gets
            // cleared by a user acknowledgment, never by the current dropping.
            if max_current
                .is_some_and(|current| critical_alert_edge(&mut critical_state, current, &settings))
            {
                data.lock().unwrap().critical_alert = true;
            }

            // Critical-alert safeguard (opt-in, see alerts::kill_guard_action):
            // on the rising edge into the red range — and then repeating while
            // the current stays red, at most once per cooldown and never over
            // a still-playing kill sound — SIGKILL the top GPU-using process.
            // By default the kill is unprivileged (same-user processes only;
            // others answer EPERM, which bounds the blast radius); with
            // `Settings::kill_other_users_processes` it goes through the
            // privileged helper instead, so its outcome arrives on the next
            // poll and the alert sound this excursion earned waits for it.
            // The outcome goes to stderr and (freshly) into the desktop UI.
            let mut terminated = false;
            let mut kill_note: Option<String> = None;
            if let Some(p) = pending_kill.take() {
                // A cross-user kill was dispatched on an earlier poll:
                // resolve it before anything else kill-related.
                match power_helper.take_kill_outcome() {
                    Some(o) if o.pid == p.pid => {
                        terminated = o.success;
                        kill_note = Some(if o.success {
                            println!(
                                "Critical-alert safeguard: killed {} (pid {})",
                                p.name, p.pid
                            );
                            format!(
                                "Critical-alert safeguard: killed {} (pid {}).",
                                p.name, p.pid
                            )
                        } else {
                            eprintln!(
                                "Critical-alert safeguard: cannot kill {} (pid {}): {}",
                                p.name, p.pid, o.message
                            );
                            format!(
                                "Critical-alert safeguard: could not kill {} (pid {}): {}.",
                                p.name, p.pid, o.message
                            )
                        });
                    }
                    Some(_) => {
                        // Stray reply for an already-resolved kill: drop it
                        // and keep waiting for the pending one's.
                        pending_kill = Some(p);
                    }
                    None if power_helper.auth_state() == AuthState::Failed => {
                        // The helper is gone and no reply will come.
                        eprintln!(
                            "Critical-alert safeguard: cannot kill {} (pid {}): privileged helper not running.",
                            p.name, p.pid
                        );
                        kill_note = Some(format!(
                            "Critical-alert safeguard: could not kill {} (pid {}): privileged helper not running.",
                            p.name, p.pid
                        ));
                    }
                    None => pending_kill = Some(p), // reply still missing
                }
            } else if max_current.is_some_and(|current| {
                kill_guard_action(
                    &mut kill_guard,
                    current,
                    &settings,
                    player.priority_sound_playing(),
                )
            }) {
                kill_guard.note_kill_attempt();
                match top_process {
                    Some(p) => {
                        if settings.kill_other_users_processes {
                            if power_helper.kill(p.pid) {
                                // Outcome + sound follow the helper's reply.
                                pending_kill = Some(p);
                            } else {
                                eprintln!(
                                    "Critical-alert safeguard: cannot kill {} (pid {}): privileged helper not running.",
                                    p.name, p.pid
                                );
                                kill_note = Some(format!(
                                    "Critical-alert safeguard: could not kill {} (pid {}): privileged helper not running.",
                                    p.name, p.pid
                                ));
                            }
                        } else {
                            match kill_process(p.pid) {
                                Ok(()) => {
                                    terminated = true;
                                    println!(
                                        "Critical-alert safeguard: killed {} (pid {})",
                                        p.name, p.pid
                                    );
                                    kill_note = Some(format!(
                                        "Critical-alert safeguard: killed {} (pid {}).",
                                        p.name, p.pid
                                    ));
                                }
                                Err(e) => {
                                    eprintln!(
                                        "Critical-alert safeguard: cannot kill {} (pid {}): {e}",
                                        p.name, p.pid
                                    );
                                    kill_note = Some(format!(
                                        "Critical-alert safeguard: could not kill {} (pid {}): {e}",
                                        p.name, p.pid
                                    ));
                                }
                            }
                        }
                    }
                    None => println!("Critical-alert safeguard: no GPU process to kill."),
                }
            }
            if let Some(note) = kill_note {
                let mut d = data.lock().unwrap();
                let now = d.start.elapsed().as_secs_f64();
                d.kill_note = Some((now, note));
            }

            // Sounds: an actual termination plays the critical-terminating
            // sound as a priority request (nothing queues ahead of it and
            // nothing interrupts it once playing); a failed kill leaves the
            // regular red alert sound, which is what this excursion earned.
            // While a cross-user kill awaits its reply the alert sound is
            // held back: a termination replaces it, a failure plays it now.
            if terminated {
                player.play_priority(settings.terminating_sound.clone());
                held_sound = None;
            } else if pending_kill.is_some() {
                if let Some(sound) = alert_sound {
                    held_sound = Some(sound);
                }
            } else if let Some(sound) = held_sound.take() {
                player.play(sound);
            } else if let Some(sound) = alert_sound {
                player.play(sound);
            }

            // Automatic minimum power limit (see alerts::auto_power_action);
            // applying it goes through the privileged helper (kept last so a
            // first-time pkexec spawn can never delay the sounds above).
            if let Some(watts) = max_current.and_then(|current| {
                auto_power_action(&mut auto_power, current, limit_w, limit_min_w, &settings)
            }) {
                println!("Automatic power limit: applying {watts} W");
                power_helper.set(watts);
            }
            thread::sleep(Duration::from_millis(poll_ms));
        }
    });
    cmd_tx
}

/// Reject physically impossible pin readings (see
/// [`MAX_PLAUSIBLE_CURRENT_A`]): `Err` names the first offending pin and is
/// reported as a sensor fault instead of being judged against the limits.
fn check_plausible(pins: &[PinReading]) -> Result<(), String> {
    for (i, p) in pins.iter().enumerate() {
        let ok = (0.0..=MAX_PLAUSIBLE_CURRENT_A).contains(&p.current)
            && (0.0..=MAX_PLAUSIBLE_VOLTAGE_V).contains(&p.voltage);
        if !ok {
            return Err(format!(
                "Implausible sensor reading (pin {i}: {:.3} V, {:.3} A)",
                p.voltage, p.current
            ));
        }
    }
    Ok(())
}

/// Open the IT8915's I2C bus and address the chip. `Err` carries a
/// user-facing reason, shown as the sensor fault in both UIs.
fn open_i2c(bus_number: Option<u32>) -> Result<File, String> {
    let Some(bus) = bus_number else {
        return Err(
            "No IT8915 I2C bus found (auto-detect failed; pass the bus number, \
                    e.g. astral-gauge 15)"
                .to_owned(),
        );
    };
    let i2c_path = format!("/dev/i2c-{bus}");
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&i2c_path)
        .map_err(|e| {
            format!("Cannot open {i2c_path}: {e} (run as root or add your user to the i2c group)")
        })?;
    if unsafe { libc::ioctl(f.as_raw_fd(), I2C_SLAVE, I2C_SLAVE_ADDR as libc::c_uint) } < 0 {
        return Err(format!(
            "Cannot address the IT8915 on {i2c_path}: {}",
            io::Error::last_os_error()
        ));
    }
    // Fail fast on a bad transfer instead of letting the kernel retry
    // (retries multiply worst-case latency).
    let _ = unsafe { libc::ioctl(f.as_raw_fd(), I2C_RETRIES, 0usize as libc::c_ulong) };
    Ok(f)
}

/// SIGKILL a process (the critical-alert safeguard). Called unprivileged by
/// the poller by default: processes owned by other users answer with EPERM
/// (the app runs as the user, which is exactly the right blast radius for a
/// safeguard); called as root by the privileged helper when
/// `Settings::kill_other_users_processes` routes the kill through it.
fn kill_process(pid: u32) -> io::Result<()> {
    let target = single_process_pid(pid).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to signal pid {pid}: not a single process id"),
        )
    })?;
    if unsafe { libc::kill(target, libc::SIGKILL) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `pid` as a `kill()` target that addresses exactly one process, or `None`.
/// `kill()` gives 0 and negative values group meanings — 0 is the caller's
/// process group, -1 is *every process it may signal* (as root: the whole
/// system), -N is process group N — so a bogus PID from NVML, which a plain
/// `as` cast would wrap negative (u32::MAX → -1), must never get there.
fn single_process_pid(pid: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(pid).ok().filter(|&p| p > 0)
}

/// Parse the helper's reply to a `KILL` command: `KILL OK <pid>` or
/// `KILL ERR <pid> <msg>` (a malformed line reads as a failed kill).
fn parse_kill_line(line: &str) -> KillOutcome {
    let (rest, success) = match line.strip_prefix("KILL OK ") {
        Some(rest) => (rest, true),
        None => match line.strip_prefix("KILL ERR ") {
            Some(rest) => (rest, false),
            None => {
                return KillOutcome {
                    pid: 0,
                    success: false,
                    message: line.to_owned(),
                }
            }
        },
    };
    let (pid_text, message) = rest.split_once(' ').unwrap_or((rest, ""));
    KillOutcome {
        pid: pid_text.parse().unwrap_or(0),
        success,
        message: message.to_owned(),
    }
}

/// Rank GPU processes for the display list and the kill safeguard: highest
/// SM utilization first, ties broken by VRAM usage, then by pid (stable
/// order). Drops pid 0 (NVML placeholders) and the app's own pid: an eframe
/// window holds a small GPU context and must never be the safeguard's victim.
fn rank_processes(mut procs: Vec<GpuProcess>, top: usize) -> Vec<GpuProcess> {
    let own = std::process::id();
    procs.retain(|p| p.pid != 0 && p.pid != own);
    procs.sort_by(|a, b| {
        b.util_percent
            .cmp(&a.util_percent)
            .then(b.mem_bytes.cmp(&a.mem_bytes))
            .then(a.pid.cmp(&b.pid))
    });
    procs.truncate(top);
    // Resolve names only for what is displayed.
    for p in &mut procs {
        p.name = process_name(p.pid);
    }
    procs
}

/// Human-readable process name: `/proc/<pid>/comm` (the executable's name),
/// falling back to `"pid N"` when the process is gone or unreadable.
fn process_name(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("pid {pid}"))
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

/// Read all 6 pins. Returns readings in pin order (index 0 = pin 0).
/// Register layout: rail `r` lives at offset `r * 4` and maps to pin
/// `NUM_PINS - 1 - r` (rail 0 = pin 5, rail 5 = pin 0 — reversed order).
fn read_pins(file: &File) -> io::Result<Vec<PinReading>> {
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
        for (offset, byte) in raw.iter_mut().enumerate() {
            *byte = smbus_read_byte(fd, REG_POWER + offset as u8)?;
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
    Ok(pins)
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
    candidates.into_iter().find(|&bus| probe_it8915(bus))
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
    /// Process/utilization lists: the provided buffer is too small, the
    /// required entry count is written back (the documented sizing protocol).
    const NVML_ERROR_INSUFFICIENT_SIZE: u32 = 7;
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
    type FnGetRunningProcesses = unsafe extern "C" fn(
        device: *mut c_void,
        count: *mut u32,
        processes: *mut NvmlProcessInfo,
    ) -> u32;
    type FnGetProcessUtilization = unsafe extern "C" fn(
        device: *mut c_void,
        samples: *mut NvmlProcessUtilizationSample,
        count: *mut u32,
        last_seen_timestamp: u64,
    ) -> u32;

    /// Matches NVML's `nvml_memory_t` (all fields in bytes). Field order
    /// matters: it must stay in sync with the C ABI (total, free, used).
    #[repr(C)]
    pub struct NvmlMemory {
        pub total: u64,
        pub free: u64,
        pub used: u64,
    }

    /// Matches NVML's `nvmlProcessInfo_v2_t` (which the `_v3` list getters
    /// also use; the compiler inserts 4 bytes of padding before the u64).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct NvmlProcessInfo {
        pid: u32,
        used_gpu_memory: u64,
        gpu_instance_id: u32,
        compute_instance_id: u32,
    }

    /// Matches NVML's `nvmlProcessUtilizationSample_t`: SM/memory/encoder
    /// utilization per process, averaged over the interval covered by the
    /// sample (`timeStamp` in microseconds).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct NvmlProcessUtilizationSample {
        pid: u32,
        time_stamp: u64,
        sm_util: u32,
        mem_util: u32,
        enc_util: u32,
        dec_util: u32,
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
        /// Process monitoring (optional symbols; feature degrades to an
        /// empty list when the driver does not export them).
        get_compute_procs: Option<FnGetRunningProcesses>,
        get_graphics_procs: Option<FnGetRunningProcesses>,
        get_process_util: Option<FnGetProcessUtilization>,
        /// Newest utilization sample timestamp consumed by
        /// [`Nvml::process_utilization`], so each poll reads only new samples.
        last_util_ts: AtomicU64,
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

    /// Like [`load_fn!`], but a missing symbol is `None` instead of a hard
    /// failure: optional features degrade instead of killing NVML entirely.
    macro_rules! load_opt_fn {
        ($lib:expr, $ty:ty, $name:expr) => {{
            let c_name = CString::new($name).ok()?;
            let sym = unsafe { libc::dlsym($lib, c_name.as_ptr()) };
            if sym.is_null() {
                None
            } else {
                Some(unsafe { std::mem::transmute::<*mut c_void, $ty>(sym) })
            }
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
            let get_compute_procs = load_opt_fn!(
                lib,
                FnGetRunningProcesses,
                "nvmlDeviceGetComputeRunningProcesses_v3"
            );
            let get_graphics_procs = load_opt_fn!(
                lib,
                FnGetRunningProcesses,
                "nvmlDeviceGetGraphicsRunningProcesses_v3"
            );
            let get_process_util = load_opt_fn!(
                lib,
                FnGetProcessUtilization,
                "nvmlDeviceGetProcessUtilization"
            );

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
                get_compute_procs,
                get_graphics_procs,
                get_process_util,
                last_util_ts: AtomicU64::new(0),
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

        /// The top GPU-using processes, ranked by utilization then VRAM use
        /// (see [`super::rank_processes`]). Empty when nothing uses the GPU
        /// or the driver does not export the process queries.
        pub fn processes(&self, top: usize) -> Vec<GpuProcess> {
            let mut merged: HashMap<u32, GpuProcess> = HashMap::new();
            for get in [self.get_compute_procs, self.get_graphics_procs]
                .into_iter()
                .flatten()
            {
                // A process can hold both a compute and a graphics context;
                // keep the largest reported memory (the two lists may report
                // the same context).
                for (pid, mem) in self.running_processes(get) {
                    let e = merged.entry(pid).or_default();
                    e.pid = pid;
                    e.mem_bytes = e.mem_bytes.max(mem);
                }
            }
            for (pid, util) in self.process_utilization() {
                if let Some(p) = merged.get_mut(&pid) {
                    p.util_percent = util;
                }
            }
            super::rank_processes(merged.into_values().collect(), top)
        }

        /// `(pid, GPU memory bytes)` for one process list (compute or
        /// graphics), using the documented sizing protocol: a first call
        /// with a NULL buffer and count 0 answers with the number of
        /// entries needed; the buffer is then oversize so a process
        /// appearing in between cannot break the read.
        fn running_processes(&self, get: FnGetRunningProcesses) -> Vec<(u32, u64)> {
            let mut count: u32 = 0;
            let rc = unsafe { (get)(self.device, &mut count, ptr::null_mut()) };
            if rc == NVML_SUCCESS || rc != NVML_ERROR_INSUFFICIENT_SIZE {
                // SUCCESS means no processes; anything else is an unexpected
                // error: skip this list rather than fail the whole sample.
                return Vec::new();
            }
            let cap = count.saturating_mul(2).saturating_add(5).clamp(1, 4096);
            let mut buf = vec![NvmlProcessInfo::default(); cap as usize];
            let mut n = cap;
            if unsafe { (get)(self.device, &mut n, buf.as_mut_ptr()) } != NVML_SUCCESS {
                return Vec::new();
            }
            buf.iter()
                .take((n as usize).min(cap as usize))
                .filter(|p| p.pid != 0)
                .map(|p| {
                    // NVML_VALUE_NOT_AVAILABLE (all bits set) when memory
                    // reporting is unavailable (e.g. some drivers hide other
                    // users' processes): show 0 rather than 18 exabytes.
                    let mem = if p.used_gpu_memory == u64::MAX {
                        0
                    } else {
                        p.used_gpu_memory
                    };
                    (p.pid, mem)
                })
                .collect()
        }

        /// Newest SM utilization percent per process. `lastSeenTimeStamp = 0`
        /// means "since the last call" on current drivers, but a timestamp
        /// cursor is kept anyway so each poll consumes each recorded sample
        /// exactly once regardless of how the driver interprets 0.
        fn process_utilization(&self) -> HashMap<u32, u32> {
            let get = match self.get_process_util {
                Some(f) => f,
                None => return HashMap::new(),
            };
            let cursor = self.last_util_ts.load(Ordering::Relaxed);
            let mut count: u32 = 0;
            let rc = unsafe { (get)(self.device, ptr::null_mut(), &mut count, cursor) };
            if rc == NVML_SUCCESS || rc != NVML_ERROR_INSUFFICIENT_SIZE {
                return HashMap::new();
            }
            let cap = count.saturating_mul(2).saturating_add(5).clamp(1, 4096);
            let mut buf = vec![NvmlProcessUtilizationSample::default(); cap as usize];
            let mut n = cap;
            if unsafe { (get)(self.device, buf.as_mut_ptr(), &mut n, cursor) } != NVML_SUCCESS {
                return HashMap::new();
            }
            let mut newest_ts = cursor;
            let mut newest: HashMap<u32, (u64, u32)> = HashMap::new();
            for s in buf.iter().take((n as usize).min(cap as usize)) {
                newest_ts = newest_ts.max(s.time_stamp);
                newest
                    .entry(s.pid)
                    .and_modify(|(ts, u)| {
                        if s.time_stamp >= *ts {
                            *ts = s.time_stamp;
                            *u = s.sm_util;
                        }
                    })
                    .or_insert((s.time_stamp, s.sm_util));
            }
            self.last_util_ts.store(newest_ts, Ordering::Relaxed);
            newest
                .into_iter()
                .map(|(pid, (_ts, util))| (pid, util))
                .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, util: u32, mem: u64) -> GpuProcess {
        GpuProcess {
            pid,
            name: String::new(),
            mem_bytes: mem,
            util_percent: util,
        }
    }

    #[test]
    fn rank_orders_by_utilization_then_memory_then_pid() {
        // Highest utilization wins; ties fall back to VRAM, then pid.
        let procs = vec![
            proc(10, 10, 100),
            proc(11, 50, 200),
            proc(12, 50, 900), // same util as 11, more memory -> ahead
            proc(13, 50, 900), // same util and memory as 12, higher pid -> behind
            proc(14, 5, 100000),
        ];
        let top = rank_processes(procs, 3);
        let pids: Vec<u32> = top.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![12, 13, 11]);
    }

    #[test]
    fn rank_truncates_to_top_n() {
        let procs = (0..10).map(|i| proc(100 + i, i, i as u64)).collect();
        assert_eq!(rank_processes(procs, 3).len(), 3);
    }

    #[test]
    fn rank_drops_own_and_zero_pids() {
        // pid 0 (NVML placeholder) and the app's own pid are never listed,
        // so the kill safeguard can never pick them.
        let own = std::process::id();
        let procs = vec![proc(0, 100, 1000), proc(own, 100, 1000), proc(77, 1, 1)];
        let top = rank_processes(procs, 5);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].pid, 77);
    }

    #[test]
    fn process_name_falls_back_for_unknown_pid() {
        // A pid far outside the allocated range has no /proc entry.
        assert_eq!(process_name(u32::MAX), "pid 4294967295");
        // The current process always has a readable /proc/comm.
        assert!(!process_name(std::process::id()).is_empty());
    }

    #[test]
    fn implausible_readings_are_sensor_faults() {
        let pin = |voltage: f64, current: f64| PinReading { voltage, current };
        let mut pins = vec![pin(12.1, 8.4); NUM_PINS];
        assert!(check_plausible(&pins).is_ok());
        // A genuinely dangerous but physical current is a reading, not a
        // fault: it must reach the red alert and the safeguard.
        pins[2] = pin(11.9, 25.0);
        assert!(check_plausible(&pins).is_ok());
        // An all-ones register (65.535) in either field is a glitch.
        pins[2] = pin(12.1, 65.535);
        let err = check_plausible(&pins).unwrap_err();
        assert!(err.contains("pin 2"), "{err}");
        pins[2] = pin(65.535, 8.4);
        assert!(check_plausible(&pins).is_err());
    }

    #[test]
    fn only_single_process_pids_reach_kill() {
        // Tested on the pure conversion only: a broken guard in a test of
        // kill_process itself would SIGKILL every process of the test user.
        assert_eq!(single_process_pid(1), Some(1));
        assert_eq!(single_process_pid(4242), Some(4242));
        assert_eq!(single_process_pid(i32::MAX as u32), Some(i32::MAX));
        // 0 = own process group; above i32::MAX wraps to group/broadcast
        // targets (u32::MAX would be kill(-1)).
        assert_eq!(single_process_pid(0), None);
        assert_eq!(single_process_pid(i32::MAX as u32 + 1), None);
        assert_eq!(single_process_pid(u32::MAX), None);
    }

    #[test]
    fn parse_kill_line_reads_pid_and_outcome() {
        let ok = parse_kill_line("KILL OK 4242");
        assert!((ok.pid, ok.success, ok.message.is_empty()) == (4242, true, true));
        let err = parse_kill_line("KILL ERR 4242 Operation not permitted (os error 1)");
        assert!((err.pid, err.success) == (4242, false));
        assert_eq!(err.message, "Operation not permitted (os error 1)");
        // A malformed line must not panic and must read as a failed kill.
        let bad = parse_kill_line("KILL BOGUS");
        assert!(!bad.success);
    }
}
