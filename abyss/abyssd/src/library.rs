//! Music library scan and resume state.
//!
//! Roots (in this order):
//! - /data/media/0/Music: internal shared storage, lower (non-FUSE) path. The
//!   FUSE view /sdcard dies with MediaProvider when the framework stops.
//! - /mnt/media_rw/<uuid>: vold's lower mount of a microSD card
//!   (system/vold/model/PublicVolume.cpp: mRawPath = "/mnt/media_rw/" +
//!   stableName). vold keeps it mounted after the framework stops, but note:
//!   if MediaProvider's FUSE session dies while system_server is still alive,
//!   StorageManagerService calls vold reset(), which unmounts and recreates the
//!   volume. See README "Risks".
//!
//! v1-lite scans on the device at start (the v1.0 design builds the index in
//! the main OS and hands it over). Sorting is a simple accent-folding compare;
//! the language-aware order (Estonian Õ Ä Ö Ü after W) comes with the index.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::{info, warn};
use serde_json::{json, Value};

pub const STATE_DIR: &str = "/data/misc/abyss/out";
pub const STATE_FILE: &str = "/data/misc/abyss/out/state.json";

const MAX_DEPTH: usize = 12;

#[derive(Clone, Debug)]
pub struct Track {
    pub path: PathBuf,
    /// Tag title, else the file name without extension.
    pub title: String,
    pub artist: String,
    pub album: String,
    pub track_no: u32,
}

/// One album (from tags): name, artist, tracks in disc order.
#[derive(Clone, Debug)]
pub struct Album {
    pub name: String,
    pub artist: String,
    pub tracks: Vec<PathBuf>,
}

/// One artist (album artist, else track artist) and their albums.
#[derive(Clone, Debug)]
pub struct Artist {
    pub name: String,
    /// Album ids, A–Z.
    pub albums: Vec<usize>,
    pub n_tracks: usize,
}

#[derive(Clone, Debug)]
pub struct Folder {
    pub name: String,
    #[allow(dead_code)] // kept for the v1.0 index hand-over / letter row
    pub path: PathBuf,
    #[allow(dead_code)]
    pub parent: Option<usize>,
    /// Child folder ids, sorted by name.
    pub subs: Vec<usize>,
    /// Tracks directly in this folder, sorted by name.
    pub tracks: Vec<Track>,
}

#[derive(Default)]
pub struct Library {
    pub folders: Vec<Folder>,
    /// Top-level folder ids (one per root that has music).
    pub roots: Vec<usize>,
    /// Albums A–Z (from tags; untagged files group by folder name).
    pub albums: Vec<Album>,
    /// Artists A–Z.
    pub artists: Vec<Artist>,
    /// Track path → (folder id, index in that folder).
    index: HashMap<PathBuf, (usize, usize)>,
}

/// Letter-row key of a name: 'A'..'Z' (accents folded), or '#' for the rest.
pub fn letter_of(name: &str) -> char {
    let skip = |n: &str| {
        let l = n.to_lowercase();
        for a in ["the ", "a "] {
            if l.starts_with(a) {
                if let Some(rest) = n.get(a.len()..).filter(|r| !r.is_empty()) {
                    return rest.to_string();
                }
            }
        }
        n.to_string()
    };
    let n = skip(name);
    match n.chars().next().map(fold) {
        Some(c) if c.is_ascii_lowercase() => c.to_ascii_uppercase(),
        _ => '#',
    }
}

fn is_audio(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("flac") || e.eq_ignore_ascii_case("mp3"))
        .unwrap_or(false)
}

/// Maps a character to its base letter for primary ordering (é → e, ß → s ...).
fn fold(c: char) -> char {
    let c = c.to_lowercase().next().unwrap_or(c);
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
        'ď' | 'đ' => 'd',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
        'ĥ' | 'ħ' => 'h',
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => 'i',
        'ĵ' => 'j',
        'ķ' => 'k',
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => 'l',
        'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => 'o',
        'ŕ' | 'ŗ' | 'ř' => 'r',
        'ß' | 'ś' | 'ŝ' | 'ş' | 'š' => 's',
        'ţ' | 'ť' | 'ŧ' => 't',
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ŵ' => 'w',
        'ý' | 'ÿ' | 'ŷ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        _ => c,
    }
}

/// Name order: accent-folded and case-insensitive first, digits as numbers
/// ("2 x" before "10 x"), then the exact string as a tie-break.
pub fn name_cmp(a: &str, b: &str) -> Ordering {
    let mut ia = a.chars().peekable();
    let mut ib = b.chars().peekable();
    loop {
        match (ia.peek().copied(), ib.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let na = take_num(&mut ia);
                let nb = take_num(&mut ib);
                match na.cmp(&nb) {
                    Ordering::Equal => {}
                    o => return o,
                }
            }
            (Some(ca), Some(cb)) => {
                ia.next();
                ib.next();
                match fold(ca).cmp(&fold(cb)) {
                    Ordering::Equal => {}
                    o => return o,
                }
            }
        }
    }
}

fn take_num(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> u64 {
    let mut n: u64 = 0;
    while let Some(c) = it.peek().copied() {
        match c.to_digit(10) {
            Some(d) => {
                n = n.saturating_mul(10).saturating_add(d as u64);
                it.next();
            }
            None => break,
        }
    }
    n
}

impl Library {
    /// Scans all roots. Never fails: missing roots are skipped.
    pub fn scan() -> Library {
        let mut lib = Library::default();
        let mut roots: Vec<(String, PathBuf)> = Vec::new();
        roots.push(("Phone".into(), PathBuf::from("/data/media/0/Music")));
        if let Ok(rd) = fs::read_dir("/mnt/media_rw") {
            let mut sd: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            sd.sort();
            for (n, p) in sd.into_iter().enumerate() {
                let label =
                    if n == 0 { "SD card".to_string() } else { format!("SD card {}", n + 1) };
                roots.push((label, p));
            }
        }
        for (label, path) in roots {
            if !path.is_dir() {
                continue;
            }
            if let Some(id) = lib.scan_dir(&path, label, None, 0) {
                lib.roots.push(id);
            }
        }
        lib.read_tags();
        info!(
            "library: {} folders, {} tracks, {} albums, {} artists",
            lib.folders.len(),
            lib.folders.iter().map(|f| f.tracks.len()).sum::<usize>(),
            lib.albums.len(),
            lib.artists.len()
        );
        lib
    }

    /// Fills tags (cached), then builds the album/artist lists and the path index.
    fn read_tags(&mut self) {
        let mut cache = crate::tags::TagCache::load();
        let mut albums: HashMap<(String, String), Vec<(u32, String, PathBuf)>> = HashMap::new();
        for fi in 0..self.folders.len() {
            let folder_name = self.folders[fi].name.clone();
            for ti in 0..self.folders[fi].tracks.len() {
                let t = &mut self.folders[fi].tracks[ti];
                let tags = cache.get(&t.path);
                if !tags.title.is_empty() {
                    t.title = tags.title.clone();
                }
                t.artist = tags.artist.clone();
                t.album = tags.album.clone();
                t.track_no = tags.track_no;
                let album = if tags.album.is_empty() { folder_name.clone() } else { tags.album };
                let artist = if !tags.album_artist.is_empty() {
                    tags.album_artist
                } else if !tags.artist.is_empty() {
                    tags.artist
                } else {
                    "Unknown artist".into()
                };
                albums.entry((album, artist)).or_default().push((
                    t.track_no,
                    t.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    t.path.clone(),
                ));
                self.index.insert(t.path.clone(), (fi, ti));
            }
        }
        cache.save();
        let mut list: Vec<Album> = albums
            .into_iter()
            .map(|((name, artist), mut v)| {
                v.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| name_cmp(&a.1, &b.1)));
                Album { name, artist, tracks: v.into_iter().map(|x| x.2).collect() }
            })
            .collect();
        list.sort_by(|a, b| name_cmp(&a.name, &b.name).then_with(|| name_cmp(&a.artist, &b.artist)));
        let mut artists: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, a) in list.iter().enumerate() {
            artists.entry(a.artist.clone()).or_default().push(i);
        }
        let mut artists: Vec<Artist> = artists
            .into_iter()
            .map(|(name, albums)| {
                let n_tracks = albums.iter().map(|&i| list[i].tracks.len()).sum();
                Artist { name, albums, n_tracks }
            })
            .collect();
        artists.sort_by(|a, b| name_cmp(&a.name, &b.name));
        self.albums = list;
        self.artists = artists;
    }

    pub fn track_count(&self) -> usize {
        self.index.len()
    }

    /// The tagged track behind a path, if it is in the library.
    pub fn track(&self, path: &Path) -> Option<&Track> {
        let &(f, t) = self.index.get(path)?;
        self.folders.get(f)?.tracks.get(t)
    }

    /// All tracks of an artist, album by album.
    pub fn artist_tracks(&self, artist: usize) -> Vec<PathBuf> {
        let Some(a) = self.artists.get(artist) else { return Vec::new() };
        a.albums.iter().flat_map(|&i| self.albums[i].tracks.iter().cloned()).collect()
    }

    /// Adds `dir` (and its subtree) if it contains music; returns its id.
    fn scan_dir(
        &mut self,
        dir: &Path,
        name: String,
        parent: Option<usize>,
        depth: usize,
    ) -> Option<usize> {
        if depth > MAX_DEPTH {
            return None;
        }
        let rd = match fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) => {
                warn!("scan {}: {e}", dir.display());
                return None;
            }
        };
        let id = self.folders.len();
        self.folders.push(Folder {
            name,
            path: dir.to_path_buf(),
            parent,
            subs: Vec::new(),
            tracks: Vec::new(),
        });
        let mut subdirs: Vec<(String, PathBuf)> = Vec::new();
        let mut tracks: Vec<Track> = Vec::new();
        for e in rd.filter_map(|e| e.ok()) {
            let fname = e.file_name().to_string_lossy().into_owned();
            if fname.starts_with('.') {
                continue;
            }
            // file_type() does not follow symlinks: no loops through links.
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                subdirs.push((fname, p));
            } else if ft.is_file() && is_audio(&p) {
                let title =
                    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(fname);
                tracks.push(Track {
                    path: p,
                    title,
                    artist: String::new(),
                    album: String::new(),
                    track_no: 0,
                });
            }
        }
        subdirs.sort_by(|a, b| name_cmp(&a.0, &b.0));
        tracks.sort_by(|a, b| name_cmp(&a.title, &b.title));
        let mut subs = Vec::new();
        for (n, p) in subdirs {
            if let Some(child) = self.scan_dir(&p, n, Some(id), depth + 1) {
                subs.push(child);
            }
        }
        if subs.is_empty() && tracks.is_empty() {
            // Nothing playable below: drop this folder again. Children were
            // dropped already, so `id` is the last entry.
            self.folders.truncate(id);
            return None;
        }
        let f = &mut self.folders[id];
        f.subs = subs;
        f.tracks = tracks;
        Some(id)
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// All tracks below `folder` (depth-first, in display order).
    pub fn tracks_below(&self, folder: usize) -> Vec<PathBuf> {
        let mut out = Vec::new();
        self.collect(folder, &mut out);
        out
    }

    fn collect(&self, folder: usize, out: &mut Vec<PathBuf>) {
        let f = &self.folders[folder];
        out.extend(f.tracks.iter().map(|t| t.path.clone()));
        for &s in &f.subs {
            self.collect(s, out);
        }
    }

    pub fn all_tracks(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for &r in &self.roots {
            self.collect(r, &mut out);
        }
        out
    }

    /// Every folder that directly holds tracks, sorted A–Z (the "Folders A–Z" list).
    pub fn album_folders(&self) -> Vec<usize> {
        let mut v: Vec<usize> =
            (0..self.folders.len()).filter(|&i| !self.folders[i].tracks.is_empty()).collect();
        v.sort_by(|&a, &b| name_cmp(&self.folders[a].name, &self.folders[b].name));
        v
    }

    pub fn find_folder(&self, name: &str) -> Option<usize> {
        self.folders.iter().position(|f| f.name.eq_ignore_ascii_case(name))
    }

    /// The folder that directly contains `track`, and the track's index there.
    pub fn locate(&self, track: &Path) -> Option<(usize, usize)> {
        self.index.get(track).copied()
    }

    pub fn title_of(&self, track: &Path) -> String {
        match self.track(track) {
            Some(t) => t.title.clone(),
            None => track.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        }
    }

    /// "Artist · Album" from tags, falling back to the folder name.
    pub fn artist_album_of(&self, track: &Path) -> String {
        match self.track(track) {
            Some(t) if !t.artist.is_empty() || !t.album.is_empty() => {
                match (t.artist.is_empty(), t.album.is_empty()) {
                    (false, false) => format!("{} · {}", t.artist, t.album),
                    (false, true) => t.artist.clone(),
                    _ => t.album.clone(),
                }
            }
            _ => self.folder_name_of(track),
        }
    }

    pub fn folder_name_of(&self, track: &Path) -> String {
        track
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// Resume state, written by abyssd only (one writer per file).
#[derive(Clone, Debug, Default)]
pub struct ResumeState {
    pub track: Option<PathBuf>,
    pub position_ms: u64,
    pub volume: u32,
    pub shuffle: bool,
}

impl ResumeState {
    pub fn load() -> ResumeState {
        let Ok(text) = fs::read_to_string(STATE_FILE) else {
            return ResumeState { volume: 40, ..Default::default() };
        };
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        ResumeState {
            track: v["track"].as_str().map(PathBuf::from),
            position_ms: v["position_ms"].as_u64().unwrap_or(0),
            volume: v["volume"].as_u64().map(|x| x.min(160) as u32).unwrap_or(40),
            shuffle: v["shuffle"].as_bool().unwrap_or(false),
        }
    }

    /// Atomic replace: write a temp file, fsync, rename over the old one.
    pub fn save(&self) -> Result<()> {
        fs::create_dir_all(STATE_DIR).context("create state dir")?;
        let v = json!({
            "version": 1,
            "track": self.track.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "position_ms": self.position_ms,
            "volume": self.volume,
            "shuffle": self.shuffle,
        });
        let tmp = format!("{STATE_FILE}.tmp");
        {
            let mut f = fs::File::create(&tmp).context("create state tmp")?;
            f.write_all(serde_json::to_string_pretty(&v)?.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, STATE_FILE).context("rename state")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{letter_of, name_cmp};
    use std::cmp::Ordering;

    #[test]
    fn natural_and_folded() {
        assert_eq!(name_cmp("2 b", "10 a"), Ordering::Less);
        assert_eq!(name_cmp("Émile", "eric"), Ordering::Less);
        assert_eq!(name_cmp("öö", "oz"), Ordering::Less);
    }

    #[test]
    fn letters() {
        assert_eq!(letter_of("The Prodigy"), 'P');
        assert_eq!(letter_of("Ärzte"), 'A');
        assert_eq!(letter_of("2Pac"), '#');
    }
}
