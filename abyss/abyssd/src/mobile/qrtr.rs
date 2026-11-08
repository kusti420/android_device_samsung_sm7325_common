//! AF_QIPCRTR sockets: the QRTR name service lookup and one QMI client per socket.
//!
//! Kernel side (kernel/samsung/sm7325/net/qrtr): `socket(AF_QIPCRTR, SOCK_DGRAM, 0)` needs no
//! permission; the first sendto binds an ephemeral port (no check either). The name service is
//! in the kernel (ns.c): a NEW_LOOKUP control packet sent to {local node, QRTR_PORT_CTRL} is
//! answered with one NEW_SERVER packet per matching service and an all-zero one at the end.
//! Closing the socket ends the client: the modem drops its calls and indication registrations
//! (QRTR DEL_CLIENT), which is what makes a crash safe.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use anyhow::{anyhow, bail, Context, Result};
use log::debug;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

use super::qmi::{Msg, Req, TYPE_INDICATION, TYPE_RESPONSE};
use crate::clock;

pub const AF_QIPCRTR: i32 = 42;
const QRTR_PORT_CTRL: u32 = 0xFFFF_FFFE;
const QRTR_TYPE_NEW_SERVER: u32 = 4;
const QRTR_TYPE_NEW_LOOKUP: u32 = 10;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrQrtr {
    sq_family: libc::sa_family_t,
    sq_node: u32,
    sq_port: u32,
}

/// One service instance as the name service lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Server {
    pub service: u32,
    /// QMI instance field: version | (instance << 8).
    pub instance: u32,
    pub node: u32,
    pub port: u32,
}

fn socket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket(2); the fd is owned right below.
    let raw = unsafe { libc::socket(AF_QIPCRTR, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh fd nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn local_addr(fd: &OwnedFd) -> io::Result<SockaddrQrtr> {
    let mut a = SockaddrQrtr { sq_family: 0, sq_node: 0, sq_port: 0 };
    let mut len = std::mem::size_of::<SockaddrQrtr>() as libc::socklen_t;
    // SAFETY: `a` is a valid, writable sockaddr_qrtr of `len` bytes.
    let r = unsafe { libc::getsockname(fd.as_raw_fd(), &mut a as *mut _ as *mut libc::sockaddr, &mut len) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(a)
}

fn send_to(fd: &OwnedFd, node: u32, port: u32, buf: &[u8]) -> io::Result<()> {
    let a = SockaddrQrtr { sq_family: AF_QIPCRTR as libc::sa_family_t, sq_node: node, sq_port: port };
    // SAFETY: `buf` and `a` are valid for the given lengths for the duration of the call.
    let r = unsafe {
        libc::sendto(
            fd.as_raw_fd(),
            buf.as_ptr() as *const libc::c_void,
            buf.len(),
            0,
            &a as *const _ as *const libc::sockaddr,
            std::mem::size_of::<SockaddrQrtr>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Receives one datagram; returns (bytes, source node, source port), or None on timeout.
fn recv_from(fd: &OwnedFd, buf: &mut [u8], timeout_ms: u64) -> io::Result<Option<(usize, u32, u32)>> {
    let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
    let t = PollTimeout::try_from(timeout_ms).unwrap_or(PollTimeout::MAX);
    match poll(&mut fds, t) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(nix::errno::Errno::EINTR) => return Ok(None),
        Err(e) => return Err(io::Error::from(e)),
    }
    let mut a = SockaddrQrtr { sq_family: 0, sq_node: 0, sq_port: 0 };
    let mut len = std::mem::size_of::<SockaddrQrtr>() as libc::socklen_t;
    // SAFETY: `buf` and `a` are valid, writable and of the given lengths.
    let n = unsafe {
        libc::recvfrom(
            fd.as_raw_fd(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            libc::MSG_DONTWAIT,
            &mut a as *mut _ as *mut libc::sockaddr,
            &mut len,
        )
    };
    if n < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(e);
    }
    Ok(Some((n as usize, a.sq_node, a.sq_port)))
}

fn le32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// Every service the name service knows (`service` 0 = all), within `timeout_ms`.
pub fn lookup(service: u32, timeout_ms: u64) -> Result<Vec<Server>> {
    let fd = socket().context("AF_QIPCRTR socket")?;
    // Bind by sending: autobind happens on the first sendto. getsockname afterwards gives the
    // local node, which is where the name service listens.
    let mut pkt = [0u8; 20];
    pkt[0..4].copy_from_slice(&QRTR_TYPE_NEW_LOOKUP.to_le_bytes());
    pkt[4..8].copy_from_slice(&service.to_le_bytes());
    // instance 0 + no filter = every instance.
    let node = {
        // An unbound socket reports node = local node already (qrtr_create sets sq_node).
        local_addr(&fd).context("qrtr getsockname")?.sq_node
    };
    send_to(&fd, node, QRTR_PORT_CTRL, &pkt).context("qrtr lookup")?;
    let deadline = clock::now_ms() + timeout_ms;
    let mut out = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let left = deadline.saturating_sub(clock::now_ms());
        if left == 0 {
            bail!("qrtr lookup: no end-of-list from the name service within {timeout_ms} ms");
        }
        let Some((n, _, _)) = recv_from(&fd, &mut buf, left)? else { continue };
        if n < 20 || le32(&buf, 0) != QRTR_TYPE_NEW_SERVER {
            continue;
        }
        let s = Server { service: le32(&buf, 4), instance: le32(&buf, 8), node: le32(&buf, 12), port: le32(&buf, 16) };
        if s.service == 0 && s.instance == 0 && s.node == 0 && s.port == 0 {
            return Ok(out);
        }
        out.push(s);
    }
}

/// Picks the server for `service` on `node` (or any node), lowest instance first.
pub fn pick(servers: &[Server], service: u32, node: Option<u32>) -> Option<Server> {
    servers
        .iter()
        .filter(|s| s.service == service && node.is_none_or(|n| s.node == n))
        .min_by_key(|s| (s.instance >> 8, s.instance & 0xFF))
        .copied()
}

/// One QMI client: a socket talking to one service.
pub struct Client {
    fd: OwnedFd,
    srv: Server,
    txn: u16,
    /// Indications that arrived while waiting for a response.
    pending: Vec<Msg>,
    name: &'static str,
}

impl Client {
    pub fn open(srv: Server, name: &'static str) -> Result<Client> {
        let fd = socket().with_context(|| format!("{name}: AF_QIPCRTR socket"))?;
        Ok(Client { fd, srv, txn: 0, pending: Vec::new(), name })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Sends `req` and waits for its response (same txn and message id).
    pub fn call(&mut self, req: Req, timeout_ms: u64) -> Result<Msg> {
        self.txn = self.txn.wrapping_add(1).max(1);
        let txn = self.txn;
        let id = req.msg_id();
        send_to(&self.fd, self.srv.node, self.srv.port, &req.encode(txn))
            .with_context(|| format!("{} 0x{id:04x}: send", self.name))?;
        let deadline = clock::now_ms() + timeout_ms;
        let mut buf = vec![0u8; 8192];
        loop {
            let left = deadline.saturating_sub(clock::now_ms());
            if left == 0 {
                bail!("{} 0x{id:04x}: no answer within {timeout_ms} ms", self.name);
            }
            let Some((n, node, port)) = recv_from(&self.fd, &mut buf, left)? else { continue };
            if node != self.srv.node || port != self.srv.port {
                continue;
            }
            let m = match Msg::decode(&buf[..n]) {
                Ok(m) => m,
                Err(e) => {
                    debug!("{}: dropping a bad message: {e:#}", self.name);
                    continue;
                }
            };
            match m.kind {
                TYPE_RESPONSE if m.txn == txn && m.msg_id == id => return Ok(m),
                TYPE_INDICATION => self.pending.push(m),
                _ => {}
            }
        }
    }

    /// Indications waiting (queued during calls, then whatever is readable now).
    pub fn indications(&mut self) -> Result<Vec<Msg>> {
        let mut out = std::mem::take(&mut self.pending);
        let mut buf = vec![0u8; 8192];
        while let Some((n, node, port)) = recv_from(&self.fd, &mut buf, 0)? {
            if node != self.srv.node || port != self.srv.port {
                continue;
            }
            if let Ok(m) = Msg::decode(&buf[..n]) {
                if m.kind == TYPE_INDICATION {
                    out.push(m);
                }
            }
        }
        Ok(out)
    }
}

/// The services Abyss uses, resolved on the modem node.
#[derive(Clone, Copy, Debug)]
pub struct Services {
    pub dms: Server,
    pub nas: Server,
    pub wds: Server,
    pub uim: Server,
}

impl Services {
    pub fn find(timeout_ms: u64) -> Result<Services> {
        use super::qmi::{SVC_DMS, SVC_NAS, SVC_UIM, SVC_WDS};
        let all = lookup(0, timeout_ms)?;
        // The modem node is the one that serves WDS (DMS also exists on some DSPs).
        let wds = pick(&all, SVC_WDS, None).ok_or_else(|| anyhow!("the modem offers no data service (WDS)"))?;
        let node = Some(wds.node);
        let need = |svc: u32, what: &str| pick(&all, svc, node).ok_or_else(|| anyhow!("the modem offers no {what} service"));
        Ok(Services {
            dms: need(SVC_DMS, "device (DMS)")?,
            nas: need(SVC_NAS, "network (NAS)")?,
            wds,
            uim: need(SVC_UIM, "SIM (UIM)")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_prefers_node_and_low_instance() {
        let s = |service, instance, node| Server { service, instance, node, port: 1 };
        let all = [s(1, 0x0101, 0), s(1, 0x0001, 0), s(2, 0x0001, 5), s(2, 0x0001, 0)];
        assert_eq!(pick(&all, 1, None).unwrap().instance, 0x0001);
        assert_eq!(pick(&all, 2, Some(0)).unwrap().node, 0);
        assert!(pick(&all, 3, None).is_none());
    }
}
