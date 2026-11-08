//! The data interface: which rmnet_dataN carries which QMAP mux id, and its byte counters.
//!
//! libsec-ril creates rmnet_data0..10 on rmnet_ipa0 at boot (rmnet_core links, kind "rmnet");
//! abyssd neither creates nor deletes links (rild re-configures them when it starts again). The
//! mux id is read from rtnetlink (RTM_GETLINK dump: IFLA_LINKINFO / IFLA_INFO_DATA /
//! IFLA_RMNET_MUX_ID), because toybox `ip` does not print it.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{bail, Context, Result};

const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_DUMP: u16 = 0x300;
const IFLA_IFNAME: u16 = 3;
const IFLA_LINK: u16 = 5;
const IFLA_LINKINFO: u16 = 18;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const IFLA_RMNET_MUX_ID: u16 = 1;

pub const PARENT: &str = "rmnet_ipa0";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub name: String,
    pub index: i32,
    pub parent: Option<i32>,
    pub kind: Option<String>,
    pub mux_id: Option<u16>,
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// Netlink attributes in `b`: (type without flags, payload).
fn attrs(b: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= b.len() {
        let len = u16::from_ne_bytes([b[i], b[i + 1]]) as usize;
        let ty = u16::from_ne_bytes([b[i + 2], b[i + 3]]) & 0x3FFF;
        if len < 4 || i + len > b.len() {
            break;
        }
        out.push((ty, &b[i + 4..i + len]));
        i += align4(len);
    }
    out
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).to_string()
}

/// One RTM_NEWLINK payload (ifinfomsg + attributes).
pub fn parse_link(payload: &[u8]) -> Option<Link> {
    // struct ifinfomsg: u8 family, u8 pad, u16 type, i32 index, u32 flags, u32 change = 16 bytes.
    if payload.len() < 16 {
        return None;
    }
    let index = i32::from_ne_bytes(payload[4..8].try_into().ok()?);
    let mut l = Link { name: String::new(), index, parent: None, kind: None, mux_id: None };
    for (t, v) in attrs(&payload[16..]) {
        match t {
            IFLA_IFNAME => l.name = cstr(v),
            IFLA_LINK if v.len() >= 4 => l.parent = Some(i32::from_ne_bytes(v[..4].try_into().ok()?)),
            IFLA_LINKINFO => {
                for (it, iv) in attrs(v) {
                    match it {
                        IFLA_INFO_KIND => l.kind = Some(cstr(iv)),
                        IFLA_INFO_DATA => {
                            for (dt, dv) in attrs(iv) {
                                if dt == IFLA_RMNET_MUX_ID && dv.len() >= 2 {
                                    l.mux_id = Some(u16::from_ne_bytes([dv[0], dv[1]]));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Some(l)
}

/// Every link (RTM_GETLINK dump).
pub fn links() -> Result<Vec<Link>> {
    // SAFETY: plain socket(2); the fd is owned right below.
    let raw = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE) };
    if raw < 0 {
        return Err(io::Error::last_os_error()).context("netlink socket");
    }
    // SAFETY: fresh fd, owned here.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let tv = libc::timeval { tv_sec: 2, tv_usec: 0 };
    // SAFETY: setsockopt with a valid timeval of the right size.
    unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    // nlmsghdr (16) + ifinfomsg (16).
    let mut req = [0u8; 32];
    req[0..4].copy_from_slice(&32u32.to_ne_bytes());
    req[4..6].copy_from_slice(&RTM_GETLINK.to_ne_bytes());
    req[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    req[8..12].copy_from_slice(&1u32.to_ne_bytes());
    req[16] = libc::AF_UNSPEC as u8;
    // SAFETY: `req` is valid for its length.
    let n = unsafe { libc::send(fd.as_raw_fd(), req.as_ptr() as *const libc::c_void, req.len(), 0) };
    if n < 0 {
        return Err(io::Error::last_os_error()).context("netlink send");
    }
    let mut out = Vec::new();
    let mut buf = vec![0u8; 32768];
    loop {
        // SAFETY: `buf` is valid and writable for its length.
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error()).context("netlink recv");
        }
        let b = &buf[..n as usize];
        let mut i = 0;
        while i + 16 <= b.len() {
            let len = u32::from_ne_bytes(b[i..i + 4].try_into().unwrap_or_default()) as usize;
            let ty = u16::from_ne_bytes([b[i + 4], b[i + 5]]);
            if len < 16 || i + len > b.len() {
                break;
            }
            match ty {
                NLMSG_DONE => return Ok(out),
                NLMSG_ERROR => bail!("netlink: RTM_GETLINK refused"),
                RTM_NEWLINK => {
                    if let Some(l) = parse_link(&b[i + 16..i + len]) {
                        out.push(l);
                    }
                }
                _ => {}
            }
            i += align4(len);
        }
    }
}

/// The data link for a call: the lowest rmnet_dataN on rmnet_ipa0 with its mux id.
pub fn data_link() -> Result<(String, u8)> {
    let all = links()?;
    pick_data_link(&all).ok_or_else(|| anyhow::anyhow!("no rmnet_data link on {PARENT}"))
}

fn data_index(name: &str) -> Option<u32> {
    name.strip_prefix("rmnet_data")?.parse().ok()
}

pub fn pick_data_link(all: &[Link]) -> Option<(String, u8)> {
    let parent = all.iter().find(|l| l.name == PARENT).map(|l| l.index);
    let mut cands: Vec<&Link> = all
        .iter()
        .filter(|l| data_index(&l.name).is_some())
        .filter(|l| parent.is_none() || l.parent.is_none() || l.parent == parent)
        .collect();
    cands.sort_by_key(|l| data_index(&l.name));
    let l = cands.first()?;
    // Netlink's answer when there is one; else libsec-ril's naming (rmnet_dataN = mux N+1).
    let mux = match l.mux_id {
        Some(m) if (1..=254).contains(&m) => m as u8,
        _ => (data_index(&l.name)? + 1).min(254) as u8,
    };
    Some((l.name.clone(), mux))
}

/// (rx, tx) bytes of `ifname` (sysfs; 0 when unreadable).
pub fn counters(ifname: &str) -> (u64, u64) {
    let rd = |f: &str| {
        std::fs::read_to_string(format!("/sys/class/net/{ifname}/statistics/{f}"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    (rd("rx_bytes"), rd("tx_bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nla(ty: u16, v: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&((v.len() + 4) as u16).to_ne_bytes());
        b.extend_from_slice(&ty.to_ne_bytes());
        b.extend_from_slice(v);
        while b.len() % 4 != 0 {
            b.push(0);
        }
        b
    }

    fn link_msg(index: i32, name: &str, parent: Option<i32>, mux: Option<u16>) -> Vec<u8> {
        let mut p = vec![0u8; 16];
        p[4..8].copy_from_slice(&index.to_ne_bytes());
        let mut n = name.as_bytes().to_vec();
        n.push(0);
        p.extend(nla(IFLA_IFNAME, &n));
        if let Some(pi) = parent {
            p.extend(nla(IFLA_LINK, &pi.to_ne_bytes()));
        }
        let mut info = nla(IFLA_INFO_KIND, b"rmnet\0");
        if let Some(m) = mux {
            info.extend(nla(IFLA_INFO_DATA | 0x8000, &nla(IFLA_RMNET_MUX_ID, &m.to_ne_bytes())));
        }
        p.extend(nla(IFLA_LINKINFO | 0x8000, &info));
        p
    }

    #[test]
    fn parses_rmnet_links() {
        let l = parse_link(&link_msg(14, "rmnet_data0", Some(12), Some(1))).unwrap();
        assert_eq!(l.name, "rmnet_data0");
        assert_eq!(l.parent, Some(12));
        assert_eq!(l.kind.as_deref(), Some("rmnet"));
        assert_eq!(l.mux_id, Some(1));
    }

    #[test]
    fn picks_lowest_data_link() {
        let all = vec![
            parse_link(&link_msg(12, "rmnet_ipa0", None, None)).unwrap(),
            parse_link(&link_msg(16, "rmnet_data10", Some(12), Some(11))).unwrap(),
            parse_link(&link_msg(15, "rmnet_data1", Some(12), Some(2))).unwrap(),
            parse_link(&link_msg(14, "rmnet_data0", Some(12), Some(7))).unwrap(),
            parse_link(&link_msg(3, "wlan0", None, None)).unwrap(),
        ];
        assert_eq!(pick_data_link(&all), Some(("rmnet_data0".into(), 7)));
        // No mux id from netlink: rmnet_dataN = mux N+1.
        let all = vec![parse_link(&link_msg(15, "rmnet_data2", None, None)).unwrap()];
        assert_eq!(pick_data_link(&all), Some(("rmnet_data2".into(), 3)));
        assert_eq!(pick_data_link(&[]), None);
    }
}
