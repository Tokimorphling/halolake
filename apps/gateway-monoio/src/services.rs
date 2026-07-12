use super::*;
use crate::gateway::{
    ClaudeVersion, ConnectTimeout, GatewayAuthPolicy, GeminiApiVersion, PassAnthropicBeta,
    SnapshotStore, UpstreamReadTimeout,
};

#[derive(Clone)]
pub(crate) struct ChatGatewayService {
    router:           AuthRouteService,
    relay:            RelayService,
    usage:            UsageReporter,
    channel_feedback: ChannelFeedbackReporter,
}

#[derive(Clone)]
pub(crate) struct ImageGatewayService {
    router:           AuthRouteService,
    relay:            RelayService,
    usage:            UsageReporter,
    channel_feedback: ChannelFeedbackReporter,
}

#[derive(Clone)]
pub(crate) struct ClaudeMessagesGatewayService {
    router:           AuthRouteService,
    relay:            RelayService,
    usage:            UsageReporter,
    channel_feedback: ChannelFeedbackReporter,
}

#[derive(Clone)]
pub(crate) struct GeminiGatewayService {
    router:           AuthRouteService,
    relay:            RelayService,
    usage:            UsageReporter,
    channel_feedback: ChannelFeedbackReporter,
}

#[derive(Clone)]
pub(crate) struct RawOpenAiGatewayService {
    router:           AuthRouteService,
    relay:            RelayService,
    usage:            UsageReporter,
    channel_feedback: ChannelFeedbackReporter,
}

#[derive(Clone)]
pub(crate) struct AuthRouteService {
    snapshots: SnapshotStore,
    auth:      AuthConfig,
    next_key:  Arc<AtomicU64>,
}

impl ChatGatewayService {
    pub(crate) fn from_params<C>(params: &C) -> Self
    where
        C: Param<SnapshotStore>
            + Param<GatewayAuthPolicy>
            + Param<ClaudeVersion>
            + Param<GeminiApiVersion>
            + Param<PassAnthropicBeta>
            + Param<ConnectTimeout>
            + Param<UpstreamReadTimeout>
            + Param<UsageReporter>
            + Param<ChannelFeedbackReporter>,
    {
        Self {
            router:           AuthRouteService {
                snapshots: params.param(),
                auth:      Param::<GatewayAuthPolicy>::param(params).0,
                next_key:  Arc::new(AtomicU64::new(0)),
            },
            relay:            RelayService::from_params(params),
            usage:            Param::<UsageReporter>::param(params),
            channel_feedback: Param::<ChannelFeedbackReporter>::param(params),
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
                path: req.path.clone(),
                headers: req.headers.clone(),
                body: req.body.clone(),
                peer_ip: ParamRef::<PeerAddr>::param_ref(&cx).0.ip(),
            })
            .await
        {
            Ok(route) => route,
            Err(err) => return Ok(route_error_response(err)),
        };

        let affinity = route.affinity.clone();
        let affinity_channel_id = route.route.channel_id.clone();
        let cx = cx.param_set(route.auth).param_set(route.route);
        let usage = usage_event_parts(&cx, openai_req.is_stream());
        let started = Instant::now();
        let resp = self
            .relay
            .call(OpenAiChatRelayRequest {
                request: openai_req,
                raw_body: req.body,
                downstream_headers: req.headers,
                cx,
            })
            .await?;
        self.router
            .record_affinity(affinity.as_ref(), &affinity_channel_id, resp.status());
        Ok(finalize_response_usage(
            &self.usage,
            &self.channel_feedback,
            usage,
            resp,
            started,
        ))
    }
}

impl ImageGatewayService {
    pub(crate) fn from_params<C>(params: &C) -> Self
    where
        C: Param<SnapshotStore>
            + Param<GatewayAuthPolicy>
            + Param<ClaudeVersion>
            + Param<GeminiApiVersion>
            + Param<PassAnthropicBeta>
            + Param<ConnectTimeout>
            + Param<UpstreamReadTimeout>
            + Param<UsageReporter>
            + Param<ChannelFeedbackReporter>,
    {
        Self {
            router:           AuthRouteService {
                snapshots: params.param(),
                auth:      Param::<GatewayAuthPolicy>::param(params).0,
                next_key:  Arc::new(AtomicU64::new(0)),
            },
            relay:            RelayService::from_params(params),
            usage:            Param::<UsageReporter>::param(params),
            channel_feedback: Param::<ChannelFeedbackReporter>::param(params),
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
                path: req.path.clone(),
                headers: req.headers.clone(),
                body: req.body.clone(),
                peer_ip: ParamRef::<PeerAddr>::param_ref(&cx).0.ip(),
            })
            .await
        {
            Ok(route) => route,
            Err(err) => return Ok(route_error_response(err)),
        };

        let affinity = route.affinity.clone();
        let affinity_channel_id = route.route.channel_id.clone();
        let cx = cx.param_set(route.auth).param_set(route.route);
        let usage = usage_event_parts(&cx, parsed.stream);
        let started = Instant::now();
        let resp = self
            .relay
            .call(OpenAiImageRelayRequest {
                kind: parsed.kind,
                stream: parsed.stream,
                payload: parsed.payload,
                body: req.body,
                downstream_headers: req.headers,
                path: req.path,
                cx,
            })
            .await?;
        self.router
            .record_affinity(affinity.as_ref(), &affinity_channel_id, resp.status());
        Ok(finalize_response_usage(
            &self.usage,
            &self.channel_feedback,
            usage,
            resp,
            started,
        ))
    }
}

impl ClaudeMessagesGatewayService {
    pub(crate) fn from_params<C>(params: &C) -> Self
    where
        C: Param<SnapshotStore>
            + Param<GatewayAuthPolicy>
            + Param<ClaudeVersion>
            + Param<GeminiApiVersion>
            + Param<PassAnthropicBeta>
            + Param<ConnectTimeout>
            + Param<UpstreamReadTimeout>
            + Param<UsageReporter>
            + Param<ChannelFeedbackReporter>,
    {
        Self {
            router:           AuthRouteService {
                snapshots: params.param(),
                auth:      Param::<GatewayAuthPolicy>::param(params).0,
                next_key:  Arc::new(AtomicU64::new(0)),
            },
            relay:            RelayService::from_params(params),
            usage:            Param::<UsageReporter>::param(params),
            channel_feedback: Param::<ChannelFeedbackReporter>::param(params),
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
                path: req.path.clone(),
                headers: req.headers.clone(),
                body: req.body.clone(),
                peer_ip: ParamRef::<PeerAddr>::param_ref(&cx).0.ip(),
            })
            .await
        {
            Ok(route) => route,
            Err(err) => return Ok(route_error_response(err)),
        };
        let affinity = route.affinity.clone();
        let affinity_channel_id = route.route.channel_id.clone();
        let cx = cx.param_set(route.auth).param_set(route.route);
        let usage = usage_event_parts(
            &cx,
            value
                .get("stream")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false),
        );
        let started = Instant::now();
        let resp = self
            .relay
            .call(ClaudeMessagesRelayRequest {
                value,
                downstream_headers: req.headers,
                cx,
            })
            .await?;
        self.router
            .record_affinity(affinity.as_ref(), &affinity_channel_id, resp.status());
        Ok(finalize_response_usage(
            &self.usage,
            &self.channel_feedback,
            usage,
            resp,
            started,
        ))
    }
}

impl GeminiGatewayService {
    pub(crate) fn from_params<C>(params: &C) -> Self
    where
        C: Param<SnapshotStore>
            + Param<GatewayAuthPolicy>
            + Param<ClaudeVersion>
            + Param<GeminiApiVersion>
            + Param<PassAnthropicBeta>
            + Param<ConnectTimeout>
            + Param<UpstreamReadTimeout>
            + Param<UsageReporter>
            + Param<ChannelFeedbackReporter>,
    {
        Self {
            router:           AuthRouteService {
                snapshots: params.param(),
                auth:      Param::<GatewayAuthPolicy>::param(params).0,
                next_key:  Arc::new(AtomicU64::new(0)),
            },
            relay:            RelayService::from_params(params),
            usage:            Param::<UsageReporter>::param(params),
            channel_feedback: Param::<ChannelFeedbackReporter>::param(params),
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
                path: req.path.clone(),
                headers: req.headers.clone(),
                body: req.body.clone(),
                peer_ip: ParamRef::<PeerAddr>::param_ref(&cx).0.ip(),
            })
            .await
        {
            Ok(route) => route,
            Err(err) => return Ok(route_error_response(err)),
        };

        let affinity = route.affinity.clone();
        let affinity_channel_id = route.route.channel_id.clone();
        let cx = cx.param_set(route.auth).param_set(route.route);
        let usage = usage_event_parts(&cx, stream);
        let started = Instant::now();
        let resp = self
            .relay
            .call(GeminiNativeRelayRequest {
                path: req.path,
                stream,
                body: req.body,
                downstream_headers: req.headers,
                cx,
            })
            .await?;
        self.router
            .record_affinity(affinity.as_ref(), &affinity_channel_id, resp.status());
        Ok(finalize_response_usage(
            &self.usage,
            &self.channel_feedback,
            usage,
            resp,
            started,
        ))
    }
}

impl RawOpenAiGatewayService {
    pub(crate) fn from_params<C>(params: &C) -> Self
    where
        C: Param<SnapshotStore>
            + Param<GatewayAuthPolicy>
            + Param<ClaudeVersion>
            + Param<GeminiApiVersion>
            + Param<PassAnthropicBeta>
            + Param<ConnectTimeout>
            + Param<UpstreamReadTimeout>
            + Param<UsageReporter>
            + Param<ChannelFeedbackReporter>,
    {
        Self {
            router:           AuthRouteService {
                snapshots: params.param(),
                auth:      Param::<GatewayAuthPolicy>::param(params).0,
                next_key:  Arc::new(AtomicU64::new(0)),
            },
            relay:            RelayService::from_params(params),
            usage:            Param::<UsageReporter>::param(params),
            channel_feedback: Param::<ChannelFeedbackReporter>::param(params),
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
                path: req.path.clone(),
                headers: req.headers.clone(),
                body: req.body.clone(),
                peer_ip: ParamRef::<PeerAddr>::param_ref(&cx).0.ip(),
            })
            .await
        {
            Ok(route) => route,
            Err(err) => return Ok(route_error_response(err)),
        };
        let affinity = route.affinity.clone();
        let affinity_channel_id = route.route.channel_id.clone();
        value["model"] = JsonValue::String(route.route.upstream_model.clone());
        // new-api ForceStreamOption targets OpenAI chat/completions (and legacy
        // completions). Responses API carries usage in `response.completed` instead.
        let path_for_stream = req.path.split('?').next().unwrap_or(req.path.as_str());
        if path_for_stream.ends_with("/chat/completions")
            || path_for_stream.ends_with("/completions")
        {
            ensure_openai_stream_include_usage(&mut value);
        }
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
        let usage = usage_event_parts(
            &cx,
            value
                .get("stream")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false),
        );
        let started = Instant::now();
        let resp = self
            .relay
            .call(OpenAiPassthroughRelayRequest {
                path: req.path,
                body,
                downstream_headers: req.headers,
                cx,
            })
            .await?;
        self.router
            .record_affinity(affinity.as_ref(), &affinity_channel_id, resp.status());
        Ok(finalize_response_usage(
            &self.usage,
            &self.channel_feedback,
            usage,
            resp,
            started,
        ))
    }
}

impl Service<RouteLookup> for AuthRouteService {
    type Response = RouteParts;
    type Error = RouteError;

    async fn call(&self, lookup: RouteLookup) -> Result<Self::Response, Self::Error> {
        let state = self.snapshots.load();
        let snapshot = &state.snapshot;
        // The affinity cache is owned by the SnapshotStore, not the snapshot, so
        // it survives snapshot swaps.
        let affinity_cache = self.snapshots.affinity_cache();
        let auth = snapshot.authenticate(&lookup.token)?;
        snapshot.authorize_ip(&auth, lookup.peer_ip)?;
        let key_seed = self.next_key.fetch_add(1, Ordering::Relaxed);
        let user_agent = lookup
            .headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let affinity = snapshot.resolve_affinity(
            &lookup.requested_model,
            &lookup.path,
            user_agent,
            auth.token.group(),
            &lookup.body,
            affinity_cache,
            |name| {
                lookup
                    .headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            },
        );
        let (route, affinity_hit) = snapshot.route_with_affinity_seed(
            &auth,
            &lookup.requested_model,
            affinity.as_ref(),
            affinity_cache,
            key_seed,
        )?;
        if affinity_hit {
            debug!(
                channel_id = %route.channel.id,
                requested_model = %lookup.requested_model,
                "channel affinity hit"
            );
        }
        let (api_key, api_key_index) = route.channel.select_api_key_with_index(key_seed);
        Ok(RouteParts {
            auth:     RequestAuth {
                user_id:  route.user_id.to_string(),
                token_id: auth.token.id().to_string(),
            },
            route:    RouteContext {
                channel_id: route.channel.id.clone(),
                provider: route.channel.provider,
                base_url: route.channel.base_url.clone(),
                api_key: api_key.to_string(),
                api_key_index,
                using_group: route.using_group.to_string(),
                requested_model: route.requested_model.to_string(),
                upstream_model: route.upstream_model.to_string(),
                proxy: route.channel.proxy.clone(),
            },
            affinity: affinity.map(route_affinity_context),
        })
    }
}

impl AuthRouteService {
    fn record_affinity(
        &self,
        affinity: Option<&RouteAffinityContext>,
        channel_id: &str,
        status: StatusCode,
    ) {
        if !status.is_success() {
            return;
        }
        let Some(affinity) = affinity else {
            return;
        };
        let state = self.snapshots.load();
        state.snapshot.record_affinity(
            self.snapshots.affinity_cache(),
            &ChannelAffinityCandidate {
                cache_key:         affinity.cache_key.clone(),
                ttl_seconds:       affinity.ttl_seconds,
                cached_channel_id: None,
                rule_name:         affinity.rule_name.clone(),
            },
            channel_id,
        );
    }
}

fn route_affinity_context(candidate: ChannelAffinityCandidate) -> RouteAffinityContext {
    RouteAffinityContext {
        cache_key:   candidate.cache_key,
        ttl_seconds: candidate.ttl_seconds,
        rule_name:   candidate.rule_name,
    }
}

#[derive(Debug, Clone)]
struct UsageEventParts {
    request_id:          String,
    user_id:             String,
    token_id:            String,
    channel_id:          String,
    api_key_index:       Option<usize>,
    using_group:         String,
    requested_model:     String,
    upstream_model:      String,
    is_stream:           bool,
    ip:                  String,
    upstream_request_id: String,
}

fn usage_event_parts<CX>(cx: &CX, is_stream: bool) -> UsageEventParts
where
    CX: ParamRef<RequestAuth> + ParamRef<RouteContext> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    let auth = ParamRef::<RequestAuth>::param_ref(cx);
    let route = ParamRef::<RouteContext>::param_ref(cx);
    let request_id = ParamRef::<RequestId>::param_ref(cx);
    let peer = ParamRef::<PeerAddr>::param_ref(cx);
    UsageEventParts {
        request_id: request_id.0.clone(),
        user_id: auth.user_id.clone(),
        token_id: auth.token_id.clone(),
        channel_id: route.channel_id.clone(),
        api_key_index: route.api_key_index,
        using_group: route.using_group.clone(),
        requested_model: route.requested_model.clone(),
        upstream_model: route.upstream_model.clone(),
        is_stream,
        ip: peer.0.ip().to_string(),
        upstream_request_id: String::new(),
    }
}

fn finalize_response_usage(
    reporter: &UsageReporter,
    channel_feedback: &ChannelFeedbackReporter,
    mut parts: UsageEventParts,
    mut resp: Response<GatewayBody>,
    started: Instant,
) -> Response<GatewayBody> {
    parts.upstream_request_id = upstream_request_id_from_headers(resp.headers());
    report_channel_feedback(channel_feedback, &parts, &resp);
    if reporter.is_enabled() && is_gateway_event_stream_response(&resp) {
        let status = usage_status_from_http(resp.status());
        let body = std::mem::replace(resp.body_mut(), full_body(Bytes::new()));
        let (report_tx, mut report_rx) = mpsc::unbounded();
        let reporter = reporter.clone();
        monoio::spawn(async move {
            if let Some(event) = report_rx.next().await {
                reporter.report(event);
            }
        });
        // Tee the stream into a monoio-http payload while collecting usage from
        // SSE frames via monoio-http stream payload.

        *resp.body_mut() = wrap_streaming_usage_body(body, report_tx, parts, status, started);
        resp
    } else {
        report_response_usage(reporter, parts, &resp, started.elapsed().as_millis() as u64);
        resp
    }
}

fn report_channel_feedback(
    reporter: &ChannelFeedbackReporter,
    parts: &UsageEventParts,
    resp: &Response<GatewayBody>,
) {
    if !reporter.is_enabled() {
        return;
    }
    let Some(meta) = resp.extensions().get::<ChannelFeedbackMeta>() else {
        return;
    };
    reporter.report(ChannelFeedbackEvent {
        request_id:         parts.request_id.clone(),
        channel_id:         parts.channel_id.clone(),
        api_key_index:      parts.api_key_index,
        status_code:        meta.status_code,
        reason:             meta.reason,
        message:            truncate_feedback_message(&meta.message),
        created_at_unix_ms: now_unix_ms_i64(),
    });
}

fn truncate_feedback_message(message: &str) -> String {
    const MAX_FEEDBACK_MESSAGE_BYTES: usize = 2048;
    if message.len() <= MAX_FEEDBACK_MESSAGE_BYTES {
        return message.to_string();
    }
    let mut end = MAX_FEEDBACK_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_string()
}

fn report_response_usage(
    reporter: &UsageReporter,
    parts: UsageEventParts,
    resp: &Response<GatewayBody>,
    latency_ms: u64,
) {
    let usage = resp.extensions().get::<ResponseUsage>().copied();
    report_usage_event(
        reporter,
        parts,
        usage_status_from_http(resp.status()),
        usage,
        latency_ms,
        // Non-stream: no separate first-token phase; leave FRT unset (UI shows N/A for stream only).
        None,
    );
}

fn report_usage_event(
    reporter: &UsageReporter,
    parts: UsageEventParts,
    status: UsageStatus,
    usage: Option<ResponseUsage>,
    latency_ms: u64,
    first_response_ms: Option<u64>,
) {
    let http_status = match status {
        UsageStatus::Success => StatusCode::OK,
        UsageStatus::ClientError => StatusCode::BAD_REQUEST,
        UsageStatus::UpstreamError | UsageStatus::GatewayError => StatusCode::BAD_GATEWAY,
    };
    log_response_usage(
        &parts.request_id,
        http_status,
        latency_ms,
        usage,
        &parts.upstream_request_id,
        parts.is_stream,
    );
    reporter.report(UsageEvent {
        request_id: parts.request_id,
        user_id: parts.user_id,
        token_id: parts.token_id,
        channel_id: parts.channel_id,
        group: parts.using_group,
        model: parts.requested_model,
        upstream_model: parts.upstream_model,
        prompt_tokens: usage.and_then(|usage| usage.prompt_tokens),
        completion_tokens: usage.and_then(|usage| usage.completion_tokens),
        total_tokens: usage.and_then(|usage| usage.total_tokens),
        cache_read_tokens: usage.and_then(|usage| usage.cache_read_tokens),
        cache_creation_tokens: usage.and_then(|usage| usage.cache_creation_tokens),
        image_tokens: usage.and_then(|usage| usage.image_tokens),
        audio_tokens: usage.and_then(|usage| usage.audio_tokens),
        quota: None,
        status,
        latency_ms,
        first_response_ms,
        is_stream: parts.is_stream,
        ip: parts.ip,
        upstream_request_id: parts.upstream_request_id,
        created_at_unix_ms: now_unix_ms_i64(),
    });
}

fn usage_status_from_http(status: StatusCode) -> UsageStatus {
    if status.is_success() {
        UsageStatus::Success
    } else if status.is_client_error() {
        UsageStatus::ClientError
    } else {
        UsageStatus::UpstreamError
    }
}

fn wrap_streaming_usage_body(
    inner: GatewayBody,
    report_tx: mpsc::UnboundedSender<UsageEvent>,
    parts: UsageEventParts,
    status: UsageStatus,
    started: Instant,
) -> GatewayBody {
    stream_body_from_async(move |mut sender| async move {
        let mut collector = SseUsageCollector::default();
        let mut status = status;
        let mut body = inner;
        // new-api RelayInfo.SetFirstResponseTime on first body chunk.
        let mut first_response_ms: Option<u64> = None;
        loop {
            match MonoioBody::next_data(&mut body).await {
                Some(Ok(bytes)) => {
                    if first_response_ms.is_none() && !bytes.is_empty() {
                        first_response_ms = Some(started.elapsed().as_millis() as u64);
                    }
                    collector.push(&bytes);
                    sender.feed_data(Some(bytes));
                }
                Some(Err(err)) => {
                    status = UsageStatus::UpstreamError;
                    sender.feed_error(err);
                    break;
                }
                None => {
                    sender.feed_data(None);
                    break;
                }
            }
        }
        let usage = collector.usage();
        let latency_ms = started.elapsed().as_millis() as u64;
        let http_status = match status {
            UsageStatus::Success => StatusCode::OK,
            UsageStatus::ClientError => StatusCode::BAD_REQUEST,
            UsageStatus::UpstreamError | UsageStatus::GatewayError => StatusCode::BAD_GATEWAY,
        };
        log_response_usage(
            &parts.request_id,
            http_status,
            latency_ms,
            usage,
            &parts.upstream_request_id,
            parts.is_stream,
        );
        let _ = report_tx.unbounded_send(UsageEvent {
            request_id: parts.request_id,
            user_id: parts.user_id,
            token_id: parts.token_id,
            channel_id: parts.channel_id,
            group: parts.using_group,
            model: parts.requested_model,
            upstream_model: parts.upstream_model,
            prompt_tokens: usage.and_then(|usage| usage.prompt_tokens),
            completion_tokens: usage.and_then(|usage| usage.completion_tokens),
            total_tokens: usage.and_then(|usage| usage.total_tokens),
            cache_read_tokens: usage.and_then(|usage| usage.cache_read_tokens),
            cache_creation_tokens: usage.and_then(|usage| usage.cache_creation_tokens),
            image_tokens: usage.and_then(|usage| usage.image_tokens),
            audio_tokens: usage.and_then(|usage| usage.audio_tokens),
            quota: None,
            status,
            latency_ms,
            first_response_ms,
            is_stream: parts.is_stream,
            ip: parts.ip,
            upstream_request_id: parts.upstream_request_id,
            created_at_unix_ms: now_unix_ms_i64(),
        });
    })
}

#[derive(Default)]
struct SseUsageCollector {
    decoder: SseBuffer,
    usage:   Option<ResponseUsage>,
}

impl SseUsageCollector {
    fn push(&mut self, bytes: &[u8]) {
        for payload in self.decoder.push_with_done(bytes, true) {
            if payload == "[DONE]" {
                continue;
            }
            if let Some(usage) = response_usage_from_json_bytes(payload.as_bytes()) {
                merge_response_usage(&mut self.usage, usage);
            }
        }
    }

    fn usage(&self) -> Option<ResponseUsage> {
        self.usage
    }
}

fn merge_response_usage(current: &mut Option<ResponseUsage>, next: ResponseUsage) {
    match current {
        Some(current) => {
            current.prompt_tokens = max_usage_value(current.prompt_tokens, next.prompt_tokens);
            current.completion_tokens =
                max_usage_value(current.completion_tokens, next.completion_tokens);
            current.total_tokens = max_usage_value(current.total_tokens, next.total_tokens);
            current.cache_read_tokens =
                max_usage_value(current.cache_read_tokens, next.cache_read_tokens);
            current.cache_creation_tokens =
                max_usage_value(current.cache_creation_tokens, next.cache_creation_tokens);
            current.image_tokens = max_usage_value(current.image_tokens, next.image_tokens);
            current.audio_tokens = max_usage_value(current.audio_tokens, next.audio_tokens);
        }
        None => *current = Some(next),
    }
}

fn max_usage_value(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn is_gateway_event_stream_response(resp: &Response<GatewayBody>) -> bool {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| {
            content_type
                .to_ascii_lowercase()
                .contains("text/event-stream")
        })
}

fn upstream_request_id_from_headers(headers: &HeaderMap) -> String {
    [
        "x-request-id",
        "x-openai-request-id",
        "request-id",
        "anthropic-request-id",
        "x-goog-request-id",
        "cf-ray",
    ]
    .into_iter()
    .find_map(|name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
    .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_usage_collector_extracts_openai_usage() {
        let mut collector = SseUsageCollector::default();
        collector.push(
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":4,\"total_tokens\":16}}\n\n",
        );
        collector.push(b"data: [DONE]\n\n");

        assert_eq!(
            collector.usage(),
            Some(ResponseUsage {
                prompt_tokens:         Some(12),
                completion_tokens:     Some(4),
                total_tokens:          Some(16),
                cache_read_tokens:     None,
                cache_creation_tokens: None,
                image_tokens:          None,
                audio_tokens:          None,
            })
        );
    }

    #[test]
    fn sse_usage_collector_extracts_claude_usage_across_chunks() {
        let mut collector = SseUsageCollector::default();
        collector.push(b"event: message_delta\ndata: {\"type\":\"message_delta\",");
        collector.push(
            b"\"usage\":{\"input_tokens\":7,\"cache_read_input_tokens\":3,\"output_tokens\":5}}\n\n",
        );

        assert_eq!(
            collector.usage(),
            Some(ResponseUsage {
                prompt_tokens:         Some(10),
                completion_tokens:     Some(5),
                total_tokens:          Some(15),
                cache_read_tokens:     Some(3),
                cache_creation_tokens: None,
                image_tokens:          None,
                audio_tokens:          None,
            })
        );
    }

    #[test]
    fn sse_usage_collector_merges_max_values() {
        let mut collector = SseUsageCollector::default();
        collector.push(b"data: {\"usage\":{\"prompt_tokens\":10}}\n\n");
        collector.push(b"data: {\"usage\":{\"completion_tokens\":6,\"total_tokens\":16}}\n\n");

        assert_eq!(
            collector.usage(),
            Some(ResponseUsage {
                prompt_tokens:         Some(10),
                completion_tokens:     Some(6),
                total_tokens:          Some(16),
                cache_read_tokens:     None,
                cache_creation_tokens: None,
                image_tokens:          None,
                audio_tokens:          None,
            })
        );
    }

    #[test]
    fn extracts_upstream_request_id_from_common_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-openai-request-id", HeaderValue::from_static(" req_123 "));

        assert_eq!(upstream_request_id_from_headers(&headers), "req_123");
    }
}
