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
//! Format (v2; v1 had no "v":2, no per-type details and listed only open/owe/psk/sae):
//! ```json
//! {"v":2, "written_ms":1731000000000, "country":"EE", "dhcp_vendor_class":"android-dhcp-16",
//!  "wep_allowed":true, "passpoint_profiles":["Example Hotspot"],
//!  "current":{"ssid_hex":"486f6d65","bssid":"aa:bb:cc:dd:ee:ff","freq":5180,
//!             "mac":"02:11:22:33:44:55","ip":"192.0.2.23","prefix":24,
//!             "gateway":"192.0.2.1","dns":["192.0.2.1"]},
//!  "networks":[{"key":"7","ssid":"Home","ssid_hex":"486f6d65","hidden":false,
//!               "security":["sae","psk"],"security_off":[],"sae_h2e_only":false,
//!               "sae_pk_only":false,"passphrase":"...","pmk":"<64 hex>",
//!               "mac":"02:11:22:33:44:55","mac_policy":"persistent","bssid":"aa:bb:cc:dd:ee:ff",
//!               "freq":5180,"rssi":-61,"last_connected_ms":1730999000000},
//!              {"key":"9","ssid":"Corp","ssid_hex":"436f7270","security":["eap","eap_wpa3"],
//!               "eap":{"method":"PEAP","phase2":"MSCHAPV2","identity":"...","password":"...",
//!                      "anonymous_identity":"anonymous@corp.example","ca_cert":"keystore://CACERT_x",
//!                      "ca_path":"","domain_suffix_match":"radius.corp.example","ocsp":0,
//!                      "min_tls":0},
//!               "mac_policy":"non_persistent"}]}
//! ```
//! `current` is null when Android was not on Wi-Fi. One entry per saved Android network (the
//! entry app merges the per-type copies WifiManager hands out back into one network by its
//! network id, `key`). `security` lists the types Android would join with, in Android's order:
//! open, owe, wep, psk, sae, eap, eap_wpa3, eap_suiteb, dpp, wapi_psk, wapi_cert, osen;
//! `security_off` the ones a transition-disable indication from the AP turned off. Types abyssd
//! cannot join are kept, so the Wi-Fi screen can say why (netblock.rs `unusable`). Optional
//! per type: `wep_keys` (Android's strings: "\"ascii\"" or hex) + `wep_tx_keyidx`, `eap`,
//! `dpp` {"connector","csign_hex","netaccesskey_hex"}, `fils` ["sha256","sha384"],
//! `suiteb` ["rsa","ecdsa"], `static_ip` {"ip","prefix","gateway","dns":[..]}.
//! `mac_policy`: factory, persistent (`mac` = Android's per-network MAC), non_persistent (a new
//! random MAC per connection, like Android's), or "randomized" (v1 = persistent).
//!
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

/// Android's security types (WifiConfiguration.SECURITY_TYPE_*), as the hand-over names them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sec {
    Open,
    Owe,
    Wep,
    Psk,
    Sae,
    /// WPA/WPA2-Enterprise (and Passpoint R1/R2).
    Eap,
    /// WPA3-Enterprise (PMF required; Passpoint R3).
    EapWpa3,
    /// WPA3-Enterprise 192-bit.
    EapSuiteB,
    Dpp,
    WapiPsk,
    WapiCert,
    Osen,
    Unknown(String),
}

impl Sec {
    pub fn parse(s: &str) -> Sec {
        match s {
            "open" => Sec::Open,
            "owe" => Sec::Owe,
            "wep" => Sec::Wep,
            "psk" => Sec::Psk,
            "sae" => Sec::Sae,
            "eap" => Sec::Eap,
            "eap_wpa3" => Sec::EapWpa3,
            "eap_suiteb" => Sec::EapSuiteB,
            "dpp" => Sec::Dpp,
            "wapi_psk" => Sec::WapiPsk,
            "wapi_cert" => Sec::WapiCert,
            "osen" => Sec::Osen,
            other => Sec::Unknown(other.to_string()),
        }
    }

    pub fn is_enterprise(&self) -> bool {
        matches!(self, Sec::Eap | Sec::EapWpa3 | Sec::EapSuiteB)
    }
}

/// The enterprise (802.1X) settings, as Android's WifiEnterpriseConfig keeps them (the
/// supplicant field values the Wi-Fi HAL would set). Secrets: identity, anonymous identity and
/// password are never logged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Eap {
    /// PEAP, TLS, TTLS, PWD, SIM, AKA, AKA', WFA-UNAUTH-TLS.
    pub method: String,
    /// "" (none), PAP, MSCHAP, MSCHAPV2, GTC, SIM, AKA, AKA'.
    pub phase2: String,
    pub identity: Option<String>,
    /// With Android's decorated-identity prefix already in front.
    pub anonymous_identity: Option<String>,
    pub password: Option<String>,
    /// "keystore://CACERT_<alias>", "keystores://<hex aliases>", "hash://server/sha256/..".
    pub ca_cert: Option<String>,
    /// A directory of CA certificates ("/system/etc/security/cacerts" = system CAs).
    pub ca_path: Option<String>,
    /// "keystore://USRCERT_<alias>".
    pub client_cert: Option<String>,
    /// "USRPKEY_<alias>" with `engine` = the keystore engine.
    pub key_id: Option<String>,
    pub engine: bool,
    pub engine_id: Option<String>,
    pub domain_suffix_match: Option<String>,
    pub altsubject_match: Option<String>,
    pub subject_match: Option<String>,
    /// 0 none, 1 request, 2 require, 3 require all non-trusted.
    pub ocsp: u8,
    pub erp: bool,
    /// Android's minimum TLS version: 0 = 1.0 … 3 = 1.3.
    pub min_tls: u8,
}

/// Wi-Fi Easy Connect (DPP) connection keys.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dpp {
    pub connector: String,
    pub csign_hex: String,
    pub netaccesskey_hex: String,
}

/// Android's IP settings for a network saved with a static address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaticIp {
    pub ip: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
}

#[derive(Clone, Debug, Default)]
pub struct Network {
    /// Android's network id (two saved networks may share an SSID with different security).
    pub key: String,
    pub ssid: String,
    pub ssid_hex: String,
    pub hidden: bool,
    /// The types Android would join with, Android's order.
    pub security: Vec<Sec>,
    /// Types turned off by the AP's transition-disable indication (never offered).
    pub security_off: Vec<Sec>,
    /// A v2 hand-over lists every type Android has; v1 only the legacy bits (see netblock.rs).
    pub v2: bool,
    pub passphrase: Option<String>,
    pub pmk: Option<String>,
    pub sae_h2e_only: bool,
    pub sae_pk_only: bool,
    pub wep_keys: [Option<String>; 4],
    pub wep_tx_keyidx: u8,
    pub eap: Option<Eap>,
    pub dpp: Option<Dpp>,
    pub fils_sha256: bool,
    pub fils_sha384: bool,
    pub suiteb_rsa: bool,
    pub suiteb_ecdsa: bool,
    pub static_ip: Option<StaticIp>,
    /// None = factory MAC (or a fresh random one with `mac_random`).
    pub mac: Option<String>,
    /// Android's non-persistent randomization: a new random MAC per connection.
    pub mac_random: bool,
    pub bssid: Option<String>,
    pub freq: Option<u32>,
    /// Signal in Android's last scan (dBm).
    pub rssi: Option<i32>,
    /// Wall-clock ms of Android's last connection to it (0 = never).
    pub last_connected_ms: u64,
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
    /// Android's "Allow WEP networks" (Android 15+); true when the app could not tell.
    pub wep_allowed: bool,
    /// Passpoint profiles Android has not turned into a network yet (named, not joinable).
    pub passpoint_profiles: Vec<String>,
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
        wep_allowed: v["wep_allowed"].as_bool().unwrap_or(true),
        passpoint_profiles: strs(&v["passpoint_profiles"]),
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
    let v2 = v["v"].as_u64().unwrap_or(1) >= 2;
    for (i, n) in v["networks"].as_array().into_iter().flatten().enumerate() {
        if let Some(net) = parse_network(n, i, v2) {
            h.networks.push(net);
        }
    }
    Ok(h)
}

fn s_opt(v: &Value) -> Option<String> {
    s(v)
}

fn sec_list(v: &Value) -> Vec<Sec> {
    let mut out: Vec<Sec> = Vec::new();
    for x in v.as_array().into_iter().flatten().filter_map(Value::as_str) {
        let t = Sec::parse(x);
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect()
}

fn parse_eap(e: &Value) -> Option<Eap> {
    let method = s(&e["method"])?.to_ascii_uppercase();
    Some(Eap {
        method,
        phase2: s(&e["phase2"]).map(|p| p.to_ascii_uppercase()).unwrap_or_default(),
        identity: s(&e["identity"]),
        anonymous_identity: s(&e["anonymous_identity"]),
        password: s(&e["password"]),
        ca_cert: s(&e["ca_cert"]),
        ca_path: s(&e["ca_path"]),
        client_cert: s(&e["client_cert"]),
        key_id: s(&e["key_id"]),
        engine: e["engine"].as_bool().unwrap_or(false),
        engine_id: s(&e["engine_id"]),
        domain_suffix_match: s(&e["domain_suffix_match"]),
        altsubject_match: s(&e["altsubject_match"]),
        subject_match: s(&e["subject_match"]),
        ocsp: e["ocsp"].as_u64().unwrap_or(0).min(3) as u8,
        erp: e["erp"].as_bool().unwrap_or(false),
        min_tls: e["min_tls"].as_u64().unwrap_or(0).min(3) as u8,
    })
}

fn parse_network(n: &Value, index: usize, v2: bool) -> Option<Network> {
    let ssid_hex = n["ssid_hex"].as_str().unwrap_or("").to_ascii_lowercase();
    if !is_hex(&ssid_hex, None) || ssid_hex.len() > 64 {
        return None;
    }
    let security = sec_list(&n["security"]);
    let security_off = sec_list(&n["security_off"]);
    if security.is_empty() && security_off.is_empty() {
        return None;
    }
    let (mac, mac_random) = match n["mac_policy"].as_str() {
        Some("factory") => (None, false),
        Some("non_persistent") => (None, true),
        _ => (s(&n["mac"]).filter(|m| is_mac(m) && m != "02:00:00:00:00:00"), false),
    };
    let mut wep_keys: [Option<String>; 4] = Default::default();
    for (slot, k) in wep_keys.iter_mut().zip(n["wep_keys"].as_array().into_iter().flatten()) {
        *slot = s(k);
    }
    let fils = strs(&n["fils"]);
    let suiteb = strs(&n["suiteb"]);
    let static_ip = n["static_ip"].as_object().and_then(|o| {
        Some(StaticIp {
            ip: o.get("ip")?.as_str()?.parse().ok()?,
            prefix: o.get("prefix")?.as_u64().filter(|&p| (1..=32).contains(&p))? as u8,
            gateway: o.get("gateway").and_then(Value::as_str).and_then(|g| g.parse().ok()),
            dns: o.get("dns").map(ips).unwrap_or_default(),
        })
    });
    Some(Network {
        key: s(&n["key"]).or_else(|| n["key"].as_i64().map(|k| k.to_string())).unwrap_or_else(|| format!("#{index}")),
        ssid: s(&n["ssid"]).unwrap_or_else(|| ssid_hex.clone()),
        ssid_hex,
        hidden: n["hidden"].as_bool().unwrap_or(false),
        security,
        security_off,
        v2,
        passphrase: s(&n["passphrase"]),
        pmk: s(&n["pmk"]).filter(|p| is_hex(p, Some(64))).map(|p| p.to_ascii_lowercase()),
        sae_h2e_only: n["sae_h2e_only"].as_bool().unwrap_or(false),
        sae_pk_only: n["sae_pk_only"].as_bool().unwrap_or(false),
        wep_keys,
        wep_tx_keyidx: n["wep_tx_keyidx"].as_u64().unwrap_or(0).min(3) as u8,
        eap: n["eap"].as_object().and_then(|_| parse_eap(&n["eap"])),
        dpp: n["dpp"].as_object().and_then(|o| {
            Some(Dpp {
                connector: o.get("connector")?.as_str()?.to_string(),
                csign_hex: o.get("csign_hex")?.as_str()?.to_ascii_lowercase(),
                netaccesskey_hex: o.get("netaccesskey_hex")?.as_str()?.to_ascii_lowercase(),
            })
        }),
        fils_sha256: fils.iter().any(|f| f == "sha256"),
        fils_sha384: fils.iter().any(|f| f == "sha384"),
        suiteb_rsa: suiteb.iter().any(|f| f == "rsa"),
        suiteb_ecdsa: suiteb.iter().any(|f| f == "ecdsa"),
        static_ip,
        mac,
        mac_random,
        bssid: s(&n["bssid"]).filter(|b| is_mac(b)),
        freq: n["freq"].as_u64().map(|f| f as u32).filter(|&f| f > 0),
        rssi: n["rssi"].as_i64().map(|r| r as i32).filter(|&r| r < 0),
        last_connected_ms: n["last_connected_ms"].as_u64().unwrap_or(0),
    })
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
        // Networks abyssd cannot join stay in the list (the screen says why).
        assert_eq!(h.networks.len(), 2);
        assert_eq!(h.networks[0].security, vec![Sec::Psk, Sec::Sae]);
        assert!(!h.networks[0].v2);
        assert_eq!(h.networks[0].mac.as_deref(), Some("02:11:22:33:44:55"));
        assert_eq!(h.networks[1].security, vec![Sec::Eap]);
        assert!(h.networks[1].eap.is_none());
        assert!(h.wep_allowed);
        let c = h.current.unwrap();
        assert_eq!(c.ssid_hex, "486f6d65");
        assert_eq!(c.freq, Some(5180));
    }

    #[test]
    fn parses_v2_details() {
        let t = r#"{"v":2,"wep_allowed":false,"networks":[
            {"key":7,"ssid":"Home","ssid_hex":"486f6d65","security":["sae"],"security_off":["psk"],
             "sae_h2e_only":true,"passphrase":"pw","mac_policy":"non_persistent","rssi":-60,
             "last_connected_ms":99,"static_ip":{"ip":"192.0.2.9","prefix":24,"gateway":"192.0.2.1",
             "dns":["192.0.2.1"]}},
            {"key":"9","ssid":"Corp","ssid_hex":"436f7270","security":["eap","eap_wpa3"],
             "eap":{"method":"peap","phase2":"mschapv2","identity":"me","password":"pw",
                    "ca_path":"/system/etc/security/cacerts","domain_suffix_match":"corp.example",
                    "ocsp":9,"min_tls":2},"fils":["sha256"],"suiteb":["rsa"]},
            {"key":"3","ssid":"Old","ssid_hex":"4f6c64","security":["wep"],
             "wep_keys":["\"abcde\"",null,"",null],"wep_tx_keyidx":0},
            {"key":"4","ssid":"Odd","ssid_hex":"4f6464","security":["wapi_psk","whatever"]}]}"#;
        let h = parse(t).unwrap();
        assert!(!h.wep_allowed);
        assert!(h.passpoint_profiles.is_empty());
        assert_eq!(h.networks.len(), 4);
        let home = &h.networks[0];
        assert!(home.v2);
        assert_eq!(home.key, "7");
        assert_eq!(home.security, vec![Sec::Sae]);
        assert_eq!(home.security_off, vec![Sec::Psk]);
        assert!(home.sae_h2e_only && home.mac_random && home.mac.is_none());
        assert_eq!(home.rssi, Some(-60));
        assert_eq!(home.last_connected_ms, 99);
        let st = home.static_ip.as_ref().unwrap();
        assert_eq!((st.ip.to_string(), st.prefix, st.dns.len()), ("192.0.2.9".into(), 24, 1));
        let corp = &h.networks[1];
        let e = corp.eap.as_ref().unwrap();
        assert_eq!((e.method.as_str(), e.phase2.as_str(), e.ocsp, e.min_tls), ("PEAP", "MSCHAPV2", 3, 2));
        assert!(corp.fils_sha256 && !corp.fils_sha384 && corp.suiteb_rsa && !corp.suiteb_ecdsa);
        let old = &h.networks[2];
        assert_eq!(old.wep_keys[0].as_deref(), Some("\"abcde\""));
        assert!(old.wep_keys[1].is_none() && old.wep_keys[2].is_none());
        assert_eq!(h.networks[3].security, vec![Sec::WapiPsk, Sec::Unknown("whatever".into())]);
    }
}
