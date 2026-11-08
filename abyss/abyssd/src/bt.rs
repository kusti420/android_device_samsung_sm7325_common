//! Bluetooth headphones: abyssd's side of `abyss-btd`.
//!
//! abyss-btd (packages/modules/Bluetooth/abyss, installed as /apex/com.android.bt/bin/abyss-btd)
//! runs Android's Bluetooth stack on its own, without the framework. abyssd switches it on and
//! off through `sys.abyss.bt=on|off` (init starts/stops the service, abyss.rc) and talks to it
//! over the init-created SOCK_SEQPACKET socket /dev/socket/abyss_bt. Protocol: abyss-design
//! reports/abyss-bt-protocol.md (byte 0 = 0x00 JSON control/event, 0x01 s16le stereo PCM).
//!
//! Split:
//! - `Bt` (main thread): turn on/off, the paired list, connect/disconnect, status lines, the
//!   last device (out/bt_last.json). Events arrive from a reader thread through a channel plus
//!   one byte on a socket pair that the main loop polls.
//! - `Shared` (main thread + player thread): the socket for sending, the PCM format the helper
//!   wants and the A2DP stream state, with a condvar so `audio::BtOut::open` can wait for
//!   "audio started".
//!
//! Bluetooth is OFF by default every session (owner, DECISIONS "Added 21:00").

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use log::{info, warn};
use serde_json::{json, Value};

use crate::{clock, control, library};

/// init starts abyss-btd on `on` (while Abyss is active) and stops it on `off`.
pub const PROP_BT: &str = "sys.abyss.bt";
const SOCKET: &str = "/dev/socket/abyss_bt";
const LAST_FILE: &str = "/data/misc/abyss/out/bt_last.json";

/// Socket connect attempts after `sys.abyss.bt=on`.
const CONNECT_RETRY_MS: u64 = 200;
const CONNECT_GIVE_UP_MS: u64 = 10_000;
/// From `sys.abyss.bt=on` to the helper's `state on`.
const TURN_ON_TIMEOUT_MS: u64 = 20_000;
/// From `connect` to `conn connected` (the stack's own page timeout is shorter).
const CONNECT_TIMEOUT_MS: u64 = 30_000;
/// From `off` to the hangup; then init is told to stop the helper anyway.
const TURN_OFF_TIMEOUT_MS: u64 = 6_000;
/// PCM waiting in the socket (≈ 0.2-0.3 s at 48 kHz): keeps pause latency low.
const SNDBUF_BYTES: libc::c_int = 32 * 1024;
/// A blocked PCM send longer than this = the helper is stuck.
const PCM_SEND_TIMEOUT_MS: u64 = 3_000;
/// How long `BtOut::open` waits for "audio started".
const START_TIMEOUT_MS: u64 = 4_000;
/// Frames per PCM packet: 20 ms at 48 kHz.
pub const PCM_FRAMES_PER_PACKET: usize = 960;

pub const PKT_JSON: u8 = 0x00;
pub const PKT_PCM: u8 = 0x01;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub addr: String,
    pub name: String,
}

/// The A2DP stream as the helper last reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Audio {
    Suspended,
    /// `play` sent, no answer yet.
    Starting,
    Started,
    Failed(String),
}

/// State the player thread needs (behind `Shared::m`).
pub struct LinkState {
    /// The connected socket (None = no helper link).
    pub fd: Option<Arc<OwnedFd>>,
    /// Bumped on every new link: a BtOut opened on an older link must reopen.
    pub link_gen: u64,
    /// Rate of the last `format` event (0 = none yet) and a counter bumped on every one.
    pub rate: u32,
    pub fmt_gen: u64,
    pub audio: Audio,
    /// The headphones A2DP is connected to.
    pub connected: Option<Device>,
}

pub struct Shared {
    pub m: Mutex<LinkState>,
    pub cv: Condvar,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            m: Mutex::new(LinkState {
                fd: None,
                link_gen: 0,
                rate: 0,
                fmt_gen: 0,
                audio: Audio::Suspended,
                connected: None,
            }),
            cv: Condvar::new(),
        }
    }

    /// Sends `play` and waits for "audio started" (or failure / timeout). Returns the PCM rate
    /// the helper wants, the format and link generations to watch, and the socket.
    pub fn start_stream(&self) -> Result<(u32, u64, u64, Arc<OwnedFd>)> {
        let fd = {
            let mut s = self.m.lock().map_err(|_| anyhow!("bt state poisoned"))?;
            if s.connected.is_none() {
                return Err(anyhow!("Bluetooth headphones not connected"));
            }
            let fd = s.fd.clone().ok_or_else(|| anyhow!("Bluetooth is off"))?;
            s.audio = Audio::Starting;
            fd
        };
        send_json_blocking(&fd, &json!({"cmd": "play"}))?;
        let deadline = clock::now_ms() + START_TIMEOUT_MS;
        let mut s = self.m.lock().map_err(|_| anyhow!("bt state poisoned"))?;
        loop {
            match &s.audio {
                Audio::Started => break,
                Audio::Failed(r) => return Err(anyhow!("Bluetooth audio did not start: {r}")),
                _ => {}
            }
            if s.fd.is_none() || s.connected.is_none() {
                return Err(anyhow!("Bluetooth went away"));
            }
            let now = clock::now_ms();
            if now >= deadline {
                s.audio = Audio::Suspended;
                return Err(anyhow!("Bluetooth audio did not start within {} s", START_TIMEOUT_MS / 1000));
            }
            s = self
                .cv
                .wait_timeout(s, Duration::from_millis(deadline - now))
                .map_err(|_| anyhow!("bt state poisoned"))?
                .0;
        }
        let rate = if s.rate > 0 {
            s.rate
        } else {
            warn!("bt: audio started without a format event, assuming 44100 Hz");
            44100
        };
        Ok((rate, s.fmt_gen, s.link_gen, fd))
    }

    /// Whether a stream opened at (`fmt_gen`, `link_gen`) may keep writing:
    /// Ok(true) = yes, Ok(false) = reopen (new format / suspended), Err = link gone.
    pub fn still_valid(&self, fmt_gen: u64, link_gen: u64) -> Result<bool> {
        let s = self.m.lock().map_err(|_| anyhow!("bt state poisoned"))?;
        if s.link_gen != link_gen || s.fd.is_none() || s.connected.is_none() {
            return Err(anyhow!("Bluetooth headphones disconnected"));
        }
        Ok(s.fmt_gen == fmt_gen && s.audio == Audio::Started)
    }

    /// `pause` from the player thread (standby). Never blocks long.
    pub fn suspend_stream(&self) {
        let fd = match self.m.lock() {
            Ok(mut s) => {
                if s.audio != Audio::Suspended {
                    s.audio = Audio::Suspended;
                }
                s.fd.clone()
            }
            Err(_) => None,
        };
        if let Some(fd) = fd {
            if let Err(e) = send_json(&fd, &json!({"cmd": "pause"})) {
                warn!("bt: pause: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Socket helpers (raw libc: SOCK_SEQPACKET is not in std).

fn connect_seqpacket(path: &str) -> std::io::Result<OwnedFd> {
    // SAFETY: plain socket(2); the fd is owned right below.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh fd owned by nobody else.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: zeroed sockaddr_un is a valid value; sun_path is filled below within bounds.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(std::io::Error::new(ErrorKind::InvalidInput, "socket path too long"));
    }
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: `addr` is a valid sockaddr_un of at least `len` bytes.
    let r = unsafe { libc::connect(fd.as_raw_fd(), &addr as *const _ as *const libc::sockaddr, len) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let sndbuf = SNDBUF_BYTES;
    let tv = libc::timeval {
        tv_sec: (PCM_SEND_TIMEOUT_MS / 1000) as libc::time_t,
        tv_usec: ((PCM_SEND_TIMEOUT_MS % 1000) * 1000) as libc::suseconds_t,
    };
    // SAFETY: valid option values of the sizes given, on an open socket.
    unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &sndbuf as *const _ as *const libc::c_void,
            std::mem::size_of_val(&sndbuf) as libc::socklen_t,
        );
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of_val(&tv) as libc::socklen_t,
        );
    }
    Ok(fd)
}

fn send_raw(fd: &OwnedFd, data: &[u8], flags: libc::c_int) -> std::io::Result<()> {
    loop {
        // SAFETY: `data` is valid for the call; MSG_NOSIGNAL: a dead peer is an error, not SIGPIPE.
        let r = unsafe {
            libc::send(fd.as_raw_fd(), data.as_ptr() as *const libc::c_void, data.len(), flags | libc::MSG_NOSIGNAL)
        };
        if r >= 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    }
}

/// Blocking send of one PCM packet (paced by the helper's backpressure, 3 s timeout).
pub fn send_pcm(fd: &OwnedFd, packet: &[u8]) -> Result<()> {
    send_raw(fd, packet, 0).map_err(|e| {
        if e.kind() == ErrorKind::WouldBlock {
            anyhow!("Bluetooth stalled (no audio taken for {} s)", PCM_SEND_TIMEOUT_MS / 1000)
        } else {
            anyhow!("Bluetooth send: {e}")
        }
    })
}

fn packet_json(v: &Value) -> Vec<u8> {
    let mut p = vec![PKT_JSON];
    p.extend(v.to_string().as_bytes());
    p
}

/// Blocking JSON send (player thread).
fn send_json_blocking(fd: &OwnedFd, v: &Value) -> Result<()> {
    send_raw(fd, &packet_json(v), 0).map_err(|e| anyhow!("Bluetooth send: {e}"))
}

/// JSON send that never blocks the UI for more than ~300 ms (the socket may be full of PCM).
fn send_json(fd: &OwnedFd, v: &Value) -> std::io::Result<()> {
    let p = packet_json(v);
    for _ in 0..2 {
        match send_raw(fd, &p, libc::MSG_DONTWAIT) {
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                let mut pfd = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
                // SAFETY: one valid pollfd for the call.
                unsafe { libc::poll(&mut pfd, 1, 150) };
            }
            r => return r,
        }
    }
    Err(std::io::Error::new(ErrorKind::WouldBlock, "socket full"))
}

// ---------------------------------------------------------------------------
// Reader thread.

enum Ev {
    Json(Value),
    Hangup,
}

fn spawn_reader(fd: Arc<OwnedFd>, gen: u64, shared: Arc<Shared>, tx: Sender<(u64, Ev)>, mut poke: UnixStream) {
    let r = std::thread::Builder::new().name("abyssd-bt".into()).spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            // SAFETY: `buf` is valid for `buf.len()` bytes.
            let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
            if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if n <= 0 {
                if let Ok(mut s) = shared.m.lock() {
                    if s.link_gen == gen {
                        s.fd = None;
                        s.connected = None;
                        s.audio = Audio::Suspended;
                    }
                }
                shared.cv.notify_all();
                let _ = tx.send((gen, Ev::Hangup));
                let _ = poke.write(&[1]);
                return;
            }
            let pkt = &buf[..n as usize];
            if pkt[0] != PKT_JSON {
                continue; // the helper sends no PCM
            }
            let Ok(v) = serde_json::from_slice::<Value>(pkt[1..].trim_ascii_end()) else {
                warn!("bt: bad event {:?}", String::from_utf8_lossy(&pkt[1..]));
                continue;
            };
            update_shared(&shared, gen, &v);
            if tx.send((gen, Ev::Json(v))).is_err() {
                return;
            }
            let _ = poke.write(&[1]);
        }
    });
    if let Err(e) = r {
        warn!("bt reader thread: {e}");
    }
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn device_of(v: &Value) -> Device {
    let addr = str_of(v, "addr").to_string();
    let name = match str_of(v, "name") {
        "" => addr.clone(),
        n => n.to_string(),
    };
    Device { addr, name }
}

/// The player-relevant part of an event, applied on the reader thread so a waiting
/// `start_stream` sees it without a main-loop round trip.
fn update_shared(shared: &Shared, gen: u64, v: &Value) {
    let Ok(mut s) = shared.m.lock() else { return };
    if s.link_gen != gen {
        return;
    }
    match str_of(v, "ev") {
        "format" => {
            s.rate = v.get("rate").and_then(|r| r.as_u64()).unwrap_or(0) as u32;
            s.fmt_gen += 1;
        }
        "audio" => {
            s.audio = match str_of(v, "state") {
                "started" => Audio::Started,
                "failed" => Audio::Failed(str_of(v, "reason").to_string()),
                _ => Audio::Suspended,
            }
        }
        "conn" => match str_of(v, "state") {
            "connected" => s.connected = Some(device_of(v)),
            "disconnected" => {
                s.connected = None;
                s.audio = Audio::Suspended;
            }
            _ => {}
        },
        "state" if str_of(v, "state") != "on" => {
            s.connected = None;
            s.audio = Audio::Suspended;
        }
        _ => {}
    }
    drop(s);
    shared.cv.notify_all();
}

// ---------------------------------------------------------------------------
// Main-thread side.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Off,
    /// `sys.abyss.bt=on` set, waiting for the socket and the helper's `state on`.
    Starting,
    On,
    /// `off` sent, waiting for the helper to exit.
    Stopping,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conn {
    None,
    Connecting(Device),
    Connected(Device),
}

/// What the daemon has to act on after Bluetooth events.
#[derive(Debug)]
pub enum Note {
    /// Connected / disconnected / helper gone: re-pick the output.
    Output,
    /// An AVRCP key from the headphones.
    Key(String),
    /// The headset changed its absolute volume (0..127).
    Volume(u32),
    /// A feedback line (also shown outside the Bluetooth screen).
    Say(String, bool),
}

pub struct Bt {
    pub shared: Arc<Shared>,
    phase: Phase,
    phase_at: u64,
    conn: Conn,
    conn_at: u64,
    devices: Vec<Device>,
    last: Option<Device>,
    /// The status line on the Bluetooth screen (text, warning?).
    line: Option<(String, bool)>,
    /// Connect to this as soon as the helper is on.
    want: Option<Device>,
    sock: Option<Arc<OwnedFd>>,
    next_try: Option<u64>,
    gen: u64,
    rx: Receiver<(u64, Ev)>,
    tx: Sender<(u64, Ev)>,
    poke_r: UnixStream,
    poke_w: UnixStream,
}

impl Bt {
    pub fn new() -> Result<Bt> {
        let (poke_r, poke_w) = UnixStream::pair()?;
        poke_r.set_nonblocking(true)?;
        poke_w.set_nonblocking(true)?;
        let (tx, rx) = channel();
        // Off by default every session; a stale "on" (e.g. abyssd restarted) is reset.
        if control::prop_str(PROP_BT).as_deref() == Some("on") {
            control::set(PROP_BT, "off");
        }
        Ok(Bt {
            shared: Arc::new(Shared::new()),
            phase: Phase::Off,
            phase_at: 0,
            conn: Conn::None,
            conn_at: 0,
            devices: Vec::new(),
            last: load_last(),
            line: None,
            want: None,
            sock: None,
            next_try: None,
            gen: 0,
            rx,
            tx,
            poke_r,
            poke_w,
        })
    }

    pub fn poll_fd(&self) -> BorrowedFd<'_> {
        self.poke_r.as_fd()
    }

    // ---- read-only views for the UI ----

    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn conn(&self) -> &Conn {
        &self.conn
    }
    pub fn devices(&self) -> &[Device] {
        &self.devices
    }
    pub fn last(&self) -> Option<&Device> {
        self.last.as_ref()
    }
    pub fn line(&self) -> Option<(&str, bool)> {
        self.line.as_ref().map(|(s, w)| (s.as_str(), *w))
    }
    pub fn connected(&self) -> Option<&Device> {
        match &self.conn {
            Conn::Connected(d) if self.phase == Phase::On => Some(d),
            _ => None,
        }
    }
    /// Turning on/off or connecting: something will happen by itself shortly.
    pub fn busy(&self) -> bool {
        matches!(self.phase, Phase::Starting | Phase::Stopping) || matches!(self.conn, Conn::Connecting(_))
    }
    /// One word for the Home row.
    pub fn summary(&self) -> String {
        match (self.phase, &self.conn) {
            (Phase::Off, _) => "off".into(),
            (Phase::Starting, _) => "turning on…".into(),
            (Phase::Stopping, _) => "turning off…".into(),
            (Phase::On, Conn::Connected(d)) => d.name.clone(),
            (Phase::On, Conn::Connecting(_)) => "connecting…".into(),
            (Phase::On, Conn::None) => "on".into(),
        }
    }

    fn set_line(&mut self, s: impl Into<String>, warn: bool) {
        self.line = Some((s.into(), warn));
    }

    // ---- actions ----

    pub fn turn_on(&mut self, now: u64) {
        if self.phase != Phase::Off {
            return;
        }
        info!("bt: turning on");
        control::set(PROP_BT, "on");
        self.phase = Phase::Starting;
        self.phase_at = now;
        self.conn = Conn::None;
        self.set_line("Turning on…", false);
        self.next_try = Some(now + CONNECT_RETRY_MS);
    }

    pub fn turn_off(&mut self, now: u64) {
        match self.phase {
            Phase::Off | Phase::Stopping => return,
            _ => {}
        }
        info!("bt: turning off");
        self.want = None;
        if let Some(fd) = self.sock.clone() {
            if let Err(e) = send_json(&fd, &json!({"cmd": "off"})) {
                warn!("bt: off: {e}");
                self.finish_off("Bluetooth is off.", false);
                return;
            }
            self.phase = Phase::Stopping;
            self.phase_at = now;
            self.set_line("Turning off…", false);
        } else {
            self.finish_off("Bluetooth is off.", false);
        }
    }

    /// `None` = the last device.
    pub fn connect(&mut self, dev: Option<Device>, now: u64) {
        let Some(dev) = dev.or_else(|| self.last.clone()) else { return };
        match self.phase {
            Phase::Off => {
                self.want = Some(dev);
                self.turn_on(now);
            }
            Phase::Starting => self.want = Some(dev),
            Phase::Stopping => {}
            Phase::On => self.send_connect(dev, now),
        }
    }

    pub fn device(&self, i: usize) -> Option<Device> {
        self.devices.get(i).cloned()
    }

    fn send_connect(&mut self, dev: Device, now: u64) {
        info!("bt: connect {} ({})", dev.name, dev.addr);
        self.send(json!({"cmd": "connect", "addr": dev.addr}));
        self.set_line(format!("Connecting to {}…", dev.name), false);
        self.conn = Conn::Connecting(dev);
        self.conn_at = now;
    }

    pub fn disconnect(&mut self) {
        info!("bt: disconnect");
        self.want = None;
        self.send(json!({"cmd": "disconnect"}));
    }

    pub fn refresh(&mut self) {
        self.send(json!({"cmd": "list"}));
    }

    /// abyssd owns the volume: keep the headset at full scale (protocol "Volume policy").
    pub fn send_full_volume(&mut self) {
        self.send(json!({"cmd": "volume", "abs": 127}));
    }

    /// Play state for the headset's AVRCP view.
    pub fn send_playstate(&mut self, state: &str, title: &str, pos_ms: u64, dur_ms: Option<u64>) {
        if self.connected().is_none() {
            return;
        }
        self.send(json!({
            "cmd": "playstate", "state": state, "title": title,
            "pos_ms": pos_ms, "dur_ms": dur_ms.unwrap_or(0),
        }));
    }

    fn send(&mut self, v: Value) {
        if let Some(fd) = self.sock.clone() {
            if let Err(e) = send_json(&fd, &v) {
                warn!("bt: send {v}: {e}");
            }
        }
    }

    /// Helper gone or switched off: clean up and tell init to stop it.
    fn finish_off(&mut self, line: &str, warn: bool) {
        self.close_link();
        control::set(PROP_BT, "off");
        self.phase = Phase::Off;
        self.conn = Conn::None;
        self.devices.clear();
        self.want = None;
        self.next_try = None;
        self.set_line(line, warn);
    }

    fn close_link(&mut self) {
        if let Some(fd) = self.sock.take() {
            // Wakes the reader thread (recv returns 0); it then drops its handle.
            // SAFETY: shutdown on an fd we still hold open.
            unsafe { libc::shutdown(fd.as_raw_fd(), libc::SHUT_RDWR) };
        }
        if let Ok(mut s) = self.shared.m.lock() {
            s.fd = None;
            s.connected = None;
            s.audio = Audio::Suspended;
            s.link_gen += 1;
        }
        self.shared.cv.notify_all();
        self.gen += 1;
    }

    fn try_link(&mut self, now: u64) {
        match connect_seqpacket(SOCKET) {
            Ok(fd) => {
                let fd = Arc::new(fd);
                self.gen += 1;
                let gen = self.gen;
                if let Ok(mut s) = self.shared.m.lock() {
                    s.fd = Some(fd.clone());
                    s.link_gen = gen;
                    s.connected = None;
                    s.audio = Audio::Suspended;
                    s.rate = 0;
                }
                match self.poke_w.try_clone() {
                    Ok(p) => spawn_reader(fd.clone(), gen, self.shared.clone(), self.tx.clone(), p),
                    Err(e) => warn!("bt: poke clone: {e}"),
                }
                self.sock = Some(fd);
                self.next_try = None;
                info!("bt: linked to abyss-btd after {} ms", now.saturating_sub(self.phase_at));
            }
            Err(e) if now.saturating_sub(self.phase_at) < CONNECT_GIVE_UP_MS => {
                if now.saturating_sub(self.phase_at) < 2 * CONNECT_RETRY_MS {
                    info!("bt: helper socket not ready yet ({e})");
                }
                self.next_try = Some(now + CONNECT_RETRY_MS);
            }
            Err(e) => {
                warn!("bt: no helper socket after {} s: {e}", CONNECT_GIVE_UP_MS / 1000);
                self.finish_off("Couldn't turn on Bluetooth (helper did not start).", true);
            }
        }
    }

    pub fn next_deadline(&self) -> Option<u64> {
        let mut t = self.next_try;
        let mut min = |x: u64| t = Some(t.map_or(x, |y: u64| y.min(x)));
        match self.phase {
            Phase::Starting => min(self.phase_at + TURN_ON_TIMEOUT_MS),
            Phase::Stopping => min(self.phase_at + TURN_OFF_TIMEOUT_MS),
            _ => {}
        }
        if matches!(self.conn, Conn::Connecting(_)) {
            min(self.conn_at + CONNECT_TIMEOUT_MS);
        }
        t
    }

    /// Timers: socket retries and the turn on / connect / turn off timeouts.
    pub fn tick(&mut self, now: u64) -> Vec<Note> {
        let mut notes = Vec::new();
        if self.next_try.is_some_and(|t| now >= t) && self.phase == Phase::Starting {
            self.try_link(now);
            if self.phase == Phase::Off {
                notes.push(Note::Say("Couldn't turn on Bluetooth.".into(), true));
            }
        }
        match self.phase {
            Phase::Starting if now >= self.phase_at + TURN_ON_TIMEOUT_MS => {
                warn!("bt: helper not on after {} s", TURN_ON_TIMEOUT_MS / 1000);
                // Ask it to stop cleanly; init stops it in any case.
                self.send(json!({"cmd": "off"}));
                self.finish_off("Couldn't turn on Bluetooth (no answer).", true);
                notes.push(Note::Say("Couldn't turn on Bluetooth.".into(), true));
            }
            Phase::Stopping if now >= self.phase_at + TURN_OFF_TIMEOUT_MS => {
                warn!("bt: helper still there {} s after off; init stops it", TURN_OFF_TIMEOUT_MS / 1000);
                self.finish_off("Bluetooth is off.", false);
                notes.push(Note::Output);
            }
            _ => {}
        }
        if let Conn::Connecting(d) = &self.conn {
            if now >= self.conn_at + CONNECT_TIMEOUT_MS {
                let name = d.name.clone();
                warn!("bt: connect to {name} timed out");
                self.send(json!({"cmd": "disconnect"}));
                self.conn = Conn::None;
                self.set_line(format!("Couldn't connect: {name} did not answer."), true);
            }
        }
        notes
    }

    /// The reader thread poked the main loop: handle every queued event.
    pub fn on_ready(&mut self, now: u64) -> Vec<Note> {
        let mut b = [0u8; 64];
        while matches!(self.poke_r.read(&mut b), Ok(n) if n > 0) {}
        let mut notes = Vec::new();
        while let Ok((gen, ev)) = self.rx.try_recv() {
            if gen != self.gen {
                continue; // from a link we already closed
            }
            match ev {
                Ev::Hangup => self.on_hangup(&mut notes),
                Ev::Json(v) => self.on_event(&v, now, &mut notes),
            }
        }
        notes
    }

    fn on_hangup(&mut self, notes: &mut Vec<Note>) {
        let was_connected = self.connected().is_some();
        if self.phase == Phase::Stopping {
            info!("bt: helper exited (off)");
            self.finish_off("Bluetooth is off.", false);
        } else {
            warn!("bt: helper hung up");
            self.finish_off("Bluetooth stopped.", true);
            notes.push(Note::Say("Bluetooth stopped.".into(), true));
        }
        if was_connected {
            notes.push(Note::Output);
        }
    }

    fn on_event(&mut self, v: &Value, now: u64, notes: &mut Vec<Note>) {
        match str_of(v, "ev") {
            "state" => {
                let st = str_of(v, "state");
                info!("bt: helper state {st}");
                match st {
                    "on" if self.phase == Phase::Starting => {
                        self.phase = Phase::On;
                        self.set_line("Bluetooth is on.", false);
                        self.send(json!({"cmd": "list"}));
                        if let Some(d) = self.want.take() {
                            // The helper may already have reconnected to it on its own; a
                            // `conn connected` then follows and wins.
                            if self.connected().is_none() {
                                self.send_connect(d, now);
                            }
                        }
                    }
                    "off" if self.phase != Phase::Stopping => {
                        let r = str_of(v, "reason");
                        let line = if r.is_empty() {
                            "Bluetooth stopped.".to_string()
                        } else {
                            format!("Couldn't turn on Bluetooth: {r}")
                        };
                        let was = self.connected().is_some();
                        self.finish_off(&line, true);
                        notes.push(Note::Say(line, true));
                        if was {
                            notes.push(Note::Output);
                        }
                    }
                    _ => {}
                }
            }
            "devices" => {
                self.devices = v
                    .get("devices")
                    .and_then(|d| d.as_array())
                    .map(|a| a.iter().map(device_of).filter(|d| !d.addr.is_empty()).collect())
                    .unwrap_or_default();
                info!("bt: {} paired device(s)", self.devices.len());
                // Keep the remembered name fresh (the headset may have been renamed).
                if let Some(l) = self.last.as_mut() {
                    if let Some(d) = self.devices.iter().find(|d| d.addr == l.addr) {
                        l.name = d.name.clone();
                    }
                }
            }
            "conn" => {
                let d = device_of(v);
                let d = self.devices.iter().find(|x| x.addr == d.addr).cloned().unwrap_or(d);
                match str_of(v, "state") {
                    "connecting" => {
                        if !matches!(self.conn, Conn::Connecting(_)) {
                            self.set_line(format!("Connecting to {}…", d.name), false);
                            self.conn = Conn::Connecting(d);
                            self.conn_at = now;
                        }
                    }
                    "connected" => {
                        info!("bt: connected {} ({})", d.name, d.addr);
                        self.set_line(format!("Connected · {}", d.name), false);
                        save_last(&d);
                        self.last = Some(d.clone());
                        self.conn = Conn::Connected(d);
                        self.send_full_volume();
                        notes.push(Note::Output);
                    }
                    "disconnected" => {
                        let reason = str_of(v, "reason").to_string();
                        info!("bt: disconnected {} ({reason})", d.name);
                        let was_connected = matches!(&self.conn, Conn::Connected(x) if x.addr == d.addr);
                        let was_connecting = matches!(&self.conn, Conn::Connecting(x) if x.addr == d.addr);
                        if was_connecting {
                            let r = if reason.is_empty() { "no answer".to_string() } else { reason };
                            self.set_line(format!("Couldn't connect: {r}"), true);
                            self.conn = Conn::None;
                        } else if was_connected {
                            let line = if reason == "user" {
                                format!("Disconnected · {}", d.name)
                            } else {
                                format!("{} disconnected: paused.", d.name)
                            };
                            self.set_line(line.clone(), reason != "user");
                            self.conn = Conn::None;
                            notes.push(Note::Say(line, reason != "user"));
                            notes.push(Note::Output);
                        }
                    }
                    _ => {}
                }
            }
            "key" => notes.push(Note::Key(str_of(v, "key").to_string())),
            "volume" => {
                if let Some(a) = v.get("abs").and_then(|a| a.as_u64()) {
                    notes.push(Note::Volume(a.min(127) as u32));
                }
            }
            // format / audio: handled for the player in update_shared.
            _ => {}
        }
    }
}

fn load_last() -> Option<Device> {
    let text = fs::read_to_string(LAST_FILE).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let d = device_of(&v);
    (!d.addr.is_empty()).then_some(d)
}

/// Atomic replace, like the resume state.
fn save_last(d: &Device) {
    let r = (|| -> std::io::Result<()> {
        fs::create_dir_all(library::STATE_DIR)?;
        let tmp = format!("{LAST_FILE}.tmp");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(json!({"addr": d.addr, "name": d.name}).to_string().as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, LAST_FILE)
    })();
    if let Err(e) = r {
        warn!("bt: save last device: {e}");
    }
}
