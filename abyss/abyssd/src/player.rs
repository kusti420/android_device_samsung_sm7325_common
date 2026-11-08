//! Playback thread. Two paths:
//! - DSP (jack, or Bluetooth with A2DP offload; FLAC <= 48 kHz / MP3): symphonia
//!   only DEMUXES; the encoded frames go to the ADSP through `CompressOut` in large
//!   fragments, and the thread sleeps in poll() until the DSP has used a fragment
//!   up. While it sleeps there `offload_sleepable` is set, so abyssd may suspend
//!   the AP. For Bluetooth the ADSP also encodes (a2dp_dsp.rs).
//! - CPU decode (USB DAC, Bluetooth in software mode, other codecs, > 48 kHz,
//!   streams, or offload failed): symphonia decode → i16 stereo → AudioOut.
//!
//! The vendored `symphonia` umbrella crate is built without codec features,
//! so the FLAC and MP3 bundles are registered here by hand.
//!
//! The main loop sends `Cmd`s over a channel and reads `Status` from a mutex.
//! State/track changes are signalled by one byte on a socket pair so the
//! single-threaded event loop wakes up (position updates are not signalled;
//! the UI reads them when it redraws).

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{anyhow, Context, Result};
use log::{info, warn};
use symphonia_core::audio::SampleBuffer;
use symphonia_core::codecs::{
    CodecRegistry, CodecType, Decoder, DecoderOptions, CODEC_TYPE_FLAC, CODEC_TYPE_MP3, CODEC_TYPE_NULL,
};
use symphonia_core::errors::Error as SymError;
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_core::meta::MetadataOptions;
use symphonia_core::probe::{Hint, Probe};
use symphonia_core::units::{Time, TimeBase};

use crate::audio::{
    AudioOut, BtOut, CompressOut, CompressStream, FlacInfo, OffloadCodec, OffloadFormat, Output,
    UsbOut, Wait, COMPRESS_VOL_UNITY,
};
use crate::stream::{Feed, FeedPlayer, Pcm};
use crate::{bt, clock};

/// The same output back within this time after an unplug pause: resume by itself.
const REPLUG_RESUME_MS: u64 = 5 * 60 * 1000;
/// "Previous" restarts the current track when we are past this point.
const PREV_RESTART_MS: u64 = 3000;
/// A paused DSP stream is closed (route powered down) after this long; play
/// then reopens the track at the paused position.
const OFFLOAD_PAUSE_STANDBY_MS: u64 = 30_000;
/// Upper bound for one poll() while the DSP plays (a non-wakeup timeout: it
/// never wakes a suspended AP, it only guards against a lost interrupt).
const OFFLOAD_POLL_MS: i32 = 20_000;
/// Bytes demuxed per refill batch.
const OFFLOAD_BATCH: usize = 128 * 1024;
/// After this many mid-track DSP failures the session stays on CPU decode.
const OFFLOAD_MAX_FAILURES: u32 = 2;

#[derive(Debug)]
pub enum Cmd {
    /// Replace the queue and start at `index`, `start_ms` into the track.
    Load {
        queue: Vec<PathBuf>,
        index: usize,
        start_ms: u64,
        play: bool,
    },
    /// Resume playback without reloading (Resume on the loaded track).
    Play,
    /// Pause (AVRCP pause/stop from Bluetooth headphones).
    Pause,
    Toggle,
    Next,
    Prev,
    /// 0..=100 (the caller applies the ceiling).
    Volume(u32),
    /// The output abyssd picked (Bluetooth > USB DAC > jack), or None = nothing.
    Output(Option<Output>),
    /// The vendor audio HAL restarted and reset the mixer.
    HalRestarted,
    /// Play a network stream (stream.rs: the radio, a module stream) instead of the queue.
    /// CPU decode; hold ▲/▼ do nothing; pause just stops taking audio from it.
    Stream(Arc<Feed>),
    Quit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlayState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub state: PlayState,
    pub track: Option<PathBuf>,
    pub index: usize,
    pub queue_len: usize,
    pub pos_ms: u64,
    pub dur_ms: Option<u64>,
    pub rate: u32,
    pub error: Option<String>,
    pub backend: &'static str,
    /// Paused because nothing is plugged in (or play was asked without output).
    pub no_output: bool,
    /// The next (up to) two tracks of the queue, for "Up next".
    pub next: Vec<PathBuf>,
    /// "FLAC" / "MP3" ("" before a track is open).
    pub codec: &'static str,
    /// Bits per sample of the file (0 = not meaningful, e.g. MP3).
    pub bits: u32,
    /// The ADSP decodes the current track (else the CPU does).
    pub dsp: bool,
    /// The source is a network stream (Cmd::Stream), not the queue.
    pub stream: bool,
}

/// Where the UI reads the live DSP position from (Player::status).
struct PosSrc {
    stream: Arc<CompressStream>,
    base_ms: u64,
    rate: u32,
    dur_ms: Option<u64>,
}

fn pos_from(p: &PosSrc) -> Option<u64> {
    let (frames, rate) = p.stream.tstamp()?;
    let rate = if rate > 0 { rate } else { p.rate.max(1) };
    let ms = p.base_ms + frames as u64 * 1000 / rate as u64;
    Some(p.dur_ms.map_or(ms, |d| ms.min(d)))
}

pub struct Player {
    tx: Sender<Cmd>,
    /// One byte per command: wakes the player thread out of its DSP poll().
    wake: UnixStream,
    pos: Arc<Mutex<Option<PosSrc>>>,
    sleepable: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
    thread: Option<JoinHandle<()>>,
    /// Set by the thread when it has stopped audio and is about to exit.
    done: Arc<AtomicBool>,
}

impl Player {
    pub fn spawn(
        out: Box<dyn AudioOut>,
        notify: UnixStream,
        volume: u32,
        output: Option<Output>,
        bt: Arc<bt::Shared>,
    ) -> Result<Player> {
        let (tx, rx) = channel();
        let status = Arc::new(Mutex::new(Status { backend: out.name(), ..Default::default() }));
        let st = status.clone();
        let done = Arc::new(AtomicBool::new(false));
        let done_t = done.clone();
        let (wake_w, wake_r) = UnixStream::pair().context("wake socketpair")?;
        let _ = wake_w.set_nonblocking(true);
        let _ = wake_r.set_nonblocking(true);
        let wake_drain = wake_w.try_clone().context("wake clone")?;
        let pos = Arc::new(Mutex::new(None));
        let sleepable = Arc::new(AtomicBool::new(false));
        let (pos_t, sleep_t) = (pos.clone(), sleepable.clone());
        let thread = std::thread::Builder::new()
            .name("abyssd-player".into())
            .spawn(move || {
                let mut e = Engine::new(rx, st, notify, out, volume, bt);
                e.wake_r = Some(wake_r);
                e.wake_w = Some(wake_drain);
                e.pos_src = pos_t;
                e.sleepable = sleep_t;
                e.set_output(output);
                e.run();
                e.close_offload();
                e.comp.standby();
                e.out.standby();
                done_t.store(true, Ordering::SeqCst);
            })
            .context("spawn player thread")?;
        Ok(Player { tx, wake: wake_w, pos, sleepable, status, thread: Some(thread), done })
    }

    pub fn send(&self, cmd: Cmd) {
        if self.tx.send(cmd).is_err() {
            warn!("player thread is gone");
        }
        // Full socket buffer = a wake is already pending; that is fine.
        let _ = (&self.wake).write(&[1u8]);
    }

    /// The status; while the DSP plays, `pos_ms` is read live from the
    /// compress stream (no thread has to wake for position updates).
    pub fn status(&self) -> Status {
        let mut s = self.status.lock().map(|s| s.clone()).unwrap_or_default();
        if let Ok(p) = self.pos.lock() {
            if let Some(ms) = p.as_ref().and_then(pos_from) {
                s.pos_ms = ms;
            }
        }
        s
    }

    /// True while the player thread only waits for the DSP (DSP playback, the
    /// ring full or draining): the AP may suspend; the DSP's write-done
    /// interrupt (glink/IPCC, IRQF_NO_SUSPEND + pm_system_wakeup in
    /// drivers/rpmsg/qcom_glink_native.c) wakes it for the next fragment.
    pub fn offload_sleepable(&self) -> bool {
        self.sleepable.load(Ordering::SeqCst)
    }

    /// Stops audio and joins the thread, waiting at most `max_ms` (init
    /// SIGKILLs 200 ms after SIGTERM). A thread stuck in a blocking write or
    /// the audio-settle wait is left behind; process exit ends it.
    pub fn shutdown(&mut self, max_ms: u64) {
        let _ = self.tx.send(Cmd::Quit);
        let until = clock::now_ms() + max_ms;
        while !self.done.load(Ordering::SeqCst) && clock::now_ms() < until {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if self.done.load(Ordering::SeqCst) {
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        } else {
            warn!("player thread did not stop within {max_ms} ms");
        }
    }
}

/// An open, decoding track.
struct Decoding {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    rate: u32,
    out_rate: u32,
    resampler: Resampler,
    sbuf: Option<SampleBuffer<i16>>,
}

/// A track playing through the DSP.
struct OffTrack {
    format: Box<dyn FormatReader>,
    track_id: u32,
    fmt: OffloadFormat,
    /// Track position (ms) of the first byte handed to this stream.
    base_ms: u64,
    dur_ms: Option<u64>,
    stream: Option<Arc<CompressStream>>,
    started: bool,
    paused_at: Option<u64>,
    pending: Vec<u8>,
    pend_off: usize,
    eof: bool,
    drain: Option<Drain>,
}

struct Drain {
    thread: JoinHandle<Result<()>>,
    done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl OffTrack {
    fn pos_ms(&self) -> Option<u64> {
        let s = self.stream.as_ref()?;
        if !self.started {
            return Some(self.base_ms);
        }
        pos_from(&PosSrc { stream: s.clone(), base_ms: self.base_ms, rate: self.fmt.rate, dur_ms: self.dur_ms })
    }
}

struct Engine {
    rx: Receiver<Cmd>,
    status: Arc<Mutex<Status>>,
    notify: UnixStream,
    out: Box<dyn AudioOut>,
    codecs: CodecRegistry,
    probe: Probe,
    queue: Vec<PathBuf>,
    index: usize,
    dec: Option<Decoding>,
    playing: bool,
    volume: u32,
    /// The output in use (None = nothing plugged in).
    output: Option<Output>,
    /// What `out` currently drives.
    out_kind: Option<Output>,
    /// The jack backend while `out` is the USB or Bluetooth one (keeps its route state).
    jack_spare: Option<Box<dyn AudioOut>>,
    /// abyss-btd link state, for `BtOut`.
    bt: Arc<bt::Shared>,
    /// Set when an unplug paused playback (for the 5 min auto-resume): when,
    /// and which output must come back (None = any, after "play" with nothing).
    unplug_paused_at: Option<(u64, Option<Output>)>,
    pos_ms: u64,
    stereo: Vec<i16>,
    resampled: Vec<i16>,
    /// The jack through the ADSP decoder (route + volume control).
    comp: CompressOut,
    /// The track when the DSP decodes it (then `dec` is None).
    off: Option<OffTrack>,
    offload_enabled: bool,
    offload_failures: u32,
    /// Nothing is open but the track must continue at `pos_ms` (after an
    /// unplug, a long pause or an offload failure).
    reopen: bool,
    wake_r: Option<UnixStream>,
    wake_w: Option<UnixStream>,
    pos_src: Arc<Mutex<Option<PosSrc>>>,
    sleepable: Arc<AtomicBool>,
    /// A network stream being played (Cmd::Stream), its decoded PCM, and its output rate.
    feed: Option<FeedPlayer>,
    feed_pcm: Vec<i16>,
    feed_rate: u32,
    feed_out_rate: u32,
    feed_resampler: Resampler,
}

impl Engine {
    fn new(
        rx: Receiver<Cmd>,
        status: Arc<Mutex<Status>>,
        notify: UnixStream,
        out: Box<dyn AudioOut>,
        volume: u32,
        bt: Arc<bt::Shared>,
    ) -> Engine {
        let mut codecs = CodecRegistry::new();
        codecs.register_all::<symphonia_bundle_flac::FlacDecoder>();
        codecs.register_all::<symphonia_bundle_mp3::MpaDecoder>();
        let mut probe = Probe::default();
        probe.register_all::<symphonia_bundle_flac::FlacReader>();
        probe.register_all::<symphonia_bundle_mp3::MpaReader>();
        let _ = notify.set_nonblocking(true);
        Engine {
            rx,
            status,
            notify,
            out,
            codecs,
            probe,
            queue: Vec::new(),
            index: 0,
            dec: None,
            playing: false,
            volume,
            output: None,
            out_kind: Some(Output::Jack),
            jack_spare: None,
            bt,
            unplug_paused_at: None,
            pos_ms: 0,
            stereo: Vec::new(),
            resampled: Vec::new(),
            comp: CompressOut::from_props(),
            off: None,
            offload_enabled: crate::control::prop_bool("persist.abyss.audio.offload", true),
            offload_failures: 0,
            reopen: false,
            wake_r: None,
            wake_w: None,
            pos_src: Arc::new(Mutex::new(None)),
            sleepable: Arc::new(AtomicBool::new(false)),
            feed: None,
            feed_pcm: Vec::new(),
            feed_rate: 0,
            feed_out_rate: 0,
            feed_resampler: Resampler::new(44100, 44100),
        }
    }

    fn loaded(&self) -> bool {
        self.dec.is_some() || self.off.is_some() || self.feed.is_some()
    }

    /// The current position, live from the DSP when it plays.
    fn current_pos(&self) -> u64 {
        self.off.as_ref().and_then(|t| t.pos_ms()).unwrap_or(self.pos_ms)
    }

    /// Empties the wake socket (bytes written by Player::send / the drain thread).
    fn drain_wake(&mut self) {
        if let Some(w) = self.wake_r.as_mut() {
            let mut buf = [0u8; 64];
            while matches!(w.read(&mut buf), Ok(n) if n > 0) {}
        }
    }

    /// Closes the DSP stream of the current track (the route stays up for the
    /// next track; `comp.standby()` powers it down).
    fn close_offload(&mut self) {
        if let Ok(mut p) = self.pos_src.lock() {
            *p = None;
        }
        self.sleepable.store(false, Ordering::SeqCst);
        let Some(mut t) = self.off.take() else { return };
        if let Some(d) = t.drain.take() {
            d.stop.store(true, Ordering::SeqCst);
            if let Some(s) = t.stream.as_ref() {
                let _ = s.stop(); // interrupts the blocked DRAIN
            }
            let _ = d.thread.join();
        }
        // Dropping the last Arc closes the fd (the kernel stops the session).
        t.stream = None;
    }

    fn run(&mut self) {
        loop {
            if self.playing && self.off.is_some() {
                self.drain_wake();
                loop {
                    match self.rx.try_recv() {
                        Ok(cmd) => {
                            if !self.handle(cmd) {
                                return;
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    }
                }
                if !self.playing || self.off.is_none() {
                    continue;
                }
                match self.step_offload() {
                    Ok(true) => {}
                    Ok(false) => self.advance(),
                    Err(e) => self.offload_failed(e),
                }
            } else if self.playing && self.dec.is_some() {
                // Drain commands between packets.
                loop {
                    match self.rx.try_recv() {
                        Ok(cmd) => {
                            if !self.handle(cmd) {
                                return;
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    }
                }
                if !self.playing || self.dec.is_none() {
                    continue;
                }
                match self.step() {
                    Ok(true) => {}
                    Ok(false) => self.advance(),
                    Err(e) => {
                        warn!("playback error: {e:#}");
                        self.set_error(Some(format!("{e}")));
                        if self.out.is_open() {
                            self.advance(); // decode problem: skip the track
                        } else {
                            // Output problem (e.g. the USB DAC was pulled):
                            // stop trying; the same output back = play on.
                            self.pause();
                            self.unplug_paused_at = Some((clock::now_ms(), self.output.clone()));
                            self.publish(true);
                        }
                    }
                }
            } else if self.playing && self.feed.is_some() {
                loop {
                    match self.rx.try_recv() {
                        Ok(cmd) => {
                            if !self.handle(cmd) {
                                return;
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    }
                }
                if !self.playing || self.feed.is_none() {
                    continue;
                }
                match self.step_feed() {
                    Ok(true) => {}
                    Ok(false) => {
                        info!("stream ended");
                        self.feed = None;
                        self.playing = false;
                        self.out.standby();
                        self.publish(true);
                    }
                    Err(e) => {
                        // Output problem (a USB DAC pulled): pause; the stream is kept.
                        warn!("stream output: {e:#}");
                        self.set_error(Some(format!("{e}")));
                        self.pause();
                        self.unplug_paused_at = Some((clock::now_ms(), self.output.clone()));
                        self.publish(true);
                    }
                }
            } else {
                if self.playing && !self.loaded() && self.reopen && !self.queue.is_empty() {
                    // e.g. an offload failure: continue on the CPU path.
                    self.open_current(self.pos_ms);
                    continue;
                }
                if self.out.is_open() {
                    // Every stream goes to standby when idle (an open idle
                    // output costs ≈ 10-20 mA).
                    self.out.standby();
                }
                self.drain_wake();
                // A paused DSP stream stays open for a short while (instant
                // resume), then it is closed and the route powered down.
                let paused_at = self.off.as_ref().and_then(|t| t.paused_at);
                let got = match paused_at {
                    Some(at) => {
                        let left = (at + OFFLOAD_PAUSE_STANDBY_MS).saturating_sub(clock::now_ms());
                        match self.rx.recv_timeout(std::time::Duration::from_millis(left.max(1))) {
                            Ok(c) => Some(c),
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                self.offload_standby();
                                continue;
                            }
                            Err(_) => return,
                        }
                    }
                    None => {
                        if self.off.as_ref().is_some_and(|t| t.stream.is_some()) {
                            // Stopped but not paused at the DSP (e.g. PAUSE
                            // failed): never leave the stream open idle.
                            self.offload_standby();
                        }
                        self.comp.standby();
                        match self.rx.recv() {
                            Ok(c) => Some(c),
                            Err(_) => return,
                        }
                    }
                };
                if let Some(cmd) = got {
                    if !self.handle(cmd) {
                        return;
                    }
                }
            }
        }
    }

    /// Returns false on Quit.
    fn handle(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Stream(f) => {
                self.dec = None;
                self.close_offload();
                self.comp.standby();
                self.out.standby();
                self.queue.clear();
                self.index = 0;
                self.reopen = false;
                self.unplug_paused_at = None;
                self.feed = Some(FeedPlayer::new(f));
                self.feed_rate = 0;
                self.pos_ms = 0;
                if let Ok(mut s) = self.status.lock() {
                    s.dur_ms = None;
                    s.codec = "STREAM";
                    s.bits = 0;
                    s.dsp = false;
                }
                info!("source: network stream (cpu decode)");
                self.play();
            }
            Cmd::Next | Cmd::Prev if self.feed.is_some() => {}
            Cmd::Load { queue, index, start_ms, play } => {
                self.feed = None;
                self.queue = queue;
                self.index = index.min(self.queue.len().saturating_sub(1));
                self.unplug_paused_at = None;
                self.open_current(start_ms);
                if play {
                    self.play();
                } else {
                    self.pause();
                }
            }
            Cmd::Play => self.play(),
            Cmd::Pause => self.pause(),
            Cmd::Toggle => {
                if self.playing {
                    self.pause()
                } else {
                    self.play()
                }
            }
            Cmd::Next => {
                if self.index + 1 < self.queue.len() {
                    self.index += 1;
                    self.open_current(0);
                }
            }
            Cmd::Prev => {
                self.pos_ms = self.current_pos();
                if self.pos_ms > PREV_RESTART_MS || self.index == 0 {
                    self.open_current(0);
                } else {
                    self.index -= 1;
                    self.open_current(0);
                }
            }
            Cmd::Volume(v) => {
                self.volume = v.min(200);
                if self.comp.is_routed() {
                    if let Err(e) = self.comp.set_volume(gain_q13(self.volume)) {
                        // Never keep playing at a level we cannot change.
                        self.offload_failed(e);
                    }
                }
            }
            Cmd::Output(o) => self.set_output(o),
            Cmd::HalRestarted => {
                info!("audio HAL restarted: re-applying the route on next write");
                self.out.hal_restarted();
                if let Some(j) = self.jack_spare.as_mut() {
                    j.hal_restarted();
                }
                // The mixer reset unrouted MultiMedia4: reopen the DSP stream
                // where it was, with a fresh route.
                let had = self.off.is_some();
                let pos = self.current_pos();
                self.close_offload();
                self.comp.hal_restarted();
                if had {
                    self.open_current(pos);
                }
            }
            Cmd::Quit => return false,
        }
        self.publish(true);
        true
    }

    /// Switches the output. Unplug = pause at once (never the speaker); the
    /// same output back within 5 min = play on; a new output while playing
    /// (e.g. a USB DAC plugged in, Bluetooth headphones connected) takes over
    /// without a pause. Bluetooth lost = pause too, even when the jack or a USB
    /// DAC is there to fall back to (it is selected, paused).
    fn set_output(&mut self, o: Option<Output>) {
        if o == self.output {
            return;
        }
        info!("output: {:?} -> {:?}", self.output.as_ref().map(|x| x.label()), o.as_ref().map(|x| x.label()));
        // Close whatever plays; the track continues at `pos` on the path that
        // fits the new output (DSP on the jack, CPU decode on USB).
        let had = self.loaded() && self.feed.is_none();
        self.feed_rate = 0; // a stream reopens the new output on its next write
        let pos = self.current_pos();
        self.close_offload();
        self.comp.standby();
        self.out.standby();
        if had {
            self.dec = None;
            self.pos_ms = pos;
            self.reopen = true;
        }
        let old = std::mem::replace(&mut self.output, o.clone());
        // The DSP path follows the output: the jack, or the Bluetooth encoder.
        self.comp.set_bluetooth(match &o {
            Some(Output::Bluetooth { .. }) => Some(self.bt.clone()),
            _ => None,
        });
        let Some(new) = o else {
            if self.playing {
                self.pause();
                self.unplug_paused_at = Some((clock::now_ms(), old));
                info!("output gone: paused");
            }
            self.publish(true);
            return;
        };
        // Point `out` at the right backend.
        if self.out_kind.as_ref() != Some(&new) {
            let fresh: Option<Box<dyn AudioOut>> = match &new {
                Output::Usb { card, .. } => Some(Box::new(UsbOut::new(*card))),
                Output::Bluetooth { .. } => Some(Box::new(BtOut::new(self.bt.clone()))),
                Output::Jack => None,
            };
            match fresh {
                Some(f) => {
                    let prev = std::mem::replace(&mut self.out, f);
                    if matches!(self.out_kind, Some(Output::Jack)) {
                        self.jack_spare = Some(prev);
                    }
                }
                None => {
                    if let Some(j) = self.jack_spare.take() {
                        self.out = j;
                    }
                }
            }
            self.out_kind = Some(new.clone());
        }
        // Bluetooth headphones went away (out of range, switched off, helper
        // stopped): pause, never jump to the wire. Their return within 5 min
        // plays on, like a replug.
        let bt_lost = matches!(old, Some(Output::Bluetooth { .. }))
            && !matches!(new, Output::Bluetooth { .. });
        if bt_lost {
            if self.playing {
                self.pause();
                self.unplug_paused_at = Some((clock::now_ms(), old));
                info!("Bluetooth gone: paused ({} selected)", new.label());
            }
            if had {
                self.open_current(pos);
            }
            self.publish(true);
            return;
        }
        if had {
            self.open_current(pos);
        }
        if let Some((t, want)) = self.unplug_paused_at.take() {
            let same = want.as_ref().is_none_or(|w| w.same(&new));
            if same && clock::now_ms().saturating_sub(t) <= REPLUG_RESUME_MS {
                info!("{} back within 5 min: playing on", new.label());
                self.play();
            }
        }
        self.publish(true);
    }

    fn play(&mut self) {
        if self.output.is_none() {
            // Never pretend to play: stay paused and say why (Now playing
            // shows "No headphones..."); plugging in within 5 min plays.
            self.playing = false;
            self.unplug_paused_at = Some((clock::now_ms(), None));
            self.publish(true);
            return;
        }
        if !self.loaded() && !self.queue.is_empty() {
            let at = if self.reopen { self.pos_ms } else { 0 };
            self.open_current(at);
        }
        self.playing = self.loaded();
        if let Some(t) = self.off.as_mut() {
            if t.paused_at.take().is_some() {
                if let Some(s) = t.stream.as_ref() {
                    if let Err(e) = s.resume() {
                        warn!("{e:#}");
                    }
                }
            }
        }
        if self.playing {
            self.set_error(None);
        }
    }

    fn pause(&mut self) {
        self.playing = false;
        let pos = self.current_pos();
        self.pos_ms = pos;
        if let Some(t) = self.off.as_mut() {
            if t.started && t.paused_at.is_none() {
                if let Some(s) = t.stream.as_ref() {
                    match s.pause() {
                        Ok(()) => t.paused_at = Some(clock::now_ms()),
                        Err(e) => warn!("{e:#}"),
                    }
                }
            }
        }
        self.sleepable.store(false, Ordering::SeqCst);
    }

    /// Long pause: close the DSP stream, power the route down; play reopens.
    fn offload_standby(&mut self) {
        self.pos_ms = self.current_pos();
        info!("offload: paused {} s, closing the stream at {} ms", OFFLOAD_PAUSE_STANDBY_MS / 1000, self.pos_ms);
        self.close_offload();
        self.comp.standby();
        self.reopen = true;
        self.publish(false);
    }

    /// A DSP stream failed mid-track: continue on the CPU path from here.
    fn offload_failed(&mut self, e: anyhow::Error) {
        self.offload_failures += 1;
        let pos = self.current_pos();
        warn!("offload: failed at {pos} ms ({e:#}); cpu decode from here");
        if self.offload_failures >= OFFLOAD_MAX_FAILURES && self.offload_enabled {
            warn!("offload: {} failures, cpu decode for the rest of the session", self.offload_failures);
            self.offload_enabled = false;
        }
        self.close_offload();
        self.comp.standby();
        let was = self.offload_enabled;
        self.offload_enabled = false;
        self.open_current(pos);
        self.offload_enabled = was;
        self.publish(true);
    }

    /// End of track: go to the next one, or stop at the end of the queue.
    fn advance(&mut self) {
        if self.index + 1 < self.queue.len() {
            self.index += 1;
            self.open_current(0);
        } else {
            info!("end of queue");
            self.dec = None;
            self.close_offload();
            self.comp.standby();
            self.playing = false;
            self.pos_ms = 0;
        }
        self.publish(true);
    }

    fn open_current(&mut self, start_ms: u64) {
        self.dec = None;
        self.close_offload();
        self.reopen = false;
        self.pos_ms = start_ms;
        let Some(path) = self.queue.get(self.index).cloned() else { return };
        let dsp_output = match self.output {
            Some(Output::Jack) => true,
            Some(Output::Bluetooth { .. }) => self.bt.offload(),
            _ => false,
        };
        if self.offload_enabled && dsp_output {
            match self.open_offload(&path, start_ms) {
                Ok(t) => {
                    // Only one front end feeds the jack: close the PCM one.
                    self.out.standby();
                    info!("track {}/{} (dsp): {}", self.index + 1, self.queue.len(), path.display());
                    self.pos_ms = t.base_ms;
                    self.off = Some(t);
                    return;
                }
                Err(e) => info!("offload: not used ({e:#}): cpu decode"),
            }
        } else if matches!(self.output, Some(Output::Usb { .. })) {
            info!("offload: not used (USB DAC output): cpu decode");
        } else if matches!(self.output, Some(Output::Bluetooth { .. })) && self.offload_enabled {
            info!("offload: not used (Bluetooth software encoding): cpu decode");
        } else if !self.offload_enabled {
            info!("offload: not used (disabled for this session): cpu decode");
        }
        self.comp.standby();
        match self.open(&path, start_ms) {
            Ok(d) => {
                info!("track {}/{}: {}", self.index + 1, self.queue.len(), path.display());
                self.dec = Some(d);
            }
            Err(e) => {
                warn!("cannot open {}: {e:#}", path.display());
                self.set_error(Some(format!("Cannot play {}", path.display())));
            }
        }
    }

    fn probe_file(&self, path: &PathBuf) -> Result<Box<dyn FormatReader>> {
        let file = open_track(path)?;
        let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let probed = self
            .probe
            .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
            .context("probe")?;
        Ok(probed.format)
    }

    /// Opens `path` for the DSP, or says why not (the caller falls back).
    fn open_offload(&mut self, path: &PathBuf, start_ms: u64) -> Result<OffTrack> {
        let mut format = self.probe_file(path)?;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| anyhow!("no audio track"))?;
        let track_id = track.id;
        let params = track.codec_params.clone();
        let rate = params.sample_rate.ok_or_else(|| anyhow!("unknown sample rate"))?;
        let channels = params.channels.map(|c| c.count() as u32).unwrap_or(2);
        let (codec, bits, flac) = match params.codec {
            c if c == CODEC_TYPE_FLAC => {
                let info = params
                    .extra_data
                    .as_deref()
                    .and_then(FlacInfo::from_streaminfo)
                    .ok_or_else(|| anyhow!("flac without STREAMINFO"))?;
                (OffloadCodec::Flac, info.bits, info)
            }
            c if c == CODEC_TYPE_MP3 => (OffloadCodec::Mp3, 16, FlacInfo::default()),
            c => return Err(anyhow!("codec {} not decoded by the DSP", codec_name(c))),
        };
        if rate > 48000 {
            return Err(anyhow!("{} Hz > 48 kHz", rate));
        }
        if !(1..=2).contains(&channels) {
            return Err(anyhow!("{channels} channels"));
        }
        if codec == OffloadCodec::Flac && bits != 16 && bits != 24 {
            return Err(anyhow!("{bits}-bit flac"));
        }
        let time_base = params.time_base;
        let dur_ms = match (time_base, params.n_frames) {
            (Some(tb), Some(n)) => Some(time_ms(tb.calc_time(n))),
            _ => None,
        };
        let mut base_ms = 0;
        if start_ms > 0 {
            let secs = start_ms as f64 / 1000.0;
            match format.seek(
                SeekMode::Coarse,
                SeekTo::Time { time: Time::from(secs), track_id: Some(track_id) },
            ) {
                Ok(to) => {
                    base_ms = time_base.map_or(start_ms, |tb| time_ms(tb.calc_time(to.actual_ts)));
                    info!("resumed at {base_ms} ms (asked {start_ms})");
                }
                Err(e) => warn!("seek to {start_ms} ms failed: {e}"),
            }
        }
        if let Ok(mut s) = self.status.lock() {
            s.dur_ms = dur_ms;
            s.rate = rate;
            s.codec = codec_name(params.codec);
            s.bits = if codec == OffloadCodec::Flac { bits } else { 0 };
            s.dsp = true;
        }
        Ok(OffTrack {
            format,
            track_id,
            fmt: OffloadFormat { codec, rate, channels, bits, flac },
            base_ms,
            dur_ms,
            stream: None,
            started: false,
            paused_at: None,
            pending: Vec::with_capacity(OFFLOAD_BATCH + 64 * 1024),
            pend_off: 0,
            eof: false,
            drain: None,
        })
    }

    /// Feeds the DSP: fills the ring, starts the stream, then sleeps until the
    /// DSP frees a fragment or a command arrives. Ok(false) = track done.
    fn step_offload(&mut self) -> Result<bool> {
        let vol = gain_q13(self.volume);
        let t = self.off.as_mut().expect("step_offload without track");
        if t.stream.is_none() {
            let s = self.comp.open(&t.fmt, vol)?;
            t.stream = Some(s);
            t.started = false;
        }
        let stream = t.stream.clone().expect("stream");
        let wake_fd = self.wake_r.as_ref().map(|w| w.as_raw_fd()).unwrap_or(-1);

        // Everything is handed over: wait for the drain thread.
        if let Some(d) = t.drain.as_ref() {
            if d.done.load(Ordering::SeqCst) {
                let d = t.drain.take().expect("drain");
                match d.thread.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => warn!("offload: {e:#}"),
                    Err(_) => warn!("offload: drain thread panicked"),
                }
                return Ok(false);
            }
            self.sleepable.store(true, Ordering::SeqCst);
            let mut fds = [libc::pollfd { fd: wake_fd, events: libc::POLLIN, revents: 0 }];
            // SAFETY: one valid pollfd for the call.
            unsafe { libc::poll(fds.as_mut_ptr(), 1, OFFLOAD_POLL_MS) };
            self.sleepable.store(false, Ordering::SeqCst);
            return Ok(true);
        }

        // Fill the ring with demuxed (still encoded) frames.
        let mut wrote = false;
        loop {
            if t.pend_off >= t.pending.len() {
                if t.eof {
                    break;
                }
                t.pending.clear();
                t.pend_off = 0;
                while t.pending.len() < OFFLOAD_BATCH {
                    match t.format.next_packet() {
                        Ok(p) => {
                            if p.track_id() == t.track_id {
                                t.pending.extend_from_slice(&p.data);
                            }
                        }
                        Err(SymError::IoError(e)) if e.kind() == ErrorKind::UnexpectedEof => {
                            t.eof = true;
                            break;
                        }
                        Err(SymError::ResetRequired) => {
                            t.eof = true;
                            break;
                        }
                        Err(SymError::DecodeError(e)) => {
                            warn!("demux error (skipping): {e}");
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                continue;
            }
            let n = stream.write(&t.pending[t.pend_off..])?;
            t.pend_off += n;
            wrote |= n > 0;
            if n == 0 || t.pend_off < t.pending.len() {
                break; // ring full
            }
        }
        let handed_all = t.eof && t.pend_off >= t.pending.len();
        if !t.started {
            stream.start()?;
            t.started = true;
            if let Ok(mut p) = self.pos_src.lock() {
                *p = Some(PosSrc {
                    stream: stream.clone(),
                    base_ms: t.base_ms,
                    rate: t.fmt.rate,
                    dur_ms: t.dur_ms,
                });
            }
        }
        if handed_all {
            // DRAIN blocks until the DSP has played the rest; it runs in its
            // own thread so commands (pause, next) still get through.
            let done = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let (s2, d2, st2) = (stream.clone(), done.clone(), stop.clone());
            let mut w = self.wake_w.as_ref().and_then(|w| w.try_clone().ok());
            let thread = std::thread::Builder::new()
                .name("abyssd-drain".into())
                .spawn(move || {
                    let r = s2.drain(&st2);
                    d2.store(true, Ordering::SeqCst);
                    if let Some(w) = w.as_mut() {
                        let _ = w.write(&[1u8]);
                    }
                    r
                })
                .context("spawn drain thread")?;
            t.drain = Some(Drain { thread, done, stop });
            return Ok(true);
        }
        if wrote {
            // Tell the main loop the refill is done (it may suspend now).
            self.sleepable.store(true, Ordering::SeqCst);
            self.publish(true);
        }
        self.sleepable.store(true, Ordering::SeqCst);
        let r = stream.wait(wake_fd, OFFLOAD_POLL_MS);
        self.sleepable.store(false, Ordering::SeqCst);
        match r? {
            Wait::Writable | Wait::Woken | Wait::Timeout => {}
        }
        if let Some(p) = self.off.as_ref().and_then(|t| t.pos_ms()) {
            self.pos_ms = p;
        }
        Ok(true)
    }

    fn open(&mut self, path: &PathBuf, start_ms: u64) -> Result<Decoding> {
        let file = open_track(path)?;
        let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let probed = self
            .probe
            .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
            .context("probe")?;
        let mut format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| anyhow!("no audio track"))?;
        let track_id = track.id;
        let params = track.codec_params.clone();
        let rate = params.sample_rate.ok_or_else(|| anyhow!("unknown sample rate"))?;
        let decoder = self.codecs.make(&params, &DecoderOptions::default()).context("decoder")?;
        let time_base = params.time_base;
        let dur_ms = match (time_base, params.n_frames) {
            (Some(tb), Some(n)) => Some(time_ms(tb.calc_time(n))),
            _ => None,
        };
        if start_ms > 0 {
            let secs = start_ms as f64 / 1000.0;
            match format.seek(
                SeekMode::Coarse,
                SeekTo::Time { time: Time::from(secs), track_id: Some(track_id) },
            ) {
                Ok(_) => info!("resumed at {start_ms} ms"),
                Err(e) => warn!("seek to {start_ms} ms failed: {e}"),
            }
        }
        if let Ok(mut s) = self.status.lock() {
            s.dur_ms = dur_ms;
            s.rate = rate;
            s.codec = codec_name(params.codec);
            s.bits = if params.codec == CODEC_TYPE_FLAC { params.bits_per_sample.unwrap_or(0) } else { 0 };
            s.dsp = false;
        }
        Ok(Decoding {
            format,
            decoder,
            track_id,
            time_base,
            rate,
            out_rate: 0,
            resampler: Resampler::new(rate, rate),
            sbuf: None,
        })
    }

    /// Decodes and plays one packet. Ok(false) = end of track.
    fn step(&mut self) -> Result<bool> {
        let dec = self.dec.as_mut().expect("step without decoder");
        if !self.out.is_open() {
            let r = self.out.open(dec.rate)?;
            dec.out_rate = r;
            dec.resampler = Resampler::new(dec.rate, r);
        }
        let packet = match dec.format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == ErrorKind::UnexpectedEof => return Ok(false),
            Err(SymError::ResetRequired) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != dec.track_id {
            return Ok(true);
        }
        if let Some(tb) = dec.time_base {
            self.pos_ms = time_ms(tb.calc_time(packet.ts()));
        }
        let decoded = match dec.decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(e)) => {
                warn!("decode error (skipping packet): {e}");
                return Ok(true);
            }
            Err(e) => return Err(e.into()),
        };
        let spec = *decoded.spec();
        let frames = decoded.frames();
        if frames == 0 {
            return Ok(true);
        }
        let need = decoded.capacity() as u64;
        // SampleBuffer::capacity() counts samples (frames × channels).
        if dec.sbuf.as_ref().is_none_or(|b| b.capacity() < need as usize * spec.channels.count()) {
            dec.sbuf = Some(SampleBuffer::<i16>::new(need, spec));
        }
        let sbuf = dec.sbuf.as_mut().expect("sample buffer");
        sbuf.copy_interleaved_ref(decoded);
        let ch = spec.channels.count().max(1);

        // To stereo with software gain (Q15).
        let gain = gain_q15(self.volume);
        self.stereo.clear();
        for f in sbuf.samples().chunks_exact(ch) {
            let (l, r) = if ch == 1 { (f[0], f[0]) } else { (f[0], f[1]) };
            self.stereo.push(((l as i32 * gain) >> 15) as i16);
            self.stereo.push(((r as i32 * gain) >> 15) as i16);
        }
        if dec.out_rate != dec.rate {
            self.resampled.clear();
            dec.resampler.process(&self.stereo, &mut self.resampled);
            self.out.write(&self.resampled)?;
        } else {
            self.out.write(&self.stereo)?;
        }
        if let Ok(mut s) = self.status.lock() {
            s.pos_ms = self.pos_ms;
        }
        Ok(true)
    }

    /// Decodes one frame of the stream and plays it. Ok(false) = the stream ended.
    fn step_feed(&mut self) -> Result<bool> {
        let Some(fp) = self.feed.as_mut() else { return Ok(false) };
        let rate = match fp.next_pcm(&mut self.feed_pcm, std::time::Duration::from_millis(200)) {
            Pcm::Samples { rate } => rate,
            // Prebuffering or a stall: wait (commands are handled between tries).
            Pcm::Buffering | Pcm::Stalled => return Ok(true),
            Pcm::Ended => return Ok(false),
        };
        if !self.out.is_open() || self.feed_rate != rate {
            self.feed_out_rate = self.out.open(rate)?;
            self.feed_rate = rate;
            self.feed_resampler = Resampler::new(rate, self.feed_out_rate);
        }
        let gain = gain_q15(self.volume);
        self.stereo.clear();
        self.stereo.extend(self.feed_pcm.iter().map(|&x| ((x as i32 * gain) >> 15) as i16));
        if self.feed_out_rate != rate {
            self.resampled.clear();
            self.feed_resampler.process(&self.stereo, &mut self.resampled);
            self.out.write(&self.resampled)?;
        } else {
            self.out.write(&self.stereo)?;
        }
        self.pos_ms += (self.feed_pcm.len() / 2) as u64 * 1000 / u64::from(rate.max(1));
        if let Ok(mut s) = self.status.lock() {
            s.pos_ms = self.pos_ms;
        }
        Ok(true)
    }

    fn set_error(&mut self, e: Option<String>) {
        if let Ok(mut s) = self.status.lock() {
            s.error = e;
        }
        self.publish(true);
    }

    /// Updates the shared status; `wake` also pokes the main loop.
    fn publish(&mut self, wake: bool) {
        if let Ok(mut s) = self.status.lock() {
            s.state = match (self.dec.is_some() || self.off.is_some() || self.feed.is_some(), self.playing) {
                (_, true) => PlayState::Playing,
                (true, false) => PlayState::Paused,
                (false, false) if !self.queue.is_empty() => PlayState::Paused,
                _ => PlayState::Stopped,
            };
            s.track = self.queue.get(self.index).cloned();
            s.index = self.index;
            s.queue_len = self.queue.len();
            s.pos_ms = self.pos_ms;
            s.no_output = self.output.is_none();
            if self.off.is_none() && self.dec.is_none() && self.feed.is_none() && !self.reopen {
                s.codec = "";
                s.dsp = false;
            }
            s.stream = self.feed.is_some();
            s.next = self.queue.iter().skip(self.index + 1).take(2).cloned().collect();
            s.backend = if self.off.is_some() { "dsp-offload" } else { self.out.name() };
        }
        if wake {
            match self.notify.write(&[1u8]) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {} // already pending
                Err(e) => warn!("player notify: {e}"),
            }
        }
    }
}

/// Opens a track and reads the WHOLE file once on a helper thread, so it sits in the page cache
/// (posix_fadvise WILLNEED / readahead(2) would be simpler, but 5.4's
/// force_page_cache_readahead clamps them to one readahead window). A FLAC is 20-60 MB and RAM
/// is plentiful with the framework stopped, so within a second the player reads from RAM only:
/// the SD card and its host controller stay idle for the rest of the track instead of waking for
/// a readahead every second or two. Read errors only cost the prefetch, never playback.
/// A newer open_track call stops an older prefetch (skipping through tracks reads only the last).
static PREFETCH_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn open_track(path: &PathBuf) -> Result<File> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let my_gen = PREFETCH_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let (p, name) = (path.clone(), path.display().to_string());
    let _ = std::thread::Builder::new().name("abyssd-prefetch".into()).spawn(move || {
        // Its own fd: the player's file offset stays untouched.
        let Ok(mut f) = File::open(&p) else { return };
        let t0 = clock::now_ms();
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        loop {
            if PREFETCH_GEN.load(Ordering::SeqCst) != my_gen {
                return;
            }
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => total += n as u64,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    warn!("prefetch {name}: {e}");
                    return;
                }
            }
        }
        info!("prefetch: {} MB in RAM in {} ms", total >> 20, clock::now_ms().saturating_sub(t0));
    });
    Ok(file)
}

fn codec_name(c: CodecType) -> &'static str {
    if c == CODEC_TYPE_FLAC {
        "FLAC"
    } else if c == CODEC_TYPE_MP3 {
        "MP3"
    } else {
        "?"
    }
}

/// Volume step → the DSP stream's Q13 gain (same 72 dB curve as `gain_q15`).
fn gain_q13(volume: u32) -> u32 {
    if volume == 0 {
        return 0;
    }
    ((gain_lin(volume) * COMPRESS_VOL_UNITY as f64).round() as u32).clamp(1, COMPRESS_VOL_UNITY)
}

fn gain_lin(volume: u32) -> f64 {
    let db = -72.0 * (1.0 - volume.min(200) as f64 / 200.0);
    10f64.powf(db / 20.0)
}

fn time_ms(t: Time) -> u64 {
    t.seconds * 1000 + (t.frac * 1000.0) as u64
}

/// Volume step 0..=200 → Q15 gain, linear in dB over 72 dB (200 = 0 dB, 100 = -36 dB,
/// 1 = -71.6 dB). The old 40 dB scale made even the lowest steps loud on the jack.
fn gain_q15(volume: u32) -> i32 {
    if volume == 0 {
        return 0;
    }
    ((gain_lin(volume) * 32767.0) as i32).max(1)
}

/// Linear-interpolation resampler for interleaved stereo i16 (good enough
/// for v1-lite; only used when the PCM refuses the file's own rate).
struct Resampler {
    step: f64,
    pos: f64,
    last: [i16; 2],
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Resampler { step: from as f64 / to.max(1) as f64, pos: 0.0, last: [0, 0] }
    }

    fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        let n = input.len() / 2;
        if n == 0 {
            return;
        }
        // Frame 0 is the last frame of the previous call, frames 1..=n are `input`.
        let frame = |k: usize, c: usize| -> f64 {
            if k == 0 {
                self.last[c] as f64
            } else {
                input[(k - 1) * 2 + c] as f64
            }
        };
        let mut pos = self.pos;
        while pos < n as f64 {
            let i = pos as usize;
            let f = pos - i as f64;
            for c in 0..2 {
                let a = frame(i, c);
                let b = frame(i + 1, c);
                out.push((a + (b - a) * f).round().clamp(-32768.0, 32767.0) as i16);
            }
            pos += self.step;
        }
        self.pos = pos - n as f64;
        self.last = [input[(n - 1) * 2], input[(n - 1) * 2 + 1]];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_length() {
        let mut r = Resampler::new(44100, 48000);
        let input = vec![0i16; 44100 * 2];
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let frames = out.len() / 2;
        assert!((47990..=48010).contains(&frames), "{frames}");
    }

    #[test]
    fn gain_curve() {
        assert_eq!(gain_q15(0), 0);
        assert_eq!(gain_q15(200), 32767);
        // 100 of 200 = -36 dB
        assert!(gain_q15(100) < 530 && gain_q15(100) > 510);
        // the lowest step is ~-71.6 dB, still audible but very quiet
        assert!(gain_q15(1) >= 1 && gain_q15(1) < 10);
        assert_eq!(gain_q13(0), 0);
        assert_eq!(gain_q13(200), 0x2000);
        assert!((120..=135).contains(&gain_q13(100))); // -36 dB of 8192 = 130
        assert!(gain_q13(1) >= 1 && gain_q13(1) <= 3);
    }
}
