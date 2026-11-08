//! Stream driver `radio-v1`: the owner's internet radio (gabb.ee) through its public API v1
//! (radio docs/public-api-v1.md), ported from the reference client
//! (radio tools/radio_v1_client.py). The first user-installable Abyss module (modules.rs)
//! binds to it; the player plays its [`Feed`] (stream.rs).
//!
//! Contract rules this follows (Compatibility section of the API doc):
//! - One address is configured (the module setting `server`, default
//!   `http://radio.example:8080`); every other link comes from `/api/v1/stations`
//!   (`*_url`, `audio.<fmt>.url`) and is used as given.
//! - Unknown fields are ignored; a missing field is unknown (the API never sends null, ""
//!   or []); an unknown `kind` is treated like `jingle`: shown as nothing, never captured.
//! - `X-Stream-Cursor` is opaque; `X-Stream-Reset: 1`, `204` and `503` are handled on every
//!   ask; errors are `{"error": …}`.
//!
//! **Bursts** (default, battery): `?cursor=` asks. The first answer brings `behind` (20 s)
//! with a reset; playback starts at 15 s in hand (asking with `min_s` = what is missing,
//! `wait=15`), then the next ask goes out when ~5 s is left, so Wi-Fi wakes about every
//! 15 s and sleeps in between. Each wake also syncs the tracklist (`since_ms` inclusive,
//! items replaced by `play_id`, `more` followed via `paging.since_ms`). A network error keeps
//! the cursor and retries forever, backing off to 15 s; coming back within the server's 60 s
//! ring continues gaplessly, else the answer is a reset. A reset lets the old decoder play
//! out, then a new one starts (never spliced). A failed tracklist never stops the audio.
//!
//! **Endless**: the plain audio URL, one long answer; frames as they come. The song comes
//! from a long poll on `now?after=<play_id>&wait=55` in a second thread, plus the tracklist.
//!
//! **Which song you hear**: wall time of the sample playing now (stream.rs marks) → the
//! tracklist item with `on_air_ms ≤ t < ends_at_ms` (an estimated end counts as open). When
//! nothing matches, one `now?at_ms=t` per wake fills the gap (a jingle, an ad, a sync that
//! has not happened yet). The capture chord saves the matched song's own fields.
//!
//! **Network**: inside Abyss only while a Wi-Fi lease is held. Each wake takes a lease
//! ([`Network::acquire`]) and releases it with the time of the next wake as the hint, so the
//! link stays associated in power save between close wakes (wifi/mod.rs linger). While the
//! link is still coming up, connects are retried every 500 ms; after that the back-off
//! applies, releasing the lease between tries.

use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{info, warn};
use serde_json::{Map, Value};

use crate::capture::{self, Capture};
use crate::flac_stream::{parse_head, Framer};
use crate::mp3_stream::Mp3Framer;
use crate::http::{Cancel, Client, Timeouts, Transport, Url};
use crate::stream::{Codec, Feed, Frame, START_S};

/// Ask again when this much audio is left (reference HEADROOM_S).
pub const HEADROOM_S: f64 = 5.0;
/// The first answer: how much audio to start with (= the latency behind live).
pub const FIRST_BEHIND_S: f64 = 20.0;
/// Network down: retry forever, backing off to this.
pub const NET_RETRY_MAX_S: f64 = 15.0;
/// While the link is coming up: fast retries for this long.
const BRINGUP_RETRY_MS: u64 = 500;
const BRINGUP_TRIES: u32 = 40;
const KEEP_SONGS: usize = 50;
/// The server holds an ask up to this (`wait`, max 15).
const ASK_WAIT_S: u64 = 15;
/// Long poll (`wait`, max 55).
const POLL_WAIT_S: u64 = 55;
/// Never hold more than this unplayed (a paused player must not make the buffer grow).
const MAX_BUFFER_S: f64 = 45.0;
/// Largest answer body accepted (30 s of FLAC is ~4-5 MB).
const MAX_AUDIO_BYTES: usize = 24 << 20;
const MAX_JSON_BYTES: usize = 4 << 20;

// ---------------------------------------------------------------------------
// Network access

/// A held claim on the network (a Wi-Fi lease inside Abyss).
pub trait NetHold: Send {
    /// `next_use_in_ms`: when the holder expects to need the network again.
    fn release(self: Box<Self>, next_use_in_ms: Option<u64>);
}

/// Where the radio gets its network from (wifi_net.rs inside Abyss; [`OpenNetwork`] on a
/// PC or in host mode).
pub trait Network: Send + Sync {
    fn acquire(&self, holder: &str) -> Box<dyn NetHold>;
    /// How sockets are opened (bound to wlan0 inside Abyss).
    fn transport(&self) -> Arc<dyn Transport>;
}

/// The network is simply there (a PC, normal Android: host mode will use it).
#[allow(dead_code)] // the PC test tools; host mode (normal Android) later
pub struct OpenNetwork(pub Arc<dyn Transport>);

#[allow(dead_code)]
struct NoHold;

impl NetHold for NoHold {
    fn release(self: Box<Self>, _: Option<u64>) {}
}

impl Network for OpenNetwork {
    fn acquire(&self, _: &str) -> Box<dyn NetHold> {
        Box::new(NoHold)
    }
    fn transport(&self) -> Arc<dyn Transport> {
        self.0.clone()
    }
}

// ---------------------------------------------------------------------------
// Configuration and songs

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// radio-v1 cursor bursts (`?cursor=&cid=`): the battery way.
    Bursts,
    /// radio-v1 plain URL (`?cid=&behind=`): one endless answer, song by long poll. Not
    /// selectable by a module yet (contract: radio-v1 = bursts); the PC test tool runs it.
    #[allow(dead_code)]
    Endless,
    /// Any endless HTTP stream, fetched as given (module protocol `http`).
    Http,
}

/// Which codec the stream carries (from the format's name or `type`; None = look at the bytes).
pub fn codec_hint(format: &str, mime: Option<&str>) -> Option<&'static str> {
    let m = mime.unwrap_or("").to_ascii_lowercase();
    let f = format.to_ascii_lowercase();
    if m.contains("flac") || f.starts_with("flac") {
        Some("FLAC")
    } else if m.contains("mpeg") || m.contains("mp3") || f.starts_with("mp3") {
        Some("MP3")
    } else {
        None
    }
}

/// One stream to play. Every URL comes from the server (the module's `$station.audio` and
/// `$station.tracklist_url`), never built here.
#[derive(Clone, Debug)]
pub struct Config {
    pub audio: Url,
    /// The tracklist (`songs`): keeps the song you hear known.
    pub songs: Option<Url>,
    /// `now` of the station, if known: `at_ms` lookups for gaps and late captures.
    pub now: Option<Url>,
    pub mode: Mode,
    /// Random id per player (`cid`), counted as one listener.
    pub cid: String,
    /// Prebuffer before playback (15 s; the module setting, clamped 5..30).
    pub start_s: f64,
    /// `$stream.tag` / `$stream.title` / `$stream.format` (station id, name, format name).
    pub tag: String,
    pub title: String,
    pub format: String,
    /// The module's approved origins: every request is checked against them.
    pub allow: Option<Vec<String>>,
}

impl Config {
    pub fn new(audio: Url) -> Config {
        Config {
            audio,
            songs: None,
            now: None,
            mode: Mode::Bursts,
            cid: random_cid(),
            start_s: START_S,
            tag: String::new(),
            title: String::new(),
            format: String::new(),
            allow: None,
        }
    }

    /// From a station of `/api/v1/stations` (the PC test tool; modules pass their own links).
    #[allow(dead_code)]
    pub fn for_station(st: &Station, format: &str) -> io::Result<Config> {
        let audio = st
            .audio
            .get(format)
            .or_else(|| st.audio.values().next())
            .cloned()
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, format!("station {} has no audio", st.id)))?;
        let mut c = Config::new(audio);
        c.songs = st.tracklist_url.clone();
        c.now = st.now_url.clone();
        c.tag = st.id.clone();
        c.title = st.name.clone();
        c.format = format.to_string();
        Ok(c)
    }
}

/// 12 hex characters from /dev/urandom (time as a fallback).
pub fn random_cid() -> String {
    let mut b = [0u8; 6];
    let ok = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_ok();
    if !ok {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        b.copy_from_slice(&t.to_le_bytes()[..6]);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// One play from the API (a song, a live set, a jingle, an ad, or a kind not known yet).
#[derive(Clone, Debug, PartialEq)]
pub struct Song {
    /// `play_id` as text (opaque to us; the API sends a number).
    pub play_id: String,
    pub kind: String,
    pub on_air_ms: i64,
    pub started_at_ms: i64,
    pub ends_at_ms: Option<i64>,
    pub ends_estimated: bool,
    /// Every field the API sent, as sent (the capture writes these).
    pub fields: Map<String, Value>,
}

fn as_i64(v: Option<&Value>) -> Option<i64> {
    v.and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).or_else(|| v.as_str()?.parse().ok()))
}

fn id_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Song {
    pub fn from_json(v: &Value) -> Option<Song> {
        let o = v.as_object()?;
        let play_id = id_text(o.get("play_id")?)?;
        let on_air_ms = as_i64(o.get("on_air_ms")).or_else(|| as_i64(o.get("started_at_ms")))?;
        Some(Song {
            play_id,
            kind: o.get("kind").and_then(Value::as_str).unwrap_or("").to_string(),
            on_air_ms,
            started_at_ms: as_i64(o.get("started_at_ms")).unwrap_or(on_air_ms),
            ends_at_ms: as_i64(o.get("ends_at_ms")),
            ends_estimated: o.get("ends_at_estimated").and_then(Value::as_bool).unwrap_or(false),
            fields: o.clone(),
        })
    }

    /// A song or a live set: shown and capturable. Any other kind (jingle, ad, unknown) is not.
    pub fn is_song(&self) -> bool {
        matches!(self.kind.as_str(), "music" | "live")
    }

    pub fn text(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
    }

    /// The one line ("Artist - Title (Mix) (LABEL CATNO)"), built by the API like capture.rs.
    pub fn line(&self) -> String {
        if let Some(l) = self.text("line") {
            return l.to_string();
        }
        match (self.text("artist"), self.text("title")) {
            (Some(a), Some(t)) => format!("{a} - {t}"),
            (Some(a), None) => a.to_string(),
            (None, Some(t)) => t.to_string(),
            (None, None) => String::new(),
        }
    }
}

/// The tracklist, kept in sync incrementally; answers "which play is at wall time t".
pub struct Songs {
    tracklist: Option<Url>,
    pub items: Vec<Song>,
}

impl Songs {
    pub fn new(tracklist: Option<Url>) -> Songs {
        Songs { tracklist, items: Vec::new() }
    }

    /// Adds or replaces (by `play_id`) one play. True = something changed.
    pub fn merge(&mut self, s: Song) -> bool {
        let changed = match self.items.iter_mut().find(|x| x.play_id == s.play_id) {
            Some(x) if *x == s => false,
            Some(x) => {
                *x = s;
                true
            }
            None => {
                self.items.push(s);
                true
            }
        };
        if changed {
            self.items.sort_by_key(|x| x.started_at_ms);
            let n = self.items.len();
            if n > KEEP_SONGS {
                self.items.drain(..n - KEEP_SONGS);
            }
        }
        changed
    }

    /// One tracklist page (`{items, more, paging}`): merged; returns (changed, next since_ms).
    pub fn apply_page(&mut self, page: &Value) -> (bool, Option<i64>) {
        let mut changed = false;
        if let Some(items) = page.get("items").and_then(Value::as_array) {
            for it in items {
                if let Some(s) = Song::from_json(it) {
                    changed |= self.merge(s);
                }
            }
        }
        let more = page.get("more").and_then(Value::as_bool).unwrap_or(false);
        let next = more.then(|| as_i64(page.get("paging").and_then(|p| p.get("since_ms")))).flatten();
        (changed, next)
    }

    /// Brings the list up to date. True = it changed.
    pub fn sync(&mut self, c: &Client) -> io::Result<bool> {
        let Some(url) = self.tracklist.clone() else { return Ok(false) };
        let mut changed = false;
        let mut since = self.items.last().map(|s| s.started_at_ms);
        for _ in 0..20 {
            let q = match since {
                Some(ms) => vec![("since_ms", ms.to_string())],
                None => vec![("limit", "5".to_string())],
            };
            let page = get_json(c, &url.with_query(&q), Timeouts::io_s(15))?;
            let (ch, next) = self.apply_page(&page);
            changed |= ch;
            match next {
                // `more`: ask again from paging.since_ms (never loop on the same value).
                Some(n) if Some(n) != since => since = Some(n),
                _ => break,
            }
        }
        Ok(changed)
    }

    /// The play on air at `t` (reference `Songs.at`): the newest with `on_air_ms <= t`, if
    /// `t` is before its end or its end is still an estimate.
    pub fn at(&self, t: i64) -> Option<&Song> {
        let it = self.items.iter().rev().find(|s| s.on_air_ms <= t)?;
        let open = match it.ends_at_ms {
            Some(e) => t < e || it.ends_estimated,
            None => true,
        };
        open.then_some(it)
    }
}

fn get_json(c: &Client, url: &Url, t: Timeouts) -> io::Result<Value> {
    let r = c.get(url, &[("Accept", "application/json")], t)?;
    let status = r.status;
    let body = r.bytes(MAX_JSON_BYTES)?;
    let v: Value = serde_json::from_slice(&body)
        .map_err(|e| io::Error::new(ErrorKind::InvalidData, format!("{url}: bad JSON: {e}")))?;
    if status != 200 {
        let what = v.get("error").and_then(Value::as_str).unwrap_or("error");
        return Err(io::Error::other(format!("HTTP {status}: {what}")));
    }
    Ok(v)
}

/// A station from `/api/v1/stations`, with its links as given (the PC test tool; inside
/// Abyss the module's own `get` lists the stations).
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Station {
    pub id: String,
    pub name: String,
    pub live: bool,
    pub description: Option<String>,
    pub bpm: Option<(f64, f64)>,
    /// `audio.<format>.url`, in the server's order.
    pub audio: std::collections::BTreeMap<String, Url>,
    pub now_url: Option<Url>,
    pub tracklist_url: Option<Url>,
}

/// `GET <base>/api/v1/stations`, links resolved against the URL they came from.
#[allow(dead_code)]
pub fn fetch_stations(c: &Client, base: &str) -> io::Result<(String, Vec<Station>)> {
    let url = Url::parse(base)?.join("/api/v1/stations")?;
    let v = get_json(c, &url, Timeouts::io_s(15))?;
    let brand = v.get("brand").and_then(Value::as_str).unwrap_or("Radio").to_string();
    let mut out = Vec::new();
    for st in v.get("stations").and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = st.get("id").and_then(Value::as_str) else { continue };
        let link = |k: &str| st.get(k).and_then(Value::as_str).and_then(|l| url.join(l).ok());
        let audio = st
            .get("audio")
            .and_then(Value::as_object)
            .map(|a| {
                a.iter()
                    .filter_map(|(k, f)| Some((k.clone(), url.join(f.get("url")?.as_str()?).ok()?)))
                    .collect()
            })
            .unwrap_or_default();
        let bpm = st.get("bpm").and_then(Value::as_array).and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?)));
        out.push(Station {
            id: id.to_string(),
            name: st.get("name").and_then(Value::as_str).unwrap_or(id).to_string(),
            live: st.get("live").and_then(Value::as_bool).unwrap_or(false),
            description: st.get("description").and_then(Value::as_str).map(str::to_string),
            bpm,
            audio,
            now_url: link("now_url"),
            tracklist_url: link("tracklist_url"),
        });
    }
    Ok((brand, out))
}

// ---------------------------------------------------------------------------
// The radio handle

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// Finding the station (first contact).
    Starting,
    /// No network yet / the server cannot be reached; the reason.
    Offline(String),
    /// Collecting the first 15 s.
    Buffering,
    Playing,
    /// The buffer ran dry; waiting for the next answer.
    Stalled,
    /// The station is not encoding right now (503).
    OffAir,
    Stopped,
}

/// What the Now-playing screen shows.
#[derive(Clone, Debug)]
pub struct RadioStatus {
    /// `$stream.tag` / `$stream.title` / `$stream.format`.
    pub tag: String,
    pub title: String,
    pub format: String,
    /// "FLAC" / "MP3" once the first answer is in.
    pub codec: Option<&'static str>,
    pub phase: Phase,
    /// What you hear (only songs and live sets; None during a jingle).
    pub song: Option<Song>,
    /// A non-song play is on (jingle, ad, unknown kind).
    pub other_on: bool,
    pub next: Vec<Song>,
    /// Server time of what you hear, ms (for "1:23 into the song").
    pub heard_ms: Option<i64>,
    pub buffered_s: f64,
    pub prebuffer: f64,
    pub latency_s: Option<f64>,
    pub mode: Mode,
    pub asks: u64,
    pub resets: u64,
    pub stalls: u64,
    pub last_error: Option<String>,
}

/// Feedback for the main loop (same pattern as bt.rs / wifi/mod.rs).
#[derive(Clone, Debug, PartialEq)]
pub enum Note {
    /// A capture was saved later (it waited for the song's name): its line.
    Saved(String),
    /// A delayed capture found nothing to save; why.
    NotSaved(String),
}

/// What the capture chord got.
#[derive(Debug)]
pub enum RadioCapture {
    /// The song you hear; save it (capture::save).
    Song(Capture),
    /// Not known yet: the radio asks the server and saves it itself (a [`Note`] follows).
    Pending,
    /// Nothing to save (nothing plays, or a jingle is on); why.
    Nothing(&'static str),
}

struct State {
    phase: Phase,
    codec: Option<&'static str>,
    next: Vec<Song>,
    asks: u64,
    resets: u64,
    last_error: Option<String>,
    /// Debug aid for tests: send a broken cursor on the next ask (the server must reset).
    corrupt_cursor: bool,
}

struct Shared {
    cfg: Config,
    feed: Arc<Feed>,
    songs: Mutex<Songs>,
    state: Mutex<State>,
    stop: Mutex<bool>,
    cv: Condvar,
    cancel: Cancel,
    notes: Mutex<Vec<Note>>,
    poke: Mutex<UnixStream>,
    net: Arc<dyn Network>,
}

impl Shared {
    fn stopped(&self) -> bool {
        *self.stop.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sleeps up to `d`; false = stop was asked.
    fn sleep(&self, d: Duration) -> bool {
        let deadline = Instant::now() + d;
        let mut g = self.stop.lock().unwrap_or_else(|e| e.into_inner());
        while !*g {
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            g = self.cv.wait_timeout(g, deadline - now).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0);
        }
        false
    }

    fn set<F: FnOnce(&mut State)>(&self, f: F) {
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g);
    }

    /// A client limited to the approved origins.
    fn client(&self) -> Client {
        let c = Client::new(self.net.transport(), self.cancel.clone());
        match &self.cfg.allow {
            Some(a) => c.with_allow(a.clone()),
            None => c,
        }
    }

    fn note(&self, n: Note) {
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).push(n);
        if let Ok(mut p) = self.poke.lock() {
            let _ = p.write(&[1]);
        }
    }
}

/// A running radio session (one station). Drop or [`stop`](Radio::stop) ends it.
pub struct Radio {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    poke_r: UnixStream,
}

impl Radio {
    pub fn start(cfg: Config, net: Arc<dyn Network>) -> io::Result<Radio> {
        for u in std::iter::once(&cfg.audio).chain(cfg.songs.iter()).chain(cfg.now.iter()) {
            if let Some(a) = &cfg.allow {
                if !a.iter().any(|o| u.same_origin(o)) {
                    return Err(io::Error::new(
                        ErrorKind::PermissionDenied,
                        format!("{} is not an approved address", u.origin_text()),
                    ));
                }
            }
        }
        let (poke_r, poke_w) = UnixStream::pair()?;
        poke_r.set_nonblocking(true)?;
        poke_w.set_nonblocking(true)?;
        let feed = Feed::with_start(cfg.start_s);
        let shared = Arc::new(Shared {
            feed,
            songs: Mutex::new(Songs::new(None)),
            state: Mutex::new(State {
                phase: Phase::Starting,
                codec: None,
                next: Vec::new(),
                asks: 0,
                resets: 0,
                last_error: None,
                corrupt_cursor: false,
            }),
            stop: Mutex::new(false),
            cv: Condvar::new(),
            cancel: Cancel::default(),
            notes: Mutex::new(Vec::new()),
            poke: Mutex::new(poke_w),
            net,
            cfg,
        });
        let s = shared.clone();
        let t = std::thread::Builder::new().name("abyssd-radio".into()).spawn(move || {
            let mut w = Worker::new(s.clone());
            w.run();
            s.feed.close();
            w.give_net(None);
        })?;
        Ok(Radio { shared, threads: vec![t], poke_r })
    }

    /// The buffer the player plays (stream.rs FeedPlayer).
    pub fn feed(&self) -> Arc<Feed> {
        self.shared.feed.clone()
    }

    /// Ends the session: the player gets `Ended` at once, network requests are aborted, the
    /// lease is released. Waits up to `max_ms` for the thread.
    pub fn stop(&mut self, max_ms: u64) {
        {
            let mut g = self.shared.stop.lock().unwrap_or_else(|e| e.into_inner());
            *g = true;
        }
        self.shared.cv.notify_all();
        self.shared.cancel.cancel();
        self.shared.feed.stop();
        self.shared.set(|s| s.phase = Phase::Stopped);
        let until = Instant::now() + Duration::from_millis(max_ms);
        for t in self.threads.drain(..) {
            while !t.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                warn!("radio thread did not stop within {max_ms} ms");
            }
        }
    }

    pub fn poll_fd(&self) -> BorrowedFd<'_> {
        self.poke_r.as_fd()
    }

    /// Drains the poke socket; returns the notes.
    pub fn on_ready(&mut self) -> Vec<Note> {
        let mut b = [0u8; 64];
        while matches!((&self.poke_r).read(&mut b), Ok(n) if n > 0) {}
        std::mem::take(&mut *self.shared.notes.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// The play you hear right now (any kind), and its wall time.
    fn heard(&self) -> (Option<i64>, Option<Song>) {
        let t = self.shared.feed.now_wall_ms();
        let song = t.and_then(|t| self.shared.songs.lock().ok()?.at(t).cloned());
        (t, song)
    }

    pub fn status(&self) -> RadioStatus {
        let st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let fs = self.shared.feed.status();
        let (heard_ms, play) = self.heard();
        let mut phase = st.phase.clone();
        if phase == Phase::Playing || phase == Phase::Buffering || phase == Phase::Stalled {
            phase = if fs.buffering || (!fs.has_stream && phase != Phase::Playing) {
                Phase::Buffering
            } else if heard_ms.is_some() && fs.buffered_s < 0.05 {
                Phase::Stalled
            } else if heard_ms.is_some() {
                Phase::Playing
            } else {
                phase
            };
        }
        RadioStatus {
            tag: self.shared.cfg.tag.clone(),
            title: self.shared.cfg.title.clone(),
            format: self.shared.cfg.format.clone(),
            codec: st.codec,
            phase,
            other_on: play.as_ref().is_some_and(|p| !p.is_song()),
            song: play.filter(Song::is_song),
            next: st.next.clone(),
            heard_ms,
            buffered_s: fs.buffered_s,
            prebuffer: self.shared.feed.prebuffer(),
            latency_s: self.shared.feed.latency_s(),
            mode: self.shared.cfg.mode,
            asks: st.asks,
            resets: st.resets,
            stalls: fs.stalls,
            last_error: st.last_error.clone(),
        }
    }

    /// The capture chord while the radio plays: the song you hear (matched by sample time).
    pub fn capture(&self) -> RadioCapture {
        let (t, play) = self.heard();
        let Some(t) = t else { return RadioCapture::Nothing("The radio is not playing yet.") };
        match play {
            Some(s) if s.is_song() => RadioCapture::Song(self.capture_of(&s, t)),
            Some(_) => RadioCapture::Nothing("A jingle is on: nothing to save."),
            None => {
                // Not in the tracklist yet: ask the server for that moment and save it then.
                let sh = self.shared.clone();
                let r = std::thread::Builder::new()
                    .name("abyssd-radio-cap".into())
                    .spawn(move || resolve_capture(sh, t));
                match r {
                    Ok(_) => RadioCapture::Pending,
                    Err(_) => RadioCapture::Nothing("The song's name is not known yet."),
                }
            }
        }
    }

    fn capture_of(&self, s: &Song, t: i64) -> Capture {
        capture_record(&self.shared, s, t)
    }

    /// Test aid: the next ask sends an unusable cursor, which the server answers with a reset.
    #[allow(dead_code)] // the PC test tool
    pub fn debug_corrupt_cursor(&self) {
        self.shared.set(|s| s.corrupt_cursor = true);
    }

    /// Test aid: abort whatever request is in flight (a simulated dropped connection).
    #[allow(dead_code)] // the PC test tool
    pub fn debug_drop_connection(&self) {
        self.shared.cancel.cancel();
        self.shared.cancel.reset();
    }
}

impl Drop for Radio {
    fn drop(&mut self) {
        if !self.threads.is_empty() {
            self.stop(300);
        }
    }
}

fn capture_record(sh: &Shared, s: &Song, t: i64) -> Capture {
    let mut fields: std::collections::BTreeMap<String, Value> =
        s.fields.iter().filter(|(k, _)| k.as_str() != "line").map(|(k, v)| (k.clone(), v.clone())).collect();
    if !sh.cfg.tag.is_empty() {
        fields.insert("station".into(), Value::from(sh.cfg.tag.clone()));
    }
    if !sh.cfg.title.is_empty() {
        fields.insert("station_name".into(), Value::from(sh.cfg.title.clone()));
    }
    fields.insert("play_id".into(), s.fields.get("play_id").cloned().unwrap_or(Value::from(s.play_id.clone())));
    fields.insert("from".into(), Value::from("radio"));
    fields.insert("heard_at_ms".into(), Value::from(t));
    fields.insert("into_song_ms".into(), Value::from((t - s.on_air_ms).max(0)));
    Capture::new("radio", s.line(), fields)
}

/// A capture that waited for the song's name: `now?at_ms=t`, then save (or say why not).
fn resolve_capture(sh: Arc<Shared>, t: i64) {
    let Some(now_url) = sh.cfg.now.clone() else {
        sh.note(Note::NotSaved("The song's name is not known yet.".into()));
        return;
    };
    let client = sh.client();
    let mut delay = 1.0f64;
    for _ in 0..12 {
        if sh.stopped() {
            break;
        }
        let hold = sh.net.acquire("radio-capture");
        let r = get_json(&client, &now_url.with_query(&[("at_ms", t.to_string())]), Timeouts::io_s(15));
        hold.release(None);
        match r {
            Ok(v) => {
                let Some(s) = v.get("item").and_then(Song::from_json) else { break };
                if !s.is_song() {
                    sh.note(Note::NotSaved("A jingle was on: nothing saved.".into()));
                    return;
                }
                let c = capture_record(&sh, &s, t);
                let line = c.line.clone();
                match capture::save(&c) {
                    Ok(()) => sh.note(Note::Saved(line)),
                    Err(e) => sh.note(Note::NotSaved(format!("Couldn't save the capture: {e}"))),
                }
                if let Ok(mut songs) = sh.songs.lock() {
                    songs.merge(s);
                }
                return;
            }
            Err(e) if e.to_string().starts_with("HTTP 404") => break,
            Err(_) => {}
        }
        if !sh.sleep(Duration::from_secs_f64(delay)) {
            break;
        }
        delay = (delay * 2.0).min(NET_RETRY_MAX_S);
    }
    sh.note(Note::NotSaved("The song's name could not be fetched.".into()));
}

// ---------------------------------------------------------------------------
// The worker

/// One audio answer.
struct Answer {
    status: u16,
    cursor: Option<String>,
    reset: bool,
    time_ms: i64,
    end_ms: i64,
    seconds: f64,
    head_bytes: Option<usize>,
    retry_after: f64,
    body: Vec<u8>,
    error: Option<String>,
    took: Duration,
}

fn ask(c: &Client, url: &Url) -> io::Result<Answer> {
    let started = Instant::now();
    let r = c.get(url, &[], Timeouts::io_s(ASK_WAIT_S + 15))?;
    let status = r.status;
    let cursor = r.header("x-stream-cursor").map(str::to_string);
    let reset = r.header("x-stream-reset").map(str::trim) == Some("1");
    let time_ms = r.header("x-stream-time-ms").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    let end_ms = r.header("x-stream-end-ms").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    let seconds = r.header_f64("x-stream-seconds").unwrap_or(0.0);
    let head_bytes = r.header_u64("x-stream-head-bytes").map(|v| v as usize);
    let retry_after = r.header_f64("retry-after").unwrap_or(2.0).clamp(1.0, 300.0);
    let body = r.bytes(MAX_AUDIO_BYTES)?;
    let error = (status >= 400)
        .then(|| serde_json::from_slice::<Value>(&body).ok()?.get("error")?.as_str().map(str::to_string))
        .flatten();
    Ok(Answer { status, cursor, reset, time_ms, end_ms, seconds, head_bytes, retry_after, body, error, took: started.elapsed() })
}

struct Worker {
    sh: Arc<Shared>,
    client: Client,
    hold: Option<Box<dyn NetHold>>,
    net_failures: u32,
    audio_url: Option<Url>,
    now_url: Option<Url>,
    /// The last `at_ms` asked for a gap, so one gap is asked once.
    gap_asked: Option<i64>,
    newest_play: Option<String>,
    /// "FLAC" / "MP3" from the format name or Content-Type (None = from the bytes).
    hint: Option<&'static str>,
}

impl Worker {
    fn new(sh: Arc<Shared>) -> Worker {
        let client = sh.client();
        let (audio_url, now_url) = (Some(sh.cfg.audio.clone()), sh.cfg.now.clone());
        let hint = codec_hint(&sh.cfg.format, None);
        Worker { sh, client, hold: None, net_failures: 0, audio_url, now_url, gap_asked: None, newest_play: None, hint }
    }

    fn take_net(&mut self) {
        if self.hold.is_none() {
            self.hold = Some(self.sh.net.acquire("radio"));
        }
    }

    fn give_net(&mut self, next_use_in_ms: Option<u64>) {
        if let Some(h) = self.hold.take() {
            h.release(next_use_in_ms);
        }
        if next_use_in_ms.is_none() {
            self.client.close_idle();
        }
    }

    /// Runs `f` with the network held, retrying forever on errors (fast while the link comes
    /// up, then backing off to 15 s with the lease released in between). None = stopped.
    fn net<T>(&mut self, what: &str, mut f: impl FnMut(&Client) -> io::Result<T>) -> Option<T> {
        let mut delay = 1.0f64;
        loop {
            if self.sh.stopped() {
                return None;
            }
            self.take_net();
            match f(&self.client) {
                Ok(v) => {
                    if self.net_failures > 0 {
                        info!("radio: {what}: back after {} failed tries", self.net_failures);
                    }
                    self.net_failures = 0;
                    return Some(v);
                }
                Err(e) => {
                    if self.sh.stopped() {
                        return None;
                    }
                    self.net_failures += 1;
                    let msg = format!("{what}: {e}");
                    if self.net_failures == 1 || self.net_failures.is_multiple_of(10) {
                        warn!("radio: {msg} (try {})", self.net_failures);
                    }
                    self.sh.set(|s| {
                        s.last_error = Some(msg.clone());
                        if matches!(s.phase, Phase::Starting | Phase::Offline(_)) || self.net_failures > 2 {
                            s.phase = Phase::Offline(e.to_string());
                        }
                    });
                    self.client.close_idle();
                    if self.net_failures <= BRINGUP_TRIES && e.kind() != ErrorKind::InvalidData {
                        // The link may still be coming up: keep the lease, try again soon.
                        if !self.sh.sleep(Duration::from_millis(BRINGUP_RETRY_MS)) {
                            return None;
                        }
                        continue;
                    }
                    self.give_net(Some((delay * 1000.0) as u64));
                    if !self.sh.sleep(Duration::from_secs_f64(delay)) {
                        return None;
                    }
                    delay = (delay * 2.0).min(NET_RETRY_MAX_S);
                }
            }
        }
    }

    fn run(&mut self) {
        let cfg = self.sh.cfg.clone();
        info!("radio: {} ({}) via {} [{:?}]", cfg.title, cfg.tag, cfg.audio, cfg.mode);
        if let Ok(mut songs) = self.sh.songs.lock() {
            *songs = Songs::new(cfg.songs.clone());
        }
        self.sh.set(|s| s.phase = Phase::Buffering);
        match cfg.mode {
            Mode::Bursts => self.bursts(),
            Mode::Endless | Mode::Http => self.endless(),
        }
    }

    /// Tracklist sync in the same wake as the audio; failures never stop the audio.
    fn sync_songs(&mut self) {
        // Sync a copy: the UI keeps reading the list while the request runs.
        let Ok(mut songs) = self.sh.songs.lock().map(|g| Songs { tracklist: g.tracklist.clone(), items: g.items.clone() })
        else {
            return;
        };
        let changed = songs.sync(&self.client).unwrap_or_else(|e| {
            info!("radio: tracklist: {e} (names can wait)");
            false
        });
        let newest = songs.items.last().map(|s| s.play_id.clone());
        if let Ok(mut g) = self.sh.songs.lock() {
            // Merge back (keeps anything a capture lookup added meanwhile).
            for s in songs.items {
                g.merge(s);
            }
        }
        if changed && newest != self.newest_play {
            self.newest_play = newest;
            self.refresh_next();
        }
    }

    /// `now` (no wait): what the rotation plans after the live song ("Up next").
    fn refresh_next(&mut self) {
        let Some(url) = self.now_url.clone() else { return };
        if let Ok(v) = get_json(&self.client, &url, Timeouts::io_s(15)) {
            self.apply_now(&v);
        }
    }

    fn apply_now(&self, v: &Value) {
        let next: Vec<Song> = v
            .get("next")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(planned_song).collect())
            .unwrap_or_default();
        self.sh.set(|s| s.next = next);
        if let Some(item) = v.get("item").and_then(Song::from_json) {
            if let Ok(mut g) = self.sh.songs.lock() {
                g.merge(item);
            }
        }
    }

    /// When nothing in the tracklist covers what you hear, ask the server once for it.
    fn fill_gap(&mut self) {
        let Some(t) = self.sh.feed.now_wall_ms() else { return };
        let covered = self.sh.songs.lock().map(|g| g.at(t).is_some()).unwrap_or(true);
        if covered || self.gap_asked.is_some_and(|g| (t - g).abs() < 30_000) {
            return;
        }
        self.gap_asked = Some(t);
        let Some(url) = self.now_url.clone() else { return };
        match get_json(&self.client, &url.with_query(&[("at_ms", t.to_string())]), Timeouts::io_s(15)) {
            Ok(v) => {
                if let Some(s) = v.get("item").and_then(Song::from_json) {
                    info!("radio: at {t}: {} ({})", s.line(), s.kind);
                    if let Ok(mut g) = self.sh.songs.lock() {
                        g.merge(s);
                    }
                }
            }
            Err(e) => info!("radio: at_ms {t}: {e}"),
        }
    }

    fn bursts(&mut self) {
        let mut cursor = String::new();
        let mut cur: Option<u64> = None;
        let mut last_end: Option<i64> = None;
        let mut rate = 44_100.0f64;
        let mut cutter = Cutter::Flac(Framer::new());
        let fmt_behind = format!("{FIRST_BEHIND_S}");
        while !self.sh.stopped() {
            let fs = self.sh.feed.status();
            // Never ask while plenty is in hand (a paused player): wait for it to play down.
            if fs.has_stream && fs.latest_started && fs.buffered_s > HEADROOM_S + 1.0 {
                let wait = (fs.buffered_s - HEADROOM_S).clamp(0.5, MAX_BUFFER_S);
                self.give_net(Some((wait * 1000.0) as u64));
                if !self.sh.sleep(Duration::from_secs_f64(wait)) {
                    return;
                }
                continue;
            }
            let start_s = self.sh.cfg.start_s;
            let need = if cur.is_none() || fs.latest_started { 1.0 } else { (start_s - fs.latest_received_s).max(1.0) };
            let mut send_cursor = cursor.clone();
            self.sh.set(|s| {
                if s.corrupt_cursor && !send_cursor.is_empty() {
                    send_cursor = format!("{send_cursor}x");
                    s.corrupt_cursor = false;
                }
            });
            let Some(base) = self.audio_url.clone() else { return };
            let url = base.with_query(&[
                ("cursor", send_cursor),
                ("cid", self.sh.cfg.cid.clone()),
                ("behind", fmt_behind.clone()),
                ("min_s", format!("{}", (need * 1000.0).round() / 1000.0)),
                ("wait", ASK_WAIT_S.to_string()),
            ]);
            let Some(a) = self.net("audio", |c| ask(c, &url)) else { return };
            self.sh.set(|s| s.asks += 1);
            self.sync_songs(); // same wake as the audio
            match a.status {
                200 => {}
                204 => {
                    if let Some(c) = a.cursor {
                        cursor = c;
                    }
                    // The server held the ask; only pace a server that answers at once.
                    if a.took < Duration::from_secs(1) && !self.sh.sleep(Duration::from_secs(1)) {
                        return;
                    }
                    continue;
                }
                503 => {
                    info!("radio: station not encoding (503), retry in {:.0} s", a.retry_after);
                    self.sh.set(|s| {
                        s.phase = Phase::OffAir;
                        s.last_error = a.error.clone().or(Some("not on air".into()));
                    });
                    self.give_net(Some((a.retry_after * 1000.0) as u64));
                    if !self.sh.sleep(Duration::from_secs_f64(a.retry_after)) {
                        return;
                    }
                    continue;
                }
                s => {
                    let why = format!("HTTP {s}: {}", a.error.as_deref().unwrap_or("?"));
                    warn!("radio: audio {why}");
                    self.sh.set(|st| st.last_error = Some(why));
                    if (400..500).contains(&s) && s != 429 {
                        cursor.clear(); // an unusable ask: start a new stream
                    }
                    self.give_net(Some(2000));
                    if !self.sh.sleep(Duration::from_secs(2)) {
                        return;
                    }
                    continue;
                }
            }
            let mut body: &[u8] = &a.body;
            if a.reset || cur.is_none() {
                let head = match open_stream(body, self.hint, true) {
                    Ok(Some((codec, n))) => {
                        let n = match codec {
                            Codec::Flac(_) => a.head_bytes.filter(|h| *h <= body.len()).unwrap_or(n).max(n),
                            Codec::Mp3 { .. } => n,
                        };
                        Some((codec, n))
                    }
                    Ok(None) => None,
                    Err(e) => {
                        warn!("radio: reset answer: {e}");
                        None
                    }
                };
                let Some((info, head_len)) = head else {
                    cursor.clear();
                    if !self.sh.sleep(Duration::from_secs(2)) {
                        return;
                    }
                    continue;
                };
                if cur.is_some() {
                    info!("radio: new stream (reset): the old one plays out, then a new decoder starts");
                    self.sh.set(|s| s.resets += 1);
                }
                body = &body[head_len..];
                rate = f64::from(info.sample_rate());
                cutter = Cutter::for_codec(&info);
                let name = info.name();
                self.sh.set(|s| s.codec = Some(name));
                cur = Some(self.sh.feed.begin_stream(info));
            }
            if let (false, Some(e)) = (a.reset, last_end) {
                if a.time_ms != e && (a.time_ms - e).abs() < 20 {
                    info!("radio: clock jitter {} ms between answers", a.time_ms - e);
                }
            }
            last_end = Some(a.end_ms);
            match a.cursor {
                Some(c) => cursor = c,
                None => {
                    warn!("radio: answer without a cursor: next ask starts over");
                    cursor.clear();
                }
            }
            cutter.push(body);
            let mut frames = Vec::new();
            while let Some(f) = cutter.next() {
                frames.push(f);
            }
            frames.extend(cutter.finish());
            let skipped = cutter.take_skipped();
            if skipped > 0 {
                warn!("radio: {skipped} bytes of an answer were not frames");
            }
            let got: f64 = frames.iter().map(|f| f64::from(f.samples)).sum::<f64>() / rate;
            if a.seconds > 0.0 && (got - a.seconds).abs() > 0.2 && self.sh.feed.status().has_stream {
                info!("radio: answer said {:.3} s, frames hold ~{got:.3} s", a.seconds);
            }
            let Some(id) = cur else { continue };
            if !self.sh.feed.push(id, frames, Some(a.time_ms)) {
                return; // stopped
            }
            let fs = self.sh.feed.status();
            if !fs.latest_started {
                self.sh.set(|s| s.phase = Phase::Buffering);
                continue; // keep buffering: min_s makes the server wait
            }
            self.sh.set(|s| {
                s.phase = Phase::Playing;
                s.last_error = None;
            });
            self.fill_gap();
            let sleep = (fs.buffered_s - HEADROOM_S).max(0.5);
            self.give_net(Some((sleep * 1000.0) as u64));
            if !self.sh.sleep(Duration::from_secs_f64(sleep)) {
                return;
            }
        }
    }

    fn endless(&mut self) {
        // The song: a long poll in its own thread (the audio connection is busy).
        let sh = self.sh.clone();
        let now_url = self.now_url.clone();
        let poller = std::thread::Builder::new()
            .name("abyssd-radio-now".into())
            .spawn(move || long_poll(sh, now_url))
            .ok();
        let Some(base) = self.audio_url.clone() else { return };
        let url = if self.sh.cfg.mode == Mode::Http {
            base // as given
        } else {
            base.with_query(&[("cid", self.sh.cfg.cid.clone()), ("behind", format!("{FIRST_BEHIND_S}"))])
        };
        let mut delay = 1.0f64;
        while !self.sh.stopped() {
            self.take_net();
            // Names before sound: the song is known from the first sample on.
            self.sync_songs();
            let started = Instant::now();
            let ended = self.endless_once(&url);
            if self.sh.stopped() {
                break;
            }
            // A stream that ran for a while reconnects at once; quick failures back off.
            if started.elapsed() > Duration::from_secs(30) {
                delay = 1.0;
            }
            info!("radio: endless stream ended ({ended}); reconnecting in {delay:.0} s");
            self.sh.set(|s| s.last_error = Some(ended));
            if !self.sh.sleep(Duration::from_secs_f64(delay)) {
                break;
            }
            delay = (delay * 2.0).min(NET_RETRY_MAX_S);
        }
        self.sh.cv.notify_all();
        if let Some(p) = poller {
            let _ = p.join();
        }
    }

    /// One endless answer, read until it ends. Returns why it ended.
    fn endless_once(&mut self, url: &Url) -> String {
        let r = match self.client.get(url, &[], Timeouts::io_s(20)) {
            Ok(r) => r,
            Err(e) => {
                self.sh.set(|s| s.phase = Phase::Offline(e.to_string()));
                return format!("connect: {e}");
            }
        };
        self.sh.set(|s| s.asks += 1);
        if r.status == 503 {
            let ra = r.header_f64("retry-after").unwrap_or(5.0).clamp(1.0, 300.0);
            self.sh.set(|s| s.phase = Phase::OffAir);
            drop(r);
            self.sh.sleep(Duration::from_secs_f64(ra));
            return "not on air (503)".into();
        }
        if r.status != 200 {
            return format!("HTTP {}", r.status);
        }
        // A plain stream has no X-Stream-Time-Ms: its own clock starts now.
        let time_ms: i64 = r.header("x-stream-time-ms").and_then(|v| v.trim().parse().ok()).unwrap_or_else(wall_now_ms);
        self.hint = self.hint.or_else(|| codec_hint("", r.header("content-type")));
        let mut rd = r.into_reader();
        let mut buf = vec![0u8; 64 * 1024];
        let mut head: Vec<u8> = Vec::new();
        let mut fr = Cutter::Flac(Framer::new());
        let mut id: Option<u64> = None;
        let mut first = true;
        let hint = self.hint;
        loop {
            if self.sh.stopped() {
                return "stopped".into();
            }
            // Backpressure: a paused player must not make the buffer grow without end.
            while self.sh.feed.status().buffered_s > MAX_BUFFER_S {
                if !self.sh.sleep(Duration::from_secs(1)) {
                    return "stopped".into();
                }
            }
            let n = match rd.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted && !self.sh.stopped() => continue,
                Err(e) => {
                    if let Some(i) = id {
                        let tail: Vec<Frame> = fr.finish().into_iter().collect();
                        self.sh.feed.push(i, tail, None);
                    }
                    return format!("read: {e}");
                }
            };
            let mut data = &buf[..n];
            if id.is_none() {
                head.extend_from_slice(data);
                match open_stream(&head, hint, false) {
                    Ok(None) if head.len() < 1 << 20 => continue,
                    Ok(None) => return "no audio frames at the start of the stream".into(),
                    Ok(Some((codec, len))) => {
                        if self.sh.feed.status().has_stream {
                            self.sh.set(|s| s.resets += 1);
                        }
                        fr = Cutter::for_codec(&codec);
                        let name = codec.name();
                        self.sh.set(|s| s.codec = Some(name));
                        id = Some(self.sh.feed.begin_stream(codec));
                        fr.push(&head[len..]);
                        head.clear();
                        data = &[];
                    }
                    Err(e) => return format!("not audio we can play: {e}"),
                }
            }
            fr.push(data);
            let mut frames = Vec::new();
            while let Some(f) = fr.next() {
                frames.push(f);
            }
            if frames.is_empty() {
                continue;
            }
            let Some(i) = id else { continue };
            let mark = if first { Some(time_ms) } else { None };
            first = false;
            if !self.sh.feed.push(i, frames, mark) {
                return "stopped".into();
            }
            let fs = self.sh.feed.status();
            self.sh.set(|s| {
                s.phase = if fs.latest_started { Phase::Playing } else { Phase::Buffering };
            });
        }
        if let Some(i) = id {
            let tail: Vec<Frame> = fr.finish().into_iter().collect();
            self.sh.feed.push(i, tail, None);
        }
        "the server closed the stream".into()
    }
}

fn wall_now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Cuts the frames of one stream, whatever its codec.
enum Cutter {
    Flac(Framer),
    Mp3(Mp3Framer),
}

impl Cutter {
    fn for_codec(c: &Codec) -> Cutter {
        match c {
            Codec::Flac(_) => Cutter::Flac(Framer::new()),
            Codec::Mp3 { .. } => Cutter::Mp3(Mp3Framer::new()),
        }
    }
    fn push(&mut self, b: &[u8]) {
        match self {
            Cutter::Flac(f) => f.push(b),
            Cutter::Mp3(f) => f.push(b),
        }
    }
    fn next(&mut self) -> Option<Frame> {
        match self {
            Cutter::Flac(f) => f.next().map(Frame::from),
            Cutter::Mp3(f) => f.next().map(|m| Frame { bytes: m.bytes, samples: m.head.samples, number: f.frames }),
        }
    }
    fn finish(&mut self) -> Option<Frame> {
        match self {
            Cutter::Flac(f) => f.finish().map(Frame::from),
            Cutter::Mp3(f) => f.finish().map(|m| Frame { bytes: m.bytes, samples: m.head.samples, number: f.frames }),
        }
    }
    fn take_skipped(&mut self) -> u64 {
        match self {
            Cutter::Flac(f) => std::mem::take(&mut f.skipped),
            Cutter::Mp3(f) => std::mem::take(&mut f.skipped),
        }
    }
}

/// The codec at the start of a stream and where its frames start. Ok(None) = need more
/// bytes (`complete` = these are all there are).
fn open_stream(b: &[u8], hint: Option<&str>, complete: bool) -> Result<Option<(Codec, usize)>, String> {
    let flac = b.starts_with(b"fLaC") || (hint == Some("FLAC") && b.len() < 4 && b"fLaC".starts_with(b));
    if flac {
        return match parse_head(b) {
            Ok(Some((info, n))) => Ok(Some((Codec::Flac(info), n))),
            Ok(None) if complete => Err("cut-off FLAC head".into()),
            Ok(None) => Ok(None),
            Err(e) => Err(e.to_string()),
        };
    }
    if b.len() < 4 && !complete {
        return Ok(None);
    }
    let mut f = Mp3Framer::new();
    f.push(b);
    let first = if complete { f.next().or_else(|| f.finish()) } else { f.next() };
    match first {
        Some(m) => Ok(Some((Codec::Mp3 { sample_rate: m.head.sample_rate, channels: m.head.channels }, 0))),
        None if complete || b.len() > 256 * 1024 => Err("neither FLAC nor MP3".into()),
        None => Ok(None),
    }
}

/// A planned song in `next` has no play_id yet: give it a stable stand-in.
fn planned_song(v: &Value) -> Option<Song> {
    if let Some(s) = Song::from_json(v) {
        return Some(s);
    }
    let mut o = v.as_object()?.clone();
    let tid = o.get("track_id").and_then(id_text).unwrap_or_default();
    o.insert("play_id".into(), Value::from(format!("planned-{tid}")));
    o.entry("on_air_ms").or_insert(Value::from(0));
    Song::from_json(&Value::Object(o))
}

/// Endless mode: `now?after=<play_id>&wait=55`, one quiet connection per song.
fn long_poll(sh: Arc<Shared>, now_url: Option<Url>) {
    let Some(url) = now_url else { return };
    let client = sh.client();
    let tracklist = sh.songs.lock().ok().and_then(|g| g.tracklist.clone());
    let mut songs_sync = Songs::new(tracklist);
    let hold = sh.net.acquire("radio-now");
    let mut after = "0".to_string();
    let mut delay = 1.0f64;
    while !sh.stopped() {
        let q = [("after", after.clone()), ("wait", POLL_WAIT_S.to_string())];
        match get_json(&client, &url.with_query(&q), Timeouts::io_s(POLL_WAIT_S + 15)) {
            Ok(v) => {
                delay = 1.0;
                let next: Vec<Song> = v
                    .get("next")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(planned_song).collect())
                    .unwrap_or_default();
                sh.set(|s| s.next = next);
                if let Some(item) = v.get("item").and_then(Song::from_json) {
                    if item.play_id != after {
                        info!("radio: on air: {} ({})", item.line(), item.kind);
                        after = item.play_id.clone();
                    }
                    if let Ok(mut g) = sh.songs.lock() {
                        g.merge(item);
                    }
                }
                // The tracklist brings the final ends_at_ms of the song before.
                songs_sync.items = sh.songs.lock().map(|g| g.items.clone()).unwrap_or_default();
                if songs_sync.sync(&client).is_ok() {
                    if let Ok(mut g) = sh.songs.lock() {
                        for s in songs_sync.items.drain(..) {
                            g.merge(s);
                        }
                    }
                }
            }
            Err(e) => {
                if sh.stopped() {
                    break;
                }
                info!("radio: now poll: {e}");
                if !sh.sleep(Duration::from_secs_f64(delay)) {
                    break;
                }
                delay = (delay * 2.0).min(NET_RETRY_MAX_S);
            }
        }
    }
    hold.release(None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn song(id: i64, start: i64, end: Option<i64>, est: bool, kind: &str) -> Value {
        let mut v = json!({"play_id": id, "kind": kind, "line": format!("A - T{id}"),
            "started_at_ms": start, "on_air_ms": start + 500, "duration_ms": 1000});
        if let Some(e) = end {
            v["ends_at_ms"] = json!(e);
        }
        if est {
            v["ends_at_estimated"] = json!(true);
        }
        v
    }

    #[test]
    fn songs_merge_by_play_id_and_match_by_time() {
        let mut s = Songs::new(None);
        let (ch, next) = s.apply_page(&json!({"items": [song(1, 0, Some(10_500), false, "music"),
            song(2, 10_000, Some(20_500), true, "music")], "more": false, "paging": {"since_ms": 1}}));
        assert!(ch);
        assert_eq!(next, None);
        assert_eq!(s.at(100), None, "before the first on_air_ms");
        assert_eq!(s.at(600).unwrap().play_id, "1");
        assert_eq!(s.at(10_499).unwrap().play_id, "1", "blend: on_air of #2 is 10_500");
        assert_eq!(s.at(10_500).unwrap().play_id, "2");
        assert_eq!(s.at(99_000).unwrap().play_id, "2", "an estimated end stays open");
        // since_ms is inclusive: #2 comes again with its final end, then #3 (a jingle).
        let (ch, next) = s.apply_page(&json!({"items": [song(2, 10_000, Some(19_000), false, "music"),
            song(3, 18_500, Some(25_000), false, "jingle")], "more": true, "paging": {"since_ms": 18_500}}));
        assert!(ch);
        assert_eq!(next, Some(18_500));
        assert_eq!(s.items.len(), 3);
        assert_eq!(s.at(18_900).unwrap().play_id, "2", "the jingle is on air from 19_000");
        assert!(!s.at(19_100).unwrap().is_song(), "the jingle: shown as nothing");
        assert_eq!(s.at(26_000), None, "after a final end: unknown");
        let (ch, _) = s.apply_page(&json!({"items": [song(2, 10_000, Some(19_000), false, "music")]}));
        assert!(!ch, "the same item again changes nothing");
    }

    #[test]
    fn unknown_kinds_and_missing_fields() {
        let s = Song::from_json(&json!({"play_id": 7, "kind": "x-new", "on_air_ms": 5})).unwrap();
        assert!(!s.is_song());
        assert_eq!(s.ends_at_ms, None);
        assert_eq!(s.line(), "");
        let s = Song::from_json(&json!({"play_id": "p", "kind": "live", "started_at_ms": 9,
            "artist": "DJ", "title": "Set", "new_field": {"a": 1}})).unwrap();
        assert!(s.is_song());
        assert_eq!((s.on_air_ms, s.line().as_str()), (9, "DJ - Set"));
        assert!(Song::from_json(&json!({"kind": "music"})).is_none(), "no play_id: not a play");
        let mut l = Songs::new(None);
        for i in 0..60 {
            l.merge(Song::from_json(&song(i, i * 10, None, false, "music")).unwrap());
        }
        assert_eq!(l.items.len(), KEEP_SONGS);
        assert_eq!(l.items[0].play_id, "10");
        let p = planned_song(&json!({"kind": "music", "track_id": 40033, "planned": true, "line": "C - P"})).unwrap();
        assert_eq!((p.play_id.as_str(), p.line().as_str()), ("planned-40033", "C - P"));
    }

    #[test]
    fn modes_and_cid() {
        assert_eq!(codec_hint("flac", None), Some("FLAC"));
        assert_eq!(codec_hint("mp3-128", None), Some("MP3"));
        assert_eq!(codec_hint("x", Some("audio/mpeg")), Some("MP3"));
        assert_eq!(codec_hint("x", None), None);
        let c = random_cid();
        assert_eq!(c.len(), 12);
        assert_ne!(c, random_cid());
        let u = Url::parse("http://h:1/a").unwrap();
        assert_eq!(Config::new(u).start_s, START_S);
    }

    #[test]
    fn stream_start_detection() {
        use crate::flac_stream::testenc::head;
        use crate::mp3_stream::testenc::frame;
        let h = head(4608);
        assert!(matches!(open_stream(&h, None, true), Ok(Some((Codec::Flac(_), 42)))));
        assert!(matches!(open_stream(&h[..10], None, false), Ok(None)));
        assert!(open_stream(&h[..10], None, true).is_err());
        let m = [frame(14, false), frame(14, false)].concat();
        assert!(matches!(open_stream(&m, None, true), Ok(Some((Codec::Mp3 { sample_rate: 44100, .. }, 0)))));
        assert!(open_stream(&[0u8; 600], None, true).is_err());
    }
}
