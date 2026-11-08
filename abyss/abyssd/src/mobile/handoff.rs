//! /data/misc/abyss/handoff/mobile.json: the SIM's data settings from Android.
//!
//! Written by the entry app (entry/src/org/ichthys/abyss/MobileHandoff.java) right before
//! entering, while the telephony framework still runs; deleted by BootReceiver after the
//! return (it may hold an APN password). 0640 system:system (setgid dir).
//!
//! ```json
//! {"v":1,"written_ms":1762600000000,"slot":0,"sub_id":1,"mcc_mnc":"24801",
//!  "operator":"Telia EE","data_enabled":true,"roaming_allowed":false,"sim_state":"READY",
//!  "apn":{"apn":"internet.emt.ee","protocol":"IPV4V6","roaming_protocol":"IP","auth":-1,
//!         "user":"","password":"","types":"default,supl"}}
//! ```

use serde_json::Value;

use super::qmi::Apn;

pub const PATH: &str = "/data/misc/abyss/handoff/mobile.json";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Handoff {
    /// 0-based SIM slot of Android's default data subscription.
    pub slot: u8,
    pub mcc_mnc: String,
    /// The network operator Android showed (fallback: the SIM's operator name).
    pub operator: String,
    pub data_enabled: bool,
    pub roaming_allowed: bool,
    /// Android's SIM state name at hand-over ("READY", "PIN_REQUIRED", …).
    pub sim_state: String,
    pub apn: Option<Apn>,
}

impl Handoff {
    /// Why mobile data cannot be used, in plain words (None = it may be tried).
    pub fn refusal(&self) -> Option<String> {
        if self.slot != 0 {
            return Some("Mobile data in Abyss uses SIM 1 only for now".into());
        }
        if !self.data_enabled {
            return Some("Mobile data is off in Android".into());
        }
        match self.sim_state.as_str() {
            "PIN_REQUIRED" | "PUK_REQUIRED" | "NETWORK_LOCKED" => {
                return Some("SIM locked: enter the PIN in Android".into());
            }
            "ABSENT" => return Some("No SIM".into()),
            _ => {}
        }
        if self.apn.as_ref().is_none_or(|a| a.apn.is_empty()) {
            return Some("No mobile data access point (APN) for this SIM in Android".into());
        }
        None
    }
}

pub fn parse(text: &str) -> Option<Handoff> {
    let v: Value = serde_json::from_str(text).ok()?;
    if v["v"].as_i64() != Some(1) {
        return None;
    }
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let apn = v.get("apn").filter(|a| a.is_object()).map(|a| Apn {
        apn: a["apn"].as_str().unwrap_or("").trim().to_string(),
        auth: a["auth"].as_i64().unwrap_or(-1) as i32,
        user: a["user"].as_str().unwrap_or("").to_string(),
        password: a["password"].as_str().unwrap_or("").to_string(),
    });
    Some(Handoff {
        slot: v["slot"].as_u64().unwrap_or(0).min(7) as u8,
        mcc_mnc: s("mcc_mnc"),
        operator: s("operator"),
        data_enabled: v["data_enabled"].as_bool().unwrap_or(false),
        roaming_allowed: v["roaming_allowed"].as_bool().unwrap_or(false),
        sim_state: s("sim_state"),
        apn,
    })
}

pub fn load() -> Option<Handoff> {
    parse(&std::fs::read_to_string(PATH).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"v":1,"written_ms":1,"slot":0,"sub_id":1,"mcc_mnc":"24801",
        "operator":"Telia EE","data_enabled":true,"roaming_allowed":false,"sim_state":"READY",
        "apn":{"apn":" internet.emt.ee ","protocol":"IPV4V6","auth":-1,"user":"","password":""}}"#;

    #[test]
    fn parses_and_allows() {
        let h = parse(SAMPLE).unwrap();
        assert_eq!(h.operator, "Telia EE");
        assert_eq!(h.apn.as_ref().unwrap().apn, "internet.emt.ee");
        assert_eq!(h.apn.as_ref().unwrap().auth, -1);
        assert_eq!(h.refusal(), None);
    }

    #[test]
    fn refusals() {
        let h = parse(SAMPLE).unwrap();
        assert!(Handoff { data_enabled: false, ..h.clone() }.refusal().unwrap().contains("off in Android"));
        assert!(Handoff { sim_state: "PIN_REQUIRED".into(), ..h.clone() }.refusal().unwrap().contains("SIM locked"));
        assert!(Handoff { slot: 1, ..h.clone() }.refusal().unwrap().contains("SIM 1"));
        assert!(Handoff { apn: None, ..h.clone() }.refusal().unwrap().contains("APN"));
        assert!(parse(r#"{"v":2}"#).is_none());
        assert!(parse("not json").is_none());
    }
}
