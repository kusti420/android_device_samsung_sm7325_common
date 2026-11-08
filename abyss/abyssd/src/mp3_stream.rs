//! MPEG audio Layer III as a network stream: frames cut out of bytes that arrive in pieces.
//!
//! The radio's `mp3` / `mp3-128` audio (radio docs/public-api-v1.md) is raw MPEG-1 Layer III
//! frames, CBR, no ID3/Xing/ICY, frames always whole. A frame's length follows from its
//! 4-byte header, so a frame is accepted when its header is valid and the next frame's header
//! (or the known end of the input) follows right after it. An ID3v2 tag at the start (other
//! servers) is skipped.

/// What a frame header says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mp3Header {
    pub len: usize,
    pub samples: u32,
    pub sample_rate: u32,
    pub channels: u32,
}

/// Parses a Layer III frame header at the start of `b` (None = not one, or too short).
pub fn header(b: &[u8]) -> Option<Mp3Header> {
    if b.len() < 4 || b[0] != 0xff || b[1] & 0xe0 != 0xe0 {
        return None;
    }
    let version = (b[1] >> 3) & 3; // 3 = MPEG-1, 2 = MPEG-2, 0 = MPEG-2.5, 1 reserved
    let layer = (b[1] >> 1) & 3; // 1 = Layer III
    if version == 1 || layer != 1 {
        return None;
    }
    let br_idx = usize::from(b[2] >> 4);
    let sr_idx = usize::from((b[2] >> 2) & 3);
    let pad = usize::from((b[2] >> 1) & 1);
    if br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None;
    }
    const BR_V1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const BR_V2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    const SR: [u32; 3] = [44100, 48000, 32000];
    let (kbps, rate, samples) = match version {
        3 => (BR_V1[br_idx], SR[sr_idx], 1152u32),
        2 => (BR_V2[br_idx], SR[sr_idx] / 2, 576),
        _ => (BR_V2[br_idx], SR[sr_idx] / 4, 576),
    };
    let len = (samples as usize / 8) * kbps as usize * 1000 / rate as usize + pad;
    let channels = if b[3] >> 6 == 3 { 1 } else { 2 };
    (len > 4).then_some(Mp3Header { len, samples, sample_rate: rate, channels })
}

/// Length of an ID3v2 tag at the start of `b`: Some(Some(n)) = skip n bytes, Some(None) = need
/// more bytes to tell, None = no tag.
fn id3_len(b: &[u8]) -> Option<Option<usize>> {
    if b.len() < 3 {
        return if b"ID3".starts_with(b) && !b.is_empty() { Some(None) } else { None };
    }
    if &b[..3] != b"ID3" {
        return None;
    }
    if b.len() < 10 {
        return Some(None);
    }
    let size = b[6..10].iter().fold(0usize, |a, &x| (a << 7) | usize::from(x & 0x7f));
    let footer = if b[5] & 0x10 != 0 { 10 } else { 0 };
    Some(Some(10 + size + footer))
}

/// One whole frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mp3Frame {
    pub bytes: Vec<u8>,
    pub head: Mp3Header,
}

#[derive(Default)]
pub struct Mp3Framer {
    buf: Vec<u8>,
    pub skipped: u64,
    pub frames: u64,
    started: bool,
}

impl Mp3Framer {
    pub fn new() -> Mp3Framer {
        Mp3Framer::default()
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn next(&mut self) -> Option<Mp3Frame> {
        self.cut(false)
    }

    /// The input ended at a message boundary: a last whole frame comes out too.
    pub fn finish(&mut self) -> Option<Mp3Frame> {
        let f = self.cut(true);
        if f.is_none() && !self.buf.is_empty() {
            self.skipped += self.buf.len() as u64;
            self.buf.clear();
        }
        f
    }

    fn cut(&mut self, at_end: bool) -> Option<Mp3Frame> {
        if !self.started {
            match id3_len(&self.buf) {
                Some(Some(n)) if self.buf.len() >= n => {
                    self.buf.drain(..n);
                }
                Some(_) if !at_end => return None,
                _ => {}
            }
            self.started = true;
        }
        let mut i = 0;
        while i + 4 <= self.buf.len() {
            if let Some(h) = header(&self.buf[i..]) {
                let end = i + h.len;
                let next_ok = match self.buf.get(end..end + 4) {
                    Some(n) => header(n).is_some_and(|n| n.sample_rate == h.sample_rate),
                    None if end <= self.buf.len() && at_end => true,
                    None if end + 4 > self.buf.len() && !at_end => {
                        // Need the next header (or the end) to be sure.
                        self.drop_front(i);
                        return None;
                    }
                    None => false,
                };
                if next_ok {
                    self.drop_front(i);
                    let bytes: Vec<u8> = self.buf.drain(..h.len).collect();
                    self.frames += 1;
                    return Some(Mp3Frame { bytes, head: h });
                }
            }
            i += 1;
        }
        // Keep the last 3 bytes: they may start a header.
        let keep = if at_end { 0 } else { self.buf.len().min(3) };
        let drop = self.buf.len() - keep;
        self.drop_front(drop);
        None
    }

    fn drop_front(&mut self, n: usize) {
        if n > 0 {
            self.skipped += n as u64;
            self.buf.drain(..n);
        }
    }
}

#[cfg(test)]
pub mod testenc {
    /// A silent-ish MPEG-1 Layer III frame header + zero payload of the right length
    /// (not decodable audio; for framing tests).
    pub fn frame(kbps_idx: u8, pad: bool) -> Vec<u8> {
        let b2 = (kbps_idx << 4) | (u8::from(pad) << 1);
        let h = [0xff, 0xfb, b2, 0x44];
        let len = super::header(&h).unwrap().len;
        let mut v = h.to_vec();
        v.resize(len, 0);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::testenc::frame;
    use super::*;

    #[test]
    fn header_lengths() {
        // 320 kbps, 44.1 kHz: 1044 / 1045 bytes; 128 kbps: 417 / 418.
        assert_eq!(header(&[0xff, 0xfb, 0xe0, 0x44]).unwrap().len, 1044);
        assert_eq!(header(&[0xff, 0xfb, 0xe2, 0x44]).unwrap().len, 1045);
        let h = header(&[0xff, 0xfb, 0x90, 0xc4]).unwrap();
        assert_eq!((h.len, h.samples, h.sample_rate, h.channels), (417, 1152, 44100, 1));
        assert!(header(&[0xff, 0xfd, 0x90, 0x44]).is_none(), "layer II");
        assert!(header(&[0xff, 0xfb, 0xf0, 0x44]).is_none(), "bad bitrate");
    }

    #[test]
    fn frames_from_pieces_with_id3_and_garbage() {
        let frames: Vec<Vec<u8>> = (0..6).map(|i| frame(if i % 2 == 0 { 14 } else { 9 }, i == 3)).collect();
        let mut all = b"ID3\x04\x00\x00\x00\x00\x00\x05hello".to_vec();
        all.extend(frames.concat());
        for piece in [1usize, 5, 333, all.len()] {
            let mut fr = Mp3Framer::new();
            let mut got = Vec::new();
            for c in all.chunks(piece) {
                fr.push(c);
                while let Some(f) = fr.next() {
                    got.push(f.bytes);
                }
            }
            got.extend(fr.finish().map(|f| f.bytes));
            assert_eq!(got, frames, "piece {piece}");
            assert_eq!(fr.skipped, 0, "piece {piece}");
        }
        let mut fr = Mp3Framer::new();
        fr.push(&[1, 2, 3]);
        fr.push(&frames[0]);
        fr.push(&frames[1][..100]);
        assert_eq!(fr.next().unwrap().bytes, frames[0]);
        assert_eq!(fr.finish(), None);
        assert_eq!(fr.skipped, 3 + 100);
    }
}
