//! Saved networks handed over by Android, and abyssd's own lease cache.
//!
//! Hand-over: /data/misc/abyss/handoff/wifi.json, written by the entry app (org.ichthys.abyss,
//! WifiHandoff.java, system uid) right before it turns airplane mode on, and deleted by its
//! BootReceiver after the return (it holds passphrases). abyssd only reads it. Why this and not
//! WifiConfigStore.xml: that file is apex_wifi_data_file (/data/misc/apexdata/com.android.wifi),
//! private to the Wi-Fi mainline module, an XML format that changes between module versions,
//! and it lacks what the fast path needs (current BSSID/frequency, the lease, the country). The
//! app gets all of it from public/system Wi-Fi and connectivity APIs while they still run.
//!
//! Format (v1):
//! ```json
//! {"v":1, "written_ms":1731000000000, "country":"EE", "dhcp_vendor_class":"android-dhcp-16",
//!  "current":{"ssid_hex":"486f6d65","bssid":"aa:bb:cc:dd:ee:ff","freq":5180,
//!             "mac":"02:11:22:33:44:55","ip":"192.0.2.23","prefix":24,
//!             "gateway":"192.0.2.1","dns":["192.0.2.1"]},
//!  "networks":[{"ssid":"Home","ssid_hex":"486f6d65","hidden":false,"security":["psk","sae"],
//!               "passphrase":"...","pmk":"<64 hex>","mac":"02:11:22:33:44:55",
//!               "mac_policy":"randomized","bssid":"aa:bb:cc:dd:ee:ff","freq":5180}]}
//! ```
//! `current` is null when Android was not on Wi-Fi. `security` lists what the network accepts
//! (open, owe, psk, sae); enterprise and WEP networks are not handed over.
//!
//! Lease cache: /data/misc/abyss/out/wifi_lease.json (abyssd writes, one writer per file):
//! the last DHCP lease plus the BSSID/frequency of the last association, for INIT-REBOOT and
//! the one-channel scan next time.

use std::fs;
use std::io::Write;
use std::net::Ipv4Addr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use log::warn;
use serde_json::{json, Value};

use super::dhcp::Lease;

pub const HANDOFF: &str = "/data/misc/abyss/handoff/wifi.json";
pub const LEASE_CACHE: &str = "/data/misc/abyss/out/wifi_lease.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sec {
    Open,
    Owe,
    Psk,
    Sae,
}

#[derive(Clone, Debug)]
pub struct Network {
    pub ssid: String,
    pub ssid_hex: String,
    pub hidden: bool,
    pub security: Vec<Sec>,
    pub passphrase: Option<String>,
    pub pmk: Option<String>,
    /// None = factory MAC.
    pub mac: Option<String>,
    pub bssid: Option<String>,
    pub freq: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Current {
    pub ssid_hex: String,
    pub bssid: Option<String>,
    pub freq: Option<u32>,
    pub mac: Option<String>,
    pub ip: Option<Ipv4Addr>,
}

#[derive(Clone, Debug, Default)]
pub struct Handoff {
    pub written_ms: u64,
    pub country: Option<String>,
    pub vendor_class: Option<String>,
    pub current: Option<Current>,
    pub networks: Vec<Network>,
}

fn s(v: &Value) -> Option<String> {
    v.as_str().filter(|x| !x.is_empty()).map(str::to_string)
}

fn is_hex(x: &str, len: Option<usize>) -> bool {
    !x.is_empty() && x.len().is_multiple_of(2) && len.is_none_or(|l| x.len() == l) && x.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_mac(x: &str) -> bool {
    x.len() == 17 && x.split(':').count() == 6 && x.split(':').all(|p| is_hex(p, Some(2)))
}

pub fn parse(text: &str) -> Result<Handoff> {
    let v: Value = serde_json::from_str(text).context("wifi.json")?;
    let mut h = Handoff {
        written_ms: v["written_ms"].as_u64().unwrap_or(0),
        country: s(&v["country"]).filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic())),
        vendor_class: s(&v["dhcp_vendor_class"]),
        current: None,
        networks: Vec::new(),
    };
    if let Some(c) = v["current"].as_object() {
        let ssid_hex = c.get("ssid_hex").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
        if is_hex(&ssid_hex, None) {
            h.current = Some(Current {
                ssid_hex,
                bssid: c.get("bssid").and_then(s_opt).filter(|b| is_mac(b)),
                freq: c.get("freq").and_then(Value::as_u64).map(|f| f as u32).filter(|&f| f > 0),
                mac: c.get("mac").and_then(s_opt).filter(|m| is_mac(m)),
                ip: c.get("ip").and_then(Value::as_str).and_then(|x| x.parse().ok()),
            });
        }
    }
    for n in v["networks"].as_array().into_iter().flatten() {
        let ssid_hex = n["ssid_hex"].as_str().unwrap_or("").to_ascii_lowercase();
        if !is_hex(&ssid_hex, None) || ssid_hex.len() > 64 {
            continue;
        }
        let security: Vec<Sec> = n["security"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| match x.as_str()? {
                "open" => Some(Sec::Open),
                "owe" => Some(Sec::Owe),
                "psk" => Some(Sec::Psk),
                "sae" => Some(Sec::Sae),
                _ => None,
            })
            .collect();
        if security.is_empty() {
            continue;
        }
        let mac = match n["mac_policy"].as_str() {
            Some("factory") => None,
            _ => s(&n["mac"]).filter(|m| is_mac(m) && m != "02:00:00:00:00:00"),
        };
        h.networks.push(Network {
            ssid: s(&n["ssid"]).unwrap_or_else(|| ssid_hex.clone()),
            ssid_hex,
            hidden: n["hidden"].as_bool().unwrap_or(false),
            security,
            passphrase: s(&n["passphrase"]),
            pmk: s(&n["pmk"]).filter(|p| is_hex(p, Some(64))),
            mac,
            bssid: s(&n["bssid"]).filter(|b| is_mac(b)),
            freq: n["freq"].as_u64().map(|f| f as u32).filter(|&f| f > 0),
        });
    }
    Ok(h)
}

fn s_opt(v: &Value) -> Option<String> {
    s(v)
}

pub fn load() -> Option<Handoff> {
    match fs::read_to_string(HANDOFF) {
        Ok(t) => match parse(&t) {
            Ok(h) => Some(h),
            Err(e) => {
                warn!("wifi: {HANDOFF}: {e:#}");
                None
            }
        },
        Err(e) => {
            warn!("wifi: no hand-over {HANDOFF}: {e}");
            None
        }
    }
}

/// abyssd's last lease and association.
#[derive(Clone, Debug)]
pub struct Cached {
    pub ssid_hex: String,
    pub bssid: Option<String>,
    pub freq: Option<u32>,
    pub lease: Lease,
    /// Wall-clock seconds when the lease was granted (boot time does not survive a reboot).
    pub wall_s: u64,
}

impl Cached {
    /// Still inside the lease time?
    pub fn valid(&self) -> bool {
        wall_s() < self.wall_s + self.lease.lease_s as u64
    }
}

pub fn wall_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn ips(v: &Value) -> Vec<Ipv4Addr> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str()?.parse().ok()).collect()
}

pub fn load_cache() -> Option<Cached> {
    let t = fs::read_to_string(LEASE_CACHE).ok()?;
    let v: Value = serde_json::from_str(&t).ok()?;
    let lease_s = v["lease_s"].as_u64()? as u32;
    Some(Cached {
        ssid_hex: v["ssid_hex"].as_str()?.to_string(),
        bssid: s(&v["bssid"]),
        freq: v["freq"].as_u64().map(|f| f as u32),
        lease: Lease {
            ip: v["ip"].as_str()?.parse().ok()?,
            prefix: v["prefix"].as_u64()? as u8,
            gateway: v["gateway"].as_str().and_then(|g| g.parse().ok()),
            dns: ips(&v["dns"]),
            server: v["server"].as_str().and_then(|g| g.parse().ok()),
            lease_s,
            t1_s: v["t1_s"].as_u64().unwrap_or(lease_s as u64 / 2) as u32,
            t2_s: v["t2_s"].as_u64().unwrap_or(lease_s as u64 * 7 / 8) as u32,
            mtu: v["mtu"].as_u64().map(|m| m as u16),
        },
        wall_s: v["wall_s"].as_u64()?,
    })
}

pub fn save_cache(c: &Cached) -> Result<()> {
    let l = &c.lease;
    let v = json!({
        "v": 1,
        "ssid_hex": c.ssid_hex,
        "bssid": c.bssid,
        "freq": c.freq,
        "ip": l.ip.to_string(),
        "prefix": l.prefix,
        "gateway": l.gateway.map(|g| g.to_string()),
        "dns": l.dns.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
        "server": l.server.map(|g| g.to_string()),
        "lease_s": l.lease_s,
        "t1_s": l.t1_s,
        "t2_s": l.t2_s,
        "mtu": l.mtu,
        "wall_s": c.wall_s,
    });
    let tmp = format!("{LEASE_CACHE}.tmp");
    let mut f = fs::File::create(&tmp).with_context(|| format!("create {tmp}"))?;
    f.write_all(v.to_string().as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp, LEASE_CACHE).with_context(|| format!("rename to {LEASE_CACHE}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_handoff() {
        let t = r#"{"v":1,"written_ms":5,"country":"EE","current":{"ssid_hex":"486F6D65","freq":5180,
            "bssid":"aa:bb:cc:dd:ee:ff","ip":"192.0.2.23"},
            "networks":[{"ssid":"Home","ssid_hex":"486f6d65","security":["psk","sae"],"passphrase":"abcdefgh",
            "mac":"02:11:22:33:44:55","mac_policy":"randomized"},
            {"ssid":"Corp","ssid_hex":"436f7270","security":["eap"]}]}"#;
        let h = parse(t).unwrap();
        assert_eq!(h.country.as_deref(), Some("EE"));
        assert_eq!(h.networks.len(), 1);
        assert_eq!(h.networks[0].security, vec![Sec::Psk, Sec::Sae]);
        let c = h.current.unwrap();
        assert_eq!(c.ssid_hex, "486f6d65");
        assert_eq!(c.freq, Some(5180));
    }
}
