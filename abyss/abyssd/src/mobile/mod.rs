//! Mobile data inside Abyss: on-demand leases over the SIM, with abyssd driving the modem itself.
//!
//! Design: abyss-design reports/impl-mobile-data.md. Same lease model as Wi-Fi (wifi/mod.rs):
//! the first lease brings the data call up, the last release lingers, then the call ends and the
//! modem goes back to the low-power mode it was found in (GoDark's airplane mode).
//!
//! The modem is driven with standard QMI messages over QRTR (qrtr.rs, qmi.rs), no rild:
//! 1. `sys.abyss.modem=abyss` → abyss.rc stops vendor.sehradiomanager + ril-daemon (once per
//!    session, at the first lease; rild comes back on leave, before the framework).
//! 2. DMS: operating mode → online.  3. UIM: the data SIM's app is READY (a PIN-locked SIM is
//!    reported, never unlocked here).  4. NAS: registered + PS attached (polled, no
//!    indications).  5. rmnet.rs: rmnet_dataN + its mux id.  6. WDS: bind the mux, start ONE
//!    call on the handed-over APN (handoff.rs), read address/DNS/MTU.  7. abyss-net: netd
//!    network 43 on rmnet_dataN (wifi/netcfg.rs).
//!
//! Only the WDS client stays open while a call is up (it owns the call; its only indication is
//! packet-service-status on change). DMS/UIM/NAS sockets are opened for a query and closed, so
//! the modem has nobody to send indications to.
//!
//! Between uses (radio bursts) the modem can stay **registered** with no call: RRC idle in DRX,
//! no QMI client open, no rmnet network. The next use then needs only the call (~20 ms on the
//! attach APN, whose LTE default bearer the modem keeps) + netd (~7 ms) instead of a ~1 s
//! re-attach from low power. Release policy ([`plan_release`]): the call stays up for a short
//! hinted gap or the uplink's switching window; the modem stays registered for a hinted gap up
//! to `linger_max_s`, or `idle_s` after a use without a hint; then low power.
//!
//! RAT: `persist.abyss.mobile.rat` = `lte` (default: NR left out of the modem's mode
//! preference while Abyss uses it) or `nr` (5G when available). Set until the next modem power
//! cycle only and put back exactly (the value read before) whenever the modem goes back to low
//! power, so rild finds the preference it left.

pub mod handoff;
pub mod probe;
pub mod qmi;
mod qrtr;
mod rmnet;

use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::Ipv4Addr;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use log::{debug, info, warn};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

use crate::wifi::netcfg::NetCfg;
use crate::wifi::Note;
use crate::{clock, control};
use qmi::{SimVerdict, MODE_LOW_POWER, MODE_MODE_ONLY_LOW_POWER, MODE_ONLINE, MODE_PERSISTENT_LOW_POWER};
use qrtr::{Client, Services};

/// on = abyss-net must run for mobile data (abyss.rc); off after the last call.
pub const PROP_MOBILE: &str = "sys.abyss.mobile";
/// rild = Android's RIL owns the modem (default, set by abyss.rc on enter and leave);
/// abyss = abyssd owns it: abyss.rc stops vendor.sehradiomanager + ril-daemon.
pub const PROP_MODEM: &str = "sys.abyss.modem";
const PROP_RILD_STATE: &str = "init.svc.ril-daemon";
/// The call stays up this long after a release without a hint (seconds, default 20).
const PROP_LINGER: &str = "persist.abyss.mobile.linger_s";
/// A hinted next use up to this keeps the modem registered until then (seconds, default 600);
/// further away: low power at once.
const PROP_LINGER_MAX: &str = "persist.abyss.mobile.linger_max_s";
/// A hinted next use up to this keeps the call itself up (seconds, default 30); further away
/// the call ends after the use and the modem waits registered.
const PROP_CALL_LINGER_MAX: &str = "persist.abyss.mobile.call_linger_max_s";
/// After a use without a hint (a station stopped), the modem stays registered this long before
/// low power (seconds, default 180 = about the break-even of idle vs. a re-attach).
const PROP_IDLE: &str = "persist.abyss.mobile.idle_s";
/// "lte" (default) or "nr": the modem's RAT preference while Abyss uses it.
pub const PROP_RAT: &str = "persist.abyss.mobile.rat";
/// 1 = WDS Go Dormant after each release (asks for RRC release; off until measured).
const PROP_DORMANT: &str = "persist.abyss.mobile.dormant";
const LINGER_DEFAULT_S: u64 = 20;
const LINGER_MAX_DEFAULT_S: u64 = 600;
const CALL_LINGER_MAX_DEFAULT_S: u64 = 30;
const IDLE_DEFAULT_S: u64 = 180;
const LINGER_LIMIT_S: u64 = 1800;
const LINGER_SLACK_MS: u64 = 5_000;
/// A failed bring-up while leases are still held is tried again after this long.
const RETRY_MS: u64 = 60_000;
/// Signal/operator refresh while the screen shows them.
const REFRESH_MS: u64 = 10_000;

const RILD_STOP_MS: u64 = 6_000;
const ONLINE_MS: u64 = 4_000;
const SIM_MS: u64 = 10_000;
const REGISTER_MS: u64 = 30_000;
const CALL_MS: u64 = 60_000;
const QMI_MS: u64 = 3_000;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Radio {
    /// "LTE", "5G NSA", "5G SA", "3G", "2G" (qmi::radio_label).
    pub rat: Option<&'static str>,
    pub operator: String,
    pub bars: Option<u8>,
    pub roaming: bool,
}

impl Radio {
    /// "LTE · Telia EE · 3 bars" ("5G NSA · …" in full, "5G · …" short for the status line).
    pub fn line(&self) -> String {
        self.line_with(true)
    }

    fn line_with(&self, full: bool) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(r) = self.rat {
            parts.push(if full || !r.starts_with("5G") { r.into() } else { "5G".into() });
        }
        if !self.operator.is_empty() {
            parts.push(self.operator.clone());
        }
        if self.roaming {
            parts.push("roaming".into());
        }
        if let Some(b) = self.bars {
            parts.push(if b == 1 { "1 bar".into() } else { format!("{b} bars") });
        }
        parts.join(" · ")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Off,
    /// Bringing up; the step for the screen ("starting the modem", "finding the network"…).
    Starting(&'static str),
    On { radio: Radio, ifname: String, ip: Ipv4Addr },
    /// No lease held; the call stays up until `until` (boot ms).
    Lingering { radio: Radio, ifname: String, ip: Ipv4Addr, until: u64 },
    /// No call; the modem stays registered (RRC idle) until `until`, then low power.
    Standby { radio: Radio, until: u64 },
    Stopping,
}

struct Shared {
    phase: Phase,
    /// Leases sent but not yet handled by the worker (state() says Coming meanwhile).
    pending: u32,
    last_error: Option<String>,
    notes: VecDeque<Note>,
    /// Bytes of finished calls this session + the counters at the current call's start.
    used_done: u64,
    call_base: Option<(String, u64)>,
    dns: Vec<Ipv4Addr>,
}

impl Shared {
    fn used(&self) -> u64 {
        let live = match &self.call_base {
            Some((ifname, base)) => {
                let (rx, tx) = rmnet::counters(ifname);
                (rx + tx).saturating_sub(*base)
            }
            None => 0,
        };
        self.used_done + live
    }
}

enum Cmd {
    Acquire { id: u64, holder: String },
    /// `warm_ms`: keep the call up at least this long (the uplink's switching window).
    Release { id: u64, next_use_in_ms: Option<u64>, warm_ms: u64 },
    /// Keep what is up (call or registration) at least until this boot ms, if no lease is held.
    Warm { until: u64 },
    Refresh,
    Shutdown(Sender<()>),
}

#[derive(Clone)]
struct Tx {
    tx: Sender<Cmd>,
    wake: Arc<UnixStream>,
}

impl Tx {
    fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
        let _ = (&*self.wake).write(&[1]);
    }
}

/// A held mobile-data lease. Dropping it releases with the default linger.
pub struct Lease {
    id: u64,
    tx: Tx,
    done: bool,
}

impl Lease {
    #[allow(dead_code)] // the uplink always passes its window (release_warm)
    pub fn release(self, next_use_in_ms: Option<u64>) {
        self.release_warm(next_use_in_ms, 0);
    }

    /// As `release`, but the call stays up at least `warm_ms` (the switching window).
    pub fn release_warm(mut self, next_use_in_ms: Option<u64>, warm_ms: u64) {
        self.tx.send(Cmd::Release { id: self.id, next_use_in_ms, warm_ms });
        self.done = true;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.done {
            self.tx.send(Cmd::Release { id: self.id, next_use_in_ms: None, warm_ms: 0 });
        }
    }
}

/// Coarse state for the uplink (uplink.rs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkState {
    Off,
    Coming,
    /// The call is up on this interface.
    Up(String),
    /// Registered, no call: a lease gets the call in ~30 ms.
    Standby,
    Failed,
}

/// Takes leases from any thread.
#[derive(Clone)]
pub struct Leaser {
    tx: Tx,
    next_id: Arc<AtomicU64>,
    shared: Arc<Mutex<Shared>>,
}

impl Leaser {
    pub fn acquire(&self, holder: &str) -> Lease {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut s) = self.shared.lock() {
            s.pending += 1;
        }
        self.tx.send(Cmd::Acquire { id, holder: holder.to_string() });
        Lease { id, tx: self.tx.clone(), done: false }
    }

    /// Keeps the call / the registration that is up now until at least `until` (boot ms).
    /// Brings nothing up; ignored while a lease is held (its release takes the window).
    pub fn warm(&self, until: u64) {
        self.tx.send(Cmd::Warm { until });
    }

    pub fn state(&self) -> LinkState {
        match self.shared.lock() {
            Ok(s) => match &s.phase {
                Phase::On { ifname, .. } | Phase::Lingering { ifname, .. } => LinkState::Up(ifname.clone()),
                _ if s.pending > 0 => LinkState::Coming,
                Phase::Standby { .. } => LinkState::Standby,
                Phase::Off if s.last_error.is_some() => LinkState::Failed,
                Phase::Off => LinkState::Off,
                Phase::Starting(_) | Phase::Stopping => LinkState::Coming,
            },
            Err(_) => LinkState::Off,
        }
    }

    /// The call's DNS servers (for names on mobile data).
    pub fn dns(&self) -> Vec<Ipv4Addr> {
        self.shared.lock().map(|s| s.dns.clone()).unwrap_or_default()
    }
}

/// For the screens (ui.rs), which only see `View`: the one Mobile of this process.
struct Global {
    shared: Arc<Mutex<Shared>>,
    tx: Tx,
    refresh_at: AtomicU64,
}

static GLOBAL: OnceLock<Global> = OnceLock::new();

fn fmt_bytes(b: u64) -> String {
    match b {
        0..=999 => format!("{b} B"),
        1_000..=999_999 => format!("{:.0} kB", b as f64 / 1e3),
        1_000_000..=99_999_999 => format!("{:.1} MB", b as f64 / 1e6),
        _ => format!("{:.0} MB", b as f64 / 1e6),
    }
}

fn snapshot() -> Option<(Phase, Option<String>, u64)> {
    let g = GLOBAL.get()?;
    let s = g.shared.lock().ok()?;
    Some((s.phase.clone(), s.last_error.clone(), s.used()))
}

/// Asks the worker for fresh signal/operator values (at most every REFRESH_MS, only while up).
fn kick_refresh() {
    let Some(g) = GLOBAL.get() else { return };
    let now = clock::now_ms();
    let at = g.refresh_at.load(Ordering::Relaxed);
    if now >= at {
        g.refresh_at.store(now + REFRESH_MS, Ordering::Relaxed);
        g.tx.send(Cmd::Refresh);
    }
}

/// The status line's network part while mobile data is up or ready: ("LTE·Telia 4▮",
/// carrying data now (a lease is held), data used this session if `with_used`). None = off.
/// Cached values only: it asks the modem nothing (the status line is drawn every redraw).
pub fn status_tag(with_used: bool) -> Option<(String, bool, u64)> {
    let g = GLOBAL.get()?;
    let s = g.shared.lock().ok()?;
    let (radio, active) = match &s.phase {
        Phase::Off | Phase::Stopping => return None,
        Phase::Starting(_) => return Some(("mobile…".into(), true, 0)),
        Phase::On { radio, .. } => (radio.clone(), true),
        Phase::Lingering { radio, .. } | Phase::Standby { radio, .. } => (radio.clone(), false),
    };
    let mut tag = match radio.rat {
        Some(r) if r.starts_with("5G") => "5G".to_string(),
        Some(r) => r.to_string(),
        None => "mobile".to_string(),
    };
    if let Some(op) = radio.operator.split_whitespace().next() {
        tag.push('·');
        tag.push_str(op);
    }
    if let Some(b) = radio.bars {
        tag.push_str(&format!(" {b}▮"));
    }
    if radio.roaming {
        tag.push_str(" R");
    }
    Some((tag, active, if with_used { s.used() } else { 0 }))
}

/// Lets the status line's bars follow the signal while the screen is on (rate-limited).
pub fn refresh_soon() {
    kick_refresh();
}

/// Data used this session, short ("12 MB"), for the status line.
pub fn used_short(bytes: u64) -> String {
    fmt_bytes(bytes)
}

/// One or two lines for the Battery screen.
pub fn detail() -> String {
    let Some((phase, err, used)) = snapshot() else {
        return "Mobile data off.".into();
    };
    let used = if used > 0 { format!(" · used {} this session", fmt_bytes(used)) } else { String::new() };
    match phase {
        Phase::Off => match err {
            Some(e) => format!("Mobile data off{used}. Last try: {e}"),
            None => format!("Mobile data off{used}. It comes on only when there is no Wi-Fi."),
        },
        Phase::On { radio, ip, .. } => {
            kick_refresh();
            format!("Mobile data on · {} · {ip}{used}", radio.line())
        }
        Phase::Lingering { radio, ip, until, .. } => {
            kick_refresh();
            format!(
                "Mobile data on · {} · {ip}{used} · off in {} s",
                radio.line(),
                until.saturating_sub(clock::now_ms()).div_ceil(1000)
            )
        }
        Phase::Standby { radio, until } => format!(
            "Mobile data ready · {} · no call{used} · modem sleeps in {} s",
            radio.line(),
            until.saturating_sub(clock::now_ms()).div_ceil(1000)
        ),
        Phase::Starting(step) => format!("Mobile data: {step}…{used}"),
        Phase::Stopping => format!("Mobile data turning off…{used}"),
    }
}

/// Whether the Battery screen should redraw every second (linger countdown, bring-up steps).
pub fn live() -> bool {
    matches!(
        snapshot(),
        Some((Phase::Starting(_) | Phase::Lingering { .. } | Phase::Standby { .. } | Phase::Stopping, _, _))
    )
}

pub struct Mobile {
    tx: Tx,
    shared: Arc<Mutex<Shared>>,
    poke_r: UnixStream,
    next_id: Arc<AtomicU64>,
    abort: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

fn prop_secs(name: &str, default: u64) -> u64 {
    control::prop_str(name).and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default).min(LINGER_LIMIT_S)
}

impl Mobile {
    pub fn new() -> Result<Mobile> {
        if control::prop_str(PROP_MOBILE).as_deref() == Some("on") {
            control::set(PROP_MOBILE, "off");
        }
        let (poke_r, poke_w) = UnixStream::pair()?;
        poke_r.set_nonblocking(true)?;
        poke_w.set_nonblocking(true)?;
        let (wake_r, wake_w) = UnixStream::pair()?;
        wake_r.set_nonblocking(true)?;
        wake_w.set_nonblocking(true)?;
        let (tx, rx) = channel();
        let shared = Arc::new(Mutex::new(Shared {
            phase: Phase::Off,
            pending: 0,
            last_error: None,
            notes: VecDeque::new(),
            used_done: 0,
            call_base: None,
            dns: Vec::new(),
        }));
        let abort = Arc::new(AtomicBool::new(false));
        let tx = Tx { tx, wake: Arc::new(wake_w) };
        let _ = GLOBAL.set(Global { shared: shared.clone(), tx: tx.clone(), refresh_at: AtomicU64::new(0) });
        let worker = Worker {
            rx,
            wake_r,
            poke_w,
            shared: shared.clone(),
            abort: abort.clone(),
            leases: HashMap::new(),
            policy: Policy {
                linger_default_ms: prop_secs(PROP_LINGER, LINGER_DEFAULT_S) * 1000,
                call_max_ms: prop_secs(PROP_CALL_LINGER_MAX, CALL_LINGER_MAX_DEFAULT_S) * 1000,
                modem_max_ms: prop_secs(PROP_LINGER_MAX, LINGER_MAX_DEFAULT_S) * 1000,
                idle_ms: prop_secs(PROP_IDLE, IDLE_DEFAULT_S) * 1000,
            },
            five_g: control::prop_str(PROP_RAT).is_some_and(|v| v.trim() == "nr"),
            dormant: control::prop_str(PROP_DORMANT).as_deref() == Some("1"),
            // abyssd restarted inside a session that already stopped rild.
            owned: control::prop_str(PROP_MODEM).as_deref() == Some("abyss"),
            found_mode: None,
            found_pref: None,
            pref_set: false,
            online: false,
            radio: Radio::default(),
            svcs: None,
            call: None,
            net: None,
            call_until: None,
            modem_until: None,
            retry_at: None,
            handoff: None,
        };
        let handle = std::thread::Builder::new().name("abyssd-mobile".into()).spawn(move || worker.run())?;
        Ok(Mobile { tx, shared, poke_r, next_id: Arc::new(AtomicU64::new(1)), abort, worker: Some(handle) })
    }

    pub fn leaser(&self) -> Leaser {
        Leaser { tx: self.tx.clone(), next_id: self.next_id.clone(), shared: self.shared.clone() }
    }

    pub fn poll_fd(&self) -> BorrowedFd<'_> {
        self.poke_r.as_fd()
    }

    /// Drains the poke socket; returns what the worker wants said (same notes as Wi-Fi).
    pub fn on_ready(&mut self) -> Vec<Note> {
        let mut b = [0u8; 64];
        loop {
            match self.poke_r.read(&mut b) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        match self.shared.lock() {
            Ok(mut s) => s.notes.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Bringing up / tearing down: abyssd must not suspend meanwhile.
    pub fn busy(&self) -> bool {
        self.shared.lock().map(|s| matches!(s.phase, Phase::Starting(_) | Phase::Stopping)).unwrap_or(false)
    }

    /// Stops the worker: ends the call and puts the modem back to low power within `budget_ms`.
    /// On SIGTERM the budget is tiny; the process exit then releases the WDS client, which ends
    /// the call on the modem side, and the phone process restores radio power after the return.
    pub fn shutdown(&mut self, budget_ms: u64) {
        self.abort.store(true, Ordering::Relaxed);
        let (ack_tx, ack_rx) = channel();
        self.tx.send(Cmd::Shutdown(ack_tx));
        match ack_rx.recv_timeout(Duration::from_millis(budget_ms)) {
            Ok(()) => {
                if let Some(h) = self.worker.take() {
                    let _ = h.join();
                }
            }
            Err(_) => warn!("mobile: tear-down did not finish within {budget_ms} ms"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The worker

struct Call {
    wds: Client,
    handle: u32,
    ifname: String,
    radio: Radio,
    ip: Ipv4Addr,
}

/// Release timing (all ms; see the module docs).
#[derive(Clone, Copy, Debug)]
struct Policy {
    /// The call after a release without a hint.
    linger_default_ms: u64,
    /// A hinted gap up to this keeps the call up.
    call_max_ms: u64,
    /// A hinted gap up to this keeps the modem registered.
    modem_max_ms: u64,
    /// The registration after a release without a hint.
    idle_ms: u64,
}

/// What happens after the last release: the call ends at `call_until` (None = now), the modem
/// goes to low power at `modem_until` (None = with the call). `modem_until` >= `call_until`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Plan {
    call_until: Option<u64>,
    modem_until: Option<u64>,
}

fn plan_release(now: u64, hint: Option<u64>, warm_ms: u64, p: &Policy) -> Plan {
    let call_ms = match hint {
        Some(h) if h <= p.call_max_ms => h + LINGER_SLACK_MS,
        Some(_) => 0,
        None => p.linger_default_ms,
    }
    .max(warm_ms);
    let modem_ms = match hint {
        Some(h) if h <= p.modem_max_ms => h + LINGER_SLACK_MS,
        Some(_) => 0,
        None => p.idle_ms,
    }
    .max(call_ms);
    Plan { call_until: (call_ms > 0).then_some(now + call_ms), modem_until: (modem_ms > 0).then_some(now + modem_ms) }
}

struct Worker {
    rx: Receiver<Cmd>,
    wake_r: UnixStream,
    poke_w: UnixStream,
    shared: Arc<Mutex<Shared>>,
    abort: Arc<AtomicBool>,
    leases: HashMap<u64, String>,
    policy: Policy,
    /// persist.abyss.mobile.rat=nr: 5G when available (default LTE).
    five_g: bool,
    dormant: bool,
    /// rild is stopped and abyssd drives the modem (until leave).
    owned: bool,
    /// The operating mode rild left the modem in (restored after each call).
    found_mode: Option<u8>,
    /// The RAT mode preference rild left (put back whenever the modem goes to low power).
    found_pref: Option<u16>,
    /// abyssd changed the mode preference (and must put `found_pref` back).
    pref_set: bool,
    /// abyssd switched the modem online and it is not back in low power yet.
    online: bool,
    /// The last radio facts (for the Standby phase).
    radio: Radio,
    svcs: Option<Services>,
    call: Option<Call>,
    net: Option<NetCfg>,
    /// No lease: the call ends at this boot ms.
    call_until: Option<u64>,
    /// No lease, no call: the modem goes to low power at this boot ms.
    modem_until: Option<u64>,
    retry_at: Option<u64>,
    handoff: Option<handoff::Handoff>,
}

/// The low-power mode to go back to: what rild left, unless that was online (abyssd restarted
/// after a crash): Abyss always runs in airplane mode (GoDark), i.e. low power.
fn rest_mode(found: Option<u8>) -> u8 {
    match found {
        Some(m @ (MODE_LOW_POWER | MODE_PERSISTENT_LOW_POWER | MODE_MODE_ONLY_LOW_POWER)) => m,
        _ => MODE_LOW_POWER,
    }
}

fn later(a: Option<u64>, b: u64) -> u64 {
    a.map_or(b, |a| a.max(b))
}

impl Worker {
    fn notify(&self) {
        let _ = (&self.poke_w).write(&[1]);
        crate::uplink::changed();
    }

    fn set_phase(&self, p: Phase) {
        if let Ok(mut s) = self.shared.lock() {
            s.phase = p;
        }
        self.notify();
    }

    fn step(&self, what: &'static str) -> Result<()> {
        if self.abort.load(Ordering::Relaxed) {
            bail!("abyssd is exiting");
        }
        self.set_phase(Phase::Starting(what));
        Ok(())
    }

    fn say(&self, text: impl Into<String>, warn: bool) {
        if let Ok(mut s) = self.shared.lock() {
            s.notes.push_back(Note::Say(text.into(), warn));
        }
        self.notify();
    }

    fn next_deadline(&self) -> Option<u64> {
        [self.call_until, self.modem_until, self.retry_at].into_iter().flatten().min()
    }

    fn run(mut self) {
        info!(
            "mobile: worker ready (call {} s / hinted ≤ {} s, registered ≤ {} s hinted / {} s idle, {}{})",
            self.policy.linger_default_ms / 1000,
            self.policy.call_max_ms / 1000,
            self.policy.modem_max_ms / 1000,
            self.policy.idle_ms / 1000,
            if self.five_g { "5G when available" } else { "LTE" },
            if self.owned { ", modem already ours" } else { "" }
        );
        loop {
            let now = clock::now_ms();
            let timeout = match self.next_deadline() {
                Some(t) => PollTimeout::try_from(t.saturating_sub(now)).unwrap_or(PollTimeout::MAX),
                None => PollTimeout::NONE,
            };
            let call_ready = {
                let mut fds = vec![PollFd::new(self.wake_r.as_fd(), PollFlags::POLLIN)];
                if let Some(c) = &self.call {
                    fds.push(PollFd::new(c.wds.fd(), PollFlags::POLLIN));
                }
                match poll(&mut fds, timeout) {
                    Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                    Err(e) => {
                        warn!("mobile: poll: {e}");
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
                fds.get(1).and_then(|f| f.revents()).is_some_and(|r| !r.is_empty())
            };
            let mut b = [0u8; 64];
            while matches!((&self.wake_r).read(&mut b), Ok(n) if n > 0) {}
            if call_ready {
                self.on_call_event();
            }
            while let Ok(cmd) = self.rx.try_recv() {
                match cmd {
                    Cmd::Shutdown(ack) => {
                        self.teardown("abyssd exiting");
                        let _ = ack.send(());
                        return;
                    }
                    Cmd::Acquire { id, holder } => {
                        // Per-burst lines are debug!: each info! line is a session.log write.
                        debug!("mobile: lease {id} taken by {holder} ({} held)", self.leases.len() + 1);
                        self.leases.insert(id, holder);
                        self.on_acquire();
                        if let Ok(mut s) = self.shared.lock() {
                            s.pending = s.pending.saturating_sub(1);
                        }
                        self.notify();
                    }
                    Cmd::Release { id, next_use_in_ms, warm_ms } => {
                        if let Some(h) = self.leases.remove(&id) {
                            debug!("mobile: lease {id} ({h}) released, next use {next_use_in_ms:?} ms, warm {warm_ms} ms");
                        }
                        self.on_release(next_use_in_ms, warm_ms);
                    }
                    Cmd::Warm { until } => self.on_warm(until),
                    Cmd::Refresh => self.refresh(),
                }
            }
            self.timers();
        }
    }

    fn timers(&mut self) {
        let now = clock::now_ms();
        if self.call_until.is_some_and(|t| now >= t) {
            self.call_until = None;
            if self.leases.is_empty() && self.call.is_some() {
                self.end_call("linger over");
                self.after_call(now);
            }
        }
        if self.modem_until.is_some_and(|t| now >= t) {
            self.modem_until = None;
            if self.leases.is_empty() && self.call.is_none() {
                self.rest_modem("no use for a while");
            }
        }
        if self.retry_at.is_some_and(|t| now >= t) {
            self.retry_at = None;
            if !self.leases.is_empty() && self.call.is_none() {
                info!("mobile: trying again ({} leases held)", self.leases.len());
                self.on_acquire();
            }
        }
    }

    /// The call just ended without a lease: wait registered until `modem_until`, or low power.
    fn after_call(&mut self, now: u64) {
        match self.modem_until.filter(|&t| t > now && self.online) {
            Some(until) => {
                debug!("mobile: call down, modem registered for {} s", (until - now) / 1000);
                self.set_phase(Phase::Standby { radio: self.radio.clone(), until });
            }
            None => self.rest_modem("next use too far away"),
        }
    }

    fn on_acquire(&mut self) {
        self.call_until = None;
        self.modem_until = None;
        if let Some(c) = &self.call {
            let p = Phase::On { radio: c.radio.clone(), ifname: c.ifname.clone(), ip: c.ip };
            self.set_phase(p);
            return;
        }
        let start = clock::now_ms();
        let warm = self.online;
        match self.bring_up() {
            Ok(()) => {
                if let Ok(mut s) = self.shared.lock() {
                    s.last_error = None;
                }
                let ms = clock::now_ms() - start;
                if warm {
                    debug!("mobile: call up from standby in {ms} ms");
                } else {
                    info!("mobile: up in {ms} ms");
                }
            }
            Err(e) => {
                let msg = format!("{e:#}");
                warn!("mobile: bring-up failed: {msg}");
                self.teardown("bring-up failed");
                if let Ok(mut s) = self.shared.lock() {
                    s.last_error = Some(msg.clone());
                }
                self.notify();
                self.say(format!("Mobile data: {msg}"), true);
                if !self.leases.is_empty() {
                    self.retry_at = Some(clock::now_ms() + RETRY_MS);
                }
            }
        }
    }

    fn on_release(&mut self, hint: Option<u64>, warm_ms: u64) {
        if !self.leases.is_empty() {
            return;
        }
        self.retry_at = None;
        if self.call.is_none() {
            return;
        }
        if self.dormant {
            if let Some(c) = &mut self.call {
                match c.wds.call(qmi::go_dormant(), QMI_MS) {
                    Ok(m) => match m.result() {
                        Ok(()) => info!("mobile: asked the modem to go dormant"),
                        Err(e) => info!("mobile: go dormant: {}", qmi::error_name(e)),
                    },
                    Err(e) => info!("mobile: go dormant: {e:#}"),
                }
            }
        }
        let now = clock::now_ms();
        let plan = plan_release(now, hint, warm_ms, &self.policy);
        self.modem_until = plan.modem_until;
        match plan.call_until {
            Some(until) => {
                debug!(
                    "mobile: call lingering {} s, registered {} s",
                    (until - now) / 1000,
                    plan.modem_until.map_or(0, |m| (m - now) / 1000)
                );
                self.call_until = Some(until);
                if let Some(c) = &self.call {
                    let p = Phase::Lingering { radio: c.radio.clone(), ifname: c.ifname.clone(), ip: c.ip, until };
                    self.set_phase(p);
                }
            }
            None => {
                self.end_call("gap between uses");
                self.after_call(now);
            }
        }
    }

    /// The switching window (uplink): keep what is up, bring nothing up.
    fn on_warm(&mut self, until: u64) {
        if !self.leases.is_empty() || until <= clock::now_ms() {
            return;
        }
        if let Some(c) = &self.call {
            let t = later(self.call_until, until);
            self.call_until = Some(t);
            self.modem_until = Some(later(self.modem_until, t));
            let p = Phase::Lingering { radio: c.radio.clone(), ifname: c.ifname.clone(), ip: c.ip, until: t };
            self.set_phase(p);
        } else if self.online && self.modem_until.is_some() {
            let t = later(self.modem_until, until);
            self.modem_until = Some(t);
            self.set_phase(Phase::Standby { radio: self.radio.clone(), until: t });
        }
    }

    /// The WDS socket is readable: the network or the modem ended the call?
    fn on_call_event(&mut self) {
        let Some(c) = &mut self.call else { return };
        let ind = match c.wds.indications() {
            Ok(v) => v,
            Err(e) => {
                warn!("mobile: wds read: {e:#}");
                Vec::new()
            }
        };
        let lost = ind
            .iter()
            .filter(|m| m.msg_id == qmi::WDS_PACKET_SERVICE_STATUS)
            .filter_map(qmi::parse_packet_status)
            .any(|s| s == 1);
        if !lost {
            return;
        }
        let reason = ind.iter().find_map(|m| m.u16(0x10)).map(|r| format!(" (reason {r})")).unwrap_or_default();
        warn!("mobile: the network ended the call{reason}");
        if let Some(c) = &mut self.call {
            c.handle = 0; // already gone: no Stop Network
        }
        self.call_until = None;
        self.end_call("call ended by the network");
        if let Ok(mut s) = self.shared.lock() {
            s.last_error = Some(format!("the network ended the data call{reason}"));
        }
        self.notify();
        if !self.leases.is_empty() {
            // Coverage gaps (tunnels, lifts) are common on the go: try again once at once.
            self.on_acquire();
        } else {
            self.after_call(clock::now_ms());
        }
    }

    /// Fresh operator/signal while a call is up or the modem waits registered (screen showing
    /// it).
    fn refresh(&mut self) {
        let Some(svcs) = self.svcs else { return };
        if self.call.is_none() && !self.online {
            return;
        }
        let Ok((serving, signal, nr)) = read_radio(&svcs) else { return };
        self.radio = Radio {
            rat: qmi::radio_label(&serving.radios, &nr, &signal),
            operator: self.operator(&serving),
            bars: signal.bars(serving.rat()),
            roaming: serving.roaming == Some(true),
        };
        let radio = self.radio.clone();
        let phase = match &mut self.call {
            Some(c) => {
                c.radio = radio.clone();
                let (ifname, ip) = (c.ifname.clone(), c.ip);
                match self.call_until {
                    Some(until) => Phase::Lingering { radio, ifname, ip, until },
                    None => Phase::On { radio, ifname, ip },
                }
            }
            None => match self.modem_until {
                Some(until) => Phase::Standby { radio, until },
                None => return,
            },
        };
        self.set_phase(phase);
    }

    fn operator(&self, s: &qmi::Serving) -> String {
        let ho = self.handoff.as_ref();
        // Android's name for the network when the PLMN matches; the modem's own otherwise.
        match (ho, s.mcc_mnc()) {
            (Some(h), Some(p)) if !h.operator.is_empty() && h.mcc_mnc == p => h.operator.clone(),
            (_, Some(p)) => s.plmn_name.clone().unwrap_or(p),
            (Some(h), None) => h.operator.clone(),
            _ => String::new(),
        }
    }

    /// Stops rild (once per session). init does it on our property (abyss.rc).
    fn own_modem(&mut self) -> Result<()> {
        if self.owned {
            return Ok(());
        }
        info!("mobile: taking the modem over from rild");
        control::set(PROP_MODEM, "abyss");
        let deadline = clock::now_ms() + RILD_STOP_MS;
        loop {
            // Unreadable counts as "still running": two drivers of one modem must never happen.
            match control::prop_str(PROP_RILD_STATE).as_deref() {
                Some("stopped") => break,
                _ if clock::now_ms() >= deadline => {
                    control::set(PROP_MODEM, "rild");
                    bail!("rild did not stop");
                }
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        // rild's QMI clients are gone with it; give the modem a moment to drop their state.
        std::thread::sleep(Duration::from_millis(200));
        self.owned = true;
        Ok(())
    }

    /// LTE or 5G for this Abyss session (before the modem goes online, so it registers with
    /// it). Best effort: a modem that refuses keeps Android's preference.
    fn apply_rat(&mut self, svcs: &Services) {
        if self.pref_set {
            return;
        }
        let r = (|| -> Result<()> {
            let mut nas = Client::open(svcs.nas, "nas")?;
            let found = match self.found_pref {
                Some(f) => f,
                None => {
                    let f = qmi::parse_mode_preference(&nas.call(qmi::get_system_selection_preference(), QMI_MS)?)?;
                    self.found_pref = Some(f);
                    f
                }
            };
            let Some(want) = qmi::wanted_mode_preference(found, self.five_g) else {
                info!("mobile: RAT preference {found:#06x} kept ({})", if self.five_g { "5G when available" } else { "LTE" });
                return Ok(());
            };
            nas.call(qmi::set_mode_preference(want), QMI_MS)?.ok("set mode preference")?;
            self.pref_set = true;
            info!(
                "mobile: RAT preference {found:#06x} -> {want:#06x} ({}) until low power",
                if self.five_g { "5G when available" } else { "LTE, no 5G" }
            );
            Ok(())
        })();
        if let Err(e) = r {
            warn!("mobile: RAT preference: {e:#}");
        }
    }

    /// Puts Android's mode preference back (the modem is in low power by now).
    fn restore_rat(&mut self) {
        let (true, Some(found), Some(svcs)) = (self.pref_set, self.found_pref, self.svcs) else { return };
        let r = Client::open(svcs.nas, "nas").and_then(|mut nas| nas.call(qmi::set_mode_preference(found), QMI_MS)?.ok("set mode preference"));
        match r {
            Ok(()) => {
                self.pref_set = false;
                info!("mobile: RAT preference back to {found:#06x}");
            }
            // Set only until the next modem power cycle; the phone process also sends Android's
            // allowed network types again when it starts after the leave.
            Err(e) => warn!("mobile: RAT preference back to {found:#06x}: {e:#}"),
        }
    }

    fn bring_up(&mut self) -> Result<()> {
        let t0 = clock::now_ms();
        let warm = self.online;
        self.step(if warm { "connecting" } else { "starting" })?;
        let ho = handoff::load().ok_or_else(|| anyhow!("no SIM details from Android (enter Abyss again)"))?;
        if let Some(r) = ho.refusal() {
            bail!("{r}");
        }
        let apn = ho.apn.clone().unwrap_or_default();
        self.handoff = Some(ho.clone());
        self.own_modem()?;
        let svcs = match self.svcs {
            Some(s) => s,
            None => {
                let s = Services::find(2_000)?;
                info!(
                    "mobile: qrtr: wds {}:{} dms {}:{} nas {}:{} uim {}:{}",
                    s.wds.node, s.wds.port, s.dms.node, s.dms.port, s.nas.node, s.nas.port, s.uim.node, s.uim.port
                );
                self.svcs = Some(s);
                s
            }
        };

        // 1. Modem online (already, from standby: one Get).
        if !warm {
            self.step("starting the modem")?;
        }
        let t_modem = clock::now_ms();
        {
            let mut dms = Client::open(svcs.dms, "dms")?;
            let mode = qmi::parse_operating_mode(&dms.call(qmi::get_operating_mode(), QMI_MS)?)?;
            if self.found_mode.is_none() {
                self.found_mode = Some(mode);
            }
            if mode != MODE_ONLINE {
                self.apply_rat(&svcs);
                dms.call(qmi::set_operating_mode(MODE_ONLINE), ONLINE_MS)?.ok("modem online")?;
                self.online = true;
                let deadline = clock::now_ms() + ONLINE_MS;
                loop {
                    let m = qmi::parse_operating_mode(&dms.call(qmi::get_operating_mode(), QMI_MS)?)?;
                    if m == MODE_ONLINE {
                        break;
                    }
                    if clock::now_ms() >= deadline {
                        bail!("modem refused online (stays {})", qmi::mode_name(m));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                info!("mobile: modem was {} ({mode}), now online", qmi::mode_name(mode));
            } else {
                self.online = true;
            }
        }
        let modem_ms = clock::now_ms() - t_modem;

        // 2. SIM.
        if !warm {
            self.step("checking the SIM")?;
        }
        let t_sim = clock::now_ms();
        {
            let mut uim = Client::open(svcs.uim, "uim")?;
            let deadline = clock::now_ms() + SIM_MS;
            let mut activated = false;
            loop {
                let cs = qmi::parse_card_status(&uim.call(qmi::get_card_status(), QMI_MS)?)?;
                match qmi::sim_verdict(&cs, ho.slot) {
                    SimVerdict::Ready { slot, kind } => {
                        if !warm {
                            info!("mobile: SIM ready ({}, slot {})", if kind == qmi::APP_USIM { "USIM" } else { "SIM" }, slot + 1);
                        }
                        break;
                    }
                    SimVerdict::Locked(what) => bail!("SIM locked: enter the {what} in Android"),
                    SimVerdict::NoSim => bail!("No SIM"),
                    SimVerdict::Unusable(why) => bail!("SIM: {why}"),
                    SimVerdict::NoSession { slot, aid } if !activated => {
                        info!("mobile: no provisioning session for slot {}; activating it", slot + 1);
                        let r = uim.call(qmi::change_provisioning_session(slot, &aid), QMI_MS)?;
                        if let Err(e) = r.result() {
                            warn!("mobile: activate provisioning session: {}", qmi::error_name(e));
                        }
                        activated = true;
                    }
                    SimVerdict::NoSession { .. } | SimVerdict::NotReady => {}
                }
                if clock::now_ms() >= deadline {
                    bail!("the SIM did not get ready");
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        let sim_ms = clock::now_ms() - t_sim;

        // 3. Registration (polled; the NAS client is closed afterwards: no indications).
        if !warm {
            self.step("finding the network")?;
        }
        let t_reg = clock::now_ms();
        let (serving, signal, nr) = {
            let mut nas = Client::open(svcs.nas, "nas")?;
            let deadline = clock::now_ms() + REGISTER_MS;
            let serving = loop {
                let s = qmi::parse_serving_system(&nas.call(qmi::get_serving_system(), QMI_MS)?)?;
                if s.registered() {
                    break s;
                }
                if s.reg == 3 {
                    bail!("the network refused the SIM");
                }
                if clock::now_ms() >= deadline {
                    bail!("no mobile network here");
                }
                if self.abort.load(Ordering::Relaxed) {
                    bail!("abyssd is exiting");
                }
                std::thread::sleep(Duration::from_millis(500));
            };
            let signal = nas
                .call(qmi::get_signal_info(), QMI_MS)
                .ok()
                .and_then(|m| qmi::parse_signal_info(&m).ok())
                .unwrap_or_default();
            let nr = nas.call(qmi::get_system_info(), QMI_MS).ok().and_then(|m| qmi::parse_system_info(&m).ok()).unwrap_or_default();
            (serving, signal, nr)
        };
        let reg_ms = clock::now_ms() - t_reg;
        if serving.roaming == Some(true) && !ho.roaming_allowed {
            bail!("roaming, and data roaming is off in Android");
        }
        let radio = Radio {
            rat: qmi::radio_label(&serving.radios, &nr, &signal),
            operator: self.operator(&serving),
            bars: signal.bars(serving.rat()),
            roaming: serving.roaming == Some(true),
        };
        self.radio = radio.clone();
        if !warm {
            info!(
                "mobile: registered {} {} in {reg_ms} ms (lte rsrp {:?}, nr rsrp {:?}, nr {:?}, {})",
                radio.rat.unwrap_or("?"),
                serving.mcc_mnc().unwrap_or_default(),
                signal.lte_rsrp,
                signal.nr_rsrp,
                nr,
                radio.line()
            );
        }

        // 4. The call.
        self.step("connecting")?;
        let t_call = clock::now_ms();
        let (ifname, mux) = rmnet::data_link()?;
        debug!("mobile: {ifname} mux {mux}");
        let mut wds = Client::open(svcs.wds, "wds")?;
        let r = wds.call(qmi::bind_mux_data_port(mux), QMI_MS)?;
        if let Err(e) = r.result() {
            if e != qmi::ERR_NO_EFFECT {
                bail!("bind {ifname}: modem error {}", qmi::error_name(e));
            }
        }
        let handle = qmi::parse_start_network(&wds.call(qmi::start_network(&apn), CALL_MS)?)
            .map_err(|e| anyhow!("data call on {} failed: {e}", apn.apn))?;
        // From here the call exists: keep it in self.call so every error path stops it.
        self.call = Some(Call { wds, handle, ifname: ifname.clone(), radio: radio.clone(), ip: Ipv4Addr::UNSPECIFIED });
        let c = self.call.as_mut().ok_or_else(|| anyhow!("call vanished"))?;
        let ip = qmi::parse_current_settings(&c.wds.call(qmi::get_current_settings(), QMI_MS)?)?;
        c.ip = ip.ip;
        let call_ms = clock::now_ms() - t_call;
        debug!("mobile: call up handle {handle:#x} in {call_ms} ms: {}/{} dns {:?} mtu {:?}", ip.ip, ip.prefix, ip.dns, ip.mtu);

        // 5. netd (abyss-net).
        control::set(PROP_MOBILE, "on");
        let mut nc = NetCfg::connect(3_000)?;
        let netd_ms = nc.up_mobile(&ifname, &ip)?;
        self.net = Some(nc);
        if let Ok(mut s) = self.shared.lock() {
            let (rx, tx) = rmnet::counters(&ifname);
            s.call_base = Some((ifname.clone(), rx + tx));
            s.dns = ip.dns.clone();
        }
        let line = format!(
            "mobile: on {} · {} in {} ms (modem {modem_ms} ms, sim {sim_ms} ms, attach {reg_ms} ms, call {call_ms} ms, netd {netd_ms} ms)",
            radio.line(),
            ip.ip,
            clock::now_ms() - t0
        );
        if warm {
            debug!("{line}");
        } else {
            info!("{line}");
        }
        self.set_phase(Phase::On { radio, ifname, ip: ip.ip });
        Ok(())
    }

    /// Ends the call (if any) and takes its network out of netd; the modem stays as it is.
    /// Every step is best effort; errors are logged.
    fn end_call(&mut self, why: &str) {
        self.call_until = None;
        let Some(mut c) = self.call.take() else {
            self.net = None;
            return;
        };
        self.set_phase(Phase::Stopping);
        if c.handle != 0 {
            match c.wds.call(qmi::stop_network(c.handle), QMI_MS) {
                Ok(m) => match m.result() {
                    Ok(()) => {}
                    Err(e) if e == qmi::ERR_NO_EFFECT => {}
                    Err(e) => warn!("mobile: stop call: {}", qmi::error_name(e)),
                },
                Err(e) => warn!("mobile: stop call: {e:#}"),
            }
        }
        if let Some(nc) = self.net.as_mut() {
            if let Err(e) = nc.down_mobile(&c.ifname) {
                warn!("mobile: netcfg down: {e:#}");
            }
        }
        if let Ok(mut s) = self.shared.lock() {
            let used = s.used();
            s.used_done = used;
            s.call_base = None;
            s.dns.clear();
        }
        drop(c); // closes the WDS client
        self.net = None;
        crate::uplink::link_down(false);
        if control::prop_str(PROP_MOBILE).as_deref() == Some("on") {
            control::set(PROP_MOBILE, "off");
        }
        debug!("mobile: call down ({why})");
    }

    /// The modem back to the low-power mode it was found in, Android's RAT preference back.
    fn rest_modem(&mut self, why: &str) {
        self.modem_until = None;
        if self.owned && (self.online || self.pref_set) {
            if let Some(svcs) = self.svcs {
                let target = rest_mode(self.found_mode);
                match set_mode(&svcs, target) {
                    Ok(true) => info!("mobile: modem back to {} ({target}): {why}", qmi::mode_name(target)),
                    Ok(false) => {}
                    Err(e) => warn!("mobile: modem back to low power: {e:#}"),
                }
            }
            self.restore_rat();
        }
        self.online = false;
        self.set_phase(Phase::Off);
    }

    /// Call down, modem to low power.
    fn teardown(&mut self, why: &str) {
        let had_call = self.call.is_some();
        self.end_call(why);
        self.rest_modem(why);
        if had_call {
            info!("mobile: off ({why})");
        }
    }
}

/// Sets the operating mode unless it already is; Ok(true) if it changed it.
fn set_mode(svcs: &Services, target: u8) -> Result<bool> {
    let mut dms = Client::open(svcs.dms, "dms")?;
    let mode = qmi::parse_operating_mode(&dms.call(qmi::get_operating_mode(), QMI_MS)?)?;
    if mode == target {
        return Ok(false);
    }
    dms.call(qmi::set_operating_mode(target), ONLINE_MS)?.ok("set operating mode")?;
    Ok(true)
}

fn read_radio(svcs: &Services) -> Result<(qmi::Serving, qmi::Signal, qmi::NrInfo)> {
    let mut nas = Client::open(svcs.nas, "nas")?;
    let serving = qmi::parse_serving_system(&nas.call(qmi::get_serving_system(), QMI_MS)?)?;
    let signal = nas
        .call(qmi::get_signal_info(), QMI_MS)
        .ok()
        .and_then(|m| qmi::parse_signal_info(&m).ok())
        .unwrap_or_default();
    let nr = nas.call(qmi::get_system_info(), QMI_MS).ok().and_then(|m| qmi::parse_system_info(&m).ok()).unwrap_or_default();
    Ok((serving, signal, nr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_mode_never_online() {
        assert_eq!(rest_mode(Some(MODE_PERSISTENT_LOW_POWER)), MODE_PERSISTENT_LOW_POWER);
        assert_eq!(rest_mode(Some(MODE_LOW_POWER)), MODE_LOW_POWER);
        assert_eq!(rest_mode(Some(MODE_ONLINE)), MODE_LOW_POWER);
        assert_eq!(rest_mode(None), MODE_LOW_POWER);
    }

    #[test]
    fn release_plans() {
        let p = Policy { linger_default_ms: 20_000, call_max_ms: 30_000, modem_max_ms: 600_000, idle_ms: 180_000 };
        let plan = |hint, warm| plan_release(1000, hint, warm, &p);
        // Classic bursts (~10 s apart): the call stays up.
        assert_eq!(plan(Some(10_000), 0), Plan { call_until: Some(16_000), modem_until: Some(16_000) });
        // Long bursts (~220 s): call down now, registered until the next burst.
        assert_eq!(plan(Some(220_000), 0), Plan { call_until: None, modem_until: Some(226_000) });
        // ... unless the switching window is open: the call stays for the window.
        assert_eq!(plan(Some(220_000), 90_000), Plan { call_until: Some(91_000), modem_until: Some(226_000) });
        // A station stopped (no hint): 20 s call, then registered for idle_s.
        assert_eq!(plan(None, 0), Plan { call_until: Some(21_000), modem_until: Some(181_000) });
        // Next use beyond linger_max: low power at once, or after the window.
        assert_eq!(plan(Some(700_000), 0), Plan { call_until: None, modem_until: None });
        assert_eq!(plan(Some(700_000), 60_000), Plan { call_until: Some(61_000), modem_until: Some(61_000) });
        let p0 = Policy { linger_default_ms: 0, idle_ms: 0, ..p };
        assert_eq!(plan_release(1000, None, 0, &p0), Plan { call_until: None, modem_until: None });
    }

    #[test]
    fn radio_line() {
        let r = Radio { rat: Some("LTE"), operator: "Telia EE".into(), bars: Some(3), roaming: false };
        assert_eq!(r.line(), "LTE · Telia EE · 3 bars");
        let r = Radio { rat: Some("5G"), operator: String::new(), bars: Some(1), roaming: true };
        assert_eq!(r.line(), "5G · roaming · 1 bar");
        assert_eq!(Radio::default().line(), "");
    }

    #[test]
    fn bytes() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(12_345), "12 kB");
        assert_eq!(fmt_bytes(1_234_567), "1.2 MB");
        assert_eq!(fmt_bytes(456_000_000), "456 MB");
    }
}
