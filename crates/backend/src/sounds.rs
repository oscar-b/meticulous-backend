//! `sounds.py`: event sounds from a theme folder, played with gst-play.
//!
//! Deliberate change: the Python keeps the speaker amplifier on from `init`.
//! Here it is on only during a sound. The WM8904 powers down about 10 s after
//! a sound (5 s PulseAudio suspend, 5 s ASoC `pmdown_time`). Its power-up and
//! power-down crackle on the speaker unless the amplifier is off by then, so
//! the amplifier turns on once the stream runs and off when the player ends.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::config::{ConfigStore, keys};
use crate::host::{Host, Process};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundEvent {
    Startup,
    HeatingStart,
    HeatingEnd,
    BrewingStart,
    BrewingEnd,
    Abort,
    Idle,
    Notification,
}

impl SoundEvent {
    /// `sound_event.name.lower()`: the key in a theme's config.json.
    pub fn key(self) -> &'static str {
        match self {
            SoundEvent::Startup => "startup",
            SoundEvent::HeatingStart => "heating_start",
            SoundEvent::HeatingEnd => "heating_end",
            SoundEvent::BrewingStart => "brewing_start",
            SoundEvent::BrewingEnd => "brewing_end",
            SoundEvent::Abort => "abort",
            SoundEvent::Idle => "idle",
            SoundEvent::Notification => "notification",
        }
    }
}

pub const DEFAULT_THEME: &str = "default";
const AUDIO_ENABLE_GPIO_CHIP: &str = "/dev/gpiochip4";
const AUDIO_ENABLE_GPIO_LINE: u32 = 25;
/// The ALSA playback stream: `state: RUNNING` once the codec has started and
/// `closed` when PulseAudio suspended the sink. By the card's ID, a link to
/// its `cardN` folder, so that this follows the card as PulseAudio does.
const PCM_STATUS: &str = "/proc/asound/wm8904audio/pcm0p/sub0/status";
/// How often the stream is checked while a sound starts.
const STREAM_POLL: Duration = Duration::from_millis(5);
/// How often the player is checked during a sound.
const PLAYER_POLL: Duration = Duration::from_millis(50);
/// Fallback only: the stream normally reports `RUNNING` well before this
/// (about 155 ms on a machine). If it does not, the amplifier turns on anyway,
/// so that a wrong status path or a very slow start cannot silence a sound.
const AMPLIFIER_ON_LIMIT: Duration = Duration::from_millis(500);
/// playsound3's `gst_play` backend: what `playsound(path, block=False)` runs.
const PLAYER: [&str; 3] = ["gst-play-1.0", "--no-interactive", "--quiet"];

/// `KNOWN_THEMES`: theme name to folder, in the order of the Python dict
/// (user themes first, then system themes; a system theme with a user
/// theme's name takes its folder and keeps its place).
type Themes = Vec<(String, Theme)>;

#[derive(Debug, Clone)]
struct Theme {
    folder: PathBuf,
    /// The system default theme without its `config.json`: the theme's
    /// config is the one built into the binary.
    built_in: bool,
}

impl Theme {
    fn on_disk(folder: PathBuf) -> Theme {
        Theme {
            folder,
            built_in: false,
        }
    }
}

/// The speaker amplifier and what tells when it may turn on.
pub trait Amplifier: Send + Sync {
    fn set(&self, on: bool);
    /// Has the codec started, so that the speaker hears none of its start?
    fn stream_running(&self) -> bool;
}

struct GpioAmplifier(gpiocdev::Request);

impl Amplifier for GpioAmplifier {
    fn set(&self, on: bool) {
        let value = if on {
            gpiocdev::line::Value::Active
        } else {
            gpiocdev::line::Value::Inactive
        };
        if let Err(error) = self.0.set_value(AUDIO_ENABLE_GPIO_LINE, value) {
            tracing::warn!("Could not set the amplifier: {error}");
        }
    }

    fn stream_running(&self) -> bool {
        pcm_running(Path::new(PCM_STATUS))
    }
}

#[derive(Default)]
struct State {
    /// `None` until `init` ran (`KNOWN_THEMES is None`).
    known_themes: Option<Themes>,
    /// PARITY: Python's CURRENT_THEME_CONFIG is DEFAULT_THEME_CONFIG, updated
    /// in place by every theme set, so keys of earlier themes linger.
    theme_config: Map<String, Value>,
    current_theme: Option<String>,
    playing: Option<Box<dyn Process>>,
    /// Tells a sound from the one that replaced it.
    generation: u64,
}

pub struct SoundPlayer {
    config: Arc<ConfigStore>,
    host: Arc<Host>,
    system_sounds: PathBuf,
    user_sounds: PathBuf,
    fake_hardware: bool,
    state: Arc<Mutex<State>>,
    /// `None` in emulation, with the hardware faked, or if the GPIO request failed.
    amplifier: Mutex<Option<Arc<dyn Amplifier>>>,
    /// Every event asked for, played or not: what the Python suite records
    /// in place of `SoundPlayer.play_event_sound`.
    #[cfg(test)]
    pub events: Mutex<Vec<SoundEvent>>,
}

impl SoundPlayer {
    pub fn new(
        config: Arc<ConfigStore>,
        host: Arc<Host>,
        system_sounds: PathBuf,
        user_sounds: PathBuf,
        fake_hardware: bool,
    ) -> SoundPlayer {
        SoundPlayer {
            config,
            host,
            system_sounds,
            user_sounds,
            fake_hardware,
            state: Arc::new(Mutex::new(State::default())),
            amplifier: Mutex::new(None),
            #[cfg(test)]
            events: Mutex::new(Vec::new()),
        }
    }

    /// `SoundPlayer.init`: take the amplifier, find the themes, set the
    /// volume and the configured theme, play the startup sound.
    pub fn init(&self, emulation: bool, play_startup_sound: bool) {
        if !emulation && !self.fake_hardware {
            match gpiocdev::Request::builder()
                .on_chip(AUDIO_ENABLE_GPIO_CHIP)
                .with_consumer("sounds")
                .with_line(AUDIO_ENABLE_GPIO_LINE)
                .as_output(gpiocdev::line::Value::Inactive)
                .request()
            {
                Ok(request) => *self.amplifier.lock() = Some(Arc::new(GpioAmplifier(request))),
                Err(error) => tracing::error!("Failed to set GPIO pin: {error}"),
            }
        }

        let mut themes = Themes::new();
        for (name, theme) in scan_folder(&self.user_sounds)
            .into_iter()
            .chain(scan_folder(&self.system_sounds))
        {
            insert(&mut themes, name, theme);
        }
        // Deliberate change: a package without the default theme's
        // config.json still has the theme (and serves its config), where
        // Python has no theme at all and logs errors on every set and play.
        // Its sounds play from the system folder if they are there.
        if folder(&themes, DEFAULT_THEME).is_none() {
            tracing::warn!(
                "No sound theme '{DEFAULT_THEME}' with a config.json in {}, using the built-in config",
                self.system_sounds.display()
            );
            themes.push((
                DEFAULT_THEME.to_string(),
                Theme {
                    folder: self.system_sounds.join(DEFAULT_THEME),
                    built_in: true,
                },
            ));
        }
        {
            let mut state = self.state.lock();
            state.theme_config = load_theme(&themes, DEFAULT_THEME).unwrap_or_default();
            state.known_themes = Some(themes);
        }

        if !self.fake_hardware
            && let Err(error) = self
                .host
                .run(&["pactl", "--", "set-sink-volume", "0", "40%"])
        {
            tracing::error!("failed to set audio volume: {error}");
        }

        let theme = self.config.get(keys::SYSTEM, keys::SOUNDS_THEME);
        self.set_theme(theme.as_str());
        if play_startup_sound {
            self.play_event(SoundEvent::Startup);
        }
    }

    pub fn available_themes(&self) -> Vec<String> {
        self.state
            .lock()
            .known_themes
            .as_ref()
            .map(|t| t.iter().map(|(name, _)| name.clone()).collect())
            .unwrap_or_default()
    }

    /// `get_theme()`: the merged theme config.
    pub fn theme_config(&self) -> Map<String, Value> {
        self.state.lock().theme_config.clone()
    }

    /// `set_theme(name)`: `None` is accepted like Python's `theme_name is None`.
    pub fn set_theme(&self, name: Option<&str>) -> bool {
        let known = {
            let state = self.state.lock();
            match &state.known_themes {
                None => {
                    tracing::warn!(
                        "Manipulating theme before SoundPlayer was initialized. Ignoring."
                    );
                    return false;
                }
                Some(themes) => name.is_none_or(|n| folder(themes, n).is_some()),
            }
        };
        if known {
            self.config.set_and_save(
                keys::SYSTEM,
                keys::SOUNDS_THEME,
                name.map_or(Value::Null, |n| Value::String(n.to_string())),
            );
            let mut state = self.state.lock();
            let themes = state.known_themes.clone().unwrap_or_default();
            // Deliberate fix: for a cleared theme (`None`) Python runs
            // `update(_load_theme(None))`, i.e. `update(False)`, a TypeError
            // out of init or play_sound; the merged config stays as it is.
            if let Some(config) = name.and_then(|n| load_theme(&themes, n)) {
                for (k, v) in config {
                    state.theme_config.insert(k, v);
                }
            }
            state.current_theme = name.map(str::to_string);
            tracing::info!("Sound theme '{}' loaded", name.unwrap_or("None"));
            true
        } else {
            tracing::error!(
                "Error: Theme '{}' is not available.",
                name.unwrap_or("None")
            );
            if name != Some(DEFAULT_THEME) {
                self.set_theme(Some(DEFAULT_THEME));
            }
            false
        }
    }

    pub fn play_event(&self, event: SoundEvent) -> bool {
        #[cfg(test)]
        self.events.lock().push(event);
        self.play(event.key())
    }

    /// `play_sound(name)`: true when played or deliberately silent.
    pub fn play(&self, name: &str) -> bool {
        if !self.config.get_bool(keys::USER, keys::SOUNDS_ENABLED) {
            tracing::info!("No sounds enabled");
            return true;
        }
        let configured = self.config.get_str(keys::SYSTEM, keys::SOUNDS_THEME);
        {
            let state = self.state.lock();
            if state.known_themes.is_none() {
                tracing::warn!("Playing sound before SoundPlayer was initialized. Ignoring.");
                return false;
            }
            if state.current_theme != configured {
                drop(state);
                self.set_theme(configured.as_deref());
            }
        }
        let mut state = self.state.lock();
        let Some(entry) = state.theme_config.get(name).cloned() else {
            tracing::info!("Sound not found");
            return false;
        };
        let file_name = match &entry {
            Value::String(s) if !s.is_empty() => s.clone(),
            Value::Null => {
                tracing::info!("Sound {name} is disabled by the theme");
                return true;
            }
            Value::String(_) => {
                tracing::info!("Sound {name} is disabled by the theme");
                return true;
            }
            Value::Object(o) if o.is_empty() => {
                tracing::info!("Sound {name} is disabled by the theme");
                return true;
            }
            other => other.to_string(),
        };
        let theme_path = state
            .current_theme
            .as_ref()
            .and_then(|t| state.known_themes.as_ref().and_then(|k| folder(k, t)))
            .cloned()
            .unwrap_or_default();
        let file_path = crate::http::absolute(&theme_path.join(&file_name));
        tracing::info!("Playing {name} from {}", file_path.display());
        if !file_path.exists() {
            tracing::warn!("Sound file not found: {}", file_path.display());
            return false;
        }
        if self.fake_hardware {
            return true;
        }
        if let Some(mut previous) = state.playing.take() {
            previous.stop();
        }
        let file = file_path.to_string_lossy();
        let mut argv = PLAYER.to_vec();
        argv.push(&file);
        match self.host.start(&argv) {
            Ok(process) => {
                state.playing = Some(process);
                state.generation += 1;
                self.watch_amplifier(state.generation);
                true
            }
            Err(error) => {
                tracing::error!("Failed to play sound: {error}");
                false
            }
        }
    }

    /// Run the amplifier for this sound, from a thread that ends with it.
    fn watch_amplifier(&self, generation: u64) {
        let Some(amplifier) = self.amplifier.lock().clone() else {
            return;
        };
        let state = self.state.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("sound-amplifier".to_string())
            .spawn(move || drive_amplifier(amplifier.as_ref(), &state, generation))
        {
            tracing::error!("Failed to watch the sound: {error}");
        }
    }

    #[cfg(test)]
    pub fn set_amplifier(&self, amplifier: Arc<dyn Amplifier>) {
        *self.amplifier.lock() = Some(amplifier);
    }
}

/// Turn the amplifier on once the stream runs and off when the player ends.
/// A newer sound takes over: the amplifier stays on and this thread just ends.
fn drive_amplifier(amplifier: &dyn Amplifier, state: &Mutex<State>, generation: u64) {
    let deadline = Instant::now() + AMPLIFIER_ON_LIMIT;
    let mut on = false;
    while still_playing(amplifier, state, generation) {
        if !on {
            on = amplifier.stream_running();
            if !on && Instant::now() >= deadline {
                tracing::warn!("The sound stream did not start, turning the amplifier on");
                on = true;
            }
            if on {
                amplifier.set(true);
            }
        }
        std::thread::sleep(if on { PLAYER_POLL } else { STREAM_POLL });
    }
}

/// Is this sound still the current one, with its player running? The current
/// sound's end turns the amplifier off under the lock, so a sound that starts
/// now can only turn it on after that.
fn still_playing(amplifier: &dyn Amplifier, state: &Mutex<State>, generation: u64) -> bool {
    let mut state = state.lock();
    if state.generation != generation {
        return false;
    }
    if state.playing.as_mut().is_none_or(|p| p.finished()) {
        state.playing = None;
        amplifier.set(false);
        return false;
    }
    true
}

fn pcm_running(status: &Path) -> bool {
    std::fs::read_to_string(status).is_ok_and(|text| text.contains("state: RUNNING"))
}

fn theme<'a>(themes: &'a Themes, name: &str) -> Option<&'a Theme> {
    themes.iter().find(|(n, _)| n == name).map(|(_, t)| t)
}

fn folder<'a>(themes: &'a Themes, name: &str) -> Option<&'a PathBuf> {
    theme(themes, name).map(|t| &t.folder)
}

/// `dict[name] = folder`: a known name keeps its place.
fn insert(themes: &mut Themes, name: String, theme: Theme) {
    match themes.iter_mut().find(|(n, _)| *n == name) {
        Some(entry) => entry.1 = theme,
        None => themes.push((name, theme)),
    }
}

/// `scan_folder`: every directory below `folder` with a readable config.json,
/// in `os.walk` order (top-down; each directory's subdirectories in listing
/// order before any of theirs; symlinked directories listed, not entered).
fn scan_folder(folder: &Path) -> Themes {
    tracing::info!("Scanning Theme folder {}", folder.display());
    let mut themes = Themes::new();
    walk(folder, &mut themes);
    themes
}

fn walk(dir: &Path, themes: &mut Themes) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let subdirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    for path in &subdirs {
        if has_config(path) {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                tracing::info!("Found sound theme '{}'", path.display());
                insert(themes, name.to_string(), Theme::on_disk(path.clone()));
            }
        } else {
            tracing::info!("'{}' has no config.json", path.display());
        }
    }
    for path in subdirs {
        if !path.is_symlink() {
            walk(&path, themes);
        }
    }
}

fn has_config(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("config.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .is_some()
}

fn load_theme(themes: &Themes, name: &str) -> Option<Map<String, Value>> {
    let theme = theme(themes, name)?;
    let text = if theme.built_in {
        crate::bundled::DEFAULT_SOUND_THEME.to_string()
    } else {
        std::fs::read_to_string(theme.folder.join("config.json")).ok()?
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Some(map),
        _ => Some(Map::new()),
    }
}

#[cfg(test)]
pub mod fake {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use parking_lot::Mutex;

    use super::Amplifier;

    /// Records every change of the amplifier; `running` is the stream.
    #[derive(Default)]
    pub struct FakeAmplifier {
        pub running: AtomicBool,
        pub changes: Mutex<Vec<bool>>,
    }

    impl FakeAmplifier {
        pub fn stream_starts(&self) {
            self.running.store(true, Ordering::SeqCst);
        }

        /// Wait for the amplifier's thread to reach `expected`.
        pub fn wait_for(&self, expected: &[bool]) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.changes.lock().as_slice() != expected && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(self.changes.lock().as_slice(), expected);
        }
    }

    impl Amplifier for FakeAmplifier {
        fn set(&self, on: bool) {
            self.changes.lock().push(on);
        }

        fn stream_running(&self) -> bool {
            self.running.load(Ordering::SeqCst)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::thread::JoinHandle;

    use super::fake::FakeAmplifier;
    use super::*;
    use crate::host::CommandRunner as _;
    use crate::host::fake::FakeRunner;

    struct Watched {
        amplifier: Arc<FakeAmplifier>,
        runner: FakeRunner,
        state: Arc<Mutex<State>>,
        thread: JoinHandle<()>,
    }

    /// One sound with its player running and the amplifier's thread started.
    fn watched(stream_running: bool) -> Watched {
        let amplifier = Arc::new(FakeAmplifier::default());
        if stream_running {
            amplifier.stream_starts();
        }
        let runner = FakeRunner::default();
        let process = runner.start(&["gst-play-1.0"]).expect("started");
        let state = Arc::new(Mutex::new(State {
            playing: Some(process),
            generation: 1,
            ..State::default()
        }));
        let thread = std::thread::spawn({
            let amplifier = amplifier.clone();
            let state = state.clone();
            move || drive_amplifier(&*amplifier, &state, 1)
        });
        Watched {
            amplifier,
            runner,
            state,
            thread,
        }
    }

    impl Watched {
        fn player_ends(&self) {
            self.runner.finished.store(true, Ordering::SeqCst);
        }

        fn thread_ends(self) -> Vec<bool> {
            self.thread.join().expect("the amplifier's thread");
            self.amplifier.changes.lock().clone()
        }
    }

    /// A user hears the crackle before every sound after an idle time if the
    /// amplifier turns on before the stream runs, and the amplifier wastes
    /// power and passes the codec's power-down noise if it stays on after the
    /// player. The order comes from `drive_amplifier`, and a test on a machine:
    /// amplifier on at `RUNNING`, off at the end, no crackle. Without the wait
    /// for the stream, the first assert would fail.
    #[test]
    fn amplifier_waits_for_the_stream_and_ends_with_the_player() {
        let sound = watched(false);
        std::thread::sleep(STREAM_POLL * 3);
        assert!(sound.amplifier.changes.lock().is_empty());

        sound.amplifier.stream_starts();
        sound.amplifier.wait_for(&[true]);

        sound.player_ends();
        assert_eq!(sound.thread_ends(), [true, false]);
    }

    /// An alert (no water) stays audible if the stream state cannot be read:
    /// after `AMPLIFIER_ON_LIMIT` the amplifier turns on anyway. Without the
    /// limit, the amplifier would never turn on and this test would fail.
    #[test]
    fn a_stream_that_never_reports_running_still_gets_the_amplifier() {
        let start = Instant::now();
        let sound = watched(false);
        sound.amplifier.wait_for(&[true]);
        assert!(start.elapsed() >= AMPLIFIER_ON_LIMIT);

        sound.player_ends();
        assert_eq!(sound.thread_ends(), [true, false]);
    }

    /// A sound that replaces another keeps the amplifier on: the older sound's
    /// thread must not cut the new sound off. Without the generation check,
    /// the old thread would turn the amplifier off.
    #[test]
    fn a_replaced_sound_leaves_the_amplifier_to_the_new_one() {
        let sound = watched(true);
        sound.amplifier.wait_for(&[true]);

        sound.state.lock().generation = 2;
        sound.player_ends();
        assert_eq!(sound.thread_ends(), [true]);
    }

    /// A sound that ends before its stream runs (a player that cannot open
    /// the file) leaves the amplifier off, and its thread ends. Without the
    /// check in the loop, the amplifier would turn on after the limit.
    #[test]
    fn a_player_that_ends_before_the_stream_keeps_the_amplifier_off() {
        let sound = watched(false);
        sound.player_ends();
        assert_eq!(sound.thread_ends(), [false]);
    }

    /// The kernel's status file: `closed` while idle, and the `state: RUNNING`
    /// a machine printed during a sound. A missing file (a renamed card) is not
    /// a running stream. Reading any text as running would fail here.
    #[test]
    fn only_a_running_stream_counts_as_running() {
        let dir = tempfile::tempdir().expect("temporary folder");
        let status = dir.path().join("status");
        assert!(!pcm_running(&status));

        std::fs::write(&status, "closed\n").expect("status");
        assert!(!pcm_running(&status));

        std::fs::write(&status, "state: RUNNING\nowner_pid   : 1042\n").expect("status");
        assert!(pcm_running(&status));
    }
}
