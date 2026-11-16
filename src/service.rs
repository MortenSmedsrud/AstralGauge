//! Background service: AstralGauge as a systemd *user* service.
//!
//! - **Unit management** ([`install`], [`uninstall`], [`restart`],
//!   [`unit_status`]): writes `astral-gauge.service` into the user's systemd
//!   unit directory and drives `systemctl --user`. A user service (not a
//!   system one) needs no root to install, starts at login, and runs inside
//!   the user's session manager, so it reaches the desktop sound server for
//!   the alert sounds and the polkit agent for the privileged helper.
//! - **Service mode** ([`run_service`], `astral-gauge --service`): the
//!   desktop app minus the window. The same poller (alerts, safeguard,
//!   automatic power limit) and web server, configured from the settings
//!   file the desktop app saves, which it re-reads whenever it changes.
//! - **Link** (a Unix socket in `$XDG_RUNTIME_DIR`): while the service runs,
//!   the desktop app does not poll the hardware itself (two monitors would
//!   alert, kill and lower the power limit twice and fight over the web
//!   port). [`spawn_monitor`] connects to the service instead and mirrors
//!   its state into the app's own `WebData` — the UI renders it unchanged —
//!   and forwards the UI's commands (power limit, authentication, alert
//!   acknowledgment). Without a service it runs the local poller, and it
//!   switches over whenever a service appears or goes away.
//!
//! Link protocol (one request line, then the reply): `HELLO <version>` →
//! `HELLO <version>`; `SYNC <since>|-` → the state lines plus the history
//! points newer than `since` (see [`encode_sync`]), ending with `end`;
//! `LIMIT <watts>`, `AUTH`, `ACK`, `PING` → `ok`.

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::alerts::{AutoMinPower, SoundPlayer};
use crate::hardware::{
    own_exe, spawn_poller, AuthState, Cmd, GpuProcess, PinReading, Poller, Sample, DEFAULT_POLL_MS,
    NUM_PINS,
};
use crate::persist;
use crate::web::{
    PowerDown, ServiceLink, WebData, WebServer, WebState, DEFAULT_WEB_PORT, HISTORY_SECS,
};

/// Command-line flag that runs this binary as the background service.
pub(crate) const SERVICE_ARG: &str = "--service";
/// The systemd user unit.
pub(crate) const UNIT_NAME: &str = "astral-gauge.service";
/// The link socket, in `$XDG_RUNTIME_DIR` (private to the user).
const SOCKET_NAME: &str = "astral-gauge.sock";
/// Link protocol version: a desktop app only shows the data of a service
/// that speaks the same one (a rebuilt app next to a still-running older
/// service asks for a service restart instead).
const LINK_VERSION: u32 = 1;
/// How often the desktop app fetches the service's state.
const VIEW_INTERVAL: Duration = Duration::from_millis(200);
/// How often the desktop app looks for a service while monitoring locally.
const SERVICE_CHECK: Duration = Duration::from_secs(1);
/// A link request that takes longer counts as a lost service.
const LINK_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the service checks the settings file for changes.
const SETTINGS_CHECK: Duration = Duration::from_secs(1);
/// Retry spacing for a web-server port that is busy (e.g. still held by
/// the desktop app that is handing the monitoring over).
const WEB_RETRY: Duration = Duration::from_secs(5);
/// History series in a SYNC reply: the pins, then power, temperature, VRAM.
const SERIES: usize = NUM_PINS + 3;

/// The settings the service runs with when nothing was saved yet: the
/// desktop app's built-in defaults (`ui::Config::defaults`).
fn default_settings() -> persist::StoredSettings {
    persist::StoredSettings {
        alerts: crate::alerts::Settings::default(),
        poll_ms: DEFAULT_POLL_MS,
        web_enabled: false,
        web_port: DEFAULT_WEB_PORT.to_string(),
    }
}

/// `$XDG_RUNTIME_DIR/astral-gauge.sock` (falling back to `/run/user/<uid>`).
fn socket_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            let p = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
            p.is_dir().then_some(p)
        })?;
    Some(dir.join(SOCKET_NAME))
}

// ---------------------------------------------------------------------------
// Unit management
// ---------------------------------------------------------------------------

/// `$XDG_CONFIG_HOME/systemd/user` (or `~/.config/systemd/user`): where
/// `systemctl --user` looks for the user's own units.
fn unit_dir() -> Option<PathBuf> {
    let env_dir = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    env_dir("XDG_CONFIG_HOME")
        .or_else(|| env_dir("HOME").map(|h| h.join(".config")))
        .map(|c| c.join("systemd").join("user"))
}

fn unit_path() -> Option<PathBuf> {
    unit_dir().map(|d| d.join(UNIT_NAME))
}

/// Quote one `ExecStart=` word: double quotes, with the backslash, quote,
/// specifier (`%`) and variable (`$`) characters escaped.
fn exec_quote(word: &str) -> String {
    let mut out = String::from("\"");
    for c in word.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The unit file for running `exe` as the service (with the I2C bus the
/// app was started with, if one was given explicitly).
fn unit_text(exe: &Path, bus: Option<u32>) -> String {
    let mut exec = format!("{} {SERVICE_ARG}", exec_quote(&exe.to_string_lossy()));
    if let Some(bus) = bus {
        let _ = write!(exec, " {bus}");
    }
    format!(
        "# Installed by AstralGauge (Settings > Background service); uninstall it there.
[Unit]
Description=AstralGauge 12VHPWR pin-current monitor
# Alert sounds go through the session's sound server: start after it when
# it is part of this login. Ordering only — the service retries the audio
# output (and the GPU, I2C and NVML) itself until they are available.
After=pipewire.service pipewire-pulse.service wireplumber.service
# Keep restarting a monitor that failed, however often.
StartLimitIntervalSec=0

[Service]
Type=simple
ExecStart={exec}
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"
    )
}

/// Run `systemctl --user <args>`: its stdout on success, the error text
/// otherwise.
fn systemctl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| format!("cannot run systemctl: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(format!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            if stderr.is_empty() { stdout } else { stderr }
        ))
    }
}

/// The service as systemd sees it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UnitStatus {
    /// The unit file exists.
    pub(crate) installed: bool,
    /// The unit file is exactly what [`install`] would write now (same
    /// executable and bus); otherwise reinstalling updates it.
    pub(crate) current: bool,
    /// Starts at login.
    pub(crate) enabled: bool,
    /// `systemctl is-active`: active, inactive, failed, activating, ...
    pub(crate) active: String,
}

/// Query the unit's state (blocking: runs `systemctl`).
pub(crate) fn unit_status(bus: Option<u32>) -> Result<UnitStatus, String> {
    let path = unit_path().ok_or("no home directory")?;
    let installed_text = fs::read_to_string(&path).ok();
    let current = match (&installed_text, own_exe()) {
        (Some(text), Ok(exe)) => *text == unit_text(&exe, bus),
        _ => false,
    };
    // Both print the state on stdout and fail (non-zero) for most states.
    let query = |verb: &str| -> Result<String, String> {
        let out = Command::new("systemctl")
            .args(["--user", verb, UNIT_NAME])
            .output()
            .map_err(|e| format!("cannot run systemctl: {e}"))?;
        let state = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if state.is_empty() && !out.status.success() {
            // No state at all: systemctl itself failed (no user manager).
            let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            return Err(format!("systemctl --user {verb}: {err}"));
        }
        Ok(state)
    };
    Ok(UnitStatus {
        installed: installed_text.is_some(),
        current,
        enabled: query("is-enabled")? == "enabled",
        active: query("is-active")?,
    })
}

/// Install (or update) and start the service for this executable.
pub(crate) fn install(bus: Option<u32>) -> Result<String, String> {
    let exe = own_exe()?;
    let path = unit_path().ok_or("no home directory")?;
    let dir = path.parent().ok_or("bad unit path")?;
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let tmp = path.with_extension("service.tmp");
    fs::write(&tmp, unit_text(&exe, bus))
        .and_then(|()| fs::rename(&tmp, &path))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", UNIT_NAME])?;
    // Restart, not start: an updated unit (new executable) takes effect too.
    systemctl(&["restart", UNIT_NAME])?;
    println!("Background service installed: {}", path.display());
    Ok("Service installed and started.".to_owned())
}

/// Stop, disable and remove the service.
pub(crate) fn uninstall() -> Result<String, String> {
    let path = unit_path().ok_or("no home directory")?;
    if path.exists() {
        systemctl(&["disable", "--now", UNIT_NAME])?;
        fs::remove_file(&path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
    }
    systemctl(&["daemon-reload"])?;
    // Forget a failed state, so nothing about the unit is left listed.
    let _ = systemctl(&["reset-failed", UNIT_NAME]);
    println!("Background service uninstalled.");
    Ok("Service stopped and uninstalled.".to_owned())
}

/// Restart the service (e.g. after rebuilding the executable it runs).
pub(crate) fn restart() -> Result<String, String> {
    systemctl(&["restart", UNIT_NAME])?;
    Ok("Service restarted.".to_owned())
}

/// What the settings dialog asks the service management to do.
#[derive(Clone, Copy)]
pub(crate) enum ServiceAction {
    Install,
    Uninstall,
    Restart,
}

/// Unit state and the last install/uninstall result, as shown in the
/// settings dialog. `systemctl` runs on short-lived threads, never on the
/// GUI thread.
#[derive(Default)]
pub(crate) struct ServiceControl {
    /// Latest unit status query (`None` before the first one finished).
    pub(crate) status: Option<Result<UnitStatus, String>>,
    /// An install/uninstall/restart is running.
    pub(crate) busy: bool,
    /// The last action's outcome: a confirmation or the error.
    pub(crate) message: Option<Result<String, String>>,
    querying: bool,
    queried_at: Option<Instant>,
}

/// Refresh the unit status in the background when the last query is older
/// than `max_age` (called every frame while the settings dialog is open).
pub(crate) fn refresh_status(
    ctl: &Arc<Mutex<ServiceControl>>,
    bus: Option<u32>,
    max_age: Duration,
) {
    {
        let mut c = ctl.lock().unwrap();
        if c.querying || c.busy || c.queried_at.is_some_and(|t| t.elapsed() < max_age) {
            return;
        }
        c.querying = true;
    }
    let ctl = ctl.clone();
    thread::spawn(move || {
        let status = unit_status(bus);
        let mut c = ctl.lock().unwrap();
        c.status = Some(status);
        c.querying = false;
        c.queried_at = Some(Instant::now());
    });
}

/// Run an install/uninstall/restart in the background.
pub(crate) fn run_action(
    ctl: &Arc<Mutex<ServiceControl>>,
    action: ServiceAction,
    bus: Option<u32>,
) {
    {
        let mut c = ctl.lock().unwrap();
        if c.busy {
            return;
        }
        c.busy = true;
        c.message = None;
    }
    let ctl = ctl.clone();
    thread::spawn(move || {
        let result = match action {
            ServiceAction::Install => install(bus),
            ServiceAction::Uninstall => uninstall(),
            ServiceAction::Restart => restart(),
        };
        if let Err(e) = &result {
            eprintln!("Background service: {e}");
        }
        let status = unit_status(bus);
        let mut c = ctl.lock().unwrap();
        c.message = Some(result);
        c.status = Some(status);
        c.queried_at = Some(Instant::now());
        c.busy = false;
    });
}

// ---------------------------------------------------------------------------
// Service mode
// ---------------------------------------------------------------------------

/// Run as the background service (`--service`); never returns normally
/// (systemd stops it with SIGTERM). Returns an exit code when it cannot
/// start, e.g. because another instance already runs.
pub(crate) fn run_service(explicit_bus: Option<u32>) -> i32 {
    println!(
        "AstralGauge {} running as a background service.",
        env!("CARGO_PKG_VERSION")
    );
    let listener = match socket_path() {
        Some(path) => {
            if UnixStream::connect(&path).is_ok() {
                eprintln!("Another AstralGauge service is already running.");
                return 1;
            }
            // A socket file left by a service that died: replace it.
            let _ = fs::remove_file(&path);
            match UnixListener::bind(&path) {
                Ok(listener) => Some(listener),
                Err(e) => {
                    eprintln!(
                        "Cannot create {}: {e}. The desktop app will not see this service.",
                        path.display()
                    );
                    None
                }
            }
        }
        None => {
            eprintln!("XDG_RUNTIME_DIR is not set: the desktop app will not see this service.");
            None
        }
    };

    let data = Arc::new(Mutex::new(WebData::new()));
    {
        let mut d = data.lock().unwrap();
        d.service_mode = true;
        d.service_link = Some(ServiceLink::default());
        d.local_monitor = true;
    }
    let player = SoundPlayer::spawn();
    let poller = spawn_poller(explicit_bus, data.clone(), player);
    if let Some(listener) = listener {
        let data = data.clone();
        let tx = poller.tx.clone();
        thread::spawn(move || serve_link(listener, data, tx));
    }
    supervise_settings(&data, &poller.tx)
}

/// Whether these settings use the privileged helper (root via pkexec).
fn privileged(s: &persist::StoredSettings) -> bool {
    s.alerts.auto_min_power != AutoMinPower::Off || s.alerts.kill_other_users_processes
}

/// The service's main loop: (re)load the settings file whenever it changes
/// and keep the web server in line with it (retrying a busy port).
fn supervise_settings(data: &Arc<Mutex<WebData>>, tx: &mpsc::Sender<Cmd>) -> ! {
    let defaults = default_settings();
    let mut applied: Option<persist::StoredSettings> = None;
    // Modification time of the settings file when it was last read (outer
    // None: never read).
    let mut stamp: Option<Option<SystemTime>> = None;
    let mut web = WebState::Stopped;
    let mut web_error: Option<String> = None;
    let mut web_retry_at = Instant::now();
    loop {
        let mtime = persist::settings_path()
            .and_then(|p| fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
        if stamp != Some(mtime) {
            stamp = Some(mtime);
            let loaded = match persist::load(&defaults) {
                Ok(saved) => saved.unwrap_or_else(|| defaults.clone()),
                Err(e) => {
                    eprintln!("Settings file problem: {e}. Keeping the current settings.");
                    applied.clone().unwrap_or_else(|| defaults.clone())
                }
            };
            if applied.as_ref() != Some(&loaded) {
                println!(
                    "{}",
                    if applied.is_none() {
                        "Settings loaded."
                    } else {
                        "Settings changed: applied."
                    }
                );
                {
                    let mut d = data.lock().unwrap();
                    d.settings = loaded.alerts.clone();
                    d.poll_ms = loaded.poll_ms;
                }
                if applied.as_ref().map(|s| s.poll_ms) != Some(loaded.poll_ms) {
                    let _ = tx.send(Cmd::Interval(loaded.poll_ms));
                }
                // Newly enabled privileged options: authenticate the helper
                // now (the desktop's polkit agent shows the prompt), like the
                // desktop app does at startup. Without an agent yet (early
                // in the login) it fails and the next privileged action asks
                // again.
                if privileged(&loaded) && !applied.as_ref().is_some_and(privileged) {
                    data.lock().unwrap().power_auth = AuthState::Idle;
                    let _ = tx.send(Cmd::GrantPowerAccess);
                }
                web_retry_at = Instant::now();
                applied = Some(loaded);
            }
        }
        let settings = applied.as_ref().unwrap_or(&defaults);

        // The web server follows the saved settings; a busy port (the
        // desktop app may still be letting go of it) is retried.
        let port = settings
            .web_port
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0);
        match (settings.web_enabled, port) {
            (false, _) => {
                if matches!(web, WebState::Running(_)) {
                    web = WebState::Stopped;
                }
                web_error = None;
            }
            (true, None) => {
                web = WebState::Stopped;
                web_error = Some(format!("invalid port \"{}\"", settings.web_port));
            }
            (true, Some(port)) => {
                let serving = matches!(&web, WebState::Running(s) if s.port == port);
                if !serving && Instant::now() >= web_retry_at {
                    web = WebState::Stopped;
                    match WebServer::start(port, data.clone()) {
                        Ok(server) => {
                            web = WebState::Running(server);
                            web_error = None;
                        }
                        Err(e) => {
                            let msg = format!("port {port} is not available: {e}");
                            if web_error.as_ref() != Some(&msg) {
                                eprintln!("Web interface: {msg} (retrying).");
                            }
                            web_error = Some(msg);
                            web_retry_at = Instant::now() + WEB_RETRY;
                        }
                    }
                }
            }
        }
        if let Some(link) = data.lock().unwrap().service_link.as_mut() {
            link.web_port = match &web {
                WebState::Running(server) => Some(server.port),
                WebState::Stopped => None,
            };
            link.web_error = web_error.clone();
        }
        thread::sleep(SETTINGS_CHECK);
    }
}

/// Accept desktop-app connections on the link socket.
fn serve_link(listener: UnixListener, data: Arc<Mutex<WebData>>, tx: mpsc::Sender<Cmd>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let data = data.clone();
                let tx = tx.clone();
                thread::spawn(move || link_client(stream, data, tx));
            }
            Err(e) => {
                eprintln!("Link socket: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Answer one desktop app's requests until it disconnects.
fn link_client(stream: UnixStream, data: Arc<Mutex<WebData>>, tx: mpsc::Sender<Cmd>) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut writer = stream;
    for line in BufReader::new(read_half).lines() {
        let Ok(line) = line else { break };
        let (verb, arg) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        let reply = match verb {
            "HELLO" => format!("HELLO {LINK_VERSION}\n"),
            "SYNC" => {
                let since = arg.parse::<f64>().ok();
                let reply = sync_reply(&data.lock().unwrap(), since);
                encode_sync(&reply)
            }
            "LIMIT" => match arg.parse::<u32>() {
                Ok(watts) => {
                    let _ = tx.send(Cmd::SetLimit(watts));
                    "ok\n".to_owned()
                }
                Err(_) => "err bad limit\n".to_owned(),
            },
            "AUTH" => {
                // Pending until the helper answers; never the stale state of
                // an earlier attempt (see the poller's auth mirroring).
                data.lock().unwrap().power_auth = AuthState::Idle;
                let _ = tx.send(Cmd::GrantPowerAccess);
                "ok\n".to_owned()
            }
            "ACK" => {
                data.lock().unwrap().critical_alert = false;
                "ok\n".to_owned()
            }
            "PING" => "ok\n".to_owned(),
            _ => "err unknown request\n".to_owned(),
        };
        if writer.write_all(reply.as_bytes()).is_err() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// State sync
// ---------------------------------------------------------------------------

/// One SYNC reply: the service's current state plus the history points newer
/// than the requested time (all of them for a first, full sync).
#[derive(Default)]
struct SyncReply {
    /// The service's clock (seconds since its `WebData::start`).
    now: f64,
    sampled_at: Option<f64>,
    pins_at: Option<f64>,
    critical: bool,
    auth: AuthState,
    power_range: Option<f64>,
    temp_range: Option<f64>,
    mem_range: Option<f64>,
    power_note: Option<String>,
    kill_note: Option<(f64, String)>,
    sensor_error: Option<String>,
    web_port: Option<u16>,
    web_error: Option<String>,
    power_down: Option<PowerDown>,
    sample: Sample,
    /// [`SERIES`] histories of (time, value), oldest first.
    series: Vec<Vec<(f64, f64)>>,
}

/// Capture the state for a SYNC reply (under the data lock: copies only).
fn sync_reply(d: &WebData, since: Option<f64>) -> SyncReply {
    let tail = |h: &Vec<(f64, f64)>| {
        let k = since.map_or(0, |s| h.partition_point(|(t, _)| *t <= s));
        h[k..].to_vec()
    };
    let mut series: Vec<Vec<(f64, f64)>> = d.history.iter().map(tail).collect();
    series.push(tail(&d.power_history));
    series.push(tail(&d.temp_history));
    series.push(tail(&d.mem_history));
    let link = d.service_link.clone().unwrap_or_default();
    SyncReply {
        now: d.start.elapsed().as_secs_f64(),
        sampled_at: d.sampled_at,
        pins_at: d.pins_at,
        critical: d.critical_alert,
        auth: d.power_auth,
        power_range: d.power_range_max,
        temp_range: d.temp_range_max,
        mem_range: d.mem_range_max,
        power_note: d.power_note.clone(),
        kill_note: d.kill_note.clone(),
        sensor_error: d.sensor_error.clone(),
        web_port: link.web_port,
        web_error: link.web_error,
        power_down: d.power_down.clone(),
        sample: d.sample.clone(),
        series,
    }
}

/// Escape text for a single line (backslash, newline, carriage return).
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// An optional number, `-` for none. `{}` prints floats in their shortest
/// exact form, so times survive the trip bit for bit (the next SYNC asks
/// for points after exactly the last one received).
fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

/// Encode a SYNC reply as lines, ending with `end`.
fn encode_sync(r: &SyncReply) -> String {
    let points: usize = r.series.iter().map(Vec::len).sum();
    let mut o = String::with_capacity(1024 + points * 24);
    let _ = writeln!(o, "now {}", r.now);
    let _ = writeln!(o, "sampled_at {}", opt(r.sampled_at));
    let _ = writeln!(o, "pins_at {}", opt(r.pins_at));
    let _ = writeln!(o, "critical {}", u8::from(r.critical));
    let auth = match r.auth {
        AuthState::Idle => "idle",
        AuthState::Active => "active",
        AuthState::Failed => "failed",
    };
    let _ = writeln!(o, "auth {auth}");
    let _ = writeln!(
        o,
        "ranges {} {} {}",
        opt(r.power_range),
        opt(r.temp_range),
        opt(r.mem_range)
    );
    if let Some(note) = &r.power_note {
        let _ = writeln!(o, "power_note {}", escape(note));
    }
    if let Some((t, note)) = &r.kill_note {
        let _ = writeln!(o, "kill_note {t} {}", escape(note));
    }
    if let Some(e) = &r.sensor_error {
        let _ = writeln!(o, "sensor_error {}", escape(e));
    }
    if let Some(port) = r.web_port {
        let _ = writeln!(o, "web_port {port}");
    }
    if let Some(e) = &r.web_error {
        let _ = writeln!(o, "web_error {}", escape(e));
    }
    if let Some(pd) = &r.power_down {
        let _ = writeln!(
            o,
            "power_down {} {} {}",
            pd.saved_w,
            pd.trigger_a,
            opt(pd.restore_in)
        );
    }
    let s = &r.sample;
    let _ = writeln!(
        o,
        "sample {} {} {} {} {} {} {} {}",
        opt(s.power_w),
        opt(s.limit_w),
        opt(s.limit_min_w),
        opt(s.limit_max_w),
        opt(s.temp_c),
        opt(s.max_temp_c),
        opt(s.mem_used_gb),
        opt(s.mem_total_gb)
    );
    for p in &s.pins {
        let _ = writeln!(o, "pin {} {}", p.voltage, p.current);
    }
    for p in &s.processes {
        let _ = writeln!(
            o,
            "proc {} {} {} {}",
            p.pid,
            p.mem_bytes,
            p.util_percent,
            escape(&p.name)
        );
    }
    for (i, h) in r.series.iter().enumerate() {
        if h.is_empty() {
            continue;
        }
        let _ = write!(o, "series {i}");
        for (t, v) in h {
            let _ = write!(o, " {t} {v}");
        }
        o.push('\n');
    }
    o.push_str("end\n");
    o
}

/// Decode the lines of a SYNC reply (without the `end` line). Unknown lines
/// are skipped, so a newer service can add fields.
fn decode_sync(lines: &[String]) -> Result<SyncReply, String> {
    fn num<T: std::str::FromStr>(word: Option<&str>) -> Result<Option<T>, String> {
        match word {
            None => Err("missing value".to_owned()),
            Some("-") => Ok(None),
            Some(w) => w
                .parse::<T>()
                .map(Some)
                .map_err(|_| format!("bad value {w:?}")),
        }
    }
    let mut r = SyncReply {
        series: vec![Vec::new(); SERIES],
        ..SyncReply::default()
    };
    let mut have_now = false;
    for line in lines {
        let (key, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        let mut words = rest.split(' ');
        let bad = |e: String| format!("SYNC {key}: {e}");
        match key {
            "now" => {
                r.now = num(words.next()).map_err(bad)?.ok_or("SYNC now: missing")?;
                have_now = true;
            }
            "sampled_at" => r.sampled_at = num(words.next()).map_err(bad)?,
            "pins_at" => r.pins_at = num(words.next()).map_err(bad)?,
            "critical" => r.critical = rest == "1",
            "auth" => {
                r.auth = match rest {
                    "active" => AuthState::Active,
                    "failed" => AuthState::Failed,
                    _ => AuthState::Idle,
                }
            }
            "ranges" => {
                r.power_range = num(words.next()).map_err(bad)?;
                r.temp_range = num(words.next()).map_err(bad)?;
                r.mem_range = num(words.next()).map_err(bad)?;
            }
            "power_note" => r.power_note = Some(unescape(rest)),
            "kill_note" => {
                let (t, note) = rest.split_once(' ').unwrap_or((rest, ""));
                let t = num(Some(t))
                    .map_err(bad)?
                    .ok_or("SYNC kill_note: no time")?;
                r.kill_note = Some((t, unescape(note)));
            }
            "sensor_error" => r.sensor_error = Some(unescape(rest)),
            "web_port" => r.web_port = num(words.next()).map_err(bad)?,
            "web_error" => r.web_error = Some(unescape(rest)),
            "power_down" => {
                let saved_w = num(words.next())
                    .map_err(bad)?
                    .ok_or("SYNC power_down: no limit")?;
                let trigger_a = num(words.next())
                    .map_err(bad)?
                    .ok_or("SYNC power_down: no trigger")?;
                let restore_in = num(words.next()).map_err(bad)?;
                r.power_down = Some(PowerDown {
                    saved_w,
                    trigger_a,
                    restore_in,
                });
            }
            "sample" => {
                let s = &mut r.sample;
                s.power_w = num(words.next()).map_err(bad)?;
                s.limit_w = num(words.next()).map_err(bad)?;
                s.limit_min_w = num(words.next()).map_err(bad)?;
                s.limit_max_w = num(words.next()).map_err(bad)?;
                s.temp_c = num(words.next()).map_err(bad)?;
                s.max_temp_c = num(words.next()).map_err(bad)?;
                s.mem_used_gb = num(words.next()).map_err(bad)?;
                s.mem_total_gb = num(words.next()).map_err(bad)?;
            }
            "pin" => {
                let voltage = num(words.next()).map_err(bad)?.unwrap_or(0.0);
                let current = num(words.next()).map_err(bad)?.unwrap_or(0.0);
                r.sample.pins.push(PinReading { voltage, current });
            }
            "proc" => {
                let mut parts = rest.splitn(4, ' ');
                let pid = num(parts.next()).map_err(bad)?.unwrap_or(0);
                let mem_bytes = num(parts.next()).map_err(bad)?.unwrap_or(0);
                let util_percent = num(parts.next()).map_err(bad)?.unwrap_or(0);
                let name = unescape(parts.next().unwrap_or(""));
                r.sample.processes.push(GpuProcess {
                    pid,
                    name,
                    mem_bytes,
                    util_percent,
                });
            }
            "series" => {
                let i: usize = num(words.next()).map_err(bad)?.unwrap_or(usize::MAX);
                let Some(h) = r.series.get_mut(i) else {
                    continue;
                };
                while let Some(t) = words.next() {
                    let t: f64 = num(Some(t)).map_err(bad)?.ok_or("SYNC series: no time")?;
                    let v: f64 = num(words.next())
                        .map_err(bad)?
                        .ok_or("SYNC series: no value")?;
                    h.push((t, v));
                }
            }
            _ => {}
        }
    }
    // Readings are all six pins or none (see `Sample::pins`).
    if !r.sample.pins.is_empty() && r.sample.pins.len() != NUM_PINS {
        return Err(format!("SYNC: {} pin readings", r.sample.pins.len()));
    }
    if !have_now {
        return Err("SYNC: no time".to_owned());
    }
    Ok(r)
}

/// Fold a SYNC reply into the desktop app's data. A `full` sync replaces
/// the histories and moves `WebData::start` onto the service's clock, so
/// the service's timestamps (graphs, staleness, the kill note's age) apply
/// unchanged. `keep_ack` / `keep_auth`: an acknowledgment or authentication
/// request is on its way to the service, so this reply's alert flag or
/// auth state predates it and must not overwrite the local one.
fn apply_sync(d: &mut WebData, r: SyncReply, full: bool, keep_ack: bool, keep_auth: bool) {
    if full {
        if let Some(start) = Instant::now().checked_sub(Duration::from_secs_f64(r.now.max(0.0))) {
            d.start = start;
        }
        for h in &mut d.history {
            h.clear();
        }
        d.power_history.clear();
        d.temp_history.clear();
        d.mem_history.clear();
    }
    let t_min = r.now - HISTORY_SECS;
    for (i, points) in r.series.into_iter().enumerate() {
        let hist = match i {
            i if i < NUM_PINS => &mut d.history[i],
            i if i == NUM_PINS => &mut d.power_history,
            i if i == NUM_PINS + 1 => &mut d.temp_history,
            _ => &mut d.mem_history,
        };
        let last = hist.last().map(|p| p.0);
        hist.extend(
            points
                .into_iter()
                .filter(|(t, _)| last.is_none_or(|l| *t > l)),
        );
        let k = hist.partition_point(|(t, _)| *t < t_min);
        hist.drain(..k);
    }
    d.sample = r.sample;
    d.sampled_at = r.sampled_at;
    d.pins_at = r.pins_at;
    if r.power_range.is_some() {
        d.power_range_max = r.power_range;
    }
    if r.temp_range.is_some() {
        d.temp_range_max = r.temp_range;
    }
    if r.mem_range.is_some() {
        d.mem_range_max = r.mem_range;
    }
    if !keep_ack {
        d.critical_alert = r.critical;
    }
    if !keep_auth {
        d.power_auth = r.auth;
    }
    d.power_note = r.power_note;
    d.kill_note = r.kill_note;
    d.sensor_error = r.sensor_error;
    d.power_down = r.power_down;
    d.service_link = Some(ServiceLink {
        web_port: r.web_port,
        web_error: r.web_error,
        incompatible: None,
    });
}

// ---------------------------------------------------------------------------
// Desktop side: local poller or service link
// ---------------------------------------------------------------------------

/// Start the desktop app's monitoring: the local poller while no service
/// runs, the service link while one does (switching whenever that changes).
/// The returned sender takes the UI's commands for whichever is active.
pub(crate) fn spawn_monitor(
    explicit_bus: Option<u32>,
    data: Arc<Mutex<WebData>>,
    player: SoundPlayer,
) -> mpsc::Sender<Cmd> {
    let (tx, rx) = mpsc::channel::<Cmd>();
    thread::spawn(move || {
        let mut local: Option<Poller> = None;
        loop {
            if let Some(stream) = socket_path().and_then(|p| UnixStream::connect(p).ok()) {
                match local.take() {
                    Some(poller) => {
                        println!("Background service started: it takes over the monitoring.");
                        poller.stop();
                    }
                    None => println!("Background service running: showing its data."),
                }
                data.lock().unwrap().local_monitor = false;
                let end = view_service(stream, &data, &rx);
                data.lock().unwrap().service_link = None;
                match end {
                    Ok(()) => return, // the app is closing
                    Err(why) => {
                        eprintln!("Lost the background service ({why}); monitoring locally.")
                    }
                }
                continue;
            }
            let poller = local.get_or_insert_with(|| {
                let poll_ms = data.lock().unwrap().poll_ms;
                let poller = spawn_poller(explicit_bus, data.clone(), player.clone());
                if poll_ms != DEFAULT_POLL_MS {
                    let _ = poller.tx.send(Cmd::Interval(poll_ms));
                }
                data.lock().unwrap().local_monitor = true;
                poller
            });
            // Forward the UI's commands until the next look for a service.
            let until = Instant::now() + SERVICE_CHECK;
            loop {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                match rx.recv_timeout(left) {
                    Ok(cmd) => {
                        let _ = poller.tx.send(cmd);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    });
    tx
}

/// Send one request line and read the reply lines up to `terminator`
/// (`None`: the reply is a single line).
fn request(
    writer: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    line: &str,
    terminator: Option<&str>,
) -> Result<Vec<String>, String> {
    writer
        .write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("link write: {e}"))?;
    let mut lines = Vec::new();
    loop {
        let mut buf = String::new();
        match reader.read_line(&mut buf) {
            Ok(0) => return Err("service closed the link".to_owned()),
            Ok(_) => {}
            Err(e) => return Err(format!("link read: {e}")),
        }
        let buf = buf.trim_end_matches(['\n', '\r']).to_owned();
        match terminator {
            None => return Ok(vec![buf]),
            Some(end) if buf == end => return Ok(lines),
            Some(_) => lines.push(buf),
        }
    }
}

/// Show a running service's data until the link breaks (`Err`, with why)
/// or the app closes (`Ok`).
fn view_service(
    stream: UnixStream,
    data: &Arc<Mutex<WebData>>,
    rx: &mpsc::Receiver<Cmd>,
) -> Result<(), String> {
    let _ = stream.set_read_timeout(Some(LINK_TIMEOUT));
    let _ = stream.set_write_timeout(Some(LINK_TIMEOUT));
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);

    let hello = request(
        &mut writer,
        &mut reader,
        &format!("HELLO {LINK_VERSION}"),
        None,
    )?
    .remove(0);
    if hello != format!("HELLO {LINK_VERSION}") {
        // A different build: keep out of its way (no local monitoring
        // next to it) and say so until it restarts or stops.
        eprintln!("The background service speaks another link version ({hello:?}).");
        data.lock().unwrap().service_link = Some(ServiceLink {
            incompatible: Some(hello),
            ..ServiceLink::default()
        });
        loop {
            match rx.recv_timeout(SERVICE_CHECK) {
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
            request(&mut writer, &mut reader, "PING", None)?;
        }
    }

    let mut since: Option<f64> = None;
    let mut pending: Vec<Cmd> = Vec::new();
    loop {
        for cmd in pending.drain(..) {
            let line = match cmd {
                Cmd::SetLimit(watts) => format!("LIMIT {watts}"),
                Cmd::GrantPowerAccess => "AUTH".to_owned(),
                Cmd::AckCritical => "ACK".to_owned(),
                // The service applies the interval from the saved settings.
                Cmd::Interval(_) => continue,
            };
            request(&mut writer, &mut reader, &line, None)?;
        }
        let lines = request(
            &mut writer,
            &mut reader,
            &format!("SYNC {}", opt(since)),
            Some("end"),
        )?;
        // Commands that came in while the reply was on its way: the reply
        // predates them (see `apply_sync`).
        loop {
            match rx.try_recv() {
                Ok(cmd) => pending.push(cmd),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        let keep_ack = pending.iter().any(|c| matches!(c, Cmd::AckCritical));
        let keep_auth = pending.iter().any(|c| matches!(c, Cmd::GrantPowerAccess));
        let reply = decode_sync(&lines)?;
        let newest = reply.sampled_at;
        let wake = {
            let mut d = data.lock().unwrap();
            let advanced = since.is_none() || newest != d.sampled_at;
            apply_sync(&mut d, reply, since.is_none(), keep_ack, keep_auth);
            d.on_sample.clone().filter(|_| advanced)
        };
        if let Some(wake) = wake {
            wake();
        }
        since = newest.or(since);

        if pending.is_empty() {
            match rx.recv_timeout(VIEW_INTERVAL) {
                Ok(cmd) => pending.push(cmd),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_data() -> WebData {
        let mut d = WebData::new();
        for (i, current) in [3.0, 3.5].iter().enumerate() {
            let pins = (0..NUM_PINS)
                .map(|p| PinReading {
                    voltage: 12.0 + p as f64 / 10.0,
                    current: current + p as f64 / 3.0,
                })
                .collect();
            d.record(Sample {
                pins,
                power_w: Some(450.25 + i as f64),
                limit_w: Some(600),
                limit_min_w: Some(400),
                limit_max_w: Some(800),
                temp_c: Some(61.0),
                max_temp_c: Some(90.0),
                mem_used_gb: Some(12.5),
                mem_total_gb: Some(31.5),
                processes: vec![GpuProcess {
                    pid: 4242,
                    name: "game \\ with\nodd name".to_owned(),
                    mem_bytes: 1 << 30,
                    util_percent: 97,
                }],
            });
            thread::sleep(Duration::from_millis(2));
        }
        d.critical_alert = true;
        d.power_auth = AuthState::Failed;
        d.kill_note = Some((
            0.5,
            "Critical-alert safeguard: killed game (pid 4242).".into(),
        ));
        d.sensor_error = None;
        d.power_down = Some(PowerDown {
            saved_w: 600,
            trigger_a: 5.5,
            restore_in: Some(12.25),
        });
        d.service_link = Some(ServiceLink {
            web_port: Some(8666),
            web_error: None,
            incompatible: None,
        });
        d
    }

    fn lines(text: &str) -> Vec<String> {
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        assert_eq!(lines.pop().as_deref(), Some("end"));
        lines
    }

    #[test]
    fn full_sync_mirrors_the_service_state() {
        let service = sample_data();
        let text = encode_sync(&sync_reply(&service, None));
        let reply = decode_sync(&lines(&text)).unwrap();
        let mut app = WebData::new();
        apply_sync(&mut app, reply, true, false, false);

        assert_eq!(app.history, service.history);
        assert_eq!(app.power_history, service.power_history);
        assert_eq!(app.temp_history, service.temp_history);
        assert_eq!(app.mem_history, service.mem_history);
        assert_eq!(app.sampled_at, service.sampled_at);
        assert_eq!(app.pins_at, service.pins_at);
        assert_eq!(app.sample.pins.len(), NUM_PINS);
        assert_eq!(app.sample.pins[5].current, service.sample.pins[5].current);
        assert_eq!(app.sample.limit_max_w, Some(800));
        assert_eq!(app.sample.processes[0].name, "game \\ with\nodd name");
        assert_eq!(app.sample.processes[0].util_percent, 97);
        assert_eq!(app.power_range_max, Some(800.0));
        assert!(app.critical_alert);
        assert_eq!(app.power_auth, AuthState::Failed);
        assert_eq!(app.kill_note, service.kill_note);
        assert_eq!(app.power_down, service.power_down);
        assert_eq!(app.service_link.as_ref().unwrap().web_port, Some(8666));
        // The app's clock now runs on the service's.
        let skew = app.start.elapsed().as_secs_f64() - service.start.elapsed().as_secs_f64();
        assert!(skew.abs() < 0.05, "clock skew {skew}");
    }

    #[test]
    fn incremental_sync_appends_only_new_points() {
        let mut service = sample_data();
        let mut app = WebData::new();
        let first = decode_sync(&lines(&encode_sync(&sync_reply(&service, None)))).unwrap();
        let since = first.sampled_at;
        apply_sync(&mut app, first, true, false, false);

        let mut next = service.sample.clone();
        next.pins = Vec::new(); // a sensor fault: a gap, not zeros
        service.record(next);
        let delta = sync_reply(&service, since);
        assert!(delta.series[..NUM_PINS].iter().all(Vec::is_empty));
        assert_eq!(delta.series[NUM_PINS].len(), 1);
        let delta = decode_sync(&lines(&encode_sync(&delta))).unwrap();
        // An acknowledgment / authentication still on its way wins over the
        // reply's older state.
        app.critical_alert = false;
        app.power_auth = AuthState::Idle;
        apply_sync(&mut app, delta, false, true, true);
        assert_eq!(app.history, service.history);
        assert_eq!(app.power_history, service.power_history);
        assert!(app.sample.pins.is_empty());
        assert!(!app.critical_alert);
        assert_eq!(app.power_auth, AuthState::Idle);
    }

    #[test]
    fn broken_sync_replies_are_rejected() {
        assert!(decode_sync(&["sampled_at 1".to_owned()]).is_err());
        assert!(decode_sync(&["now 1".to_owned(), "pin 12 3".to_owned()]).is_err());
        assert!(decode_sync(&["now x".to_owned()]).is_err());
        // Unknown lines are fine (a newer service).
        assert!(decode_sync(&["now 1".to_owned(), "future 1 2".to_owned()]).is_ok());
    }

    #[test]
    fn unit_file_runs_this_executable_as_the_service() {
        let text = unit_text(Path::new("/opt/my apps/astral-gauge"), Some(15));
        assert!(text.contains("ExecStart=\"/opt/my apps/astral-gauge\" --service 15\n"));
        assert!(text.contains("Restart=always"));
        assert!(text.contains("WantedBy=default.target"));
        let text = unit_text(Path::new("/x/100%$\"a\\b"), None);
        assert!(text.contains("ExecStart=\"/x/100%%$$\\\"a\\\\b\" --service\n"));
    }

    #[test]
    fn escaping_round_trips() {
        for s in ["plain", "a\\b", "line\nbreak\r", "trailing\\", ""] {
            assert_eq!(unescape(&escape(s)), s);
            assert!(!escape(s).contains('\n'));
        }
    }
}
