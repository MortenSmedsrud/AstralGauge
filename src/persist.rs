//! Settings persistence: the applied [`Config`](crate::ui::Config) is saved
//! as a small JSON document so it survives between runs, at
//! `$XDG_CONFIG_HOME/astral-gauge/settings.json`, falling back to
//! `~/.config/astral-gauge/settings.json`.
//!
//! Like the web JSON in [`crate::web`], the document is written and parsed
//! by hand: plain objects holding strings, numbers, booleans and null
//! (arrays are parsed and skipped, never written). [`load`] tolerates
//! unknown keys (forward compatibility) and missing ones (defaults), clamps
//! numbers into their allowed ranges, and never touches a corrupt file — it
//! reports it instead, and only accepting new settings overwrites it.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use crate::alerts::{AutoMinPower, EmbeddedSound, Settings, Sound, MAX_LIMIT_A, MIN_LIMIT_A};

/// App directory under the config home, and the file name inside it.
const APP_DIR: &str = "astral-gauge";
const FILE_NAME: &str = "settings.json";

/// The persisted part of [`crate::ui::Config`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StoredSettings {
    pub(crate) alerts: Settings,
    pub(crate) poll_ms: u64,
    pub(crate) web_enabled: bool,
    pub(crate) web_port: String,
    /// The web server's listen address (an IP address, as text).
    pub(crate) web_bind: String,
}

/// Where the settings live: `$XDG_CONFIG_HOME/astral-gauge/settings.json`,
/// or `~/.config/astral-gauge/settings.json` when `XDG_CONFIG_HOME` is unset
/// (or not absolute). `None` when neither variable helps, disabling
/// persistence for the session.
pub(crate) fn settings_path() -> Option<PathBuf> {
    let base =
        env_dir("XDG_CONFIG_HOME").or_else(|| env_dir("HOME").map(|home| home.join(".config")))?;
    Some(base.join(APP_DIR).join(FILE_NAME))
}

/// A non-empty absolute path from an environment variable.
fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty() && p.is_absolute())
}

/// Read the settings file. `Ok(Some(_))` is a usable document (missing
/// fields taken from `defaults`), `Ok(None)` no file yet (first run), and
/// `Err(msg)` a corrupt file — left untouched, never silently reset.
pub(crate) fn load(defaults: &StoredSettings) -> Result<Option<StoredSettings>, String> {
    match settings_path() {
        Some(path) => load_from(&path, defaults),
        None => Ok(None),
    }
}

/// [`load`] from an explicit file (the system service's
/// `/etc/astral-gauge/settings.json`).
pub(crate) fn load_from(
    path: &Path,
    defaults: &StoredSettings,
) -> Result<Option<StoredSettings>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    parse(&text, defaults)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Parse a settings document (see [`load`] for the tolerance rules). Also
/// used for settings that arrive over the service link.
pub(crate) fn parse(text: &str, defaults: &StoredSettings) -> Result<StoredSettings, String> {
    from_json(&parse_json(text)?, defaults)
}

/// Write the settings atomically (a sibling `.tmp` file plus rename, so a
/// crash can never leave a half-written file), creating the directory.
pub(crate) fn save(stored: &StoredSettings) -> Result<(), String> {
    let path = settings_path().ok_or("no config directory: set $XDG_CONFIG_HOME or $HOME")?;
    save_to(&path, stored)
}

/// [`save`] to an explicit file.
pub(crate) fn save_to(path: &Path, stored: &StoredSettings) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let json = to_json(stored);
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = fs::write(&tmp, &json) {
        return Err(format!("cannot write {}: {e}", tmp.display()));
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("cannot replace {}: {e}", path.display()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// The whole document, pretty-printed in a fixed key order.
pub(crate) fn to_json(stored: &StoredSettings) -> String {
    let s = &stored.alerts;
    let mut out = String::from("{\n");
    let _ = writeln!(out, "  \"yellow_limit_a\": {},", s.yellow_limit_a);
    let _ = writeln!(out, "  \"red_limit_a\": {},", s.red_limit_a);
    let _ = writeln!(
        out,
        "  \"yellow_sound_enabled\": {},",
        s.yellow_sound_enabled
    );
    out.push_str("  \"yellow_sound\": ");
    write_sound(&mut out, &s.yellow_sound);
    out.push_str(",\n");
    let _ = writeln!(out, "  \"red_sound_enabled\": {},", s.red_sound_enabled);
    out.push_str("  \"red_sound\": ");
    write_sound(&mut out, &s.red_sound);
    out.push_str(",\n");
    let _ = writeln!(
        out,
        "  \"auto_min_power\": \"{}\",",
        auto_key(s.auto_min_power)
    );
    let _ = writeln!(
        out,
        "  \"kill_top_process_on_red\": {},",
        s.kill_top_process_on_red
    );
    let _ = writeln!(
        out,
        "  \"kill_other_users_processes\": {},",
        s.kill_other_users_processes
    );
    out.push_str("  \"terminating_sound\": ");
    write_sound(&mut out, &s.terminating_sound);
    out.push_str(",\n");
    let _ = writeln!(out, "  \"poll_ms\": {},", stored.poll_ms);
    let _ = writeln!(out, "  \"web_enabled\": {},", stored.web_enabled);
    out.push_str("  \"web_port\": ");
    write_string(&mut out, &stored.web_port);
    out.push_str(",\n  \"web_bind\": ");
    write_string(&mut out, &stored.web_bind);
    out.push('\n');
    out.push('}');
    out.push('\n');
    out
}

/// A sound is either `"embedded:<name>"` (a sound inside the executable) or
/// `{ "file": "<path>" }` (a user-selected `.wav`).
fn write_sound(out: &mut String, sound: &Sound) {
    match sound {
        Sound::Embedded(embedded) => {
            let _ = write!(out, "\"embedded:");
            out.push_str(embedded_key(*embedded));
            out.push('"');
        }
        Sound::File(path) => {
            out.push_str("{ \"file\": ");
            write_string(out, &path.display().to_string());
            out.push_str(" }");
        }
    }
}

pub(crate) fn embedded_key(sound: EmbeddedSound) -> &'static str {
    match sound {
        EmbeddedSound::Attention => "attention",
        EmbeddedSound::Critical => "critical",
        EmbeddedSound::AttentionPoweringDown => "attention-powering-down",
        EmbeddedSound::CriticalPoweringDown => "critical-powering-down",
        EmbeddedSound::CriticalTerminating => "critical-terminating",
    }
}

pub(crate) fn embedded_from_key(key: &str) -> Option<EmbeddedSound> {
    match key {
        "attention" => Some(EmbeddedSound::Attention),
        "critical" => Some(EmbeddedSound::Critical),
        "attention-powering-down" => Some(EmbeddedSound::AttentionPoweringDown),
        "critical-powering-down" => Some(EmbeddedSound::CriticalPoweringDown),
        "critical-terminating" => Some(EmbeddedSound::CriticalTerminating),
        _ => None,
    }
}

fn auto_key(mode: AutoMinPower) -> &'static str {
    match mode {
        AutoMinPower::Off => "off",
        AutoMinPower::OnYellow => "on_yellow",
        AutoMinPower::OnRed => "on_red",
    }
}

/// One JSON string literal, escaping what JSON requires (other UTF-8
/// characters are written as-is).
fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// Field values are taken when present (and valid), defaults otherwise;
/// numbers are clamped into the ranges the settings panel offers, and the
/// yellow/red order is repaired instead of being rejected.
fn from_json(root: &Json, defaults: &StoredSettings) -> Result<StoredSettings, String> {
    if !matches!(root, Json::Obj(_)) {
        return Err("settings must be a JSON object".to_owned());
    }
    let d = &defaults.alerts;
    let mut alerts = d.clone();

    let yellow = number(root, "yellow_limit_a")?.unwrap_or(d.yellow_limit_a);
    let red = number(root, "red_limit_a")?.unwrap_or(d.red_limit_a);
    // The sliders keep yellow <= red; repair hand-edited violations.
    let (yellow, red) = (yellow.min(red), yellow.max(red));
    alerts.yellow_limit_a = yellow.clamp(MIN_LIMIT_A, MAX_LIMIT_A);
    alerts.red_limit_a = red.clamp(MIN_LIMIT_A, MAX_LIMIT_A);

    alerts.yellow_sound_enabled =
        boolean(root, "yellow_sound_enabled")?.unwrap_or(d.yellow_sound_enabled);
    if let Some(v) = root.get("yellow_sound") {
        alerts.yellow_sound = parse_sound(v)?;
    }
    alerts.red_sound_enabled = boolean(root, "red_sound_enabled")?.unwrap_or(d.red_sound_enabled);
    if let Some(v) = root.get("red_sound") {
        alerts.red_sound = parse_sound(v)?;
    }
    match root.get("auto_min_power") {
        None => {}
        Some(Json::Str(text)) => {
            alerts.auto_min_power = match text.as_str() {
                "off" => AutoMinPower::Off,
                "on_yellow" => AutoMinPower::OnYellow,
                "on_red" => AutoMinPower::OnRed,
                other => return Err(format!("unknown auto_min_power \"{other}\"")),
            };
        }
        Some(_) => return Err("auto_min_power must be a string".to_owned()),
    }
    alerts.kill_top_process_on_red =
        boolean(root, "kill_top_process_on_red")?.unwrap_or(d.kill_top_process_on_red);
    alerts.kill_other_users_processes =
        boolean(root, "kill_other_users_processes")?.unwrap_or(d.kill_other_users_processes);
    if let Some(v) = root.get("terminating_sound") {
        alerts.terminating_sound = parse_sound(v)?;
    }

    let poll_ms = match root.get("poll_ms") {
        None => defaults.poll_ms,
        Some(Json::Num(v)) => v.round().clamp(100.0, 10_000.0) as u64,
        Some(_) => return Err("poll_ms must be a number".to_owned()),
    };
    let web_enabled = boolean(root, "web_enabled")?.unwrap_or(defaults.web_enabled);
    let web_port = match root.get("web_port") {
        None => defaults.web_port.clone(),
        Some(Json::Str(text)) => {
            let text = text.trim();
            match text.parse::<u16>() {
                Ok(port) if port != 0 => text.to_owned(),
                _ => {
                    return Err(format!(
                        "web_port must be a number from 1 to 65535, got \"{text}\""
                    ))
                }
            }
        }
        Some(_) => return Err("web_port must be a string".to_owned()),
    };
    let web_bind = match root.get("web_bind") {
        // Older files: the address the server always listened on.
        None => defaults.web_bind.clone(),
        Some(Json::Str(text)) => {
            let text = text.trim();
            match text.parse::<std::net::IpAddr>() {
                Ok(_) => text.to_owned(),
                Err(_) => return Err(format!("web_bind must be an IP address, got \"{text}\"")),
            }
        }
        Some(_) => return Err("web_bind must be a string".to_owned()),
    };

    Ok(StoredSettings {
        alerts,
        poll_ms,
        web_enabled,
        web_port,
        web_bind,
    })
}

fn number(root: &Json, key: &str) -> Result<Option<f32>, String> {
    match root.get(key) {
        None => Ok(None),
        Some(Json::Num(v)) if v.is_finite() => Ok(Some(*v as f32)),
        Some(_) => Err(format!("{key} must be a number")),
    }
}

fn boolean(root: &Json, key: &str) -> Result<Option<bool>, String> {
    match root.get(key) {
        None => Ok(None),
        Some(Json::Bool(v)) => Ok(Some(*v)),
        Some(_) => Err(format!("{key} must be true or false")),
    }
}

fn parse_sound(v: &Json) -> Result<Sound, String> {
    match v {
        Json::Str(text) => {
            let key = text.strip_prefix("embedded:").ok_or_else(|| {
                format!("sound must be \"embedded:...\" or {{ \"file\": ... }}, got \"{text}\"")
            })?;
            embedded_from_key(key)
                .map(Sound::Embedded)
                .ok_or_else(|| format!("unknown embedded sound \"{key}\""))
        }
        Json::Obj(_) => match v.get("file") {
            Some(Json::Str(path)) if !path.is_empty() => Ok(Sound::File(PathBuf::from(path))),
            _ => Err("sound file entry needs a \"file\" string".to_owned()),
        },
        _ => Err("sound must be a string or a { \"file\": ... } object".to_owned()),
    }
}

// ---------------------------------------------------------------------------
// A minimal JSON parser (the same subset the writer above produces)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// Parse a whole JSON document (trailing content is an error).
fn parse_json(text: &str) -> Result<Json, String> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    parser.skip_ws();
    let value = parser.parse_value()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(format!("trailing content (byte {})", parser.pos));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' (byte {})", c as char, self.pos))
        }
    }

    fn parse_value(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(Json::Str(self.parse_string()?)),
            Some(b't') => self.parse_literal("true", Json::Bool(true)),
            Some(b'f') => self.parse_literal("false", Json::Bool(false)),
            Some(b'n') => self.parse_literal("null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            Some(c) => Err(format!(
                "unexpected byte '{}' (byte {})",
                c as char, self.pos
            )),
            None => Err("unexpected end of input".to_owned()),
        }
    }

    fn parse_literal(&mut self, word: &str, value: Json) -> Result<Json, String> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(format!("invalid literal (byte {})", self.pos))
        }
    }

    fn parse_object(&mut self) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut entries = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(entries));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(entries));
                }
                _ => return Err(format!("expected ',' or '}}' (byte {})", self.pos)),
            }
        }
    }

    fn parse_array(&mut self) -> Result<Json, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(format!("expected ',' or ']' (byte {})", self.pos)),
            }
        }
    }

    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "invalid number bytes".to_owned())?;
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("invalid number \"{text}\""))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let Some(&byte) = self.bytes.get(self.pos) else {
                return Err(format!("unterminated string (byte {})", self.pos));
            };
            self.pos += 1;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(&escape) = self.bytes.get(self.pos) else {
                        return Err(format!("unterminated escape (byte {})", self.pos));
                    };
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.parse_unicode_escape()?),
                        other => {
                            return Err(format!(
                                "invalid escape '\\{}' (byte {})",
                                other as char,
                                self.pos - 1
                            ))
                        }
                    }
                }
                0x00..=0x1F => {
                    return Err(format!("control byte in string (byte {})", self.pos - 1))
                }
                0x80..=0xFF => {
                    // Multi-byte UTF-8 sequence: passed through unchanged.
                    let start = self.pos - 1;
                    let end = start + utf8_seq_len(byte);
                    if end > self.bytes.len() {
                        return Err(format!("truncated UTF-8 sequence (byte {start})"));
                    }
                    let text = std::str::from_utf8(&self.bytes[start..end])
                        .map_err(|_| format!("invalid UTF-8 (byte {start})"))?;
                    out.push_str(text);
                    self.pos = end;
                }
                plain => out.push(plain as char),
            }
        }
    }

    /// A `\uXXXX` escape (with surrogate pairs), as produced by
    /// [`write_string`]'s control-character escapes.
    fn parse_unicode_escape(&mut self) -> Result<char, String> {
        let first = self.parse_hex4()?;
        if (0xD800..0xDC00).contains(&first) {
            if self.bytes[self.pos..].starts_with(b"\\u") {
                self.pos += 2;
                let second = self.parse_hex4()?;
                if (0xDC00..0xE000).contains(&second) {
                    let cp = 0x1_0000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                    return char::from_u32(cp).ok_or("invalid surrogate pair".to_owned());
                }
            }
            return Err("unpaired surrogate escape".to_owned());
        }
        char::from_u32(first).ok_or_else(|| "invalid \\u escape".to_owned())
    }

    fn parse_hex4(&mut self) -> Result<u32, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err(format!("truncated \\u escape (byte {})", self.pos));
        }
        let text = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| "invalid \\u escape".to_owned())?;
        let value = u32::from_str_radix(text, 16)
            .map_err(|_| format!("invalid hex \"{text}\" in \\u escape"))?;
        self.pos += 4;
        Ok(value)
    }
}

/// Total length of the UTF-8 sequence starting with this byte (a stray
/// continuation byte reports 1 so the UTF-8 validation below rejects it).
fn utf8_seq_len(first: u8) -> usize {
    if first & 0xE0 == 0xC0 {
        2
    } else if first & 0xF0 == 0xE0 {
        3
    } else if first & 0xF8 == 0xF0 {
        4
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::DEFAULT_POLL_MS;
    use crate::web::{DEFAULT_WEB_BIND, DEFAULT_WEB_PORT};

    fn defaults() -> StoredSettings {
        StoredSettings {
            alerts: Settings::default(),
            poll_ms: DEFAULT_POLL_MS,
            web_enabled: false,
            web_port: DEFAULT_WEB_PORT.to_string(),
            web_bind: DEFAULT_WEB_BIND.to_owned(),
        }
    }

    fn round_trip(text: &str) -> Result<StoredSettings, String> {
        from_json(&parse_json(text)?, &defaults())
    }

    #[test]
    fn settings_survive_a_round_trip() {
        let stored = StoredSettings {
            alerts: Settings {
                yellow_limit_a: 8.5,
                red_limit_a: 11.3,
                yellow_sound_enabled: true,
                yellow_sound: Sound::Embedded(EmbeddedSound::AttentionPoweringDown),
                red_sound_enabled: false,
                red_sound: Sound::File(PathBuf::from("/tmp/sélect \"one\"\n.wav")),
                auto_min_power: AutoMinPower::OnYellow,
                kill_top_process_on_red: true,
                kill_other_users_processes: true,
                terminating_sound: Sound::Embedded(EmbeddedSound::CriticalTerminating),
            },
            poll_ms: 250,
            web_enabled: true,
            web_port: "9001".to_owned(),
            web_bind: "10.0.0.12".to_owned(),
        };
        let parsed = round_trip(&to_json(&stored)).unwrap();
        assert_eq!(parsed, stored);

        let defaults = defaults();
        assert_eq!(round_trip(&to_json(&defaults)).unwrap(), defaults);
    }

    #[test]
    fn missing_and_unknown_keys_use_defaults() {
        let loaded = round_trip(r#"{ "made_up": 1, "poll_ms": 500 }"#).unwrap();
        let d = defaults();
        assert_eq!(loaded.alerts, d.alerts);
        assert_eq!(loaded.poll_ms, 500);
        assert!(!loaded.web_enabled);
        assert_eq!(loaded.web_port, d.web_port);
        assert_eq!(loaded.web_bind, "0.0.0.0");
    }

    #[test]
    fn out_of_range_values_are_clamped_and_ordered() {
        // Beyond the sliders' bounds, and yellow > red: clamped and repaired.
        let loaded = round_trip(r#"{ "yellow_limit_a": 50.0, "red_limit_a": 0.5 }"#).unwrap();
        assert_eq!(loaded.alerts.yellow_limit_a, MIN_LIMIT_A);
        assert_eq!(loaded.alerts.red_limit_a, MAX_LIMIT_A);
        let loaded = round_trip(r#"{ "poll_ms": 1e9 }"#).unwrap();
        assert_eq!(loaded.poll_ms, 10_000);
    }

    #[test]
    fn invalid_values_are_rejected_not_defaulted() {
        for bad in [
            r#"{ "poll_ms": "fast" }"#,
            r#"{ "auto_min_power": "sometimes" }"#,
            r#"{ "web_port": "0" }"#,
            r#"{ "web_bind": "my-pc" }"#,
            r#"{ "web_bind": 127 }"#,
            r#"{ "web_enabled": "yes" }"#,
            r#"{ "yellow_sound": "/plain/path.wav" }"#,
            r#"{ "yellow_sound": "embedded:who_hummed_this" }"#,
            r#"[1, 2, 3]"#,
            r#"{ "yellow_sound": { "file": "" } }"#,
        ] {
            assert!(round_trip(bad).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn saved_file_is_read_back() {
        // Real file round-trip through save()/load() in a throwaway HOME.
        // (Other tests never read environment variables, so the temporary
        // HOME cannot confuse them.)
        let root = std::env::temp_dir().join(format!("astral-gauge-test-{}", std::process::id()));
        std::env::set_var("HOME", root.join("home"));
        std::env::remove_var("XDG_CONFIG_HOME");
        let stored = StoredSettings {
            alerts: Settings {
                yellow_limit_a: 5.0,
                ..Settings::default()
            },
            poll_ms: 777,
            web_enabled: true,
            web_port: "9999".to_owned(),
            web_bind: "::".to_owned(),
        };
        save(&stored).expect("save");
        assert!(settings_path()
            .unwrap()
            .ends_with("astral-gauge/settings.json"));
        assert_eq!(load(&defaults()).unwrap().as_ref(), Some(&stored));
        std::env::remove_var("HOME");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn broken_json_is_reported_not_parsed() {
        for bad in [
            "",
            "{",
            r#"{ "a": }"#,
            r#"{ "a": tru}"#,
            r#"{ "a": "unterminated }"#,
            r#"{ "a": 0x1 }"#,
            "{} garbage",
        ] {
            assert!(parse_json(bad).is_err(), "should reject: {bad:?}");
        }
    }
}
