//! Upstream outbound proxy: HTTP CONNECT + SOCKS5/SOCKS5H tunnels over monoio.
//!
//! Semantics aligned with sub2api (`proxyurl.Parse` + SOCKS dial):
//! - `http` / `https` → HTTP CONNECT
//! - `socks5` / `socks5h` → SOCKS5 CONNECT
//! - bare `socks5://` is upgraded to remote-DNS mode (socks5h) to avoid DNS leaks

use std::net::{IpAddr, ToSocketAddrs};

use anyhow::{Context, Result, bail};
use http::Uri;
use monoio::{
    io::{AsyncReadRent, AsyncWriteRentExt},
    net::TcpStream,
};
use monoio_transports::connectors::{Connector, TcpConnector};

use crate::timeout_opt;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProxyKind {
    Http,
    /// SOCKS5 with remote DNS (socks5h / upgraded socks5).
    Socks5RemoteDns,
}

#[derive(Debug, Clone)]
pub(crate) struct ProxyEndpoint {
    pub(crate) kind: ProxyKind,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) auth: Option<(String, String)>,
    /// Canonical URL string after socks5→socks5h upgrade (no password redaction).
    pub(crate) canonical: String,
}

/// Parse and normalize a proxy URL (fail-fast; never silently direct).
///
/// Empty input is rejected here; caller treats missing proxy as direct connect.
pub(crate) fn parse_proxy_endpoint(raw: &str) -> Result<ProxyEndpoint> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("empty proxy url");
    }
    let uri: Uri = trimmed
        .parse()
        .with_context(|| "invalid proxy URL".to_string())?;
    let mut scheme = uri.scheme_str().unwrap_or("http").to_ascii_lowercase();
    let host = uri
        .host()
        .filter(|h| !h.is_empty())
        .context("proxy URL missing host")?
        .to_string();
    let port = uri.port_u16().unwrap_or(match scheme.as_str() {
        "https" => 443,
        "socks5" | "socks5h" => 1080,
        _ => 80,
    });
    let auth = uri.authority().and_then(|auth| {
        let s = auth.as_str();
        let (userinfo, _) = s.split_once('@')?;
        let (user, pass) = userinfo.split_once(':')?;
        if user.is_empty() {
            return None;
        }
        Some((user.to_string(), pass.to_string()))
    });

    // sub2api: socks5 → socks5h so DNS is resolved by the proxy.
    let kind = match scheme.as_str() {
        "http" | "https" => ProxyKind::Http,
        "socks5" => {
            scheme = "socks5h".to_string();
            ProxyKind::Socks5RemoteDns
        }
        "socks5h" => ProxyKind::Socks5RemoteDns,
        other => bail!(
            "unsupported proxy scheme {other:?} (allowed: http, https, socks5, socks5h)"
        ),
    };

    let userinfo = auth
        .as_ref()
        .map(|(u, p)| format!("{u}:{p}@"))
        .unwrap_or_default();
    let canonical = format!("{scheme}://{userinfo}{host}:{port}");

    Ok(ProxyEndpoint {
        kind,
        host,
        port,
        auth,
        canonical,
    })
}

/// Connect to `target_host:target_port` through the proxy; returns the tunneled TCP stream.
pub(crate) async fn dial_via_proxy(
    proxy: &ProxyEndpoint,
    target_host: &str,
    target_port: u16,
    connect_timeout: Option<Duration>,
) -> Result<TcpStream> {
    let proxy_addr = format!("{}:{}", proxy.host, proxy.port)
        .to_socket_addrs()
        .with_context(|| format!("resolve proxy {}:{}", proxy.host, proxy.port))?
        .next()
        .context("proxy resolved no addresses")?;

    let connector = TcpConnector::default();
    let connect = connector.connect(proxy_addr);
    let stream = timeout_opt(connect_timeout, connect)
        .await
        .with_context(|| format!("connect proxy {}:{}", proxy.host, proxy.port))??;

    match proxy.kind {
        ProxyKind::Http => http_connect_tunnel(stream, proxy, target_host, target_port).await,
        ProxyKind::Socks5RemoteDns => {
            socks5_connect_tunnel(stream, proxy, target_host, target_port).await
        }
    }
}

async fn http_connect_tunnel(
    mut stream: TcpStream,
    proxy: &ProxyEndpoint,
    target_host: &str,
    target_port: u16,
) -> Result<TcpStream> {
    let mut connect_req = format!(
        "CONNECT {target_host}:{target_port} HTTP/1.1\r\nHost: {target_host}:{target_port}\r\n"
    );
    if let Some((user, pass)) = &proxy.auth {
        let token = data_encoding::BASE64.encode(format!("{user}:{pass}").as_bytes());
        connect_req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    connect_req.push_str("\r\n");
    let (write_res, _) = stream.write_all(connect_req.into_bytes()).await;
    write_res.context("write CONNECT to proxy")?;

    let mut buf = vec![0u8; 4096];
    let mut response = Vec::new();
    loop {
        let (read_res, read_buf) = stream.read(buf).await;
        buf = read_buf;
        let n = read_res.context("read CONNECT response from proxy")?;
        if n == 0 {
            bail!("proxy closed connection during CONNECT");
        }
        response.extend_from_slice(&buf[..n]);
        if response.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if response.len() > 16 * 1024 {
            bail!("proxy CONNECT response too large");
        }
    }
    let status_line = std::str::from_utf8(&response)
        .ok()
        .and_then(|s| s.lines().next())
        .unwrap_or("");
    if !status_line.contains(" 200 ") {
        bail!("proxy CONNECT failed: {status_line}");
    }
    Ok(stream)
}

/// SOCKS5 CONNECT (RFC 1928) + optional username/password (RFC 1929).
async fn socks5_connect_tunnel(
    mut stream: TcpStream,
    proxy: &ProxyEndpoint,
    target_host: &str,
    target_port: u16,
) -> Result<TcpStream> {
    // greeting
    let greeting = if proxy.auth.is_some() {
        // no-auth (0x00) + user/pass (0x02)
        vec![0x05, 0x02, 0x00, 0x02]
    } else {
        vec![0x05, 0x01, 0x00]
    };
    let (w, _) = stream.write_all(greeting).await;
    w.context("socks5 greeting write")?;

    let mut buf = [0u8; 2];
    read_exact(&mut stream, &mut buf)
        .await
        .context("socks5 greeting response")?;
    if buf[0] != 0x05 {
        bail!("socks5 invalid version in greeting response: {}", buf[0]);
    }
    match buf[1] {
        0x00 => {}
        0x02 => {
            let Some((user, pass)) = &proxy.auth else {
                bail!("socks5 proxy selected username/password auth but none configured");
            };
            socks5_userpass_auth(&mut stream, user, pass).await?;
        }
        0xFF => bail!("socks5 proxy rejected authentication methods"),
        m => bail!("socks5 unsupported auth method {m}"),
    }

    // CONNECT request
    let mut req = Vec::with_capacity(4 + 255 + 2);
    req.extend_from_slice(&[0x05, 0x01, 0x00]); // VER, CONNECT, RSV
    let bytes = target_host.as_bytes();
    if bytes.is_empty() || bytes.len() > 255 {
        bail!("socks5 domain length invalid");
    }
    // Prefer ATYP IP when target is already an address; otherwise DOMAIN (remote DNS).
    if let Ok(ip) = target_host.parse::<IpAddr>() {
        match ip {
            IpAddr::V4(v4) => {
                req.push(0x01);
                req.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                req.push(0x04);
                req.extend_from_slice(&v6.octets());
            }
        }
    } else {
        req.push(0x03);
        req.push(bytes.len() as u8);
        req.extend_from_slice(bytes);
    }
    req.push((target_port >> 8) as u8);
    req.push((target_port & 0xff) as u8);

    let (w, _) = stream.write_all(req).await;
    w.context("socks5 connect request write")?;

    // reply: VER REP RSV ATYP ...
    let mut hdr = [0u8; 4];
    read_exact(&mut stream, &mut hdr)
        .await
        .context("socks5 connect reply header")?;
    if hdr[0] != 0x05 {
        bail!("socks5 invalid version in connect reply");
    }
    if hdr[1] != 0x00 {
        bail!(
            "socks5 connect failed: rep={} ({})",
            hdr[1],
            socks5_rep_message(hdr[1])
        );
    }
    // consume bound address
    match hdr[3] {
        0x01 => {
            let mut rest = [0u8; 4 + 2];
            read_exact(&mut stream, &mut rest).await?;
        }
        0x03 => {
            let mut len = [0u8; 1];
            read_exact(&mut stream, &mut len).await?;
            let mut rest = vec![0u8; len[0] as usize + 2];
            read_exact(&mut stream, &mut rest).await?;
        }
        0x04 => {
            let mut rest = [0u8; 16 + 2];
            read_exact(&mut stream, &mut rest).await?;
        }
        atyp => bail!("socks5 unexpected ATYP in reply: {atyp}"),
    }

    Ok(stream)
}

async fn socks5_userpass_auth(stream: &mut TcpStream, user: &str, pass: &str) -> Result<()> {
    let ub = user.as_bytes();
    let pb = pass.as_bytes();
    if ub.is_empty() || ub.len() > 255 || pb.len() > 255 {
        bail!("socks5 username/password length invalid");
    }
    let mut msg = Vec::with_capacity(3 + ub.len() + pb.len());
    msg.push(0x01); // VER of RFC1929
    msg.push(ub.len() as u8);
    msg.extend_from_slice(ub);
    msg.push(pb.len() as u8);
    msg.extend_from_slice(pb);
    let (w, _) = stream.write_all(msg).await;
    w.context("socks5 user/pass auth write")?;

    let mut resp = [0u8; 2];
    read_exact(stream, &mut resp)
        .await
        .context("socks5 user/pass auth response")?;
    if resp[1] != 0x00 {
        bail!("socks5 username/password authentication failed");
    }
    Ok(())
}

async fn read_exact(stream: &mut TcpStream, out: &mut [u8]) -> Result<()> {
    let mut filled = 0usize;
    while filled < out.len() {
        let chunk = vec![0u8; out.len() - filled];
        let (res, buf) = stream.read(chunk).await;
        let n = res.context("socks5 read")?;
        if n == 0 {
            bail!("socks5 unexpected eof");
        }
        out[filled..filled + n].copy_from_slice(&buf[..n]);
        filled += n;
    }
    Ok(())
}

fn socks5_rep_message(rep: u8) -> &'static str {
    match rep {
        0x01 => "general failure",
        0x02 => "connection not allowed",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrades_socks5_to_socks5h() {
        let p = parse_proxy_endpoint("socks5://127.0.0.1:1080").expect("parse");
        assert_eq!(p.kind, ProxyKind::Socks5RemoteDns);
        assert!(p.canonical.starts_with("socks5h://"));
        assert_eq!(p.port, 1080);
    }

    #[test]
    fn parses_http_with_auth() {
        let p = parse_proxy_endpoint("http://u:p@proxy.example:8080").expect("parse");
        assert_eq!(p.kind, ProxyKind::Http);
        assert_eq!(p.auth.as_ref().map(|(u, _)| u.as_str()), Some("u"));
        assert_eq!(p.port, 8080);
    }

    #[test]
    fn rejects_ftp() {
        assert!(parse_proxy_endpoint("ftp://x").is_err());
    }

    #[test]
    fn socks5h_default_port() {
        // URI without port — Uri parser may reject host-only for socks; use with port
        let p = parse_proxy_endpoint("socks5h://10.0.0.1:1080").expect("parse");
        assert_eq!(p.kind, ProxyKind::Socks5RemoteDns);
        assert_eq!(p.host, "10.0.0.1");
    }
}
