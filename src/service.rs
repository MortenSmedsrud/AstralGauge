//! Background service: AstralGauge as a systemd **system** service, plus a
//! small per-user sound agent.
//!
//! - **System service** (`astral-gauge.service` in `/etc/systemd/system`,
//!   `astral-gauge --service`): runs as root from boot, whether or not
//!   anybody is logged in — the case of a GPU busy with, say, an inference
//!   server while nobody is at the machine. As root it reads the I2C bus,
//!   sets the NVML power limit and kills processes directly: no polkit
//!   prompt (which needs a logged-in desktop) stands between an excursion
//!   and the safeguards. It runs a root-owned copy of the executable
//!   (`/usr/local/lib/astral-gauge/astral-gauge`, never the user-writable
//!   build that installed it) with its settings in
//!   `/etc/astral-gauge/settings.json`.
//! - **Installation** ([`install`], [`uninstall`]): the settings dialog runs
//!   this executable through pkexec in installer mode ([`run_install`],
//!   [`run_uninstall`]) — one administrator prompt — which copies the
//!   executable, writes the settings and the unit, and enables and starts it.
//! - **Sound agent** (`astral-gauge-sound.service`, a systemd *user* unit,
//!   `astral-gauge --sound-agent`, [`run_sound_agent`]): the system service
//!   has no sound output (sound servers live in login sessions), so it
//!   relays its alert sounds ([`SoundRelay`]); the agent, running whenever
//!   the user is logged in, fetches and plays them. Without an agent the
//!   open desktop app plays them.
//! - **Link** (Unix socket `/run/astral-gauge/link.sock`): while the
//!   service runs, the desktop app does not poll the hardware itself (two
//!   monitors would alert, kill and lower the power limit twice and fight
//!   over the web port). [`spawn_monitor`] connects to the service instead
//!   and mirrors its state into the app's own `WebData` — the UI renders it
//!   unchanged — and forwards the UI's commands (power limit,
//!   authentication, alert acknowledgment, accepted settings). Without a
//!   service it runs the local poller, switching whenever a service appears
//!   or goes away. Every local user may read the state; the requests that
//!   change something are only taken from root and the service's owner (the
//!   user who installed it, `--owner=<uid>`; checked with `SO_PEERCRED`).
//!
//! Link protocol (one request line, then the reply): `HELLO <version>` →
//! `HELLO <version>`; `SYNC <since>|- [<settings rev>]` → the state lines
//! plus the history points newer than `since` (see [`encode_sync`]), ending
//! with `end`; `SOUNDS <cursor>|- <0|1> agent|app` → the relayed sounds (see
//! [`encode_sounds`]); `LIMIT <watts>`, `AUTH`, `ACK`, `PING`, `SETTINGS
//! <escaped json>`, `RESTART` → `ok` or `err <why>`.

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::alerts::{
    AutoMinPower, Listener, RelayFetch, RelayedSound, Sound, SoundPlayer, SoundRelay, RELAY_MAX_AGE,
};
use crate::hardware::{
    own_exe, spawn_poller, AuthState, Cmd, GpuProcess, PinReading, Poller, Sample, DEFAULT_POLL_MS,
    NUM_PINS,
};
use crate::persist;
use crate::web::{
    web_target, PowerDown, ServiceLink, WebData, WebServer, WebState, DEFAULT_WEB_BIND,
    DEFAULT_WEB_PORT, HISTORY_SECS,
};

/// Command-line flag that runs this binary as the system service.
pub(crate) const SERVICE_ARG: &str = "--service";
/// Command-line flag that runs this binary as the per-user sound agent.
pub(crate) const SOUND_AGENT_ARG: &str = "--sound-agent";
/// Installer modes (run as root through pkexec by the settings dialog).
pub(crate) const INSTALL_ARG: &str = "--install-system-service";
pub(crate) const UNINSTALL_ARG: &str = "--uninstall-system-service";
/// `--owner=<uid>`: the user the system service belongs to (see the link).
const OWNER_PREFIX: &str = "--owner=";

/// The system unit.
pub(crate) const UNIT_NAME: &str = "astral-gauge.service";
/// The sound agent's user unit.
pub(crate) const SOUND_UNIT: &str = "astral-gauge-sound.service";
/// The per-user service of earlier versions (same name, user manager); the
/// system service replaces it.
const LEGACY_UNIT: &str = "astral-gauge.service";

/// Install locations, relative to the file-system root (tests install into
/// a temporary directory instead).
const INSTALL_DIR: &str = "usr/local/lib/astral-gauge";
const EXE_NAME: &str = "astral-gauge";
const CONFIG_DIR: &str = "etc/astral-gauge";
const SETTINGS_NAME: &str = "settings.json";
const SYSTEM_UNIT_DIR: &str = "etc/systemd/system";
/// The link socket (its directory is the unit's `RuntimeDirectory`).
const SOCKET_PATH: &str = "/run/astral-gauge/link.sock";
/// The per-user service of earlier versions listened here, in
/// `$XDG_RUNTIME_DIR`.
const LEGACY_SOCKET_NAME: &str = "astral-gauge.sock";
/// Development overrides: run `--service` as a normal user on another socket
/// and settings file (`ASTRAL_GAUGE_SOCKET` also points the desktop app and
/// the sound agent at that socket).
const SOCKET_ENV: &str = "ASTRAL_GAUGE_SOCKET";
const SETTINGS_ENV: &str = "ASTRAL_GAUGE_SETTINGS";

/// Link protocol version: a desktop app only shows the data of a service
/// that speaks the same one (a rebuilt app next to a still-running older
/// service asks for a service update instead).
const LINK_VERSION: u32 = 2;
/// How often the desktop app fetches the service's state.
const VIEW_INTERVAL: Duration = Duration::from_millis(200);
/// How often the sound agent asks for new sounds.
const AGENT_POLL: Duration = Duration::from_millis(150);
/// How often the desktop app (and the sound agent) look for a service.
const SERVICE_CHECK: Duration = Duration::from_secs(1);
/// A link request that takes longer counts as a lost service.
const LINK_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the service checks the settings file for changes.
const SETTINGS_CHECK: Duration = Duration::from_secs(1);
/// Retry spacing for a web-server port that is busy (e.g. still held by
/// the desktop app that is handing the monitoring over).
const WEB_RETRY: Duration = Duration::from_secs(5);
/// Largest settings document accepted (over the link or the installer's
/// stdin).
const MAX_SETTINGS_BYTES: u64 = 64 * 1024;
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
        web_bind: DEFAULT_WEB_BIND.to_owned(),
    }
}

/// An absolute path from an environment variable.
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// The link socket: `/run/astral-gauge/link.sock`, or `$ASTRAL_GAUGE_SOCKET`.
fn socket_path() -> PathBuf {
    env_path(SOCKET_ENV).unwrap_or_else(|| PathBuf::from(SOCKET_PATH))
}

/// Where the per-user service of an earlier version listens (never while a
/// development socket is set: tests must not meet the real one).
fn legacy_socket_path() -> Option<PathBuf> {
    if env_path(SOCKET_ENV).is_some() {
        return None;
    }
    env_path("XDG_RUNTIME_DIR").map(|d| d.join(LEGACY_SOCKET_NAME))
}

/// The service's settings file: `/etc/astral-gauge/settings.json`, or
/// `$ASTRAL_GAUGE_SETTINGS`.
fn service_settings_path() -> PathBuf {
    env_path(SETTINGS_ENV).unwrap_or_else(|| Path::new("/").join(CONFIG_DIR).join(SETTINGS_NAME))
}

/// The executable the system service runs, under `root`.
fn installed_exe(root: &Path) -> PathBuf {
    root.join(INSTALL_DIR).join(EXE_NAME)
}

fn system_unit_path(root: &Path) -> PathBuf {
    root.join(SYSTEM_UNIT_DIR).join(UNIT_NAME)
}

/// `--owner=<uid>` from the command line.
fn owner_arg() -> Option<u32> {
    std::env::args().find_map(|a| a.strip_prefix(OWNER_PREFIX)?.parse().ok())
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// The user id of the process at the other end of a link connection.
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    // SAFETY: SO_PEERCRED fills a `ucred` of the given length; the buffer
    // is a properly sized and aligned local.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0).then_some(cred.uid)
}

// ---------------------------------------------------------------------------
// Unit files
// ---------------------------------------------------------------------------

/// `$XDG_CONFIG_HOME/systemd/user` (or `~/.config/systemd/user`): where
/// `systemctl --user` looks for the user's own units.
fn user_unit_dir() -> Option<PathBuf> {
    env_path("XDG_CONFIG_HOME")
        .or_else(|| env_path("HOME").map(|h| h.join(".config")))
        .map(|c| c.join("systemd").join("user"))
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

/// The system unit running `exe` as the service of user `owner` (with the
/// I2C bus the app was started with, if one was given explicitly).
fn system_unit_text(exe: &Path, owner: u32, bus: Option<u32>) -> String {
    let mut exec = format!(
        "{} {SERVICE_ARG} {OWNER_PREFIX}{owner}",
        exec_quote(&exe.to_string_lossy())
    );
    if let Some(bus) = bus {
        let _ = write!(exec, " {bus}");
    }
    format!(
        "# Installed by AstralGauge (Settings > Background service); uninstall it there.
[Unit]
Description=AstralGauge 12VHPWR pin-current monitor
# Ordering only: the service retries the GPU's I2C bus and NVML itself
# until the driver is up.
After=systemd-modules-load.service nvidia-persistenced.service
# Keep restarting a monitor that failed, however often.
StartLimitIntervalSec=0

[Service]
Type=simple
ExecStart={exec}
Restart=always
RestartSec=2
# The link socket's directory (/run/astral-gauge).
RuntimeDirectory=astral-gauge
RuntimeDirectoryMode=0755
# Runs as root (I2C, the NVML power limit, the safeguard's kill) but needs
# to write nothing except its settings.
ProtectSystem=full
ReadWritePaths=-/etc/astral-gauge
ProtectHome=yes
PrivateTmp=yes
NoNewPrivileges=yes

[Install]
WantedBy=multi-user.target
"
    )
}

/// The sound agent's user unit.
fn sound_unit_text(exe: &Path) -> String {
    format!(
        "# Installed by AstralGauge (Settings > Background service); uninstall it there.
[Unit]
Description=AstralGauge alert sounds (plays the system service's alerts in this session)
# The sounds go through the session's sound server: start after it. Ordering
# only — the agent retries the audio output and the service link itself.
After=pipewire.service pipewire-pulse.service wireplumber.service
StartLimitIntervalSec=0

[Service]
Type=simple
ExecStart={} {SOUND_AGENT_ARG}
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
",
        exec_quote(&exe.to_string_lossy())
    )
}

// ---------------------------------------------------------------------------
// Installer (root, through pkexec)
// ---------------------------------------------------------------------------

/// Write one file atomically (a sibling temporary file, then rename) with
/// the given mode.
fn write_file(path: &Path, contents: &[u8], mode: u32) -> Result<(), String> {
    let dir = path.parent().ok_or("bad path")?;
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let tmp = path.with_extension("new");
    fs::write(&tmp, contents)
        .and_then(|()| fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)))
        .and_then(|()| fs::rename(&tmp, path))
        .map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("cannot write {}: {e}", path.display())
        })
}

/// Put the service's files under `root`: the executable (a copy of `exe`),
/// its settings and the unit.
fn install_files(
    root: &Path,
    exe: &Path,
    owner: u32,
    bus: Option<u32>,
    settings: &persist::StoredSettings,
) -> Result<(), String> {
    let target = installed_exe(root);
    let bytes = fs::read(exe).map_err(|e| format!("cannot read {}: {e}", exe.display()))?;
    write_file(&target, &bytes, 0o755)?;
    let settings_path = root.join(CONFIG_DIR).join(SETTINGS_NAME);
    persist::save_to(&settings_path, settings)?;
    let unit = system_unit_text(&installed_exe(Path::new("/")), owner, bus);
    write_file(&system_unit_path(root), unit.as_bytes(), 0o644)
}

/// Remove the service's files under `root` (missing ones are fine).
fn remove_files(root: &Path) -> Result<(), String> {
    let remove = |path: PathBuf, dir: bool| {
        let r = if dir {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match r {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("cannot remove {}: {e}", path.display()))
            }
            _ => Ok(()),
        }
    };
    remove(system_unit_path(root), false)?;
    remove(root.join(INSTALL_DIR), true)?;
    remove(root.join(CONFIG_DIR), true)
}

/// Run `systemctl <args>` (the system manager): its stdout on success, the
/// error text otherwise.
fn systemctl(user: bool, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("systemctl");
    if user {
        cmd.arg("--user");
    }
    let out = cmd
        .args(args)
        .output()
        .map_err(|e| format!("cannot run systemctl: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(format!(
            "systemctl{} {} failed: {}",
            if user { " --user" } else { "" },
            args.join(" "),
            if stderr.is_empty() { stdout } else { stderr }
        ))
    }
}

/// Installer mode (`--install-system-service --owner=<uid> [bus]`, as root):
/// the settings document arrives on stdin. Installs (or updates) the system
/// service and (re)starts it.
pub(crate) fn run_install(explicit_bus: Option<u32>) -> i32 {
    let result = (|| -> Result<(), String> {
        if !running_as_root() {
            return Err("the installer must run as root".to_owned());
        }
        let owner = owner_arg().ok_or("no --owner=<uid> given")?;
        let mut text = String::new();
        std::io::stdin()
            .take(MAX_SETTINGS_BYTES)
            .read_to_string(&mut text)
            .map_err(|e| format!("cannot read the settings: {e}"))?;
        let settings = persist::parse(&text, &default_settings())
            .map_err(|e| format!("bad settings document: {e}"))?;
        // The running executable itself, even if its file was replaced.
        install_files(
            Path::new("/"),
            Path::new("/proc/self/exe"),
            owner,
            explicit_bus,
            &settings,
        )?;
        systemctl(false, &["daemon-reload"])?;
        systemctl(false, &["enable", UNIT_NAME])?;
        // Restart, not start: an update (new executable) takes effect too.
        systemctl(false, &["restart", UNIT_NAME])?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            println!("System service installed and started.");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// Uninstaller mode (`--uninstall-system-service`, as root).
pub(crate) fn run_uninstall() -> i32 {
    let result = (|| -> Result<(), String> {
        if !running_as_root() {
            return Err("the uninstaller must run as root".to_owned());
        }
        if system_unit_path(Path::new("/")).exists() {
            systemctl(false, &["disable", "--now", UNIT_NAME])?;
        }
        remove_files(Path::new("/"))?;
        systemctl(false, &["daemon-reload"])?;
        // Forget a failed state, so nothing about the unit is left listed.
        let _ = systemctl(false, &["reset-failed", UNIT_NAME]);
        Ok(())
    })();
    match result {
        Ok(()) => {
            println!("System service uninstalled.");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// Run this executable as root through pkexec (one administrator prompt)
/// with `args`, feeding `input` on stdin.
fn pkexec_self(args: &[String], input: &str) -> Result<(), String> {
    let exe = own_exe()?;
    let mut child = Command::new("pkexec")
        .arg(&exe)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run pkexec ({e}); is polkit installed?"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("pkexec: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    Err(match out.status.code() {
        // pkexec: the authentication was dismissed or not granted.
        Some(126 | 127) => "The administrator authentication was not given.".to_owned(),
        _ if stderr.is_empty() => format!("the installer failed ({})", out.status),
        _ => stderr,
    })
}

fn user_unit_path(name: &str) -> Option<PathBuf> {
    user_unit_dir().map(|d| d.join(name))
}

/// Install (or update) the sound agent's user unit and (re)start it.
fn install_sound_agent() -> Result<(), String> {
    let path = user_unit_path(SOUND_UNIT).ok_or("no home directory")?;
    write_file(
        &path,
        sound_unit_text(&installed_exe(Path::new("/"))).as_bytes(),
        0o644,
    )?;
    systemctl(true, &["daemon-reload"])?;
    systemctl(true, &["enable", SOUND_UNIT])?;
    systemctl(true, &["restart", SOUND_UNIT])?;
    Ok(())
}

/// Stop and remove a user unit (missing is fine).
fn remove_user_unit(name: &str) -> Result<(), String> {
    let path = user_unit_path(name).ok_or("no home directory")?;
    if path.exists() {
        systemctl(true, &["disable", "--now", name])?;
        fs::remove_file(&path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
        systemctl(true, &["daemon-reload"])?;
    }
    let _ = systemctl(true, &["reset-failed", name]);
    Ok(())
}

/// Install (or update) and start the system service for this executable
/// (one administrator prompt), with these settings, replacing the per-user
/// service of an earlier version; and install or remove the sound agent.
pub(crate) fn install(
    bus: Option<u32>,
    settings: &persist::StoredSettings,
    sound_agent: bool,
) -> Result<String, String> {
    // The old per-user service would keep monitoring next to the new one:
    // stop it first (and start it again if the installation fails).
    let legacy = user_unit_path(LEGACY_UNIT).is_some_and(|p| p.exists());
    if legacy {
        let _ = systemctl(true, &["stop", LEGACY_UNIT]);
    }
    let mut args = vec![
        INSTALL_ARG.to_owned(),
        format!("{OWNER_PREFIX}{}", current_uid()),
    ];
    if let Some(bus) = bus {
        args.push(bus.to_string());
    }
    if let Err(e) = pkexec_self(&args, &persist::to_json(settings)) {
        if legacy {
            let _ = systemctl(true, &["start", LEGACY_UNIT]);
        }
        return Err(e);
    }
    println!("System service installed.");
    if legacy {
        remove_user_unit(LEGACY_UNIT)?;
        println!("Removed the per-user service of the earlier version.");
    }
    if sound_agent {
        install_sound_agent()?;
    } else {
        remove_user_unit(SOUND_UNIT)?;
    }
    Ok(if legacy {
        "System service installed and started (it replaces the earlier per-user service)."
    } else {
        "System service installed and started."
    }
    .to_owned())
}

/// Stop and remove the system service (one administrator prompt) and the
/// sound agent.
pub(crate) fn uninstall() -> Result<String, String> {
    remove_user_unit(SOUND_UNIT)?;
    pkexec_self(&[UNINSTALL_ARG.to_owned()], "")?;
    println!("System service uninstalled.");
    Ok("Service stopped and uninstalled.".to_owned())
}

/// Restart the service: asked over the link (no prompt; systemd starts it
/// again), or through systemctl when it does not answer (which asks for
/// the administrator password itself). Also restarts the sound agent.
pub(crate) fn restart() -> Result<String, String> {
    let over_link = UnixStream::connect(socket_path()).ok().and_then(|stream| {
        let _ = stream.set_read_timeout(Some(LINK_TIMEOUT));
        let mut writer = stream.try_clone().ok()?;
        let mut reader = BufReader::new(stream);
        let reply = request(&mut writer, &mut reader, "RESTART", None).ok()?;
        (reply.first().map(String::as_str) == Some("ok")).then_some(())
    });
    if over_link.is_none() {
        systemctl(false, &["restart", UNIT_NAME])?;
    }
    if user_unit_path(SOUND_UNIT).is_some_and(|p| p.exists()) {
        systemctl(true, &["restart", SOUND_UNIT])?;
    }
    Ok("Service restarted.".to_owned())
}

/// Install or remove only the sound agent.
pub(crate) fn set_sound_agent(on: bool) -> Result<String, String> {
    if on {
        install_sound_agent()?;
        Ok("Sound agent installed: alert sounds play in your session.".to_owned())
    } else {
        remove_user_unit(SOUND_UNIT)?;
        Ok("Sound agent removed.".to_owned())
    }
}

/// Whether two files have the same contents (the installed copy of the
/// executable against this one). Cached by size and modification time:
/// the settings dialog asks every two seconds.
fn same_file(a: &Path, b: &Path) -> bool {
    type Key = (u64, Option<SystemTime>);
    static CACHE: Mutex<Option<(Key, Key, bool)>> = Mutex::new(None);
    let key = |p: &Path| fs::metadata(p).ok().map(|m| (m.len(), m.modified().ok()));
    let (Some(ka), Some(kb)) = (key(a), key(b)) else {
        return false;
    };
    if ka.0 != kb.0 {
        return false;
    }
    let mut cache = CACHE.lock().unwrap();
    if let Some((ca, cb, same)) = *cache {
        if ca == ka && cb == kb {
            return same;
        }
    }
    let same = matches!((fs::read(a), fs::read(b)), (Ok(x), Ok(y)) if x == y);
    *cache = Some((ka, kb, same));
    same
}

/// The services as systemd sees them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UnitStatus {
    /// The system unit file exists.
    pub(crate) installed: bool,
    /// The installed service is exactly what [`install`] would put there
    /// now (same executable, owner and bus); otherwise reinstalling updates
    /// it.
    pub(crate) current: bool,
    /// Starts at boot.
    pub(crate) enabled: bool,
    /// `systemctl is-active`: active, inactive, failed, activating, ...
    pub(crate) active: String,
    /// The sound agent's user unit file exists.
    pub(crate) sound_agent: bool,
    /// Its `systemctl --user is-active` state.
    pub(crate) sound_agent_active: String,
    /// The per-user service of an earlier version is still installed.
    pub(crate) legacy: bool,
}

/// Query the units' state (blocking: runs `systemctl`).
pub(crate) fn unit_status(bus: Option<u32>) -> Result<UnitStatus, String> {
    let root = Path::new("/");
    let installed_text = fs::read_to_string(system_unit_path(root)).ok();
    let current = match (&installed_text, own_exe()) {
        (Some(text), Ok(exe)) => {
            *text == system_unit_text(&installed_exe(root), current_uid(), bus)
                && same_file(&exe, &installed_exe(root))
        }
        _ => false,
    };
    // Both print the state on stdout and fail (non-zero) for most states.
    let query = |user: bool, verb: &str, unit: &str| -> Result<String, String> {
        let mut cmd = Command::new("systemctl");
        if user {
            cmd.arg("--user");
        }
        let out = cmd
            .args([verb, unit])
            .output()
            .map_err(|e| format!("cannot run systemctl: {e}"))?;
        let state = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if state.is_empty() && !out.status.success() {
            // No state at all: systemctl itself failed.
            let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            return Err(format!("systemctl {verb}: {err}"));
        }
        Ok(state)
    };
    let sound_agent = user_unit_path(SOUND_UNIT).is_some_and(|p| p.exists());
    Ok(UnitStatus {
        installed: installed_text.is_some(),
        current,
        enabled: query(false, "is-enabled", UNIT_NAME)? == "enabled",
        active: query(false, "is-active", UNIT_NAME)?,
        sound_agent,
        sound_agent_active: if sound_agent {
            query(true, "is-active", SOUND_UNIT).unwrap_or_else(|e| e)
        } else {
            String::new()
        },
        legacy: user_unit_path(LEGACY_UNIT).is_some_and(|p| p.exists()),
    })
}

/// What the settings dialog asks the service management to do.
#[derive(Clone)]
pub(crate) enum ServiceAction {
    /// Install or update the system service with these settings, with or
    /// without the sound agent.
    Install(Box<persist::StoredSettings>, bool),
    Uninstall,
    Restart,
    SoundAgent(bool),
}

/// Unit state and the last action's result, as shown in the settings
/// dialog. `systemctl` and pkexec run on short-lived threads, never on the
/// GUI thread.
#[derive(Default)]
pub(crate) struct ServiceControl {
    /// Latest unit status query (`None` before the first one finished).
    pub(crate) status: Option<Result<UnitStatus, String>>,
    /// An action is running.
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

/// Run a service action in the background.
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
            ServiceAction::Install(settings, agent) => install(bus, &settings, agent),
            ServiceAction::Uninstall => uninstall(),
            ServiceAction::Restart => restart(),
            ServiceAction::SoundAgent(on) => set_sound_agent(on),
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

/// What the link threads share.
struct LinkShared {
    data: Arc<Mutex<WebData>>,
    tx: mpsc::Sender<Cmd>,
    relay: Arc<SoundRelay>,
    /// The user (besides root) whose requests may change something.
    owner: u32,
    settings_path: PathBuf,
}

/// Run as the system service (`--service [--owner=<uid>] [bus]`); never
/// returns normally (systemd stops it with SIGTERM). Returns an exit code
/// when it cannot start, e.g. because another instance already runs.
pub(crate) fn run_service(explicit_bus: Option<u32>) -> i32 {
    let owner = owner_arg().unwrap_or_else(current_uid);
    println!(
        "AstralGauge {} running as the {} service (owner uid {owner}).",
        env!("CARGO_PKG_VERSION"),
        if running_as_root() {
            "system"
        } else {
            "background"
        }
    );
    // The safeguard's default kill reaches the owner's processes only (the
    // opt-in cross-user kill reaches everyone's).
    crate::hardware::set_safeguard_user(owner);

    let path = socket_path();
    if UnixStream::connect(&path).is_ok() {
        eprintln!("Another AstralGauge service is already running.");
        return 1;
    }
    // A socket file left by a service that died: replace it.
    let _ = fs::remove_file(&path);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(e) => {
            // Without its link the service would monitor unseen next to a
            // desktop app doing the same: refuse to run instead.
            eprintln!("Cannot create {}: {e}.", path.display());
            return 1;
        }
    };
    // Every local user may connect (and see the readings); requests that
    // change something are checked against the peer's uid.
    if let Err(e) = fs::set_permissions(&path, fs::Permissions::from_mode(0o666)) {
        eprintln!("Cannot open {} to local users: {e}.", path.display());
    }

    let data = Arc::new(Mutex::new(WebData::new()));
    {
        let mut d = data.lock().unwrap();
        d.service_mode = true;
        d.service_link = Some(ServiceLink::default());
        d.local_monitor = true;
    }
    // No sound output outside a login session: the sounds go to the relay,
    // where the sound agent (or the desktop app) picks them up.
    let (player, relay) = SoundPlayer::relay();
    let poller = spawn_poller(explicit_bus, data.clone(), player);
    let settings_path = service_settings_path();
    let shared = Arc::new(LinkShared {
        data: data.clone(),
        tx: poller.tx.clone(),
        relay,
        owner,
        settings_path: settings_path.clone(),
    });
    thread::spawn(move || serve_link(listener, shared));
    supervise_settings(&data, &poller.tx, &settings_path)
}

/// Whether these settings use the privileged helper (root via pkexec).
fn privileged(s: &persist::StoredSettings) -> bool {
    s.alerts.auto_min_power != AutoMinPower::Off || s.alerts.kill_other_users_processes
}

/// The service's main loop: (re)load the settings file whenever it changes
/// and keep the web server in line with it (retrying a busy port).
fn supervise_settings(
    data: &Arc<Mutex<WebData>>,
    tx: &mpsc::Sender<Cmd>,
    settings_path: &Path,
) -> ! {
    let defaults = default_settings();
    let mut applied: Option<persist::StoredSettings> = None;
    let mut revision: u64 = 0;
    // Modification time of the settings file when it was last read (outer
    // None: never read).
    let mut stamp: Option<Option<SystemTime>> = None;
    let mut web = WebState::Stopped;
    let mut web_error: Option<String> = None;
    let mut web_retry_at = Instant::now();
    loop {
        let mtime = fs::metadata(settings_path)
            .ok()
            .and_then(|m| m.modified().ok());
        if stamp != Some(mtime) {
            stamp = Some(mtime);
            let loaded = match persist::load_from(settings_path, &defaults) {
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
                revision += 1;
                {
                    let mut d = data.lock().unwrap();
                    d.settings = loaded.alerts.clone();
                    d.poll_ms = loaded.poll_ms;
                    d.service_settings = Some((revision, persist::to_json(&loaded)));
                }
                if applied.as_ref().map(|s| s.poll_ms) != Some(loaded.poll_ms) {
                    let _ = tx.send(Cmd::Interval(loaded.poll_ms));
                }
                // Newly enabled privileged options: make sure the helper is
                // available (as root that is immediate; a development
                // service run as a normal user asks through pkexec).
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
        // desktop app may still be letting go of it) or a listen address
        // that is not up yet (network still starting) is retried.
        match (
            settings.web_enabled,
            web_target(&settings.web_bind, &settings.web_port),
        ) {
            (false, _) => {
                if matches!(web, WebState::Running(_)) {
                    web = WebState::Stopped;
                }
                web_error = None;
            }
            (true, Err(e)) => {
                web = WebState::Stopped;
                web_error = Some(e);
            }
            (true, Ok(addr)) => {
                let serving = matches!(&web, WebState::Running(s) if s.addr == addr);
                if !serving && Instant::now() >= web_retry_at {
                    web = WebState::Stopped;
                    match WebServer::start(addr, data.clone()) {
                        Ok(server) => {
                            web = WebState::Running(server);
                            web_error = None;
                        }
                        Err(e) => {
                            let msg = format!("cannot listen on {addr}: {e}");
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
            (link.web_port, link.web_bind) = match &web {
                WebState::Running(server) => (Some(server.addr.port()), Some(server.addr.ip())),
                WebState::Stopped => (None, None),
            };
            link.web_error = web_error.clone();
        }
        thread::sleep(SETTINGS_CHECK);
    }
}

/// Accept connections on the link socket.
fn serve_link(listener: UnixListener, shared: Arc<LinkShared>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let shared = shared.clone();
                thread::spawn(move || link_client(stream, &shared));
            }
            Err(e) => {
                eprintln!("Link socket: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Answer one client's requests until it disconnects.
fn link_client(stream: UnixStream, shared: &LinkShared) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    // Root and the owner may change things; everyone else only looks.
    let trusted = peer_uid(&stream).is_some_and(|uid| uid == 0 || uid == shared.owner);
    let mut writer = stream;
    let mut reader = BufReader::new(read_half);
    loop {
        // One request line, bounded (any local user can connect): the
        // largest is a settings document, escaped.
        let mut line = String::new();
        match (&mut reader)
            .take(2 * MAX_SETTINGS_BYTES)
            .read_line(&mut line)
        {
            Ok(0) | Err(_) => break,
            Ok(_) if !line.ends_with('\n') => break, // too long (or cut off)
            Ok(_) => {}
        }
        let line = line.trim_end_matches(['\n', '\r']);
        let (verb, arg) = line.split_once(' ').unwrap_or((line, ""));
        let mut restart = false;
        let reply = match verb {
            "HELLO" => format!("HELLO {LINK_VERSION}\n"),
            "SYNC" => {
                let mut words = arg.split(' ');
                let since = words.next().and_then(|w| w.parse::<f64>().ok());
                let known_rev = words.next().and_then(|w| w.parse::<u64>().ok());
                let reply = sync_reply(&shared.data.lock().unwrap(), since, known_rev);
                encode_sync(&reply)
            }
            "SOUNDS" => {
                let mut words = arg.split(' ');
                let cursor = words.next().and_then(|w| w.parse::<u64>().ok());
                let playing = words.next() == Some("1");
                let who = match words.next() {
                    Some("agent") => Listener::Agent,
                    _ => Listener::App,
                };
                encode_sounds(&shared.relay.fetch(who, cursor, playing, Instant::now()))
            }
            "ACK" => {
                // Anyone who sees the alert may acknowledge it (like the
                // web page's POST /ack).
                shared.data.lock().unwrap().critical_alert = false;
                "ok\n".to_owned()
            }
            "PING" => "ok\n".to_owned(),
            "LIMIT" | "AUTH" | "SETTINGS" | "RESTART" if !trusted => {
                "err only the service's owner can do that\n".to_owned()
            }
            "LIMIT" => match arg.parse::<u32>() {
                Ok(watts) => {
                    let _ = shared.tx.send(Cmd::SetLimit(watts));
                    "ok\n".to_owned()
                }
                Err(_) => "err bad limit\n".to_owned(),
            },
            "AUTH" => {
                // Pending until the helper answers; never the stale state of
                // an earlier attempt (see the poller's auth mirroring).
                shared.data.lock().unwrap().power_auth = AuthState::Idle;
                let _ = shared.tx.send(Cmd::GrantPowerAccess);
                "ok\n".to_owned()
            }
            "SETTINGS" => {
                // Validated like the file itself; the settings loop applies
                // it within a second.
                let text = unescape(arg);
                match persist::parse(&text, &default_settings())
                    .and_then(|s| persist::save_to(&shared.settings_path, &s))
                {
                    Ok(()) => "ok\n".to_owned(),
                    Err(e) => format!("err {}\n", escape(&e)),
                }
            }
            "RESTART" => {
                restart = true;
                "ok\n".to_owned()
            }
            _ => "err unknown request\n".to_owned(),
        };
        if writer.write_all(reply.as_bytes()).is_err() {
            break;
        }
        if restart {
            // systemd starts the service again (Restart=always).
            println!("Restart requested over the link.");
            let _ = writer.flush();
            std::process::exit(0);
        }
    }
}

// ---------------------------------------------------------------------------
// Sound relay over the link
// ---------------------------------------------------------------------------

/// Encode a SOUNDS reply: one `sound <seq> <age ms> <0|1 priority> embedded
/// <key>|file <escaped path>` line per sound, then `next <cursor>`, `agent
/// <0|1>` and `end`.
fn encode_sounds(f: &RelayFetch) -> String {
    let mut o = String::new();
    for s in &f.sounds {
        let what = match &s.sound {
            Sound::Embedded(e) => format!("embedded {}", persist::embedded_key(*e)),
            Sound::File(path) => format!("file {}", escape(&path.to_string_lossy())),
        };
        let _ = writeln!(
            o,
            "sound {} {} {} {what}",
            s.seq,
            s.age.as_millis(),
            u8::from(s.priority)
        );
    }
    let _ = writeln!(o, "next {}", f.next);
    let _ = writeln!(o, "agent {}", u8::from(f.agent));
    o.push_str("end\n");
    o
}

/// Decode the lines of a SOUNDS reply (without `end`).
fn decode_sounds(lines: &[String]) -> Result<RelayFetch, String> {
    let mut f = RelayFetch {
        sounds: Vec::new(),
        next: 0,
        agent: false,
    };
    let mut have_next = false;
    for line in lines {
        let (key, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        match key {
            "sound" => {
                let mut w = rest.splitn(5, ' ');
                let bad = || format!("SOUNDS: bad line {line:?}");
                let seq = w.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
                let age_ms: u64 = w.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
                let priority = w.next() == Some("1");
                let sound = match (w.next(), w.next()) {
                    (Some("embedded"), Some(k)) => {
                        Sound::Embedded(persist::embedded_from_key(k).ok_or_else(bad)?)
                    }
                    (Some("file"), Some(p)) => Sound::File(PathBuf::from(unescape(p))),
                    // A kind this build does not know: skip it.
                    _ => continue,
                };
                f.sounds.push(RelayedSound {
                    seq,
                    sound,
                    priority,
                    age: Duration::from_millis(age_ms),
                });
            }
            "next" => {
                f.next = rest.parse().map_err(|_| "SOUNDS: bad cursor")?;
                have_next = true;
            }
            "agent" => f.agent = rest == "1",
            _ => {}
        }
    }
    if !have_next {
        return Err("SOUNDS: no cursor".to_owned());
    }
    Ok(f)
}

/// Play relayed sounds that are still timely.
fn play_relayed(player: &SoundPlayer, sounds: Vec<RelayedSound>) {
    for s in sounds {
        if s.age > RELAY_MAX_AGE {
            continue;
        }
        if s.priority {
            player.play_priority(s.sound);
        } else {
            player.play(s.sound);
        }
    }
}

/// Sound-agent mode (`--sound-agent`, the user unit): play the system
/// service's alert sounds in this login session. Never returns; it waits
/// for the service (and the audio output) as long as it takes.
pub(crate) fn run_sound_agent() -> i32 {
    println!("AstralGauge sound agent: plays the system service's alert sounds.");
    let player = SoundPlayer::spawn();
    let mut last_problem: Option<String> = None;
    loop {
        let why = agent_session(&player, &mut last_problem);
        if last_problem.as_ref() != Some(&why) {
            eprintln!("{why} (retrying).");
            last_problem = Some(why);
        }
        thread::sleep(SERVICE_CHECK);
    }
}

/// One connection to the service: fetch and play sounds until it breaks.
fn agent_session(player: &SoundPlayer, last_problem: &mut Option<String>) -> String {
    let path = socket_path();
    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(e) => return format!("The service is not running ({}: {e})", path.display()),
    };
    let _ = stream.set_read_timeout(Some(LINK_TIMEOUT));
    let _ = stream.set_write_timeout(Some(LINK_TIMEOUT));
    let Ok(mut writer) = stream.try_clone() else {
        return "Cannot use the link socket".to_owned();
    };
    let mut reader = BufReader::new(stream);
    let hello = match request(
        &mut writer,
        &mut reader,
        &format!("HELLO {LINK_VERSION}"),
        None,
    ) {
        Ok(mut lines) => lines.remove(0),
        Err(e) => return e,
    };
    if hello != format!("HELLO {LINK_VERSION}") {
        thread::sleep(Duration::from_secs(5));
        return format!(
            "The service is a different build ({hello}); restart this agent after updating it"
        );
    }
    println!("Connected to the AstralGauge service.");
    *last_problem = None;
    let mut cursor: Option<u64> = None;
    loop {
        let line = format!(
            "SOUNDS {} {} agent",
            opt(cursor),
            u8::from(player.priority_sound_playing())
        );
        let fetched = match request(&mut writer, &mut reader, &line, Some("end"))
            .and_then(|lines| decode_sounds(&lines))
        {
            Ok(f) => f,
            Err(e) => return format!("Lost the service ({e})"),
        };
        play_relayed(player, fetched.sounds);
        cursor = Some(fetched.next);
        thread::sleep(AGENT_POLL);
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
    web_bind: Option<std::net::IpAddr>,
    web_error: Option<String>,
    power_down: Option<PowerDown>,
    /// The service's settings (revision, JSON document), when the client's
    /// copy is older.
    settings: Option<(u64, String)>,
    sample: Sample,
    /// [`SERIES`] histories of (time, value), oldest first.
    series: Vec<Vec<(f64, f64)>>,
}

/// Capture the state for a SYNC reply (under the data lock: copies only).
/// `known_rev`: the settings revision the client already has.
fn sync_reply(d: &WebData, since: Option<f64>, known_rev: Option<u64>) -> SyncReply {
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
        web_bind: link.web_bind,
        web_error: link.web_error,
        power_down: d.power_down.clone(),
        settings: d
            .service_settings
            .clone()
            .filter(|(rev, _)| Some(*rev) != known_rev),
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
    if let Some(bind) = r.web_bind {
        let _ = writeln!(o, "web_bind {bind}");
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
    if let Some((rev, json)) = &r.settings {
        let _ = writeln!(o, "settings {rev} {}", escape(json));
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
            "web_bind" => r.web_bind = num(words.next()).map_err(bad)?,
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
            "settings" => {
                let (rev, json) = rest.split_once(' ').unwrap_or((rest, ""));
                let rev = num(Some(rev))
                    .map_err(bad)?
                    .ok_or("SYNC settings: no revision")?;
                r.settings = Some((rev, unescape(json)));
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
    if r.settings.is_some() {
        d.service_settings = r.settings;
    }
    d.service_link = Some(ServiceLink {
        web_port: r.web_port,
        web_bind: r.web_bind,
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
/// `player` plays the local poller's sounds, and the service's while no
/// sound agent plays them.
pub(crate) fn spawn_monitor(
    explicit_bus: Option<u32>,
    data: Arc<Mutex<WebData>>,
    player: SoundPlayer,
) -> mpsc::Sender<Cmd> {
    let (tx, rx) = mpsc::channel::<Cmd>();
    thread::spawn(move || {
        let mut local: Option<Poller> = None;
        loop {
            // The system service; or the per-user service of an earlier
            // version, which only gets an "update it" notice (it speaks an
            // older link version) but must not get a local poller next to it.
            let found = UnixStream::connect(socket_path())
                .ok()
                .map(|s| (s, false))
                .or_else(|| {
                    legacy_socket_path()
                        .and_then(|p| UnixStream::connect(p).ok())
                        .map(|s| (s, true))
                });
            if let Some((stream, legacy)) = found {
                match local.take() {
                    Some(poller) => {
                        println!("Background service started: it takes over the monitoring.");
                        poller.stop();
                    }
                    None => println!("Background service running: showing its data."),
                }
                data.lock().unwrap().local_monitor = false;
                let end = view_service(stream, legacy, &data, &rx, &player);
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
/// or the app closes (`Ok`). `legacy`: connected to the per-user service of
/// an earlier version (left as soon as the system service answers).
fn view_service(
    stream: UnixStream,
    legacy: bool,
    data: &Arc<Mutex<WebData>>,
    rx: &mpsc::Receiver<Cmd>,
    player: &SoundPlayer,
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
        // next to it) and say so until it is updated or stops.
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
            // The system service appeared (the old one is still running
            // until the installation stops it): switch over.
            if legacy && UnixStream::connect(socket_path()).is_ok() {
                return Err("a newer service started".to_owned());
            }
        }
    }

    let mut since: Option<f64> = None;
    let mut sound_cursor: Option<u64> = None;
    let mut pending: Vec<Cmd> = Vec::new();
    loop {
        for cmd in pending.drain(..) {
            let line = match cmd {
                Cmd::SetLimit(watts) => format!("LIMIT {watts}"),
                Cmd::GrantPowerAccess => "AUTH".to_owned(),
                Cmd::AckCritical => "ACK".to_owned(),
                Cmd::Settings(json) => format!("SETTINGS {}", escape(&json)),
                // The service applies the interval from its settings.
                Cmd::Interval(_) => continue,
            };
            let reply = request(&mut writer, &mut reader, &line, None)?.remove(0);
            if let Some(why) = reply.strip_prefix("err ") {
                let verb = line.split(' ').next().unwrap_or_default();
                eprintln!("The service refused {verb}: {why}");
                data.lock().unwrap().service_error = Some(format!(
                    "The background service did not accept the {}: {}",
                    match verb {
                        "SETTINGS" => "settings",
                        "LIMIT" => "power limit",
                        "AUTH" => "authentication request",
                        _ => "request",
                    },
                    unescape(why)
                ));
            }
        }
        let known_rev = data.lock().unwrap().service_settings.as_ref().map(|s| s.0);
        let lines = request(
            &mut writer,
            &mut reader,
            &format!("SYNC {} {}", opt(since), opt(known_rev)),
            Some("end"),
        )?;
        // The service's alert sounds: played here while no sound agent
        // plays them in this session.
        let sounds = request(
            &mut writer,
            &mut reader,
            &format!(
                "SOUNDS {} {} app",
                opt(sound_cursor),
                u8::from(player.priority_sound_playing())
            ),
            Some("end"),
        )
        .and_then(|lines| decode_sounds(&lines))?;
        if !sounds.agent {
            play_relayed(player, sounds.sounds);
        }
        sound_cursor = Some(sounds.next);
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
            web_bind: Some("10.0.0.12".parse().unwrap()),
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
        let text = encode_sync(&sync_reply(&service, None, None));
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
        let link = app.service_link.as_ref().unwrap();
        assert_eq!(link.web_port, Some(8666));
        assert_eq!(link.web_bind, Some("10.0.0.12".parse().unwrap()));
        // The app's clock now runs on the service's.
        let skew = app.start.elapsed().as_secs_f64() - service.start.elapsed().as_secs_f64();
        assert!(skew.abs() < 0.05, "clock skew {skew}");
    }

    #[test]
    fn incremental_sync_appends_only_new_points() {
        let mut service = sample_data();
        let mut app = WebData::new();
        let first = decode_sync(&lines(&encode_sync(&sync_reply(&service, None, None)))).unwrap();
        let since = first.sampled_at;
        apply_sync(&mut app, first, true, false, false);

        let mut next = service.sample.clone();
        next.pins = Vec::new(); // a sensor fault: a gap, not zeros
        service.record(next);
        let delta = sync_reply(&service, since, None);
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
    fn unit_files_run_the_installed_executable() {
        let text = system_unit_text(Path::new("/opt/my apps/astral-gauge"), 1000, Some(15));
        assert!(
            text.contains("ExecStart=\"/opt/my apps/astral-gauge\" --service --owner=1000 15\n")
        );
        assert!(text.contains("Restart=always"));
        assert!(text.contains("RuntimeDirectory=astral-gauge\n"));
        assert!(text.contains("ReadWritePaths=-/etc/astral-gauge\n"));
        assert!(text.contains("WantedBy=multi-user.target"));
        let text = system_unit_text(Path::new("/x/100%$\"a\\b"), 0, None);
        assert!(text.contains("ExecStart=\"/x/100%%$$\\\"a\\\\b\" --service --owner=0\n"));
        let agent = sound_unit_text(&installed_exe(Path::new("/")));
        assert!(agent
            .contains("ExecStart=\"/usr/local/lib/astral-gauge/astral-gauge\" --sound-agent\n"));
        assert!(agent.contains("WantedBy=default.target"));
        // A bus number given as an owner flag never reaches the bus slot.
        assert!("--owner=1000".parse::<u32>().is_err());
    }

    #[test]
    fn installer_puts_and_removes_the_files() {
        let root = std::env::temp_dir().join(format!("ag-install-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let exe = root.join("build").join("astral-gauge");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"binary").unwrap();
        let mut settings = default_settings();
        settings.alerts.red_limit_a = 9.5;
        settings.web_enabled = true;

        install_files(&root, &exe, 1000, Some(3), &settings).unwrap();
        let installed = installed_exe(&root);
        assert_eq!(fs::read(&installed).unwrap(), b"binary");
        let mode = fs::metadata(&installed).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(same_file(&exe, &installed));
        let saved = persist::load_from(
            &root.join(CONFIG_DIR).join(SETTINGS_NAME),
            &default_settings(),
        )
        .unwrap();
        assert_eq!(saved, Some(settings));
        let unit = fs::read_to_string(system_unit_path(&root)).unwrap();
        // The unit names the final location, not the staging root.
        assert_eq!(
            unit,
            system_unit_text(&installed_exe(Path::new("/")), 1000, Some(3))
        );

        remove_files(&root).unwrap();
        assert!(!installed.exists());
        assert!(!root.join(CONFIG_DIR).exists());
        assert!(!system_unit_path(&root).exists());
        remove_files(&root).unwrap(); // already gone: fine
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn relayed_sounds_round_trip() {
        let fetch = RelayFetch {
            sounds: vec![
                RelayedSound {
                    seq: 7,
                    sound: Sound::Embedded(crate::alerts::EmbeddedSound::CriticalPoweringDown),
                    priority: false,
                    age: Duration::from_millis(120),
                },
                RelayedSound {
                    seq: 8,
                    sound: Sound::File(PathBuf::from("/home/u/my sounds/kill\\it\n.wav")),
                    priority: true,
                    age: Duration::ZERO,
                },
            ],
            next: 9,
            agent: true,
        };
        let back = decode_sounds(&lines(&encode_sounds(&fetch))).unwrap();
        assert_eq!(back, fetch);
        // Unknown kinds are skipped, a missing cursor is an error.
        let odd = ["sound 1 0 0 midi x".to_owned(), "next 2".to_owned()];
        assert!(decode_sounds(&odd).unwrap().sounds.is_empty());
        assert!(decode_sounds(&["agent 1".to_owned()]).is_err());
    }

    #[test]
    fn sync_carries_changed_settings_only() {
        let mut service = sample_data();
        let json = persist::to_json(&default_settings());
        service.service_settings = Some((3, json.clone()));
        // A client without (or with an older) copy gets them ...
        let reply = decode_sync(&lines(&encode_sync(&sync_reply(&service, None, None)))).unwrap();
        assert_eq!(reply.settings, Some((3, json.clone())));
        let mut app = WebData::new();
        apply_sync(&mut app, reply, true, false, false);
        assert_eq!(app.service_settings, Some((3, json)));
        // ... a client that has them does not, and keeps its copy.
        let reply = sync_reply(&service, None, Some(3));
        assert!(reply.settings.is_none());
        let reply = decode_sync(&lines(&encode_sync(&reply))).unwrap();
        apply_sync(&mut app, reply, false, false, false);
        assert_eq!(app.service_settings.as_ref().map(|s| s.0), Some(3));
    }

    #[test]
    fn escaping_round_trips() {
        for s in ["plain", "a\\b", "line\nbreak\r", "trailing\\", ""] {
            assert_eq!(unescape(&escape(s)), s);
            assert!(!escape(s).contains('\n'));
        }
    }
}
