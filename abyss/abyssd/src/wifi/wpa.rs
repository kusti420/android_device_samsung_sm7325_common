//! wpa_supplicant's control interface (text protocol), as a client.
//!
//! Android's own supplicant service (`wpa_supplicant`, vendor
//! /vendor/etc/init/android.hardware.wifi.supplicant-service.rc) runs with only the global control
//! interface `-g@android:wpa_wlan0`, i.e. the init socket /dev/socket/wpa_wlan0 (SOCK_DGRAM,
//! wifi:wifi 0660, label wpa_socket). abyss.rc starts it on `sys.abyss.wifi=on`. abyssd then adds
//! wlan0 with `INTERFACE_ADD` (no config file: a blank in-memory config) and drives it with
//! `IFNAME=wlan0 <command>`, which the global interface forwards to the interface.
//!
//! Two sockets, each bound to its own abstract name so the supplicant can answer
//! (`sendto` the sender's address): one for request/reply, one ATTACHed for events
//! ("IFNAME=wlan0 <3>CTRL-EVENT-DISCONNECTED ...").

use std::io::ErrorKind;
use std::os::android::net::SocketAddrExt;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use log::{debug, info};

use crate::clock;

pub const SOCKET: &str = "/dev/socket/wpa_wlan0";
pub const IFACE: &str = "wlan0";
const REPLY_MAX: usize = 4096;

static SEQ: AtomicU32 = AtomicU32::new(0);

fn bound_socket(tag: &str) -> Result<UnixDatagram> {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!("abyssd.wpa.{}.{tag}.{n}", std::process::id());
    let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
    let s = UnixDatagram::bind_addr(&addr).with_context(|| format!("bind @{name}"))?;
    s.connect(SOCKET).with_context(|| format!("connect {SOCKET}"))?;
    Ok(s)
}

pub struct Wpa {
    cmd: UnixDatagram,
    ev: Option<UnixDatagram>,
}

impl Wpa {
    /// Connects to the global control interface, retrying until `PING` answers `PONG` or
    /// `timeout_ms` passes (the service was just started by init).
    pub fn connect(timeout_ms: u64) -> Result<Wpa> {
        let deadline = clock::now_ms() + timeout_ms;
        let mut last = anyhow!("not tried");
        loop {
            match bound_socket("cmd").and_then(|s| {
                let w = Wpa { cmd: s, ev: None };
                let r = w.request("PING", 500)?;
                if r.trim() == "PONG" {
                    Ok(w)
                } else {
                    Err(anyhow!("PING answered {r:?}"))
                }
            }) {
                Ok(w) => return Ok(w),
                Err(e) => last = e,
            }
            if clock::now_ms() >= deadline {
                return Err(last.context("wpa_supplicant control socket"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// One command, one reply. Stale replies (from a request that timed out) are dropped first.
    pub fn request(&self, cmd: &str, timeout_ms: u64) -> Result<String> {
        self.cmd.set_nonblocking(true)?;
        let mut buf = vec![0u8; REPLY_MAX];
        while self.cmd.recv(&mut buf).is_ok() {}
        self.cmd.set_nonblocking(false)?;
        self.cmd.set_read_timeout(Some(Duration::from_millis(timeout_ms.max(1))))?;
        self.cmd.send(cmd.as_bytes()).with_context(|| format!("send {}", redact(cmd)))?;
        match self.cmd.recv(&mut buf) {
            Ok(n) => {
                let r = String::from_utf8_lossy(&buf[..n]).into_owned();
                debug!("wpa: {} -> {}", redact(cmd), r.trim_end());
                Ok(r)
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                bail!("no reply to {} within {timeout_ms} ms", redact(cmd))
            }
            Err(e) => Err(e).with_context(|| format!("recv for {}", redact(cmd))),
        }
    }

    /// `IFNAME=wlan0 <cmd>`.
    pub fn iface(&self, cmd: &str) -> Result<String> {
        self.request(&format!("IFNAME={IFACE} {cmd}"), 3_000)
    }

    /// `IFNAME=wlan0 <cmd>` that must answer OK.
    pub fn iface_ok(&self, cmd: &str) -> Result<()> {
        let r = self.iface(cmd)?;
        if r.trim() == "OK" {
            Ok(())
        } else {
            bail!("{} -> {}", redact(cmd), r.trim())
        }
    }

    /// Adds wlan0 to the supplicant (blank config, nl80211) unless it is already there.
    /// The supplicant's nl80211 driver brings the interface up (and down again on removal).
    pub fn add_iface(&self) -> Result<()> {
        let list = self.request("INTERFACES", 2_000)?;
        if list.lines().any(|l| l.trim() == IFACE) {
            info!("wpa: {IFACE} already added");
            return Ok(());
        }
        // <ifname>TAB<confname>TAB<driver>TAB<ctrl_interface>TAB<driver_param>TAB<bridge>
        let r = self.request(&format!("INTERFACE_ADD {IFACE}\t\tnl80211\t\t\t"), 8_000)?;
        if r.trim() != "OK" {
            bail!("INTERFACE_ADD {IFACE} -> {}", r.trim());
        }
        info!("wpa: {IFACE} added");
        Ok(())
    }

    pub fn remove_iface(&self) -> Result<()> {
        let r = self.request(&format!("INTERFACE_REMOVE {IFACE}"), 3_000)?;
        if r.trim() != "OK" {
            bail!("INTERFACE_REMOVE -> {}", r.trim());
        }
        Ok(())
    }

    /// `ADD_NETWORK` → the new network id.
    pub fn add_network(&self) -> Result<u32> {
        let r = self.iface("ADD_NETWORK")?;
        r.trim().parse::<u32>().map_err(|_| anyhow!("ADD_NETWORK -> {}", r.trim()))
    }

    pub fn remove_network(&self, id: u32) -> Result<()> {
        self.iface_ok(&format!("REMOVE_NETWORK {id}"))
    }

    /// `SET_NETWORK <id> <name> <value>`.
    pub fn set_network(&self, id: u32, name: &str, value: &str) -> Result<()> {
        self.iface_ok(&format!("SET_NETWORK {id} {name} {value}"))
    }

    /// `STATUS` as key=value pairs.
    pub fn status(&self) -> Result<Vec<(String, String)>> {
        let r = self.iface("STATUS")?;
        Ok(r.lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect())
    }

    /// Opens the event socket and ATTACHes it to the global interface.
    pub fn attach(&mut self) -> Result<()> {
        if self.ev.is_some() {
            return Ok(());
        }
        let s = bound_socket("ev")?;
        s.set_read_timeout(Some(Duration::from_millis(1_000)))?;
        s.send(b"ATTACH")?;
        let mut buf = [0u8; 64];
        let n = s.recv(&mut buf).context("ATTACH reply")?;
        if buf[..n].trim_ascii() != b"OK" {
            bail!("ATTACH -> {}", String::from_utf8_lossy(&buf[..n]));
        }
        s.set_nonblocking(true)?;
        self.ev = Some(s);
        Ok(())
    }

    pub fn event_fd(&self) -> Option<BorrowedFd<'_>> {
        self.ev.as_ref().map(|s| s.as_fd())
    }

    /// Every queued event line, e.g. "IFNAME=wlan0 <3>CTRL-EVENT-CONNECTED - Connection to ...".
    pub fn events(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Some(s) = &self.ev else { return out };
        let mut buf = vec![0u8; REPLY_MAX];
        while let Ok(n) = s.recv(&mut buf) {
            out.push(String::from_utf8_lossy(&buf[..n]).into_owned());
        }
        out
    }

    pub fn detach(&mut self) {
        if let Some(s) = self.ev.take() {
            let _ = s.send(b"DETACH");
        }
    }
}

impl Drop for Wpa {
    fn drop(&mut self) {
        self.detach();
    }
}

/// Never log secrets: SET_NETWORK / SET_CRED values of secret fields are replaced
/// (netblock.rs `is_secret`: passwords, PSK, SAE password, EAP identities, WEP keys, DPP keys).
pub fn redact(cmd: &str) -> String {
    super::netblock::redact(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts() {
        assert_eq!(redact("IFNAME=wlan0 SET_NETWORK 0 psk \"secret\""), "IFNAME=wlan0 SET_NETWORK 0 psk <hidden>");
        assert_eq!(redact("IFNAME=wlan0 SET_NETWORK 0 ssid 4142"), "IFNAME=wlan0 SET_NETWORK 0 ssid 4142");
    }
}
