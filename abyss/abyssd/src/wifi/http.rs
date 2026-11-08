//! A tiny HTTP/1.1 GET for the Wi-Fi test (status line only; no TLS, no redirects).

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::os::fd::{FromRawFd, OwnedFd};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use super::dns::bind_to_device;

/// "http://host[:port]/path" → (host, port, path).
pub fn split_url(url: &str) -> Result<(String, u16, String)> {
    let rest = url.strip_prefix("http://").ok_or_else(|| anyhow!("only http:// URLs: {url}"))?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().context("bad port")?),
        None => (hostport, 80),
    };
    if host.is_empty() {
        bail!("no host in {url}");
    }
    Ok((host.to_string(), port, path.to_string()))
}

/// TCP connect bound to `ifname` (SO_BINDTODEVICE before connect), with timeouts.
fn connect(ip: Ipv4Addr, port: u16, ifname: &str, timeout: Duration) -> Result<TcpStream> {
    // SAFETY: plain socket(2); the fd is owned right below.
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        bail!("tcp socket: {}", std::io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh fd owned by nobody else.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    bind_to_device(&fd, ifname)?;
    let s = TcpStream::from(fd);
    // A blocking connect(2) honours SO_SNDTIMEO on Linux.
    s.set_write_timeout(Some(timeout))?;
    s.set_read_timeout(Some(timeout))?;
    let addr = SocketAddrV4::new(ip, port);
    // SAFETY: an all-zero sockaddr_in is valid; the fields are set below.
    let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = port.to_be();
    sin.sin_addr = libc::in_addr { s_addr: u32::from(*addr.ip()).to_be() };
    // SAFETY: `sin` is a valid sockaddr_in of the size given, on our own socket.
    let r = unsafe {
        libc::connect(
            std::os::fd::AsRawFd::as_raw_fd(&s),
            &sin as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if r < 0 {
        bail!("connect {addr}: {}", std::io::Error::last_os_error());
    }
    Ok(s)
}

/// GET `path` from `host` at `ip`; returns the HTTP status code.
pub fn get_status(ip: Ipv4Addr, port: u16, host: &str, path: &str, ifname: &str) -> Result<u16> {
    let mut s = connect(ip, port, ifname, Duration::from_secs(5))?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: abyssd\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).context("http send")?;
    let mut buf = [0u8; 512];
    let mut got = 0;
    // The status line is all we need.
    while got < buf.len() {
        let n = s.read(&mut buf[got..]).context("http read")?;
        if n == 0 {
            break;
        }
        got += n;
        if buf[..got].windows(2).any(|w| w == b"\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf[..got]);
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next().and_then(|c| c.parse::<u16>().ok())) {
        (Some(v), Some(code)) if v.starts_with("HTTP/") => Ok(code),
        _ => bail!("bad HTTP status line {line:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let (h, p, path) = split_url("http://connectivitycheck.gstatic.com/generate_204").unwrap();
        assert_eq!((h.as_str(), p, path.as_str()), ("connectivitycheck.gstatic.com", 80, "/generate_204"));
        assert!(split_url("https://x/").is_err());
        assert_eq!(split_url("http://1.2.3.4:8080").unwrap().2, "/");
    }
}
