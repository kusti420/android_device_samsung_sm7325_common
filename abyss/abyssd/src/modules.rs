//! The module host: user-installed Abyss modules (`runtime = "pure"`), drawn and run by abyssd.
//!
//! Contract: abyss-design reports/module-install-contract.md (installer ⇄ abyssd), on top of
//! docs/reference/app-package.md (manifest, menu.nix) and api1-sugar.md §7 (menus are sugar).
//!
//! - **Store** (§3): `/data/misc/abyss/store/<id>/` with `install.json` (status `ready`, ids
//!   matching), `manifest.json` (parsed abyss-app.nix), `menu.json` (parsed menu.nix). Names
//!   starting with "." are the installer's staging/old/trash dirs: skipped. Read once on entry.
//! - **Settings** (§4): `/data/misc/abyss/conf/settings/app-<id>.env`, env names mapped back to
//!   setting keys through the manifest, typed, defaults filled in. Read-only inside Abyss.
//! - **Network** (§4): a module may only reach the origins in `install.json` `approved.net`;
//!   every fetch is checked, links from answers included (http.rs `Client::with_allow`).
//! - **Menus** (§5): `list` and `text` screens, `get` (+ `every`, `refreshAt`, `page`),
//!   `items.from = "$data.…"` with a `row` template and `as`, `order = "reverse"`, `empty`,
//!   `error`, `guide`, `when`/`unless` lines, the hole grammar with `:time :clock :dur`, the
//!   `stream` built-in (radio-v1 / http, radio_source.rs) with `$stream.*` and `show`, and
//!   the manifest's `capture = { line; kind; fields; }`.
//! - `menu:header`: a list screen's `header` lines (with `style`) above its rows.
//!   `media:stream:screen`: a stream item may pass its row on (`as`), name the station's `now`
//!   link (`$stream.next`), and `show` a list screen with a header: that screen opens instead of
//!   Now Playing, lists may come `from = "$stream.…"`, and Now Playing draws the header.
//! - **Features** (§6): [`FEATURES`] is exactly what this host implements; the ROM ships the
//!   same list as /system_ext/etc/abyss/features for the installer.
//!
//! Everything here runs on the main thread except fetches, which run on short-lived threads
//! holding a Wi-Fi lease and report back through a poke socket (the bt.rs pattern).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{info, warn};
use serde_json::{json, Map, Value};

use crate::capture::Capture;
use crate::http::{Cancel, Client, Timeouts, Url};
use crate::radio_source::{self, codec_hint, Config, Mode, Network, Phase, Radio, RadioCapture, RadioStatus};
use crate::stream::{Feed, START_S};

pub const STORE: &str = "/data/misc/abyss/store";
pub const SETTINGS_DIR: &str = "/data/misc/abyss/conf/settings";
/// The last module stream (what Continue plays when radio was the last source): written by
/// abyssd only, next to state.json.
pub const LAST_STREAM: &str = "/data/misc/abyss/out/last_stream.json";

/// The features this module host implements (shipped as /system_ext/etc/abyss/features).
pub const FEATURES: &[&str] = &[
    "apps",
    "settings",
    "menu:get",
    "media:stream",
    "media:stream:radio-v1",
    "media:stream:http",
    "media:stream:screen",
    "menu:header",
    "capture:fields",
    "post:capture",
    "needs:media",
    "radio:wifi",
];
/// App API levels served.
pub const API_LEVELS: &[i64] = &[1];
/// Largest `get` answer (contract §8.7 proposal).
const GET_MAX: usize = 256 * 1024;
/// Shortest `every`.
const EVERY_MIN_MS: u64 = 5_000;
/// One `$stream` value serves every hole of a draw (status() copies the song lists).
const STREAM_CACHE_MS: u128 = 250;
/// Names an `as` may not take.
const RESERVED_NS: &[&str] =
    &["state", "settings", "battery", "time", "value", "data", "row", "stream", "abyss", "pub"];
pub const NEW_ADDRESS_TEXT: &str = "This module needs you to allow its new server address in Settings.";

pub const CATEGORIES: &[(&str, &str)] = &[
    ("music", "Music"),
    ("spoken", "Spoken"),
    ("time", "Time"),
    ("home", "Home"),
    ("info", "Info"),
    ("tools", "Tools"),
    ("games", "Games"),
];

// ---------------------------------------------------------------------------
// Installed modules

#[derive(Clone, Debug)]
pub struct Module {
    pub id: String,
    pub name: String,
    pub version: String,
    pub category: String,
    pub manifest: Value,
    pub menu: Value,
    /// Typed setting values by setting key (file values, else defaults).
    pub settings: Map<String, Value>,
    /// Approved origins (`install.json` approved.net).
    pub net: Vec<String>,
}

fn read_json(p: &Path) -> Result<Value, String> {
    let b = fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
    serde_json::from_slice(&b).map_err(|e| format!("{}: {e}", p.display()))
}

fn valid_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Loads every ready module. Returns the modules (sorted by name) and one line per problem
/// (for out/errors and the log).
pub fn load(store: &Path, settings_dir: &Path) -> (Vec<Module>, Vec<String>) {
    let mut mods = Vec::new();
    let mut errs = Vec::new();
    let Ok(rd) = fs::read_dir(store) else { return (mods, errs) };
    let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        let Some(name) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
        if name.starts_with('.') || !dir.is_dir() {
            continue;
        }
        match load_one(&dir, &name, settings_dir) {
            Ok(m) => mods.push(m),
            Err(e) => errs.push(format!("module {name}: {e}")),
        }
    }
    mods.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    for m in &mods {
        info!("module {} {} ({}, net {:?})", m.id, m.version, m.category, m.net);
    }
    for e in &errs {
        warn!("{e}");
    }
    info!("modules: {} loaded ({} skipped)", mods.len(), errs.len());
    (mods, errs)
}

fn load_one(dir: &Path, name: &str, settings_dir: &Path) -> Result<Module, String> {
    if !valid_id(name) {
        return Err("not a module id".into());
    }
    let inst = read_json(&dir.join("install.json"))?;
    let st = inst.get("status").and_then(Value::as_str).unwrap_or("");
    if st != "ready" {
        return Err(format!("install status {st:?}"));
    }
    if inst.get("id").and_then(Value::as_str) != Some(name) {
        return Err("install.json id differs".into());
    }
    let manifest = read_json(&dir.join("manifest.json"))?;
    let m = manifest.as_object().ok_or("manifest.json is not an object")?;
    if m.get("id").and_then(Value::as_str) != Some(name) {
        return Err("manifest.json id differs".into());
    }
    let api = m.get("api").and_then(Value::as_i64).unwrap_or(0);
    if !API_LEVELS.contains(&api) {
        return Err(format!("api {api} needs a newer IchthysOS"));
    }
    let runtime = m.get("runtime").and_then(Value::as_str).unwrap_or("script");
    if runtime != "pure" {
        return Err(format!("runtime {runtime:?} is not supported in this build"));
    }
    let req = m.get("requires");
    if req.and_then(|r| r.get("abyss")).and_then(Value::as_i64).is_some_and(|l| !API_LEVELS.iter().any(|a| *a >= l)) {
        return Err("needs a newer app API level".into());
    }
    for f in req.and_then(|r| r.get("features")).and_then(Value::as_array).into_iter().flatten() {
        let f = f.as_str().unwrap_or("?");
        if !FEATURES.contains(&f) {
            return Err(format!("needs `{f}`; this IchthysOS doesn't have it yet"));
        }
    }
    let menu = match m.get("menu").and_then(Value::as_str) {
        Some(_) => read_json(&dir.join("menu.json"))?,
        None => Value::Null,
    };
    if !menu.is_null() && menu.get("menu").and_then(Value::as_i64) != Some(1) {
        return Err("menu.json is not menu api 1".into());
    }
    let net = inst
        .get("approved")
        .and_then(|a| a.get("net"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(|s| s.to_ascii_lowercase()).collect())
        .unwrap_or_default();
    let env = fs::read_to_string(settings_dir.join(format!("app-{name}.env"))).unwrap_or_default();
    let settings = settings_values(&manifest, &parse_env(&env));
    let text = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(Module {
        id: name.to_string(),
        name: text("name"),
        version: text("version"),
        category: text("category"),
        menu,
        settings,
        net,
        manifest,
    })
}

/// The dotenv subset of files.md (`KEY=value`, `KEY="a \" b"`, `#` comments).
pub fn parse_env(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim();
        let val = if let Some(q) = v.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
            let mut s = String::new();
            let mut it = q.chars();
            while let Some(c) = it.next() {
                if c == '\\' {
                    match it.next() {
                        Some('n') => s.push('\n'),
                        Some(o) => s.push(o),
                        None => {}
                    }
                } else {
                    s.push(c);
                }
            }
            s
        } else {
            v.to_string()
        };
        out.insert(k.trim().to_string(), val);
    }
    out
}

fn upper_snake(key: &str) -> String {
    let mut s = String::new();
    for (i, c) in key.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            s.push('_');
        }
        s.push(c.to_ascii_uppercase());
    }
    s
}

/// Typed values per setting key: the file's value when it fits the type, else the default.
pub fn settings_values(manifest: &Value, env: &BTreeMap<String, String>) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(schema) = manifest.get("settings").and_then(Value::as_object) else { return out };
    for (key, def) in schema {
        let ty = def.get("type").and_then(Value::as_str).unwrap_or("text");
        if ty == "secret" {
            continue; // never a hole, never in the .env
        }
        let env_name = def.get("env").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| upper_snake(key));
        let from_file = env.get(&env_name).and_then(|raw| match ty {
            "number" => {
                let n: f64 = raw.trim().parse().ok().filter(|n: &f64| n.is_finite())?;
                let min = def.get("min").and_then(Value::as_f64).unwrap_or(f64::MIN);
                let max = def.get("max").and_then(Value::as_f64).unwrap_or(f64::MAX);
                if n < min || n > max {
                    return None;
                }
                if def.get("integer").and_then(Value::as_bool).unwrap_or(false) || n.fract() == 0.0 {
                    Some(Value::from(n as i64))
                } else {
                    Some(Value::from(n))
                }
            }
            "bool" => match raw.trim() {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            },
            "enum" => {
                let ok = def.get("choices").and_then(Value::as_array).is_none_or(|c| {
                    c.iter().any(|x| x.get("value").and_then(Value::as_str).or(x.as_str()) == Some(raw.as_str()))
                });
                ok.then(|| Value::from(raw.clone()))
            }
            _ => Some(Value::from(raw.clone())),
        });
        if let Some(v) = from_file.or_else(|| def.get("default").cloned()) {
            out.insert(key.clone(), v);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Holes

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fmt {
    Time,
    Clock,
    Dur,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Lit(String),
    Hole { ns: String, keys: Vec<String>, fmt: Option<Fmt> },
}

fn key_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Splits a string into literal text and holes (contract §5.2).
pub fn parse_holes(s: &str) -> Vec<Piece> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut i = 0;
    while i < b.len() {
        let ns_ok = i + 1 < b.len() && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_');
        if b[i] != b'$' || !ns_ok {
            let ch = s[i..].chars().next().unwrap_or('?');
            lit.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let mut j = i + 1;
        while j < b.len() && key_char(b[j]) {
            j += 1;
        }
        let ns = s[i + 1..j].to_string();
        let mut keys = Vec::new();
        while j + 1 < b.len() && b[j] == b'.' && key_char(b[j + 1]) {
            let k0 = j + 1;
            let mut k = k0;
            loop {
                while k < b.len() && key_char(b[k]) {
                    k += 1;
                }
                if k + 1 < b.len() && b[k] == b'-' && key_char(b[k + 1]) {
                    k += 1;
                    continue;
                }
                break;
            }
            keys.push(s[k0..k].to_string());
            j = k;
        }
        let mut fmt = None;
        if j < b.len() && b[j] == b':' {
            for (word, f) in [("time", Fmt::Time), ("clock", Fmt::Clock), ("dur", Fmt::Dur)] {
                let end = j + 1 + word.len();
                if s[j + 1..].starts_with(word) && (end >= b.len() || !key_char(b[end])) {
                    fmt = Some(f);
                    j = end;
                    break;
                }
            }
        }
        if !lit.is_empty() {
            out.push(Piece::Lit(std::mem::take(&mut lit)));
        }
        out.push(Piece::Hole { ns, keys, fmt });
        i = j;
    }
    if !lit.is_empty() {
        out.push(Piece::Lit(lit));
    }
    out
}

/// Where holes look their names up: nearest scope first.
#[derive(Default)]
pub struct Scope<'a> {
    names: Vec<(&'a str, &'a Value)>,
}

impl<'a> Scope<'a> {
    pub fn new() -> Scope<'a> {
        Scope { names: Vec::new() }
    }
    /// Adds a name; a later push of the same name wins (nearest).
    pub fn push(&mut self, ns: &'a str, v: &'a Value) {
        self.names.push((ns, v));
    }

    fn get(&self, ns: &str, keys: &[String]) -> Option<&'a Value> {
        let (_, root) = self.names.iter().rev().find(|(n, _)| *n == ns)?;
        let mut v: &Value = root;
        for k in keys {
            v = match v {
                Value::Object(o) => o.get(k)?,
                Value::Array(a) => a.get(k.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(v)
    }
}

fn present(v: &Value) -> bool {
    !matches!(v, Value::Null | Value::Bool(false)) && v.as_str() != Some("")
}

/// `when`: present and not false / 0 / "".
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None => false,
        Some(v) => present(v) && v.as_f64() != Some(0.0),
    }
}

fn wall_now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// (hour, minute, second) in the phone's time zone (also the status line's clock, ui.rs).
pub fn local_hms(ms: i64) -> (i64, i64, i64) {
    let t: libc::time_t = (ms.div_euclid(1000)) as libc::time_t;
    // SAFETY: localtime_r writes into our own zeroed struct; both pointers are valid.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let ok = !unsafe { libc::localtime_r(&t, &mut tm) }.is_null();
    if ok {
        (i64::from(tm.tm_hour), i64::from(tm.tm_min), i64::from(tm.tm_sec))
    } else {
        let s = ms.div_euclid(1000).rem_euclid(86_400);
        (s / 3600, s % 3600 / 60, s % 60)
    }
}

fn format_value(v: &Value, fmt: Option<Fmt>) -> Option<String> {
    if let Some(f) = fmt {
        let ms = v.as_f64().or_else(|| v.as_str()?.parse().ok())? as i64;
        return Some(match f {
            Fmt::Time => {
                let (h, m, _) = local_hms(ms);
                format!("{h:02}:{m:02}")
            }
            Fmt::Clock => {
                let (h, m, s) = local_hms(ms);
                format!("{h:02}:{m:02}:{s:02}")
            }
            Fmt::Dur => {
                let s = ms.max(0) / 1000;
                if s >= 3600 {
                    format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
                } else {
                    format!("{}:{:02}", s / 60, s % 60)
                }
            }
        });
    }
    Some(match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            _ => n.to_string(),
        },
        other => other.to_string(),
    })
}

/// Fills every hole; None when one is missing (absent, null, false or "").
pub fn fill(s: &str, scope: &Scope) -> Option<String> {
    let mut out = String::new();
    for p in parse_holes(s) {
        match p {
            Piece::Lit(l) => out.push_str(&l),
            Piece::Hole { ns, keys, fmt } => {
                let v = if ns == "time" && keys.is_empty() {
                    Value::from(wall_now_ms())
                } else {
                    scope.get(&ns, &keys).filter(|v| present(v))?.clone()
                };
                let fmt = if ns == "time" && keys.is_empty() && fmt.is_none() { Some(Fmt::Time) } else { fmt };
                out.push_str(&format_value(&v, fmt)?);
            }
        }
    }
    Some(out)
}

/// The value of a string that is exactly one hole (keeps its JSON type), else None.
pub fn single<'a>(s: &str, scope: &Scope<'a>) -> Option<&'a Value> {
    match parse_holes(s).as_slice() {
        [Piece::Hole { ns, keys, fmt: None }] => scope.get(ns, keys),
        _ => None,
    }
}

fn is_single_hole(s: &str) -> bool {
    matches!(parse_holes(s).as_slice(), [Piece::Hole { fmt: None, .. }])
}

/// "30s", "5m", "1h", "500ms" → ms.
pub fn parse_dur_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (n, u) = s.split_at(split);
    let n: f64 = n.parse().ok()?;
    let mul = match u {
        "ms" => 1.0,
        "s" => 1000.0,
        "m" => 60_000.0,
        "h" => 3_600_000.0,
        _ => return None,
    };
    Some((n * mul) as u64)
}

/// RFC 6901 pointer into a JSON value.
fn pointer<'a>(v: &'a Value, p: &str) -> Option<&'a Value> {
    v.pointer(p)
}

/// `guide = "▲▼ move  ● open"` → [("▲▼", "move"), ("●", "open")].
pub fn guide_pairs(s: &str) -> Vec<(String, String)> {
    s.split("  ")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| match c.split_once(' ') {
            Some((k, m)) => (k.to_string(), m.trim().to_string()),
            None => (c.to_string(), String::new()),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Screens

#[derive(Clone, Debug, PartialEq)]
pub enum Load {
    None,
    Loading(u64),
    Ready,
    Failed(String),
}

#[derive(Clone, Debug)]
enum Place {
    /// Main › Apps: the categories that have modules.
    Apps,
    Category(String),
    Screen { module: usize, screen: String },
}

#[derive(Clone, Debug)]
struct Frame {
    place: Place,
    /// `as` names visible here (inherited; later wins).
    binds: Vec<(String, Value)>,
    load: Load,
    /// The answers of `get` (first page first).
    pages: Vec<Value>,
    sel: usize,
    /// Wall ms of the last fetch.
    fetched_at: i64,
}

impl Frame {
    fn new(place: Place, binds: Vec<(String, Value)>) -> Frame {
        Frame { place, binds, load: Load::None, pages: Vec::new(), sel: 0, fetched_at: 0 }
    }
}

/// One row as the UI draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct RowView {
    pub label: String,
    pub meta: String,
    pub dim: bool,
}

/// How a header line is drawn (`style`, menu:header).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// Body text (the default).
    Plain,
    /// Large: the song's artist and title.
    Big,
    /// Body text, dim: details.
    Dim,
    /// A small accent heading over what follows ("UP NEXT"); left out when nothing follows.
    Head,
}

impl Style {
    fn parse(s: Option<&str>) -> Style {
        match s {
            Some("big") => Style::Big,
            Some("dim") => Style::Dim,
            Some("head") => Style::Head,
            _ => Style::Plain,
        }
    }
}

/// One header line as the UI draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub text: String,
    pub style: Style,
}

/// Drops headings that nothing follows: one right before another heading, and (`at_end`, Now
/// Playing, where no rows follow the header) one at the end.
pub fn drop_bare_heads(lines: &mut Vec<Line>, at_end: bool) {
    let mut out: Vec<Line> = Vec::with_capacity(lines.len());
    for l in lines.drain(..) {
        if l.style == Style::Head && out.last().is_some_and(|p| p.style == Style::Head) {
            out.pop();
        }
        out.push(l);
    }
    while at_end && out.last().is_some_and(|l| l.style == Style::Head) {
        out.pop();
    }
    *lines = out;
}

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    /// Rows (the Back row included, last).
    Rows(Vec<RowView>),
    /// Text lines, then rows (a Back row).
    Text(Vec<String>, Vec<RowView>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScreenView {
    pub title: String,
    pub sub: Option<String>,
    /// A list screen's `header` lines (menu:header), drawn above the rows; empty otherwise.
    pub header: Vec<Line>,
    pub body: Body,
    /// From the screen's `guide`; empty = abyssd's own.
    pub guide: Vec<(String, String)>,
    /// A one-line notice (loading, an error, the empty text), with warn colour or not.
    pub note: Option<(String, bool)>,
    pub sel: usize,
}

/// What a selection asks the daemon to do.
#[derive(Debug)]
pub enum Effect {
    None,
    /// Leave the module screens (Back on the first one).
    Exit,
    /// A stream started: hand this feed to the player; open Now Playing (`player`), or stay on
    /// the module screens, where the stream's `show` list screen was just opened.
    Play { feed: Arc<Feed>, player: bool },
    /// The picked stream is the one already playing: keep it, unpause if paused, open Now
    /// Playing (`player`) or stay on the module screens (its `show` list screen is open).
    Continue { player: bool },
    /// `media = "play|pause|toggle|next|prev"`.
    Media(String),
    /// `say`, `buzz`, `show` built-ins: the text (the daemon has no voice yet: shown).
    Show(String),
    Buzz(String),
    Battery,
    /// Something this build cannot do (the text says so).
    Unsupported(String),
}

/// A fetch result coming back from its thread.
struct Done {
    req: u64,
    result: Result<Value, String>,
    append: bool,
}

/// Where a stream was picked: that screen (its data, its context) and the row, so hold ▲ / ▼
/// can step to the station next to it and Continue can start it again after a restart.
#[derive(Clone, Debug)]
struct Origin {
    frame: Frame,
    act: ItemAct,
}

/// The last stream, as saved in [`LAST_STREAM`].
#[derive(Clone, Debug)]
struct LastStream {
    module: usize,
    origin: Origin,
    title: String,
}

struct StreamSession {
    module: usize,
    /// Where it was picked (None for a stream started some other way).
    origin: Option<Origin>,
    /// Which station and format plays (`$stream.tag`, `$stream.format`): picking the same one
    /// again must not restart it.
    key: (String, String),
    radio: Radio,
    /// The `show` screen, drawn as Now Playing (with the item's context and its own `get`).
    frame: Option<Frame>,
    paused: bool,
}

/// Which screen the person sees (for refreshes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visible {
    Module,
    NowPlaying,
    Other,
}

/// The host: modules, the screen stack, fetches, the stream.
pub struct Host {
    pub modules: Vec<Module>,
    pub errors: Vec<String>,
    stack: Vec<Frame>,
    net: Arc<dyn Network>,
    next_req: u64,
    done: Arc<Mutex<Vec<Done>>>,
    poke_r: UnixStream,
    poke_w: Arc<Mutex<UnixStream>>,
    stream: Option<StreamSession>,
    /// Facts for `$battery.*` (set by the daemon).
    pub battery: Value,
    /// `$stream` as last built, and when (one value per draw; see STREAM_CACHE_MS).
    stream_cache: RefCell<Option<(Instant, Rc<Value>)>>,
    /// The selected row per module screen, kept when the screen closes (this session).
    memo: BTreeMap<(usize, String), usize>,
    /// While stepping stations: refresh an open `show` screen instead of opening one.
    stepping: bool,
    /// The last stream (for Continue), where it is saved (None: not saved, tests).
    last: Option<LastStream>,
    last_path: Option<PathBuf>,
    /// Radio was the last source: Home's Continue and the entry start resume it.
    last_is_source: bool,
    /// Shown ready at entry (Now playing, ● starts it; Wi-Fi only then).
    armed: bool,
}

impl Host {
    pub fn new(modules: Vec<Module>, errors: Vec<String>, net: Arc<dyn Network>) -> io::Result<Host> {
        let (poke_r, poke_w) = UnixStream::pair()?;
        poke_r.set_nonblocking(true)?;
        poke_w.set_nonblocking(true)?;
        Ok(Host {
            modules,
            errors,
            stack: Vec::new(),
            net,
            next_req: 1,
            done: Arc::new(Mutex::new(Vec::new())),
            poke_r,
            poke_w: Arc::new(Mutex::new(poke_w)),
            stream: None,
            battery: Value::Null,
            stream_cache: RefCell::new(None),
            memo: BTreeMap::new(),
            stepping: false,
            last: None,
            last_path: None,
            last_is_source: false,
            armed: false,
        })
    }

    /// Loads the store and settings at their ROM paths, and the last stream.
    pub fn from_device(net: Arc<dyn Network>) -> io::Result<Host> {
        let (m, e) = load(Path::new(STORE), Path::new(SETTINGS_DIR));
        let mut h = Host::new(m, e, net)?;
        h.use_last_file(Path::new(LAST_STREAM));
        Ok(h)
    }

    /// Reads the last stream from `path` and saves it there from now on.
    pub fn use_last_file(&mut self, path: &Path) {
        self.last = fs::read(path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| self.last_from_json(&v));
        if let Some(l) = &self.last {
            info!("last stream: {} ({})", l.title, self.modules[l.module].id);
        }
        self.last_path = Some(path.to_path_buf());
    }

    fn last_from_json(&self, v: &Value) -> Option<LastStream> {
        if v.get("v").and_then(Value::as_i64) != Some(1) {
            return None;
        }
        let id = v.get("module")?.as_str()?;
        let module = self.modules.iter().position(|m| m.id == id)?;
        let screen = v.get("screen")?.as_str()?.to_string();
        self.screen_def(module, &screen)?;
        let act = match (v.get("item").and_then(Value::as_u64), v.get("row").and_then(Value::as_array)) {
            (Some(i), _) => ItemAct::Static(i as usize),
            (None, Some(r)) => ItemAct::Row(r.first()?.as_u64()? as usize, r.get(1)?.as_u64()? as usize),
            _ => return None,
        };
        let binds = v
            .get("binds")?
            .as_object()?
            .iter()
            .filter(|(k, _)| !RESERVED_NS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut frame = Frame::new(Place::Screen { module, screen }, binds);
        frame.pages = v.get("pages").and_then(Value::as_array).cloned().unwrap_or_default();
        frame.load = Load::Ready;
        let title = v.get("title").and_then(Value::as_str).unwrap_or("").to_string();
        Some(LastStream { module, origin: Origin { frame, act }, title })
    }

    /// Remembers the stream that just started (memory and file).
    fn remember_last(&mut self, origin: Origin) {
        let Some(s) = self.stream.as_ref() else { return };
        let module = s.module;
        let title = s.radio.status().title;
        let Place::Screen { screen, .. } = &origin.frame.place else { return };
        let mut binds = Map::new();
        for (k, v) in &origin.frame.binds {
            binds.insert(k.clone(), v.clone());
        }
        let mut v = json!({"v": 1, "module": self.modules[module].id, "screen": screen, "binds": binds,
            "pages": origin.frame.pages, "title": title});
        match origin.act {
            ItemAct::Static(i) => v["item"] = json!(i),
            ItemAct::Row(p, i) => v["row"] = json!([p, i]),
        }
        if let Some(p) = &self.last_path {
            let tmp = p.with_extension("json.tmp");
            let r = fs::write(&tmp, v.to_string()).and_then(|_| fs::rename(&tmp, p));
            if let Err(e) = r {
                warn!("save {}: {e}", p.display());
            }
        }
        self.last = Some(LastStream { module, origin, title });
    }

    /// The player's last source was this module stream (true) or the library (false).
    pub fn set_last_source(&mut self, stream: bool) {
        self.last_is_source = stream;
        if !stream {
            self.armed = false;
        }
    }

    /// Home's Continue: the last stream's title while radio was the last source and no stream
    /// runs.
    pub fn continue_title(&self) -> Option<String> {
        let l = self.last.as_ref().filter(|_| self.last_is_source && self.stream.is_none())?;
        Some(if l.title.is_empty() { self.modules[l.module].name.clone() } else { l.title.clone() })
    }

    /// At entry: show the last stream ready on Now playing (nothing is fetched until ●).
    /// False when there is none.
    pub fn arm(&mut self) -> bool {
        self.armed = self.continue_title().is_some();
        self.armed
    }

    /// The last stream is shown ready (● starts it).
    pub fn armed(&self) -> bool {
        self.armed && self.stream.is_none()
    }

    /// Starts the last stream again (Continue, ● on the ready Now playing).
    pub fn continue_last(&mut self) -> Effect {
        self.armed = false;
        let Some(l) = self.last.clone() else { return Effect::Unsupported("Nothing to continue yet.".into()) };
        let Place::Screen { screen, .. } = &l.origin.frame.place else { return Effect::None };
        let Some(def) = self.screen_def(l.module, screen).cloned() else { return Effect::None };
        self.stepping = true; // no screen of its own opens: the caller shows Now playing
        let eff = self.act(&l.origin.frame, l.module, &def, l.origin.act);
        self.stepping = false;
        eff
    }

    /// Hold ▲ / ▼ while a module stream plays: the next / previous station of the list it was
    /// picked from (wrapping). The person stays where they are; an open station screen follows.
    pub fn step_station(&mut self, dir: i32) -> Effect {
        let Some(o) = self.stream.as_ref().and_then(|s| s.origin.clone()) else { return Effect::None };
        let Place::Screen { module, screen } = &o.frame.place else { return Effect::None };
        let module = *module;
        let Some(def) = self.screen_def(module, screen).cloned() else { return Effect::None };
        let acts: Vec<ItemAct> = self
            .items_of(&o.frame, &def)
            .into_iter()
            .map(|(_, a)| a)
            .filter(|a| match a {
                ItemAct::Static(i) => def.get("items").and_then(|x| x.get(*i)).and_then(|x| x.get("stream")).is_some(),
                ItemAct::Row(..) => def.get("items").and_then(|x| x.get("row")).and_then(|x| x.get("stream")).is_some(),
            })
            .collect();
        let Some(k) = acts.iter().position(|a| *a == o.act) else { return Effect::None };
        let n = acts.len() as i32;
        let next = acts[(k as i32 + dir).rem_euclid(n) as usize];
        self.stepping = true;
        let eff = self.act(&o.frame, module, &def, next);
        self.stepping = false;
        eff
    }

    /// The playing station's own screen (its `show` list) is in front and the stream came
    /// from a list: there hold ▲ / ▼ steps stations, as on Now playing (keys.rs `station`).
    pub fn station_in_front(&self) -> bool {
        let Some(s) = self.stream.as_ref().filter(|s| s.origin.is_some()) else { return false };
        let Some(Place::Screen { module, screen }) = self.stack.last().map(|f| &f.place) else { return false };
        matches!(s.frame.as_ref().map(|f| &f.place),
            Some(Place::Screen { module: m, screen: sh }) if m == module && sh == screen)
    }

    pub fn poll_fd(&self) -> BorrowedFd<'_> {
        self.poke_r.as_fd()
    }

    /// Main-loop poke: fetch results and stream notes arrived. True = redraw.
    pub fn on_ready(&mut self) -> (bool, Vec<radio_source::Note>) {
        let mut b = [0u8; 64];
        while matches!((&self.poke_r).read(&mut b), Ok(n) if n > 0) {}
        let done = std::mem::take(&mut *self.done.lock().unwrap_or_else(|e| e.into_inner()));
        let mut changed = !done.is_empty();
        for d in done {
            let np = self.stream.as_mut().and_then(|s| s.frame.as_mut());
            let Some(f) = self.stack.iter_mut().rev().chain(np).find(|f| f.load == Load::Loading(d.req)) else {
                continue;
            };
            match d.result {
                Ok(v) => {
                    if !d.append {
                        f.pages.clear();
                    }
                    f.pages.push(v);
                    f.load = Load::Ready;
                }
                Err(e) => {
                    info!("modules: get failed: {e}");
                    f.load = Load::Failed(e);
                }
            }
        }
        let notes = match self.stream.as_mut() {
            Some(s) => s.radio.on_ready(),
            None => Vec::new(),
        };
        changed |= !notes.is_empty();
        (changed, notes)
    }

    /// The fd of the stream's notes (poll it too while a stream plays).
    pub fn stream_fd(&self) -> Option<BorrowedFd<'_>> {
        self.stream.as_ref().map(|s| s.radio.poll_fd())
    }

    /// Installed modules' names, in load order (Home lists the first few).
    pub fn names(&self) -> Vec<String> {
        self.modules.iter().map(|m| m.name.clone()).collect()
    }

    /// Opens one module's start screen from Home; Back from it returns to Home.
    pub fn open_direct(&mut self, module: usize) {
        self.close_all();
        if module < self.modules.len() {
            self.open_module(module);
        }
    }

    /// Opens Main › Apps (or straight into the category when only one has modules).
    pub fn open_apps(&mut self) {
        self.close_all();
        let cats = self.categories();
        let place = match cats.as_slice() {
            [(only, _, _)] => Place::Category(only.to_string()),
            _ => Place::Apps,
        };
        self.stack.push(Frame::new(place, Vec::new()));
    }

    fn categories(&self) -> Vec<(&'static str, &'static str, usize)> {
        CATEGORIES
            .iter()
            .filter_map(|(id, label)| {
                let n = self.modules.iter().filter(|m| m.category == *id).count();
                (n > 0).then_some((*id, *label, n))
            })
            .collect()
    }

    fn module_ids_in(&self, cat: &str) -> Vec<usize> {
        (0..self.modules.len()).filter(|i| self.modules[*i].category == cat).collect()
    }

    fn screen_def(&self, module: usize, screen: &str) -> Option<&Value> {
        self.modules.get(module)?.menu.get("screens")?.get(screen)
    }

    fn open_module(&mut self, module: usize) {
        let start = self.modules[module].menu.get("start").and_then(Value::as_str).unwrap_or("main").to_string();
        self.push_screen(module, &start, Vec::new());
    }

    fn push_screen(&mut self, module: usize, screen: &str, binds: Vec<(String, Value)>) {
        let mut f = Frame::new(Place::Screen { module, screen: screen.to_string() }, binds);
        f.sel = self.memo.get(&(module, screen.to_string())).copied().unwrap_or(0);
        self.stack.push(f);
        self.start_get(false, false);
    }

    /// Keeps a closing screen's selected row for its next opening (not a stream's live list,
    /// whose rows move by themselves).
    fn keep_sel(&mut self, f: &Frame) {
        if let Place::Screen { module, screen } = &f.place {
            let live = self.screen_def(*module, screen).and_then(|d| d.get("header")).is_some();
            if !live {
                self.memo.insert((*module, screen.clone()), f.sel);
            }
        }
    }

    fn close_all(&mut self) {
        for f in std::mem::take(&mut self.stack) {
            self.keep_sel(&f);
        }
    }

    /// Fills the scope for a frame (settings, data, binds, stream, battery).
    fn with_scope<R>(&self, f: &Frame, row: Option<&Value>, k: impl FnOnce(&Scope) -> R) -> R {
        let module = match f.place {
            Place::Screen { module, .. } => Some(module),
            _ => None,
        };
        let empty = Value::Null;
        let settings = module.map(|m| Value::Object(self.modules[m].settings.clone())).unwrap_or(Value::Null);
        let data = f.pages.first().unwrap_or(&empty);
        let stream = self.stream_value(module);
        let mut sc = Scope::new();
        sc.push("settings", &settings);
        sc.push("battery", &self.battery);
        if let Some(s) = stream.as_deref() {
            sc.push("stream", s);
        }
        for (n, v) in &f.binds {
            sc.push(n, v);
        }
        sc.push("data", data);
        if let Some(r) = row {
            sc.push("row", r);
        }
        k(&sc)
    }

    /// `$stream.*` while this module's stream is the player's source.
    fn stream_value(&self, module: Option<usize>) -> Option<Rc<Value>> {
        let s = self.stream.as_ref()?;
        if module.is_some_and(|m| m != s.module) {
            return None;
        }
        let mut c = self.stream_cache.borrow_mut();
        if let Some((at, v)) = c.as_ref() {
            if at.elapsed().as_millis() < STREAM_CACHE_MS {
                return Some(v.clone());
            }
        }
        let v = Rc::new(stream_json(&s.radio.status(), s.paused));
        *c = Some((Instant::now(), v.clone()));
        Some(v)
    }

    /// Whether the module screen in front changes by itself (it shows `$stream` in a header
    /// while the stream plays): the daemon redraws it once a second while the screen is on.
    pub fn live(&self) -> bool {
        let Some(s) = self.stream.as_ref().filter(|s| !s.paused) else { return false };
        match self.stack.last().map(|f| &f.place) {
            Some(Place::Screen { module, screen }) if *module == s.module => {
                self.screen_def(*module, screen).and_then(|d| d.get("header")).is_some()
            }
            _ => false,
        }
    }

    /// The URL a frame's `get` fetches now (None = the screen has no `get`).
    fn get_url(&self, f: &Frame, append: bool) -> Option<Result<(String, usize), String>> {
        let Place::Screen { module, screen } = &f.place else { return None };
        let get = self.screen_def(*module, screen)?.get("get")?;
        let url_t = get.get("url").and_then(Value::as_str)?;
        let mut url = self.with_scope(f, None, |sc| fill(url_t, sc));
        if append {
            // `page = { param; value; }`: the value at the pointer in the last answer.
            let page = get.get("page");
            let param = page.and_then(|p| p.get("param")).and_then(Value::as_str);
            let ptr = page.and_then(|p| p.get("value")).and_then(Value::as_str);
            let val = match (f.pages.last(), ptr) {
                (Some(last), Some(p)) => pointer(last, p).and_then(|v| format_value(v, None)),
                _ => None,
            };
            url = match (url, param, val) {
                (Some(u), Some(p), Some(v)) => Some(set_param(&u, p, &v)),
                _ => None,
            };
        }
        Some(url.map(|u| (u, *module)).ok_or_else(|| "missing address".to_string()))
    }

    /// Starts a fetch on its own thread; returns its request id.
    fn spawn_get(&mut self, url: String, module: usize, append: bool) -> Result<u64, String> {
        let req = self.next_req;
        self.next_req += 1;
        let allow = self.modules[module].net.clone();
        let (net, done, poke) = (self.net.clone(), self.done.clone(), self.poke_w.clone());
        std::thread::Builder::new()
            .name("abyssd-mod-get".into())
            .spawn(move || {
                let result = fetch(&net, &allow, &url);
                done.lock().unwrap_or_else(|e| e.into_inner()).push(Done { req, result, append });
                if let Ok(mut p) = poke.lock() {
                    let _ = p.write(&[1]);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(req)
    }

    /// Starts the `get` of the top frame, or of the Now Playing frame (append = next page).
    fn start_get(&mut self, now_playing: bool, append: bool) {
        let f = if now_playing { self.stream.as_ref().and_then(|s| s.frame.as_ref()) } else { self.stack.last() };
        let Some(job) = f.and_then(|f| self.get_url(f, append)) else { return };
        let r = job.and_then(|(u, m)| self.spawn_get(u, m, append));
        let f = if now_playing { self.stream.as_mut().and_then(|s| s.frame.as_mut()) } else { self.stack.last_mut() };
        if let Some(f) = f {
            f.fetched_at = wall_now_ms();
            f.load = match r {
                Ok(req) => Load::Loading(req),
                Err(e) => Load::Failed(e),
            };
        }
    }

    /// Re-fetches due by `every` / `refreshAt` on the screen the person sees (screen on only).
    /// Returns the next due time (wall ms) for the main loop's timer, if any.
    pub fn tick(&mut self, screen_on: bool, visible: Visible) -> Option<i64> {
        let np = match visible {
            Visible::Module => false,
            Visible::NowPlaying => true,
            Visible::Other => return None,
        };
        let f = if np { self.stream.as_ref()?.frame.as_ref()? } else { self.stack.last()? };
        let Place::Screen { module, screen } = &f.place else { return None };
        let get = self.screen_def(*module, screen)?.get("get")?;
        if !screen_on || matches!(f.load, Load::Loading(_)) {
            return None;
        }
        let now = wall_now_ms();
        let every = get.get("every").and_then(Value::as_str).and_then(parse_dur_ms).map(|m| m.max(EVERY_MIN_MS) as i64);
        let at = get
            .get("refreshAt")
            .and_then(Value::as_str)
            .and_then(|p| pointer(f.pages.first()?, p))
            .and_then(Value::as_f64)
            .map(|v| v as i64 + 1500)
            .filter(|t| *t > f.fetched_at);
        let due = [every.map(|e| f.fetched_at + e), at].into_iter().flatten().min()?;
        if now >= due {
            self.start_get(np, false);
            return None;
        }
        Some(due)
    }

    /// The screen to draw.
    pub fn view(&self) -> Option<ScreenView> {
        let f = self.stack.last()?;
        Some(self.view_of(f))
    }

    /// Now Playing for the module stream: its `show` screen, else a plain stream view.
    pub fn now_playing_view(&self) -> Option<ScreenView> {
        let Some(s) = self.stream.as_ref() else { return self.ready_view() };
        if let Some(f) = &s.frame {
            let mut v = self.view_of(f);
            v.body = match v.body {
                Body::Text(l, _) => Body::Text(l, Vec::new()),
                // A list `show` screen (media:stream:screen): Now Playing draws its header only,
                // with the player's own keys (its guide is for the list).
                Body::Rows(_) => {
                    v.guide.clear();
                    Body::Text(Vec::new(), Vec::new())
                }
            };
            drop_bare_heads(&mut v.header, true);
            return Some(v);
        }
        let st = s.radio.status();
        let mut lines = vec![phase_text(&st, s.paused)];
        match &st.song {
            Some(song) => {
                lines.extend(song.text("artist").map(str::to_string));
                lines.extend(song.text("title").map(str::to_string));
                lines.extend(song.text("label").map(|l| format!("Label: {l}")));
            }
            None if st.other_on => lines.push("A jingle is on.".into()),
            None => {}
        }
        lines.extend(st.next.first().map(|n| format!("Coming up: {}", n.line())));
        Some(ScreenView {
            title: st.title.clone(),
            sub: None,
            header: Vec::new(),
            body: Body::Text(lines, Vec::new()),
            guide: Vec::new(),
            note: None,
            sel: 0,
        })
    }

    /// The last stream shown ready at entry: its name, and that ● plays it.
    fn ready_view(&self) -> Option<ScreenView> {
        let title = self.continue_title().filter(|_| self.armed)?;
        let line = |text: &str, style| Line { text: text.into(), style };
        Some(ScreenView {
            title: self.modules[self.last.as_ref()?.module].name.clone(),
            sub: None,
            header: vec![line(&title, Style::Big), line("● plays it.", Style::Dim)],
            body: Body::Text(Vec::new(), Vec::new()),
            guide: Vec::new(),
            note: None,
            sel: 0,
        })
    }

    fn view_of(&self, f: &Frame) -> ScreenView {
        let back = RowView { label: "Back".into(), meta: String::new(), dim: true };
        match &f.place {
            Place::Apps => {
                let mut rows: Vec<RowView> = self
                    .categories()
                    .into_iter()
                    .map(|(_, l, n)| RowView { label: format!("{l} ›"), meta: format!("{n}"), dim: false })
                    .collect();
                let note = match (rows.is_empty(), self.errors.len()) {
                    (true, 0) => Some(("No modules yet. Install them in Android: Settings › Abyss › Modules.".to_string(), false)),
                    (_, 0) => None,
                    (_, n) => Some((format!("{n} module(s) could not be opened in this build (Settings › Abyss › Modules)."), true)),
                };
                rows.push(back);
                ScreenView { title: "APPS".into(), sub: None, header: Vec::new(), body: Body::Rows(rows), guide: Vec::new(), note, sel: f.sel }
            }
            Place::Category(c) => {
                let mut rows: Vec<RowView> = self
                    .module_ids_in(c)
                    .into_iter()
                    .map(|i| RowView { label: format!("{} ›", self.modules[i].name), meta: String::new(), dim: false })
                    .collect();
                rows.push(back);
                let title = CATEGORIES.iter().find(|x| x.0 == c).map(|x| x.1).unwrap_or("APPS").to_uppercase();
                ScreenView { title, sub: None, header: Vec::new(), body: Body::Rows(rows), guide: Vec::new(), note: None, sel: f.sel }
            }
            Place::Screen { module, screen } => self.module_view(f, *module, screen),
        }
    }

    /// The items of a list screen: (row view, what selecting it does).
    fn items_of(&self, f: &Frame, def: &Value) -> Vec<(RowView, ItemAct)> {
        let mut out = Vec::new();
        match def.get("items") {
            Some(Value::Array(items)) => {
                for (i, it) in items.iter().enumerate() {
                    let label = it.get("label").and_then(Value::as_str).unwrap_or("");
                    let Some(label) = self.with_scope(f, None, |sc| fill(label, sc)) else { continue };
                    let meta = it.get("sub").and_then(Value::as_str).and_then(|s| self.with_scope(f, None, |sc| fill(s, sc)));
                    let goto = it.get("goto").is_some();
                    let label = if goto && !label.ends_with('›') { format!("{label} ›") } else { label };
                    out.push((RowView { label, meta: meta.unwrap_or_default(), dim: false }, ItemAct::Static(i)));
                }
            }
            Some(Value::Object(spec)) => {
                let from = spec.get("from").and_then(Value::as_str).unwrap_or("");
                let row = spec.get("row").cloned().unwrap_or(Value::Null);
                let reverse = spec.get("order").and_then(Value::as_str) == Some("reverse");
                let label_t = row.get("label").and_then(Value::as_str).unwrap_or("?");
                let sub_t = row.get("sub").and_then(Value::as_str);
                // `$data.…` lists come page by page; any other (`$stream.…`) once, from the scope.
                let pages: Vec<Option<&Value>> =
                    if from.starts_with("$data") { f.pages.iter().map(Some).collect() } else { vec![None] };
                for (pi, page) in pages.into_iter().enumerate() {
                    let mut pf = f.clone();
                    pf.pages = page.cloned().into_iter().collect();
                    let arr = self.with_scope(&pf, None, |sc| single(from, sc).and_then(Value::as_array).cloned());
                    let Some(arr) = arr else { continue };
                    let mut idx: Vec<usize> = (0..arr.len()).collect();
                    if reverse {
                        idx.reverse();
                    }
                    for i in idx {
                        let el = &arr[i];
                        let label = self.with_scope(f, Some(el), |sc| fill(label_t, sc)).filter(|l| !l.trim().is_empty());
                        let meta = sub_t.and_then(|s| self.with_scope(f, Some(el), |sc| fill(s, sc)));
                        out.push((
                            RowView { label: label.unwrap_or_else(|| "?".into()), meta: meta.unwrap_or_default(), dim: false },
                            ItemAct::Row(pi, i),
                        ));
                    }
                }
            }
            _ => {}
        }
        out
    }

    fn more_row(&self, f: &Frame, def: &Value) -> Option<RowView> {
        let page = def.get("get")?.get("page")?;
        let ptr = page.get("value")?.as_str()?;
        let last = f.pages.last()?;
        pointer(last, ptr).filter(|v| present(v))?;
        // Only while the last page brought something.
        let from = def.get("items")?.get("from")?.as_str()?;
        let mut pf = f.clone();
        pf.pages = vec![last.clone()];
        let n = self.with_scope(&pf, None, |sc| single(from, sc).and_then(Value::as_array).map(Vec::len)).unwrap_or(0);
        (n > 0).then(|| RowView {
            label: page.get("label").and_then(Value::as_str).unwrap_or("More").to_string(),
            meta: String::new(),
            dim: false,
        })
    }

    /// `lines` / `header` entries: a string, or `{ text; when?; unless?; style?; }`; a line whose
    /// holes are not all present, or whose `when` / `unless` says so, is left out.
    fn lines_of(&self, f: &Frame, lines: Option<&Value>) -> Vec<Line> {
        let mut out = Vec::new();
        for l in lines.and_then(Value::as_array).into_iter().flatten() {
            let (t, when, unless, style) = match l {
                Value::String(s) => (s.as_str(), None, None, None),
                Value::Object(o) => (
                    o.get("text").and_then(Value::as_str).unwrap_or(""),
                    o.get("when").and_then(Value::as_str),
                    o.get("unless").and_then(Value::as_str),
                    o.get("style").and_then(Value::as_str),
                ),
                _ => continue,
            };
            let text = self.with_scope(f, None, |sc| {
                let show = when.is_none_or(|w| truthy(single(w, sc))) && unless.is_none_or(|u| !truthy(single(u, sc)));
                if show {
                    fill(t, sc)
                } else {
                    None
                }
            });
            if let Some(text) = text {
                out.push(Line { text, style: Style::parse(style) });
            }
        }
        out
    }

    fn module_view(&self, f: &Frame, module: usize, screen: &str) -> ScreenView {
        let back = RowView { label: "Back".into(), meta: String::new(), dim: true };
        let Some(def) = self.screen_def(module, screen) else {
            return ScreenView {
                title: self.modules[module].name.to_uppercase(),
                sub: None,
                header: Vec::new(),
                body: Body::Rows(vec![back]),
                guide: Vec::new(),
                note: Some((format!("This module has no screen '{screen}'."), true)),
                sel: 0,
            };
        };
        let text = |k: &str| def.get(k).and_then(Value::as_str);
        let title = text("title").and_then(|t| self.with_scope(f, None, |sc| fill(t, sc))).unwrap_or_else(|| self.modules[module].name.to_uppercase());
        let sub = text("sub").and_then(|t| self.with_scope(f, None, |sc| fill(t, sc)));
        let guide = text("guide").map(guide_pairs).unwrap_or_default();
        let has_get = def.get("get").is_some();
        let note = match &f.load {
            Load::Loading(_) if f.pages.is_empty() => Some(("Loading…".to_string(), false)),
            Load::Failed(e) if e.contains("not an approved address") => Some((NEW_ADDRESS_TEXT.to_string(), true)),
            Load::Failed(_) => Some((text("error").unwrap_or("Couldn't load this screen.").to_string(), true)),
            _ => None,
        };
        let kind = text("kind").unwrap_or("list");
        let body = match kind {
            "text" => {
                let lines = if has_get && f.pages.is_empty() {
                    Vec::new()
                } else {
                    self.lines_of(f, def.get("lines")).into_iter().map(|l| l.text).collect()
                };
                Body::Text(lines, vec![back])
            }
            "list" => {
                let mut rows: Vec<RowView> = self.items_of(f, def).into_iter().map(|(r, _)| r).collect();
                // A list from `$data` is empty once loaded; one from `$stream` (no get) at once.
                let from_scope = def.get("items").is_some_and(Value::is_object) && !has_get;
                let empty = rows.is_empty() && (f.load == Load::Ready || from_scope);
                if let Some(m) = self.more_row(f, def) {
                    rows.push(m);
                }
                rows.push(back);
                let mut header = if has_get && f.pages.is_empty() { Vec::new() } else { self.lines_of(f, def.get("header")) };
                drop_bare_heads(&mut header, false);
                let note = note.or_else(|| empty.then(|| (text("empty").unwrap_or("Nothing here.").to_string(), false)));
                return ScreenView { title, sub, header, body: Body::Rows(rows), guide, note, sel: f.sel };
            }
            _ => {
                return ScreenView {
                    title,
                    sub,
                    header: Vec::new(),
                    body: Body::Rows(vec![back]),
                    guide,
                    note: Some(("This screen needs a newer IchthysOS.".into(), true)),
                    sel: 0,
                }
            }
        };
        ScreenView { title, sub, header: Vec::new(), body, guide, note, sel: f.sel }
    }

    fn row_count(&self) -> usize {
        match self.view().map(|v| v.body) {
            Some(Body::Rows(r)) | Some(Body::Text(_, r)) => r.len(),
            None => 0,
        }
    }

    pub fn up(&mut self) {
        let n = self.row_count();
        if let Some(f) = self.stack.last_mut() {
            if n > 0 {
                f.sel = if f.sel == 0 { n - 1 } else { f.sel - 1 };
            }
        }
    }

    pub fn down(&mut self) {
        let n = self.row_count();
        if let Some(f) = self.stack.last_mut() {
            if n > 0 {
                f.sel = (f.sel + 1) % n;
            }
        }
    }

    /// Back (the Back row, or ● ● from a module screen). Exit = leave the module screens.
    pub fn back(&mut self) -> Effect {
        if let Some(f) = self.stack.pop() {
            self.keep_sel(&f);
        }
        if self.stack.is_empty() {
            Effect::Exit
        } else {
            Effect::None
        }
    }

    /// ● on the selected row.
    pub fn select(&mut self) -> Effect {
        let Some(f) = self.stack.last().cloned() else { return Effect::Exit };
        let n = self.row_count();
        if f.sel + 1 >= n {
            return self.back(); // the last row is always Back
        }
        match &f.place {
            Place::Apps => {
                let cats = self.categories();
                if let Some((id, _, _)) = cats.get(f.sel) {
                    self.stack.push(Frame::new(Place::Category(id.to_string()), Vec::new()));
                }
                Effect::None
            }
            Place::Category(c) => {
                if let Some(&m) = self.module_ids_in(c).get(f.sel) {
                    self.open_module(m);
                }
                Effect::None
            }
            Place::Screen { module, screen } => {
                let (module, screen) = (*module, screen.clone());
                let Some(def) = self.screen_def(module, &screen).cloned() else { return Effect::None };
                let items = self.items_of(&f, &def);
                if f.sel == items.len() && self.more_row(&f, &def).is_some() {
                    self.start_get(false, true);
                    return Effect::None;
                }
                let Some((_, act)) = items.into_iter().nth(f.sel) else { return Effect::None };
                self.act(&f, module, &def, act)
            }
        }
    }

    fn act(&mut self, f: &Frame, module: usize, def: &Value, act: ItemAct) -> Effect {
        let (item, el): (Value, Option<Value>) = match act {
            ItemAct::Static(i) => (def.get("items").and_then(|a| a.get(i)).cloned().unwrap_or(Value::Null), None),
            ItemAct::Row(pi, i) => {
                let row = def.get("items").and_then(|s| s.get("row")).cloned().unwrap_or(Value::Null);
                let from = def.get("items").and_then(|s| s.get("from")).and_then(Value::as_str).unwrap_or("");
                let mut pf = f.clone();
                if from.starts_with("$data") {
                    pf.pages = f.pages.get(pi).cloned().into_iter().collect();
                }
                let el = self.with_scope(&pf, None, |sc| single(from, sc).and_then(|a| a.get(i)).cloned());
                (row, el)
            }
        };
        let s = |k: &str| item.get(k).and_then(Value::as_str);
        if let Some(goto) = s("goto") {
            let mut binds = f.binds.clone();
            if let (Some(name), Some(el)) = (s("as"), el.clone()) {
                if !RESERVED_NS.contains(&name) {
                    binds.push((name.to_string(), el));
                }
            }
            self.push_screen(module, goto, binds);
            return Effect::None;
        }
        if item.get("back").and_then(Value::as_bool) == Some(true) {
            return self.back();
        }
        if let Some(st) = item.get("stream") {
            // media:stream:screen: `as` passes the picked row on to the stream's holes and its
            // `show` screen, as for goto.
            let mut sf = f.clone();
            if let (Some(name), Some(el)) = (s("as"), el.clone()) {
                if !RESERVED_NS.contains(&name) {
                    sf.binds.push((name.to_string(), el));
                }
            }
            let eff = self.start_stream(&sf, module, st);
            if matches!(eff, Effect::Play { .. } | Effect::Continue { .. }) {
                let origin = Origin { frame: f.clone(), act };
                if let Some(s) = self.stream.as_mut() {
                    s.origin = Some(origin.clone());
                }
                if matches!(eff, Effect::Play { .. }) {
                    self.remember_last(origin);
                }
                self.last_is_source = true;
            }
            return eff;
        }
        if let Some(m) = s("media") {
            return Effect::Media(m.to_string());
        }
        for k in ["say", "show"] {
            if let Some(t) = s(k) {
                return Effect::Show(self.with_scope(f, el.as_ref(), |sc| fill(t, sc)).unwrap_or_default());
            }
        }
        if let Some(b) = s("buzz") {
            return Effect::Buzz(b.to_string());
        }
        if item.get("battery").and_then(Value::as_bool) == Some(true) {
            return Effect::Battery;
        }
        if item.get("run").is_some() {
            return Effect::Unsupported("Scripts come with Termux staging (not in this build).".into());
        }
        Effect::Unsupported("This needs a newer IchthysOS.".into())
    }

    fn start_stream(&mut self, f: &Frame, module: usize, st: &Value) -> Effect {
        let cfg = self.with_scope(f, None, |sc| stream_config(st, sc));
        let mut cfg = match cfg {
            Ok(c) => c,
            Err(e) => return Effect::Unsupported(e),
        };
        cfg.allow = Some(self.modules[module].net.clone());
        // The same station in the same format is already the stream: keep it (no new prebuffer,
        // no gap), just show it, like Resume does for a library track.
        let key = (cfg.tag.clone(), cfg.format.clone());
        let show = st.get("show").and_then(Value::as_str).filter(|sh| self.screen_def(module, sh).is_some());
        // A list screen as `show` (media:stream:screen) opens on the module screens instead of
        // Now Playing; Now Playing draws its header.
        let show_list = show
            .filter(|sh| self.screen_def(module, sh).and_then(|d| d.get("kind")).and_then(Value::as_str).unwrap_or("list") == "list");
        if self.stream.as_ref().is_some_and(|s| s.module == module && s.key == key) {
            if let Some(sh) = show_list.filter(|_| !self.stepping) {
                self.open_show(module, sh, f.binds.clone());
            }
            return Effect::Continue { player: show_list.is_none() };
        }
        self.stop_stream();
        let frame = show.map(|sh| Frame::new(Place::Screen { module, screen: sh.to_string() }, f.binds.clone()));
        match Radio::start(cfg, self.net.clone()) {
            Ok(radio) => {
                let feed = radio.feed();
                self.stream = Some(StreamSession { module, origin: None, key, radio, frame, paused: false });
                self.armed = false;
                self.start_get(true, false); // the show screen's own data ("Coming up")
                if let Some(sh) = show {
                    // An open screen of the old station shows the new one.
                    for fr in self.stack.iter_mut() {
                        if matches!(&fr.place, Place::Screen { module: m, screen: s } if *m == module && s == sh) {
                            fr.binds = f.binds.clone();
                        }
                    }
                }
                if let Some(sh) = show_list.filter(|_| !self.stepping) {
                    self.open_show(module, sh, f.binds.clone());
                }
                Effect::Play { feed, player: show_list.is_none() }
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Effect::Unsupported(NEW_ADDRESS_TEXT.into()),
            Err(e) => Effect::Unsupported(format!("Can't play this: {e}")),
        }
    }

    /// Opens the stream's list `show` screen on top of the module screens (not twice).
    fn open_show(&mut self, module: usize, screen: &str, binds: Vec<(String, Value)>) {
        let on_top = matches!(self.stack.last().map(|f| &f.place),
            Some(Place::Screen { module: m, screen: s }) if *m == module && s == screen);
        if !on_top {
            self.push_screen(module, screen, binds);
        }
    }

    /// The person paused / resumed the player while the stream is its source.
    pub fn set_paused(&mut self, paused: bool) {
        if let Some(s) = self.stream.as_mut() {
            s.paused = paused;
        }
        self.stream_cache.replace(None);
    }

    pub fn stream_status(&self) -> Option<RadioStatus> {
        self.stream.as_ref().map(|s| s.radio.status())
    }

    pub fn streaming(&self) -> bool {
        self.stream.is_some()
    }

    /// Ends the stream (another source took the player, or the person stopped it).
    pub fn stop_stream(&mut self) {
        self.stream_cache.replace(None);
        if let Some(mut s) = self.stream.take() {
            let st = s.radio.status();
            info!(
                "stream {} ({:?}) stopped: {} asks, {} resets, {} stalls, prebuffer {:.0} %{}",
                st.tag,
                st.mode,
                st.asks,
                st.resets,
                st.stalls,
                st.prebuffer * 100.0,
                st.last_error.map(|e| format!(", last problem: {e}")).unwrap_or_default()
            );
            s.radio.stop(500);
        }
    }

    /// The capture chord: the module's `capture` while its stream plays or one of its screens
    /// is in front (`module_in_front`). None = no module applies (the daemon falls back).
    pub fn capture(&self, module_in_front: bool) -> Option<Result<Capture, &'static str>> {
        let module = match (&self.stream, self.stack.last().map(|f| &f.place)) {
            (Some(s), _) => s.module,
            (None, Some(Place::Screen { module, .. })) if module_in_front => *module,
            _ => return None,
        };
        let m = &self.modules[module];
        let Some(cap) = m.manifest.get("capture") else {
            // No mapping: the stream's own record of the song you hear.
            let s = self.stream.as_ref()?;
            return Some(match s.radio.capture() {
                RadioCapture::Song(c) => Ok(c),
                RadioCapture::Pending => Err("Saving it when the song's name arrives…"),
                RadioCapture::Nothing(w) => Err(w),
            });
        };
        let f = self.stack.last().cloned().unwrap_or_else(|| Frame::new(Place::Apps, Vec::new()));
        let mut f = f;
        if !matches!(&f.place, Place::Screen { module: x, .. } if *x == module) {
            f = Frame::new(Place::Screen { module, screen: String::new() }, Vec::new());
        }
        Some(self.with_scope(&f, None, |sc| capture_of(cap, sc)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemAct {
    Static(usize),
    Row(usize, usize),
}

fn set_param(url: &str, param: &str, value: &str) -> String {
    let (base, q) = url.split_once('?').unwrap_or((url, ""));
    let mut parts: Vec<String> =
        q.split('&').filter(|p| !p.is_empty() && p.split('=').next() != Some(param)).map(str::to_string).collect();
    parts.push(format!("{param}={}", crate::http::encode_query(&[("", value.to_string())]).trim_start_matches('=')));
    format!("{base}?{}", parts.join("&"))
}

/// One `get`: a lease, the allowlist, 256 KiB, JSON.
fn fetch(net: &Arc<dyn Network>, allow: &[String], url: &str) -> Result<Value, String> {
    let u = Url::parse(url).map_err(|e| e.to_string())?;
    let client = Client::new(net.transport(), Cancel::default()).with_allow(allow.to_vec());
    if !client.allows(&u) {
        warn!("modules: refused {} (not an approved address)", u.origin_text());
        return Err(format!("{} is not an approved address", u.origin_text()));
    }
    let hold = net.acquire("module-get");
    let mut last = String::new();
    // The link may still be coming up: a few quick tries inside the lease.
    for i in 0..8 {
        match client.get(&u, &[("Accept", "application/json")], Timeouts::io_s(15)) {
            Ok(r) => {
                let status = r.status;
                let body = r.bytes(GET_MAX);
                hold.release(Some(30_000));
                let body = body.map_err(|e| e.to_string())?;
                let v: Value = serde_json::from_slice(&body).map_err(|e| format!("not JSON: {e}"))?;
                if status != 200 {
                    return Err(format!("HTTP {status}: {}", v.get("error").and_then(Value::as_str).unwrap_or("?")));
                }
                return Ok(v);
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                hold.release(None);
                return Err(e.to_string());
            }
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(Duration::from_millis(400 * (i + 1)));
            }
        }
    }
    hold.release(None);
    Err(last)
}

/// `$stream` as JSON (contract §5.3).
pub fn stream_json(st: &RadioStatus, paused: bool) -> Value {
    let state = if paused {
        "paused"
    } else {
        match st.phase {
            Phase::Playing => "playing",
            Phase::Stalled | Phase::Offline(_) | Phase::OffAir => "stalled",
            _ => "buffering",
        }
    };
    let mut o = Map::new();
    let mut put = |k: &str, v: &str| {
        if !v.is_empty() {
            o.insert(k.into(), Value::from(v));
        }
    };
    put("tag", &st.tag);
    put("title", &st.title);
    put("format", &st.format);
    put("state", state);
    put("codec", st.codec.unwrap_or(""));
    if let Some(l) = st.latency_s {
        o.insert("behind_s".into(), Value::from((l * 10.0).round() / 10.0));
    }
    o.insert("buffered_s".into(), Value::from((st.buffered_s * 10.0).round() / 10.0));
    if let Some(t) = st.heard_ms {
        o.insert("heard_ms".into(), Value::from(t));
    }
    if let Some(e) = &st.last_error {
        o.insert("problem".into(), Value::from(e.clone()));
    }
    if let Some(s) = &st.song {
        o.insert("song".into(), Value::Object(s.fields.clone()));
        // media:stream:screen: how long the song you hear still plays (its end - what you hear).
        if let (Some(end), Some(t)) = (s.ends_at_ms, st.heard_ms) {
            o.insert("left_ms".into(), Value::from((end - t).max(0)));
        }
    } else if st.other_on {
        o.insert("jingle".into(), Value::Bool(true));
    }
    let songs = |l: &[radio_source::Song]| Value::Array(l.iter().map(|s| Value::Object(s.fields.clone())).collect());
    if !st.played.is_empty() {
        o.insert("played".into(), songs(&st.played));
    }
    if !st.coming.is_empty() {
        o.insert("next".into(), songs(&st.coming));
    }
    Value::Object(o)
}

fn phase_text(st: &RadioStatus, paused: bool) -> String {
    if paused {
        return "Paused".into();
    }
    match &st.phase {
        Phase::Starting => "Starting…".into(),
        Phase::Buffering => format!("Buffering {:.0} %", st.prebuffer * 100.0),
        Phase::Playing => format!("Playing · {:.0} s behind live", st.latency_s.unwrap_or(0.0)),
        Phase::Stalled => "Waiting for the next burst…".into(),
        Phase::Offline(_) => "No network: trying again…".into(),
        Phase::OffAir => "The station is not on air right now.".into(),
        Phase::Stopped => "Stopped".into(),
    }
}

/// A `stream = { … }` item, holes filled, into a radio_source config.
pub fn stream_config(st: &Value, sc: &Scope) -> Result<Config, String> {
    let s = |k: &str| st.get(k).and_then(Value::as_str);
    let filled = |k: &str| s(k).and_then(|t| fill(t, sc));
    let protocol = s("protocol").unwrap_or("http");
    let mode = match protocol {
        "radio-v1" => Mode::Bursts,
        "http" => Mode::Http,
        p => return Err(format!("Stream protocol '{p}' needs a newer IchthysOS.")),
    };
    let mut format = filled("format").unwrap_or_default();
    let mut mime = None;
    let mut long = None; // radio #84: the format's advertised long-burst limits
    let url = if let Some(u) = filled("url") {
        u
    } else {
        let formats = s("formats").and_then(|t| single(t, sc)).and_then(Value::as_object).ok_or("This stream has no address.")?;
        let (name, entry) = formats
            .get_key_value(&format)
            .or_else(|| formats.iter().next())
            .ok_or("This station has no audio.")?;
        format = name.clone();
        mime = entry.get("type").and_then(Value::as_str).map(str::to_string);
        long = entry.get("long_bursts").and_then(radio_source::LongBursts::from_json);
        entry.get("url").and_then(Value::as_str).ok_or("This stream has no address.")?.to_string()
    };
    let audio = Url::parse(&url).map_err(|e| e.to_string())?;
    let mut cfg = Config::new(audio);
    cfg.mode = mode;
    cfg.long = long.filter(|_| mode == Mode::Bursts);
    cfg.songs = filled("songs").and_then(|u| Url::parse(&u).ok());
    // media:stream:screen: the station's `now` link keeps `$stream.next` ("Up next") known.
    cfg.now = filled("now").and_then(|u| Url::parse(&u).ok());
    cfg.tag = filled("tag").unwrap_or_default();
    cfg.title = filled("title").unwrap_or_default();
    cfg.format = codec_hint(&format, mime.as_deref()).map(|_| format.clone()).unwrap_or(format);
    let pre = match st.get("prebuffer") {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(t)) => single(t, sc).and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok())),
        _ => None,
    };
    cfg.start_s = pre.unwrap_or(START_S).clamp(5.0, 30.0);
    Ok(cfg)
}

/// The record of a `capture = { line; kind; fields; }` (contract §5.4).
pub fn capture_of(cap: &Value, sc: &Scope) -> Result<Capture, &'static str> {
    let line_t = cap.get("line").and_then(Value::as_str).ok_or("Nothing to capture here.")?;
    let line = fill(line_t, sc).filter(|l| !l.trim().is_empty()).ok_or("Nothing to capture: no song is known right now.")?;
    let kind = static_kind(cap.get("kind").and_then(Value::as_str).unwrap_or("app"));
    let mut fields = BTreeMap::new();
    for (k, v) in cap.get("fields").and_then(Value::as_object).into_iter().flatten() {
        let Some(t) = v.as_str() else {
            fields.insert(k.clone(), v.clone());
            continue;
        };
        let val = if is_single_hole(t) {
            single(t, sc).filter(|v| present(v)).cloned()
        } else {
            fill(t, sc).map(Value::from)
        };
        if let Some(val) = val {
            fields.insert(k.clone(), val);
        }
    }
    Ok(Capture::new(kind, line, fields))
}

/// capture.rs keeps `kind` as a &'static str: known kinds map directly, others are kept once.
fn static_kind(k: &str) -> &'static str {
    match k {
        "radio" => "radio",
        "app" => "app",
        "music" => "music",
        "note" => "note",
        "track" => "track",
        other => {
            static KINDS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
            let mut g = KINDS.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(k) = g.iter().find(|x| **x == other) {
                return k;
            }
            if g.len() >= 32 {
                return "app";
            }
            let s: &'static str = Box::leak(other.to_string().into_boxed_str());
            g.push(s);
            s
        }
    }
}

/// /system_ext/etc/abyss/features as it must be shipped (one feature per line).
#[cfg(test)]
pub fn features_file() -> String {
    let mut s = String::from("# Features of this abyssd's module host (abyssd/src/modules.rs FEATURES).\n");
    for f in FEATURES {
        s.push_str(f);
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hole_grammar() {
        use Piece::*;
        let h = |ns: &str, keys: &[&str], fmt| Hole { ns: ns.into(), keys: keys.iter().map(|k| k.to_string()).collect(), fmt };
        assert_eq!(parse_holes("$settings.server/api/v1/stations"), vec![h("settings", &["server"], None), Lit("/api/v1/stations".into())]);
        assert_eq!(parse_holes("$row.bpm.0-$row.bpm.1"), vec![h("row", &["bpm", "0"], None), Lit("-".into()), h("row", &["bpm", "1"], None)]);
        assert_eq!(parse_holes("Ends $data.item.ends_at_ms:time."), vec![Lit("Ends ".into()), h("data", &["item", "ends_at_ms"], Some(Fmt::Time)), Lit(".".into())]);
        assert_eq!(parse_holes("$a.b:timex"), vec![h("a", &["b"], None), Lit(":timex".into())]);
        assert_eq!(parse_holes("$x.mp3-128.url"), vec![h("x", &["mp3-128", "url"], None)]);
        assert_eq!(parse_holes("cost $5 · ok"), vec![Lit("cost $5 · ok".into())]);
        assert_eq!(parse_holes("$station.tracklist_url?limit=20"), vec![h("station", &["tracklist_url"], None), Lit("?limit=20".into())]);
    }

    #[test]
    fn fill_rules() {
        let data = json!({"item": {"artist": "A", "mix": "", "bpm": 150, "dur": 205250, "est": false, "zero": 0}, "next": [{"line": "N0"}]});
        let st = json!({"name": "HARDSTYLE"});
        let mut sc = Scope::new();
        sc.push("data", &data);
        sc.push("station", &st);
        assert_eq!(fill("$data.item.artist · $station.name", &sc).as_deref(), Some("A · HARDSTYLE"));
        assert_eq!(fill("Mix: $data.item.mix", &sc), None, "empty string = missing");
        assert_eq!(fill("Est: $data.item.est", &sc), None, "false = missing");
        assert_eq!(fill("$data.item.zero BPM", &sc).as_deref(), Some("0 BPM"), "0 is present in a line");
        assert!(!truthy(single("$data.item.zero", &sc)), "but not true for when");
        assert_eq!(fill("$data.item.bpm BPM", &sc).as_deref(), Some("150 BPM"));
        assert_eq!(fill("Next: $data.next.0.line", &sc).as_deref(), Some("Next: N0"));
        assert_eq!(fill("Then: $data.next.1.line", &sc), None);
        assert_eq!(fill("$data.item.dur:dur", &sc).as_deref(), Some("3:25"));
        assert_eq!(fill("$data.item.dur:time", &sc).map(|s| s.len()), Some(5));
        assert_eq!(single("$data.item.bpm", &sc), Some(&json!(150)));
        assert_eq!(single("x $data.item.bpm", &sc), None);
        assert_eq!(parse_dur_ms("30s"), Some(30_000));
        assert_eq!(parse_dur_ms("1.5m"), Some(90_000));
        assert_eq!(parse_dur_ms("10"), None);
        assert_eq!(guide_pairs("▲▼ move  ● open"), vec![("▲▼".into(), "move".into()), ("●".into(), "open".into())]);
        assert_eq!(set_param("http://h/t?limit=20&before_ms=1", "before_ms", "99"), "http://h/t?limit=20&before_ms=99");
    }

    #[test]
    fn env_and_settings_types() {
        let env = parse_env("#abyss-settings v1\n# owner x\nSERVER=http://h:1\nFORMAT=mp3\nPREBUFFER_SECONDS=12\nQ=\"a \\\"b\\\" \\$c\"\nBAD=99\n");
        assert_eq!(env.get("Q").map(String::as_str), Some("a \"b\" $c"));
        let man = json!({"settings": {
            "server": {"type": "text", "default": "http://d", "env": "SERVER"},
            "format": {"type": "enum", "choices": [{"value": "flac"}, {"value": "mp3"}], "default": "flac", "env": "FORMAT"},
            "prebufferSeconds": {"type": "number", "integer": true, "min": 5, "max": 30, "default": 15, "env": "PREBUFFER_SECONDS"},
            "bad": {"type": "number", "min": 1, "max": 10, "default": 3, "env": "BAD"},
            "loud": {"type": "bool", "default": false},
            "key": {"type": "secret"}
        }});
        let v = settings_values(&man, &env);
        assert_eq!(v["server"], json!("http://h:1"));
        assert_eq!(v["format"], json!("mp3"));
        assert_eq!(v["prebufferSeconds"], json!(12));
        assert_eq!(v["bad"], json!(3), "out of range: default");
        assert_eq!(v["loud"], json!(false));
        assert!(!v.contains_key("key"), "secrets are never holes");
        assert_eq!(upper_snake("prebufferSeconds"), "PREBUFFER_SECONDS");
    }

    #[test]
    fn stream_item_and_capture_mapping() {
        let station = json!({"id": "hardstyle", "name": "HARDSTYLE",
            "audio": {"flac": {"url": "http://radio.example:8080/api/v1/audio/hardstyle/flac", "type": "audio/flac"},
                      "mp3": {"url": "http://radio.example:8080/api/v1/audio/hardstyle/mp3", "type": "audio/mpeg"}},
            "tracklist_url": "http://radio.example:8080/api/v1/tracklist/hardstyle"});
        let settings = json!({"format": "opus", "prebufferSeconds": 40});
        let mut sc = Scope::new();
        sc.push("settings", &settings);
        sc.push("station", &station);
        let item = json!({"protocol": "radio-v1", "formats": "$station.audio", "format": "$settings.format",
            "prebuffer": "$settings.prebufferSeconds", "songs": "$station.tracklist_url", "tag": "$station.id", "title": "$station.name"});
        let c = stream_config(&item, &sc).unwrap();
        assert_eq!(c.mode, Mode::Bursts);
        assert_eq!(c.audio.path(), "/api/v1/audio/hardstyle/flac", "unknown format: the first entry");
        assert_eq!(c.format, "flac");
        assert_eq!(c.start_s, 30.0, "clamped");
        assert_eq!((c.tag.as_str(), c.title.as_str()), ("hardstyle", "HARDSTYLE"));
        assert!(c.songs.is_some());
        assert!(stream_config(&json!({"protocol": "hls", "url": "http://x/"}), &sc).is_err());

        let stream = json!({"tag": "hardstyle", "title": "HARDSTYLE", "song": {"line": "A - T (L C1)", "artist": "A",
            "title": "T", "label": "L", "catalog_number": "C1", "play_id": 45502, "on_air_ms": 1794129776596i64, "kind": "music"}});
        let mut sc = Scope::new();
        sc.push("stream", &stream);
        let cap = json!({"kind": "radio", "line": "$stream.song.line", "fields": {"artist": "$stream.song.artist",
            "mix": "$stream.song.mix", "play_id": "$stream.song.play_id", "on_air_ms": "$stream.song.on_air_ms",
            "song_kind": "$stream.song.kind", "station": "$stream.tag", "source": "gabbee-radio", "both": "$stream.tag/$stream.title"}});
        let c = capture_of(&cap, &sc).unwrap();
        let v: Value = serde_json::from_str(&c.to_json_line()).unwrap();
        assert_eq!(v["kind"], "radio");
        assert_eq!(v["line"], "A - T (L C1)");
        assert_eq!(v["fields"]["play_id"], json!(45502), "a single hole keeps its JSON type");
        assert_eq!(v["fields"]["on_air_ms"], json!(1794129776596i64));
        assert_eq!(v["fields"]["source"], "gabbee-radio");
        assert_eq!(v["fields"]["both"], "hardstyle/HARDSTYLE");
        assert!(v["fields"].get("mix").is_none(), "missing parts are left out");
        let empty = json!({});
        let mut sc = Scope::new();
        sc.push("stream", &empty);
        assert!(capture_of(&cap, &sc).is_err(), "no song: nothing saved");
        assert_eq!(static_kind("x-acme-thing"), "x-acme-thing");
    }

    #[test]
    fn header_lines_styles_and_bare_heads() {
        let mk = |t: &str, style| Line { text: t.into(), style };
        let l0 = vec![mk("A", Style::Big), mk("UP NEXT", Style::Head), mk("PLAYED", Style::Head), mk("x", Style::Plain), mk("END", Style::Head)];
        let mut l = l0.clone();
        drop_bare_heads(&mut l, true);
        assert_eq!(l, vec![mk("A", Style::Big), mk("PLAYED", Style::Head), mk("x", Style::Plain)]);
        let mut l = l0;
        drop_bare_heads(&mut l, false);
        assert_eq!(l, vec![mk("A", Style::Big), mk("PLAYED", Style::Head), mk("x", Style::Plain), mk("END", Style::Head)], "rows follow the last one");
        assert_eq!(Style::parse(Some("dim")), Style::Dim);
        assert_eq!(Style::parse(Some("nope")), Style::Plain, "an unknown style draws plain");
        assert_eq!(Style::parse(None), Style::Plain);
    }

    #[test]
    fn stream_now_link_and_song_lists() {
        let station = json!({"id": "hs", "name": "HS", "audio": {"mp3": {"url": "http://radio.example:8080/a/mp3"}},
            "now_url": "http://radio.example:8080/api/v1/now/hs"});
        let mut sc = Scope::new();
        sc.push("station", &station);
        let c = stream_config(&json!({"protocol": "radio-v1", "formats": "$station.audio", "now": "$station.now_url"}), &sc).unwrap();
        assert_eq!(c.now.as_ref().map(|u| u.path().to_string()).as_deref(), Some("/api/v1/now/hs"));
        let song = |id: i64, on: i64, end: Option<i64>| {
            let mut v = json!({"play_id": id, "kind": "music", "artist": format!("A{id}"), "on_air_ms": on});
            if let Some(e) = end {
                v["ends_at_ms"] = json!(e);
            }
            radio_source::Song::from_json(&v).unwrap()
        };
        let st = RadioStatus {
            tag: "hs".into(),
            title: "HS".into(),
            format: "mp3".into(),
            codec: Some("MP3"),
            phase: Phase::Playing,
            song: Some(song(3, 3000, Some(10_000))),
            other_on: false,
            next: Vec::new(),
            played: vec![song(2, 2000, None), song(1, 1000, None)],
            coming: vec![song(4, 10_000, None)],
            heard_ms: Some(4500),
            buffered_s: 10.0,
            prebuffer: 1.0,
            latency_s: Some(20.0),
            mode: Mode::Bursts,
            asks: 1,
            resets: 0,
            stalls: 0,
            last_error: None,
        };
        let v = stream_json(&st, false);
        assert_eq!(v["left_ms"], json!(5500));
        assert_eq!(v["played"][0]["artist"], "A2", "newest first");
        assert_eq!(v["next"][0]["artist"], "A4");
        assert!(v.get("jingle").is_none());
        let mut sc = Scope::new();
        sc.push("stream", &v);
        assert_eq!(fill("$stream.left_ms:dur left", &sc).as_deref(), Some("0:05 left"));
        let jingle = RadioStatus { song: None, other_on: true, played: Vec::new(), coming: Vec::new(), ..st };
        let v = stream_json(&jingle, false);
        assert_eq!(v["jingle"], json!(true));
        assert!(v.get("left_ms").is_none() && v.get("played").is_none() && v.get("next").is_none(), "empty lists are left out");
    }

    #[test]
    fn stream_show_list_screen() {
        let root = std::env::temp_dir().join(format!("abyss-mod-show-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = root.join("store");
        store_with(&store, "radio", "ready", &["apps", "menu:get", "media:stream", "media:stream:http", "media:stream:screen", "menu:header"]);
        fs::write(store.join("radio/menu.json"), json!({"menu": 1, "start": "main", "screens": {
            "main": {"title": "T", "items": [{"label": "Play", "stream": {"url": "http://127.0.0.1:9/a", "tag": "a", "title": "A", "show": "np"}}]},
            "np": {"title": "$stream.title", "header": [
                    {"text": "Not playing.", "unless": "$stream"},
                    {"text": "$stream.song.artist", "style": "big"},
                    {"text": "Starting...", "when": "$stream", "unless": "$stream.heard_ms", "style": "dim"},
                    {"text": "UP NEXT", "style": "head", "when": "$stream.next.0"},
                    "$stream.next.0.line",
                    {"text": "PLAYED BEFORE", "style": "head"}],
                "items": {"from": "$stream.played", "row": {"label": "$row.artist", "goto": "main", "as": "song"}},
                "empty": "Nothing played yet.", "guide": "▲▼ scroll"}}}).to_string()).unwrap();
        let (mods, errs) = load(&store, &root.join("settings"));
        assert!(errs.is_empty(), "{errs:?}");
        let net: Arc<dyn Network> = Arc::new(radio_source::OpenNetwork(Arc::new(crate::http::TcpTransport::default())));
        let mut h = Host::new(mods, errs, net).unwrap();
        h.open_direct(0);
        assert!(matches!(h.select(), Effect::Play { player: false, .. }), "a list show screen opens on the module screens");
        let v = h.view().unwrap();
        assert_eq!(v.title, "A");
        let texts: Vec<(&str, Style)> = v.header.iter().map(|l| (l.text.as_str(), l.style)).collect();
        assert_eq!(texts, vec![("Starting...", Style::Dim), ("PLAYED BEFORE", Style::Head)]);
        assert_eq!(v.body, Body::Rows(vec![RowView { label: "Back".into(), meta: String::new(), dim: true }]));
        assert_eq!(v.note, Some(("Nothing played yet.".into(), false)), "a $stream list is empty at once");
        assert!(h.live(), "a header screen of the playing stream redraws by itself");
        // Now Playing draws the header, without a heading that nothing follows.
        let np = h.now_playing_view().unwrap();
        assert_eq!(np.header.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), vec!["Starting..."]);
        assert_eq!(np.body, Body::Text(Vec::new(), Vec::new()));
        assert!(np.guide.is_empty(), "the player's own keys, not the list's");
        // Back, then the same stream again: kept, and its screen opens again (once).
        assert!(matches!(h.select(), Effect::None), "Back from np");
        assert_eq!(h.view().unwrap().title, "T");
        assert!(matches!(h.select(), Effect::Continue { player: false }));
        assert_eq!(h.view().unwrap().title, "A");
        h.stop_stream();
        assert!(!h.live());
        let v = h.view().unwrap();
        assert_eq!(v.header.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), vec!["Not playing.", "PLAYED BEFORE"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn station_steps_continue_and_row_memory() {
        let root = std::env::temp_dir().join(format!("abyss-mod-step-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = root.join("store");
        store_with(&store, "radio", "ready", &["apps", "menu:get", "media:stream", "media:stream:http", "media:stream:screen", "menu:header"]);
        let st = |t: &str| json!({"label": t.to_uppercase(), "stream": {"url": format!("http://127.0.0.1:9/{t}"), "tag": t, "title": t.to_uppercase(), "show": "np"}});
        fs::write(store.join("radio/menu.json"), json!({"menu": 1, "start": "main", "screens": {
            "main": {"title": "T", "items": [st("a"), {"label": "Info", "goto": "np"}, st("b"), st("c")]},
            "np": {"title": "$stream.title", "header": ["$stream.tag"], "items": {"from": "$stream.played", "row": {"label": "$row.artist", "goto": "main"}}}}}).to_string()).unwrap();
        let (mods, errs) = load(&store, &root.join("settings"));
        let net: Arc<dyn Network> = Arc::new(radio_source::OpenNetwork(Arc::new(crate::http::TcpTransport::default())));
        let last = root.join("last_stream.json");
        let mut h = Host::new(mods.clone(), errs.clone(), net.clone()).unwrap();
        h.use_last_file(&last);
        assert!(h.continue_title().is_none() && !h.arm(), "nothing played yet");
        let tag = |h: &Host| h.stream_status().map(|s| s.tag).unwrap_or_default();
        h.open_direct(0);
        assert!(matches!(h.select(), Effect::Play { .. }));
        assert_eq!(tag(&h), "a");
        assert!(h.station_in_front(), "weareone-fm's shape: static stream rows, a list `show`");
        let depth = h.stack.len();
        // Hold ▼ / ▲: the next / previous station of that list (rows without a stream skipped).
        assert!(matches!(h.step_station(1), Effect::Play { .. }));
        assert_eq!(tag(&h), "b");
        assert_eq!(h.stack.len(), depth, "no screen opens while stepping");
        assert_eq!(h.view().unwrap().header[0].text, "b", "the open station screen follows");
        h.step_station(-1);
        h.step_station(-1);
        assert_eq!(tag(&h), "c", "wraps around");
        assert!(last.is_file(), "the last stream is saved");
        h.stop_stream();
        // After a restart: radio was the last source, so Continue / the entry start offer it.
        let mut h = Host::new(mods, errs, net).unwrap();
        h.use_last_file(&last);
        assert!(h.continue_title().is_none(), "only while radio was the last source");
        h.set_last_source(true);
        assert_eq!(h.continue_title().as_deref(), Some("C"));
        assert!(h.arm() && h.armed());
        let np = h.now_playing_view().unwrap();
        assert_eq!(np.header.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), vec!["C", "● plays it."]);
        assert!(h.stream_status().is_none(), "nothing fetched while it is only shown");
        assert!(matches!(h.continue_last(), Effect::Play { .. }));
        assert_eq!(tag(&h), "c");
        assert!(!h.armed() && h.stack.is_empty(), "no module screen opens for Continue");
        assert!(matches!(h.step_station(1), Effect::Play { .. }), "stepping works from a continued stream");
        assert_eq!(tag(&h), "a");
        h.stop_stream();
        h.set_last_source(false);
        assert!(h.continue_title().is_none());
        // The selected row is kept per screen when it closes (Back, or Home).
        h.open_direct(0);
        h.down();
        h.down();
        assert_eq!(h.view().unwrap().sel, 2);
        h.open_direct(0);
        assert_eq!(h.view().unwrap().sel, 2, "the list opens where it was left");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn station_steps_from_a_data_list_and_its_screen_counts() {
        // gabbee-radio's shape: stations come from `get` ($data rows); `show` is a list screen.
        let root = std::env::temp_dir().join(format!("abyss-mod-step2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = root.join("store");
        store_with(&store, "radio", "ready", &["apps", "menu:get", "media:stream", "media:stream:http", "media:stream:screen", "menu:header"]);
        fs::write(store.join("radio/menu.json"), json!({"menu": 1, "start": "stations", "screens": {
            "stations": {"title": "R", "get": {"url": "http://127.0.0.1:9/stations"},
                "items": {"from": "$data.stations", "row": {"label": "$row.name", "as": "station",
                    "stream": {"url": "$station.url", "tag": "$station.id", "title": "$station.name", "show": "radio"}}}},
            "radio": {"title": "$station.name", "header": ["$stream.tag"],
                "items": {"from": "$stream.played", "row": {"label": "$row.artist", "goto": "stations"}}}}}).to_string()).unwrap();
        let (mods, errs) = load(&store, &root.join("settings"));
        assert!(errs.is_empty(), "{errs:?}");
        let net: Arc<dyn Network> = Arc::new(radio_source::OpenNetwork(Arc::new(crate::http::TcpTransport::default())));
        let mut h = Host::new(mods, errs, net).unwrap();
        h.open_direct(0);
        // The stations list arrives (as if fetched).
        let st = |id: &str| json!({"id": id, "name": id.to_uppercase(), "url": format!("http://127.0.0.1:9/{id}")});
        if let Some(f) = h.stack.last_mut() {
            f.pages = vec![json!({"stations": [st("a"), st("b"), st("c")]})];
            f.load = Load::Ready;
        }
        assert!(!h.station_in_front());
        h.down();
        assert!(matches!(h.select(), Effect::Play { player: false, .. }));
        let tag = |h: &Host| h.stream_status().map(|s| s.tag).unwrap_or_default();
        assert_eq!(tag(&h), "b");
        assert!(h.station_in_front(), "its station screen is in front");
        assert!(matches!(h.step_station(1), Effect::Play { .. }));
        assert_eq!(tag(&h), "c");
        assert!(h.station_in_front(), "still the station screen, now of the new station");
        assert_eq!(h.view().unwrap().title, "C", "the open station screen follows");
        h.step_station(1);
        assert_eq!(tag(&h), "a", "wraps");
        h.back();
        assert!(!h.station_in_front(), "the stations list is not a station screen");
        h.stop_stream();
        let _ = fs::remove_dir_all(&root);
    }

    fn store_with(dir: &Path, id: &str, status: &str, features: &[&str]) {
        let d = dir.join(id);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("install.json"), json!({"v": 1, "id": id, "status": status, "approved": {"net": ["HTTP://127.0.0.1:9"]}}).to_string()).unwrap();
        fs::write(d.join("manifest.json"), json!({"id": id, "name": format!("N {id}"), "version": "0.1.0", "api": 1,
            "category": "music", "runtime": "pure", "about": "a", "menu": "menu.nix", "requires": {"abyss": 1, "features": features},
            "settings": {"server": {"type": "text", "default": "http://127.0.0.1:9", "env": "SERVER"}}}).to_string()).unwrap();
        fs::write(d.join("menu.json"), json!({"menu": 1, "start": "main", "screens": {
            "main": {"title": "T", "items": [{"label": "Hello $settings.server", "goto": "info"}, {"label": "Hidden $settings.nope", "goto": "info"}]},
            "info": {"kind": "text", "title": "INFO", "lines": ["one", {"text": "two", "when": "$settings.server"}, {"text": "three", "unless": "$settings.server"}]},
            "odd": {"kind": "number"}}}).to_string()).unwrap();
    }

    #[test]
    fn store_loading_and_navigation() {
        let root = std::env::temp_dir().join(format!("abyss-mod-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = root.join("store");
        store_with(&store, "good", "ready", &["apps", "menu:get"]);
        store_with(&store, "pending", "pending", &[]);
        store_with(&store, "future", "ready", &["menu:time-travel"]);
        store_with(&store, ".staging-good-1", "ready", &[]);
        fs::create_dir_all(root.join("settings")).unwrap();
        let (mods, errs) = load(&store, &root.join("settings"));
        assert_eq!(mods.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["good"]);
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("menu:time-travel")));
        assert_eq!(mods[0].net, vec!["http://127.0.0.1:9"]);
        let net: Arc<dyn Network> = Arc::new(radio_source::OpenNetwork(Arc::new(crate::http::TcpTransport::default())));
        let mut h = Host::new(mods, errs, net).unwrap();
        h.open_apps();
        let v = h.view().unwrap();
        assert_eq!(v.title, "MUSIC", "one category: opened directly");
        assert!(matches!(h.select(), Effect::None));
        let v = h.view().unwrap();
        assert_eq!(v.title, "T");
        match &v.body {
            Body::Rows(r) => {
                assert_eq!(r.iter().map(|x| x.label.as_str()).collect::<Vec<_>>(), vec!["Hello http://127.0.0.1:9 ›", "Back"]);
            }
            b => panic!("{b:?}"),
        }
        h.select();
        match h.view().unwrap().body {
            Body::Text(l, r) => {
                assert_eq!(l, vec!["one", "two"]);
                assert_eq!(r.len(), 1);
            }
            b => panic!("{b:?}"),
        }
        assert!(matches!(h.select(), Effect::None), "Back to the list");
        assert_eq!(h.view().unwrap().title, "T");
        h.down();
        assert!(matches!(h.select(), Effect::None), "Back to the category");
        assert_eq!(h.view().unwrap().title, "MUSIC");
        h.down();
        assert!(matches!(h.select(), Effect::Exit), "Back leaves the module screens");
        assert!(h.view().is_none());
        // The file the ROM ships for the installer lists exactly what this host implements.
        let shipped: Vec<&str> = include_str!("../../etc/features")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert_eq!(shipped, FEATURES, "abyss/etc/features is out of step with modules.rs FEATURES");
        assert!(features_file().contains("media:stream:radio-v1\n"));
        let _ = fs::remove_dir_all(&root);
    }
}
