//! The network the radio and the modules get inside Abyss: Wi-Fi first, mobile data when there
//! is no known Wi-Fi (abyss-design reports/impl-mobile-data.md §3).
//!
//! `persist.abyss.net.mode` (Settings › Abyss › Internet in Abyss):
//! - `wifi`: Wi-Fi leases only, as before.
//! - `wifi_mobile` (default): a Wi-Fi lease per hold; when Wi-Fi cannot come up (no known
//!   network, wrong key, no address) or is lost, ONE mobile-data lease serves every hold, and
//!   Wi-Fi is not tried again for `persist.abyss.net.wifi_retry_s` (default 600), so a radio
//!   burst on the go does not first spend 5-20 s looking for home. After that window the next
//!   hold tries Wi-Fi again alongside the running mobile call and drops mobile once Wi-Fi is up.
//! - `mobile`: mobile-data leases only.
//!
//! Event-driven, no thread of its own: the decision is made when a hold is taken, when a
//! connection is opened and when a hold ends. Each new socket is bound to the interface that is
//! up at that moment (wlan0, else rmnet_dataN). A connect while the link is still coming up
//! waits for the Wi-Fi/mobile workers' change signal ([`changed`]), not a poll step, so the
//! first request goes out as soon as the address is there.
//!
//! **Switching window** (`persist.abyss.net.switch_window_s`, default 90, 0 = off): after the
//! person acts on a module (a station picked or stepped, a module screen opened or scrolled:
//! [`touch`] from main.rs, and every `module-get`), whatever uplink is up stays up that long
//! after the last hold ends (Wi-Fi associated in power save, the mobile call up), even when the
//! next burst is minutes away; a station switch inside it starts at once. On mobile data the
//! window also keeps the uplink on mobile (no Wi-Fi retry inside it). Outside the window the
//! normal policy applies (Wi-Fi off past linger_max; mobile: call down, modem registered).
//!
//! Names: resolved by abyssd's own stub resolver (wifi/dns.rs) against the servers of the link
//! in use (the DHCP lease's on Wi-Fi, the call's on mobile), one TTL cache per link that lives
//! as long as those servers stay the same (so across bursts and station switches).

use std::collections::HashMap;
use std::io::{self, ErrorKind, Read, Write};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use log::info;

use crate::http::{Aborter, Conn, Resolve, TcpTransport, Transport, Url};
use crate::radio_source::{NetHold, Network};
use crate::wifi::dns;
use crate::wifi::{Lease as WifiLease, LinkState as WifiState, Leaser as WifiLeaser};
use crate::{clock, control, mobile};

pub const PROP_MODE: &str = "persist.abyss.net.mode";
const PROP_WIFI_RETRY: &str = "persist.abyss.net.wifi_retry_s";
/// The switching window (seconds, default 90, 0 = off, at most 600).
pub const PROP_SWITCH_WINDOW: &str = "persist.abyss.net.switch_window_s";
const SWITCH_WINDOW_DEFAULT_S: u64 = 90;
/// A touch re-sends the window to the workers only when it moved this much (key repeats).
const WARM_RESEND_MS: u64 = 5_000;
/// The radio format to ask for on mobile data ("same" = no change). Default mp3-128: FLAC is
/// ~450 MB/h, mp3-128 ~58 MB/h.
pub const PROP_MOBILE_FORMAT: &str = "persist.abyss.net.mobile_format";
const DEFAULT_MOBILE_FORMAT: &str = "same";
const WIFI_RETRY_DEFAULT_S: u64 = 600;
/// A Wi-Fi lease that shows "failed" this long after it was taken really failed. The workers
/// report a lease they have not handled yet as "coming", so this is only a safety margin.
const WIFI_FAIL_GRACE_MS: u64 = 200;
/// The longest single wait for a change signal (the fail grace above is time, not an event).
const WAIT_STEP_MS: u64 = 250;
/// The Wi-Fi interface (wifi/wpa.rs IFACE).
const WIFI_IFACE: &str = "wlan0";

static CHANGED: Mutex<u64> = Mutex::new(0);
static CHANGED_CV: Condvar = Condvar::new();

/// The Wi-Fi or mobile worker changed phase (or handled a lease): wakes connects waiting for a
/// link.
pub fn changed() {
    if let Ok(mut g) = CHANGED.lock() {
        *g = g.wrapping_add(1);
    }
    CHANGED_CV.notify_all();
}

/// Bumped whenever the Wi-Fi link / the mobile call goes down (wifi/mod.rs teardown,
/// mobile/mod.rs end_call).
static EPOCH_WIFI: AtomicU64 = AtomicU64::new(0);
static EPOCH_MOBILE: AtomicU64 = AtomicU64::new(0);

/// The Wi-Fi link (`wifi` true) or the mobile call went down: connections made over it before
/// are dead even if the same address comes back (the server's FIN was lost; behind carrier NAT
/// a reused socket can even black-hole until the read timeout).
pub fn link_down(wifi: bool) {
    if wifi { &EPOCH_WIFI } else { &EPOCH_MOBILE }.fetch_add(1, Ordering::Relaxed);
}

/// A connection made through the uplink. A keep-alive reuse after its link went down fails at
/// the first write, so http.rs's reuse path takes a fresh connection at once.
struct LinkConn {
    inner: Box<dyn Conn>,
    epoch: &'static AtomicU64,
    at: u64,
}

impl Read for LinkConn {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.inner.read(b)
    }
}

impl Write for LinkConn {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if self.epoch.load(Ordering::Relaxed) != self.at {
            return Err(io::Error::new(ErrorKind::ConnectionReset, "the link went down since this connection was made"));
        }
        self.inner.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Conn for LinkConn {
    fn set_timeouts(&self, t: Duration) -> io::Result<()> {
        self.inner.set_timeouts(t)
    }
    fn aborter(&self) -> Option<Aborter> {
        self.inner.aborter()
    }
}

fn change_gen() -> u64 {
    CHANGED.lock().map(|g| *g).unwrap_or(0)
}

/// Waits until [`changed`] after `seen`, at most `ms`.
fn wait_change(seen: u64, ms: u64) {
    let Ok(g) = CHANGED.lock() else { return };
    let _ = CHANGED_CV.wait_timeout_while(g, Duration::from_millis(ms), |g| *g == seen);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Wifi,
    WifiMobile,
    Mobile,
}

impl Mode {
    pub fn parse(v: Option<&str>) -> Mode {
        match v.map(str::trim) {
            Some("wifi") => Mode::Wifi,
            Some("mobile") => Mode::Mobile,
            _ => Mode::WifiMobile,
        }
    }
}

struct HoldState {
    wifi: Option<WifiLease>,
    since: u64,
}

struct State {
    holds: HashMap<u64, HoldState>,
    next_id: u64,
    mobile_lease: Option<mobile::Lease>,
    /// Boot ms until which Wi-Fi is not tried (it just failed).
    wifi_skip_until: u64,
    /// Boot ms until which the switching window keeps the uplink up.
    window_until: u64,
    /// The window end last sent to the workers.
    warm_sent: u64,
}

/// One resolver cache per link, dropped when the link's DNS servers change.
struct Dns {
    r: dns::Resolver,
    servers: Vec<Ipv4Addr>,
}

impl Dns {
    fn new() -> Mutex<Dns> {
        Mutex::new(Dns { r: dns::Resolver::new(), servers: Vec::new() })
    }
}

struct Inner {
    mode: Mode,
    wifi_retry_ms: u64,
    window_ms: u64,
    wifi: WifiLeaser,
    mobile: mobile::Leaser,
    state: Mutex<State>,
    dns_mobile: Mutex<Dns>,
    dns_wifi: Mutex<Dns>,
}

static ON_MOBILE: OnceLock<Arc<Inner>> = OnceLock::new();

/// What the uplink should do now (pure; unit-tested).
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Keep going as is.
    Stay,
    /// Wi-Fi failed: drop the Wi-Fi leases, use mobile data, skip Wi-Fi for a while.
    ToMobile,
    /// Wi-Fi is up again: the mobile lease can go.
    DropMobile,
}

fn verdict(mode: Mode, wifi: WifiState, oldest_wifi_hold_ms: Option<u64>, mobile_held: bool) -> Verdict {
    if mode != Mode::WifiMobile {
        return Verdict::Stay;
    }
    match (wifi, oldest_wifi_hold_ms) {
        (WifiState::Up, Some(_)) if mobile_held => Verdict::DropMobile,
        (WifiState::Failed, Some(age)) if age >= WIFI_FAIL_GRACE_MS => Verdict::ToMobile,
        _ => Verdict::Stay,
    }
}

impl Inner {
    /// Applies `verdict` (called with the state locked).
    fn settle(&self, st: &mut State) {
        let now = clock::now_ms();
        let oldest = st.holds.values().filter(|h| h.wifi.is_some()).map(|h| now.saturating_sub(h.since)).max();
        match verdict(self.mode, self.wifi.state(), oldest, st.mobile_lease.is_some()) {
            Verdict::Stay => {}
            Verdict::ToMobile => {
                info!(
                    "uplink: no Wi-Fi; mobile data for {} hold(s), Wi-Fi again in {} s",
                    st.holds.len(),
                    self.wifi_retry_ms / 1000
                );
                st.wifi_skip_until = now + self.wifi_retry_ms;
                for h in st.holds.values_mut() {
                    if let Some(w) = h.wifi.take() {
                        w.release(None);
                    }
                }
                if st.mobile_lease.is_none() {
                    st.mobile_lease = Some(self.mobile.acquire("uplink"));
                }
            }
            Verdict::DropMobile => {
                info!("uplink: Wi-Fi is back; mobile data can go");
                if let Some(m) = st.mobile_lease.take() {
                    m.release(None);
                }
            }
        }
    }

    fn acquire(&self, holder: &str) -> u64 {
        // A module screen fetching its data = the person is on it (opened, refreshed while
        // looking): the switching window starts.
        if holder == "module-get" {
            self.touch();
        }
        let Ok(mut st) = self.state.lock() else { return 0 };
        st.next_id += 1;
        let id = st.next_id;
        let now = clock::now_ms();
        let try_wifi = match self.mode {
            Mode::Wifi => true,
            Mode::Mobile => false,
            Mode::WifiMobile => now >= st.wifi_skip_until,
        };
        // Wi-Fi again after its retry window while mobile data is in use (call up, or the modem
        // registered between bursts): both at once. The hold uses mobile data at once and moves
        // to Wi-Fi when that comes up (settle: DropMobile), instead of waiting through a 5-20 s
        // search for a home network that is not there.
        let mobile_live = matches!(self.mobile.state(), mobile::LinkState::Up(_) | mobile::LinkState::Standby);
        let alongside = self.mode == Mode::WifiMobile && try_wifi && mobile_live && self.wifi.state() != WifiState::Up;
        let wifi = try_wifi.then(|| self.wifi.acquire(holder));
        if (!try_wifi || alongside) && st.mobile_lease.is_none() {
            st.mobile_lease = Some(self.mobile.acquire("uplink"));
        }
        st.holds.insert(id, HoldState { wifi, since: now });
        self.settle(&mut st);
        id
    }

    fn release(&self, id: u64, next_use_in_ms: Option<u64>) {
        let Ok(mut st) = self.state.lock() else { return };
        let warm = warm_ms(st.window_until, clock::now_ms());
        if let Some(h) = st.holds.remove(&id) {
            if let Some(w) = h.wifi {
                w.release_warm(next_use_in_ms, warm);
            }
        }
        if st.holds.is_empty() {
            if let Some(m) = st.mobile_lease.take() {
                m.release_warm(next_use_in_ms, warm);
            }
        } else {
            self.settle(&mut st);
        }
    }

    /// The person acted on a module: open (or extend) the switching window.
    fn touch(&self) {
        if self.window_ms == 0 {
            return;
        }
        let now = clock::now_ms();
        let until = now + self.window_ms;
        let Ok(mut st) = self.state.lock() else { return };
        st.window_until = st.window_until.max(until);
        // On mobile data because Wi-Fi just failed: stay on it while the person switches.
        if self.mode == Mode::WifiMobile && now < st.wifi_skip_until {
            st.wifi_skip_until = st.wifi_skip_until.max(until);
        }
        // No hold: re-arm a lingering link (a hold's release takes the window by itself).
        if st.holds.is_empty() && until >= st.warm_sent + WARM_RESEND_MS {
            st.warm_sent = until;
            self.wifi.warm(until);
            self.mobile.warm(until);
        }
    }

    /// The interface to bind to right now, waiting up to `timeout` while one is coming up.
    fn route(&self, timeout: Duration) -> Route {
        let deadline = clock::now_ms() + timeout.as_millis() as u64;
        loop {
            let seen = change_gen();
            let (wifi_wanted, mobile_wanted) = match self.state.lock() {
                Ok(mut st) => {
                    self.settle(&mut st);
                    (st.holds.values().any(|h| h.wifi.is_some()), st.mobile_lease.is_some())
                }
                Err(_) => (false, false),
            };
            let wifi = self.wifi.state();
            if wifi == WifiState::Up {
                return Route::Wifi;
            }
            let mobile = self.mobile.state();
            if let mobile::LinkState::Up(ifname) = mobile {
                return Route::Mobile(ifname);
            }
            let coming = wifi == WifiState::Coming
                || mobile == mobile::LinkState::Coming
                // Wi-Fi failed under a hold: settle() moves the holds to mobile data.
                || wifi_wanted && wifi == WifiState::Failed
                || mobile_wanted && mobile == mobile::LinkState::Standby;
            let now = clock::now_ms();
            if !coming || now >= deadline {
                // Let the Wi-Fi transport fail the normal way; the caller retries.
                return Route::Wifi;
            }
            wait_change(seen, (deadline - now).min(WAIT_STEP_MS));
        }
    }

    fn resolve(&self, which: &Mutex<Dns>, servers: Vec<Ipv4Addr>, name: &str, ifname: &str) -> io::Result<Vec<Ipv4Addr>> {
        if servers.is_empty() {
            return Err(io::Error::new(ErrorKind::NotFound, format!("no DNS server on {ifname}")));
        }
        let mut d = which.lock().map_err(|_| io::Error::other("dns lock"))?;
        if d.servers != servers {
            d.r.clear();
            d.servers = servers.clone();
        }
        d.r.resolve(name, &servers, ifname).map_err(|e| io::Error::new(ErrorKind::NotFound, format!("{e:#}")))
    }
}

/// What is left of the switching window at `now` (0 = closed).
fn warm_ms(window_until: u64, now: u64) -> u64 {
    window_until.saturating_sub(now)
}

enum Route {
    Wifi,
    Mobile(String),
}

pub struct Uplink {
    inner: Arc<Inner>,
    transport: Arc<dyn Transport>,
}

impl Uplink {
    pub fn new(wifi: WifiLeaser, mobile: mobile::Leaser) -> Uplink {
        let mode = Mode::parse(control::prop_str(PROP_MODE).as_deref());
        let wifi_retry_ms = control::prop_str(PROP_WIFI_RETRY)
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(WIFI_RETRY_DEFAULT_S)
            .min(7200)
            * 1000;
        let window_ms = control::prop_str(PROP_SWITCH_WINDOW)
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(SWITCH_WINDOW_DEFAULT_S)
            .min(600)
            * 1000;
        info!(
            "uplink: mode {mode:?} (Wi-Fi retry {} s, switching window {} s, mobile format {:?})",
            wifi_retry_ms / 1000,
            window_ms / 1000,
            format_pref()
        );
        let inner = Arc::new(Inner {
            mode,
            wifi_retry_ms,
            window_ms,
            wifi,
            mobile,
            state: Mutex::new(State {
                holds: HashMap::new(),
                next_id: 0,
                mobile_lease: None,
                wifi_skip_until: 0,
                window_until: 0,
                warm_sent: 0,
            }),
            dns_mobile: Dns::new(),
            dns_wifi: Dns::new(),
        });
        let _ = ON_MOBILE.set(inner.clone());
        let transport = Arc::new(UplinkTransport { inner: inner.clone() });
        Uplink { inner, transport }
    }
}

struct Hold {
    id: u64,
    inner: Arc<Inner>,
    done: bool,
}

impl NetHold for Hold {
    fn release(mut self: Box<Self>, next_use_in_ms: Option<u64>) {
        self.done = true;
        self.inner.release(self.id, next_use_in_ms);
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if !self.done {
            self.inner.release(self.id, None);
        }
    }
}

impl Network for Uplink {
    fn acquire(&self, holder: &str) -> Box<dyn NetHold> {
        let id = self.inner.acquire(holder);
        Box::new(Hold { id, inner: self.inner.clone(), done: false })
    }
    fn transport(&self) -> Arc<dyn Transport> {
        self.transport.clone()
    }
}

struct UplinkTransport {
    inner: Arc<Inner>,
}

impl Transport for UplinkTransport {
    fn connect(&self, url: &Url, timeout: Duration) -> io::Result<Box<dyn Conn>> {
        let (ifname, wifi) = match self.inner.route(timeout) {
            Route::Wifi => (WIFI_IFACE.to_string(), true),
            Route::Mobile(ifname) => (ifname, false),
        };
        let inner = self.inner.clone();
        let dev = ifname.clone();
        let resolve: Resolve = Arc::new(move |name: &str| {
            if wifi {
                inner.resolve(&inner.dns_wifi, inner.wifi.dns(), name, &dev)
            } else {
                inner.resolve(&inner.dns_mobile, inner.mobile.dns(), name, &dev)
            }
        });
        let epoch = if wifi { &EPOCH_WIFI } else { &EPOCH_MOBILE };
        let at = epoch.load(Ordering::Relaxed);
        let inner = TcpTransport { bind_device: Some(ifname), resolve: Some(resolve) }.connect(url, timeout)?;
        Ok(Box::new(LinkConn { inner, epoch, at }))
    }
}

fn format_pref() -> Option<String> {
    let f = control::prop_str(PROP_MOBILE_FORMAT).unwrap_or_else(|| DEFAULT_MOBILE_FORMAT.into());
    (f != "same").then_some(f)
}

/// True when the next stream will most likely go over mobile data: mode "mobile", a mobile
/// call is up, or Wi-Fi just failed (inside its retry window).
pub fn on_mobile() -> bool {
    let Some(inner) = ON_MOBILE.get() else { return false };
    match inner.mode {
        Mode::Wifi => false,
        Mode::Mobile => true,
        Mode::WifiMobile => {
            if inner.wifi.state() == WifiState::Up {
                return false;
            }
            matches!(inner.mobile.state(), mobile::LinkState::Up(_) | mobile::LinkState::Standby)
                || inner.state.lock().map(|st| clock::now_ms() < st.wifi_skip_until).unwrap_or(false)
        }
    }
}

/// The person acted on a module (main.rs: a station picked or stepped, a module screen used):
/// opens the switching window. Cheap and non-blocking (the key loop calls it).
pub fn touch() {
    if let Some(inner) = ON_MOBILE.get() {
        inner.touch();
    }
}

/// The audio format to prefer for a stream that starts now (None = the module's own choice).
/// For the radio module's format pick (modules.rs): use it when the station offers it.
#[allow(dead_code)] // wired by the radio module host (modules.rs owner)
pub fn mobile_format() -> Option<String> {
    if on_mobile() {
        format_pref()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        assert_eq!(Mode::parse(None), Mode::WifiMobile);
        assert_eq!(Mode::parse(Some("wifi")), Mode::Wifi);
        assert_eq!(Mode::parse(Some(" mobile ")), Mode::Mobile);
        assert_eq!(Mode::parse(Some("junk")), Mode::WifiMobile);
    }

    #[test]
    fn verdicts() {
        use WifiState::*;
        let m = Mode::WifiMobile;
        // A fresh lease may still see the last attempt's failure: wait out the grace.
        assert_eq!(verdict(m, Failed, Some(100), false), Verdict::Stay);
        assert_eq!(verdict(m, Failed, Some(2_000), false), Verdict::ToMobile);
        assert_eq!(verdict(m, Coming, Some(9_000), false), Verdict::Stay);
        assert_eq!(verdict(m, Up, Some(1), true), Verdict::DropMobile);
        assert_eq!(verdict(m, Up, Some(1), false), Verdict::Stay);
        // No Wi-Fi hold (already on mobile): nothing to decide.
        assert_eq!(verdict(m, Failed, None, true), Verdict::Stay);
        // Other modes never switch.
        assert_eq!(verdict(Mode::Wifi, Failed, Some(9_000), false), Verdict::Stay);
        assert_eq!(verdict(Mode::Mobile, Up, Some(1), true), Verdict::Stay);
    }

    #[test]
    fn window_left() {
        assert_eq!(warm_ms(0, 5_000), 0);
        assert_eq!(warm_ms(95_000, 5_000), 90_000);
        assert_eq!(warm_ms(4_000, 5_000), 0);
    }
}
