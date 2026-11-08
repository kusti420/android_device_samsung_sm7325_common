//! Screens, drawn after the canvas boards (Main, Library, Player, Help) on a
//! 1080 x 2400 OLED: pure black, a status header, recovery-style rows with
//! the label left and a dim value right, and a key-guide footer on every
//! screen, adapted to what the keys do there.
//!
//! The selection is "▸ " plus the accent colour (bold); there are no filled
//! bars. Every list has a Back row at the top and the bottom.

use std::path::PathBuf;

use crate::audio::Output;
use crate::battery::Battery;
use crate::bt::{Bt, Conn, Phase};
use crate::display::Canvas;
use crate::font::{FontData, BIG, BODY, BODY_BOLD, SMALL};
use crate::library::{letter_of, Library};
use crate::player::{PlayState, Status};
use crate::modules::Host;
use crate::module_ui;
use crate::wifi::Wifi;

// OLED palette (DECISIONS / OLED board).
pub const BG: u32 = 0x000000;
pub const TEXT: u32 = 0xA8A290;
pub const DIM: u32 = 0x7D7869;
pub const ACCENT: u32 = 0x5DB04C;
pub const WARN: u32 = 0xD08A2A;
pub const LINE: u32 = 0x25231C;

const MARGIN: i32 = 64;
/// Top of the header (below the punch-hole camera).
const TOP: i32 = 120;
const ROW_H: i32 = 84;
const SMALL_LH: i32 = 54;
const FOOTER_BOTTOM: i32 = 2400 - 64;
/// Lists with more entries than this start with the letter row.
const LETTER_ROW_MIN: usize = 24;

pub const NO_OUTPUT_TEXT: &str =
    "No headphones. Plug in wired or USB headphones, or connect Bluetooth headphones: menu › Bluetooth.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListKind {
    Artists,
    Albums,
    Folders,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Home,
    Music,
    /// "Pick a letter first" for a long list.
    Letters(ListKind),
    /// A list, optionally only the names under one letter.
    List(ListKind, Option<char>),
    Artist(usize),
    Album(usize),
    Folder(usize),
    NowPlaying,
    Battery,
    /// Bluetooth headphones: turn on, pick paired headphones, disconnect, turn off.
    Bluetooth,
    Help,
    PowerOff,
    /// Main › Apps and the installed modules' screens (modules.rs keeps their stack).
    Module,
}

/// What the daemon must do after a selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    None,
    Resume,
    ShuffleAll,
    PlayFolder(usize),
    PlayTrack { folder: usize, index: usize },
    PlayAlbum { album: usize, index: usize },
    PlayArtist(usize),
    /// Start abyss-btd (`sys.abyss.bt=on`).
    BtOn,
    /// Stop it (`off`, then `sys.abyss.bt=off`).
    BtOff,
    /// Connect: Some(index into the paired list) or None = the last headphones
    /// (turns Bluetooth on first if needed).
    BtConnect(Option<usize>),
    BtDisconnect,
    /// Ask for the paired list again.
    BtRefresh,
    /// Battery screen "Wi-Fi test": lease → GET generate_204 → release (wifi/mod.rs).
    WifiTest,
    /// Main › Apps (modules.rs).
    OpenApps,
    /// Home › an installed module by index (modules.rs), straight to its start screen.
    OpenModule(usize),
    Leave,
    PowerOff,
}

#[derive(Clone, Copy)]
enum RowAct {
    Back,
    Go(Screen),
    Do(Request),
}

struct Row {
    label: String,
    meta: String,
    act: RowAct,
    color: u32,
}

impl Row {
    fn new(label: impl Into<String>, act: RowAct) -> Row {
        Row { label: label.into(), meta: String::new(), act, color: TEXT }
    }
    fn meta(mut self, m: impl Into<String>) -> Row {
        self.meta = m.into();
        self
    }
    fn color(mut self, c: u32) -> Row {
        self.color = c;
        self
    }
}

/// Everything a screen may show, borrowed from the daemon for one draw.
pub struct View<'a> {
    pub lib: &'a Library,
    pub status: &'a Status,
    pub battery: &'a Battery,
    pub volume: u32,
    pub volume_max: u32,
    pub output: Option<&'a Output>,
    pub bt: &'a Bt,
    /// Wi-Fi leases (off unless something holds one).
    pub wifi: &'a Wifi,
    /// Installed modules and their screens / stream.
    pub modules: &'a Host,
    pub resume_track: Option<&'a PathBuf>,
    pub resume_pos_ms: u64,
    /// Some(seconds left) while the "back to Android" cancel window runs.
    pub leaving_in: Option<u64>,
    /// Some(seconds left) while the low-battery shutdown warning runs.
    pub low_battery_in: Option<u64>,
    /// Show the volume overlay (2 s after a volume key with the screen on).
    pub volume_overlay: bool,
    /// One line of feedback above the key guide.
    pub msg: Option<(&'a str, u32)>,
}

pub struct Ui {
    pub screen: Screen,
    sel: usize,
    top: usize,
    stack: Vec<(Screen, usize, usize)>,
}

/// "FLAC 44.1 kHz · 16 bit · DSP" / "MP3 44.1 kHz · CPU decode".
fn format_tag(st: &Status) -> Option<String> {
    if st.codec.is_empty() || st.rate == 0 {
        return None;
    }
    let khz = if st.rate.is_multiple_of(1000) {
        format!("{} kHz", st.rate / 1000)
    } else {
        format!("{:.1} kHz", st.rate as f64 / 1000.0)
    };
    let bits = if st.bits > 0 { format!(" · {} bit", st.bits) } else { String::new() };
    let who = if st.dsp { "DSP" } else { "CPU decode" };
    Some(format!("{} {khz}{bits} · {who}", st.codec))
}

fn mmss(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Greedy word wrap into lines of at most `max_w` pixels.
fn wrap(f: &FontData, text: &str, max_w: u32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let cand = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if f.text_width(&cand) <= max_w || cur.is_empty() {
            cur = cand;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines.into_iter().map(|l| f.ellipsize(&l, max_w)).collect()
}

fn hours_short(b: &Battery) -> String {
    if b.charging() {
        return "charging".into();
    }
    match b.hours_left() {
        Some(h) if h >= 1.0 => format!("≈ {:.0} h", h),
        Some(h) => format!("≈ {:.0} min", h * 60.0),
        None => "≈ ? h".into(),
    }
}

fn pct(b: &Battery) -> String {
    b.capacity.map(|c| format!("{c}%")).unwrap_or_else(|| "?%".into())
}

/// Key guide lines (key column, meaning) per screen.
fn guide(screen: Screen) -> &'static [(&'static str, &'static str)] {
    match screen {
        Screen::Home => &[
            ("screen on", "▲ ▼ move   ● select"),
            ("screen off", "keys play music"),
            ("hold ▲+▼ 2 s", "help"),
            ("hold ● 3 s", "back to Android"),
        ],
        Screen::NowPlaying => &[
            ("tap ▲ / ▼", "volume up / down"),
            ("hold ▲ / ▼", "next / previous"),
            ("● / ● ●", "play·pause / menu"),
            ("▼ + ● short", "save this song"),
            ("hold ● 3 s", "back to Android"),
        ],
        Screen::Letters(_) => &[
            ("▲ ▼", "move between letters"),
            ("●", "open the letter"),
            ("Back", "last cell, returns"),
            ("hold ● 3 s", "back to Android"),
        ],
        Screen::PowerOff => &[
            ("▲ ▼", "move"),
            ("●", "select"),
            ("Keep it on", "returns to the menu"),
            ("hold ● 3 s", "back to Android"),
        ],
        Screen::Bluetooth => &[
            ("▲ ▼", "move"),
            ("●", "select"),
            ("Back", "first and last row"),
            ("hold ● 3 s", "back to Android"),
        ],
        Screen::Battery => &[
            ("▲ ▼", "move"),
            ("●", "select"),
            ("Wi-Fi test", "connect, fetch, off again"),
            ("hold ● 3 s", "back to Android"),
        ],
        Screen::Help => &[
            ("▲ ▼", "move"),
            ("●", "select (Back returns)"),
            ("screen off", "keys play music"),
            ("hold ● 3 s", "back to Android"),
        ],
        _ => &[
            ("▲ ▼", "move"),
            ("● select", "a song plays at once"),
            ("Back", "first and last row"),
            ("hold ● 3 s", "back to Android"),
        ],
    }
}

/// Rows of the Bluetooth screen. The last headphones are always offered first.
fn bt_rows(bt: &Bt, back: impl Fn() -> Row) -> Vec<Row> {
    let mut r = vec![back()];
    let last = bt.last();
    let connect_last = |r: &mut Vec<Row>, meta: &str| {
        if let Some(d) = last {
            r.push(
                Row::new(format!("Connect {}", d.name), RowAct::Do(Request::BtConnect(None)))
                    .meta(meta)
                    .color(ACCENT),
            );
        }
    };
    match bt.phase() {
        Phase::Off => {
            connect_last(&mut r, "turns on");
            r.push(Row::new("Turn on", RowAct::Do(Request::BtOn)).meta("off now"));
        }
        Phase::Starting => {
            r.push(Row::new("Turning on…", RowAct::Do(Request::None)).color(DIM));
            r.push(Row::new("Turn off", RowAct::Do(Request::BtOff)));
        }
        Phase::Stopping => {
            r.push(Row::new("Turning off…", RowAct::Do(Request::None)).color(DIM));
        }
        Phase::On => {
            match bt.conn() {
                Conn::Connected(d) => {
                    r.push(Row::new("Connected", RowAct::Do(Request::None)).meta(d.name.clone()).color(ACCENT));
                    r.push(Row::new("Disconnect", RowAct::Do(Request::BtDisconnect)));
                }
                Conn::Connecting(d) => {
                    r.push(
                        Row::new(format!("Connecting to {}…", d.name), RowAct::Do(Request::None)).color(DIM),
                    );
                    r.push(Row::new("Stop connecting", RowAct::Do(Request::BtDisconnect)));
                }
                Conn::None => {
                    connect_last(&mut r, "last used");
                    let mut any = false;
                    for (i, d) in bt.devices().iter().enumerate() {
                        if last.is_some_and(|l| l.addr == d.addr) {
                            continue;
                        }
                        any = true;
                        r.push(
                            Row::new(d.name.clone(), RowAct::Do(Request::BtConnect(Some(i)))).meta("paired"),
                        );
                    }
                    if !any && last.is_none() {
                        r.push(
                            Row::new("No paired headphones", RowAct::Do(Request::BtRefresh))
                                .meta("pair in Android")
                                .color(DIM),
                        );
                    }
                    r.push(Row::new("Refresh list", RowAct::Do(Request::BtRefresh)).color(DIM));
                }
            }
            r.push(Row::new("Turn off", RowAct::Do(Request::BtOff)));
        }
    }
    r.push(back());
    r
}

fn list_len(lib: &Library, kind: ListKind) -> usize {
    match kind {
        ListKind::Artists => lib.artists.len(),
        ListKind::Albums => lib.albums.len(),
        ListKind::Folders => lib.album_folders().len(),
    }
}

/// (name, value-on-the-right, screen to open) for every entry of a list.
fn list_items(lib: &Library, kind: ListKind) -> Vec<(String, String, Screen)> {
    match kind {
        ListKind::Artists => lib
            .artists
            .iter()
            .enumerate()
            .map(|(i, a)| (a.name.clone(), format!("{}", a.n_tracks), Screen::Artist(i)))
            .collect(),
        ListKind::Albums => lib
            .albums
            .iter()
            .enumerate()
            .map(|(i, a)| (a.name.clone(), format!("{}", a.tracks.len()), Screen::Album(i)))
            .collect(),
        ListKind::Folders => lib
            .album_folders()
            .into_iter()
            .map(|id| {
                let f = &lib.folders[id];
                (f.name.clone(), format!("{}", f.tracks.len()), Screen::Folder(id))
            })
            .collect(),
    }
}

fn kind_name(kind: ListKind) -> &'static str {
    match kind {
        ListKind::Artists => "ARTISTS",
        ListKind::Albums => "ALBUMS",
        ListKind::Folders => "FOLDERS",
    }
}

/// Letters that have entries, in row order ('#' first).
fn letters(lib: &Library, kind: ListKind) -> Vec<char> {
    let mut v: Vec<char> = list_items(lib, kind).iter().map(|x| letter_of(&x.0)).collect();
    v.sort_by_key(|&c| if c == '#' { 0 } else { c as u32 });
    v.dedup();
    v
}

/// How many installed modules Home lists by name (the rest are under Apps ›).
const HOME_MODULES: usize = 3;

impl Ui {
    pub fn new() -> Ui {
        Ui { screen: Screen::Home, sel: 0, top: 0, stack: Vec::new() }
    }

    fn rows(&self, v: &View) -> Vec<Row> {
        let back = || Row::new("Back", RowAct::Back).color(DIM);
        let lib = v.lib;
        match self.screen {
            Screen::Home => {
                let st = v.status;
                let music_meta = match (st.state, v.output.is_some()) {
                    (PlayState::Playing, _) => "playing",
                    (PlayState::Paused, false) => "paused, no headphones",
                    (PlayState::Paused, true) => "paused",
                    (PlayState::Stopped, false) => "no headphones",
                    _ => "",
                };
                let np_meta = match (st.stream, v.modules.stream_status()) {
                    (true, Some(r)) => r.title,
                    _ => st.track.as_ref().map(|t| lib.title_of(t)).unwrap_or_else(|| "nothing".into()),
                };
                let mut rows = vec![
                    Row::new("Music ›", RowAct::Go(Screen::Music)).meta(music_meta),
                    Row::new("Now playing", RowAct::Go(Screen::NowPlaying)).meta(np_meta),
                ];
                // Installed modules show on Home under their own names (a few of them), so a new
                // one is found where the person looks first; all of them stay under Apps ›.
                let names = v.modules.names();
                for (i, name) in names.iter().enumerate().take(HOME_MODULES) {
                    rows.push(Row::new(format!("{name} ›"), RowAct::Do(Request::OpenModule(i))).meta("app"));
                }
                if names.is_empty() || names.len() > HOME_MODULES {
                    rows.push(Row::new("Apps ›", RowAct::Do(Request::OpenApps)).meta(match names.len() {
                        0 => "none yet".to_string(),
                        n => n.to_string(),
                    }));
                }
                rows.extend([
                    Row::new("Battery", RowAct::Go(Screen::Battery)).meta(pct(v.battery)),
                    Row::new("Bluetooth ›", RowAct::Go(Screen::Bluetooth)).meta(v.bt.summary()),
                    Row::new("Help", RowAct::Go(Screen::Help)).meta("hold ▲+▼"),
                    Row::new("Back to Android", RowAct::Do(Request::Leave)).meta("about 30 s"),
                    Row::new("Power off", RowAct::Go(Screen::PowerOff)),
                ]);
                rows
            }
            Screen::Music => {
                let mut r = vec![back()];
                let resume = Row::new("Resume", RowAct::Do(Request::Resume));
                r.push(match v.resume_track {
                    Some(t) => resume.meta(format!(
                        "{} · {}",
                        lib.title_of(t),
                        mmss(v.resume_pos_ms)
                    )),
                    None => resume.meta("nothing yet").color(DIM),
                });
                r.push(
                    Row::new("Shuffle all", RowAct::Do(Request::ShuffleAll))
                        .meta(format!("{} songs", lib.track_count())),
                );
                for (label, kind) in [
                    ("Artists ›", ListKind::Artists),
                    ("Albums ›", ListKind::Albums),
                    ("Folders ›", ListKind::Folders),
                ] {
                    let n = list_len(lib, kind);
                    let to = if n > LETTER_ROW_MIN {
                        Screen::Letters(kind)
                    } else {
                        Screen::List(kind, None)
                    };
                    r.push(Row::new(label, RowAct::Go(to)).meta(format!("A–Z · {n}")));
                }
                match lib.find_folder("AbyssTest") {
                    Some(id) => r.push(
                        Row::new("AbyssTest", RowAct::Do(Request::PlayFolder(id)))
                            .meta(format!("{} songs", lib.tracks_below(id).len())),
                    ),
                    None => r.push(
                        Row::new("AbyssTest", RowAct::Do(Request::None)).meta("not found").color(DIM),
                    ),
                }
                let out = match v.output {
                    Some(o) => format!("auto · {}", o.short()),
                    None => "auto · none plugged in".into(),
                };
                r.push(Row::new("Output", RowAct::Do(Request::None)).meta(out));
                r.push(back());
                r
            }
            Screen::Letters(kind) => {
                let mut r: Vec<Row> = letters(lib, kind)
                    .into_iter()
                    .map(|c| Row::new(c.to_string(), RowAct::Go(Screen::List(kind, Some(c)))))
                    .collect();
                r.push(back());
                r
            }
            Screen::List(kind, letter) => {
                let mut r = vec![back()];
                for (name, meta, to) in list_items(lib, kind) {
                    if letter.is_some_and(|c| letter_of(&name) != c) {
                        continue;
                    }
                    r.push(Row::new(format!("{name} ›"), RowAct::Go(to)).meta(meta));
                }
                if r.len() == 1 {
                    r.push(Row::new("No music found", RowAct::Do(Request::None)).color(DIM));
                }
                r.push(back());
                r
            }
            Screen::Artist(id) => {
                let mut r = vec![back()];
                if let Some(a) = lib.artists.get(id) {
                    r.push(
                        Row::new("Play all", RowAct::Do(Request::PlayArtist(id)))
                            .meta(format!("{} songs", a.n_tracks))
                            .color(ACCENT),
                    );
                    for &al in &a.albums {
                        let album = &lib.albums[al];
                        r.push(
                            Row::new(format!("{} ›", album.name), RowAct::Go(Screen::Album(al)))
                                .meta(format!("{}", album.tracks.len())),
                        );
                    }
                }
                r.push(back());
                r
            }
            Screen::Album(id) => {
                let mut r = vec![back()];
                if let Some(a) = lib.albums.get(id) {
                    r.push(
                        Row::new("Play all", RowAct::Do(Request::PlayAlbum { album: id, index: 0 }))
                            .meta(a.artist.clone())
                            .color(ACCENT),
                    );
                    for (i, t) in a.tracks.iter().enumerate() {
                        let no = lib.track(t).map(|x| x.track_no).unwrap_or(0);
                        let label = if no > 0 {
                            format!("{no:02} {}", lib.title_of(t))
                        } else {
                            lib.title_of(t)
                        };
                        r.push(Row::new(label, RowAct::Do(Request::PlayAlbum { album: id, index: i })));
                    }
                }
                r.push(back());
                r
            }
            Screen::Folder(id) => {
                let mut r = vec![back()];
                if let Some(f) = lib.folders.get(id) {
                    r.push(Row::new("Play all", RowAct::Do(Request::PlayFolder(id))).color(ACCENT));
                    for &s in &f.subs {
                        r.push(Row::new(
                            format!("{} ›", lib.folders[s].name),
                            RowAct::Go(Screen::Folder(s)),
                        ));
                    }
                    for (i, t) in f.tracks.iter().enumerate() {
                        r.push(Row::new(
                            t.title.clone(),
                            RowAct::Do(Request::PlayTrack { folder: id, index: i }),
                        ));
                    }
                }
                r.push(back());
                r
            }
            Screen::NowPlaying => vec![],
            Screen::Bluetooth => bt_rows(v.bt, back),
            Screen::Battery => {
                let meta = if v.wifi.test_running() {
                    "running…".to_string()
                } else {
                    v.wifi.last_test().map(|t| t.short).unwrap_or_else(|| "on, fetch, off".into())
                };
                vec![
                    back(),
                    Row::new("Wi-Fi test", RowAct::Do(Request::WifiTest)).meta(meta),
                    back(),
                ]
            }
            Screen::Help => vec![back()],
            // Drawn and driven by modules.rs (main.rs routes the keys there).
            Screen::Module => vec![],
            Screen::PowerOff => vec![
                Row::new("Keep it on", RowAct::Back),
                Row::new("Yes, power off", RowAct::Do(Request::PowerOff)).color(WARN),
            ],
        }
    }

    fn fix_scroll(&mut self, n: usize, vis: usize) {
        if n == 0 {
            self.sel = 0;
            self.top = 0;
            return;
        }
        self.sel = self.sel.min(n - 1);
        if self.sel < self.top {
            self.top = self.sel;
        }
        if self.sel >= self.top + vis {
            self.top = self.sel + 1 - vis;
        }
    }

    pub fn up(&mut self, v: &View) {
        let n = self.rows(v).len();
        if n > 0 {
            self.sel = if self.sel == 0 { n - 1 } else { self.sel - 1 };
        }
    }

    pub fn down(&mut self, v: &View) {
        let n = self.rows(v).len();
        if n > 0 {
            self.sel = (self.sel + 1) % n;
        }
    }

    pub fn select(&mut self, v: &View) -> Request {
        let rows = self.rows(v);
        let Some(row) = rows.get(self.sel) else { return Request::None };
        match row.act {
            RowAct::Back => {
                self.back();
                Request::None
            }
            RowAct::Go(s) => {
                self.go(s);
                Request::None
            }
            RowAct::Do(r) => r,
        }
    }

    /// Opens `s` on top of the current screen.
    pub fn go(&mut self, s: Screen) {
        if self.screen == s {
            return;
        }
        // Never stack two players: going to Now playing from inside it is a no-op,
        // and leaving it returns to where the song was picked.
        self.stack.retain(|(x, _, _)| *x != s);
        self.stack.push((self.screen, self.sel, self.top));
        self.screen = s;
        // Lists start on the first real row, not on Back.
        self.sel = match s {
            Screen::Music
            | Screen::List(..)
            | Screen::Artist(_)
            | Screen::Album(_)
            | Screen::Folder(_)
            | Screen::Bluetooth
            | Screen::Battery => 1,
            _ => 0,
        };
        self.top = 0;
    }

    pub fn back(&mut self) {
        if let Some((s, sel, top)) = self.stack.pop() {
            self.screen = s;
            self.sel = sel;
            self.top = top;
        } else {
            self.screen = Screen::Home;
            self.sel = 0;
            self.top = 0;
        }
    }

    /// Whether the screen shows something that changes by itself every second.
    pub fn is_live(&self, v: &View) -> bool {
        v.leaving_in.is_some()
            || v.low_battery_in.is_some()
            || (self.screen == Screen::NowPlaying && v.status.state == PlayState::Playing)
            // The Wi-Fi line counts the linger down / follows the bring-up.
            || (self.screen == Screen::Battery && (v.wifi.lingering() || v.wifi.busy()))
    }

    pub fn draw(&mut self, cv: &mut Canvas, v: &View) {
        cv.clear(BG);
        let w = cv.w as i32;
        let max_w = (w - 2 * MARGIN) as u32;

        if let Some(s) = v.leaving_in {
            let y = 900;
            BIG.draw(cv, MARGIN, y, "Back to Android", WARN);
            BODY.draw(cv, MARGIN, y + 120, &format!("in {s} s"), TEXT);
            BODY.draw(cv, MARGIN, y + 210, "Any key cancels.", DIM);
            SMALL.draw(cv, MARGIN, y + 300, "You stay in Abyss if you press a key.", DIM);
            self.draw_footer(cv, &[("any key", "stay in Abyss")]);
            return;
        }

        let y = match self.screen {
            Screen::Home => self.draw_home_header(cv, v),
            Screen::Module => {
                if let Some(view) = v.modules.view() {
                    module_ui::draw(cv, &view, v.msg);
                }
                return;
            }
            Screen::NowPlaying if v.status.stream => {
                if let Some(np) = v.modules.now_playing_view() {
                    let state = if v.output.is_none() {
                        ("NO HEADPHONES", WARN)
                    } else {
                        match v.status.state {
                            PlayState::Playing => ("RADIO · PLAYING", ACCENT),
                            _ => ("RADIO · PAUSED", DIM),
                        }
                    };
                    let msg = v.status.error.as_deref().map(|e| (e, WARN)).or(v.msg);
                    module_ui::draw_now_playing(cv, &np, state, (v.volume, v.volume_max), guide(Screen::NowPlaying), msg);
                    if v.volume_overlay {
                        self.draw_volume_overlay(cv, v);
                    }
                    return;
                }
                self.draw_now_playing(cv, v, max_w);
                self.draw_footer(cv, guide(Screen::NowPlaying));
                return;
            }
            Screen::NowPlaying => {
                self.draw_now_playing(cv, v, max_w);
                self.draw_footer(cv, guide(Screen::NowPlaying));
                if v.volume_overlay {
                    self.draw_volume_overlay(cv, v);
                }
                return;
            }
            Screen::Music => {
                let sub = format!("{} songs · scanned on the phone", v.lib.track_count());
                self.draw_title(cv, "MUSIC", Some(&sub))
            }
            Screen::Letters(kind) => {
                self.draw_title(cv, kind_name(kind), Some("Pick a letter first: long lists open one letter at a time."))
            }
            Screen::List(kind, letter) => {
                let t = match letter {
                    Some(c) => format!("{} · {c}", kind_name(kind)),
                    None => format!("{} A–Z", kind_name(kind)),
                };
                self.draw_title(cv, &t, None)
            }
            Screen::Artist(id) => {
                let n = v.lib.artists.get(id).map(|a| a.name.clone()).unwrap_or_default();
                self.draw_title(cv, &n, Some("artist"))
            }
            Screen::Album(id) => {
                let (n, a) = v
                    .lib
                    .albums
                    .get(id)
                    .map(|a| (a.name.clone(), a.artist.clone()))
                    .unwrap_or_default();
                self.draw_title(cv, &n, Some(&a))
            }
            Screen::Folder(id) => {
                let n = v.lib.folders.get(id).map(|f| f.name.clone()).unwrap_or_default();
                self.draw_title(cv, &n, Some("folder"))
            }
            Screen::Battery => {
                let y = self.draw_title(cv, "BATTERY", None);
                self.draw_battery(cv, v, y)
            }
            Screen::Bluetooth => {
                let sub = match v.bt.phase() {
                    Phase::Off => "Off. Uses more battery while on. Pair new headphones in Android first.",
                    _ => "Headphones paired in Android. Connected Bluetooth wins over wired.",
                };
                let mut y = self.draw_title(cv, "BLUETOOTH", Some(sub));
                if let Some((line, warn)) = v.bt.line() {
                    for l in wrap(&BODY, line, max_w).into_iter().take(2) {
                        BODY.draw(cv, MARGIN, y, &l, if warn { WARN } else { TEXT });
                        y += 64;
                    }
                    y += 30;
                }
                y
            }
            Screen::Help => {
                let y = self.draw_title(cv, "HELP", Some("Opened with: hold ▲ + ▼ for 2 s."));
                self.draw_help(cv, y, max_w)
            }
            Screen::PowerOff => {
                let y = self.draw_title(cv, "POWER OFF", None);
                let mut y = y;
                for l in wrap(&SMALL, "Power off? ▼ to \"Yes, power off\", then ●. \"Keep it on\" returns. Your place in the music is saved.", max_w) {
                    SMALL.draw(cv, MARGIN, y, &l, WARN);
                    y += SMALL_LH;
                }
                y + 30
            }
        };

        // Rows.
        let footer_top = self.footer_top(guide(self.screen).len());
        let msg_h = if v.msg.is_some() || v.low_battery_in.is_some() { 2 * SMALL_LH } else { 0 };
        let list_bottom = footer_top - 30 - msg_h;
        let rows = self.rows(v);
        if let Screen::Letters(_) = self.screen {
            self.draw_letters(cv, &rows, y);
        } else {
            let vis = ((list_bottom - y) / ROW_H).max(1) as usize;
            self.fix_scroll(rows.len(), vis);
            self.draw_rows(cv, &rows, y, vis, max_w);
        }

        // Feedback line(s) above the guide.
        let mut my = footer_top - 30 - msg_h;
        if let Some(s) = v.low_battery_in {
            SMALL.draw(cv, MARGIN, my, &format!("Battery low: power off in {s} s"), WARN);
            my += SMALL_LH;
        }
        if let Some((m, c)) = v.msg {
            for l in wrap(&SMALL, m, max_w).into_iter().take(2) {
                SMALL.draw(cv, MARGIN, my, &l, c);
                my += SMALL_LH;
            }
        }
        self.draw_footer(cv, guide(self.screen));
    }

    fn footer_top(&self, lines: usize) -> i32 {
        FOOTER_BOTTOM - lines as i32 * SMALL_LH
    }

    /// The key guide at the bottom: key column dim, meaning in text colour.
    fn draw_footer(&self, cv: &mut Canvas, lines: &[(&str, &str)]) {
        let mut y = self.footer_top(lines.len());
        cv.fill_rect(MARGIN, y - 18, cv.w as i32 - 2 * MARGIN, 2, LINE);
        let col = lines.iter().map(|(k, _)| SMALL.text_width(k)).max().unwrap_or(0) as i32
            + SMALL.text_width("  ") as i32;
        for (k, m) in lines {
            SMALL.draw(cv, MARGIN, y, k, DIM);
            SMALL.draw(cv, MARGIN + col, y, m, TEXT);
            y += SMALL_LH;
        }
    }

    /// Letter-spaced heading in the accent colour, optional dim subline.
    /// Returns where the content starts.
    fn draw_title(&self, cv: &mut Canvas, title: &str, sub: Option<&str>) -> i32 {
        let max_w = (cv.w as i32 - 2 * MARGIN) as u32;
        let t = BODY_BOLD.ellipsize(title, max_w.saturating_sub(4 * title.chars().count() as u32).max(200));
        let mut x = MARGIN;
        for c in t.chars() {
            let s = c.to_string();
            x += BODY_BOLD.draw(cv, x, TOP, &s, ACCENT) as i32 + 4;
        }
        let mut y = TOP + 76;
        if let Some(s) = sub {
            for l in wrap(&SMALL, s, max_w).into_iter().take(2) {
                SMALL.draw(cv, MARGIN, y, &l, DIM);
                y += SMALL_LH;
            }
        }
        y + 36
    }

    fn draw_home_header(&self, cv: &mut Canvas, v: &View) -> i32 {
        let max_w = (cv.w as i32 - 2 * MARGIN) as u32;
        // Fish mark + name, as on the Main board.
        let mut x = MARGIN;
        x += BODY_BOLD.draw(cv, x, TOP, "≈ ", ACCENT) as i32;
        for c in "ICHTHYS ABYSS".chars() {
            x += BODY_BOLD.draw(cv, x, TOP, &c.to_string(), ACCENT) as i32 + 8;
        }
        let mut y = TOP + 76;
        for l in wrap(&SMALL, "Android is asleep. Calls and texts wait until you are back.", max_w) {
            SMALL.draw(cv, MARGIN, y, &l, DIM);
            y += SMALL_LH;
        }
        y += 24;
        let col = SMALL.text_width("battery  ") as i32;
        let b = v.battery;
        let kv = |cv: &mut Canvas, y: i32, k: &str, val: &str, tail: &str| {
            SMALL.draw(cv, MARGIN, y, k, DIM);
            let x = MARGIN + col;
            let adv = SMALL.draw(cv, x, y, val, TEXT) as i32;
            SMALL.draw(cv, x + adv, y, tail, DIM);
        };
        kv(cv, y, "battery", &format!("{}   {}", pct(b), hours_short(b)), " of music");
        y += SMALL_LH;
        let out = v.output.map(|o| o.label()).unwrap_or_else(|| "none plugged in".into());
        kv(cv, y, "output", &SMALL.ellipsize(&out, max_w - col as u32), "");
        y += SMALL_LH;
        // "Wi-Fi off" / "Wi-Fi on (Home)": Wi-Fi is on only while a lease is held (or lingers).
        kv(cv, y, "wi-fi", &SMALL.ellipsize(&v.wifi.summary(), max_w - col as u32), "");
        y += SMALL_LH;
        if v.bt.phase() == Phase::Off {
            kv(cv, y, "off", "Bluetooth, mobile", "");
        } else {
            kv(cv, y, "off", "mobile", "");
            y += SMALL_LH;
            kv(cv, y, "on", &SMALL.ellipsize(&format!("Bluetooth · {}", v.bt.summary()), max_w - col as u32), "");
        }
        y + SMALL_LH + 36
    }

    fn draw_rows(&mut self, cv: &mut Canvas, rows: &[Row], y0: i32, vis: usize, max_w: u32) {
        let w = cv.w as i32;
        let marker_w = BODY.text_width("▸ ") as i32;
        for (i, row) in rows.iter().enumerate().skip(self.top).take(vis) {
            let y = y0 + (i - self.top) as i32 * ROW_H;
            let on = i == self.sel;
            let (font, lc, mc) = if on { (&BODY_BOLD, ACCENT, ACCENT) } else { (&BODY, row.color, DIM) };
            let meta_w = if row.meta.is_empty() {
                0
            } else {
                // The value never takes more than 40 % of the row.
                let m = SMALL.ellipsize(&row.meta, max_w * 2 / 5);
                let mw = SMALL.text_width(&m) as i32;
                SMALL.draw(cv, w - MARGIN - mw, y + 8, &m, mc);
                mw + 24
            };
            let room = (max_w as i32 - marker_w - meta_w).max(80) as u32;
            let label = font.ellipsize(&row.label, room);
            if on {
                BODY_BOLD.draw(cv, MARGIN, y, "▸", ACCENT);
            }
            font.draw(cv, MARGIN + marker_w, y, &label, lc);
        }
        if self.top > 0 {
            SMALL.draw(cv, w - MARGIN - 30, y0 - 50, "▲", DIM);
        }
        if self.top + vis < rows.len() {
            SMALL.draw(cv, w - MARGIN - 30, y0 + vis as i32 * ROW_H - 10, "▼", DIM);
        }
    }

    /// The letter row as a grid (Library board): # A B C ... and Back.
    fn draw_letters(&self, cv: &mut Canvas, rows: &[Row], y0: i32) {
        let cell = 136;
        let per_row = ((cv.w as i32 - 2 * MARGIN) / cell).max(1) as usize;
        let n_letters = rows.len().saturating_sub(1);
        for (i, row) in rows.iter().take(n_letters).enumerate() {
            let x = MARGIN + (i % per_row) as i32 * cell;
            let y = y0 + (i / per_row) as i32 * 130;
            let on = i == self.sel;
            if on {
                cv.outline(x, y, cell - 16, 116, 4, ACCENT);
            }
            let tw = BIG.text_width(&row.label) as i32;
            BIG.draw(cv, x + (cell - 16 - tw) / 2, y + 16, &row.label, if on { ACCENT } else { TEXT });
        }
        let by = y0 + n_letters.div_ceil(per_row) as i32 * 130 + 30;
        let on = self.sel == n_letters;
        if on {
            BODY_BOLD.draw(cv, MARGIN, by, "▸ Back", ACCENT);
        } else {
            BODY.draw(cv, MARGIN + BODY.text_width("▸ ") as i32, by, "Back", DIM);
        }
    }

    fn draw_now_playing(&self, cv: &mut Canvas, v: &View, max_w: u32) {
        let st = v.status;
        let w = cv.w as i32;
        let mut y = TOP;
        // State word left, volume right.
        let (word, wc) = if v.output.is_none() {
            ("NO HEADPHONES", WARN)
        } else {
            match st.state {
                PlayState::Playing => ("PLAYING", ACCENT),
                PlayState::Paused => ("PAUSED", DIM),
                PlayState::Stopped => ("STOPPED", DIM),
            }
        };
        SMALL.draw(cv, MARGIN, y, word, wc);
        let vol = format!("vol {} / {}", v.volume, v.volume_max);
        SMALL.draw(cv, w - MARGIN - SMALL.text_width(&vol) as i32, y, &vol, DIM);
        y += SMALL_LH + 30;

        match &st.track {
            Some(t) => {
                for l in wrap(&BIG, &v.lib.title_of(t), max_w).into_iter().take(2) {
                    BIG.draw(cv, MARGIN, y, &l, TEXT);
                    y += 96;
                }
                BODY.draw(cv, MARGIN, y, &BODY.ellipsize(&v.lib.artist_album_of(t), max_w), DIM);
                y += 70;
                if st.queue_len > 0 {
                    let q = format!("track {} of {}", st.index + 1, st.queue_len);
                    SMALL.draw(cv, MARGIN, y, &q, DIM);
                    y += SMALL_LH;
                }
                // Format and who decodes it: "FLAC 44.1 kHz · 16 bit · DSP".
                if let Some(f) = format_tag(st) {
                    SMALL.draw(cv, MARGIN, y, &SMALL.ellipsize(&f, max_w), DIM);
                    y += SMALL_LH;
                }
                // Elapsed / total with a thin bar.
                y += 10;
                let el = mmss(st.pos_ms);
                let tot = st.dur_ms.map(mmss).unwrap_or_else(|| "–:––".into());
                let ew = SMALL.draw(cv, MARGIN, y, &el, DIM) as i32;
                let tw = SMALL.text_width(&tot) as i32;
                SMALL.draw(cv, w - MARGIN - tw, y, &tot, DIM);
                let bx = MARGIN + ew + 24;
                let bw = w - MARGIN - tw - 24 - bx;
                let by = y + 24;
                cv.fill_rect(bx, by, bw, 6, LINE);
                if let Some(d) = st.dur_ms.filter(|&d| d > 0) {
                    let f = (st.pos_ms.min(d) as f64 / d as f64 * bw as f64) as i32;
                    cv.fill_rect(bx, by, f, 6, TEXT);
                }
                y += SMALL_LH + 30;
            }
            None => {
                BIG.draw(cv, MARGIN, y, "Nothing playing", DIM);
                y += 110;
                SMALL.draw(cv, MARGIN, y, "● ● opens the menu: Music › to pick songs.", DIM);
                y += SMALL_LH + 30;
            }
        }

        // Battery box.
        let box_top = y;
        let inner = MARGIN + 40;
        y += 32;
        SMALL.draw(cv, inner, y, "MUSIC LEFT ON THIS BATTERY", DIM);
        y += SMALL_LH + 6;
        let b = v.battery;
        let hw = BIG.draw(cv, inner, y, &hours_short(b), TEXT) as i32;
        SMALL.draw(cv, inner + hw + 24, y + 24, &format!("left · battery {}", pct(b)), DIM);
        y += 100;
        cv.outline(MARGIN, box_top, w - 2 * MARGIN, y - box_top + 10, 2, LINE);
        y += 50;

        // Output.
        let bt_out = matches!(v.output, Some(Output::Bluetooth { .. }));
        let head = if bt_out {
            "PLAYING ON · Bluetooth wins while connected"
        } else {
            "PLAYING ON · follows what is plugged in"
        };
        SMALL.draw(cv, MARGIN, y, head, DIM);
        y += SMALL_LH + 6;
        match v.output {
            Some(o) => {
                BODY.draw(cv, MARGIN, y, &BODY.ellipsize(&o.label(), max_w), TEXT);
                y += 70;
                let note = if bt_out {
                    "Disconnect pauses. Same headphones back within 5 min: plays on."
                } else {
                    "Unplug pauses. Same headphones back within 5 min: plays on."
                };
                for l in wrap(&SMALL, note, max_w).into_iter().take(2) {
                    SMALL.draw(cv, MARGIN, y, &l, DIM);
                    y += SMALL_LH;
                }
            }
            None => {
                for l in wrap(&BODY, NO_OUTPUT_TEXT, max_w).into_iter().take(4) {
                    BODY.draw(cv, MARGIN, y, &l, WARN);
                    y += 64;
                }
            }
        }
        y += 20;

        // Volume bar.
        SMALL.draw(cv, MARGIN, y, "VOLUME", DIM);
        let vt = format!("{} / {}", v.volume, v.volume_max);
        let vw = SMALL.text_width(&vt) as i32;
        SMALL.draw(cv, w - MARGIN - vw, y, &vt, TEXT);
        let bx = MARGIN + SMALL.text_width("VOLUME  ") as i32;
        let bw = w - MARGIN - vw - 24 - bx;
        cv.fill_rect(bx, y + 22, bw, 6, LINE);
        let f = (v.volume.min(v.volume_max) as f64 / v.volume_max.max(1) as f64 * bw as f64) as i32;
        cv.fill_rect(bx, y + 22, f, 6, ACCENT);
        y += SMALL_LH + 30;

        // Up next.
        if !st.next.is_empty() {
            SMALL.draw(cv, MARGIN, y, "UP NEXT", DIM);
            y += SMALL_LH + 4;
            for (i, t) in st.next.iter().enumerate() {
                let c = if i == 0 { TEXT } else { DIM };
                SMALL.draw(cv, MARGIN, y, &SMALL.ellipsize(&v.lib.title_of(t), max_w), c);
                y += SMALL_LH;
            }
        }

        // Errors / feedback just above the guide.
        let ft = self.footer_top(guide(Screen::NowPlaying).len());
        let mut my = ft - 30 - 2 * SMALL_LH;
        if let Some(s) = v.low_battery_in {
            SMALL.draw(cv, MARGIN, my, &format!("Battery low: power off in {s} s"), WARN);
            my += SMALL_LH;
        }
        if let Some(e) = &st.error {
            SMALL.draw(cv, MARGIN, my, &SMALL.ellipsize(e, max_w), WARN);
        } else if let Some((m, c)) = v.msg {
            SMALL.draw(cv, MARGIN, my, &SMALL.ellipsize(m, max_w), c);
        }
    }

    /// Big volume box in the middle of the screen, 2 s after a volume key.
    fn draw_volume_overlay(&self, cv: &mut Canvas, v: &View) {
        let w = cv.w as i32;
        let (x, y, bw, bh) = (MARGIN, 900, w - 2 * MARGIN, 320);
        cv.fill_rect(x, y, bw, bh, BG);
        cv.outline(x, y, bw, bh, 4, ACCENT);
        // Three stacked rows, nothing shares a line: title, bar, note.
        let t = format!("Volume {} / {}", v.volume, v.volume_max);
        BIG.draw(cv, x + 48, y + 36, &BIG.ellipsize(&t, (bw - 96) as u32), TEXT);
        let (gx, gy, gw) = (x + 48, y + 150, bw - 96);
        cv.fill_rect(gx, gy, gw, 32, LINE);
        let f = (v.volume.min(v.volume_max) as f64 / v.volume_max.max(1) as f64 * gw as f64) as i32;
        cv.fill_rect(gx, gy, f, 32, if v.volume > 140 { WARN } else { ACCENT });
        // Safe-listening mark at 140 of 200.
        let mark = gx + (140.0 / v.volume_max.max(1) as f64 * gw as f64) as i32;
        cv.fill_rect(mark - 2, gy - 12, 4, 56, DIM);
        let (note, c) = if v.volume > 140 { ("loud: above the safe mark", WARN) } else { ("safe up to the mark (140)", DIM) };
        SMALL.draw(cv, x + 48, y + 216, note, c);
    }

    fn draw_battery(&self, cv: &mut Canvas, v: &View, y0: i32) -> i32 {
        let b = v.battery;
        let mut y = y0;
        BIG.draw(cv, MARGIN, y, &pct(b), TEXT);
        y += 110;
        BODY.draw(cv, MARGIN, y, &format!("{} of music", hours_short(b)), TEXT);
        y += 80;
        SMALL.draw(cv, MARGIN, y, &format!("status   {}", b.status), DIM);
        if let Some(ma) = b.current_avg_ma {
            y += SMALL_LH;
            SMALL.draw(cv, MARGIN, y, &format!("draw     {} mA (average)", ma.abs()), DIM);
        }
        if let Some(mv) = b.voltage_mv {
            y += SMALL_LH;
            SMALL.draw(cv, MARGIN, y, &format!("voltage  {}.{:02} V", mv / 1000, (mv % 1000) / 10), DIM);
        }
        y += SMALL_LH;
        SMALL.draw(cv, MARGIN, y, "estimate: level × 4500 mAh × 97 % ÷ draw", DIM);
        y += SMALL_LH + 30;
        // Wi-Fi: state, then the last test result (the "Wi-Fi test" row below runs one).
        let max_w = (cv.w as i32 - 2 * MARGIN) as u32;
        SMALL.draw(cv, MARGIN, y, "WI-FI", DIM);
        y += SMALL_LH;
        for l in wrap(&SMALL, &v.wifi.detail(), max_w).into_iter().take(2) {
            SMALL.draw(cv, MARGIN, y, &l, TEXT);
            y += SMALL_LH;
        }
        if let Some(t) = v.wifi.last_test().filter(|_| !v.wifi.test_running()) {
            for l in wrap(&SMALL, &format!("Last test: {}", t.line), max_w).into_iter().take(3) {
                SMALL.draw(cv, MARGIN, y, &l, if t.ok { ACCENT } else { WARN });
                y += SMALL_LH;
            }
        } else if v.wifi.test_running() {
            SMALL.draw(cv, MARGIN, y, "Test running…", DIM);
            y += SMALL_LH;
        }
        // Saved networks Abyss cannot join, with the reason (wifi/netblock.rs `unusable`).
        let skipped = v.wifi.skipped();
        if !skipped.is_empty() {
            let text = format!("Not usable in Abyss: {}", skipped.join(" · "));
            for l in wrap(&SMALL, &text, max_w).into_iter().take(4) {
                SMALL.draw(cv, MARGIN, y, &l, DIM);
                y += SMALL_LH;
            }
        }
        y + 30
    }

    fn draw_help(&self, cv: &mut Canvas, y0: i32, max_w: u32) -> i32 {
        let mut y = y0;
        SMALL.draw(cv, MARGIN, y, "WHERE YOU ARE", DIM);
        y += SMALL_LH;
        let where_ = "You are in Abyss. Android is asleep: calls and texts wait until you are back. Going back takes about 30 s.";
        for l in wrap(&SMALL, where_, max_w) {
            SMALL.draw(cv, MARGIN, y, &l, TEXT);
            y += SMALL_LH;
        }
        y += 30;
        SMALL.draw(cv, MARGIN, y, "KEYS · Now playing and screen off", DIM);
        y += SMALL_LH;
        let keys: &[(&str, &str)] = &[
            ("tap ▲ / ▼", "louder / quieter"),
            ("hold ▲ / ▼", "next / previous song"),
            ("●", "play / pause"),
            ("● ●", "the menu"),
            ("▲ + ▼", "play / pause"),
            ("▼ + ● short", "save this song"),
            ("headset 1×", "play / pause"),
            ("headset 2× 3×", "next · previous"),
            ("hold ● 3 s", "back to Android"),
        ];
        let col = SMALL.text_width("headset 2× 3× ") as i32;
        for (k, m) in keys {
            SMALL.draw(cv, MARGIN, y, k, ACCENT);
            SMALL.draw(cv, MARGIN + col, y, m, TEXT);
            y += SMALL_LH;
        }
        y += 20;
        SMALL.draw(cv, MARGIN, y, "KEYS · menus", DIM);
        y += SMALL_LH;
        for (k, m) in [
            ("▲ ▼", "move"),
            ("●", "select · lists have Back"),
            ("▼ + ● short", "save the song that plays"),
            ("hold ▼ + ●", "nothing (Samsung force restart)"),
        ] {
            SMALL.draw(cv, MARGIN, y, k, ACCENT);
            SMALL.draw(cv, MARGIN + col, y, m, TEXT);
            y += SMALL_LH;
        }
        y + 30
    }
}
