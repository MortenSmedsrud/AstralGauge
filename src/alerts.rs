//! Limit checking and alert sounds.
//!
//! [`Settings`] holds the yellow/red current limits and the sound
//! configuration used across the app. [`over_limit_sound`] folds one poll
//! sample's maximum pin current into the latched alert state and decides
//! which (if any) alert sound should play now. [`critical_alert_edge`] and
//! [`kill_guard_action`] derive the latching UI notification and the
//! critical-alert kill safeguard's actions from the same current. [`SoundPlayer`]
//! owns a rodio output thread that plays the queued sounds, one at a time.

use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use rodio::{Decoder, OutputStream, Sink, Source};

/// Allowed range and defaults for the current limits (amperes).
pub(crate) const MIN_LIMIT_A: f32 = 1.0;
pub(crate) const MAX_LIMIT_A: f32 = 12.0;
pub(crate) const DEFAULT_YELLOW_LIMIT_A: f32 = 9.2;
pub(crate) const DEFAULT_RED_LIMIT_A: f32 = 10.0;

/// Minimum time between two alert sounds while a limit stays exceeded: after
/// a sound plays, the next one waits this long (only if the limit is still
/// surpassed; dropping below the yellow limit resets the alert state).
pub(crate) const ALERT_REPEAT_DELAY: Duration = Duration::from_secs(15);

/// How long the pin current must stay below the automatic power-limit trigger
/// level before the previously saved power limit is restored (hysteresis
/// against flapping right at the threshold).
pub(crate) const AUTO_POWER_RESTORE_DELAY: Duration = Duration::from_secs(30);

/// Minimum time between two critical-alert safeguard kills while the current
/// stays in the red range (a kill loop faster than this is worse than the
/// excursion). Additionally, a kill never interrupts a still-playing
/// critical-terminating sound of the previous kill.
pub(crate) const KILL_REPEAT_COOLDOWN: Duration = Duration::from_secs(5);

/// How long the alert sound of a red excursion is held back while a
/// cross-user kill (sent through the privileged helper) awaits its reply: a
/// termination within it replaces the alert with the critical-terminating
/// sound. Past it the alert plays anyway — the helper may be stuck behind an
/// open password prompt or hung, and a red current must never stay silent.
pub(crate) const KILL_SOUND_HOLD: Duration = Duration::from_secs(2);

/// How long a cross-user kill waits for the helper's reply before it is
/// given up (reported as failed), so a hung helper cannot block the
/// safeguard's further kills forever. A late reply is discarded when the
/// next kill is sent.
pub(crate) const KILL_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// What to do about a cross-user kill still awaiting the helper's reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KillReplyWait {
    /// Keep waiting, and keep the excursion's alert sound held back.
    HoldSound,
    /// Keep waiting for the outcome, but play alert sounds normally again.
    PlaySound,
    /// Stop waiting: report the kill as failed and re-enable the safeguard.
    GiveUp,
}

/// Decide, from how long the reply has been outstanding, how a pending
/// cross-user kill is treated (see [`KILL_SOUND_HOLD`],
/// [`KILL_REPLY_TIMEOUT`]).
pub(crate) fn kill_reply_wait(waited: Duration) -> KillReplyWait {
    if waited >= KILL_REPLY_TIMEOUT {
        KillReplyWait::GiveUp
    } else if waited >= KILL_SOUND_HOLD {
        KillReplyWait::PlaySound
    } else {
        KillReplyWait::HoldSound
    }
}

/// How long the pin readings must stay unavailable (I2C read failures, no
/// chip found) before the sensor fault plays its alert sound: a single
/// failed transfer is retried on the next poll without a sound.
pub(crate) const SENSOR_FAULT_GRACE: Duration = Duration::from_secs(3);

/// When the power management limit should be automatically lowered to its
/// hardware minimum (requires the privileged power helper, see
/// [`crate::hardware::PowerHelper`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum AutoMinPower {
    /// Feature disabled.
    #[default]
    Off,
    /// Lower the limit when a pin reaches the yellow limit.
    OnYellow,
    /// Lower the limit when a pin reaches the red limit.
    OnRed,
}

impl AutoMinPower {
    /// Position in the settings combo box.
    pub(crate) fn index(self) -> usize {
        match self {
            AutoMinPower::Off => 0,
            AutoMinPower::OnYellow => 1,
            AutoMinPower::OnRed => 2,
        }
    }

    pub(crate) fn from_index(i: usize) -> AutoMinPower {
        match i {
            1 => AutoMinPower::OnYellow,
            2 => AutoMinPower::OnRed,
            _ => AutoMinPower::Off,
        }
    }
}

/// One of the alert sounds compiled into the executable, so a single binary
/// is all that needs to be distributed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EmbeddedSound {
    /// Yellow-range alert.
    Attention,
    /// Red-range alert.
    Critical,
    /// Yellow-range alert while the automatic power limit is lowering the GPU.
    AttentionPoweringDown,
    /// Red-range alert while the automatic power limit is lowering the GPU.
    CriticalPoweringDown,
    /// A process was terminated by the critical-alert safeguard.
    CriticalTerminating,
}

impl EmbeddedSound {
    /// The WAV bytes embedded in the executable.
    pub(crate) fn bytes(self) -> &'static [u8] {
        match self {
            EmbeddedSound::Attention => include_bytes!("../sound/attention.wav"),
            EmbeddedSound::Critical => include_bytes!("../sound/critical.wav"),
            EmbeddedSound::AttentionPoweringDown => {
                include_bytes!("../sound/attention-powering-down.wav")
            }
            EmbeddedSound::CriticalPoweringDown => {
                include_bytes!("../sound/critical-powering-down.wav")
            }
            EmbeddedSound::CriticalTerminating => {
                include_bytes!("../sound/critical-terminating.wav")
            }
        }
    }

    /// Short name shown in the settings dialog.
    pub(crate) fn display_name(self) -> &'static str {
        match self {
            EmbeddedSound::Attention => "attention",
            EmbeddedSound::Critical => "critical",
            EmbeddedSound::AttentionPoweringDown => "attention (powering down)",
            EmbeddedSound::CriticalPoweringDown => "critical (powering down)",
            EmbeddedSound::CriticalTerminating => "critical-terminating",
        }
    }
}

/// An alert sound: one of the sounds embedded in the executable (the
/// default) or a `.wav` file the user selected in the settings dialog.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Sound {
    /// A sound shipped inside the executable.
    Embedded(EmbeddedSound),
    /// A sound file chosen by the user.
    File(PathBuf),
}

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
    /// Play `yellow_sound` while a pin's current is in the yellow range.
    pub(crate) yellow_sound_enabled: bool,
    pub(crate) yellow_sound: Sound,
    /// Play `red_sound` while a pin's current is in the red range.
    pub(crate) red_sound_enabled: bool,
    pub(crate) red_sound: Sound,
    /// Automatically lower the GPU power management limit to its minimum
    /// when a pin current reaches the selected limit level.
    pub(crate) auto_min_power: AutoMinPower,
    /// Critical-alert safeguard: kill (SIGKILL) the top GPU-using process
    /// when a pin current enters the red range. Disabled by default; see
    /// [`kill_guard_action`].
    pub(crate) kill_top_process_on_red: bool,
    /// Route the safeguard's SIGKILL through the privileged power helper, so
    /// processes of other users (for example an inference service running as
    /// root) can be terminated too. Needs the helper's administrator
    /// credentials (shared with the power limiting — one prompt serves
    /// both); a denied authentication reverts it, exactly like
    /// `auto_min_power`. Only meaningful while `kill_top_process_on_red` is
    /// enabled.
    pub(crate) kill_other_users_processes: bool,
    /// Sound played when the safeguard actually terminates a process. It is
    /// a priority sound: nothing interrupts it, and it replaces the red
    /// alert of the same excursion.
    pub(crate) terminating_sound: Sound,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            yellow_limit_a: DEFAULT_YELLOW_LIMIT_A,
            red_limit_a: DEFAULT_RED_LIMIT_A,
            yellow_sound_enabled: false,
            yellow_sound: Sound::Embedded(EmbeddedSound::Attention),
            red_sound_enabled: false,
            red_sound: Sound::Embedded(EmbeddedSound::Critical),
            auto_min_power: AutoMinPower::default(),
            kill_top_process_on_red: false,
            kill_other_users_processes: false,
            terminating_sound: Sound::Embedded(EmbeddedSound::CriticalTerminating),
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
///
/// While the automatic minimum power limit is enabled at (or below) the
/// reached level, the embedded default sounds play their "powering down"
/// variants instead, since reaching that level lowers the GPU power limit
/// (see [`active_sound`]).
pub(crate) fn over_limit_sound(
    state: &mut AlertState,
    max_current: f64,
    s: &Settings,
) -> Option<Sound> {
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
        Some(active_sound(
            if over_red {
                &s.red_sound
            } else {
                &s.yellow_sound
            },
            s,
            over_red,
        ))
    } else {
        None
    }
}

/// The sound to actually play for a reached range. With the automatic power
/// lowering active at the reached level, the embedded defaults become their
/// "powering down" variants; a user-selected sound file is always kept as-is.
///
/// `over_red` says which range was reached (this is only consulted for
/// sounds triggered by a yellow-or-higher current).
fn active_sound(sound: &Sound, s: &Settings, over_red: bool) -> Sound {
    let powering_down = match s.auto_min_power {
        AutoMinPower::Off => false,
        // Any caller is over yellow already, so OnYellow always triggers.
        AutoMinPower::OnYellow => true,
        AutoMinPower::OnRed => over_red,
    };
    match (sound, powering_down) {
        (Sound::Embedded(EmbeddedSound::Attention), true) => {
            Sound::Embedded(EmbeddedSound::AttentionPoweringDown)
        }
        (Sound::Embedded(EmbeddedSound::Critical), true) => {
            Sound::Embedded(EmbeddedSound::CriticalPoweringDown)
        }
        (other, _) => other.clone(),
    }
}

/// Critical-alert safeguard state carried across poll samples: whether the
/// red range is currently latched (so entering it counts as a rising edge)
/// and when the safeguard last fired (start of the repeat cooldown).
#[derive(Default)]
pub(crate) struct KillGuardState {
    latched_red: bool,
    last_kill: Option<Instant>,
}

impl KillGuardState {
    /// Start the repeat cooldown after the poller attempted a kill (whether
    /// or not it succeeded: a targetless or refused kill must not be retried
    /// every poll).
    pub(crate) fn note_kill_attempt(&mut self) {
        self.last_kill = Some(Instant::now());
    }
}

/// Fold one sample's maximum pin current into the critical-alert safeguard
/// state and return whether the top GPU-using process should be killed now.
///
/// Only fires while [`Settings::kill_top_process_on_red`] is enabled. It
/// fires on the rising edge into the red range and then **repeats while the
/// current stays red**, at most once per [`KILL_REPEAT_COOLDOWN`]. A kill is
/// also held back while `kill_sound_playing` is true: the previous kill's
/// critical-terminating sound must never be interrupted. The cooldown runs
/// between any two kills (so oscillating around the threshold cannot turn
/// into a kill loop), and the latch follows the red range continuously, so
/// enabling the option mid-excursion does not kill retroactively — only a
/// fresh edge (or a repeat of a kill this excursion already made) does.
pub(crate) fn kill_guard_action(
    state: &mut KillGuardState,
    max_current: f64,
    s: &Settings,
    kill_sound_playing: bool,
) -> bool {
    let over_red = max_current >= s.red_limit_a as f64;
    let rising = over_red && !state.latched_red;
    let due = state
        .last_kill
        .is_none_or(|t| t.elapsed() >= KILL_REPEAT_COOLDOWN);
    // First kill of an excursion on its rising edge; afterwards, repeats are
    // driven by the cooldown alone (never retroactively when the option gets
    // enabled while already in the red range).
    let fire = s.kill_top_process_on_red
        && over_red
        && !kill_sound_playing
        && due
        && (rising || state.last_kill.is_some());
    state.latched_red = over_red;
    fire
}

/// Critical-alert notification state carried across poll samples: whether the
/// red range is currently latched, so re-entering it is a fresh alert edge.
#[derive(Default)]
pub(crate) struct CriticalAlertState {
    latched_red: bool,
}

/// Fold one sample's maximum pin current into the critical-alert notification
/// state and return whether the critical alert just *triggered* (rising edge
/// into the red range). That edge shows the latching "critical current"
/// notification in the UIs; only a user acknowledgment clears it, and the
/// next red excursion arms it again.
pub(crate) fn critical_alert_edge(
    state: &mut CriticalAlertState,
    max_current: f64,
    s: &Settings,
) -> bool {
    let over_red = max_current >= s.red_limit_a as f64;
    let rising = over_red && !state.latched_red;
    state.latched_red = over_red;
    rising
}

/// Sensor-fault alert state carried across poll samples: since when the pin
/// readings have been unavailable, and whether this fault already alerted.
#[derive(Default)]
pub(crate) struct SensorFaultState {
    failing_since: Option<Instant>,
    alerted: bool,
}

/// Fold one poll's sensor health into the fault state and return the sound
/// to play now, if any.
///
/// Without pin readings the current limits cannot be checked at all, so an
/// unreadable sensor must not stay silent: once the readings have been
/// missing for [`SENSOR_FAULT_GRACE`], the alert sound plays **once** per
/// fault (a permanently missing chip does not nag every few seconds; the UI
/// banner stays up). Readings coming back re-arm it. It uses the yellow
/// (attention) sound when that is enabled, otherwise the red one when that
/// is enabled — anyone who enabled an alert sound hears a lost sensor.
pub(crate) fn sensor_fault_sound(
    state: &mut SensorFaultState,
    readings_ok: bool,
    s: &Settings,
) -> Option<Sound> {
    if readings_ok {
        *state = SensorFaultState::default();
        return None;
    }
    let since = *state.failing_since.get_or_insert_with(Instant::now);
    if state.alerted || since.elapsed() < SENSOR_FAULT_GRACE {
        return None;
    }
    state.alerted = true;
    if s.yellow_sound_enabled {
        Some(s.yellow_sound.clone())
    } else if s.red_sound_enabled {
        Some(s.red_sound.clone())
    } else {
        None
    }
}

/// Automatic-power-limit state carried across poll samples: the limit to
/// restore once the trigger condition clears (set while the limit is lowered)
/// and when the pin current first dropped below the trigger level.
#[derive(Default)]
pub(crate) struct AutoPowerState {
    saved_limit_w: Option<u32>,
    below_since: Option<Instant>,
}

impl AutoPowerState {
    /// Forget the auto-lowered state without restoring: used when the user
    /// applies a power limit manually, which takes precedence.
    pub(crate) fn overridden(&mut self) {
        self.saved_limit_w = None;
        self.below_since = None;
    }
}

/// Fold one sample into the automatic power-limit state machine and return
/// the power limit (watts) to apply now, if any.
///
/// When enabled, reaching the trigger level lowers the limit to its minimum
/// (saving the current limit), once per excursion. The saved limit is
/// restored after the current stays below the trigger level for
/// [`AUTO_POWER_RESTORE_DELAY`], or immediately when the feature gets
/// disabled. Does nothing when the limit is already at (or below) the
/// minimum or the NVML values are unavailable.
pub(crate) fn auto_power_action(
    state: &mut AutoPowerState,
    max_current: f64,
    limit_w: Option<u32>,
    limit_min_w: Option<u32>,
    s: &Settings,
) -> Option<u32> {
    let trigger_a = match s.auto_min_power {
        AutoMinPower::Off => {
            // Disabled: if we lowered the limit earlier, put it back.
            let saved = state.saved_limit_w.take();
            state.below_since = None;
            return saved;
        }
        AutoMinPower::OnYellow => s.yellow_limit_a as f64,
        AutoMinPower::OnRed => s.red_limit_a as f64,
    };

    if max_current >= trigger_a {
        state.below_since = None;
        // One lowering per excursion; re-arm only after a restore.
        if state.saved_limit_w.is_some() {
            return None;
        }
        match (limit_w, limit_min_w) {
            (Some(current), Some(min)) if current > min => {
                state.saved_limit_w = Some(current);
                Some(min)
            }
            _ => None,
        }
    } else if let Some(saved) = state.saved_limit_w {
        // Hysteresis: restore only once the current has been below the
        // trigger level for the full delay.
        let since = state.below_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= AUTO_POWER_RESTORE_DELAY {
            state.saved_limit_w = None;
            state.below_since = None;
            return Some(saved);
        }
        None
    } else {
        state.below_since = None;
        None
    }
}

/// Readable, seekable and sendable input for rodio's decoder, so the
/// `Cursor` over an embedded WAV and the `BufReader` over a file on disk can
/// share one playback code path through a single boxed trait object.
trait ReadSeekSend: std::io::Read + std::io::Seek + Send + Sync {}
impl<T: std::io::Read + std::io::Seek + Send + Sync + ?Sized> ReadSeekSend for T {}

type SoundReader = Box<dyn ReadSeekSend>;

/// One playback request: a sound plus whether it is a priority sound. The
/// critical-terminating sound is the only priority one: it must never be
/// dropped in favor of other requests and nothing interrupts it.
struct SoundRequest {
    sound: Sound,
    priority: bool,
}

/// Decide which of two requests collected in one batch plays next: priority
/// requests win over normal ones (older alerts are redundant anyway), and
/// within a priority class the newest wins.
fn next_request(current: SoundRequest, next: SoundRequest) -> SoundRequest {
    if next.priority && !current.priority {
        next
    } else if current.priority && !next.priority {
        current
    } else {
        next
    }
}

/// Handle to the alert-sound playback thread. Sound is best-effort: `play`
/// never blocks, and requests are dropped when no audio output exists
/// (headless session, missing device, undecodable file). Cloning shares the
/// one playback thread.
#[derive(Clone)]
pub(crate) struct SoundPlayer {
    tx: mpsc::Sender<SoundRequest>,
    /// Set while a priority (critical-terminating) sound is playing; lets the
    /// poller hold back the next safeguard kill instead of cutting the
    /// previous kill's sound in on.
    priority_playing: Arc<AtomicBool>,
}

impl SoundPlayer {
    /// Spawn the playback thread. rodio's output stream is created lazily on
    /// the first request, so apps run without an audio device never pay for
    /// (or fail on) audio initialization.
    pub(crate) fn spawn() -> SoundPlayer {
        let (tx, rx) = mpsc::channel();
        let priority_playing = Arc::new(AtomicBool::new(false));
        let flag = priority_playing.clone();
        thread::spawn(move || audio_loop(rx, flag));
        SoundPlayer {
            tx,
            priority_playing,
        }
    }

    /// Whether the critical-terminating (priority) sound is playing right
    /// now. False when no audio output exists at all.
    pub(crate) fn priority_sound_playing(&self) -> bool {
        self.priority_playing.load(Ordering::SeqCst)
    }

    /// Queue a sound for playback. Sounds play one at a time; while one is
    /// playing, at most the most recently queued normal sound is kept (and
    /// never at the expense of a queued priority sound).
    pub(crate) fn play(&self, sound: Sound) {
        let _ = self.tx.send(SoundRequest {
            sound,
            priority: false,
        });
    }

    /// Queue a priority sound: it outranks every pending normal request and
    /// the sound it waits behind is never cut in on once it is playing.
    pub(crate) fn play_priority(&self, sound: Sound) {
        let _ = self.tx.send(SoundRequest {
            sound,
            priority: true,
        });
    }
}

/// How long a sound may take beyond its own length before the output counts
/// as stalled (see `audio_loop`).
const SOUND_STALL_MARGIN: Duration = Duration::from_secs(5);

/// Open the default audio output. `None` (logged once per failure streak via
/// `warned`) when there is none yet: without a desktop session, or while the
/// sound server has not started.
fn open_audio(warned: &mut bool) -> Option<(OutputStream, Sink)> {
    let opened = OutputStream::try_default()
        .map_err(|e| format!("No audio output available: {e}"))
        .and_then(|(stream, handle)| {
            Sink::try_new(&handle)
                .map(|sink| (stream, sink))
                .map_err(|e| format!("Audio sink unavailable: {e}"))
        });
    match opened {
        Ok(session) => {
            if *warned {
                println!("Audio output available again.");
            }
            *warned = false;
            Some(session)
        }
        Err(e) => {
            if !*warned {
                eprintln!("{e}. Alert sounds are skipped until it is available.");
                *warned = true;
            }
            None
        }
    }
}

fn audio_loop(rx: mpsc::Receiver<SoundRequest>, priority_playing: Arc<AtomicBool>) {
    // rodio stops playback when the output stream is dropped, so the session
    // lives for the whole thread; it is None until the first successful try.
    // A missing output is retried on every request (the sound server can come
    // up after the app, e.g. when it runs as a service started at login).
    let mut session: Option<(OutputStream, Sink)> = None;
    let mut init_warned = false;

    while let Ok(mut request) = rx.recv() {
        // Play only the one remaining request: while a sound was playing,
        // older queued alerts are redundant, and priority ones (the
        // critical-terminating sound) outrank everything else.
        while let Ok(newer) = rx.try_recv() {
            request = next_request(request, newer);
        }

        if session.is_none() {
            session = open_audio(&mut init_warned);
        }
        let mut stalled = false;
        if let Some((_, sink)) = &session {
            let source = match &request.sound {
                // Embedded WAVs decode straight from the bytes in the binary.
                Sound::Embedded(embedded) => {
                    Decoder::new(Box::new(Cursor::new(embedded.bytes())) as SoundReader)
                        .map_err(|e| format!("built-in sound: {e}"))
                }
                Sound::File(path) => File::open(path)
                    .map_err(|e| format!("{}: {e}", path.display()))
                    .and_then(|f| {
                        Decoder::new(Box::new(BufReader::new(f)) as SoundReader)
                            .map_err(|e| format!("{}: {e}", path.display()))
                    }),
            };
            match source {
                // Waits until this sound finished, so only one plays at a
                // time; the next queued file is picked up by the loop above.
                Ok(source) => {
                    // A sound server that went away (restarted, session
                    // ended) never drains the sink: give up on this output
                    // after the sound's length plus a margin and open a new
                    // one for the next request instead of hanging forever.
                    let limit = source.total_duration().unwrap_or(Duration::from_secs(30))
                        + SOUND_STALL_MARGIN;
                    let priority = request.priority;
                    if priority {
                        priority_playing.store(true, Ordering::SeqCst);
                    }
                    sink.append(source);
                    let started = Instant::now();
                    while !sink.empty() {
                        if started.elapsed() > limit {
                            eprintln!("Audio output stalled; reopening it for the next sound.");
                            sink.stop();
                            stalled = true;
                            break;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    if priority {
                        priority_playing.store(false, Ordering::SeqCst);
                    }
                }
                Err(msg) => eprintln!("Cannot play alert sound: {msg}"),
            }
        }
        if stalled {
            session = None;
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
        assert_eq!(s.auto_min_power, AutoMinPower::Off);
        assert!(!s.kill_top_process_on_red && !s.kill_other_users_processes);
        assert!(s.yellow_limit_a >= MIN_LIMIT_A);
        assert!(s.yellow_limit_a <= s.red_limit_a);
        assert!(s.red_limit_a <= MAX_LIMIT_A);
        // The default sounds are the ones embedded in the executable.
        assert_eq!(s.yellow_sound, Sound::Embedded(EmbeddedSound::Attention));
        assert_eq!(s.red_sound, Sound::Embedded(EmbeddedSound::Critical));
        assert_eq!(
            s.terminating_sound,
            Sound::Embedded(EmbeddedSound::CriticalTerminating)
        );
    }

    #[test]
    fn priority_requests_beat_normal_ones() {
        let normal = |s: u8| SoundRequest {
            sound: Sound::File(PathBuf::from(format!("n{s}.wav"))),
            priority: false,
        };
        let priority = || SoundRequest {
            sound: Sound::Embedded(EmbeddedSound::CriticalTerminating),
            priority: true,
        };
        // The critical-terminating sound outranks anything queued around it,
        // in both arrival orders, and is never replaced by a newer alert.
        assert!(next_request(normal(1), priority()).priority);
        assert!(next_request(priority(), normal(2)).priority);
        // Within one class the newest request still wins.
        assert_eq!(
            next_request(normal(1), normal(2)).sound,
            Sound::File(PathBuf::from("n2.wav"))
        );
        assert!(next_request(priority(), priority()).priority);
    }

    #[test]
    fn embedded_sounds_are_decodable() {
        // The bytes compiled into the binary must play through the same
        // decoder path the audio thread uses.
        for sound in [
            EmbeddedSound::Attention,
            EmbeddedSound::Critical,
            EmbeddedSound::AttentionPoweringDown,
            EmbeddedSound::CriticalPoweringDown,
            EmbeddedSound::CriticalTerminating,
        ] {
            let bytes = sound.bytes();
            assert!(bytes.len() > 44, "{sound:?} looks empty");
            assert_eq!(&bytes[0..4], b"RIFF", "{sound:?} is not a WAV");
            let reader: SoundReader = Box::new(Cursor::new(bytes));
            Decoder::new(reader).unwrap_or_else(|e| panic!("{sound:?} undecodable: {e}"));
        }
    }

    #[test]
    fn over_limit_sound_edge_triggers() {
        let mut s = Settings {
            yellow_sound_enabled: true,
            red_sound_enabled: true,
            ..Default::default()
        };
        let mut st = AlertState::default();
        // Below yellow: nothing.
        assert_eq!(over_limit_sound(&mut st, 5.0, &s), None);
        // Enter yellow: the sound fires on the edge, then stays silent
        // within the repeat delay.
        assert_eq!(
            over_limit_sound(&mut st, 9.5, &s),
            Some(s.yellow_sound.clone())
        );
        assert_eq!(over_limit_sound(&mut st, 9.8, &s), None);
        // Enter red: the red sound fires right away (escalation).
        assert_eq!(
            over_limit_sound(&mut st, 10.5, &s),
            Some(s.red_sound.clone())
        );
        assert_eq!(over_limit_sound(&mut st, 11.0, &s), None);
        // Falling back into yellow: no repeat, yellow was already latched.
        assert_eq!(over_limit_sound(&mut st, 9.5, &s), None);
        // Dropping below yellow resets the state, so it fires again.
        assert_eq!(over_limit_sound(&mut st, 1.0, &s), None);
        assert_eq!(
            over_limit_sound(&mut st, 9.5, &s),
            Some(s.yellow_sound.clone())
        );
        // A disabled sound never fires.
        s.yellow_sound_enabled = false;
        let mut st2 = AlertState::default();
        assert_eq!(over_limit_sound(&mut st2, 9.5, &s), None);
    }

    #[test]
    fn auto_power_lowers_on_yellow_and_restores() {
        let mut s = Settings {
            auto_min_power: AutoMinPower::OnYellow,
            ..Default::default()
        };
        let mut st = AutoPowerState::default();
        // Below yellow: nothing. Yellow triggers a lowering to the minimum.
        assert_eq!(
            auto_power_action(&mut st, 5.0, Some(580), Some(300), &s),
            None
        );
        assert_eq!(
            auto_power_action(&mut st, 9.5, Some(580), Some(300), &s),
            Some(300)
        );
        // Still lowered: no repeat while the excursion lasts.
        assert_eq!(
            auto_power_action(&mut st, 9.7, Some(300), Some(300), &s),
            None
        );
        // Below yellow again: restore waits for the delay...
        assert_eq!(
            auto_power_action(&mut st, 5.0, Some(300), Some(300), &s),
            None
        );
        st.below_since = Some(Instant::now() - AUTO_POWER_RESTORE_DELAY);
        // ... then the saved limit comes back.
        assert_eq!(
            auto_power_action(&mut st, 5.0, Some(300), Some(300), &s),
            Some(580)
        );
        // A new excursion lowers again.
        assert_eq!(
            auto_power_action(&mut st, 9.5, Some(580), Some(300), &s),
            Some(300)
        );
        // Disabling the feature while lowered restores immediately.
        s.auto_min_power = AutoMinPower::Off;
        assert_eq!(
            auto_power_action(&mut st, 9.5, Some(300), Some(300), &s),
            Some(580)
        );
        assert_eq!(
            auto_power_action(&mut st, 9.5, Some(300), Some(300), &s),
            None
        );
    }

    #[test]
    fn auto_power_red_trigger_and_bounds() {
        let s = Settings {
            auto_min_power: AutoMinPower::OnRed,
            ..Default::default()
        };
        let mut st = AutoPowerState::default();
        // OnRed does not react to yellow currents.
        assert_eq!(
            auto_power_action(&mut st, 9.5, Some(580), Some(300), &s),
            None
        );
        assert_eq!(
            auto_power_action(&mut st, 10.5, Some(580), Some(300), &s),
            Some(300)
        );
        // Already at the minimum, or NVML constraints unknown: no-op.
        let mut st2 = AutoPowerState::default();
        assert_eq!(
            auto_power_action(&mut st2, 10.5, Some(300), Some(300), &s),
            None
        );
        assert_eq!(auto_power_action(&mut st2, 10.5, Some(580), None, &s), None);
        assert_eq!(auto_power_action(&mut st2, 10.5, None, Some(300), &s), None);
    }

    #[test]
    fn kill_guard_repeats_while_red_with_cooldown() {
        let mut s = Settings::default();
        let mut state = KillGuardState::default();
        s.red_sound_enabled = true;
        let red = s.red_limit_a as f64 + 0.5;
        let yellow = (s.yellow_limit_a + s.red_limit_a) as f64 / 2.0;
        let green = s.yellow_limit_a as f64 - 1.0;

        // Disabled: entering and staying in red never kills.
        assert!(!kill_guard_action(&mut state, red, &s, false));
        assert!(!kill_guard_action(&mut state, red, &s, false));

        // Enabled mid-excursion: the red range is already latched and this
        // excursion has not killed yet, so no retroactive kill.
        s.kill_top_process_on_red = true;
        assert!(!kill_guard_action(&mut state, red, &s, false));

        // Leave red (yellow does not fire, it is not the trigger range),
        // then a new red excursion fires on the rising edge.
        assert!(!kill_guard_action(&mut state, yellow, &s, false));
        assert!(kill_guard_action(&mut state, red, &s, false));
        state.note_kill_attempt();

        // Still red: no repeat before the cooldown elapsed.
        assert!(!kill_guard_action(&mut state, red, &s, false));
        // Once the kill sound finished and the cooldown expired, it repeats.
        state.last_kill = Some(Instant::now() - KILL_REPEAT_COOLDOWN);
        assert!(!kill_guard_action(&mut state, red, &s, true)); // sound still playing
        state.last_kill = Some(Instant::now() - KILL_REPEAT_COOLDOWN);
        assert!(kill_guard_action(&mut state, red, &s, false));
        state.note_kill_attempt();
        state.last_kill = Some(Instant::now() - KILL_REPEAT_COOLDOWN);
        assert!(kill_guard_action(&mut state, red, &s, false));
        state.note_kill_attempt();

        // Leaving red stops the repeats (the cooldown never runs while
        // green), and re-entering respects it too: within the cooldown the
        // fresh edge waits, then fires.
        assert!(!kill_guard_action(&mut state, yellow, &s, false));
        assert!(!kill_guard_action(&mut state, red, &s, false));
        state.last_kill = Some(Instant::now() - KILL_REPEAT_COOLDOWN);
        assert!(kill_guard_action(&mut state, red, &s, false));
        state.note_kill_attempt();
        assert!(!kill_guard_action(&mut state, green, &s, false));
        assert!(!kill_guard_action(&mut state, red, &s, false)); // in cooldown

        // Disabling the safeguard stops it again, even while red.
        state.last_kill = Some(Instant::now() - KILL_REPEAT_COOLDOWN);
        s.kill_top_process_on_red = false;
        assert!(!kill_guard_action(&mut state, red, &s, false));
    }

    #[test]
    fn pending_kill_never_mutes_the_alert_for_long() {
        let ms = Duration::from_millis;
        assert_eq!(kill_reply_wait(ms(0)), KillReplyWait::HoldSound);
        assert_eq!(
            kill_reply_wait(KILL_SOUND_HOLD - ms(1)),
            KillReplyWait::HoldSound
        );
        // A slow reply (password prompt open, hung helper): sounds resume.
        assert_eq!(kill_reply_wait(KILL_SOUND_HOLD), KillReplyWait::PlaySound);
        assert_eq!(
            kill_reply_wait(KILL_REPLY_TIMEOUT - ms(1)),
            KillReplyWait::PlaySound
        );
        // No reply at all: give up so the safeguard can act again.
        assert_eq!(kill_reply_wait(KILL_REPLY_TIMEOUT), KillReplyWait::GiveUp);
        assert!(KILL_SOUND_HOLD < KILL_REPLY_TIMEOUT);
    }

    #[test]
    fn critical_alert_triggers_on_red_edges() {
        let s = Settings::default();
        let mut state = CriticalAlertState::default();
        let red = s.red_limit_a as f64 + 0.5;
        let yellow = (s.yellow_limit_a + s.red_limit_a) as f64 / 2.0;
        let green = s.yellow_limit_a as f64 - 1.0;

        // Only the rising edge into the red range triggers the notification;
        // it never re-triggers while the excursion lasts (the latching UI
        // state is cleared by the user, not by this state machine).
        assert!(!critical_alert_edge(&mut state, green, &s));
        assert!(!critical_alert_edge(&mut state, yellow, &s));
        assert!(critical_alert_edge(&mut state, red, &s));
        assert!(!critical_alert_edge(&mut state, red, &s));
        assert!(!critical_alert_edge(&mut state, yellow, &s));
        assert!(!critical_alert_edge(&mut state, green, &s));

        // A fresh excursion is a fresh trigger.
        assert!(critical_alert_edge(&mut state, red, &s));
    }

    #[test]
    fn sensor_fault_alerts_once_after_grace() {
        let mut s = Settings {
            red_sound_enabled: true,
            ..Default::default()
        };
        let mut st = SensorFaultState::default();
        // Healthy readings: silence. A fresh fault waits out the grace.
        assert_eq!(sensor_fault_sound(&mut st, true, &s), None);
        assert_eq!(sensor_fault_sound(&mut st, false, &s), None);
        // Grace over: one alert (only red is enabled, so the red sound)...
        st.failing_since = Some(Instant::now() - SENSOR_FAULT_GRACE);
        assert_eq!(
            sensor_fault_sound(&mut st, false, &s),
            Some(s.red_sound.clone())
        );
        // ... and no nagging while the same fault lasts.
        st.failing_since = Some(Instant::now() - SENSOR_FAULT_GRACE * 10);
        assert_eq!(sensor_fault_sound(&mut st, false, &s), None);
        // Readings back re-arm it; the yellow sound is preferred if enabled.
        assert_eq!(sensor_fault_sound(&mut st, true, &s), None);
        s.yellow_sound_enabled = true;
        assert_eq!(sensor_fault_sound(&mut st, false, &s), None);
        st.failing_since = Some(Instant::now() - SENSOR_FAULT_GRACE);
        assert_eq!(
            sensor_fault_sound(&mut st, false, &s),
            Some(s.yellow_sound.clone())
        );
        // With every alert sound disabled a fault stays silent (UI only).
        let quiet = Settings::default();
        let mut st = SensorFaultState {
            failing_since: Some(Instant::now() - SENSOR_FAULT_GRACE),
            alerted: false,
        };
        assert_eq!(sensor_fault_sound(&mut st, false, &quiet), None);
    }

    #[test]
    fn over_limit_sound_repeats_after_delay() {
        let s = Settings {
            yellow_sound_enabled: true,
            ..Default::default()
        };
        let mut st = AlertState::default();
        assert_eq!(
            over_limit_sound(&mut st, 9.5, &s),
            Some(s.yellow_sound.clone())
        );
        // Simulate the repeat delay having passed while still over the limit.
        st.last_play = Some(Instant::now() - ALERT_REPEAT_DELAY);
        assert_eq!(
            over_limit_sound(&mut st, 9.5, &s),
            Some(s.yellow_sound.clone())
        );
        // ... and it goes quiet again until the delay passes once more.
        assert_eq!(over_limit_sound(&mut st, 9.6, &s), None);
    }

    #[test]
    fn auto_power_switches_to_powering_down_sounds() {
        let mut s = Settings {
            yellow_sound_enabled: true,
            red_sound_enabled: true,
            ..Default::default()
        };
        let mine = Sound::File(PathBuf::from("/tmp/mine.wav"));

        // Feature off: the plain embedded sounds.
        let mut st = AlertState::default();
        assert_eq!(
            over_limit_sound(&mut st, 9.5, &s),
            Some(Sound::Embedded(EmbeddedSound::Attention))
        );

        // On yellow: both ranges play their powering-down variant (the red
        // escalation happens while the GPU is being powered down too).
        s.auto_min_power = AutoMinPower::OnYellow;
        let mut st = AlertState::default();
        assert_eq!(
            over_limit_sound(&mut st, 9.5, &s),
            Some(Sound::Embedded(EmbeddedSound::AttentionPoweringDown))
        );
        assert_eq!(
            over_limit_sound(&mut st, 10.5, &s),
            Some(Sound::Embedded(EmbeddedSound::CriticalPoweringDown))
        );

        // On red: yellow excursions keep the plain sound; red switches.
        s.auto_min_power = AutoMinPower::OnRed;
        let mut st = AlertState::default();
        assert_eq!(
            over_limit_sound(&mut st, 9.5, &s),
            Some(Sound::Embedded(EmbeddedSound::Attention))
        );
        assert_eq!(
            over_limit_sound(&mut st, 10.5, &s),
            Some(Sound::Embedded(EmbeddedSound::CriticalPoweringDown))
        );

        // A user-selected file is never replaced.
        s.yellow_sound = mine.clone();
        s.auto_min_power = AutoMinPower::OnYellow;
        let mut st = AlertState::default();
        assert_eq!(over_limit_sound(&mut st, 9.5, &s), Some(mine));
    }
}
