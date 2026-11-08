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
//! **Long bursts** (radio #84): when the station's audio entry advertises `long_bursts` (the
//! server's ring and limits), asks carry `long=1`: the first answer starts ~4-4.5 min behind
//! live ([`long_hold_s`]: 270 s, less if the server allows less or the compressed audio would
//! pass [`LONG_MAX_BYTES`]), and the next ask goes out when ~45 s is left, so Wi-Fi wakes about
//! every 3.5-4 min. `max_s` caps an answer so the buffer never passes the hold (after a pause
//! or a stall). The audio is kept compressed (decoded a frame at a time by the player) and a
//! long answer goes to the feed as it arrives: playback starts after the first 15 s, not after
//! the whole download. An answer cut off mid-body is asked again with the same cursor and the
//! frames already fed are skipped (a reset answer: skipped by wall time), so nothing repeats.
//! Without `long_bursts` everything is exactly the classic loop above.
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
/// The first tracklist sync of a stream: this many recent plays (what played before you tuned in).
const FIRST_SYNC_SONGS: usize = 20;
/// `$stream.played` / `$stream.next` hold at most this many songs.
const PLAYED_MAX: usize = 30;
const COMING_MAX: usize = 3;
/// The server holds an ask up to this (`wait`, max 15).
const ASK_WAIT_S: u64 = 15;
/// Long poll (`wait`, max 55).
const POLL_WAIT_S: u64 = 55;
/// Never hold more than this unplayed (a paused player must not make the buffer grow).
const MAX_BUFFER_S: f64 = 45.0;
/// Largest answer body accepted (30 s of FLAC is ~4-5 MB).
const MAX_AUDIO_BYTES: usize = 24 << 20;
const MAX_JSON_BYTES: usize = 4 << 20;
/// Long bursts: the longest hold (the first answer, and the latency behind live).
pub const LONG_HOLD_S: f64 = 270.0;
/// Long bursts: ask again when this much is left (less while the buffer is short).
pub const LONG_HEADROOM_S: f64 = 45.0;
/// Long bursts: never hold more compressed audio than this (FLAC ~4 min, MP3 320 the hold).
pub const LONG_MAX_BYTES: f64 = 40.0 * 1024.0 * 1024.0;
/// What one answer may bring beyond the hold (`max_s` = hold + this - what is held).
const LONG_SLACK_S: f64 = 15.0;
/// Long bursts only beat the classic loop when the server lets a stream start further back.
const CLASSIC_MAX_BEHIND_S: f64 = 30.0;
/// A long answer goes to the feed in pieces of about this much audio.
const LONG_PIECE_S: f64 = 2.0;

/// The server's long-burst limits (`long_bursts` of a station's audio entry, radio #84).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LongBursts {
    pub ring_s: f64,
    pub max_behind_s: f64,
    pub max_min_s: f64,
    pub max_wait_s: f64,
}

impl LongBursts {
    /// From the advertised object; None when it is missing, malformed or no better than the
    /// classic limits (then the classic loop runs).
    pub fn from_json(v: &Value) -> Option<LongBursts> {
        let n = |k: &str| v.get(k).and_then(Value::as_f64).filter(|x| x.is_finite() && *x > 0.0);
        let lb = LongBursts {
            ring_s: n("ring_s")?,
            max_behind_s: n("max_behind_s")?,
            max_min_s: n("max_min_s").unwrap_or(1.0),
            max_wait_s: n("max_wait_s").unwrap_or(ASK_WAIT_S as f64),
        };
        (lb.max_behind_s > CLASSIC_MAX_BEHIND_S && lb.ring_s > lb.max_behind_s).then_some(lb)
    }
}

/// Bytes per second of a stream before its first answer is measured (on the high side, so
/// the first hold never passes the RAM cap).
pub fn guess_bytes_per_s(format: &str) -> f64 {
    let f = format.to_ascii_lowercase();
    match codec_hint(&f, None) {
        Some("MP3") if f.contains("128") => 17_000.0,
        Some("MP3") => 41_000.0,
        _ => 160_000.0,
    }
}

/// How much a long-burst stream holds (= how far behind live it starts): [`LONG_HOLD_S`],
/// capped by the server's `max_behind_s` and by [`LONG_MAX_BYTES`] at `bytes_per_s`.
pub fn long_hold_s(lb: &LongBursts, bytes_per_s: f64) -> f64 {
    let by_ram = LONG_MAX_BYTES / bytes_per_s.max(1.0) - LONG_SLACK_S;
    LONG_HOLD_S.min(lb.max_behind_s).min(by_ram).max(FIRST_BEHIND_S)
}

/// Ask again when this much is left: a fifth of what the last answer left in hand, within
/// [`HEADROOM_S`]..[`LONG_HEADROOM_S`] (a short stream after a server restart asks like the
/// classic loop instead of all the time).
pub fn long_headroom_s(peak_s: f64) -> f64 {
    (0.2 * peak_s).clamp(HEADROOM_S, LONG_HEADROOM_S)
}

/// `max_s` of a long ask: what may come without the buffer passing the hold.
pub fn long_max_s(hold_s: f64, held_s: f64) -> f64 {
    (hold_s + LONG_SLACK_S - held_s).max(5.0)
}

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
    /// The server's long-burst limits for this format (None = classic bursts, as before).
    pub long: Option<LongBursts>,
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
            long: None,
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
        c.long = st.long_bursts.get(format).copied();
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
                None => vec![("limit", FIRST_SYNC_SONGS.to_string())],
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

    /// Around what you hear: (the songs that played before it, newest first; the songs you hear
    /// after it, oldest first, then the planned ones). `t` = the wall time of what you hear
    /// (None while starting: everything but the newest play counts as before), `heard` = its
    /// play. Songs and live sets only; planned songs already in the list are left out.
    pub fn around(&self, t: Option<i64>, heard: Option<&Song>, planned: &[Song]) -> (Vec<Song>, Vec<Song>) {
        let split = match (heard, t) {
            (Some(h), _) => h.on_air_ms,
            (None, Some(t)) => t,
            (None, None) => self.items.last().map(|s| s.on_air_ms).unwrap_or(i64::MAX),
        };
        let heard_id = heard.map(|h| h.play_id.as_str());
        let mut played = Vec::new();
        let mut coming = Vec::new();
        for s in &self.items {
            if !s.is_song() || Some(s.play_id.as_str()) == heard_id {
                continue;
            }
            if s.on_air_ms < split || (heard.is_none() && t.is_some() && s.on_air_ms <= split) {
                played.push(s.clone());
            } else {
                coming.push(s.clone());
            }
        }
        played.reverse();
        played.truncate(PLAYED_MAX);
        let track = |s: &Song| s.fields.get("track_id").and_then(id_text);
        for p in planned {
            let dup = track(p).is_some_and(|tid| {
                coming.iter().chain(heard).any(|c| track(c).as_deref() == Some(tid.as_str()) && c.on_air_ms >= split)
            });
            if p.is_song() && !dup {
                coming.push(p.clone());
            }
        }
        coming.truncate(COMING_MAX);
        (played, coming)
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
    /// `audio.<format>.long_bursts`, where the server offers them.
    pub long_bursts: std::collections::BTreeMap<String, LongBursts>,
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
        let long_bursts = st
            .get("audio")
            .and_then(Value::as_object)
            .map(|a| a.iter().filter_map(|(k, f)| Some((k.clone(), LongBursts::from_json(f.get("long_bursts")?)?))).collect())
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
            long_bursts,
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
    /// The songs before the one you hear, newest first (`$stream.played`).
    pub played: Vec<Song>,
    /// The songs you hear after it: the tracklist's newer plays, then the planned ones
    /// (`$stream.next`).
    pub coming: Vec<Song>,
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
        let (played, coming) = self
            .shared
            .songs
            .lock()
            .map(|g| g.around(heard_ms, play.as_ref(), &st.next))
            .unwrap_or_default();
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
            played,
            coming,
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
    let mut a = answer_head(&r, started);
    a.body = r.bytes(MAX_AUDIO_BYTES)?;
    a.error = (a.status >= 400)
        .then(|| serde_json::from_slice::<Value>(&a.body).ok()?.get("error")?.as_str().map(str::to_string))
        .flatten();
    a.took = started.elapsed();
    Ok(a)
}

/// An answer's status and headers (body empty).
fn answer_head(r: &crate::http::Response<'_>, started: Instant) -> Answer {
    let status = r.status;
    let cursor = r.header("x-stream-cursor").map(str::to_string);
    let reset = r.header("x-stream-reset").map(str::trim) == Some("1");
    let time_ms = r.header("x-stream-time-ms").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    let end_ms = r.header("x-stream-end-ms").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    let seconds = r.header_f64("x-stream-seconds").unwrap_or(0.0);
    let head_bytes = r.header_u64("x-stream-head-bytes").map(|v| v as usize);
    let retry_after = r.header_f64("retry-after").unwrap_or(2.0).clamp(1.0, 300.0);
    Answer {
        status,
        cursor,
        reset,
        time_ms,
        end_ms,
        seconds,
        head_bytes,
        retry_after,
        body: Vec::new(),
        error: None,
        took: started.elapsed(),
    }
}

/// Where a long answer that broke off had got to: the next answer skips what was fed.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Partial {
    /// The feed stream it went to.
    stream: u64,
    /// Its X-Stream-Time-Ms (server time of its first sample).
    time_ms: i64,
    /// Samples of it that reached the feed (counted from its first sample).
    samples: u64,
    /// Server time of the end of what reached the feed.
    end_ms: i64,
}

/// What of an answer the feed already has. After `p` broke off, the same stream asked with the
/// same cursor starts where `p` started: skip exactly the samples it fed (`Ok`). A reset (a new
/// stream: a cut-off first answer asked again, an unusable cursor, an encoder restart) skips
/// by wall time up to the end of everything fed (`Err(end_ms)`), so the new stream begins where
/// the old one ends instead of replaying the headroom.
fn resume_skip(p: Option<Partial>, reset: bool, stream: Option<u64>, fed_end_ms: Option<i64>) -> Option<Result<u64, i64>> {
    if !reset {
        return p.filter(|p| stream == Some(p.stream)).map(|p| Ok(p.samples));
    }
    p.map(|p| p.end_ms).into_iter().chain(fed_end_ms).max().map(Err)
}

/// The long-burst side of the worker (classic bursts keep their own locals in `bursts`).
struct LongState {
    lb: LongBursts,
    cursor: String,
    /// The feed stream answers go to.
    cur: Option<u64>,
    codec: Option<Codec>,
    rate: f64,
    /// Measured bytes per second of audio (for the RAM cap), starting from a guess.
    bytes_per_s: f64,
    /// What the last answer left in hand (s): sets the headroom.
    peak_s: f64,
    partial: Option<Partial>,
    /// Server time of the end of everything fed so far.
    fed_end_ms: Option<i64>,
}

impl LongState {
    fn hold_s(&self) -> f64 {
        long_hold_s(&self.lb, self.bytes_per_s)
    }
    fn headroom_s(&self) -> f64 {
        long_headroom_s(self.peak_s)
    }
}

/// One long ask, read as it comes: frames go to the feed in pieces (playback can start after
/// the first seconds; the body is never held twice). The query is built here, so a retry
/// after an error asks with what holds then. Ok = the whole answer was read (for 200, every
/// frame is in the feed; `body` is empty).
fn long_ask(c: &Client, base: &Url, sh: &Shared, hint: Option<&'static str>, st: &mut LongState) -> io::Result<Answer> {
    let fs = sh.feed.status();
    let starting = st.cur.is_none() || !fs.latest_started;
    let hold = st.hold_s();
    let need = if !starting { 1.0 } else { (sh.cfg.start_s - fs.latest_received_s).max(1.0) };
    // What the answer may add to the RAM: the part it repeats is skipped, not kept. Without a
    // cursor the answer is a reset from `behind` back, cut where the feed ends (so up to the
    // hold); asked again after a cut-off, the same answer comes again and its fed part is skipped.
    let repeat_s = st
        .partial
        .filter(|p| Some(p.stream) == st.cur)
        .map_or(0.0, |p| p.samples as f64 / st.rate.max(1.0));
    let held = if st.cur.is_some() && !st.cursor.is_empty() { (fs.buffered_s - repeat_s).max(0.0) } else { 0.0 };
    let mut cursor = st.cursor.clone();
    sh.set(|s| {
        if s.corrupt_cursor && !cursor.is_empty() {
            cursor = format!("{cursor}x");
            s.corrupt_cursor = false;
        }
    });
    let wait = st.lb.max_wait_s.clamp(1.0, 30.0).round() as u64;
    let url = base.with_query(&[
        ("cursor", cursor),
        ("cid", sh.cfg.cid.clone()),
        ("long", "1".into()),
        ("behind", format!("{}", hold.round())),
        ("min_s", format!("{}", (need.min(st.lb.max_min_s) * 1000.0).round() / 1000.0)),
        ("max_s", format!("{}", long_max_s(hold, held).round())),
        ("wait", wait.to_string()),
    ]);
    let started = Instant::now();
    let mut r = c.get(&url, &[], Timeouts::io_s(wait + 15))?;
    let mut a = answer_head(&r, started);
    if a.status != 200 {
        a.body = r.bytes(MAX_JSON_BYTES)?;
        a.error = serde_json::from_slice::<Value>(&a.body).ok().and_then(|v| v.get("error")?.as_str().map(str::to_string));
        a.took = started.elapsed();
        return Ok(a);
    }
    let skip = resume_skip(st.partial, a.reset, st.cur, st.cur.and(st.fed_end_ms));
    let mut buf = vec![0u8; 64 * 1024];
    let mut head: Vec<u8> = Vec::new();
    let mut stream = (!a.reset).then_some(st.cur).flatten();
    let mut cutter = st.codec.as_ref().map(Cutter::for_codec);
    if stream.is_none() {
        cutter = None; // a new stream: opened on its head below
    }
    let mut pos: u64 = 0; // samples of this answer seen (fed or skipped)
    let mut fed: u64 = 0; // of those, how many reached the feed (counted from the answer's start)
    let mut marked = false;
    let mut piece: Vec<Frame> = Vec::new();
    let mut piece_samples: u64 = 0;
    let mut body_bytes: u64 = 0;
    let piece_max = (LONG_PIECE_S * st.rate.max(8000.0)) as u64;
    let limit = (2.0 * LONG_MAX_BYTES) as u64;
    let result: io::Result<()> = (|| {
        loop {
            let n = r.read_some(&mut buf)?;
            body_bytes += n as u64;
            if body_bytes > limit {
                return Err(io::Error::new(ErrorKind::InvalidData, "long answer bigger than the RAM cap"));
            }
            let at_end = n == 0;
            let mut data = &buf[..n];
            if cutter.is_none() {
                head.extend_from_slice(data);
                data = &[];
                match open_stream(&head, hint, at_end) {
                    Ok(None) if head.len() < 1 << 20 => {}
                    Ok(None) => return Err(io::Error::new(ErrorKind::InvalidData, "no audio frames at the start of the answer")),
                    Ok(Some((info, n0))) => {
                        let n0 = match info {
                            Codec::Flac(_) => a.head_bytes.filter(|h| *h <= head.len()).unwrap_or(n0).max(n0),
                            Codec::Mp3 { .. } => n0,
                        };
                        if st.cur.is_some() {
                            info!("radio: new stream (reset): the old one plays out, then a new decoder starts");
                            sh.set(|s| s.resets += 1);
                        }
                        st.rate = f64::from(info.sample_rate());
                        let mut ct = Cutter::for_codec(&info);
                        ct.push(&head[n0..]);
                        cutter = Some(ct);
                        let name = info.name();
                        sh.set(|s| s.codec = Some(name));
                        let id = sh.feed.begin_stream(info.clone());
                        st.codec = Some(info);
                        st.cur = Some(id);
                        stream = Some(id);
                        head = Vec::new();
                    }
                    Err(e) => return Err(io::Error::new(ErrorKind::InvalidData, format!("reset answer: {e}"))),
                }
            }
            let Some(ct) = cutter.as_mut() else {
                if at_end {
                    return Err(io::Error::new(ErrorKind::InvalidData, "cut-off head"));
                }
                continue;
            };
            ct.push(data);
            let mut got: Vec<Frame> = Vec::new();
            while let Some(f) = ct.next() {
                got.push(f);
            }
            if at_end {
                got.extend(ct.finish());
                let skipped = ct.take_skipped();
                if skipped > 0 {
                    warn!("radio: {skipped} bytes of an answer were not frames");
                }
            }
            let rate = st.rate.max(1.0);
            for f in got {
                let start = pos;
                pos += u64::from(f.samples);
                let keep = match skip {
                    Some(Ok(samples)) => start >= samples,
                    // Wall time: keep a frame unless most of it was already fed.
                    Some(Err(end_ms)) => {
                        a.time_ms + ((start as f64 + f64::from(f.samples) / 2.0) * 1000.0 / rate) as i64 >= end_ms
                    }
                    None => true,
                };
                if !keep {
                    fed = pos;
                    continue;
                }
                piece_samples += u64::from(f.samples);
                piece.push(f);
                if !marked && piece.len() == 1 {
                    // the mark goes with the first frame fed: its own server time
                    marked = true;
                    let t0 = a.time_ms + (start as f64 * 1000.0 / rate) as i64;
                    let id = stream.unwrap_or(0);
                    if !sh.feed.push(id, Vec::new(), Some(t0)) {
                        return Err(io::Error::new(ErrorKind::Interrupted, "stopped"));
                    }
                }
                if piece_samples >= piece_max {
                    if !sh.feed.push(stream.unwrap_or(0), std::mem::take(&mut piece), None) {
                        return Err(io::Error::new(ErrorKind::Interrupted, "stopped"));
                    }
                    piece_samples = 0;
                    fed = pos;
                }
            }
            if at_end {
                if !piece.is_empty() && !sh.feed.push(stream.unwrap_or(0), std::mem::take(&mut piece), None) {
                    return Err(io::Error::new(ErrorKind::Interrupted, "stopped"));
                }
                fed = pos;
                return Ok(());
            }
        }
    })();
    a.took = started.elapsed();
    if let Err(e) = result {
        // What reached the feed stays: the retry (same cursor) skips it.
        if !piece.is_empty() {
            let n: u64 = piece.iter().map(|f| u64::from(f.samples)).sum();
            if sh.feed.push(stream.unwrap_or(0), std::mem::take(&mut piece), None) {
                fed += n;
            }
        }
        if let Some(id) = stream.filter(|_| fed > 0) {
            let end_ms = a.time_ms + (fed as f64 * 1000.0 / st.rate.max(1.0)) as i64;
            let mut p = Partial { stream: id, time_ms: a.time_ms, samples: fed, end_ms };
            // A retry that broke off earlier than the one before still has everything the one
            // before fed: keep the furthest point.
            if let Some(q) = st.partial {
                if q.stream == p.stream && q.time_ms == p.time_ms {
                    p.samples = p.samples.max(q.samples);
                }
                p.end_ms = p.end_ms.max(q.end_ms);
            }
            st.partial = Some(p);
            st.fed_end_ms = Some(st.fed_end_ms.unwrap_or(p.end_ms).max(p.end_ms));
        }
        if e.kind() == ErrorKind::InvalidData {
            st.cursor.clear(); // unusable: start a new stream (the feed's part is skipped by time)
        }
        return Err(e);
    }
    st.partial = None;
    st.fed_end_ms = Some(st.fed_end_ms.unwrap_or(a.end_ms).max(a.end_ms));
    if a.seconds >= 5.0 && body_bytes > 0 {
        let bps = body_bytes as f64 / a.seconds;
        st.bytes_per_s = if st.bytes_per_s > 0.0 { 0.5 * st.bytes_per_s + 0.5 * bps } else { bps };
    }
    let got_s = pos as f64 / st.rate.max(1.0);
    if a.seconds > 0.0 && (got_s - a.seconds).abs() > 0.2 {
        info!("radio: answer said {:.3} s, frames hold ~{got_s:.3} s", a.seconds);
    }
    Ok(a)
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

    /// Long bursts, before the first ask: names before sound (a long first answer starts playing
    /// while it still arrives, before the sync that follows it). Retried like the audio while the
    /// link is down; any answer from the server ends it (names can wait). False = stopped.
    fn first_sync(&mut self) -> bool {
        if self.sh.cfg.songs.is_none() {
            return true;
        }
        let sh = self.sh.clone();
        let got = self.net("tracklist", |c| {
            let Ok(mut songs) = sh.songs.lock().map(|g| Songs { tracklist: g.tracklist.clone(), items: g.items.clone() })
            else {
                return Ok(None);
            };
            match songs.sync(c) {
                Ok(_) => Ok(Some(songs)),
                Err(e) if matches!(e.kind(), ErrorKind::Other | ErrorKind::InvalidData | ErrorKind::PermissionDenied) => {
                    info!("radio: tracklist: {e} (names can wait)");
                    Ok(None)
                }
                Err(e) => Err(e),
            }
        });
        let Some(got) = got else { return false };
        if let Some(songs) = got {
            let newest = songs.items.last().map(|s| s.play_id.clone());
            if let Ok(mut g) = self.sh.songs.lock() {
                for s in songs.items {
                    g.merge(s);
                }
            }
            if newest.is_some() && newest != self.newest_play {
                self.newest_play = newest;
                self.refresh_next();
            }
        }
        true
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

    /// Long bursts: minutes per wake (module doc). Same answers and rules as [`bursts`](Self::bursts);
    /// the asks carry `long=1`, `max_s`, and the answer goes to the feed as it arrives.
    fn bursts_long(&mut self, lb: LongBursts) {
        let mut st = LongState {
            lb,
            cursor: String::new(),
            cur: None,
            codec: None,
            rate: 44_100.0,
            bytes_per_s: guess_bytes_per_s(&self.sh.cfg.format),
            peak_s: 0.0,
            partial: None,
            fed_end_ms: None,
        };
        let mut last_end: Option<i64> = None;
        info!("radio: long bursts (server ring {:.0} s, max behind {:.0} s): hold {:.0} s", lb.ring_s, lb.max_behind_s, st.hold_s());
        while !self.sh.stopped() {
            let fs = self.sh.feed.status();
            // Never ask while plenty is in hand (a paused player): wait for it to play down.
            let headroom = st.headroom_s();
            if fs.has_stream && fs.latest_started && fs.buffered_s > headroom + 1.0 {
                let wait = (fs.buffered_s - headroom).clamp(0.5, st.hold_s() + LONG_SLACK_S);
                self.give_net(Some((wait * 1000.0) as u64));
                if !self.sh.sleep(Duration::from_secs_f64(wait)) {
                    return;
                }
                continue;
            }
            let Some(base) = self.audio_url.clone() else { return };
            if st.cur.is_none() && !self.first_sync() {
                return;
            }
            let (sh, hint) = (self.sh.clone(), self.hint);
            let Some(a) = self.net("audio", |c| long_ask(c, &base, &sh, hint, &mut st)) else { return };
            self.sh.set(|s| s.asks += 1);
            self.sync_songs(); // same wake as the audio
            match a.status {
                200 => {}
                204 => {
                    if let Some(c) = a.cursor {
                        st.cursor = c;
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
                        st.cursor.clear(); // an unusable ask: start a new stream
                    }
                    self.give_net(Some(2000));
                    if !self.sh.sleep(Duration::from_secs(2)) {
                        return;
                    }
                    continue;
                }
            }
            if let (false, Some(e)) = (a.reset, last_end) {
                if a.time_ms != e && (a.time_ms - e).abs() < 20 {
                    info!("radio: clock jitter {} ms between answers", a.time_ms - e);
                }
            }
            last_end = Some(a.end_ms);
            match a.cursor {
                Some(c) => st.cursor = c,
                None => {
                    warn!("radio: answer without a cursor: next ask starts over");
                    st.cursor.clear();
                }
            }
            let fs = self.sh.feed.status();
            st.peak_s = fs.buffered_s;
            if !fs.latest_started {
                self.sh.set(|s| s.phase = Phase::Buffering);
                continue; // keep buffering: min_s makes the server wait
            }
            self.sh.set(|s| {
                s.phase = Phase::Playing;
                s.last_error = None;
            });
            self.fill_gap();
            let sleep = (fs.buffered_s - st.headroom_s()).max(0.5);
            self.give_net(Some((sleep * 1000.0) as u64));
            if !self.sh.sleep(Duration::from_secs_f64(sleep)) {
                return;
            }
        }
    }

    fn bursts(&mut self) {
        if let Some(lb) = self.sh.cfg.long {
            return self.bursts_long(lb);
        }
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

    #[test]
    fn played_and_coming_around_what_you_hear() {
        let mut l = Songs::new(None);
        for (id, start, kind) in [(1, 1000, "music"), (2, 2000, "jingle"), (3, 3000, "music"), (4, 4000, "music"), (5, 5000, "live")] {
            let mut v = song(id, start, Some(start + 1000), false, kind);
            v["track_id"] = json!(100 + id);
            l.merge(Song::from_json(&v).unwrap());
        }
        let planned = |tid: i64| {
            let mut s = Song::from_json(&json!({"play_id": format!("planned-{tid}"), "kind": "music", "on_air_ms": 0})).unwrap();
            s.fields.insert("track_id".into(), json!(tid));
            s
        };
        let next = vec![planned(105), planned(106), planned(107)];
        let ids = |v: &[Song]| v.iter().map(|s| s.play_id.clone()).collect::<Vec<_>>();
        // You hear #3 (live is #5): before = #1 (the jingle is no song), after = #4, #5, then
        // the planned ones (#105 = #5's track, already listed).
        let heard = l.items[2].clone();
        let (played, coming) = l.around(Some(3600), Some(&heard), &next);
        assert_eq!(ids(&played), vec!["1"]);
        assert_eq!(ids(&coming), vec!["4", "5", "planned-106"], "at most three, duplicates left out");
        // A jingle (#2) is on: what aired up to now played, the rest comes.
        let (played, coming) = l.around(Some(2600), None, &[]);
        assert_eq!(ids(&played), vec!["1"]);
        assert_eq!(ids(&coming), vec!["3", "4", "5"]);
        // Starting (nothing heard yet): all but the live play are "before", newest first.
        let (played, coming) = l.around(None, None, &[]);
        assert_eq!(ids(&played), vec!["4", "3", "1"]);
        assert_eq!(ids(&coming), vec!["5"]);
    }

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

    #[test]
    fn long_burst_limits_and_plan() {
        let lb = LongBursts::from_json(&json!({"ring_s": 360, "max_behind_s": 300, "max_min_s": 240, "max_wait_s": 30})).unwrap();
        assert_eq!(lb, LongBursts { ring_s: 360.0, max_behind_s: 300.0, max_min_s: 240.0, max_wait_s: 30.0 });
        // Missing, malformed or no better than the classic limits: the classic loop.
        assert!(LongBursts::from_json(&json!({"ring_s": 60, "max_behind_s": 30})).is_none());
        assert!(LongBursts::from_json(&json!({"ring_s": "x", "max_behind_s": 300})).is_none());
        assert!(LongBursts::from_json(&json!({"max_behind_s": 300})).is_none());
        // The hold: 270 s for MP3, less for FLAC (RAM cap), never past the server's max.
        let flac = long_hold_s(&lb, 175_000.0); // a loud FLAC stream: the RAM cap decides
        assert!((200.0..270.0).contains(&flac), "{flac}");
        assert!((flac + LONG_SLACK_S) * 175_000.0 <= LONG_MAX_BYTES + 1.0);
        assert_eq!(long_hold_s(&lb, 40_000.0), LONG_HOLD_S);
        let small = LongBursts { max_behind_s: 120.0, ..lb };
        assert_eq!(long_hold_s(&small, 40_000.0), 120.0);
        assert_eq!(long_hold_s(&lb, guess_bytes_per_s("flac")), long_hold_s(&lb, 160_000.0));
        assert!(guess_bytes_per_s("mp3-128") < guess_bytes_per_s("mp3"));
        // Headroom: 45 s on a full hold, the classic 5 s on a short stream (no asking all the time).
        assert_eq!(long_headroom_s(260.0), LONG_HEADROOM_S);
        assert_eq!(long_headroom_s(20.0), HEADROOM_S);
        // max_s: what may come without passing the hold.
        assert_eq!(long_max_s(270.0, 45.0), 240.0);
        assert_eq!(long_max_s(270.0, 400.0), 5.0);
        // Resume after a cut-off answer: same stream = exact samples, a reset = by wall time.
        let p = Partial { stream: 3, time_ms: 1000, samples: 4410, end_ms: 1100 };
        assert_eq!(resume_skip(Some(p), false, Some(3), Some(1100)), Some(Ok(4410)));
        assert_eq!(resume_skip(Some(p), true, Some(3), Some(1100)), Some(Err(1100)));
        assert_eq!(resume_skip(None, false, Some(3), Some(900)), None, "a whole answer before: nothing to skip");
        // Any reset skips what the feed has (the old stream plays it out): no replayed headroom.
        assert_eq!(resume_skip(None, true, Some(3), Some(900)), Some(Err(900)));
        assert_eq!(resume_skip(None, true, None, None), None, "the first answer");
    }

    #[test]
    fn stream_config_reads_long_bursts() {
        let lb = json!({"ring_s": 360, "max_behind_s": 300, "max_min_s": 240, "max_wait_s": 30});
        let station = json!({"audio": {"flac": {"url": "http://radio.example:8080/a/flac", "type": "audio/flac", "long_bursts": lb},
                                       "mp3": {"url": "http://radio.example:8080/a/mp3"}}});
        let mut sc = crate::modules::Scope::new();
        sc.push("station", &station);
        let cfg = |fmt: &str, proto: &str| {
            crate::modules::stream_config(&json!({"protocol": proto, "formats": "$station.audio", "format": fmt}), &sc).unwrap()
        };
        assert_eq!(cfg("flac", "radio-v1").long.map(|l| l.max_behind_s), Some(300.0));
        assert_eq!(cfg("mp3", "radio-v1").long, None, "not advertised: classic bursts");
        assert_eq!(cfg("flac", "http").long, None, "a plain stream never asks for bursts");
    }

    /// A radio server in memory: answers by the ask's `cursor`, and can cut a body off.
    struct FakeRadio {
        asks: Mutex<Vec<String>>,
        answer: Box<dyn Fn(usize, &str) -> (Vec<u8>, Option<usize>) + Send + Sync>,
    }

    struct FakeConn {
        radio: Arc<FakeRadio>,
        req: Vec<u8>,
        out: Vec<u8>,
        at: usize,
        cut: Option<usize>,
    }

    impl Read for FakeConn {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            let end = self.cut.unwrap_or(self.out.len()).min(self.out.len());
            if self.at >= end && self.cut.is_some() {
                return Err(io::Error::new(ErrorKind::ConnectionReset, "fake: cut off"));
            }
            let n = b.len().min(end - self.at).min(700);
            b[..n].copy_from_slice(&self.out[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    impl Write for FakeConn {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.req.extend_from_slice(b);
            if self.req.windows(4).any(|w| w == b"\r\n\r\n") && self.out.is_empty() {
                let line = String::from_utf8_lossy(&self.req).lines().next().unwrap_or("").to_string();
                let target = line.split(' ').nth(1).unwrap_or("").to_string();
                let k = {
                    let mut a = self.radio.asks.lock().unwrap();
                    a.push(target.clone());
                    a.len() - 1
                };
                let (out, cut) = (self.radio.answer)(k, &target);
                self.out = out;
                self.cut = cut;
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl crate::http::Conn for FakeConn {
        fn set_timeouts(&self, _: Duration) -> io::Result<()> {
            Ok(())
        }
        fn aborter(&self) -> Option<crate::http::Aborter> {
            None
        }
    }

    struct FakeTransport(Arc<FakeRadio>);

    impl Transport for FakeTransport {
        fn connect(&self, _: &Url, _: Duration) -> io::Result<Box<dyn crate::http::Conn>> {
            Ok(Box::new(FakeConn { radio: self.0.clone(), req: Vec::new(), out: Vec::new(), at: 0, cut: None }))
        }
    }

    fn query(target: &str, k: &str) -> Option<String> {
        let q = target.split_once('?')?.1;
        q.split('&').find_map(|kv| {
            let (a, b) = kv.split_once('=')?;
            (a == k).then(|| b.replace("%2E", ".").replace("%2e", "."))
        })
    }

    #[test]
    fn long_bursts_resume_cut_off_answers_without_repeats_or_gaps() {
        use crate::flac_stream::testenc::{frame, head};
        use crate::stream::{FeedPlayer, Pcm};
        // Frame i: 4410 samples (0.1 s), both channels hold i, server time 1_000_000 + 100 i ms.
        const T0: i64 = 1_000_000;
        let answer = |frames: std::ops::Range<u64>, first: u64, reset: bool, cursor: &str| {
            let mut body = if reset { head(4410) } else { Vec::new() };
            let head_len = body.len();
            for i in frames.clone() {
                body.extend(frame(i - first, 4410, Some(i as i16), Some(-(i as i16))));
            }
            let h = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nX-Stream-Cursor: {cursor}\r\nX-Stream-Reset: {}\r\n\
                 X-Stream-Time-Ms: {}\r\nX-Stream-End-Ms: {}\r\nX-Stream-Seconds: {:.3}\r\nX-Stream-Head-Bytes: {head_len}\r\n\r\n",
                body.len(),
                u8::from(reset),
                T0 + 100 * frames.start as i64,
                T0 + 100 * frames.end as i64,
                (frames.end - frames.start) as f64 / 10.0,
            );
            let at = h.len();
            ([h.into_bytes(), body].concat(), at)
        };
        let radio = Arc::new(FakeRadio {
            asks: Mutex::new(Vec::new()),
            answer: Box::new(move |k, target| {
                let cursor = query(target, "cursor").unwrap_or_default();
                match (k, cursor.as_str()) {
                    // The first answer breaks off half way; its retry (cursor "" again) is a new
                    // stream that starts 1 s later: what was fed is skipped by wall time.
                    (0, "") => {
                        let (a, at) = answer(0..50, 0, true, "E.0.50");
                        let cut = at + (a.len() - at) / 2;
                        (a, Some(cut))
                    }
                    (_, "") => (answer(10..60, 10, true, "E.10.60").0, None),
                    // A continuation breaks off; asked again with the same cursor it comes again
                    // from the same start: what was fed is skipped by samples.
                    (2, "E.10.60") => {
                        let (a, at) = answer(60..80, 10, false, "E.10.80");
                        (a.clone(), Some(at + (a.len() - at) * 3 / 5))
                    }
                    (_, "E.10.60") => (answer(60..80, 10, false, "E.10.80").0, None),
                    _ => (b"HTTP/1.1 204 No Content\r\nConnection: close\r\nX-Stream-Cursor: E.10.80\r\nContent-Length: 0\r\n\r\n".to_vec(), None),
                }
            }),
        });
        let mut cfg = Config::new(Url::parse("http://radio.example:8080/api/v1/audio/hs/flac").unwrap());
        cfg.format = "flac".into();
        cfg.start_s = 0.5;
        cfg.long = Some(LongBursts { ring_s: 360.0, max_behind_s: 300.0, max_min_s: 240.0, max_wait_s: 30.0 });
        let net: Arc<dyn Network> = Arc::new(OpenNetwork(Arc::new(FakeTransport(radio.clone()))));
        let mut r = Radio::start(cfg, net).unwrap();
        let mut p = FeedPlayer::new(r.feed());
        let mut out = Vec::new();
        let mut got: Vec<(i16, Option<i64>)> = Vec::new();
        let until = Instant::now() + Duration::from_secs(20);
        while got.len() < 80 && Instant::now() < until {
            match p.next_pcm(&mut out, Duration::from_millis(50)) {
                Pcm::Samples { .. } => got.push((out[0], r.feed().now_wall_ms())),
                Pcm::Ended => break,
                _ => {}
            }
        }
        let asks = radio.asks.lock().unwrap().clone();
        r.stop(1000);
        let values: Vec<i16> = got.iter().map(|g| g.0).collect();
        assert_eq!(values, (0..80).collect::<Vec<i16>>(), "every frame once, in order; asks {asks:?}");
        // The clock follows: after frame i plays, you hear the server time of frame i + 1.
        for (v, t) in &got {
            assert_eq!(*t, Some(T0 + 100 * (i64::from(*v) + 1)), "frame {v}");
        }
        assert!(asks.len() >= 4, "{asks:?}");
        let first = &asks[0];
        assert_eq!(query(first, "long").as_deref(), Some("1"));
        assert!(query(first, "behind").and_then(|b| b.parse::<f64>().ok()).is_some_and(|b| b > 200.0), "{first}");
        assert!(query(first, "max_s").is_some() && query(first, "wait").as_deref() == Some("30"), "{first}");
        assert_eq!(query(&asks[3], "cursor").as_deref(), Some("E.10.60"), "a cut continuation is asked again as it was");
        assert_eq!(r.status().resets, 1, "the retried first answer is the one new stream");
    }
}
