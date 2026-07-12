use super::*;
use halolake_control_plane::{SnapshotRequest, SnapshotResponse};
use service_async::stack::FactoryStack;

#[derive(Clone)]
pub struct Gateway {
    pub(crate) snapshots:                SnapshotStore,
    pub(crate) request_body_limit_bytes: usize,
    pub(crate) chat:                     ChatGatewayService,
    pub(crate) image:                    ImageGatewayService,
    pub(crate) claude:                   ClaudeMessagesGatewayService,
    pub(crate) gemini:                   GeminiGatewayService,
    pub(crate) raw_openai:               RawOpenAiGatewayService,
}

#[derive(Clone)]
pub(crate) struct AppParams {
    snapshots:                SnapshotStore,
    protocol:                 ProtocolConfig,
    upstream:                 UpstreamConfig,
    auth:                     AuthConfig,
    usage:                    UsageReporter,
    channel_feedback:         ChannelFeedbackReporter,
    request_body_limit_bytes: usize,
}

#[derive(Clone)]
pub(crate) struct SnapshotStore {
    inner:          Arc<ArcSwap<SnapshotState>>,
    // The affinity cache lives here, NOT inside the atomically-swapped
    // SnapshotState, so recorded affinities survive snapshot refresh. Held
    // behind an Arc so a future per-worker/shared deployment can decide the
    // sharing model without touching the routing code.
    affinity_cache: Arc<ChannelAffinityCache>,
}

pub(crate) struct SnapshotState {
    pub(crate) snapshot: IndexedSnapshot,
    pub(crate) models:   Arc<[String]>,
}

impl SnapshotStore {
    fn new(snapshot: GatewaySnapshot, affinity_cache: Arc<ChannelAffinityCache>) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(ArcSwap::from_pointee(SnapshotState::new(snapshot)?)),
            // Shared across workers so SO_REUSEPORT load-balancing does not
            // silently break session stickiness for Codex/Claude CLI affinity.
            affinity_cache,
        })
    }

    pub(crate) fn load(&self) -> arc_swap::Guard<Arc<SnapshotState>> {
        self.inner.load()
    }

    pub(crate) fn affinity_cache(&self) -> &Arc<ChannelAffinityCache> {
        &self.affinity_cache
    }

    pub(crate) fn version(&self) -> u64 {
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
/// runtime, its own `Gateway` / snapshot store, and its own listener socket bound
/// with `SO_REUSEPORT`. The kernel load-balances accepted connections across the
/// sockets. Snapshot state is still shared-nothing per worker; only the channel
/// affinity cache is process-shared so session stickiness survives fan-out.
///
/// This function is synchronous on purpose: it must run OUTSIDE any monoio runtime
/// so it can spawn the per-core runtimes itself. `main` therefore calls it directly
/// rather than from `#[monoio::main]`.
pub fn run_from_config_file(path: &str) -> Result<()> {
    let config = GatewayConfig::load(path)?;
    let worker_count = resolve_worker_count(config.server.workers);
    let listen = config.server.listen;
    // Affinity is process-wide: workers share nothing else on the request hot
    // path, but session stickiness must survive SO_REUSEPORT fan-out.
    let affinity_cache = Arc::new(ChannelAffinityCache::new());
    info!(
        %listen,
        worker_count,
        "starting halolake monoio gateway (thread-per-core); snapshot loads per worker"
    );

    let mut handles = Vec::with_capacity(worker_count);
    for worker_id in 0..worker_count {
        // Each worker gets an independent config clone and its own snapshot
        // store; only the affinity cache is shared across cores.
        let worker_config = config.clone();
        let affinity_cache = affinity_cache.clone();
        let handle = std::thread::Builder::new()
            .name(format!("halolake-gw-{worker_id}"))
            .spawn(move || run_worker(worker_id, worker_config, affinity_cache))
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
fn run_worker(
    worker_id: usize,
    config: GatewayConfig,
    affinity_cache: Arc<ChannelAffinityCache>,
) -> Result<()> {
    let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
        .enable_timer()
        .build()
        .with_context(|| format!("build monoio runtime for worker {worker_id}"))?;
    rt.block_on(run_worker_async(worker_id, config, affinity_cache))
}

async fn run_worker_async(
    worker_id: usize,
    mut config: GatewayConfig,
    affinity_cache: Arc<ChannelAffinityCache>,
) -> Result<()> {
    let snapshot_source = MonoioHttpSnapshotSource::from_config(&config.control)?;
    if let Some(source) = &snapshot_source {
        let snapshot_url = config.control.snapshot_url.as_deref().unwrap_or_default();
        debug!(worker_id, %snapshot_url, "loading gateway snapshot from control api");
        // control-api may still be booting in one-shot Docker; retry briefly.
        let max_attempts = 30u32;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match source
                .clone()
                .call(SnapshotRequest {
                    since_version: None,
                })
                .await
            {
                Ok(SnapshotResponse::Updated { snapshot }) => {
                    info!(
                        worker_id,
                        snapshot_version = snapshot.version,
                        token_count = snapshot.tokens.len(),
                        channel_count = snapshot.channels.len(),
                        attempt,
                        "loaded gateway snapshot from control api"
                    );
                    config.replace_snapshot(snapshot);
                    break;
                }
                Ok(SnapshotResponse::NotModified { version }) => {
                    warn!(
                        worker_id,
                        snapshot_version = version,
                        "control api returned not-modified for initial snapshot load"
                    );
                    break;
                }
                Err(err) if attempt < max_attempts => {
                    warn!(
                        worker_id,
                        attempt,
                        max_attempts,
                        %err,
                        "initial snapshot load failed; retrying"
                    );
                    monoio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(err) => {
                    anyhow::bail!("load gateway snapshot from control api: {err}");
                }
            }
        }
    }
    let snapshot_poll_interval = config
        .control
        .snapshot_poll_interval_ms
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(5));
    let listen = config.server.listen;
    let system_instance_reporter = SystemInstanceReporter::from_config(&config.control)?;
    let gateway = Gateway::try_from_config(config, affinity_cache)?;
    if let Some(source) = snapshot_source
        && !snapshot_poll_interval.is_zero()
    {
        spawn_snapshot_polling(source, gateway.snapshots.clone(), snapshot_poll_interval);
    }
    if worker_id == 0
        && let Some(reporter) = system_instance_reporter
    {
        spawn_system_instance_reporter(reporter);
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
    let listener = TcpListener::bind_with_config(addr, &opts).context("bind gateway listener")?;

    // monolake-style FactoryStack:
    //   Gateway
    //     -> GatewayAppService
    //       -> ConnectionReuseService
    //         -> HttpH1CoreService
    // accept(TcpStream, peer) drives the outer core.
    let conn_svc = FactoryStack::new(())
        .replace(gateway)
        .push(crate::downstream::GatewayAppService::layer())
        .push(crate::downstream::ConnectionReuseService::layer())
        .push(crate::downstream::HttpH1CoreService::layer())
        .make()
        .map_err(|err| anyhow::anyhow!("build downstream http service stack: {err:?}"))?;

    loop {
        let (stream, peer) = listener.accept().await.context("accept connection")?;
        debug!(%peer, "accepted downstream connection");
        let conn_svc = conn_svc.clone();
        monoio::spawn(async move {
            if let Err(err) = conn_svc.call((stream, peer)).await {
                warn!(?err, %peer, "downstream http connection failed");
            }
        });
    }
}

impl GatewayConfig {
    pub fn load(path: &str) -> Result<Self> {
        let data = fs::read_to_string(path).with_context(|| format!("read config {path}"))?;
        let mut config: Self =
            toml::from_str(&data).with_context(|| format!("parse config {path}"))?;
        config.resolve_control_env()?;
        config.resolve_channel_env_keys()?;
        Ok(config)
    }

    fn resolve_control_env(&mut self) -> Result<()> {
        let empty = self
            .control
            .internal_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none();
        if empty {
            let from_env = std::env::var("HALOLAKE_INTERNAL_KEY")
                .ok()
                .or_else(|| std::env::var("HALOLAKE_INTERNAL_SECRET").ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if let Some(key) = from_env {
                self.control.internal_key = Some(key);
            }
        }
        Ok(())
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
            version:          self.version,
            tokens:           self.tokens,
            channels:         self.channels,
            model_mappings:   self.model_mappings,
            channel_affinity: self.channel_affinity,
            group_routing:    self.group_routing,
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
    pub fn try_from_config(
        config: GatewayConfig,
        affinity_cache: Arc<ChannelAffinityCache>,
    ) -> Result<Self> {
        let params = AppParams::try_from_config(config, affinity_cache)?;
        Ok(Self {
            snapshots:                params.param(),
            request_body_limit_bytes: Param::<RequestBodyLimit>::param(&params).0,
            chat:                     ChatGatewayService::from_params(&params),
            image:                    ImageGatewayService::from_params(&params),
            claude:                   ClaudeMessagesGatewayService::from_params(&params),
            gemini:                   GeminiGatewayService::from_params(&params),
            raw_openai:               RawOpenAiGatewayService::from_params(&params),
        })
    }

    pub fn snapshot_version(&self) -> u64 {
        self.snapshots.version()
    }
}

impl AppParams {
    fn try_from_config(
        config: GatewayConfig,
        affinity_cache: Arc<ChannelAffinityCache>,
    ) -> Result<Self> {
        let request_body_limit_bytes = config.server.request_body_limit_bytes;
        let protocol = config.protocol.clone();
        let upstream = config.upstream;
        let auth = config.auth;
        let usage = UsageReporter::from_config(&config.control)?;
        let channel_feedback = ChannelFeedbackReporter::from_config(&config.control)?;
        let snapshot = config.into_snapshot();
        let snapshots = SnapshotStore::new(snapshot, affinity_cache)?;

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
