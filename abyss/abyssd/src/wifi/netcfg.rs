//! IP configuration through netd, via the `abyss-net` helper.
//!
//! Why a helper: netd only skips its permission check for uid 1000 (system); for every other
//! caller, root included, it asks system_server's permission service and waits forever while
//! system_server is stopped (netd.rs). abyssd runs as root, so the INetd calls are made by
//! `abyss-net` = this same binary started by init as `/system_ext/bin/abyssd --net-helper`,
//! `user system`, SELinux domain abyss_net (abyss.rc, sepolicy/private/abyss_net.te). init starts
//! it together with wpa_supplicant on `sys.abyss.wifi=on` and stops it on `off`.
//!
//! Protocol: init socket /dev/socket/abyss_net (SOCK_SEQPACKET, system:system 0660), one JSON
//! request per packet, one JSON reply per packet:
//!
//! - `{"op":"up","ifname":"wlan0","ip":"192.0.2.23","prefix":24,"subnet":"192.0.2.0/24",
//!   "gateway":"192.0.2.1"}` → network NET_ID (physical, PERMISSION_NONE) with wlan0, the
//!   address, the subnet route, the default route, and NET_ID as the default network.
//! - `{"op":"down","ifname":"wlan0"}` → default cleared, NET_ID destroyed, addresses cleared.
//! - `{"op":"ping"}` → netd's isAlive.
//!
//! Reply: `{"ok":true,"ms":<n>}` or `{"ok":false,"err":"<method>: <netd status>"}`.
//!
//! Clean-up guarantee: on SIGTERM (init `stop`, i.e. every Wi-Fi off and every leave, also after
//! an abyssd crash) the helper runs "down" and takes wlan0 down (interfaceSetCfg "down"), so
//! Android never comes back to a netd that still holds the Abyss network or a link that is up.

use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use log::{error, info, warn, LevelFilter};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use serde_json::{json, Value};

use super::netd::{self, Netd};
use crate::clock;

pub const SOCKET: &str = "/dev/socket/abyss_net";
const SOCKET_NAME: &str = "abyss_net";

/// The Abyss network's netId. ConnectivityService allocates 100..65535 (NetIdManager), netd also
/// accepts the OEM range 1..50 (NetworkController MIN_OEM_ID..MAX_OEM_ID). Using an OEM id means
/// a network left behind could never collide with one Android creates after the return.
pub const NET_ID: i32 = 42;

// ---------------------------------------------------------------------------
// abyssd side

fn connect_seqpacket(path: &str) -> std::io::Result<OwnedFd> {
    // SAFETY: plain socket(2); the fd is owned right below.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh fd owned by nobody else.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: zeroed sockaddr_un is a valid value; sun_path is filled below within bounds.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(std::io::Error::new(ErrorKind::InvalidInput, "socket path too long"));
    }
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: `addr` is a valid sockaddr_un of at least `len` bytes.
    let r = unsafe { libc::connect(fd.as_raw_fd(), &addr as *const _ as *const libc::sockaddr, len) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

/// abyssd's connection to the helper.
pub struct NetCfg {
    s: UnixStream,
}

impl NetCfg {
    /// Connects, retrying while init is still starting the helper.
    pub fn connect(timeout_ms: u64) -> Result<NetCfg> {
        let deadline = clock::now_ms() + timeout_ms;
        loop {
            match connect_seqpacket(SOCKET) {
                Ok(fd) => {
                    let s = UnixStream::from(fd);
                    s.set_read_timeout(Some(Duration::from_secs(10)))?;
                    s.set_write_timeout(Some(Duration::from_secs(2)))?;
                    return Ok(NetCfg { s });
                }
                Err(e) if clock::now_ms() < deadline => {
                    log::debug!("abyss-net not there yet: {e}");
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e).context(format!("connect {SOCKET}")),
            }
        }
    }

    fn request(&mut self, v: &Value) -> Result<Value> {
        self.s.write_all(v.to_string().as_bytes()).context("abyss-net send")?;
        let mut buf = vec![0u8; 4096];
        let n = self.s.read(&mut buf).context("abyss-net reply")?;
        if n == 0 {
            bail!("abyss-net hung up");
        }
        let r: Value = serde_json::from_slice(&buf[..n]).context("abyss-net reply json")?;
        if r["ok"].as_bool() == Some(true) {
            Ok(r)
        } else {
            Err(anyhow!("{}", r["err"].as_str().unwrap_or("abyss-net failed")))
        }
    }

    pub fn up(&mut self, ifname: &str, lease: &super::dhcp::Lease) -> Result<()> {
        let mut req = json!({
            "op": "up",
            "ifname": ifname,
            "ip": lease.ip.to_string(),
            "prefix": lease.prefix,
            "subnet": lease.subnet(),
        });
        if let Some(gw) = lease.gateway {
            req["gateway"] = json!(gw.to_string());
        }
        let r = self.request(&req)?;
        info!("netcfg: up {ifname} {}/{} via netd in {} ms", lease.ip, lease.prefix, r["ms"]);
        Ok(())
    }

    pub fn down(&mut self, ifname: &str) -> Result<()> {
        self.request(&json!({"op": "down", "ifname": ifname}))?;
        info!("netcfg: down {ifname}");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The helper (`abyssd --net-helper`, uid system, domain abyss_net)

fn ignore(e: netd::CallError, benign: i32) -> Result<()> {
    if e.errno == benign {
        Ok(())
    } else {
        Err(anyhow!("{e}"))
    }
}

fn do_up(n: &Netd, req: &Value) -> Result<()> {
    let ifname = req["ifname"].as_str().ok_or_else(|| anyhow!("no ifname"))?;
    let ip = req["ip"].as_str().ok_or_else(|| anyhow!("no ip"))?;
    let prefix = req["prefix"].as_i64().ok_or_else(|| anyhow!("no prefix"))? as i32;
    let subnet = req["subnet"].as_str().ok_or_else(|| anyhow!("no subnet"))?;
    // A network from an earlier lease (or a crashed session) would make create fail.
    let _ = n.network_clear_default();
    n.network_destroy(NET_ID).or_else(|e| ignore(e, netd::ENONET))?;
    n.network_create_physical(NET_ID, netd::PERMISSION_NONE).map_err(|e| anyhow!("{e}"))?;
    n.network_add_interface(NET_ID, ifname).map_err(|e| anyhow!("{e}"))?;
    n.interface_add_address(ifname, ip, prefix).or_else(|e| ignore(e, netd::EEXIST))?;
    n.network_add_route(NET_ID, ifname, subnet, "").or_else(|e| ignore(e, netd::EEXIST))?;
    if let Some(gw) = req["gateway"].as_str() {
        n.network_add_route(NET_ID, ifname, "0.0.0.0/0", gw).or_else(|e| ignore(e, netd::EEXIST))?;
    }
    n.network_set_default(NET_ID).map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

/// Everything "down" does, best effort; errors are logged, never fatal.
fn do_down(n: &Netd, ifname: &str) {
    if let Err(e) = n.network_clear_default() {
        warn!("down: {e}");
    }
    if let Err(e) = n.network_destroy(NET_ID) {
        if e.errno != netd::ENONET {
            warn!("down: {e}");
        }
    }
    if let Err(e) = n.interface_clear_addrs(ifname) {
        warn!("down: {e}");
    }
}

fn handle(netd: &mut Option<Netd>, msg: &[u8]) -> Value {
    let start = clock::now_ms();
    let req: Value = match serde_json::from_slice(msg) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "err": format!("bad request: {e}")}),
    };
    if netd.is_none() {
        match Netd::connect() {
            Ok(n) => *netd = Some(n),
            Err(e) => return json!({"ok": false, "err": format!("{e:#}")}),
        }
    }
    let Some(n) = netd.as_ref() else { return json!({"ok": false, "err": "no netd"}) };
    let op = req["op"].as_str().unwrap_or("");
    let res = match op {
        "up" => do_up(n, &req),
        "down" => {
            do_down(n, req["ifname"].as_str().unwrap_or("wlan0"));
            Ok(())
        }
        "ping" => n.is_alive().map_err(|e| anyhow!("{e}")).and_then(|a| {
            if a {
                Ok(())
            } else {
                Err(anyhow!("netd says it is not alive"))
            }
        }),
        _ => Err(anyhow!("unknown op {op:?}")),
    };
    let ms = clock::now_ms() - start;
    match res {
        Ok(()) => {
            info!("{op}: ok in {ms} ms");
            json!({"ok": true, "ms": ms})
        }
        Err(e) => {
            warn!("{op}: {e:#}");
            json!({"ok": false, "err": format!("{e:#}"), "ms": ms})
        }
    }
}

/// init's socket for us: ANDROID_SOCKET_abyss_net=<fd>.
fn control_socket() -> Result<UnixListener> {
    let v = std::env::var(format!("ANDROID_SOCKET_{SOCKET_NAME}")).context("no init socket")?;
    let fd: i32 = v.trim().parse().context("bad init socket fd")?;
    // SAFETY: init passed this fd to us for exactly this socket and nothing else owns it.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: plain fcntl(2) on our own fd: init's socket fds must not leak into children.
    unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    Ok(UnixListener::from(owned))
}

/// Entry point of `abyssd --net-helper`. Returns the exit code.
pub fn helper_main() -> i32 {
    android_logger::init_once(
        android_logger::Config::default().with_tag("abyss-net").with_max_level(LevelFilter::Info),
    );
    info!("abyss-net starting (pid {}, uid {})", std::process::id(), nix::unistd::getuid());
    match helper_loop() {
        Ok(()) => 0,
        Err(e) => {
            error!("fatal: {e:#}");
            1
        }
    }
}

fn helper_loop() -> Result<()> {
    let (sig_r, sig_w) = UnixStream::pair()?;
    sig_r.set_nonblocking(true)?;
    sig_w.set_nonblocking(true)?;
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGTERM, sig_w.try_clone()?)?;
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGINT, sig_w)?;
    let listener = control_socket()?;
    let mut netd: Option<Netd> = None;
    let mut client: Option<UnixStream> = None;
    let mut buf = vec![0u8; 4096];
    loop {
        let (sig, lst, cli) = {
            let mut fds = vec![
                PollFd::new(sig_r.as_fd(), PollFlags::POLLIN),
                PollFd::new(listener.as_fd(), PollFlags::POLLIN),
            ];
            if let Some(c) = &client {
                fds.push(PollFd::new(c.as_fd(), PollFlags::POLLIN));
            }
            match poll(&mut fds, PollTimeout::NONE) {
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(e).context("poll"),
            }
            let ready = |i: usize| fds.get(i).and_then(|f| f.revents()).is_some_and(|r| !r.is_empty());
            (ready(0), ready(1), ready(2))
        };
        if sig {
            info!("SIGTERM: taking the Abyss network and wlan0 down");
            let n = match netd.take() {
                Some(n) => Some(n),
                None => Netd::connect().ok(),
            };
            if let Some(n) = n {
                do_down(&n, "wlan0");
                // Expected to report "Failed to add addr" after the flag change (netd.rs).
                let _ = n.interface_set_flag("wlan0", "down");
            }
            return Ok(());
        }
        if lst {
            match listener.accept() {
                // One client (abyssd); a new connection replaces an old one.
                Ok((s, _)) => client = Some(s),
                Err(e) => warn!("accept: {e}"),
            }
        }
        if cli {
            let Some(c) = client.as_mut() else { continue };
            match c.read(&mut buf) {
                Ok(0) | Err(_) => client = None,
                Ok(n) => {
                    let reply = handle(&mut netd, &buf[..n]);
                    if c.write_all(reply.to_string().as_bytes()).is_err() {
                        client = None;
                    }
                }
            }
        }
    }
}
