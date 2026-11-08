//! abyssd v1-lite (r2): the Abyss keys-only music daemon.
//!
//! Single-threaded event loop (poll on the evdev fds, the player's notify
//! socket and a signal pipe, with the poll timeout as the timer) plus one
//! decode/playback thread. See README.md for what v1-lite does and does not do.

// binder's declare_binder_interface! (wifi/netd.rs) uses alloc internally.
extern crate alloc;

mod audio;
mod backlight;
mod battery;
mod bt;
mod capture;
mod clock;
mod control;
mod display;
mod flac_stream;
mod font;
mod font_data;
mod input;
mod keys;
mod http;
mod library;
mod module_ui;
mod modules;
mod mp3_stream;
mod player;
mod radio_source;
mod stream;
mod suspend;
mod tags;
mod ui;
mod wifi;
mod wifi_net;

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use log::{error, info, warn, LevelFilter, Log, Metadata, Record};
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

use audio::Output;
use battery::Battery;
use bt::Bt;
use display::Display;
use input::{Input, Inputs};
use keys::{Action, Ctx, KeyMachine};
use library::{Library, ResumeState};
use player::{Cmd, PlayState, Player, Status};
use ui::{Request, Screen, Ui, View, ACCENT, DIM, TEXT, WARN};
use wifi::Wifi;

/// Screen-off timeout after the last key: persist.abyss.screen_timeout_s
/// (seconds, default 15, clamped to 5..120), read once at start.
const PROP_SCREEN_TIMEOUT: &str = "persist.abyss.screen_timeout_s";
const SCREEN_TIMEOUT_DEFAULT_S: u64 = 15;
const SCREEN_TIMEOUT_MIN_S: u64 = 5;
const SCREEN_TIMEOUT_MAX_S: u64 = 120;
/// Help stays lit longer: it is there to be read (at least this, or the setting).
const HELP_SCREEN_MS: u64 = 30_000;
/// "Saved: <line>" after a capture (screen on).
const CAPTURE_MSG_MS: u64 = 2_000;
/// The on-screen volume box after a volume key (screen on only).
const VOLUME_OVERLAY_MS: u64 = 2_000;
/// Feedback lines ("Playing on USB · ...", "Cancelled ...").
const MSG_MS: u64 = 4_000;
/// Headset button: presses closer than this count together (1× 2× 3×).
const HEADSET_TAP_MS: u64 = 450;
const OUTPUT_CHECK_MS: u64 = 2_000;
const OUTPUT_CHECK_IDLE_MS: u64 = 10_000;
const BATTERY_EVERY_MS: u64 = 30_000;
/// Resume position is saved this often while playing (plus on every pause, track
/// change and exit), so Resume and a crash both land within ~15 s of where you were.
const SAVE_EVERY_MS: u64 = 15_000;
const IDLE_BEFORE_SUSPEND_MS: u64 = 10_000;
/// After every wake from suspend, stay awake at least this long (owner hit an
/// "uncontrollable" Abyss on 2026-11-07 with immediate re-suspend).
const POST_RESUME_AWAKE_MS: u64 = 1_500;
/// While the DSP plays: try to suspend again this soon after a resume (the
/// DSP's fragment interrupt woke us; the player refills in a few ms).
const OFFLOAD_SUSPEND_RETRY_MS: u64 = 500;
/// 200 volume steps, the same count IchthysOS uses for media volume (owner, 2026-11-07).
/// One key tap = one step. The curve in player.rs spans 72 dB, so the low steps are quiet.
const VOLUME_MAX: u32 = 200;
const VOLUME_STEP: u32 = 1;
/// Safe-listening limit (70 % of 200): going above needs a second ▲ within MSG_MS.
const VOLUME_SAFE: u32 = 140;
/// Taps closer than this count as one fast streak for the adaptive step.
const VOLUME_SPAM_MS: u64 = 350;
/// The adaptive step never goes above 5 steps per tap.
const VOLUME_STEP_MAX: u32 = 5;
/// Low-battery shutdown: at the 3 % reserve, after a 60 s warning.
const LOW_BATTERY_PCT: u32 = 3;
const LOW_BATTERY_WARN_MS: u64 = 60_000;
const SESSION_LOG: &str = "/data/misc/abyss/out/session.log";
/// More audio HAL restarts than this within `HAL_RESTART_WINDOW_MS` = give up and leave.
const HAL_RESTART_MAX: usize = 3;
const HAL_RESTART_WINDOW_MS: u64 = 60_000;
/// Wi-Fi tear-down budget on exit: generous when abyssd leaves on its own (Back to Android,
/// power off), tiny on SIGTERM (init SIGKILLs 200 ms after it; abyss.rc's leave path stops
/// wpa_supplicant + abyss-net anyway, and abyss-net cleans netd on its way out).
const WIFI_STOP_MS: u64 = 1_500;
const WIFI_STOP_TERM_MS: u64 = 40;

// ---------------------------------------------------------------------------
// Logging: logcat (tag "abyssd") + /data/misc/abyss/out/session.log.

struct Logger {
    logcat: android_logger::AndroidLogger,
    file: Option<Mutex<File>>,
}

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= log::Level::Info
    }
    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        self.logcat.log(r);
        if let Some(f) = &self.file {
            if let Ok(mut f) = f.lock() {
                let _ = writeln!(f, "{:>9} {:<5} {}", clock::now_ms(), r.level(), r.args());
            }
        }
    }
    fn flush(&self) {}
}

fn init_logging() {
    let _ = fs::create_dir_all(library::STATE_DIR);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(SESSION_LOG)
        .ok()
        .map(Mutex::new);
    let logcat = android_logger::AndroidLogger::new(
        android_logger::Config::default().with_tag("abyssd").with_max_level(LevelFilter::Info),
    );
    if log::set_boxed_logger(Box::new(Logger { logcat, file })).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
}

// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exit {
    Leave,
    PowerOff,
    Term,
}

struct App {
    display: Display,
    inputs: Inputs,
    keys: KeyMachine,
    ui: Ui,
    lib: Library,
    player: Player,
    status: Status,
    battery: Battery,
    battery_at: u64,
    resume: ResumeState,
    volume: u32,
    /// 3.5 mm jack switch state (evdev).
    headphones: bool,
    /// The output in use: Bluetooth (connected) > USB DAC > jack > None.
    output: Option<Output>,
    /// Bluetooth headphones through abyss-btd (off by default).
    bt: Bt,
    /// Wi-Fi leases (wifi/mod.rs); off unless something holds one.
    wifi: Wifi,
    /// Installed modules (store/), their screens and their stream (modules.rs).
    modules: modules::Host,
    /// A Cmd::Stream was sent and the player has not reported it yet.
    stream_sent: bool,
    /// The next module refresh (`every` / `refreshAt`), boot ms.
    modules_due: Option<u64>,
    output_check_at: u64,
    screen_on: bool,
    screen_off_at: u64,
    /// From persist.abyss.screen_timeout_s (see screen_timeout_ms()).
    screen_timeout_ms: u64,
    /// The volume box shows until this time (screen on only).
    volume_overlay_until: u64,
    /// One feedback line above the key guide, until the given time.
    msg: Option<(String, u32, u64)>,
    /// Headset button presses waiting to be counted (1× / 2× / 3×).
    headset_taps: u32,
    headset_deadline: Option<u64>,
    /// Volume above VOLUME_SAFE was confirmed this session.
    loud_ok: bool,
    /// The "too loud" warning shows until this time; a ▲ before then confirms.
    loud_warned_until: u64,
    /// Adaptive volume step state: quick taps in a row and the last tap.
    vol_streak: u32,
    vol_last_at: u64,
    vol_last_up: bool,
    dirty: bool,
    last_draw: u64,
    low_battery_at: Option<u64>,
    save_at: u64,
    last_activity: u64,
    /// No suspend attempt before this (set after each attempt).
    suspend_retry_at: u64,
    /// Last wake from suspend; no new suspend for POST_RESUME_AWAKE_MS after it.
    resumed_at: u64,
    suspend_enabled: bool,
    hal_restarts: Vec<u64>,
    exit: Option<Exit>,
}

/// Builds a `View` from disjoint fields so `self.ui` can be borrowed mutably.
macro_rules! view {
    ($s:ident, $now:expr) => {
        View {
            lib: &$s.lib,
            status: &$s.status,
            battery: &$s.battery,
            volume: $s.volume,
            volume_max: VOLUME_MAX,
            output: $s.output.as_ref(),
            bt: &$s.bt,
            wifi: &$s.wifi,
            modules: &$s.modules,
            resume_track: $s.resume.track.as_ref(),
            resume_pos_ms: $s.resume.position_ms,
            leaving_in: $s.keys.leave_at().map(|at| at.saturating_sub($now).div_ceil(1000)),
            low_battery_in: $s.low_battery_at.map(|at| at.saturating_sub($now).div_ceil(1000)),
            volume_overlay: $s.screen_on && $now < $s.volume_overlay_until,
            msg: $s.msg.as_ref().filter(|m| $now < m.2).map(|m| (m.0.as_str(), m.1)),
        }
    };
}

impl App {
    fn ctx(&self) -> Ctx {
        // Screen-off music keys work while something plays or is paused
        // mid-queue and an output is there; otherwise the first press wakes
        // the screen (which then says what is missing).
        let loaded = match self.status.state {
            PlayState::Playing => true,
            PlayState::Paused => self.status.queue_len > 0,
            PlayState::Stopped => false,
        };
        Ctx {
            screen_on: self.screen_on,
            player: self.ui.screen == Screen::NowPlaying,
            loaded: loaded && self.output.is_some(),
        }
    }

    fn say(&mut self, now: u64, text: impl Into<String>, color: u32) {
        self.say_for(now, text, color, MSG_MS);
    }

    fn say_for(&mut self, now: u64, text: impl Into<String>, color: u32, ms: u64) {
        self.msg = Some((text.into(), color, now + ms));
        self.dirty = true;
    }

    /// Help stays lit at least 30 s (longer if the screen timeout is longer).
    fn help_ms(&self) -> u64 {
        HELP_SCREEN_MS.max(self.screen_timeout_ms)
    }

    /// ▼ + ● short: capture what this screen shows / what plays (capture.rs).
    fn capture(&mut self, now: u64) {
        self.status = self.player.status();
        // A module stream that plays, or a module screen in front: the module's own capture.
        if let Some(r) = self.modules.capture(self.ui.screen == Screen::Module && self.screen_on) {
            let result = r.map_err(str::to_string).and_then(|mut c| {
                c.fields.insert("abyss_screen".into(), serde_json::Value::from(format!("{:?}", self.ui.screen)));
                c.fields.insert("screen_on".into(), serde_json::Value::from(self.screen_on));
                match capture::save(&c) {
                    Ok(()) => Ok(c.line),
                    Err(e) => {
                        warn!("capture: write {}: {e}", capture::CAPTURES);
                        Err(format!("Couldn't save the capture: {e}"))
                    }
                }
            });
            self.capture_feedback(now, result);
            return;
        }
        let music = match (&self.status.track, self.status.state) {
            (Some(track), PlayState::Playing | PlayState::Paused) => Some(capture::MusicNow {
                track,
                pos_ms: self.status.pos_ms,
                dur_ms: self.status.dur_ms,
                playing: self.status.state == PlayState::Playing,
                codec: self.status.codec,
            }),
            _ => None,
        };
        let scene = capture::Scene { screen: self.ui.screen, screen_on: self.screen_on, music };
        let result = match capture::capture_now(&scene) {
            None => Err("Nothing to capture here.".to_string()),
            Some(c) => match capture::save(&c) {
                Ok(()) => Ok(c.line),
                Err(e) => {
                    warn!("capture: write {}: {e}", capture::CAPTURES);
                    Err(format!("Couldn't save the capture: {e}"))
                }
            },
        };
        self.capture_feedback(now, result);
    }

    fn capture_feedback(&mut self, now: u64, result: std::result::Result<String, String>) {
        match result {
            // Screen on: a 2 s line. Screen off: one short buzz, the screen stays dark.
            Ok(line) if self.screen_on => self.say_for(now, format!("Saved: {line}"), ACCENT, CAPTURE_MSG_MS),
            Ok(_) => {
                capture::buzz();
            }
            // Failures always show (the screen wakes), so a capture is never lost silently.
            Err(m) => {
                info!("capture: {m}");
                self.say(now, m, WARN);
                self.screen_on_for(now, self.screen_timeout_ms);
            }
        }
    }

    fn screen_on_for(&mut self, now: u64, ms: u64) {
        if !self.screen_on {
            self.display.unblank();
            backlight::apply();
            self.screen_on = true;
        }
        self.screen_off_at = self.screen_off_at.max(now + ms);
        self.dirty = true;
    }

    fn screen_off(&mut self) {
        if let Err(e) = self.display.blank() {
            warn!("blank: {e:#}");
        }
        self.screen_on = false;
        self.volume_overlay_until = 0;
        if self.ui.screen == Screen::PowerOff {
            self.ui.back();
        }
    }

    fn redraw(&mut self, now: u64) {
        self.status = self.player.status();
        let v = view!(self, now);
        self.ui.draw(&mut self.display.canvas, &v);
        if let Err(e) = self.display.present() {
            warn!("present: {e:#}");
        }
        self.dirty = false;
        self.last_draw = now;
    }

    fn on_input(&mut self, ev: Input) {
        let now = clock::now_ms();
        match ev {
            Input::Key { key, down, t } => {
                self.status = self.player.status();
                let ctx = self.ctx();
                for a in self.keys.on_key(key, down, t, ctx) {
                    self.act(a, now);
                }
            }
            Input::Headphones(present) => {
                info!("jack: headphones {}", if present { "in" } else { "out" });
                self.headphones = present;
                self.update_output(now);
            }
            Input::HeadsetButton => {
                // Counted in timers(): 1× play/pause, 2× next, 3× previous.
                self.headset_taps += 1;
                self.headset_deadline = Some(now + HEADSET_TAP_MS);
            }
            Input::HeadsetVolume(up) => {
                self.act(if up { Action::VolUp } else { Action::VolDown }, now)
            }
        }
    }

    /// Re-picks the output: Bluetooth (connected) > USB DAC > 3.5 mm jack > nothing.
    /// Never the speaker. Losing Bluetooth pauses (player.rs set_output).
    fn update_output(&mut self, now: u64) {
        let new = match self.bt.connected() {
            Some(d) => Some(Output::Bluetooth { addr: d.addr.clone(), name: d.name.clone() }),
            None => wired_output(self.headphones),
        };
        if new != self.output {
            info!(
                "output: {} -> {}",
                self.output.as_ref().map(|o| o.label()).unwrap_or_else(|| "none".into()),
                new.as_ref().map(|o| o.label()).unwrap_or_else(|| "none".into())
            );
            let bt_lost = matches!(self.output, Some(Output::Bluetooth { .. }))
                && !matches!(new, Some(Output::Bluetooth { .. }));
            match &new {
                Some(o) if bt_lost => self.say(now, format!("Bluetooth gone: paused. {} is ready.", o.label()), WARN),
                Some(o) => self.say(now, format!("Playing on {}", o.label()), ACCENT),
                None if bt_lost => self.say(now, "Bluetooth gone: paused.", WARN),
                None => self.say(now, "Headphones out: paused.", WARN),
            }
            self.output = new.clone();
            self.player.send(Cmd::Output(new));
            self.dirty = true;
        }
        self.output_check_at = now + self.output_check_every();
    }

    /// /proc/asound/cards is re-read every 2 s while the screen is on or a
    /// USB DAC plays (a pulled DAC also fails the next write at once), else
    /// every 10 s so plugging one in with the screen off is noticed.
    fn output_check_every(&self) -> u64 {
        let usb_playing = matches!(self.output, Some(Output::Usb { .. }))
            && self.status.state == PlayState::Playing;
        if self.screen_on || usb_playing {
            OUTPUT_CHECK_MS
        } else {
            OUTPUT_CHECK_IDLE_MS
        }
    }

    fn act(&mut self, a: Action, now: u64) {
        match a {
            Action::Activity => {
                self.last_activity = now;
                if self.screen_on {
                    self.screen_off_at = self.screen_off_at.max(now + self.screen_timeout_ms);
                }
            }
            Action::Wake => self.screen_on_for(now, self.screen_timeout_ms),
            Action::Menu => {
                // ● ● with the screen off is a "look": wake on the screen you left
                // (usually Now Playing). Only with the screen on does it go to the menu.
                if self.screen_on && self.ui.screen == Screen::NowPlaying {
                    self.ui.back();
                }
                self.screen_on_for(now, self.screen_timeout_ms);
            }
            Action::Help => {
                self.ui.go(Screen::Help);
                self.screen_on_for(now, self.help_ms());
            }
            Action::Capture => self.capture(now),
            Action::MenuUp | Action::MenuDown | Action::Select if self.ui.screen == Screen::Module => {
                let eff = match a {
                    Action::MenuUp => {
                        self.modules.up();
                        modules::Effect::None
                    }
                    Action::MenuDown => {
                        self.modules.down();
                        modules::Effect::None
                    }
                    _ => self.modules.select(),
                };
                self.screen_on_for(now, self.screen_timeout_ms);
                self.on_module_effect(eff, now);
                self.dirty = true;
            }
            Action::MenuUp | Action::MenuDown | Action::Select => {
                let v = view!(self, now);
                let req = match a {
                    Action::MenuUp => {
                        self.ui.up(&v);
                        Request::None
                    }
                    Action::MenuDown => {
                        self.ui.down(&v);
                        Request::None
                    }
                    _ => self.ui.select(&v),
                };
                let ms = if self.ui.screen == Screen::Help { self.help_ms() } else { self.screen_timeout_ms };
                self.screen_on_for(now, ms);
                self.request(req, now);
            }
            Action::VolUp | Action::VolDown => {
                if a == Action::VolUp && self.volume >= VOLUME_SAFE && !self.loud_ok {
                    // At the safe limit: the first ▲ only warns, a second ▲ while the
                    // warning shows unlocks the louder steps for this session.
                    if self.loud_warned_until > now {
                        self.loud_ok = true;
                    } else {
                        self.loud_warned_until = now + MSG_MS;
                        self.say(now, "Above 140 of 200 is loud. Press ▲ again to go louder.", ui::WARN);
                        return;
                    }
                }
                // Adaptive step: quick repeated taps in the same direction speed up,
                // 1 → 2 → 3 → 4 steps per tap (every 3 quick taps), back to 1 after a pause.
                let up = a == Action::VolUp;
                if up == self.vol_last_up && now.saturating_sub(self.vol_last_at) <= VOLUME_SPAM_MS {
                    self.vol_streak += 1;
                } else {
                    self.vol_streak = 0;
                }
                self.vol_last_at = now;
                self.vol_last_up = up;
                let step = (VOLUME_STEP + self.vol_streak / 3).min(VOLUME_STEP_MAX);
                self.volume = if up {
                    let mut v = (self.volume + step).min(VOLUME_MAX);
                    // A fast streak never jumps past the safe mark without the confirm.
                    if !self.loud_ok && self.volume <= VOLUME_SAFE {
                        v = v.min(VOLUME_SAFE);
                    }
                    v
                } else {
                    self.volume.saturating_sub(step)
                };
                info!("volume {}/{}", self.volume, VOLUME_MAX);
                self.player.send(Cmd::Volume(self.volume));
                self.resume.volume = self.volume;
                // Screen on: the volume box for 2 s. Screen off: stay dark.
                if self.screen_on {
                    self.volume_overlay_until = now + VOLUME_OVERLAY_MS;
                    self.screen_off_at = self.screen_off_at.max(now + self.screen_timeout_ms);
                    self.dirty = true;
                }
            }
            Action::Next => self.player.send(Cmd::Next),
            Action::Prev => self.player.send(Cmd::Prev),
            Action::PlayPause => {
                self.player.send(Cmd::Toggle);
                if self.output.is_none() && self.screen_on {
                    self.say(now, "No headphones: plug in wired or USB ones, or connect Bluetooth ›.", WARN);
                }
            }
            Action::LeaveArm => {
                info!("● held 3 s: back to Android in 3 s unless cancelled");
                self.screen_on_for(now, keys::LEAVE_CANCEL_MS + self.screen_timeout_ms);
            }
            Action::LeaveCancel => {
                info!("leave cancelled");
                self.say(now, "Cancelled. You are still in Abyss.", TEXT);
                self.screen_on_for(now, self.screen_timeout_ms);
            }
            Action::Leave => self.exit = Some(Exit::Leave),
        }
    }

    fn load(&mut self, queue: Vec<PathBuf>, index: usize, start_ms: u64, now: u64) {
        if queue.is_empty() {
            return;
        }
        // Local music takes the player from a module stream.
        self.modules.stop_stream();
        self.stream_sent = false;
        self.player.send(Cmd::Load { queue, index, start_ms, play: true });
        // Playing something always shows the player.
        self.ui.go(Screen::NowPlaying);
        self.screen_on_for(now, self.screen_timeout_ms);
    }

    fn request(&mut self, req: Request, now: u64) {
        match req {
            Request::None => {}
            Request::Resume => {
                let Some(track) = self.resume.track.clone() else {
                    self.say(now, "Nothing to resume yet.", DIM);
                    return;
                };
                // That track is already loaded (playing or paused): never reload or seek.
                // Just make sure it plays and show the player.
                if self.status.track.as_ref() == Some(&track)
                    && self.status.state != PlayState::Stopped
                {
                    if self.status.state == PlayState::Paused {
                        self.player.send(Cmd::Play);
                    }
                    self.ui.go(Screen::NowPlaying);
                    self.screen_on_for(now, self.screen_timeout_ms);
                    return;
                }
                let start = self.resume.position_ms;
                match self.lib.locate(&track) {
                    Some((folder, index)) => {
                        let q = self.lib.folders[folder]
                            .tracks
                            .iter()
                            .map(|t| t.path.clone())
                            .collect();
                        self.load(q, index, start, now);
                    }
                    None if track.exists() => self.load(vec![track], 0, start, now),
                    None => {
                        warn!("resume track gone: {}", track.display());
                        self.say(now, "The last song is gone from the card.", WARN);
                    }
                }
            }
            Request::ShuffleAll => {
                let mut q = self.lib.all_tracks();
                shuffle(&mut q, now);
                self.resume.shuffle = true;
                self.load(q, 0, 0, now);
            }
            Request::PlayFolder(id) => {
                let q = self.lib.tracks_below(id);
                self.resume.shuffle = false;
                self.load(q, 0, 0, now);
            }
            Request::PlayTrack { folder, index } => {
                let q = self.lib.folders[folder].tracks.iter().map(|t| t.path.clone()).collect();
                self.resume.shuffle = false;
                self.load(q, index, 0, now);
            }
            Request::PlayAlbum { album, index } => {
                let q = self.lib.albums.get(album).map(|a| a.tracks.clone()).unwrap_or_default();
                self.resume.shuffle = false;
                self.load(q, index, 0, now);
            }
            Request::PlayArtist(id) => {
                let q = self.lib.artist_tracks(id);
                self.resume.shuffle = false;
                self.load(q, 0, 0, now);
            }
            // Bluetooth stays inside Abyss (abyss-btd); no leave, no Abyss Light.
            Request::BtOn => self.bt.turn_on(now),
            Request::BtOff => self.bt.turn_off(now),
            Request::BtConnect(i) => {
                let dev = i.and_then(|i| self.bt.device(i));
                self.bt.connect(dev, now);
            }
            Request::BtDisconnect => self.bt.disconnect(),
            Request::BtRefresh => self.bt.refresh(),
            Request::WifiTest => {
                if self.wifi.test_running() {
                    self.say(now, "Wi-Fi test already running…", DIM);
                } else {
                    self.wifi.run_test();
                    self.say_for(now, "Wi-Fi test: connecting…", TEXT, 20_000);
                    // Keep the screen up for the result (a cold start takes a few seconds).
                    self.screen_on_for(now, 30_000);
                }
            }
            Request::Leave => {
                // Same 3 s cancel window as the ● hold.
                self.keys.arm_leave(now);
                self.screen_on_for(now, keys::LEAVE_CANCEL_MS + self.screen_timeout_ms);
            }
            Request::PowerOff => self.exit = Some(Exit::PowerOff),
            Request::OpenApps => {
                self.modules.open_apps();
                self.ui.go(Screen::Module);
                self.screen_on_for(now, self.screen_timeout_ms);
            }
        }
    }

    /// What a module selection asks for (modules.rs Effect).
    fn on_module_effect(&mut self, eff: modules::Effect, now: u64) {
        use modules::Effect;
        match eff {
            Effect::None => {}
            Effect::Exit => self.ui.back(),
            Effect::Play(feed) => {
                self.stream_sent = true;
                self.player.send(Cmd::Stream(feed));
                self.ui.go(Screen::NowPlaying);
                self.screen_on_for(now, self.screen_timeout_ms);
                if self.output.is_none() {
                    self.say(now, "No headphones: plug in wired or USB ones, or connect Bluetooth ›.", WARN);
                }
            }
            Effect::Media(op) => match op.as_str() {
                "play" => self.player.send(Cmd::Play),
                "pause" => self.player.send(Cmd::Pause),
                "toggle" => self.player.send(Cmd::Toggle),
                "next" => self.player.send(Cmd::Next),
                "prev" => self.player.send(Cmd::Prev),
                _ => self.say(now, "This needs a newer IchthysOS.", WARN),
            },
            Effect::Show(t) => self.say(now, t, TEXT),
            Effect::Buzz(pattern) => {
                // One tick for now; patterns come with the bell's vibrator work.
                info!("module buzz '{pattern}'");
                capture::buzz();
            }
            Effect::Battery => {
                let pct = self.battery.capacity.map(|c| format!("Battery {c}%")).unwrap_or_else(|| "Battery ?".into());
                self.say(now, pct, TEXT);
            }
            Effect::Unsupported(t) => self.say(now, t, WARN),
        }
    }

    /// Module fetch results / stream notes arrived.
    fn on_modules(&mut self, now: u64) {
        let (changed, notes) = self.modules.on_ready();
        if changed {
            self.dirty = true;
        }
        for n in notes {
            match n {
                radio_source::Note::Saved(line) => {
                    if self.screen_on {
                        self.say_for(now, format!("Saved: {line}"), ACCENT, CAPTURE_MSG_MS);
                    } else {
                        capture::buzz();
                    }
                }
                radio_source::Note::NotSaved(why) => self.say(now, why, WARN),
            }
        }
    }

    /// init re-set `sys.abyss.hal=restarted`: the vendor audio HAL restarted.
    fn on_hal_restarted(&mut self, now: u64) {
        self.hal_restarts.retain(|&t| now.saturating_sub(t) <= HAL_RESTART_WINDOW_MS);
        self.hal_restarts.push(now);
        warn!("audio HAL restarted ({} in the last 60 s)", self.hal_restarts.len());
        if self.hal_restarts.len() > HAL_RESTART_MAX {
            error!("audio HAL keeps restarting: going back to Android");
            self.exit = Some(Exit::Leave);
            return;
        }
        self.player.send(Cmd::HalRestarted);
    }

    /// Player state changed: refresh, save resume state on transitions.
    fn on_player(&mut self) {
        let old = std::mem::take(&mut self.status);
        self.status = self.player.status();
        // The stream left the player (it ended, or local music replaced it): stop fetching.
        if self.status.stream {
            self.stream_sent = false;
        } else if !self.stream_sent && self.modules.streaming() {
            self.modules.stop_stream();
        }
        self.modules.set_paused(self.status.stream && self.status.state != PlayState::Playing);
        if old.track != self.status.track || old.state != self.status.state {
            self.save_resume();
            self.send_bt_playstate();
        }
        self.dirty = true;
    }

    /// The headset's AVRCP view of what plays (title, state).
    fn send_bt_playstate(&mut self) {
        let st = match self.status.state {
            PlayState::Playing => "playing",
            PlayState::Paused => "paused",
            PlayState::Stopped => "stopped",
        };
        let title = self.status.track.as_ref().map(|t| self.lib.title_of(t)).unwrap_or_default();
        self.bt.send_playstate(st, &title, self.status.pos_ms, self.status.dur_ms);
    }

    /// Notes from the Bluetooth side (events and timers).
    fn on_bt(&mut self, notes: Vec<bt::Note>, now: u64) {
        for n in notes {
            match n {
                bt::Note::Output => {
                    self.update_output(now);
                    self.status = self.player.status();
                    self.send_bt_playstate();
                }
                bt::Note::Say(text, warn) => self.say(now, text, if warn { WARN } else { TEXT }),
                bt::Note::Key(k) => self.on_bt_key(&k, now),
                bt::Note::Volume(abs) => {
                    // abyssd owns the volume: the headset's own change becomes one
                    // step (same path as the keys, with the safe limit), then the
                    // headset goes back to full scale.
                    // 127 is our own "full scale" coming back (or the headset's
                    // max): ignore it, or each echo would step the volume up.
                    // Up presses at full scale arrive as the vol_up key instead.
                    info!("bt: headset volume {abs}/127");
                    if abs < 127 {
                        self.act(Action::VolDown, now);
                        self.bt.send_full_volume();
                    }
                }
            }
        }
        self.dirty = true;
    }

    /// Notes from the Wi-Fi worker (bring-up failures, test results).
    fn on_wifi(&mut self, notes: Vec<wifi::Note>, now: u64) {
        for n in notes {
            match n {
                wifi::Note::Say(text, warn) => {
                    self.say_for(now, text, if warn { WARN } else { ACCENT }, 8_000);
                }
            }
        }
        self.dirty = true;
    }

    /// AVRCP keys = the headset button's actions.
    fn on_bt_key(&mut self, k: &str, now: u64) {
        info!("bt: key {k}");
        self.status = self.player.status();
        let playing = self.status.state == PlayState::Playing;
        self.last_activity = now;
        match k {
            // One-button headsets send PLAY whatever the state: toggle then.
            "play" if playing => self.player.send(Cmd::Pause),
            "play" => self.player.send(Cmd::Play),
            "pause" | "stop" => {
                if playing {
                    self.player.send(Cmd::Pause)
                }
            }
            "play_pause" => self.player.send(Cmd::Toggle),
            "next" => self.player.send(Cmd::Next),
            "prev" => self.player.send(Cmd::Prev),
            "vol_up" => self.act(Action::VolUp, now),
            "vol_down" => self.act(Action::VolDown, now),
            _ => warn!("bt: unknown key {k:?}"),
        }
    }

    fn save_resume(&mut self) {
        if self.status.track.is_some() {
            self.resume.track = self.status.track.clone();
            self.resume.position_ms = self.status.pos_ms;
        }
        self.resume.volume = self.volume;
        if let Err(e) = self.resume.save() {
            warn!("save state: {e:#}");
        }
    }

    /// Time-driven work. Called on every loop iteration.
    fn timers(&mut self, now: u64) {
        let ctx = self.ctx();
        for a in self.keys.tick(now, ctx) {
            self.act(a, now);
        }
        if self.keys.leave_pending() {
            self.dirty = true; // countdown digits
        }
        if let Some(at) = self.headset_deadline {
            if now >= at {
                let n = std::mem::take(&mut self.headset_taps);
                self.headset_deadline = None;
                info!("headset button {n}×");
                self.player.send(match n {
                    1 => Cmd::Toggle,
                    2 => Cmd::Next,
                    _ => Cmd::Prev,
                });
            }
        }
        if now >= self.output_check_at {
            self.update_output(now);
        }
        let visible = match self.ui.screen {
            Screen::Module => modules::Visible::Module,
            Screen::NowPlaying if self.status.stream => modules::Visible::NowPlaying,
            _ => modules::Visible::Other,
        };
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.modules_due = self.modules.tick(self.screen_on, visible).map(|due| now + (due - wall).max(0) as u64);
        let notes = self.bt.tick(now);
        if !notes.is_empty() || self.bt.next_deadline().is_some_and(|t| t <= now) {
            self.dirty = true;
        }
        if !notes.is_empty() {
            self.on_bt(notes, now);
        }
        if self.volume_overlay_until != 0 && now >= self.volume_overlay_until {
            self.volume_overlay_until = 0;
            self.dirty = true;
        }
        if self.msg.as_ref().is_some_and(|m| now >= m.2) {
            self.msg = None;
            self.dirty = true;
        }
        if self.screen_on
            && !self.keys.leave_pending()
            && !self.keys.any_down()
            && now >= self.screen_off_at
        {
            self.screen_off();
        }
        if now >= self.battery_at {
            self.battery = Battery::read();
            self.modules.battery = serde_json::json!({ "pct": self.battery.capacity });
            self.battery_at = now + BATTERY_EVERY_MS;
            self.dirty = true;
            self.check_low_battery(now);
        }
        if let Some(at) = self.low_battery_at {
            if now >= at {
                warn!("battery at reserve: powering off");
                self.exit = Some(Exit::PowerOff);
            }
        }
        if self.status.state == PlayState::Playing && now >= self.save_at {
            self.status = self.player.status();
            self.save_resume();
            self.save_at = now + SAVE_EVERY_MS;
        }
        if self.screen_on && now >= self.last_draw + 1000 {
            let v = view!(self, now);
            if self.ui.is_live(&v) {
                self.dirty = true;
            }
        }
    }

    fn check_low_battery(&mut self, now: u64) {
        let low =
            !self.battery.charging() && self.battery.capacity.is_some_and(|c| c <= LOW_BATTERY_PCT);
        match (low, self.low_battery_at) {
            (true, None) => {
                warn!("battery {:?} %: power off in 60 s", self.battery.capacity);
                self.low_battery_at = Some(now + LOW_BATTERY_WARN_MS);
                self.screen_on_for(now, LOW_BATTERY_WARN_MS);
            }
            (false, Some(_)) => self.low_battery_at = None,
            _ => {}
        }
    }

    /// The next time `timers()` has something to do.
    fn next_wake(&self, now: u64) -> u64 {
        let mut t = self.battery_at.min(self.output_check_at);
        if let Some(k) = self.keys.next_deadline(self.ctx()) {
            t = t.min(k);
        }
        if let Some(h) = self.headset_deadline {
            t = t.min(h);
        }
        if let Some(b) = self.bt.next_deadline() {
            t = t.min(b);
        }
        if let Some(m) = self.modules_due {
            t = t.min(m);
        }
        if self.screen_on {
            t = t.min(self.screen_off_at);
            // Live screens (playing Now playing, countdowns) tick once a second.
            t = t.min(self.last_draw + 1000);
            if self.volume_overlay_until != 0 {
                t = t.min(self.volume_overlay_until);
            }
            if let Some(m) = &self.msg {
                t = t.min(m.2);
            }
        }
        if let Some(at) = self.low_battery_at {
            t = t.min(at);
        }
        if self.status.state == PlayState::Playing {
            t = t.min(self.save_at);
        }
        // Wake up for the next suspend attempt (only when it lies ahead, so a
        // refused suspend cannot spin).
        if self.suspend_enabled && !self.screen_on {
            let at = self.suspend_retry_at.max(self.last_activity + IDLE_BEFORE_SUSPEND_MS);
            if at > now {
                t = t.min(at);
            }
        }
        t.max(now)
    }

    /// Suspend is allowed while music plays only when the DSP decodes it and
    /// the player thread is just waiting for the DSP's next write-done
    /// interrupt (a wakeup source: glink IPCC IRQ). CPU-decode playback never
    /// suspends; Bluetooth playback is always CPU decode, so it never suspends
    /// either. Nor while Bluetooth is turning on/off or connecting.
    fn may_suspend(&self, now: u64) -> bool {
        let bt_playing = matches!(self.output, Some(Output::Bluetooth { .. }))
            && self.status.state == PlayState::Playing;
        self.suspend_enabled
            && !self.screen_on
            && now >= self.suspend_retry_at
            && now >= self.resumed_at + POST_RESUME_AWAKE_MS
            && !bt_playing
            && !self.bt.busy()
            && !self.wifi.busy()
            && (self.status.state != PlayState::Playing || self.player.offload_sleepable())
            && !self.keys.any_down()
            && self.low_battery_at.is_none()
            && self.headset_deadline.is_none()
            && now.saturating_sub(self.last_activity) >= IDLE_BEFORE_SUSPEND_MS
    }
}

/// persist.abyss.screen_timeout_s in ms (default 15 s, clamped to 5..120 s).
fn screen_timeout_ms() -> u64 {
    let s = control::prop_str(PROP_SCREEN_TIMEOUT)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(SCREEN_TIMEOUT_DEFAULT_S);
    s.clamp(SCREEN_TIMEOUT_MIN_S, SCREEN_TIMEOUT_MAX_S) * 1000
}

/// USB DAC > 3.5 mm jack > nothing (the wired part of the output order).
fn wired_output(headphones: bool) -> Option<Output> {
    match audio::usb_cards().into_iter().next() {
        Some((card, name)) => Some(Output::Usb { card, name }),
        None if headphones => Some(Output::Jack),
        None => None,
    }
}

/// Fisher-Yates with xorshift64 (no rand crate needed for a playlist).
fn shuffle<T>(v: &mut [T], seed: u64) {
    let mut x = seed ^ 0x9E37_79B9_7F4A_7C15;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

fn drain(s: &mut UnixStream) {
    let mut buf = [0u8; 64];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}

/// Watches `sys.abyss.hal` on a helper thread (property waits block) and
/// pokes the main loop with one byte per change.
fn spawn_hal_watch(mut tx: UnixStream) {
    let r = std::thread::Builder::new().name("abyssd-halwatch".into()).spawn(move || {
        let Ok(mut w) =
            rustutils::android::system_properties::PropertyWatcher::new("sys.abyss.hal")
        else {
            return;
        };
        // Sync to the current serial first so a value left from an earlier
        // session does not count: the first wait only finds the property,
        // the second records its serial (both return at once).
        let _ = w.wait(Some(std::time::Duration::ZERO));
        let _ = w.wait(Some(std::time::Duration::ZERO));
        loop {
            if w.wait(None).is_err() {
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
            if w.read_string().ok().flatten().as_deref() == Some("restarted")
                && tx.write_all(&[1]).is_err()
            {
                return;
            }
        }
    });
    if let Err(e) = r {
        warn!("hal watch thread: {e}");
    }
}

/// Opens /dev/dri/card0 and pushes a black frame. Without CAP_SYS_ADMIN only
/// the first opener after the composer exits is DRM master, so retry with
/// backoff for about 10 s (the composer may still be shutting down).
/// Ok(None) = SIGTERM arrived while waiting.
fn open_display(sig_r: &mut UnixStream) -> Result<Option<Display>> {
    let start = clock::now_ms();
    let mut delay_ms = 50;
    loop {
        let attempt = Display::open("/dev/dri/card0").and_then(|mut d| {
            d.unblank();
            d.canvas.clear(ui::BG);
            d.present().map(|_| d)
        });
        match attempt {
            Ok(d) => {
                info!("display ready after {} ms", clock::now_ms() - start);
                return Ok(Some(d));
            }
            Err(e) if clock::now_ms().saturating_sub(start) < 10_000 => {
                warn!("display not ready ({e:#}); retrying in {delay_ms} ms");
            }
            Err(e) => return Err(e).context("no DRM master after 10 s"),
        }
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        delay_ms = (delay_ms * 2).min(1000);
        let mut b = [0u8; 8];
        if matches!(sig_r.read(&mut b), Ok(n) if n > 0) {
            return Ok(None);
        }
    }
}

fn splash(display: &mut Display, line: &str) {
    display.canvas.clear(ui::BG);
    font::BODY_BOLD.draw(&mut display.canvas, 64, 1000, "≈ ICHTHYS ABYSS", ACCENT);
    font::BODY.draw(&mut display.canvas, 64, 1100, line, DIM);
    if let Err(e) = display.present() {
        warn!("present: {e:#}");
    }
}

fn run() -> Result<Exit> {
    // SIGTERM/SIGINT → one byte on a socket pair, polled with everything else.
    let (mut sig_r, sig_w) = UnixStream::pair().context("signal pipe")?;
    sig_r.set_nonblocking(true)?;
    sig_w.set_nonblocking(true)?;
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGTERM, sig_w.try_clone()?)?;
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGINT, sig_w)?;

    let Some(mut display) = open_display(&mut sig_r)? else {
        info!("SIGTERM while waiting for the display");
        return Ok(Exit::Term);
    };
    // The first (black) frame is up: ask init for our backlight level.
    backlight::apply();
    splash(&mut display, "Looking for music…");

    let inputs = Inputs::open().context("input")?;
    let lib = Library::scan();
    let resume = ResumeState::load();
    let volume = resume.volume.min(VOLUME_MAX);
    let headphones = inputs.headphones_present();
    // Bluetooth is off at start (off by default every session).
    let output = wired_output(headphones);
    let bt = Bt::new().context("bluetooth")?;
    let wifi = Wifi::new().context("wifi")?;
    // Modules reach the network only through Wi-Fi leases, on sockets bound to wlan0.
    let net: Arc<dyn radio_source::Network> = Arc::new(wifi_net::WifiNet::new(wifi.leaser()));
    let modules = modules::Host::from_device(net).context("modules")?;
    info!(
        "jack at start: {headphones}; output {}; volume {volume} %",
        output.as_ref().map(|o| o.label()).unwrap_or_else(|| "none".into())
    );

    let (mut notify_r, notify_w) = UnixStream::pair().context("player notify")?;
    notify_r.set_nonblocking(true)?;
    let player =
        Player::spawn(audio::jack_backend(), notify_w, volume, output.clone(), bt.shared.clone())?;
    info!("audio backend: {}", player.status().backend);
    let (mut hal_r, hal_w) = UnixStream::pair().context("hal watch")?;
    hal_r.set_nonblocking(true)?;
    spawn_hal_watch(hal_w);

    let screen_timeout_ms = screen_timeout_ms();
    info!("screen timeout {} s", screen_timeout_ms / 1000);
    let now = clock::now_ms();
    let mut app = App {
        display,
        inputs,
        keys: KeyMachine::new(),
        ui: Ui::new(),
        lib,
        player,
        status: Status::default(),
        battery: Battery::read(),
        battery_at: now + BATTERY_EVERY_MS,
        resume,
        volume,
        headphones,
        output,
        bt,
        wifi,
        modules,
        stream_sent: false,
        modules_due: None,
        output_check_at: now + OUTPUT_CHECK_MS,
        screen_on: true,
        screen_off_at: now + screen_timeout_ms,
        screen_timeout_ms,
        volume_overlay_until: 0,
        msg: None,
        headset_taps: 0,
        headset_deadline: None,
        loud_ok: false,
        loud_warned_until: 0,
        vol_streak: 0,
        vol_last_at: 0,
        vol_last_up: true,
        dirty: true,
        last_draw: 0,
        low_battery_at: None,
        save_at: now + SAVE_EVERY_MS,
        last_activity: now,
        suspend_retry_at: 0,
        resumed_at: 0,
        // Deep sleep stays opt-in (persist.abyss.suspend=1) until it is proven on
        // the phone: on 2026-11-07 the first run left Abyss unresponsive to keys.
        suspend_enabled: control::prop_bool(control::PROP_SUSPEND, false),
        hal_restarts: Vec::new(),
        exit: None,
    };
    if app.lib.is_empty() {
        warn!("no music found under /data/media/0/Music or /mnt/media_rw/*");
    }
    app.check_low_battery(now);

    let mut events = Vec::new();
    loop {
        let now = clock::now_ms();
        app.timers(now);
        if let Some(e) = app.exit {
            return finish(app, e);
        }
        if app.dirty && app.screen_on {
            app.redraw(now);
        }
        if app.may_suspend(now) {
            info!("suspend: entering (screen off, idle {} ms)", now.saturating_sub(app.last_activity));
            let slept = suspend::try_suspend();
            let woke = clock::now_ms();
            info!("suspend: {} after {} ms", if slept { "woke" } else { "aborted" }, woke.saturating_sub(now));
            // Stay awake a moment after every wake so the key (or refill) that woke us
            // is read and handled before the next suspend attempt.
            app.resumed_at = woke;
            // Don't retry in a tight loop. While the DSP plays, the next try
            // comes right after the refill the resume was for.
            let playing = app.player.status().state == PlayState::Playing;
            app.suspend_retry_at = clock::now_ms()
                + if playing { OFFLOAD_SUSPEND_RETRY_MS } else { IDLE_BEFORE_SUSPEND_MS };
            continue;
        }

        let timeout_ms = app.next_wake(now).saturating_sub(now);
        let n_inputs = app.inputs.len();
        const FIXED: usize = 7; // signal, player, hal watch, bluetooth, wifi, modules, module stream
        let mut ready = vec![false; FIXED + n_inputs];
        {
            let mut fds = Vec::with_capacity(FIXED + n_inputs);
            fds.push(PollFd::new(sig_r.as_fd(), PollFlags::POLLIN));
            fds.push(PollFd::new(notify_r.as_fd(), PollFlags::POLLIN));
            fds.push(PollFd::new(hal_r.as_fd(), PollFlags::POLLIN));
            fds.push(PollFd::new(app.bt.poll_fd(), PollFlags::POLLIN));
            fds.push(PollFd::new(app.wifi.poll_fd(), PollFlags::POLLIN));
            fds.push(PollFd::new(app.modules.poll_fd(), PollFlags::POLLIN));
            fds.push(PollFd::new(app.modules.stream_fd().unwrap_or(app.modules.poll_fd()), PollFlags::POLLIN));
            for fd in app.inputs.fds() {
                fds.push(PollFd::new(fd, PollFlags::POLLIN));
            }
            let timeout = PollTimeout::try_from(timeout_ms).unwrap_or(PollTimeout::MAX);
            match poll(&mut fds, timeout) {
                Ok(_) => {}
                Err(Errno::EINTR) => continue,
                Err(e) => return Err(e).context("poll"),
            }
            for (i, fd) in fds.iter().enumerate() {
                ready[i] = fd.revents().is_some_and(|r| !r.is_empty());
            }
        }

        if ready[0] {
            drain(&mut sig_r);
            info!("SIGTERM/SIGINT");
            return finish(app, Exit::Term);
        }
        if ready[1] {
            drain(&mut notify_r);
            app.on_player();
        }
        if ready[2] {
            drain(&mut hal_r);
            app.on_hal_restarted(clock::now_ms());
        }
        if ready[3] {
            let now = clock::now_ms();
            let notes = app.bt.on_ready(now);
            app.on_bt(notes, now);
        }
        if ready[4] {
            let notes = app.wifi.on_ready();
            app.on_wifi(notes, clock::now_ms());
        }
        if ready[5] || ready[6] {
            app.on_modules(clock::now_ms());
        }
        for i in 0..n_inputs {
            if ready[FIXED + i] {
                events.clear();
                app.inputs.read(i, &mut events);
                for ev in events.drain(..) {
                    app.on_input(ev);
                }
            }
        }
    }
}

/// Stops audio, saves state, then hands control back to init.
fn finish(mut app: App, how: Exit) -> Result<Exit> {
    app.status = app.player.status();
    app.save_resume();
    // init SIGKILLs 200 ms after SIGTERM (gentle_kill): keep the whole exit
    // path well inside that.
    app.modules.stop_stream();
    app.player.shutdown(120);
    app.wifi.shutdown(if how == Exit::Term { WIFI_STOP_TERM_MS } else { WIFI_STOP_MS });
    let _ = app.display.blank();
    // Android's backlight level is restored by init (abyss.rc) on leave; init also
    // stops abyss-btd (before zygote) and resets sys.abyss.bt.
    drop(app); // releases DRM master before Android's composer starts
    match how {
        Exit::Leave => control::request_leave("user"),
        Exit::PowerOff => control::power_off(),
        Exit::Term => {}
    }
    Ok(how)
}

fn main() {
    // `abyssd --net-helper`: the abyss-net service (uid system), netd configuration for Wi-Fi
    // leases (wifi/netcfg.rs). No display, no session log.
    if std::env::args().nth(1).as_deref() == Some("--net-helper") {
        std::process::exit(wifi::netcfg::helper_main());
    }
    // Files abyssd creates stay group-readable for system (see abyss.rc).
    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o007));
    init_logging();
    info!("abyssd v1-lite r2 starting (pid {})", std::process::id());
    // A panic must not leave the phone stuck in Abyss.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |p| {
        error!("panic: {p}");
        control::request_leave("abyssd panic");
        default_hook(p);
    }));
    match run() {
        Ok(how) => {
            info!("exit: {how:?}");
            std::process::exit(0);
        }
        Err(e) => {
            error!("fatal: {e:#}");
            // Crash guard: never leave the user on a dead black phone.
            control::request_leave("abyssd error");
            std::process::exit(1);
        }
    }
}
