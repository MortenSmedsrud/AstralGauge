//! Web interface: the shared sample/history store and the HTTP server that
//! mirrors the desktop UI for other devices.
//!
//! [`WebData`] is the app's central state (latest sample, histories, mirrored
//! UI settings) behind a single `Arc<Mutex<_>>`; the poller thread records
//! into it while the GUI renders and the web server serves from it.
//! [`WebServer`] runs the tiny hand-rolled HTTP server (page + JSON API).

use std::io;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::alerts::Settings;
use crate::hardware::{AuthState, Sample, DEFAULT_POLL_MS, NUM_PINS};

/// Default port for the web interface (editable in the UI).
pub(crate) const DEFAULT_WEB_PORT: u16 = 8666;
/// Default listen address: every IPv4 interface, so other devices on the
/// local network can open the page (editable in the UI, e.g. `127.0.0.1`
/// for this computer only).
pub(crate) const DEFAULT_WEB_BIND: &str = "0.0.0.0";

/// The socket address to serve on, from the settings' listen-address and
/// port fields; `Err` says what is wrong with them.
pub(crate) fn web_target(bind: &str, port: &str) -> Result<SocketAddr, String> {
    let bind = bind.trim();
    let ip: IpAddr = bind.parse().map_err(|_| {
        format!("\"{bind}\" is not an IP address (e.g. 0.0.0.0, 127.0.0.1 or 10.0.0.12)")
    })?;
    match port.trim().parse::<u16>() {
        Ok(port) if port != 0 => Ok(SocketAddr::new(ip, port)),
        _ => Err("the port must be a number between 1 and 65535".to_owned()),
    }
}

/// The URL that opens a server listening on `addr` from this computer:
/// `localhost` when it listens on every interface.
pub(crate) fn local_url(addr: SocketAddr) -> String {
    let host = match addr.ip() {
        ip if ip.is_unspecified() => "localhost".to_owned(),
        IpAddr::V6(v6) => format!("[{v6}]"),
        ip => ip.to_string(),
    };
    format!("http://{host}:{}/", addr.port())
}

/// This computer's own network addresses (interfaces that are up with a
/// link, without
/// loopback and IPv6 link-local ones, which need a scope to bind), with
/// their interface names: the choices for the listen address.
pub(crate) fn interface_addresses() -> Vec<(String, IpAddr)> {
    let mut out = Vec::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
        return out;
    }
    let mut cur = ifap;
    while !cur.is_null() {
        // SAFETY: getifaddrs returned a valid list, freed below.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        // Up and with a link (a cable plugged in, a bridge with members).
        let running = (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_uint;
        if ifa.ifa_addr.is_null() || ifa.ifa_flags & running != running {
            continue;
        }
        let family = i32::from(unsafe { (*ifa.ifa_addr).sa_family });
        let ip = match family {
            libc::AF_INET => {
                let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
                IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)))
            }
            libc::AF_INET6 => {
                let sin6 = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in6) };
                IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr))
            }
            _ => continue,
        };
        let link_local_v6 = matches!(ip, IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80);
        if ip.is_loopback() || link_local_v6 {
            continue;
        }
        let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }
            .to_string_lossy()
            .into_owned();
        if !out.contains(&(name.clone(), ip)) {
            out.push((name, ip));
        }
    }
    unsafe { libc::freeifaddrs(ifap) };
    out
}

/// Time buckets per series sent to a web client (see [`m4_downsample`]: at
/// most 4 points each). About one per canvas pixel, so the browser can't
/// show more anyway, and it bounds the payload at the 60-minute horizon.
const WEB_BUCKETS: f64 = 400.0;

/// Keep a little more history than the largest horizon
pub(crate) const HISTORY_SECS: f64 = 60.0 * 60.0 + 120.0;

/// Age (seconds) after which the last pin reading counts as stale: three
/// sample intervals, but at least 3 s. Checked by the GUI and the web page
/// themselves, so a hung or dead poller thread shows up too. Mirrored as
/// `staleAfter` in the web page's JS.
pub(crate) fn stale_after_secs(poll_ms: u64) -> f64 {
    (3.0 * poll_ms as f64 / 1000.0).max(3.0)
}

/// The data shared between the GUI and the web server: the latest sample,
/// the history buffers, the permanent y ranges and a few mirrored UI
/// settings. Both sides take one mutex; the critical sections are short.
pub(crate) struct WebData {
    /// Latest sample from the poller thread.
    pub(crate) sample: Sample,
    /// App start; all histories use elapsed seconds from this instant.
    pub(crate) start: std::time::Instant,
    /// Per-pin history of (elapsed seconds, current A), oldest first.
    pub(crate) history: Vec<Vec<(f64, f64)>>,
    /// GPU metric histories of (elapsed seconds, value), oldest first.
    pub(crate) power_history: Vec<(f64, f64)>, // W
    pub(crate) temp_history: Vec<(f64, f64)>, // °C
    pub(crate) mem_history: Vec<(f64, f64)>,  // VRAM used, GB
    /// Permanent y ranges (max value) for the metric graphs, learned from
    /// NVML: max power limit (W), max supported temp (°C), total VRAM (GB).
    pub(crate) power_range_max: Option<f64>,
    pub(crate) temp_range_max: Option<f64>,
    pub(crate) mem_range_max: Option<f64>,
    /// Sample interval, mirrored so the web UI can display it.
    pub(crate) poll_ms: u64,
    /// The "Show graphs" toggle, mirrored so the web UI matches the GUI.
    pub(crate) show_graphs: bool,
    /// The applied limits and sounds, mirrored so the poller (alert sounds)
    /// and the web page (thresholds) follow the settings dialog.
    pub(crate) settings: Settings,
    /// Latest status of the privileged power helper, set by the poller after
    /// each sample. Desktop-UI display only; deliberately not in the web JSON.
    pub(crate) power_note: Option<String>,
    /// Authentication state of the privileged power helper, set by the poller
    /// after each sample. Desktop-UI only; deliberately not in the web JSON.
    pub(crate) power_auth: AuthState,
    /// Last critical-alert safeguard action (killed/could-not-kill note)
    /// with its age reference (seconds since `start`). Desktop-UI only (and
    /// deliberately not in the web JSON); the UI shows it only while fresh.
    pub(crate) kill_note: Option<(f64, String)>,
    /// Latching critical-current notification: set by the poller on the
    /// rising edge into the red range, shown (flashing) in both UIs until a
    /// user acknowledges it — an "Acknowledge" click in the desktop UI or a
    /// `POST /ack` on the web page. The current returning to normal never
    /// clears it; the next red excursion arms it again.
    pub(crate) critical_alert: bool,
    /// Why the latest sample has no pin readings (no IT8915 bus, I2C device
    /// not accessible, read failure); `None` while the readings are fine.
    /// Shown as a sensor-fault banner in both UIs.
    pub(crate) sensor_error: Option<String>,
    /// When (seconds since `start`) the last valid pin readings were
    /// recorded; `None` before the first one. Drives the staleness warning
    /// (see [`stale_after_secs`]).
    pub(crate) pins_at: Option<f64>,
    /// When (seconds since `start`) the newest sample of any kind was
    /// recorded; `None` before the first one. The desktop graphs scroll by
    /// this time, not the wall clock, so they move exactly one sample per
    /// sample instead of by however much time passed between repaints.
    pub(crate) sampled_at: Option<f64>,
    /// Called by the poller (outside the lock) after each recorded sample:
    /// the GUI sets it to request a repaint, so frames follow the data
    /// instead of a separate timer that drifts against the poll cadence.
    pub(crate) on_sample: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// This process is the background service (`--service`): mirrored to
    /// the web JSON as `service` so the page says so.
    pub(crate) service_mode: bool,
    /// The background service's monitoring state as far as the UIs care:
    /// in the service itself its own web-server state (published to
    /// clients); in the desktop app `Some` while it is showing a running
    /// service's data instead of polling the hardware itself (see
    /// `service::spawn_monitor`).
    pub(crate) service_link: Option<ServiceLink>,
    /// Set while this process's own poller monitors the hardware (the
    /// desktop app without a background service). The desktop app only
    /// runs its own web server then; with a service the service serves it.
    pub(crate) local_monitor: bool,
    /// Set while the automatic power limit has the GPU powered down (the
    /// limit lowered by an alert and not restored yet). Published by the
    /// poller after every sample; shown as a banner in both UIs.
    pub(crate) power_down: Option<PowerDown>,
}

/// The automatic power limit's lowering in effect (see
/// [`WebData::power_down`]).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PowerDown {
    /// The limit (W) that comes back when it restores.
    pub(crate) saved_w: u32,
    /// The current (A) the pins must stay below for the restore.
    pub(crate) trigger_a: f64,
    /// Seconds until the restore; `None` while the current is still at or
    /// above the trigger (the countdown has not started).
    pub(crate) restore_in: Option<f64>,
}

/// What a desktop app showing a background service's data knows about it
/// (see [`WebData::service_link`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ServiceLink {
    /// Port of the service's web interface while it is serving.
    pub(crate) web_port: Option<u16>,
    /// The address it listens on (`None` from an older service: all).
    pub(crate) web_bind: Option<IpAddr>,
    /// Why the service's web interface is not serving although enabled
    /// (port busy, invalid port); it keeps retrying.
    pub(crate) web_error: Option<String>,
    /// Set when the running service is a different build that speaks
    /// another link protocol: its data cannot be shown until it restarts.
    pub(crate) incompatible: Option<String>,
}

impl WebData {
    pub(crate) fn new() -> Self {
        let mut d = WebData {
            sample: Sample::default(),
            start: std::time::Instant::now(),
            history: vec![Vec::new(); NUM_PINS],
            power_history: Vec::new(),
            temp_history: Vec::new(),
            mem_history: Vec::new(),
            power_range_max: None,
            temp_range_max: None,
            mem_range_max: None,
            poll_ms: DEFAULT_POLL_MS,
            show_graphs: true,
            settings: Settings::default(),
            power_note: None,
            power_auth: AuthState::Idle,
            kill_note: None,
            critical_alert: false,
            sensor_error: None,
            pins_at: None,
            sampled_at: None,
            on_sample: None,
            service_mode: false,
            service_link: None,
            local_monitor: false,
            power_down: None,
        };
        // Pre-reserve history for the default 1 Hz sample rate so the common
        // case never reallocates; a faster slider setting just grows the vecs.
        let reserve = (HISTORY_SECS * 1000.0 / DEFAULT_POLL_MS as f64) as usize + 1024;
        for h in &mut d.history {
            h.reserve(reserve);
        }
        d.power_history.reserve(reserve);
        d.temp_history.reserve(reserve);
        d.mem_history.reserve(reserve);
        d
    }

    /// Fold one poller sample into the shared state: replace the latest
    /// sample, append it to the histories (pruning what expired) and cache
    /// the permanent NVML ranges. Called from the poller thread so data
    /// keeps flowing while the GUI window is minimized or not drawing.
    pub(crate) fn record(&mut self, sample: Sample) {
        let now = self.start.elapsed().as_secs_f64();
        self.sampled_at = Some(now);
        // Without valid pin readings (see `Sample::pins`) the pin histories
        // get a gap instead of made-up zeros.
        let pins_valid = sample.pins.len() == NUM_PINS;
        if pins_valid {
            for (hist, pin) in self.history.iter_mut().zip(&sample.pins) {
                hist.push((now, pin.current));
            }
            self.pins_at = Some(now);
        }
        // Power: prefer NVML, fall back to the estimated pin sum so the
        // graph still has data when NVML is unavailable.
        let power_val = sample.power_w.or_else(|| {
            pins_valid.then(|| {
                sample
                    .pins
                    .iter()
                    .map(|p| p.voltage * p.current)
                    .sum::<f64>()
            })
        });
        if let Some(p) = power_val {
            self.power_history.push((now, p));
        }
        if let Some(t) = sample.temp_c {
            self.temp_history.push((now, t));
        }
        if let Some(m) = sample.mem_used_gb {
            self.mem_history.push((now, m));
        }
        // Cache the permanent y ranges reported by NVML. They are static
        // per GPU, so the first successful poll fixes the graph scales.
        if let Some(m) = sample.limit_max_w {
            self.power_range_max = Some(m as f64);
        }
        if let Some(m) = sample.max_temp_c {
            self.temp_range_max = Some(m);
        }
        if let Some(m) = sample.mem_total_gb {
            self.mem_range_max = Some(m);
        }
        self.sample = sample;

        // Prune what fell out of the largest horizon.
        let t_min = now - HISTORY_SECS;
        let prune = |hist: &mut Vec<(f64, f64)>| {
            let k = hist.partition_point(|(t, _)| *t < t_min);
            hist.drain(..k);
        };
        for hist in &mut self.history {
            prune(hist);
        }
        prune(&mut self.power_history);
        prune(&mut self.temp_history);
        prune(&mut self.mem_history);
    }
}

/// Web server lifecycle. Dropping the `Running` state (or replacing it with
/// `Stopped`) stops the server and joins its thread.
pub(crate) enum WebState {
    Stopped,
    Running(WebServer),
}

pub(crate) struct WebServer {
    /// Where it listens (address and port).
    pub(crate) addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl WebServer {
    /// Bind the port synchronously so a busy port is reported to the UI
    /// before the checkbox is accepted, then spawn the serve thread.
    pub(crate) fn start(addr: SocketAddr, data: Arc<Mutex<WebData>>) -> io::Result<WebServer> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = thread::spawn(move || web_serve(listener, data, thread_stop));
        println!("Web interface listening on http://{local}/");
        Ok(WebServer {
            addr: local,
            stop,
            handle: Some(handle),
        })
    }

    /// Ask the serve loop to exit and wait for it. Bounded: the loop polls
    /// the flag every 50 ms and never blocks on the data mutex.
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        println!("Web interface stopped ({})", self.addr);
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn web_serve(listener: TcpListener, data: Arc<Mutex<WebData>>, stop: Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                // The accepted socket inherits the listener's non-blocking
                // mode; put it back to blocking for the request.
                let _ = stream.set_nonblocking(false);
                let data = data.clone();
                thread::spawn(move || web_handle(stream, data));
            }
            // No connection yet: poll the stop flag again shortly.
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Serve one HTTP connection: read the request head, answer with the page,
/// the JSON data, or an error, then close the connection.
fn web_handle(mut stream: TcpStream, data: Arc<Mutex<WebData>>) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

    let mut buf = [0u8; 4096];
    let mut head: Vec<u8> = Vec::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if head.len() > 8192 {
                    return;
                }
            }
            Err(_) => return,
        }
    }

    let request = String::from_utf8_lossy(&head);
    let request_line = request.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    match (method, path) {
        ("GET", "/" | "/index.html") => {
            web_respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                INDEX_HTML,
            );
        }
        ("GET", "/data") => {
            let window_secs = query_param(query, "window")
                .and_then(|w| w.parse::<u64>().ok())
                .filter(|w| (60..=3600).contains(w))
                .unwrap_or(60);
            // Try (re-)locking instead of blocking so a busy GUI frame can
            // never deadlock against the shutdown join; clients retry on 503.
            let mut body = None;
            for _ in 0..10 {
                if let Ok(d) = data.try_lock() {
                    body = Some(web_json(&d, window_secs as f64));
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            match body {
                Some(body) => web_respond(
                    &mut stream,
                    "200 OK",
                    "application/json; charset=utf-8",
                    &body,
                ),
                None => web_respond(&mut stream, "503 Service Unavailable", "text/plain", "busy"),
            }
        }
        ("POST", "/ack") => {
            // Acknowledge the critical-current alert: clears the latching
            // notification in both UIs. Same try-lock pattern as /data so a
            // connection thread can never block the shutdown join.
            let mut cleared = false;
            for _ in 0..10 {
                if let Ok(mut d) = data.try_lock() {
                    d.critical_alert = false;
                    cleared = true;
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            if cleared {
                web_respond(&mut stream, "200 OK", "text/plain", "OK");
            } else {
                web_respond(&mut stream, "503 Service Unavailable", "text/plain", "busy");
            }
        }
        ("GET", _) => web_respond(&mut stream, "404 Not Found", "text/plain", "Not found"),
        _ => web_respond(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            "Method not allowed",
        ),
    }
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .find_map(|pair| pair.split_once('='))
        .and_then(|(k, v)| (k == key).then_some(v))
}

/// Write a complete HTTP response; the connection is then closed.
fn web_respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Serialize the shared state for one web client: the latest sample plus
/// the history for the requested time window (seconds). Hand-rolled to keep
/// the dependency list short.
fn web_json(data: &WebData, window_secs: f64) -> String {
    let now = data.start.elapsed().as_secs_f64();
    let t_min = now - window_secs;
    let s = &data.sample;

    let mut out = String::with_capacity(64 * 1024);
    out.push_str("{\"now\":");
    json_f(&mut out, now, 1);
    out.push_str(",\"window\":");
    out.push_str(&(window_secs as u64).to_string());
    out.push_str(",\"poll_ms\":");
    out.push_str(&data.poll_ms.to_string());
    out.push_str(",\"limits\":[");
    out.push_str(&format!(
        "{:.1},{:.1}",
        data.settings.yellow_limit_a, data.settings.red_limit_a
    ));
    out.push(']');
    out.push_str(",\"show_graphs\":");
    out.push_str(if data.show_graphs { "true" } else { "false" });
    out.push_str(",\"critical_alert\":");
    out.push_str(if data.critical_alert { "true" } else { "false" });
    out.push_str(",\"power_down\":");
    match &data.power_down {
        Some(pd) => {
            out.push_str(&format!(
                "{{\"saved\":{},\"trigger\":{:.1},\"restore_in\":",
                pd.saved_w, pd.trigger_a
            ));
            json_opt_f(&mut out, pd.restore_in, 0);
            out.push('}');
        }
        None => out.push_str("null"),
    }
    out.push_str(",\"service\":");
    out.push_str(if data.service_mode { "true" } else { "false" });
    out.push_str(",\"safeguard\":");
    out.push_str(if data.settings.kill_top_process_on_red {
        "true"
    } else {
        "false"
    });
    out.push_str(",\"sensor_error\":");
    match &data.sensor_error {
        Some(e) => json_string(&mut out, e),
        None => out.push_str("null"),
    }
    // Age of the last valid pin reading (null: none yet).
    out.push_str(",\"pins_age\":");
    json_opt_f(&mut out, data.pins_at.map(|t| now - t), 1);
    out.push_str(",\"sample\":{\"pins\":[");
    for (i, p) in s.pins.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("[{:.3},{:.3}]", p.voltage, p.current));
    }
    out.push_str("],\"power_w\":");
    json_opt_f(&mut out, s.power_w, 1);
    out.push_str(",\"limit_w\":");
    json_opt_u32(&mut out, s.limit_w);
    out.push_str(",\"limit_min_w\":");
    json_opt_u32(&mut out, s.limit_min_w);
    out.push_str(",\"limit_max_w\":");
    json_opt_u32(&mut out, s.limit_max_w);
    out.push_str(",\"temp_c\":");
    json_opt_f(&mut out, s.temp_c, 1);
    out.push_str(",\"max_temp_c\":");
    json_opt_f(&mut out, s.max_temp_c, 1);
    out.push_str(",\"mem_used_gb\":");
    json_opt_f(&mut out, s.mem_used_gb, 2);
    out.push_str(",\"mem_total_gb\":");
    json_opt_f(&mut out, s.mem_total_gb, 1);
    // Top GPU processes as [pid, name, mem GB, util %], highest first.
    out.push_str("},\"processes\":[");
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    for (i, p) in s.processes.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        out.push_str(&p.pid.to_string());
        out.push(',');
        json_string(&mut out, &p.name);
        out.push_str(&format!(
            ",{:.2},{}",
            p.mem_bytes as f64 / GIB,
            p.util_percent
        ));
        out.push(']');
    }
    out.push_str("],\"range_max\":{\"power\":");
    json_opt_f(&mut out, data.power_range_max, 1);
    out.push_str(",\"temp\":");
    json_opt_f(&mut out, data.temp_range_max, 1);
    out.push_str(",\"mem\":");
    json_opt_f(&mut out, data.mem_range_max, 1);
    out.push_str("},\"data\":{\"pins\":[");
    for (i, hist) in data.history.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_series(&mut out, hist, t_min, window_secs);
    }
    out.push_str("],\"power\":");
    push_series(&mut out, &data.power_history, t_min, window_secs);
    out.push_str(",\"temp\":");
    push_series(&mut out, &data.temp_history, t_min, window_secs);
    out.push_str(",\"mem\":");
    push_series(&mut out, &data.mem_history, t_min, window_secs);
    out.push_str("}}");
    out
}

/// M4 downsampling for line graphs: split the time-sorted `pts` into
/// buckets of `bucket_secs` and keep each bucket's first, min, max and last
/// point (in time order, duplicates once). With about one bucket per pixel,
/// a polyline through the result draws the same picture as one through
/// every sample: no spike is ever dropped.
///
/// Buckets are aligned to absolute time (`floor(t / bucket_secs)`), not to
/// the window start, so as the window slides every complete bucket keeps
/// exactly the same points and the line does not shimmer. (Picking every
/// Nth sample counted from the window start chose a different subset on
/// each poll.) Mirrored by `m4` in `INDEX_HTML`'s JS.
pub(crate) fn m4_downsample(pts: &[(f64, f64)], bucket_secs: f64) -> Vec<(f64, f64)> {
    if !bucket_secs.is_finite() || bucket_secs <= 0.0 || pts.len() <= 4 {
        return pts.to_vec();
    }
    let mut out = Vec::with_capacity(pts.len().min(4 * 1024));
    let mut start = 0;
    while start < pts.len() {
        let b = (pts[start].0 / bucket_secs).floor();
        let mut end = start + 1;
        while end < pts.len() && (pts[end].0 / bucket_secs).floor() == b {
            end += 1;
        }
        let bucket = &pts[start..end];
        let (mut lo, mut hi) = (0, 0);
        for (i, p) in bucket.iter().enumerate() {
            if p.1 < bucket[lo].1 {
                lo = i;
            }
            if p.1 > bucket[hi].1 {
                hi = i;
            }
        }
        let mut idx = [0, lo, hi, bucket.len() - 1];
        idx.sort_unstable();
        for (n, &i) in idx.iter().enumerate() {
            if n == 0 || idx[n - 1] != i {
                out.push(bucket[i]);
            }
        }
        start = end;
    }
    out
}

/// Append one history as a JSON array of [t, v] pairs, restricted to the
/// visible window and M4-downsampled into WEB_BUCKETS time buckets (the
/// newest sample is always the last point of the last bucket).
fn push_series(out: &mut String, hist: &[(f64, f64)], t_min: f64, window: f64) {
    let k = hist.partition_point(|(t, _)| *t < t_min);
    let pts = m4_downsample(&hist[k..], window / WEB_BUCKETS);
    out.push('[');
    for (i, (t, v)) in pts.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("[{t:.2},{v:.2}]"));
    }
    out.push(']');
}

fn json_f(out: &mut String, v: f64, decimals: usize) {
    out.push_str(&format!("{v:.prec$}", prec = decimals));
}

fn json_opt_f(out: &mut String, v: Option<f64>, decimals: usize) {
    match v {
        Some(v) => json_f(out, v, decimals),
        None => out.push_str("null"),
    }
}

fn json_opt_u32(out: &mut String, v: Option<u32>) {
    match v {
        Some(v) => out.push_str(&v.to_string()),
        None => out.push_str("null"),
    }
}

/// Append a JSON string literal, escaping what JSON requires (process names
/// come from `/proc` and may contain anything except NUL).
fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The single-page web UI. It polls /data and mirrors the graphical UI:
/// the same metrics, tables, color coding and graphs, with a variable
/// 1..60 minute time window.
const INDEX_HTML: &str = r###"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>AstralGauge</title>
<style>
  /* Mirrors ui::palette. */
  :root {
    color-scheme: dark;
    --bg: #0e1014; --card: #171a20; --card-stroke: #242830; --tile: #1e2229; --plot: #111318;
    --track: #2a2f38; --text: #e6e9ee; --dim: #8b93a0; --accent: #60a5fa;
    --green: #4ade80; --yellow: #facc15; --red: #ef4444; --orange: #fb923c;
    --power: #fbb03b; --temp: #f87171; --vram: #81b2fa; --pdown: #c084fc;
  }
  * { box-sizing: border-box; }
  body { margin: 0; background: var(--bg); color: var(--text); font-family: system-ui, -apple-system, 'Segoe UI', sans-serif; font-size: 14px; }
  .wrap { max-width: 1100px; margin: 0 auto; padding: 16px 16px 40px; display: flex; flex-direction: column; gap: 10px; }
  .mono { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
  .top { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
  .title { font-size: 21px; font-weight: 700; }
  .sub { color: var(--dim); font-size: 13px; }
  .pill { margin-left: auto; display: inline-flex; align-items: center; gap: 6px; padding: 3px 10px; border-radius: 999px; font-size: 12px; font-weight: 600; color: var(--dim); background: rgba(139,147,160,.15); }
  .pill i { width: 7px; height: 7px; border-radius: 50%; background: currentColor; }
  .pill.live { color: var(--green); background: rgba(74,222,128,.15); }
  .pill.warn { color: var(--orange); background: rgba(251,146,60,.15); }
  .svc { display: none; padding: 3px 10px; border-radius: 999px; font-size: 12px; font-weight: 600; color: var(--accent); background: rgba(96,165,250,.15); }
  .banner { display: none; align-items: center; flex-wrap: wrap; gap: 12px; padding: 9px 12px; border-radius: 10px; font-weight: 700; }
  .banner.alert { background: #991b1b; border: 1px solid var(--red); color: #fff; font-size: 15px; animation: blink 1s step-end infinite; }
  .banner.warn { background: #3a240e; border: 1px solid var(--orange); color: #fdba74; }
  @keyframes blink { 50% { background: #451212; } }
  .banner.red { background: #451212; border: 1px solid var(--red); color: #fecaca; font-size: 15px; }
  .banner.yellow { background: #423408; border: 1px solid var(--yellow); color: #fde68a; font-size: 15px; }
  .banner.pdown { background: #2e1a4a; border: 1px solid var(--pdown); color: #e9d5ff; }
  .banner .bdot { width: 9px; height: 9px; border-radius: 50%; background: currentColor; flex: none; }
  .banner .why { font-weight: 400; opacity: .85; }
  .pill.yel { color: var(--yellow); background: rgba(250,204,21,.15); }
  .pill.crit { color: var(--red); background: rgba(239,68,68,.18); }
  /* Colored edge around the whole page while a limit is exceeded. */
  #edge { position: fixed; inset: 0; pointer-events: none; border: 4px solid transparent; border-radius: 6px; z-index: 10; }
  #edge.yellow { border-color: var(--yellow); }
  #edge.red { border-color: var(--red); }
  button { font: inherit; font-size: 13px; color: var(--text); background: #262b34; border: 1px solid #303640; border-radius: 4px; padding: 3px 10px; cursor: pointer; }
  button:hover { background: #323844; border-color: #464e5c; }
  .card { background: var(--card); border: 1px solid var(--card-stroke); border-radius: 10px; padding: 12px; min-width: 0; }
  .head { display: flex; align-items: center; gap: 10px; margin-bottom: 8px; flex-wrap: wrap; }
  .cap { font-size: 11px; font-weight: 600; letter-spacing: .04em; text-transform: uppercase; color: var(--dim); }
  .head > :last-child { margin-left: auto; }
  .stats { display: grid; grid-template-columns: repeat(3, 1fr); gap: 10px; }
  .big { font-size: 26px; font-weight: 600; margin: 2px 0 6px; font-variant-numeric: tabular-nums; }
  .big small { font-size: 14px; font-weight: 400; color: var(--dim); margin-left: 4px; }
  .foot { font-size: 12px; color: var(--dim); margin-top: 6px; }
  .meter { position: relative; height: 6px; border-radius: 3px; background: var(--track); }
  .meter b { position: absolute; left: 0; top: 0; bottom: 0; border-radius: 3px; min-width: 6px; }
  .meter b.none { display: none; }
  .meter i { position: absolute; top: -2px; bottom: -2px; width: 2px; margin-left: -1px; box-shadow: 0 0 0 1px var(--tile); }
  .legend { display: flex; gap: 12px; font-size: 11px; }
  .dot::before { content: ''; display: inline-block; width: 7px; height: 7px; border-radius: 50%; background: currentColor; margin-right: 5px; }
  .pins { display: grid; grid-template-columns: repeat(3, 1fr); gap: 8px; }
  .tile { background: var(--tile); border-radius: 8px; padding: 8px 10px; }
  .tile.y { background: #342e14; }
  .tile.r { background: #40181b; }
  .tile .th { display: flex; justify-content: space-between; }
  .tile .big { font-size: 22px; }
  .proc { display: grid; grid-template-columns: minmax(0, 1fr) 80px 48px 84px; gap: 12px; align-items: center; height: 22px; font-size: 13px; }
  .proc .name { overflow: hidden; white-space: nowrap; text-overflow: ellipsis; }
  .proc .pct { color: var(--dim); text-align: right; font-size: 12px; }
  .proc .mem { text-align: right; font-size: 12px; }
  .tag { font-size: 10px; color: var(--red); background: rgba(239,68,68,.18); border-radius: 4px; padding: 1px 4px; margin-left: 8px; }
  .armed { font-size: 11px; color: var(--red); display: none; }
  .seg { display: flex; gap: 4px; flex-wrap: wrap; }
  .seg button { background: transparent; border-color: transparent; color: var(--text); }
  .seg button:hover { background: #262b34; }
  .seg button.on { background: #2563eb; color: #fff; }
  .grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: 10px; }
  .cell { min-width: 0; }
  .gt { display: flex; justify-content: space-between; font-size: 12px; color: var(--dim); margin-bottom: 3px; }
  .gt span:last-child { font-weight: 600; }
  canvas { display: block; width: 100%; height: 120px; background: var(--plot); border-radius: 6px; }
  .dim { color: var(--dim); }
  #err { display: none; color: var(--red); }
  @media (max-width: 760px) {
    .stats { grid-template-columns: 1fr; }
    .pins, .grid { grid-template-columns: repeat(2, 1fr); }
  }
  @media (max-width: 420px) { .grid { grid-template-columns: 1fr; } }
</style>
</head>
<body>
<div id="edge"></div>
<div class="wrap">
  <div class="top">
    <span class="title">AstralGauge</span>
    <span class="sub">ASUS RTX 5090 ROG Astral</span>
    <span class="pill" id="status"><i></i><span>Starting</span></span>
    <span class="svc" id="svc" title="Served by the AstralGauge background service (astral-gauge.service)">Service</span>
  </div>
  <div class="banner" id="level"></div>
  <div class="banner pdown" id="pdown"></div>
  <div class="banner alert" id="alert"><span>⚠ ALERT: critical current detected! Please check your GPU power cables!</span><button id="ackbtn" type="button">Acknowledge</button></div>
  <div class="banner warn" id="sensor"></div>
  <div class="stats">
    <div class="card">
      <div class="cap" id="powercap">Board power</div>
      <div class="big"><span id="power">—</span><small>W</small></div>
      <div class="meter"><b id="powerbar" style="background: var(--power)"></b></div>
      <div class="foot" id="limit"></div>
    </div>
    <div class="card">
      <div class="cap">GPU temperature</div>
      <div class="big"><span id="temp">—</span><small>°C</small></div>
      <div class="meter"><b id="tempbar" style="background: var(--temp)"></b></div>
      <div class="foot" id="tempfoot"></div>
    </div>
    <div class="card">
      <div class="cap">VRAM</div>
      <div class="big"><span id="vram">—</span><small>GB</small></div>
      <div class="meter"><b id="vrambar" style="background: var(--vram)"></b></div>
      <div class="foot" id="vramfoot"></div>
    </div>
  </div>
  <div class="card">
    <div class="head"><span class="cap">Connector pins</span><span class="legend" id="legend"></span></div>
    <div class="pins" id="pins"></div>
  </div>
  <div class="card">
    <div class="head"><span class="cap">Top GPU processes</span><span class="armed" id="armed" title="On a red current, the first process in this list is killed">kill safeguard armed</span></div>
    <div id="procs"></div>
  </div>
  <div class="card" id="graphs">
    <div class="head"><span class="cap">History</span><div class="seg" id="windows"></div></div>
    <div class="grid" id="grid"></div>
  </div>
  <div class="foot" id="interval"></div>
  <div id="err"></div>
</div>
<script>
'use strict';

const HORIZONS = [1, 5, 10, 15, 30, 60];
let windowMin = 1;
// [yellow, red] current limits (A); updated from the settings dialog.
let limits = [9.2, 10.0];

// Mirrors ui::palette, as [r, g, b] for canvas drawing.
const C = {
  plot: [17, 19, 24], track: [42, 47, 56], text: [230, 233, 238], dim: [139, 147, 160],
  stroke: [36, 40, 48], tile: [30, 34, 41],
  green: [74, 222, 128], yellow: [250, 204, 21], red: [239, 68, 68],
  power: [251, 176, 59], temp: [248, 113, 113], vram: [129, 178, 250]
};
function rgba(c, a) { return 'rgba(' + c[0] + ',' + c[1] + ',' + c[2] + ',' + (a == null ? 1 : a) + ')'; }

const winsEl = document.getElementById('windows');
HORIZONS.forEach(function (m) {
  const b = document.createElement('button');
  b.type = 'button';
  b.textContent = m + 'm';
  b.addEventListener('click', function () { windowMin = m; syncWindowButtons(); tick(); });
  winsEl.appendChild(b);
});
function syncWindowButtons() {
  Array.from(winsEl.children).forEach(function (b, i) {
    b.classList.toggle('on', HORIZONS[i] === windowMin);
  });
}
syncWindowButtons();

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}

// Pin tiles: pin number + voltage, current, meter with the limits marked.
const pinsEl = document.getElementById('pins');
const tiles = [];
for (let i = 0; i < 6; i++) {
  const tile = el('div', 'tile');
  const th = el('div', 'th');
  th.appendChild(el('span', 'cap', 'Pin ' + i));
  const volt = el('span', 'foot', '— V');
  volt.style.marginTop = '0';
  th.appendChild(volt);
  const big = el('div', 'big');
  const cur = el('span', null, '—');
  big.appendChild(cur);
  big.appendChild(el('small', null, 'A'));
  const meter = el('div', 'meter');
  const bar = el('b');
  const ty = el('i');
  const tr = el('i');
  ty.style.background = rgba(C.yellow);
  tr.style.background = rgba(C.red);
  meter.appendChild(bar);
  meter.appendChild(ty);
  meter.appendChild(tr);
  tile.appendChild(th);
  tile.appendChild(big);
  tile.appendChild(meter);
  pinsEl.appendChild(tile);
  tiles.push({ tile: tile, volt: volt, cur: cur, bar: bar, ty: ty, tr: tr });
}

const NAMES = ['Pin 0', 'Pin 1', 'Pin 2', 'Pin 3', 'Pin 4', 'Pin 5', 'Power', 'Temperature', 'VRAM'];
const METRIC_COLORS = { 6: C.power, 7: C.temp, 8: C.vram };
const UNITS = ['A', 'A', 'A', 'A', 'A', 'A', 'W', '°C', 'GB'];
const grid = document.getElementById('grid');
const canvases = [];
const graphValues = [];
// Last mouse position per canvas (CSS px, null when the pointer is away).
const hoverPos = new Array(NAMES.length).fill(null);
// Latest drawGraph arguments, so mouse moves can redraw between polls.
const drawArgs = new Array(NAMES.length).fill(null);
NAMES.forEach(function (n, i) {
  const cell = el('div', 'cell');
  const title = el('div', 'gt');
  title.appendChild(el('span', null, n));
  const value = el('span', 'mono');
  title.appendChild(value);
  const cv = document.createElement('canvas');
  cell.appendChild(title);
  cell.appendChild(cv);
  grid.appendChild(cell);
  canvases.push(cv);
  graphValues.push(value);
  cv.addEventListener('mousemove', function (e) {
    const r = cv.getBoundingClientRect();
    hoverPos[i] = { x: e.clientX - r.left, y: e.clientY - r.top };
    redrawGraph(i);
  });
  cv.addEventListener('mouseleave', function () {
    hoverPos[i] = null;
    redrawGraph(i);
  });
});
function redrawGraph(i) {
  const a = drawArgs[i];
  if (a) drawGraph(canvases[i], a[0], a[1], a[2], a[3], a[4]);
}

function currentRgb(v) {
  if (v < limits[0]) return C.green;
  if (v < limits[1]) return C.yellow;
  return C.red;
}
function currentColor(v) { return rgba(currentRgb(v)); }

// M4 downsampling, mirroring web::m4_downsample: per time bucket of
// `bucket` seconds (aligned to absolute time), keep the first, min, max and
// last point in time order.
function m4(pts, bucket) {
  if (!(bucket > 0) || pts.length <= 4) return pts;
  const out = [];
  let s = 0;
  while (s < pts.length) {
    const b = Math.floor(pts[s][0] / bucket);
    let e = s + 1;
    while (e < pts.length && Math.floor(pts[e][0] / bucket) === b) e++;
    let lo = s, hi = s;
    for (let i = s; i < e; i++) {
      if (pts[i][1] < pts[lo][1]) lo = i;
      if (pts[i][1] > pts[hi][1]) hi = i;
    }
    const idx = [s, lo, hi, e - 1].sort(function (a, b) { return a - b; });
    for (let n = 0; n < 4; n++) {
      if (n === 0 || idx[n] !== idx[n - 1]) out.push(pts[idx[n]]);
    }
    s = e;
  }
  return out;
}

function drawGraph(cv, pts, now, win, fixedMax, idx) {
  drawArgs[idx] = [pts, now, win, fixedMax, idx];
  const dpr = window.devicePixelRatio || 1;
  const w = cv.clientWidth, h = cv.clientHeight;
  if (w === 0 || h === 0) return;
  const pw = Math.round(w * dpr), ph = Math.round(h * dpr);
  if (cv.width !== pw || cv.height !== ph) { cv.width = pw; cv.height = ph; }
  const ctx = cv.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);

  const isPin = idx < 6;
  const t0 = now - win;
  let k = 0;
  while (k < pts.length && pts[k][0] < t0) k++;
  const vis = pts.slice(k);
  const n = vis.length;

  // Newest value in the title row.
  const valEl = graphValues[idx];
  if (n > 0) {
    const v = vis[n - 1][1];
    valEl.textContent = v.toFixed(idx === 7 ? 0 : (isPin ? 2 : 1)) + ' ' + UNITS[idx];
    valEl.style.color = isPin ? currentColor(v) : rgba(METRIC_COLORS[idx]);
  } else {
    valEl.textContent = '';
  }

  let yMax, baseMax = 0;
  if (isPin) {
    // keep the limit lines visible (30 % headroom above red leaves room for
    // its label), grow the scale for spikes
    baseMax = limits[1] * 1.3;
    yMax = baseMax;
    for (let i = 0; i < n; i++) yMax = Math.max(yMax, vis[i][1] * 1.05);
  } else if (fixedMax && fixedMax > 0) {
    yMax = fixedMax;
  } else {
    yMax = 1.0;
    for (let i = 0; i < n; i++) yMax = Math.max(yMax, vis[i][1]);
    yMax *= 1.08;
  }

  const xOf = function (t) { return ((t - t0) / win) * w; };
  const yOf = function (v) {
    if (isPin) return (1 - v / yMax) * h;
    return (1 - Math.min(Math.max(v, 0), yMax) / yMax) * h;
  };

  // Faint horizontal grid at the quarters.
  ctx.strokeStyle = rgba(C.track, 0.6);
  ctx.lineWidth = 1;
  for (let i = 1; i < 4; i++) {
    const y = Math.round(h * i / 4) + 0.5;
    ctx.beginPath(); ctx.moveTo(4, y); ctx.lineTo(w - 4, y); ctx.stroke();
  }

  ctx.font = '9px monospace';
  if (!isPin || yMax > baseMax) {
    ctx.fillStyle = rgba(C.dim);
    ctx.textAlign = 'left';
    ctx.textBaseline = 'top';
    ctx.fillText(yMax.toFixed(0), 4, 3);
  }

  if (isPin) {
    // Dashed, muted limit lines; the red label above its line, the yellow
    // one below its own, each on a small backing and kept inside the plot.
    const lims = [[limits[0], C.yellow, false], [limits[1], C.red, true]];
    for (let i = 0; i < lims.length; i++) {
      const lim = lims[i][0], col = rgba(lims[i][1], 0.55), above = lims[i][2];
      const y = Math.round(yOf(lim)) + 0.5;
      ctx.strokeStyle = col;
      ctx.setLineDash([4, 3]);
      ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(w, y); ctx.stroke();
      ctx.setLineDash([]);
      const text = lim.toFixed(1);
      const tw = ctx.measureText(text).width, th = 9;
      let top = above ? y - 1 - th : y + 1;
      top = Math.min(Math.max(top, 2), h - 2 - th);
      ctx.fillStyle = rgba(C.plot, 0.85);
      ctx.fillRect(w - 4 - tw - 1, top - 1, tw + 2, th + 2);
      ctx.fillStyle = col;
      ctx.textAlign = 'left';
      ctx.textBaseline = 'top';
      ctx.fillText(text, w - 4 - tw, top);
    }
  }

  // Line through one M4 bucket per CSS pixel (time-aligned, so it stays
  // put while the window slides; spikes always survive) over a soft fill.
  // Each segment takes the color of its end point; runs of one color are
  // drawn (and filled) as one path.
  const line = m4(vis, win / w);
  const fillAlpha = isPin ? 0.16 : 0.22;
  function drawRun(run, c) {
    if (run.length < 2) return;
    const grad = ctx.createLinearGradient(0, 0, 0, h);
    grad.addColorStop(0, rgba(c, fillAlpha));
    grad.addColorStop(1, rgba(c, 0));
    ctx.beginPath();
    ctx.moveTo(run[0][0], h);
    for (const p of run) ctx.lineTo(p[0], p[1]);
    ctx.lineTo(run[run.length - 1][0], h);
    ctx.closePath();
    ctx.fillStyle = grad;
    ctx.fill();
    ctx.beginPath();
    ctx.moveTo(run[0][0], run[0][1]);
    for (let i = 1; i < run.length; i++) ctx.lineTo(run[i][0], run[i][1]);
    ctx.strokeStyle = rgba(c);
    ctx.lineWidth = 1.5;
    ctx.lineJoin = 'round';
    ctx.stroke();
  }
  let run = [], col = null;
  for (let j = 1; j < line.length; j++) {
    const c = isPin ? currentRgb(line[j][1]) : METRIC_COLORS[idx];
    if (c !== col) {
      drawRun(run, col);
      run = [[xOf(line[j - 1][0]), yOf(line[j - 1][1])]];
      col = c;
    }
    run.push([xOf(line[j][0]), yOf(line[j][1])]);
  }
  drawRun(run, col);

  // Hover readout, drawn last so it sits above the lines.
  drawHover(ctx, w, h, vis, t0, win, xOf, yOf, idx);
}

// Crosshair readout under the pointer, mirroring the desktop UI:
// nearest sample by time, a dim vertical line, a value-colored dot,
// and a value label along the top that flips sides near the right edge.
function drawHover(ctx, w, h, vis, t0, win, xOf, yOf, idx) {
  const hv = hoverPos[idx];
  if (!hv || vis.length === 0) return;

  const frac = Math.min(Math.max(hv.x / w, 0), 1);
  const t = t0 + frac * win;
  let k = 0;
  while (k < vis.length && vis[k][0] < t) k++;
  let p;
  if (k === 0) p = vis[0];
  else if (k >= vis.length) p = vis[vis.length - 1];
  else p = (t - vis[k - 1][0] <= vis[k][0] - t) ? vis[k - 1] : vis[k];
  const x = xOf(p[0]);
  const y = yOf(p[1]);

  ctx.strokeStyle = rgba(C.dim, 0.4);
  ctx.lineWidth = 1;
  ctx.beginPath(); ctx.moveTo(x, 0); ctx.lineTo(x, h); ctx.stroke();

  ctx.fillStyle = idx < 6 ? currentColor(p[1]) : rgba(METRIC_COLORS[idx]);
  ctx.beginPath(); ctx.arc(x, y, 3.5, 0, Math.PI * 2); ctx.fill();
  ctx.strokeStyle = rgba(C.plot);
  ctx.lineWidth = 1.5;
  ctx.stroke();

  const label = p[1].toFixed(2) + ' ' + UNITS[idx];
  ctx.font = '11px monospace';
  const lw = ctx.measureText(label).width;
  let lx = x + 8;
  if (lx + lw + 8 > w - 2) lx = Math.max(x - lw - 16, 2);
  ctx.fillStyle = rgba(C.tile);
  ctx.strokeStyle = rgba(C.stroke);
  ctx.lineWidth = 1;
  ctx.beginPath();
  if (ctx.roundRect) ctx.roundRect(lx, 4, lw + 8, 17, 4); else ctx.rect(lx, 4, lw + 8, 17);
  ctx.fill();
  ctx.stroke();
  ctx.fillStyle = rgba(C.text);
  ctx.textAlign = 'left';
  ctx.textBaseline = 'top';
  ctx.fillText(label, lx + 4, 7);
}

// Latching critical-current alert: shown while the poller has it armed,
// dismissed only by the Acknowledge button (which POSTs /ack).
const alertEl = document.getElementById('alert');
document.getElementById('ackbtn').addEventListener('click', function () {
  alertEl.style.display = 'none';
  fetch('/ack', { method: 'POST' }).catch(function () {});
});

// Mirrors web::stale_after_secs: three sample intervals, at least 3 s.
function staleAfter(pollMs) { return Math.max(3 * pollMs / 1000, 3); }

// The pins at or above the yellow / red limits, for the alert banner
// (mirrors ui::alert_summary). null while every pin is green.
function alertLevel(pins) {
  const red = [], yellow = [];
  pins.forEach(function (p, i) {
    const t = 'pin ' + i + ': ' + p[1].toFixed(2) + ' A';
    if (p[1] >= limits[1]) red.push(t); else if (p[1] >= limits[0]) yellow.push(t);
  });
  if (red.length) return { cls: 'red', title: 'CRITICAL CURRENT', why: red.join(', ') + ' (red limit ' + limits[1].toFixed(1) + ' A)' };
  if (yellow.length) return { cls: 'yellow', title: 'HIGH CURRENT', why: yellow.join(', ') + ' (warning limit ' + limits[0].toFixed(1) + ' A)' };
  return null;
}

function setStatus(text, cls) {
  const st = document.getElementById('status');
  st.className = 'pill' + (cls ? ' ' + cls : '');
  st.lastChild.textContent = text;
}

function setMeter(bar, frac) {
  if (frac == null || !isFinite(frac)) { bar.classList.add('none'); return; }
  bar.classList.remove('none');
  bar.style.width = (Math.min(Math.max(frac, 0), 1) * 100) + '%';
}

function render(d) {
  const s = d.sample;
  const pinsOk = s.pins.length === 6;
  if (d.limits) limits = d.limits;

  alertEl.style.display = d.critical_alert ? 'flex' : 'none';
  document.getElementById('svc').style.display = d.service ? 'inline-block' : 'none';

  // Sensor fault / stale readings: the pin currents are not being checked.
  const sensorEl = document.getElementById('sensor');
  let sensorMsg = '';
  const stale = d.pins_age == null ? d.now > staleAfter(d.poll_ms) : d.pins_age > staleAfter(d.poll_ms);
  if (d.sensor_error != null) {
    sensorMsg = '⚠ SENSOR UNAVAILABLE: ' + d.sensor_error + ' — pin currents are NOT monitored!';
    setStatus('Sensor fault', 'warn');
  } else if (stale) {
    sensorMsg = d.pins_age == null
      ? '⚠ No pin readings yet — pin currents are NOT monitored!'
      : '⚠ Pin readings are stale (last update ' + d.pins_age.toFixed(0) + ' s ago) — monitoring may have stopped!';
    setStatus('Stale', 'warn');
  } else if (d.pins_age == null) {
    setStatus('Starting', '');
  } else {
    setStatus('Live', 'live');
  }
  sensorEl.textContent = sensorMsg;
  sensorEl.style.display = sensorMsg ? 'flex' : 'none';

  // Live alert state (mirrors ui.rs): which pins are in the yellow / red
  // range right now, as a banner, the status pill, a colored page edge and
  // the tab title. Not while the readings are missing or stale.
  const lvl = (!sensorMsg && pinsOk) ? alertLevel(s.pins) : null;
  const levelEl = document.getElementById('level');
  levelEl.replaceChildren();
  if (lvl) {
    levelEl.className = 'banner ' + lvl.cls;
    levelEl.appendChild(el('span', 'bdot'));
    levelEl.appendChild(el('span', '', lvl.title));
    levelEl.appendChild(el('span', 'why', lvl.why));
    levelEl.style.display = 'flex';
    setStatus(lvl.cls === 'red' ? 'Critical' : 'Warning', lvl.cls === 'red' ? 'crit' : 'yel');
  } else {
    levelEl.style.display = 'none';
  }
  document.getElementById('edge').className = lvl ? lvl.cls : '';

  // GPU powered down by the automatic power limit.
  const pd = d.power_down;
  const pdEl = document.getElementById('pdown');
  pdEl.replaceChildren();
  if (pd) {
    const applied = s.limit_w != null && s.limit_min_w != null && s.limit_w <= s.limit_min_w;
    pdEl.appendChild(el('span', 'bdot'));
    pdEl.appendChild(el('span', '', applied
      ? 'GPU POWERED DOWN — power limit at the ' + s.limit_w + ' W minimum (normally ' + pd.saved + ' W)'
      : 'POWERING DOWN — lowering the power limit (normally ' + pd.saved + ' W) to its minimum…'));
    pdEl.appendChild(el('span', 'why', pd.restore_in != null
      ? 'Restores to ' + pd.saved + ' W in ' + Math.ceil(pd.restore_in) + ' s.'
      : 'Restores 30 s after every pin is back below ' + pd.trigger.toFixed(1) + ' A.'));
    pdEl.style.display = 'flex';
  } else {
    pdEl.style.display = 'none';
  }
  document.title = (lvl ? '⚠ ' + lvl.title + ' — ' : pd ? 'GPU POWERED DOWN — ' : '') + 'AstralGauge';

  // Summary cards. Total power prefers NVML and falls back to the sum of
  // the pins' V*A.
  let power = s.power_w;
  if (power == null && pinsOk) {
    power = 0;
    for (const p of s.pins) power += p[0] * p[1];
  }
  document.getElementById('powercap').textContent =
    s.power_w == null && power != null ? 'Board power (est.)' : 'Board power';
  document.getElementById('power').textContent = power == null ? '—' : power.toFixed(1);
  setMeter(document.getElementById('powerbar'), power != null && s.limit_w ? power / s.limit_w : null);
  let limitText = 'Limit —';
  if (s.limit_w != null) {
    limitText = 'Limit ' + s.limit_w + ' W';
    if (s.limit_min_w != null && s.limit_max_w != null) {
      limitText += ' · range ' + s.limit_min_w + '–' + s.limit_max_w + ' W';
    }
  }
  document.getElementById('limit').textContent = limitText;

  const maxTemp = d.range_max.temp;
  document.getElementById('temp').textContent = s.temp_c == null ? '—' : s.temp_c.toFixed(0);
  setMeter(document.getElementById('tempbar'), s.temp_c != null && maxTemp ? s.temp_c / maxTemp : null);
  document.getElementById('tempfoot').textContent = maxTemp ? 'max ' + maxTemp.toFixed(0) + ' °C' : '';

  document.getElementById('vram').textContent = s.mem_used_gb == null ? '—' : s.mem_used_gb.toFixed(1);
  setMeter(document.getElementById('vrambar'),
    s.mem_used_gb != null && s.mem_total_gb ? s.mem_used_gb / s.mem_total_gb : null);
  document.getElementById('vramfoot').textContent = s.mem_total_gb != null ? 'of ' + s.mem_total_gb.toFixed(0) + ' GB' : '';

  document.getElementById('interval').textContent = 'Sample interval: ' + d.poll_ms + ' ms';

  // Pin legend and tiles (dashes while there are no valid pin readings).
  const y = limits[0].toFixed(1), r = limits[1].toFixed(1);
  const legend = document.getElementById('legend');
  legend.innerHTML = '';
  [['< ' + y + ' A', C.green], [y + '–' + r + ' A', C.yellow], ['≥ ' + r + ' A', C.red]].forEach(function (e) {
    const sp = el('span', 'dot', e[0]);
    sp.style.color = rgba(e[1]);
    legend.appendChild(sp);
  });
  const scale = limits[1] * 1.25;
  for (let i = 0; i < 6; i++) {
    const t = tiles[i];
    const v = pinsOk ? s.pins[i][0] : null;
    const c = pinsOk ? s.pins[i][1] : null;
    t.volt.textContent = (v == null ? '—' : v.toFixed(2)) + ' V';
    t.cur.textContent = c == null ? '—' : c.toFixed(2);
    const col = c == null ? C.dim : currentRgb(c);
    t.cur.style.color = rgba(col);
    t.bar.style.background = rgba(col);
    setMeter(t.bar, c == null ? null : c / scale);
    t.tile.className = 'tile' + (c == null ? '' : (col === C.red ? ' r' : (col === C.yellow ? ' y' : '')));
    t.ty.style.left = (limits[0] / scale * 100) + '%';
    t.tr.style.left = (limits[1] / scale * 100) + '%';
  }

  // Top GPU-using processes (NVML): [pid, name, mem GB, util %], highest
  // first. With the kill safeguard on, the first one is its target.
  document.getElementById('armed').style.display = d.safeguard ? 'inline' : 'none';
  const procsEl = document.getElementById('procs');
  procsEl.innerHTML = '';
  const procs = d.processes || [];
  if (procs.length === 0) procsEl.appendChild(el('div', 'foot', 'No GPU-using processes'));
  procs.forEach(function (p, rank) {
    const row = el('div', 'proc');
    const name = el('div', 'name', String(p[1]));
    name.title = p[1] + ' (pid ' + p[0] + ')';
    if (rank === 0 && d.safeguard) name.appendChild(el('span', 'tag', 'TARGET'));
    const meter = el('div', 'meter');
    const bar = el('b');
    bar.style.background = 'var(--accent)';
    meter.appendChild(bar);
    setMeter(bar, p[3] > 0 ? p[3] / 100 : null);
    row.appendChild(name);
    row.appendChild(meter);
    row.appendChild(el('div', 'pct mono', p[3] + ' %'));
    row.appendChild(el('div', 'mem mono', p[2].toFixed(2) + ' GB'));
    procsEl.appendChild(row);
  });

  const graphsEl = document.getElementById('graphs');
  graphsEl.style.display = d.show_graphs ? '' : 'none';
  if (!d.show_graphs) return;
  const series = [
    d.data.pins[0], d.data.pins[1], d.data.pins[2],
    d.data.pins[3], d.data.pins[4], d.data.pins[5],
    d.data.power, d.data.temp, d.data.mem
  ];
  const fixedMax = [null, null, null, null, null, null, d.range_max.power, d.range_max.temp, d.range_max.mem];
  for (let i = 0; i < series.length; i++) {
    drawGraph(canvases[i], series[i], d.now, d.window, fixedMax[i], i);
  }
}

let fails = 0;
function showErr(msg) {
  const e = document.getElementById('err');
  e.style.display = msg ? 'block' : 'none';
  if (msg) e.textContent = msg;
}
async function tick() {
  try {
    const r = await fetch('/data?window=' + windowMin * 60, { cache: 'no-store' });
    if (!r.ok) throw new Error('HTTP ' + r.status);
    render(await r.json());
    fails = 0;
    showErr('');
  } catch (e) {
    if (++fails >= 5) {
      showErr('Cannot reach AstralGauge. Is the web interface enabled?');
      setStatus('Offline', 'warn');
    }
  }
}
setInterval(tick, 1000);
tick();
</script>
</body>
</html>
"###;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{GpuProcess, PinReading, Sample, NUM_PINS};
    use std::net::TcpListener;

    fn test_data() -> Arc<Mutex<WebData>> {
        let mut d = WebData::new();
        d.sample = Sample {
            pins: vec![
                PinReading {
                    voltage: 12.0,
                    current: 5.0,
                },
                PinReading {
                    voltage: 12.1,
                    current: 6.0,
                },
                PinReading {
                    voltage: 12.2,
                    current: 7.0,
                },
                PinReading {
                    voltage: 12.3,
                    current: 8.0,
                },
                PinReading {
                    voltage: 12.4,
                    current: 9.5,
                },
                PinReading {
                    voltage: 12.5,
                    current: 10.5,
                },
            ],
            power_w: Some(300.5),
            limit_w: Some(575),
            limit_min_w: Some(150),
            limit_max_w: Some(600),
            temp_c: Some(55.0),
            max_temp_c: Some(108.0),
            mem_used_gb: Some(20.5),
            mem_total_gb: Some(32.0),
            processes: vec![GpuProcess {
                pid: 4242,
                name: "game \"named\".exe".to_owned(),
                mem_bytes: (8.5 * 1024.0 * 1024.0 * 1024.0) as u64,
                util_percent: 97,
            }],
        };
        d.power_range_max = Some(600.0);
        d.temp_range_max = Some(108.0);
        d.mem_range_max = Some(32.0);
        d.critical_alert = true;
        let now = d.start.elapsed().as_secs_f64();
        for i in 0..NUM_PINS {
            for j in 0..10 {
                d.history[i].push((now - (10 - j) as f64, 5.0 + i as f64));
            }
        }
        for j in 0..10 {
            d.power_history.push((now - (10 - j) as f64, 300.0));
        }
        Arc::new(Mutex::new(d))
    }

    fn free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    }

    fn http_get(port: u16, target: &str) -> String {
        http_request("GET", port, target)
    }

    fn http_request(method: &str, port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let req =
            format!("{method} {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn web_json_contains_expected_fields() {
        let data = test_data();
        let d = data.lock().unwrap();
        let json = web_json(&d, 60.0);
        assert!(json.contains("\"window\":60"));
        assert!(json.contains("\"power_w\":300.5"));
        assert!(json.contains("\"limit_w\":575"));
        assert!(json.contains("\"temp_c\":55.0"));
        assert!(json.contains("\"mem_total_gb\":32.0"));
        assert!(json.contains("\"show_graphs\":true"));
        assert!(json.contains("\"critical_alert\":true"));
        assert!(json.contains("\"safeguard\":false"));
        assert!(json.contains("\"service\":false"));
        assert!(json.contains("\"power_down\":null"));
        assert!(json.contains("\"sensor_error\":null"));
        assert!(json.contains("\"pins_age\":null"));
        assert!(json.contains("\"poll_ms\":1000"));
        assert!(json.contains("\"limits\":[9.2,10.0]"));
        // Top process entry, with the process name JSON-escaped.
        assert!(json.contains(r#""processes":[[4242,"game \"named\".exe",8.50,97]]"#));
        // Two \"pins\" arrays: the latest sample and the history data.
        assert_eq!(json.matches("\"pins\":[").count(), 2);
        // All 10 per-pin history points fit inside the 60 s window. History
        // values use 2 decimals, so pin 0's ten 5.0 A samples read "5.00]"
        // (the sample pins use 3 decimals and cannot collide).
        assert_eq!(json.matches("5.00]").count(), 10);
    }

    #[test]
    fn web_data_record_appends_and_caches_ranges() {
        let mut d = WebData::new();
        let s = Sample {
            pins: vec![
                PinReading {
                    voltage: 12.0,
                    current: 5.0,
                };
                NUM_PINS
            ],
            power_w: None,
            limit_w: Some(575),
            limit_min_w: Some(150),
            limit_max_w: Some(600),
            temp_c: Some(55.0),
            max_temp_c: Some(108.0),
            mem_used_gb: Some(20.5),
            mem_total_gb: Some(32.0),
            processes: vec![],
        };
        d.record(s.clone());
        d.record(s);
        assert_eq!(d.history[0].len(), 2);
        assert_eq!(d.history[NUM_PINS - 1].len(), 2);
        // power_w is None, so the estimated pin sum (6 x 12.0 x 5.0) is used
        assert_eq!(d.power_history.last().map(|(_, v)| *v), Some(360.0));
        assert_eq!(d.temp_history.len(), 2);
        assert_eq!(d.mem_history.len(), 2);
        assert_eq!(d.power_range_max, Some(600.0));
        assert_eq!(d.temp_range_max, Some(108.0));
        assert_eq!(d.mem_range_max, Some(32.0));
        assert_eq!(d.sample.limit_w, Some(575));
    }

    #[test]
    fn record_without_pins_leaves_a_gap_not_zeros() {
        let mut d = WebData::new();
        let good = Sample {
            pins: vec![
                PinReading {
                    voltage: 12.0,
                    current: 5.0,
                };
                NUM_PINS
            ],
            ..Default::default()
        };
        d.record(good);
        let first_at = d.pins_at.expect("valid readings are timestamped");
        // A sensor fault: no pin readings, no NVML power either.
        d.sensor_error = Some("I2C read failed: boom".to_owned());
        d.record(Sample::default());
        // No zero currents and no 0 W estimate enter the histories, and the
        // last-good timestamp stays where it was (so the UIs can go stale).
        assert_eq!(d.history[0].len(), 1);
        assert_eq!(d.power_history.len(), 1);
        assert_eq!(d.pins_at, Some(first_at));
        let json = web_json(&d, 60.0);
        assert!(json.contains("\"sensor_error\":\"I2C read failed: boom\""));
        assert!(json.contains("\"pins\":[]"));
        assert!(json.contains("\"pins_age\":"));
        assert!(!json.contains("\"pins_age\":null"));
    }

    #[test]
    fn m4_keeps_extremes_and_is_stable_while_sliding() {
        // 10 samples per second for 100 s; one single-sample spike at 42.3 s.
        let pts: Vec<(f64, f64)> = (0..1000)
            .map(|i| {
                let t = i as f64 * 0.1;
                (
                    t,
                    if i == 423 {
                        50.0
                    } else {
                        5.0 + (i % 7) as f64 * 0.1
                    },
                )
            })
            .collect();
        let out = m4_downsample(&pts, 1.0);
        // At most 4 points per 1 s bucket, time order, newest sample last.
        assert!(out.len() <= 4 * 100);
        assert!(out.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(out.last(), pts.last());
        // The spike survives (stride decimation could skip it).
        assert!(out.contains(&pts[423]) && pts[423].1 == 50.0);
        // Every bucket keeps its true min and max.
        assert_eq!(out.iter().map(|p| p.1).fold(f64::MAX, f64::min), 5.0);

        // Sliding the window (dropping old samples, appending new ones)
        // leaves every complete bucket's points unchanged.
        let slid: Vec<(f64, f64)> = pts[37..]
            .iter()
            .copied()
            .chain((1000..1037).map(|i| (i as f64 * 0.1, 5.0)))
            .collect();
        let out2 = m4_downsample(&slid, 1.0);
        let middle = |o: &[(f64, f64)]| -> Vec<(f64, f64)> {
            o.iter()
                .copied()
                .filter(|p| p.0 >= 4.0 && p.0 < 99.0)
                .collect()
        };
        assert_eq!(middle(&out), middle(&out2));

        // Short series and a zero bucket width pass through untouched.
        assert_eq!(m4_downsample(&pts[..4], 1.0), pts[..4].to_vec());
        assert_eq!(m4_downsample(&pts, 0.0), pts);
    }

    #[test]
    fn stale_threshold_follows_the_interval() {
        assert_eq!(stale_after_secs(100), 3.0);
        assert_eq!(stale_after_secs(1000), 3.0);
        assert_eq!(stale_after_secs(5000), 15.0);
    }

    #[test]
    fn query_param_parses() {
        assert_eq!(query_param("window=300&x=1", "window"), Some("300"));
        assert_eq!(query_param("x=1", "window"), None);
        assert_eq!(query_param("", "window"), None);
    }

    #[test]
    fn listen_address_settings_parse_and_link() {
        let v4 = web_target(" 10.0.0.12 ", "8666").unwrap();
        assert_eq!(v4.to_string(), "10.0.0.12:8666");
        assert_eq!(local_url(v4), "http://10.0.0.12:8666/");
        assert_eq!(
            local_url(web_target("0.0.0.0", "8666").unwrap()),
            "http://localhost:8666/"
        );
        assert_eq!(
            local_url(web_target("::1", "80").unwrap()),
            "http://[::1]:80/"
        );
        assert!(web_target("", "8666").is_err());
        assert!(web_target("my-pc", "8666").is_err());
        assert!(web_target("0.0.0.0", "0").is_err());
        // Loopback only: reachable here, not bound to other interfaces.
        let port = free_port();
        let local = web_target("127.0.0.1", &port.to_string()).unwrap();
        let server = WebServer::start(local, test_data()).unwrap();
        assert!(http_get(port, "/").starts_with("HTTP/1.1 200 OK"));
        drop(server);
    }

    #[test]
    fn web_server_serves_and_rejects_busy_port() {
        let port = free_port();
        let data = test_data();

        // A port already taken must be reported as an error, not started.
        let blocker = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let all = web_target(DEFAULT_WEB_BIND, &port.to_string()).unwrap();
        assert!(WebServer::start(all, data.clone()).is_err());
        drop(blocker);

        let server = WebServer::start(all, data.clone()).unwrap();
        assert_eq!(server.addr, all);
        let page = http_get(port, "/");
        assert!(page.starts_with("HTTP/1.1 200 OK"));
        assert!(page.contains("AstralGauge"));
        assert!(page.contains("critical current detected"));

        let json_resp = http_get(port, "/data?window=60");
        assert!(json_resp.starts_with("HTTP/1.1 200 OK"));
        let body = json_resp.split("\r\n\r\n").nth(1).unwrap();
        assert!(body.contains("\"power_w\":300.5"));
        assert!(body.contains("\"critical_alert\":true"));

        // Acknowledging the critical alert clears the latching notification.
        let ack = http_request("POST", port, "/ack");
        assert!(ack.starts_with("HTTP/1.1 200 OK"));
        assert!(!data.lock().unwrap().critical_alert);
        let after_ack = http_get(port, "/data?window=60");
        assert!(after_ack.contains("\"critical_alert\":false"));

        let not_found = http_get(port, "/nope");
        assert!(not_found.starts_with("HTTP/1.1 404"));

        // Stopping the server must join the thread and free the port.
        drop(server);
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    }
}
