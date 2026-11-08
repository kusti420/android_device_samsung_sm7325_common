//! Native FLAC as a network stream: the head (`fLaC` + metadata, STREAMINFO) and the frames,
//! cut out of bytes that arrive in pieces.
//!
//! The radio's v1 audio (radio docs/public-api-v1.md) is byte for byte a `.flac` file whose
//! STREAMINFO says "total samples = 0, no MD5". Burst answers always hold whole frames; an
//! endless stream delivers them in arbitrary TCP pieces. Instead of running symphonia's FLAC
//! demuxer on a growing buffer (it treats the end of the bytes as the end of the file), this
//! module finds the frame boundaries itself and hands whole frames to symphonia's FLAC
//! decoder as packets (the offload work confirmed symphonia's packets are raw frames).
//! Whole frames are also exactly what a later DSP offload of the stream needs.
//!
//! A frame ends where its CRC-16 (over the whole frame, footer included) is zero AND a valid
//! frame header (sync, reserved bits, CRC-8) starts, or the input ends at a known message
//! boundary (`finish`). The two checks together make a false cut practically impossible.

use std::fmt;

/// A frame longer than this is garbage: drop a byte and look for the next sync.
const MAX_FRAME: usize = 1 << 20;

#[derive(Debug, PartialEq, Eq)]
pub enum FlacError {
    NotFlac,
    NoStreamInfo,
    BadStreamInfo(&'static str),
}

impl fmt::Display for FlacError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlacError::NotFlac => write!(f, "not a FLAC stream (no fLaC marker)"),
            FlacError::NoStreamInfo => write!(f, "FLAC head without STREAMINFO"),
            FlacError::BadStreamInfo(w) => write!(f, "bad STREAMINFO: {w}"),
        }
    }
}

impl std::error::Error for FlacError {}

/// The STREAMINFO block (what the decoder is opened with).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    /// The 34-byte block body (symphonia's FLAC decoder takes it as `extra_data`).
    pub raw: [u8; 34],
    pub sample_rate: u32,
    pub channels: u32,
    pub bits: u32,
    pub max_block: u32,
}

impl StreamInfo {
    pub fn parse(b: &[u8]) -> Result<StreamInfo, FlacError> {
        let raw: [u8; 34] = b.get(..34).and_then(|s| s.try_into().ok()).ok_or(FlacError::BadStreamInfo("short"))?;
        let max_block = u32::from(u16::from_be_bytes([raw[2], raw[3]]));
        let x = u64::from_be_bytes([raw[10], raw[11], raw[12], raw[13], raw[14], raw[15], raw[16], raw[17]]);
        let sample_rate = (x >> 44) as u32;
        let channels = ((x >> 41) & 0x7) as u32 + 1;
        let bits = ((x >> 36) & 0x1f) as u32 + 1;
        if sample_rate == 0 || sample_rate > 655_350 {
            return Err(FlacError::BadStreamInfo("sample rate"));
        }
        if max_block < 16 {
            return Err(FlacError::BadStreamInfo("block size"));
        }
        Ok(StreamInfo { raw, sample_rate, channels, bits, max_block })
    }
}

/// Parses the head at the start of `b`: Ok(None) = not all of it is here yet; else the
/// STREAMINFO and the head's length in bytes (where the first frame starts).
pub fn parse_head(b: &[u8]) -> Result<Option<(StreamInfo, usize)>, FlacError> {
    if b.len() < 4 {
        return if b"fLaC".starts_with(b) { Ok(None) } else { Err(FlacError::NotFlac) };
    }
    if &b[..4] != b"fLaC" {
        return Err(FlacError::NotFlac);
    }
    let mut at = 4;
    let mut info = None;
    loop {
        let Some(h) = b.get(at..at + 4) else { return Ok(None) };
        let last = h[0] & 0x80 != 0;
        let kind = h[0] & 0x7f;
        let len = (usize::from(h[1]) << 16) | (usize::from(h[2]) << 8) | usize::from(h[3]);
        let body = at + 4;
        if b.len() < body + len {
            return Ok(None);
        }
        if kind == 0 {
            info = Some(StreamInfo::parse(&b[body..body + len])?);
        }
        if kind == 127 {
            return Err(FlacError::BadStreamInfo("invalid metadata block type"));
        }
        at = body + len;
        if last {
            return info.map(|i| Some((i, at))).ok_or(FlacError::NoStreamInfo);
        }
    }
}

/// What a frame header says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub len: usize,
    /// Samples per channel in this frame.
    pub samples: u32,
    /// The frame (fixed blocking) or first sample (variable blocking) number.
    pub number: u64,
    pub variable: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HeaderAt {
    Valid(Header),
    Invalid,
    /// Not enough bytes to tell.
    Short,
}

/// Checks for a frame header at the start of `b` (sync, reserved bits, CRC-8).
pub fn header_at(b: &[u8]) -> HeaderAt {
    use HeaderAt::*;
    let need = |n: usize| b.len() < n;
    if need(2) {
        return if b.is_empty() || b[0] == 0xff { Short } else { Invalid };
    }
    if b[0] != 0xff || b[1] & 0xfe != 0xf8 {
        return Invalid;
    }
    if need(4) {
        return Short;
    }
    let variable = b[1] & 1 == 1;
    let bs_code = b[2] >> 4;
    let sr_code = b[2] & 0x0f;
    let ch = b[3] >> 4;
    let ss = (b[3] >> 1) & 0x7;
    if bs_code == 0 || sr_code == 15 || ch > 10 || ss == 3 || b[3] & 1 != 0 {
        return Invalid;
    }
    // The coded number: UTF-8 style, 1..7 bytes.
    let mut at = 4;
    if need(at + 1) {
        return Short;
    }
    let first = b[at];
    let extra = match first.leading_ones() {
        0 => 0,
        1 => return Invalid,
        n @ 2..=7 => (n - 1) as usize,
        _ => return Invalid,
    };
    if need(at + 1 + extra) {
        return Short;
    }
    let mut number = if extra == 0 { u64::from(first) } else { u64::from(first) & ((1u64 << (6 - extra)) - 1) };
    for i in 0..extra {
        let c = b[at + 1 + i];
        if c & 0xc0 != 0x80 {
            return Invalid;
        }
        number = (number << 6) | u64::from(c & 0x3f);
    }
    at += 1 + extra;
    let samples = match bs_code {
        1 => 192,
        2..=5 => 576u32 << (bs_code - 2),
        6 => {
            if need(at + 1) {
                return Short;
            }
            at += 1;
            u32::from(b[at - 1]) + 1
        }
        7 => {
            if need(at + 2) {
                return Short;
            }
            at += 2;
            u32::from(u16::from_be_bytes([b[at - 2], b[at - 1]])) + 1
        }
        _ => 256u32 << (bs_code - 8),
    };
    at += match sr_code {
        12 => 1,
        13 | 14 => 2,
        _ => 0,
    };
    if need(at + 1) {
        return Short;
    }
    if crc8(&b[..at]) != b[at] {
        return Invalid;
    }
    Valid(Header { len: at + 1, samples, number, variable })
}

pub fn crc8(b: &[u8]) -> u8 {
    let mut c = 0u8;
    for &x in b {
        c ^= x;
        for _ in 0..8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0x07 } else { c << 1 };
        }
    }
    c
}

fn crc16_step(table: &[u16; 256], c: u16, x: u8) -> u16 {
    (c << 8) ^ table[usize::from((c >> 8) as u8 ^ x)]
}

fn crc16_table() -> &'static [u16; 256] {
    static T: std::sync::OnceLock<[u16; 256]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u16; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = (i as u16) << 8;
            for _ in 0..8 {
                c = if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 };
            }
            *e = c;
        }
        t
    })
}

pub fn crc16(b: &[u8]) -> u16 {
    let t = crc16_table();
    b.iter().fold(0u16, |c, &x| crc16_step(t, c, x))
}

/// One whole FLAC frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub bytes: Vec<u8>,
    pub samples: u32,
    pub number: u64,
}

/// Cuts frames out of bytes that arrive in pieces. Feed it with [`push`](Framer::push), take
/// frames with [`next`](Framer::next); at a known end of input (a burst answer is complete,
/// the connection closed) call [`finish`](Framer::finish) so the last frame comes out too.
#[derive(Default)]
pub struct Framer {
    buf: Vec<u8>,
    /// Start of the current frame (a valid header) in `buf`, if one was found.
    start: Option<(usize, Header)>,
    /// Bytes of the current frame already run through the CRC.
    scan: usize,
    crc: u16,
    /// Bytes thrown away while looking for a sync (damage, or a stream joined mid-frame).
    pub skipped: u64,
    pub frames: u64,
}

impl Framer {
    pub fn new() -> Framer {
        Framer::default()
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// The next whole frame, if its end is known.
    pub fn next(&mut self) -> Option<Frame> {
        self.cut(false)
    }

    /// The input ended at a message boundary: the bytes held end the last frame.
    pub fn finish(&mut self) -> Option<Frame> {
        self.cut(true)
    }

    fn drop_front(&mut self, n: usize) {
        self.buf.drain(..n);
        self.start = None;
        self.scan = 0;
        self.crc = 0;
    }

    fn cut(&mut self, at_end: bool) -> Option<Frame> {
        let table = crc16_table();
        loop {
            // 1. Find a frame start.
            let (s, h) = match self.start {
                Some(x) => x,
                None => {
                    let mut i = 0;
                    let found = loop {
                        match self.buf.get(i..).map(header_at) {
                            Some(HeaderAt::Valid(h)) => break Some((i, h)),
                            Some(HeaderAt::Short) if !at_end => break None,
                            Some(HeaderAt::Short) | None => {
                                i = self.buf.len();
                                break None;
                            }
                            Some(HeaderAt::Invalid) => i += 1,
                        }
                    };
                    let skip = found.map_or(i, |(i, _)| i);
                    if skip > 0 {
                        self.skipped += skip as u64;
                        self.drop_front(skip);
                    }
                    let (_, h) = found?;
                    self.start = Some((0, h));
                    self.scan = 0;
                    self.crc = 0;
                    (0, h)
                }
            };
            // 2. Run the CRC forward; at every zero, see whether a header follows.
            let min_end = s + h.len + 2;
            while s + self.scan < self.buf.len() {
                let p = s + self.scan;
                self.crc = crc16_step(table, self.crc, self.buf[p]);
                self.scan += 1;
                let end = p + 1;
                if end < min_end || self.crc != 0 {
                    continue;
                }
                match header_at(&self.buf[end..]) {
                    HeaderAt::Valid(_) => return Some(self.take(s, end, h)),
                    // At the end of the input a partial header after it is a cut frame.
                    HeaderAt::Short if at_end => return Some(self.take(s, end, h)),
                    HeaderAt::Short => {
                        // Decide when more bytes are here: step back so this spot is checked again.
                        self.scan -= 1;
                        self.crc = crc16(&self.buf[s..p]);
                        return None;
                    }
                    HeaderAt::Invalid => {}
                }
            }
            if self.scan > MAX_FRAME {
                // No end in sight: not a real frame start. Skip its first byte, resync.
                self.skipped += 1;
                self.drop_front(s + 1);
                continue;
            }
            if at_end && !self.buf.is_empty() {
                // The input ended inside a frame (a cut connection): the rest is useless.
                self.skipped += self.buf.len() as u64;
                self.buf.clear();
                self.start = None;
                self.scan = 0;
                self.crc = 0;
            }
            return None;
        }
    }

    fn take(&mut self, s: usize, end: usize, h: Header) -> Frame {
        let bytes = self.buf[s..end].to_vec();
        self.buf.drain(..end);
        self.start = None;
        self.scan = 0;
        self.crc = 0;
        self.frames += 1;
        Frame { bytes, samples: h.samples, number: h.number }
    }
}

/// Test helpers: a tiny FLAC encoder (CONSTANT and VERBATIM subframes) good enough to build
/// valid streams for the framer and the decoder.
#[cfg(test)]
pub mod testenc {
    use super::{crc16, crc8};

    pub struct Bits {
        pub out: Vec<u8>,
        acc: u64,
        n: u32,
    }

    impl Bits {
        pub fn new() -> Bits {
            Bits { out: Vec::new(), acc: 0, n: 0 }
        }
        pub fn put(&mut self, v: u64, bits: u32) {
            for i in (0..bits).rev() {
                self.acc = (self.acc << 1) | ((v >> i) & 1);
                self.n += 1;
                if self.n == 8 {
                    self.out.push(self.acc as u8);
                    self.acc = 0;
                    self.n = 0;
                }
            }
        }
        pub fn align(&mut self) {
            while self.n != 0 {
                self.put(0, 1);
            }
        }
    }

    /// `fLaC` + STREAMINFO (44.1 kHz, stereo, 16 bit, block 4608, total 0, no MD5).
    pub fn head(block: u16) -> Vec<u8> {
        let mut v = b"fLaC".to_vec();
        v.extend_from_slice(&[0x80, 0, 0, 34]);
        let mut b = Bits::new();
        b.put(u64::from(block), 16);
        b.put(u64::from(block), 16);
        b.put(0, 24);
        b.put(0, 24);
        b.put(44100, 20);
        b.put(1, 3);
        b.put(15, 5);
        b.put(0, 36);
        for _ in 0..16 {
            b.put(0, 8);
        }
        v.extend_from_slice(&b.out);
        v
    }

    fn utf8_num(n: u64) -> Vec<u8> {
        if n < 0x80 {
            return vec![n as u8];
        }
        let mut cont = Vec::new();
        let mut x = n;
        let mut k = 1;
        loop {
            cont.push(0x80 | (x & 0x3f) as u8);
            x >>= 6;
            k += 1;
            if x < (1 << (7 - k)) {
                break;
            }
        }
        let lead = ((0xff00u16 >> k) as u8) | x as u8;
        let mut v = vec![lead];
        v.extend(cont.into_iter().rev());
        v
    }

    /// One frame, 16-bit stereo 44.1 kHz, `n` samples (block size code 7), number `num`.
    /// `left`/`right`: Some(value) = CONSTANT subframe, None = VERBATIM ramp.
    pub fn frame(num: u64, n: u16, left: Option<i16>, right: Option<i16>) -> Vec<u8> {
        let mut h = vec![0xff, 0xf8, (7 << 4) | 9, (1 << 4) | (4 << 1)];
        h.extend(utf8_num(num));
        h.extend_from_slice(&(n - 1).to_be_bytes());
        h.push(crc8(&h));
        let mut b = Bits::new();
        for c in [left, right] {
            match c {
                Some(v) => {
                    b.put(0, 1);
                    b.put(0, 6);
                    b.put(0, 1);
                    b.put(u64::from(v as u16), 16);
                }
                None => {
                    b.put(0, 1);
                    b.put(1, 6);
                    b.put(0, 1);
                    for i in 0..n {
                        b.put(u64::from((i as i16).wrapping_mul(7) as u16), 16);
                    }
                }
            }
        }
        b.align();
        let mut f = h;
        f.extend_from_slice(&b.out);
        let c = crc16(&f);
        f.extend_from_slice(&c.to_be_bytes());
        f
    }
}

#[cfg(test)]
mod tests {
    use super::testenc::*;
    use super::*;

    #[test]
    fn crcs_match_the_spec() {
        // CRC-8 poly 0x07 and CRC-16 poly 0x8005 of "123456789".
        assert_eq!(crc8(b"123456789"), 0xf4);
        assert_eq!(crc16(b"123456789"), 0xfee8);
        let f = frame(3, 100, Some(5), None);
        assert_eq!(crc16(&f), 0, "a frame with its footer CRCs to zero");
    }

    #[test]
    fn head_parses_and_waits_for_more() {
        let h = head(4608);
        assert_eq!(h.len(), 42);
        let (i, n) = parse_head(&h).unwrap().unwrap();
        assert_eq!((i.sample_rate, i.channels, i.bits, i.max_block, n), (44100, 2, 16, 4608, 42));
        assert_eq!(parse_head(&h[..20]).unwrap(), None);
        assert_eq!(parse_head(b"fL").unwrap(), None);
        assert_eq!(parse_head(b"ID3\x03xxxx"), Err(FlacError::NotFlac));
        // A padding block before the STREAMINFO-less last block: no info.
        assert_eq!(parse_head(b"fLaC\x81\x00\x00\x00"), Err(FlacError::NoStreamInfo));
    }

    #[test]
    fn header_fields() {
        let f = frame(70_000, 4608, Some(1), Some(2));
        match header_at(&f) {
            HeaderAt::Valid(h) => {
                assert_eq!((h.samples, h.number, h.variable), (4608, 70_000, false));
            }
            o => panic!("{o:?}"),
        }
        assert_eq!(header_at(&f[..5]), HeaderAt::Short);
        let mut bad = f.clone();
        bad[2] ^= 0x01; // sample rate code changed: CRC-8 fails
        assert_eq!(header_at(&bad), HeaderAt::Invalid);
        assert_eq!(header_at(&[0x00, 0x11]), HeaderAt::Invalid);
    }

    fn stream(n: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
        let frames: Vec<Vec<u8>> = (0..n as u64)
            .map(|i| frame(i, 300 + i as u16 * 7, if i % 2 == 0 { Some(i as i16) } else { None }, Some(-3)))
            .collect();
        (frames.concat(), frames)
    }

    #[test]
    fn frames_from_any_split_of_the_bytes() {
        let (all, frames) = stream(12);
        for piece in [1usize, 2, 3, 7, 64, 1000, all.len()] {
            let mut fr = Framer::new();
            let mut got = Vec::new();
            for c in all.chunks(piece) {
                fr.push(c);
                while let Some(f) = fr.next() {
                    got.push(f);
                }
            }
            // The last frame needs the end of the input.
            assert_eq!(got.len(), frames.len() - 1, "piece {piece}");
            got.push(fr.finish().expect("last frame"));
            assert_eq!(fr.finish(), None);
            for (i, f) in got.iter().enumerate() {
                assert_eq!(f.bytes, frames[i], "frame {i}, piece {piece}");
                assert_eq!(f.number, i as u64);
                assert_eq!(f.samples, 300 + i as u32 * 7);
            }
            assert_eq!(fr.skipped, 0);
        }
    }

    #[test]
    fn garbage_and_a_cut_frame_are_skipped() {
        let (all, frames) = stream(4);
        let mut fr = Framer::new();
        fr.push(&[0x12, 0xff, 0x00, 0x77]);
        fr.push(&all);
        fr.push(&frames[0][..frames[0].len() / 2]); // a connection cut mid-frame
        let mut got = Vec::new();
        while let Some(f) = fr.next() {
            got.push(f);
        }
        assert_eq!(got.len(), 4, "the 4th frame ends where a (cut) header starts");
        assert_eq!(fr.finish(), None, "half a frame at the end is dropped");
        assert_eq!(fr.skipped, 4 + frames[0].len() as u64 / 2);
    }
}
