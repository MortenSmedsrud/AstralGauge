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
elevated privileges, they may prompt for administrator credentials (via
`pkexec`) on first use.

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

## License

MIT — see [LICENSE](LICENSE). The full list of third-party licenses for the
dependencies is embedded in the binary and shown in the About dialog
(generated into `licenses.txt`).

---

*This project was developed with the assistance of the Zed agent and Qwen3.8-27B.*