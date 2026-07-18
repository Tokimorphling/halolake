use super::*;
use crate::{
    gateway::{
        ClaudeVersion, ConnectTimeout, GeminiApiVersion, PassAnthropicBeta, UpstreamReadTimeout,
    },
    upstream_proxy::{ProxyTransportRequest, ProxyTransportService, parse_proxy_endpoint},
};
use monoio_transports::{
    connectors::pollio::PollIo,
    http::hyper::{HyperBody, HyperH1Connector, MonoioBody as HyperIncomingBody},
};

pub(crate) type HyperRequestBody =
    HyperBody<HttpBody, Option<std::result::Result<Bytes, HttpError>>>;
pub(crate) type StreamingHttpUpstream =
    HyperH1Connector<PollIo<TcpConnector>, SocketAddr, HyperRequestBody>;

const CODEX_DEFAULT_ORIGINATOR: &str = "codex-tui";
const CODEX_DEFAULT_USER_AGENT: &str =
    "codex-tui/0.135.0 (Mac OS 26.5.0; arm64) iTerm.app/3.6.10 (codex-tui; 0.135.0)";

#[derive(Clone)]
pub(crate) struct RelayService {
    claude_version: Arc<str>,
    pass_anthropic_beta: bool,
    gemini_api_version: Arc<str>,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    dns: LocalDnsResolver,
    http: Rc<StreamingHttpUpstream>,
    https: HttpsUpstream,
    proxy_transport: ProxyTransportService,
}

impl RelayService {
    pub(crate) fn from_params<C>(params: &C) -> Self
    where
        C: Param<ClaudeVersion>
            + Param<GeminiApiVersion>
            + Param<PassAnthropicBeta>
            + Param<ConnectTimeout>
            + Param<UpstreamReadTimeout>
            + Param<ProxyTransportService>,
    {
        let read_timeout = <C as Param<UpstreamReadTimeout>>::param(params).0;
        let mut https = HttpsUpstream::default();
        https.set_read_timeout(read_timeout);

        Self {
            claude_version: <C as Param<ClaudeVersion>>::param(params).0,
            pass_anthropic_beta: <C as Param<PassAnthropicBeta>>::param(params).0,
            gemini_api_version: <C as Param<GeminiApiVersion>>::param(params).0,
            connect_timeout: <C as Param<ConnectTimeout>>::param(params).0,
            read_timeout,
            dns: LocalDnsResolver::default(),
            http: Rc::new(HyperH1Connector::new(PollIo(TcpConnector::default()))),
            https,
            proxy_transport: <C as Param<ProxyTransportService>>::param(params),
        }
    }

    async fn send_upstream(
        &self,
        channel_identity: &str,
        uri: Uri,
        req: Request<HttpBody>,
        proxy: &ProxyRoute,
    ) -> Result<Response<HttpBody>> {
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
            %channel_identity,
            proxy = proxy.redacted_label(),
            connect_timeout_ms = ?self.connect_timeout.map(|d| d.as_millis()),
            read_timeout_ms = ?self.read_timeout.map(|d| d.as_millis()),
            "upstream request prepared"
        );

        match proxy {
            ProxyRoute::Required(proxy_url) => {
                return self
                    .send_upstream_via_proxy(channel_identity, uri, req, proxy_url)
                    .await;
            }
            ProxyRoute::Unavailable => {
                anyhow::bail!("configured upstream proxy is unavailable");
            }
            ProxyRoute::Direct => {}
        }

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
            let addr = timeout_opt(self.connect_timeout, self.dns.resolve(host, port))
                .await
                .context("http upstream DNS timeout")??;
            debug!(%host, port, %addr, "acquiring http upstream connection");
            let (resp, can_reuse) = send_pooled_http1(
                &self.http,
                addr,
                req,
                self.connect_timeout,
                self.read_timeout,
            )
            .await
            .with_context(|| format!("send http upstream request {host}{path}"))?;
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

    async fn send_upstream_via_proxy(
        &self,
        channel_identity: &str,
        uri: Uri,
        req: Request<HttpBody>,
        proxy_url: &str,
    ) -> Result<Response<HttpBody>> {
        let proxy = parse_proxy_endpoint(proxy_url)?;
        let target_host = uri.host().context("upstream uri missing host")?.to_string();
        let target_port =
            uri.port_u16()
                .unwrap_or(if uri.scheme() == Some(&http::uri::Scheme::HTTPS) {
                    443
                } else {
                    80
                });

        debug!(
            %channel_identity,
            proxy = %proxy.redacted_label(),
            %target_host,
            target_port,
            "connecting upstream via proxy"
        );

        self.proxy_transport
            .call(ProxyTransportRequest {
                channel_identity: channel_identity.to_string(),
                proxy,
                target_host: target_host.clone(),
                target_port,
                target_tls: uri.scheme() == Some(&http::uri::Scheme::HTTPS),
                request: req,
            })
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("send request via proxy to {target_host}:{target_port}"))
    }
}

/// Worker-local pooled HTTP/1 client that returns after response headers and
/// pumps the body concurrently. The native monoio-transports H1 connection
/// drains the complete response inside `send_request`, which breaks SSE TTFT
/// and prevents keep-alive reuse.
pub(crate) async fn send_pooled_http1(
    connector: &StreamingHttpUpstream,
    addr: SocketAddr,
    mut req: Request<HttpBody>,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
) -> Result<(Response<HttpBody>, bool)> {
    let connect = connector.connect(addr);
    let mut conn = timeout_opt(connect_timeout, connect)
        .await
        .context("acquire pooled HTTP/1 connection")??;
    let reused = conn.is_reused();
    prepare_hyper_request_headers(&mut req);
    let req = req.map(HyperRequestBody::new);
    let response = match read_timeout {
        Some(read_timeout) => monoio::time::timeout(read_timeout, conn.send_request(req))
            .await
            .map_err(|_| anyhow::anyhow!("upstream response head idle timeout"))?,
        None => conn.send_request(req).await,
    }
    .context("send pooled HTTP/1 request")?;
    let can_reuse = !conn.is_closed();
    let (parts, incoming) = response.into_parts();
    // Keep the pooled H1 lease out of the idle queue until the response body
    // reaches EOF. Returning the sender immediately after the response head
    // makes a concurrent request wait on this busy connection instead of
    // opening another socket, which serializes long-lived SSE requests.
    let body = streaming_hyper_response_body(HyperIncomingBody::new(incoming), read_timeout, conn);
    debug!(%addr, http1_reused = reused, can_reuse, "pooled HTTP/1 response head received");
    Ok((Response::from_parts(parts, body), can_reuse))
}

fn prepare_hyper_request_headers(req: &mut Request<HttpBody>) {
    // Framing follows the actual body, never downstream or channel override
    // headers. Keeping stale CL/TE can truncate a request, poison H1 reuse, or
    // create an ambiguous smuggling boundary. Hyper supplies chunked encoding
    // automatically when a streaming body has no content length.
    req.headers_mut().remove(header::CONTENT_LENGTH);
    req.headers_mut().remove(header::TRANSFER_ENCODING);

    let content_length = match req.body().stream_hint() {
        monoio_http::common::body::StreamHint::None => Some(0),
        monoio_http::common::body::StreamHint::Fixed => {
            req.body().ready_data().map(|body| body.len())
        }
        monoio_http::common::body::StreamHint::Stream => None,
    };
    if let Some(content_length) = content_length {
        let value = HeaderValue::from_str(&content_length.to_string())
            .expect("usize content length is always a valid header value");
        req.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
}

fn streaming_hyper_response_body<B, G>(
    mut body: B,
    read_timeout: Option<Duration>,
    connection_guard: G,
) -> HttpBody
where
    B: MonoioBody<Data = Bytes> + 'static,
    B::Error: std::fmt::Display + 'static,
    G: 'static,
{
    let (payload, mut sender) = stream_payload_pair::<Bytes, HttpError>();
    monoio::spawn(async move {
        loop {
            let next = match read_timeout {
                Some(read_timeout) => {
                    match monoio::time::timeout(read_timeout, body.next_data()).await {
                        Ok(next) => next,
                        Err(_) => {
                            sender.feed_error(HttpError::IOError(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "upstream response body idle timeout",
                            )));
                            sender.feed_data(None);
                            break;
                        }
                    }
                }
                None => body.next_data().await,
            };

            match next {
                Some(Ok(data)) => sender.feed_data(Some(data)),
                Some(Err(error)) => {
                    sender.feed_error(HttpError::IOError(std::io::Error::other(format!(
                        "hyper upstream response body: {error}"
                    ))));
                    sender.feed_data(None);
                    break;
                }
                None => {
                    sender.feed_data(None);
                    break;
                }
            }
        }
        // Drop the incoming body before returning the sender lease to the
        // connector pool. On cancellation/error this lets Hyper mark the
        // connection unusable before `Pooled::drop` evaluates it.
        drop(body);
        drop(connection_guard);
    });
    HttpBody::H1(Payload::Stream(payload))
}

pub(crate) struct OpenAiChatRelayRequest<CX> {
    pub(crate) request: openai::ChatCompletionRequest,
    /// Original downstream body; used for redacted structured logs and
    /// OpenAI passthrough so unknown fields (e.g. reasoning_effort) survive.
    pub(crate) raw_body: Bytes,
    pub(crate) downstream_headers: HeaderMap,
    pub(crate) cx: CX,
}

impl<CX> Service<OpenAiChatRelayRequest<CX>> for RelayService
where
    CX: ParamRef<RouteContext> + ParamRef<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    type Response = Response<GatewayBody>;
    type Error = Infallible;

    async fn call(&self, req: OpenAiChatRelayRequest<CX>) -> Result<Self::Response, Self::Error> {
        let route = ParamRef::<RouteContext>::param_ref(&req.cx).clone();
        debug_relay(
            &req.cx,
            req.request.is_stream(),
            Some(req.raw_body.as_ref()),
        );

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
                    .send_claude_json(&route, &req.downstream_headers, &claude_req, "/v1/messages")
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
                        return Ok(upstream_transport_error_response(&err.to_string()));
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
                let prepared = match prepare_openai_text_upstream(
                    "/v1/chat/completions",
                    &req.raw_body,
                    &route.upstream_model,
                    &route.upstream_endpoint_type,
                ) {
                    Ok(v) => v,
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &format!("failed to prepare OpenAI upstream request: {err}"),
                        ));
                    }
                };
                let target = OpenAiResponseTarget::ChatCompletions {
                    stream: req.request.is_stream(),
                    requested_model: route.requested_model.clone(),
                };
                self.openai_passthrough(route, prepared, &req.downstream_headers, target)
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
                    .send_gemini_json(&route, &req.downstream_headers, &gemini_req, &upstream_path)
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
                        return Ok(upstream_transport_error_response(&err.to_string()));
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

pub(crate) struct ClaudeMessagesRelayRequest<CX> {
    pub(crate) value: JsonValue,
    pub(crate) downstream_headers: HeaderMap,
    pub(crate) cx: CX,
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
        let body_bytes = serde_json::to_vec(&req.value).ok();
        debug_relay(&req.cx, is_stream, body_bytes.as_deref());

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
                        return Ok(upstream_transport_error_response(&err.to_string()));
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
                let chat_body = match serde_json::to_vec(&openai_req) {
                    Ok(b) => Bytes::from(b),
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &format!("failed to serialize OpenAI request: {err}"),
                        ));
                    }
                };
                let prepared = match prepare_openai_text_upstream(
                    "/v1/chat/completions",
                    &chat_body,
                    &route.upstream_model,
                    &route.upstream_endpoint_type,
                ) {
                    Ok(v) => v,
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &format!("failed to prepare OpenAI upstream request: {err}"),
                        ));
                    }
                };
                let upstream = match self
                    .send_openai_body(
                        &route,
                        &req.downstream_headers,
                        prepared.body,
                        &prepared.path,
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
                        return Ok(upstream_transport_error_response(&err.to_string()));
                    }
                };
                if !upstream.status().is_success() {
                    return Ok(upstream_error_response(upstream).await);
                }
                let upstream = if prepared.protocol == OpenAiTextProtocol::Responses {
                    if openai_req.is_stream() {
                        stream_responses_as_openai_chat(
                            upstream,
                            route.requested_model.clone(),
                            prepared.tool_name_reverse,
                        )
                    } else {
                        buffered_responses_as_openai_chat(
                            upstream,
                            route.requested_model.clone(),
                            prepared.tool_name_reverse,
                        )
                        .await
                    }
                } else {
                    upstream
                };
                if !upstream.status().is_success() {
                    return Ok(upstream);
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
                    .send_gemini_json(&route, &req.downstream_headers, &gemini_req, &upstream_path)
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
                        return Ok(upstream_transport_error_response(&err.to_string()));
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

pub(crate) struct OpenAiImageRelayRequest<CX> {
    pub(crate) kind: OpenAiImageRouteKind,
    pub(crate) stream: bool,
    pub(crate) payload: OpenAiImagePayload,
    pub(crate) body: Bytes,
    pub(crate) downstream_headers: HeaderMap,
    pub(crate) path: String,
    pub(crate) cx: CX,
}

pub(crate) struct OpenAiPassthroughRelayRequest<CX> {
    pub(crate) path: String,
    pub(crate) body: Bytes,
    pub(crate) downstream_headers: HeaderMap,
    pub(crate) cx: CX,
}

pub(crate) struct GeminiNativeRelayRequest<CX> {
    pub(crate) path: String,
    pub(crate) stream: bool,
    pub(crate) body: Bytes,
    pub(crate) downstream_headers: HeaderMap,
    pub(crate) cx: CX,
}

impl<CX> Service<OpenAiImageRelayRequest<CX>> for RelayService
where
    CX: ParamRef<RouteContext> + ParamRef<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    type Response = Response<GatewayBody>;
    type Error = Infallible;

    async fn call(&self, req: OpenAiImageRelayRequest<CX>) -> Result<Self::Response, Self::Error> {
        let route = ParamRef::<RouteContext>::param_ref(&req.cx).clone();
        debug_relay(&req.cx, req.stream, Some(req.body.as_ref()));

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
                        return Ok(upstream_transport_error_response(&err.to_string()));
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
                    .send_gemini_json(&route, &req.downstream_headers, &gemini_req, &upstream_path)
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
                        return Ok(upstream_transport_error_response(&err.to_string()));
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
        debug_relay(
            &req.cx,
            is_stream_like(&req.downstream_headers),
            Some(req.body.as_ref()),
        );
        if route.provider != Provider::OpenAi {
            return Ok(json_error(
                StatusCode::BAD_GATEWAY,
                "routing_error",
                "raw OpenAI passthrough requires an OpenAI provider channel",
            ));
        }
        let prepared = match prepare_openai_text_upstream(
            &req.path,
            &req.body,
            &route.upstream_model,
            &route.upstream_endpoint_type,
        ) {
            Ok(v) => v,
            Err(err) => {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("failed to prepare OpenAI upstream request: {err}"),
                ));
            }
        };
        self.openai_passthrough(
            route,
            prepared,
            &req.downstream_headers,
            OpenAiResponseTarget::Passthrough,
        )
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
        debug_relay(&req.cx, req.stream, Some(req.body.as_ref()));

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
                        return Ok(upstream_transport_error_response(&err.to_string()));
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
                let chat_body = match serde_json::to_vec(&openai_req) {
                    Ok(b) => Bytes::from(b),
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &format!("failed to serialize OpenAI request: {err}"),
                        ));
                    }
                };
                let prepared = match prepare_openai_text_upstream(
                    "/v1/chat/completions",
                    &chat_body,
                    &route.upstream_model,
                    &route.upstream_endpoint_type,
                ) {
                    Ok(v) => v,
                    Err(err) => {
                        return Ok(json_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            &format!("failed to prepare OpenAI upstream request: {err}"),
                        ));
                    }
                };
                let upstream = match self
                    .send_openai_body(
                        &route,
                        &req.downstream_headers,
                        prepared.body,
                        &prepared.path,
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
                        return Ok(upstream_transport_error_response(&err.to_string()));
                    }
                };
                if !upstream.status().is_success() {
                    return Ok(upstream_error_response(upstream).await);
                }
                let upstream = if prepared.protocol == OpenAiTextProtocol::Responses {
                    if req.stream {
                        stream_responses_as_openai_chat(
                            upstream,
                            route.requested_model.clone(),
                            prepared.tool_name_reverse,
                        )
                    } else {
                        buffered_responses_as_openai_chat(
                            upstream,
                            route.requested_model.clone(),
                            prepared.tool_name_reverse,
                        )
                        .await
                    }
                } else {
                    upstream
                };
                if !upstream.status().is_success() {
                    return Ok(upstream);
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
        let oauth = has_explicit_header_override(&route.header_override, "authorization");
        let path = claude_upstream_path(path, oauth);
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
            );
        if !oauth {
            builder = builder.header("x-api-key", route.api_key.as_str());
        }

        if self.pass_anthropic_beta
            && let Some(beta) = downstream_headers.get("anthropic-beta")
        {
            builder = builder.header("anthropic-beta", beta);
        }
        if let Some(user_agent) = downstream_headers.get(header::USER_AGENT) {
            builder = builder.header(header::USER_AGENT, user_agent);
        }
        // Stainless / Claude CLI identity headers (allowlisted).
        for name in [
            "X-Stainless-Arch",
            "X-Stainless-Lang",
            "X-Stainless-Os",
            "X-Stainless-Package-Version",
            "X-Stainless-Retry-Count",
            "X-Stainless-Runtime",
            "X-Stainless-Runtime-Version",
            "X-Stainless-Timeout",
            "X-App",
            "Anthropic-Dangerous-Direct-Browser-Access",
        ] {
            if let Some(value) = downstream_headers.get(name) {
                builder = builder.header(name, value);
            }
        }

        let mut req = builder
            .body(HttpBody::fixed_body(Some(body)))
            .context("build Claude upstream request")?;
        apply_channel_header_override(req.headers_mut(), route, downstream_headers);
        let channel_identity =
            proxy_circuit_channel_identity(route.management_channel_id, &route.channel_id);
        self.send_upstream(&channel_identity, uri, req, &route.proxy)
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
        let codex_identity = uses_codex_identity(&route.upstream_endpoint_type);
        // Preserve client identity headers. new-api does not rewrite User-Agent by
        // default; affinity rules and Codex/Claude CLIs also rely on the original UA.
        if let Some(user_agent) = downstream_or_fallback_header(
            downstream_headers,
            header::USER_AGENT.as_str(),
            codex_identity.then_some(CODEX_DEFAULT_USER_AGENT),
        ) {
            builder = builder.header(header::USER_AGENT, user_agent);
        }
        if let Some(beta) = downstream_headers.get("OpenAI-Beta") {
            builder = builder.header("OpenAI-Beta", beta);
        }
        if let Some(originator) = downstream_or_fallback_header(
            downstream_headers,
            "Originator",
            codex_identity.then_some(CODEX_DEFAULT_ORIGINATOR),
        ) {
            builder = builder.header("Originator", originator);
        }
        // Common Codex / OpenAI client headers (allowlisted; credentials never forwarded).
        for name in [
            "Session_id",
            "X-Codex-Beta-Features",
            "X-Codex-Turn-Metadata",
            "OpenAI-Organization",
            "OpenAI-Project",
        ] {
            if let Some(value) = downstream_headers.get(name) {
                builder = builder.header(name, value);
            }
        }
        let mut req = builder
            .body(HttpBody::fixed_body(Some(body)))
            .context("build OpenAI upstream request")?;
        apply_channel_header_override(req.headers_mut(), route, downstream_headers);
        ensure_codex_session_header(req.headers_mut(), codex_identity);
        let channel_identity =
            proxy_circuit_channel_identity(route.management_channel_id, &route.channel_id);
        self.send_upstream(&channel_identity, uri, req, &route.proxy)
            .await
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
            .header(header::ACCEPT, "application/json, text/event-stream");
        if !has_explicit_header_override(&route.header_override, "authorization") {
            builder = builder.header("x-goog-api-key", route.api_key.as_str());
        }
        if let Some(accept) = downstream_headers.get(header::ACCEPT) {
            builder = builder.header(header::ACCEPT, accept);
        }
        if let Some(user_agent) = downstream_headers.get(header::USER_AGENT) {
            builder = builder.header(header::USER_AGENT, user_agent);
        }
        let mut req = builder
            .body(HttpBody::fixed_body(Some(body)))
            .context("build Gemini upstream request")?;
        apply_channel_header_override(req.headers_mut(), route, downstream_headers);
        let channel_identity =
            proxy_circuit_channel_identity(route.management_channel_id, &route.channel_id);
        self.send_upstream(&channel_identity, uri, req, &route.proxy)
            .await
    }

    async fn openai_passthrough(
        &self,
        route: RouteContext,
        prepared: PreparedOpenAiText,
        downstream_headers: &HeaderMap,
        target: OpenAiResponseTarget,
    ) -> Result<Response<GatewayBody>, Infallible> {
        match self
            .send_openai_body(&route, downstream_headers, prepared.body, &prepared.path)
            .await
        {
            Ok(resp) if !resp.status().is_success() => {
                Ok(upstream_to_response_with_usage(resp).await)
            }
            Ok(resp) => match (prepared.protocol, target) {
                (
                    OpenAiTextProtocol::Responses,
                    OpenAiResponseTarget::ChatCompletions {
                        stream: true,
                        requested_model,
                    },
                ) => Ok(stream_responses_as_openai_chat(
                    resp,
                    requested_model,
                    prepared.tool_name_reverse,
                )),
                (
                    OpenAiTextProtocol::Responses,
                    OpenAiResponseTarget::ChatCompletions {
                        stream: false,
                        requested_model,
                    },
                ) => Ok(buffered_responses_as_openai_chat(
                    resp,
                    requested_model,
                    prepared.tool_name_reverse,
                )
                .await),
                _ => Ok(upstream_to_response_with_usage(resp).await),
            },
            Err(err) => {
                error!(
                    ?err,
                    channel_id = %route.channel_id,
                    base_url = %route.base_url,
                    upstream_model = %route.upstream_model,
                    "OpenAI upstream request failed"
                );
                Ok(upstream_transport_error_response(&err.to_string()))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiTextProtocol {
    ChatCompletions,
    Responses,
}

struct PreparedOpenAiText {
    path: String,
    body: Bytes,
    protocol: OpenAiTextProtocol,
    tool_name_reverse: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiResponsesProfile {
    Generic,
    Codex,
}

enum OpenAiResponseTarget {
    Passthrough,
    ChatCompletions {
        stream: bool,
        requested_model: String,
    },
}

/// new-api-compatible upstream path selection for OpenAI text APIs.
/// Channel setting `upstream_endpoint_type`:
/// - empty/auto: keep client path (rewrite model only for chat)
/// - openai: force `/v1/chat/completions` (+ convert responses body → chat if needed)
/// - openai-response / openai-response-compact: force responses path (+ convert chat → responses)
/// - xai-response: force xAI-compatible `/responses` (+ convert chat → responses)
/// - codex: force ChatGPT Codex `/responses` (+ convert chat → responses)
fn prepare_openai_text_upstream(
    client_path: &str,
    raw_body: &[u8],
    upstream_model: &str,
    endpoint_type: &str,
) -> Result<PreparedOpenAiText> {
    let client_path = client_path.split('?').next().unwrap_or(client_path);
    let mode = normalize_upstream_endpoint_type(endpoint_type);
    let client_is_responses =
        client_path.ends_with("/responses") || client_path.ends_with("/responses/compact");
    let client_is_chat = client_path.ends_with("/chat/completions");

    match mode.as_str() {
        "openai" => {
            let body = if client_is_responses {
                convert_responses_body_to_chat(raw_body, upstream_model)?
            } else {
                rewrite_openai_chat_model_body(raw_body, upstream_model)?
            };
            Ok(PreparedOpenAiText {
                path: "/v1/chat/completions".to_string(),
                body,
                protocol: OpenAiTextProtocol::ChatCompletions,
                tool_name_reverse: std::collections::BTreeMap::new(),
            })
        }
        "codex" | "xai-response" => {
            let (body, tool_name_reverse) = if client_is_chat || !client_is_responses {
                convert_chat_body_to_responses(
                    raw_body,
                    upstream_model,
                    false,
                    if mode == "codex" {
                        OpenAiResponsesProfile::Codex
                    } else {
                        OpenAiResponsesProfile::Generic
                    },
                )?
            } else {
                (
                    rewrite_openai_responses_model_body(raw_body, upstream_model)?,
                    std::collections::BTreeMap::new(),
                )
            };
            Ok(PreparedOpenAiText {
                path: "/responses".to_string(),
                body,
                protocol: OpenAiTextProtocol::Responses,
                tool_name_reverse,
            })
        }
        "openai-response" => {
            let (body, tool_name_reverse) = if client_is_chat || !client_is_responses {
                convert_chat_body_to_responses(
                    raw_body,
                    upstream_model,
                    false,
                    OpenAiResponsesProfile::Generic,
                )?
            } else {
                (
                    rewrite_openai_responses_model_body(raw_body, upstream_model)?,
                    std::collections::BTreeMap::new(),
                )
            };
            Ok(PreparedOpenAiText {
                path: "/v1/responses".to_string(),
                body,
                protocol: OpenAiTextProtocol::Responses,
                tool_name_reverse,
            })
        }
        "openai-response-compact" => {
            let (body, tool_name_reverse) = if client_is_chat || !client_is_responses {
                convert_chat_body_to_responses(
                    raw_body,
                    upstream_model,
                    true,
                    OpenAiResponsesProfile::Generic,
                )?
            } else {
                (
                    rewrite_openai_responses_model_body(raw_body, upstream_model)?,
                    std::collections::BTreeMap::new(),
                )
            };
            Ok(PreparedOpenAiText {
                path: "/v1/responses/compact".to_string(),
                body,
                protocol: OpenAiTextProtocol::Responses,
                tool_name_reverse,
            })
        }
        _ => {
            // auto: preserve client path; still rewrite model id
            if client_is_chat {
                let body = rewrite_openai_chat_model_body(raw_body, upstream_model)?;
                Ok(PreparedOpenAiText {
                    path: client_path.to_string(),
                    body,
                    protocol: OpenAiTextProtocol::ChatCompletions,
                    tool_name_reverse: std::collections::BTreeMap::new(),
                })
            } else if client_is_responses {
                let body = rewrite_openai_responses_model_body(raw_body, upstream_model)?;
                Ok(PreparedOpenAiText {
                    path: client_path.to_string(),
                    body,
                    protocol: OpenAiTextProtocol::Responses,
                    tool_name_reverse: std::collections::BTreeMap::new(),
                })
            } else {
                // other openai paths (embeddings/completions): pass through, try model rewrite
                let body = rewrite_openai_any_model_body(raw_body, upstream_model)
                    .unwrap_or_else(|_| Bytes::copy_from_slice(raw_body));
                Ok(PreparedOpenAiText {
                    path: client_path.to_string(),
                    body,
                    protocol: OpenAiTextProtocol::ChatCompletions,
                    tool_name_reverse: std::collections::BTreeMap::new(),
                })
            }
        }
    }
}

fn normalize_upstream_endpoint_type(raw: &str) -> String {
    let v = raw.trim().to_ascii_lowercase();
    match v.as_str() {
        "" | "auto" => "auto".to_string(),
        "openai" | "chat" | "chat_completions" | "chat-completions" => "openai".to_string(),
        "openai-response" | "openai_response" | "responses" | "response" => {
            "openai-response".to_string()
        }
        "openai-response-compact" | "openai_response_compact" | "responses-compact" => {
            "openai-response-compact".to_string()
        }
        "xai-response" | "xai_response" => "xai-response".to_string(),
        "codex" | "chatgpt-codex" | "chatgpt_codex" => "codex".to_string(),
        other => other.to_string(),
    }
}

fn uses_codex_identity(endpoint_type: &str) -> bool {
    normalize_upstream_endpoint_type(endpoint_type) == "codex"
}

fn ensure_codex_session_header(headers: &mut HeaderMap, codex_identity: bool) {
    if !codex_identity
        || !headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|user_agent| user_agent.contains("Mac OS"))
        || ["Session_id", "session_id", "Session-Id"]
            .iter()
            .any(|name| headers.get(*name).is_some())
    {
        return;
    }
    if let Ok(session_id) = HeaderValue::from_str(&Uuid::new_v4().to_string()) {
        headers.insert("Session_id", session_id);
    }
}

fn rewrite_openai_responses_model_body(raw_body: &[u8], upstream_model: &str) -> Result<Bytes> {
    let mut value: JsonValue =
        serde_json::from_slice(raw_body).context("parse OpenAI responses request body")?;
    value["model"] = JsonValue::String(upstream_model.to_string());
    Ok(Bytes::from(
        serde_json::to_vec(&value).context("serialize OpenAI responses request body")?,
    ))
}

fn rewrite_openai_any_model_body(raw_body: &[u8], upstream_model: &str) -> Result<Bytes> {
    let mut value: JsonValue =
        serde_json::from_slice(raw_body).context("parse OpenAI request body")?;
    if value.get("model").is_some() {
        value["model"] = JsonValue::String(upstream_model.to_string());
    }
    Ok(Bytes::from(
        serde_json::to_vec(&value).context("serialize OpenAI request body")?,
    ))
}

/// Chat Completions → Responses conversion aligned with the pinned CLIProxyAPI Codex
/// translator for typed messages and function-call transcripts.
fn convert_chat_body_to_responses(
    raw_body: &[u8],
    upstream_model: &str,
    compact: bool,
    profile: OpenAiResponsesProfile,
) -> Result<(Bytes, std::collections::BTreeMap<String, String>)> {
    let chat: JsonValue =
        serde_json::from_slice(raw_body).context("parse chat request for responses convert")?;
    let tool_names = codex_tool_name_map(&chat, profile);
    let tool_name_reverse = tool_names
        .iter()
        .map(|(original, shortened)| (shortened.clone(), original.clone()))
        .collect();
    let mut out = serde_json::Map::new();
    out.insert(
        "model".into(),
        JsonValue::String(upstream_model.to_string()),
    );
    out.insert(
        "input".into(),
        chat_messages_to_responses_input(&chat, &tool_names),
    );
    match profile {
        OpenAiResponsesProfile::Codex => {
            out.insert("instructions".into(), JsonValue::String(String::new()));
            out.insert(
                "stream".into(),
                JsonValue::Bool(
                    chat.get("stream")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false),
                ),
            );
            out.insert(
                "reasoning".into(),
                serde_json::json!({
                    "effort": chat
                        .get("reasoning_effort")
                        .cloned()
                        .unwrap_or_else(|| JsonValue::String("medium".into())),
                    "summary": "auto",
                }),
            );
            out.insert("parallel_tool_calls".into(), JsonValue::Bool(true));
            out.insert(
                "include".into(),
                serde_json::json!(["reasoning.encrypted_content"]),
            );
        }
        OpenAiResponsesProfile::Generic => {
            if let Some(stream) = chat.get("stream") {
                out.insert("stream".into(), stream.clone());
            }
            if let Some(temp) = chat.get("temperature") {
                out.insert("temperature".into(), temp.clone());
            }
            if let Some(top_p) = chat.get("top_p") {
                out.insert("top_p".into(), top_p.clone());
            }
            if let Some(max_out) = chat
                .get("max_completion_tokens")
                .or_else(|| chat.get("max_tokens"))
            {
                out.insert("max_output_tokens".into(), max_out.clone());
            }
            if let Some(parallel) = chat.get("parallel_tool_calls") {
                out.insert("parallel_tool_calls".into(), parallel.clone());
            }
            if let Some(effort) = chat.get("reasoning_effort") {
                out.insert("reasoning".into(), serde_json::json!({"effort": effort}));
            }
        }
    }
    if let Some(tools) = chat.get("tools").and_then(JsonValue::as_array) {
        out.insert(
            "tools".into(),
            JsonValue::Array(
                tools
                    .iter()
                    .map(|tool| chat_tool_to_responses_tool(tool, &tool_names))
                    .collect(),
            ),
        );
    }
    if let Some(tool_choice) = chat.get("tool_choice") {
        out.insert(
            "tool_choice".into(),
            chat_tool_choice_to_responses(tool_choice, &tool_names),
        );
    }
    if let Some(response_format) = chat.get("response_format") {
        out.insert(
            "text".into(),
            serde_json::json!({"format": chat_response_format_to_responses(response_format)}),
        );
    }
    if let Some(user) = chat.get("user") {
        out.insert("user".into(), user.clone());
    }
    out.insert("store".into(), JsonValue::Bool(false));
    if compact {
        // compact API is non-stream in new-api tests
        out.remove("stream");
    }
    Ok((
        Bytes::from(
            serde_json::to_vec(&JsonValue::Object(out)).context("serialize responses body")?,
        ),
        tool_name_reverse,
    ))
}

fn chat_messages_to_responses_input(
    chat: &JsonValue,
    tool_names: &std::collections::BTreeMap<String, String>,
) -> JsonValue {
    let mut input = Vec::new();
    for message in chat
        .get("messages")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        let role = message
            .get("role")
            .and_then(JsonValue::as_str)
            .unwrap_or("user");
        if role == "tool" {
            input.push(serde_json::json!({
                "type": "function_call_output",
                "call_id": message
                    .get("tool_call_id")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default(),
                "output": chat_tool_output_to_responses(message.get("content")),
            }));
            continue;
        }

        let content = chat_message_content_to_responses(role, message.get("content"));
        if role != "assistant" || !content.is_empty() {
            input.push(serde_json::json!({
                "type": "message",
                "role": if role == "system" { "developer" } else { role },
                "content": content,
            }));
        }
        if role == "assistant" {
            for call in message
                .get("tool_calls")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                if call.get("type").and_then(JsonValue::as_str) != Some("function") {
                    continue;
                }
                input.push(serde_json::json!({
                    "type": "function_call",
                    "call_id": call.get("id").and_then(JsonValue::as_str).unwrap_or_default(),
                    "name": mapped_tool_name(
                        call.get("function")
                            .and_then(|function| function.get("name"))
                            .and_then(JsonValue::as_str)
                            .unwrap_or_default(),
                        tool_names,
                    ),
                    "arguments": call
                        .get("function")
                        .and_then(|function| function.get("arguments"))
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default(),
                }));
            }
        }
    }
    JsonValue::Array(input)
}

fn chat_message_content_to_responses(role: &str, content: Option<&JsonValue>) -> Vec<JsonValue> {
    let part_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    match content {
        Some(JsonValue::String(text)) if !text.is_empty() => {
            vec![serde_json::json!({"type": part_type, "text": text})]
        }
        Some(JsonValue::Array(parts)) => parts
            .iter()
            .filter_map(|part| chat_content_part_to_responses(role, part_type, part))
            .collect(),
        _ => Vec::new(),
    }
}

fn chat_content_part_to_responses(
    role: &str,
    text_part_type: &str,
    part: &JsonValue,
) -> Option<JsonValue> {
    match part.get("type").and_then(JsonValue::as_str) {
        Some("text") => Some(serde_json::json!({
            "type": text_part_type,
            "text": part.get("text").and_then(JsonValue::as_str).unwrap_or_default(),
        })),
        Some("image_url") if role == "user" => {
            let image = part.get("image_url")?;
            let mut out = serde_json::json!({"type": "input_image"});
            if let Some(url) = image.get("url").and_then(JsonValue::as_str) {
                out["image_url"] = JsonValue::String(url.to_string());
            }
            if let Some(file_id) = image.get("file_id").and_then(JsonValue::as_str) {
                out["file_id"] = JsonValue::String(file_id.to_string());
            }
            if let Some(detail) = image.get("detail").and_then(JsonValue::as_str) {
                out["detail"] = JsonValue::String(detail.to_string());
            }
            Some(out)
        }
        Some("file") if role == "user" => {
            let file = part.get("file")?;
            let mut out = serde_json::json!({"type": "input_file"});
            for key in ["file_id", "file_data", "file_url", "filename"] {
                if let Some(value) = file.get(key).and_then(JsonValue::as_str) {
                    out[key] = JsonValue::String(value.to_string());
                }
            }
            Some(out)
        }
        Some("input_audio") if role == "user" => {
            let audio = part.get("input_audio")?;
            let data = audio.get("data").and_then(JsonValue::as_str)?;
            let mut out = serde_json::json!({"type": "input_audio", "data": data});
            if let Some(format) = audio.get("format").and_then(JsonValue::as_str) {
                out["format"] = JsonValue::String(format.to_string());
            }
            Some(out)
        }
        Some("input_text" | "output_text" | "input_image" | "input_file") => Some(part.clone()),
        _ => None,
    }
}

fn chat_tool_output_to_responses(content: Option<&JsonValue>) -> JsonValue {
    match content {
        Some(JsonValue::String(text)) => JsonValue::String(text.clone()),
        Some(JsonValue::Array(parts)) => JsonValue::Array(
            parts
                .iter()
                .map(chat_tool_output_part_to_responses)
                .collect(),
        ),
        Some(value) => {
            JsonValue::String(serde_json::to_string(value).unwrap_or_else(|_| value.to_string()))
        }
        None => JsonValue::String(String::new()),
    }
}

fn chat_tool_output_part_to_responses(part: &JsonValue) -> JsonValue {
    match part.get("type").and_then(JsonValue::as_str) {
        Some("text") => serde_json::json!({
            "type": "input_text",
            "text": part.get("text").and_then(JsonValue::as_str).unwrap_or_default(),
        }),
        Some("image_url") => {
            let image = part.get("image_url").unwrap_or(&JsonValue::Null);
            let mut out = serde_json::json!({"type": "input_image"});
            let mut valid = false;
            if let Some(url) = image.get("url").and_then(JsonValue::as_str) {
                out["image_url"] = JsonValue::String(url.to_string());
                valid = true;
            }
            if let Some(file_id) = image.get("file_id").and_then(JsonValue::as_str) {
                out["file_id"] = JsonValue::String(file_id.to_string());
                valid = true;
            }
            if let Some(detail) = image.get("detail").and_then(JsonValue::as_str) {
                out["detail"] = JsonValue::String(detail.to_string());
            }
            if valid {
                out
            } else {
                chat_tool_output_fallback(part)
            }
        }
        Some("file") => {
            let file = part.get("file").unwrap_or(&JsonValue::Null);
            let mut out = serde_json::json!({"type": "input_file"});
            let mut valid = false;
            for key in ["file_id", "file_data", "file_url"] {
                if let Some(value) = file.get(key).and_then(JsonValue::as_str) {
                    out[key] = JsonValue::String(value.to_string());
                    valid = true;
                }
            }
            if let Some(filename) = file.get("filename").and_then(JsonValue::as_str) {
                out["filename"] = JsonValue::String(filename.to_string());
            }
            if valid {
                out
            } else {
                chat_tool_output_fallback(part)
            }
        }
        _ => chat_tool_output_fallback(part),
    }
}

fn chat_tool_output_fallback(part: &JsonValue) -> JsonValue {
    serde_json::json!({
        "type": "input_text",
        "text": serde_json::to_string(part).unwrap_or_else(|_| part.to_string()),
    })
}

fn chat_tool_to_responses_tool(
    tool: &JsonValue,
    tool_names: &std::collections::BTreeMap<String, String>,
) -> JsonValue {
    if tool.get("type").and_then(JsonValue::as_str) != Some("function") {
        return tool.clone();
    }
    let function = tool.get("function").unwrap_or(&JsonValue::Null);
    let mut out = serde_json::json!({"type": "function"});
    for key in ["name", "description", "parameters", "strict"] {
        if let Some(value) = function.get(key) {
            out[key] = if key == "name" {
                JsonValue::String(mapped_tool_name(
                    value.as_str().unwrap_or_default(),
                    tool_names,
                ))
            } else {
                value.clone()
            };
        }
    }
    out
}

fn chat_tool_choice_to_responses(
    tool_choice: &JsonValue,
    tool_names: &std::collections::BTreeMap<String, String>,
) -> JsonValue {
    if tool_choice.get("type").and_then(JsonValue::as_str) != Some("function") {
        return tool_choice.clone();
    }
    serde_json::json!({
        "type": "function",
        "name": mapped_tool_name(
            tool_choice
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(JsonValue::as_str)
                .unwrap_or_default(),
            tool_names,
        ),
    })
}

fn codex_tool_name_map(
    chat: &JsonValue,
    profile: OpenAiResponsesProfile,
) -> std::collections::BTreeMap<String, String> {
    if profile != OpenAiResponsesProfile::Codex {
        return std::collections::BTreeMap::new();
    }
    let mut mapped = std::collections::BTreeMap::new();
    let mut used = std::collections::BTreeSet::new();
    let mut names: Vec<&str> = chat
        .get("tools")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("function"))
        .filter_map(|function| function.get("name"))
        .filter_map(JsonValue::as_str)
        .collect();
    names.extend(
        chat.get("messages")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .flat_map(|message| {
                message
                    .get("tool_calls")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter_map(|call| call.get("function"))
            .filter_map(|function| function.get("name"))
            .filter_map(JsonValue::as_str),
    );
    if let Some(choice_name) = chat
        .get("tool_choice")
        .and_then(|choice| choice.get("function"))
        .and_then(|function| function.get("name"))
        .and_then(JsonValue::as_str)
    {
        names.push(choice_name);
    }
    for name in names {
        if mapped.contains_key(name) {
            continue;
        }
        let base = shorten_codex_tool_name(name);
        let mut candidate = base.clone();
        let mut suffix = 1usize;
        while used.contains(&candidate) {
            let tail = format!("~{suffix}");
            candidate = format!("{}{}", truncate_utf8_bytes(&base, 64 - tail.len()), tail);
            suffix += 1;
        }
        used.insert(candidate.clone());
        mapped.insert(name.to_string(), candidate);
    }
    mapped
}

fn mapped_tool_name(name: &str, tool_names: &std::collections::BTreeMap<String, String>) -> String {
    tool_names
        .get(name)
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

fn shorten_codex_tool_name(name: &str) -> String {
    if name.len() <= 64 {
        return name.to_string();
    }
    if let Some(last) = name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.rsplit("__").next())
    {
        return format!("mcp__{}", truncate_utf8_bytes(last, 59));
    }
    truncate_utf8_bytes(name, 64).to_string()
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn chat_response_format_to_responses(response_format: &JsonValue) -> JsonValue {
    match response_format.get("type").and_then(JsonValue::as_str) {
        Some("json_schema") => {
            let schema = response_format
                .get("json_schema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let mut out = serde_json::json!({"type": "json_schema"});
            if let Some(object) = schema.as_object() {
                for key in ["name", "strict", "schema"] {
                    if let Some(value) = object.get(key) {
                        out[key] = value.clone();
                    }
                }
            }
            out
        }
        Some(kind) => serde_json::json!({"type": kind}),
        None => response_format.clone(),
    }
}

/// Minimal responses → chat conversion for forced openai upstream.
fn convert_responses_body_to_chat(raw_body: &[u8], upstream_model: &str) -> Result<Bytes> {
    let resp: JsonValue =
        serde_json::from_slice(raw_body).context("parse responses request for chat convert")?;
    let input = resp
        .get("input")
        .cloned()
        .unwrap_or(JsonValue::Array(vec![]));
    // If input is a string, wrap as user message
    let messages = match input {
        JsonValue::String(s) => serde_json::json!([{"role": "user", "content": s}]),
        JsonValue::Array(arr) => JsonValue::Array(arr),
        other => serde_json::json!([{"role": "user", "content": other}]),
    };
    let mut out = serde_json::Map::new();
    out.insert(
        "model".into(),
        JsonValue::String(upstream_model.to_string()),
    );
    out.insert("messages".into(), messages);
    if let Some(stream) = resp.get("stream") {
        out.insert("stream".into(), stream.clone());
    }
    if let Some(temp) = resp.get("temperature") {
        out.insert("temperature".into(), temp.clone());
    }
    if let Some(top_p) = resp.get("top_p") {
        out.insert("top_p".into(), top_p.clone());
    }
    if let Some(max_out) = resp.get("max_output_tokens") {
        out.insert("max_tokens".into(), max_out.clone());
    }
    if let Some(tools) = resp.get("tools") {
        out.insert("tools".into(), tools.clone());
    }
    if let Some(tool_choice) = resp.get("tool_choice") {
        out.insert("tool_choice".into(), tool_choice.clone());
    }
    if let Some(user) = resp.get("user") {
        out.insert("user".into(), user.clone());
    }
    if out.get("stream").and_then(|v| v.as_bool()) == Some(true) {
        out.insert(
            "stream_options".into(),
            serde_json::json!({"include_usage": true}),
        );
    }
    Ok(Bytes::from(
        serde_json::to_vec(&JsonValue::Object(out)).context("serialize chat body")?,
    ))
}

fn rewrite_openai_chat_model_body(raw_body: &[u8], upstream_model: &str) -> Result<Bytes> {
    let mut value: JsonValue =
        serde_json::from_slice(raw_body).context("parse OpenAI chat request body")?;
    value["model"] = JsonValue::String(upstream_model.to_string());
    ensure_openai_stream_include_usage(&mut value);
    Ok(Bytes::from(
        serde_json::to_vec(&value).context("serialize OpenAI chat request body")?,
    ))
}

fn downstream_or_fallback_header(
    downstream_headers: &HeaderMap,
    name: &str,
    fallback: Option<&'static str>,
) -> Option<HeaderValue> {
    downstream_headers
        .get(name)
        .cloned()
        .or_else(|| fallback.map(HeaderValue::from_static))
}

fn claude_upstream_path(path: &str, oauth: bool) -> &str {
    if oauth && path == "/v1/messages" {
        "/v1/messages?beta=true"
    } else {
        path
    }
}

/// Apply channel `header_override` last so it wins over default/auth headers.
/// Supports `{api_key}` and `{client_header:Name}` (new-api compatible).
fn has_explicit_header_override(
    overrides: &std::collections::BTreeMap<String, String>,
    name: &str,
) -> bool {
    overrides
        .keys()
        .any(|header| header.eq_ignore_ascii_case(name))
}

fn apply_channel_header_override(
    headers: &mut HeaderMap,
    route: &RouteContext,
    downstream_headers: &HeaderMap,
) {
    if route.header_override.is_empty() {
        return;
    }
    for (name, template) in &route.header_override {
        let Some(value) =
            resolve_header_override_value(template, &route.api_key, downstream_headers)
        else {
            continue;
        };
        let Ok(header_name) = HeaderName::try_from(name.as_str()) else {
            continue;
        };
        let Ok(header_value) = HeaderValue::try_from(value.as_str()) else {
            continue;
        };
        headers.insert(header_name, header_value);
    }
}

fn resolve_header_override_value(
    template: &str,
    api_key: &str,
    downstream_headers: &HeaderMap,
) -> Option<String> {
    let trimmed = template.trim();
    if trimmed.is_empty() {
        return None;
    }
    const CLIENT_HEADER_PREFIX: &str = "{client_header:";
    if let Some(rest) = trimmed.strip_prefix(CLIENT_HEADER_PREFIX) {
        let name = rest.strip_suffix('}')?.trim();
        if name.is_empty() {
            return None;
        }
        return downstream_headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
    }
    let value = trimmed.replace("{api_key}", api_key);
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_channel::oneshot;
    use monoio::{
        io::{AsyncReadRent, AsyncWriteRentExt},
        net::TcpListener,
    };

    #[test]
    fn hyper_request_framing_is_rebuilt_from_the_actual_body() {
        let mut fixed = Request::builder()
            .header(header::CONTENT_LENGTH, "999")
            .header(header::TRANSFER_ENCODING, "chunked")
            .body(HttpBody::fixed_body(Some(Bytes::from_static(b"hello"))))
            .expect("fixed request");
        prepare_hyper_request_headers(&mut fixed);
        assert_eq!(fixed.headers().get(header::CONTENT_LENGTH).unwrap(), "5");
        assert!(!fixed.headers().contains_key(header::TRANSFER_ENCODING));

        let (payload, _sender) = stream_payload_pair::<Bytes, HttpError>();
        let mut streaming = Request::builder()
            .header(header::CONTENT_LENGTH, "5")
            .header(header::TRANSFER_ENCODING, "identity")
            .body(HttpBody::H1(Payload::Stream(payload)))
            .expect("streaming request");
        prepare_hyper_request_headers(&mut streaming);
        assert!(!streaming.headers().contains_key(header::CONTENT_LENGTH));
        assert!(!streaming.headers().contains_key(header::TRANSFER_ENCODING));
    }

    #[monoio::test_all(enable_timer = true)]
    async fn direct_http_returns_sse_before_eof_and_reuses_connection() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow SSE upstream");
        let address = listener.local_addr().expect("slow SSE upstream address");
        let (release_eof_tx, release_eof_rx) = oneshot::channel::<()>();

        let server = monoio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept upstream connection");
            let first = read_http_head(&mut stream).await;
            assert!(first.starts_with(b"GET /events HTTP/1.1\r\n"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\nd\r\ndata: first\n\n\r\n"
                        .to_vec(),
                )
                .await
                .0
                .expect("write first SSE event");

            release_eof_rx.await.expect("release first response EOF");
            stream
                .write_all(b"0\r\n\r\n".to_vec())
                .await
                .0
                .expect("write first response EOF");

            let second =
                monoio::time::timeout(Duration::from_millis(500), read_http_head(&mut stream))
                    .await
                    .expect("second request must reuse the first TCP connection");
            assert!(second.starts_with(b"GET /second HTTP/1.1\r\n"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok"
                        .to_vec(),
                )
                .await
                .0
                .expect("write second response");
        });

        let relay = RelayService {
            claude_version: Arc::from("2023-06-01"),
            pass_anthropic_beta: true,
            gemini_api_version: Arc::from("v1beta"),
            connect_timeout: Some(Duration::from_millis(500)),
            read_timeout: Some(Duration::from_millis(500)),
            dns: LocalDnsResolver::default(),
            http: Rc::new(HyperH1Connector::new(PollIo(TcpConnector::default()))),
            https: HttpsUpstream::default(),
            proxy_transport: ProxyTransportService::new(
                Some(Duration::from_millis(500)),
                Some(Duration::from_millis(500)),
                crate::upstream_proxy::ProxyCircuitPolicy::new(3, Duration::from_millis(100)),
            ),
        };
        let base_uri: Uri = format!("http://{address}/events")
            .parse()
            .expect("parse test URI");
        let first_request = Request::builder()
            .method(Method::GET)
            .uri("/events")
            .header(header::HOST, address.to_string())
            .body(HttpBody::default())
            .expect("build first request");

        let first_response = monoio::time::timeout(
            Duration::from_millis(500),
            relay.send_upstream("test-channel", base_uri, first_request, &ProxyRoute::Direct),
        )
        .await
        .expect("response head must arrive before upstream EOF")
        .expect("first upstream response");
        assert_eq!(first_response.status(), StatusCode::OK);
        let mut first_body = first_response.into_body();
        let first_chunk = monoio::time::timeout(Duration::from_millis(500), first_body.next_data())
            .await
            .expect("first SSE chunk must arrive before EOF")
            .expect("first SSE body chunk")
            .expect("valid first SSE body chunk");
        assert_eq!(first_chunk, Bytes::from_static(b"data: first\n\n"));

        release_eof_tx.send(()).expect("release first response EOF");
        assert!(
            monoio::time::timeout(Duration::from_millis(500), first_body.next_data())
                .await
                .expect("first response EOF timeout")
                .is_none()
        );

        let second_uri: Uri = format!("http://{address}/second")
            .parse()
            .expect("parse second test URI");
        let second_request = Request::builder()
            .method(Method::GET)
            .uri("/second")
            .header(header::HOST, address.to_string())
            .body(HttpBody::default())
            .expect("build second request");
        let second_response = monoio::time::timeout(
            Duration::from_millis(500),
            relay.send_upstream(
                "test-channel",
                second_uri,
                second_request,
                &ProxyRoute::Direct,
            ),
        )
        .await
        .expect("second response must use the pooled connection")
        .expect("second upstream response");
        let mut second_body = second_response.into_body();
        assert_eq!(
            second_body
                .next_data()
                .await
                .expect("second response body")
                .expect("valid second response body"),
            Bytes::from_static(b"ok")
        );
        assert!(second_body.next_data().await.is_none());
        server.await;
    }

    #[monoio::test_all(enable_timer = true)]
    async fn concurrent_sse_requests_do_not_lease_the_same_http1_connection() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind concurrent SSE upstream");
        let address = listener
            .local_addr()
            .expect("concurrent SSE upstream address");

        let server = monoio::spawn(async move {
            let (mut first_stream, _) = listener.accept().await.expect("accept first connection");
            let first = read_http_head(&mut first_stream).await;
            assert!(first.starts_with(b"GET /first HTTP/1.1\r\n"));
            first_stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\nd\r\ndata: first\n\n\r\n"
                        .to_vec(),
                )
                .await
                .0
                .expect("write first in-flight SSE event");

            // The first response deliberately remains open. A correct H1 pool
            // therefore establishes a second TCP connection instead of waiting
            // for the first sender to become ready.
            let (mut second_stream, _) =
                monoio::time::timeout(Duration::from_millis(500), listener.accept())
                    .await
                    .expect("concurrent request must open another TCP connection")
                    .expect("accept second connection");
            let second = read_http_head(&mut second_stream).await;
            assert!(second.starts_with(b"GET /second HTTP/1.1\r\n"));
            second_stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok"
                        .to_vec(),
                )
                .await
                .0
                .expect("write concurrent response");
            first_stream
                .write_all(b"0\r\n\r\n".to_vec())
                .await
                .0
                .expect("finish first response");
        });

        let relay = RelayService {
            claude_version: Arc::from("2023-06-01"),
            pass_anthropic_beta: true,
            gemini_api_version: Arc::from("v1beta"),
            connect_timeout: Some(Duration::from_millis(500)),
            read_timeout: Some(Duration::from_millis(500)),
            dns: LocalDnsResolver::default(),
            http: Rc::new(HyperH1Connector::new(PollIo(TcpConnector::default()))),
            https: HttpsUpstream::default(),
            proxy_transport: ProxyTransportService::new(
                Some(Duration::from_millis(500)),
                Some(Duration::from_millis(500)),
                crate::upstream_proxy::ProxyCircuitPolicy::new(3, Duration::from_millis(100)),
            ),
        };

        let first_uri: Uri = format!("http://{address}/first")
            .parse()
            .expect("parse first URI");
        let first_request = Request::builder()
            .method(Method::GET)
            .uri("/first")
            .header(header::HOST, address.to_string())
            .body(HttpBody::default())
            .expect("build first request");
        let first_response = relay
            .send_upstream(
                "test-channel",
                first_uri,
                first_request,
                &ProxyRoute::Direct,
            )
            .await
            .expect("first upstream response");
        let mut first_body = first_response.into_body();
        assert_eq!(
            first_body
                .next_data()
                .await
                .expect("first SSE body chunk")
                .expect("valid first SSE body chunk"),
            Bytes::from_static(b"data: first\n\n")
        );

        let second_uri: Uri = format!("http://{address}/second")
            .parse()
            .expect("parse second URI");
        let second_request = Request::builder()
            .method(Method::GET)
            .uri("/second")
            .header(header::HOST, address.to_string())
            .body(HttpBody::default())
            .expect("build second request");
        let second_response = monoio::time::timeout(
            Duration::from_millis(500),
            relay.send_upstream(
                "test-channel",
                second_uri,
                second_request,
                &ProxyRoute::Direct,
            ),
        )
        .await
        .expect("concurrent response must not wait for first SSE EOF")
        .expect("second upstream response");
        let mut second_body = second_response.into_body();
        assert_eq!(
            second_body
                .next_data()
                .await
                .expect("second response body")
                .expect("valid second response body"),
            Bytes::from_static(b"ok")
        );
        assert!(second_body.next_data().await.is_none());
        assert!(first_body.next_data().await.is_none());
        server.await;
    }

    async fn read_http_head(stream: &mut TcpStream) -> Vec<u8> {
        let mut head = Vec::new();
        loop {
            let (result, buffer) = stream.read(vec![0u8; 1024]).await;
            let read = result.expect("read HTTP request head");
            assert!(read > 0, "unexpected EOF while reading HTTP request head");
            head.extend_from_slice(&buffer[..read]);
            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                return head;
            }
            assert!(head.len() <= 16 * 1024, "HTTP request head too large");
        }
    }

    #[test]
    fn authorization_override_suppresses_provider_api_key_header() {
        let overrides = std::collections::BTreeMap::from([(
            "aUtHoRiZaTiOn".to_string(),
            "Bearer {api_key}".to_string(),
        )]);

        assert!(has_explicit_header_override(&overrides, "authorization"));
        assert!(!has_explicit_header_override(&overrides, "x-api-key"));
        assert_eq!(
            claude_upstream_path("/v1/messages", true),
            "/v1/messages?beta=true"
        );
        assert_eq!(claude_upstream_path("/v1/messages", false), "/v1/messages");
    }

    #[test]
    fn codex_identity_fallback_preserves_downstream_headers() {
        let mut downstream = HeaderMap::new();
        downstream.insert(
            header::USER_AGENT,
            HeaderValue::from_static("codex_vscode/1.2.3"),
        );
        downstream.insert("Originator", HeaderValue::from_static("codex_vscode"));

        assert_eq!(
            downstream_or_fallback_header(
                &downstream,
                header::USER_AGENT.as_str(),
                Some(CODEX_DEFAULT_USER_AGENT),
            )
            .as_ref()
            .and_then(|value| value.to_str().ok()),
            Some("codex_vscode/1.2.3")
        );
        assert_eq!(
            downstream_or_fallback_header(
                &downstream,
                "Originator",
                Some(CODEX_DEFAULT_ORIGINATOR),
            )
            .as_ref()
            .and_then(|value| value.to_str().ok()),
            Some("codex_vscode")
        );
        assert_eq!(
            downstream_or_fallback_header(
                &HeaderMap::new(),
                "Originator",
                Some(CODEX_DEFAULT_ORIGINATOR),
            ),
            Some(HeaderValue::from_static("codex-tui"))
        );
    }

    #[test]
    fn codex_endpoint_targets_chatgpt_backend_responses() {
        assert_eq!(normalize_upstream_endpoint_type("codex"), "codex");
        assert_eq!(normalize_upstream_endpoint_type("chatgpt_codex"), "codex");
        let body = serde_json::json!({
            "model": "requested-model",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        });
        let raw = serde_json::to_vec(&body).expect("request should serialize");

        let prepared = prepare_openai_text_upstream("/v1/chat/completions", &raw, "gpt-5", "codex")
            .expect("Codex request should prepare");
        let uri = upstream_uri("https://chatgpt.com/backend-api/codex", &prepared.path)
            .expect("Codex endpoint should be valid");
        let rewritten: JsonValue =
            serde_json::from_slice(&prepared.body).expect("rewritten request should be JSON");

        assert_eq!(
            uri.to_string(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(prepared.protocol, OpenAiTextProtocol::Responses);
        assert_eq!(rewritten["model"], "gpt-5");
        assert_eq!(rewritten["input"][0]["type"], "message");
        assert_eq!(rewritten["input"][0]["role"], "user");
        assert_eq!(rewritten["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(rewritten["input"][0]["content"][0]["text"], "hello");
        assert_eq!(rewritten["stream"], true);
    }

    #[test]
    fn xai_response_uses_responses_path_without_codex_identity() {
        assert_eq!(
            normalize_upstream_endpoint_type("xai_response"),
            "xai-response"
        );
        assert!(!uses_codex_identity("xai-response"));
        let raw = serde_json::to_vec(&serde_json::json!({
            "model": "requested-model",
            "messages": [{"role": "user", "content": "hello"}],
        }))
        .expect("request should serialize");

        let prepared =
            prepare_openai_text_upstream("/v1/chat/completions", &raw, "grok-4", "xai-response")
                .expect("xAI request should prepare");
        let versioned = upstream_uri("https://api.x.ai/v1", &prepared.path)
            .expect("versioned xAI endpoint should be valid");
        let custom = upstream_uri("https://xai.example.test/custom", &prepared.path)
            .expect("custom xAI endpoint should be valid");
        let rewritten: JsonValue =
            serde_json::from_slice(&prepared.body).expect("rewritten request should be JSON");

        assert_eq!(prepared.path, "/responses");
        assert_eq!(prepared.protocol, OpenAiTextProtocol::Responses);
        assert_eq!(versioned.to_string(), "https://api.x.ai/v1/responses");
        assert_eq!(
            custom.to_string(),
            "https://xai.example.test/custom/responses"
        );
        assert_eq!(rewritten["model"], "grok-4");
        assert_eq!(rewritten["input"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn codex_maps_chat_tool_transcript_to_typed_responses_items() {
        let long_name = format!("mcp__weather__{}", "forecast".repeat(12));
        let raw = serde_json::to_vec(&serde_json::json!({
            "model": "requested-model",
            "messages": [
                {"role": "system", "content": "Be precise."},
                {"role": "user", "content": "Weather?"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": long_name, "arguments": "{\"city\":\"Paris\"}"}
                    }]
                },
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": long_name,
                    "description": "Get weather",
                    "parameters": {"type": "object"}
                }
            }],
            "temperature": 0.3,
            "max_tokens": 42,
            "stream": true
        }))
        .expect("request should serialize");

        let prepared = prepare_openai_text_upstream("/v1/chat/completions", &raw, "gpt-5", "codex")
            .expect("Codex request should prepare");
        let value: JsonValue =
            serde_json::from_slice(&prepared.body).expect("request should remain JSON");
        let input = value["input"].as_array().expect("input should be an array");

        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "sunny");
        let shortened = value["tools"][0]["name"]
            .as_str()
            .expect("tool name should be present");
        assert!(shortened.len() <= 64);
        assert_eq!(input[2]["name"], shortened);
        assert_eq!(prepared.tool_name_reverse.get(shortened), Some(&long_name));
        assert_eq!(value["instructions"], "");
        assert_eq!(value["reasoning"]["effort"], "medium");
        assert_eq!(value["reasoning"]["summary"], "auto");
        assert_eq!(value["parallel_tool_calls"], true);
        assert_eq!(value["include"][0], "reasoning.encrypted_content");
        assert!(value.get("temperature").is_none());
        assert!(value.get("max_output_tokens").is_none());
    }

    #[test]
    fn xai_response_keeps_generic_responses_sampling_fields() {
        let raw = serde_json::to_vec(&serde_json::json!({
            "model": "requested-model",
            "messages": [{"role": "user", "content": "hello"}],
            "temperature": 0.4,
            "top_p": 0.8,
            "max_completion_tokens": 123
        }))
        .expect("request should serialize");

        let prepared =
            prepare_openai_text_upstream("/v1/chat/completions", &raw, "grok-4.5", "xai-response")
                .expect("xAI request should prepare");
        let value: JsonValue =
            serde_json::from_slice(&prepared.body).expect("request should remain JSON");

        assert_eq!(value["temperature"], 0.4);
        assert_eq!(value["top_p"], 0.8);
        assert_eq!(value["max_output_tokens"], 123);
        assert!(value.get("instructions").is_none());
    }

    #[test]
    fn native_responses_request_stays_responses_protocol() {
        let raw = br#"{"model":"requested-model","input":"hello","stream":true}"#;
        let prepared = prepare_openai_text_upstream("/v1/responses", raw, "gpt-5", "codex")
            .expect("native Responses request should prepare");
        let value: JsonValue =
            serde_json::from_slice(&prepared.body).expect("request should remain JSON");

        assert_eq!(prepared.path, "/responses");
        assert_eq!(prepared.protocol, OpenAiTextProtocol::Responses);
        assert_eq!(value["input"], "hello");
        assert_eq!(value["stream"], true);
    }

    #[test]
    fn codex_mac_user_agent_gets_session_id_without_overriding_existing_one() {
        let mut generated = HeaderMap::new();
        generated.insert(
            header::USER_AGENT,
            HeaderValue::from_static(CODEX_DEFAULT_USER_AGENT),
        );
        ensure_codex_session_header(&mut generated, true);
        let session_id = generated
            .get("Session_id")
            .and_then(|value| value.to_str().ok())
            .expect("Mac Codex identity should get Session_id");
        assert!(Uuid::parse_str(session_id).is_ok());

        let mut existing = generated.clone();
        existing.insert("Session_id", HeaderValue::from_static("downstream-session"));
        ensure_codex_session_header(&mut existing, true);
        assert_eq!(
            existing.get("Session_id"),
            Some(&HeaderValue::from_static("downstream-session"))
        );

        let mut non_codex = HeaderMap::new();
        non_codex.insert(
            header::USER_AGENT,
            HeaderValue::from_static(CODEX_DEFAULT_USER_AGENT),
        );
        ensure_codex_session_header(&mut non_codex, false);
        assert!(!non_codex.contains_key("Session_id"));
    }
}
