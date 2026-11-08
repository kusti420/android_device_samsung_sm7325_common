//! A small HTTP/1.1 client for abyssd's own network work (modules such as the radio).
//!
//! - GET only (all our module traffic is GET), keep-alive with one idle connection per
//!   origin, Content-Length / chunked / read-to-close bodies, a streaming body reader for
//!   endless audio, hard limits on header and body sizes, timeouts on every step.
//! - Plain `http://` over `std::net::TcpStream`. TLS is additive later: a [`Transport`] for
//!   [`Scheme::Https`] wraps the TCP connection; nothing above the transport changes.
//! - Sockets can be bound to one interface (`SO_BINDTODEVICE`, e.g. `wlan0` inside Abyss,
//!   like wifi/http.rs does), so a request never leaks out of another route.
//! - [`Cancel`]: another thread can abort a blocked request (a 15 s burst ask, a 55 s long
//!   poll) by shutting its socket down; the request then fails with `Interrupted`.
//!
//! Network use inside Abyss is only allowed while a Wi-Fi lease is held (wifi/mod.rs); the
//! client itself does not know about leases. The radio takes a lease before every wake and
//! releases it after (radio_source.rs).

use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Longest status line + headers we accept.
const MAX_HEAD: usize = 16 * 1024;
/// Longest chunk-size line.
const MAX_LINE: usize = 1024;
const USER_AGENT: &str = "abyssd/1 (IchthysOS Abyss)";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scheme {
    Http,
    /// Parsed so links stay valid; served only once a TLS [`Transport`] is registered.
    Https,
}

/// An absolute http(s) URL, split for the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    pub scheme: Scheme,
    /// Host name or address as written (no brackets: IPv6 literals are not supported).
    pub host: String,
    pub port: u16,
    /// Path plus query, always starting with '/'.
    pub target: String,
}

impl Url {
    pub fn parse(s: &str) -> io::Result<Url> {
        let s = s.trim();
        let (scheme, rest) = if let Some(r) = strip_prefix_ci(s, "http://") {
            (Scheme::Http, r)
        } else if let Some(r) = strip_prefix_ci(s, "https://") {
            (Scheme::Https, r)
        } else {
            return Err(bad(format!("not an http(s) URL: {s}")));
        };
        let (hostport, target) = match rest.find(['/', '?']) {
            Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
            Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
            None => (rest, "/".to_string()),
        };
        let target = target.split('#').next().unwrap_or("/").to_string();
        if hostport.contains('@') || hostport.starts_with('[') {
            return Err(bad(format!("unsupported authority in {s}")));
        }
        let default_port = if scheme == Scheme::Http { 80 } else { 443 };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().map_err(|_| bad(format!("bad port in {s}")))?),
            None => (hostport, default_port),
        };
        if host.is_empty() || port == 0 {
            return Err(bad(format!("no host in {s}")));
        }
        Ok(Url { scheme, host: host.to_ascii_lowercase(), port, target })
    }

    /// Resolves a link found in an answer against this URL (absolute links win, like
    /// `urllib.parse.urljoin` for the cases a server sends: absolute, `/path`, `path`).
    #[allow(dead_code)] // radio_source::fetch_stations (the PC test tool); modules get absolute links
    pub fn join(&self, link: &str) -> io::Result<Url> {
        let link = link.trim();
        if strip_prefix_ci(link, "http://").is_some() || strip_prefix_ci(link, "https://").is_some() {
            return Url::parse(link);
        }
        if let Some(rest) = link.strip_prefix("//") {
            let s = match self.scheme {
                Scheme::Http => format!("http://{rest}"),
                Scheme::Https => format!("https://{rest}"),
            };
            return Url::parse(&s);
        }
        let target = if link.starts_with('/') {
            link.to_string()
        } else if link.starts_with('?') {
            format!("{}{link}", self.path())
        } else {
            let dir = match self.path().rfind('/') {
                Some(i) => &self.path()[..=i],
                None => "/",
            };
            format!("{dir}{link}")
        };
        Ok(Url { target, ..self.clone() })
    }

    /// The path without the query.
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("/")
    }

    /// The same URL with `pairs` as its query (replacing any query it had).
    pub fn with_query(&self, pairs: &[(&str, String)]) -> Url {
        let q = encode_query(pairs);
        let target = if q.is_empty() { self.path().to_string() } else { format!("{}?{q}", self.path()) };
        Url { target, ..self.clone() }
    }

    /// "host:port" for the Host header (port left out when it is the default).
    pub fn authority(&self) -> String {
        let default = if self.scheme == Scheme::Http { 80 } else { 443 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    fn origin(&self) -> (Scheme, String, u16) {
        (self.scheme, self.host.clone(), self.port)
    }

    /// `scheme://host[:port]` (port left out when it is the default), lower case.
    pub fn origin_text(&self) -> String {
        let s = if self.scheme == Scheme::Http { "http" } else { "https" };
        format!("{s}://{}", self.authority())
    }

    /// Same scheme, host and port as `origin` (`scheme://host[:port]`, any case).
    pub fn same_origin(&self, origin: &str) -> bool {
        Url::parse(origin).is_ok_and(|o| o.origin() == self.origin())
    }
}

impl std::fmt::Display for Url {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = if self.scheme == Scheme::Http { "http" } else { "https" };
        write!(f, "{s}://{}{}", self.authority(), self.target)
    }
}

fn strip_prefix_ci<'a>(s: &'a str, p: &str) -> Option<&'a str> {
    (s.len() >= p.len() && s.is_char_boundary(p.len()) && s[..p.len()].eq_ignore_ascii_case(p))
        .then(|| &s[p.len()..])
}

/// `k=v&k2=v2`, percent-encoding everything but unreserved characters. An empty value
/// stays as `k=` (the radio's first ask is `cursor=`).
pub fn encode_query(pairs: &[(&str, String)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        pct(&mut out, k);
        out.push('=');
        pct(&mut out, v);
    }
    out
}

fn pct(out: &mut String, s: &str) {
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

fn bad(msg: String) -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, msg)
}

fn proto(msg: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, msg.into())
}

// ---------------------------------------------------------------------------
// Transports

/// One connection a transport made. `abort` must make a blocked read/write on it return
/// (any thread).
pub trait Conn: Read + Write + Send {
    fn set_timeouts(&self, t: Duration) -> io::Result<()>;
    /// A handle that aborts this connection from another thread.
    fn aborter(&self) -> Option<Aborter>;
}

/// Shuts one connection down from another thread.
pub struct Aborter(Box<dyn Fn() + Send + Sync>);

impl Aborter {
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Aborter {
        Aborter(Box::new(f))
    }
    fn abort(&self) {
        (self.0)()
    }
}

/// Opens connections for one scheme. TLS later = another implementation that wraps
/// `TcpTransport`'s stream.
pub trait Transport: Send + Sync {
    fn connect(&self, url: &Url, timeout: Duration) -> io::Result<Box<dyn Conn>>;
}

/// Name lookup for hosts that are not plain IPv4 addresses (Abyss has its own resolver in
/// wifi/dns.rs; bionic's getaddrinfo does not work there).
pub type Resolve = Arc<dyn Fn(&str) -> io::Result<Vec<Ipv4Addr>> + Send + Sync>;

/// Plain TCP, optionally bound to one interface.
#[derive(Clone, Default)]
pub struct TcpTransport {
    /// `SO_BINDTODEVICE` before connect (needs CAP_NET_RAW; abyssd has it inside Abyss).
    pub bind_device: Option<String>,
    /// None = the system resolver (fine on the host; inside Abyss pass the Wi-Fi one).
    pub resolve: Option<Resolve>,
}

impl TcpTransport {
    fn addrs(&self, url: &Url) -> io::Result<Vec<SocketAddr>> {
        if let Ok(ip) = url.host.parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(ip, url.port)]);
        }
        match &self.resolve {
            Some(r) => Ok(r(&url.host)?.into_iter().map(|ip| SocketAddr::new(IpAddr::V4(ip), url.port)).collect()),
            None => Ok((url.host.as_str(), url.port).to_socket_addrs()?.filter(|a| a.is_ipv4()).collect()),
        }
    }
}

struct TcpConn(TcpStream);

impl Read for TcpConn {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.read(b)
    }
}

impl Write for TcpConn {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Conn for TcpConn {
    fn set_timeouts(&self, t: Duration) -> io::Result<()> {
        self.0.set_read_timeout(Some(t))?;
        self.0.set_write_timeout(Some(t))
    }
    fn aborter(&self) -> Option<Aborter> {
        let s = self.0.try_clone().ok()?;
        Some(Aborter::new(move || {
            let _ = s.shutdown(Shutdown::Both);
        }))
    }
}

impl Transport for TcpTransport {
    fn connect(&self, url: &Url, timeout: Duration) -> io::Result<Box<dyn Conn>> {
        let addrs = self.addrs(url)?;
        let mut last = io::Error::new(ErrorKind::NotFound, format!("no IPv4 address for {}", url.host));
        for a in addrs {
            match connect_one(a, self.bind_device.as_deref(), timeout) {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    return Ok(Box::new(TcpConn(s)));
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

fn connect_one(addr: SocketAddr, device: Option<&str>, timeout: Duration) -> io::Result<TcpStream> {
    let Some(dev) = device else {
        return TcpStream::connect_timeout(&addr, timeout);
    };
    let SocketAddr::V4(v4) = addr else {
        return Err(io::Error::new(ErrorKind::Unsupported, "IPv4 only"));
    };
    // SAFETY: plain socket(2); the fd is owned right below.
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh fd owned by nobody else.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let name = dev.as_bytes();
    // SAFETY: setsockopt on our own fd with a valid pointer/length pair.
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
        return Err(io::Error::last_os_error());
    }
    let s = TcpStream::from(fd);
    // A blocking connect(2) honours SO_SNDTIMEO on Linux.
    s.set_write_timeout(Some(timeout))?;
    s.set_read_timeout(Some(timeout))?;
    // SAFETY: an all-zero sockaddr_in is valid; the fields are set below.
    let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = v4.port().to_be();
    sin.sin_addr = libc::in_addr { s_addr: u32::from(*v4.ip()).to_be() };
    // SAFETY: `sin` is a valid sockaddr_in of the size given, on our own socket.
    let r = unsafe {
        libc::connect(
            s.as_raw_fd(),
            &sin as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(s)
}

// ---------------------------------------------------------------------------
// Cancel

/// Aborts the requests registered on it. Clone it into the thread that wants to stop the
/// worker; `cancel()` stays in force until `reset()`.
#[derive(Clone, Default)]
pub struct Cancel(Arc<Mutex<CancelInner>>);

#[derive(Default)]
struct CancelInner {
    cancelled: bool,
    live: Vec<(u64, Aborter)>,
    next: u64,
}

impl Cancel {
    pub fn cancel(&self) {
        if let Ok(mut c) = self.0.lock() {
            c.cancelled = true;
            for (_, a) in c.live.drain(..) {
                a.abort();
            }
        }
    }
    #[allow(dead_code)] // Radio::debug_drop_connection (the PC test tool)
    pub fn reset(&self) {
        if let Ok(mut c) = self.0.lock() {
            c.cancelled = false;
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.lock().map(|c| c.cancelled).unwrap_or(true)
    }
    fn register(&self, a: Option<Aborter>) -> io::Result<Option<u64>> {
        let mut c = self.0.lock().map_err(|_| io::Error::other("cancel lock"))?;
        if c.cancelled {
            if let Some(a) = a {
                a.abort();
            }
            return Err(io::Error::new(ErrorKind::Interrupted, "cancelled"));
        }
        let Some(a) = a else { return Ok(None) };
        c.next += 1;
        let id = c.next;
        c.live.push((id, a));
        Ok(Some(id))
    }
    fn unregister(&self, id: Option<u64>) {
        if let (Some(id), Ok(mut c)) = (id, self.0.lock()) {
            c.live.retain(|(i, _)| *i != id);
        }
    }
}

// ---------------------------------------------------------------------------
// Client

/// What a request may take.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    pub connect: Duration,
    /// Each read / write (a long poll needs its `wait` plus slack here).
    pub io: Duration,
}

impl Timeouts {
    pub fn io_s(secs: u64) -> Timeouts {
        Timeouts { connect: Duration::from_secs(5), io: Duration::from_secs(secs) }
    }
}

pub struct Client {
    http: Arc<dyn Transport>,
    https: Option<Arc<dyn Transport>>,
    /// One idle keep-alive connection per origin (the audio ask and the tracklist sync of
    /// one wake share it).
    idle: Mutex<Vec<((Scheme, String, u16), Box<dyn Conn>)>>,
    cancel: Cancel,
    /// Only these origins may be fetched (a module's approved `net`); None = any.
    allow: Option<Vec<String>>,
}

impl Client {
    pub fn new(http: Arc<dyn Transport>, cancel: Cancel) -> Client {
        Client { http, https: None, idle: Mutex::new(Vec::new()), cancel, allow: None }
    }

    /// Restricts every request (links from answers included) to these origins
    /// (`scheme://host[:port]`). Anything else fails with `PermissionDenied` and is logged.
    pub fn with_allow(mut self, origins: Vec<String>) -> Client {
        self.allow = Some(origins);
        self
    }

    /// Whether `url` may be fetched by this client.
    pub fn allows(&self, url: &Url) -> bool {
        self.allow.as_ref().is_none_or(|a| a.iter().any(|o| url.same_origin(o)))
    }

    /// Registers a TLS transport for https:// (none is built yet: the seam TLS plugs into).
    #[allow(dead_code)]
    pub fn with_https(mut self, t: Arc<dyn Transport>) -> Client {
        self.https = Some(t);
        self
    }

    /// Drops idle keep-alive connections (call when the Wi-Fi lease ends: the link may go).
    pub fn close_idle(&self) {
        if let Ok(mut v) = self.idle.lock() {
            v.clear();
        }
    }

    fn transport(&self, url: &Url) -> io::Result<&Arc<dyn Transport>> {
        match url.scheme {
            Scheme::Http => Ok(&self.http),
            Scheme::Https => self
                .https
                .as_ref()
                .ok_or_else(|| io::Error::new(ErrorKind::Unsupported, "https is not built into abyssd yet")),
        }
    }

    fn take_idle(&self, url: &Url) -> Option<Box<dyn Conn>> {
        let mut v = self.idle.lock().ok()?;
        let o = url.origin();
        let i = v.iter().position(|(k, _)| *k == o)?;
        Some(v.swap_remove(i).1)
    }

    fn put_idle(&self, origin: (Scheme, String, u16), c: Box<dyn Conn>) {
        if let Ok(mut v) = self.idle.lock() {
            v.retain(|(k, _)| *k != origin);
            v.push((origin, c));
        }
    }

    /// GET `url`. The answer's body is not read yet: call [`Response::bytes`] (and get the
    /// connection back for keep-alive) or [`Response::into_reader`] for a stream.
    pub fn get(&self, url: &Url, extra: &[(&str, &str)], t: Timeouts) -> io::Result<Response<'_>> {
        if !self.allows(url) {
            log::warn!("http: refused {} (not an approved address)", url.origin_text());
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!("{} is not an approved address", url.origin_text()),
            ));
        }
        // A kept connection may have been closed by the server meanwhile: one retry on a
        // fresh one (GET is idempotent).
        if let Some(c) = self.take_idle(url) {
            match self.exchange(c, url, extra, t, true) {
                Ok(r) => return Ok(r),
                Err(e) if e.kind() == ErrorKind::Interrupted => return Err(e),
                Err(_) => {}
            }
        }
        let c = self.transport(url)?.connect(url, t.connect)?;
        self.exchange(c, url, extra, t, false)
    }

    fn exchange(
        &self,
        conn: Box<dyn Conn>,
        url: &Url,
        extra: &[(&str, &str)],
        t: Timeouts,
        reused: bool,
    ) -> io::Result<Response<'_>> {
        conn.set_timeouts(t.io)?;
        let reg = self.cancel.register(conn.aborter())?;
        let mut w = Wire::new(conn);
        let r = (|| {
            let mut req = format!(
                "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {USER_AGENT}\r\nAccept-Encoding: identity\r\n",
                url.target,
                url.authority()
            );
            for (k, v) in extra {
                if k.contains(['\r', '\n', ':']) || v.contains(['\r', '\n']) {
                    return Err(bad("header with a line break".into()));
                }
                req.push_str(&format!("{k}: {v}\r\n"));
            }
            req.push_str("\r\n");
            w.conn.write_all(req.as_bytes())?;
            w.conn.flush()?;
            read_head(&mut w, reused)
        })();
        match r {
            Ok((status, headers)) => {
                let framing = framing(status, &headers)?;
                let keep = !headers.iter().any(|(k, v)| {
                    k.eq_ignore_ascii_case("connection") && v.to_ascii_lowercase().contains("close")
                }) && !matches!(framing, Framing::ToClose);
                Ok(Response {
                    status,
                    headers,
                    body: Body { wire: w, framing, done: false },
                    keep,
                    origin: url.origin(),
                    client: self,
                    reg,
                })
            }
            Err(e) => {
                self.cancel.unregister(reg);
                if self.cancel.is_cancelled() {
                    return Err(io::Error::new(ErrorKind::Interrupted, "cancelled"));
                }
                Err(e)
            }
        }
    }
}

/// Buffered reading on a connection.
struct Wire {
    conn: Box<dyn Conn>,
    buf: Vec<u8>,
    pos: usize,
}

impl Wire {
    fn new(conn: Box<dyn Conn>) -> Wire {
        Wire { conn, buf: Vec::with_capacity(16 * 1024), pos: 0 }
    }

    /// More bytes into the buffer; Ok(0) = the peer closed.
    fn fill(&mut self) -> io::Result<usize> {
        if self.pos > 0 && self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
        let old = self.buf.len();
        self.buf.resize(old + 16 * 1024, 0);
        let r = loop {
            match self.conn.read(&mut self.buf[old..]) {
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                r => break r,
            }
        };
        let n = *r.as_ref().unwrap_or(&0);
        self.buf.truncate(old + n);
        r
    }

    fn avail(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    /// One CRLF-terminated line (without the CRLF), at most `max` bytes.
    fn line(&mut self, max: usize) -> io::Result<String> {
        loop {
            if let Some(i) = self.avail().windows(2).position(|w| w == b"\r\n") {
                let s = String::from_utf8_lossy(&self.avail()[..i]).into_owned();
                self.pos += i + 2;
                return Ok(s);
            }
            if self.avail().len() > max {
                return Err(proto("line too long"));
            }
            if self.fill()? == 0 {
                return Err(io::Error::new(ErrorKind::UnexpectedEof, "connection closed"));
            }
        }
    }

    /// Up to `out.len()` bytes; Ok(0) = closed.
    fn read_some(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.avail().is_empty() && self.fill()? == 0 {
            return Ok(0);
        }
        let n = out.len().min(self.avail().len());
        out[..n].copy_from_slice(&self.avail()[..n]);
        self.pos += n;
        Ok(n)
    }
}

fn read_head(w: &mut Wire, reused: bool) -> io::Result<(u16, Vec<(String, String)>)> {
    let mut total = 0usize;
    let status_line = match w.line(MAX_HEAD) {
        Err(e) if reused && e.kind() == ErrorKind::UnexpectedEof => {
            return Err(io::Error::new(ErrorKind::ConnectionReset, "kept connection was closed"))
        }
        r => r?,
    };
    total += status_line.len();
    let mut parts = status_line.splitn(3, ' ');
    let ver = parts.next().unwrap_or("");
    if !ver.starts_with("HTTP/1.") {
        return Err(proto(format!("not HTTP/1.x: {status_line:.60}")));
    }
    let status = parts
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| proto(format!("bad status line: {status_line:.60}")))?;
    let mut headers = Vec::new();
    loop {
        let l = w.line(MAX_HEAD)?;
        total += l.len() + 2;
        if total > MAX_HEAD {
            return Err(proto("headers too long"));
        }
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    // 1xx (100 Continue) is never sent to a GET; treat it as a protocol error.
    if status < 200 {
        return Err(proto(format!("unexpected status {status}")));
    }
    Ok((status, headers))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    None,
    Length(u64),
    Chunked { left: u64, last: bool },
    ToClose,
}

fn framing(status: u16, headers: &[(String, String)]) -> io::Result<Framing> {
    if status == 204 || status == 304 {
        return Ok(Framing::None);
    }
    let get = |n: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.as_str());
    if let Some(te) = get("transfer-encoding") {
        if te.to_ascii_lowercase().contains("chunked") {
            return Ok(Framing::Chunked { left: 0, last: false });
        }
    }
    if let Some(cl) = get("content-length") {
        let n = cl.parse::<u64>().map_err(|_| proto("bad Content-Length"))?;
        return Ok(if n == 0 { Framing::None } else { Framing::Length(n) });
    }
    Ok(Framing::ToClose)
}

/// An answer. Drop it to discard the body (the connection is then closed, not kept).
pub struct Response<'c> {
    pub status: u16,
    headers: Vec<(String, String)>,
    body: Body,
    keep: bool,
    origin: (Scheme, String, u16),
    client: &'c Client,
    reg: Option<u64>,
}

struct Body {
    wire: Wire,
    framing: Framing,
    done: bool,
}

impl Body {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.done || out.is_empty() {
            return Ok(0);
        }
        loop {
            match self.framing {
                Framing::None => {
                    self.done = true;
                    return Ok(0);
                }
                Framing::Length(left) => {
                    let want = out.len().min(left.min(usize::MAX as u64) as usize);
                    let n = self.wire.read_some(&mut out[..want])?;
                    if n == 0 {
                        return Err(io::Error::new(ErrorKind::UnexpectedEof, "body cut short"));
                    }
                    let left = left - n as u64;
                    self.framing = Framing::Length(left);
                    if left == 0 {
                        self.done = true;
                    }
                    return Ok(n);
                }
                Framing::ToClose => {
                    let n = self.wire.read_some(out)?;
                    if n == 0 {
                        self.done = true;
                    }
                    return Ok(n);
                }
                Framing::Chunked { left, last } => {
                    if left == 0 {
                        if last {
                            // Trailers until the empty line.
                            while !self.wire.line(MAX_HEAD)?.is_empty() {}
                            self.done = true;
                            return Ok(0);
                        }
                        let l = self.wire.line(MAX_LINE)?;
                        let size = l.split(';').next().unwrap_or("").trim();
                        let n = u64::from_str_radix(size, 16).map_err(|_| proto("bad chunk size"))?;
                        self.framing = Framing::Chunked { left: n, last: n == 0 };
                        continue;
                    }
                    let want = out.len().min(left.min(usize::MAX as u64) as usize);
                    let n = self.wire.read_some(&mut out[..want])?;
                    if n == 0 {
                        return Err(io::Error::new(ErrorKind::UnexpectedEof, "chunk cut short"));
                    }
                    let left = left - n as u64;
                    if left == 0 && !self.wire.line(MAX_LINE)?.is_empty() {
                        return Err(proto("no CRLF after a chunk"));
                    }
                    self.framing = Framing::Chunked { left, last: false };
                    return Ok(n);
                }
            }
        }
    }
}

impl<'c> Response<'c> {
    /// The first header of that name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn header_u64(&self, name: &str) -> Option<u64> {
        self.header(name).and_then(|v| v.trim().parse().ok())
    }

    pub fn header_f64(&self, name: &str) -> Option<f64> {
        self.header(name).and_then(|v| v.trim().parse().ok()).filter(|v: &f64| v.is_finite())
    }

    /// Reads the whole body (at most `max` bytes), then keeps the connection for the next
    /// request to this origin when the server allows it.
    pub fn bytes(mut self, max: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        if let Framing::Length(n) = self.body.framing {
            if n > max as u64 {
                return Err(proto(format!("answer too big ({n} bytes)")));
            }
            out.reserve(n as usize);
        }
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = self.body.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if out.len() + n > max {
                return Err(proto("answer too big"));
            }
            out.extend_from_slice(&buf[..n]);
        }
        self.finish();
        Ok(out)
    }

    /// The body as a stream (endless audio). The connection is not kept afterwards.
    pub fn into_reader(mut self) -> BodyReader<'c> {
        self.keep = false;
        let body = std::mem::replace(
            &mut self.body,
            Body { wire: Wire::new(Box::new(Closed)), framing: Framing::None, done: true },
        );
        BodyReader { body, client: self.client, reg: self.reg.take() }
    }

    fn finish(&mut self) {
        self.client.cancel.unregister(self.reg.take());
        if self.keep && self.body.done && self.body.wire.avail().is_empty() {
            let wire = std::mem::replace(&mut self.body.wire, Wire::new(Box::new(Closed)));
            self.client.put_idle(self.origin.clone(), wire.conn);
        }
    }
}

impl Drop for Response<'_> {
    fn drop(&mut self) {
        self.client.cancel.unregister(self.reg.take());
    }
}

/// A streaming body (endless audio). Drop closes the connection.
pub struct BodyReader<'c> {
    body: Body,
    client: &'c Client,
    reg: Option<u64>,
}

impl Read for BodyReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.body.read(out)
    }
}

impl Drop for BodyReader<'_> {
    fn drop(&mut self) {
        self.client.cancel.unregister(self.reg.take());
    }
}

/// A placeholder connection (moved-out bodies).
struct Closed;

impl Read for Closed {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }
}

impl Write for Closed {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(ErrorKind::NotConnected, "closed"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Conn for Closed {
    fn set_timeouts(&self, _: Duration) -> io::Result<()> {
        Ok(())
    }
    fn aborter(&self) -> Option<Aborter> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn url_parse_join_query() {
        let u = Url::parse("http://radio.example:8080/api/v1/stations").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.target.as_str()), ("radio.example", 8080, "/api/v1/stations"));
        assert_eq!(u.authority(), "radio.example:8080");
        let a = u.join("http://radio.example:8080/api/v1/audio/hardstyle/flac").unwrap();
        assert_eq!(a.path(), "/api/v1/audio/hardstyle/flac");
        assert_eq!(u.join("/x?y=1").unwrap().target, "/x?y=1");
        assert_eq!(u.join("now/a").unwrap().target, "/api/v1/now/a");
        let q = a.with_query(&[("cursor", String::new()), ("cid", "ab c".into()), ("min_s", "1.5".into())]);
        assert_eq!(q.target, "/api/v1/audio/hardstyle/flac?cursor=&cid=ab%20c&min_s=1.5");
        assert_eq!(q.to_string(), "http://radio.example:8080/api/v1/audio/hardstyle/flac?cursor=&cid=ab%20c&min_s=1.5");
        assert_eq!(Url::parse("HTTP://Example.org").unwrap().target, "/");
        assert_eq!(Url::parse("http://h?x=1").unwrap().target, "/?x=1");
        assert_eq!(Url::parse("https://h/p").unwrap().port, 443);
        assert!(Url::parse("ftp://h/").is_err());
        assert!(Url::parse("http://:80/").is_err());
        assert!(Url::parse("http://u@h/").is_err());
    }

    /// A one-shot server: answers each accepted connection with the canned bytes in order.
    fn serve(answers: Vec<Vec<Vec<u8>>>) -> (Url, thread::JoinHandle<Vec<String>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = thread::spawn(move || {
            let mut reqs = Vec::new();
            for conn_answers in answers {
                let (mut s, _) = l.accept().unwrap();
                for a in conn_answers {
                    let mut buf = Vec::new();
                    let mut b = [0u8; 1];
                    while !buf.ends_with(b"\r\n\r\n") {
                        if s.read(&mut b).unwrap() == 0 {
                            return reqs;
                        }
                        buf.push(b[0]);
                    }
                    reqs.push(String::from_utf8_lossy(&buf).into_owned());
                    s.write_all(&a).unwrap();
                }
            }
            reqs
        });
        (Url::parse(&format!("http://127.0.0.1:{port}/a")).unwrap(), h)
    }

    fn client() -> Client {
        Client::new(Arc::new(TcpTransport::default()), Cancel::default())
    }

    #[test]
    fn keep_alive_lengths_and_chunks() {
        let (u, h) = serve(vec![vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-Stream-Cursor: c1\r\n\r\nhello".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n".to_vec(),
            b"HTTP/1.1 204 No Content\r\nX-Stream-Cursor: c2\r\n\r\n".to_vec(),
        ]]);
        let c = client();
        let r = c.get(&u, &[], Timeouts::io_s(5)).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.header("x-stream-cursor"), Some("c1"));
        assert_eq!(r.bytes(100).unwrap(), b"hello");
        let r = c.get(&u.join("/b").unwrap(), &[("X-A", "1")], Timeouts::io_s(5)).unwrap();
        assert_eq!(r.bytes(100).unwrap(), b"abcde");
        let r = c.get(&u, &[], Timeouts::io_s(5)).unwrap();
        assert_eq!(r.status, 204);
        assert_eq!(r.header("X-STREAM-CURSOR"), Some("c2"));
        assert!(r.bytes(10).unwrap().is_empty());
        let reqs = h.join().unwrap();
        assert_eq!(reqs.len(), 3, "all three on one connection");
        assert!(reqs[1].starts_with("GET /b HTTP/1.1\r\n"));
        assert!(reqs[1].contains("X-A: 1\r\n"));
        assert!(reqs[0].contains("Host: 127.0.0.1:"));
    }

    #[test]
    fn stale_kept_connection_is_retried_once() {
        // First connection answers once, then closes; the second request must reconnect.
        let (u, h) = serve(vec![
            vec![b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nx".to_vec()],
            vec![b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\ny".to_vec()],
        ]);
        let c = client();
        assert_eq!(c.get(&u, &[], Timeouts::io_s(5)).unwrap().bytes(10).unwrap(), b"x");
        thread::sleep(Duration::from_millis(50));
        assert_eq!(c.get(&u, &[], Timeouts::io_s(5)).unwrap().bytes(10).unwrap(), b"y");
        assert_eq!(h.join().unwrap().len(), 2);
    }

    #[test]
    fn read_to_close_stream_and_limits() {
        let (u, h) = serve(vec![
            vec![b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nstreaming body".to_vec()],
            vec![b"HTTP/1.1 503 Busy\r\nRetry-After: 7\r\nContent-Length: 20\r\n\r\n{\"error\":\"not now\"}\n".to_vec()],
        ]);
        let c = client();
        let r = c.get(&u, &[], Timeouts::io_s(5)).unwrap();
        let mut rd = r.into_reader();
        let mut s = String::new();
        rd.read_to_string(&mut s).unwrap();
        assert_eq!(s, "streaming body");
        drop(rd);
        let r = c.get(&u, &[], Timeouts::io_s(5)).unwrap();
        assert_eq!((r.status, r.header_f64("retry-after")), (503, Some(7.0)));
        assert!(r.bytes(5).is_err(), "over the limit");
        h.join().unwrap();
    }

    #[test]
    fn allowlist_refuses_other_origins() {
        let c = client().with_allow(vec!["http://radio.example:8080".into(), "HTTP://Example.org".into()]);
        assert!(c.allows(&Url::parse("http://radio.example:8080/api/v1/now/x").unwrap()));
        assert!(c.allows(&Url::parse("http://example.org:80/a").unwrap()));
        assert!(!c.allows(&Url::parse("http://radio.example:8081/").unwrap()));
        assert!(!c.allows(&Url::parse("https://radio.example:8080/").unwrap()));
        assert!(!c.allows(&Url::parse("http://evil.example/").unwrap()));
        let e = c.get(&Url::parse("http://127.0.0.1:9/").unwrap(), &[], Timeouts::io_s(1)).err().unwrap();
        assert_eq!(e.kind(), ErrorKind::PermissionDenied);
        assert_eq!(Url::parse("http://H:80/x").unwrap().origin_text(), "http://h");
    }

    #[test]
    fn cancel_aborts_a_blocked_request() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            thread::sleep(Duration::from_millis(1500));
            drop(s);
        });
        let c = Arc::new(client());
        let c2 = c.clone();
        let t = thread::spawn(move || {
            let u = Url::parse(&format!("http://127.0.0.1:{port}/slow")).unwrap();
            let start = std::time::Instant::now();
            let r = c2.get(&u, &[], Timeouts::io_s(30));
            (r.is_err(), start.elapsed())
        });
        thread::sleep(Duration::from_millis(200));
        c.cancel.cancel();
        let (err, took) = t.join().unwrap();
        assert!(err);
        assert!(took < Duration::from_millis(1200), "{took:?}");
        assert!(c.get(&Url::parse("http://127.0.0.1:1/").unwrap(), &[], Timeouts::io_s(1)).is_err());
        h.join().unwrap();
    }
}
