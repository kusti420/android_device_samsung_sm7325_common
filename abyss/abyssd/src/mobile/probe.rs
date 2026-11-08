//! `abyssd --qmi-probe`: what this modem offers, read-only (impl-mobile-data.md §10, P1-P7).
//!
//! Safe while rild runs: it opens its own QMI clients and sends only Get messages (never a Set,
//! never an identity read: no IMEI/IMSI/ICCID/MSISDN, no AID bytes). Needs root (`su -c`):
//! AF_QIPCRTR itself needs no permission, but the shell's SELinux domain may not create one.
//! Prints to stdout; exit code 0 if the name service answered.

use super::qmi::{self, Req};
use super::qrtr::{self, Client, Server};
use super::rmnet;

fn show(m: &qmi::Msg) -> String {
    let mut out = match m.result() {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("error {} ({e:#06x})", qmi::error_name(e)),
    };
    for (t, v) in &m.tlvs {
        if *t == 0x02 {
            continue;
        }
        let hex: Vec<String> = v.iter().take(48).map(|b| format!("{b:02x}")).collect();
        out.push_str(&format!("\n      tlv 0x{t:02x} [{}] {}{}", v.len(), hex.join(" "), if v.len() > 48 { " …" } else { "" }));
    }
    out
}

fn ask(srv: Option<Server>, name: &'static str, req: Req) -> Result<qmi::Msg, String> {
    let srv = srv.ok_or_else(|| format!("no {name} service"))?;
    let mut c = Client::open(srv, name).map_err(|e| format!("{e:#}"))?;
    c.call(req, 3_000).map_err(|e| format!("{e:#}"))
}

pub fn main() -> i32 {
    println!("abyssd qmi probe (read-only)");
    // P1
    let all = match qrtr::lookup(0, 3_000) {
        Ok(v) => v,
        Err(e) => {
            println!("P1 qrtr lookup failed: {e:#}");
            return 1;
        }
    };
    println!("P1 qrtr services ({}):", all.len());
    let mut sorted = all.clone();
    sorted.sort_by_key(|s| (s.node, s.service, s.instance));
    for s in &sorted {
        println!(
            "   node {:>3} port {:>5}  service 0x{:04x} {:<8} instance 0x{:04x} (v{} i{})",
            s.node,
            s.port,
            s.service,
            qmi::service_name(s.service),
            s.instance,
            s.instance & 0xFF,
            s.instance >> 8
        );
    }
    let wds = qrtr::pick(&all, qmi::SVC_WDS, None);
    let node = wds.map(|s| s.node);
    let on_modem = |svc| qrtr::pick(&all, svc, node);
    println!("   modem node: {node:?}");

    // P2
    match ask(on_modem(qmi::SVC_DMS), "dms", Req::new(qmi::DMS_GET_REVISION)) {
        Ok(m) => println!("P2 dms revision: {}", qmi::parse_string(&m, 0x01).unwrap_or_else(|| show(&m))),
        Err(e) => println!("P2 dms revision: {e}"),
    }
    match ask(on_modem(qmi::SVC_DMS), "dms", qmi::get_operating_mode()) {
        Ok(m) => match qmi::parse_operating_mode(&m) {
            Ok(mode) => println!("P2 dms operating mode: {mode} {}  [{}]", qmi::mode_name(mode), show(&m)),
            Err(e) => println!("P2 dms operating mode: {e:#}"),
        },
        Err(e) => println!("P2 dms operating mode: {e}"),
    }

    // P3
    match ask(on_modem(qmi::SVC_UIM), "uim", qmi::get_card_status()) {
        Ok(m) => match qmi::parse_card_status(&m) {
            Ok(cs) => {
                println!("P3 uim: gw primary {:#06x}, gw secondary {:#06x}", cs.gw_primary, cs.gw_secondary);
                for (i, c) in cs.cards.iter().enumerate() {
                    println!("   slot {}: card state {} error {}", i + 1, c.state, c.error);
                    for a in &c.apps {
                        println!(
                            "      app type {} state {} perso {} pin1 state {} (retries {}) aid {} bytes",
                            a.kind,
                            a.state,
                            a.perso_state,
                            a.pin1_state,
                            a.pin1_retries,
                            a.aid.len()
                        );
                    }
                    println!("      verdict for data: {:?}", redact(qmi::sim_verdict(&cs, i as u8)));
                }
            }
            Err(e) => println!("P3 uim card status: {e:#}"),
        },
        Err(e) => println!("P3 uim card status: {e}"),
    }

    // P4
    match ask(on_modem(qmi::SVC_NAS), "nas", qmi::get_serving_system()) {
        Ok(m) => match qmi::parse_serving_system(&m) {
            Ok(s) => println!(
                "P4 nas serving: reg {} ps {} radios {:?} roaming {:?} plmn {:?} name {:?}",
                s.reg,
                s.ps_attached,
                s.radios,
                s.roaming,
                s.mcc_mnc(),
                s.plmn_name
            ),
            Err(e) => println!("P4 nas serving: {e:#}"),
        },
        Err(e) => println!("P4 nas serving: {e}"),
    }
    match ask(on_modem(qmi::SVC_NAS), "nas", qmi::get_signal_info()) {
        Ok(m) => println!("P4 nas signal: {}", show(&m)),
        Err(e) => println!("P4 nas signal: {e}"),
    }

    // P5
    match ask(wds, "wds", qmi::get_profile_list()) {
        Ok(m) => match qmi::parse_profile_list(&m) {
            Ok(l) => println!("P5 wds 3gpp profiles: {l:?}"),
            Err(e) => println!("P5 wds profiles: {e:#} [{}]", show(&m)),
        },
        Err(e) => println!("P5 wds profiles: {e}"),
    }
    match ask(wds, "wds", qmi::get_packet_service_status()) {
        Ok(m) => println!("P5 wds packet status (this client): {:?}  [{}]", qmi::parse_packet_status(&m), show(&m)),
        Err(e) => println!("P5 wds packet status: {e}"),
    }

    // P6
    match ask(on_modem(qmi::SVC_WDA), "wda", qmi::get_data_format()) {
        Ok(m) => println!("P6 wda data format, endpoint {{4,1}}: {}", show(&m)),
        Err(e) => println!("P6 wda data format: {e}"),
    }

    // P7
    match rmnet::links() {
        Ok(l) => {
            for x in l.iter().filter(|x| x.name.starts_with("rmnet")) {
                let (rx, tx) = rmnet::counters(&x.name);
                println!(
                    "P7 link {} index {} parent {:?} kind {:?} mux {:?} rx {rx} tx {tx}",
                    x.name, x.index, x.parent, x.kind, x.mux_id
                );
            }
            println!("P7 abyssd would use: {:?}", rmnet::pick_data_link(&l));
        }
        Err(e) => println!("P7 rmnet links: {e:#}"),
    }
    0
}

/// The verdict without AID bytes.
fn redact(v: qmi::SimVerdict) -> String {
    match v {
        qmi::SimVerdict::NoSession { slot, aid } => format!("NoSession {{ slot: {slot}, aid: {} bytes }}", aid.len()),
        other => format!("{other:?}"),
    }
}
