//! Web interface: the shared sample/history store and the HTTP server that
//! mirrors the desktop UI for other devices.
//!
//! [`WebData`] is the app's central state (latest sample, histories, mirrored
//! UI settings) behind a single `Arc<Mutex<_>>`; the poller thread records
//! into it while the GUI renders and the web server serves from it.
//! [`WebServer`] runs the tiny hand-rolled HTTP server (page + JSON API).

use std::io;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::alerts::Settings;
use crate::hardware::{AuthState, Sample, DEFAULT_POLL_MS, NUM_PINS};

/// Default port for the web interface (editable in the UI).
pub(crate) const DEFAULT_WEB_PORT: u16 = 8666;

/// Max points per series sent to a web client; the canvas can't show more
/// anyway, and it keeps the payload small at the 60-minute horizon.
const WEB_MAX_PTS: usize = 600;

/// Keep a little more history than the largest horizon
pub(crate) const HISTORY_SECS: f64 = 60.0 * 60.0 + 120.0;

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
        for i in 0..NUM_PINS {
            self.history[i].push((now, sample.pins[i].current));
        }
        // Power: prefer NVML, fall back to the estimated pin sum so the
        // graph still has data when NVML is unavailable.
        let power_val = sample.power_w.or_else(|| {
            Some(
                sample
                    .pins
                    .iter()
                    .map(|p| p.voltage * p.current)
                    .sum::<f64>(),
            )
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
    pub(crate) port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl WebServer {
    /// Bind the port synchronously so a busy port is reported to the UI
    /// before the checkbox is accepted, then spawn the serve thread.
    pub(crate) fn start(port: u16, data: Arc<Mutex<WebData>>) -> io::Result<WebServer> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = thread::spawn(move || web_serve(listener, data, thread_stop));
        println!("Web interface listening on http://{local}/");
        Ok(WebServer {
            port,
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
        println!("Web interface stopped (port {})", self.port);
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
    out.push_str("]");
    out.push_str(",\"show_graphs\":");
    out.push_str(if data.show_graphs { "true" } else { "false" });
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
        push_series(&mut out, hist, t_min);
    }
    out.push_str("],\"power\":");
    push_series(&mut out, &data.power_history, t_min);
    out.push_str(",\"temp\":");
    push_series(&mut out, &data.temp_history, t_min);
    out.push_str(",\"mem\":");
    push_series(&mut out, &data.mem_history, t_min);
    out.push_str("}}");
    out
}

/// Append one history as a JSON array of [t, v] pairs, restricted to the
/// visible window and downsampled to at most WEB_MAX_PTS points (always
/// keeping the newest sample).
fn push_series(out: &mut String, hist: &[(f64, f64)], t_min: f64) {
    let k = hist.partition_point(|(t, _)| *t < t_min);
    let vis = &hist[k..];
    if vis.is_empty() {
        out.push_str("[]");
        return;
    }
    let n = vis.len();
    let stride = n.div_ceil(WEB_MAX_PTS).max(1);
    let mut idx: Vec<usize> = (0..n).step_by(stride).collect();
    if idx.last() != Some(&(n - 1)) {
        idx.push(n - 1);
    }
    out.push('[');
    for (i, &j) in idx.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("[{:.1},{:.2}]", vis[j].0, vis[j].1));
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
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { margin: 0; background: #1e1e1e; color: #d4d4d4; font-family: system-ui, -apple-system, sans-serif; }
  .wrap { max-width: 1100px; margin: 0 auto; padding: 10px 16px 48px; }
  h1 { font-size: 22px; margin: 10px 0 6px; }
  .stats { display: flex; flex-wrap: wrap; gap: 20px; font-size: 17px; font-weight: 600; }
  #power { font-size: 21px; }
  .line { margin: 8px 0; font-size: 14px; }
  .dim { color: #999; }
  .tables { display: flex; gap: 48px; margin: 10px 0 4px; }
  .tables h2 { font-size: 14px; font-weight: 700; margin: 0 0 4px; }
  table { border-collapse: collapse; }
  td { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 13px; padding: 1px 16px 1px 0; white-space: pre; }
  .g { color: #50dc50; }
  .y { color: #f0d228; }
  .r { color: #f03c3c; }
  .legend { font-size: 12px; margin: 2px 0 10px; }
  .legend span { margin-right: 14px; }
  .graph-title { font-weight: 700; margin: 14px 0 6px; }
  .windows { margin: 0 0 8px; }
  .windows button { background: #2b2b2b; color: #8a8a8a; border: 1px solid #3d3d3d; border-radius: 3px; padding: 3px 12px; margin-right: 4px; font-size: 13px; cursor: pointer; }
  .windows button:hover { background: #343434; }
  .windows button.on { color: #78beff; font-weight: 700; border-color: #78beff; }
  .grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: 8px; }
  .cell { min-width: 0; }
  .name { font-weight: 700; font-size: 13px; margin-bottom: 2px; }
  canvas { display: block; width: 100%; height: 150px; background: #232323; border: 1px solid #3a3a3a; }
  #err { display: none; color: #f03c3c; margin-top: 12px; }
  @media (max-width: 800px) { .grid { grid-template-columns: repeat(2, 1fr); } .tables { gap: 24px; } }
</style>
</head>
<body>
<div class="wrap">
  <h1>ASUS 5090 ROG Astral</h1>
  <div class="stats">
    <span id="power">Total power: …</span>
    <span id="temp"></span>
    <span id="vram"></span>
  </div>
  <div id="limit" class="line"></div>
  <div id="interval" class="line dim"></div>
  <div class="tables">
    <div>
      <h2>Voltage (V)</h2>
      <table id="volt"></table>
    </div>
    <div>
      <h2>Current (A)</h2>
      <table id="curr"></table>
    </div>
  </div>
  <div class="legend" id="legend">
    <span class="r">● &gt; 10A</span><span class="y">● 9.2–10A</span><span class="g">● &lt; 9.2A</span>
  </div>
  <div class="tables" id="procs">
    <div>
      <h2>Top GPU processes</h2>
      <table id="proctab"></table>
    </div>
  </div>
  <div id="graphs">
    <div class="graph-title">Current and GPU metrics over time</div>
    <div class="windows" id="windows"></div>
    <div class="grid" id="grid"></div>
  </div>
  <div id="err"></div>
</div>
<script>
'use strict';

const HORIZONS = [1, 5, 10, 15, 30, 60];
let windowMin = 1;
// [yellow, red] current limits (A); updated from the settings dialog.
let limits = [9.2, 10.0];

const winsEl = document.getElementById('windows');
HORIZONS.forEach(function (m) {
  const b = document.createElement('button');
  b.textContent = m + 'm';
  b.addEventListener('click', function () { windowMin = m; syncWindowButtons(); });
  winsEl.appendChild(b);
});
function syncWindowButtons() {
  Array.from(winsEl.children).forEach(function (b, i) {
    b.classList.toggle('on', HORIZONS[i] === windowMin);
  });
}
syncWindowButtons();

const NAMES = ['Pin 0', 'Pin 1', 'Pin 2', 'Pin 3', 'Pin 4', 'Pin 5', 'Power (W)', 'Temp (°C)', 'VRAM (GB)'];
const METRIC_COLORS = { 6: '#ffa500', 7: '#ff5a46', 8: '#8cbefe' };
const grid = document.getElementById('grid');
const canvases = [];
// Last mouse position per canvas (CSS px, null when the pointer is away).
const hoverPos = new Array(NAMES.length).fill(null);
// Latest drawGraph arguments, so mouse moves can redraw between polls.
const drawArgs = new Array(NAMES.length).fill(null);
NAMES.forEach(function (n, i) {
  const cell = document.createElement('div');
  cell.className = 'cell';
  const label = document.createElement('div');
  label.className = 'name';
  label.textContent = n;
  const cv = document.createElement('canvas');
  cell.appendChild(label);
  cell.appendChild(cv);
  grid.appendChild(cell);
  canvases.push(cv);
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

function currentColor(v) {
  if (v < limits[0]) return '#50dc50';
  if (v < limits[1]) return '#f0d228';
  return '#f03c3c';
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

  let yMax;
  if (isPin) {
    // keep the limit lines visible, grow the scale for spikes
    yMax = limits[1] * 1.1;
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

  ctx.font = '9px monospace';
  ctx.fillStyle = '#8a8a8a';
  if (!isPin || yMax > limits[1] * 1.1) ctx.fillText(yMax.toFixed(0), 2, 10);
  ctx.fillText('0', 2, h - 3);

  if (isPin) {
    const lims = [[limits[0], '#a5964b'], [limits[1], '#af5555']];
    for (let i = 0; i < lims.length; i++) {
      const lim = lims[i][0], col = lims[i][1];
      const y = yOf(lim);
      ctx.strokeStyle = col;
      ctx.setLineDash([4, 3]);
      ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(w, y); ctx.stroke();
      ctx.setLineDash([]);
      ctx.fillStyle = col;
      ctx.fillText(String(lim), w - 20, Math.max(y - 3, 9));
    }
  }

  if (n >= 2) {
    const maxPts = Math.max(2, Math.floor(w) * 2);
    const stride = Math.max(1, Math.ceil(n / maxPts));
    ctx.lineWidth = isPin ? 1 : 1.5;
    if (!isPin) ctx.strokeStyle = METRIC_COLORS[idx];
    let i = 0;
    while (i + stride < n) {
      const j = i + stride;
      ctx.beginPath();
      ctx.moveTo(xOf(vis[i][0]), yOf(vis[i][1]));
      ctx.lineTo(xOf(vis[j][0]), yOf(vis[j][1]));
      if (isPin) ctx.strokeStyle = currentColor(vis[j][1]);
      ctx.stroke();
      i = j;
    }
    if (i < n - 1) {
      const j = n - 1;
      ctx.beginPath();
      ctx.moveTo(xOf(vis[i][0]), yOf(vis[i][1]));
      ctx.lineTo(xOf(vis[j][0]), yOf(vis[j][1]));
      if (isPin) ctx.strokeStyle = currentColor(vis[j][1]);
      ctx.stroke();
    }
  }

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

  ctx.strokeStyle = '#565656';
  ctx.lineWidth = 1;
  ctx.beginPath(); ctx.moveTo(x, 0); ctx.lineTo(x, h); ctx.stroke();

  ctx.fillStyle = idx < 6 ? currentColor(p[1]) : METRIC_COLORS[idx];
  ctx.beginPath(); ctx.arc(x, y, 3, 0, Math.PI * 2); ctx.fill();
  ctx.strokeStyle = '#232323';
  ctx.stroke();

  const unit = idx < 6 ? 'A' : ['W', '°C', 'GB'][idx - 6];
  const label = p[1].toFixed(2) + ' ' + unit;
  ctx.font = '11px monospace';
  const lw = ctx.measureText(label).width;
  let lx = x + 6;
  if (lx + lw > w - 2) lx = Math.max(x - lw - 6, 1);
  ctx.fillStyle = '#232323';
  ctx.fillRect(lx - 1, 1, lw + 2, 14);
  ctx.fillStyle = '#f0f0f0';
  ctx.fillText(label, lx, 12);
}

function pinLabel(pin, value) {
  return 'Pin ' + String(pin).padStart(2) + ': ' + value.toFixed(3).padStart(7);
}

function render(d) {
  const s = d.sample;

  if (s.power_w != null) {
    document.getElementById('power').textContent = 'Total power: ' + s.power_w.toFixed(1) + ' W';
  } else {
    let sum = 0;
    for (const p of s.pins) sum += p[0] * p[1];
    document.getElementById('power').textContent = 'Total power: ' + sum.toFixed(1) + ' W (estimated)';
  }
  document.getElementById('temp').textContent = s.temp_c != null ? 'GPU temp: ' + s.temp_c.toFixed(0) + ' °C' : '';
  const vramEl = document.getElementById('vram');
  if (s.mem_used_gb != null) {
    vramEl.textContent = s.mem_total_gb != null
      ? 'VRAM: ' + s.mem_used_gb.toFixed(1) + ' / ' + s.mem_total_gb.toFixed(0) + ' GB'
      : 'VRAM: ' + s.mem_used_gb.toFixed(1) + ' GB';
  } else {
    vramEl.textContent = '';
  }

  const limitEl = document.getElementById('limit');
  if (s.limit_w != null) {
    let t = 'Power limit: ' + s.limit_w + ' W';
    if (s.limit_min_w != null && s.limit_max_w != null) {
      t += '    (range ' + s.limit_min_w + '–' + s.limit_max_w + ' W)';
    }
    limitEl.textContent = t;
  } else {
    limitEl.textContent = '';
  }
  document.getElementById('interval').textContent = 'Sample interval: ' + d.poll_ms + ' ms';

  // Top GPU-using processes (NVML): [pid, name, mem GB, util %], highest first.
  const pt = document.getElementById('proctab');
  pt.innerHTML = '';
  const procs = d.processes || [];
  if (procs.length === 0) {
    const cell = pt.insertRow().insertCell();
    cell.textContent = 'No GPU-using processes';
    cell.className = 'dim';
  } else {
    for (const p of procs) {
      const row = pt.insertRow();
      row.insertCell().textContent = String(p[1]).padEnd(24).slice(0, 24);
      row.insertCell().textContent = String(p[3]).padStart(4) + ' %';
      row.insertCell().textContent = p[2].toFixed(2).padStart(6) + ' GB';
    }
  }

  // current limits (from the settings dialog) and matching legend
  if (d.limits) {
    limits = d.limits;
    document.getElementById('legend').innerHTML =
      '<span class="r">● ≥ ' + limits[1] + 'A</span>' +
      '<span class="y">● ' + limits[0] + '–' + limits[1] + 'A</span>' +
      '<span class="g">● < ' + limits[0] + 'A</span>';
  }

  // tables (skip until the first sample with all pins has arrived)
  if (s.pins.length === 6) {
    const volt = document.getElementById('volt');
    const curr = document.getElementById('curr');
    volt.innerHTML = '';
    curr.innerHTML = '';
    for (let row = 0; row < 2; row++) {
      const vr = volt.insertRow();
      const cr = curr.insertRow();
      for (let c = 0; c < 3; c++) {
        const pin = row * 3 + c;
        vr.insertCell().textContent = pinLabel(pin, s.pins[pin][0]);
        const cell = cr.insertCell();
        const cur = s.pins[pin][1];
        cell.textContent = pinLabel(pin, cur);
        cell.className = cur < limits[0] ? 'g' : (cur < limits[1] ? 'y' : 'r');
      }
    }
  }

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
  const el = document.getElementById('err');
  el.style.display = msg ? 'block' : 'none';
  if (msg) el.textContent = msg;
}
async function tick() {
  try {
    const r = await fetch('/data?window=' + windowMin * 60, { cache: 'no-store' });
    if (!r.ok) throw new Error('HTTP ' + r.status);
    render(await r.json());
    fails = 0;
    showErr('');
  } catch (e) {
    if (++fails >= 5) showErr('Cannot reach AstralGauge. Is the web interface enabled?');
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
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let req = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
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
    fn query_param_parses() {
        assert_eq!(query_param("window=300&x=1", "window"), Some("300"));
        assert_eq!(query_param("x=1", "window"), None);
        assert_eq!(query_param("", "window"), None);
    }

    #[test]
    fn web_server_serves_and_rejects_busy_port() {
        let port = free_port();
        let data = test_data();

        // A port already taken must be reported as an error, not started.
        let blocker = TcpListener::bind(("127.0.0.1", port)).unwrap();
        assert!(WebServer::start(port, data.clone()).is_err());
        drop(blocker);

        let server = WebServer::start(port, data.clone()).unwrap();
        let page = http_get(port, "/");
        assert!(page.starts_with("HTTP/1.1 200 OK"));
        assert!(page.contains("AstralGauge"));

        let json_resp = http_get(port, "/data?window=60");
        assert!(json_resp.starts_with("HTTP/1.1 200 OK"));
        let body = json_resp.split("\r\n\r\n").nth(1).unwrap();
        assert!(body.contains("\"power_w\":300.5"));

        let not_found = http_get(port, "/nope");
        assert!(not_found.starts_with("HTTP/1.1 404"));

        // Stopping the server must join the thread and free the port.
        drop(server);
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    }
}
