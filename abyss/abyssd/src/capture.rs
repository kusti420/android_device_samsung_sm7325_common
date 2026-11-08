//! Capture: the screenshot chord (▼ + ● short, keys.rs `Action::Capture`).
//!
//! What a capture means depends on what you are looking at (or listening to).
//! Each kind of thing that can be captured is a `CaptureSource`; `sources()`
//! lists the ones that apply to the current scene, most specific first, and
//! the first that returns `Some` wins. Every capture is appended as one JSON
//! line to `/data/misc/abyss/out/captures.jsonl`, where the main-OS Abyss app
//! (system_app, group system) reads it later.
//!
//! Adding a source (a future Abyss app or script):
//!   1. implement `CaptureSource` (pick a new `kind`, fill `line` with the one
//!      line a person wants to see, put everything else in `fields`);
//!   2. return it from `sources()` for the screen(s) it belongs to.
//!
//! The file format stays the same for every kind, so the reader needs no
//! change to list a new kind (it shows `line`, groups by `kind`).
//!
//! Line format (one JSON object per line, UTF-8, keys sorted):
//!
//! ```text
//! {"fields":{...},"kind":"music","line":"Artist - Title (Mix) (LABEL CAT01)",
//!  "time":"2026-11-07T22:59:01Z","time_ms":1793919541000,"v":1}
//! ```

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{info, warn};
use serde_json::{json, Value};

use crate::tags::{self, RawTags};
use crate::ui::Screen;

pub const CAPTURES: &str = "/data/misc/abyss/out/captures.jsonl";
/// rw for abyssd (root), read for group system (the dir is setgid system),
/// nothing for others. umask 007 leaves 0640 as it is.
const MODE: u32 = 0o640;
/// timed_output vibrator (Samsung vibrator HAL, hardware/samsung/aidl/vibrator).
const VIBRATOR: &str = "/sys/class/timed_output/vibrator/enable";
/// ro.vendor.vibrator_hal.tick_duration on this device.
const BUZZ_MS: u32 = 40;

/// One saved capture.
#[derive(Clone, Debug, PartialEq)]
pub struct Capture {
    /// What was captured: "music", later others ("note", "radio", ...).
    pub kind: &'static str,
    /// The one line a person reads ("Artist - Title (Mix) (LABEL CAT01)").
    pub line: String,
    /// Everything else, source-specific. Values may nest (music: "tags").
    pub fields: BTreeMap<String, Value>,
    /// Wall clock, ms since the Unix epoch (UTC).
    pub timestamp_ms: u64,
}

impl Capture {
    pub fn new(kind: &'static str, line: String, fields: BTreeMap<String, Value>) -> Capture {
        let timestamp_ms =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        Capture { kind, line, fields, timestamp_ms }
    }

    /// The JSON line (no trailing newline; serde_json escapes any newline).
    pub fn to_json_line(&self) -> String {
        let v = json!({
            "v": 1,
            "kind": self.kind,
            "line": self.line,
            "time": iso_utc(self.timestamp_ms),
            "time_ms": self.timestamp_ms,
            "fields": self.fields,
        });
        v.to_string()
    }
}

/// Something that can be captured right now.
pub trait CaptureSource {
    /// None = nothing to capture (e.g. the song is gone).
    fn capture(&self) -> Option<Capture>;
}

/// What is loaded in the player.
#[derive(Clone, Copy, Debug)]
pub struct MusicNow<'a> {
    pub track: &'a Path,
    pub pos_ms: u64,
    pub dur_ms: Option<u64>,
    pub playing: bool,
    pub codec: &'static str,
}

/// What the person is looking at / listening to when the chord fires.
pub struct Scene<'a> {
    pub screen: Screen,
    pub screen_on: bool,
    /// Some while a track is loaded (playing or paused).
    pub music: Option<MusicNow<'a>>,
}

/// The sources that apply to `scene`, most specific first.
pub fn sources<'a>(scene: &Scene<'a>) -> Vec<Box<dyn CaptureSource + 'a>> {
    let mut v: Vec<Box<dyn CaptureSource + 'a>> = Vec::new();
    // Screen-specific sources go here first (none yet besides Now playing).
    // Music: Now playing, the screen off with a song loaded, and as the
    // fallback on every other screen while a song is loaded (that song is
    // what you hear).
    if let Some(m) = scene.music {
        v.push(Box::new(MusicSource(m)));
    }
    v
}

/// The first source that has something, plus where it was taken
/// ("abyss_screen", "screen_on" in `fields`).
pub fn capture_now(scene: &Scene) -> Option<Capture> {
    let mut c = sources(scene).iter().find_map(|s| s.capture())?;
    c.fields.insert("abyss_screen".into(), Value::from(format!("{:?}", scene.screen)));
    c.fields.insert("screen_on".into(), Value::from(scene.screen_on));
    Some(c)
}

/// Appends one capture as a JSON line, then fsyncs.
pub fn save(c: &Capture) -> std::io::Result<()> {
    save_to(Path::new(CAPTURES), c)
}

fn save_to(path: &Path, c: &Capture) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let existed = path.exists();
    let mut f = OpenOptions::new().create(true).append(true).mode(MODE).open(path)?;
    // A file left from an earlier build (or made by hand) gets our mode back.
    if f.metadata().is_ok_and(|m| m.permissions().mode() & 0o777 != MODE) {
        let _ = f.set_permissions(fs::Permissions::from_mode(MODE));
    }
    let mut line = c.to_json_line();
    line.push('\n');
    // One write() of the whole line: O_APPEND keeps it in one piece.
    f.write_all(line.as_bytes())?;
    f.sync_all()?;
    if !existed {
        // Make the new directory entry durable too.
        if let Some(dir) = path.parent() {
            if let Ok(d) = fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
    }
    info!("capture {}: {}", c.kind, c.line);
    Ok(())
}

/// One short haptic tick (screen-off feedback). Best effort: false when the
/// vibrator node is missing or not writable (logged once).
pub fn buzz() -> bool {
    static WARNED: AtomicBool = AtomicBool::new(false);
    match fs::write(VIBRATOR, BUZZ_MS.to_string()) {
        Ok(()) => true,
        Err(e) => {
            if !WARNED.swap(true, Ordering::Relaxed) {
                // TODO: devices with sec_vibrator_inputff (no timed_output) need an
                // EV_FF effect on that evdev node instead.
                warn!("capture: no haptic ({VIBRATOR}: {e})");
            }
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Music

/// The song that is loaded: "Artist - Title (Mix) (LABEL CATNO)" from its tags.
pub struct MusicSource<'a>(pub MusicNow<'a>);

impl CaptureSource for MusicSource<'_> {
    fn capture(&self) -> Option<Capture> {
        let m = &self.0;
        let raw = tags::read_raw(m.track);
        let song = SongInfo::from_tags(&raw, m.track);
        let line = song.line();
        if line.is_empty() {
            return None;
        }
        let mut f = BTreeMap::new();
        let mut put = |k: &str, v: &str| {
            if !v.is_empty() {
                f.insert(k.to_string(), Value::from(v));
            }
        };
        put("artist", &song.artist);
        put("title", &song.title);
        put("mix", &song.mix);
        put("label", &song.label);
        put("catalog_number", &song.catno);
        put("album", raw.first(&["ALBUM", "TALB"]).unwrap_or(""));
        put("codec", m.codec);
        f.insert("path".into(), Value::from(m.track.to_string_lossy().into_owned()));
        f.insert("position_ms".into(), Value::from(m.pos_ms));
        if let Some(d) = m.dur_ms {
            f.insert("duration_ms".into(), Value::from(d));
        }
        f.insert("playing".into(), Value::from(m.playing));
        f.insert("tags".into(), raw_json(&raw));
        Some(Capture::new("music", line, f))
    }
}

/// Raw tags as a JSON object: KEY -> "value", or ["v1", "v2"] when it repeats.
fn raw_json(raw: &RawTags) -> Value {
    let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in &raw.0 {
        m.entry(k.clone()).or_default().push(v.clone());
    }
    let obj: serde_json::Map<String, Value> = m
        .into_iter()
        .map(|(k, mut v)| {
            let val = if v.len() == 1 { Value::from(v.remove(0)) } else { Value::from(v) };
            (k, val)
        })
        .collect();
    Value::Object(obj)
}

const ARTIST: &[&str] = &["ARTIST", "TPE1"];
const ALBUM_ARTIST: &[&str] = &["ALBUMARTIST", "ALBUM ARTIST", "TPE2"];
const TITLE: &[&str] = &["TITLE", "TIT2"];
/// A mix / version name as written ("Extended Mix").
const MIX: &[&str] = &[
    "VERSION",
    "MIX",
    "SUBTITLE",
    "TIT3",
    "TXXX:MIX",
    "TXXX:VERSION",
    "TXXX:MIXNAME",
    "TXXX:SUBTITLE",
];
/// A remixer's name ("Headhunterz" -> "Headhunterz Remix").
const MIX_ARTIST: &[&str] = &["MIXARTIST", "REMIXER", "TPE4", "TXXX:MIXARTIST", "TXXX:REMIXER"];
const LABEL: &[&str] =
    &["LABEL", "ORGANIZATION", "PUBLISHER", "TPUB", "TXXX:LABEL", "TXXX:PUBLISHER"];
const CATNO: &[&str] = &[
    "CATALOGNUMBER",
    "LABELNO",
    "CATALOG NUMBER",
    "CATALOG",
    "CATNO",
    "TXXX:CATALOGNUMBER",
    "TXXX:CATALOG NUMBER",
    "TXXX:CATALOG",
    "TXXX:CATNO",
    "TXXX:LABELNO",
];

/// The parts of the capture line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SongInfo {
    pub artist: String,
    /// As tagged (may already end in "(Mix)").
    pub title: String,
    /// The mix / version: from the title's last parentheses, else a tag.
    pub mix: String,
    /// True when `mix` came from a tag and is not in `title` yet.
    mix_from_tag: bool,
    pub label: String,
    pub catno: String,
}

impl SongInfo {
    pub fn from_tags(raw: &RawTags, path: &Path) -> SongInfo {
        let mut artist = raw.all(ARTIST).join(", ");
        if artist.is_empty() {
            artist = raw.first(ALBUM_ARTIST).unwrap_or("").to_string();
        }
        let mut title = raw.first(TITLE).unwrap_or("").to_string();
        if title.is_empty() {
            title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        }
        let (mut mix, mut mix_from_tag) = (title_mix(&title).unwrap_or("").to_string(), false);
        if mix.is_empty() {
            let tagged = raw
                .first(MIX)
                .map(|m| strip_parens(m).to_string())
                .or_else(|| raw.first(MIX_ARTIST).map(remixer_mix));
            if let Some(m) = tagged.filter(|m| !m.is_empty()) {
                // "Song - Extended Mix" already says it.
                mix_from_tag = !title.to_lowercase().contains(&m.to_lowercase());
                mix = m;
            }
        }
        SongInfo {
            artist,
            title,
            mix,
            mix_from_tag,
            label: raw.first(LABEL).unwrap_or("").to_string(),
            catno: raw.first(CATNO).unwrap_or("").to_string(),
        }
    }

    /// "Artist - Title (Mix) (LABEL CATNO)"; missing parts left out cleanly.
    pub fn line(&self) -> String {
        let mut s = match (self.artist.is_empty(), self.title.is_empty()) {
            (false, false) => format!("{} - {}", self.artist, self.title),
            (false, true) => self.artist.clone(),
            _ => self.title.clone(),
        };
        if self.mix_from_tag && !self.mix.is_empty() {
            s.push_str(&format!(" ({})", self.mix));
        }
        let label = self.label.trim();
        let catno = self.catno.trim();
        let tail = match (label.is_empty(), catno.is_empty()) {
            (false, false) if label.to_lowercase().ends_with(&catno.to_lowercase()) => {
                label.to_string()
            }
            (false, false) => format!("{label} {catno}"),
            (false, true) => label.to_string(),
            (true, false) => catno.to_string(),
            (true, true) => String::new(),
        };
        if !tail.is_empty() {
            s.push_str(&format!(" ({tail})"));
        }
        s.trim().to_string()
    }
}

/// The mix / version a title ends in: "Song (Extended Mix)" -> "Extended Mix",
/// "[Radio Edit]", "(Headhunterz Remix)". A credit or a source in the last
/// parentheses ("(feat. X)", "(From \"Film\")", "(with X)", "(prod. X)",
/// "(vs X)", "(x X)") is not a mix: None. Same rule as the radio's public API
/// (`split_mix` in the radio's api/app/public_v1.py), so a song captured from
/// a local file and from the radio gets the same `mix`.
pub fn title_mix(title: &str) -> Option<&str> {
    trailing_paren(title).filter(|inner| !is_credit(inner))
}

/// The first word is feat / ft / featuring / with / from / prod / vs / x
/// (any case, followed by a non-word character or the end), like the radio's
/// `^(?:feat\b|ft\b|featuring\b|with\b|from\b|prod\b|vs\b|x\b)`.
fn is_credit(inner: &str) -> bool {
    let word: String =
        inner.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<String>().to_lowercase();
    matches!(word.as_str(), "feat" | "ft" | "featuring" | "with" | "from" | "prod" | "vs" | "x")
}

/// "Song (Extended Mix)" -> "Extended Mix"; also [ ]. None without one.
fn trailing_paren(s: &str) -> Option<&str> {
    let s = s.trim_end();
    let close = s.chars().last()?;
    let open = match close {
        ')' => '(',
        ']' => '[',
        _ => return None,
    };
    let start = s.rfind(open)?;
    let inner = s[start + 1..s.len() - 1].trim();
    (!inner.is_empty() && start > 0).then_some(inner)
}

/// "(Extended Mix)" -> "Extended Mix".
fn strip_parens(s: &str) -> &str {
    let t = s.trim();
    let b = t.as_bytes();
    if b.len() >= 2 && matches!((b[0], b[b.len() - 1]), (b'(', b')') | (b'[', b']')) {
        t[1..t.len() - 1].trim()
    } else {
        t
    }
}

/// A remixer tag becomes "<name> Remix" unless it already names the mix.
fn remixer_mix(name: &str) -> String {
    let n = strip_parens(name);
    let l = n.to_lowercase();
    if l.is_empty() || l.contains("mix") || l.contains("edit") || l.contains("version") {
        n.to_string()
    } else {
        format!("{n} Remix")
    }
}

/// ms since the epoch -> "2026-11-07T22:59:01Z".
fn iso_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(p: &[(&str, &str)]) -> RawTags {
        RawTags(p.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
    }

    fn line(p: &[(&str, &str)]) -> String {
        SongInfo::from_tags(&raw(p), Path::new("/m/file name.flac")).line()
    }

    #[test]
    fn full_line_from_vorbis() {
        let l = line(&[
            ("ARTIST", "Headhunterz"),
            ("TITLE", "Orange Heart"),
            ("VERSION", "Extended Mix"),
            ("LABEL", "Scantraxx"),
            ("CATALOGNUMBER", "SCAN123"),
        ]);
        assert_eq!(l, "Headhunterz - Orange Heart (Extended Mix) (Scantraxx SCAN123)");
    }

    #[test]
    fn mix_already_in_title_is_not_repeated() {
        let l = line(&[("ARTIST", "A"), ("TITLE", "Song (Radio Edit)"), ("VERSION", "Radio Edit")]);
        assert_eq!(l, "A - Song (Radio Edit)");
        let s = SongInfo::from_tags(&raw(&[("TITLE", "Song (Radio Edit)")]), Path::new("x.flac"));
        assert_eq!(s.mix, "Radio Edit");
        assert_eq!(
            line(&[("ARTIST", "A"), ("TITLE", "Song - Extended Mix"), ("VERSION", "Extended Mix")]),
            "A - Song - Extended Mix"
        );
    }

    #[test]
    fn credits_and_sources_are_not_mixes() {
        for t in [
            "Song (feat. Kiiara)",
            "Song (Feat. Kiiara)",
            "Song (ft. X)",
            "Song (featuring X)",
            "Song (From \"Film\")",
            "Song [from The Movie]",
            "Song (with Y)",
            "Song (prod. Z)",
            "Song (vs X)",
            "Song (vs. X)",
            "Song (x Someone)",
            "Song (X Someone)",
        ] {
            assert_eq!(title_mix(t), None, "{t}");
            let s = SongInfo::from_tags(&raw(&[("TITLE", t)]), Path::new("x.flac"));
            assert_eq!(s.mix, "", "{t}");
        }
        // Words that only start like a credit are mixes.
        assert_eq!(title_mix("Song (Xtra Mix)"), Some("Xtra Mix"));
        assert_eq!(title_mix("Song (Withdrawal Remix)"), Some("Withdrawal Remix"));
        assert_eq!(title_mix("Song (Fromage Edit)"), Some("Fromage Edit"));
        assert_eq!(title_mix("Song (Extended Mix)"), Some("Extended Mix"));
        // A credit in the title leaves room for the tagged mix, and the title keeps the credit.
        assert_eq!(
            line(&[("ARTIST", "A"), ("TITLE", "Song (feat. B)"), ("VERSION", "Extended Mix")]),
            "A - Song (feat. B) (Extended Mix)"
        );
        assert_eq!(line(&[("ARTIST", "A"), ("TITLE", "Song (feat. B)")]), "A - Song (feat. B)");
    }

    #[test]
    fn missing_parts_leave_no_empty_parentheses() {
        assert_eq!(line(&[("ARTIST", "A"), ("TITLE", "T")]), "A - T");
        assert_eq!(line(&[("TITLE", "T"), ("ORGANIZATION", "Lbl")]), "T (Lbl)");
        assert_eq!(line(&[("ARTIST", "A"), ("TITLE", "T"), ("LABELNO", "X-01")]), "A - T (X-01)");
        assert_eq!(line(&[("VERSION", "  ")]), "file name");
        assert_eq!(line(&[]), "file name");
    }

    #[test]
    fn id3_frames_and_remixer() {
        let l = line(&[
            ("TPE1", "B"),
            ("TIT2", "Track"),
            ("TPE4", "Someone"),
            ("TPUB", "Dirty Workz"),
            ("TXXX:CATALOGNUMBER", "DWX 001"),
        ]);
        assert_eq!(l, "B - Track (Someone Remix) (Dirty Workz DWX 001)");
        assert_eq!(
            line(&[("TPE1", "B"), ("TIT2", "T"), ("TXXX:MIX", "(Club Mix)")]),
            "B - T (Club Mix)"
        );
        assert_eq!(
            line(&[("TPE1", "B"), ("TIT2", "T"), ("TIT3", "Original Mix")]),
            "B - T (Original Mix)"
        );
    }

    #[test]
    fn several_artists_and_label_ending_in_catno() {
        assert_eq!(
            line(&[
                ("ARTIST", "A"),
                ("ARTIST", "B"),
                ("TITLE", "T"),
                ("LABEL", "Lbl CAT1"),
                ("CATALOGNUMBER", "CAT1")
            ]),
            "A, B - T (Lbl CAT1)"
        );
    }

    #[test]
    fn json_line_shape() {
        let mut f = BTreeMap::new();
        f.insert("position_ms".to_string(), Value::from(1234u64));
        let c = Capture {
            kind: "music",
            line: "A - T\nx".into(),
            fields: f,
            timestamp_ms: 1_762_556_341_000,
        };
        let s = c.to_json_line();
        assert!(!s.contains('\n'));
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"], "music");
        assert_eq!(v["v"], 1);
        assert_eq!(v["fields"]["position_ms"], 1234);
        assert_eq!(v["time"], "2025-11-07T22:59:01Z");
    }

    #[test]
    fn iso_dates() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(951_782_400_000), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn append_is_one_line_each_with_mode_0640() {
        let dir = std::env::temp_dir().join(format!("abyss-cap-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = dir.join("captures.jsonl");
        let _ = fs::remove_file(&p);
        let c = Capture::new("music", "A - T".into(), BTreeMap::new());
        save_to(&p, &c).unwrap();
        save_to(&p, &c).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o640);
        let _ = fs::remove_dir_all(&dir);
    }
}
