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

Target platform is Linux only (i2c-dev ioctls, `/sys/bus/i2c`, NVML).

## Running & developing

```sh
cargo run -- [i2c-bus-number]   # e.g. cargo run -- 15
cargo test                      # unit tests (no GPU/display required)
cargo check
```

- Without an argument the I2C bus is auto-detected (scans
  `/sys/bus/i2c/devices/*/name` for `"NVIDIA i2c adapter 1 at <PCI>"`, then
  probes candidates for a sane 10–14 V reading); falls back to bus 0.
- Reading the chip needs permission on `/dev/i2c-N` (root or `i2c` group).
  Everything degrades gracefully: no I2C → zero pin readings; no NVML →
  power estimated as Σ(V·A) of the pins.
- The shipped alert WAVs are **embedded into the executable** with
  `include_bytes!` (see `alerts::EmbeddedSound`): `attention.wav` /
  `critical.wav` (yellow/red alerts) and their `-powering-down.wav`
  variants. The `sound/` directory is therefore only needed at build time;
  a single binary is all that gets distributed. Users can still pick their
  own `.wav` per threshold in the settings dialog (and revert to the
  built-in with its Reset button).
- rodio pulls in cpal/ALSA: building needs the ALSA dev headers
  (`libasound2-dev` and `pkg-config` on Debian/Ubuntu).

## File organization

```
src/main.rs      Entry point: resolve I2C bus, create the shared
                 Arc<Mutex<WebData>>, spawn the sound player + poller, build
                 ui::App and run eframe. Also dispatches `--power-helper`
                 (privileged helper mode) before anything else. Keep this
                 file thin.
src/hardware.rs  HARDWARE POLLING. IT8915 constants + raw i2c-dev/SMBus
                 ioctls (block read of all 24 registers, byte-read
                 fallback), bus auto-detection/probing, `PinReading`,
                 `Sample`, `Cmd`, and `spawn_poller()` (the background
                 poller thread). Contains `mod nvml` — the hand-rolled
                 dlopen FFI bindings. Also the privileged power helper:
                 `PowerHelper` (spawns this same binary as root through
                 pkexec) and `run_power_helper()` (the helper mode).
src/alerts.rs    LIMIT CHECKING + SOUNDS. `Settings` (limits + sound
                 config + `auto_min_power` mode), `Sound` (embedded WAV via
                 `include_bytes!` or a user-selected file), `AlertState` +
                 `over_limit_sound()` (edge + repeat decision logic),
                 `AutoPowerState` + `auto_power_action()` (automatic
                 minimum-power-limit state machine), `SoundPlayer` (rodio
                 playback thread) and the shipped-sound default paths.
src/web.rs       WEB SERVER + SHARED STATE. `WebData` (latest sample,
                 histories, mirrored UI settings), `WebData::record()`
                 (sample ingestion + history pruning), `WebServer` /
                 `WebState` lifecycle, the tiny HTTP server
                 (`GET /` → page, `GET /data?window=SECS` → JSON),
                 hand-rolled JSON serialization, and `INDEX_HTML`
                 (the whole single-page web UI, JS included).
src/ui.rs        DESKTOP UI. `App` (eframe::App::update: main window, settings
                 dialog, graphs), `Config` (everything the settings panel
                 edits: limits, sounds, sample interval, web server — new
                 app-wide settings go here), `FileDialog`
                 (built-in .wav picker), `add_pin_graph`,
                 `add_metric_graph`, `draw_hover_readout`, colors.
sound/           Shipped alert WAVs, embedded into the binary at compile
                 time (attention/critical = yellow/red alerts, each with a
                 -powering-down variant for the automatic power feature).
src/bin/         Empty (no extra binaries).
```

## Threading model (important)

One `Arc<Mutex<WebData>>` (created in `main`) is the single sync point; all
critical sections are short — keep them that way:

1. **Poller thread** (`hardware::spawn_poller`): opens I2C + NVML once,
   loops: drain `Cmd` channel (`SetLimit`/`Interval`) → read NVML + pins →
   lock the data, read `settings`, `record(sample)` → run
   `alerts::over_limit_sound` outside the lock → `SoundPlayer::play`.
   Monitored data keeps flowing while the window is minimized.
2. **Audio thread** (`alerts::SoundPlayer`): owns the rodio
   `OutputStream`/`Sink` (created lazily on first play; absence of audio is
   logged once and sounds become no-ops). Plays **one sound at a time**;
   when a sound finishes it drops stale queued requests and plays the
   newest. Never blocks the caller.
3. **Privileged power helper** (`hardware::PowerHelper`): the app runs as a
   normal user but NVML's limit setter needs root. On demand (manual Apply,
   the automatic min-power feature, or the "Test privileged access"
   button) the poller spawns `pkexec <own exe> --power-helper`, which asks
   the desktop polkit agent for the password and runs `run_power_helper()`
   as root. The helper stays alive, speaks `SET <watts>`/`PING` lines on
   stdin and `OK`/`ERR …` lines on stdout; reader threads relay replies into
   a status cell the poller mirrors as `WebData::power_note` (GUI-only, not
   in the web JSON) **and** a `hardware::AuthState` (Idle / Active / Failed)
   mirrored as `WebData::power_auth`. Enabling automatic min-power in the
   settings dialog immediately pings the helper (so the password prompt shows
   while the user is at the dialog) and watches `power_auth`: a `Failed`
   state turns the option back off (it cannot work without privileges),
   `Active` keeps it. Auth is asked once per app run; a denied/absent agent
   degrades to an error note, never a crash.
4. **Web server thread(s)** (`web::web_serve` + one thread per connection):
   `try_lock` with retry → 503 on contention so a GUI frame can never
   deadlock against the server-thread join at shutdown.
5. **GUI thread** (`ui::App::update`): renders from `WebData`, mirrors the
   UI-side settings into it (`poll_ms`, `show_graphs`, `settings`), sends
   `Cmd`s to the poller (`SetLimit`, `Interval`, `GrantPowerAccess`).
   Redraws at the poll interval when idle.

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
  excursion.

## Cross-cutting conventions

- Module visibility uses `pub(crate)` liberally (binary crate; the modules
  are one cohesive app, not a public API).
- Module dependency direction: `main → {hardware, alerts, web, ui}`,
  `hardware → {alerts, web}`, `ui → {alerts, hardware, web}`. No cycles;
  `alerts` depends on nothing else in the crate.
- Hand-rolled JSON (`web_json`) and the embedded `INDEX_HTML` must stay in
  sync: field names in the JSON are consumed by the JS in `web.rs`.
- Web page (`INDEX_HTML`) and `ui.rs` deliberately mirror each other (same
  metrics, colors, thresholds, hover readouts). When changing one, change
  the other.
- History timestamps are seconds since `WebData::start` (`Instant`);
  retention is `web::HISTORY_SECS` (62 min); the web page offers 1–60 min
  windows (`ui::HORIZONS_MIN`, duplicated as `HORIZONS` in the JS).
- Colors: green `< yellow`, yellow `[yellow, red)`, red `>= red`
  (`ui::current_color` ↔ JS `currentColor`); tables/graphs use the applied
  `Settings`, never hard-coded thresholds (the JS fetches live `limits`
  from `/data`).
- The 24 power registers must be read with **one I2C block read**
  (byte-reads take the GPU's I2C bus lock and can stutter games); the
  byte-read fallback exists for kernels without block support and for
  probing.
- Rail→pin mapping is reversed: rail `r` at offset `r*4` maps to pin
  `NUM_PINS - 1 - r` (see `read_pins`).

## Tests

`cargo test` (11 tests, no GPU/display needed):

- `alerts::tests`: settings defaults; embedded WAVs decode; alert edge
  triggering; 15 s repeat-while-exceeded behavior; powering-down sound
  switch with auto power; auto-power lower/restore edges and bounds.
- `web::tests`: JSON field content, `record()` history append/prune/range
  caching, query-param parsing, HTTP server serve/404/busy-port/restart.

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
- eframe 0.31 / egui 0.31 API (e.g. `StrokeKind::Middle` in painter calls);
  rodio 0.20 API (`OutputStream::try_default`, `Sink`, `Decoder`) — 0.21+
  renamed these.
