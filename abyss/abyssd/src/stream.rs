//! Network audio streams: the buffer between the network thread and the player, and the
//! CPU decoder on the player side.
//!
//! Port of the stream logic of the radio's reference client (radio tools/radio_v1_client.py,
//! class `Stream`): one *stream* runs from an answer with `X-Stream-Reset: 1` to the next
//! one. Each stream has its own decoder; a new stream never feeds the old decoder. When a
//! reset comes, the old stream plays out what it holds, then the new one starts.
//!
//! - The network side ([`Feed::begin_stream`], [`Feed::push`]) appends whole FLAC frames
//!   (flac_stream.rs) and one **mark** per answer: where the answer starts in the stream (in
//!   samples) and its `X-Stream-Time-Ms`.
//! - The player side ([`FeedPlayer::next_pcm`]) takes frames in order, decodes them with
//!   symphonia's FLAC decoder and returns interleaved stereo i16 (what `AudioOut` takes).
//!   A stream starts only once [`START_S`] is in hand (15 s), or the session's own start; if it
//!   runs dry later it stalls and resumes, never skipping ahead.
//! - **Early start** ([`Feed::set_early_start`], a live `http` stream set to "as soon as
//!   possible"): a stream also starts with only that much (~2 s) in hand once its network side
//!   says it is safe ([`Feed::burst_over`]: the connect burst is over, or still arrives faster
//!   than 1.5× real time, so playing cannot catch up with it): an Icecast server sends a few
//!   seconds at once on connect, then real time, so waiting longer only adds silence.
//! - **Start gate** ([`Feed::hold_start_until`]): nothing starts before the gate opens or its
//!   deadline passes (names before sound: the tracklist arrives on a second connection while
//!   the audio fills).
//! - Wall time of the sample playing now = the Time-Ms of the answer that carried it +
//!   (samples played since that answer's first sample / rate) ([`Feed::now_wall_ms`]).
//!
//! The player counts a frame as played when it hands its PCM to the output, so the clock is
//! off by at most one frame plus the output's own buffer (~0.1-0.3 s): far below song
//! boundaries.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use log::{info, warn};
use symphonia_core::audio::SampleBuffer;
use symphonia_core::codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_FLAC, CODEC_TYPE_MP3};
use symphonia_core::formats::Packet;

use crate::flac_stream::StreamInfo;

/// What a stream carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Codec {
    /// Native FLAC: STREAMINFO from the head.
    Flac(StreamInfo),
    /// MPEG-1/2 Layer III: from the first frame header.
    Mp3 { sample_rate: u32, channels: u32 },
}

impl Codec {
    pub fn sample_rate(&self) -> u32 {
        match self {
            Codec::Flac(i) => i.sample_rate,
            Codec::Mp3 { sample_rate, .. } => *sample_rate,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Codec::Flac(_) => "FLAC",
            Codec::Mp3 { .. } => "MP3",
        }
    }
}

/// One whole compressed frame (FLAC or MP3) and how many samples (per channel) it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub bytes: Vec<u8>,
    pub samples: u32,
    pub number: u64,
}

impl From<crate::flac_stream::Frame> for Frame {
    fn from(f: crate::flac_stream::Frame) -> Frame {
        Frame { bytes: f.bytes, samples: f.samples, number: f.number }
    }
}

/// (Re)start playback only with this much in hand (reference client START_S).
pub const START_S: f64 = 15.0;

/// One answer's place on the wall clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    /// Sample (per channel) in the stream where the answer's first sample is.
    pub at: u64,
    /// Its X-Stream-Time-Ms (server wall clock).
    pub time_ms: i64,
}

struct StreamBuf {
    id: u64,
    codec: Codec,
    frames: VecDeque<Frame>,
    /// Compressed bytes held (what the frames take in RAM).
    bytes: u64,
    received: u64,
    played: u64,
    marks: Vec<Mark>,
    started: bool,
    /// No more data will come (a newer stream exists, or the session stopped).
    ended: bool,
    /// The network side says the connect burst is over (early start may begin).
    burst_over: bool,
}

impl StreamBuf {
    fn rate(&self) -> f64 {
        f64::from(self.codec.sample_rate().max(1))
    }
    fn held_s(&self) -> f64 {
        (self.received - self.played) as f64 / self.rate()
    }
    /// Whether this stream may start now: its start in hand, or (early start) the burst over
    /// with the early amount in hand; never while the gate is closed.
    fn may_start(&self, start_s: f64, early_s: f64, gate_open: bool) -> bool {
        gate_open && (self.held_s() >= start_s || (early_s > 0.0 && self.burst_over && self.held_s() >= early_s))
    }
    fn wall_ms(&self, sample: u64) -> Option<i64> {
        let i = self.marks.partition_point(|m| m.at <= sample).checked_sub(1)?;
        let m = self.marks[i];
        Some(m.time_ms + ((sample - m.at) as f64 * 1000.0 / self.rate()) as i64)
    }
}

#[derive(Default)]
struct Inner {
    streams: VecDeque<StreamBuf>,
    next_id: u64,
    /// The session is over: the player gets `Ended` once it has played what it holds.
    closed: bool,
    /// Stop now: drop everything, `Ended` at once.
    stopped: bool,
    stalls: u64,
    /// The playing stream is dry right now (one stall episode).
    dry: bool,
    start_samples_s: f64,
    /// Early start (0 = off): seconds in hand that suffice once the burst is over.
    early_s: f64,
    /// Nothing starts before this (None = open).
    gate_until: Option<Instant>,
}

impl Inner {
    fn gate_open(&self) -> bool {
        self.gate_until.is_none_or(|t| Instant::now() >= t)
    }

    /// Starts every waiting stream that may start now.
    fn start_ready(&mut self) {
        let (start_s, early_s, gate) = (self.start_samples_s, self.early_s, self.gate_open());
        for s in self.streams.iter_mut().filter(|s| !s.started && !s.ended) {
            if s.may_start(start_s, early_s, gate) {
                s.started = true;
                info!("stream {}: {:.1} s in hand, starting{}", s.id, s.held_s(), if s.burst_over { " (early start)" } else { "" });
            }
        }
    }
}

/// What the network side sees of the buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FeedStatus {
    /// Seconds received but not played yet, over every stream that is still queued.
    pub buffered_s: f64,
    /// The compressed bytes those take.
    pub buffered_bytes: u64,
    /// The newest stream: seconds received, and whether it has started playing.
    pub latest_received_s: f64,
    pub latest_started: bool,
    /// Some stream exists.
    pub has_stream: bool,
    /// The player is waiting for the 15 s prebuffer (nothing plays).
    pub buffering: bool,
    /// The stream that plays ran dry (waiting for data) since this many times.
    pub stalls: u64,
    pub streams: usize,
}

/// The buffer shared by the network thread (writer) and the player (reader).
pub struct Feed {
    inner: Mutex<Inner>,
    cv: Condvar,
}

impl std::fmt::Debug for Feed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Feed({:?})", self.status())
    }
}

impl Feed {
    /// `start_s`: prebuffer before a stream starts (tests use less).
    pub fn with_start(start_s: f64) -> Arc<Feed> {
        Arc::new(Feed {
            inner: Mutex::new(Inner { start_samples_s: start_s, ..Default::default() }),
            cv: Condvar::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new stream (a reset answer): the stream before it gets no more data and plays out.
    pub fn begin_stream(&self, codec: Codec) -> u64 {
        let mut g = self.lock();
        if let Some(last) = g.streams.back_mut() {
            last.ended = true;
        }
        g.next_id += 1;
        let id = g.next_id;
        info!("stream {id}: {codec:?} (prebuffer {:.0} s)", g.start_samples_s);
        g.streams.push_back(StreamBuf {
            id,
            codec,
            frames: VecDeque::new(),
            bytes: 0,
            received: 0,
            played: 0,
            marks: Vec::new(),
            started: false,
            ended: false,
            burst_over: false,
        });
        drop(g);
        self.cv.notify_all();
        id
    }

    /// Appends one answer's frames to stream `id`; `time_ms` = its X-Stream-Time-Ms (None
    /// for later pieces of the same answer, e.g. an endless stream). False = that stream is
    /// gone (stopped, or played out).
    pub fn push(&self, id: u64, frames: Vec<Frame>, time_ms: Option<i64>) -> bool {
        let mut g = self.lock();
        let Some(s) = g.streams.iter_mut().find(|s| s.id == id && !s.ended) else { return false };
        if let Some(t) = time_ms {
            if s.marks.last().is_none_or(|m| m.at != s.received) {
                s.marks.push(Mark { at: s.received, time_ms: t });
            } else if let Some(m) = s.marks.last_mut() {
                m.time_ms = t; // an empty answer before: this one starts at the same sample
            }
        }
        for f in frames {
            s.received += u64::from(f.samples);
            s.bytes += f.bytes.len() as u64;
            s.frames.push_back(f);
        }
        g.start_ready();
        drop(g);
        self.cv.notify_all();
        true
    }

    /// Early start (live `http` streams): a stream may start with `secs` in hand once its
    /// connect burst is over. 0 = off.
    pub fn set_early_start(&self, secs: f64) {
        self.lock().early_s = secs.max(0.0);
    }

    /// Early start is safe for stream `id`: its connect burst is over (arrival fell to about
    /// real time), or it still arrives clearly faster than it plays.
    pub fn burst_over(&self, id: u64) {
        let mut g = self.lock();
        if let Some(s) = g.streams.iter_mut().find(|s| s.id == id) {
            s.burst_over = true;
        }
        g.start_ready();
        drop(g);
        self.cv.notify_all();
    }

    /// Nothing starts before `until` (None = open the gate now).
    pub fn hold_start_until(&self, until: Option<Instant>) {
        let mut g = self.lock();
        g.gate_until = until;
        g.start_ready();
        drop(g);
        self.cv.notify_all();
    }

    /// No more data: the player plays what it holds, then gets `Ended`.
    pub fn close(&self) {
        let mut g = self.lock();
        g.closed = true;
        for s in g.streams.iter_mut() {
            s.ended = true;
        }
        drop(g);
        self.cv.notify_all();
    }

    /// Stop at once (the person stopped the radio): `Ended` on the next pull.
    pub fn stop(&self) {
        let mut g = self.lock();
        g.stopped = true;
        g.closed = true;
        g.streams.clear();
        drop(g);
        self.cv.notify_all();
    }

    pub fn status(&self) -> FeedStatus {
        let g = self.lock();
        let buffered_s = g.streams.iter().map(|s| (s.received - s.played) as f64 / s.rate()).sum();
        let latest = g.streams.back();
        FeedStatus {
            buffered_s,
            buffered_bytes: g.streams.iter().map(|s| s.bytes).sum(),
            latest_received_s: latest.map_or(0.0, |s| s.received as f64 / s.rate()),
            latest_started: latest.is_some_and(|s| s.started),
            has_stream: latest.is_some(),
            buffering: g.streams.front().is_some_and(|s| !s.started && !s.ended),
            stalls: g.stalls,
            streams: g.streams.len(),
        }
    }

    /// Server wall-clock time of the sample playing now (None before playback starts).
    pub fn now_wall_ms(&self) -> Option<i64> {
        let g = self.lock();
        let s = g.streams.front()?;
        if !s.started && s.played == 0 {
            return None;
        }
        s.wall_ms(s.played)
    }

    /// Seconds behind the live edge (the newest sample received vs the one playing).
    pub fn latency_s(&self) -> Option<f64> {
        let g = self.lock();
        let s = g.streams.front()?;
        s.started.then(|| (s.received - s.played) as f64 / s.rate())
    }

    /// Prebuffer progress of the stream that is about to play, 0..=1.
    pub fn prebuffer(&self) -> f64 {
        let g = self.lock();
        match g.streams.front() {
            Some(s) if !s.started => {
                let need = if g.early_s > 0.0 { g.early_s.min(g.start_samples_s) } else { g.start_samples_s };
                (s.held_s() / need.max(0.001)).min(1.0)
            }
            Some(_) => 1.0,
            None => 0.0,
        }
    }

    /// The next frame for the player, waiting up to `wait` for one.
    fn pull(&self, wait: Duration) -> Pull {
        let deadline = Instant::now() + wait;
        let mut g = self.lock();
        loop {
            if g.stopped {
                return Pull::Ended;
            }
            // Played-out streams leave the queue.
            while g.streams.front().is_some_and(|s| s.ended && s.frames.is_empty()) {
                g.streams.pop_front();
            }
            // A gate deadline may have passed meanwhile.
            g.start_ready();
            let closed = g.closed;
            let state = match g.streams.front_mut() {
                None if closed => return Pull::Ended,
                None => Pull::Buffering,
                Some(s) if !s.started && !s.ended => Pull::Buffering,
                Some(s) => match s.frames.pop_front() {
                    Some(f) => {
                        s.played += u64::from(f.samples);
                        s.bytes = s.bytes.saturating_sub(f.bytes.len() as u64);
                        s.started = true;
                        let (id, codec) = (s.id, s.codec.clone());
                        g.dry = false;
                        return Pull::Frame { id, codec, frame: f };
                    }
                    None => Pull::Stalled,
                },
            };
            if matches!(state, Pull::Stalled) && !g.dry {
                g.dry = true;
                g.stalls += 1;
            }
            let now = Instant::now();
            if now >= deadline {
                return state;
            }
            g = self.cv.wait_timeout(g, deadline - now).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0);
        }
    }
}

enum Pull {
    Frame { id: u64, codec: Codec, frame: Frame },
    Buffering,
    Stalled,
    Ended,
}

/// What [`FeedPlayer::next_pcm`] produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pcm {
    /// `out` holds interleaved stereo i16 at this rate.
    Samples { rate: u32 },
    /// Waiting for the prebuffer (nothing plays yet).
    Buffering,
    /// The stream ran dry: no data within the wait (stall, resume later).
    Stalled,
    /// The session ended and everything was played.
    Ended,
}

/// The player-side decoder for one [`Feed`].
pub struct FeedPlayer {
    feed: Arc<Feed>,
    decoder: Option<(u64, Box<dyn Decoder>)>,
    sbuf: Option<SampleBuffer<i16>>,
    pub decode_errors: u64,
    pub frames: u64,
}

impl FeedPlayer {
    pub fn new(feed: Arc<Feed>) -> FeedPlayer {
        FeedPlayer { feed, decoder: None, sbuf: None, decode_errors: 0, frames: 0 }
    }

    /// Makes sure the decoder is the one of stream `id`; false = it cannot be opened.
    fn ensure_decoder(&mut self, id: u64, codec: &Codec) -> bool {
        if self.decoder.as_ref().is_none_or(|(d, _)| *d != id) {
            // A new stream gets a new decoder (never the old one).
            let mut p = CodecParameters::new();
            let made: symphonia_core::errors::Result<Box<dyn Decoder>> = match codec {
                Codec::Flac(info) => {
                    p.for_codec(CODEC_TYPE_FLAC)
                        .with_sample_rate(info.sample_rate)
                        .with_bits_per_sample(info.bits)
                        .with_extra_data(info.raw.to_vec().into_boxed_slice());
                    symphonia_bundle_flac::FlacDecoder::try_new(&p, &DecoderOptions::default())
                        .map(|d| Box::new(d) as Box<dyn Decoder>)
                }
                Codec::Mp3 { sample_rate, .. } => {
                    p.for_codec(CODEC_TYPE_MP3).with_sample_rate(*sample_rate);
                    symphonia_bundle_mp3::MpaDecoder::try_new(&p, &DecoderOptions::default())
                        .map(|d| Box::new(d) as Box<dyn Decoder>)
                }
            };
            match made {
                Ok(d) => self.decoder = Some((id, d)),
                Err(e) => {
                    warn!("stream {id}: no decoder: {e}");
                    self.decoder = None;
                    return false;
                }
            }
            self.sbuf = None;
        }
        true
    }

    /// Decodes the next frame into `out` (cleared first) as interleaved stereo i16. Waits at
    /// most `wait` for data, so the caller can handle its own commands in between.
    pub fn next_pcm(&mut self, out: &mut Vec<i16>, wait: Duration) -> Pcm {
        out.clear();
        loop {
            let (id, codec, frame) = match self.feed.pull(wait) {
                Pull::Frame { id, codec, frame } => (id, codec, frame),
                Pull::Buffering => return Pcm::Buffering,
                Pull::Stalled => return Pcm::Stalled,
                Pull::Ended => return Pcm::Ended,
            };
            if !self.ensure_decoder(id, &codec) {
                self.decode_errors += 1;
                continue;
            }
            let Some((_, dec)) = self.decoder.as_mut() else { continue };
            let packet = Packet::new_from_slice(0, frame.number, u64::from(frame.samples), &frame.bytes);
            let decoded = match dec.decode(&packet) {
                Ok(d) => d,
                Err(e) => {
                    // A damaged frame: skip it (its time still counts as played).
                    self.decode_errors += 1;
                    warn!("stream {id}: frame {} not decoded: {e}", frame.number);
                    continue;
                }
            };
            let spec = *decoded.spec();
            let rate = spec.rate;
            let n = decoded.frames();
            if n == 0 {
                continue;
            }
            let need = decoded.capacity();
            if self.sbuf.as_ref().is_none_or(|b| b.capacity() < need * spec.channels.count()) {
                self.sbuf = Some(SampleBuffer::<i16>::new(need as u64, spec));
            }
            let Some(sbuf) = self.sbuf.as_mut() else { continue };
            sbuf.copy_interleaved_ref(decoded);
            let ch = spec.channels.count().max(1);
            out.reserve(n * 2);
            for f in sbuf.samples().chunks_exact(ch) {
                let (l, r) = if ch == 1 { (f[0], f[0]) } else { (f[0], f[1]) };
                out.push(l);
                out.push(r);
            }
            self.frames += 1;
            return Pcm::Samples { rate };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flac_stream::testenc::{frame, head};
    use crate::flac_stream::{parse_head, Framer};
    use crate::mp3_stream::{Mp3Framer, testenc::frame as mp3_frame};

    fn frames(from: u64, n: usize, len: u16, value: i16) -> Vec<Frame> {
        let mut fr = Framer::new();
        for i in 0..n as u64 {
            fr.push(&frame(from + i, len, Some(value), Some(-value)));
        }
        let mut v = Vec::new();
        while let Some(f) = fr.next() {
            v.push(f.into());
        }
        v.extend(fr.finish().map(Frame::from));
        v
    }

    fn info() -> Codec {
        Codec::Flac(parse_head(&head(4410)).unwrap().unwrap().0)
    }

    #[test]
    fn mp3_frames_count_as_time() {
        let feed = Feed::with_start(0.0);
        let id = feed.begin_stream(Codec::Mp3 { sample_rate: 44100, channels: 2 });
        let mut fr = Mp3Framer::new();
        for _ in 0..4 {
            fr.push(&mp3_frame(14, false));
        }
        let mut fs: Vec<Frame> = Vec::new();
        while let Some(f) = fr.next() {
            fs.push(Frame { bytes: f.bytes, samples: f.head.samples, number: 0 });
        }
        fs.extend(fr.finish().map(|f| Frame { bytes: f.bytes, samples: f.head.samples, number: 0 }));
        assert_eq!(fs.len(), 4);
        feed.push(id, fs, Some(1000));
        assert!((feed.status().buffered_s - 4.0 * 1152.0 / 44100.0).abs() < 1e-9);
        assert_eq!(feed.status().buffered_bytes, 4 * mp3_frame(14, false).len() as u64);
        // Zero payloads are not real audio: the decoder may refuse them; it must not panic.
        let mut p = FeedPlayer::new(feed.clone());
        let mut out = Vec::new();
        feed.close();
        let mut n = 0;
        while p.next_pcm(&mut out, Duration::ZERO) != Pcm::Ended {
            n += 1;
            assert!(n < 10);
        }
    }

    #[test]
    fn early_start_after_the_burst_and_the_gate() {
        // Start 15 s, early 2 s: 2.5 s in hand starts only once the burst is over.
        let feed = Feed::with_start(15.0);
        feed.set_early_start(2.0);
        let mut p = FeedPlayer::new(feed.clone());
        let mut out = Vec::new();
        let id = feed.begin_stream(info());
        assert!(feed.push(id, frames(0, 25, 4410, 1), Some(1_000)));
        assert!(feed.status().buffering, "2.5 s is not 15 s while the burst still comes");
        feed.burst_over(id);
        assert!(!feed.status().buffering, "burst over with 2.5 s in hand: start");
        assert_eq!(p.next_pcm(&mut out, Duration::from_millis(1)), Pcm::Samples { rate: 44100 });
        // Less than the early amount: the burst being over is not enough.
        let feed = Feed::with_start(15.0);
        feed.set_early_start(2.0);
        let id = feed.begin_stream(info());
        feed.push(id, frames(0, 10, 4410, 1), Some(1_000));
        feed.burst_over(id);
        assert!(feed.status().buffering);
        feed.push(id, frames(10, 10, 4410, 1), None);
        assert!(!feed.status().buffering, "2 s in hand after the burst: start");
        // The gate: nothing starts before it opens, whatever is in hand.
        let feed = Feed::with_start(1.0);
        feed.hold_start_until(Some(Instant::now() + Duration::from_secs(60)));
        let id = feed.begin_stream(info());
        feed.push(id, frames(0, 20, 4410, 1), Some(1_000));
        assert!(feed.status().buffering, "gate closed");
        feed.hold_start_until(None);
        assert!(!feed.status().buffering, "gate opened");
        // A gate deadline that passes starts it on the player's next pull.
        let feed = Feed::with_start(1.0);
        feed.hold_start_until(Some(Instant::now() + Duration::from_millis(30)));
        let id = feed.begin_stream(info());
        feed.push(id, frames(0, 20, 4410, 1), Some(1_000));
        let mut p = FeedPlayer::new(feed.clone());
        assert_eq!(p.next_pcm(&mut out, Duration::from_millis(1)), Pcm::Buffering);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(p.next_pcm(&mut out, Duration::from_millis(1)), Pcm::Samples { rate: 44100 });
    }

    #[test]
    fn prebuffer_then_play_then_stall_then_end() {
        // Prebuffer 1 s; frames of 4410 samples = 0.1 s.
        let feed = Feed::with_start(1.0);
        let mut p = FeedPlayer::new(feed.clone());
        let mut out = Vec::new();
        assert_eq!(p.next_pcm(&mut out, Duration::from_millis(1)), Pcm::Buffering);
        let id = feed.begin_stream(info());
        assert!(feed.push(id, frames(0, 5, 4410, 100), Some(1_000_000)));
        assert!(feed.status().buffering);
        assert_eq!(p.next_pcm(&mut out, Duration::from_millis(1)), Pcm::Buffering);
        assert!(feed.push(id, frames(5, 5, 4410, 100), Some(1_000_500)));
        assert!(!feed.status().buffering);
        assert_eq!(feed.now_wall_ms(), Some(1_000_000));
        for _ in 0..10 {
            assert_eq!(p.next_pcm(&mut out, Duration::from_millis(1)), Pcm::Samples { rate: 44100 });
            assert_eq!(out.len(), 4410 * 2);
            assert_eq!((out[0], out[1]), (100, -100));
        }
        // Wall time: the second answer started at sample 22050 with Time-Ms 1_000_500.
        assert_eq!(feed.now_wall_ms(), Some(1_000_500 + 500));
        assert_eq!(p.next_pcm(&mut out, Duration::from_millis(5)), Pcm::Stalled);
        assert_eq!(feed.status().stalls, 1);
        // Data again: resume without a new prebuffer, never skipping.
        assert!(feed.push(id, frames(10, 1, 4410, 7), Some(1_001_000)));
        assert_eq!(p.next_pcm(&mut out, Duration::from_millis(1)), Pcm::Samples { rate: 44100 });
        assert_eq!(out[0], 7);
        feed.close();
        assert_eq!(p.next_pcm(&mut out, Duration::from_millis(1)), Pcm::Ended);
        assert_eq!(p.decode_errors, 0);
    }

    #[test]
    fn a_reset_plays_out_the_old_stream_first() {
        let feed = Feed::with_start(0.3);
        let mut p = FeedPlayer::new(feed.clone());
        let mut out = Vec::new();
        let a = feed.begin_stream(info());
        feed.push(a, frames(0, 4, 4410, 1), Some(0));
        assert_eq!(p.next_pcm(&mut out, Duration::ZERO), Pcm::Samples { rate: 44100 });
        // Reset: a new stream whose frame numbers start at 0 again.
        let b = feed.begin_stream(info());
        assert!(!feed.push(a, frames(4, 1, 4410, 1), Some(0)), "the old stream takes no more data");
        feed.push(b, frames(0, 2, 4410, 2), Some(50_000));
        let mut seen = Vec::new();
        for _ in 0..3 {
            assert_eq!(p.next_pcm(&mut out, Duration::ZERO), Pcm::Samples { rate: 44100 });
            seen.push(out[0]);
        }
        assert_eq!(seen, vec![1, 1, 1], "the old stream plays out");
        // The new stream has 0.2 s < 0.3 s: it waits for its own prebuffer.
        assert_eq!(p.next_pcm(&mut out, Duration::ZERO), Pcm::Buffering);
        feed.push(b, frames(2, 2, 4410, 2), Some(50_200));
        assert_eq!(p.next_pcm(&mut out, Duration::ZERO), Pcm::Samples { rate: 44100 });
        assert_eq!(out[0], 2);
        assert_eq!(feed.now_wall_ms(), Some(50_100));
        feed.stop();
        assert_eq!(p.next_pcm(&mut out, Duration::ZERO), Pcm::Ended);
    }

    #[test]
    fn a_damaged_frame_is_skipped() {
        let feed = Feed::with_start(0.0);
        let mut p = FeedPlayer::new(feed.clone());
        let id = feed.begin_stream(info());
        let mut fs = frames(0, 3, 4410, 9);
        let n = fs[1].bytes.len();
        fs[1].bytes[n / 2] ^= 0x55;
        feed.push(id, fs, Some(0));
        let mut out = Vec::new();
        assert_eq!(p.next_pcm(&mut out, Duration::ZERO), Pcm::Samples { rate: 44100 });
        assert_eq!(p.next_pcm(&mut out, Duration::ZERO), Pcm::Samples { rate: 44100 });
        assert!(p.decode_errors <= 1);
    }
}
