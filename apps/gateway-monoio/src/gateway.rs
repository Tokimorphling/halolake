use super::*;
use halolake_control_plane::{SnapshotRequest, SnapshotResponse};

#[derive(Clone)]
pub struct Gateway {
    snapshots: SnapshotStore,
    request_body_limit_bytes: usize,
    chat: ChatGatewayService,
    image: ImageGatewayService,
    claude: ClaudeMessagesGatewayService,
    gemini: GeminiGatewayService,
    raw_openai: RawOpenAiGatewayService,
}

#[derive(Clone)]
pub(crate) struct AppParams {
    snapshots: SnapshotStore,
    protocol: ProtocolConfig,
    upstream: UpstreamConfig,
    auth: AuthConfig,
    usage: UsageReporter,
    channel_feedback: ChannelFeedbackReporter,
    request_body_limit_bytes: usize,
}

#[derive(Clone)]
pub(crate) struct SnapshotStore {
    inner: Arc<ArcSwap<SnapshotState>>,
    // The affinity cache lives here, NOT inside the atomically-swapped
    // SnapshotState, so recorded affinities survive snapshot refresh. Held
    // behind an Arc so a future per-worker/shared deployment can decide the
    // sharing model without touching the routing code.
    affinity_cache: Arc<ChannelAffinityCache>,
}

pub(crate) struct SnapshotState {
    pub(crate) snapshot: IndexedSnapshot,
    pub(crate) models: Arc<[String]>,
}

impl SnapshotStore {
    fn new(snapshot: GatewaySnapshot) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(ArcSwap::from_pointee(SnapshotState::new(snapshot)?)),
            affinity_cache: Arc::new(ChannelAffinityCache::new()),
        })
    }

    pub(crate) fn load(&self) -> arc_swap::Guard<Arc<SnapshotState>> {
        self.inner.load()
    }

    pub(crate) fn affinity_cache(&self) -> &Arc<ChannelAffinityCache> {
        &self.affinity_cache
    }

    fn version(&self) -> u64 {
        self.load().snapshot.version()
    }

    fn store_snapshot(&self, snapshot: GatewaySnapshot) -> Result<u64> {
        let state = SnapshotState::new(snapshot)?;
        let version = state.snapshot.version();
        self.inner.store(Arc::new(state));
        Ok(version)
    }
}

impl SnapshotState {
    fn new(snapshot: GatewaySnapshot) -> Result<Self> {
        let models = snapshot
            .model_mappings
            .iter()
            .map(|mapping| mapping.requested_model.clone())
            .collect::<Vec<_>>()
            .into();
        Ok(Self {
            snapshot: snapshot.index().context("index gateway snapshot")?,
            models,
        })
    }
}

#[derive(Clone)]
pub(crate) struct ClaudeVersion(pub(crate) Arc<str>);

#[derive(Clone)]
pub(crate) struct GeminiApiVersion(pub(crate) Arc<str>);

#[derive(Clone, Copy)]
pub(crate) struct PassAnthropicBeta(pub(crate) bool);

#[derive(Clone, Copy)]
pub(crate) struct ConnectTimeout(pub(crate) Option<Duration>);

#[derive(Clone, Copy)]
pub(crate) struct UpstreamReadTimeout(pub(crate) Option<Duration>);

#[derive(Clone, Copy)]
pub(crate) struct RequestBodyLimit(pub(crate) usize);

#[derive(Clone, Copy)]
pub(crate) struct GatewayAuthPolicy(pub(crate) AuthConfig);

/// Loads the config and runs the gateway as a thread-per-core fleet.
///
/// Monoio is a thread-per-core runtime: each worker owns its own single-threaded
/// runtime, its own `Gateway` (and therefore its own snapshot store + affinity
/// cache), and its own listener socket bound with `SO_REUSEPORT`. The kernel load
/// -balances accepted connections across the sockets, so workers share nothing on
/// the request hot path — matching the shared-nothing data-plane design.
///
/// This function is synchronous on purpose: it must run OUTSIDE any monoio runtime
/// so it can spawn the per-core runtimes itself. `main` therefore calls it directly
/// rather than from `#[monoio::main]`.
pub fn run_from_config_file(path: &str) -> Result<()> {
    let config = GatewayConfig::load(path)?;
    let worker_count = resolve_worker_count(config.server.workers);
    let listen = config.server.listen;
    info!(
        %listen,
        worker_count,
        token_count = config.tokens.len(),
        channel_count = config.channels.len(),
        mapping_count = config.model_mappings.len(),
        "starting halolake monoio gateway (thread-per-core)"
    );

    let mut handles = Vec::with_capacity(worker_count);
    for worker_id in 0..worker_count {
        // Each worker gets an independent config clone so nothing is shared across
        // cores; the Arc-backed state inside Gateway is rebuilt per worker.
        let worker_config = config.clone();
        let handle = std::thread::Builder::new()
            .name(format!("halolake-gw-{worker_id}"))
            .spawn(move || run_worker(worker_id, worker_config))
            .with_context(|| format!("spawn gateway worker {worker_id}"))?;
        handles.push(handle);
    }

    // If any worker's bootstrap fails (e.g. initial snapshot load), surface the
    // first error. Workers that entered their serve loop never return.
    let mut first_error = None;
    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                error!(?err, "gateway worker exited with error");
                first_error.get_or_insert(err);
            }
            Err(_) => {
                error!("gateway worker thread panicked");
            }
        }
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn resolve_worker_count(configured: usize) -> usize {
    if configured > 0 {
        return configured;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Entry point for a single worker thread: builds a dedicated monoio runtime and
/// drives the async bootstrap (initial snapshot load, polling, serve loop).
fn run_worker(worker_id: usize, config: GatewayConfig) -> Result<()> {
    let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
        .enable_timer()
        .build()
        .with_context(|| format!("build monoio runtime for worker {worker_id}"))?;
    rt.block_on(run_worker_async(worker_id, config))
}

async fn run_worker_async(worker_id: usize, mut config: GatewayConfig) -> Result<()> {
    let snapshot_source = MonoioHttpSnapshotSource::from_config(&config.control)?;
    if let Some(source) = &snapshot_source {
        let snapshot_url = config.control.snapshot_url.as_deref().unwrap_or_default();
        debug!(worker_id, %snapshot_url, "loading gateway snapshot from control api");
        match source
            .clone()
            .call(SnapshotRequest {
                since_version: None,
            })
            .await
        {
            Ok(SnapshotResponse::Updated { snapshot }) => {
                debug!(
                    worker_id,
                    snapshot_version = snapshot.version,
                    "loaded gateway snapshot from control api"
                );
                config.replace_snapshot(snapshot);
            }
            Ok(SnapshotResponse::NotModified { version }) => {
                warn!(
                    worker_id,
                    snapshot_version = version,
                    "control api returned not-modified for initial snapshot load"
                );
            }
            Err(err) => {
                anyhow::bail!("load gateway snapshot from control api: {err}");
            }
        }
    }
    let snapshot_poll_interval = config
        .control
        .snapshot_poll_interval_ms
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(5));
    let listen = config.server.listen;
    let gateway = Gateway::try_from_config(config)?;
    if let Some(source) = snapshot_source
        && !snapshot_poll_interval.is_zero()
    {
        spawn_snapshot_polling(source, gateway.snapshots.clone(), snapshot_poll_interval);
    }
    debug!(
        worker_id,
        %listen,
        snapshot_version = gateway.snapshots.version(),
        "gateway worker listening"
    );
    serve(listen, gateway).await
}

fn spawn_snapshot_polling(
    source: MonoioHttpSnapshotSource,
    snapshots: SnapshotStore,
    interval: Duration,
) {
    monoio::spawn(async move {
        let mut since_version = snapshots.version();
        loop {
            monoio::time::sleep(interval).await;
            match source
                .call(SnapshotRequest {
                    since_version: Some(since_version),
                })
                .await
            {
                Ok(SnapshotResponse::NotModified { version }) => {
                    since_version = version.max(since_version);
                }
                Ok(SnapshotResponse::Updated { snapshot }) => {
                    let version = snapshot.version;
                    match snapshots.store_snapshot(snapshot) {
                        Ok(version) => {
                            since_version = version;
                            debug!(snapshot_version = version, "gateway snapshot refreshed");
                        }
                        Err(err) => {
                            warn!(
                                snapshot_version = version,
                                ?err,
                                "failed to index refreshed gateway snapshot"
                            );
                        }
                    }
                }
                Err(err) => {
                    warn!(?err, "failed to refresh gateway snapshot");
                }
            }
        }
    });
}

pub async fn serve(addr: SocketAddr, gateway: Gateway) -> Result<()> {
    // reuse_port lets every worker thread bind the same address so the kernel
    // load-balances accepted connections across the per-core runtimes.
    let opts = ListenerOpts::new().reuse_port(true).reuse_addr(true);
    let listener =
        TcpListener::bind_with_config(addr, &opts).context("bind gateway listener")?;
    loop {
        let (stream, peer) = listener.accept().await.context("accept connection")?;
        debug!(%peer, "accepted downstream connection");
        let gateway = gateway.clone();
        monoio::spawn(async move {
            let stream = match stream.into_poll_io() {
                Ok(stream) => MonoioIo::new(stream),
                Err(err) => {
                    warn!(?err, "failed to enter poll-io mode");
                    return;
                }
            };
            let service = service_fn(move |req| {
                let gateway = gateway.clone();
                async move { Ok::<_, Infallible>(gateway.handle(req, peer).await) }
            });
            if let Err(err) = http1::Builder::new()
                .timer(monoio_compat::hyper::MonoioTimer)
                .serve_connection(stream, service)
                .await
            {
                warn!(?err, "downstream http connection failed");
            }
        });
    }
}

impl GatewayConfig {
    pub fn load(path: &str) -> Result<Self> {
        let data = fs::read_to_string(path).with_context(|| format!("read config {path}"))?;
        let mut config: Self =
            toml::from_str(&data).with_context(|| format!("parse config {path}"))?;
        config.resolve_channel_env_keys()?;
        Ok(config)
    }

    fn resolve_channel_env_keys(&mut self) -> Result<()> {
        for channel in &mut self.channels {
            if channel.api_key.is_empty() {
                if let Some(env_name) = &channel.api_key_env {
                    channel.api_key = std::env::var(env_name).with_context(|| {
                        format!("read env var {env_name} for channel {}", channel.id)
                    })?;
                }
            }
        }
        Ok(())
    }

    fn into_snapshot(self) -> GatewaySnapshot {
        GatewaySnapshot {
            version: self.version,
            tokens: self.tokens,
            channels: self.channels,
            model_mappings: self.model_mappings,
            channel_affinity: self.channel_affinity,
            group_routing: self.group_routing,
        }
    }

    fn replace_snapshot(&mut self, snapshot: GatewaySnapshot) {
        self.version = snapshot.version;
        self.tokens = snapshot.tokens;
        self.channels = snapshot.channels;
        self.model_mappings = snapshot.model_mappings;
        self.channel_affinity = snapshot.channel_affinity;
        self.group_routing = snapshot.group_routing;
    }
}

impl Gateway {
    pub fn try_from_config(config: GatewayConfig) -> Result<Self> {
        let params = AppParams::try_from_config(config)?;
        Ok(Self {
            snapshots: params.param(),
            request_body_limit_bytes: Param::<RequestBodyLimit>::param(&params).0,
            chat: ChatGatewayService::from_params(&params),
            image: ImageGatewayService::from_params(&params),
            claude: ClaudeMessagesGatewayService::from_params(&params),
            gemini: GeminiGatewayService::from_params(&params),
            raw_openai: RawOpenAiGatewayService::from_params(&params),
        })
    }

    pub fn snapshot_version(&self) -> u64 {
        self.snapshots.version()
    }

    async fn handle(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<GatewayBody> {
        let request_id = RequestId(Uuid::new_v4().simple().to_string());
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let span = tracing::debug_span!(
            "gateway.request",
            request_id = %request_id.0,
            peer_addr = %peer,
            %method,
            %path,
        );

        async move {
            let base_cx = RequestContext::new()
                .param_set(request_id)
                .param_set(PeerAddr(peer));

            match (req.method(), req.uri().path()) {
                (&Method::GET, "/healthz") => json_response(
                    StatusCode::OK,
                    serde_json::json!({
                        "status": "ok",
                        "snapshot_version": self.snapshots.version(),
                    }),
                ),
                (&Method::GET, "/v1/models") => self.models_response(),
                (&Method::POST, "/v1/chat/completions") => {
                    let cx = base_cx.param_set(DownstreamProtocol::OpenAiChat);
                    match self.collect_request(req).await {
                        Ok(request) => self
                            .chat
                            .call((request, cx))
                            .await
                            .unwrap_or_else(|never| match never {}),
                        Err(resp) => resp,
                    }
                }
                (&Method::POST, "/v1/images/generations")
                | (&Method::POST, "/v1/images/edits")
                | (&Method::POST, "/v1/edits") => {
                    let cx = base_cx.param_set(DownstreamProtocol::OpenAiImage);
                    match self.collect_request(req).await {
                        Ok(request) => self
                            .image
                            .call((request, cx))
                            .await
                            .unwrap_or_else(|never| match never {}),
                        Err(resp) => resp,
                    }
                }
                (&Method::POST, "/v1/messages") => {
                    let cx = base_cx.param_set(DownstreamProtocol::ClaudeMessages);
                    match self.collect_request(req).await {
                        Ok(request) => self
                            .claude
                            .call((request, cx))
                            .await
                            .unwrap_or_else(|never| match never {}),
                        Err(resp) => resp,
                    }
                }
                (&Method::POST, path) if is_gemini_generate_content_path(path) => {
                    let cx = base_cx.param_set(DownstreamProtocol::GeminiGenerateContent);
                    match self.collect_request(req).await {
                        Ok(request) => self
                            .gemini
                            .call((request, cx))
                            .await
                            .unwrap_or_else(|never| match never {}),
                        Err(resp) => resp,
                    }
                }
                (&Method::POST, "/v1/responses")
                | (&Method::POST, "/v1/completions")
                | (&Method::POST, "/v1/embeddings") => {
                    let cx = base_cx.param_set(DownstreamProtocol::OpenAiRaw);
                    match self.collect_request(req).await {
                        Ok(request) => self
                            .raw_openai
                            .call((request, cx))
                            .await
                            .unwrap_or_else(|never| match never {}),
                        Err(resp) => resp,
                    }
                }
                _ => json_error(StatusCode::NOT_FOUND, "not_found", "route not found"),
            }
        }
        .instrument(span)
        .await
    }

    async fn collect_request(
        &self,
        req: Request<Incoming>,
    ) -> Result<GatewayRequest, Response<GatewayBody>> {
        let path = req
            .uri()
            .path_and_query()
            .map_or(req.uri().path(), |pq| pq.as_str())
            .to_string();
        let headers = req.headers().clone();
        let body = match req.into_body().collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(err) => {
                return Err(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("failed to read request body: {err}"),
                ));
            }
        };
        if body.len() > self.request_body_limit_bytes {
            return Err(json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "request body exceeds configured limit",
            ));
        }
        Ok(GatewayRequest {
            headers,
            path,
            body,
        })
    }

    fn models_response(&self) -> Response<GatewayBody> {
        let state = self.snapshots.load();
        let models = state
            .models
            .iter()
            .map(|id| serde_json::json!({"id": id, "object": "model", "owned_by": "halolake"}))
            .collect::<Vec<_>>();
        json_response(
            StatusCode::OK,
            serde_json::json!({"object": "list", "data": models}),
        )
    }
}

impl AppParams {
    fn try_from_config(config: GatewayConfig) -> Result<Self> {
        let request_body_limit_bytes = config.server.request_body_limit_bytes;
        let protocol = config.protocol.clone();
        let upstream = config.upstream;
        let auth = config.auth;
        let usage = UsageReporter::from_config(&config.control)?;
        let channel_feedback = ChannelFeedbackReporter::from_config(&config.control)?;
        let snapshot = config.into_snapshot();
        let snapshots = SnapshotStore::new(snapshot)?;

        Ok(Self {
            snapshots,
            protocol,
            upstream,
            auth,
            usage,
            channel_feedback,
            request_body_limit_bytes,
        })
    }
}

impl Param<SnapshotStore> for AppParams {
    fn param(&self) -> SnapshotStore {
        self.snapshots.clone()
    }
}

impl Param<ClaudeVersion> for AppParams {
    fn param(&self) -> ClaudeVersion {
        ClaudeVersion(Arc::from(self.protocol.claude_version.as_str()))
    }
}

impl Param<GeminiApiVersion> for AppParams {
    fn param(&self) -> GeminiApiVersion {
        GeminiApiVersion(Arc::from(self.protocol.gemini_api_version.as_str()))
    }
}

impl Param<PassAnthropicBeta> for AppParams {
    fn param(&self) -> PassAnthropicBeta {
        PassAnthropicBeta(self.protocol.pass_anthropic_beta)
    }
}

impl Param<GatewayAuthPolicy> for AppParams {
    fn param(&self) -> GatewayAuthPolicy {
        GatewayAuthPolicy(self.auth)
    }
}

impl Param<UsageReporter> for AppParams {
    fn param(&self) -> UsageReporter {
        self.usage.clone()
    }
}

impl Param<ChannelFeedbackReporter> for AppParams {
    fn param(&self) -> ChannelFeedbackReporter {
        self.channel_feedback.clone()
    }
}

impl Param<ConnectTimeout> for AppParams {
    fn param(&self) -> ConnectTimeout {
        ConnectTimeout(self.upstream.connect_timeout_ms.map(Duration::from_millis))
    }
}

impl Param<UpstreamReadTimeout> for AppParams {
    fn param(&self) -> UpstreamReadTimeout {
        UpstreamReadTimeout(self.upstream.read_timeout_ms.map(Duration::from_millis))
    }
}

impl Param<RequestBodyLimit> for AppParams {
    fn param(&self) -> RequestBodyLimit {
        RequestBodyLimit(self.request_body_limit_bytes)
    }
}
