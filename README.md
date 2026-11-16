# AstralGauge

AstralGauge is a Linux desktop application that monitors the six 12VHPWR
power rails of an ASUS RTX 5090 ROG Astral GPU in real time.

It reads per-rail voltage and current from the GPU's internal ITE IT8915FN
monitoring chip over I2C, and pulls total GPU power, power-management limit,
temperature and VRAM usage from NVML. Per-pin current limits drive
green/yellow/red color coding and optional alert sounds, and a built-in web
server mirrors the whole UI (page + JSON API) for other devices.

## Supported hardware

This app is written specifically for the **ASUS ROG Astral RTX 5090** (and
should also work on the **RTX 5080** variant, which shares the same board
design and monitoring chip).

> **Do not run this on other graphics cards.** The I2C register layout,
> rail-to-pin mapping and current limits are hard-coded for the Astral
> board. Running it against a different GPU will produce meaningless
> readings at best and may misbehave at worst.

## Safeguards

Beyond monitoring, AstralGauge can act on the readings to protect the card.
When a rail goes above its thresholds it can:

- **Automatically lower the GPU's power limit** (powering it down), and/or
- **Kill the highest GPU-consuming processes.**

Both actions are opt-in (enabled in the settings dialog). Because they need
elevated privileges, the app asks for administrator credentials (via
`pkexec`) on first use. The [background service](#background-service) runs
as root and acts without asking, also when nobody is logged in.

## Building

The project is a Rust binary. Building requires:

- A Rust toolchain (`cargo`).
- The ALSA development headers, since `rodio` pulls in `cpal`/ALSA
  (`libasound2-dev` and `pkg-config` on Debian/Ubuntu).
- A C compiler and `libfontconfig-dev` (`libfontconfig1-dev` on
  Debian/Ubuntu) — the Wayland client-side-decoration titlebar is rendered
  through winit's `crossfont` backend, which compiles a bundled FreeType
  (`freetype-sys`) at build time.

```sh
cargo build --release
```

> **Regenerating the license file (optional).** The embedded third-party
> license text (`licenses.txt`) is regenerated automatically by `build.rs`
> only when the dependency set changes (`Cargo.lock`, `about.toml` or
> `licenses.py` become newer than it). Doing so needs `cargo-about`
> (`cargo install cargo-about`) and `python3`. If they're missing the build
> just warns and reuses the checked-in `licenses.txt`, so a normal build
> never requires them. You can also run it manually with `sh licenses.sh`.

The binary is produced at `target/release/astral-gauge`.

## Running

```sh
cargo run --release -- [i2c-bus-number]   # e.g. cargo run --release -- 15
```

(`cargo run` builds in debug mode by default; add `--release` to run the
optimized binary built above. The `--` separates cargo's flags from the
app's own arguments.)

- Without an argument the I2C bus is auto-detected (it scans
  `/sys/bus/i2c/devices/*/name` for the NVIDIA adapter and probes candidates
  for a sane 10–14 V reading). There is no fallback bus: if nothing is
  found, pin monitoring is off and the app says so.
- Reading the chip needs permission on `/dev/i2c-N` (run as root, or add
  your user to the `i2c` group).
- Everything degrades gracefully, but never silently: without I2C access
  (or when reads fail) both UIs show a "SENSOR UNAVAILABLE" banner with the
  reason instead of pin values, readings older than three sample intervals
  are flagged as stale, and an enabled alert sound plays once when the
  sensor stays unreadable for 3 s. With no NVML the total power is estimated
  as Σ(V·A) of the pins.

## Web interface

Enable it in **Settings → Web server**. **Listen on** controls who can open
it:

- **All networks (0.0.0.0)**, the default: other devices on your local
  network open `http://<this PC's address>:<port>/`. The settings dialog
  lists those URLs.
- **This computer only (127.0.0.1).**
- One of this PC's own addresses, to serve only that network, or any other
  IP address.

The page has no password. Anyone who can reach it sees the readings and can
acknowledge the critical alert. If another device can't connect, check the
firewall (e.g. `sudo ufw allow from 10.0.0.0/24 to any port 8666
proto tcp`). On macOS, also allow the browser under System Settings →
Privacy & Security → Local Network.

## Background service

AstralGauge can run in the background as a systemd **system** service
(`astral-gauge.service`). It starts at boot, runs as root and protects the
card whether or not anyone is logged in, for example while an inference
server runs unattended. Install or uninstall it from **Settings →
Background service**. Installing asks for the administrator password once.

- **What it does.** The readings, the critical-current notification, the
  safeguards (automatic power limit, killing GPU processes) and the web
  interface (when enabled) keep running with the window closed. Because it
  runs as root, lowering the power limit and killing processes never wait
  for a password. "Also kill processes of other users" still decides
  whether processes of users other than you can be killed.
- **What gets installed.** A root-owned copy of the executable in
  `/usr/local/lib/astral-gauge/`, its settings in
  `/etc/astral-gauge/settings.json` and the unit in `/etc/systemd/system/`.
  The service never runs the build in your home directory. After
  rebuilding, the settings dialog offers **Update service**.
- **Settings.** The service starts with the settings in effect in the app.
  Changes you accept in the app go to the service and apply within a
  second. When the app starts while the service runs, it uses the
  service's settings.
- **The desktop app.** While the service runs, the app doesn't read the
  hardware itself. It shows the service's data, marked with a **Service**
  pill and a banner with a link to the web interface. The web page shows a
  **Service** chip. Only you and root can change the service's settings or
  power limit; other local users can only view it.
- **Sounds.** A system service has no sound output. The optional **sound
  agent** (`astral-gauge-sound.service`, a user service, on by default)
  plays the service's alert sounds whenever you're logged in. Without it,
  they play only while the app is open.
- **Waiting for dependencies.** The GPU's I2C bus, NVML and a busy web
  port are retried until they're available, and systemd restarts the
  service if it ever exits.
- **Upgrading.** If the per-user service of an earlier version is
  installed, installing the system service replaces it.
- **Security.** The web server then runs as root. It is confined by the
  unit's hardening (read-only system, no access to home directories), but
  keep it on **This computer only** or a trusted network.

Logs go to the journal: `journalctl -u astral-gauge` (the service) and
`journalctl --user -u astral-gauge-sound` (the sound agent).

## License

MIT — see [LICENSE](LICENSE). The full list of third-party licenses for the
dependencies is embedded in the binary and shown in the About dialog
(generated into `licenses.txt`).

---
