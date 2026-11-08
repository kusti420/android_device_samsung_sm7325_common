//! Minimal DHCPv4 client (RFC 2131) for Abyss Wi-Fi leases.
//!
//! Runs over an AF_PACKET (SOCK_DGRAM, ETH_P_IP) socket bound to wlan0, so it works before the
//! interface has an address and does not depend on netd's policy routing, rp_filter or a UDP
//! socket on port 68. abyssd builds the IPv4 + UDP headers itself and sends every message to the
//! Ethernet broadcast address; replies are read straight off the link (any frame to our MAC or
//! to broadcast), filtered by UDP port 68, transaction id and client hardware address.
//!
//! Fast path (battery-nerd §4.3): INIT-REBOOT with the cached address (one REQUEST/ACK), Rapid
//! Commit (RFC 4039) offered on DISCOVER, no RFC 5227 ARP probe. Renewal: REQUEST with ciaddr
//! (sent as broadcast, which every server answers; unicast RENEWING would need the server's MAC).
//!
//! The client identifier is 0x01 + MAC and the vendor class is Android's
//! ("android-dhcp-<release>", from the hand-over), the same as NetworkStack's DhcpClient, so the
//! router sees the same client it gave Android's lease to and INIT-REBOOT keeps the address.

use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, bail, Result};
use log::{info, warn};

use crate::clock;

const OP_REQUEST: u8 = 1;
const OP_REPLY: u8 = 2;
const MAGIC: [u8; 4] = [99, 130, 83, 99];
const CLIENT_PORT: u16 = 68;
const SERVER_PORT: u16 = 67;
const ETH_P_IP: u16 = 0x0800;
/// linux/if_packet.h PACKET_OUTGOING (not exported by the libc crate for Android).
const PACKET_OUTGOING: u8 = 4;
/// BOOTP fixed header (op .. file) before the magic cookie.
const BOOTP_LEN: usize = 236;

// Option codes.
const OPT_PAD: u8 = 0;
const OPT_SUBNET: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_HOSTNAME: u8 = 12;
const OPT_MTU: u8 = 26;
const OPT_REQUESTED_IP: u8 = 50;
const OPT_LEASE_TIME: u8 = 51;
const OPT_MSG_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_PARAM_LIST: u8 = 55;
const OPT_MAX_SIZE: u8 = 57;
const OPT_T1: u8 = 58;
const OPT_T2: u8 = 59;
const OPT_VENDOR_CLASS: u8 = 60;
const OPT_CLIENT_ID: u8 = 61;
const OPT_RAPID_COMMIT: u8 = 80;
const OPT_END: u8 = 255;

/// INIT-REBOOT: wait this long for the ACK/NAK before falling back to DISCOVER.
const REBOOT_WAIT_MS: u64 = 1_500;
/// DISCOVER/REQUEST retransmissions (RFC 2131 §4.1 suggests 4 s doubling; a phone on a home
/// router answers within milliseconds, so start shorter).
const RETRY_MS: [u64; 4] = [800, 1_600, 3_200, 4_000];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgType {
    Discover = 1,
    Offer = 2,
    Request = 3,
    Ack = 5,
    Nak = 6,
}

impl MsgType {
    fn from_u8(v: u8) -> Option<MsgType> {
        Some(match v {
            1 => MsgType::Discover,
            2 => MsgType::Offer,
            3 => MsgType::Request,
            5 => MsgType::Ack,
            6 => MsgType::Nak,
            _ => return None,
        })
    }
}

/// A bound address. Times are seconds as the server gave them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub ip: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub server: Option<Ipv4Addr>,
    pub lease_s: u32,
    pub t1_s: u32,
    pub t2_s: u32,
    pub mtu: Option<u16>,
}

impl Lease {
    /// The connected subnet, "a.b.c.0/24".
    pub fn subnet(&self) -> String {
        let mask = prefix_mask(self.prefix);
        let net = u32::from(self.ip) & mask;
        format!("{}/{}", Ipv4Addr::from(net), self.prefix)
    }
}

pub fn prefix_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix.min(32) as u32)
    }
}

fn mask_prefix(mask: Ipv4Addr) -> u8 {
    u32::from(mask).leading_ones() as u8
}

/// What goes into one client message.
pub struct Params<'a> {
    pub mac: [u8; 6],
    pub xid: u32,
    pub secs: u16,
    pub ciaddr: Ipv4Addr,
    pub requested: Option<Ipv4Addr>,
    pub server_id: Option<Ipv4Addr>,
    pub vendor_class: Option<&'a str>,
    pub hostname: Option<&'a str>,
    pub rapid_commit: bool,
}

/// A parsed server message.
#[derive(Clone, Debug)]
pub struct Reply {
    pub xid: u32,
    pub yiaddr: Ipv4Addr,
    pub chaddr: [u8; 6],
    pub msg_type: Option<u8>,
    pub server_id: Option<Ipv4Addr>,
    pub subnet: Option<Ipv4Addr>,
    pub router: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub lease_s: Option<u32>,
    pub t1_s: Option<u32>,
    pub t2_s: Option<u32>,
    pub mtu: Option<u16>,
    pub rapid_commit: bool,
}

impl Reply {
    pub fn kind(&self) -> Option<MsgType> {
        self.msg_type.and_then(MsgType::from_u8)
    }

    /// The lease an ACK grants. Defaults: /24 when the server sends no mask, T1 = 50 %,
    /// T2 = 87.5 % of the lease (RFC 2131 §4.4.5), 1 h when no lease time is given.
    pub fn lease(&self) -> Lease {
        let lease_s = self.lease_s.unwrap_or(3600);
        Lease {
            ip: self.yiaddr,
            prefix: self.subnet.map(mask_prefix).unwrap_or(24),
            gateway: self.router,
            dns: self.dns.clone(),
            server: self.server_id,
            lease_s,
            t1_s: self.t1_s.unwrap_or(lease_s / 2),
            t2_s: self.t2_s.unwrap_or((lease_s as u64 * 7 / 8) as u32),
            mtu: self.mtu,
        }
    }
}

fn put_opt(b: &mut Vec<u8>, code: u8, data: &[u8]) {
    b.push(code);
    b.push(data.len().min(255) as u8);
    b.extend_from_slice(&data[..data.len().min(255)]);
}

/// The BOOTP/DHCP payload (what goes inside UDP).
pub fn build(t: MsgType, p: &Params) -> Vec<u8> {
    let mut b = vec![0u8; BOOTP_LEN];
    b[0] = OP_REQUEST;
    b[1] = 1; // htype Ethernet
    b[2] = 6; // hlen
    b[4..8].copy_from_slice(&p.xid.to_be_bytes());
    b[8..10].copy_from_slice(&p.secs.to_be_bytes());
    // Broadcast flag: ask for broadcast replies (we may have no address yet).
    if p.ciaddr.is_unspecified() {
        b[10] = 0x80;
    }
    b[12..16].copy_from_slice(&p.ciaddr.octets());
    b[28..34].copy_from_slice(&p.mac);
    b.extend_from_slice(&MAGIC);
    put_opt(&mut b, OPT_MSG_TYPE, &[t as u8]);
    let mut cid = vec![1u8];
    cid.extend_from_slice(&p.mac);
    put_opt(&mut b, OPT_CLIENT_ID, &cid);
    if let Some(ip) = p.requested {
        put_opt(&mut b, OPT_REQUESTED_IP, &ip.octets());
    }
    if let Some(s) = p.server_id {
        put_opt(&mut b, OPT_SERVER_ID, &s.octets());
    }
    put_opt(&mut b, OPT_MAX_SIZE, &1500u16.to_be_bytes());
    if let Some(v) = p.vendor_class.filter(|v| !v.is_empty()) {
        put_opt(&mut b, OPT_VENDOR_CLASS, v.as_bytes());
    }
    if let Some(h) = p.hostname.filter(|h| !h.is_empty()) {
        put_opt(&mut b, OPT_HOSTNAME, h.as_bytes());
    }
    if p.rapid_commit {
        put_opt(&mut b, OPT_RAPID_COMMIT, &[]);
    }
    put_opt(
        &mut b,
        OPT_PARAM_LIST,
        &[OPT_SUBNET, OPT_ROUTER, OPT_DNS, OPT_MTU, OPT_LEASE_TIME, OPT_T1, OPT_T2, OPT_RAPID_COMMIT],
    );
    b.push(OPT_END);
    // Pad to the classic 300-byte BOOTP minimum; some relays drop shorter messages.
    while b.len() < 300 {
        b.push(OPT_PAD);
    }
    b
}

fn ip4(d: &[u8]) -> Option<Ipv4Addr> {
    (d.len() >= 4).then(|| Ipv4Addr::new(d[0], d[1], d[2], d[3]))
}

fn be32(d: &[u8]) -> Option<u32> {
    (d.len() >= 4).then(|| u32::from_be_bytes([d[0], d[1], d[2], d[3]]))
}

/// Parses a BOOTP reply payload; None if it is not a DHCP reply.
pub fn parse(b: &[u8]) -> Option<Reply> {
    if b.len() < BOOTP_LEN + 4 || b[0] != OP_REPLY || b[BOOTP_LEN..BOOTP_LEN + 4] != MAGIC {
        return None;
    }
    let mut r = Reply {
        xid: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        yiaddr: Ipv4Addr::new(b[16], b[17], b[18], b[19]),
        chaddr: [b[28], b[29], b[30], b[31], b[32], b[33]],
        msg_type: None,
        server_id: None,
        subnet: None,
        router: None,
        dns: Vec::new(),
        lease_s: None,
        t1_s: None,
        t2_s: None,
        mtu: None,
        rapid_commit: false,
    };
    let mut i = BOOTP_LEN + 4;
    while i < b.len() {
        let code = b[i];
        if code == OPT_PAD {
            i += 1;
            continue;
        }
        if code == OPT_END || i + 1 >= b.len() {
            break;
        }
        let len = b[i + 1] as usize;
        let Some(d) = b.get(i + 2..i + 2 + len) else { break };
        match code {
            OPT_MSG_TYPE if len >= 1 => r.msg_type = Some(d[0]),
            OPT_SERVER_ID => r.server_id = ip4(d),
            OPT_SUBNET => r.subnet = ip4(d),
            // First router only.
            OPT_ROUTER => r.router = ip4(d),
            OPT_DNS => r.dns = d.chunks_exact(4).filter_map(ip4).collect(),
            OPT_LEASE_TIME => r.lease_s = be32(d),
            OPT_T1 => r.t1_s = be32(d),
            OPT_T2 => r.t2_s = be32(d),
            OPT_MTU if len >= 2 => r.mtu = Some(u16::from_be_bytes([d[0], d[1]])),
            OPT_RAPID_COMMIT => r.rapid_commit = true,
            _ => {}
        }
        i += 2 + len;
    }
    Some(r)
}

/// RFC 1071 one's-complement sum.
fn csum(data: &[u8], mut sum: u32) -> u16 {
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    if let [last] = chunks.remainder() {
        sum += (*last as u32) << 8;
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// IPv4 + UDP headers around a DHCP payload (client port 68 → server port 67).
pub fn wrap(src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
    let udp_len = 8 + payload.len();
    let total = 20 + udp_len;
    let mut p = Vec::with_capacity(total);
    p.extend_from_slice(&[0x45, 0x10]); // v4, IHL 5; TOS low-delay like Android's client
    p.extend_from_slice(&(total as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0, 0x40, 0]); // id 0, DF
    p.extend_from_slice(&[64, 17, 0, 0]); // TTL 64, UDP, checksum later
    p.extend_from_slice(&src.octets());
    p.extend_from_slice(&dst.octets());
    let ipsum = csum(&p[..20], 0);
    p[10..12].copy_from_slice(&ipsum.to_be_bytes());
    p.extend_from_slice(&CLIENT_PORT.to_be_bytes());
    p.extend_from_slice(&SERVER_PORT.to_be_bytes());
    p.extend_from_slice(&(udp_len as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(payload);
    // UDP checksum over the pseudo header + UDP header + payload.
    let mut pseudo = 0u32;
    for w in src.octets().chunks(2).chain(dst.octets().chunks(2)) {
        pseudo += u16::from_be_bytes([w[0], w[1]]) as u32;
    }
    pseudo += 17 + udp_len as u32;
    let mut s = csum(&p[20..], pseudo);
    if s == 0 {
        s = 0xffff;
    }
    p[26..28].copy_from_slice(&s.to_be_bytes());
    p
}

/// The UDP payload of an IPv4 frame to port 68, if it is one.
pub fn unwrap(frame: &[u8]) -> Option<&[u8]> {
    if frame.len() < 28 || frame[0] >> 4 != 4 || frame[9] != 17 {
        return None;
    }
    let ihl = (frame[0] & 0x0f) as usize * 4;
    let total = u16::from_be_bytes([frame[2], frame[3]]) as usize;
    if ihl < 20 || total > frame.len() || total < ihl + 8 {
        return None;
    }
    // Fragments are not reassembled (DHCP replies fit one frame).
    if u16::from_be_bytes([frame[6], frame[7]]) & 0x3fff != 0 {
        return None;
    }
    let udp = &frame[ihl..total];
    if u16::from_be_bytes([udp[2], udp[3]]) != CLIENT_PORT {
        return None;
    }
    let ulen = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    if ulen < 8 || ulen > udp.len() {
        return None;
    }
    Some(&udp[8..ulen])
}

/// How the address was obtained (for the log and the test result).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    InitReboot,
    RapidCommit,
    Discover,
    /// Android's static address for the network (no DHCP).
    Static,
}

impl How {
    pub fn as_str(self) -> &'static str {
        match self {
            How::InitReboot => "init-reboot",
            How::RapidCommit => "rapid-commit",
            How::Discover => "discover",
            How::Static => "static",
        }
    }
}

pub struct Client {
    fd: OwnedFd,
    ifindex: i32,
    mac: [u8; 6],
    vendor_class: Option<String>,
    xid_seed: u32,
}

impl Client {
    /// AF_PACKET socket on `ifname` (needs CAP_NET_RAW; SELinux packet_socket in abyssd.te).
    pub fn open(ifname: &str, mac: [u8; 6], vendor_class: Option<String>) -> Result<Client> {
        let cname = std::ffi::CString::new(ifname)?;
        // SAFETY: valid NUL-terminated string for the duration of the call.
        let ifindex = unsafe { libc::if_nametoindex(cname.as_ptr()) } as i32;
        if ifindex == 0 {
            bail!("no interface {ifname}: {}", std::io::Error::last_os_error());
        }
        let proto = (ETH_P_IP).to_be() as libc::c_int;
        // SAFETY: plain socket(2); the fd is owned right below.
        let raw = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, proto) };
        if raw < 0 {
            bail!("packet socket: {}", std::io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh fd owned by nobody else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: an all-zero sockaddr_ll is a valid value; the fields we need are set below.
        let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        sll.sll_family = libc::AF_PACKET as u16;
        sll.sll_protocol = ETH_P_IP.to_be();
        sll.sll_ifindex = ifindex;
        // SAFETY: `sll` is a valid sockaddr_ll of the size given.
        let r = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &sll as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if r < 0 {
            bail!("packet bind {ifname}: {}", std::io::Error::last_os_error());
        }
        let xid_seed = (clock::now_ms() as u32) ^ u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]);
        Ok(Client { fd, ifindex, mac, vendor_class, xid_seed })
    }

    fn next_xid(&mut self) -> u32 {
        // xorshift32: unpredictable enough for a transaction id.
        let mut x = self.xid_seed.max(1);
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.xid_seed = x;
        x
    }

    fn params(&self, xid: u32, secs: u16) -> Params<'_> {
        Params {
            mac: self.mac,
            xid,
            secs,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            requested: None,
            server_id: None,
            vendor_class: self.vendor_class.as_deref(),
            hostname: None,
            rapid_commit: false,
        }
    }

    fn send(&self, payload: &[u8], src: Ipv4Addr) -> Result<()> {
        let frame = wrap(src, Ipv4Addr::BROADCAST, payload);
        // SAFETY: an all-zero sockaddr_ll is a valid value; the fields we need are set below.
        let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        sll.sll_family = libc::AF_PACKET as u16;
        sll.sll_protocol = ETH_P_IP.to_be();
        sll.sll_ifindex = self.ifindex;
        sll.sll_halen = 6;
        sll.sll_addr[..6].copy_from_slice(&[0xff; 6]);
        // SAFETY: `frame` and `sll` are valid for the call; sizes match the buffers.
        let r = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                frame.as_ptr() as *const libc::c_void,
                frame.len(),
                0,
                &sll as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if r < 0 {
            bail!("dhcp send: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Waits until `deadline` (boot ms) for a reply to `xid`; None on timeout.
    fn recv(&self, xid: u32, deadline: u64, abort: &AtomicBool) -> Result<Option<Reply>> {
        let mut buf = [0u8; 1600];
        loop {
            if abort.load(Ordering::Relaxed) {
                bail!("aborted");
            }
            let now = clock::now_ms();
            if now >= deadline {
                return Ok(None);
            }
            // Wake at least every 200 ms to notice an abort.
            let wait = (deadline - now).min(200) as libc::c_int;
            let mut pfd = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            let n = unsafe { libc::poll(&mut pfd, 1, wait) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                bail!("dhcp poll: {e}");
            }
            if n == 0 {
                continue;
            }
            // SAFETY: an all-zero sockaddr_ll is a valid value for recvfrom to fill.
            let mut from: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
            let mut flen = std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
            // SAFETY: `buf` and `from` are valid writable buffers of the sizes given.
            let len = unsafe {
                libc::recvfrom(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    libc::MSG_DONTWAIT,
                    &mut from as *mut _ as *mut libc::sockaddr,
                    &mut flen,
                )
            };
            if len < 0 {
                let e = std::io::Error::last_os_error();
                if matches!(e.raw_os_error(), Some(libc::EAGAIN) | Some(libc::EINTR)) {
                    continue;
                }
                bail!("dhcp recv: {e}");
            }
            // Our own transmissions come back on a packet socket too.
            if from.sll_pkttype == PACKET_OUTGOING {
                continue;
            }
            let Some(payload) = unwrap(&buf[..len as usize]) else { continue };
            let Some(r) = parse(payload) else { continue };
            if r.xid == xid && r.chaddr == self.mac {
                return Ok(Some(r));
            }
        }
    }

    /// Gets an address: INIT-REBOOT with `cached` first (if given), then DISCOVER with Rapid
    /// Commit. Returns the lease and how it was obtained.
    pub fn obtain(&mut self, cached: Option<Ipv4Addr>, abort: &AtomicBool) -> Result<(Lease, How)> {
        let start = clock::now_ms();
        if let Some(ip) = cached {
            let xid = self.next_xid();
            let mut p = self.params(xid, 0);
            p.requested = Some(ip);
            let msg = build(MsgType::Request, &p);
            info!("dhcp: INIT-REBOOT request {ip}");
            // Two tries inside the window (the first frame after association is sometimes lost).
            'reboot: for wait in [REBOOT_WAIT_MS / 2, REBOOT_WAIT_MS / 2] {
                self.send(&msg, Ipv4Addr::UNSPECIFIED)?;
                let deadline = clock::now_ms() + wait;
                while let Some(r) = self.recv(xid, deadline, abort)? {
                    match self.reboot_result(&r, ip) {
                        Some(Ok(l)) => return Ok((l, How::InitReboot)),
                        Some(Err(e)) => {
                            info!("dhcp: {e}; starting over with DISCOVER");
                            break 'reboot;
                        }
                        None => {}
                    }
                }
            }
            info!("dhcp: INIT-REBOOT got no ACK within {REBOOT_WAIT_MS} ms; DISCOVER");
        }
        self.discover(start, abort)
    }

    /// Some(Ok) = ACK for `ip`, Some(Err) = NAK, None = ignore this message.
    fn reboot_result(&self, r: &Reply, ip: Ipv4Addr) -> Option<Result<Lease>> {
        match r.kind() {
            Some(MsgType::Ack) if r.yiaddr == ip => Some(Ok(r.lease())),
            Some(MsgType::Ack) => Some(Err(anyhow!("ACK for {} instead of {ip}", r.yiaddr))),
            Some(MsgType::Nak) => Some(Err(anyhow!("NAK for {ip}"))),
            _ => None,
        }
    }

    fn discover(&mut self, start: u64, abort: &AtomicBool) -> Result<(Lease, How)> {
        let xid = self.next_xid();
        let mut offer: Option<Reply> = None;
        for wait in RETRY_MS {
            let secs = ((clock::now_ms() - start) / 1000).min(u16::MAX as u64) as u16;
            let mut p = self.params(xid, secs);
            p.rapid_commit = true;
            self.send(&build(MsgType::Discover, &p), Ipv4Addr::UNSPECIFIED)?;
            info!("dhcp: DISCOVER (xid {xid:08x})");
            let deadline = clock::now_ms() + wait;
            while let Some(r) = self.recv(xid, deadline, abort)? {
                match r.kind() {
                    // Rapid Commit: the server answered DISCOVER with an ACK straight away.
                    Some(MsgType::Ack) if r.rapid_commit && !r.yiaddr.is_unspecified() => {
                        return Ok((r.lease(), How::RapidCommit));
                    }
                    Some(MsgType::Offer) if !r.yiaddr.is_unspecified() => {
                        offer = Some(r);
                        break;
                    }
                    _ => {}
                }
            }
            if offer.is_some() {
                break;
            }
        }
        let Some(o) = offer else { bail!("no DHCP offer") };
        info!("dhcp: OFFER {} from {:?}", o.yiaddr, o.server_id);
        for wait in RETRY_MS {
            let secs = ((clock::now_ms() - start) / 1000).min(u16::MAX as u64) as u16;
            let mut p = self.params(xid, secs);
            p.requested = Some(o.yiaddr);
            p.server_id = o.server_id;
            self.send(&build(MsgType::Request, &p), Ipv4Addr::UNSPECIFIED)?;
            let deadline = clock::now_ms() + wait;
            while let Some(r) = self.recv(xid, deadline, abort)? {
                match r.kind() {
                    Some(MsgType::Ack) if r.yiaddr == o.yiaddr => return Ok((r.lease(), How::Discover)),
                    Some(MsgType::Nak) => bail!("NAK for offered {}", o.yiaddr),
                    _ => {}
                }
            }
        }
        bail!("no DHCP ACK for {}", o.yiaddr)
    }

    /// RENEWING/REBINDING: REQUEST with ciaddr. Ok(None) = NAK (address lost).
    pub fn renew(&mut self, lease: &Lease, abort: &AtomicBool) -> Result<Option<Lease>> {
        let xid = self.next_xid();
        let mut p = self.params(xid, 0);
        p.ciaddr = lease.ip;
        let msg = build(MsgType::Request, &p);
        for wait in [1_000u64, 2_000] {
            self.send(&msg, lease.ip)?;
            let deadline = clock::now_ms() + wait;
            while let Some(r) = self.recv(xid, deadline, abort)? {
                match r.kind() {
                    Some(MsgType::Ack) if r.yiaddr == lease.ip => return Ok(Some(r.lease())),
                    Some(MsgType::Ack) => {
                        warn!("dhcp: renew ACK for {} instead of {}", r.yiaddr, lease.ip);
                        return Ok(None);
                    }
                    Some(MsgType::Nak) => return Ok(None),
                    _ => {}
                }
            }
        }
        bail!("no answer to renew")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply_bytes(t: MsgType, xid: u32, mac: [u8; 6], ip: Ipv4Addr) -> Vec<u8> {
        let mut b = vec![0u8; BOOTP_LEN];
        b[0] = OP_REPLY;
        b[4..8].copy_from_slice(&xid.to_be_bytes());
        b[16..20].copy_from_slice(&ip.octets());
        b[28..34].copy_from_slice(&mac);
        b.extend_from_slice(&MAGIC);
        put_opt(&mut b, OPT_MSG_TYPE, &[t as u8]);
        put_opt(&mut b, OPT_SERVER_ID, &[192, 0, 2, 1]);
        put_opt(&mut b, OPT_SUBNET, &[255, 255, 255, 0]);
        put_opt(&mut b, OPT_ROUTER, &[192, 0, 2, 1]);
        put_opt(&mut b, OPT_DNS, &[192, 0, 2, 1, 9, 9, 9, 9]);
        put_opt(&mut b, OPT_LEASE_TIME, &86400u32.to_be_bytes());
        b.push(OPT_END);
        b
    }

    #[test]
    fn build_parse_roundtrip() {
        let mac = [2, 0, 0, 0, 0, 1];
        let ip = Ipv4Addr::new(192, 0, 2, 23);
        let r = parse(&reply_bytes(MsgType::Ack, 7, mac, ip)).unwrap();
        assert_eq!(r.kind(), Some(MsgType::Ack));
        let l = r.lease();
        assert_eq!(l.ip, ip);
        assert_eq!(l.prefix, 24);
        assert_eq!(l.dns.len(), 2);
        assert_eq!(l.t1_s, 43200);
        assert_eq!(l.subnet(), "192.0.2.0/24");
        let p = Params {
            mac,
            xid: 7,
            secs: 0,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            requested: Some(ip),
            server_id: None,
            vendor_class: Some("android-dhcp-16"),
            hostname: None,
            rapid_commit: true,
        };
        let m = build(MsgType::Request, &p);
        assert!(m.len() >= 300);
        assert_eq!(&m[BOOTP_LEN..BOOTP_LEN + 4], &MAGIC);
    }

    #[test]
    fn wrap_unwrap() {
        let payload = [1u8, 2, 3, 4, 5];
        let f = wrap(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, &payload);
        // IP header checksum verifies to zero.
        assert_eq!(csum(&f[..20], 0), 0);
        // Rewrite the destination port to 68 to look like a reply and unwrap it.
        let mut g = f.clone();
        g[22..24].copy_from_slice(&CLIENT_PORT.to_be_bytes());
        assert_eq!(unwrap(&g), Some(&payload[..]));
        assert_eq!(unwrap(&f), None);
    }
}
