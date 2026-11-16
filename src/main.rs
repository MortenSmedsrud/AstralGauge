//! AstralGauge: watches the six 12VHPWR rails of an ASUS RTX 5090 ROG
//! Astral through its IT8915FN monitoring chip (I2C), alongside NVML GPU
//! metrics, and alerts when the per-pin current limits are exceeded.
//!
//! The app is a thin entry point; the functionality lives in:
//! - [`hardware`]: I2C + NVML polling (and the background poller thread)
//! - [`alerts`]: current-limit checking and rodio alert sounds
//! - [`web`]: shared sample/history store and the HTTP server
//! - [`ui`]: the eframe/egui desktop UI

mod alerts;
mod hardware;
mod persist;
mod ui;
mod web;

use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::hardware::{find_i2c_bus, POWER_HELPER_ARG};
use crate::web::WebData;

fn main() {
    // Privileged power-limit helper mode: this binary, re-executed as root
    // through pkexec (see hardware::PowerHelper). It only answers limit
    // commands on stdin/stdout and never opens the UI.
    if std::env::args().any(|a| a == POWER_HELPER_ARG) {
        std::process::exit(hardware::run_power_helper());
    }

    let bus_arg: Option<String> = std::env::args().nth(1);

    // No fallback bus: guessing (say, bus 0) would read whatever device
    // answers at the IT8915's address there and could pass garbage off as
    // pin currents. Without a bus the sensor is reported as unavailable.
    let bus_number = bus_arg
        .as_ref()
        .and_then(|s| s.parse::<u32>().ok())
        .or_else(find_i2c_bus);
    match bus_number {
        Some(bus) => println!("Using I2C bus: {bus}"),
        None => {
            eprintln!("Warning: IT8915 auto-detect failed; pin currents will NOT be monitored.");
            eprintln!("Pass the bus number explicitly, e.g.: astral-gauge 15");
        }
    }

    // The single shared state: the poller records samples into it, the GUI
    // renders from it and the web server serves it.
    let data = Arc::new(Mutex::new(WebData::new()));

    // Alert-sound playback thread (one sound at a time; see `alerts`).
    let player = alerts::SoundPlayer::spawn();

    // Background poller: reads the pins and NVML, records into the shared
    // data, and checks the current limits on every sample.
    let cmd_tx = hardware::spawn_poller(bus_number, data.clone(), player);

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
    let app = ui::App::new(cmd_tx, data.clone());

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
