//! The key grammar (DECISIONS "Added 18:58"; fixed defaults, chords in v1.1).
//!
//! Music keys: on the Now playing screen, and with the screen off while a
//! queue is loaded:
//!   tap ▲ / tap ▼        volume up / down (on release, < 600 ms)
//!   hold ▲ ≥ 600 ms      next track (fires once while held)
//!   hold ▼ ≥ 600 ms      previous track
//!   ●                    play / pause (after the 400 ms double-press window)
//!   ● ●                  the menu (wakes the screen if it is off)
//!   ▲ + ▼ (short)        play / pause (as on the Music board)
//! Menus (screen on, any other screen):
//!   ▲ ▼ move (on press), ● select (on release, so a long ● hold never selects)
//!   hold ▲ / ▼           keeps moving (after 600 ms, one row every 150 ms, up to 5 s)
//!   ▲ + ▼ (short)        Back one level (the cursor does not move first)
//! Screen off, nothing loaded: the first press of any key only wakes the screen.
//! Always:
//!   hold ▲ + ▼ 2 s       Help
//!   hold ● 3 s           "Back to Android in 3 s, any key cancels", then leave
//!   key held > 5 s       ignored until released (except the ● hold above)
//!   ▼ + ● short          Capture (the screenshot chord): both down within
//!                        300 ms of each other, first release within 1.5 s of
//!                        the first press. Fires on that release.
//!   ▼ + ● longer         never acted on (Samsung force restart = ▼+● ~7 s;
//!                        DECISIONS: never bind ▼+● holds)
//!
//! The machine is pure: it takes key edges and the current time and returns
//! actions; `next_deadline()` tells the event loop when to call `tick()`.

use crate::input::Key;

pub const HOLD_MS: u64 = 600;
pub const DOUBLE_MS: u64 = 400;
pub const LEAVE_HOLD_MS: u64 = 3000;
pub const LEAVE_CANCEL_MS: u64 = 3000;
pub const IGNORE_HELD_MS: u64 = 5000;
pub const HELP_HOLD_MS: u64 = 2000;
/// Capture chord: ▼ and ● must go down at most this far apart ...
pub const CAPTURE_GAP_MS: u64 = 300;
/// ... and the first of them must come up within this time of the first press.
pub const CAPTURE_RELEASE_MS: u64 = 1500;
/// Menus: a held ▲ / ▼ moves again every this often (the first repeat comes at HOLD_MS).
pub const REPEAT_MS: u64 = 150;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Turn the screen on (no other effect).
    Wake,
    MenuUp,
    MenuDown,
    Select,
    VolUp,
    VolDown,
    Next,
    Prev,
    PlayPause,
    /// ● ● on Now playing or with the screen off: open the menu.
    Menu,
    /// ▲ + ▼ short in a menu: one level back (the Back row without going to it).
    Back,
    /// ▲ + ▼ held 2 s.
    Help,
    /// ● held 3 s: show the cancel banner (screen on).
    LeaveArm,
    /// A key was pressed during the cancel window.
    LeaveCancel,
    /// Cancel window ran out: go back to Android now.
    Leave,
    /// ▼ + ● short (the screenshot chord): capture what this screen shows
    /// (capture.rs decides what that means).
    Capture,
    /// Any key edge: resets the screen-off timer.
    Activity,
}

/// What the machine needs to know about the rest of the daemon.
#[derive(Clone, Copy, Debug)]
pub struct Ctx {
    pub screen_on: bool,
    /// The Now playing screen is showing.
    pub player: bool,
    /// A queue is loaded (playing or paused).
    pub loaded: bool,
}

impl Ctx {
    /// ▲ ▼ ● are music keys (not menu keys).
    fn music(self) -> bool {
        if self.screen_on {
            self.player
        } else {
            self.loaded
        }
    }
}

#[derive(Clone, Copy, Default)]
struct KeyState {
    down: bool,
    since: u64,
    /// This press must not produce any further action (woke the screen, was
    /// part of a combo, moved the menu, ...).
    consumed: bool,
    /// The hold action (next/prev/leave-arm) already fired for this press.
    hold_fired: bool,
    /// This press cancelled "back to Android": it can't start a capture chord.
    no_chord: bool,
    /// This press moved the menu cursor (MenuUp/MenuDown on press).
    moved: bool,
    /// Menus: the next auto-repeat move of this held key (0 = none).
    repeat_at: u64,
}

/// A ▼ + ● press pair in progress.
#[derive(Clone, Copy)]
struct Chord {
    /// When the first of the two keys went down.
    first: u64,
    /// Still a valid capture (not too far apart, not three keys, not decided).
    ok: bool,
}

#[derive(Default)]
pub struct KeyMachine {
    st: [KeyState; 3],
    /// ▲ and ▼ were down together during the current presses.
    combo_ud: bool,
    combo_since: u64,
    combo_done: bool,
    /// The ▲ + ▼ press began in a menu (its first key moved the cursor, which was undone):
    /// a short one is Back.
    combo_menu: bool,
    /// Time ● was released after a short music-mode press (waiting for a 2nd).
    power_tap: Option<u64>,
    /// While Some, the "back to Android" cancel window ends at this time.
    leave_at: Option<u64>,
    /// The release of the current ● press must be ignored (second half of
    /// ● ●, or the press that woke the screen).
    skip_power_release: bool,
    /// ▼ and ● were down together during the current presses.
    chord: Option<Chord>,
}

impl KeyMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn leave_pending(&self) -> bool {
        self.leave_at.is_some()
    }

    /// End of the running "back to Android" cancel window, if any.
    pub fn leave_at(&self) -> Option<u64> {
        self.leave_at
    }

    /// Starts the 3 s cancel window from the menu row "Back to Android".
    pub fn arm_leave(&mut self, now: u64) {
        self.leave_at = Some(now + LEAVE_CANCEL_MS);
    }

    pub fn any_down(&self) -> bool {
        self.st.iter().any(|s| s.down)
    }

    pub fn on_key(&mut self, key: Key, down: bool, t: u64, ctx: Ctx) -> Vec<Action> {
        let mut out = vec![Action::Activity];
        if down {
            self.press(key, t, ctx, &mut out);
        } else {
            self.release(key, t, ctx, &mut out);
        }
        out
    }

    fn press(&mut self, key: Key, t: u64, ctx: Ctx, out: &mut Vec<Action>) {
        let i = key.idx();
        self.st[i] = KeyState { down: true, since: t, ..KeyState::default() };

        // Any press during the cancel window cancels going back.
        if self.leave_at.take().is_some() {
            self.st[i].consumed = true;
            self.st[i].no_chord = true;
            if key == Key::Power {
                self.skip_power_release = true;
            }
            out.push(Action::LeaveCancel);
            return;
        }

        let up = self.st[Key::Up.idx()].down;
        let dn = self.st[Key::Down.idx()].down;
        let pw = self.st[Key::Power.idx()].down;

        // ▼ + ●: the capture chord when short, else Samsung's force restart.
        // Both presses are poisoned (no volume / prev / play-pause / leave hold);
        // release() decides the capture, holds never act.
        if dn && pw {
            self.st[Key::Down.idx()].consumed = true;
            self.st[Key::Power.idx()].consumed = true;
            self.st[Key::Up.idx()].repeat_at = 0;
            self.st[Key::Down.idx()].repeat_at = 0;
            self.skip_power_release = true;
            self.power_tap = None;
            if key == Key::Up {
                // A third key: no capture, and no ▲ + ▼ action either.
                self.st[i].consumed = true;
                if let Some(c) = &mut self.chord {
                    c.ok = false;
                }
                self.combo_done = true;
                return;
            }
            let other = self.st[if key == Key::Down { Key::Power } else { Key::Down }.idx()];
            let ok = t.saturating_sub(other.since) <= CAPTURE_GAP_MS
                && !up
                && !other.no_chord
                && !self.st[i].no_chord
                && !other.hold_fired;
            if up {
                self.combo_done = true; // ▲ + ▼ already down: no play/pause or Help
            }
            self.chord = Some(Chord { first: other.since, ok });
            return;
        }
        // ▲ + ▼ together: play/pause (short) or Help (2 s).
        if up && dn {
            self.st[Key::Up.idx()].consumed = true;
            self.st[Key::Down.idx()].consumed = true;
            self.st[Key::Up.idx()].repeat_at = 0;
            self.st[Key::Down.idx()].repeat_at = 0;
            if !self.combo_ud {
                self.combo_ud = true;
                self.combo_since = t;
                self.combo_done = false;
                // In a menu the first key already moved the cursor: move it back, so ▲ + ▼
                // (Back, or Help when held) leaves the list where it was. Not when that key
                // was held long enough to scroll: then it is no Back either.
                let first = if key == Key::Up { Key::Down } else { Key::Up };
                let f = self.st[first.idx()];
                self.combo_menu = f.moved && t.saturating_sub(f.since) < HOLD_MS;
                if self.combo_menu {
                    out.push(if first == Key::Up { Action::MenuDown } else { Action::MenuUp });
                }
            }
            return;
        }

        // Screen off, nothing to control: the first press only wakes.
        if !ctx.screen_on && !ctx.loaded {
            out.push(Action::Wake);
            if key == Key::Power {
                // Keep the press alive for the 3 s leave hold.
                self.skip_power_release = true;
            } else {
                self.st[i].consumed = true;
            }
            return;
        }

        if ctx.music() {
            if key == Key::Power {
                if let Some(t0) = self.power_tap.take() {
                    if t.saturating_sub(t0) <= DOUBLE_MS {
                        out.push(Action::Menu);
                        // The press stays alive for the 3 s leave hold, but
                        // its release must not toggle playback.
                        self.skip_power_release = true;
                    }
                }
            }
            // ▲ / ▼ decide on release (tap) or in tick() (hold).
            return;
        }

        // Menu keys: move on press, keep moving while held (tick()).
        match key {
            Key::Up | Key::Down => {
                out.push(if key == Key::Up { Action::MenuUp } else { Action::MenuDown });
                let s = &mut self.st[i];
                s.consumed = true;
                s.moved = true;
                s.repeat_at = t + HOLD_MS;
            }
            Key::Power => {} // select on release
        }
    }

    fn release(&mut self, key: Key, t: u64, ctx: Ctx, out: &mut Vec<Action>) {
        let i = key.idx();
        let s = self.st[i];
        self.st[i].down = false;
        if !s.down {
            return; // release without a press we saw (e.g. held at start)
        }
        let held = t.saturating_sub(s.since);

        // ▼ + ● chord: decide on the first release of the pair.
        if let Some(c) = self.chord {
            if matches!(key, Key::Down | Key::Power) {
                if c.ok {
                    if t.saturating_sub(c.first) <= CAPTURE_RELEASE_MS {
                        out.push(Action::Capture);
                    }
                    self.chord = Some(Chord { ok: false, ..c });
                }
                if key == Key::Power {
                    self.skip_power_release = false;
                }
                if !self.st[Key::Down.idx()].down && !self.st[Key::Power.idx()].down {
                    self.chord = None;
                }
                if self.combo_ud && !self.st[Key::Up.idx()].down && !self.st[Key::Down.idx()].down {
                    self.combo_ud = false;
                    self.combo_menu = false;
                }
                return;
            }
        }

        // ▲ + ▼ combo: decide on the first release of the pair.
        if self.combo_ud && matches!(key, Key::Up | Key::Down) {
            if !self.combo_done {
                self.combo_done = true;
                if t.saturating_sub(self.combo_since) < HELP_HOLD_MS {
                    if ctx.music() {
                        out.push(Action::PlayPause);
                    } else if self.combo_menu {
                        out.push(Action::Back);
                    } else if !ctx.screen_on {
                        out.push(Action::Wake);
                    }
                }
            }
            if !self.st[Key::Up.idx()].down && !self.st[Key::Down.idx()].down {
                self.combo_ud = false;
                self.combo_menu = false;
            }
            return;
        }

        if key == Key::Power && std::mem::take(&mut self.skip_power_release) {
            return;
        }
        if s.consumed || s.hold_fired || held > IGNORE_HELD_MS {
            return;
        }

        match key {
            Key::Up | Key::Down => {
                if ctx.music() && held < HOLD_MS {
                    out.push(if key == Key::Up { Action::VolUp } else { Action::VolDown });
                }
            }
            Key::Power => {
                if held >= LEAVE_HOLD_MS {
                    return;
                }
                if ctx.music() {
                    // Wait for a possible second press (● ● = menu).
                    self.power_tap = Some(t);
                } else {
                    out.push(Action::Select);
                }
            }
        }
    }

    /// Time-driven transitions (holds, double-press window, cancel window).
    pub fn tick(&mut self, now: u64, ctx: Ctx) -> Vec<Action> {
        let mut out = Vec::new();
        if let Some(at) = self.leave_at {
            if now >= at {
                self.leave_at = None;
                out.push(Action::Leave);
                return out;
            }
        }
        if let Some(t0) = self.power_tap {
            if now.saturating_sub(t0) > DOUBLE_MS {
                self.power_tap = None;
                out.push(Action::PlayPause);
            }
        }
        if self.combo_ud
            && !self.combo_done
            && self.st[Key::Up.idx()].down
            && self.st[Key::Down.idx()].down
            && now.saturating_sub(self.combo_since) >= HELP_HOLD_MS
        {
            self.combo_done = true;
            out.push(Action::Help);
        }
        if ctx.music() {
            for key in [Key::Up, Key::Down] {
                let s = &mut self.st[key.idx()];
                if s.down && !s.consumed && !s.hold_fired {
                    let held = now.saturating_sub(s.since);
                    if (HOLD_MS..=IGNORE_HELD_MS).contains(&held) {
                        s.hold_fired = true;
                        out.push(if key == Key::Up { Action::Next } else { Action::Prev });
                    }
                }
            }
        } else if ctx.screen_on {
            // A held ▲ / ▼ in a menu keeps moving. It stops for good once ● joins in or
            // the key is held past IGNORE_HELD_MS (a key stuck in a pocket).
            let pw = self.st[Key::Power.idx()].down;
            for key in [Key::Up, Key::Down] {
                let s = &mut self.st[key.idx()];
                if s.down && s.repeat_at != 0 && now >= s.repeat_at {
                    if pw || now.saturating_sub(s.since) > IGNORE_HELD_MS {
                        s.repeat_at = 0;
                        continue;
                    }
                    s.repeat_at = now + REPEAT_MS;
                    out.push(if key == Key::Up { Action::MenuUp } else { Action::MenuDown });
                }
            }
        }
        let p = &mut self.st[Key::Power.idx()];
        if p.down && !p.consumed && !p.hold_fired && now.saturating_sub(p.since) >= LEAVE_HOLD_MS {
            p.hold_fired = true;
            self.skip_power_release = false;
            self.power_tap = None;
            self.leave_at = Some(now + LEAVE_CANCEL_MS);
            out.push(Action::LeaveArm);
        }
        out
    }

    /// The next time `tick()` has something to decide, if any.
    pub fn next_deadline(&self, ctx: Ctx) -> Option<u64> {
        let mut d: Option<u64> = None;
        let mut min = |t: u64| d = Some(d.map_or(t, |x: u64| x.min(t)));
        if let Some(at) = self.leave_at {
            min(at);
        }
        if let Some(t0) = self.power_tap {
            min(t0 + DOUBLE_MS + 1);
        }
        if self.combo_ud && !self.combo_done {
            min(self.combo_since + HELP_HOLD_MS);
        }
        if ctx.music() {
            for key in [Key::Up, Key::Down] {
                let s = &self.st[key.idx()];
                if s.down && !s.consumed && !s.hold_fired {
                    min(s.since + HOLD_MS);
                }
            }
        } else if ctx.screen_on {
            // tick() either moves (the next repeat lies ahead again) or stops the repeat.
            for key in [Key::Up, Key::Down] {
                let s = &self.st[key.idx()];
                if s.down && s.repeat_at != 0 {
                    min(s.repeat_at);
                }
            }
        }
        let p = &self.st[Key::Power.idx()];
        if p.down && !p.consumed && !p.hold_fired {
            min(p.since + LEAVE_HOLD_MS);
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFF_MUSIC: Ctx = Ctx { screen_on: false, player: false, loaded: true };
    const OFF_IDLE: Ctx = Ctx { screen_on: false, player: false, loaded: false };
    const MENU: Ctx = Ctx { screen_on: true, player: false, loaded: true };
    const PLAYER: Ctx = Ctx { screen_on: true, player: true, loaded: true };

    fn acts(v: Vec<Action>) -> Vec<Action> {
        v.into_iter().filter(|a| *a != Action::Activity).collect()
    }

    #[test]
    fn tap_up_is_volume_off_and_on_player() {
        for ctx in [OFF_MUSIC, PLAYER] {
            let mut m = KeyMachine::new();
            assert!(acts(m.on_key(Key::Up, true, 0, ctx)).is_empty());
            assert_eq!(acts(m.on_key(Key::Up, false, 200, ctx)), vec![Action::VolUp]);
        }
    }

    #[test]
    fn hold_up_is_next_once() {
        for ctx in [OFF_MUSIC, PLAYER] {
            let mut m = KeyMachine::new();
            m.on_key(Key::Up, true, 0, ctx);
            assert_eq!(m.next_deadline(ctx), Some(HOLD_MS));
            assert_eq!(m.tick(600, ctx), vec![Action::Next]);
            assert!(m.tick(900, ctx).is_empty());
            assert!(acts(m.on_key(Key::Up, false, 1000, ctx)).is_empty());
        }
    }

    #[test]
    fn hold_down_is_prev() {
        let mut m = KeyMachine::new();
        m.on_key(Key::Down, true, 0, PLAYER);
        assert_eq!(m.tick(650, PLAYER), vec![Action::Prev]);
    }

    #[test]
    fn power_toggles_after_double_window() {
        let mut m = KeyMachine::new();
        m.on_key(Key::Power, true, 0, PLAYER);
        assert!(acts(m.on_key(Key::Power, false, 100, PLAYER)).is_empty());
        assert!(m.tick(400, PLAYER).is_empty());
        assert_eq!(m.tick(501, PLAYER), vec![Action::PlayPause]);
    }

    #[test]
    fn double_power_is_menu() {
        for ctx in [OFF_MUSIC, PLAYER] {
            let mut m = KeyMachine::new();
            m.on_key(Key::Power, true, 0, ctx);
            m.on_key(Key::Power, false, 100, ctx);
            assert_eq!(acts(m.on_key(Key::Power, true, 300, ctx)), vec![Action::Menu]);
            assert!(acts(m.on_key(Key::Power, false, 400, MENU)).is_empty());
            assert!(m.tick(1000, MENU).is_empty());
        }
    }

    #[test]
    fn both_is_play_pause_and_long_is_help() {
        let mut m = KeyMachine::new();
        m.on_key(Key::Up, true, 0, OFF_MUSIC);
        m.on_key(Key::Down, true, 50, OFF_MUSIC);
        assert_eq!(acts(m.on_key(Key::Up, false, 300, OFF_MUSIC)), vec![Action::PlayPause]);
        assert!(acts(m.on_key(Key::Down, false, 320, OFF_MUSIC)).is_empty());

        let mut m = KeyMachine::new();
        m.on_key(Key::Up, true, 0, MENU);
        m.on_key(Key::Down, true, 50, MENU);
        assert_eq!(m.tick(2050, MENU), vec![Action::Help]);
        assert!(acts(m.on_key(Key::Up, false, 2500, MENU)).is_empty());
    }

    #[test]
    fn first_press_without_music_only_wakes() {
        let mut m = KeyMachine::new();
        assert_eq!(acts(m.on_key(Key::Down, true, 0, OFF_IDLE)), vec![Action::Wake]);
        assert!(acts(m.on_key(Key::Down, false, 100, MENU)).is_empty());
        let mut m = KeyMachine::new();
        assert_eq!(acts(m.on_key(Key::Power, true, 0, OFF_IDLE)), vec![Action::Wake]);
        assert!(acts(m.on_key(Key::Power, false, 100, MENU)).is_empty());
    }

    #[test]
    fn hold_power_leaves_unless_cancelled() {
        let mut m = KeyMachine::new();
        m.on_key(Key::Power, true, 0, MENU);
        assert_eq!(m.tick(3000, MENU), vec![Action::LeaveArm]);
        assert!(acts(m.on_key(Key::Power, false, 3500, MENU)).is_empty());
        assert_eq!(m.tick(6000, MENU), vec![Action::Leave]);

        let mut m = KeyMachine::new();
        m.on_key(Key::Power, true, 0, PLAYER);
        m.tick(3000, PLAYER);
        m.on_key(Key::Power, false, 3100, PLAYER);
        assert!(m.tick(3600, PLAYER).is_empty()); // no play/pause after the hold
        assert_eq!(acts(m.on_key(Key::Up, true, 4000, PLAYER)), vec![Action::LeaveCancel]);
        assert!(m.tick(7000, PLAYER).is_empty());
    }

    #[test]
    fn down_plus_power_never_acts() {
        let mut m = KeyMachine::new();
        m.on_key(Key::Down, true, 0, MENU);
        m.on_key(Key::Power, true, 100, MENU);
        assert!(m.tick(8000, MENU).is_empty());
        assert!(acts(m.on_key(Key::Power, false, 8000, MENU)).is_empty());
    }

    #[test]
    fn menu_moves_and_selects() {
        let mut m = KeyMachine::new();
        assert_eq!(acts(m.on_key(Key::Down, true, 0, MENU)), vec![Action::MenuDown]);
        m.on_key(Key::Down, false, 50, MENU);
        m.on_key(Key::Power, true, 100, MENU);
        assert_eq!(acts(m.on_key(Key::Power, false, 200, MENU)), vec![Action::Select]);
    }

    #[test]
    fn capture_chord_fires_on_release() {
        for ctx in [OFF_MUSIC, PLAYER, MENU, OFF_IDLE] {
            // ● first, then ▼.
            let mut m = KeyMachine::new();
            m.on_key(Key::Power, true, 0, ctx);
            assert!(acts(m.on_key(Key::Down, true, 120, ctx)).is_empty());
            assert!(m.tick(500, ctx).is_empty());
            assert_eq!(acts(m.on_key(Key::Down, false, 600, ctx)), vec![Action::Capture]);
            assert!(acts(m.on_key(Key::Power, false, 650, ctx)).is_empty());
            assert!(m.tick(5000, ctx).is_empty()); // no play/pause, no leave arm
        }
        // ▼ first (music): no volume step, no previous, ● release ends it.
        let mut m = KeyMachine::new();
        m.on_key(Key::Down, true, 0, PLAYER);
        m.on_key(Key::Power, true, 250, PLAYER);
        assert!(m.tick(900, PLAYER).is_empty());
        assert_eq!(acts(m.on_key(Key::Power, false, 1000, PLAYER)), vec![Action::Capture]);
        assert!(acts(m.on_key(Key::Down, false, 1100, PLAYER)).is_empty());
        assert!(m.tick(5000, PLAYER).is_empty());
    }

    #[test]
    fn capture_chord_long_hold_never_acts() {
        for ctx in [OFF_MUSIC, PLAYER, MENU] {
            let mut m = KeyMachine::new();
            m.on_key(Key::Down, true, 0, ctx);
            m.on_key(Key::Power, true, 50, ctx);
            for t in [600, 1600, 3000, 7000, 9000] {
                assert!(m.tick(t, ctx).is_empty(), "tick {t}");
            }
            assert!(acts(m.on_key(Key::Down, false, 1600, ctx)).is_empty());
            assert!(acts(m.on_key(Key::Power, false, 1700, ctx)).is_empty());
            assert!(m.tick(9000, ctx).is_empty());
        }
    }

    #[test]
    fn capture_chord_needs_presses_close_together() {
        let mut m = KeyMachine::new();
        m.on_key(Key::Power, true, 0, PLAYER);
        m.on_key(Key::Down, true, 400, PLAYER);
        assert!(acts(m.on_key(Key::Down, false, 600, PLAYER)).is_empty());
        assert!(acts(m.on_key(Key::Power, false, 700, PLAYER)).is_empty());
        assert!(m.tick(5000, PLAYER).is_empty());
    }

    #[test]
    fn capture_chord_with_three_keys_does_nothing() {
        let mut m = KeyMachine::new();
        m.on_key(Key::Up, true, 0, PLAYER);
        m.on_key(Key::Down, true, 50, PLAYER);
        m.on_key(Key::Power, true, 100, PLAYER);
        assert!(acts(m.on_key(Key::Down, false, 300, PLAYER)).is_empty());
        assert!(acts(m.on_key(Key::Up, false, 320, PLAYER)).is_empty());
        assert!(acts(m.on_key(Key::Power, false, 340, PLAYER)).is_empty());
        assert!(m.tick(5000, PLAYER).is_empty());
    }

    #[test]
    fn down_alone_still_works_after_a_capture() {
        let mut m = KeyMachine::new();
        m.on_key(Key::Down, true, 0, PLAYER);
        m.on_key(Key::Power, true, 100, PLAYER);
        assert_eq!(acts(m.on_key(Key::Power, false, 300, PLAYER)), vec![Action::Capture]);
        m.on_key(Key::Down, false, 350, PLAYER);
        // tap ▼ = volume down, hold ▼ = previous
        m.on_key(Key::Down, true, 1000, PLAYER);
        assert_eq!(acts(m.on_key(Key::Down, false, 1200, PLAYER)), vec![Action::VolDown]);
        m.on_key(Key::Down, true, 2000, PLAYER);
        assert_eq!(m.tick(2600, PLAYER), vec![Action::Prev]);
        assert!(acts(m.on_key(Key::Down, false, 2800, PLAYER)).is_empty());
        // menus: ▼ moves
        assert_eq!(acts(m.on_key(Key::Down, true, 3000, MENU)), vec![Action::MenuDown]);
    }

    #[test]
    fn power_alone_still_works_after_a_capture() {
        let mut m = KeyMachine::new();
        m.on_key(Key::Power, true, 0, PLAYER);
        m.on_key(Key::Down, true, 100, PLAYER);
        assert_eq!(acts(m.on_key(Key::Down, false, 300, PLAYER)), vec![Action::Capture]);
        m.on_key(Key::Power, false, 350, PLAYER);
        // ● = play/pause after the double window
        m.on_key(Key::Power, true, 1000, PLAYER);
        assert!(acts(m.on_key(Key::Power, false, 1100, PLAYER)).is_empty());
        assert_eq!(m.tick(1501, PLAYER), vec![Action::PlayPause]);
        // ● ● = menu
        m.on_key(Key::Power, true, 2000, PLAYER);
        m.on_key(Key::Power, false, 2100, PLAYER);
        assert_eq!(acts(m.on_key(Key::Power, true, 2300, PLAYER)), vec![Action::Menu]);
        m.on_key(Key::Power, false, 2400, MENU);
        // menus: ● selects; hold ● 3 s arms leave
        m.on_key(Key::Power, true, 3000, MENU);
        assert_eq!(acts(m.on_key(Key::Power, false, 3100, MENU)), vec![Action::Select]);
        m.on_key(Key::Power, true, 4000, MENU);
        assert_eq!(m.tick(7000, MENU), vec![Action::LeaveArm]);
    }

    #[test]
    fn held_menu_key_keeps_moving() {
        let mut m = KeyMachine::new();
        assert_eq!(acts(m.on_key(Key::Down, true, 0, MENU)), vec![Action::MenuDown]);
        assert_eq!(m.next_deadline(MENU), Some(HOLD_MS));
        assert!(m.tick(599, MENU).is_empty());
        assert_eq!(m.tick(600, MENU), vec![Action::MenuDown]);
        assert_eq!(m.next_deadline(MENU), Some(600 + REPEAT_MS));
        assert!(m.tick(700, MENU).is_empty());
        assert_eq!(m.tick(750, MENU), vec![Action::MenuDown]);
        assert_eq!(m.tick(900, MENU), vec![Action::MenuDown]);
        // Release: no volume step, no select, no more moves.
        assert!(acts(m.on_key(Key::Down, false, 950, MENU)).is_empty());
        assert_eq!(m.next_deadline(MENU), None);
        assert!(m.tick(2000, MENU).is_empty());
        // ▲ too.
        m.on_key(Key::Up, true, 3000, MENU);
        assert_eq!(m.tick(3600, MENU), vec![Action::MenuUp]);
    }

    #[test]
    fn menu_repeat_stops_after_the_stuck_key_limit() {
        let mut m = KeyMachine::new();
        m.on_key(Key::Up, true, 0, MENU);
        let mut t = HOLD_MS;
        let mut moves = 0;
        while t <= IGNORE_HELD_MS + 1000 {
            moves += m.tick(t, MENU).len();
            t += REPEAT_MS;
        }
        assert!(moves > 20 && moves < 40, "{moves}");
        assert_eq!(m.next_deadline(MENU), None);
        assert!(m.tick(IGNORE_HELD_MS + 2000, MENU).is_empty());
    }

    #[test]
    fn no_menu_repeat_on_the_player_or_screen_off() {
        // Now playing: hold ▲ = next once, never a menu move.
        let mut m = KeyMachine::new();
        m.on_key(Key::Up, true, 0, PLAYER);
        assert_eq!(m.tick(600, PLAYER), vec![Action::Next]);
        assert!(m.tick(900, PLAYER).is_empty());
        assert!(m.tick(1200, PLAYER).is_empty());
        // The screen went off while ▲ was held in a menu: no moves in the dark.
        let mut m = KeyMachine::new();
        m.on_key(Key::Up, true, 0, MENU);
        assert!(m.tick(700, OFF_IDLE).is_empty());
        assert_eq!(m.next_deadline(OFF_IDLE), None);
    }

    #[test]
    fn up_down_short_in_a_menu_is_back_without_moving() {
        for first in [Key::Up, Key::Down] {
            let second = if first == Key::Up { Key::Down } else { Key::Up };
            let mut m = KeyMachine::new();
            let moved = acts(m.on_key(first, true, 0, MENU));
            let undo = acts(m.on_key(second, true, 80, MENU));
            // The first key's move is undone at once.
            let expect_undo = if first == Key::Up { Action::MenuDown } else { Action::MenuUp };
            assert_eq!(moved.len(), 1);
            assert_eq!(undo, vec![expect_undo]);
            assert!(m.tick(700, MENU).is_empty(), "no repeat while both are down");
            assert_eq!(acts(m.on_key(first, false, 300, MENU)), vec![Action::Back]);
            assert!(acts(m.on_key(second, false, 320, MENU)).is_empty());
            assert!(m.tick(3000, MENU).is_empty());
        }
    }

    #[test]
    fn up_down_held_in_a_menu_is_help_not_back() {
        let mut m = KeyMachine::new();
        m.on_key(Key::Down, true, 0, MENU);
        assert_eq!(acts(m.on_key(Key::Up, true, 50, MENU)), vec![Action::MenuUp]);
        assert_eq!(m.tick(2050, MENU), vec![Action::Help]);
        assert!(acts(m.on_key(Key::Up, false, 2500, MENU)).is_empty());
        assert!(acts(m.on_key(Key::Down, false, 2520, MENU)).is_empty());
    }

    #[test]
    fn up_down_after_scrolling_is_not_back() {
        // ▼ held long enough to scroll, then ▲: neither an undo nor Back.
        let mut m = KeyMachine::new();
        m.on_key(Key::Down, true, 0, MENU);
        assert_eq!(m.tick(600, MENU), vec![Action::MenuDown]);
        assert!(acts(m.on_key(Key::Up, true, 700, MENU)).is_empty());
        assert!(acts(m.on_key(Key::Up, false, 900, MENU)).is_empty());
        assert!(acts(m.on_key(Key::Down, false, 950, MENU)).is_empty());
    }

    #[test]
    fn up_down_that_woke_the_screen_is_not_back() {
        // Screen off, nothing loaded: ▲ wakes; the ▲ + ▼ that follows must not go Back.
        let mut m = KeyMachine::new();
        assert_eq!(acts(m.on_key(Key::Up, true, 0, OFF_IDLE)), vec![Action::Wake]);
        assert!(acts(m.on_key(Key::Down, true, 60, MENU)).is_empty());
        assert!(acts(m.on_key(Key::Up, false, 200, MENU)).is_empty());
        assert!(acts(m.on_key(Key::Down, false, 220, MENU)).is_empty());
    }

    #[test]
    fn up_down_keeps_its_old_meaning_on_the_player_and_screen_off() {
        for ctx in [PLAYER, OFF_MUSIC] {
            let mut m = KeyMachine::new();
            m.on_key(Key::Down, true, 0, ctx);
            m.on_key(Key::Up, true, 40, ctx);
            assert_eq!(acts(m.on_key(Key::Down, false, 300, ctx)), vec![Action::PlayPause]);
        }
    }

    #[test]
    fn capture_chord_stops_a_menu_repeat() {
        let mut m = KeyMachine::new();
        m.on_key(Key::Down, true, 0, MENU);
        m.on_key(Key::Power, true, 100, MENU);
        assert_eq!(m.next_deadline(MENU), None, "no repeat, no leave hold");
        for t in [600, 750, 900, 1200] {
            assert!(m.tick(t, MENU).is_empty(), "tick {t}");
        }
        assert_eq!(acts(m.on_key(Key::Power, false, 1300, MENU)), vec![Action::Capture]);
        assert!(acts(m.on_key(Key::Down, false, 1350, MENU)).is_empty());
    }

    #[test]
    fn select_after_a_held_move_still_works() {
        let mut m = KeyMachine::new();
        m.on_key(Key::Down, true, 0, MENU);
        m.tick(600, MENU);
        m.on_key(Key::Down, false, 700, MENU);
        m.on_key(Key::Power, true, 800, MENU);
        assert_eq!(acts(m.on_key(Key::Power, false, 900, MENU)), vec![Action::Select]);
    }

    #[test]
    fn cancel_press_does_not_start_a_capture() {
        let mut m = KeyMachine::new();
        m.arm_leave(0);
        assert_eq!(acts(m.on_key(Key::Power, true, 100, MENU)), vec![Action::LeaveCancel]);
        m.on_key(Key::Down, true, 150, MENU);
        assert!(acts(m.on_key(Key::Down, false, 300, MENU)).is_empty());
        assert!(acts(m.on_key(Key::Power, false, 350, MENU)).is_empty());
    }
}
