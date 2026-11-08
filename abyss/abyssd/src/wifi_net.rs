//! The network for modules and the radio inside Abyss: a Wi-Fi lease per use (wifi/mod.rs)
//! and sockets bound to wlan0 (like the Wi-Fi test in wifi/http.rs).
//!
//! Leases are taken from the radio's and the module fetches' own threads, so this uses
//! `wifi::Leaser`, a cloneable, thread-safe handle on the same lease API as
//! `Wifi::acquire` (hook patch in wifi/mod.rs).
//!
//! Inside abyssd the uplink (uplink.rs) now takes Wi-Fi leases itself (it needs
//! `Lease::release_warm` and its own resolver); this stays for the radio-host test tools.
#![allow(dead_code)]

use std::sync::Arc;

use crate::http::{TcpTransport, Transport};
use crate::radio_source::{NetHold, Network};
use crate::wifi::{Lease, Leaser};

/// The Wi-Fi interface Abyss brings up (wifi/wpa.rs IFACE).
const IFACE: &str = "wlan0";

pub struct WifiNet {
    leaser: Leaser,
    transport: Arc<dyn Transport>,
}

impl WifiNet {
    pub fn new(leaser: Leaser) -> WifiNet {
        // Addresses only: the radio and module URLs of this build are IPv4 literals. Names
        // need the Wi-Fi resolver (wifi/dns.rs) passed as `resolve` once it is shared.
        let transport = Arc::new(TcpTransport { bind_device: Some(IFACE.into()), resolve: None });
        WifiNet { leaser, transport }
    }
}

struct Hold(Lease);

impl NetHold for Hold {
    fn release(self: Box<Self>, next_use_in_ms: Option<u64>) {
        self.0.release(next_use_in_ms);
    }
}

impl Network for WifiNet {
    fn acquire(&self, holder: &str) -> Box<dyn NetHold> {
        Box::new(Hold(self.leaser.acquire(holder)))
    }
    fn transport(&self) -> Arc<dyn Transport> {
        self.transport.clone()
    }
}
