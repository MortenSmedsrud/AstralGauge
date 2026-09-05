# AGENTS.md

Context for AI agents (and humans) working on **PowerMonitor**.

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
- The `sound/` directory holds the shipped alert WAVs: `attention.wav`
  (yellow) and `critical.wav` (red). The defaults
  (`alerts::default_sound_path("attention")`) resolve relative to the
  working directory — run from the repo root or the default alert sounds
  won't be found.
- rodio pulls in cpal/ALSA: building needs the ALSA dev headers
  (`libasound2-dev` and `pkg-config` on Debian/Ubuntu).

## File organization

```
src/main.rs      Entry point: resolve I2C bus, create the shared
                 Arc<Mutex<WebData>>, spawn the sound player + poller,
                 build ui::App and run eframe. Keep this file thin.
src/hardware.rs  HARDWARE POLLING. IT8915 constants + raw i2c-dev/SMBus
                 ioctls (block read of all 24 registers, byte-read
                 fallback), bus auto-detection/probing, `PinReading`,
                 `Sample`, `Cmd`, and `spawn_poller()` (the background
                 poller thread). Contains `mod nvml` — the hand-rolled
                 dlopen FFI bindings.
src/alerts.rs    LIMIT CHECKING + SOUNDS. `Settings` (limits + sound
                 config), `AlertState` + `over_limit_sound()` (edge +
                 repeat decision logic), `SoundPlayer` (rodio playback
                 thread) and the shipped-sound default paths.
src/web.rs       WEB SERVER + SHARED STATE. `WebData` (latest sample,
                 histories, mirrored UI settings), `WebData::record()`
                 (sample ingestion + history pruning), `WebServer` /
                 `WebState` lifecycle, the tiny HTTP server
                 (`GET /` → page, `GET /data?window=SECS` → JSON),
                 hand-rolled JSON serialization, and `INDEX_HTML`
                 (the whole single-page web UI, JS included).
src/ui.rs        DESKTOP UI. `App` (eframe::App::update: main window,
                 settings dialog, web toggle, graphs), `FileDialog`
                 (built-in .wav picker), `add_pin_graph`,
                 `add_metric_graph`, `draw_hover_readout`, colors.
sound/           Shipped alert WAVs (attention.wav = yellow, critical.wav = red).
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
3. **Web server thread(s)** (`web::web_serve` + one thread per connection):
   `try_lock` with retry → 503 on contention so a GUI frame can never
   deadlock against the server-thread join at shutdown.
4. **GUI thread** (`ui::App::update`): renders from `WebData`, mirrors the
   UI-side settings into it (`poll_ms`, `show_graphs`, `settings`), sends
   `Cmd`s to the poller. Redraws at the poll interval when idle.

## Alert sound rules (implemented in `alerts::over_limit_sound`)

- Below yellow limit: state resets, silence.
- Entering yellow / red (rising edge): that range's sound plays once
  immediately, if enabled (red has priority over yellow).
- While a limit **stays** exceeded, the sound repeats at most once per
  `ALERT_REPEAT_DELAY` (15 s). Red escalation from yellow is immediate.
- Only one sound ever plays at a time (serialized on the audio thread);
  sounds are best-effort and must never block or crash the app.

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

`cargo test` (7 tests, no GPU/display needed):

- `alerts::tests`: settings defaults + shipped-sound paths; alert edge
  triggering; 15 s repeat-while-exceeded behavior.
- `web::tests`: JSON field content, `record()` history append/prune/range
  caching, query-param parsing, HTTP server serve/404/busy-port/restart.

Tests that bind ports use `free_port()` (bind :0, drop, reuse). When adding
tests, remember `default_sound_path` resolves relative to the crate root
(CWD of `cargo test`).

## Gotchas

- All NVML power functions work in **milliwatts** (v1 getters *and* the v1
  setter, per the API docs; the setters reject plain watts with NVML error 2).
  The app keeps watts internally: `to_watts` converts the getter values and
  `set_power_limit` multiplies by 1000. `nvidia-smi -pl` also takes watts but
  does its own conversion. Setting a limit requires root.
- `I2cSmbusData` must stay 34 bytes (`#[repr(C)]` union matching the kernel);
  shrinking it breaks the block read.
- The poller measures and periodically logs I2C batch latency (every 500th
  poll, first at 50) — don't remove it; it's the canary for I2C bus
  contention.
- Web `/data` `window` param is clamped to 60..=3600 s server-side.
- eframe 0.31 / egui 0.31 API (e.g. `StrokeKind::Middle` in painter calls);
  rodio 0.20 API (`OutputStream::try_default`, `Sink`, `Decoder`) — 0.21+
  renamed these.
