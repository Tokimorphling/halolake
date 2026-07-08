use std::{
    convert::Infallible,
    error::Error,
    fs,
    net::{SocketAddr, ToSocketAddrs},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use bytes::Bytes;
use certain_map::{Param, ParamRef, ParamSet};
use futures_util::{StreamExt, TryStreamExt};
use halolake_api_contract::{JsonValue, claude, gemini, openai};
use halolake_protocol::{
    ClaudeSseTranslator, GeminiSseToOpenAiTranslator, OpenAiSseToClaudeTranslator,
    OpenAiSseToGeminiTranslator, claude_messages_to_openai_chat,
    claude_messages_to_openai_chat_request, gemini_imagen_to_openai_image_response,
    gemini_request_to_openai_chat, gemini_response_to_openai_chat, openai_chat_to_claude_messages,
    openai_chat_to_claude_messages_response, openai_chat_to_gemini_request,
    openai_chat_to_gemini_response, openai_image_to_gemini_imagen_request,
};
use halolake_router_core::{ChannelConfig, GatewaySnapshot, IndexedSnapshot, Provider, RouteError};
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri, header};
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::{body::Frame, body::Incoming, server::conn::http1, service::service_fn};
use monoio::{
    io::IntoPollIo,
    net::{TcpListener, TcpStream},
};
use monoio_compat::hyper::MonoioIo;
use monoio_http::common::body::{Body as MonoioBody, FixedBody, HttpBody, HttpBodyStream};
use monoio_transports::{
    connectors::{Connector, TcpConnector, TcpTlsAddr, TlsConnector, TlsStream},
    http::{HttpConnection, HttpConnector},
};
use serde::{Deserialize, Serialize};
use service_async::Service;
use tracing::{Instrument, debug, error, info, warn};
use uuid::Uuid;

type BoxError = Box<dyn Error + Send + Sync>;
pub type GatewayBody = UnsyncBoxBody<Bytes, BoxError>;
type HttpUpstream = HttpConnector<TcpConnector, SocketAddr, TcpStream>;
type HttpsUpstream = HttpConnector<TlsConnector<TcpConnector>, TcpTlsAddr, TlsStream<TcpStream>>;

certain_map::certain_map! {
    #[empty(RequestContextEmpty)]
    #[full(FullRequestContext)]
    #[style = "unfilled"]
    #[derive(Clone)]
    pub struct RequestContext {
        request_id: RequestId,
        peer_addr: PeerAddr,
        downstream_protocol: DownstreamProtocol,
        request_auth: RequestAuth,
        route_context: RouteContext,
    }
}

#[derive(Debug, Clone)]
pub struct RequestId(pub String);

#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownstreamProtocol {
    OpenAiChat,
    OpenAiImage,
    OpenAiRaw,
    ClaudeMessages,
    GeminiGenerateContent,
}

#[derive(Debug, Clone)]
pub struct RequestAuth {
    pub user_id: String,
    pub token_id: String,
}

#[derive(Debug, Clone)]
pub struct RouteContext {
    pub channel_id: String,
    pub provider: Provider,
    pub base_url: String,
    pub api_key: String,
    pub requested_model: String,
    pub upstream_model: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub protocol: ProtocolConfig,
    #[serde(default)]
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default = "default_version")]
    pub version: u64,
    #[serde(default)]
    pub tokens: Vec<halolake_router_core::TokenConfig>,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    #[serde(default)]
    pub model_mappings: Vec<halolake_router_core::ModelMapping>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default = "default_body_limit")]
    pub request_body_limit_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            request_body_limit_bytes: default_body_limit(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProtocolConfig {
    #[serde(default = "default_claude_version")]
    pub claude_version: String,
    #[serde(default = "default_true")]
    pub pass_anthropic_beta: bool,
    #[serde(default = "default_gemini_api_version")]
    pub gemini_api_version: String,
}

impl Default for ProtocolConfig {
    fn default() -> Self {
        Self {
            claude_version: default_claude_version(),
            pass_anthropic_beta: true,
            gemini_api_version: default_gemini_api_version(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct UpstreamConfig {
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
    #[serde(default)]
    pub read_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct AuthConfig {
    #[serde(default = "default_true")]
    pub accept_bearer: bool,
    #[serde(default = "default_true")]
    pub accept_x_api_key: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            accept_bearer: true,
            accept_x_api_key: true,
        }
    }
}

#[derive(Clone)]
pub struct Gateway {
    snapshot: Arc<IndexedSnapshot>,
    models: Arc<[String]>,
    request_body_limit_bytes: usize,
    chat: ChatGatewayService,
    image: ImageGatewayService,
    claude: ClaudeMessagesGatewayService,
    gemini: GeminiGatewayService,
    raw_openai: RawOpenAiGatewayService,
}

#[derive(Clone)]
struct ChatGatewayService {
    router: AuthRouteService,
    relay: RelayService,
}

#[derive(Clone)]
struct ImageGatewayService {
    router: AuthRouteService,
    relay: RelayService,
}

#[derive(Clone)]
struct ClaudeMessagesGatewayService {
    router: AuthRouteService,
    relay: RelayService,
}

#[derive(Clone)]
struct GeminiGatewayService {
    router: AuthRouteService,
    relay: RelayService,
}

#[derive(Clone)]
struct RawOpenAiGatewayService {
    router: AuthRouteService,
    relay: RelayService,
}

#[derive(Clone)]
struct AuthRouteService {
    snapshot: Arc<IndexedSnapshot>,
    auth: AuthConfig,
}

#[derive(Clone)]
struct RelayService {
    claude_version: Arc<str>,
    pass_anthropic_beta: bool,
    gemini_api_version: Arc<str>,
    connect_timeout: Option<Duration>,
    http: HttpUpstream,
    https: HttpsUpstream,
}

#[derive(Clone)]
struct AppParams {
    snapshot: Arc<IndexedSnapshot>,
    models: Arc<[String]>,
    protocol: ProtocolConfig,
    upstream: UpstreamConfig,
    auth: AuthConfig,
    request_body_limit_bytes: usize,
}

#[derive(Clone)]
struct ClaudeVersion(Arc<str>);

#[derive(Clone)]
struct GeminiApiVersion(Arc<str>);

#[derive(Clone, Copy)]
struct PassAnthropicBeta(bool);

#[derive(Clone, Copy)]
struct ConnectTimeout(Option<Duration>);

#[derive(Clone, Copy)]
struct UpstreamReadTimeout(Option<Duration>);

#[derive(Clone, Copy)]
struct RequestBodyLimit(usize);

#[derive(Clone, Copy)]
struct GatewayAuthPolicy(AuthConfig);

struct GatewayRequest {
    headers: HeaderMap,
    path: String,
    body: Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiImageRouteKind {
    Generations,
    Edits,
    LegacyEdits,
}

struct ParsedOpenAiImageRequest {
    kind: OpenAiImageRouteKind,
    model: String,
    stream: bool,
    payload: OpenAiImagePayload,
}

enum OpenAiImagePayload {
    Json {
        request: openai::ImageRequest,
        value: JsonValue,
    },
    Multipart,
}

struct RouteLookup {
    token: String,
    requested_model: String,
}

#[derive(Debug, Clone)]
struct RouteParts {
    auth: RequestAuth,
    route: RouteContext,
}

pub async fn run_from_config_file(path: &str) -> Result<()> {
    let config = GatewayConfig::load(path)?;
    let listen = config.server.listen;
    let connect_timeout_ms = config.upstream.connect_timeout_ms;
    let read_timeout_ms = config.upstream.read_timeout_ms;
    let token_count = config.tokens.len();
    let channel_count = config.channels.len();
    let mapping_count = config.model_mappings.len();
    let gateway = Gateway::try_from_config(config)?;
    info!(
        %listen,
        snapshot_version = gateway.snapshot.version(),
        ?connect_timeout_ms,
        ?read_timeout_ms,
        token_count,
        channel_count,
        mapping_count,
        "starting halolake monoio gateway"
    );
    serve(listen, gateway).await
}

pub async fn serve(addr: SocketAddr, gateway: Gateway) -> Result<()> {
    let listener = TcpListener::bind(addr).context("bind gateway listener")?;
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
        }
    }
}

impl Gateway {
    pub fn try_from_config(config: GatewayConfig) -> Result<Self> {
        let params = AppParams::try_from_config(config)?;
        Ok(Self {
            snapshot: params.param(),
            models: params.models.clone(),
            request_body_limit_bytes: Param::<RequestBodyLimit>::param(&params).0,
            chat: ChatGatewayService::from_params(&params),
            image: ImageGatewayService::from_params(&params),
            claude: ClaudeMessagesGatewayService::from_params(&params),
            gemini: GeminiGatewayService::from_params(&params),
            raw_openai: RawOpenAiGatewayService::from_params(&params),
        })
    }

    pub fn snapshot_version(&self) -> u64 {
        self.snapshot.version()
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
                        "snapshot_version": self.snapshot.version(),
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
        let models = self
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
        let snapshot = config.into_snapshot();
        let models = snapshot
            .model_mappings
            .iter()
            .map(|mapping| mapping.requested_model.clone())
            .collect::<Vec<_>>()
            .into();

        Ok(Self {
            snapshot: Arc::new(snapshot.index().context("index gateway snapshot")?),
            models,
            protocol,
            upstream,
            auth,
            request_body_limit_bytes,
        })
    }
}

impl ChatGatewayService {
    fn from_params<C>(params: &C) -> Self
    where
        C: Param<Arc<IndexedSnapshot>>
            + Param<GatewayAuthPolicy>
            + Param<ClaudeVersion>
            + Param<GeminiApiVersion>
            + Param<PassAnthropicBeta>
            + Param<ConnectTimeout>
            + Param<UpstreamReadTimeout>,
    {
        Self {
            router: AuthRouteService {
                snapshot: params.param(),
                auth: Param::<GatewayAuthPolicy>::param(params).0,
            },
            relay: RelayService::from_params(params),
        }
    }
}

impl<CX> Service<(GatewayRequest, CX)> for ChatGatewayService
where
    CX: ParamSet<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
    <CX as ParamSet<RequestAuth>>::Transformed: ParamSet<RouteContext>,
    <<CX as ParamSet<RequestAuth>>::Transformed as ParamSet<RouteContext>>::Transformed:
        ParamRef<RequestAuth> + ParamRef<RouteContext> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    type Response = Response<GatewayBody>;
    type Error = Infallible;

    async fn call(&self, (req, cx): (GatewayRequest, CX)) -> Result<Self::Response, Self::Error> {
        let openai_req: openai::ChatCompletionRequest = match serde_json::from_slice(&req.body) {
            Ok(req) => req,
            Err(err) => {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("invalid OpenAI chat request: {err}"),
                ));
            }
        };
        let token = match self.router.extract_token(&req.headers) {
            Some(token) => token,
            None => {
                return Ok(json_error(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "missing gateway token",
                ));
            }
        };
        let route = match self
            .router
            .call(RouteLookup {
                token,
                requested_model: openai_req.model.clone(),
            })
            .await
        {
            Ok(route) => route,
            Err(err) => return Ok(route_error_response(err)),
        };

        let cx = cx.param_set(route.auth).param_set(route.route);
        self.relay
            .call(OpenAiChatRelayRequest {
                request: openai_req,
                cx,
            })
            .await
    }
}

impl ImageGatewayService {
    fn from_params<C>(params: &C) -> Self
    where
        C: Param<Arc<IndexedSnapshot>>
            + Param<GatewayAuthPolicy>
            + Param<ClaudeVersion>
            + Param<GeminiApiVersion>
            + Param<PassAnthropicBeta>
            + Param<ConnectTimeout>
            + Param<UpstreamReadTimeout>,
    {
        Self {
            router: AuthRouteService {
                snapshot: params.param(),
                auth: Param::<GatewayAuthPolicy>::param(params).0,
            },
            relay: RelayService::from_params(params),
        }
    }
}

impl<CX> Service<(GatewayRequest, CX)> for ImageGatewayService
where
    CX: ParamSet<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
    <CX as ParamSet<RequestAuth>>::Transformed: ParamSet<RouteContext>,
    <<CX as ParamSet<RequestAuth>>::Transformed as ParamSet<RouteContext>>::Transformed:
        ParamRef<RequestAuth> + ParamRef<RouteContext> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    type Response = Response<GatewayBody>;
    type Error = Infallible;

    async fn call(&self, (req, cx): (GatewayRequest, CX)) -> Result<Self::Response, Self::Error> {
        let parsed = match parse_openai_image_request(&req) {
            Ok(parsed) => parsed,
            Err(resp) => return Ok(resp),
        };
        let token = match self.router.extract_token(&req.headers) {
            Some(token) => token,
            None => {
                return Ok(json_error(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "missing gateway token",
                ));
            }
        };
        let route = match self
            .router
            .call(RouteLookup {
                token,
                requested_model: parsed.model.clone(),
            })
            .await
        {
            Ok(route) => route,
            Err(err) => return Ok(route_error_response(err)),
        };

        let cx = cx.param_set(route.auth).param_set(route.route);
        self.relay
            .call(OpenAiImageRelayRequest {
                kind: parsed.kind,
                stream: parsed.stream,
                payload: parsed.payload,
                body: req.body,
                downstream_headers: req.headers,
                path: req.path,
                cx,
            })
            .await
    }
}

impl ClaudeMessagesGatewayService {
    fn from_params<C>(params: &C) -> Self
    where
        C: Param<Arc<IndexedSnapshot>>
            + Param<GatewayAuthPolicy>
            + Param<ClaudeVersion>
            + Param<GeminiApiVersion>
            + Param<PassAnthropicBeta>
            + Param<ConnectTimeout>
            + Param<UpstreamReadTimeout>,
    {
        Self {
            router: AuthRouteService {
                snapshot: params.param(),
                auth: Param::<GatewayAuthPolicy>::param(params).0,
            },
            relay: RelayService::from_params(params),
        }
    }
}

impl<CX> Service<(GatewayRequest, CX)> for ClaudeMessagesGatewayService
where
    CX: ParamSet<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
    <CX as ParamSet<RequestAuth>>::Transformed: ParamSet<RouteContext>,
    <<CX as ParamSet<RequestAuth>>::Transformed as ParamSet<RouteContext>>::Transformed:
        ParamRef<RequestAuth> + ParamRef<RouteContext> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    type Response = Response<GatewayBody>;
    type Error = Infallible;

    async fn call(&self, (req, cx): (GatewayRequest, CX)) -> Result<Self::Response, Self::Error> {
        let value: JsonValue = match serde_json::from_slice(&req.body) {
            Ok(req) => req,
            Err(err) => {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("invalid Claude messages request: {err}"),
                ));
            }
        };
        let Some(model) = value
            .get("model")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
        else {
            return Ok(json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Claude messages body must contain a string model field",
            ));
        };
        let token = match self.router.extract_token(&req.headers) {
            Some(token) => token,
            None => {
                return Ok(json_error(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "missing gateway token",
                ));
            }
        };
        let route = match self
            .router
            .call(RouteLookup {
                token,
                requested_model: model,
            })
            .await
        {
            Ok(route) => route,
            Err(err) => return Ok(route_error_response(err)),
        };
        let cx = cx.param_set(route.auth).param_set(route.route);
        self.relay
            .call(ClaudeMessagesRelayRequest {
                value,
                downstream_headers: req.headers,
                cx,
            })
            .await
    }
}

impl GeminiGatewayService {
    fn from_params<C>(params: &C) -> Self
    where
        C: Param<Arc<IndexedSnapshot>>
            + Param<GatewayAuthPolicy>
            + Param<ClaudeVersion>
            + Param<GeminiApiVersion>
            + Param<PassAnthropicBeta>
            + Param<ConnectTimeout>
            + Param<UpstreamReadTimeout>,
    {
        Self {
            router: AuthRouteService {
                snapshot: params.param(),
                auth: Param::<GatewayAuthPolicy>::param(params).0,
            },
            relay: RelayService::from_params(params),
        }
    }
}

impl<CX> Service<(GatewayRequest, CX)> for GeminiGatewayService
where
    CX: ParamSet<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
    <CX as ParamSet<RequestAuth>>::Transformed: ParamSet<RouteContext>,
    <<CX as ParamSet<RequestAuth>>::Transformed as ParamSet<RouteContext>>::Transformed:
        ParamRef<RequestAuth> + ParamRef<RouteContext> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    type Response = Response<GatewayBody>;
    type Error = Infallible;

    async fn call(&self, (req, cx): (GatewayRequest, CX)) -> Result<Self::Response, Self::Error> {
        let Some((requested_model, stream)) = parse_gemini_generate_content_path(&req.path) else {
            return Ok(json_error(
                StatusCode::NOT_FOUND,
                "not_found",
                "Gemini route not found",
            ));
        };
        let token = match self.router.extract_token(&req.headers) {
            Some(token) => token,
            None => {
                return Ok(json_error(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "missing gateway token",
                ));
            }
        };
        let route = match self
            .router
            .call(RouteLookup {
                token,
                requested_model,
            })
            .await
        {
            Ok(route) => route,
            Err(err) => return Ok(route_error_response(err)),
        };

        let cx = cx.param_set(route.auth).param_set(route.route);
        self.relay
            .call(GeminiNativeRelayRequest {
                path: req.path,
                stream,
                body: req.body,
                downstream_headers: req.headers,
                cx,
            })
            .await
    }
}

impl RawOpenAiGatewayService {
    fn from_params<C>(params: &C) -> Self
    where
        C: Param<Arc<IndexedSnapshot>>
            + Param<GatewayAuthPolicy>
            + Param<ClaudeVersion>
            + Param<GeminiApiVersion>
            + Param<PassAnthropicBeta>
            + Param<ConnectTimeout>
            + Param<UpstreamReadTimeout>,
    {
        Self {
            router: AuthRouteService {
                snapshot: params.param(),
                auth: Param::<GatewayAuthPolicy>::param(params).0,
            },
            relay: RelayService::from_params(params),
        }
    }
}

impl<CX> Service<(GatewayRequest, CX)> for RawOpenAiGatewayService
where
    CX: ParamSet<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
    <CX as ParamSet<RequestAuth>>::Transformed: ParamSet<RouteContext>,
    <<CX as ParamSet<RequestAuth>>::Transformed as ParamSet<RouteContext>>::Transformed:
        ParamRef<RequestAuth> + ParamRef<RouteContext> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    type Response = Response<GatewayBody>;
    type Error = Infallible;

    async fn call(&self, (req, cx): (GatewayRequest, CX)) -> Result<Self::Response, Self::Error> {
        let mut value: JsonValue = match serde_json::from_slice(&req.body) {
            Ok(req) => req,
            Err(err) => {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("invalid OpenAI request body: {err}"),
                ));
            }
        };
        let Some(model) = value
            .get("model")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
        else {
            return Ok(json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "request body must contain a string model field",
            ));
        };
        let token = match self.router.extract_token(&req.headers) {
            Some(token) => token,
            None => {
                return Ok(json_error(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "missing gateway token",
                ));
            }
        };
        let route = match self
            .router
            .call(RouteLookup {
                token,
                requested_model: model,
            })
            .await
        {
            Ok(route) => route,
            Err(err) => return Ok(route_error_response(err)),
        };
        value["model"] = JsonValue::String(route.route.upstream_model.clone());
        let body = match serde_json::to_vec(&value) {
            Ok(body) => Bytes::from(body),
            Err(err) => {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("failed to rewrite request model: {err}"),
                ));
            }
        };

        let cx = cx.param_set(route.auth).param_set(route.route);
        self.relay
            .call(OpenAiPassthroughRelayRequest {
                path: req.path,
                body,
                downstream_headers: req.headers,
                cx,
            })
            .await
    }
}

impl Service<RouteLookup> for AuthRouteService {
    type Response = RouteParts;
    type Error = RouteError;

    async fn call(&self, lookup: RouteLookup) -> Result<Self::Response, Self::Error> {
        let auth = self.snapshot.authenticate(&lookup.token)?;
        let route = self.snapshot.route(&auth, &lookup.requested_model)?;
        Ok(RouteParts {
            auth: RequestAuth {
                user_id: route.user_id.to_string(),
                token_id: auth.token.id.clone(),
            },
            route: RouteContext {
                channel_id: route.channel.id.clone(),
                provider: route.channel.provider,
                base_url: route.channel.base_url.clone(),
                api_key: route.channel.api_key.clone(),
                requested_model: route.requested_model.to_string(),
                upstream_model: route.upstream_model.to_string(),
            },
        })
    }
}

impl AuthRouteService {
    fn extract_token(&self, headers: &HeaderMap) -> Option<String> {
        if self.auth.accept_bearer {
            if let Some(token) = bearer_token(headers) {
                return Some(token);
            }
        }
        if self.auth.accept_x_api_key {
            return headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(str::to_string);
        }
        None
    }
}

impl RelayService {
    fn from_params<C>(params: &C) -> Self
    where
        C: Param<ClaudeVersion>
            + Param<GeminiApiVersion>
            + Param<PassAnthropicBeta>
            + Param<ConnectTimeout>
            + Param<UpstreamReadTimeout>,
    {
        let read_timeout = <C as Param<UpstreamReadTimeout>>::param(params).0;
        let mut http = HttpUpstream::build_tcp_http1_only();
        http.set_read_timeout(read_timeout);
        let mut https = HttpsUpstream::default();
        https.set_read_timeout(read_timeout);

        Self {
            claude_version: <C as Param<ClaudeVersion>>::param(params).0,
            pass_anthropic_beta: <C as Param<PassAnthropicBeta>>::param(params).0,
            gemini_api_version: <C as Param<GeminiApiVersion>>::param(params).0,
            connect_timeout: <C as Param<ConnectTimeout>>::param(params).0,
            http,
            https,
        }
    }

    async fn send_upstream(&self, uri: Uri, req: Request<HttpBody>) -> Result<Response<HttpBody>> {
        let method = req.method().clone();
        let path = req.uri().to_string();
        let version = req.version();
        let scheme = uri.scheme_str().unwrap_or("http");
        let host = uri.host().unwrap_or("<missing-host>").to_string();
        let port = uri.port_u16();
        let body_hint = req.body().stream_hint();
        debug!(
            %method,
            %scheme,
            %host,
            ?port,
            %path,
            ?version,
            ?body_hint,
            connect_timeout_ms = ?self.connect_timeout.map(|d| d.as_millis()),
            "upstream request prepared"
        );
        if uri.scheme() == Some(&http::uri::Scheme::HTTPS) {
            let key: TcpTlsAddr = uri.try_into().context("invalid https upstream uri")?;
            debug!(%host, ?port, "acquiring https upstream connection");
            let connect = self.https.connect(key);
            let mut conn = timeout_opt(self.connect_timeout, connect)
                .await
                .with_context(|| format!("acquire https upstream connection {host}:{port:?}"))??;
            match &conn {
                HttpConnection::Http1(conn) => {
                    debug!(
                        %host,
                        ?port,
                        protocol = "http/1.1",
                        http1_reused = conn.is_reused(),
                        "https upstream connection acquired"
                    );
                }
                HttpConnection::Http2(_) => {
                    debug!(
                        %host,
                        ?port,
                        protocol = "h2",
                        "https upstream connection acquired"
                    );
                }
            }
            let (resp, can_reuse) = conn.send_request(req).await;
            let resp = resp.with_context(|| format!("send https upstream request {host}{path}"))?;
            debug!(
                %host,
                %path,
                status = %resp.status(),
                response_version = ?resp.version(),
                can_reuse,
                "https upstream responded"
            );
            Ok(resp)
        } else {
            let host = uri.host().context("http upstream uri missing host")?;
            let port = uri.port_u16().unwrap_or(80);
            let addr = format!("{host}:{port}")
                .to_socket_addrs()
                .with_context(|| format!("resolve http upstream {host}:{port}"))?
                .next()
                .context("http upstream resolved no addresses")?;
            debug!(%host, port, %addr, "acquiring http upstream connection");
            let connect = self.http.connect(addr);
            let mut conn = timeout_opt(self.connect_timeout, connect)
                .await
                .with_context(|| format!("acquire http upstream connection {host}:{port}"))??;
            match &conn {
                HttpConnection::Http1(conn) => {
                    debug!(
                        %host,
                        port,
                        %addr,
                        protocol = "http/1.1",
                        http1_reused = conn.is_reused(),
                        "http upstream connection acquired"
                    );
                }
                HttpConnection::Http2(_) => {
                    debug!(
                        %host,
                        port,
                        %addr,
                        protocol = "h2",
                        "http upstream connection acquired"
                    );
                }
            }
            let (resp, can_reuse) = conn.send_request(req).await;
            let resp = resp.with_context(|| format!("send http upstream request {host}{path}"))?;
            debug!(
                %host,
                %path,
                status = %resp.status(),
                response_version = ?resp.version(),
                can_reuse,
                "http upstream responded"
            );
            Ok(resp)
        }
    }
}

struct OpenAiChatRelayRequest<CX> {
    request: openai::ChatCompletionRequest,
    cx: CX,
}

impl<CX> Service<OpenAiChatRelayRequest<CX>> for RelayService
where
    CX: ParamRef<RouteContext> + ParamRef<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    type Response = Response<GatewayBody>;
    type Error = Infallible;

    async fn call(&self, req: OpenAiChatRelayRequest<CX>) -> Result<Self::Response, Self::Error> {
        let route = ParamRef::<RouteContext>::param_ref(&req.cx).clone();
        debug_relay(&req.cx, req.request.is_stream());

        match route.provider {
            Provider::Claude => {
                let claude_req =
                    match openai_chat_to_claude_messages(&req.request, &route.upstream_model) {
                        Ok(req) => req,
                        Err(err) => {
                            return Ok(json_error(
                                StatusCode::BAD_REQUEST,
                                "invalid_request_error",
                                &err.to_string(),
                            ));
                        }
                    };
                let upstream = match self
                    .send_claude_json(&route, &HeaderMap::new(), &claude_req, "/v1/messages")
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => {
                        error!(
                            ?err,
                            channel_id = %route.channel_id,
                            base_url = %route.base_url,
                            upstream_model = %route.upstream_model,
                            "Claude upstream request failed"
                        );
                        return Ok(json_error(
                            StatusCode::BAD_GATEWAY,
                            "bad_gateway",
                            &err.to_string(),
                        ));
                    }
                };
                if !upstream.status().is_success() {
                    return Ok(upstream_error_response(upstream).await);
                }
                if req.request.is_stream() {
                    Ok(stream_claude_as_openai(upstream, route.requested_model))
                } else {
                    Ok(buffered_claude_as_openai(upstream, route.requested_model).await)
                }
            }
            Provider::OpenAi => {
                let mut request = req.request;
                request.model = route.upstream_model.clone();
                let body = match serde_json::to_vec(&request) {
                    Ok(body) => Bytes::from(body),
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &format!("failed to rewrite OpenAI chat request: {err}"),
                        ));
                    }
                };
                self.openai_passthrough(route, "/v1/chat/completions", body, &HeaderMap::new())
                    .await
            }
            Provider::Gemini => {
                let gemini_req = match openai_chat_to_gemini_request(&req.request) {
                    Ok(req) => req,
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &err.to_string(),
                        ));
                    }
                };
                let upstream_path = self
                    .gemini_generate_content_path(&route.upstream_model, req.request.is_stream());
                let upstream = match self
                    .send_gemini_json(&route, &HeaderMap::new(), &gemini_req, &upstream_path)
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => {
                        error!(
                            ?err,
                            channel_id = %route.channel_id,
                            base_url = %route.base_url,
                            upstream_model = %route.upstream_model,
                            "Gemini upstream request failed"
                        );
                        return Ok(json_error(
                            StatusCode::BAD_GATEWAY,
                            "bad_gateway",
                            &err.to_string(),
                        ));
                    }
                };
                if !upstream.status().is_success() {
                    return Ok(upstream_error_response(upstream).await);
                }
                if req.request.is_stream() {
                    Ok(stream_gemini_as_openai(upstream, route.requested_model))
                } else {
                    Ok(buffered_gemini_as_openai(upstream, route.requested_model).await)
                }
            }
        }
    }
}

struct ClaudeMessagesRelayRequest<CX> {
    value: JsonValue,
    downstream_headers: HeaderMap,
    cx: CX,
}

impl<CX> Service<ClaudeMessagesRelayRequest<CX>> for RelayService
where
    CX: ParamRef<RouteContext> + ParamRef<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    type Response = Response<GatewayBody>;
    type Error = Infallible;

    async fn call(
        &self,
        req: ClaudeMessagesRelayRequest<CX>,
    ) -> Result<Self::Response, Self::Error> {
        let route = ParamRef::<RouteContext>::param_ref(&req.cx).clone();
        let is_stream = req
            .value
            .get("stream")
            .and_then(JsonValue::as_bool)
            .unwrap_or_else(|| is_stream_like(&req.downstream_headers));
        debug_relay(&req.cx, is_stream);

        match route.provider {
            Provider::Claude => {
                let mut value = req.value;
                value["model"] = JsonValue::String(route.upstream_model.clone());
                let body = match serde_json::to_vec(&value) {
                    Ok(body) => Bytes::from(body),
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &format!("failed to rewrite Claude messages model: {err}"),
                        ));
                    }
                };
                let upstream = match self
                    .send_claude_body(&route, &req.downstream_headers, body, "/v1/messages")
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => {
                        error!(
                            ?err,
                            channel_id = %route.channel_id,
                            base_url = %route.base_url,
                            upstream_model = %route.upstream_model,
                            "Claude upstream request failed"
                        );
                        return Ok(json_error(
                            StatusCode::BAD_GATEWAY,
                            "bad_gateway",
                            &err.to_string(),
                        ));
                    }
                };
                Ok(upstream_to_response(upstream))
            }
            Provider::OpenAi => {
                let claude_req: claude::MessagesRequest = match serde_json::from_value(req.value) {
                    Ok(req) => req,
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &format!("invalid Claude messages request: {err}"),
                        ));
                    }
                };
                let openai_req = match claude_messages_to_openai_chat_request(
                    &claude_req,
                    &route.upstream_model,
                ) {
                    Ok(req) => req,
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &err.to_string(),
                        ));
                    }
                };
                let upstream = match self
                    .send_openai_json(
                        &route,
                        &HeaderMap::new(),
                        &openai_req,
                        "/v1/chat/completions",
                    )
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => {
                        error!(
                            ?err,
                            channel_id = %route.channel_id,
                            base_url = %route.base_url,
                            upstream_model = %route.upstream_model,
                            "OpenAI upstream request failed"
                        );
                        return Ok(json_error(
                            StatusCode::BAD_GATEWAY,
                            "bad_gateway",
                            &err.to_string(),
                        ));
                    }
                };
                if !upstream.status().is_success() {
                    return Ok(upstream_error_response(upstream).await);
                }
                if openai_req.is_stream() {
                    Ok(stream_openai_as_claude(upstream, route.requested_model))
                } else {
                    Ok(buffered_openai_as_claude(upstream, route.requested_model).await)
                }
            }
            Provider::Gemini => {
                let claude_req: claude::MessagesRequest = match serde_json::from_value(req.value) {
                    Ok(req) => req,
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &format!("invalid Claude messages request: {err}"),
                        ));
                    }
                };
                let openai_req = match claude_messages_to_openai_chat_request(
                    &claude_req,
                    &route.upstream_model,
                ) {
                    Ok(req) => req,
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &err.to_string(),
                        ));
                    }
                };
                let gemini_req = match openai_chat_to_gemini_request(&openai_req) {
                    Ok(req) => req,
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &err.to_string(),
                        ));
                    }
                };
                let upstream_path = self
                    .gemini_generate_content_path(&route.upstream_model, openai_req.is_stream());
                let upstream = match self
                    .send_gemini_json(&route, &HeaderMap::new(), &gemini_req, &upstream_path)
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => {
                        error!(
                            ?err,
                            channel_id = %route.channel_id,
                            base_url = %route.base_url,
                            upstream_model = %route.upstream_model,
                            "Gemini upstream request failed"
                        );
                        return Ok(json_error(
                            StatusCode::BAD_GATEWAY,
                            "bad_gateway",
                            &err.to_string(),
                        ));
                    }
                };
                if !upstream.status().is_success() {
                    return Ok(upstream_error_response(upstream).await);
                }
                if openai_req.is_stream() {
                    Ok(stream_gemini_as_claude(upstream, route.requested_model))
                } else {
                    Ok(buffered_gemini_as_claude(upstream, route.requested_model).await)
                }
            }
        }
    }
}

struct OpenAiImageRelayRequest<CX> {
    kind: OpenAiImageRouteKind,
    stream: bool,
    payload: OpenAiImagePayload,
    body: Bytes,
    downstream_headers: HeaderMap,
    path: String,
    cx: CX,
}

struct OpenAiPassthroughRelayRequest<CX> {
    path: String,
    body: Bytes,
    downstream_headers: HeaderMap,
    cx: CX,
}

struct GeminiNativeRelayRequest<CX> {
    path: String,
    stream: bool,
    body: Bytes,
    downstream_headers: HeaderMap,
    cx: CX,
}

impl<CX> Service<OpenAiImageRelayRequest<CX>> for RelayService
where
    CX: ParamRef<RouteContext> + ParamRef<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    type Response = Response<GatewayBody>;
    type Error = Infallible;

    async fn call(&self, req: OpenAiImageRelayRequest<CX>) -> Result<Self::Response, Self::Error> {
        let route = ParamRef::<RouteContext>::param_ref(&req.cx).clone();
        debug_relay(&req.cx, req.stream);

        match route.provider {
            Provider::OpenAi => {
                let body = match req.payload {
                    OpenAiImagePayload::Json { mut value, .. } => {
                        value["model"] = JsonValue::String(route.upstream_model.clone());
                        match serde_json::to_vec(&value) {
                            Ok(body) => Bytes::from(body),
                            Err(err) => {
                                return Ok(json_error(
                                    StatusCode::BAD_REQUEST,
                                    "invalid_request_error",
                                    &format!("failed to rewrite OpenAI image request: {err}"),
                                ));
                            }
                        }
                    }
                    OpenAiImagePayload::Multipart => match rewrite_multipart_model_field(
                        req.body,
                        &req.downstream_headers,
                        &route.upstream_model,
                    ) {
                        Ok(body) => body,
                        Err(err) => {
                            return Ok(json_error(
                                StatusCode::BAD_REQUEST,
                                "invalid_request_error",
                                &err,
                            ));
                        }
                    },
                };
                let content_type = content_type_header(&req.downstream_headers).cloned();
                let upstream = match self
                    .send_openai_body_with_content_type(
                        &route,
                        &req.downstream_headers,
                        body,
                        &req.path,
                        content_type,
                    )
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => {
                        error!(
                            ?err,
                            channel_id = %route.channel_id,
                            base_url = %route.base_url,
                            upstream_model = %route.upstream_model,
                            "OpenAI image upstream request failed"
                        );
                        return Ok(json_error(
                            StatusCode::BAD_GATEWAY,
                            "bad_gateway",
                            &err.to_string(),
                        ));
                    }
                };
                if req.stream
                    && upstream.status().is_success()
                    && !is_event_stream_response(&upstream)
                {
                    Ok(openai_image_json_as_stream(upstream).await)
                } else {
                    Ok(upstream_to_response(upstream))
                }
            }
            Provider::Gemini => {
                if req.kind != OpenAiImageRouteKind::Generations {
                    return Ok(json_error(
                        StatusCode::BAD_GATEWAY,
                        "routing_error",
                        "OpenAI image edits to Gemini upstream is not implemented in new-api",
                    ));
                }
                let OpenAiImagePayload::Json { request, .. } = req.payload else {
                    return Ok(json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        "Gemini image generation requires a JSON OpenAI image request",
                    ));
                };
                let gemini_req =
                    match openai_image_to_gemini_imagen_request(&request, &route.upstream_model) {
                        Ok(req) => req,
                        Err(err) => {
                            return Ok(json_error(
                                StatusCode::BAD_REQUEST,
                                "invalid_request_error",
                                &err.to_string(),
                            ));
                        }
                    };
                let upstream_path = self.gemini_imagen_predict_path(&route.upstream_model);
                let upstream = match self
                    .send_gemini_json(&route, &HeaderMap::new(), &gemini_req, &upstream_path)
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => {
                        error!(
                            ?err,
                            channel_id = %route.channel_id,
                            base_url = %route.base_url,
                            upstream_model = %route.upstream_model,
                            "Gemini image upstream request failed"
                        );
                        return Ok(json_error(
                            StatusCode::BAD_GATEWAY,
                            "bad_gateway",
                            &err.to_string(),
                        ));
                    }
                };
                if !upstream.status().is_success() {
                    return Ok(upstream_error_response(upstream).await);
                }
                Ok(buffered_gemini_imagen_as_openai(upstream).await)
            }
            Provider::Claude => Ok(json_error(
                StatusCode::BAD_GATEWAY,
                "routing_error",
                "OpenAI image requests to Claude upstream are not implemented in new-api",
            )),
        }
    }
}

impl<CX> Service<OpenAiPassthroughRelayRequest<CX>> for RelayService
where
    CX: ParamRef<RouteContext> + ParamRef<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    type Response = Response<GatewayBody>;
    type Error = Infallible;

    async fn call(
        &self,
        req: OpenAiPassthroughRelayRequest<CX>,
    ) -> Result<Self::Response, Self::Error> {
        let route = ParamRef::<RouteContext>::param_ref(&req.cx).clone();
        debug_relay(&req.cx, is_stream_like(&req.downstream_headers));
        if route.provider != Provider::OpenAi {
            return Ok(json_error(
                StatusCode::BAD_GATEWAY,
                "routing_error",
                "raw OpenAI passthrough requires an OpenAI provider channel",
            ));
        }
        self.openai_passthrough(route, &req.path, req.body, &req.downstream_headers)
            .await
    }
}

impl<CX> Service<GeminiNativeRelayRequest<CX>> for RelayService
where
    CX: ParamRef<RouteContext> + ParamRef<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    type Response = Response<GatewayBody>;
    type Error = Infallible;

    async fn call(&self, req: GeminiNativeRelayRequest<CX>) -> Result<Self::Response, Self::Error> {
        let route = ParamRef::<RouteContext>::param_ref(&req.cx).clone();
        debug_relay(&req.cx, req.stream);

        match route.provider {
            Provider::Gemini => {
                let upstream_path = rewrite_gemini_model_in_path(&req.path, &route.upstream_model);
                let upstream = match self
                    .send_gemini_body(&route, &req.downstream_headers, req.body, &upstream_path)
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => {
                        error!(
                            ?err,
                            channel_id = %route.channel_id,
                            base_url = %route.base_url,
                            upstream_model = %route.upstream_model,
                            "Gemini upstream request failed"
                        );
                        return Ok(json_error(
                            StatusCode::BAD_GATEWAY,
                            "bad_gateway",
                            &err.to_string(),
                        ));
                    }
                };
                Ok(upstream_to_response(upstream))
            }
            Provider::OpenAi => {
                let gemini_req: gemini::GeminiChatRequest = match serde_json::from_slice(&req.body)
                {
                    Ok(req) => req,
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &format!("invalid Gemini generateContent request: {err}"),
                        ));
                    }
                };
                let openai_req = match gemini_request_to_openai_chat(
                    &gemini_req,
                    &route.upstream_model,
                    req.stream,
                ) {
                    Ok(req) => req,
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &err.to_string(),
                        ));
                    }
                };
                let upstream = match self
                    .send_openai_json(
                        &route,
                        &HeaderMap::new(),
                        &openai_req,
                        "/v1/chat/completions",
                    )
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => {
                        error!(
                            ?err,
                            channel_id = %route.channel_id,
                            base_url = %route.base_url,
                            upstream_model = %route.upstream_model,
                            "OpenAI upstream request failed"
                        );
                        return Ok(json_error(
                            StatusCode::BAD_GATEWAY,
                            "bad_gateway",
                            &err.to_string(),
                        ));
                    }
                };
                if !upstream.status().is_success() {
                    return Ok(upstream_error_response(upstream).await);
                }
                if req.stream {
                    Ok(stream_openai_as_gemini(upstream))
                } else {
                    Ok(buffered_openai_as_gemini(upstream).await)
                }
            }
            Provider::Claude => Ok(json_error(
                StatusCode::BAD_GATEWAY,
                "routing_error",
                "Gemini generateContent to Claude upstream is not implemented in new-api",
            )),
        }
    }
}

impl RelayService {
    async fn send_claude_json<T: Serialize>(
        &self,
        route: &RouteContext,
        downstream_headers: &HeaderMap,
        payload: &T,
        path: &str,
    ) -> Result<Response<HttpBody>> {
        let body = serde_json::to_vec(payload).context("serialize Claude request")?;
        self.send_claude_body(route, downstream_headers, Bytes::from(body), path)
            .await
    }

    async fn send_claude_body(
        &self,
        route: &RouteContext,
        downstream_headers: &HeaderMap,
        body: Bytes,
        path: &str,
    ) -> Result<Response<HttpBody>> {
        let uri = upstream_uri(&route.base_url, path)?;
        debug!(
            channel_id = %route.channel_id,
            provider = ?route.provider,
            base_url = %route.base_url,
            path,
            body_len = body.len(),
            pass_anthropic_beta = self.pass_anthropic_beta,
            has_anthropic_beta = downstream_headers.contains_key("anthropic-beta"),
            anthropic_version = anthropic_version(downstream_headers, &self.claude_version),
            "building Claude upstream request"
        );
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(path_and_query(&uri, path))
            .header(header::HOST, authority(&uri)?)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(
                "anthropic-version",
                anthropic_version(downstream_headers, &self.claude_version),
            )
            .header("x-api-key", route.api_key.as_str());

        if self.pass_anthropic_beta {
            if let Some(beta) = downstream_headers.get("anthropic-beta") {
                builder = builder.header("anthropic-beta", beta);
            }
        }

        let req = builder
            .body(HttpBody::fixed_body(Some(body)))
            .context("build Claude upstream request")?;
        self.send_upstream(uri, req).await
    }

    async fn send_openai_json<T: Serialize>(
        &self,
        route: &RouteContext,
        downstream_headers: &HeaderMap,
        payload: &T,
        path: &str,
    ) -> Result<Response<HttpBody>> {
        let body = serde_json::to_vec(payload).context("serialize OpenAI request")?;
        self.send_openai_body(route, downstream_headers, Bytes::from(body), path)
            .await
    }

    async fn send_openai_body(
        &self,
        route: &RouteContext,
        downstream_headers: &HeaderMap,
        body: Bytes,
        path: &str,
    ) -> Result<Response<HttpBody>> {
        self.send_openai_body_with_content_type(route, downstream_headers, body, path, None)
            .await
    }

    async fn send_openai_body_with_content_type(
        &self,
        route: &RouteContext,
        downstream_headers: &HeaderMap,
        body: Bytes,
        path: &str,
        content_type: Option<HeaderValue>,
    ) -> Result<Response<HttpBody>> {
        let uri = upstream_uri(&route.base_url, path)?;
        let content_type =
            content_type.unwrap_or_else(|| HeaderValue::from_static("application/json"));
        debug!(
            channel_id = %route.channel_id,
            provider = ?route.provider,
            base_url = %route.base_url,
            path,
            body_len = body.len(),
            accept = ?downstream_headers.get(header::ACCEPT),
            content_type = ?content_type,
            has_openai_beta = downstream_headers.contains_key("OpenAI-Beta"),
            "building OpenAI upstream request"
        );
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(path_and_query(&uri, path))
            .header(header::HOST, authority(&uri)?)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::AUTHORIZATION, format!("Bearer {}", route.api_key));
        if let Some(accept) = downstream_headers.get(header::ACCEPT) {
            builder = builder.header(header::ACCEPT, accept);
        }
        if let Some(beta) = downstream_headers.get("OpenAI-Beta") {
            builder = builder.header("OpenAI-Beta", beta);
        }
        let req = builder
            .body(HttpBody::fixed_body(Some(body)))
            .context("build OpenAI upstream request")?;
        self.send_upstream(uri, req).await
    }

    fn gemini_generate_content_path(&self, model: &str, stream: bool) -> String {
        if stream {
            format!(
                "/{}/models/{}:streamGenerateContent?alt=sse",
                self.gemini_api_version, model
            )
        } else {
            format!(
                "/{}/models/{}:generateContent",
                self.gemini_api_version, model
            )
        }
    }

    fn gemini_imagen_predict_path(&self, model: &str) -> String {
        format!("/{}/models/{}:predict", self.gemini_api_version, model)
    }

    async fn send_gemini_json<T: Serialize>(
        &self,
        route: &RouteContext,
        downstream_headers: &HeaderMap,
        payload: &T,
        path: &str,
    ) -> Result<Response<HttpBody>> {
        let body = serde_json::to_vec(payload).context("serialize Gemini request")?;
        self.send_gemini_body(route, downstream_headers, Bytes::from(body), path)
            .await
    }

    async fn send_gemini_body(
        &self,
        route: &RouteContext,
        downstream_headers: &HeaderMap,
        body: Bytes,
        path: &str,
    ) -> Result<Response<HttpBody>> {
        let uri = upstream_uri(&route.base_url, path)?;
        debug!(
            channel_id = %route.channel_id,
            provider = ?route.provider,
            base_url = %route.base_url,
            path,
            body_len = body.len(),
            accept = ?downstream_headers.get(header::ACCEPT),
            "building Gemini upstream request"
        );
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(path_and_query(&uri, path))
            .header(header::HOST, authority(&uri)?)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header("x-goog-api-key", route.api_key.as_str());
        if let Some(accept) = downstream_headers.get(header::ACCEPT) {
            builder = builder.header(header::ACCEPT, accept);
        }
        let req = builder
            .body(HttpBody::fixed_body(Some(body)))
            .context("build Gemini upstream request")?;
        self.send_upstream(uri, req).await
    }

    async fn openai_passthrough(
        &self,
        route: RouteContext,
        path: &str,
        body: Bytes,
        downstream_headers: &HeaderMap,
    ) -> Result<Response<GatewayBody>, Infallible> {
        match self
            .send_openai_body(&route, downstream_headers, body, path)
            .await
        {
            Ok(resp) => Ok(upstream_to_response(resp)),
            Err(err) => {
                error!(
                    ?err,
                    channel_id = %route.channel_id,
                    base_url = %route.base_url,
                    upstream_model = %route.upstream_model,
                    "OpenAI upstream request failed"
                );
                Ok(json_error(
                    StatusCode::BAD_GATEWAY,
                    "bad_gateway",
                    &err.to_string(),
                ))
            }
        }
    }
}

impl Param<Arc<IndexedSnapshot>> for AppParams {
    fn param(&self) -> Arc<IndexedSnapshot> {
        self.snapshot.clone()
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

async fn buffered_claude_as_openai(
    mut upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading Claude response: {err}"),
            );
        }
    };
    let claude_resp: claude::MessagesResponse = match serde_json::from_slice(&payload) {
        Ok(resp) => resp,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid Claude response: {err}"),
            );
        }
    };
    json_response(
        StatusCode::OK,
        claude_messages_to_openai_chat(claude_resp, requested_model),
    )
}

fn stream_claude_as_openai(
    upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let mut translator = ClaudeSseTranslator::new(requested_model);
    let mut decoder = SseBuffer::default();
    let stream = HttpBodyStream::from(upstream.into_body())
        .map_err(|err| std::io::Error::other(err.to_string()))
        .map(move |chunk| {
            chunk.map(|bytes| {
                let mut out = Vec::new();
                for payload in decoder.push(&bytes) {
                    match translator.translate_sse_payload(&payload) {
                        Ok(events) => {
                            for event in events {
                                write_sse_data(&mut out, &event);
                            }
                        }
                        Err(err) => {
                            warn!(?err, "failed to translate Claude SSE event");
                        }
                    }
                }
                Frame::data(Bytes::from(out))
            })
        })
        .map_err(|err| -> BoxError { Box::new(err) });

    response_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(http_body_util::BodyExt::boxed_unsync(StreamBody::new(
            stream,
        )))
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        })
}

async fn buffered_openai_as_claude(
    mut upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading OpenAI response: {err}"),
            );
        }
    };
    let openai_resp: openai::ChatCompletionResponse = match serde_json::from_slice(&payload) {
        Ok(resp) => resp,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid OpenAI response: {err}"),
            );
        }
    };
    json_response(
        StatusCode::OK,
        openai_chat_to_claude_messages_response(openai_resp, requested_model),
    )
}

fn stream_openai_as_claude(
    upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let mut translator = OpenAiSseToClaudeTranslator::new(requested_model);
    let mut decoder = SseBuffer::default();
    let stream = HttpBodyStream::from(upstream.into_body())
        .map_err(|err| std::io::Error::other(err.to_string()))
        .map(move |chunk| {
            chunk.map(|bytes| {
                let mut out = Vec::new();
                for payload in decoder.push_with_done(&bytes, true) {
                    match translator.translate_sse_payload(&payload) {
                        Ok(events) => {
                            for event in events {
                                write_claude_sse_event(&mut out, &event);
                            }
                        }
                        Err(err) => {
                            warn!(?err, "failed to translate OpenAI SSE event");
                        }
                    }
                }
                Frame::data(Bytes::from(out))
            })
        })
        .map_err(|err| -> BoxError { Box::new(err) });

    response_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(http_body_util::BodyExt::boxed_unsync(StreamBody::new(
            stream,
        )))
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        })
}

async fn buffered_gemini_as_openai(
    mut upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading Gemini response: {err}"),
            );
        }
    };
    let gemini_resp: gemini::GeminiChatResponse = match serde_json::from_slice(&payload) {
        Ok(resp) => resp,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid Gemini response: {err}"),
            );
        }
    };
    json_response(
        StatusCode::OK,
        gemini_response_to_openai_chat(gemini_resp, requested_model),
    )
}

fn stream_gemini_as_openai(
    upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let mut translator = GeminiSseToOpenAiTranslator::new(requested_model);
    let mut decoder = SseBuffer::default();
    let stream = HttpBodyStream::from(upstream.into_body())
        .map_err(|err| std::io::Error::other(err.to_string()))
        .map(move |chunk| {
            chunk.map(|bytes| {
                let mut out = Vec::new();
                for payload in decoder.push_with_done(&bytes, true) {
                    match translator.translate_sse_payload(&payload) {
                        Ok(events) => {
                            for event in events {
                                write_sse_data(&mut out, &event);
                            }
                        }
                        Err(err) => {
                            warn!(?err, "failed to translate Gemini SSE event");
                        }
                    }
                }
                Frame::data(Bytes::from(out))
            })
        })
        .map_err(|err| -> BoxError { Box::new(err) });

    response_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(http_body_util::BodyExt::boxed_unsync(StreamBody::new(
            stream,
        )))
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        })
}

async fn buffered_gemini_as_claude(
    mut upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading Gemini response: {err}"),
            );
        }
    };
    let gemini_resp: gemini::GeminiChatResponse = match serde_json::from_slice(&payload) {
        Ok(resp) => resp,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid Gemini response: {err}"),
            );
        }
    };
    let openai_resp = gemini_response_to_openai_chat(gemini_resp, requested_model.clone());
    json_response(
        StatusCode::OK,
        openai_chat_to_claude_messages_response(openai_resp, requested_model),
    )
}

fn stream_gemini_as_claude(
    upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let mut gemini_to_openai = GeminiSseToOpenAiTranslator::new(requested_model.clone());
    let mut openai_to_claude = OpenAiSseToClaudeTranslator::new(requested_model);
    let mut decoder = SseBuffer::default();
    let stream = HttpBodyStream::from(upstream.into_body())
        .map_err(|err| std::io::Error::other(err.to_string()))
        .map(move |chunk| {
            chunk.map(|bytes| {
                let mut out = Vec::new();
                for payload in decoder.push_with_done(&bytes, true) {
                    match gemini_to_openai.translate_sse_payload(&payload) {
                        Ok(openai_events) => {
                            for openai_event in openai_events {
                                match openai_to_claude.translate_sse_payload(&openai_event) {
                                    Ok(claude_events) => {
                                        for event in claude_events {
                                            write_claude_sse_event(&mut out, &event);
                                        }
                                    }
                                    Err(err) => {
                                        warn!(?err, "failed to translate chained OpenAI SSE event");
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            warn!(?err, "failed to translate Gemini SSE event");
                        }
                    }
                }
                Frame::data(Bytes::from(out))
            })
        })
        .map_err(|err| -> BoxError { Box::new(err) });

    response_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(http_body_util::BodyExt::boxed_unsync(StreamBody::new(
            stream,
        )))
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        })
}

async fn buffered_openai_as_gemini(mut upstream: Response<HttpBody>) -> Response<GatewayBody> {
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading OpenAI response: {err}"),
            );
        }
    };
    let openai_resp: openai::ChatCompletionResponse = match serde_json::from_slice(&payload) {
        Ok(resp) => resp,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid OpenAI response: {err}"),
            );
        }
    };
    json_response(StatusCode::OK, openai_chat_to_gemini_response(openai_resp))
}

fn stream_openai_as_gemini(upstream: Response<HttpBody>) -> Response<GatewayBody> {
    let mut translator = OpenAiSseToGeminiTranslator::new();
    let mut decoder = SseBuffer::default();
    let stream = HttpBodyStream::from(upstream.into_body())
        .map_err(|err| std::io::Error::other(err.to_string()))
        .map(move |chunk| {
            chunk.map(|bytes| {
                let mut out = Vec::new();
                for payload in decoder.push_with_done(&bytes, true) {
                    match translator.translate_sse_payload(&payload) {
                        Ok(events) => {
                            for event in events {
                                write_sse_data(&mut out, &event);
                            }
                        }
                        Err(err) => {
                            warn!(?err, "failed to translate OpenAI SSE event");
                        }
                    }
                }
                Frame::data(Bytes::from(out))
            })
        })
        .map_err(|err| -> BoxError { Box::new(err) });

    response_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(http_body_util::BodyExt::boxed_unsync(StreamBody::new(
            stream,
        )))
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        })
}

async fn buffered_gemini_imagen_as_openai(
    mut upstream: Response<HttpBody>,
) -> Response<GatewayBody> {
    let status = upstream.status();
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading Gemini image response: {err}"),
            );
        }
    };
    let gemini_resp: gemini::GeminiImageResponse = match serde_json::from_slice(&payload) {
        Ok(resp) => resp,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid Gemini image response: {err}"),
            );
        }
    };
    match gemini_imagen_to_openai_image_response(gemini_resp, now_unix_i64()) {
        Ok(resp) => json_response(status, resp),
        Err(err) => json_error(StatusCode::BAD_GATEWAY, "bad_gateway", &err.to_string()),
    }
}

async fn openai_image_json_as_stream(mut upstream: Response<HttpBody>) -> Response<GatewayBody> {
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading OpenAI image response: {err}"),
            );
        }
    };
    let image_resp: openai::ImageResponse = match serde_json::from_slice(&payload) {
        Ok(resp) => resp,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid OpenAI image response: {err}"),
            );
        }
    };

    let created = if image_resp.created == 0 {
        now_unix_i64()
    } else {
        image_resp.created
    };
    let mut out = Vec::new();
    for image in image_resp.data {
        let mut payload = serde_json::Map::new();
        payload.insert(
            "type".to_string(),
            JsonValue::String("image_generation.completed".to_string()),
        );
        payload.insert(
            "created_at".to_string(),
            JsonValue::Number(serde_json::Number::from(created)),
        );
        if !image.url.is_empty() {
            payload.insert("url".to_string(), JsonValue::String(image.url));
        }
        if !image.b64_json.is_empty() {
            payload.insert("b64_json".to_string(), JsonValue::String(image.b64_json));
        }
        if !image.revised_prompt.is_empty() {
            payload.insert(
                "revised_prompt".to_string(),
                JsonValue::String(image.revised_prompt),
            );
        }
        if let Err(err) = write_openai_image_sse_payload(
            &mut out,
            "image_generation.completed",
            &JsonValue::Object(payload),
        ) {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            );
        }
    }
    write_sse_data(&mut out, "[DONE]");

    response_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(full_body(out))
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        })
}

fn upstream_to_response(upstream: Response<HttpBody>) -> Response<GatewayBody> {
    let (parts, body) = upstream.into_parts();
    let stream = HttpBodyStream::from(body)
        .map_err(|err| std::io::Error::other(err.to_string()))
        .map_ok(Frame::data)
        .map_err(|err| -> BoxError { Box::new(err) });

    let mut builder = response_builder(parts.status);
    for (name, value) in &parts.headers {
        if is_forward_response_header(name) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(http_body_util::BodyExt::boxed_unsync(StreamBody::new(
            stream,
        )))
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        })
}

async fn upstream_error_response(mut upstream: Response<HttpBody>) -> Response<GatewayBody> {
    let status = upstream.status();
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => Bytes::from(format!("failed reading upstream error body: {err}")),
    };
    let message = String::from_utf8_lossy(&payload);
    json_error(status, "upstream_error", &message)
}

fn route_error_response(err: RouteError) -> Response<GatewayBody> {
    let status = match err {
        RouteError::Unauthorized => StatusCode::UNAUTHORIZED,
        RouteError::ModelForbidden => StatusCode::FORBIDDEN,
        RouteError::ModelNotFound => StatusCode::NOT_FOUND,
        RouteError::ChannelNotFound
        | RouteError::ChannelDisabled
        | RouteError::ChannelModelMismatch => StatusCode::BAD_GATEWAY,
    };
    json_error(status, "routing_error", &err.to_string())
}

fn json_response<T: Serialize>(status: StatusCode, value: T) -> Response<GatewayBody> {
    let body = match serde_json::to_vec(&value) {
        Ok(body) => body,
        Err(err) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            );
        }
    };
    response_builder(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full_body(body))
        .unwrap()
}

fn json_error(status: StatusCode, error_type: &str, message: &str) -> Response<GatewayBody> {
    let value = openai::ErrorResponse {
        error: openai::ErrorBody {
            message: message.to_string(),
            error_type: error_type.to_string(),
            code: None,
        },
    };
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| {
        b"{\"error\":{\"message\":\"internal error\",\"type\":\"internal_error\"}}".to_vec()
    });
    response_builder(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full_body(body))
        .unwrap()
}

fn response_builder(status: StatusCode) -> http::response::Builder {
    Response::builder().status(status)
}

fn full_body(body: impl Into<Bytes>) -> GatewayBody {
    Full::new(body.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    auth.strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

fn parse_openai_image_request(
    req: &GatewayRequest,
) -> Result<ParsedOpenAiImageRequest, Response<GatewayBody>> {
    let Some(kind) = OpenAiImageRouteKind::from_path(&req.path) else {
        return Err(json_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "OpenAI image route not found",
        ));
    };

    if kind == OpenAiImageRouteKind::Edits && is_multipart_content_type(&req.headers) {
        let model = match multipart_string_field(&req.body, &req.headers, "model") {
            Ok(Some(model)) if !model.trim().is_empty() => model.trim().to_string(),
            Ok(_) => {
                return Err(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "model is required",
                ));
            }
            Err(err) => {
                return Err(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &err,
                ));
            }
        };
        if let Ok(Some(n)) = multipart_string_field(&req.body, &req.headers, "n") {
            let n = n.trim();
            if !n.is_empty() {
                let parsed = n.parse::<u32>().ok();
                if parsed.is_none_or(|n| n > openai::MAX_IMAGE_N) {
                    return Err(json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("n must be an integer between 1 and {}", openai::MAX_IMAGE_N),
                    ));
                }
            }
        }
        let stream = match multipart_string_field(&req.body, &req.headers, "stream") {
            Ok(Some(value)) if !value.trim().is_empty() => match parse_form_bool(value.trim()) {
                Some(value) => value,
                None => {
                    return Err(json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        "invalid stream value",
                    ));
                }
            },
            _ => false,
        };
        return Ok(ParsedOpenAiImageRequest {
            kind,
            model,
            stream,
            payload: OpenAiImagePayload::Multipart,
        });
    }

    let mut value: JsonValue = match serde_json::from_slice(&req.body) {
        Ok(value) => value,
        Err(err) => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid OpenAI image request: {err}"),
            ));
        }
    };
    let mut image_req: openai::ImageRequest = match serde_json::from_value(value.clone()) {
        Ok(req) => req,
        Err(err) => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid OpenAI image request: {err}"),
            ));
        }
    };
    if let Err(message) = normalize_openai_image_request(&mut image_req, &mut value) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &message,
        ));
    }

    Ok(ParsedOpenAiImageRequest {
        kind,
        model: image_req.model.clone(),
        stream: image_req.is_stream(),
        payload: OpenAiImagePayload::Json {
            request: image_req,
            value,
        },
    })
}

impl OpenAiImageRouteKind {
    fn from_path(path_and_query: &str) -> Option<Self> {
        match path_and_query
            .split_once('?')
            .map_or(path_and_query, |(path, _)| path)
        {
            "/v1/images/generations" => Some(Self::Generations),
            "/v1/images/edits" => Some(Self::Edits),
            "/v1/edits" => Some(Self::LegacyEdits),
            _ => None,
        }
    }
}

fn normalize_openai_image_request(
    request: &mut openai::ImageRequest,
    value: &mut JsonValue,
) -> Result<(), String> {
    if request.model.is_empty() {
        return Err("model is required".to_string());
    }
    if request.size.contains('\u{00d7}') {
        return Err("size contains multiplication sign; use 'x' instead".to_string());
    }
    if request.n.is_some_and(|n| n > openai::MAX_IMAGE_N) {
        return Err(format!(
            "n must be an integer between 1 and {}",
            openai::MAX_IMAGE_N
        ));
    }

    match request.model.as_str() {
        "dall-e-2" | "dall-e" => {
            if !request.size.is_empty()
                && request.size != "256x256"
                && request.size != "512x512"
                && request.size != "1024x1024"
            {
                return Err(
                    "size must be one of 256x256, 512x512, or 1024x1024 for dall-e-2 or dall-e"
                        .to_string(),
                );
            }
            if request.size.is_empty() {
                request.size = "1024x1024".to_string();
                set_json_string(value, "size", &request.size);
            }
        }
        "dall-e-3" => {
            if !request.size.is_empty()
                && request.size != "1024x1024"
                && request.size != "1024x1792"
                && request.size != "1792x1024"
            {
                return Err(
                    "size must be one of 1024x1024, 1024x1792 or 1792x1024 for dall-e-3"
                        .to_string(),
                );
            }
            if request.quality.is_empty() {
                request.quality = "standard".to_string();
                set_json_string(value, "quality", &request.quality);
            }
            if request.size.is_empty() {
                request.size = "1024x1024".to_string();
                set_json_string(value, "size", &request.size);
            }
        }
        "gpt-image-1" => {
            if request.quality.is_empty() {
                request.quality = "auto".to_string();
                set_json_string(value, "quality", &request.quality);
            }
        }
        _ => {}
    }

    if request.n.unwrap_or(0) == 0 {
        request.n = Some(1);
        value["n"] = JsonValue::Number(serde_json::Number::from(1));
    }

    Ok(())
}

fn set_json_string(value: &mut JsonValue, key: &str, data: &str) {
    value[key] = JsonValue::String(data.to_string());
}

fn content_type_header(headers: &HeaderMap) -> Option<&HeaderValue> {
    headers.get(header::CONTENT_TYPE)
}

fn is_multipart_content_type(headers: &HeaderMap) -> bool {
    content_type_header(headers)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| {
            content_type
                .to_ascii_lowercase()
                .contains("multipart/form-data")
        })
}

fn multipart_string_field(
    body: &[u8],
    headers: &HeaderMap,
    field: &str,
) -> Result<Option<String>, String> {
    let Some((start, end)) = multipart_field_range(body, headers, field)? else {
        return Ok(None);
    };
    let value = std::str::from_utf8(&body[start..end])
        .map_err(|err| format!("multipart field {field} is not utf-8: {err}"))?;
    Ok(Some(value.trim().to_string()))
}

fn rewrite_multipart_model_field(
    body: Bytes,
    headers: &HeaderMap,
    upstream_model: &str,
) -> Result<Bytes, String> {
    let Some((start, end)) = multipart_field_range(&body, headers, "model")? else {
        return Err("multipart image edit form must contain model field".to_string());
    };
    if &body[start..end] == upstream_model.as_bytes() {
        return Ok(body);
    }

    let mut rewritten =
        Vec::with_capacity(body.len() + upstream_model.len().saturating_sub(end - start));
    rewritten.extend_from_slice(&body[..start]);
    rewritten.extend_from_slice(upstream_model.as_bytes());
    rewritten.extend_from_slice(&body[end..]);
    Ok(Bytes::from(rewritten))
}

fn multipart_field_range(
    body: &[u8],
    headers: &HeaderMap,
    field: &str,
) -> Result<Option<(usize, usize)>, String> {
    let content_type = content_type_header(headers)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| "multipart request missing content-type".to_string())?;
    let boundary = multipart_boundary(content_type)
        .ok_or_else(|| "multipart request missing boundary".to_string())?;
    let marker = {
        let mut marker = Vec::with_capacity(boundary.len() + 2);
        marker.extend_from_slice(b"--");
        marker.extend_from_slice(boundary.as_bytes());
        marker
    };

    let Some(mut pos) = find_bytes(body, &marker) else {
        return Ok(None);
    };

    loop {
        let mut part_start = pos + marker.len();
        if body.get(part_start..part_start + 2) == Some(b"--") {
            return Ok(None);
        }
        if body.get(part_start..part_start + 2) == Some(b"\r\n") {
            part_start += 2;
        } else if body.get(part_start) == Some(&b'\n') {
            part_start += 1;
        }

        let Some(next_rel) = find_bytes(&body[part_start..], &marker) else {
            return Ok(None);
        };
        let part_end = part_start + next_rel;
        let part = &body[part_start..part_end];
        let Some((header_len, separator_len)) = multipart_header_separator(part) else {
            pos = part_end;
            continue;
        };
        let header_bytes = &part[..header_len];
        if multipart_headers_match_field(header_bytes, field) {
            let start = part_start + header_len + separator_len;
            let mut end = part_end;
            if end >= 2 && &body[end - 2..end] == b"\r\n" {
                end -= 2;
            } else if end >= 1 && body[end - 1] == b'\n' {
                end -= 1;
            }
            return Ok(Some((start, end)));
        }
        pos = part_end;
    }
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    for part in content_type.split(';') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("boundary") {
            continue;
        }
        let value = value.trim();
        return Some(
            value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value)
                .to_string(),
        );
    }
    None
}

fn multipart_header_separator(part: &[u8]) -> Option<(usize, usize)> {
    find_bytes(part, b"\r\n\r\n")
        .map(|idx| (idx, 4))
        .or_else(|| find_bytes(part, b"\n\n").map(|idx| (idx, 2)))
}

fn multipart_headers_match_field(headers: &[u8], field: &str) -> bool {
    let Ok(headers) = std::str::from_utf8(headers) else {
        return false;
    };
    let quoted = format!("name=\"{field}\"");
    let raw = format!("name={field}");
    headers.lines().any(|line| {
        let Some((name, value)) = line.trim().split_once(':') else {
            return false;
        };
        name.eq_ignore_ascii_case("content-disposition")
            && (value.contains(&quoted) || value.contains(&raw))
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_form_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

fn anthropic_version<'a>(headers: &'a HeaderMap, default: &'a str) -> &'a str {
    headers
        .get("anthropic-version")
        .and_then(|value| value.to_str().ok())
        .filter(|version| !version.is_empty())
        .unwrap_or(default)
}

fn upstream_uri(base_url: &str, path: &str) -> Result<Uri> {
    format!("{}{}", base_url.trim_end_matches('/'), path)
        .parse()
        .context("invalid upstream uri")
}

fn path_and_query<'a>(uri: &'a Uri, fallback: &'a str) -> &'a str {
    uri.path_and_query().map_or(fallback, |pq| pq.as_str())
}

fn authority(uri: &Uri) -> Result<HeaderValue> {
    let authority = uri.authority().context("upstream uri missing authority")?;
    HeaderValue::from_str(authority.as_str()).context("invalid upstream authority header")
}

async fn timeout_opt<F, T>(timeout: Option<Duration>, fut: F) -> Result<T>
where
    F: std::future::Future<Output = T>,
{
    match timeout {
        Some(timeout) => monoio::time::timeout(timeout, fut)
            .await
            .map_err(|_| anyhow::anyhow!("upstream connect timeout")),
        None => Ok(fut.await),
    }
}

fn debug_relay<CX>(cx: &CX, stream: bool)
where
    CX: ParamRef<RouteContext> + ParamRef<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    let route = ParamRef::<RouteContext>::param_ref(cx);
    let auth = ParamRef::<RequestAuth>::param_ref(cx);
    let request_id = ParamRef::<RequestId>::param_ref(cx);
    let peer = ParamRef::<PeerAddr>::param_ref(cx);
    debug!(
        request_id = %request_id.0,
        peer_addr = %peer.0,
        user_id = %auth.user_id,
        token_id = %auth.token_id,
        channel_id = %route.channel_id,
        provider = ?route.provider,
        requested_model = %route.requested_model,
        upstream_model = %route.upstream_model,
        stream,
        "relay request"
    );
}

fn is_stream_like(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/event-stream"))
}

fn is_event_stream_response(resp: &Response<HttpBody>) -> bool {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| {
            content_type
                .to_ascii_lowercase()
                .contains("text/event-stream")
        })
}

fn is_gemini_generate_content_path(path: &str) -> bool {
    parse_gemini_generate_content_path(path).is_some()
}

fn parse_gemini_generate_content_path(path_and_query: &str) -> Option<(String, bool)> {
    let (path, query) = path_and_query
        .split_once('?')
        .map_or((path_and_query, ""), |(path, query)| (path, query));
    let marker = "/models/";
    let model_start = path.find(marker)? + marker.len();
    let rest = &path[model_start..];
    let (model, action) = rest.split_once(':')?;
    if model.is_empty() {
        return None;
    }
    let stream =
        action == "streamGenerateContent" || query.split('&').any(|item| item == "alt=sse");
    if action == "generateContent" || action == "streamGenerateContent" {
        Some((model.to_string(), stream))
    } else {
        None
    }
}

fn rewrite_gemini_model_in_path(path_and_query: &str, upstream_model: &str) -> String {
    let Some((path, query)) = path_and_query.split_once('?') else {
        return rewrite_gemini_model_in_path_no_query(path_and_query, upstream_model);
    };
    format!(
        "{}?{}",
        rewrite_gemini_model_in_path_no_query(path, upstream_model),
        query
    )
}

fn rewrite_gemini_model_in_path_no_query(path: &str, upstream_model: &str) -> String {
    let Some(model_start) = path.find("/models/").map(|idx| idx + "/models/".len()) else {
        return path.to_string();
    };
    let Some(colon) = path[model_start..].find(':').map(|idx| model_start + idx) else {
        return path.to_string();
    };
    let mut out = String::with_capacity(path.len() + upstream_model.len());
    out.push_str(&path[..model_start]);
    out.push_str(upstream_model);
    out.push_str(&path[colon..]);
    out
}

fn is_forward_response_header(name: &http::HeaderName) -> bool {
    !matches!(
        name.as_str(),
        "connection"
            | "transfer-encoding"
            | "content-length"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

fn write_sse_data(out: &mut Vec<u8>, event: &str) {
    if event == "[DONE]" {
        out.extend_from_slice(b"data: [DONE]\n\n");
    } else {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(event.as_bytes());
        out.extend_from_slice(b"\n\n");
    }
}

fn write_claude_sse_event(out: &mut Vec<u8>, event: &str) {
    if let Ok(value) = serde_json::from_str::<JsonValue>(event) {
        if let Some(event_type) = value.get("type").and_then(JsonValue::as_str) {
            out.extend_from_slice(b"event: ");
            out.extend_from_slice(event_type.as_bytes());
            out.extend_from_slice(b"\n");
        }
    }
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(event.as_bytes());
    out.extend_from_slice(b"\n\n");
}

fn write_openai_image_sse_payload(
    out: &mut Vec<u8>,
    event_name: &str,
    payload: &JsonValue,
) -> Result<()> {
    if !event_name.is_empty() {
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(event_name.as_bytes());
        out.extend_from_slice(b"\n");
    }
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(&serde_json::to_vec(payload)?);
    out.extend_from_slice(b"\n\n");
    Ok(())
}

fn now_unix_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[derive(Default)]
struct SseBuffer {
    pending: String,
}

impl SseBuffer {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.push_with_done(bytes, false)
    }

    fn push_with_done(&mut self, bytes: &[u8], include_done: bool) -> Vec<String> {
        let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
        self.pending.push_str(&text);
        let mut payloads = Vec::new();

        while let Some(pos) = self.pending.find("\n\n") {
            let event = self.pending[..pos].to_string();
            self.pending.drain(..pos + 2);
            for payload in sse_event_payloads(&event) {
                if include_done || payload != "[DONE]" {
                    payloads.push(payload);
                }
            }
        }

        payloads
    }
}

fn sse_event_payloads(event: &str) -> Vec<String> {
    let mut data = Vec::new();
    for line in event.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            data.push(payload.trim_start());
        }
    }
    if data.is_empty() {
        Vec::new()
    } else {
        vec![data.join("\n")]
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8080))
}

fn default_body_limit() -> usize {
    16 * 1024 * 1024
}

fn default_claude_version() -> String {
    "2023-06-01".to_string()
}

fn default_gemini_api_version() -> String {
    "v1beta".to_string()
}

fn default_version() -> u64 {
    1
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_image_json_request_with_new_api_defaults() {
        let req = GatewayRequest {
            headers: HeaderMap::new(),
            path: "/v1/images/generations".to_string(),
            body: Bytes::from_static(br#"{"model":"gpt-image-1","prompt":"draw"}"#),
        };

        let parsed = parse_openai_image_request(&req).expect("image request should parse");

        assert_eq!(parsed.kind, OpenAiImageRouteKind::Generations);
        assert_eq!(parsed.model, "gpt-image-1");
        let OpenAiImagePayload::Json { request, value } = parsed.payload else {
            panic!("expected json image payload");
        };
        assert_eq!(request.n, Some(1));
        assert_eq!(request.quality, "auto");
        assert_eq!(value["n"], JsonValue::Number(serde_json::Number::from(1)));
        assert_eq!(value["quality"], JsonValue::String("auto".to_string()));
    }

    #[test]
    fn parses_and_rewrites_multipart_image_edit_model() {
        let boundary = "halolake-boundary";
        let body = Bytes::from(format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"model\"\r\n\r\n\
             gpt-image-1\r\n\
             --{boundary}\r\n\
             Content-Disposition: form-data; name=\"prompt\"\r\n\r\n\
             edit it\r\n\
             --{boundary}--\r\n"
        ));
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
        );

        let model = multipart_string_field(&body, &headers, "model")
            .expect("multipart parse should succeed")
            .expect("model field should exist");
        assert_eq!(model, "gpt-image-1");

        let rewritten = rewrite_multipart_model_field(body, &headers, "imagen-4")
            .expect("multipart model rewrite should succeed");
        let rewritten_model = multipart_string_field(&rewritten, &headers, "model")
            .expect("rewritten multipart parse should succeed")
            .expect("rewritten model field should exist");
        assert_eq!(rewritten_model, "imagen-4");
    }
}
