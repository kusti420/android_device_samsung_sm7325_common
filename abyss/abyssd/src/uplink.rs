//! The network the radio and the modules get inside Abyss: Wi-Fi first, mobile data when there
//! is no known Wi-Fi (abyss-design reports/impl-mobile-data.md §3).
//!
//! `persist.abyss.net.mode` (Settings › Abyss › Internet in Abyss):
//! - `wifi`: Wi-Fi leases only (wifi_net.rs), as before.
//! - `wifi_mobile` (default): a Wi-Fi lease per hold; when Wi-Fi cannot come up (no known
//!   network, wrong key, no address) or is lost, ONE mobile-data lease serves every hold, and
//!   Wi-Fi is not tried again for `persist.abyss.net.wifi_retry_s` (default 600), so a radio
//!   burst on the go does not first spend 5-20 s looking for home. After that window the next
//!   hold tries Wi-Fi again alongside the running mobile call and drops mobile once Wi-Fi is up.
//! - `mobile`: mobile-data leases only.
//!
//! Event-driven, no thread of its own: the decision is made when a hold is taken, when a
//! connection is opened and when a hold ends. Each new socket is bound to the interface that is
//! up at that moment (wlan0, else rmnet_dataN).

use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use log::info;

use crate::http::{Conn, Resolve, TcpTransport, Transport, Url};
use crate::radio_source::{NetHold, Network};
use crate::wifi::dns;
use crate::wifi::{LinkState as WifiState, Leaser as WifiLeaser};
use crate::wifi_net::WifiNet;
use crate::{clock, control, mobile};

pub const PROP_MODE: &str = "persist.abyss.net.mode";
const PROP_WIFI_RETRY: &str = "persist.abyss.net.wifi_retry_s";
/// The radio format to ask for on mobile data ("same" = no change). Default mp3-128: FLAC is
/// ~450 MB/h, mp3-128 ~58 MB/h.
pub const PROP_MOBILE_FORMAT: &str = "persist.abyss.net.mobile_format";
const DEFAULT_MOBILE_FORMAT: &str = "same";
const WIFI_RETRY_DEFAULT_S: u64 = 600;
/// A Wi-Fi lease that shows "failed" this long after it was taken really failed (before that it
/// may still be the previous attempt's state, not yet replaced by the worker).
const WIFI_FAIL_GRACE_MS: u64 = 1_500;
const WAIT_STEP_MS: u64 = 100;

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
    wifi: Option<Box<dyn NetHold>>,
    since: u64,
}

struct State {
    holds: HashMap<u64, HoldState>,
    next_id: u64,
    mobile_lease: Option<mobile::Lease>,
    /// Boot ms until which Wi-Fi is not tried (it just failed).
    wifi_skip_until: u64,
}

struct Inner {
    mode: Mode,
    wifi_retry_ms: u64,
    wifi_net: WifiNet,
    wifi: WifiLeaser,
    mobile: mobile::Leaser,
    state: Mutex<State>,
    dns: Mutex<dns::Resolver>,
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
        let Ok(mut st) = self.state.lock() else { return 0 };
        st.next_id += 1;
        let id = st.next_id;
        let now = clock::now_ms();
        let try_wifi = match self.mode {
            Mode::Wifi => true,
            Mode::Mobile => false,
            Mode::WifiMobile => now >= st.wifi_skip_until,
        };
        let wifi = try_wifi.then(|| self.wifi_net.acquire(holder));
        if !try_wifi && st.mobile_lease.is_none() {
            st.mobile_lease = Some(self.mobile.acquire("uplink"));
        }
        st.holds.insert(id, HoldState { wifi, since: now });
        self.settle(&mut st);
        id
    }

    fn release(&self, id: u64, next_use_in_ms: Option<u64>) {
        let Ok(mut st) = self.state.lock() else { return };
        if let Some(h) = st.holds.remove(&id) {
            if let Some(w) = h.wifi {
                w.release(next_use_in_ms);
            }
        }
        if st.holds.is_empty() {
            if let Some(m) = st.mobile_lease.take() {
                m.release(next_use_in_ms);
            }
        } else {
            self.settle(&mut st);
        }
    }

    /// The interface to bind to right now, waiting up to `timeout` while one is coming up.
    fn route(&self, timeout: Duration) -> Route {
        let deadline = clock::now_ms() + timeout.as_millis() as u64;
        loop {
            if let Ok(mut st) = self.state.lock() {
                self.settle(&mut st);
            }
            let wifi = self.wifi.state();
            if wifi == WifiState::Up {
                return Route::Wifi;
            }
            if let mobile::LinkState::Up(ifname) = self.mobile.state() {
                return Route::Mobile(ifname);
            }
            let coming = wifi == WifiState::Coming
                || matches!(self.mobile.state(), mobile::LinkState::Coming)
                || self.state.lock().map(|st| st.holds.values().any(|h| h.wifi.is_some())).unwrap_or(false)
                    && wifi == WifiState::Failed;
            if !coming || clock::now_ms() >= deadline {
                // Let the Wi-Fi transport fail the normal way; the caller retries.
                return Route::Wifi;
            }
            std::thread::sleep(Duration::from_millis(WAIT_STEP_MS));
        }
    }

    fn resolve_mobile(&self, name: &str, ifname: &str) -> io::Result<Vec<Ipv4Addr>> {
        let servers = self.mobile.dns();
        if servers.is_empty() {
            return Err(io::Error::new(ErrorKind::NotFound, "mobile data has no DNS server"));
        }
        let mut r = self.dns.lock().map_err(|_| io::Error::other("dns lock"))?;
        r.resolve(name, &servers, ifname).map_err(|e| io::Error::new(ErrorKind::NotFound, format!("{e:#}")))
    }
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
        info!("uplink: mode {mode:?} (Wi-Fi retry {} s, mobile format {:?})", wifi_retry_ms / 1000, format_pref());
        let inner = Arc::new(Inner {
            mode,
            wifi_retry_ms,
            wifi_net: WifiNet::new(wifi.clone()),
            wifi,
            mobile,
            state: Mutex::new(State { holds: HashMap::new(), next_id: 0, mobile_lease: None, wifi_skip_until: 0 }),
            dns: Mutex::new(dns::Resolver::new()),
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
        match self.inner.route(timeout) {
            Route::Wifi => self.inner.wifi_net.transport().connect(url, timeout),
            Route::Mobile(ifname) => {
                let inner = self.inner.clone();
                let dev = ifname.clone();
                let resolve: Resolve = Arc::new(move |name: &str| inner.resolve_mobile(name, &dev));
                TcpTransport { bind_device: Some(ifname), resolve: Some(resolve) }.connect(url, timeout)
            }
        }
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
            matches!(inner.mobile.state(), mobile::LinkState::Up(_))
                || inner.state.lock().map(|st| clock::now_ms() < st.wifi_skip_until).unwrap_or(false)
        }
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
        assert_eq!(verdict(m, Failed, Some(200), false), Verdict::Stay);
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
}
