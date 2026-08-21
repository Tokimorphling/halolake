use super::*;
use crate::{
    gateway::{
        ClaudeVersion, ConnectTimeout, GatewayAuthPolicy, GeminiApiVersion, PassAnthropicBeta,
        SnapshotStore, UpstreamReadTimeout,
    },
    upstream_proxy::ProxyTransportService,
};

const MAX_UPSTREAM_ATTEMPTS: u8 = 3;
const DEFAULT_PRECONSUMED_TOKENS: u64 = 500;

fn should_retry_upstream(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    ) || status == StatusCode::UNAUTHORIZED
        || status == StatusCode::FORBIDDEN
}

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
    next_key:  Rc<Cell<u64>>,
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
            + Param<ProxyTransportService>
            + Param<UsageReporter>
            + Param<ChannelFeedbackReporter>,
    {
        Self {
            router:           AuthRouteService {
                snapshots: params.param(),
                auth:      Param::<GatewayAuthPolicy>::param(params).0,
                next_key:  Rc::new(Cell::new(0)),
            },
            relay:            RelayService::from_params(params),
            usage:            Param::<UsageReporter>::param(params),
            channel_feedback: Param::<ChannelFeedbackReporter>::param(params),
        }
    }
}

impl<CX> Service<(GatewayRequest, CX)> for ChatGatewayService
where
    CX: Clone + ParamSet<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
    <CX as ParamSet<RequestAuth>>::Transformed: ParamSet<Rc<RouteContext>>,
    <<CX as ParamSet<RequestAuth>>::Transformed as ParamSet<Rc<RouteContext>>>::Transformed:
        ParamRef<RequestAuth>
            + ParamRef<Rc<RouteContext>>
            + ParamRef<RequestId>
            + ParamRef<PeerAddr>,
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
        let tokens = match self.router.extract_tokens(&req.headers) {
            Some(tokens) => tokens,
            None => {
                return Ok(json_error(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "missing gateway token",
                ));
            }
        };
        let lookup = RouteLookup {
            tokens,
            requested_model: openai_req.model.clone().into(),
            path: req.path.clone(),
            headers: req.headers.clone(),
            body: req.body.clone(),
            peer_ip: ParamRef::<PeerAddr>::param_ref(&cx).0.ip(),
        };
        let mut route = match self.router.call(lookup.clone()).await {
            Ok(route) => route,
            Err(err) => return Ok(route_error_response(err)),
        };
        let reserve_cx = cx
            .clone()
            .param_set(route.auth.clone())
            .param_set(route.route.clone());
        let reserved_quota = match reserve_request_quota(
            &self.usage,
            &usage_event_parts(&reserve_cx, openai_req.is_stream()),
            &req.body,
            // Reuse the typed parse instead of re-parsing the raw body.
            openai_req
                .max_completion_tokens
                .map(u64::from)
                .max(openai_req.max_tokens.map(u64::from)),
        )
        .await
        {
            Ok(reserved) => reserved,
            Err(response) => return Ok(response),
        };
        let started = Instant::now();
        let mut final_response = None;
        let mut failed_channels = Vec::new();
        for attempt in 0..MAX_UPSTREAM_ATTEMPTS {
            let attempt_cx = cx
                .clone()
                .param_set(route.auth.clone())
                .param_set(route.route.clone());
            let attempt_usage = usage_event_parts(&attempt_cx, openai_req.is_stream());
            let response = self
                .relay
                .call(OpenAiChatRelayRequest {
                    request:            openai_req.clone(),
                    raw_body:           req.body.clone(),
                    downstream_headers: req.headers.clone(),
                    cx:                 attempt_cx,
                })
                .await?;
            if attempt + 1 < MAX_UPSTREAM_ATTEMPTS
                && !route
                    .affinity
                    .as_ref()
                    .is_some_and(|affinity| affinity.skip_retry_on_failure)
                && should_retry_upstream(response.status())
            {
                let previous = route.route.channel_id.to_string();
                failed_channels.push(previous);
                if let Ok(next) = self
                    .router
                    .retry_route(&lookup, u64::from(attempt + 1), &failed_channels)
                    .await
                {
                    report_channel_feedback(&self.channel_feedback, &attempt_usage, &response);
                    drop(response);
                    route = next;
                    continue;
                }
            }
            final_response = Some(response);
            break;
        }
        let resp = final_response.expect("upstream attempt loop always produces a response");
        let final_cx = cx
            .clone()
            .param_set(route.auth.clone())
            .param_set(route.route.clone());
        let mut usage = usage_event_parts(&final_cx, openai_req.is_stream());
        usage.pre_consumed_quota = reserved_quota;
        let affinity = route.affinity.clone();
        let affinity_channel_id = route.route.channel_id.clone();
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
            + Param<ProxyTransportService>
            + Param<UsageReporter>
            + Param<ChannelFeedbackReporter>,
    {
        Self {
            router:           AuthRouteService {
                snapshots: params.param(),
                auth:      Param::<GatewayAuthPolicy>::param(params).0,
                next_key:  Rc::new(Cell::new(0)),
            },
            relay:            RelayService::from_params(params),
            usage:            Param::<UsageReporter>::param(params),
            channel_feedback: Param::<ChannelFeedbackReporter>::param(params),
        }
    }
}

impl<CX> Service<(GatewayRequest, CX)> for ImageGatewayService
where
    CX: Clone + ParamSet<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
    <CX as ParamSet<RequestAuth>>::Transformed: ParamSet<Rc<RouteContext>>,
    <<CX as ParamSet<RequestAuth>>::Transformed as ParamSet<Rc<RouteContext>>>::Transformed:
        ParamRef<RequestAuth>
            + ParamRef<Rc<RouteContext>>
            + ParamRef<RequestId>
            + ParamRef<PeerAddr>,
{
    type Response = Response<GatewayBody>;
    type Error = Infallible;

    async fn call(&self, (req, cx): (GatewayRequest, CX)) -> Result<Self::Response, Self::Error> {
        let parsed = match parse_openai_image_request(&req) {
            Ok(parsed) => parsed,
            Err(resp) => return Ok(resp),
        };
        let tokens = match self.router.extract_tokens(&req.headers) {
            Some(tokens) => tokens,
            None => {
                return Ok(json_error(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "missing gateway token",
                ));
            }
        };
        let lookup = RouteLookup {
            tokens,
            requested_model: parsed.model.clone().into(),
            path: req.path.clone(),
            headers: req.headers.clone(),
            body: req.body.clone(),
            peer_ip: ParamRef::<PeerAddr>::param_ref(&cx).0.ip(),
        };
        let mut route = match self.router.call(lookup.clone()).await {
            Ok(route) => route,
            Err(err) => return Ok(route_error_response(err)),
        };
        let reserve_cx = cx
            .clone()
            .param_set(route.auth.clone())
            .param_set(route.route.clone());
        let output_cap = match &parsed.payload {
            OpenAiImagePayload::Json { value, .. } => json_output_cap(value),
            OpenAiImagePayload::Multipart => None,
        };
        let reserved_quota = match reserve_request_quota(
            &self.usage,
            &usage_event_parts(&reserve_cx, parsed.stream),
            &req.body,
            output_cap,
        )
        .await
        {
            Ok(reserved) => reserved,
            Err(response) => return Ok(response),
        };
        let started = Instant::now();
        let mut final_response = None;
        let mut failed_channels = Vec::new();
        for attempt in 0..MAX_UPSTREAM_ATTEMPTS {
            let attempt_cx = cx
                .clone()
                .param_set(route.auth.clone())
                .param_set(route.route.clone());
            let attempt_usage = usage_event_parts(&attempt_cx, parsed.stream);
            let response = self
                .relay
                .call(OpenAiImageRelayRequest {
                    kind:               parsed.kind,
                    stream:             parsed.stream,
                    payload:            parsed.payload.clone(),
                    body:               req.body.clone(),
                    downstream_headers: req.headers.clone(),
                    path:               req.path.clone(),
                    cx:                 attempt_cx,
                })
                .await?;
            if attempt + 1 < MAX_UPSTREAM_ATTEMPTS
                && !route
                    .affinity
                    .as_ref()
                    .is_some_and(|affinity| affinity.skip_retry_on_failure)
                && should_retry_upstream(response.status())
            {
                let previous = route.route.channel_id.to_string();
                failed_channels.push(previous);
                if let Ok(next) = self
                    .router
                    .retry_route(&lookup, u64::from(attempt + 1), &failed_channels)
                    .await
                {
                    report_channel_feedback(&self.channel_feedback, &attempt_usage, &response);
                    drop(response);
                    route = next;
                    continue;
                }
            }
            final_response = Some(response);
            break;
        }
        let resp = final_response.expect("upstream attempt loop always produces a response");
        let final_cx = cx
            .clone()
            .param_set(route.auth.clone())
            .param_set(route.route.clone());
        let mut usage = usage_event_parts(&final_cx, parsed.stream);
        usage.pre_consumed_quota = reserved_quota;
        let affinity = route.affinity.clone();
        let affinity_channel_id = route.route.channel_id.clone();
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
            + Param<ProxyTransportService>
            + Param<UsageReporter>
            + Param<ChannelFeedbackReporter>,
    {
        Self {
            router:           AuthRouteService {
                snapshots: params.param(),
                auth:      Param::<GatewayAuthPolicy>::param(params).0,
                next_key:  Rc::new(Cell::new(0)),
            },
            relay:            RelayService::from_params(params),
            usage:            Param::<UsageReporter>::param(params),
            channel_feedback: Param::<ChannelFeedbackReporter>::param(params),
        }
    }
}

impl<CX> Service<(GatewayRequest, CX)> for ClaudeMessagesGatewayService
where
    CX: Clone + ParamSet<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
    <CX as ParamSet<RequestAuth>>::Transformed: ParamSet<Rc<RouteContext>>,
    <<CX as ParamSet<RequestAuth>>::Transformed as ParamSet<Rc<RouteContext>>>::Transformed:
        ParamRef<RequestAuth>
            + ParamRef<Rc<RouteContext>>
            + ParamRef<RequestId>
            + ParamRef<PeerAddr>,
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
        let tokens = match self.router.extract_tokens(&req.headers) {
            Some(tokens) => tokens,
            None => {
                return Ok(json_error(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "missing gateway token",
                ));
            }
        };
        let lookup = RouteLookup {
            tokens,
            requested_model: model.into(),
            path: req.path.clone(),
            headers: req.headers.clone(),
            body: req.body.clone(),
            peer_ip: ParamRef::<PeerAddr>::param_ref(&cx).0.ip(),
        };
        let mut route = match self.router.call(lookup.clone()).await {
            Ok(route) => route,
            Err(err) => return Ok(route_error_response(err)),
        };
        let is_stream = value
            .get("stream")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let reserve_cx = cx
            .clone()
            .param_set(route.auth.clone())
            .param_set(route.route.clone());
        let reserved_quota = match reserve_request_quota(
            &self.usage,
            &usage_event_parts(&reserve_cx, is_stream),
            &req.body,
            json_output_cap(&value),
        )
        .await
        {
            Ok(reserved) => reserved,
            Err(response) => return Ok(response),
        };
        let started = Instant::now();
        let mut final_response = None;
        let mut failed_channels = Vec::new();
        for attempt in 0..MAX_UPSTREAM_ATTEMPTS {
            let attempt_cx = cx
                .clone()
                .param_set(route.auth.clone())
                .param_set(route.route.clone());
            let attempt_usage = usage_event_parts(&attempt_cx, is_stream);
            let response = self
                .relay
                .call(ClaudeMessagesRelayRequest {
                    value:              value.clone(),
                    path:               req.path.clone(),
                    downstream_headers: req.headers.clone(),
                    cx:                 attempt_cx,
                })
                .await?;
            if attempt + 1 < MAX_UPSTREAM_ATTEMPTS
                && !route
                    .affinity
                    .as_ref()
                    .is_some_and(|affinity| affinity.skip_retry_on_failure)
                && should_retry_upstream(response.status())
            {
                let previous = route.route.channel_id.to_string();
                failed_channels.push(previous);
                if let Ok(next) = self
                    .router
                    .retry_route(&lookup, u64::from(attempt + 1), &failed_channels)
                    .await
                {
                    report_channel_feedback(&self.channel_feedback, &attempt_usage, &response);
                    drop(response);
                    route = next;
                    continue;
                }
            }
            final_response = Some(response);
            break;
        }
        let resp = final_response.expect("upstream attempt loop always produces a response");
        let final_cx = cx
            .clone()
            .param_set(route.auth.clone())
            .param_set(route.route.clone());
        let mut usage = usage_event_parts(&final_cx, is_stream);
        usage.pre_consumed_quota = reserved_quota;
        let affinity = route.affinity.clone();
        let affinity_channel_id = route.route.channel_id.clone();
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
            + Param<ProxyTransportService>
            + Param<UsageReporter>
            + Param<ChannelFeedbackReporter>,
    {
        Self {
            router:           AuthRouteService {
                snapshots: params.param(),
                auth:      Param::<GatewayAuthPolicy>::param(params).0,
                next_key:  Rc::new(Cell::new(0)),
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
    <CX as ParamSet<RequestAuth>>::Transformed: ParamSet<Rc<RouteContext>>,
    <<CX as ParamSet<RequestAuth>>::Transformed as ParamSet<Rc<RouteContext>>>::Transformed:
        ParamRef<RequestAuth>
            + ParamRef<Rc<RouteContext>>
            + ParamRef<RequestId>
            + ParamRef<PeerAddr>,
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
        let tokens = match self.router.extract_tokens(&req.headers) {
            Some(tokens) => tokens,
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
                tokens,
                requested_model: requested_model.into(),
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
        let mut usage = usage_event_parts(&cx, stream);
        let reserved_quota =
            match reserve_request_quota(&self.usage, &usage, &req.body, None).await {
            Ok(reserved) => reserved,
            Err(response) => return Ok(response),
        };
        usage.pre_consumed_quota = reserved_quota;
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
            + Param<ProxyTransportService>
            + Param<UsageReporter>
            + Param<ChannelFeedbackReporter>,
    {
        Self {
            router:           AuthRouteService {
                snapshots: params.param(),
                auth:      Param::<GatewayAuthPolicy>::param(params).0,
                next_key:  Rc::new(Cell::new(0)),
            },
            relay:            RelayService::from_params(params),
            usage:            Param::<UsageReporter>::param(params),
            channel_feedback: Param::<ChannelFeedbackReporter>::param(params),
        }
    }
}

impl<CX> Service<(GatewayRequest, CX)> for RawOpenAiGatewayService
where
    CX: Clone + ParamSet<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
    <CX as ParamSet<RequestAuth>>::Transformed: ParamSet<Rc<RouteContext>>,
    <<CX as ParamSet<RequestAuth>>::Transformed as ParamSet<Rc<RouteContext>>>::Transformed:
        ParamRef<RequestAuth>
            + ParamRef<Rc<RouteContext>>
            + ParamRef<RequestId>
            + ParamRef<PeerAddr>,
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
        let tokens = match self.router.extract_tokens(&req.headers) {
            Some(tokens) => tokens,
            None => {
                return Ok(json_error(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "missing gateway token",
                ));
            }
        };
        let lookup = RouteLookup {
            tokens,
            requested_model: model.into(),
            path: req.path.clone(),
            headers: req.headers.clone(),
            body: req.body.clone(),
            peer_ip: ParamRef::<PeerAddr>::param_ref(&cx).0.ip(),
        };
        let mut route = match self.router.call(lookup.clone()).await {
            Ok(route) => route,
            Err(err) => return Ok(route_error_response(err)),
        };
        // new-api ForceStreamOption targets OpenAI chat/completions (and legacy
        // completions). Responses API carries usage in `response.completed` instead.
        let path_for_stream = req.path.split('?').next().unwrap_or(req.path.as_str());
        let is_stream = value
            .get("stream")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let reserve_cx = cx
            .clone()
            .param_set(route.auth.clone())
            .param_set(route.route.clone());
        let reserved_quota = match reserve_request_quota(
            &self.usage,
            &usage_event_parts(&reserve_cx, is_stream),
            &req.body,
            json_output_cap(&value),
        )
        .await
        {
            Ok(reserved) => reserved,
            Err(response) => return Ok(response),
        };
        let started = Instant::now();
        let mut final_response = None;
        let mut failed_channels = Vec::new();
        for attempt in 0..MAX_UPSTREAM_ATTEMPTS {
            // Mutate the owned value in place instead of deep-cloning the whole
            // body per attempt; both rewrites are idempotent across retries.
            value["model"] = JsonValue::String(route.route.upstream_model.to_string());
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
            let attempt_cx = cx
                .clone()
                .param_set(route.auth.clone())
                .param_set(route.route.clone());
            let attempt_usage = usage_event_parts(&attempt_cx, is_stream);
            let response = self
                .relay
                .call(OpenAiPassthroughRelayRequest {
                    path: req.path.clone(),
                    body,
                    downstream_headers: req.headers.clone(),
                    cx: attempt_cx,
                })
                .await?;
            if attempt + 1 < MAX_UPSTREAM_ATTEMPTS
                && !route
                    .affinity
                    .as_ref()
                    .is_some_and(|affinity| affinity.skip_retry_on_failure)
                && should_retry_upstream(response.status())
            {
                let previous = route.route.channel_id.to_string();
                failed_channels.push(previous);
                if let Ok(next) = self
                    .router
                    .retry_route(&lookup, u64::from(attempt + 1), &failed_channels)
                    .await
                {
                    report_channel_feedback(&self.channel_feedback, &attempt_usage, &response);
                    drop(response);
                    route = next;
                    continue;
                }
            }
            final_response = Some(response);
            break;
        }
        let resp = final_response.expect("upstream attempt loop always produces a response");
        let final_cx = cx
            .clone()
            .param_set(route.auth.clone())
            .param_set(route.route.clone());
        let mut usage = usage_event_parts(&final_cx, is_stream);
        usage.pre_consumed_quota = reserved_quota;
        let affinity = route.affinity.clone();
        let affinity_channel_id = route.route.channel_id.clone();
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
        let key_seed = self.next_key.get();
        self.next_key.set(key_seed.wrapping_add(1));
        self.select_route(&lookup, key_seed, true, &[])
    }
}

impl AuthRouteService {
    fn select_route(
        &self,
        lookup: &RouteLookup,
        key_seed: u64,
        with_affinity: bool,
        excluded_channel_ids: &[&str],
    ) -> Result<RouteParts, RouteError> {
        let state = self.snapshots.load();
        let snapshot = &state.snapshot;
        // The affinity cache is owned by the SnapshotStore, not the snapshot, so
        // it survives snapshot swaps.
        let affinity_cache = self.snapshots.affinity_cache();
        let user_agent = lookup
            .headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let mut last_error = RouteError::Unauthorized;
        for token in &lookup.tokens {
            let auth = match snapshot.authenticate(token) {
                Ok(auth) => auth,
                Err(err) => {
                    last_error = err;
                    continue;
                }
            };
            if let Err(err) = snapshot.authorize_ip(&auth, lookup.peer_ip) {
                last_error = err;
                continue;
            }
            let affinity = if with_affinity {
                snapshot.resolve_affinity(
                    ChannelAffinityRequest {
                        requested_model: &lookup.requested_model,
                        path: &lookup.path,
                        user_agent,
                        using_group: auth.token.group(),
                        body: &lookup.body,
                    },
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
                )
            } else {
                None
            };
            let (route, affinity_hit) = match affinity.as_ref() {
                Some(affinity) => match snapshot.route_with_affinity_seed(
                    &auth,
                    &lookup.requested_model,
                    Some(affinity),
                    affinity_cache,
                    key_seed,
                ) {
                    Ok(route) => route,
                    Err(err) => {
                        last_error = err;
                        continue;
                    }
                },
                None => {
                    let selected = if excluded_channel_ids.is_empty() {
                        snapshot.route_with_seed(&auth, &lookup.requested_model, key_seed)
                    } else {
                        snapshot.route_with_seed_excluding(
                            &auth,
                            &lookup.requested_model,
                            key_seed,
                            excluded_channel_ids,
                        )
                    };
                    match selected {
                        Ok(route) => (route, false),
                        Err(err) => {
                            last_error = err;
                            continue;
                        }
                    }
                }
            };
            if affinity_hit {
                debug!(
                    channel_id = %route.channel.id,
                    requested_model = %lookup.requested_model,
                    "channel affinity hit"
                );
            }
            let cached = state
                .channels
                .get(route.channel.id.as_str())
                .expect("cached channel parts exist for every indexed channel");
            let (api_key, api_key_index) = route.channel.select_api_key_with_index(key_seed);
            return Ok(RouteParts {
                auth:     RequestAuth {
                    user_id:  route.user_id.to_string(),
                    token_id: auth.token.id().to_string(),
                },
                route:    Rc::new(RouteContext {
                    channel_id: cached.channel_id.clone(),
                    management_channel_id: route.channel.management_id,
                    provider: route.channel.provider,
                    base_url: cached.base_url.clone(),
                    api_key: api_key.to_string().into(),
                    // Precomputed per snapshot; selected with the same seed
                    // rule as select_api_key_with_index.
                    api_key_fingerprint: cached.fingerprint_for_seed(key_seed),
                    api_key_index,
                    using_group: route.using_group.to_string().into(),
                    // decision.requested_model always equals the lookup input,
                    // so reuse its FastStr instead of copying from the borrow.
                    requested_model: lookup.requested_model.clone(),
                    upstream_model: route.upstream_model.to_string().into(),
                    proxy: cached.proxy.clone(),
                    header_override: cached.header_override.clone(),
                    upstream_endpoint_type: cached.endpoint_type.clone(),
                }),
                affinity: affinity.map(route_affinity_context),
            });
        }
        Err(last_error)
    }

    /// Select a fresh route after an upstream failure. Affinity is bypassed so
    /// a pinned, unhealthy channel cannot trap retries. Incrementing the seed
    /// also rotates multi-key channels when they are the only candidate.
    async fn retry_route(
        &self,
        lookup: &RouteLookup,
        attempt: u64,
        excluded_channels: &[String],
    ) -> Result<RouteParts, RouteError> {
        let base_seed = self.next_key.get().wrapping_add(attempt.saturating_sub(1));
        let excluded_refs = excluded_channels
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        self.select_route(lookup, base_seed, false, &excluded_refs)
            .or_else(|_| self.select_route(lookup, base_seed, false, &[]))
    }

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
                cache_key:             affinity.cache_key.clone(),
                ttl_seconds:           affinity.ttl_seconds,
                cached_channel_id:     None,
                rule_name:             affinity.rule_name.clone(),
                skip_retry_on_failure: affinity.skip_retry_on_failure,
            },
            channel_id,
        );
    }
}

fn route_affinity_context(candidate: ChannelAffinityCandidate) -> RouteAffinityContext {
    RouteAffinityContext {
        cache_key:             candidate.cache_key,
        ttl_seconds:           candidate.ttl_seconds,
        rule_name:             candidate.rule_name,
        skip_retry_on_failure: candidate.skip_retry_on_failure,
    }
}

#[derive(Debug, Clone)]
struct UsageEventParts {
    request_id:          String,
    user_id:             String,
    token_id:            String,
    channel_id:          String,
    api_key_index:       Option<usize>,
    api_key_fingerprint: String,
    using_group:         String,
    requested_model:     String,
    upstream_model:      String,
    is_stream:           bool,
    ip:                  String,
    upstream_request_id: String,
    pre_consumed_quota:  Option<i64>,
}

fn usage_event_parts<CX>(cx: &CX, is_stream: bool) -> UsageEventParts
where
    CX: ParamRef<RequestAuth>
        + ParamRef<Rc<RouteContext>>
        + ParamRef<RequestId>
        + ParamRef<PeerAddr>,
{
    let auth = ParamRef::<RequestAuth>::param_ref(cx);
    let route = ParamRef::<Rc<RouteContext>>::param_ref(cx);
    let request_id = ParamRef::<RequestId>::param_ref(cx);
    let peer = ParamRef::<PeerAddr>::param_ref(cx);
    UsageEventParts {
        request_id: request_id.0.clone(),
        user_id: auth.user_id.clone(),
        token_id: auth.token_id.clone(),
        channel_id: reporting_channel_id(route.management_channel_id, &route.channel_id),
        api_key_index: route.api_key_index,
        api_key_fingerprint: route.api_key_fingerprint.to_string(),
        using_group: route.using_group.to_string(),
        requested_model: route.requested_model.to_string(),
        upstream_model: route.upstream_model.to_string(),
        is_stream,
        ip: peer.0.ip().to_string(),
        upstream_request_id: String::new(),
        pre_consumed_quota: None,
    }
}

async fn reserve_request_quota(
    reporter: &UsageReporter,
    parts: &UsageEventParts,
    body: &[u8],
    output_cap: Option<u64>,
) -> Result<Option<i64>, Response<GatewayBody>> {
    let (prompt_tokens, completion_tokens) = estimate_request_tokens(body, output_cap);
    let event = UsageEvent {
        request_id:            parts.request_id.clone(),
        user_id:               parts.user_id.clone(),
        token_id:              parts.token_id.clone(),
        channel_id:            String::new(),
        group:                 parts.using_group.clone(),
        model:                 parts.requested_model.clone(),
        upstream_model:        parts.upstream_model.clone(),
        prompt_tokens:         Some(prompt_tokens),
        completion_tokens:     Some(completion_tokens),
        total_tokens:          Some(prompt_tokens.saturating_add(completion_tokens)),
        cache_read_tokens:     None,
        cache_creation_tokens: None,
        image_tokens:          None,
        audio_tokens:          None,
        quota:                 None,
        pre_consumed_quota:    None,
        status:                UsageStatus::Success,
        latency_ms:            0,
        first_response_ms:     None,
        is_stream:             parts.is_stream,
        ip:                    parts.ip.clone(),
        upstream_request_id:   String::new(),
        created_at_unix_ms:    now_unix_ms_i64(),
    };
    match reporter.preconsume(event).await {
        Ok(reserved) => Ok(reserved),
        Err(UsageError::InsufficientQuota) => Err(json_error(
            StatusCode::PAYMENT_REQUIRED,
            "insufficient_quota",
            "quota is insufficient for this request",
        )),
        Err(err) => {
            warn!(?err, "quota reservation unavailable; rejecting request");
            Err(json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "quota_unavailable",
                "quota service is unavailable",
            ))
        }
    }
}

fn estimate_request_tokens(body: &[u8], output_cap: Option<u64>) -> (u64, u64) {
    // Match new-api's admission behavior: when the client does not provide an
    // output cap, reserve at least its default pre-consume token floor instead
    // of admitting a potentially long generation on only a tiny JSON body.
    let input = (body.len() as u64 / 4)
        .max(1)
        .max(DEFAULT_PRECONSUMED_TOKENS);
    let output = output_cap.unwrap_or(0).min(1_000_000);
    (input.min(10_000_000), output)
}

/// Max explicit output cap across the OpenAI-style keys, read from an
/// already-parsed body so quota estimation never re-parses the payload.
fn json_output_cap(value: &JsonValue) -> Option<u64> {
    ["max_tokens", "max_completion_tokens", "max_output_tokens"]
        .into_iter()
        .filter_map(|key| value.get(key).and_then(JsonValue::as_u64))
        .max()
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
        // Tee the stream into a monoio-http payload while collecting usage from
        // SSE frames via monoio-http stream payload.
        let new_body = wrap_streaming_usage_body(body, reporter.clone(), parts, status, started);
        *resp.body_mut() = new_body;
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
    reporter.report(channel_feedback_event(parts, meta));
}

fn channel_feedback_event(
    parts: &UsageEventParts,
    meta: &ChannelFeedbackMeta,
) -> ChannelFeedbackEvent {
    ChannelFeedbackEvent {
        request_id:          parts.request_id.clone(),
        channel_id:          parts.channel_id.clone(),
        api_key_index:       parts.api_key_index,
        api_key_fingerprint: Some(parts.api_key_fingerprint.clone()),
        status_code:         meta.status_code,
        reason:              meta.reason,
        message:             truncate_feedback_message(&meta.message),
        created_at_unix_ms:  now_unix_ms_i64(),
    }
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
        pre_consumed_quota: parts.pre_consumed_quota,
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
    reporter: UsageReporter,
    parts: UsageEventParts,
    status: UsageStatus,
    started: Instant,
) -> GatewayBody {
    let body = stream_body_from_async(move |mut sender| async move {
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
            pre_consumed_quota: parts.pre_consumed_quota,
            status,
            latency_ms,
            first_response_ms,
            is_stream: parts.is_stream,
            ip: parts.ip,
            upstream_request_id: parts.upstream_request_id,
            created_at_unix_ms: now_unix_ms_i64(),
        });
    });
    body
}

#[derive(Default)]
struct SseUsageCollector {
    decoder: SseBuffer,
    usage:   Option<ResponseUsage>,
}

impl SseUsageCollector {
    fn push(&mut self, bytes: &[u8]) {
        let usage = &mut self.usage;
        self.decoder.for_each_payload(bytes, false, |payload| {
            if !may_contain_response_usage(payload) {
                return;
            }
            if let Some(next) = response_usage_from_json_bytes(payload) {
                merge_response_usage(usage, next);
            }
        });
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
    fn extract_tokens(&self, headers: &HeaderMap) -> Option<Vec<String>> {
        gateway_token_candidates(&self.auth, headers)
    }
}

fn gateway_token_candidates(auth: &AuthConfig, headers: &HeaderMap) -> Option<Vec<String>> {
    let mut tokens = Vec::with_capacity(2);
    if auth.accept_bearer
        && let Some(token) = bearer_token(headers)
    {
        tokens.push(token);
    }
    if auth.accept_x_api_key {
        if let Some(token) = headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_string)
        {
            if !tokens.iter().any(|candidate| candidate == &token) {
                tokens.push(token);
            }
        }
    }
    (!tokens.is_empty()).then_some(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_auth_keeps_all_distinct_enabled_candidates() {
        let auth = AuthConfig {
            accept_bearer:    true,
            accept_x_api_key: true,
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer stale".parse().unwrap());
        headers.insert("x-api-key", "valid".parse().unwrap());
        assert_eq!(
            gateway_token_candidates(&auth, &headers),
            Some(vec!["stale".to_string(), "valid".to_string()])
        );

        headers.insert("x-api-key", "stale".parse().unwrap());
        assert_eq!(
            gateway_token_candidates(&auth, &headers),
            Some(vec!["stale".to_string()])
        );
    }

    #[test]
    fn quota_estimate_uses_new_api_floor_and_explicit_output_limit() {
        assert_eq!(
            estimate_request_tokens(br#"{"model":"gpt","messages":[]}"#, None),
            (DEFAULT_PRECONSUMED_TOKENS, 0)
        );
        assert_eq!(
            estimate_request_tokens(br#"{"max_tokens":2048}"#, Some(2048)),
            (DEFAULT_PRECONSUMED_TOKENS, 2048)
        );
    }

    #[test]
    fn json_output_cap_takes_max_across_keys() {
        assert_eq!(
            json_output_cap(&serde_json::json!({
                "max_tokens": 2048,
                "max_output_tokens": 1024
            })),
            Some(2048)
        );
        assert_eq!(json_output_cap(&serde_json::json!({})), None);
    }

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

    #[test]
    fn api_key_fingerprint_is_sha256_and_not_plaintext() {
        let fingerprint = api_key_fingerprint("secret");
        assert_eq!(
            fingerprint,
            "sha256:2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b"
        );
        assert!(!fingerprint.contains("secret"));
    }

    #[test]
    fn feedback_event_uses_management_id_and_selected_key_fingerprint() {
        let parts = UsageEventParts {
            request_id:          "req-1".to_string(),
            user_id:             "user-1".to_string(),
            token_id:            "token-1".to_string(),
            channel_id:          reporting_channel_id(Some(42), "route-alias"),
            api_key_index:       Some(3),
            api_key_fingerprint: api_key_fingerprint("secret"),
            using_group:         "default".to_string(),
            requested_model:     "model-a".to_string(),
            upstream_model:      "model-a".to_string(),
            is_stream:           false,
            ip:                  "127.0.0.1".to_string(),
            upstream_request_id: String::new(),
            pre_consumed_quota:  None,
        };
        let event =
            channel_feedback_event(&parts, &ChannelFeedbackMeta::transport("connection failed"));

        assert_eq!(event.channel_id, "42");
        assert_eq!(event.api_key_index, Some(3));
        assert_eq!(
            event.api_key_fingerprint.as_deref(),
            Some("sha256:2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b")
        );
    }
}
