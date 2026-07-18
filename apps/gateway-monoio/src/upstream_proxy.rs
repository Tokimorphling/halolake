//! Upstream outbound proxy transport for monoio workers.
//!
//! HTTP, HTTPS, SOCKS5 and SOCKS5H proxy URLs are normalized into a typed
//! [`ProxyEndpoint`]. HTTPS proxies establish TLS to the proxy before issuing
//! CONNECT; HTTPS targets establish a second TLS layer inside that tunnel.
//! Response headers are returned immediately while a worker-local task pumps
//! the HTTP/1 body into the gateway body type.
//!
//! ## Key Components
//!
//! - [`ProxyTransportService`]: typed worker-local transport boundary.
//! - [`ProxyTransportRequest`]: channel, proxy and target-scoped request context.
//! - [`ProxyEndpoint`]: normalized proxy configuration with redacted diagnostics.
//!
//! ## Failure Isolation
//!
//! A short circuit breaker is scoped by channel, proxy identity and target
//! authority. DNS results are tried sequentially within one total connect
//! budget, so a stale address does not force a channel offline.

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http::{Request, Response, Uri};
use monoio::{
    io::{AsyncReadRent, AsyncWriteRent, AsyncWriteRentExt, Split, sink::SinkExt, stream::Stream},
    net::TcpStream,
};
use monoio_http::{
    common::{
        body::{Body as MonoioBody, HttpBody},
        error::HttpError,
    },
    h1::{
        codec::{ClientCodec, decoder::PayloadDecoder},
        payload::{Payload, fixed_payload_pair, stream_payload_pair},
    },
};
use monoio_transports::connectors::{Connector, TcpConnector};
use service_async::Service;
use std::{
    cell::RefCell,
    collections::HashMap,
    fmt, io,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    rc::Rc,
    time::{Duration, Instant},
};
use tracing::{debug, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ProxyKind {
    Http,
    Https,
    /// SOCKS5 with remote DNS (socks5h / upgraded socks5).
    Socks5RemoteDns,
}

/// Worker-local policy for rejecting repeated failures on one channel's proxy route.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProxyCircuitPolicy {
    failure_threshold: u32,
    cooldown: Duration,
}

impl ProxyCircuitPolicy {
    pub(crate) fn new(failure_threshold: u32, cooldown: Duration) -> Self {
        Self {
            failure_threshold,
            cooldown,
        }
    }
}

/// Typed request accepted by [`ProxyTransportService`].
pub(crate) struct ProxyTransportRequest {
    pub(crate) channel_identity: String,
    pub(crate) proxy: ProxyEndpoint,
    pub(crate) target_host: String,
    pub(crate) target_port: u16,
    pub(crate) target_tls: bool,
    pub(crate) request: Request<HttpBody>,
}

/// Errors produced before an upstream response head is available.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ProxyTransportError {
    #[error("proxy circuit is open; retry after {retry_after_ms} ms")]
    CircuitOpen { retry_after_ms: u64 },
    #[error("proxy transport failed: {0}")]
    Transport(#[source] anyhow::Error),
}

/// Per-worker proxy transport with route-scoped failure isolation.
///
/// Clones share only worker-local circuit state. The key includes channel ID,
/// proxy credentials and target authority, so a broken auth-file channel cannot
/// suppress another channel even when both use the same proxy and upstream.
#[derive(Clone)]
pub(crate) struct ProxyTransportService {
    circuit: ProxyCircuitBreaker,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
}

impl ProxyTransportService {
    pub(crate) fn new(
        connect_timeout: Option<Duration>,
        read_timeout: Option<Duration>,
        circuit_policy: ProxyCircuitPolicy,
    ) -> Self {
        Self {
            circuit: ProxyCircuitBreaker::new(circuit_policy),
            connect_timeout,
            read_timeout,
        }
    }
}

impl Service<ProxyTransportRequest> for ProxyTransportService {
    type Response = Response<HttpBody>;
    type Error = ProxyTransportError;

    async fn call(&self, req: ProxyTransportRequest) -> Result<Self::Response, Self::Error> {
        let ProxyTransportRequest {
            channel_identity,
            proxy,
            target_host,
            target_port,
            target_tls,
            request,
        } = req;
        let proxy_label = proxy.redacted_label();
        let permit = self
            .circuit
            .acquire(
                &channel_identity,
                &proxy,
                &target_host,
                target_port,
                target_tls,
            )
            .map_err(|rejection| {
                let retry_after_ms = duration_millis(rejection.retry_after);
                debug!(
                    %channel_identity,
                    proxy = %proxy_label,
                    %target_host,
                    target_port,
                    retry_after_ms,
                    outcome = "circuit_open",
                    "proxy transport rejected request"
                );
                ProxyTransportError::CircuitOpen { retry_after_ms }
            })?;
        let started = Instant::now();
        let result = send_request_via_proxy(
            &proxy,
            &target_host,
            target_port,
            target_tls,
            request,
            self.connect_timeout,
            self.read_timeout,
        )
        .await;

        match result {
            Ok(response) => {
                let recovered = permit.success();
                debug!(
                    %channel_identity,
                    proxy = %proxy_label,
                    %target_host,
                    target_port,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    recovered,
                    outcome = "response_head",
                    "proxy transport completed"
                );
                Ok(response)
            }
            Err(error) => {
                let update = permit.failure();
                warn!(
                    %channel_identity,
                    proxy = %proxy_label,
                    %target_host,
                    target_port,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    consecutive_failures = update.consecutive_failures,
                    circuit_opened = update.opened,
                    error = %error,
                    outcome = "transport_error",
                    "proxy transport failed"
                );
                Err(ProxyTransportError::Transport(error))
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ProxyCircuitKey {
    channel_identity: String,
    kind: ProxyKind,
    proxy_host: String,
    proxy_port: u16,
    proxy_auth: Option<(String, String)>,
    target_host: String,
    target_port: u16,
    target_tls: bool,
}

impl ProxyCircuitKey {
    fn new(
        channel_identity: &str,
        proxy: &ProxyEndpoint,
        target_host: &str,
        target_port: u16,
        target_tls: bool,
    ) -> Self {
        Self {
            channel_identity: channel_identity.to_string(),
            kind: proxy.kind,
            proxy_host: proxy.host.clone(),
            proxy_port: proxy.port,
            proxy_auth: proxy.auth.clone(),
            target_host: target_host.to_string(),
            target_port,
            target_tls,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ProxyCircuitState {
    Closed { consecutive_failures: u32 },
    Open { retry_at: Instant },
    HalfOpen,
}

#[derive(Clone)]
struct ProxyCircuitBreaker {
    policy: ProxyCircuitPolicy,
    states: Rc<RefCell<HashMap<ProxyCircuitKey, ProxyCircuitState>>>,
}

impl ProxyCircuitBreaker {
    fn new(policy: ProxyCircuitPolicy) -> Self {
        Self {
            policy,
            states: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    fn acquire(
        &self,
        channel_identity: &str,
        proxy: &ProxyEndpoint,
        target_host: &str,
        target_port: u16,
        target_tls: bool,
    ) -> Result<ProxyCircuitPermit, ProxyCircuitRejection> {
        if self.policy.failure_threshold == 0 {
            return Ok(ProxyCircuitPermit::disabled());
        }

        let key = ProxyCircuitKey::new(
            channel_identity,
            proxy,
            target_host,
            target_port,
            target_tls,
        );
        let now = Instant::now();
        let mut states = self.states.borrow_mut();
        match states.get(&key).copied() {
            Some(ProxyCircuitState::Open { retry_at }) if retry_at > now => {
                Err(ProxyCircuitRejection {
                    retry_after: retry_at.duration_since(now),
                })
            }
            Some(ProxyCircuitState::Open { .. }) => {
                states.insert(key.clone(), ProxyCircuitState::HalfOpen);
                Ok(ProxyCircuitPermit::armed(self.clone(), key))
            }
            Some(ProxyCircuitState::HalfOpen) => Err(ProxyCircuitRejection {
                retry_after: self.policy.cooldown,
            }),
            Some(ProxyCircuitState::Closed { .. }) | None => {
                Ok(ProxyCircuitPermit::armed(self.clone(), key))
            }
        }
    }

    fn record_success(&self, key: &ProxyCircuitKey) -> bool {
        self.states.borrow_mut().remove(key).is_some()
    }

    fn record_failure(&self, key: &ProxyCircuitKey) -> ProxyCircuitFailureUpdate {
        let mut states = self.states.borrow_mut();
        let previous = states.get(key).copied();
        let consecutive_failures = match previous {
            Some(ProxyCircuitState::Closed {
                consecutive_failures,
            }) => consecutive_failures.saturating_add(1),
            Some(ProxyCircuitState::HalfOpen) => self.policy.failure_threshold,
            Some(ProxyCircuitState::Open { .. }) => self.policy.failure_threshold,
            None => 1,
        };
        let opened = consecutive_failures >= self.policy.failure_threshold;
        let next = if opened {
            ProxyCircuitState::Open {
                retry_at: Instant::now() + self.policy.cooldown,
            }
        } else {
            ProxyCircuitState::Closed {
                consecutive_failures,
            }
        };
        states.insert(key.clone(), next);
        ProxyCircuitFailureUpdate {
            consecutive_failures,
            opened,
        }
    }
}

struct ProxyCircuitPermit {
    breaker: Option<ProxyCircuitBreaker>,
    key: Option<ProxyCircuitKey>,
    armed: bool,
}

impl ProxyCircuitPermit {
    fn disabled() -> Self {
        Self {
            breaker: None,
            key: None,
            armed: false,
        }
    }

    fn armed(breaker: ProxyCircuitBreaker, key: ProxyCircuitKey) -> Self {
        Self {
            breaker: Some(breaker),
            key: Some(key),
            armed: true,
        }
    }

    fn success(mut self) -> bool {
        self.armed = false;
        match (&self.breaker, &self.key) {
            (Some(breaker), Some(key)) => breaker.record_success(key),
            _ => false,
        }
    }

    fn failure(mut self) -> ProxyCircuitFailureUpdate {
        self.armed = false;
        match (&self.breaker, &self.key) {
            (Some(breaker), Some(key)) => breaker.record_failure(key),
            _ => ProxyCircuitFailureUpdate::default(),
        }
    }
}

impl Drop for ProxyCircuitPermit {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let (Some(breaker), Some(key)) = (&self.breaker, &self.key) {
            breaker.record_failure(key);
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ProxyCircuitFailureUpdate {
    consecutive_failures: u32,
    opened: bool,
}

#[derive(Debug, Clone, Copy)]
struct ProxyCircuitRejection {
    retry_after: Duration,
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

#[derive(Clone)]
pub(crate) struct ProxyEndpoint {
    pub(crate) kind: ProxyKind,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) auth: Option<(String, String)>,
}

impl ProxyEndpoint {
    /// A credential-free label suitable for structured logs and diagnostics.
    pub(crate) fn redacted_label(&self) -> String {
        let scheme = match self.kind {
            ProxyKind::Http => "http",
            ProxyKind::Https => "https",
            ProxyKind::Socks5RemoteDns => "socks5h",
        };
        let host = if self.host.parse::<std::net::Ipv6Addr>().is_ok() {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let credentials = self.auth.as_ref().map(|_| "<redacted>@").unwrap_or("");
        format!("{scheme}://{credentials}{host}:{}", self.port)
    }
}

impl fmt::Debug for ProxyEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyEndpoint")
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("has_auth", &self.auth.is_some())
            .finish()
    }
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
    let scheme = uri.scheme_str().unwrap_or("http").to_ascii_lowercase();
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

    // Treat socks5 as socks5h so the proxy resolves target hostnames and the
    // gateway never leaks target DNS lookups.
    let kind = match scheme.as_str() {
        "http" => ProxyKind::Http,
        "https" => ProxyKind::Https,
        "socks5" => ProxyKind::Socks5RemoteDns,
        "socks5h" => ProxyKind::Socks5RemoteDns,
        other => {
            bail!("unsupported proxy scheme {other:?} (allowed: http, https, socks5, socks5h)")
        }
    };

    Ok(ProxyEndpoint {
        kind,
        host,
        port,
        auth,
    })
}

/// Send one HTTP/1 request through a required outbound proxy.
pub(crate) async fn send_request_via_proxy(
    proxy: &ProxyEndpoint,
    target_host: &str,
    target_port: u16,
    target_tls: bool,
    req: Request<HttpBody>,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
) -> Result<Response<HttpBody>> {
    let stream = connect_proxy_tcp(proxy, connect_timeout).await?;

    match proxy.kind {
        ProxyKind::Http => {
            let tunnel = timeout_stage(
                connect_timeout,
                "HTTP proxy CONNECT handshake",
                http_connect_tunnel(stream, proxy, target_host, target_port),
            )
            .await??;
            send_over_tunnel(
                tunnel,
                target_host,
                target_tls,
                req,
                connect_timeout,
                read_timeout,
            )
            .await
        }
        ProxyKind::Https => {
            let connector = tls_connector()?;
            let proxy_tls = timeout_stage(
                connect_timeout,
                "TLS handshake with HTTPS proxy",
                connector.connect(&proxy.host, stream),
            )
            .await?
            .with_context(|| format!("TLS handshake with HTTPS proxy {}", proxy.host))?;
            let tunnel = timeout_stage(
                connect_timeout,
                "HTTPS proxy CONNECT handshake",
                http_connect_tunnel(proxy_tls, proxy, target_host, target_port),
            )
            .await??;
            send_over_tunnel(
                tunnel,
                target_host,
                target_tls,
                req,
                connect_timeout,
                read_timeout,
            )
            .await
        }
        ProxyKind::Socks5RemoteDns => {
            let tunnel = timeout_stage(
                connect_timeout,
                "SOCKS5 proxy handshake",
                socks5_connect_tunnel(stream, proxy, target_host, target_port),
            )
            .await??;
            send_over_tunnel(
                tunnel,
                target_host,
                target_tls,
                req,
                connect_timeout,
                read_timeout,
            )
            .await
        }
    }
}

async fn connect_proxy_tcp(
    proxy: &ProxyEndpoint,
    connect_timeout: Option<Duration>,
) -> Result<TcpStream> {
    let proxy_addrs = (proxy.host.as_str(), proxy.port)
        .to_socket_addrs()
        .with_context(|| format!("resolve proxy {}:{}", proxy.host, proxy.port))?
        .collect::<Vec<_>>();
    if proxy_addrs.is_empty() {
        bail!("proxy resolved no addresses");
    }

    connect_proxy_addrs(proxy_addrs, connect_timeout)
        .await
        .with_context(|| format!("connect proxy {}:{}", proxy.host, proxy.port))
}

async fn connect_proxy_addrs(
    proxy_addrs: Vec<SocketAddr>,
    connect_timeout: Option<Duration>,
) -> Result<TcpStream> {
    let started = Instant::now();
    let address_count = proxy_addrs.len();
    let connector = TcpConnector::default();
    let mut last_error = None;

    for (index, proxy_addr) in proxy_addrs.into_iter().enumerate() {
        let attempts_left = address_count - index;
        let attempt_timeout = connect_timeout.map(|budget| {
            let remaining = budget.saturating_sub(started.elapsed());
            remaining
                .checked_div(attempts_left as u32)
                .unwrap_or(remaining)
        });
        if attempt_timeout.is_some_and(|duration| duration.is_zero()) {
            break;
        }

        match timeout_stage(
            attempt_timeout,
            "connect proxy TCP address",
            connector.connect(proxy_addr),
        )
        .await
        {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(error)) => {
                last_error = Some(anyhow::anyhow!("{proxy_addr}: {error}"));
            }
            Err(error) => {
                last_error = Some(error.context(format!("address {proxy_addr}")));
            }
        }
    }

    match last_error {
        Some(error) => Err(error)
            .with_context(|| format!("all {address_count} resolved proxy addresses failed")),
        None => bail!("proxy TCP connect budget exhausted before an address succeeded"),
    }
}

async fn send_over_tunnel<IO>(
    stream: IO,
    target_host: &str,
    target_tls: bool,
    req: Request<HttpBody>,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
) -> Result<Response<HttpBody>>
where
    IO: AsyncReadRent + AsyncWriteRent + Split + 'static,
{
    if target_tls {
        let connector = tls_connector()?;
        let tls_stream = timeout_stage(
            connect_timeout,
            "TLS handshake with upstream target",
            connector.connect(target_host, stream),
        )
        .await?
        .with_context(|| format!("TLS handshake with upstream target {target_host}"))?;
        proxy_send_request(tls_stream, req, read_timeout).await
    } else {
        proxy_send_request(stream, req, read_timeout).await
    }
}

fn tls_connector() -> Result<monoio_native_tls::TlsConnector> {
    let native = native_tls::TlsConnector::new().context("build native-tls connector")?;
    Ok(monoio_native_tls::TlsConnector::from(native))
}

async fn timeout_stage<F, T>(timeout: Option<Duration>, stage: &'static str, fut: F) -> Result<T>
where
    F: std::future::Future<Output = T>,
{
    match timeout {
        Some(duration) => monoio::time::timeout(duration, fut)
            .await
            .map_err(|_| anyhow::anyhow!("{stage} timed out after {duration:?}")),
        None => Ok(fut.await),
    }
}

async fn http_connect_tunnel<IO>(
    mut stream: IO,
    proxy: &ProxyEndpoint,
    target_host: &str,
    target_port: u16,
) -> Result<IO>
where
    IO: AsyncReadRent + AsyncWriteRent,
{
    let target_authority = host_port(target_host, target_port);
    let mut connect_req =
        format!("CONNECT {target_authority} HTTP/1.1\r\nHost: {target_authority}\r\n");
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
    if status_line.split_whitespace().nth(1) != Some("200") {
        bail!("proxy CONNECT failed: {status_line}");
    }
    Ok(stream)
}

fn host_port(host: &str, port: u16) -> String {
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

async fn proxy_send_request<IO>(
    stream: IO,
    req: Request<HttpBody>,
    read_timeout: Option<Duration>,
) -> Result<Response<HttpBody>>
where
    IO: AsyncReadRent + AsyncWriteRent + Split + 'static,
{
    let mut codec = ClientCodec::new(stream);
    timeout_stage(
        read_timeout,
        "write request through proxy tunnel",
        codec.send_and_flush(req),
    )
    .await?
    .map_err(|err| anyhow::anyhow!("send upstream request via proxy: {err:?}"))?;

    let response = timeout_stage(
        read_timeout,
        "wait for upstream response headers through proxy",
        codec.next(),
    )
    .await?;
    let response = match response {
        Some(Ok(response)) => response,
        Some(Err(err)) => bail!("decode upstream response via proxy: {err}"),
        None => bail!("proxy upstream closed without response"),
    };

    let (parts, payload_decoder) = response.into_parts();
    let body = match payload_decoder {
        PayloadDecoder::None => HttpBody::from(Payload::None),
        decoder @ PayloadDecoder::Fixed(_) => {
            let (payload, payload_sender) = fixed_payload_pair::<Bytes, HttpError>();
            monoio::spawn(async move {
                let mut framed_payload = decoder.with_io(codec);
                let item = next_body_data(&mut framed_payload, read_timeout).await;
                payload_sender.feed(item.unwrap_or_else(|| {
                    Err(HttpError::from(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "proxy upstream fixed body ended without data",
                    )))
                }));
            });
            HttpBody::from(Payload::Fixed(payload))
        }
        decoder @ PayloadDecoder::Streamed(_) => {
            let (payload, mut payload_sender) = stream_payload_pair::<Bytes, HttpError>();
            monoio::spawn(async move {
                let mut framed_payload = decoder.with_io(codec);
                loop {
                    match next_body_data(&mut framed_payload, read_timeout).await {
                        Some(Ok(data)) => payload_sender.feed_data(Some(data)),
                        Some(Err(err)) => {
                            payload_sender.feed_error(err);
                            payload_sender.feed_data(None);
                            break;
                        }
                        None => {
                            payload_sender.feed_data(None);
                            break;
                        }
                    }
                }
            });
            HttpBody::from(Payload::Stream(payload))
        }
    };

    Ok(Response::from_parts(parts, body))
}

async fn next_body_data<B>(
    body: &mut B,
    read_timeout: Option<Duration>,
) -> Option<Result<Bytes, HttpError>>
where
    B: MonoioBody<Data = Bytes, Error = HttpError>,
{
    match read_timeout {
        Some(duration) => {
            match monoio::time::timeout(duration, MonoioBody::next_data(body)).await {
                Ok(item) => item,
                Err(_) => Some(Err(HttpError::from(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("proxy upstream body idle timeout after {duration:?}"),
                )))),
            }
        }
        None => MonoioBody::next_data(body).await,
    }
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
    use futures_channel::oneshot;
    use monoio::net::TcpListener;

    #[test]
    fn upgrades_socks5_to_socks5h() {
        let p = parse_proxy_endpoint("socks5://127.0.0.1:1080").expect("parse");
        assert_eq!(p.kind, ProxyKind::Socks5RemoteDns);
        assert!(p.redacted_label().starts_with("socks5h://"));
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
    fn distinguishes_https_proxy_and_redacts_credentials() {
        let p =
            parse_proxy_endpoint("https://unique-proxy-user:unique-proxy-password@proxy.example")
                .expect("parse");
        assert_eq!(p.kind, ProxyKind::Https);
        assert_eq!(p.port, 443);

        let label = p.redacted_label();
        let debug = format!("{p:?}");
        for rendered in [&label, &debug] {
            assert!(!rendered.contains("unique-proxy-user"));
            assert!(!rendered.contains("unique-proxy-password"));
        }
        assert_eq!(label, "https://<redacted>@proxy.example:443");
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

    #[test]
    fn circuit_isolates_channels_and_proxy_credentials() {
        let policy = ProxyCircuitPolicy::new(1, Duration::from_secs(30));
        let breaker = ProxyCircuitBreaker::new(policy);
        let proxy_a =
            parse_proxy_endpoint("http://alice:secret@proxy.example:8080").expect("parse proxy A");
        let proxy_b =
            parse_proxy_endpoint("http://bob:secret@proxy.example:8080").expect("parse proxy B");

        breaker
            .acquire("channel-a", &proxy_a, "api.example", 443, true)
            .expect("first channel A request")
            .failure();
        assert!(matches!(
            breaker.acquire("channel-a", &proxy_a, "api.example", 443, true),
            Err(ProxyCircuitRejection { .. })
        ));

        breaker
            .acquire("channel-b", &proxy_a, "api.example", 443, true)
            .expect("channel B must have independent state")
            .success();
        breaker
            .acquire("channel-a", &proxy_b, "api.example", 443, true)
            .expect("credential identity must have independent state")
            .success();
        breaker
            .acquire("channel-a", &proxy_a, "other.example", 443, true)
            .expect("target authority must have independent state")
            .success();
    }

    #[test]
    fn circuit_allows_one_half_open_probe_and_recovers() {
        let policy = ProxyCircuitPolicy::new(1, Duration::ZERO);
        let breaker = ProxyCircuitBreaker::new(policy);
        let proxy = parse_proxy_endpoint("http://proxy.example:8080").expect("parse proxy");

        breaker
            .acquire("channel-a", &proxy, "api.example", 443, true)
            .expect("initial request")
            .failure();
        let probe = breaker
            .acquire("channel-a", &proxy, "api.example", 443, true)
            .expect("cooldown elapsed");
        assert!(matches!(
            breaker.acquire("channel-a", &proxy, "api.example", 443, true),
            Err(ProxyCircuitRejection { .. })
        ));
        assert!(probe.success());
        breaker
            .acquire("channel-a", &proxy, "api.example", 443, true)
            .expect("successful probe closes circuit")
            .success();
    }

    #[test]
    fn dropped_transport_permit_counts_as_a_failure() {
        let policy = ProxyCircuitPolicy::new(1, Duration::from_secs(30));
        let breaker = ProxyCircuitBreaker::new(policy);
        let proxy = parse_proxy_endpoint("http://proxy.example:8080").expect("parse proxy");

        let permit = breaker
            .acquire("channel-a", &proxy, "api.example", 443, true)
            .expect("initial request");
        drop(permit);
        assert!(matches!(
            breaker.acquire("channel-a", &proxy, "api.example", 443, true),
            Err(ProxyCircuitRejection { .. })
        ));
    }

    #[monoio::test_all(enable_timer = true)]
    async fn tries_next_resolved_proxy_address_within_total_budget() {
        let failed_listener = TcpListener::bind("127.0.0.1:0").expect("bind failed address");
        let failed_address = failed_listener.local_addr().expect("failed address");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind working address");
        let working_address = listener.local_addr().expect("working address");
        drop(failed_listener);
        let server = monoio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept fallback connection");
            drop(stream);
        });

        let stream = connect_proxy_addrs(
            vec![failed_address, working_address],
            Some(Duration::from_millis(500)),
        )
        .await
        .expect("second resolved address should connect");
        drop(stream);
        server.await;
    }

    #[monoio::test_all(enable_timer = true)]
    async fn returns_chunked_response_before_proxy_body_arrives() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
        let address = listener.local_addr().expect("proxy address");
        let (release_body_tx, release_body_rx) = oneshot::channel::<()>();

        let server = monoio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept proxy connection");
            let connect = read_http_head(&mut stream).await;
            assert!(connect.starts_with(b"CONNECT upstream.example:80 HTTP/1.1\r\n"));
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n".to_vec())
                .await
                .0
                .expect("write CONNECT response");

            let request = read_http_head(&mut stream).await;
            assert!(request.starts_with(b"GET /events HTTP/1.1\r\n"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\n"
                        .to_vec(),
                )
                .await
                .0
                .expect("write response headers");

            release_body_rx.await.expect("release response body");
            stream
                .write_all(b"5\r\nhello\r\n0\r\n\r\n".to_vec())
                .await
                .0
                .expect("write response body");
        });

        let proxy = parse_proxy_endpoint(&format!("http://{address}")).expect("parse proxy");
        let request = Request::builder()
            .method("GET")
            .uri("/events")
            .header("host", "upstream.example")
            .body(HttpBody::default())
            .expect("build request");
        let response = monoio::time::timeout(
            Duration::from_millis(500),
            send_request_via_proxy(
                &proxy,
                "upstream.example",
                80,
                false,
                request,
                Some(Duration::from_millis(500)),
                Some(Duration::from_millis(500)),
            ),
        )
        .await
        .expect("response headers should not wait for body")
        .expect("proxy request");
        assert_eq!(response.status(), http::StatusCode::OK);

        release_body_tx.send(()).expect("release response body");
        let mut body = response.into_body();
        let chunk =
            monoio::time::timeout(Duration::from_millis(500), MonoioBody::next_data(&mut body))
                .await
                .expect("body timeout")
                .expect("body chunk")
                .expect("valid body chunk");
        assert_eq!(chunk, Bytes::from_static(b"hello"));
        assert!(MonoioBody::next_data(&mut body).await.is_none());
        server.await;
    }

    #[monoio::test_all(enable_timer = true)]
    async fn times_out_stalled_http_connect_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
        let address = listener.local_addr().expect("proxy address");
        let (release_tx, release_rx) = oneshot::channel::<()>();

        let server = monoio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept proxy connection");
            let _ = read_http_head(&mut stream).await;
            let _ = release_rx.await;
        });

        let proxy = parse_proxy_endpoint(&format!("http://{address}")).expect("parse proxy");
        let request = Request::builder()
            .method("GET")
            .uri("/")
            .header("host", "upstream.example")
            .body(HttpBody::default())
            .expect("build request");
        let error = send_request_via_proxy(
            &proxy,
            "upstream.example",
            80,
            false,
            request,
            Some(Duration::from_millis(30)),
            Some(Duration::from_millis(500)),
        )
        .await
        .expect_err("CONNECT must time out");
        assert!(error.to_string().contains("HTTP proxy CONNECT handshake"));
        release_tx.send(()).expect("release proxy task");
        server.await;
    }

    #[monoio::test_all(enable_timer = true)]
    async fn times_out_stalled_socks5_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
        let address = listener.local_addr().expect("proxy address");
        let (release_tx, release_rx) = oneshot::channel::<()>();

        let server = monoio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept proxy connection");
            let mut greeting = [0u8; 3];
            read_exact(&mut stream, &mut greeting)
                .await
                .expect("read SOCKS5 greeting");
            assert_eq!(greeting, [0x05, 0x01, 0x00]);
            let _ = release_rx.await;
        });

        let proxy = parse_proxy_endpoint(&format!("socks5h://{address}")).expect("parse proxy");
        let request = Request::builder()
            .method("GET")
            .uri("/")
            .header("host", "upstream.example")
            .body(HttpBody::default())
            .expect("build request");
        let error = send_request_via_proxy(
            &proxy,
            "upstream.example",
            80,
            false,
            request,
            Some(Duration::from_millis(30)),
            Some(Duration::from_millis(500)),
        )
        .await
        .expect_err("SOCKS5 greeting must time out");
        assert!(error.to_string().contains("SOCKS5 proxy handshake"));
        release_tx.send(()).expect("release proxy task");
        server.await;
    }

    #[monoio::test_all(enable_timer = true)]
    async fn propagates_proxy_body_idle_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
        let address = listener.local_addr().expect("proxy address");
        let (release_tx, release_rx) = oneshot::channel::<()>();

        let server = monoio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept proxy connection");
            let _ = read_http_head(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n".to_vec())
                .await
                .0
                .expect("write CONNECT response");
            let _ = read_http_head(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec())
                .await
                .0
                .expect("write response headers");
            let _ = release_rx.await;
        });

        let proxy = parse_proxy_endpoint(&format!("http://{address}")).expect("parse proxy");
        let request = Request::builder()
            .method("GET")
            .uri("/events")
            .header("host", "upstream.example")
            .body(HttpBody::default())
            .expect("build request");
        let response = send_request_via_proxy(
            &proxy,
            "upstream.example",
            80,
            false,
            request,
            Some(Duration::from_millis(500)),
            Some(Duration::from_millis(30)),
        )
        .await
        .expect("response headers");
        let mut body = response.into_body();
        let error = MonoioBody::next_data(&mut body)
            .await
            .expect("timeout item")
            .expect_err("body must time out");
        assert!(error.to_string().contains("body idle timeout"));
        assert!(MonoioBody::next_data(&mut body).await.is_none());

        release_tx.send(()).expect("release proxy task");
        server.await;
    }

    async fn read_http_head(stream: &mut TcpStream) -> Vec<u8> {
        let mut head = Vec::new();
        loop {
            let (result, buffer) = stream.read(vec![0u8; 1024]).await;
            let read = result.expect("read HTTP head");
            assert!(read > 0, "unexpected EOF while reading HTTP head");
            head.extend_from_slice(&buffer[..read]);
            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                return head;
            }
            assert!(head.len() <= 16 * 1024, "HTTP head too large");
        }
    }
}
