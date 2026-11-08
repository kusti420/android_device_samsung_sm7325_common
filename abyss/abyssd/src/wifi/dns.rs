//! abyssd's own DNS stub resolver (A records, UDP, IPv4 only) with a TTL cache.
//!
//! Why not bionic getaddrinfo: it asks netd's DnsResolver (dnsproxyd), which only answers for a
//! network that has a resolver configuration (IDnsResolver.setResolverConfiguration). abyssd
//! does not set one yet (that parcel is large and version-dependent; see reports/impl-wifi.md),
//! so abyssd resolves its own names against the DHCP-provided servers. Scripts (later) need the
//! resolver configuration; this module covers abyssd's own fetches (the Wi-Fi test, radio API).

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::os::fd::AsRawFd;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use log::info;

use crate::clock;

const TYPE_A: u16 = 1;
const TYPE_CNAME: u16 = 5;
const CLASS_IN: u16 = 1;
/// Cache entries live at least this long (a TTL of 0 would make every lookup go out).
const MIN_TTL_S: u64 = 30;
const MAX_TTL_S: u64 = 6 * 3600;

pub fn query(id: u16, name: &str) -> Result<Vec<u8>> {
    let mut q = Vec::with_capacity(32 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00]); // RD
    q.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // 1 question
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            bail!("bad name {name:?}");
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&TYPE_A.to_be_bytes());
    q.extend_from_slice(&CLASS_IN.to_be_bytes());
    Ok(q)
}

/// Skips a (possibly compressed) name, returns the offset after it.
fn skip_name(b: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let len = *b.get(i)? as usize;
        if len == 0 {
            return Some(i + 1);
        }
        if len & 0xc0 == 0xc0 {
            return Some(i + 2);
        }
        i += 1 + len;
    }
}

/// A records and the smallest TTL from a response to `id`.
pub fn parse(b: &[u8], id: u16) -> Result<(Vec<Ipv4Addr>, u64)> {
    if b.len() < 12 || u16::from_be_bytes([b[0], b[1]]) != id {
        bail!("not our answer");
    }
    if b[2] & 0x80 == 0 {
        bail!("not a response");
    }
    let rcode = b[3] & 0x0f;
    if rcode != 0 {
        bail!("rcode {rcode}");
    }
    let qd = u16::from_be_bytes([b[4], b[5]]);
    let an = u16::from_be_bytes([b[6], b[7]]);
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(b, i).ok_or_else(|| anyhow!("short question"))? + 4;
    }
    let mut out = Vec::new();
    let mut ttl = u64::MAX;
    for _ in 0..an {
        i = skip_name(b, i).ok_or_else(|| anyhow!("short answer"))?;
        let h = b.get(i..i + 10).ok_or_else(|| anyhow!("short answer"))?;
        let t = u16::from_be_bytes([h[0], h[1]]);
        let class = u16::from_be_bytes([h[2], h[3]]);
        let rttl = u32::from_be_bytes([h[4], h[5], h[6], h[7]]) as u64;
        let rlen = u16::from_be_bytes([h[8], h[9]]) as usize;
        let data = b.get(i + 10..i + 10 + rlen).ok_or_else(|| anyhow!("short rdata"))?;
        if t == TYPE_A && class == CLASS_IN && rlen == 4 {
            out.push(Ipv4Addr::new(data[0], data[1], data[2], data[3]));
            ttl = ttl.min(rttl);
        } else if t == TYPE_CNAME {
            ttl = ttl.min(rttl);
        }
        i += 10 + rlen;
    }
    if out.is_empty() {
        bail!("no A record");
    }
    Ok((out, ttl))
}

/// Sets SO_BINDTODEVICE (CAP_NET_RAW): the socket uses wlan0 whatever the routing says.
pub fn bind_to_device(fd: &impl AsRawFd, ifname: &str) -> Result<()> {
    let name = ifname.as_bytes();
    // SAFETY: `name` is valid for `name.len()` bytes during the call.
    let r = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            name.as_ptr() as *const libc::c_void,
            name.len() as libc::socklen_t,
        )
    };
    if r < 0 {
        bail!("SO_BINDTODEVICE {ifname}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

pub struct Resolver {
    cache: HashMap<String, (Vec<Ipv4Addr>, u64)>,
    id: u16,
}

impl Resolver {
    pub fn new() -> Resolver {
        Resolver { cache: HashMap::new(), id: (clock::now_ms() as u16) | 1 }
    }

    pub fn clear(&mut self) {
        self.cache.clear();
    }

    /// Resolves `name` through `servers` (tried in order, 2 tries each, 1.5 s per try).
    pub fn resolve(&mut self, name: &str, servers: &[Ipv4Addr], ifname: &str) -> Result<Vec<Ipv4Addr>> {
        if let Ok(ip) = name.parse::<Ipv4Addr>() {
            return Ok(vec![ip]);
        }
        let key = name.to_ascii_lowercase();
        let now = clock::now_ms();
        if let Some((ips, until)) = self.cache.get(&key) {
            if now < *until {
                return Ok(ips.clone());
            }
        }
        if servers.is_empty() {
            bail!("no DNS server from DHCP");
        }
        let sock = UdpSocket::bind("0.0.0.0:0").context("dns socket")?;
        bind_to_device(&sock, ifname)?;
        sock.set_read_timeout(Some(Duration::from_millis(1_500)))?;
        let mut last = anyhow!("no answer");
        let mut buf = [0u8; 1500];
        for &server in servers.iter().take(3) {
            for _ in 0..2 {
                self.id = self.id.wrapping_add(0x3b1d);
                let id = self.id;
                let q = query(id, &key)?;
                if let Err(e) = sock.send_to(&q, SocketAddrV4::new(server, 53)) {
                    last = anyhow!("send to {server}: {e}");
                    break;
                }
                let deadline = clock::now_ms() + 1_500;
                loop {
                    match sock.recv_from(&mut buf) {
                        Ok((n, from)) if from.ip() == server => match parse(&buf[..n], id) {
                            Ok((ips, ttl)) => {
                                let ttl_ms = ttl.clamp(MIN_TTL_S, MAX_TTL_S) * 1000;
                                info!("dns: {key} -> {ips:?} via {server} (ttl {ttl} s)");
                                self.cache.insert(key, (ips.clone(), clock::now_ms() + ttl_ms));
                                return Ok(ips);
                            }
                            Err(e) if e.to_string() == "not our answer" => {}
                            Err(e) => {
                                last = anyhow!("{server}: {e}");
                                break;
                            }
                        },
                        Ok(_) => {}
                        Err(e) => {
                            last = anyhow!("{server}: {e}");
                            break;
                        }
                    }
                    if clock::now_ms() >= deadline {
                        break;
                    }
                }
            }
        }
        Err(last.context(format!("resolve {key}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let q = query(0x1234, "connectivitycheck.gstatic.com").unwrap();
        // Build an answer: the question + one A record with a compressed name pointer.
        let mut a = q.clone();
        a[2] = 0x81;
        a[3] = 0x80;
        a[7] = 1; // ancount
        a.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 142, 250, 74, 99]);
        let (ips, ttl) = parse(&a, 0x1234).unwrap();
        assert_eq!(ips, vec![Ipv4Addr::new(142, 250, 74, 99)]);
        assert_eq!(ttl, 60);
        assert!(parse(&a, 0x1235).is_err());
    }
}
