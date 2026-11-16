# AGENTS.md

Context for AI agents (and humans) working on **AstralGauge** (formerly PowerMonitor).

## What this app is

A Linux desktop app (Rust, eframe/egui) that monitors the six 12VHPWR power
rails of an **ASUS RTX 5090 ROG Astral** GPU:

- An **ITE IT8915FN** monitoring chip on the GPU's internal I2C bus provides
  per-rail voltage and current (6 pins, 12 V rails).
- **NVML** (`libnvidia-ml.so.1`, loaded at runtime via `dlopen`, no C deps)
  provides total GPU power, power-management limit (read/write), GPU
  temperature and VRAM usage.
- Per-pin **current limits** (yellow/red ranges) drive the green/yellow/red
  color coding everywhere and optional **alert sounds** (rodio).
- A built-in **web server** mirrors the whole UI (page + JSON API) for other
  devices; graphs support hover readouts everywhere.
- Optionally it runs as a **background service** (systemd user unit,
  installed from the settings dialog); the desktop app then shows the
  service's data instead of reading the hardware itself.

Target platform is Linux only (i2c-dev ioctls, `/sys/bus/i2c`, NVML).

## Running & developing

```sh
cargo run -- [i2c-bus-number]   # e.g. cargo run -- 15
cargo run -- --service [bus]    # headless service mode (what the unit runs)
cargo test                      # unit tests (no GPU/display required)
cargo check
sh licenses.sh                  # regenerate the embedded license text
                                # (build.rs does this automatically when
                                # Cargo.lock changes; --check = CI mode)
```

- Without an argument the I2C bus is auto-detected (scans
  `/sys/bus/i2c/devices/*/name` for `"NVIDIA i2c adapter 1 at <PCI>"`, then
  probes candidates for a sane 10–14 V reading). No fallback bus: without
  one the sensor is reported unavailable (guessing would read garbage).
  The poller retries a missing bus / unopenable `/dev/i2c-N` / NVML with a
  backoff (`DEVICE_RETRY_FIRST` 2 s doubling to `DEVICE_RETRY_MAX` 30 s), so
  a service started before the driver or `i2c-dev` recovers on its own.
- Reading the chip needs permission on `/dev/i2c-N` (root or `i2c` group).
  Everything degrades gracefully but visibly: no I2C / failed reads → a
  sample with **empty** `pins` (never zeros, which would look green) plus
  `WebData::sensor_error`, a "SENSOR UNAVAILABLE" banner in both UIs, and
  the alert/kill/auto-power state machines skipped for that poll;
  `alerts::sensor_fault_sound` plays an enabled alert sound once after
  `SENSOR_FAULT_GRACE` (3 s). Both UIs also flag readings older than
  `web::stale_after_secs` (3 intervals, ≥ 3 s) from `WebData::pins_at`, which
  catches a hung or dead poller. No NVML → power estimated as Σ(V·A) of the
  pins.
- The shipped alert WAVs are **embedded into the executable** with
  `include_bytes!` (see `alerts::EmbeddedSound`): `attention.wav` /
  `critical.wav` (yellow/red alerts), their `-powering-down.wav` variants,
  and `critical-terminating.wav` (the safeguard killed a process). The
  `sound/` directory is therefore only needed at build time; a single
  binary is all that gets distributed. Users can still pick their own `.wav`
  per sound (thresholds and the terminating one) in the settings dialog
  (and revert to the built-in with its Reset button).
- rodio pulls in cpal/ALSA: building needs the ALSA dev headers
  (`libasound2-dev` and `pkg-config` on Debian/Ubuntu).
- Regenerating the license file (`sh licenses.sh`, run by build.rs when
  `Cargo.lock`/`about.toml`/`licenses.py` change) needs `cargo-about`
  (`cargo install cargo-about`) and `python3`. A failed regeneration only
  warns; the checked-in `licenses.txt` is used as-is.
- The Wayland CSD titlebar is rendered through winit's `crossfont` font
  backend (see the `winit` dep in `Cargo.toml`): building it additionally
  needs a C compiler (crossfont compiles a bundled FreeType) and
  `libfontconfig-dev` (Debian/Ubuntu: `libfontconfig1-dev`).

## File organization

```
src/main.rs      Entry point: parse the optional explicit I2C bus, create
                 the shared Arc<Mutex<WebData>>, spawn the sound player +
                 `service::spawn_monitor` (local poller or service link),
                 build ui::App and run eframe. Also dispatches
                 `--power-helper` (privileged helper mode) and `--service`
                 (background service mode) before anything else. Keep this
                 file thin.
src/hardware.rs  HARDWARE POLLING. IT8915 constants + raw i2c-dev/SMBus
                 ioctls (block read of all 24 registers, byte-read
                 fallback), bus auto-detection/probing, `PinReading`,
                 `Sample`, `GpuProcess`, `Cmd`, and `spawn_poller()` (the
                 background poller thread; returns a `Poller` whose
                 `stop()` closes its command channel and joins it), plus
                 `own_exe()` (this binary's path, minus the " (deleted)"
                 suffix a rebuild leaves on `/proc/self/exe`). Contains `mod nvml` — the
                 hand-rolled dlopen FFI bindings, including the top GPU-
                 process list (compute + graphics per-process VRAM, and
                 per-process SM utilization from `nvmlDeviceGetProcessUtilization`;
                 names come from `/proc/<pid>/comm`) and `rank_processes`
                 (compositors/display servers last, then utilization
                 averaged over 5 s by `UtilWindow`, then VRAM, then pid;
                 own and pid 0 excluded).
                 Also the privileged power helper: `PowerHelper` (spawns
                 this same binary as root through pkexec; `KILL <pid>` is
                 the safeguard's cross-user kill) and
                 `run_power_helper()` (the helper mode), plus `kill_process`
                 (the safeguard's SIGKILL: unprivileged by default, called
                 as root by the helper for the opt-in cross-user kill).
src/alerts.rs    LIMIT CHECKING + SOUNDS. `Settings` (limits + sound
                 config + `auto_min_power` mode + the `kill_top_process_on_`
                 `red` / `kill_other_users_processes` safeguard flags),
                 `Sound` (embedded WAV via `include_bytes!`
                 or a user-selected file), `AlertState` +
                 `over_limit_sound()` (edge + repeat decision logic),
                 `KillGuardState` + `kill_guard_action()` (opt-in kill on the
                 red edge, repeating while red stays exceeded at most once
                 per `KILL_REPEAT_COOLDOWN`, gated on the previous kill's
                 sound still playing), `CriticalAlertState` +
                 `critical_alert_edge()` (red rising edge that arms the
                 latching critical notification), `AutoPowerState` +
                 `auto_power_action()` (automatic minimum-power-limit state
                 machine), `SoundPlayer` (rodio playback thread, priority-
                 aware for the critical-terminating sound, with
                 `priority_sound_playing()` so the safeguard never cuts in on
                 the kill sound) and the shipped-sound default paths.
src/web.rs       WEB SERVER + SHARED STATE. `WebData` (latest sample —
                 including the top GPU processes, served as the JSON
                 `processes` array — histories, mirrored UI settings),
                 `WebData::record()`
                 (sample ingestion + history pruning), `WebServer` /
                 `WebState` lifecycle, the tiny HTTP server
                 (`GET /` → page, `GET /data?window=SECS` → JSON,
                 `POST /ack` → acknowledge the critical alert),
                 hand-rolled JSON serialization, and `INDEX_HTML`
                 (the whole single-page web UI, JS included).
src/service.rs   BACKGROUND SERVICE. Unit management (`install`,
                 `uninstall`, `restart`, `unit_status`, `unit_text`; the
                 settings dialog runs them off the GUI thread via
                 `ServiceControl` / `run_action` / `refresh_status`), the
                 headless service mode `run_service` (poller + web server
                 from the settings file, re-read when it changes), the link
                 socket server, the SYNC state encoding (`sync_reply`,
                 `encode_sync`, `decode_sync`, `apply_sync`), and the
                 desktop side `spawn_monitor` (local poller while no
                 service runs, service link while one does). See
                 "Background service" below.
src/persist.rs   SETTINGS PERSISTENCE. The applied `ui::Config` is stored as
                 a small hand-written JSON document at
                 `$XDG_CONFIG_HOME/astral-gauge/settings.json` (falling back
                 to `~/.config/astral-gauge/settings.json`). Written
                 atomically (temp file + rename) whenever the settings panel
                 accepts a change (or a denied power authentication rolls a
                 saved option back) and loaded at startup by `ui::App::new`,
                 which also re-applies the sample interval, restarts the
                 saved web server, and — when the automatic minimum power was
                 saved enabled — authenticates the power helper immediately.
                 Like `web`'s JSON the format is hand-rolled: the loader
                 ignores unknown keys, defaults missing ones, clamps numbers
                 into the panel's ranges, and reports a corrupt file (which
                 it never touches) instead of silently resetting it.
src/ui.rs        DESKTOP UI. `App` (eframe::App::ui: main window built
                 from cards — header with a Live/Stale/Sensor-fault status
                 pill, the flashing critical-current and sensor banners,
                 power/temperature/VRAM summary cards with meters (the
                 power card holds the power-limit control), the six pin
                 tiles, the "Top GPU processes" card, the History card with
                 the window selector and graphs — settings dialog, the About
                 dialog (scrollable embedded license text, `ABOUT_TEXT` from
                 `licenses.txt`), graphs),
                 `Config` (everything the settings panel edits: limits,
                 sounds, sample interval, web server, critical-alert
                 safeguard — new app-wide settings go here; every field is
                 persisted by `persist` — loaded in `App::new`, saved on
                 Accept; its "Reset defaults" button restores
                 `Config::defaults()` (the pre-persistence built-in
                 defaults) into the draft for Accept to commit), `FileDialog`
                 (built-in .wav picker), `add_pin_graph`,
                 `add_metric_graph`, `draw_hover_readout`, and the look:
                 `palette` (all colors; mirrored as CSS variables / the
                 `C` table in `INDEX_HTML`), `apply_theme` (dark-only egui
                 style, installed from `main`), and the small widget helpers
                 (`card`, `banner`, `meter`, `stat_card`, `pin_tile`,
                 `status_pill`, `dot_label` — egui's fonts have no "●", so
                 dots are painted).
sound/           Shipped alert WAVs, embedded into the binary at compile
                 time (attention/critical = yellow/red alerts, each with a
                 -powering-down variant for the automatic power feature,
                 plus critical-terminating for the kill safeguard).
build.rs         Keeps `licenses.txt` fresh: when a crate is added,
                 removed or updated (Cargo.lock, about.toml or
                 licenses.py become newer than it) it regenerates the
                 file via `licenses.sh` before compiling; a failed
                 regeneration only warns (the checked-in file stays in
                 place, so a missing cargo-about/python3 never breaks a
                 build).
licenses.sh      (Re)generates `licenses.txt`: `cargo about generate
                 --format json` (needs cargo-about) → `python3
                 licenses.py` renders the plain-text list. `sh
                 licenses.sh --check` exits non-zero when the file is
                 stale (used by build.rs; also usable in CI or a
                 pre-commit hook).
about.toml       cargo-about config: the accepted license set.
licenses.py      Stdlib-only renderer: one full license text per license
                 kind plus "Used by" lines carrying each group's
                 copyright notice (from the crate's license file, falling
                 back to the crate's `authors`), and a static Notes
                 section with the copyright/reserved-font names of the
                 embedded default fonts (the crate ships generic license
                 files without copyright headers) and the Bitstream Vera
                 License text for the Hack font glyphs.
licenses.txt     Generated by `sh licenses.sh` (automatically by build.rs
                 when the inputs change, or manually); embedded at
                 compile time, so it is committed alongside Cargo.lock.
src/bin/         Empty (no extra binaries).
```

## Threading model (important)

One `Arc<Mutex<WebData>>` (created in `main`) is the single sync point; all
critical sections are short — keep them that way:

0. **Monitor thread** (`service::spawn_monitor`, desktop app only): owns
   the choice between monitoring locally and showing the background
   service. While no service answers on the link socket it runs the local
   poller and forwards the UI's `Cmd`s to it, looking for a service every
   second; once one answers it stops the poller (`Poller::stop`, joined, so
   nothing records after it), clears `WebData::local_monitor` and mirrors
   the service via SYNC every 200 ms, forwarding `SetLimit` / `GrantPower`
   `Access` / `AckCritical` (as `LIMIT` / `AUTH` / `ACK`). A broken link
   brings the local poller straight back.
1. **Poller thread** (`hardware::spawn_poller`): opens I2C + NVML (retrying
   what is missing with a backoff), loops (one poll per interval, measured
   from each poll's start so the read time does not stretch the spacing;
   the inter-poll wait is a `recv_timeout` on the command channel, so
   commands apply as they arrive and a closed channel ends the thread):
   drain `Cmd` channel (`SetLimit`/`Interval`/`GrantPowerAccess`) → read NVML + pins +
   top GPU processes → lock the data, read `settings`, `record(sample)` →
   run `alerts::over_limit_sound` outside the lock (evaluated, not played
   yet) → on a red rising edge, set `WebData::critical_alert` (the latching
   notification) → with the kill safeguard on, `alerts::kill_guard_action`
   SIGKILLs the top-ranked process on the red edge and repeats while the
   current stays red at most once per `KILL_REPEAT_COOLDOWN` (5 s) — never
   while the previous kill's sound is still playing. The kill is
   unprivileged by default (other users' processes answer EPERM); with
   `Settings::kill_other_users_processes` it goes through the power helper
   (`KILL <pid>`), so the outcome arrives on the next poll and the
   excursion's alert sound is held back until a termination (which plays
   the critical-terminating sound) or a failure (which plays it) is known —
   but never longer than `alerts::KILL_SOUND_HOLD` (2 s; e.g. a password
   prompt is open), and a reply missing for `KILL_REPLY_TIMEOUT` (30 s)
   counts as a failed kill so the safeguard can fire again
   (`alerts::kill_reply_wait`).
   The outcome lands in `WebData::kill_note`, GUI-only → play: a successful
   termination sends the critical-terminating sound via
   `SoundPlayer::play_priority` (replacing that excursion's red alert);
   otherwise the pending alert sound plays.
   Monitored data keeps flowing while the window is minimized.
2. **Audio thread** (`alerts::SoundPlayer`): owns the rodio
   `OutputStream`/`Sink` (created lazily on first play; a missing output is
   logged once per failure streak and retried on the next request, so a
   sound server that starts after the app still gets used; a sink that does
   not drain within the sound's length + `SOUND_STALL_MARGIN` is dropped and
   reopened). `SoundPlayer` is `Clone` (one shared thread). Plays **one sound at a time**;
   when a sound finishes it drops stale queued requests and plays the
   newest. Never blocks the caller.
3. **Privileged power helper** (`hardware::PowerHelper`): the app runs as a
   normal user but NVML's limit setter (and the safeguard's cross-user kill)
   needs root. On demand (manual Apply, the automatic min-power feature,
   a red excursion with the cross-user kill enabled, the "Test privileged
   access" button, or at startup when the saved settings have either
   privileged option enabled) the poller spawns `pkexec <own exe>
   --power-helper`, which asks the desktop polkit agent for the password
   and runs `run_power_helper()` as root. Every prompt's purpose is
   announced to the user: spawning the helper sets a `power_note` saying the
   administrator authentication is for the GPU power limiting and the
   privileged process kill, and the settings panel shows the pending /
   granted / denied state next to the options. The helper stays alive,
   speaks `SET <watts>`/`PING`/`KILL <pid>` lines on stdin and `OK`/`ERR …`
   /`KILL OK <pid>`/`KILL ERR <pid> …` lines on stdout; reader threads relay
   replies into a status cell the poller mirrors as `WebData::power_note`
   (GUI-only, not in the web JSON) **and** a `hardware::AuthState` (Idle /
   Active / Failed) mirrored as `WebData::power_auth` — on change only, and
   right after a `GrantPowerAccess`: whoever requests an authentication
   (the UI, the service link's `AUTH`) first resets `power_auth` to `Idle`,
   and a stale unchanged state must not overwrite that before the request
   is handled (the pending-auth rollback would misfire) (kill outcomes into a
   separate cell consumed on the next poll). Enabling the automatic
   min-power or the cross-user kill option in the settings dialog
   immediately pings the helper (so the password prompt shows while the
   user is at the dialog) and watches `power_auth`: a `Failed` state turns
   the option(s) back off (they cannot work without privileges), `Active`
   keeps them. One authentication serves both (enabling both at once never
   asks twice); the same happens at startup when either was saved enabled.
   Auth is asked once per app run; a denied/absent agent degrades to an
   error note, never a crash.
4. **Web server thread(s)** (`web::web_serve` + one thread per connection):
   `try_lock` with retry → 503 on contention so a GUI frame can never
   deadlock against the server-thread join at shutdown (both `/data` and
   `POST /ack` follow this pattern).
5. **GUI thread** (`ui::App::ui`): renders from `WebData`, mirrors the
   UI-side settings into it (`poll_ms`, `show_graphs`, `settings`), sends
   `Cmd`s to the poller (`SetLimit`, `Interval`, `GrantPowerAccess`).
   `App::new` loads the saved settings (see `persist`); if the automatic
   min-power was saved enabled it sends `GrantPowerAccess` right away — the
   root prompt shows at startup — and a denial turns the option back off
   (`auto_power_pending_auth`), persisting the rollback. Repaints are
   driven by the data: the poller calls `WebData::on_sample` (a
   `request_repaint` installed in `main`) after every recorded sample, and
   the graphs end at `WebData::sampled_at` (the newest sample) instead of
   the wall clock, so they scroll exactly one sample per sample. A
   fallback timer (`IDLE_REPAINT_MS`, 1 s, or the poll interval if slower;
   250 ms while the critical alert flashes) keeps time-based text moving
   when samples stop. The GUI runs its own web server only while
   `WebData::local_monitor` is set (reconciled every frame; a busy port is
   retried every 5 s and shown in the settings dialog); while a service
   monitors, the service serves the page.

## Background service (`service.rs`)

- **Unit**: `astral-gauge.service`, a systemd **user** unit in
  `$XDG_CONFIG_HOME/systemd/user` (`~/.config/systemd/user`): no root to
  install, starts at login (`WantedBy=default.target`), `Restart=always`
  with `RestartSec=5` and `StartLimitIntervalSec=0`, ordered `After=` the
  PipeWire/WirePlumber user units (ordering only). `ExecStart` is this
  executable (`hardware::own_exe`, systemd-quoted) with `--service` and the
  explicit bus if the app got one. A user unit (not a system one) is what
  lets the service play sounds through the session's sound server and show
  polkit prompts through the session's agent. The settings dialog's
  "Background service" section installs (write unit → `daemon-reload` →
  `enable` → `restart`), updates (when the unit differs from what this
  executable would write), restarts and uninstalls (`disable --now` →
  remove → `daemon-reload` → `reset-failed`); actions apply immediately,
  outside the Accept/Cancel draft.
- **Service mode** (`--service`, `run_service`): `WebData::service_mode`
  (web JSON `service`, the page's "Service" chip), the regular poller and
  sound player, settings from `persist` re-read whenever the file's mtime
  changes (every second; persist writes atomically): limits/sounds,
  interval, web server (busy port retried every `WEB_RETRY`), and newly
  enabled privileged options authenticate the helper at once. The service
  never rolls settings back itself (a denied prompt at login just means the
  next privileged action asks again); the desktop app's pending-auth logic
  still applies when it is open. Exits with 1 if another service already
  answers on the socket.
- **Link**: Unix socket `$XDG_RUNTIME_DIR/astral-gauge.sock` (fallback
  `/run/user/<uid>`), line protocol, `LINK_VERSION` handshake (`HELLO`). A
  different version shows a "restart needed" banner/pill instead of data
  (and still no local monitoring). `SYNC <since>` returns the scalars, the
  latest sample (pins, processes) and every history point with
  `t > since`; times are printed with `{}` (shortest exact form) so the
  next `since` matches bit for bit. The first (full) sync moves the app's
  `WebData::start` onto the service's clock, so all service timestamps
  apply unchanged; replies that predate a pending `ACK`/`AUTH` keep the
  local flag/state (`apply_sync`'s `keep_ack`/`keep_auth`).
- **Desktop indication**: a "Service" pill next to the status pill, a
  banner below the header ("Running as a background service", with a link
  to the service's web interface or its error), and the web-server row in
  the settings says "served by the service on port N".

## Alert sound rules (implemented in `alerts::over_limit_sound`)

- Below yellow limit: state resets, silence.
- Entering yellow / red (rising edge): that range's sound plays once
  immediately, if enabled (red has priority over yellow).
- While the automatic minimum-power feature is enabled at (or below) the
  reached level, the built-in default sounds play their `*-powering-down.wav`
  variants instead (`alerts::active_sound`); a user-selected file is never
  replaced. At a red excursion both OnYellow and OnRed count as active.
- While a limit **stays** exceeded, the sound repeats at most once per
  `ALERT_REPEAT_DELAY` (15 s). Red escalation from yellow is immediate.
- Only one sound ever plays at a time (serialized on the audio thread);
  sounds are best-effort and must never block or crash the app.
- The critical-terminating sound (see the safeguard below) is a **priority**
  sound (`SoundPlayer::play_priority`): while one is playing nothing
  interrupts it, and when requests pile up it outranks every normal alert
  request (`alerts::next_request`).

## Top GPU processes & critical-alert kill safeguard

Every sample carries NVML's `top 3 GPU-using processes` (`TOP_PROCESSES`),
ranked with display servers / compositors (`COMPOSITORS`: kwin_wayland,
Xwayland, gnome-shell, ...) after every other process, then by SM
utilization averaged over the last 5 s (`UtilWindow`: all driver samples of
the window, a missing sample counting as 0 — single samples flip between the
game and the compositor, which the driver charges for presenting the game's
frames), then VRAM usage, then pid; the app's own pid and
pid 0 are never listed (the eframe window itself holds a small GPU
context). Shown in the main window and the web page.

Optional (`Settings::kill_top_process_on_red`, default off, settings dialog
"Critical-alert safeguard"):

- Rising edge into the red range: SIGKILL the **first entry of that list**,
  and **keep killing while the current stays red**, at most once per
  `alerts::KILL_REPEAT_COOLDOWN` (5 s). A kill is additionally held back
  while the previous kill's terminating sound is still playing
  (`SoundPlayer::priority_sound_playing`). The cooldown runs between any two
  kills (so oscillating around the red threshold cannot become a kill loop);
  enabling the option mid-excursion still never kills retroactively — a
  repeat only ever follows a kill this excursion already started.
- Unprivileged by default: `kill()` only reaches processes of the app's
  own user (others answer EPERM — logged, not fatal), which deliberately
  bounds the blast radius. Opt-in `Settings::kill_other_users_processes`
  (settings dialog, only checkable while the safeguard is on): the SIGKILL
  goes through the privileged power helper instead, so other users'
  (root's) processes — e.g. an inference service — can be terminated too.
  It needs the helper's administrator credentials (shared with the power
  limiting, asked once — at startup when saved enabled, or when enabled in
  the dialog) and reverts automatically on a denied authentication, exactly
  like the automatic minimum power.
- Result (killed / could-not-kill) goes to stderr and into
  `WebData::kill_note`, shown in the main window for a minute (GUI-only,
  not in the web JSON).
- An actual kill plays `Settings::terminating_sound` (default: the embedded
  `critical-terminating.wav`, user-replaceable in the safeguard settings
  like the alert sounds) as a priority sound: it is never dropped for other
  requests, nothing interrupts it, and it **replaces** the red alert sound
  of the same excursion (a failed kill plays the regular red alert
  instead).

## Live alert and power-down indicators

Separate from the latching notification below, both UIs show the *current*
state prominently (`ui::alert_summary` / `level_banner` /
`power_down_banner` ↔ JS `alertLevel` and the `#level` / `#pdown`
banners):

- **Yellow / red** (`alerts::level` per pin, worst wins; never judged from
  missing or stale readings): a "HIGH CURRENT" (yellow) or "CRITICAL
  CURRENT" (red) banner directly under the header naming the pins in that
  range, the status pill reads Warning / Critical, a 4 px colored edge runs
  around the whole window (desktop: a foreground-layer stroke on the
  viewport; web: the fixed `#edge` div), and the window / tab title gets a
  "⚠ HIGH CURRENT —" / "⚠ CRITICAL CURRENT —" prefix (the desktop sends
  `ViewportCommand::Title` on change only), so the taskbar shows it behind
  a fullscreen game. It goes away with the current.
- **Powered down** (`WebData::power_down`, published by the poller from
  `AutoPowerState::lowered` after every sample; web JSON `power_down`
  `{saved, trigger, restore_in}` or null; link line `power_down`): a purple
  (`palette::PDOWN`) banner, "GPU POWERED DOWN — power limit at the N W
  minimum (normally M W)" once NVML reports the minimum ("POWERING DOWN …"
  while the privileged helper has not applied it yet), with the restore
  countdown or the trigger level it waits for; purple edge and title when
  no yellow/red applies.

## Latching critical-current notification

Independent of the kill safeguard: on the red rising edge
(`alerts::critical_alert_edge`) the poller sets `WebData::critical_alert`.
Both UIs then show a **flashing red** full-width banner below the header
("⚠ ALERT: critical current detected! Please check your GPU power
cables!") with an **Acknowledge** button that appears and disappears with it
(`ui.rs` ↔ `INDEX_HTML`, kept in sync; the web one POSTs `/ack`).

- **User-dismissal only:** power returning to normal never hides it; the
  desktop button and `POST /ack` are the only ways it clears, and the next
  red excursion shows it again. With a background service the desktop
  button also sends `Cmd::AckCritical` (link `ACK`) to clear the service's
  flag.
- Mirrored in the web JSON as `critical_alert` (the sensor state as
  `sensor_error` + `pins_age`, shown as the web page's `#sensor` banner);
  the desktop banner's fill blinks via `ALERT_BLINK_SECS` and forces 250 ms
  idle repaints while shown (the web page blinks with a CSS `step-end`
  keyframe). The kill safeguard's on/off state is mirrored as `safeguard`
  (the web page's "kill safeguard armed" note and TARGET tag).

## Automatic minimum power limit (`alerts::auto_power_action`)

Optional (`Settings::auto_min_power` = Off / On yellow / On red):

- Rising edge to the configured level: save the current NVML limit and lower
  it to the constraint minimum (once per excursion, via the pkexec helper).
- Below the level for `AUTO_POWER_RESTORE_DELAY` (30 s): restore the saved
  limit. Disabling the feature while lowered restores immediately.
- A manual `Cmd::SetLimit` overrides the auto state (`overridden()`): no
  restore happens over a user-set limit.
- No-op when the limit already is at the minimum or NVML data is missing.
- Privileges are required: accepting this option in the settings dialog
  immediately authenticates the power helper and **reverts the option if
  authentication fails** (`ui::App::auto_power_pending_auth` watching
  `WebData::power_auth`), so enabling it never silently fails at the first
  excursion. The same happens at startup when the persisted settings have
  it enabled: `App::new` authenticates the helper then (the prompt is
  announced as being for the power limiting and the privileged process
  kill) and reverts — and re-saves — the option when the credentials are
  not given. The cross-user kill safeguard option shares this machinery:
  the same pending-auth watch reverts **both** options when the
  authentication is denied, and one prompt serves both.

## Cross-cutting conventions

- Commit messages are a single line in Conventional Commits form
  (`fix: …`, `feat: …`, `docs: …`, `refactor: …`, `test: …`, `chore: …`):
  no body and no trailers — in particular no `Co-Authored-By` (or other
  attribution) lines. One logical change per commit.
- Never commit unless the user asks for it. Leave changes uncommitted in
  the working tree until the user has tested them: broken commits are
  unwanted, so nothing gets committed just because it builds or the unit
  tests pass.
- Module visibility uses `pub(crate)` liberally (binary crate; the modules
  are one cohesive app, not a public API).
- Module dependency direction: `main → {hardware, alerts, web, ui,
  service}`, `hardware → {alerts, web}`, `service → {hardware, alerts, web,
  persist}`, `ui → {alerts, hardware, web, persist, service}`,
  `persist → alerts`. No cycles; `alerts` depends on nothing else in the
  crate (`web::ServiceLink` lives in `web` so `web` needs no `service`).
- Hand-rolled JSON (`web_json`) and the embedded `INDEX_HTML` must stay in
  sync: field names in the JSON are consumed by the JS in `web.rs`.
- Web page (`INDEX_HTML`) and `ui.rs` deliberately mirror each other (same
  metrics, colors, thresholds, hover readouts). When changing one, change
  the other.
- History timestamps are seconds since `WebData::start` (`Instant`);
  retention is `web::HISTORY_SECS` (62 min); the web page offers 1–60 min
  windows (`ui::HORIZONS_MIN`, duplicated as `HORIZONS` in the JS).
- History timestamps (pin/power/temp/mem series) are seconds since
  `WebData::start`; the process list is snapshot-only (no history).
- Graphs never decimate by stride (every Nth sample shimmers as the window
  slides and drops spikes). Both UIs draw a line through
  `web::m4_downsample` (↔ JS `m4`): per time bucket of about one pixel,
  aligned to absolute time, the first/min/max/last point. `/data` series
  are M4-reduced to `WEB_BUCKETS` (400) buckets server-side, then again to
  the canvas width in the browser.
- Colors: green `< yellow`, yellow `[yellow, red)`, red `>= red`
  (`ui::current_color` ↔ JS `currentColor`, values from `ui::palette`);
  pin tiles/graphs use the applied
  `Settings`, never hard-coded thresholds (the JS fetches live `limits`
  from `/data`).
- The NVML process-list getters take `nvmlProcessInfo_v2_t` (24 bytes: the
  `_v3` functions reuse the v2 struct) and are sized with the documented
  two-call protocol (NULL buffer + count 0 → `NVML_ERROR_INSUFFICIENT_SIZE`
  with the count). `usedGpuMemory == u64::MAX` means "not reported".
  `nvmlDeviceGetProcessUtilization` fills 32-byte samples; a timestamp
  cursor (the newest `timeStamp` seen) advances each poll so samples are
  consumed once, and a process missing from the newest batch shows 0 %.
- The 24 power registers must be read with **one I2C block read**
  (byte-reads take the GPU's I2C bus lock and can stutter games); the
  byte-read fallback exists for kernels without block support and for
  probing.
- Every polled reading passes `check_plausible`, which only rejects
  recognizable glitches as sensor faults: an all-ones register (65.535 A or
  V), or pins totalling more than `BOARD_POWER_FACTOR` (2) x NVML board power
  + `BOARD_POWER_MARGIN_W` (100 W; NVML averages, the pins are
  instantaneous). There is deliberately **no fixed current ceiling**: one pin
  carrying the whole card's power is the failure this app exists for. High
  readings consistent with NVML, or without NVML data, count as real.
- Rail→pin mapping is reversed: rail `r` at offset `r*4` maps to pin
  `NUM_PINS - 1 - r` (see `read_pins`).

## Tests

`cargo test` (43 tests, no GPU/display needed):

- `alerts::tests`: settings defaults; embedded WAVs decode; alert edge
  triggering; 15 s repeat-while-exceeded behavior; powering-down sound
  switch with auto power; auto-power lower/restore edges and bounds;
  kill-guard repeat-while-red behavior with the 5 s cooldown and kill-sound
  gating; critical-notification red-edge triggering; priority requests
  outranking normal playback requests; the sensor-fault sound (once, after
  the grace period, re-armed by recovery); the pending-kill sound hold and
  reply timeout.
- `hardware::tests`: process ranking (utilization/memory/pid order,
  compositors last, truncation, own + zero pid exclusion), the 5 s
  utilization averaging, `/proc` name fallback, the
  privileged helper's `KILL` reply parsing, the pin-reading plausibility
  check, and the single-process pid guard in front of `kill()` (tested on
  the pure conversion — never call `kill_process` with a bad pid in a test).
- `persist::tests`: settings JSON round-trips (defaults, custom values,
  user sound paths with escapes), a real save()/load() file round-trip in a
  throwaway HOME, unknown/missing-key tolerance, clamping and yellow/red
  order repair, rejection of invalid values, and corrupt-document detection.
- `service::tests`: full and incremental SYNC round trips into a fresh
  `WebData` (histories, sample, processes with escaped names, flags, clock
  alignment, gaps, pending ACK/AUTH kept), malformed replies rejected,
  unknown lines tolerated, the unit file's quoted `ExecStart`, line
  escaping. (Never call `install`/`uninstall` in a test: they drive the
  real user manager.)
- `ui::tests`: the live alert summary (worst range, pins named, red over
  yellow); embedded license-text sanity (the generated `licenses.txt` is
  non-empty, lists one "Used by:" section per license, and has no template
  leftovers or HTML escaping); readings shown as dashes when missing; the sensor
  fault / staleness warning.
- `web::tests`: JSON field content (incl. process entries, name escaping and
  the latching `critical_alert` flag, `safeguard`, `service`, `sensor_error`, `pins_age`),
  `record()` history append/prune/range caching and gaps (no zeros) for
  samples without pins, M4 graph downsampling (spikes kept, complete
  buckets unchanged while the window slides), the stale threshold, query-param parsing, HTTP server serve/404/busy-port/restart and
  the `POST /ack` dismissal.

Tests that bind ports use `free_port()` (bind :0, drop, reuse).

## Gotchas

- All NVML power functions work in **milliwatts** (v1 getters *and* the v1
  setter, per the API docs; the setters reject plain watts with NVML error 2).
  The app keeps watts internally: `to_watts` converts the getter values and
  `set_power_limit` multiplies by 1000. `nvidia-smi -pl` also takes watts but
  does its own conversion. Setting a limit requires root: an unprivileged
  setter gets NVML error 4 and the poller then routes the write through the
  pkexec helper, which re-executes the app binary with `--power-helper` as
  root (polkit password prompt on first use).
- `I2cSmbusData` must stay 34 bytes (`#[repr(C)]` union matching the kernel);
  shrinking it breaks the block read.
- The poller measures and periodically logs I2C batch latency (every 500th
  poll, first at 50) — don't remove it; it's the canary for I2C bus
  contention.
- Web `/data` `window` param is clamped to 60..=3600 s server-side.
- eframe 0.36 / egui 0.36 API. `eframe::App`'s entry point is
  `fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame)` — the app
  receives a margin-less root `Ui` (panels are built on it; in 0.36
  `CentralPanel::show` takes `&mut Ui`, `Window::show` still takes
  `&Context`, and `Ui::fonts` became `Ui::fonts_mut` with the immutable
  `FontsView`). eframe 0.36 defaults to the wgpu renderer; this project
  pins the 0.31-era glow (OpenGL) backend explicitly in `Cargo.toml`
  (`default-features = false` + feature list), plus a direct `winit` dep
  with `default-features = false` that re-enables the features eframe no
  longer turns on — `wayland-dlopen` and the Adwaita CSD via
  `wayland-csd-adwaita-crossfont`. `default-features = false` matters:
  without it the direct dep silently re-adds winit's whole default set,
  including the default `ab_glyph` CSD backend → the unmaintained
  `ttf-parser` (RUSTSEC-2026-0192).
- `cargo audit` is clean. The former `ttf-parser` warning (RUSTSEC
  2026-0192, unmaintained) is gone: the Wayland CSD now uses winit's
  `crossfont` backend instead of the default `ab_glyph` one, so
  `ttf-parser`/`owned_ttf_parser`/`ab_glyph` are out of the graph. The
  price is a bundled-FreeType C build + libfontconfig at compile time
  (see Running & developing). `quick-xml` ≥0.41 (via `wayland-scanner`)
  is the only copy left; the vulnerable 0.30 chain (`zbus 4` →
  `zbus_xml 4`) is gone since eframe 0.34 pulls `atspi` → `zbus 5`.
- rodio 0.20 API (`OutputStream::try_default`, `Sink`, `Decoder`) — 0.21+
  renamed these.
