//! AstralGauge: watches the six 12VHPWR rails of an ASUS RTX 5090 ROG
//! Astral through its IT8915FN monitoring chip (I2C), alongside NVML GPU
//! metrics, and alerts when the per-pin current limits are exceeded.
//!
//! The app is a thin entry point; the functionality lives in:
//! - [`hardware`]: I2C + NVML polling (and the background poller thread)
//! - [`alerts`]: current-limit checking and rodio alert sounds
//! - [`web`]: shared sample/history store and the HTTP server
//! - [`ui`]: the eframe/egui desktop UI
//! - [`service`]: the background (systemd system) service, its installer,
//!   the per-user sound agent and the desktop app's link to the service

mod alerts;
mod hardware;
mod persist;
mod service;
mod ui;
mod web;

use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::hardware::POWER_HELPER_ARG;
use crate::web::WebData;

fn main() {
    // Privileged power-limit helper mode: this binary, re-executed as root
    // through pkexec (see hardware::PowerHelper). It only answers limit
    // commands on stdin/stdout and never opens the UI.
    if std::env::args().any(|a| a == POWER_HELPER_ARG) {
        std::process::exit(hardware::run_power_helper());
    }

    // An explicit IT8915 bus number (`astral-gauge [--service] 15`; flags
    // such as `--owner=1000` never parse as one). Without
    // one the poller auto-detects the bus, and keeps retrying while none is
    // found. There is no fallback bus: guessing (say, bus 0) would read
    // whatever device answers at the IT8915's address there and could pass
    // garbage off as pin currents.
    let explicit_bus: Option<u32> = std::env::args().skip(1).find_map(|a| a.parse().ok());

    // The other modes of this binary (see `service`): the installer and
    // uninstaller (run as root through pkexec by the settings dialog), the
    // background service (the same monitoring without a window, run by the
    // systemd system unit) and the sound agent (the user unit that plays the
    // service's alert sounds in the login session).
    let has = |flag: &str| std::env::args().any(|a| a == flag);
    if has(service::INSTALL_ARG) {
        std::process::exit(service::run_install(explicit_bus));
    }
    if has(service::UNINSTALL_ARG) {
        std::process::exit(service::run_uninstall());
    }
    if has(service::SERVICE_ARG) {
        std::process::exit(service::run_service(explicit_bus));
    }
    if has(service::SOUND_AGENT_ARG) {
        std::process::exit(service::run_sound_agent());
    }

    // The single shared state: the poller records samples into it, the GUI
    // renders from it and the web server serves it.
    let data = Arc::new(Mutex::new(WebData::new()));

    // Alert-sound playback thread (one sound at a time; see `alerts`).
    let player = alerts::SoundPlayer::spawn();

    // Monitoring: the background poller (reads the pins and NVML, records
    // into the shared data, checks the current limits on every sample) —
    // or, while the background service runs, a link that shows the
    // service's data instead (see `service::spawn_monitor`).
    let cmd_tx = service::spawn_monitor(explicit_bus, data.clone(), player);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([780.0, 960.0])
            .with_min_inner_size([560.0, 640.0])
            .with_resizable(true),
        ..Default::default()
    };

    // The app loads the settings saved on the previous run (see
    // `persist`), re-applies them, and authenticates the privileged power
    // helper when the saved automatic power limit needs it.
    let app = ui::App::new(cmd_tx, data.clone(), explicit_bus);

    let creator = Box::new(move |cc: &eframe::CreationContext<'_>| {
        // Repaint as soon as the poller records a sample (see
        // `WebData::on_sample`).
        ui::apply_theme(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        data.lock().unwrap().on_sample = Some(Arc::new(move || ctx.request_repaint()));
        Ok(Box::new(app) as Box<dyn eframe::App>)
    });
    if let Err(e) = eframe::run_native("AstralGauge", options, creator) {
        eprintln!("eframe error: {e}");
        std::process::exit(1);
    }
}
