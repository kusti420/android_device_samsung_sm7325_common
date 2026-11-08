//! Wi-Fi inside Abyss: on-demand leases.
//!
//! Anything that needs the network takes a [`Lease`] (`Wifi::acquire`) and drops or releases it
//! when done. The first lease brings Wi-Fi up, the last release takes it down again, after an
//! optional linger: stay associated (power save) if the next use is soon, so ten ticks a minute
//! apart cost one association instead of ten (battery-nerd §4.4).
//!
//! Bring-up (worker thread, blocking, ~1-3 s on a known network):
//! 1. `sys.abyss.wifi=on` → abyss.rc starts Android's `wpa_supplicant` (global control socket
//!    only) and the `abyss-net` helper (netcfg.rs).
//! 2. wpa.rs: INTERFACE_ADD wlan0 (the supplicant brings the link up; the WCN6750 firmware
//!    starts on link up), country code, the saved networks from the hand-over (handoff.rs):
//!    first only the last network on its last frequency/BSSID (one-channel scan), then all of
//!    them with a full scan.
//! 3. dhcp.rs: INIT-REBOOT with the cached address, else DISCOVER (Rapid Commit).
//! 4. netcfg.rs: abyss-net configures netd (network 42 + address + routes + default network).
//! 5. `SET ps 1` (802.11 power save).
//!
//! Tear-down: netd network down, DISCONNECT + INTERFACE_REMOVE (link down; the firmware stops
//! 10 s later, gInterfaceChangeWait), `sys.abyss.wifi=off` (init stops both services; abyss-net
//! cleans netd once more on its way out).
//!
//! Threading: `Wifi` lives on the main thread; the state machine runs on its own thread
//! ("abyssd-wifi") so nothing here can stall the key/audio loop. Commands go over a channel plus
//! a wake byte; status comes back through `Shared` plus one byte on a socket pair that the main
//! loop polls (like bt.rs).
//!
//! The future `job`/`state` primitives (abyss-design docs/reference) take leases through this
//! API; `holder` names who holds one (logs only for now).

pub mod dhcp;
mod dns;
mod handoff;
mod http;
pub mod netcfg;
mod netd;
mod wpa;

use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::Ipv4Addr;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use log::{info, warn};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

use crate::{clock, control};
use handoff::{Handoff, Network, Sec};
use netcfg::NetCfg;
use wpa::{Wpa, IFACE};

/// init starts wpa_supplicant + abyss-net on `on` (while Abyss is active), stops both on `off`.
pub const PROP_WIFI: &str = "sys.abyss.wifi";
/// Linger after the last release when the holder gave no hint (seconds, default 30, 0..1200).
const PROP_LINGER: &str = "persist.abyss.wifi.linger_s";
/// A hinted next use further away than this goes fully off instead (seconds, default 600).
const PROP_LINGER_MAX: &str = "persist.abyss.wifi.linger_max_s";
/// "factory" = always the factory MAC; default "handoff" = Android's per-network MAC.
const PROP_MAC: &str = "persist.abyss.wifi.mac";
const PROP_TEST_URL: &str = "persist.abyss.wifi.test_url";
const DEFAULT_TEST_URL: &str = "http://connectivitycheck.gstatic.com/generate_204";
const LINGER_DEFAULT_S: u64 = 30;
const LINGER_MAX_DEFAULT_S: u64 = 600;
const LINGER_LIMIT_S: u64 = 1200;
/// Extra time on top of a hinted next use, so the next lease finds the link still up.
const LINGER_SLACK_MS: u64 = 5_000;

/// One-channel attempt on the last known network.
const FAST_ASSOC_MS: u64 = 5_000;
/// Full scan over every handed-over network.
const FULL_ASSOC_MS: u64 = 15_000;
const STATUS_POLL_MS: u64 = 150;
/// Associated link lost: how long the supplicant may try to get it back.
const RECONNECT_MS: u64 = 20_000;
/// Renew retry when the server did not answer.
const RENEW_RETRY_MS: u64 = 60_000;
const MAX_NETWORKS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Off,
    Starting,
    Associating(String),
    Dhcp(String),
    On { ssid: String, ip: Ipv4Addr },
    /// No lease held; associated until `until` (boot ms).
    Lingering { ssid: String, ip: Ipv4Addr, until: u64 },
    Reconnecting(String),
    Stopping,
}

#[derive(Clone, Debug)]
pub struct TestResult {
    pub ok: bool,
    /// Short, for the row's value ("OK 204 · 1.4 s", "failed").
    pub short: String,
    /// The full line for the screen body.
    pub line: String,
}

/// Feedback for the main loop.
pub enum Note {
    /// Text for the feedback line; true = warning colour.
    Say(String, bool),
}

struct Shared {
    phase: Phase,
    last_error: Option<String>,
    test_running: bool,
    last_test: Option<TestResult>,
    notes: VecDeque<Note>,
}

enum Cmd {
    Acquire { id: u64, holder: String },
    Release { id: u64, next_use_in_ms: Option<u64> },
    /// The Wi-Fi test, holding its own lease (taken through the public API).
    Test(Lease),
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

/// A held Wi-Fi lease. Dropping it releases with the default linger.
pub struct Lease {
    id: u64,
    tx: Tx,
    done: bool,
}

impl Lease {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Releases now. `next_use_in_ms`: when the holder expects to need Wi-Fi again (keeps the
    /// link up until then if that is within persist.abyss.wifi.linger_max_s).
    pub fn release(mut self, next_use_in_ms: Option<u64>) {
        self.tx.send(Cmd::Release { id: self.id, next_use_in_ms });
        self.done = true;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.done {
            self.tx.send(Cmd::Release { id: self.id, next_use_in_ms: None });
        }
    }
}

/// Takes leases from any thread; same semantics as `Wifi::acquire`.
#[derive(Clone)]
pub struct Leaser {
    tx: Tx,
    next_id: Arc<AtomicU64>,
}

impl Leaser {
    pub fn acquire(&self, holder: &str) -> Lease {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.tx.send(Cmd::Acquire { id, holder: holder.to_string() });
        Lease { id, tx: self.tx.clone(), done: false }
    }
}

pub struct Wifi {
    tx: Tx,
    shared: Arc<Mutex<Shared>>,
    poke_r: UnixStream,
    /// Shared with every `Leaser`, so lease ids stay unique across threads.
    next_id: Arc<AtomicU64>,
    abort: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

fn prop_secs(name: &str, default: u64) -> u64 {
    control::prop_str(name)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
        .min(LINGER_LIMIT_S)
}

impl Wifi {
    pub fn new() -> Result<Wifi> {
        // A stale "on" (abyssd restarted inside one session) would leave the services running.
        if control::prop_str(PROP_WIFI).as_deref() == Some("on") {
            control::set(PROP_WIFI, "off");
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
            last_error: None,
            test_running: false,
            last_test: None,
            notes: VecDeque::new(),
        }));
        let abort = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            rx,
            wake_r,
            poke_w,
            shared: shared.clone(),
            abort: abort.clone(),
            leases: HashMap::new(),
            linger_default_ms: prop_secs(PROP_LINGER, LINGER_DEFAULT_S) * 1000,
            linger_max_ms: prop_secs(PROP_LINGER_MAX, LINGER_MAX_DEFAULT_S) * 1000,
            factory_mac: control::prop_str(PROP_MAC).as_deref() == Some("factory"),
            test_url: control::prop_str(PROP_TEST_URL).unwrap_or_else(|| DEFAULT_TEST_URL.into()),
            wpa: None,
            net: None,
            link: None,
            linger_until: None,
            reconnect_until: None,
            dns: dns::Resolver::new(),
        };
        let handle = std::thread::Builder::new().name("abyssd-wifi".into()).spawn(move || worker.run())?;
        Ok(Wifi {
            tx: Tx { tx, wake: Arc::new(wake_w) },
            shared,
            poke_r,
            next_id: Arc::new(AtomicU64::new(1)),
            abort,
            worker: Some(handle),
        })
    }

    /// Takes a lease; Wi-Fi comes up in the background (watch `phase()` / the notes).
    pub fn acquire(&mut self, holder: &str) -> Lease {
        self.leaser().acquire(holder)
    }

    /// A handle that takes leases from any thread (the radio, module fetches).
    pub fn leaser(&self) -> Leaser {
        Leaser { tx: self.tx.clone(), next_id: self.next_id.clone() }
    }

    /// The Battery screen's "Wi-Fi test": acquire → GET the test URL → release. The lease goes
    /// to the worker with the test; commands run in order, so the Acquire (and the bring-up it
    /// starts) is done before the test steps run.
    pub fn run_test(&mut self) {
        if let Ok(mut s) = self.shared.lock() {
            if s.test_running {
                return;
            }
            s.test_running = true;
        }
        let lease = self.acquire("wifi-test");
        self.tx.send(Cmd::Test(lease));
    }

    pub fn poll_fd(&self) -> BorrowedFd<'_> {
        self.poke_r.as_fd()
    }

    /// Drains the poke socket; returns what the worker wants said.
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

    /// Associated without a lease (the Battery screen counts the linger down).
    pub fn lingering(&self) -> bool {
        self.shared.lock().map(|s| matches!(s.phase, Phase::Lingering { .. })).unwrap_or(false)
    }

    /// One line for the Battery screen: the state with the address, or the last failure.
    pub fn detail(&self) -> String {
        let (phase, err) = match self.shared.lock() {
            Ok(s) => (s.phase.clone(), s.last_error.clone()),
            Err(_) => (Phase::Off, None),
        };
        match phase {
            Phase::Off => match err {
                Some(e) => format!("Wi-Fi off. Last try: {e}"),
                None => "Wi-Fi off. It comes on only while something needs it.".into(),
            },
            Phase::On { ssid, ip } => format!("Wi-Fi on · {ssid} · {ip}"),
            Phase::Lingering { ssid, ip, until } => format!(
                "Wi-Fi on · {ssid} · {ip} · off in {} s",
                until.saturating_sub(clock::now_ms()).div_ceil(1000)
            ),
            _ => format!("Wi-Fi {}", self.summary()),
        }
    }

    /// Home status value: "off", "on (Home)", "connecting (Home)…".
    pub fn summary(&self) -> String {
        let (phase, err) = match self.shared.lock() {
            Ok(s) => (s.phase.clone(), s.last_error.is_some()),
            Err(_) => (Phase::Off, false),
        };
        match phase {
            Phase::Off if err => "off · last try failed".into(),
            Phase::Off => "off".into(),
            Phase::Starting => "starting…".into(),
            Phase::Associating(s) if s.is_empty() => "connecting…".into(),
            Phase::Associating(s) => format!("connecting ({s})…"),
            Phase::Dhcp(s) => format!("getting an address ({s})…"),
            Phase::On { ssid, .. } | Phase::Lingering { ssid, .. } => format!("on ({ssid})"),
            Phase::Reconnecting(s) => format!("reconnecting ({s})…"),
            Phase::Stopping => "turning off…".into(),
        }
    }

    /// Bringing up / tearing down / testing: abyssd must not suspend meanwhile.
    pub fn busy(&self) -> bool {
        match self.shared.lock() {
            Ok(s) => {
                s.test_running
                    || matches!(
                        s.phase,
                        Phase::Starting
                            | Phase::Associating(_)
                            | Phase::Dhcp(_)
                            | Phase::Reconnecting(_)
                            | Phase::Stopping
                    )
            }
            Err(_) => false,
        }
    }

    pub fn test_running(&self) -> bool {
        self.shared.lock().map(|s| s.test_running).unwrap_or(false)
    }

    pub fn last_test(&self) -> Option<TestResult> {
        self.shared.lock().ok().and_then(|s| s.last_test.clone())
    }

    /// Stops the worker. `budget_ms`: how long a clean tear-down may take (abyss.rc's leave path
    /// stops both services anyway, and abyss-net cleans netd on SIGTERM).
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
            Err(_) => warn!("wifi: tear-down did not finish within {budget_ms} ms; init stops the services"),
        }
    }
}

// ---------------------------------------------------------------------------
// The worker

/// The association + address while up.
struct Link {
    ssid: String,
    ssid_hex: String,
    bssid: Option<String>,
    freq: Option<u32>,
    dhcp: dhcp::Client,
    lease: dhcp::Lease,
    /// Boot ms of the last ACK.
    lease_at: u64,
    renew_at: u64,
    associated: bool,
}

impl Link {
    fn expire_at(&self) -> u64 {
        self.lease_at + self.lease.lease_s as u64 * 1000
    }
    fn rebind_at(&self) -> u64 {
        self.lease_at + self.lease.t2_s as u64 * 1000
    }
}

struct Worker {
    rx: Receiver<Cmd>,
    wake_r: UnixStream,
    poke_w: UnixStream,
    shared: Arc<Mutex<Shared>>,
    abort: Arc<AtomicBool>,
    leases: HashMap<u64, String>,
    linger_default_ms: u64,
    linger_max_ms: u64,
    factory_mac: bool,
    test_url: String,
    wpa: Option<Wpa>,
    net: Option<NetCfg>,
    link: Option<Link>,
    linger_until: Option<u64>,
    reconnect_until: Option<u64>,
    dns: dns::Resolver,
}

/// Network ids added to the supplicant, with the hand-over entry each one is.
type Added = Vec<(u32, Network)>;

fn mac_bytes(s: &str) -> Option<[u8; 6]> {
    let v: Vec<u8> = s.split(':').filter_map(|p| u8::from_str_radix(p, 16).ok()).collect();
    (v.len() == 6).then(|| [v[0], v[1], v[2], v[3], v[4], v[5]])
}

fn kv<'a>(st: &'a [(String, String)], k: &str) -> Option<&'a str> {
    st.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str())
}

impl Worker {
    fn notify(&self) {
        let _ = (&self.poke_w).write(&[1]);
    }

    fn set_phase(&self, p: Phase) {
        if let Ok(mut s) = self.shared.lock() {
            if s.phase != p {
                s.phase = p;
            }
        }
        self.notify();
    }

    fn say(&self, text: impl Into<String>, warn: bool) {
        if let Ok(mut s) = self.shared.lock() {
            s.notes.push_back(Note::Say(text.into(), warn));
        }
        self.notify();
    }

    fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }

    fn run(mut self) {
        info!(
            "wifi: worker ready (linger {} s, max {} s, mac {})",
            self.linger_default_ms / 1000,
            self.linger_max_ms / 1000,
            if self.factory_mac { "factory" } else { "hand-over" }
        );
        loop {
            let now = clock::now_ms();
            let timeout = match self.next_deadline() {
                Some(t) => PollTimeout::try_from(t.saturating_sub(now)).unwrap_or(PollTimeout::MAX),
                None => PollTimeout::NONE,
            };
            let ev_ready = {
                let mut fds = vec![PollFd::new(self.wake_r.as_fd(), PollFlags::POLLIN)];
                if let Some(fd) = self.wpa.as_ref().and_then(|w| w.event_fd()) {
                    fds.push(PollFd::new(fd, PollFlags::POLLIN));
                }
                match poll(&mut fds, timeout) {
                    Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                    Err(e) => {
                        warn!("wifi: poll: {e}");
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
                fds.get(1).and_then(|f| f.revents()).is_some_and(|r| !r.is_empty())
            };
            let mut b = [0u8; 64];
            while matches!((&self.wake_r).read(&mut b), Ok(n) if n > 0) {}
            while let Ok(cmd) = self.rx.try_recv() {
                if let Cmd::Shutdown(ack) = cmd {
                    self.teardown("abyssd exiting");
                    let _ = ack.send(());
                    return;
                }
                self.handle(cmd);
            }
            if ev_ready {
                self.on_events();
            }
            self.timers();
        }
    }

    fn next_deadline(&self) -> Option<u64> {
        let mut t: Option<u64> = None;
        let mut take = |x: Option<u64>| {
            if let Some(x) = x {
                t = Some(t.map_or(x, |y| y.min(x)));
            }
        };
        take(self.linger_until);
        take(self.reconnect_until);
        take(self.link.as_ref().filter(|l| l.associated).map(|l| l.renew_at));
        t
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Acquire { id, holder } => {
                info!("wifi: lease {id} taken by {holder} ({} held)", self.leases.len() + 1);
                self.leases.insert(id, holder);
                self.on_acquire();
            }
            Cmd::Release { id, next_use_in_ms } => {
                if let Some(h) = self.leases.remove(&id) {
                    info!("wifi: lease {id} ({h}) released, next use {next_use_in_ms:?} ms");
                }
                self.on_release(next_use_in_ms);
            }
            Cmd::Test(lease) => self.run_test(lease),
            Cmd::Shutdown(_) => {}
        }
    }

    fn on_acquire(&mut self) {
        self.linger_until = None;
        if let Some(l) = &self.link {
            let p = Phase::On { ssid: l.ssid.clone(), ip: l.lease.ip };
            if l.associated {
                self.set_phase(p);
            }
            return;
        }
        if let Err(e) = self.bring_up() {
            let msg = format!("{e:#}");
            warn!("wifi: bring-up failed: {msg}");
            self.teardown("bring-up failed");
            if let Ok(mut s) = self.shared.lock() {
                s.last_error = Some(msg.clone());
            }
            self.say(format!("Wi-Fi: {msg}"), true);
        }
    }

    fn on_release(&mut self, hint: Option<u64>) {
        if !self.leases.is_empty() || self.link.is_none() {
            if self.leases.is_empty() && self.wpa.is_some() {
                self.teardown("last lease released");
            }
            return;
        }
        let linger = match hint {
            Some(h) if h <= self.linger_max_ms => h + LINGER_SLACK_MS,
            Some(_) => 0,
            None => self.linger_default_ms,
        };
        if linger == 0 {
            self.teardown("last lease released");
            return;
        }
        let until = clock::now_ms() + linger;
        self.linger_until = Some(until);
        if let Some(l) = &self.link {
            info!("wifi: lingering {} s on {} (power save)", linger / 1000, l.ssid);
            let p = Phase::Lingering { ssid: l.ssid.clone(), ip: l.lease.ip, until };
            self.set_phase(p);
        }
    }

    fn timers(&mut self) {
        let now = clock::now_ms();
        if self.linger_until.is_some_and(|t| now >= t) {
            self.linger_until = None;
            if self.leases.is_empty() {
                self.teardown("linger over");
            }
        }
        if self.reconnect_until.is_some_and(|t| now >= t) {
            self.reconnect_until = None;
            warn!("wifi: the network did not come back within {} s", RECONNECT_MS / 1000);
            self.teardown("lost the network");
            if let Ok(mut s) = self.shared.lock() {
                s.last_error = Some("lost the network".into());
            }
            self.say("Wi-Fi lost the network.", true);
        }
        if self.link.as_ref().is_some_and(|l| l.associated && now >= l.renew_at) {
            self.renew(now);
        }
    }

    /// RENEWING (and, past T2, REBINDING; both are a broadcast REQUEST with ciaddr here).
    fn renew(&mut self, now: u64) {
        let Some(l) = self.link.as_mut() else { return };
        match l.dhcp.renew(&l.lease, &self.abort) {
            Ok(Some(new)) => {
                info!("dhcp: renewed {} for {} s", new.ip, new.lease_s);
                l.lease = new;
                l.lease_at = now;
                l.renew_at = now + l.lease.t1_s as u64 * 1000;
                let c = handoff::Cached {
                    ssid_hex: l.ssid_hex.clone(),
                    bssid: l.bssid.clone(),
                    freq: l.freq,
                    lease: l.lease.clone(),
                    wall_s: handoff::wall_s(),
                };
                if let Err(e) = handoff::save_cache(&c) {
                    warn!("wifi: lease cache: {e:#}");
                }
            }
            Ok(None) => {
                warn!("dhcp: renew refused (NAK): address {} lost", l.lease.ip);
                self.readdress();
            }
            Err(e) if now >= l.expire_at() => {
                warn!("dhcp: lease expired ({e:#})");
                self.readdress();
            }
            Err(e) => {
                let left = l.expire_at().saturating_sub(now);
                let retry = RENEW_RETRY_MS.min(left / 2).max(5_000);
                warn!(
                    "dhcp: renew failed ({e:#}); retry in {} s{}",
                    retry / 1000,
                    if now >= l.rebind_at() { " (rebinding)" } else { "" }
                );
                l.renew_at = now + retry;
            }
        }
    }

    /// The address is gone: get a new one (DISCOVER) and reconfigure netd.
    fn readdress(&mut self) {
        let r = (|| -> Result<()> {
            let net = self.net.as_mut().ok_or_else(|| anyhow!("no abyss-net"))?;
            let l = self.link.as_mut().ok_or_else(|| anyhow!("no link"))?;
            net.down(IFACE)?;
            let (lease, how) = l.dhcp.obtain(None, &self.abort)?;
            net.up(IFACE, &lease)?;
            info!("dhcp: new address {} ({})", lease.ip, how.as_str());
            let now = clock::now_ms();
            l.lease = lease;
            l.lease_at = now;
            l.renew_at = now + l.lease.t1_s as u64 * 1000;
            Ok(())
        })();
        if let Err(e) = r {
            warn!("wifi: re-address failed: {e:#}");
            self.teardown("address lost");
        }
    }

    fn on_events(&mut self) {
        let Some(w) = &self.wpa else { return };
        let events = w.events();
        for e in events {
            if e.contains("CTRL-EVENT-DISCONNECTED") {
                if let Some(l) = self.link.as_mut().filter(|l| l.associated) {
                    warn!("wifi: disconnected from {} ({})", l.ssid, e.trim());
                    l.associated = false;
                    let ssid = l.ssid.clone();
                    self.reconnect_until = Some(clock::now_ms() + RECONNECT_MS);
                    self.dns.clear();
                    self.set_phase(Phase::Reconnecting(ssid));
                }
            } else if e.contains("CTRL-EVENT-CONNECTED") && self.link.as_ref().is_some_and(|l| !l.associated) {
                info!("wifi: associated again ({})", e.trim());
                self.reconnect_until = None;
                self.after_reconnect();
            }
        }
    }

    /// Back on the network after a drop: confirm the address (INIT-REBOOT), else re-address.
    fn after_reconnect(&mut self) {
        let Some(l) = self.link.as_mut() else { return };
        l.associated = true;
        let ip = l.lease.ip;
        match l.dhcp.obtain(Some(ip), &self.abort) {
            Ok((lease, _)) if lease.ip == ip => {
                let now = clock::now_ms();
                l.lease = lease;
                l.lease_at = now;
                l.renew_at = now + l.lease.t1_s as u64 * 1000;
            }
            Ok((lease, _)) => {
                info!("dhcp: address changed {ip} -> {} after reconnect", lease.ip);
                let r = self.net.as_mut().map(|n| n.down(IFACE).and_then(|_| n.up(IFACE, &lease)));
                if let Some(Err(e)) = r {
                    warn!("wifi: netd reconfigure: {e:#}");
                }
                let now = clock::now_ms();
                if let Some(l) = self.link.as_mut() {
                    l.lease = lease;
                    l.lease_at = now;
                    l.renew_at = now + l.lease.t1_s as u64 * 1000;
                }
            }
            Err(e) => {
                warn!("wifi: no address after reconnect: {e:#}");
                self.teardown("no address after reconnect");
                return;
            }
        }
        if let Some(l) = &self.link {
            let p = if self.leases.is_empty() {
                Phase::Lingering { ssid: l.ssid.clone(), ip: l.lease.ip, until: self.linger_until.unwrap_or(0) }
            } else {
                Phase::On { ssid: l.ssid.clone(), ip: l.lease.ip }
            };
            self.set_phase(p);
        }
    }

    /// Network blocks for the hand-over entry `n`.
    fn add_network(
        &self,
        w: &Wpa,
        n: &Network,
        mac: Option<&str>,
        freq: Option<u32>,
        bssid: Option<&str>,
        priority: usize,
    ) -> Result<u32> {
        let id = w.add_network()?;
        w.set_network(id, "ssid", &n.ssid_hex)?;
        if n.hidden {
            w.set_network(id, "scan_ssid", "1")?;
        }
        let has = |s: Sec| n.security.contains(&s);
        // key_mgmt takes a space-separated list (the control interface passes the rest of the
        // line as the value).
        let (km, pmf) = if has(Sec::Psk) && has(Sec::Sae) {
            ("WPA-PSK SAE", 1)
        } else if has(Sec::Sae) {
            ("SAE", 2)
        } else if has(Sec::Psk) {
            ("WPA-PSK", 0)
        } else if has(Sec::Owe) {
            ("OWE", 2)
        } else {
            ("NONE", 0)
        };
        w.set_network(id, "key_mgmt", km)?;
        if pmf > 0 {
            w.set_network(id, "ieee80211w", &pmf.to_string())?;
        }
        if has(Sec::Psk) {
            // The precomputed PMK skips 4096 rounds of PBKDF2 per connect (battery-nerd §4.3).
            match (&n.pmk, &n.passphrase) {
                (Some(pmk), _) => w.set_network(id, "psk", pmk)?,
                (None, Some(p)) => w.set_network(id, "psk", &wpa::quoted(p)?)?,
                (None, None) => bail!("{}: no passphrase in the hand-over", n.ssid),
            }
        }
        if has(Sec::Sae) {
            let p = n.passphrase.as_deref().ok_or_else(|| anyhow!("{}: no SAE password", n.ssid))?;
            // An unquoted str value is hex: any byte is safe on the control interface.
            w.set_network(id, "sae_password", &wpa::hex(p.as_bytes()))?;
        }
        match mac {
            Some(m) => {
                w.set_network(id, "mac_addr", "3")?;
                w.set_network(id, "mac_value", m)?;
            }
            None => w.set_network(id, "mac_addr", "0")?,
        }
        if let Some(f) = freq {
            w.set_network(id, "scan_freq", &f.to_string())?;
        }
        if let Some(b) = bssid {
            if let Err(e) = w.set_network(id, "bssid_hint", b) {
                info!("wifi: bssid_hint not taken: {e:#}");
            }
        }
        w.set_network(id, "priority", &(MAX_NETWORKS - priority.min(MAX_NETWORKS)).to_string())?;
        Ok(id)
    }

    /// Polls STATUS until COMPLETED; returns the STATUS pairs. Early exit on a wrong password
    /// or (fast attempt) "network not found".
    fn wait_completed(&self, timeout_ms: u64, fast: bool) -> Result<Vec<(String, String)>> {
        let w = self.wpa.as_ref().ok_or_else(|| anyhow!("no supplicant"))?;
        let deadline = clock::now_ms() + timeout_ms;
        loop {
            if self.aborted() {
                bail!("aborted");
            }
            for e in w.events() {
                if e.contains("CTRL-EVENT-SSID-TEMP-DISABLED") && e.contains("WRONG_KEY") {
                    bail!("wrong Wi-Fi password (change it in Android, then enter Abyss again)");
                }
                if fast && e.contains("CTRL-EVENT-NETWORK-NOT-FOUND") {
                    bail!("not found on the last channel");
                }
            }
            let st = w.status()?;
            if kv(&st, "wpa_state") == Some("COMPLETED") {
                return Ok(st);
            }
            if clock::now_ms() >= deadline {
                bail!("not associated within {} s (state {})", timeout_ms / 1000, kv(&st, "wpa_state").unwrap_or("?"));
            }
            std::thread::sleep(Duration::from_millis(STATUS_POLL_MS));
        }
    }

    fn bring_up(&mut self) -> Result<()> {
        let t0 = clock::now_ms();
        self.set_phase(Phase::Starting);
        if let Ok(mut s) = self.shared.lock() {
            s.last_error = None;
        }
        let ho: Handoff = handoff::load().unwrap_or_default();
        if ho.networks.is_empty() {
            bail!("no saved Wi-Fi networks were handed over (connect to Wi-Fi in Android once, then enter Abyss again)");
        }
        let cache = handoff::load_cache();

        // Candidate order: Android's current network, abyssd's last one, the rest.
        let mut order: Vec<Network> = Vec::new();
        let push = |hex: Option<&str>, all: &[Network], order: &mut Vec<Network>| {
            if let Some(h) = hex {
                if let Some(n) = all.iter().find(|n| n.ssid_hex == h) {
                    if !order.iter().any(|o| o.ssid_hex == n.ssid_hex) {
                        order.push(n.clone());
                    }
                }
            }
        };
        push(ho.current.as_ref().map(|c| c.ssid_hex.as_str()), &ho.networks, &mut order);
        push(cache.as_ref().map(|c| c.ssid_hex.as_str()), &ho.networks, &mut order);
        for n in &ho.networks {
            push(Some(n.ssid_hex.as_str()), &ho.networks, &mut order);
        }
        order.truncate(MAX_NETWORKS);

        let factory = self.factory_mac;
        let mac_for = |n: &Network| -> Option<String> {
            if factory {
                return None;
            }
            match &ho.current {
                Some(c) if c.ssid_hex == n.ssid_hex && c.mac.is_some() => c.mac.clone(),
                _ => n.mac.clone(),
            }
        };

        control::set(PROP_WIFI, "on");
        let mut w = Wpa::connect(5_000)?;
        w.add_iface()?;
        if let Err(e) = w.attach() {
            warn!("wifi: no supplicant events ({e:#}); disconnects show up late");
        }
        if let Some(cc) = &ho.country {
            if let Err(e) = w.iface_ok(&format!("SET country {cc}")) {
                warn!("wifi: country {cc}: {e:#}");
            }
        }
        w.iface_ok("REMOVE_NETWORK all")?;
        self.wpa = Some(w);
        let t_sup = clock::now_ms();

        // Fast attempt: the first candidate on its last channel (fresher of the hand-over and
        // abyssd's own cache).
        let first = order[0].clone();
        let (mut freq, mut bssid) = (first.freq, first.bssid.clone());
        if let Some(c) = ho.current.as_ref().filter(|c| c.ssid_hex == first.ssid_hex) {
            freq = c.freq.or(freq);
            bssid = c.bssid.clone().or(bssid);
        }
        if let Some(c) = cache.as_ref().filter(|c| c.ssid_hex == first.ssid_hex) {
            let newer = c.wall_s * 1000 > ho.written_ms || freq.is_none();
            if newer {
                freq = c.freq.or(freq);
                bssid = c.bssid.clone().or(bssid);
            }
        }
        let mut added: Added = Vec::new();
        let mut status = None;
        if let Some(f) = freq {
            self.set_phase(Phase::Associating(first.ssid.clone()));
            let w = self.wpa.as_ref().ok_or_else(|| anyhow!("no supplicant"))?;
            let id = self.add_network(w, &first, mac_for(&first).as_deref(), Some(f), bssid.as_deref(), 0)?;
            w.iface_ok(&format!("SELECT_NETWORK {id}"))?;
            added.push((id, first.clone()));
            info!("wifi: fast attempt {} on {f} MHz{}", first.ssid, bssid.as_deref().map(|b| format!(" ({b})")).unwrap_or_default());
            match self.wait_completed(FAST_ASSOC_MS, true) {
                Ok(st) => status = Some(st),
                Err(e) if self.aborted() => return Err(e),
                Err(e) => info!("wifi: fast attempt failed: {e:#}; full scan"),
            }
        }
        if status.is_none() {
            self.set_phase(Phase::Associating(String::new()));
            let w = self.wpa.as_ref().ok_or_else(|| anyhow!("no supplicant"))?;
            w.iface_ok("REMOVE_NETWORK all")?;
            added.clear();
            for (i, n) in order.iter().enumerate() {
                match self.add_network(w, n, mac_for(n).as_deref(), None, None, i) {
                    Ok(id) => added.push((id, n.clone())),
                    Err(e) => warn!("wifi: skipping {}: {e:#}", n.ssid),
                }
            }
            if added.is_empty() {
                bail!("none of the saved networks can be used here");
            }
            w.iface_ok("ENABLE_NETWORK all")?;
            w.iface_ok("REASSOCIATE")?;
            status = Some(self.wait_completed(FULL_ASSOC_MS, false)?);
        }
        let st = status.ok_or_else(|| anyhow!("not associated"))?;
        let t_assoc = clock::now_ms();
        let id: Option<u32> = kv(&st, "id").and_then(|v| v.parse().ok());
        let net = added
            .iter()
            .find(|(i, _)| Some(*i) == id)
            .map(|(_, n)| n.clone())
            .ok_or_else(|| anyhow!("associated to a network we did not add (id {id:?})"))?;
        let mac = kv(&st, "address").and_then(mac_bytes).ok_or_else(|| anyhow!("no own MAC in STATUS"))?;
        let cur_bssid = kv(&st, "bssid").map(str::to_string);
        let cur_freq = kv(&st, "freq").and_then(|f| f.parse().ok());
        info!(
            "wifi: associated {} bssid {} {} MHz key_mgmt {} in {} ms",
            net.ssid,
            cur_bssid.as_deref().unwrap_or("?"),
            cur_freq.unwrap_or(0),
            kv(&st, "key_mgmt").unwrap_or("?"),
            t_assoc - t_sup
        );

        // DHCP: INIT-REBOOT with the cached address of this network, if any.
        self.set_phase(Phase::Dhcp(net.ssid.clone()));
        let cached_ip = cache
            .as_ref()
            .filter(|c| c.ssid_hex == net.ssid_hex && c.valid())
            .map(|c| c.lease.ip)
            .or_else(|| ho.current.as_ref().filter(|c| c.ssid_hex == net.ssid_hex).and_then(|c| c.ip));
        let mut client = dhcp::Client::open(IFACE, mac, ho.vendor_class.clone())?;
        let (lease, how) = client.obtain(cached_ip, &self.abort)?;
        let t_dhcp = clock::now_ms();

        let mut nc = NetCfg::connect(3_000)?;
        nc.up(IFACE, &lease)?;
        self.net = Some(nc);
        let t_net = clock::now_ms();
        if let Some(w) = &self.wpa {
            if let Err(e) = w.iface_ok("SET ps 1") {
                info!("wifi: power save: {e:#}");
            }
        }
        let c = handoff::Cached {
            ssid_hex: net.ssid_hex.clone(),
            bssid: cur_bssid.clone(),
            freq: cur_freq,
            lease: lease.clone(),
            wall_s: handoff::wall_s(),
        };
        if let Err(e) = handoff::save_cache(&c) {
            warn!("wifi: lease cache: {e:#}");
        }
        info!(
            "wifi: on {} {}/{} gw {} dns {:?} ({}) in {} ms: supplicant {} ms, assoc {} ms, dhcp {} ms, netd {} ms",
            net.ssid,
            lease.ip,
            lease.prefix,
            lease.gateway.map(|g| g.to_string()).unwrap_or_else(|| "none".into()),
            lease.dns,
            how.as_str(),
            t_net - t0,
            t_sup - t0,
            t_assoc - t_sup,
            t_dhcp - t_assoc,
            t_net - t_dhcp
        );
        self.dns.clear();
        let now = clock::now_ms();
        self.link = Some(Link {
            ssid: net.ssid.clone(),
            ssid_hex: net.ssid_hex.clone(),
            bssid: cur_bssid,
            freq: cur_freq,
            dhcp: client,
            renew_at: now + lease.t1_s as u64 * 1000,
            lease_at: now,
            lease,
            associated: true,
        });
        if let Some(l) = &self.link {
            self.set_phase(Phase::On { ssid: l.ssid.clone(), ip: l.lease.ip });
        }
        Ok(())
    }

    /// Whatever is up goes down; always ends in Phase::Off with the services stopped.
    fn teardown(&mut self, why: &str) {
        if self.wpa.is_none() && self.net.is_none() && self.link.is_none() {
            if control::prop_str(PROP_WIFI).as_deref() == Some("on") {
                control::set(PROP_WIFI, "off");
            }
            self.set_phase(Phase::Off);
            return;
        }
        info!("wifi: off ({why})");
        self.set_phase(Phase::Stopping);
        self.linger_until = None;
        self.reconnect_until = None;
        if let Some(n) = self.net.as_mut() {
            if let Err(e) = n.down(IFACE) {
                warn!("wifi: netd down: {e:#}");
            }
        }
        if let Some(w) = self.wpa.as_ref() {
            let _ = w.iface("DISCONNECT");
            if let Err(e) = w.remove_iface() {
                warn!("wifi: {e:#}");
            }
        }
        self.link = None;
        self.net = None;
        self.wpa = None;
        self.dns.clear();
        control::set(PROP_WIFI, "off");
        self.set_phase(Phase::Off);
    }

    /// The test lease is already held (its Acquire ran first): resolve + GET, then release.
    fn run_test(&mut self, lease: Lease) {
        let t0 = clock::now_ms();
        info!("wifi: test with lease {}", lease.id());
        let result = self.test_steps(t0);
        // Through the public API: the Release is queued and handled on the next loop turn.
        lease.release(None);
        let r = match result {
            Ok((code, line)) => {
                let ok = code == 204;
                let secs = (clock::now_ms() - t0) as f64 / 1000.0;
                TestResult {
                    ok,
                    short: if ok { format!("OK 204 · {secs:.1} s") } else { format!("HTTP {code}") },
                    line: if ok { line } else { format!("{line} (expected 204: a captive portal?)") },
                }
            }
            Err(e) => TestResult { ok: false, short: "failed".into(), line: format!("Failed: {e:#}") },
        };
        info!("wifi: test {}: {}", if r.ok { "OK" } else { "FAILED" }, r.line);
        self.say(format!("Wi-Fi test: {}", r.line), !r.ok);
        if let Ok(mut s) = self.shared.lock() {
            s.test_running = false;
            s.last_test = Some(r);
        }
        self.notify();
    }

    fn test_steps(&mut self, t0: u64) -> Result<(u16, String)> {
        let (ssid, ip, dns) = match &self.link {
            Some(l) if l.associated => (l.ssid.clone(), l.lease.ip, l.lease.dns.clone()),
            _ => {
                let e = self.shared.lock().ok().and_then(|s| s.last_error.clone());
                bail!("{}", e.unwrap_or_else(|| "Wi-Fi did not come up".into()));
            }
        };
        let t_up = clock::now_ms();
        let (host, port, path) = http::split_url(&self.test_url)?;
        let ips = self.dns.resolve(&host, &dns, IFACE)?;
        let t_dns = clock::now_ms();
        let code = http::get_status(ips[0], port, &host, &path, IFACE)?;
        let t_http = clock::now_ms();
        let line = format!(
            "{ssid} · {ip} · HTTP {code} in {:.1} s (Wi-Fi up after {:.1} s, DNS {} ms, GET {} ms)",
            (t_http - t0) as f64 / 1000.0,
            (t_up - t0) as f64 / 1000.0,
            t_dns - t_up,
            t_http - t_dns
        );
        Ok((code, line))
    }
}
