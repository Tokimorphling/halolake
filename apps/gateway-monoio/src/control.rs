use super::*;
use halolake_api_contract::ApiResponse;
use halolake_control_plane::{
    SnapshotError, SnapshotRequest, SnapshotResponse, UsageAck, UsageError, UsageEventBatch,
};

const INTERNAL_KEY_HEADER: &str = "x-halolake-internal-key";

#[derive(Clone)]
pub(crate) struct MonoioHttpSnapshotSource {
    snapshot_url:    Arc<str>,
    internal_key:    Option<Arc<str>>,
    connect_timeout: Option<Duration>,
    http:            HttpUpstream,
    https:           HttpsUpstream,
}

impl MonoioHttpSnapshotSource {
    pub(crate) fn from_config(config: &ControlPlaneConfig) -> Result<Option<Self>> {
        let Some(snapshot_url) = &config.snapshot_url else {
            return Ok(None);
        };
        let uri: Uri = snapshot_url
            .parse()
            .with_context(|| format!("parse control snapshot url {snapshot_url}"))?;
        if uri.host().is_none() {
            anyhow::bail!("control snapshot url must include host");
        }

        let read_timeout = config.read_timeout_ms.map(Duration::from_millis);
        let mut http = HttpUpstream::build_tcp_http1_only();
        http.set_read_timeout(read_timeout);
        let mut https = HttpsUpstream::default();
        https.set_read_timeout(read_timeout);

        Ok(Some(Self {
            snapshot_url: Arc::from(snapshot_url.as_str()),
            internal_key: config.internal_key.as_deref().map(Arc::<str>::from),
            connect_timeout: config.connect_timeout_ms.map(Duration::from_millis),
            http,
            https,
        }))
    }

    fn request_uri(&self, since_version: Option<u64>) -> Result<Uri, SnapshotError> {
        let mut url = self.snapshot_url.to_string();
        if let Some(version) = since_version {
            if url.contains('?') {
                url.push('&');
            } else {
                url.push('?');
            }
            url.push_str("since_version=");
            url.push_str(&version.to_string());
        }
        url.parse()
            .map_err(|err| SnapshotError::InvalidResponse(format!("invalid snapshot url: {err}")))
    }

    async fn send(&self, uri: Uri, req: Request<HttpBody>) -> Result<Response<HttpBody>> {
        if uri.scheme() == Some(&http::uri::Scheme::HTTPS) {
            let key: TcpTlsAddr = uri
                .clone()
                .try_into()
                .context("invalid https control snapshot uri")?;
            let connect = self.https.connect(key);
            let mut conn = timeout_opt(self.connect_timeout, connect)
                .await
                .context("control snapshot connect timeout")?
                .context("connect https control snapshot source")?;
            let (resp, _) = conn.send_request(req).await;
            resp.context("send https control snapshot request")
        } else {
            let host = uri.host().context("control snapshot uri missing host")?;
            let port = uri.port_u16().unwrap_or(80);
            let addr = format!("{host}:{port}")
                .to_socket_addrs()
                .with_context(|| format!("resolve control snapshot source {host}:{port}"))?
                .next()
                .context("control snapshot source resolved no addresses")?;
            let connect = self.http.connect(addr);
            let mut conn = timeout_opt(self.connect_timeout, connect)
                .await
                .context("control snapshot connect timeout")?
                .context("connect http control snapshot source")?;
            let (resp, _) = conn.send_request(req).await;
            resp.context("send http control snapshot request")
        }
    }
}

impl Service<SnapshotRequest> for MonoioHttpSnapshotSource {
    type Response = SnapshotResponse;
    type Error = SnapshotError;

    async fn call(&self, req: SnapshotRequest) -> Result<Self::Response, Self::Error> {
        let uri = self.request_uri(req.since_version)?;
        let path = uri
            .path_and_query()
            .map_or(uri.path(), |path_and_query| path_and_query.as_str())
            .to_string();

        let mut builder = Request::builder()
            .method(Method::GET)
            .uri(path.as_str())
            .header(header::HOST, authority(&uri).map_err(snapshot_transport)?)
            .header(header::ACCEPT, "application/json");
        if let Some(internal_key) = &self.internal_key {
            builder = builder.header(INTERNAL_KEY_HEADER, internal_key.as_ref());
        }

        let upstream_req = builder
            .body(HttpBody::fixed_body(Some(Bytes::new())))
            .map_err(|err| SnapshotError::Transport(err.to_string()))?;
        let mut upstream = self
            .send(uri, upstream_req)
            .await
            .map_err(snapshot_transport)?;

        if upstream.status() == StatusCode::NOT_MODIFIED {
            let version = upstream
                .headers()
                .get("x-halolake-snapshot-version")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .or(req.since_version)
                .unwrap_or_default();
            return Ok(SnapshotResponse::NotModified { version });
        }

        if !upstream.status().is_success() {
            return Err(SnapshotError::Transport(format!(
                "control snapshot source returned {}",
                upstream.status()
            )));
        }

        let payload = upstream
            .body_mut()
            .to_ready()
            .await
            .map_err(|err| SnapshotError::Transport(err.to_string()))?
            .unwrap_or_default();
        serde_json::from_slice(&payload)
            .map_err(|err| SnapshotError::InvalidResponse(err.to_string()))
    }
}

fn snapshot_transport(err: anyhow::Error) -> SnapshotError {
    SnapshotError::Transport(err.to_string())
}

#[derive(Clone)]
pub(crate) struct MonoioHttpUsageSink {
    usage_url:       Arc<str>,
    internal_key:    Option<Arc<str>>,
    connect_timeout: Option<Duration>,
    http:            HttpUpstream,
    https:           HttpsUpstream,
}

impl MonoioHttpUsageSink {
    pub(crate) fn from_config(config: &ControlPlaneConfig) -> Result<Option<Self>> {
        let Some(usage_url) = &config.usage_url else {
            return Ok(None);
        };
        let uri: Uri = usage_url
            .parse()
            .with_context(|| format!("parse control usage url {usage_url}"))?;
        if uri.host().is_none() {
            anyhow::bail!("control usage url must include host");
        }

        let read_timeout = config.read_timeout_ms.map(Duration::from_millis);
        let mut http = HttpUpstream::build_tcp_http1_only();
        http.set_read_timeout(read_timeout);
        let mut https = HttpsUpstream::default();
        https.set_read_timeout(read_timeout);

        Ok(Some(Self {
            usage_url: Arc::from(usage_url.as_str()),
            internal_key: config.internal_key.as_deref().map(Arc::<str>::from),
            connect_timeout: config.connect_timeout_ms.map(Duration::from_millis),
            http,
            https,
        }))
    }

    async fn send(&self, uri: Uri, req: Request<HttpBody>) -> Result<Response<HttpBody>> {
        if uri.scheme() == Some(&http::uri::Scheme::HTTPS) {
            let key: TcpTlsAddr = uri.clone().try_into().context("invalid https usage uri")?;
            let connect = self.https.connect(key);
            let mut conn = timeout_opt(self.connect_timeout, connect)
                .await
                .context("control usage connect timeout")?
                .context("connect https control usage sink")?;
            let (resp, _) = conn.send_request(req).await;
            resp.context("send https control usage request")
        } else {
            let host = uri.host().context("control usage uri missing host")?;
            let port = uri.port_u16().unwrap_or(80);
            let addr = format!("{host}:{port}")
                .to_socket_addrs()
                .with_context(|| format!("resolve control usage sink {host}:{port}"))?
                .next()
                .context("control usage sink resolved no addresses")?;
            let connect = self.http.connect(addr);
            let mut conn = timeout_opt(self.connect_timeout, connect)
                .await
                .context("control usage connect timeout")?
                .context("connect http control usage sink")?;
            let (resp, _) = conn.send_request(req).await;
            resp.context("send http control usage request")
        }
    }
}

impl Service<UsageEventBatch> for MonoioHttpUsageSink {
    type Response = UsageAck;
    type Error = UsageError;

    async fn call(&self, batch: UsageEventBatch) -> Result<Self::Response, Self::Error> {
        let uri: Uri = self
            .usage_url
            .parse()
            .map_err(|err| UsageError::Transport(format!("invalid usage url: {err}")))?;
        let path = uri
            .path_and_query()
            .map_or(uri.path(), |path_and_query| path_and_query.as_str())
            .to_string();
        let body = serde_json::to_vec(&batch)
            .map(Bytes::from)
            .map_err(|err| UsageError::InvalidResponse(err.to_string()))?;

        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(path.as_str())
            .header(header::HOST, authority(&uri).map_err(usage_transport)?)
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(internal_key) = &self.internal_key {
            builder = builder.header(INTERNAL_KEY_HEADER, internal_key.as_ref());
        }

        let upstream_req = builder
            .body(HttpBody::fixed_body(Some(body)))
            .map_err(|err| UsageError::Transport(err.to_string()))?;
        let mut upstream = self
            .send(uri, upstream_req)
            .await
            .map_err(usage_transport)?;
        if !upstream.status().is_success() {
            return Err(UsageError::Transport(format!(
                "control usage sink returned {}",
                upstream.status()
            )));
        }

        let payload = upstream
            .body_mut()
            .to_ready()
            .await
            .map_err(|err| UsageError::Transport(err.to_string()))?
            .unwrap_or_default();
        let resp: ApiResponse<UsageAck> = serde_json::from_slice(&payload)
            .map_err(|err| UsageError::InvalidResponse(err.to_string()))?;
        if resp.success {
            resp.data
                .ok_or_else(|| UsageError::InvalidResponse("missing usage ack".to_string()))
        } else {
            Err(UsageError::Transport(resp.message))
        }
    }
}

fn usage_transport(err: anyhow::Error) -> UsageError {
    UsageError::Transport(err.to_string())
}

#[derive(Clone)]
pub(crate) struct MonoioHttpChannelFeedbackSink {
    channel_feedback_url: Arc<str>,
    internal_key:         Option<Arc<str>>,
    connect_timeout:      Option<Duration>,
    http:                 HttpUpstream,
    https:                HttpsUpstream,
}

impl MonoioHttpChannelFeedbackSink {
    pub(crate) fn from_config(config: &ControlPlaneConfig) -> Result<Option<Self>> {
        let Some(channel_feedback_url) = &config.channel_feedback_url else {
            return Ok(None);
        };
        let uri: Uri = channel_feedback_url.parse().with_context(|| {
            format!("parse control channel feedback url {channel_feedback_url}")
        })?;
        if uri.host().is_none() {
            anyhow::bail!("control channel feedback url must include host");
        }

        let read_timeout = config.read_timeout_ms.map(Duration::from_millis);
        let mut http = HttpUpstream::build_tcp_http1_only();
        http.set_read_timeout(read_timeout);
        let mut https = HttpsUpstream::default();
        https.set_read_timeout(read_timeout);

        Ok(Some(Self {
            channel_feedback_url: Arc::from(channel_feedback_url.as_str()),
            internal_key: config.internal_key.as_deref().map(Arc::<str>::from),
            connect_timeout: config.connect_timeout_ms.map(Duration::from_millis),
            http,
            https,
        }))
    }

    async fn send(&self, uri: Uri, req: Request<HttpBody>) -> Result<Response<HttpBody>> {
        if uri.scheme() == Some(&http::uri::Scheme::HTTPS) {
            let key: TcpTlsAddr = uri
                .clone()
                .try_into()
                .context("invalid https channel feedback uri")?;
            let connect = self.https.connect(key);
            let mut conn = timeout_opt(self.connect_timeout, connect)
                .await
                .context("control channel feedback connect timeout")?
                .context("connect https control channel feedback sink")?;
            let (resp, _) = conn.send_request(req).await;
            resp.context("send https control channel feedback request")
        } else {
            let host = uri
                .host()
                .context("control channel feedback uri missing host")?;
            let port = uri.port_u16().unwrap_or(80);
            let addr = format!("{host}:{port}")
                .to_socket_addrs()
                .with_context(|| format!("resolve control channel feedback sink {host}:{port}"))?
                .next()
                .context("control channel feedback sink resolved no addresses")?;
            let connect = self.http.connect(addr);
            let mut conn = timeout_opt(self.connect_timeout, connect)
                .await
                .context("control channel feedback connect timeout")?
                .context("connect http control channel feedback sink")?;
            let (resp, _) = conn.send_request(req).await;
            resp.context("send http control channel feedback request")
        }
    }
}

impl Service<ChannelFeedbackBatch> for MonoioHttpChannelFeedbackSink {
    type Response = ChannelFeedbackAck;
    type Error = ChannelFeedbackError;

    async fn call(&self, batch: ChannelFeedbackBatch) -> Result<Self::Response, Self::Error> {
        let uri: Uri = self.channel_feedback_url.parse().map_err(|err| {
            ChannelFeedbackError::Transport(format!("invalid channel feedback url: {err}"))
        })?;
        let path = uri
            .path_and_query()
            .map_or(uri.path(), |path_and_query| path_and_query.as_str())
            .to_string();
        let body = serde_json::to_vec(&batch)
            .map(Bytes::from)
            .map_err(|err| ChannelFeedbackError::InvalidResponse(err.to_string()))?;

        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(path.as_str())
            .header(
                header::HOST,
                authority(&uri).map_err(channel_feedback_transport)?,
            )
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(internal_key) = &self.internal_key {
            builder = builder.header(INTERNAL_KEY_HEADER, internal_key.as_ref());
        }

        let upstream_req = builder
            .body(HttpBody::fixed_body(Some(body)))
            .map_err(|err| ChannelFeedbackError::Transport(err.to_string()))?;
        let mut upstream = self
            .send(uri, upstream_req)
            .await
            .map_err(channel_feedback_transport)?;
        if !upstream.status().is_success() {
            return Err(ChannelFeedbackError::Transport(format!(
                "control channel feedback sink returned {}",
                upstream.status()
            )));
        }

        let payload = upstream
            .body_mut()
            .to_ready()
            .await
            .map_err(|err| ChannelFeedbackError::Transport(err.to_string()))?
            .unwrap_or_default();
        let resp: ApiResponse<ChannelFeedbackAck> = serde_json::from_slice(&payload)
            .map_err(|err| ChannelFeedbackError::InvalidResponse(err.to_string()))?;
        if resp.success {
            resp.data.ok_or_else(|| {
                ChannelFeedbackError::InvalidResponse("missing channel feedback ack".to_string())
            })
        } else {
            Err(ChannelFeedbackError::Transport(resp.message))
        }
    }
}

fn channel_feedback_transport(err: anyhow::Error) -> ChannelFeedbackError {
    ChannelFeedbackError::Transport(err.to_string())
}

#[derive(Clone, Default)]
pub(crate) struct UsageReporter {
    sink: Option<MonoioHttpUsageSink>,
}

impl UsageReporter {
    pub(crate) fn from_config(config: &ControlPlaneConfig) -> Result<Self> {
        Ok(Self {
            sink: MonoioHttpUsageSink::from_config(config)?,
        })
    }

    pub(crate) fn report(&self, event: UsageEvent) {
        let Some(sink) = self.sink.clone() else {
            return;
        };
        monoio::spawn(async move {
            let request_id = event.request_id.clone();
            let batch = UsageEventBatch::new(vec![event]);
            match sink.call(batch).await {
                Ok(ack) => {
                    debug!(%request_id, accepted = ack.accepted, "reported usage event");
                }
                Err(err) => {
                    warn!(%request_id, ?err, "failed to report usage event");
                }
            }
        });
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.sink.is_some()
    }
}

#[derive(Clone, Default)]
pub(crate) struct ChannelFeedbackReporter {
    sink: Option<MonoioHttpChannelFeedbackSink>,
}

impl ChannelFeedbackReporter {
    pub(crate) fn from_config(config: &ControlPlaneConfig) -> Result<Self> {
        Ok(Self {
            sink: MonoioHttpChannelFeedbackSink::from_config(config)?,
        })
    }

    pub(crate) fn report(&self, event: ChannelFeedbackEvent) {
        let Some(sink) = self.sink.clone() else {
            return;
        };
        monoio::spawn(async move {
            let request_id = event.request_id.clone();
            let channel_id = event.channel_id.clone();
            let batch = ChannelFeedbackBatch::new(vec![event]);
            match sink.call(batch).await {
                Ok(ack) => {
                    debug!(
                        %request_id,
                        %channel_id,
                        accepted = ack.accepted,
                        disabled_channels = ack.disabled_channels,
                        disabled_keys = ack.disabled_keys,
                        "reported channel feedback"
                    );
                }
                Err(err) => {
                    warn!(
                        %request_id,
                        %channel_id,
                        ?err,
                        "failed to report channel feedback"
                    );
                }
            }
        });
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.sink.is_some()
    }
}

/// Report gateway process heartbeats to control-api System Info.
#[derive(Clone)]
pub(crate) struct SystemInstanceReporter {
    url:             Arc<str>,
    internal_key:    Option<Arc<str>>,
    connect_timeout: Option<Duration>,
    http:            HttpUpstream,
    https:           HttpsUpstream,
    started_at:      i64,
}

impl SystemInstanceReporter {
    pub(crate) fn from_config(config: &ControlPlaneConfig) -> Result<Option<Self>> {
        let Some(url) = &config.system_instance_url else {
            return Ok(None);
        };
        let uri: Uri = url
            .parse()
            .with_context(|| format!("parse control system instance url {url}"))?;
        if uri.host().is_none() {
            anyhow::bail!("control system instance url must include host");
        }
        let read_timeout = config.read_timeout_ms.map(Duration::from_millis);
        let mut http = HttpUpstream::build_tcp_http1_only();
        http.set_read_timeout(read_timeout);
        let mut https = HttpsUpstream::default();
        https.set_read_timeout(read_timeout);
        Ok(Some(Self {
            url: Arc::from(url.as_str()),
            internal_key: config.internal_key.as_deref().map(Arc::<str>::from),
            connect_timeout: config.connect_timeout_ms.map(Duration::from_millis),
            http,
            https,
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        }))
    }

    async fn send(&self, uri: Uri, req: Request<HttpBody>) -> Result<Response<HttpBody>> {
        if uri.scheme() == Some(&http::uri::Scheme::HTTPS) {
            let key: TcpTlsAddr = uri
                .clone()
                .try_into()
                .context("invalid https system instance uri")?;
            let connect = self.https.connect(key);
            let mut conn = timeout_opt(self.connect_timeout, connect)
                .await
                .context("control system instance connect timeout")?
                .context("connect https control system instance sink")?;
            let (resp, _) = conn.send_request(req).await;
            resp.context("send https control system instance request")
        } else {
            let host = uri
                .host()
                .context("control system instance uri missing host")?;
            let port = uri.port_u16().unwrap_or(80);
            let addr = format!("{host}:{port}")
                .to_socket_addrs()
                .with_context(|| format!("resolve control system instance sink {host}:{port}"))?
                .next()
                .context("control system instance sink resolved no addresses")?;
            let connect = self.http.connect(addr);
            let mut conn = timeout_opt(self.connect_timeout, connect)
                .await
                .context("control system instance connect timeout")?
                .context("connect http control system instance sink")?;
            let (resp, _) = conn.send_request(req).await;
            resp.context("send http control system instance request")
        }
    }

    pub(crate) async fn report_once(&self) {
        let host = gateway_host_key();
        let node_name = format!("{host}/gateway");
        let rss = gateway_process_rss_bytes();
        let info = serde_json::json!({
            "schema_version": 1,
            "node": {
                "name": node_name,
                "source": "gateway",
                "manually_configured": std::env::var("HALOLAKE_NODE_NAME").is_ok()
                    || std::env::var("NODE_NAME").is_ok(),
                "should_configure_manually": std::env::var("HALOLAKE_NODE_NAME").is_err()
                    && std::env::var("NODE_NAME").is_err(),
                "process": "gateway",
                "host_key": host,
            },
            "role": {
                "is_master": false,
                "process": "gateway",
            },
            "runtime": {
                "version": env!("CARGO_PKG_VERSION"),
                "goos": std::env::consts::OS,
                "goarch": std::env::consts::ARCH,
                "started_at": self.started_at,
            },
            "host": {
                "hostname": host,
            },
            "resources": {
                "process": "gateway",
                "cpu": { "usage_percent": null, "scope": "host" },
                "memory": {
                    "usage_percent": null,
                    "used_bytes": rss,
                    "process_rss_bytes": rss,
                    "scope": "process",
                },
                "storage": {
                    "total_bytes": null,
                    "used_bytes": null,
                    "free_bytes": null,
                    "used_percent": null,
                    "scope": "host",
                }
            }
        });
        let body = match serde_json::to_vec(&serde_json::json!({
            "node_name": node_name,
            "info": info,
            "started_at": self.started_at,
        })) {
            Ok(bytes) => Bytes::from(bytes),
            Err(err) => {
                warn!(?err, "serialize system instance report");
                return;
            }
        };
        let uri: Uri = match self.url.parse() {
            Ok(uri) => uri,
            Err(err) => {
                warn!(?err, "invalid system instance url");
                return;
            }
        };
        let path = uri
            .path_and_query()
            .map_or(uri.path(), |pq| pq.as_str())
            .to_string();
        let host_hdr = match authority(&uri) {
            Ok(a) => a,
            Err(err) => {
                warn!(?err, "system instance authority");
                return;
            }
        };
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(path.as_str())
            .header(header::HOST, host_hdr)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json");
        if let Some(internal_key) = &self.internal_key {
            builder = builder.header(INTERNAL_KEY_HEADER, internal_key.as_ref());
        }
        let req = match builder.body(HttpBody::fixed_body(Some(body))) {
            Ok(req) => req,
            Err(err) => {
                warn!(?err, "build system instance request");
                return;
            }
        };
        match self.send(uri, req).await {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => warn!(status = %resp.status(), "system instance report rejected"),
            Err(err) => warn!(?err, "system instance report failed"),
        }
    }
}

pub(crate) fn spawn_system_instance_reporter(reporter: SystemInstanceReporter) {
    monoio::spawn(async move {
        reporter.report_once().await;
        loop {
            monoio::time::sleep(Duration::from_secs(30)).await;
            reporter.report_once().await;
        }
    });
}

fn gateway_host_key() -> String {
    for key in ["HALOLAKE_NODE_NAME", "NODE_NAME", "HOSTNAME"] {
        if let Ok(name) = std::env::var(key) {
            let name = name.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    "halolake".to_string()
}

fn gateway_process_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
                return Some(kb.saturating_mul(1024));
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
