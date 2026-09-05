//! Limit checking and alert sounds.
//!
//! [`Settings`] holds the yellow/red current limits and the sound
//! configuration used across the app. [`over_limit_sound`] folds one poll
//! sample's maximum pin current into the latched alert state and decides
//! which (if any) alert sound should play now. [`SoundPlayer`] owns a rodio
//! output thread that plays the queued sounds, one at a time.

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rodio::{Decoder, OutputStream, Sink};

/// Allowed range and defaults for the current limits (amperes).
pub(crate) const MIN_LIMIT_A: f32 = 1.0;
pub(crate) const MAX_LIMIT_A: f32 = 12.0;
pub(crate) const DEFAULT_YELLOW_LIMIT_A: f32 = 9.2;
pub(crate) const DEFAULT_RED_LIMIT_A: f32 = 10.0;

/// Directory with the shipped alert sounds (relative to the working dir).
const SOUND_DIR: &str = "sound";

/// Minimum time between two alert sounds while a limit stays exceeded: after
/// a sound plays, the next one waits this long (only if the limit is still
/// surpassed; dropping below the yellow limit resets the alert state).
pub(crate) const ALERT_REPEAT_DELAY: Duration = Duration::from_secs(15);

/// The alarm thresholds and sounds of the app. The green/yellow/red color
/// ranges in the tables and graphs, and the warning sounds, follow the
/// limits: below `yellow_limit_a` is green, from there up to (but not
/// including) `red_limit_a` is yellow, and at `red_limit_a` or above is red.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Settings {
    /// Current (A) at which the yellow range starts.
    pub(crate) yellow_limit_a: f32,
    /// Current (A) at which the red range starts.
    pub(crate) red_limit_a: f32,
    /// Play `yellow_sound_path` while a pin's current is in the yellow range.
    pub(crate) yellow_sound_enabled: bool,
    pub(crate) yellow_sound_path: PathBuf,
    /// Play `red_sound_path` while a pin's current is in the red range.
    pub(crate) red_sound_enabled: bool,
    pub(crate) red_sound_path: PathBuf,
}

/// Path of one of the shipped alert sounds: `sound/<prefix>.wav`, resolved
/// relative to the working directory.
fn default_sound_path(prefix: &str) -> PathBuf {
    PathBuf::from(SOUND_DIR).join(format!("{prefix}.wav"))
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            yellow_limit_a: DEFAULT_YELLOW_LIMIT_A,
            red_limit_a: DEFAULT_RED_LIMIT_A,
            yellow_sound_enabled: false,
            yellow_sound_path: default_sound_path("attention"),
            red_sound_enabled: false,
            red_sound_path: default_sound_path("critical"),
        }
    }
}

/// Alert state carried across poll samples: which limit ranges are currently
/// latched (so entering a range counts as a rising edge) and when the last
/// alert sound played (for the repeat delay).
#[derive(Default)]
pub(crate) struct AlertState {
    latched_yellow: bool,
    latched_red: bool,
    last_play: Option<Instant>,
}

/// Fold one sample's maximum pin current into the latched over-limit state
/// and return the alert sound to play now, if any.
///
/// Red takes priority over yellow. A sound fires on the rising edge of its
/// range, and repeats at most once per [`ALERT_REPEAT_DELAY`] while the limit
/// stays surpassed. Dropping below the yellow limit resets the state, so the
/// next excursion alerts immediately again.
pub(crate) fn over_limit_sound(
    state: &mut AlertState,
    max_current: f64,
    s: &Settings,
) -> Option<PathBuf> {
    let over_yellow = max_current >= s.yellow_limit_a as f64;
    let over_red = max_current >= s.red_limit_a as f64;

    if !over_yellow {
        state.latched_yellow = false;
        state.latched_red = false;
        state.last_play = None;
        return None;
    }

    let enabled = if over_red {
        s.red_sound_enabled
    } else {
        s.yellow_sound_enabled
    };
    // Rising edge into the active range, or the repeat delay expired while
    // the limit is still surpassed.
    let rising = if over_red {
        !state.latched_red
    } else {
        !state.latched_yellow
    };
    let due = state
        .last_play
        .is_some_and(|t| t.elapsed() >= ALERT_REPEAT_DELAY);

    state.latched_yellow = true;
    state.latched_red = over_red;

    if enabled && (rising || due) {
        state.last_play = Some(Instant::now());
        Some(if over_red {
            s.red_sound_path.clone()
        } else {
            s.yellow_sound_path.clone()
        })
    } else {
        None
    }
}

/// Handle to the alert-sound playback thread. Sound is best-effort: `play`
/// never blocks, and requests are dropped when no audio output exists
/// (headless session, missing device, undecodable file).
pub(crate) struct SoundPlayer {
    tx: mpsc::Sender<PathBuf>,
}

impl SoundPlayer {
    /// Spawn the playback thread. rodio's output stream is created lazily on
    /// the first request, so apps run without an audio device never pay for
    /// (or fail on) audio initialization.
    pub(crate) fn spawn() -> SoundPlayer {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || audio_loop(rx));
        SoundPlayer { tx }
    }

    /// Queue a sound file for playback. Sounds play one at a time; while one
    /// is playing, at most the most recently queued file is kept.
    pub(crate) fn play(&self, path: PathBuf) {
        let _ = self.tx.send(path);
    }
}

fn audio_loop(rx: mpsc::Receiver<PathBuf>) {
    // rodio stops playback when the output stream is dropped, so the session
    // lives for the whole thread; it is None until the first successful try.
    let mut session: Option<(OutputStream, Sink)> = None;
    let mut init_warned = false;

    while let Some(mut path) = rx.recv().ok() {
        // Play only the newest request: while a sound was playing, older
        // queued alerts are already redundant.
        while let Ok(newer) = rx.try_recv() {
            path = newer;
        }

        if session.is_none() {
            session = match OutputStream::try_default() {
                Ok((stream, handle)) => match Sink::try_new(&handle) {
                    Ok(sink) => Some((stream, sink)),
                    Err(e) => {
                        if !init_warned {
                            eprintln!("Audio sink unavailable: {e}. Alert sounds disabled.");
                            init_warned = true;
                        }
                        None
                    }
                },
                Err(e) => {
                    if !init_warned {
                        eprintln!("No audio output available: {e}. Alert sounds disabled.");
                        init_warned = true;
                    }
                    None
                }
            };
            if session.is_none() {
                continue;
            }
        }

        if let Some((_, sink)) = &session {
            let source = File::open(&path)
                .map_err(|e| format!("{}: {e}", path.display()))
                .and_then(|f| {
                    Decoder::new(BufReader::new(f)).map_err(|e| format!("{}: {e}", path.display()))
                });
            match source {
                // Blocks until this sound finished, so only one plays at a
                // time; the next queued file is picked up by the loop above.
                Ok(source) => {
                    sink.append(source);
                    sink.sleep_until_end();
                }
                Err(msg) => eprintln!("Cannot play alert sound: {msg}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_defaults_are_valid() {
        let s = Settings::default();
        // The defaults match the thresholds this app shipped with.
        assert_eq!(s.yellow_limit_a, DEFAULT_YELLOW_LIMIT_A);
        assert_eq!(s.red_limit_a, DEFAULT_RED_LIMIT_A);
        assert!(!s.yellow_sound_enabled && !s.red_sound_enabled);
        assert!(s.yellow_limit_a >= MIN_LIMIT_A);
        assert!(s.yellow_limit_a <= s.red_limit_a);
        assert!(s.red_limit_a <= MAX_LIMIT_A);
        // The shipped sounds are `sound/<prefix>.wav`.
        assert_eq!(
            default_sound_path("attention"),
            PathBuf::from("sound/attention.wav")
        );
        assert_eq!(
            default_sound_path("critical"),
            PathBuf::from("sound/critical.wav")
        );
    }

    #[test]
    fn over_limit_sound_edge_triggers() {
        let mut s = Settings::default();
        s.yellow_sound_enabled = true;
        s.red_sound_enabled = true;
        let mut st = AlertState::default();
        // Below yellow: nothing.
        assert_eq!(over_limit_sound(&mut st, 5.0, &s), None);
        // Enter yellow: the sound fires on the edge, then stays silent
        // within the repeat delay.
        assert_eq!(
            over_limit_sound(&mut st, 9.5, &s),
            Some(s.yellow_sound_path.clone())
        );
        assert_eq!(over_limit_sound(&mut st, 9.8, &s), None);
        // Enter red: the red sound fires right away (escalation).
        assert_eq!(
            over_limit_sound(&mut st, 10.5, &s),
            Some(s.red_sound_path.clone())
        );
        assert_eq!(over_limit_sound(&mut st, 11.0, &s), None);
        // Falling back into yellow: no repeat, yellow was already latched.
        assert_eq!(over_limit_sound(&mut st, 9.5, &s), None);
        // Dropping below yellow resets the state, so it fires again.
        assert_eq!(over_limit_sound(&mut st, 1.0, &s), None);
        assert_eq!(
            over_limit_sound(&mut st, 9.5, &s),
            Some(s.yellow_sound_path.clone())
        );
        // A disabled sound never fires.
        s.yellow_sound_enabled = false;
        let mut st2 = AlertState::default();
        assert_eq!(over_limit_sound(&mut st2, 9.5, &s), None);
    }

    #[test]
    fn over_limit_sound_repeats_after_delay() {
        let mut s = Settings::default();
        s.yellow_sound_enabled = true;
        let mut st = AlertState::default();
        assert_eq!(
            over_limit_sound(&mut st, 9.5, &s),
            Some(s.yellow_sound_path.clone())
        );
        // Simulate the repeat delay having passed while still over the limit.
        st.last_play = Some(Instant::now() - ALERT_REPEAT_DELAY);
        assert_eq!(
            over_limit_sound(&mut st, 9.5, &s),
            Some(s.yellow_sound_path.clone())
        );
        // ... and it goes quiet again until the delay passes once more.
        assert_eq!(over_limit_sound(&mut st, 9.6, &s), None);
    }
}
