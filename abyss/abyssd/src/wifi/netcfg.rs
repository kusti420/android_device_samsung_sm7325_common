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
//! Mobile data (abyssd/src/mobile) uses the same ops with `"net_id":43` (MOBILE_NET_ID), plus
//! `"link_up":true` (interfaceSetCfg "up" before the address: abyss-net has no NET_ADMIN and
//! rmnet links are not brought up by anyone else), `"mtu":<n>` and `"default_dev":true` (the
//! default route straight onto the raw-IP point-to-point link, no gateway); "down" with
//! `"link_down":true` also takes the link down. `net_id` defaults to NET_ID (Wi-Fi). The newest
//! network that is up is netd's default; taking one down hands the default to the other one.
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
/// Mobile data's network (same OEM range; Wi-Fi and mobile can be up at the same time while the
/// uplink switches).
pub const MOBILE_NET_ID: i32 = 43;

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

    /// Mobile data: rmnet link up, address, routes onto the link, MOBILE_NET_ID as default.
    pub fn up_mobile(&mut self, ifname: &str, s: &crate::mobile::qmi::IpSettings) -> Result<u64> {
        let mut req = json!({
            "op": "up",
            "net_id": MOBILE_NET_ID,
            "ifname": ifname,
            "ip": s.ip.to_string(),
            "prefix": s.prefix,
            "subnet": s.subnet(),
            "link_up": true,
            "default_dev": true,
        });
        if let Some(m) = s.mtu {
            req["mtu"] = json!(m);
        }
        let r = self.request(&req)?;
        let ms = r["ms"].as_u64().unwrap_or(0);
        info!("netcfg: up {ifname} {}/{} (mobile) via netd in {ms} ms", s.ip, s.prefix);
        Ok(ms)
    }

    pub fn down_mobile(&mut self, ifname: &str) -> Result<()> {
        self.request(&json!({"op": "down", "net_id": MOBILE_NET_ID, "ifname": ifname, "link_down": true}))?;
        info!("netcfg: down {ifname} (mobile)");
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

/// The networks this helper brought up, oldest first; the newest is netd's default.
#[derive(Default)]
struct Nets {
    up: Vec<(i32, String)>,
}

fn net_id_of(req: &Value) -> i32 {
    req["net_id"].as_i64().map(|v| v as i32).filter(|v| [NET_ID, MOBILE_NET_ID].contains(v)).unwrap_or(NET_ID)
}

fn do_up(n: &Netd, req: &Value, nets: &mut Nets) -> Result<()> {
    let net_id = net_id_of(req);
    let ifname = req["ifname"].as_str().ok_or_else(|| anyhow!("no ifname"))?;
    let ip = req["ip"].as_str().ok_or_else(|| anyhow!("no ip"))?;
    let prefix = req["prefix"].as_i64().ok_or_else(|| anyhow!("no prefix"))? as i32;
    let subnet = req["subnet"].as_str().ok_or_else(|| anyhow!("no subnet"))?;
    // A network from an earlier lease (or a crashed session) would make create fail. The
    // default is only cleared when no other Abyss network is up (it moves to this one below).
    if nets.up.iter().all(|(id, _)| *id == net_id) {
        let _ = n.network_clear_default();
    }
    n.network_destroy(net_id).or_else(|e| ignore(e, netd::ENONET))?;
    nets.up.retain(|(id, _)| *id != net_id);
    if req["link_up"].as_bool() == Some(true) {
        // Before the address: interfaceSetCfg clears the IPv4 address, then reports the empty
        // one as "Failed to add addr" (netd.rs); the flag is applied either way.
        let _ = n.interface_set_flag(ifname, "up");
    }
    if let Some(mtu) = req["mtu"].as_i64() {
        if let Err(e) = n.interface_set_mtu(ifname, mtu as i32) {
            warn!("up: {e} (keeping the link's MTU)");
        }
    }
    n.network_create_physical(net_id, netd::PERMISSION_NONE).map_err(|e| anyhow!("{e}"))?;
    n.network_add_interface(net_id, ifname).map_err(|e| anyhow!("{e}"))?;
    n.interface_add_address(ifname, ip, prefix).or_else(|e| ignore(e, netd::EEXIST))?;
    n.network_add_route(net_id, ifname, subnet, "").or_else(|e| ignore(e, netd::EEXIST))?;
    if req["default_dev"].as_bool() == Some(true) {
        n.network_add_route(net_id, ifname, "0.0.0.0/0", "").or_else(|e| ignore(e, netd::EEXIST))?;
    } else if let Some(gw) = req["gateway"].as_str() {
        n.network_add_route(net_id, ifname, "0.0.0.0/0", gw).or_else(|e| ignore(e, netd::EEXIST))?;
    }
    n.network_set_default(net_id).map_err(|e| anyhow!("{e}"))?;
    nets.up.push((net_id, ifname.to_string()));
    Ok(())
}

/// Everything "down" does, best effort; errors are logged, never fatal.
fn do_down(n: &Netd, net_id: i32, ifname: &str, link_down: bool, nets: &mut Nets) {
    nets.up.retain(|(id, _)| *id != net_id);
    // The other Abyss network (if one is up) becomes the default again.
    let res = match nets.up.last() {
        Some((other, _)) => n.network_set_default(*other),
        None => n.network_clear_default(),
    };
    if let Err(e) = res {
        warn!("down: {e}");
    }
    if let Err(e) = n.network_destroy(net_id) {
        if e.errno != netd::ENONET {
            warn!("down: {e}");
        }
    }
    if let Err(e) = n.interface_clear_addrs(ifname) {
        warn!("down: {e}");
    }
    if link_down {
        // Expected to report "Failed to add addr" after the flag change (netd.rs).
        let _ = n.interface_set_flag(ifname, "down");
    }
}

fn handle(netd: &mut Option<Netd>, nets: &mut Nets, msg: &[u8]) -> Value {
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
        "up" => do_up(n, &req, nets),
        "down" => {
            let link_down = req["link_down"].as_bool() == Some(true);
            do_down(n, net_id_of(&req), req["ifname"].as_str().unwrap_or("wlan0"), link_down, nets);
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
    // init only creates and binds a service socket; the service itself has to listen(2). Without
    // it every accept() failed with EINVAL and abyssd's connect() found no listener, so Wi-Fi
    // never got past DHCP.
    // SAFETY: listen(2) on our own bound socket fd.
    if unsafe { libc::listen(owned.as_raw_fd(), 4) } != 0 {
        return Err(std::io::Error::last_os_error()).context("listen on the init socket");
    }
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
    let mut nets = Nets::default();
    // abyssd's Wi-Fi and mobile workers each hold their own connection.
    let mut clients: Vec<UnixStream> = Vec::new();
    let mut buf = vec![0u8; 4096];
    loop {
        let (sig, lst, cli) = {
            let mut fds = vec![
                PollFd::new(sig_r.as_fd(), PollFlags::POLLIN),
                PollFd::new(listener.as_fd(), PollFlags::POLLIN),
            ];
            for c in &clients {
                fds.push(PollFd::new(c.as_fd(), PollFlags::POLLIN));
            }
            match poll(&mut fds, PollTimeout::NONE) {
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(e).context("poll"),
            }
            let ready = |i: usize| fds.get(i).and_then(|f| f.revents()).is_some_and(|r| !r.is_empty());
            let cli: Vec<bool> = (0..clients.len()).map(|i| ready(2 + i)).collect();
            (ready(0), ready(1), cli)
        };
        // A listener that can no longer accept (init closed or shut its socket while stopping
        // us: accept() says EINVAL forever) would turn this loop into a busy spin that floods
        // the log; treat it like SIGTERM and go down cleanly.
        let mut dead_listener = false;
        if lst && !sig {
            match listener.accept() {
                // Pushed at the end, so this round's `cli` indices stay valid; a closed
                // client is removed when its read returns 0.
                Ok((s, _)) => clients.push(s),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(e) => {
                    warn!("accept: {e}; control socket unusable, going down");
                    dead_listener = true;
                }
            }
        }
        if sig || dead_listener {
            info!("SIGTERM: taking the Abyss networks and wlan0 down");
            let n = match netd.take() {
                Some(n) => Some(n),
                None => Netd::connect().ok(),
            };
            if let Some(n) = n {
                // Mobile data first (its rmnet link goes down too), then Wi-Fi as before.
                let mobile: Vec<String> =
                    nets.up.iter().filter(|(id, _)| *id == MOBILE_NET_ID).map(|(_, i)| i.clone()).collect();
                for ifname in mobile {
                    do_down(&n, MOBILE_NET_ID, &ifname, true, &mut nets);
                }
                do_down(&n, NET_ID, "wlan0", false, &mut nets);
                // Expected to report "Failed to add addr" after the flag change (netd.rs).
                let _ = n.interface_set_flag("wlan0", "down");
            }
            return Ok(());
        }
        // Requests are served one at a time, in poll order (one netd connection, invariant I7).
        let mut dead = Vec::new();
        for (i, ready) in cli.iter().enumerate() {
            if !ready {
                continue;
            }
            let Some(c) = clients.get_mut(i) else { continue };
            match c.read(&mut buf) {
                Ok(0) | Err(_) => dead.push(i),
                Ok(n) => {
                    let reply = handle(&mut netd, &mut nets, &buf[..n]);
                    if c.write_all(reply.to_string().as_bytes()).is_err() {
                        dead.push(i);
                    }
                }
            }
        }
        for i in dead.into_iter().rev() {
            clients.remove(i);
        }
    }
}
