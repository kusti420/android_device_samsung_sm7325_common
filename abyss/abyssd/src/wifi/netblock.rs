//! A saved Android network → wpa_supplicant network blocks (`SET_NETWORK <id> <name> <value>`).
//!
//! The mapping follows what Android's Wi-Fi HAL glue sets for the same network
//! (packages/modules/Wifi SupplicantStaNetworkHalAidlImpl.saveWifiConfiguration +
//! SecurityParams.createSecurityParamsBySecurityType), with two differences that only widen
//! what can be joined, never weaken a network:
//! - Android picks one security type per connection from its own scan matching (or merges PSK
//!   and SAE when the driver offloads the upgrade). abyssd has no scan matcher, so one block
//!   offers every type Android would use for the network and the supplicant picks per AP; PMF is
//!   then "capable" (`ieee80211w=1`), as Android's merge rule has it (NativeUtil
//!   .getOptimalPmfSettingForConfig), and "required" (`2`) only when every offered type
//!   requires it.
//! - Fast-transition / SAE-EXT-KEY AKMs (FT-PSK, FT-SAE, SAE-EXT-KEY, FT-EAP) go into a second
//!   block of the same network at one priority step lower, so an AP that also offers the plain
//!   AKM still gets exactly Android's choice and only an AP that offers nothing else (FT-only
//!   corporate/mesh, Wi-Fi 7 SAE-EXT-KEY-only) is joined through it. Android on this device has
//!   config_wifi_fast_bss_transition_enabled=false and would not join those at all.
//! - `sae_pwe=2` (hunting-and-pecking or hash-to-element, whichever the AP offers; 1 when
//!   Android has the network as H2E-only). Android sets 0 when its overlay does not declare H2E
//!   support (this device), which cannot join 6 GHz or H2E-only APs.
//!
//! Every block is independent: if the supplicant refuses one (an AKM it was built without), the
//! others still go in. Fields in [`non_fatal`] may fail without dropping the block.
//!
//! Values: SSIDs, SAE passwords, EAP identities and passwords go hex-encoded (an unquoted string
//! value is hex to the supplicant), so any byte is safe on the control socket; other strings
//! are quoted ("..."; the supplicant takes everything up to the last quote) and refused when
//! they hold control characters.

use super::handoff::{Eap, Network, Sec};

/// Field name, value.
pub type Fields = Vec<(&'static str, String)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub fields: Fields,
    /// One priority step below the network's main block (fallback AKMs, open next to OWE).
    pub lower: bool,
}

/// Which MAC the supplicant uses for the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mac {
    Factory,
    /// Android's MAC for the network (persistent randomization, or the one in use right now).
    Fixed(String),
    /// A new random MAC per connection (Android's non-persistent randomization).
    Random,
}

#[derive(Clone, Debug)]
pub struct Opts {
    pub mac: Mac,
    /// Scan only this frequency (the fast attempt).
    pub freq: Option<u32>,
    pub bssid: Option<String>,
    /// The main block's priority; lower blocks get one less.
    pub priority: u32,
    /// Android's "Allow WEP networks".
    pub wep_allowed: bool,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts { mac: Mac::Factory, freq: None, bssid: None, priority: 1, wep_allowed: true }
    }
}

/// Fields whose refusal leaves the block usable (logged): cipher lists (the supplicant default
/// is CCMP+TKIP), the BSSID hint, ERP, H2E-or-not.
pub fn non_fatal(name: &str, value: &str) -> bool {
    matches!(name, "pairwise" | "group" | "group_mgmt" | "bssid_hint" | "erp")
        || (name == "sae_pwe" && value == "2")
}

/// Network fields whose values are secrets: never logged.
pub fn is_secret(name: &str) -> bool {
    matches!(
        name,
        "psk"
            | "sae_password"
            | "sae_password_id"
            | "password"
            | "identity"
            | "anonymous_identity"
            | "private_key_passwd"
            | "pin"
            | "wep_key0"
            | "wep_key1"
            | "wep_key2"
            | "wep_key3"
            | "dpp_netaccesskey"
            | "dpp_csign"
            | "dpp_connector"
    )
}

/// A control-interface command with any secret value replaced (SET_NETWORK / SET_CRED).
pub fn redact(cmd: &str) -> String {
    let words: Vec<&str> = cmd.split(' ').collect();
    for (i, w) in words.iter().enumerate() {
        if (*w == "SET_NETWORK" || *w == "SET_CRED") && words.len() > i + 3 && is_secret(words[i + 2]) {
            return format!("{} <hidden>", words[..i + 3].join(" "));
        }
    }
    cmd.to_string()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// "..." for a supplicant string field.
fn quoted(what: &str, s: &str) -> Result<String, String> {
    if s.chars().any(|c| c.is_control()) {
        return Err(format!("its {what} has control characters"));
    }
    Ok(format!("\"{s}\""))
}

fn is_hex_str(s: &str) -> bool {
    !s.is_empty() && s.len().is_multiple_of(2) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What Android's SecurityParams allow per type (pairwise, group).
fn ciphers(t: &Sec) -> (&'static [&'static str], &'static [&'static str]) {
    match t {
        Sec::Psk => (&["CCMP", "TKIP"], &["CCMP", "TKIP", "WEP40", "WEP104"]),
        Sec::Sae | Sec::Owe | Sec::Dpp => (&["CCMP", "GCMP", "GCMP-256"], &["CCMP", "GCMP", "GCMP-256"]),
        Sec::Eap => (&["CCMP", "TKIP", "GCMP-256"], &["CCMP", "TKIP", "GCMP-256"]),
        Sec::EapWpa3 => (&["CCMP", "GCMP-256"], &["CCMP", "GCMP-256"]),
        Sec::EapSuiteB => (&["GCMP", "GCMP-256"], &["GCMP", "GCMP-256"]),
        _ => (&[], &[]),
    }
}

/// Android's per-type PMF requirement.
fn pmf_required(t: &Sec) -> bool {
    matches!(t, Sec::Sae | Sec::Owe | Sec::EapWpa3 | Sec::EapSuiteB | Sec::Dpp)
}

fn union(types: &[Sec], pick: impl Fn(&Sec) -> &'static [&'static str]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for t in types {
        for c in pick(t) {
            if !out.contains(c) {
                out.push(c);
            }
        }
    }
    out.join(" ")
}

/// key_mgmt + ieee80211w + ciphers for a block offering `types`.
fn suite(f: &mut Fields, types: &[Sec], km: &str) {
    f.push(("key_mgmt", km.to_string()));
    let pmf = if types.iter().all(pmf_required) { "2" } else { "1" };
    f.push(("ieee80211w", pmf.to_string()));
    let pw = union(types, |t| ciphers(t).0);
    if !pw.is_empty() {
        f.push(("pairwise", pw));
    }
    let gr = union(types, |t| ciphers(t).1);
    if !gr.is_empty() {
        f.push(("group", gr));
    }
}

fn valid_wpa_passphrase(p: &str) -> bool {
    (8..=63).contains(&p.len()) && p.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// Personal (PSK/SAE) part: the main block and the fast-transition / SAE-EXT-KEY fallback.
fn personal(n: &Network, on: &[Sec], out: &mut Vec<Block>) -> Result<(), String> {
    let pass = n.passphrase.as_deref().filter(|p| !p.is_empty());
    let mut want_psk = on.contains(&Sec::Psk);
    let want_sae = on.contains(&Sec::Sae);
    // A v1 hand-over read only the legacy key-management bits, which show SAE alone for a
    // network Android keeps as "WPA2 or WPA3" (PSK + SAE): offer WPA2 too when the password
    // is a valid WPA2 passphrase (8..63 printable ASCII). v2 lists every type Android has.
    if !n.v2 && want_sae && !want_psk && pass.is_some_and(valid_wpa_passphrase) {
        want_psk = true;
    }
    let psk_ok = n.pmk.is_some() || pass.is_some_and(valid_wpa_passphrase);
    // SAE needs the password itself (a raw PMK is WPA2-only).
    let sae_ok = pass.is_some();
    let mut types = Vec::new();
    if want_psk && psk_ok {
        types.push(Sec::Psk);
    }
    if want_sae && sae_ok {
        types.push(Sec::Sae);
    }
    if types.is_empty() {
        return Err(if pass.is_some() {
            "its password is not a valid WPA2 passphrase (8 to 63 characters)".into()
        } else {
            "no password was handed over".into()
        });
    }
    let has = |t: Sec| types.contains(&t);
    let mut main = Vec::new();
    let mut extra = Vec::new();
    if has(Sec::Psk) {
        main.extend(["WPA-PSK", "WPA-PSK-SHA256"]);
        extra.push("FT-PSK");
    }
    if has(Sec::Sae) {
        main.push("SAE");
        extra.extend(["FT-SAE", "SAE-EXT-KEY", "FT-SAE-EXT-KEY"]);
    }
    let mut secrets: Fields = Vec::new();
    if has(Sec::Psk) {
        // The precomputed PMK skips 4096 rounds of PBKDF2 per connect (battery-nerd §4.3).
        match (&n.pmk, pass) {
            (Some(pmk), _) => secrets.push(("psk", pmk.clone())),
            (None, Some(p)) => secrets.push(("psk", quoted("password", p)?)),
            (None, None) => unreachable!("psk_ok"),
        }
    }
    if has(Sec::Sae) {
        let p = pass.unwrap_or_default();
        secrets.push(("sae_password", hex(p.as_bytes())));
        secrets.push(("sae_pwe", if n.sae_h2e_only { "1" } else { "2" }.into()));
        if n.sae_pk_only {
            secrets.push(("sae_pk", "1".into()));
        }
    }
    for (km, lower) in [(main.join(" "), false), (extra.join(" "), true)] {
        let mut f = Fields::new();
        suite(&mut f, &types, &km);
        f.extend(secrets.iter().cloned());
        out.push(Block { fields: f, lower });
    }
    Ok(())
}

fn wep(n: &Network, wep_allowed: bool, out: &mut Vec<Block>) -> Result<(), String> {
    if !wep_allowed {
        return Err("WEP is turned off in Android (Wi-Fi settings › Allow WEP networks)".into());
    }
    let mut f: Fields = vec![("key_mgmt", "NONE".into()), ("auth_alg", "OPEN SHARED".into())];
    let names = ["wep_key0", "wep_key1", "wep_key2", "wep_key3"];
    let mut any = false;
    for (i, k) in n.wep_keys.iter().enumerate() {
        let Some(k) = k.as_deref().filter(|k| !k.is_empty()) else { continue };
        let ok = if let Some(inner) = k.strip_prefix('"').and_then(|k| k.strip_suffix('"')) {
            matches!(inner.len(), 5 | 13 | 16) && !inner.chars().any(|c| c.is_control())
        } else {
            matches!(k.len(), 10 | 26 | 32) && is_hex_str(k)
        };
        if !ok {
            return Err(format!("WEP key {} is not a valid key", i + 1));
        }
        f.push((names[i], k.to_string()));
        any = true;
    }
    let tx = n.wep_tx_keyidx as usize;
    if !any || n.wep_keys[tx].as_deref().is_none_or(str::is_empty) {
        return Err("no WEP key was handed over".into());
    }
    f.push(("wep_tx_keyidx", tx.to_string()));
    out.push(Block { fields: f, lower: false });
    Ok(())
}

fn is_sim(m: &str) -> bool {
    matches!(m, "SIM" | "AKA" | "AKA'")
}

/// Android's EAP phase-1 TLS flags (StaNetwork::generateTlsParams / setTlsFlagsFor192BitMode).
fn tls_phase1(e: &Eap, suiteb_rsa: bool) -> String {
    let (mut d13, mut d12, mut d11, mut d10) = (false, false, false, false);
    if suiteb_rsa {
        // 192-bit RSA mode: TLS 1.2 only, plus tls_suiteb.
        d13 = true;
        d11 = true;
        d10 = true;
    } else {
        // Minimum TLS version; lower versions off cascadingly.
        match e.min_tls {
            3 => (d12, d11, d10) = (true, true, true),
            2 => (d11, d10) = (true, true),
            1 => d10 = true,
            _ => {}
        }
    }
    let b = |x: bool| if x { 1 } else { 0 };
    let mut s = format!(
        "tls_disable_tlsv1_3={} tls_disable_tlsv1_2={} tls_disable_tlsv1_1={} tls_disable_tlsv1_0={}",
        b(d13),
        b(d12),
        b(d11),
        b(d10)
    );
    if suiteb_rsa {
        s.push_str(" tls_suiteb=1");
    }
    s
}

fn enterprise(n: &Network, on: &[Sec], out: &mut Vec<Block>) -> Result<(), String> {
    let e = n.eap.as_ref().ok_or("its enterprise (802.1X) settings were not handed over")?;
    if is_sim(&e.method) || is_sim(&e.phase2) {
        return Err("SIM-based sign-in (EAP-SIM/AKA) needs Android's phone service, which sleeps in Abyss".into());
    }
    let method = match e.method.as_str() {
        m @ ("PEAP" | "TLS" | "TTLS" | "PWD" | "WFA-UNAUTH-TLS") => m,
        "" => return Err("no enterprise sign-in method (EAP) is set".into()),
        other => return Err(format!("enterprise sign-in method {other} is not supported in Abyss")),
    };
    let needs_password = matches!(method, "PEAP" | "TTLS" | "PWD");
    if needs_password && e.identity.as_deref().is_none_or(str::is_empty) {
        return Err("no enterprise identity (user name) was handed over".into());
    }
    if needs_password && e.password.as_deref().is_none_or(str::is_empty) {
        return Err("no enterprise password was handed over".into());
    }
    if method == "TLS" && e.client_cert.is_none() {
        return Err("no client certificate is set for its EAP-TLS sign-in".into());
    }
    let types: Vec<Sec> = on.iter().filter(|t| t.is_enterprise()).cloned().collect();
    let suiteb = types.contains(&Sec::EapSuiteB);
    let mut main = vec!["WPA-EAP", "IEEE8021X", "WPA-EAP-SHA256"];
    let mut extra = vec!["FT-EAP"];
    if suiteb {
        main.insert(2, "WPA-EAP-SUITE-B-192");
        extra.push("FT-EAP-SHA384");
    }
    if n.fils_sha256 {
        main.push("FILS-SHA256");
    }
    if n.fils_sha384 {
        main.push("FILS-SHA384");
    }
    let mut eapf: Fields = vec![("eap", method.to_string())];
    if !e.phase2.is_empty() && e.phase2 != "NONE" {
        let prefix = if method == "TTLS" && e.phase2 == "GTC" { "autheap=" } else { "auth=" };
        eapf.push(("phase2", quoted("phase 2 method", &format!("{prefix}{}", e.phase2))?));
    }
    if let Some(v) = e.identity.as_deref().filter(|v| !v.is_empty()) {
        eapf.push(("identity", hex(v.as_bytes())));
    }
    if let Some(v) = e.anonymous_identity.as_deref().filter(|v| !v.is_empty()) {
        eapf.push(("anonymous_identity", hex(v.as_bytes())));
    }
    if let Some(v) = e.password.as_deref().filter(|v| !v.is_empty()) {
        eapf.push(("password", hex(v.as_bytes())));
    }
    for (name, what, v) in [
        ("client_cert", "client certificate", &e.client_cert),
        ("ca_cert", "CA certificate", &e.ca_cert),
        ("subject_match", "subject match", &e.subject_match),
        ("engine_id", "key engine", &e.engine_id),
    ] {
        if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
            eapf.push((name, quoted(what, v)?));
        }
    }
    if e.engine {
        eapf.push(("engine", "1".into()));
    }
    for (name, what, v) in [
        ("key_id", "private key", &e.key_id),
        ("altsubject_match", "alternate subject match", &e.altsubject_match),
        ("domain_suffix_match", "domain", &e.domain_suffix_match),
        ("ca_path", "CA path", &e.ca_path),
    ] {
        if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
            eapf.push((name, quoted(what, v)?));
        }
    }
    if e.ocsp > 0 {
        eapf.push(("ocsp", e.ocsp.to_string()));
    }
    if e.erp {
        eapf.push(("erp", "1".into()));
    }
    let suiteb_rsa = suiteb && n.suiteb_rsa;
    eapf.push(("phase1", quoted("phase 1", &tls_phase1(e, suiteb_rsa))?));
    if suiteb && n.suiteb_ecdsa && !suiteb_rsa {
        eapf.push(("openssl_ciphers", "\"SUITEB192\"".into()));
    }
    for (km, lower) in [(main.join(" "), false), (extra.join(" "), true)] {
        let mut f = Fields::new();
        suite(&mut f, &types, &km);
        if suiteb {
            f.push(("group_mgmt", "BIP-GMAC-256".into()));
        }
        f.extend(eapf.iter().cloned());
        out.push(Block { fields: f, lower });
    }
    Ok(())
}

fn dpp(n: &Network, out: &mut Vec<Block>) -> Result<(), String> {
    let d = n.dpp.as_ref().ok_or("its Easy Connect (DPP) keys were not handed over")?;
    if d.connector.is_empty() || !is_hex_str(&d.csign_hex) || !is_hex_str(&d.netaccesskey_hex) {
        return Err("its Easy Connect (DPP) keys are incomplete".into());
    }
    let mut f = Fields::new();
    suite(&mut f, &[Sec::Dpp], "DPP");
    f.push(("dpp_connector", quoted("DPP connector", &d.connector)?));
    f.push(("dpp_csign", d.csign_hex.clone()));
    f.push(("dpp_netaccesskey", d.netaccesskey_hex.clone()));
    out.push(Block { fields: f, lower: false });
    Ok(())
}

/// Why abyssd cannot join `n` at all (shown on the Wi-Fi screen), or None.
pub fn unusable(n: &Network, wep_allowed: bool) -> Option<String> {
    plan(n, &Opts { wep_allowed, ..Opts::default() }).err()
}

/// Whether the network signs in with a certificate from Android's keystore (keystore2 runs in
/// Abyss and the supplicant reads it directly; certificates migrated from the legacy keystore
/// are only reachable through Android's Wi-Fi service and fail).
pub fn uses_keystore(n: &Network) -> bool {
    n.eap.as_ref().is_some_and(|e| {
        [&e.ca_cert, &e.client_cert].iter().any(|v| v.as_deref().is_some_and(|v| v.starts_with("keystore")))
            || e.engine
    })
}

/// The blocks for `n`, or why there are none.
pub fn plan(n: &Network, o: &Opts) -> Result<Vec<Block>, String> {
    let on: Vec<Sec> = n.security.iter().filter(|t| !n.security_off.contains(t)).cloned().collect();
    let mut out: Vec<Block> = Vec::new();
    let mut first_err: Option<String> = None;
    let mut note = |r: Result<(), String>| {
        if let Err(e) = r {
            first_err.get_or_insert(e);
        }
    };
    if on.iter().any(Sec::is_enterprise) {
        note(enterprise(n, &on, &mut out));
    }
    if on.contains(&Sec::Psk) || on.contains(&Sec::Sae) {
        note(personal(n, &on, &mut out));
    }
    if on.contains(&Sec::Dpp) {
        note(dpp(n, &mut out));
    }
    if on.contains(&Sec::Owe) {
        // OWE (Enhanced Open). An open network Android auto-upgraded to OWE also gets the open
        // block below, one step lower: the OWE block takes an OWE / OWE-transition AP, the open
        // one a plain open AP.
        let mut f = Fields::new();
        suite(&mut f, &[Sec::Owe], "OWE");
        out.push(Block { fields: f, lower: false });
    }
    if on.contains(&Sec::Open) {
        out.push(Block { fields: vec![("key_mgmt", "NONE".into())], lower: on.contains(&Sec::Owe) });
    }
    if on.contains(&Sec::Wep) {
        note(wep(n, o.wep_allowed, &mut out));
    }
    for t in &on {
        match t {
            Sec::WapiPsk | Sec::WapiCert => note(Err("WAPI networks are not supported in Abyss".into())),
            Sec::Osen => note(Err("Passpoint sign-up (OSEN) networks are not supported in Abyss".into())),
            Sec::Unknown(x) => note(Err(format!("unknown security type \"{x}\""))),
            _ => {}
        }
    }
    if out.is_empty() {
        return Err(first_err.unwrap_or_else(|| {
            if on.is_empty() {
                "the router turned its only security type off (transition disable)".into()
            } else {
                "no usable security type".into()
            }
        }));
    }
    // Common fields, in front (SSID) and behind (MAC, scan, priority).
    let ssid = hex(&decode_hex(&n.ssid_hex));
    for b in &mut out {
        let mut f: Fields = vec![("ssid", ssid.clone())];
        if n.hidden {
            f.push(("scan_ssid", "1".into()));
        }
        f.append(&mut b.fields);
        match &o.mac {
            Mac::Fixed(m) => {
                f.push(("mac_addr", "3".into()));
                f.push(("mac_value", m.clone()));
            }
            Mac::Random => f.push(("mac_addr", "1".into())),
            Mac::Factory => f.push(("mac_addr", "0".into())),
        }
        if let Some(fr) = o.freq {
            f.push(("scan_freq", fr.to_string()));
        }
        if let Some(bs) = &o.bssid {
            f.push(("bssid_hint", bs.clone()));
        }
        let p = if b.lower { o.priority.saturating_sub(1) } else { o.priority };
        f.push(("priority", p.to_string()));
        b.fields = f;
    }
    Ok(out)
}

/// A supplicant event that explains a failed connection, as a line for the screen.
pub fn event_hint(e: &str, keystore: bool) -> Option<String> {
    if e.contains("CTRL-EVENT-EAP-TLS-CERT-ERROR") {
        return Some("the network's server certificate was not accepted (check the CA certificate and domain in Android's Wi-Fi settings)".into());
    }
    if e.contains("CTRL-EVENT-EAP-FAILURE")
        || (e.contains("CTRL-EVENT-SSID-TEMP-DISABLED") && e.contains("reason=AUTH_FAILED"))
    {
        return Some(if keystore {
            "enterprise sign-in failed: the certificate may not be readable in Abyss (keystore), or it was refused".into()
        } else {
            "enterprise sign-in failed (user name or password refused?)".into()
        });
    }
    if let Some(i) = e.find("CTRL-REQ-") {
        // "CTRL-REQ-PASSWORD-3:Password needed for SSID Corp"
        let what = e[i + 9..].split(['-', ':']).next().unwrap_or("").to_ascii_lowercase();
        return Some(match what.as_str() {
            "sim" => "the network wants SIM authentication, which needs Android's phone service".into(),
            _ => format!("the network asked for a {what} that Android did not hand over"),
        });
    }
    if e.contains("CTRL-EVENT-ASSOC-REJECT") {
        let code = e.split("status_code=").nth(1).and_then(|r| r.split(' ').next()).unwrap_or("?");
        return Some(format!("the router refused the connection (status {code})"));
    }
    if e.contains("CTRL-EVENT-NETWORK-NOT-FOUND") {
        return Some("none of the saved networks is in range".into());
    }
    None
}

/// Candidates in Android's order: the network Android is on, abyssd's last one, then the rest
/// by Android's last connection (newest first) and, among equals (never connected), by the
/// signal in Android's last scan. The supplicant then joins the highest-priority one it sees.
pub fn order(mut usable: Vec<Network>, current: Option<&str>, last: Option<&str>, max: usize) -> Vec<Network> {
    usable.sort_by(|a, b| {
        b.last_connected_ms.cmp(&a.last_connected_ms).then(b.rssi.unwrap_or(-127).cmp(&a.rssi.unwrap_or(-127)))
    });
    let mut out: Vec<Network> = Vec::new();
    for hex in [current, last].into_iter().flatten() {
        if let Some(n) = usable.iter().find(|n| n.ssid_hex == hex) {
            if !out.iter().any(|o| o.key == n.key) {
                out.push(n.clone());
            }
        }
    }
    for n in usable {
        if !out.iter().any(|o| o.key == n.key) {
            out.push(n);
        }
    }
    out.truncate(max);
    out
}

fn decode_hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::super::handoff::{Dpp, Eap, Network, Sec};
    use super::*;

    fn net(security: &[Sec]) -> Network {
        Network {
            key: "1".into(),
            ssid: "Home".into(),
            ssid_hex: "486f6d65".into(),
            security: security.to_vec(),
            v2: true,
            ..Network::default()
        }
    }

    /// The blocks as "name value" lines, one Vec per block.
    fn lines(n: &Network, o: &Opts) -> Vec<Vec<String>> {
        plan(n, o).unwrap().iter().map(|b| b.fields.iter().map(|(k, v)| format!("{k} {v}")).collect()).collect()
    }

    fn get<'a>(block: &'a [String], name: &str) -> Option<&'a str> {
        block.iter().find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix(' ')))
    }

    #[test]
    fn wpa2_wpa3_transition_home_router() {
        // The owner's router: Android keeps PSK + auto-upgraded SAE.
        let mut n = net(&[Sec::Sae, Sec::Psk]);
        n.passphrase = Some("correct horse".into());
        n.pmk = Some("ab".repeat(32));
        let o = Opts { mac: Mac::Fixed("02:11:22:33:44:55".into()), priority: 30, ..Opts::default() };
        let b = lines(&n, &o);
        assert_eq!(b.len(), 2);
        assert_eq!(
            b[0],
            vec![
                "ssid 486f6d65",
                "key_mgmt WPA-PSK WPA-PSK-SHA256 SAE",
                "ieee80211w 1",
                "pairwise CCMP TKIP GCMP GCMP-256",
                "group CCMP TKIP WEP40 WEP104 GCMP GCMP-256",
                &format!("psk {}", "ab".repeat(32)),
                &format!("sae_password {}", hex(b"correct horse")),
                "sae_pwe 2",
                "mac_addr 3",
                "mac_value 02:11:22:33:44:55",
                "priority 30",
            ]
        );
        assert_eq!(get(&b[1], "key_mgmt"), Some("FT-PSK FT-SAE SAE-EXT-KEY FT-SAE-EXT-KEY"));
        assert_eq!(get(&b[1], "priority"), Some("29"));
    }

    #[test]
    fn psk_only_and_passphrase_quoting() {
        let mut n = net(&[Sec::Psk]);
        n.passphrase = Some("abc\"defgh".into());
        n.hidden = true;
        let b = lines(&n, &Opts::default());
        assert_eq!(get(&b[0], "scan_ssid"), Some("1"));
        assert_eq!(get(&b[0], "key_mgmt"), Some("WPA-PSK WPA-PSK-SHA256"));
        assert_eq!(get(&b[0], "ieee80211w"), Some("1"));
        assert_eq!(get(&b[0], "psk"), Some("\"abc\"defgh\""));
        assert_eq!(get(&b[0], "sae_password"), None);
        assert_eq!(get(&b[0], "mac_addr"), Some("0"));
        assert_eq!(get(&b[1], "key_mgmt"), Some("FT-PSK"));
        // A raw PMK only: WPA2 works, SAE cannot.
        let mut n = net(&[Sec::Psk, Sec::Sae]);
        n.pmk = Some("cd".repeat(32));
        let b = lines(&n, &Opts::default());
        assert_eq!(get(&b[0], "key_mgmt"), Some("WPA-PSK WPA-PSK-SHA256"));
    }

    #[test]
    fn sae_only_requires_pmf_and_honours_h2e_pk() {
        let mut n = net(&[Sec::Sae]);
        n.passphrase = Some("pw".into());
        n.sae_h2e_only = true;
        n.sae_pk_only = true;
        let b = lines(&n, &Opts::default());
        assert_eq!(get(&b[0], "key_mgmt"), Some("SAE"));
        assert_eq!(get(&b[0], "ieee80211w"), Some("2"));
        assert_eq!(get(&b[0], "sae_pwe"), Some("1"));
        assert_eq!(get(&b[0], "sae_pk"), Some("1"));
        assert_eq!(get(&b[0], "psk"), None);
        assert!(!non_fatal("sae_pwe", "1") && non_fatal("sae_pwe", "2"));
    }

    #[test]
    fn transition_disable_drops_psk() {
        let mut n = net(&[Sec::Psk, Sec::Sae]);
        n.security_off = vec![Sec::Psk];
        n.passphrase = Some("password1".into());
        let b = lines(&n, &Opts::default());
        assert_eq!(get(&b[0], "key_mgmt"), Some("SAE"));
        assert_eq!(get(&b[0], "ieee80211w"), Some("2"));
    }

    #[test]
    fn v1_sae_with_wpa2_passphrase_gets_psk_too() {
        let mut n = net(&[Sec::Sae]);
        n.v2 = false;
        n.passphrase = Some("password1".into());
        let b = lines(&n, &Opts::default());
        assert_eq!(get(&b[0], "key_mgmt"), Some("WPA-PSK WPA-PSK-SHA256 SAE"));
        assert_eq!(get(&b[0], "ieee80211w"), Some("1"));
        // v2 says what Android has: SAE-only stays SAE-only (no downgrade to WPA2).
        n.v2 = true;
        assert_eq!(get(&lines(&n, &Opts::default())[0], "key_mgmt"), Some("SAE"));
        // Too short for WPA2: SAE only, even from v1.
        n.v2 = false;
        n.passphrase = Some("short".into());
        assert_eq!(get(&lines(&n, &Opts::default())[0], "key_mgmt"), Some("SAE"));
    }

    #[test]
    fn missing_password_is_a_reason() {
        let n = net(&[Sec::Psk]);
        assert_eq!(unusable(&n, true).as_deref(), Some("no password was handed over"));
        let mut n = net(&[Sec::Psk]);
        n.passphrase = Some("x".repeat(64));
        assert!(unusable(&n, true).unwrap().contains("not a valid WPA2 passphrase"));
    }

    #[test]
    fn open_owe_and_owe_transition() {
        let b = lines(&net(&[Sec::Open]), &Opts::default());
        assert_eq!(b.len(), 1);
        assert_eq!(b[0], vec!["ssid 486f6d65", "key_mgmt NONE", "mac_addr 0", "priority 1"]);
        let b = lines(&net(&[Sec::Owe]), &Opts::default());
        assert_eq!(b.len(), 1);
        assert_eq!(get(&b[0], "key_mgmt"), Some("OWE"));
        assert_eq!(get(&b[0], "ieee80211w"), Some("2"));
        // Open + auto-upgraded OWE: OWE first, open one step lower.
        let o = Opts { priority: 10, mac: Mac::Random, ..Opts::default() };
        let b = lines(&net(&[Sec::Open, Sec::Owe]), &o);
        assert_eq!((get(&b[0], "key_mgmt"), get(&b[0], "priority")), (Some("OWE"), Some("10")));
        assert_eq!((get(&b[1], "key_mgmt"), get(&b[1], "priority")), (Some("NONE"), Some("9")));
        assert_eq!(get(&b[1], "mac_addr"), Some("1"));
    }

    #[test]
    fn wep_keys() {
        let mut n = net(&[Sec::Wep]);
        n.wep_keys = [Some("\"abcde\"".into()), None, Some("0123456789".into()), None];
        n.wep_tx_keyidx = 2;
        let b = lines(&n, &Opts::default());
        assert_eq!(
            b[0][1..6],
            ["key_mgmt NONE", "auth_alg OPEN SHARED", "wep_key0 \"abcde\"", "wep_key2 0123456789", "wep_tx_keyidx 2"]
        );
        assert!(unusable(&n, false).unwrap().contains("Allow WEP"));
        n.wep_keys[2] = Some("0123".into());
        assert_eq!(unusable(&n, true).as_deref(), Some("WEP key 3 is not a valid key"));
        n.wep_keys = Default::default();
        assert_eq!(unusable(&n, true).as_deref(), Some("no WEP key was handed over"));
    }

    fn peap() -> Eap {
        Eap {
            method: "PEAP".into(),
            phase2: "MSCHAPV2".into(),
            identity: Some("alice".into()),
            anonymous_identity: Some("anonymous@corp.example".into()),
            password: Some("s3cret".into()),
            ca_path: Some("/system/etc/security/cacerts".into()),
            domain_suffix_match: Some("radius.corp.example".into()),
            ..Eap::default()
        }
    }

    #[test]
    fn peap_mschapv2_with_system_cas() {
        let mut n = net(&[Sec::Eap, Sec::EapWpa3]);
        n.eap = Some(peap());
        let b = lines(&n, &Opts::default());
        assert_eq!(
            b[0],
            vec![
                "ssid 486f6d65",
                "key_mgmt WPA-EAP IEEE8021X WPA-EAP-SHA256",
                "ieee80211w 1",
                "pairwise CCMP TKIP GCMP-256",
                "group CCMP TKIP GCMP-256",
                "eap PEAP",
                "phase2 \"auth=MSCHAPV2\"",
                &format!("identity {}", hex(b"alice")),
                &format!("anonymous_identity {}", hex(b"anonymous@corp.example")),
                &format!("password {}", hex(b"s3cret")),
                "domain_suffix_match \"radius.corp.example\"",
                "ca_path \"/system/etc/security/cacerts\"",
                "phase1 \"tls_disable_tlsv1_3=0 tls_disable_tlsv1_2=0 tls_disable_tlsv1_1=0 tls_disable_tlsv1_0=0\"",
                "mac_addr 0",
                "priority 1",
            ]
        );
        assert_eq!(get(&b[1], "key_mgmt"), Some("FT-EAP"));
        assert!(!uses_keystore(&n));
        // WPA3-Enterprise only: PMF required.
        let mut n = net(&[Sec::EapWpa3]);
        n.eap = Some(peap());
        let b = lines(&n, &Opts::default());
        assert_eq!(get(&b[0], "ieee80211w"), Some("2"));
        assert_eq!(get(&b[0], "pairwise"), Some("CCMP GCMP-256"));
    }

    #[test]
    fn ttls_gtc_pap_pwd_and_min_tls() {
        let mut n = net(&[Sec::Eap]);
        n.eap = Some(Eap { method: "TTLS".into(), phase2: "GTC".into(), min_tls: 2, ..peap() });
        let b = lines(&n, &Opts::default());
        assert_eq!(get(&b[0], "phase2"), Some("\"autheap=GTC\""));
        assert!(get(&b[0], "phase1").unwrap().contains("tls_disable_tlsv1_1=1 tls_disable_tlsv1_0=1\""));
        n.eap = Some(Eap { method: "TTLS".into(), phase2: "PAP".into(), ..peap() });
        assert_eq!(get(&lines(&n, &Opts::default())[0], "phase2"), Some("\"auth=PAP\""));
        n.eap = Some(Eap { method: "PWD".into(), phase2: String::new(), ..peap() });
        let b = lines(&n, &Opts::default());
        assert_eq!(get(&b[0], "eap"), Some("PWD"));
        assert_eq!(get(&b[0], "phase2"), None);
        n.eap = Some(Eap { password: None, ..peap() });
        assert_eq!(unusable(&n, true).as_deref(), Some("no enterprise password was handed over"));
        n.eap = Some(Eap { identity: None, ..peap() });
        assert!(unusable(&n, true).unwrap().contains("identity"));
    }

    #[test]
    fn tls_with_keystore_certificates() {
        let mut n = net(&[Sec::Eap]);
        n.eap = Some(Eap {
            method: "TLS".into(),
            phase2: String::new(),
            identity: Some("dev42".into()),
            password: None,
            anonymous_identity: None,
            ca_cert: Some("keystore://CACERT_corp".into()),
            client_cert: Some("keystore://USRCERT_corp".into()),
            key_id: Some("USRPKEY_corp".into()),
            engine: true,
            engine_id: Some("keystore".into()),
            ocsp: 1,
            ..Eap::default()
        });
        let b = lines(&n, &Opts::default());
        for want in [
            "eap TLS",
            "client_cert \"keystore://USRCERT_corp\"",
            "ca_cert \"keystore://CACERT_corp\"",
            "engine_id \"keystore\"",
            "engine 1",
            "key_id \"USRPKEY_corp\"",
            "ocsp 1",
        ] {
            assert!(b[0].iter().any(|l| l == want), "missing {want}: {:?}", b[0]);
        }
        assert!(uses_keystore(&n));
        n.eap.as_mut().unwrap().client_cert = None;
        assert!(unusable(&n, true).unwrap().contains("client certificate"));
    }

    #[test]
    fn wpa3_enterprise_192_bit() {
        let mut n = net(&[Sec::EapSuiteB]);
        n.suiteb_rsa = true;
        n.eap = Some(Eap {
            method: "TLS".into(),
            phase2: String::new(),
            identity: Some("dev".into()),
            client_cert: Some("keystore://USRCERT_x".into()),
            ca_cert: Some("keystore://CACERT_x".into()),
            ..Eap::default()
        });
        let b = lines(&n, &Opts::default());
        assert_eq!(get(&b[0], "key_mgmt"), Some("WPA-EAP IEEE8021X WPA-EAP-SUITE-B-192 WPA-EAP-SHA256"));
        assert_eq!(get(&b[0], "ieee80211w"), Some("2"));
        assert_eq!(get(&b[0], "pairwise"), Some("GCMP GCMP-256"));
        assert_eq!(get(&b[0], "group_mgmt"), Some("BIP-GMAC-256"));
        assert_eq!(
            get(&b[0], "phase1"),
            Some("\"tls_disable_tlsv1_3=1 tls_disable_tlsv1_2=0 tls_disable_tlsv1_1=1 tls_disable_tlsv1_0=1 tls_suiteb=1\"")
        );
        assert_eq!(get(&b[0], "openssl_ciphers"), None);
        assert_eq!(get(&b[1], "key_mgmt"), Some("FT-EAP FT-EAP-SHA384"));
        n.suiteb_rsa = false;
        n.suiteb_ecdsa = true;
        assert_eq!(get(&lines(&n, &Opts::default())[0], "openssl_ciphers"), Some("\"SUITEB192\""));
    }

    #[test]
    fn fils_akms() {
        let mut n = net(&[Sec::Eap]);
        n.eap = Some(peap());
        n.fils_sha256 = true;
        n.fils_sha384 = true;
        let b = lines(&n, &Opts::default());
        assert_eq!(get(&b[0], "key_mgmt"), Some("WPA-EAP IEEE8021X WPA-EAP-SHA256 FILS-SHA256 FILS-SHA384"));
    }

    #[test]
    fn sim_based_and_others_are_unusable_with_a_reason() {
        let mut n = net(&[Sec::Eap]);
        n.eap = Some(Eap { method: "AKA'".into(), ..Eap::default() });
        assert!(unusable(&n, true).unwrap().contains("SIM-based"));
        n.eap = Some(Eap { phase2: "SIM".into(), ..peap() });
        assert!(unusable(&n, true).unwrap().contains("SIM-based"));
        n.eap = None;
        assert!(unusable(&n, true).unwrap().contains("not handed over"));
        assert!(unusable(&net(&[Sec::WapiPsk]), true).unwrap().contains("WAPI"));
        assert!(unusable(&net(&[Sec::Osen]), true).unwrap().contains("OSEN"));
        assert!(unusable(&net(&[Sec::Unknown("x".into())]), true).unwrap().contains("unknown"));
        let mut n = net(&[Sec::Psk]);
        n.security_off = vec![Sec::Psk];
        assert!(unusable(&n, true).unwrap().contains("transition disable"));
    }

    #[test]
    fn dpp_keys() {
        let mut n = net(&[Sec::Dpp]);
        n.dpp = Some(Dpp { connector: "eyJ0.x.y".into(), csign_hex: "3059".into(), netaccesskey_hex: "3077".into() });
        let b = lines(&n, &Opts::default());
        assert_eq!(b.len(), 1);
        assert_eq!(
            b[0][1..7],
            [
                "key_mgmt DPP",
                "ieee80211w 2",
                "pairwise CCMP GCMP GCMP-256",
                "group CCMP GCMP GCMP-256",
                "dpp_connector \"eyJ0.x.y\"",
                "dpp_csign 3059"
            ]
        );
        n.dpp.as_mut().unwrap().csign_hex = "xyz".into();
        assert!(unusable(&n, true).unwrap().contains("incomplete"));
    }

    #[test]
    fn fast_attempt_fields() {
        let mut n = net(&[Sec::Psk]);
        n.passphrase = Some("password1".into());
        let o = Opts { freq: Some(5180), bssid: Some("aa:bb:cc:dd:ee:ff".into()), priority: 32, ..Opts::default() };
        let b = lines(&n, &o);
        let tail: Vec<&str> = b[0].iter().rev().take(4).rev().map(String::as_str).collect();
        assert_eq!(tail, ["mac_addr 0", "scan_freq 5180", "bssid_hint aa:bb:cc:dd:ee:ff", "priority 32"]);
    }

    #[test]
    fn control_characters_are_refused() {
        let mut n = net(&[Sec::Eap]);
        n.eap = Some(Eap { domain_suffix_match: Some("a\nb".into()), ..peap() });
        assert!(unusable(&n, true).unwrap().contains("control characters"));
    }

    #[test]
    fn redacts_secrets() {
        for (cmd, want) in [
            ("IFNAME=wlan0 SET_NETWORK 0 psk \"secret\"", "IFNAME=wlan0 SET_NETWORK 0 psk <hidden>"),
            ("IFNAME=wlan0 SET_NETWORK 3 identity 616c696365", "IFNAME=wlan0 SET_NETWORK 3 identity <hidden>"),
            ("IFNAME=wlan0 SET_NETWORK 3 password 7333", "IFNAME=wlan0 SET_NETWORK 3 password <hidden>"),
            ("IFNAME=wlan0 SET_NETWORK 3 anonymous_identity 61", "IFNAME=wlan0 SET_NETWORK 3 anonymous_identity <hidden>"),
            ("IFNAME=wlan0 SET_NETWORK 1 wep_key0 \"abcde\"", "IFNAME=wlan0 SET_NETWORK 1 wep_key0 <hidden>"),
            ("IFNAME=wlan0 SET_NETWORK 1 sae_password 6162", "IFNAME=wlan0 SET_NETWORK 1 sae_password <hidden>"),
            ("IFNAME=wlan0 SET_NETWORK 0 ssid 4142", "IFNAME=wlan0 SET_NETWORK 0 ssid 4142"),
            ("IFNAME=wlan0 SET_NETWORK 0 key_mgmt WPA-PSK SAE", "IFNAME=wlan0 SET_NETWORK 0 key_mgmt WPA-PSK SAE"),
        ] {
            assert_eq!(redact(cmd), want);
        }
    }

    #[test]
    fn every_secret_field_in_any_plan_is_redacted() {
        let mut n = net(&[Sec::Eap]);
        n.eap = Some(peap());
        let mut p = net(&[Sec::Psk, Sec::Sae]);
        p.passphrase = Some("password1".into());
        let mut w = net(&[Sec::Wep]);
        w.wep_keys[0] = Some("\"abcde\"".into());
        for n in [n, p, w] {
            for b in plan(&n, &Opts::default()).unwrap() {
                for (k, v) in &b.fields {
                    let cmd = format!("IFNAME=wlan0 SET_NETWORK 0 {k} {v}");
                    let shown = redact(&cmd);
                    for secret in ["alice", "616c696365", "s3cret", "password1", "abcde"] {
                        assert!(!shown.contains(secret), "{shown}");
                    }
                }
            }
        }
    }

    #[test]
    fn candidate_order() {
        let mk = |key: &str, hex: &str, last: u64, rssi: Option<i32>| Network {
            key: key.into(),
            ssid_hex: hex.into(),
            last_connected_ms: last,
            rssi,
            ..net(&[Sec::Psk])
        };
        let all = vec![
            mk("a", "61", 0, Some(-80)),
            mk("b", "62", 50, None),
            mk("c", "63", 0, Some(-40)),
            mk("d", "64", 90, Some(-90)),
            mk("e", "65", 10, None),
        ];
        let keys = |v: Vec<Network>| v.into_iter().map(|n| n.key).collect::<Vec<_>>().join("");
        assert_eq!(keys(order(all.clone(), None, None, 16)), "dbeca");
        assert_eq!(keys(order(all.clone(), Some("65"), Some("61"), 16)), "eadbc");
        assert_eq!(keys(order(all.clone(), Some("61"), Some("61"), 3)), "adb");
        assert_eq!(keys(order(all, Some("ff"), None, 16)), "dbeca");
    }

    #[test]
    fn event_hints() {
        let h = |e: &str, ks: bool| event_hint(e, ks);
        assert!(h("IFNAME=wlan0 <3>CTRL-EVENT-EAP-TLS-CERT-ERROR reason=1 depth=0", false).unwrap().contains("server certificate"));
        assert!(h("<3>CTRL-EVENT-EAP-FAILURE EAP authentication failed", true).unwrap().contains("keystore"));
        assert!(h("<3>CTRL-EVENT-SSID-TEMP-DISABLED id=0 ssid=\"Corp\" auth_failures=1 duration=10 reason=AUTH_FAILED", false)
            .unwrap()
            .contains("user name or password"));
        assert_eq!(
            h("<3>CTRL-REQ-PASSWORD-3:Password needed for SSID Corp", false).as_deref(),
            Some("the network asked for a password that Android did not hand over")
        );
        assert!(h("<3>CTRL-REQ-SIM-0:GSM-AUTH:abc needed for SSID Op", false).unwrap().contains("SIM"));
        assert_eq!(
            h("<3>CTRL-EVENT-ASSOC-REJECT bssid=aa:bb:cc:dd:ee:ff status_code=17", false).as_deref(),
            Some("the router refused the connection (status 17)")
        );
        assert!(h("<3>CTRL-EVENT-NETWORK-NOT-FOUND", false).is_some());
        assert!(h("<3>CTRL-EVENT-SCAN-RESULTS", false).is_none());
    }
}
