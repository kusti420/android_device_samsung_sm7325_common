//! QMI messages over QRTR: the TLV codec and the few messages Abyss sends (pure, host-tested).
//!
//! On QRTR there is no QMUX header and no CTL service: one AF_QIPCRTR socket is one client.
//! A message is `u8 type` (0 request, 2 response, 4 indication), `u16 txn`, `u16 msg id`,
//! `u16 len`, then TLVs `u8 type, u16 len, value`, all little-endian (kernel
//! include/linux/soc/qcom/qmi.h `struct qmi_header`). Every response carries TLV 0x02
//! `{u16 result, u16 error}`.
//!
//! Only standard QMI messages are used (the same ones libsec-ril uses on this modem); nothing
//! here reads an identity (IMEI, IMSI, ICCID, MSISDN). Message and TLV numbers follow the QMI
//! service definitions as documented by libqmi (data/qmi-service-{dms,nas,wds,uim,wda}.json).

use std::net::Ipv4Addr;

use anyhow::{anyhow, bail, Result};

pub const SVC_WDS: u32 = 0x01;
pub const SVC_DMS: u32 = 0x02;
pub const SVC_NAS: u32 = 0x03;
pub const SVC_UIM: u32 = 0x0B;
pub const SVC_WDA: u32 = 0x1A;

pub const TYPE_REQUEST: u8 = 0;
pub const TYPE_RESPONSE: u8 = 2;
pub const TYPE_INDICATION: u8 = 4;

// DMS
pub const DMS_GET_REVISION: u16 = 0x0023;
pub const DMS_GET_OPERATING_MODE: u16 = 0x002D;
pub const DMS_SET_OPERATING_MODE: u16 = 0x002E;
// NAS
pub const NAS_GET_SERVING_SYSTEM: u16 = 0x0024;
pub const NAS_SET_SYSTEM_SELECTION_PREFERENCE: u16 = 0x0033;
pub const NAS_GET_SYSTEM_SELECTION_PREFERENCE: u16 = 0x0034;
pub const NAS_GET_SYSTEM_INFO: u16 = 0x004D;
pub const NAS_GET_SIGNAL_INFO: u16 = 0x004F;
// WDS
pub const WDS_START_NETWORK: u16 = 0x0020;
pub const WDS_STOP_NETWORK: u16 = 0x0021;
pub const WDS_PACKET_SERVICE_STATUS: u16 = 0x0022;
pub const WDS_GO_DORMANT: u16 = 0x0025;
pub const WDS_GET_PROFILE_LIST: u16 = 0x002A;
pub const WDS_GET_CURRENT_SETTINGS: u16 = 0x002D;
pub const WDS_BIND_MUX_DATA_PORT: u16 = 0x00A2;
// UIM
pub const UIM_GET_CARD_STATUS: u16 = 0x002F;
pub const UIM_CHANGE_PROVISIONING_SESSION: u16 = 0x0038;
// WDA
pub const WDA_GET_DATA_FORMAT: u16 = 0x0021;

/// QMI_ERR_NO_EFFECT: "already in that state" (e.g. stopping a call that is already gone).
pub const ERR_NO_EFFECT: u16 = 0x001A;

pub fn service_name(id: u32) -> &'static str {
    match id {
        0x01 => "wds",
        0x02 => "dms",
        0x03 => "nas",
        0x04 => "qos",
        0x05 => "wms",
        0x06 => "pds",
        0x07 => "auth",
        0x08 => "at",
        0x09 => "voice",
        0x0A => "cat2",
        0x0B => "uim",
        0x0C => "pbm",
        0x10 => "loc",
        0x11 => "sar",
        0x12 => "ims",
        0x14 => "csd",
        0x1A => "wda",
        0x1E => "dfs",
        0x22 => "coex",
        0x24 => "pdc",
        0x29 => "rfrpe",
        0x2A => "dsd",
        0x2B => "ssctl",
        0x2F => "dpm",
        0x31 => "ims-a",
        0x36 => "ims-dcm",
        _ => "?",
    }
}

// ---------------------------------------------------------------------------------------------
// Codec

/// An outgoing message.
pub struct Req {
    msg_id: u16,
    body: Vec<u8>,
}

impl Req {
    pub fn new(msg_id: u16) -> Req {
        Req { msg_id, body: Vec::new() }
    }

    pub fn tlv(mut self, t: u8, v: &[u8]) -> Req {
        self.body.push(t);
        self.body.extend_from_slice(&(v.len() as u16).to_le_bytes());
        self.body.extend_from_slice(v);
        self
    }

    pub fn u8(self, t: u8, v: u8) -> Req {
        self.tlv(t, &[v])
    }

    pub fn u32(self, t: u8, v: u32) -> Req {
        self.tlv(t, &v.to_le_bytes())
    }

    pub fn msg_id(&self) -> u16 {
        self.msg_id
    }

    pub fn encode(&self, txn: u16) -> Vec<u8> {
        let mut out = Vec::with_capacity(7 + self.body.len());
        out.push(TYPE_REQUEST);
        out.extend_from_slice(&txn.to_le_bytes());
        out.extend_from_slice(&self.msg_id.to_le_bytes());
        out.extend_from_slice(&(self.body.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.body);
        out
    }
}

/// An incoming message (response or indication).
#[derive(Debug, Clone)]
pub struct Msg {
    pub kind: u8,
    pub txn: u16,
    pub msg_id: u16,
    pub tlvs: Vec<(u8, Vec<u8>)>,
}

impl Msg {
    pub fn decode(b: &[u8]) -> Result<Msg> {
        if b.len() < 7 {
            bail!("short QMI message ({} bytes)", b.len());
        }
        let kind = b[0];
        let txn = u16::from_le_bytes([b[1], b[2]]);
        let msg_id = u16::from_le_bytes([b[3], b[4]]);
        let len = u16::from_le_bytes([b[5], b[6]]) as usize;
        let body = b.get(7..7 + len).ok_or_else(|| anyhow!("QMI message length {len} past the end"))?;
        let mut tlvs = Vec::new();
        let mut i = 0;
        while i < body.len() {
            if i + 3 > body.len() {
                bail!("truncated TLV header");
            }
            let t = body[i];
            let l = u16::from_le_bytes([body[i + 1], body[i + 2]]) as usize;
            let v = body.get(i + 3..i + 3 + l).ok_or_else(|| anyhow!("TLV 0x{t:02x} length {l} past the end"))?;
            tlvs.push((t, v.to_vec()));
            i += 3 + l;
        }
        Ok(Msg { kind, txn, msg_id, tlvs })
    }

    pub fn tlv(&self, t: u8) -> Option<&[u8]> {
        self.tlvs.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_slice())
    }

    pub fn u8(&self, t: u8) -> Option<u8> {
        self.tlv(t).and_then(|v| v.first().copied())
    }

    pub fn u16(&self, t: u8) -> Option<u16> {
        self.tlv(t).and_then(|v| Some(u16::from_le_bytes([*v.first()?, *v.get(1)?])))
    }

    pub fn u32(&self, t: u8) -> Option<u32> {
        self.tlv(t).and_then(|v| Some(u32::from_le_bytes(v.get(..4)?.try_into().ok()?)))
    }

    pub fn ipv4(&self, t: u8) -> Option<Ipv4Addr> {
        self.u32(t).map(Ipv4Addr::from)
    }

    /// TLV 0x02: Ok(()) or the QMI error code.
    pub fn result(&self) -> std::result::Result<(), u16> {
        match self.tlv(0x02) {
            Some(v) if v.len() >= 4 => {
                let res = u16::from_le_bytes([v[0], v[1]]);
                let err = u16::from_le_bytes([v[2], v[3]]);
                if res == 0 {
                    Ok(())
                } else {
                    Err(err)
                }
            }
            _ => Err(0xFFFF),
        }
    }

    /// `result()` as anyhow, with the message name for the log/screen.
    pub fn ok(&self, what: &str) -> Result<()> {
        self.result().map_err(|e| anyhow!("{what}: modem error {} ({e:#06x})", error_name(e)))
    }
}

pub fn error_name(e: u16) -> &'static str {
    match e {
        0x0001 => "malformed message",
        0x0002 => "no memory",
        0x0003 => "internal",
        0x0004 => "aborted",
        0x0005 => "client ids exhausted",
        0x0006 => "unabortable transaction",
        0x0007 => "invalid client id",
        0x0008 => "no thresholds",
        0x0009 => "invalid handle",
        0x000A => "invalid profile",
        0x000B => "invalid PIN id",
        0x000C => "incorrect PIN",
        0x000D => "no network found",
        0x000E => "call failed",
        0x000F => "out of call",
        0x0010 => "not provisioned",
        0x0011 => "missing argument",
        0x0013 => "argument too long",
        0x0016 => "invalid transaction id",
        0x0017 => "device in use",
        0x0018 => "network unsupported",
        0x0019 => "device unsupported",
        0x001A => "no effect",
        0x001B => "no free profile",
        0x001C => "invalid PDP type",
        0x001D => "invalid technology preference",
        0x001E => "invalid profile type",
        0x001F => "invalid service type",
        0x0020 => "invalid register action",
        0x0021 => "invalid PS attach action",
        0x0022 => "authentication failed",
        0x0023 => "PIN blocked",
        0x0024 => "PIN permanently blocked",
        0x0025 => "SIM not initialized",
        0x0029 => "info unavailable",
        0x002B => "access denied",
        0x002E => "invalid argument",
        0x0030 => "invalid message id",
        0x0031 => "operation in progress",
        0x003A => "not supported",
        0x003F => "SIM file not found",
        0x0052 => "requested number unsupported",
        0x005A => "invalid operation",
        0xFFFF => "no result",
        _ => "error",
    }
}

// ---------------------------------------------------------------------------------------------
// DMS

/// DMS operating modes (QMI_DMS_OPERATING_MODE).
pub const MODE_ONLINE: u8 = 0;
pub const MODE_LOW_POWER: u8 = 1;
pub const MODE_PERSISTENT_LOW_POWER: u8 = 6;
pub const MODE_MODE_ONLY_LOW_POWER: u8 = 7;

pub fn mode_name(m: u8) -> &'static str {
    match m {
        0 => "online",
        1 => "low-power",
        2 => "factory-test",
        3 => "offline",
        4 => "resetting",
        5 => "shutting-down",
        6 => "persistent-low-power",
        7 => "mode-only-low-power",
        8 => "online-no-tx",
        _ => "?",
    }
}

pub fn get_operating_mode() -> Req {
    Req::new(DMS_GET_OPERATING_MODE)
}

pub fn set_operating_mode(mode: u8) -> Req {
    Req::new(DMS_SET_OPERATING_MODE).u8(0x01, mode)
}

pub fn parse_operating_mode(m: &Msg) -> Result<u8> {
    m.ok("get operating mode")?;
    m.u8(0x01).ok_or_else(|| anyhow!("get operating mode: no mode"))
}

pub fn parse_string(m: &Msg, t: u8) -> Option<String> {
    m.tlv(t).map(|v| String::from_utf8_lossy(v).trim_end_matches('\0').to_string())
}

// ---------------------------------------------------------------------------------------------
// UIM

pub const CARD_ABSENT: u8 = 0;
pub const CARD_PRESENT: u8 = 1;

pub const APP_SIM: u8 = 1;
pub const APP_USIM: u8 = 2;

pub const APP_DETECTED: u8 = 1;
pub const APP_PIN_REQUIRED: u8 = 2;
pub const APP_PUK_REQUIRED: u8 = 3;
pub const APP_CHECK_PERSO: u8 = 4;
pub const APP_PIN_BLOCKED: u8 = 5;
pub const APP_ILLEGAL: u8 = 6;
pub const APP_READY: u8 = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UimApp {
    pub kind: u8,
    pub state: u8,
    pub perso_state: u8,
    pub aid: Vec<u8>,
    pub pin1_state: u8,
    pub pin1_retries: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UimCard {
    pub state: u8,
    pub error: u8,
    pub apps: Vec<UimApp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardStatus {
    /// (slot << 8) | app, 0xFFFF = no session.
    pub gw_primary: u16,
    pub gw_secondary: u16,
    pub cards: Vec<UimCard>,
}

/// What the SIM means for a data call, in plain words for the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimVerdict {
    Ready { slot: u8, kind: u8 },
    /// The PIN (or PUK) is needed: typing it lives in Android.
    Locked(&'static str),
    NoSim,
    /// Card present, app not ready yet (initialising): wait.
    NotReady,
    /// Card present, but no primary provisioning session: activate one for (slot, aid).
    NoSession { slot: u8, aid: Vec<u8> },
    Unusable(String),
}

struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Rd<'a> {
    fn new(b: &'a [u8]) -> Rd<'a> {
        Rd { b, i: 0 }
    }
    fn u8(&mut self) -> Result<u8> {
        let v = *self.b.get(self.i).ok_or_else(|| anyhow!("card status truncated"))?;
        self.i += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes([self.u8()?, self.u8()?]))
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let v = self.b.get(self.i..self.i + n).ok_or_else(|| anyhow!("card status truncated"))?;
        self.i += n;
        Ok(v)
    }
}

pub fn get_card_status() -> Req {
    Req::new(UIM_GET_CARD_STATUS)
}

pub fn parse_card_status(m: &Msg) -> Result<CardStatus> {
    m.ok("get card status")?;
    let v = m.tlv(0x10).ok_or_else(|| anyhow!("get card status: no card status"))?;
    let mut r = Rd::new(v);
    let gw_primary = r.u16()?;
    let _1x_primary = r.u16()?;
    let gw_secondary = r.u16()?;
    let _1x_secondary = r.u16()?;
    let n_cards = r.u8()?;
    let mut cards = Vec::new();
    for _ in 0..n_cards {
        let state = r.u8()?;
        let _upin_state = r.u8()?;
        let _upin_retries = r.u8()?;
        let _upuk_retries = r.u8()?;
        let error = r.u8()?;
        let n_apps = r.u8()?;
        let mut apps = Vec::new();
        for _ in 0..n_apps {
            let kind = r.u8()?;
            let state = r.u8()?;
            let perso_state = r.u8()?;
            let _perso_feature = r.u8()?;
            let _perso_retries = r.u8()?;
            let _perso_unblock = r.u8()?;
            let aid_len = r.u8()? as usize;
            let aid = r.bytes(aid_len)?.to_vec();
            let _upin_replaces_pin1 = r.u8()?;
            let pin1_state = r.u8()?;
            let pin1_retries = r.u8()?;
            let _puk1_retries = r.u8()?;
            let _pin2_state = r.u8()?;
            let _pin2_retries = r.u8()?;
            let _puk2_retries = r.u8()?;
            apps.push(UimApp { kind, state, perso_state, aid, pin1_state, pin1_retries });
        }
        cards.push(UimCard { state, error, apps });
    }
    Ok(CardStatus { gw_primary, gw_secondary, cards })
}

/// `slot`: the SIM slot (0-based) Android uses for data (hand-over).
pub fn sim_verdict(cs: &CardStatus, slot: u8) -> SimVerdict {
    let Some(card) = cs.cards.get(slot as usize) else { return SimVerdict::NoSim };
    if card.state == CARD_ABSENT {
        return SimVerdict::NoSim;
    }
    if card.state != CARD_PRESENT {
        return SimVerdict::Unusable(format!("SIM error ({})", card.error));
    }
    // The 3GPP app (USIM, else SIM) of this card.
    let pick = card
        .apps
        .iter()
        .enumerate()
        .find(|(_, a)| a.kind == APP_USIM)
        .or_else(|| card.apps.iter().enumerate().find(|(_, a)| a.kind == APP_SIM));
    let Some((app_idx, app)) = pick else {
        return if card.apps.is_empty() { SimVerdict::NotReady } else { SimVerdict::Unusable("no SIM/USIM application".into()) };
    };
    match app.state {
        APP_PIN_REQUIRED => return SimVerdict::Locked("PIN"),
        APP_PUK_REQUIRED | APP_PIN_BLOCKED => return SimVerdict::Locked("PUK"),
        APP_ILLEGAL => return SimVerdict::Unusable("the SIM is blocked by the network".into()),
        // Personalisation (network lock) needs Android's unlock screen too.
        APP_CHECK_PERSO if app.perso_state == 2 => return SimVerdict::Locked("network unlock code"),
        _ => {}
    }
    let session = cs.gw_primary;
    let session_here = session != 0xFFFF && (session >> 8) as u8 == slot && (session & 0xFF) as usize == app_idx;
    if !session_here {
        if session != 0xFFFF {
            // Primary session on another slot: the modem's data SIM is not Android's.
            return SimVerdict::Unusable(format!("the modem uses SIM {} for data", (session >> 8) + 1));
        }
        return if matches!(app.state, APP_DETECTED | APP_READY) {
            SimVerdict::NoSession { slot, aid: app.aid.clone() }
        } else {
            SimVerdict::NotReady
        };
    }
    if app.state == APP_READY {
        SimVerdict::Ready { slot, kind: app.kind }
    } else {
        SimVerdict::NotReady
    }
}

/// Activate the GW primary provisioning session for (slot, aid) (what QCRIL does at boot).
pub fn change_provisioning_session(slot: u8, aid: &[u8]) -> Req {
    let mut app = vec![slot + 1, aid.len() as u8];
    app.extend_from_slice(aid);
    // 0x01: session type 0 = primary GW provisioning, activate 1. 0x10: slot (1-based) + AID.
    Req::new(UIM_CHANGE_PROVISIONING_SESSION).tlv(0x01, &[0, 1]).tlv(0x10, &app)
}

// ---------------------------------------------------------------------------------------------
// NAS

pub const RADIO_GSM: u8 = 4;
pub const RADIO_UMTS: u8 = 5;
pub const RADIO_LTE: u8 = 8;
pub const RADIO_TDSCDMA: u8 = 9;
pub const RADIO_NR5G: u8 = 12;

pub fn radio_name(r: u8) -> &'static str {
    match r {
        RADIO_GSM => "2G",
        RADIO_UMTS => "3G",
        RADIO_LTE => "LTE",
        RADIO_TDSCDMA => "3G",
        RADIO_NR5G => "5G",
        1..=3 => "CDMA",
        _ => "?",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Serving {
    /// 0 not registered, 1 registered, 2 searching, 3 denied, 4 unknown.
    pub reg: u8,
    /// 0 unknown, 1 attached, 2 detached.
    pub ps_attached: u8,
    pub radios: Vec<u8>,
    pub roaming: Option<bool>,
    pub mcc: Option<u16>,
    pub mnc: Option<u16>,
    pub plmn_name: Option<String>,
}

impl Serving {
    pub fn registered(&self) -> bool {
        self.reg == 1 && self.ps_attached == 1
    }

    /// The best radio in use: 5G (NSA shows LTE + NR) > LTE > 3G > 2G.
    pub fn rat(&self) -> Option<u8> {
        [RADIO_NR5G, RADIO_LTE, RADIO_UMTS, RADIO_TDSCDMA, RADIO_GSM]
            .into_iter()
            .find(|r| self.radios.contains(r))
            .or_else(|| self.radios.first().copied())
    }

    pub fn mcc_mnc(&self) -> Option<String> {
        let (mcc, mnc) = (self.mcc?, self.mnc?);
        // Two-digit MNCs print as two digits ("24801"); a three-digit one as three.
        Some(if mnc < 100 { format!("{mcc:03}{mnc:02}") } else { format!("{mcc:03}{mnc:03}") })
    }
}

pub fn get_serving_system() -> Req {
    Req::new(NAS_GET_SERVING_SYSTEM)
}

pub fn parse_serving_system(m: &Msg) -> Result<Serving> {
    m.ok("get serving system")?;
    let v = m.tlv(0x01).ok_or_else(|| anyhow!("get serving system: no serving system"))?;
    if v.len() < 5 {
        bail!("get serving system: short serving system");
    }
    let n = v[4] as usize;
    let radios = v.get(5..5 + n).unwrap_or(&[]).to_vec();
    let mut s = Serving { reg: v[0], ps_attached: v[2], radios, ..Serving::default() };
    // 0x10 roaming indicator: 0 = roaming ON, 1 = off (home).
    s.roaming = m.u8(0x10).map(|r| r == 0);
    if let Some(p) = m.tlv(0x12) {
        if p.len() >= 5 {
            s.mcc = Some(u16::from_le_bytes([p[0], p[1]]));
            s.mnc = Some(u16::from_le_bytes([p[2], p[3]]));
            let l = p[4] as usize;
            let name = p.get(5..5 + l).map(|d| String::from_utf8_lossy(d).trim().to_string());
            s.plmn_name = name.filter(|n| !n.is_empty() && n.chars().all(|c| !c.is_control()));
        }
    }
    Ok(s)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Signal {
    /// LTE RSRP (dBm).
    pub lte_rsrp: Option<i16>,
    /// 5G NR RSRP (dBm).
    pub nr_rsrp: Option<i16>,
    /// WCDMA / GSM RSSI (dBm).
    pub rssi_3g: Option<i8>,
    pub rssi_2g: Option<i8>,
}

impl Signal {
    /// 0..=4 bars, Android's default thresholds (CarrierConfig lte_rsrp_thresholds_int
    /// -128/-118/-108/-98; NR ss_rsrp -110/-90/-80/-65; WCDMA rssi -113..; GSM -107/-103/-97/-89).
    pub fn bars(&self, rat: Option<u8>) -> Option<u8> {
        let level = |v: i32, t: [i32; 4]| t.iter().filter(|&&x| v >= x).count() as u8;
        let lte = self.lte_rsrp.filter(|&v| (-140..=-44).contains(&v)).map(|v| level(v as i32, [-128, -118, -108, -98]));
        let nr = self.nr_rsrp.filter(|&v| (-140..=-44).contains(&v)).map(|v| level(v as i32, [-110, -90, -80, -65]));
        let w = self.rssi_3g.filter(|&v| v < 0).map(|v| level(v as i32, [-107, -102, -97, -89]));
        let g = self.rssi_2g.filter(|&v| v < 0).map(|v| level(v as i32, [-107, -103, -97, -89]));
        match rat {
            Some(RADIO_NR5G) => nr.or(lte),
            Some(RADIO_LTE) => lte,
            Some(RADIO_UMTS) | Some(RADIO_TDSCDMA) => w,
            Some(RADIO_GSM) => g,
            _ => lte.or(nr).or(w).or(g),
        }
    }
}

pub fn get_signal_info() -> Req {
    Req::new(NAS_GET_SIGNAL_INFO)
}

pub fn parse_signal_info(m: &Msg) -> Result<Signal> {
    m.ok("get signal info")?;
    // 0x12 GSM {i8 rssi}; 0x13 WCDMA {i8 rssi, i16 ecio}; 0x14 LTE {i8 rssi, i8 rsrq, i16 rsrp,
    // i16 snr}; 0x17 5G {i16 rsrp, i16 snr}.
    Ok(Signal {
        rssi_2g: m.tlv(0x12).and_then(|v| v.first()).map(|&b| b as i8),
        rssi_3g: m.tlv(0x13).and_then(|v| v.first()).map(|&b| b as i8),
        lte_rsrp: m.tlv(0x14).filter(|v| v.len() >= 4).map(|v| i16::from_le_bytes([v[2], v[3]])),
        nr_rsrp: m.tlv(0x17).filter(|v| v.len() >= 2).map(|v| i16::from_le_bytes([v[0], v[1]])),
    })
}

/// NR state from NAS Get System Info (TLV ids as in libqmi 1.38's qmi-service-nas.json).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NrInfo {
    /// 0x4A NR5G Service Status Info, first byte: 0 none, 1 limited, 2 available, 4 power save.
    pub service: Option<u8>,
    /// 0x4E "EUTRA with NR5G availability": the LTE cell offers EN-DC (5G NSA anchor).
    pub endc_available: Option<bool>,
    /// 0x4F "DCNR restriction": the network does not allow EN-DC for this subscription.
    pub dcnr_restricted: Option<bool>,
}

pub fn get_system_info() -> Req {
    Req::new(NAS_GET_SYSTEM_INFO)
}

pub fn parse_system_info(m: &Msg) -> Result<NrInfo> {
    m.ok("get system info")?;
    Ok(NrInfo {
        service: m.u8(0x4A),
        endc_available: m.u8(0x4E).map(|b| b != 0),
        dcnr_restricted: m.u8(0x4F).map(|b| b != 0),
    })
}

/// The radio shown on the status line: "5G SA" (NR alone), "5G NSA" (LTE anchor with an NR
/// leg in use: NR service available, or an NR signal measured on an EN-DC cell), else the
/// serving radio's own name. `radios` = Get Serving System's radio list.
pub fn radio_label(radios: &[u8], nr: &NrInfo, sig: &Signal) -> Option<&'static str> {
    let lte = radios.contains(&RADIO_LTE);
    let nr_listed = radios.contains(&RADIO_NR5G);
    if nr_listed && !lte {
        return Some("5G SA");
    }
    if lte {
        let restricted = nr.dcnr_restricted == Some(true) || nr.endc_available == Some(false);
        let nr_rsrp = sig.nr_rsrp.is_some_and(|v| (-140..=-44).contains(&v));
        if nr_listed || (!restricted && (nr.service == Some(2) || nr_rsrp)) {
            return Some("5G NSA");
        }
        return Some(radio_name(RADIO_LTE));
    }
    [RADIO_UMTS, RADIO_TDSCDMA, RADIO_GSM].into_iter().find(|r| radios.contains(r)).or_else(|| radios.first().copied()).map(radio_name)
}

/// QMI NAS RAT mode preference bits (TLV 0x11 of Get/Set System Selection Preference).
pub const MODE_PREF_LTE: u16 = 1 << 4;
pub const MODE_PREF_NR5G: u16 = 1 << 6;

pub fn get_system_selection_preference() -> Req {
    Req::new(NAS_GET_SYSTEM_SELECTION_PREFERENCE)
}

pub fn parse_mode_preference(m: &Msg) -> Result<u16> {
    m.ok("get system selection preference")?;
    m.u16(0x11).ok_or_else(|| anyhow!("get system selection preference: no mode preference"))
}

/// Sets only the RAT mode preference, until the next modem power cycle (0x17 change duration
/// 0): the modem's saved (NV) preference that rild set is never written.
pub fn set_mode_preference(mode: u16) -> Req {
    Req::new(NAS_SET_SYSTEM_SELECTION_PREFERENCE).tlv(0x11, &mode.to_le_bytes()).u8(0x17, 0)
}

/// The mode preference for Abyss from the one Android left (`found`): LTE = without NR,
/// 5G = with NR. None = leave it as it is (already so, or not an LTE-capable preference).
pub fn wanted_mode_preference(found: u16, five_g: bool) -> Option<u16> {
    if found & MODE_PREF_LTE == 0 {
        return None;
    }
    let want = if five_g { found | MODE_PREF_NR5G } else { found & !MODE_PREF_NR5G };
    (want != found).then_some(want)
}

// ---------------------------------------------------------------------------------------------
// WDS

/// IPA embedded endpoint of rmnet_ipa0 (kernel rmnet_ipa.c: RMNET_IOCTL_GET_EPID = 1;
/// qmi_rmnet_i.h DATA_EP_TYPE_EMBEDDED_V01 = 4).
pub const EP_TYPE_EMBEDDED: u32 = 4;
pub const EP_IFACE_IPA: u32 = 1;

fn ep_id(ep_type: u32, iface: u32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&ep_type.to_le_bytes());
    b[4..].copy_from_slice(&iface.to_le_bytes());
    b
}

pub fn bind_mux_data_port(mux_id: u8) -> Req {
    // 0x10 endpoint {u32 type, u32 iface}, 0x11 mux id, 0x13 client type 1 = tethered (the AP).
    Req::new(WDS_BIND_MUX_DATA_PORT)
        .tlv(0x10, &ep_id(EP_TYPE_EMBEDDED, EP_IFACE_IPA))
        .u8(0x11, mux_id)
        .u32(0x13, 1)
}

/// The APN the call uses (hand-over).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Apn {
    pub apn: String,
    /// Android's authtype: -1 unset, 0 none, 1 PAP, 2 CHAP, 3 PAP or CHAP.
    pub auth: i32,
    pub user: String,
    pub password: String,
}

pub fn start_network(apn: &Apn) -> Req {
    let mut r = Req::new(WDS_START_NETWORK).tlv(0x14, apn.apn.as_bytes()).u8(0x19, 4);
    // 0x16 authentication preference bitmask: 1 PAP, 2 CHAP. Unset (-1) with a user name means
    // "PAP or CHAP" in Android (ApnSetting); without a user nothing is sent.
    let auth = match apn.auth {
        -1 if !apn.user.is_empty() => 3,
        a @ 0..=3 => a as u8,
        _ => 0,
    };
    if auth != 0 || !apn.user.is_empty() {
        r = r.u8(0x16, auth);
    }
    if !apn.user.is_empty() {
        r = r.tlv(0x17, apn.user.as_bytes());
    }
    if !apn.password.is_empty() {
        r = r.tlv(0x18, apn.password.as_bytes());
    }
    r
}

/// Start Network's answer: the packet data handle, or why the call failed.
pub fn parse_start_network(m: &Msg) -> std::result::Result<u32, String> {
    if let Err(e) = m.result() {
        let verbose = m.tlv(0x11).filter(|v| v.len() >= 4).map(|v| {
            (u16::from_le_bytes([v[0], v[1]]), u16::from_le_bytes([v[2], v[3]]))
        });
        let reason = m.u16(0x10);
        return Err(match verbose {
            Some((t, r)) => format!("{} (end reason {} {r})", error_name(e), call_end_type(t)),
            None => match reason {
                Some(r) => format!("{} (end reason {r})", error_name(e)),
                None => error_name(e).to_string(),
            },
        });
    }
    m.u32(0x01).ok_or_else(|| "no call handle".to_string())
}

fn call_end_type(t: u16) -> &'static str {
    match t {
        1 => "mobile-ip",
        2 => "internal",
        3 => "call-manager",
        6 => "3gpp",
        7 => "ppp",
        8 => "ehrpd",
        9 => "ipv6",
        12 => "handoff",
        _ => "other",
    }
}

pub fn stop_network(handle: u32) -> Req {
    Req::new(WDS_STOP_NETWORK).u32(0x01, handle)
}

pub fn go_dormant() -> Req {
    Req::new(WDS_GO_DORMANT)
}

pub fn get_packet_service_status() -> Req {
    Req::new(WDS_PACKET_SERVICE_STATUS)
}

/// 1 disconnected, 2 connected, 3 suspended, 4 authenticating.
pub fn parse_packet_status(m: &Msg) -> Option<u8> {
    if m.kind == TYPE_RESPONSE {
        m.result().ok()?;
    }
    m.u8(0x01)
}

pub fn get_profile_list() -> Req {
    Req::new(WDS_GET_PROFILE_LIST).u8(0x10, 0)
}

/// (index, name) of each 3GPP profile.
pub fn parse_profile_list(m: &Msg) -> Result<Vec<(u8, String)>> {
    m.ok("get profile list")?;
    let v = m.tlv(0x01).ok_or_else(|| anyhow!("no profile list"))?;
    let mut r = Rd::new(v);
    let n = r.u8()?;
    let mut out = Vec::new();
    for _ in 0..n {
        let _ty = r.u8()?;
        let idx = r.u8()?;
        let l = r.u8()? as usize;
        out.push((idx, String::from_utf8_lossy(r.bytes(l)?).to_string()));
    }
    Ok(out)
}

/// Requested-settings mask: DNS (1<<4), IP address (1<<8), gateway (1<<9), MTU (1<<13).
pub fn get_current_settings() -> Req {
    Req::new(WDS_GET_CURRENT_SETTINGS).u32(0x10, (1 << 4) | (1 << 8) | (1 << 9) | (1 << 13))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpSettings {
    pub ip: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub mtu: Option<u32>,
}

impl IpSettings {
    pub fn subnet(&self) -> String {
        let mask = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix as u32) };
        format!("{}/{}", Ipv4Addr::from(u32::from(self.ip) & mask), self.prefix)
    }
}

pub fn parse_current_settings(m: &Msg) -> Result<IpSettings> {
    m.ok("get current settings")?;
    let ip = m.ipv4(0x1E).filter(|a| !a.is_unspecified()).ok_or_else(|| anyhow!("the call has no IPv4 address"))?;
    let prefix = match m.u32(0x21) {
        Some(mask) if mask != 0 => mask.leading_ones() as u8,
        _ => 32,
    };
    let gateway = m.ipv4(0x20).filter(|a| !a.is_unspecified());
    let dns = [0x15, 0x16].into_iter().filter_map(|t| m.ipv4(t)).filter(|a| !a.is_unspecified()).collect();
    let mtu = m.u32(0x29).filter(|&v| (576..=9000).contains(&v));
    Ok(IpSettings { ip, prefix, gateway, dns, mtu })
}

// ---------------------------------------------------------------------------------------------
// WDA

pub fn get_data_format() -> Req {
    Req::new(WDA_GET_DATA_FORMAT).tlv(0x10, &ep_id(EP_TYPE_EMBEDDED, EP_IFACE_IPA))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(msg_id: u16, tlvs: &[(u8, &[u8])]) -> Msg {
        let mut r = Req::new(msg_id).tlv(0x02, &[0, 0, 0, 0]);
        for (t, v) in tlvs {
            r = r.tlv(*t, v);
        }
        let mut b = r.encode(7);
        b[0] = TYPE_RESPONSE;
        Msg::decode(&b).unwrap()
    }

    #[test]
    fn encode_layout() {
        let b = set_operating_mode(MODE_ONLINE).encode(0x1234);
        assert_eq!(b, vec![0, 0x34, 0x12, 0x2E, 0x00, 4, 0, 0x01, 1, 0, 0]);
    }

    #[test]
    fn decode_roundtrip_and_result() {
        let m = resp(DMS_GET_OPERATING_MODE, &[(0x01, &[1])]);
        assert_eq!(m.txn, 7);
        assert_eq!(m.msg_id, DMS_GET_OPERATING_MODE);
        assert_eq!(parse_operating_mode(&m).unwrap(), MODE_LOW_POWER);
        let mut b = Req::new(1).tlv(0x02, &[1, 0, 0x1A, 0]).encode(1);
        b[0] = TYPE_RESPONSE;
        let e = Msg::decode(&b).unwrap();
        assert_eq!(e.result(), Err(ERR_NO_EFFECT));
    }

    #[test]
    fn decode_rejects_truncation() {
        assert!(Msg::decode(&[2, 0, 0, 1, 0]).is_err());
        assert!(Msg::decode(&[2, 0, 0, 1, 0, 9, 0, 1]).is_err());
        assert!(Msg::decode(&[2, 0, 0, 1, 0, 4, 0, 1, 5, 0, 1]).is_err());
    }

    fn card_status(gw: u16, apps: &[(u8, u8)]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&gw.to_le_bytes());
        v.extend_from_slice(&0xFFFFu16.to_le_bytes());
        v.extend_from_slice(&0xFFFFu16.to_le_bytes());
        v.extend_from_slice(&0xFFFFu16.to_le_bytes());
        v.push(2); // two slots
        // slot 1: present
        v.extend_from_slice(&[CARD_PRESENT, 0, 3, 10, 0, apps.len() as u8]);
        for (kind, state) in apps {
            v.extend_from_slice(&[*kind, *state, 0, 0, 0, 0]);
            v.push(4);
            v.extend_from_slice(&[0xA0, 0, 0, 0x87]);
            v.extend_from_slice(&[0, 1, 3, 10, 1, 3, 10]);
        }
        // slot 2: absent
        v.extend_from_slice(&[CARD_ABSENT, 0, 0, 0, 0, 0]);
        v
    }

    #[test]
    fn sim_states() {
        let ready = resp(UIM_GET_CARD_STATUS, &[(0x10, &card_status(0x0000, &[(APP_USIM, APP_READY)]))]);
        let cs = parse_card_status(&ready).unwrap();
        assert_eq!(cs.cards.len(), 2);
        assert_eq!(cs.cards[0].apps[0].aid, vec![0xA0, 0, 0, 0x87]);
        assert_eq!(sim_verdict(&cs, 0), SimVerdict::Ready { slot: 0, kind: APP_USIM });
        assert_eq!(sim_verdict(&cs, 1), SimVerdict::NoSim);

        let pin = resp(UIM_GET_CARD_STATUS, &[(0x10, &card_status(0x0000, &[(APP_USIM, APP_PIN_REQUIRED)]))]);
        assert_eq!(sim_verdict(&parse_card_status(&pin).unwrap(), 0), SimVerdict::Locked("PIN"));
        let puk = resp(UIM_GET_CARD_STATUS, &[(0x10, &card_status(0x0000, &[(APP_USIM, APP_PIN_BLOCKED)]))]);
        assert_eq!(sim_verdict(&parse_card_status(&puk).unwrap(), 0), SimVerdict::Locked("PUK"));

        let nosess = resp(UIM_GET_CARD_STATUS, &[(0x10, &card_status(0xFFFF, &[(APP_USIM, APP_DETECTED)]))]);
        assert_eq!(
            sim_verdict(&parse_card_status(&nosess).unwrap(), 0),
            SimVerdict::NoSession { slot: 0, aid: vec![0xA0, 0, 0, 0x87] }
        );
        // Session on slot 2 while Android's data SIM is slot 1.
        let other = resp(UIM_GET_CARD_STATUS, &[(0x10, &card_status(0x0100, &[(APP_USIM, APP_READY)]))]);
        assert!(matches!(sim_verdict(&parse_card_status(&other).unwrap(), 0), SimVerdict::Unusable(_)));
        // ISIM first, USIM second: the USIM (index 1) is the one the session must point at.
        let two = resp(UIM_GET_CARD_STATUS, &[(0x10, &card_status(0x0001, &[(5, APP_READY), (APP_USIM, APP_READY)]))]);
        assert_eq!(sim_verdict(&parse_card_status(&two).unwrap(), 0), SimVerdict::Ready { slot: 0, kind: APP_USIM });
        // Truncated status is an error, not a panic.
        let bad = resp(UIM_GET_CARD_STATUS, &[(0x10, &card_status(0, &[(APP_USIM, APP_READY)])[..20])]);
        assert!(parse_card_status(&bad).is_err());
    }

    #[test]
    fn provisioning_request() {
        let b = change_provisioning_session(0, &[0xA0, 0x01]).encode(1);
        let m = Msg::decode(&b).unwrap();
        assert_eq!(m.tlv(0x01), Some(&[0u8, 1][..]));
        assert_eq!(m.tlv(0x10), Some(&[1u8, 2, 0xA0, 0x01][..]));
    }

    #[test]
    fn serving_system() {
        let mut plmn = Vec::new();
        plmn.extend_from_slice(&248u16.to_le_bytes());
        plmn.extend_from_slice(&1u16.to_le_bytes());
        plmn.push(8);
        plmn.extend_from_slice(b"Telia EE");
        let m = resp(NAS_GET_SERVING_SYSTEM, &[(0x01, &[1, 1, 1, 2, 2, RADIO_LTE, RADIO_NR5G]), (0x10, &[1]), (0x12, &plmn)]);
        let s = parse_serving_system(&m).unwrap();
        assert!(s.registered());
        assert_eq!(s.rat(), Some(RADIO_NR5G));
        assert_eq!(s.roaming, Some(false));
        assert_eq!(s.mcc_mnc().as_deref(), Some("24801"));
        assert_eq!(s.plmn_name.as_deref(), Some("Telia EE"));
        let searching = resp(NAS_GET_SERVING_SYSTEM, &[(0x01, &[2, 0, 2, 0, 0])]);
        let s = parse_serving_system(&searching).unwrap();
        assert!(!s.registered());
        assert_eq!(s.rat(), None);
        let roam = resp(NAS_GET_SERVING_SYSTEM, &[(0x01, &[1, 1, 1, 2, 1, RADIO_LTE]), (0x10, &[0])]);
        assert_eq!(parse_serving_system(&roam).unwrap().roaming, Some(true));
    }

    #[test]
    fn signal_bars() {
        let mut lte = vec![0xC0u8, 0xF6];
        lte.extend_from_slice(&(-105i16).to_le_bytes());
        lte.extend_from_slice(&50i16.to_le_bytes());
        let m = resp(NAS_GET_SIGNAL_INFO, &[(0x14, &lte)]);
        let s = parse_signal_info(&m).unwrap();
        assert_eq!(s.lte_rsrp, Some(-105));
        assert_eq!(s.bars(Some(RADIO_LTE)), Some(3));
        assert_eq!(Signal { lte_rsrp: Some(-90), ..Default::default() }.bars(Some(RADIO_LTE)), Some(4));
        assert_eq!(Signal { lte_rsrp: Some(-130), ..Default::default() }.bars(Some(RADIO_LTE)), Some(0));
        // Invalid / missing values give no bars rather than a wrong number.
        assert_eq!(Signal { lte_rsrp: Some(0), ..Default::default() }.bars(Some(RADIO_LTE)), None);
        assert_eq!(Signal { lte_rsrp: Some(-100), ..Default::default() }.bars(Some(RADIO_NR5G)), Some(3));
    }

    #[test]
    fn nr_label() {
        let none = NrInfo::default();
        let sig = Signal::default();
        assert_eq!(radio_label(&[RADIO_LTE], &none, &sig), Some("LTE"));
        assert_eq!(radio_label(&[RADIO_NR5G], &none, &sig), Some("5G SA"));
        assert_eq!(radio_label(&[RADIO_LTE, RADIO_NR5G], &none, &sig), Some("5G NSA"));
        let nsa = NrInfo { service: Some(2), endc_available: Some(true), dcnr_restricted: Some(false) };
        assert_eq!(radio_label(&[RADIO_LTE], &nsa, &sig), Some("5G NSA"));
        // An NR measurement on an EN-DC cell counts; on a restricted one it does not.
        let nr_sig = Signal { nr_rsrp: Some(-95), ..Default::default() };
        assert_eq!(radio_label(&[RADIO_LTE], &none, &nr_sig), Some("5G NSA"));
        let restricted = NrInfo { dcnr_restricted: Some(true), ..nsa };
        assert_eq!(radio_label(&[RADIO_LTE], &restricted, &nr_sig), Some("LTE"));
        // EN-DC offered but no NR leg (idle, LTE-only preference): still LTE.
        let ready = NrInfo { service: Some(0), endc_available: Some(true), dcnr_restricted: Some(false) };
        assert_eq!(radio_label(&[RADIO_LTE], &ready, &sig), Some("LTE"));
        assert_eq!(radio_label(&[RADIO_UMTS], &none, &sig), Some("3G"));
        let m = resp(NAS_GET_SYSTEM_INFO, &[(0x4A, &[2, 2, 0]), (0x4E, &[1]), (0x4F, &[0])]);
        assert_eq!(parse_system_info(&m).unwrap(), nsa);
    }

    #[test]
    fn mode_preference() {
        let android = 0x04 | 0x08 | MODE_PREF_LTE | MODE_PREF_NR5G; // GSM UMTS LTE NR
        assert_eq!(wanted_mode_preference(android, false), Some(0x04 | 0x08 | MODE_PREF_LTE));
        assert_eq!(wanted_mode_preference(android, true), None);
        assert_eq!(wanted_mode_preference(0x1C, true), Some(0x1C | MODE_PREF_NR5G));
        assert_eq!(wanted_mode_preference(0x1C, false), None);
        assert_eq!(wanted_mode_preference(0x0C, false), None, "no LTE: left alone");
        let r = set_mode_preference(0x1C).encode(7);
        assert_eq!(&r[3..5], &NAS_SET_SYSTEM_SELECTION_PREFERENCE.to_le_bytes());
        assert_eq!(&r[7..], &[0x11, 2, 0, 0x1C, 0, 0x17, 1, 0, 0]);
        let m = resp(NAS_GET_SYSTEM_SELECTION_PREFERENCE, &[(0x11, &[0x5C, 0])]);
        assert_eq!(parse_mode_preference(&m).unwrap(), 0x5C);
    }

    #[test]
    fn start_network_tlvs() {
        let apn = Apn { apn: "internet.emt.ee".into(), auth: -1, ..Default::default() };
        let m = Msg::decode(&start_network(&apn).encode(1)).unwrap();
        assert_eq!(m.tlv(0x14), Some(&b"internet.emt.ee"[..]));
        assert_eq!(m.u8(0x19), Some(4));
        assert!(m.tlv(0x16).is_none() && m.tlv(0x17).is_none() && m.tlv(0x18).is_none());
        let apn = Apn { apn: "x".into(), auth: -1, user: "u".into(), password: "p".into() };
        let m = Msg::decode(&start_network(&apn).encode(1)).unwrap();
        assert_eq!(m.u8(0x16), Some(3));
        assert_eq!(m.tlv(0x17), Some(&b"u"[..]));
        assert_eq!(m.tlv(0x18), Some(&b"p"[..]));
        let apn = Apn { apn: "x".into(), auth: 1, user: "u".into(), password: String::new() };
        let m = Msg::decode(&start_network(&apn).encode(1)).unwrap();
        assert_eq!(m.u8(0x16), Some(1));
    }

    #[test]
    fn start_network_answers() {
        let ok = resp(WDS_START_NETWORK, &[(0x01, &0xCAFEu32.to_le_bytes())]);
        assert_eq!(parse_start_network(&ok), Ok(0xCAFE));
        let mut b = Req::new(WDS_START_NETWORK).tlv(0x02, &[1, 0, 0x0E, 0]).tlv(0x10, &[3, 0]).tlv(0x11, &[6, 0, 33, 0]).encode(1);
        b[0] = TYPE_RESPONSE;
        let e = parse_start_network(&Msg::decode(&b).unwrap()).unwrap_err();
        assert!(e.contains("call failed") && e.contains("3gpp 33"), "{e}");
    }

    #[test]
    fn current_settings() {
        let le = |a: [u8; 4]| u32::from(Ipv4Addr::from(a)).to_le_bytes();
        let m = resp(
            WDS_GET_CURRENT_SETTINGS,
            &[
                (0x15, &le([194, 0, 2, 53])),
                (0x16, &le([0, 0, 0, 0])),
                (0x1E, &le([10, 77, 1, 6])),
                (0x20, &le([10, 77, 1, 5])),
                (0x21, &le([255, 255, 255, 252])),
                (0x29, &1500u32.to_le_bytes()),
            ],
        );
        let s = parse_current_settings(&m).unwrap();
        assert_eq!(s.ip, Ipv4Addr::new(10, 77, 1, 6));
        assert_eq!(s.prefix, 30);
        assert_eq!(s.subnet(), "10.77.1.4/30");
        assert_eq!(s.gateway, Some(Ipv4Addr::new(10, 77, 1, 5)));
        assert_eq!(s.dns, vec![Ipv4Addr::new(194, 0, 2, 53)]);
        assert_eq!(s.mtu, Some(1500));
        let none = resp(WDS_GET_CURRENT_SETTINGS, &[(0x29, &1500u32.to_le_bytes())]);
        assert!(parse_current_settings(&none).is_err());
    }

    #[test]
    fn bind_mux_layout() {
        let m = Msg::decode(&bind_mux_data_port(1).encode(1)).unwrap();
        assert_eq!(m.tlv(0x10), Some(&[4u8, 0, 0, 0, 1, 0, 0, 0][..]));
        assert_eq!(m.u8(0x11), Some(1));
        assert_eq!(m.u32(0x13), Some(1));
    }

    #[test]
    fn profile_list() {
        let m = resp(WDS_GET_PROFILE_LIST, &[(0x01, &[2, 0, 1, 3, b'i', b'n', b't', 0, 2, 0])]);
        assert_eq!(parse_profile_list(&m).unwrap(), vec![(1, "int".to_string()), (2, String::new())]);
    }
}
