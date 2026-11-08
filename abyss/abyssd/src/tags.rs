//! Minimal tag reader: FLAC Vorbis comments and MP3 ID3v2 (2.3 / 2.4) text
//! frames: title, artist, album artist, album, track number (`read`, for the
//! library), and every text field as-is (`read_raw`, for captures).
//!
//! Only the start of the file is read (FLAC metadata blocks are skipped with
//! seeks, so embedded cover art costs nothing). Results are cached in
//! /data/misc/abyss/out/tags.tsv keyed by path + size + mtime, so only new or
//! changed files are opened on later entries.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use log::{info, warn};

const CACHE: &str = "/data/misc/abyss/out/tags.tsv";
/// Upper bound for one ID3v2 tag or Vorbis comment block we parse.
const MAX_TAG: usize = 1 << 20;
/// `read_raw` leaves out longer values (embedded pictures, lyrics dumps).
const MAX_RAW_VALUE: usize = 4096;

/// Every text field of a file, in file order: (KEY, value). Keys are upper
/// case: Vorbis comment names (ARTIST, LABEL, CATALOGNUMBER, ...) or ID3v2
/// frame ids (TPE1, TIT2, TPUB, ...); an ID3 TXXX frame is "TXXX:<DESCRIPTION>".
/// A key can repeat (multi-value fields).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawTags(pub Vec<(String, String)>);

impl RawTags {
    /// All non-empty values of the first key in `keys` that has any.
    pub fn all(&self, keys: &[&str]) -> Vec<&str> {
        for k in keys {
            let v: Vec<&str> = self
                .0
                .iter()
                .filter(|(kk, v)| kk.eq_ignore_ascii_case(k) && !v.is_empty())
                .map(|(_, v)| v.as_str())
                .collect();
            if !v.is_empty() {
                return v;
            }
        }
        Vec::new()
    }

    /// The first non-empty value of the first key in `keys` that has one.
    pub fn first(&self, keys: &[&str]) -> Option<&str> {
        self.all(keys).into_iter().next()
    }
}

/// Every text field of the file (FLAC Vorbis comments, ID3v2.3/2.4 text and
/// TXXX frames). Empty when the file has no tags we understand.
pub fn read_raw(path: &Path) -> RawTags {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let r = match ext.as_str() {
        "flac" => flac_comments(path).map(|b| vorbis_fields(&b)),
        "mp3" => id3_frames(path).map(|(ver, b, o)| id3_fields(ver, &b, o)),
        _ => None,
    };
    let mut raw = r.unwrap_or_default();
    raw.retain(|(k, v)| {
        !v.is_empty()
            && v.len() <= MAX_RAW_VALUE
            && !matches!(k.as_str(), "METADATA_BLOCK_PICTURE" | "COVERART" | "COVERARTMIME")
    });
    RawTags(raw)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track_no: u32,
}

fn clean(s: &str) -> String {
    s.trim_matches(char::from(0))
        .chars()
        .map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

fn track_no(s: &str) -> u32 {
    s.split('/').next().and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}

pub fn read(path: &Path) -> Tags {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let r = match ext.as_str() {
        "flac" => read_flac(path),
        "mp3" => read_id3(path),
        _ => None,
    };
    r.unwrap_or_default()
}

fn read_flac(path: &Path) -> Option<Tags> {
    flac_comments(path).map(|b| parse_vorbis(&b))
}

/// The raw VORBIS_COMMENT block of a FLAC file.
fn flac_comments(path: &Path) -> Option<Vec<u8>> {
    let mut f = File::open(path).ok()?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).ok()?;
    if &magic != b"fLaC" {
        // Some FLAC files start with an ID3v2 tag; not worth chasing.
        return None;
    }
    loop {
        let mut h = [0u8; 4];
        f.read_exact(&mut h).ok()?;
        let last = h[0] & 0x80 != 0;
        let kind = h[0] & 0x7f;
        let len = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        if kind == 4 {
            if len > MAX_TAG {
                return None;
            }
            let mut b = vec![0u8; len];
            f.read_exact(&mut b).ok()?;
            return Some(b);
        }
        if last {
            return None;
        }
        f.seek(SeekFrom::Current(len as i64)).ok()?;
    }
}

/// (KEY, value) pairs of a Vorbis comment block, keys upper case.
fn vorbis_fields(b: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let u32le = |o: usize| -> Option<usize> {
        b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
    };
    let Some(vlen) = u32le(0) else { return out };
    let mut o = 4 + vlen;
    let Some(n) = u32le(o) else { return out };
    o += 4;
    for _ in 0..n.min(4096) {
        let Some(l) = u32le(o) else { break };
        o += 4;
        let Some(c) = o.checked_add(l).and_then(|e| b.get(o..e)) else { break };
        o += l;
        let s = String::from_utf8_lossy(c);
        let Some((k, v)) = s.split_once('=') else { continue };
        out.push((k.trim().to_ascii_uppercase(), clean(v)));
    }
    out
}

fn parse_vorbis(b: &[u8]) -> Tags {
    let mut t = Tags::default();
    for (k, v) in vorbis_fields(b) {
        match k.as_str() {
            "TITLE" if t.title.is_empty() => t.title = v,
            "ARTIST" if t.artist.is_empty() => t.artist = v,
            "ALBUMARTIST" | "ALBUM ARTIST" if t.album_artist.is_empty() => t.album_artist = v,
            "ALBUM" if t.album.is_empty() => t.album = v,
            "TRACKNUMBER" if t.track_no == 0 => t.track_no = track_no(&v),
            _ => {}
        }
    }
    t
}

fn syncsafe(b: &[u8]) -> usize {
    ((b[0] as usize & 0x7f) << 21)
        | ((b[1] as usize & 0x7f) << 14)
        | ((b[2] as usize & 0x7f) << 7)
        | (b[3] as usize & 0x7f)
}

fn read_id3(path: &Path) -> Option<Tags> {
    let (ver, b, mut o) = id3_frames(path)?;
    let mut t = Tags::default();
    while let Some((id, data, next)) = id3_frame(ver, &b, o) {
        o = next;
        let field = match id {
            b"TIT2" => &mut t.title,
            b"TPE1" => &mut t.artist,
            b"TPE2" => &mut t.album_artist,
            b"TALB" => &mut t.album,
            b"TRCK" => {
                t.track_no = track_no(&id3_text(data));
                continue;
            }
            _ => continue,
        };
        if field.is_empty() {
            *field = clean(&id3_text(data));
        }
    }
    Some(t)
}

/// One ID3v2 frame at `o`: (id, body, offset of the next frame).
fn id3_frame(ver: u8, b: &[u8], o: usize) -> Option<(&[u8], &[u8], usize)> {
    if o + 10 > b.len() || b[o] == 0 {
        return None;
    }
    let id = &b[o..o + 4];
    let len = if ver == 4 { syncsafe(&b[o + 4..o + 8]) } else { u32_be(&b[o + 4..o + 8]) };
    let start = o + 10;
    let data = b.get(start..start.checked_add(len)?)?;
    Some((id, data, start + len))
}

/// Every text frame (T***) of an ID3v2 tag; TXXX as "TXXX:<DESCRIPTION>".
/// Multi-value frames (v2.4, NUL-separated) give one pair per value.
fn id3_fields(ver: u8, b: &[u8], mut o: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    while let Some((id, data, next)) = id3_frame(ver, b, o) {
        o = next;
        if id[0] != b'T' || !id.iter().all(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        let id = String::from_utf8_lossy(id).into_owned();
        let vals = id3_strings(data);
        if id == "TXXX" {
            let mut it = vals.into_iter();
            let desc = it.next().map(|d| clean(&d).to_ascii_uppercase()).unwrap_or_default();
            for v in it {
                out.push((format!("TXXX:{desc}"), clean(&v)));
            }
        } else {
            for v in vals {
                out.push((id.clone(), clean(&v)));
            }
        }
    }
    out
}

/// The tag body of an MP3 (after the extended header): (version, tag, first frame offset).
fn id3_frames(path: &Path) -> Option<(u8, Vec<u8>, usize)> {
    let mut f = File::open(path).ok()?;
    let mut h = [0u8; 10];
    f.read_exact(&mut h).ok()?;
    if &h[0..3] != b"ID3" {
        return None;
    }
    let ver = h[3];
    if !(3..=4).contains(&ver) {
        return None;
    }
    let size = syncsafe(&h[6..10]).min(MAX_TAG);
    let mut b = vec![0u8; size];
    f.read_exact(&mut b).ok()?;
    let mut o = 0;
    if h[5] & 0x40 != 0 && b.len() >= 4 {
        // Extended header: skip it.
        let n = if ver == 4 { syncsafe(&b[0..4]) } else { u32_be(&b[0..4]) + 4 };
        o = n.min(b.len());
    }
    Some((ver, b, o))
}

fn u32_be(b: &[u8]) -> usize {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize
}

/// ID3 text frame body: encoding byte + text (first value only).
fn id3_text(d: &[u8]) -> String {
    id3_strings(d).into_iter().next().unwrap_or_default()
}

/// ID3 text frame body: encoding byte + NUL-separated strings (all of them;
/// a trailing terminator does not make an extra empty one).
fn id3_strings(d: &[u8]) -> Vec<String> {
    let Some((&enc, s)) = d.split_first() else { return Vec::new() };
    let mut out: Vec<String> = match enc {
        0 => s.split(|&c| c == 0).map(|p| p.iter().map(|&c| c as char).collect()).collect(),
        1 | 2 => {
            let units: Vec<u16> = s
                .chunks_exact(2)
                .map(|c| if enc == 2 { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) })
                .collect();
            units
                .split(|&u| u == 0)
                .map(|p| {
                    // Each UTF-16 string may carry its own BOM.
                    let (p, swap) = match p.first() {
                        Some(0xFEFF) => (&p[1..], false),
                        Some(0xFFFE) => (&p[1..], true),
                        _ => (p, false),
                    };
                    let p: Vec<u16> = if swap { p.iter().map(|u| u.swap_bytes()).collect() } else { p.to_vec() };
                    String::from_utf16_lossy(&p)
                })
                .collect()
        }
        _ => s.split(|&c| c == 0).map(|p| String::from_utf8_lossy(p).into_owned()).collect(),
    };
    if out.len() > 1 && out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

/// Path → tags, with the on-disk cache.
pub struct TagCache {
    map: HashMap<PathBuf, (u64, i64, Tags)>,
    used: HashMap<PathBuf, (u64, i64, Tags)>,
    pub read_files: usize,
}

impl TagCache {
    pub fn load() -> TagCache {
        let mut map = HashMap::new();
        if let Ok(text) = fs::read_to_string(CACHE) {
            for line in text.lines() {
                let p: Vec<&str> = line.split('\t').collect();
                if p.len() != 8 {
                    continue;
                }
                let (Ok(size), Ok(mtime)) = (p[1].parse(), p[2].parse()) else { continue };
                let t = Tags {
                    title: p[3].into(),
                    artist: p[4].into(),
                    album: p[5].into(),
                    album_artist: p[6].into(),
                    track_no: p[7].parse().unwrap_or(0),
                };
                map.insert(PathBuf::from(p[0]), (size, mtime, t));
            }
        }
        TagCache { map, used: HashMap::new(), read_files: 0 }
    }

    pub fn get(&mut self, path: &Path) -> Tags {
        let (size, mtime) =
            fs::metadata(path).map(|m| (m.size(), m.mtime())).unwrap_or((0, 0));
        let t = match self.map.remove(path) {
            Some((s, m, t)) if s == size && m == mtime => t,
            _ => {
                self.read_files += 1;
                read(path)
            }
        };
        self.used.insert(path.to_path_buf(), (size, mtime, t.clone()));
        t
    }

    /// Writes the entries used in this scan (tmp + rename).
    pub fn save(&self) {
        let tmp = format!("{CACHE}.tmp");
        let r = (|| -> std::io::Result<()> {
            let mut f = std::io::BufWriter::new(File::create(&tmp)?);
            for (p, (s, m, t)) in &self.used {
                let Some(ps) = p.to_str() else { continue };
                if ps.contains('\t') || ps.contains('\n') {
                    continue;
                }
                writeln!(
                    f,
                    "{ps}\t{s}\t{m}\t{}\t{}\t{}\t{}\t{}",
                    t.title, t.artist, t.album, t.album_artist, t.track_no
                )?;
            }
            f.into_inner()?.sync_all()?;
            fs::rename(&tmp, CACHE)
        })();
        match r {
            Ok(()) => info!("tags: {} files read, {} cached", self.read_files, self.used.len()),
            Err(e) => warn!("tags cache: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vorbis_block() {
        let mut b = Vec::new();
        b.extend(3u32.to_le_bytes());
        b.extend(b"ven");
        let c = ["TITLE=Song", "ARTIST=Band", "ALBUM=Rec", "TRACKNUMBER=3/12"];
        b.extend((c.len() as u32).to_le_bytes());
        for s in c {
            b.extend((s.len() as u32).to_le_bytes());
            b.extend(s.as_bytes());
        }
        let t = parse_vorbis(&b);
        assert_eq!((t.title.as_str(), t.artist.as_str(), t.album.as_str(), t.track_no), ("Song", "Band", "Rec", 3));
    }

    #[test]
    fn id3_utf16() {
        let d = [1u8, 0xFF, 0xFE, b'H', 0, b'i', 0, 0, 0];
        assert_eq!(id3_text(&d), "Hi");
    }

    fn vorbis(c: &[&str]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(3u32.to_le_bytes());
        b.extend(b"ven");
        b.extend((c.len() as u32).to_le_bytes());
        for s in c {
            b.extend((s.len() as u32).to_le_bytes());
            b.extend(s.as_bytes());
        }
        b
    }

    fn id3_frame(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut f = id.to_vec();
        f.extend((body.len() as u32).to_be_bytes()); // v2.3: plain size
        f.extend([0, 0]);
        f.extend(body);
        f
    }

    #[test]
    fn raw_flac_file() {
        let dir = std::env::temp_dir().join(format!("abyss-tags-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.flac");
        let c = vorbis(&["artist=A", "TITLE=T", "LABEL=L", "CATALOGNUMBER=C1", "METADATA_BLOCK_PICTURE=xx"]);
        let mut f = b"fLaC".to_vec();
        f.extend([0x00, 0, 0, 2, 0xAA, 0xBB]); // STREAMINFO stand-in, 2 bytes
        f.extend([0x84, 0, (c.len() >> 8) as u8, c.len() as u8]);
        f.extend(&c);
        fs::write(&p, &f).unwrap();
        let r = read_raw(&p);
        assert_eq!(r.first(&["ARTIST"]), Some("A"));
        assert_eq!(r.first(&["ORGANIZATION", "LABEL"]), Some("L"));
        assert_eq!(r.first(&["CATALOGNUMBER"]), Some("C1"));
        assert!(r.first(&["METADATA_BLOCK_PICTURE"]).is_none());
        assert_eq!(read(&p).title, "T");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn raw_id3_file() {
        let dir = std::env::temp_dir().join(format!("abyss-id3-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.mp3");
        let mut frames = Vec::new();
        frames.extend(id3_frame(b"TPE1", b"\x03Artist\x00"));
        frames.extend(id3_frame(b"TIT2", &[1, 0xFF, 0xFE, b'S', 0, b'o', 0, 0, 0]));
        frames.extend(id3_frame(b"TPUB", b"\x00Label"));
        frames.extend(id3_frame(b"TXXX", b"\x00CATALOGNUMBER\x00CAT 9"));
        frames.extend(id3_frame(b"APIC", b"\x00junk"));
        let mut f = b"ID3\x03\x00\x00".to_vec();
        let n = frames.len();
        f.extend([(n >> 21) as u8 & 0x7f, (n >> 14) as u8 & 0x7f, (n >> 7) as u8 & 0x7f, n as u8 & 0x7f]);
        f.extend(&frames);
        f.extend([0xFF, 0xFB, 0, 0]);
        fs::write(&p, &f).unwrap();
        let r = read_raw(&p);
        assert_eq!(r.first(&["TPE1"]), Some("Artist"));
        assert_eq!(r.first(&["TIT2"]), Some("So"));
        assert_eq!(r.first(&["TPUB"]), Some("Label"));
        assert_eq!(r.first(&["TXXX:CATALOGNUMBER"]), Some("CAT 9"));
        assert!(r.0.iter().all(|(k, _)| k != "APIC"));
        let t = read(&p);
        assert_eq!((t.artist.as_str(), t.title.as_str()), ("Artist", "So"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn id3_multi_value_and_bom_per_string() {
        let d = [1u8, 0xFF, 0xFE, b'a', 0, 0, 0, 0xFE, 0xFF, 0, b'b', 0, 0];
        assert_eq!(id3_strings(&d), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(id3_strings(b"\x00x\x00y"), vec!["x".to_string(), "y".to_string()]);
    }
}
