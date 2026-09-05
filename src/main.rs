//! Power Monitor: watches the six 12VHPWR rails of an ASUS RTX 5090 ROG
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
mod ui;
mod web;

use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::hardware::find_i2c_bus;
use crate::web::WebData;

fn main() {
    let bus_arg: Option<String> = std::env::args().nth(1);

    let bus_number = match bus_arg
        .as_ref()
        .and_then(|s| s.parse::<u32>().ok())
        .or_else(find_i2c_bus)
    {
        Some(bus) => bus,
        None => {
            eprintln!("Warning: auto-detect failed. Defaulting to bus 0.");
            eprintln!("Pass the bus number explicitly, e.g.: power-monitor 15");
            0
        }
    };

    println!("Using I2C bus: {bus_number}");

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
            .with_inner_size([720.0, 860.0])
            .with_resizable(true),
        ..Default::default()
    };

    let app = ui::App::new(cmd_tx, data);

    if let Err(e) = eframe::run_native("Power Monitor", options, Box::new(|_cc| Ok(Box::new(app))))
    {
        eprintln!("eframe error: {e}");
        std::process::exit(1);
    }
}
