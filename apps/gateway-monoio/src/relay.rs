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
    claude_version:      Arc<str>,
    pass_anthropic_beta: bool,
    gemini_api_version:  Arc<str>,
    connect_timeout:     Option<Duration>,
    read_timeout:        Option<Duration>,
    dns:                 LocalDnsResolver,
    http:                Rc<StreamingHttpUpstream>,
    https:               HttpsUpstream,
    proxy_transport:     ProxyTransportService,
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
    pub(crate) request:            openai::ChatCompletionRequest,
    /// Original downstream body; used for redacted structured logs and
    /// OpenAI passthrough so unknown fields (e.g. reasoning_effort) survive.
    pub(crate) raw_body:           Bytes,
    pub(crate) downstream_headers: HeaderMap,
    pub(crate) cx:                 CX,
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
                    stream:          req.request.is_stream(),
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
    pub(crate) value:              JsonValue,
    pub(crate) downstream_headers: HeaderMap,
    pub(crate) cx:                 CX,
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
    pub(crate) kind:               OpenAiImageRouteKind,
    pub(crate) stream:             bool,
    pub(crate) payload:            OpenAiImagePayload,
    pub(crate) body:               Bytes,
    pub(crate) downstream_headers: HeaderMap,
    pub(crate) path:               String,
    pub(crate) cx:                 CX,
}

pub(crate) struct OpenAiPassthroughRelayRequest<CX> {
    pub(crate) path:               String,
    pub(crate) body:               Bytes,
    pub(crate) downstream_headers: HeaderMap,
    pub(crate) cx:                 CX,
}

pub(crate) struct GeminiNativeRelayRequest<CX> {
    pub(crate) path:               String,
    pub(crate) stream:             bool,
    pub(crate) body:               Bytes,
    pub(crate) downstream_headers: HeaderMap,
    pub(crate) cx:                 CX,
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
        let client_path = req.path.split('?').next().unwrap_or(req.path.as_str());
        let client_is_responses =
            client_path.ends_with("/responses") || client_path.ends_with("/responses/compact");
        let stream = serde_json::from_slice::<JsonValue>(&req.body)
            .ok()
            .and_then(|body| body.get("stream").and_then(JsonValue::as_bool))
            .unwrap_or(false);
        debug_relay(
            &req.cx,
            stream || is_stream_like(&req.downstream_headers),
            Some(req.body.as_ref()),
        );
        if route.provider == Provider::Claude && client_is_responses {
            if client_path.ends_with("/responses/compact") {
                return Ok(json_error(
                    StatusCode::NOT_IMPLEMENTED,
                    "not_supported",
                    "OpenAI Responses compact is not supported by Anthropic upstreams; use an \
                     OpenAI channel for /v1/responses/compact",
                ));
            }
            let include_encrypted_reasoning = serde_json::from_slice::<JsonValue>(&req.body)
                .ok()
                .and_then(|body| body.get("include").and_then(JsonValue::as_array).cloned())
                .is_some_and(|include| {
                    include
                        .iter()
                        .any(|value| value.as_str() == Some("reasoning.encrypted_content"))
                });
            let tool_mapping = match responses_tool_mapping(&req.body) {
                Ok(mapping) => mapping,
                Err(err) => {
                    return Ok(json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("failed to inspect Responses tools: {err}"),
                    ));
                }
            };
            let cache_mode = prompt_cache_mode_for_base_url(&route.base_url);
            let claude_request = match openai_responses_to_claude_messages_with_tools_and_cache_mode(
                &req.body,
                route.upstream_model.clone(),
                &tool_mapping,
                cache_mode,
            ) {
                Ok(request) => request,
                Err(err) => {
                    return Ok(json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("failed to convert Responses request to Claude: {err}"),
                    ));
                }
            };
            let upstream = match self
                .send_claude_json(
                    &route,
                    &req.downstream_headers,
                    &claude_request,
                    "/v1/messages",
                )
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    error!(
                        ?err,
                        channel_id = %route.channel_id,
                        base_url = %route.base_url,
                        upstream_model = %route.upstream_model,
                        "Responses to Claude upstream request failed"
                    );
                    return Ok(upstream_transport_error_response(&err.to_string()));
                }
            };
            if !upstream.status().is_success() {
                return Ok(upstream_error_response(upstream).await);
            }
            return if stream {
                Ok(stream_claude_as_responses(
                    upstream,
                    route.requested_model,
                    include_encrypted_reasoning,
                    tool_mapping,
                ))
            } else {
                Ok(buffered_claude_as_responses(
                    upstream,
                    route.requested_model,
                    include_encrypted_reasoning,
                    tool_mapping,
                )
                .await)
            };
        }
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
        let target = if client_is_responses {
            OpenAiResponseTarget::Responses {
                stream,
                requested_model: route.requested_model.clone(),
            }
        } else {
            OpenAiResponseTarget::Passthrough
        };
        self.openai_passthrough(route, prepared, &req.downstream_headers, target)
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

fn prompt_cache_mode_for_base_url(base_url: &str) -> PromptCacheMode {
    let host = base_url
        .parse::<Uri>()
        .ok()
        .and_then(|uri| uri.host().map(str::to_ascii_lowercase));
    if host.as_deref() == Some("api.anthropic.com") {
        PromptCacheMode::Automatic
    } else {
        PromptCacheMode::Explicit
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
                (
                    OpenAiTextProtocol::ChatCompletions,
                    OpenAiResponseTarget::Responses {
                        stream: true,
                        requested_model,
                    },
                ) => Ok(stream_openai_chat_as_responses(
                    resp,
                    requested_model,
                    prepared.responses_tool_mapping,
                )),
                (
                    OpenAiTextProtocol::ChatCompletions,
                    OpenAiResponseTarget::Responses {
                        stream: false,
                        requested_model,
                    },
                ) => Ok(buffered_openai_chat_as_responses(
                    resp,
                    requested_model,
                    prepared.responses_tool_mapping,
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
    path:                   String,
    body:                   Bytes,
    protocol:               OpenAiTextProtocol,
    tool_name_reverse:      std::collections::BTreeMap<String, String>,
    responses_tool_mapping: ResponsesToolMapping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiResponsesProfile {
    Generic,
    Codex,
}

enum OpenAiResponseTarget {
    Passthrough,
    ChatCompletions {
        stream:          bool,
        requested_model: String,
    },
    Responses {
        stream:          bool,
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
            let (body, responses_tool_mapping) = if client_is_responses {
                convert_responses_body_to_chat(raw_body, upstream_model)?
            } else {
                (
                    rewrite_openai_chat_model_body(raw_body, upstream_model)?,
                    ResponsesToolMapping::default(),
                )
            };
            Ok(PreparedOpenAiText {
                path: "/v1/chat/completions".to_string(),
                body,
                protocol: OpenAiTextProtocol::ChatCompletions,
                tool_name_reverse: std::collections::BTreeMap::new(),
                responses_tool_mapping,
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
                responses_tool_mapping: ResponsesToolMapping::default(),
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
                responses_tool_mapping: ResponsesToolMapping::default(),
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
                responses_tool_mapping: ResponsesToolMapping::default(),
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
                    responses_tool_mapping: ResponsesToolMapping::default(),
                })
            } else if client_is_responses {
                let body = rewrite_openai_responses_model_body(raw_body, upstream_model)?;
                Ok(PreparedOpenAiText {
                    path: client_path.to_string(),
                    body,
                    protocol: OpenAiTextProtocol::Responses,
                    tool_name_reverse: std::collections::BTreeMap::new(),
                    responses_tool_mapping: ResponsesToolMapping::default(),
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
                    responses_tool_mapping: ResponsesToolMapping::default(),
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
            // Preserve provider-specific reasoning signatures across the
            // Chat -> Responses -> Chat round trip when the upstream exposes
            // encrypted reasoning content.
            out.insert(
                "include".into(),
                serde_json::json!(["reasoning.encrypted_content"]),
            );
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
        if role == "assistant" {
            let reasoning = message
                .get("reasoning_content")
                .or_else(|| message.get("reasoning"))
                .and_then(JsonValue::as_str)
                .unwrap_or_default();
            let signature = message
                .get("reasoning_signature")
                .or_else(|| message.get("reasoning_content_signature"))
                .or_else(|| message.get("reasoningSignature"))
                .or_else(|| message.get("encrypted_content"))
                .and_then(JsonValue::as_str)
                .filter(|value| !value.is_empty());
            if !reasoning.is_empty() || signature.is_some() {
                let mut item = serde_json::json!({
                    "type": "reasoning",
                    "summary": if reasoning.is_empty() {
                        JsonValue::Array(Vec::new())
                    } else {
                        serde_json::json!([{"type":"summary_text","text":reasoning}])
                    },
                });
                if let Some(signature) = signature {
                    item["encrypted_content"] = JsonValue::String(signature.to_string());
                }
                input.push(item);
            }
        }
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
                let mut item = serde_json::json!({
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
                });
                if let Some(signature) = call
                    .get("thought_signature")
                    .or_else(|| call.get("thoughtSignature"))
                    .and_then(JsonValue::as_str)
                    .filter(|value| !value.is_empty())
                {
                    item["thought_signature"] = JsonValue::String(signature.to_string());
                }
                input.push(item);
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

/// Responses → Chat Completions conversion for channels forced to `/v1/chat/completions`.
fn convert_responses_body_to_chat(
    raw_body: &[u8],
    upstream_model: &str,
) -> Result<(Bytes, ResponsesToolMapping)> {
    let resp: JsonValue =
        serde_json::from_slice(raw_body).context("parse responses request for chat convert")?;
    let tool_mapping = responses_tool_mapping(raw_body)
        .context("collect Responses client tool mapping for chat convert")?;
    if resp
        .get("previous_response_id")
        .is_some_and(|value| !value.is_null())
    {
        anyhow::bail!(
            "previous_response_id cannot be represented by a stateless Chat Completions upstream"
        );
    }
    let mut messages = Vec::new();
    if let Some(instructions) = resp.get("instructions") {
        let content = responses_content_to_chat(instructions, false);
        if !chat_content_is_empty(&content) {
            messages.push(serde_json::json!({"role": "system", "content": content}));
        }
    }
    let mut additional_tools = Vec::new();
    match resp.get("input") {
        Some(JsonValue::String(text)) => {
            messages.push(serde_json::json!({"role": "user", "content": text}));
        }
        Some(JsonValue::Array(items)) => {
            for item in items {
                if item.get("type").and_then(JsonValue::as_str) == Some("additional_tools") {
                    if let Some(tools) = item.get("tools").and_then(JsonValue::as_array) {
                        additional_tools.extend(tools.iter().cloned());
                    }
                    continue;
                }
                responses_input_item_to_chat(item, &mut messages)?;
            }
        }
        Some(JsonValue::Null) | None => {}
        Some(other) => {
            messages.push(serde_json::json!({
                "role": "user",
                "content": serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
            }));
        }
    }
    messages = normalize_responses_chat_messages(messages);
    if messages.is_empty() {
        anyhow::bail!("Responses input produced no Chat Completions messages");
    }
    let mut out = serde_json::Map::new();
    out.insert(
        "model".into(),
        JsonValue::String(upstream_model.to_string()),
    );
    out.insert("messages".into(), JsonValue::Array(messages));
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
    if let Some(tools) = resp.get("tools").and_then(JsonValue::as_array) {
        additional_tools.extend(tools.iter().cloned());
    }
    if !additional_tools.is_empty() {
        let chat_tools: Vec<JsonValue> = additional_tools
            .iter()
            .filter_map(responses_tool_to_chat)
            .collect();
        if !chat_tools.is_empty() {
            out.insert("tools".into(), JsonValue::Array(chat_tools));
        }
    }
    if let Some(tool_choice) = resp.get("tool_choice")
        && let Some(tool_choice) = responses_tool_choice_to_chat(tool_choice)
    {
        out.insert("tool_choice".into(), tool_choice);
    }
    if let Some(user) = resp.get("user") {
        out.insert("user".into(), user.clone());
    }
    if let Some(parallel) = resp.get("parallel_tool_calls") {
        out.insert("parallel_tool_calls".into(), parallel.clone());
    }
    if let Some(effort) = resp
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
    {
        out.insert("reasoning_effort".into(), effort.clone());
    }
    if let Some(format) = resp.get("text").and_then(|text| text.get("format")) {
        out.insert(
            "response_format".into(),
            responses_text_format_to_chat(format),
        );
    }
    if out.get("stream").and_then(|v| v.as_bool()) == Some(true) {
        out.insert(
            "stream_options".into(),
            serde_json::json!({"include_usage": true}),
        );
    }
    Ok((
        Bytes::from(serde_json::to_vec(&JsonValue::Object(out)).context("serialize chat body")?),
        tool_mapping,
    ))
}

fn responses_input_item_to_chat(item: &JsonValue, messages: &mut Vec<JsonValue>) -> Result<()> {
    let item_type = item.get("type").and_then(JsonValue::as_str);
    match item_type {
        Some("message") | None if item.get("role").is_some() => {
            let role = item
                .get("role")
                .and_then(JsonValue::as_str)
                .unwrap_or("user");
            let content = responses_content_to_chat(
                item.get("content").unwrap_or(&JsonValue::Null),
                role == "assistant",
            );
            let mut message = serde_json::json!({"role": role, "content": content});
            if let Some(name) = item.get("name") {
                message["name"] = name.clone();
            }
            messages.push(message);
        }
        Some("function_call") | Some("tool_search_call") => {
            let is_tool_search = item_type == Some("tool_search_call");
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(JsonValue::as_str)
                .unwrap_or_default();
            let mut tool_call = serde_json::json!({
                "id": call_id,
                "type": "function",
                "function": {
                    "name": if is_tool_search {
                        "tool_search"
                    } else {
                        item.get("name").and_then(JsonValue::as_str).unwrap_or_default()
                    },
                    "arguments": responses_arguments_to_chat_string(
                        item.get("arguments").or_else(|| item.get("input"))
                    ),
                },
            });
            if let Some(signature) = item
                .get("thought_signature")
                .or_else(|| item.get("thoughtSignature"))
            {
                tool_call["thoughtSignature"] = signature.clone();
            }
            merge_responses_assistant(messages, None, None, Some(tool_call));
        }
        Some("custom_tool_call") => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(JsonValue::as_str)
                .unwrap_or_default();
            let input = item
                .get("input")
                .and_then(JsonValue::as_str)
                .unwrap_or_default();
            let mut tool_call = serde_json::json!({
                "id": call_id,
                "type": "function",
                "function": {
                    "name": item.get("name").and_then(JsonValue::as_str).unwrap_or_default(),
                    "arguments": serde_json::to_string(&serde_json::json!({"input": input}))
                        .unwrap_or_else(|_| "{\"input\":\"\"}".to_string()),
                },
            });
            if let Some(signature) = item
                .get("thought_signature")
                .or_else(|| item.get("thoughtSignature"))
            {
                tool_call["thoughtSignature"] = signature.clone();
            }
            merge_responses_assistant(messages, None, None, Some(tool_call));
        }
        Some("function_call_output")
        | Some("custom_tool_call_output")
        | Some("tool_search_output") => {
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default(),
                "content": responses_content_to_chat(
                    item.get("output").unwrap_or(&JsonValue::Null),
                    false,
                ),
            }));
        }
        Some("reasoning") => {
            let reasoning = responses_reasoning_text(item);
            let signature = item
                .get("encrypted_content")
                .or_else(|| item.get("signature"))
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            if !reasoning.is_empty() || signature.is_some() {
                merge_responses_assistant(messages, Some(reasoning), signature, None);
            }
        }
        Some("item_reference") => {
            anyhow::bail!("Responses item_reference cannot be resolved for Chat Completions")
        }
        Some(
            "web_search_call" | "file_search_call" | "computer_call" | "message_output"
            | "additional_tools",
        ) => {}
        Some(kind) => anyhow::bail!("unsupported Responses input item type for Chat: {kind}"),
        None => {
            messages.push(serde_json::json!({
                "role": "user",
                "content": serde_json::to_string(item).unwrap_or_else(|_| item.to_string()),
            }));
        }
    }
    Ok(())
}

fn normalize_responses_chat_messages(messages: Vec<JsonValue>) -> Vec<JsonValue> {
    let messages = merge_adjacent_responses_assistant_messages(messages);
    let mut replies = std::collections::BTreeMap::new();
    for message in &messages {
        if message.get("role").and_then(JsonValue::as_str) == Some("tool")
            && let Some(call_id) = message.get("tool_call_id").and_then(JsonValue::as_str)
            && !call_id.is_empty()
        {
            replies.insert(call_id.to_string(), message.clone());
        }
    }

    let mut emitted = std::collections::BTreeSet::new();
    let mut normalized = Vec::with_capacity(messages.len());
    for mut message in messages {
        let role = message
            .get("role")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        if role == "tool" {
            // Tool replies are re-emitted immediately after their answered
            // assistant tool_calls message; standalone/orphan replies are invalid
            // for Chat Completions and must not reach the upstream.
            continue;
        }
        let Some(calls) = message.get("tool_calls").and_then(JsonValue::as_array) else {
            normalized.push(message);
            continue;
        };
        let kept = calls
            .iter()
            .filter(|call| {
                let id = call
                    .get("id")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default();
                !id.is_empty() && replies.contains_key(id) && !emitted.contains(id)
            })
            .cloned()
            .collect::<Vec<_>>();
        if kept.is_empty() {
            message.as_object_mut().map(|object| {
                object.remove("tool_calls");
            });
            if chat_message_has_content_or_reasoning(&message) {
                normalized.push(message);
            }
            continue;
        }
        if let Some(object) = message.as_object_mut() {
            object.insert("tool_calls".to_string(), JsonValue::Array(kept.clone()));
        }
        normalized.push(message);
        for call in kept {
            let id = call
                .get("id")
                .and_then(JsonValue::as_str)
                .unwrap_or_default();
            if let Some(reply) = replies.get(id)
                && emitted.insert(id.to_string())
            {
                normalized.push(reply.clone());
            }
        }
    }
    normalized
}

fn merge_adjacent_responses_assistant_messages(messages: Vec<JsonValue>) -> Vec<JsonValue> {
    let mut merged: Vec<JsonValue> = Vec::with_capacity(messages.len());
    for message in messages {
        let is_assistant = message.get("role").and_then(JsonValue::as_str) == Some("assistant");
        if is_assistant
            && let Some(previous) = merged.last_mut()
            && previous.get("role").and_then(JsonValue::as_str) == Some("assistant")
        {
            merge_responses_chat_assistant(previous, &message);
        } else {
            merged.push(message);
        }
    }
    merged
}

fn merge_responses_chat_assistant(target: &mut JsonValue, source: &JsonValue) {
    let target_object = target
        .as_object_mut()
        .expect("assistant message is an object");
    if let Some(content) = source.get("content")
        && !chat_content_is_empty(content)
    {
        let existing = target_object
            .get("content")
            .cloned()
            .unwrap_or(JsonValue::Null);
        target_object.insert(
            "content".to_string(),
            merge_chat_content_values(existing, content.clone()),
        );
    }
    for key in ["reasoning_content", "reasoning_signature"] {
        if let Some(value) = source.get(key).and_then(JsonValue::as_str)
            && !value.is_empty()
        {
            let previous = target_object
                .get(key)
                .and_then(JsonValue::as_str)
                .unwrap_or_default();
            target_object.insert(
                key.to_string(),
                JsonValue::String(format!("{previous}{value}")),
            );
        }
    }
    if let Some(calls) = source.get("tool_calls").and_then(JsonValue::as_array) {
        let entry = target_object
            .entry("tool_calls".to_string())
            .or_insert_with(|| JsonValue::Array(Vec::new()));
        if let JsonValue::Array(existing) = entry {
            existing.extend(calls.iter().cloned());
        }
    }
}

fn merge_chat_content_values(left: JsonValue, right: JsonValue) -> JsonValue {
    match (left, right) {
        (JsonValue::Null, value) => value,
        (JsonValue::String(left), JsonValue::String(right)) => {
            JsonValue::String(format!("{left}{right}"))
        }
        (JsonValue::Array(mut left), JsonValue::Array(right)) => {
            left.extend(right);
            JsonValue::Array(left)
        }
        (JsonValue::Array(mut left), value) => {
            left.push(value);
            JsonValue::Array(left)
        }
        (left, JsonValue::Array(mut right)) => {
            right.insert(0, left);
            JsonValue::Array(right)
        }
        (left, right) => JsonValue::Array(vec![left, right]),
    }
}

fn chat_message_has_content_or_reasoning(message: &JsonValue) -> bool {
    message
        .get("content")
        .is_some_and(|content| !chat_content_is_empty(content))
        || message
            .get("reasoning_content")
            .and_then(JsonValue::as_str)
            .is_some_and(|reasoning| !reasoning.is_empty())
}

fn merge_responses_assistant(
    messages: &mut Vec<JsonValue>,
    reasoning: Option<String>,
    reasoning_signature: Option<String>,
    tool_call: Option<JsonValue>,
) {
    let can_merge = messages
        .last()
        .and_then(|message| message.get("role"))
        .and_then(JsonValue::as_str)
        == Some("assistant");
    if !can_merge {
        messages.push(serde_json::json!({"role": "assistant", "content": JsonValue::Null}));
    }
    let message = messages
        .last_mut()
        .expect("assistant message inserted when none was present");
    if let Some(reasoning) = reasoning {
        let existing = message
            .get("reasoning_content")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        message["reasoning_content"] = JsonValue::String(format!("{existing}{reasoning}"));
    }
    if let Some(signature) = reasoning_signature {
        message["reasoning_signature"] = JsonValue::String(signature);
    }
    if let Some(tool_call) = tool_call {
        match message.get_mut("tool_calls") {
            Some(JsonValue::Array(calls)) => calls.push(tool_call),
            _ => message["tool_calls"] = JsonValue::Array(vec![tool_call]),
        }
    }
}

fn responses_content_to_chat(value: &JsonValue, assistant: bool) -> JsonValue {
    match value {
        JsonValue::String(text) => JsonValue::String(text.clone()),
        JsonValue::Array(parts) => {
            let converted: Vec<JsonValue> = parts
                .iter()
                .filter_map(|part| responses_content_part_to_chat(part, assistant))
                .collect();
            if converted.len() == 1
                && converted[0].get("type").and_then(JsonValue::as_str) == Some("text")
            {
                converted[0].get("text").cloned().unwrap_or(JsonValue::Null)
            } else {
                JsonValue::Array(converted)
            }
        }
        JsonValue::Null => JsonValue::Null,
        other => {
            JsonValue::String(serde_json::to_string(other).unwrap_or_else(|_| other.to_string()))
        }
    }
}

fn responses_content_part_to_chat(part: &JsonValue, assistant: bool) -> Option<JsonValue> {
    match part.get("type").and_then(JsonValue::as_str) {
        Some("input_text" | "output_text" | "text" | "summary_text") => Some(serde_json::json!({
            "type": "text",
            "text": part.get("text").and_then(JsonValue::as_str).unwrap_or_default(),
        })),
        Some("refusal") => Some(serde_json::json!({
            "type": "text",
            "text": part
                .get("refusal")
                .or_else(|| part.get("text"))
                .and_then(JsonValue::as_str)
                .unwrap_or_default(),
        })),
        Some("input_image") if !assistant => {
            let mut image = serde_json::Map::new();
            if let Some(url) = part.get("image_url").or_else(|| part.get("url")) {
                image.insert("url".into(), url.clone());
            }
            if let Some(file_id) = part.get("file_id") {
                image.insert("file_id".into(), file_id.clone());
            }
            if let Some(detail) = part.get("detail") {
                image.insert("detail".into(), detail.clone());
            }
            (!image.is_empty()).then(
                || serde_json::json!({"type": "image_url", "image_url": JsonValue::Object(image)}),
            )
        }
        Some("input_file") if !assistant => {
            let mut file = serde_json::Map::new();
            for key in ["file_id", "file_data", "file_url", "filename"] {
                if let Some(value) = part.get(key) {
                    file.insert(key.into(), value.clone());
                }
            }
            (!file.is_empty())
                .then(|| serde_json::json!({"type": "file", "file": JsonValue::Object(file)}))
        }
        Some("input_audio") if !assistant => Some(serde_json::json!({
            "type": "input_audio",
            "input_audio": {
                "data": part.get("data").and_then(JsonValue::as_str).unwrap_or_default(),
                "format": part.get("format").and_then(JsonValue::as_str).unwrap_or("wav"),
            },
        })),
        _ => None,
    }
}

fn responses_reasoning_text(item: &JsonValue) -> String {
    let mut out = String::new();
    for part in item
        .get("summary")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(text) = part.get("text").and_then(JsonValue::as_str) {
            out.push_str(text);
        }
    }
    if out.is_empty()
        && let Some(text) = item.get("content").and_then(JsonValue::as_str)
    {
        out.push_str(text);
    }
    out
}

fn chat_content_is_empty(content: &JsonValue) -> bool {
    match content {
        JsonValue::Null => true,
        JsonValue::String(text) => text.is_empty(),
        JsonValue::Array(parts) => parts.is_empty(),
        _ => false,
    }
}

fn responses_tool_to_chat(tool: &JsonValue) -> Option<JsonValue> {
    let tool_type = tool.get("type").and_then(JsonValue::as_str)?;
    if tool_type == "tool_search" {
        return Some(serde_json::json!({
            "type":"function",
            "function":{
                "name":"tool_search",
                "description":"Search and load tools available to the current task.",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "query":{"type":"string"},
                        "limit":{"type":"integer"}
                    },
                    "required":["query"]
                }
            }
        }));
    }
    if !matches!(tool_type, "function" | "custom") {
        return None;
    }
    let mut function = serde_json::Map::new();
    for key in ["name", "description", "parameters", "strict"] {
        if let Some(value) = tool.get(key) {
            function.insert(key.into(), value.clone());
        }
    }
    if tool_type == "custom" {
        function.insert(
            "parameters".into(),
            serde_json::json!({
                "type":"object",
                "properties":{"input":{"type":"string"}},
                "required":["input"]
            }),
        );
        function.remove("strict");
    }
    Some(serde_json::json!({
        "type": "function",
        "function": JsonValue::Object(function),
    }))
}

fn responses_tool_choice_to_chat(tool_choice: &JsonValue) -> Option<JsonValue> {
    let Some(kind) = tool_choice.get("type").and_then(JsonValue::as_str) else {
        return Some(tool_choice.clone());
    };
    match kind {
        "function" | "custom" => Some(serde_json::json!({
            "type": "function",
            "function": {
                "name": tool_choice.get("name").and_then(JsonValue::as_str).unwrap_or_default(),
            },
        })),
        "tool_search" => Some(serde_json::json!({
            "type":"function",
            "function":{"name":"tool_search"}
        })),
        // Server-side tools are not exposed by a Chat Completions upstream;
        // forwarding a choice that points at a dropped tool causes a 400.
        "web_search" | "file_search" | "computer" => None,
        _ => Some(tool_choice.clone()),
    }
}

fn responses_arguments_to_chat_string(value: Option<&JsonValue>) -> String {
    match value {
        Some(JsonValue::String(value)) => value.clone(),
        Some(JsonValue::Null) | None => "{}".to_string(),
        Some(value) => serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn responses_text_format_to_chat(format: &JsonValue) -> JsonValue {
    match format.get("type").and_then(JsonValue::as_str) {
        Some("json_schema") => serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": format.get("name").cloned().unwrap_or_else(|| JsonValue::String("response".into())),
                "strict": format.get("strict").cloned().unwrap_or(JsonValue::Bool(false)),
                "schema": format.get("schema").cloned().unwrap_or_else(|| serde_json::json!({})),
            },
        }),
        Some(kind) => serde_json::json!({"type": kind}),
        None => format.clone(),
    }
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
    fn selects_automatic_cache_only_for_native_anthropic_api() {
        assert_eq!(
            prompt_cache_mode_for_base_url("https://api.anthropic.com"),
            PromptCacheMode::Automatic
        );
        assert_eq!(
            prompt_cache_mode_for_base_url("https://bedrock-runtime.us-east-1.amazonaws.com"),
            PromptCacheMode::Explicit
        );
        assert_eq!(
            prompt_cache_mode_for_base_url("https://anthropic.example.com"),
            PromptCacheMode::Explicit
        );
    }

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
            claude_version:      Arc::from("2023-06-01"),
            pass_anthropic_beta: true,
            gemini_api_version:  Arc::from("v1beta"),
            connect_timeout:     Some(Duration::from_millis(500)),
            read_timeout:        Some(Duration::from_millis(500)),
            dns:                 LocalDnsResolver::default(),
            http:                Rc::new(HyperH1Connector::new(PollIo(TcpConnector::default()))),
            https:               HttpsUpstream::default(),
            proxy_transport:     ProxyTransportService::new(
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
            claude_version:      Arc::from("2023-06-01"),
            pass_anthropic_beta: true,
            gemini_api_version:  Arc::from("v1beta"),
            connect_timeout:     Some(Duration::from_millis(500)),
            read_timeout:        Some(Duration::from_millis(500)),
            dns:                 LocalDnsResolver::default(),
            http:                Rc::new(HyperH1Connector::new(PollIo(TcpConnector::default()))),
            https:               HttpsUpstream::default(),
            proxy_transport:     ProxyTransportService::new(
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
    fn forced_chat_converts_typed_responses_history_without_losing_reasoning_or_tools() {
        let raw = serde_json::to_vec(&serde_json::json!({
            "model": "requested-model",
            "instructions": "Be exact.",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "inspect"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AA==", "detail": "high"}
                ]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "need a tool"}], "encrypted_content": "reasoning-signature"},
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":1}", "thought_signature": "gemini-signature"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
            ],
            "tools": [{"type": "function", "name": "lookup", "description": "Lookup", "parameters": {"type": "object"}, "strict": true}],
            "tool_choice": {"type": "function", "name": "lookup"},
            "reasoning": {"effort": "high"},
            "text": {"format": {"type": "json_schema", "name": "result", "strict": true, "schema": {"type": "object"}}},
            "max_output_tokens": 128,
            "stream": true
        }))
        .expect("Responses request should serialize");
        let prepared =
            prepare_openai_text_upstream("/v1/responses", &raw, "gpt-upstream", "openai")
                .expect("forced Chat request should prepare");
        let body: JsonValue =
            serde_json::from_slice(&prepared.body).expect("prepared body should be JSON");
        assert_eq!(prepared.protocol, OpenAiTextProtocol::ChatCompletions);
        assert_eq!(body["model"], "gpt-upstream");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"][0]["text"], "inspect");
        assert_eq!(
            body["messages"][1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AA=="
        );
        assert_eq!(body["messages"][2]["reasoning_content"], "need a tool");
        assert_eq!(
            body["messages"][2]["reasoning_signature"],
            "reasoning-signature"
        );
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["thoughtSignature"],
            "gemini-signature"
        );
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["messages"][3]["tool_call_id"], "call_1");
        assert_eq!(body["tools"][0]["function"]["name"], "lookup");
        assert_eq!(body["tool_choice"]["function"]["name"], "lookup");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["max_tokens"], 128);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn forced_chat_rejects_unresolvable_previous_response_id() {
        let raw = serde_json::to_vec(&serde_json::json!({
            "model": "requested-model",
            "previous_response_id": "resp_previous",
            "input": "continue"
        }))
        .expect("Responses request should serialize");
        let err = prepare_openai_text_upstream("/v1/responses", &raw, "gpt-upstream", "openai")
            .err()
            .expect("stateful reference must be rejected");
        assert!(err.to_string().contains("previous_response_id"));
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

    #[test]
    fn forced_chat_preserves_reasoning_with_parallel_tools_and_next_turn_reasoning() {
        let raw = serde_json::to_vec(&serde_json::json!({
            "model": "requested-model",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "first thought"}], "encrypted_content": "sig-1"},
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_2", "name": "fetch", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "a"},
                {"type": "function_call_output", "call_id": "call_2", "output": "b"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "second thought"}], "encrypted_content": "sig-2"}
            ],
            "tools": [
                {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
                {"type": "function", "name": "fetch", "parameters": {"type": "object"}}
            ]
        }))
        .unwrap();
        let prepared =
            prepare_openai_text_upstream("/v1/responses", &raw, "gpt-upstream", "openai")
                .expect("forced Chat request should prepare");
        let body: JsonValue = serde_json::from_slice(&prepared.body).unwrap();
        let messages = body["messages"].as_array().unwrap();
        let reasoning_1 = messages
            .iter()
            .find(|m| m["reasoning_content"] == "first thought")
            .expect("first reasoning should survive");
        assert_eq!(reasoning_1["reasoning_signature"], "sig-1");
        let assistant_with_tools = messages
            .iter()
            .find(|m| {
                m.get("tool_calls")
                    .is_some_and(|c| c.as_array().unwrap().len() == 2)
            })
            .expect("parallel tool calls should be grouped");
        assert_eq!(
            assistant_with_tools["tool_calls"][0]["function"]["name"],
            "lookup"
        );
        assert_eq!(
            assistant_with_tools["tool_calls"][1]["function"]["name"],
            "fetch"
        );
        let reasoning_2 = messages
            .iter()
            .find(|m| m["reasoning_content"] == "second thought")
            .expect("second-turn reasoning should survive");
        assert_eq!(reasoning_2["reasoning_signature"], "sig-2");
    }

    // ---- mock Anthropic upstream integration tests for the
    // /v1/responses -> /v1/messages -> /v1/responses direct bridge ----

    fn test_relay_service() -> RelayService {
        RelayService {
            claude_version:      Arc::from("2023-06-01"),
            pass_anthropic_beta: true,
            gemini_api_version:  Arc::from("v1beta"),
            connect_timeout:     Some(Duration::from_secs(2)),
            read_timeout:        Some(Duration::from_secs(2)),
            dns:                 LocalDnsResolver::default(),
            http:                Rc::new(HyperH1Connector::new(PollIo(TcpConnector::default()))),
            https:               HttpsUpstream::default(),
            proxy_transport:     ProxyTransportService::new(
                Some(Duration::from_secs(2)),
                Some(Duration::from_secs(2)),
                crate::upstream_proxy::ProxyCircuitPolicy::new(3, Duration::from_millis(100)),
            ),
        }
    }

    fn test_route_context(base_url: String) -> RouteContext {
        RouteContext {
            channel_id: "test-channel".to_string(),
            management_channel_id: Some(42),
            provider: Provider::Claude,
            base_url,
            api_key: "test-anthropic-key".to_string(),
            api_key_fingerprint: "sha256:test-fingerprint".to_string(),
            api_key_index: Some(0),
            using_group: "default".to_string(),
            requested_model: "gpt-5".to_string(),
            upstream_model: "claude-sonnet-4".to_string(),
            proxy: ProxyRoute::Direct,
            header_override: std::collections::BTreeMap::new(),
            upstream_endpoint_type: "auto".to_string(),
        }
    }

    fn test_full_context(
        route: RouteContext,
    ) -> impl ParamRef<RouteContext>
    + ParamRef<RequestAuth>
    + ParamRef<RequestId>
    + ParamRef<PeerAddr>
    + Clone {
        RequestContext::new()
            .param_set(RequestId("test-request".to_string()))
            .param_set(PeerAddr("127.0.0.1:1".parse::<SocketAddr>().unwrap()))
            .param_set(DownstreamProtocol::OpenAiRaw)
            .param_set(RequestAuth {
                user_id:  "user-1".to_string(),
                token_id: "token-1".to_string(),
            })
            .param_set(route)
    }

    /// Read a full HTTP/1.1 request: head up to `\r\n\r\n` then a
    /// Content-Length-bounded body. Returns (request_line, body).
    ///
    /// Unlike `read_http_head`, this preserves body bytes that arrive in the
    /// same TCP read as the head terminator (which `read_http_head` would
    /// discard).
    async fn read_http_request_with_body(stream: &mut monoio::net::TcpStream) -> (String, Vec<u8>) {
        let mut buf = Vec::new();
        let head_end;
        loop {
            let (result, read_buf) = stream.read(vec![0u8; 8192]).await;
            let read = result.expect("read HTTP request head");
            assert!(read > 0, "unexpected EOF while reading HTTP request head");
            buf.extend_from_slice(&read_buf[..read]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                head_end = pos + 4;
                break;
            }
            assert!(buf.len() <= 64 * 1024, "HTTP request head too large");
        }
        let head_str = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let content_length = head_str
            .lines()
            .find_map(|line| {
                let lower = line.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        let mut body = buf[head_end..].to_vec();
        while body.len() < content_length {
            let (result, read_buf) = stream.read(vec![0u8; 8192]).await;
            let read = result.expect("read HTTP request body");
            assert!(read > 0, "unexpected EOF while reading request body");
            body.extend_from_slice(&read_buf[..read]);
        }
        body.truncate(content_length);
        let request_line = head_str.lines().next().unwrap_or_default().to_string();
        (request_line, body)
    }

    async fn collect_response_body(response: Response<GatewayBody>) -> Bytes {
        let mut body = response.into_body();
        let mut collected = Vec::new();
        while let Some(chunk) = MonoioBody::next_data(&mut body).await {
            match chunk {
                Ok(bytes) => collected.extend_from_slice(&bytes),
                Err(_) => break,
            }
        }
        Bytes::from(collected)
    }

    /// Parse an SSE byte stream into (event_type, data_json) pairs.
    fn parse_responses_sse(payload: &[u8]) -> Vec<(String, JsonValue)> {
        let text = String::from_utf8_lossy(payload);
        let mut events = Vec::new();
        for frame in text.split("\n\n") {
            if frame.trim().is_empty() {
                continue;
            }
            let mut event_type = String::new();
            let mut data = String::new();
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event_type = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(rest.trim());
                }
            }
            if !data.is_empty() {
                let json: JsonValue = serde_json::from_str(&data)
                    .unwrap_or(JsonValue::Object(serde_json::Map::new()));
                events.push((event_type, json));
            }
        }
        events
    }

    /// A Responses request exercising namespace, custom and plain function tools
    /// plus encrypted-reasoning include.
    fn integration_responses_request(stream: bool) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "model": "gpt-5",
            "instructions": "be precise",
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]}
            ],
            "tools": [
                {"type":"function","name":"plain_tool","description":"plain"},
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","description":"spawn","parameters":{"type":"object"}}
                ]},
                {"type":"custom","name":"lookup","description":"lookup"}
            ],
            "include": ["reasoning.encrypted_content"],
            "stream": stream
        }))
        .expect("serialize responses request")
    }

    #[monoio::test_all(enable_timer = true)]
    async fn buffered_responses_bridge_emits_reasoning_text_tool_and_usage() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock anthropic");
        let address = listener.local_addr().expect("mock address");

        let server = monoio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let (request_line, body) = read_http_request_with_body(&mut stream).await;
            assert!(
                request_line.starts_with("POST /v1/messages HTTP/1.1"),
                "upstream request line was {request_line}"
            );
            let claude_request: JsonValue =
                serde_json::from_slice(&body).expect("upstream body should be Claude JSON");
            assert_eq!(claude_request["model"], "claude-sonnet-4");
            // Namespace child flattened to collaboration__spawn_agent.
            let tools = claude_request["tools"].as_array().expect("tools array");
            let tool_names: Vec<&str> = tools
                .iter()
                .map(|t| t["name"].as_str().unwrap_or(""))
                .collect();
            assert!(tool_names.contains(&"collaboration__spawn_agent"));
            assert!(tool_names.contains(&"plain_tool"));
            assert!(tool_names.contains(&"lookup"));

            let response = serde_json::json!({
                "id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4",
                "content":[
                    {"type":"thinking","thinking":"why","signature":"sig-buffered"},
                    {"type":"text","text":"hello"},
                    {"type":"tool_use","id":"toolu_1","name":"lookup","input":{"input":"{\"q\":1}"}},
                    {"type":"tool_use","id":"toolu_2","name":"collaboration__spawn_agent","input":{"task":"x"}}
                ],
                "stop_reason":"end_turn",
                "usage":{
                    "input_tokens":10,
                    "output_tokens":5,
                    "cache_read_input_tokens":3,
                    "cache_creation_input_tokens":2
                }
            });
            let body = serde_json::to_vec(&response).unwrap();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            stream
                .write_all(head.into_bytes())
                .await
                .0
                .expect("write head");
            stream.write_all(body).await.0.expect("write body");
        });

        let relay = test_relay_service();
        let cx = test_full_context(test_route_context(format!("http://{address}")));
        let request = OpenAiPassthroughRelayRequest {
            path: "/v1/responses".to_string(),
            body: Bytes::from(integration_responses_request(false)),
            downstream_headers: HeaderMap::new(),
            cx,
        };
        let response = monoio::time::timeout(Duration::from_secs(5), relay.call(request))
            .await
            .expect("relay call should not time out")
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::OK);
        let payload = collect_response_body(response).await;
        server.await;
        let value: JsonValue = serde_json::from_slice(&payload).expect("responses JSON");
        assert_eq!(value["object"], "response");
        let output = value["output"].as_array().expect("output array");
        let reasoning = output
            .iter()
            .find(|item| item["type"] == "reasoning")
            .expect("reasoning output item");
        assert_eq!(reasoning["summary"][0]["text"], "why");
        assert_eq!(reasoning["encrypted_content"], "sig-buffered");
        let message = output
            .iter()
            .find(|item| item["type"] == "message")
            .expect("message output item");
        assert_eq!(message["content"][0]["text"], "hello");
        let custom_call = output
            .iter()
            .find(|item| item["type"] == "custom_tool_call")
            .expect("custom_tool_call output item");
        assert_eq!(custom_call["name"], "lookup");
        assert_eq!(custom_call["call_id"], "toolu_1");
        assert_eq!(custom_call["input"], "{\"q\":1}");
        // namespace tool restored to name + namespace.
        let ns_call = output
            .iter()
            .find(|item| item["type"] == "function_call" && item.get("namespace").is_some())
            .expect("namespace function_call output item");
        assert_eq!(ns_call["name"], "spawn_agent");
        assert_eq!(ns_call["namespace"], "collaboration");
        assert_eq!(ns_call["call_id"], "toolu_2");

        let usage = &value["usage"];
        assert_eq!(usage["input_tokens"], 15);
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 3);
        assert_eq!(usage["cache_creation_input_tokens"], 2);
        assert_eq!(usage["output_tokens"], 5);
        assert_eq!(usage["total_tokens"], 20);
    }

    #[monoio::test_all(enable_timer = true)]
    async fn sse_responses_bridge_streams_reasoning_signature_text_and_tools() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock anthropic sse");
        let address = listener.local_addr().expect("mock address");

        let server = monoio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let (request_line, _body) = read_http_request_with_body(&mut stream).await;
            assert!(request_line.starts_with("POST /v1/messages HTTP/1.1"));
            let head =
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\n";
            stream
                .write_all(head.to_vec())
                .await
                .0
                .expect("write sse head");

            let frames = [
                r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_sse","type":"message","role":"assistant","model":"claude-sonnet-4","content":[],"stop_reason":null,"usage":{"input_tokens":2,"output_tokens":0}}}

"#,
                r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":null}}

"#,
                r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"why"}}

"#,
                r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-sse"}}

"#,
                r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}

"#,
                r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

"#,
                r#"event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hello"}}

"#,
                r#"event: content_block_stop
data: {"type":"content_block_stop","index":1}

"#,
                r#"event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"lookup","input":{}}}

"#,
                r#"event: content_block_stop
data: {"type":"content_block_stop","index":2}

"#,
                r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5,"input_tokens":10,"cache_read_input_tokens":3,"cache_creation_input_tokens":2}}

"#,
                r#"event: message_stop
data: {"type":"message_stop"}

"#,
            ];
            for frame in frames {
                let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
                stream
                    .write_all(chunk.into_bytes())
                    .await
                    .0
                    .expect("write chunk");
            }
            stream
                .write_all(b"0\r\n\r\n".to_vec())
                .await
                .0
                .expect("write terminator");
        });

        let relay = test_relay_service();
        let cx = test_full_context(test_route_context(format!("http://{address}")));
        let request = OpenAiPassthroughRelayRequest {
            path: "/v1/responses".to_string(),
            body: Bytes::from(integration_responses_request(true)),
            downstream_headers: HeaderMap::new(),
            cx,
        };
        let response = monoio::time::timeout(Duration::from_secs(5), relay.call(request))
            .await
            .expect("relay call should not time out")
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::OK);
        let payload = collect_response_body(response).await;
        server.await;
        let events = parse_responses_sse(&payload);
        let event_types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();
        assert!(
            event_types.contains(&"response.created"),
            "missing created: {event_types:?}"
        );
        assert!(
            event_types.contains(&"response.reasoning_summary_text.delta"),
            "missing reasoning delta: {event_types:?}"
        );
        assert!(
            event_types.contains(&"response.reasoning_summary_text.done"),
            "missing reasoning done: {event_types:?}"
        );
        assert!(
            event_types.contains(&"response.output_text.delta"),
            "missing text delta: {event_types:?}"
        );
        assert!(
            event_types.contains(&"response.custom_tool_call_input.done"),
            "missing custom tool done: {event_types:?}"
        );
        let completed = events
            .iter()
            .find(|(t, _)| t == "response.completed")
            .expect("response.completed terminal event");
        let output = completed.1["response"]["output"]
            .as_array()
            .expect("output");
        let reasoning_item = output
            .iter()
            .find(|item| item["type"] == "reasoning")
            .expect("reasoning item in terminal output");
        assert_eq!(reasoning_item["encrypted_content"], "sig-sse");
        assert_eq!(reasoning_item["summary"][0]["text"], "why");
        let usage = &completed.1["response"]["usage"];
        assert_eq!(usage["input_tokens"], 15);
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 3);
        assert_eq!(usage["cache_creation_input_tokens"], 2);

        // sub2api: content_part.added must precede the first output_text.delta
        // for the same item, otherwise the SDK's accumulating stream helper
        // raises IndexError.
        let part_added_pos = event_types
            .iter()
            .position(|t| *t == "response.content_part.added")
            .expect("content_part.added");
        let first_text_delta_pos = event_types
            .iter()
            .position(|t| *t == "response.output_text.delta")
            .expect("output_text.delta");
        assert!(
            part_added_pos < first_text_delta_pos,
            "content_part.added must precede output_text.delta: {event_types:?}"
        );

        // sub2api: response.completed must carry the full output list (the SDK's
        // get_final_response() reads it directly).
        let msg_item = output
            .iter()
            .find(|item| item["type"] == "message")
            .expect("message item in terminal output");
        assert_eq!(msg_item["content"][0]["text"], "hello");
    }

    #[monoio::test_all(enable_timer = true)]
    async fn responses_bridge_maps_upstream_non_2xx_to_error() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind error upstream");
        let address = listener.local_addr().expect("mock address");

        let server = monoio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let (_line, _body) = read_http_request_with_body(&mut stream).await;
            let error_body =
                br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
            let head = format!(
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: \
                 application/json\r\nContent-Length: {}\r\n\r\n",
                error_body.len()
            );
            stream
                .write_all(head.into_bytes())
                .await
                .0
                .expect("write head");
            stream
                .write_all(error_body.to_vec())
                .await
                .0
                .expect("write body");
        });

        let relay = test_relay_service();
        let cx = test_full_context(test_route_context(format!("http://{address}")));
        let request = OpenAiPassthroughRelayRequest {
            path: "/v1/responses".to_string(),
            body: Bytes::from(integration_responses_request(false)),
            downstream_headers: HeaderMap::new(),
            cx,
        };
        let response = monoio::time::timeout(Duration::from_secs(5), relay.call(request))
            .await
            .expect("relay call should not time out")
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let payload = collect_response_body(response).await;
        server.await;
        let value: JsonValue = serde_json::from_slice(&payload).expect("error JSON");
        assert_eq!(value["error"]["type"], "upstream_error");
        assert!(
            value["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("slow down"))
        );
    }

    #[monoio::test_all(enable_timer = true)]
    async fn responses_bridge_emits_terminal_event_on_eof_without_message_stop() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind eof upstream");
        let address = listener.local_addr().expect("mock address");

        let server = monoio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let (_line, _body) = read_http_request_with_body(&mut stream).await;
            let head =
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\n";
            stream.write_all(head.to_vec()).await.0.expect("write head");
            // message_start + a single text delta, then EOF without message_stop.
            let frame = "event: message_start\ndata: \
                         {\"type\":\"message_start\",\"message\":{\"id\":\"msg_eof\",\"type\":\"\
                         message\",\"role\":\"assistant\",\"model\":\"claude\",\"content\":[],\"\
                         stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n";
            let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
            stream
                .write_all(chunk.into_bytes())
                .await
                .0
                .expect("write frame");
            let text_frame = "event: content_block_start\ndata: \
                              {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"\
                              type\":\"text\",\"text\":\"\"}}\n\n";
            let chunk = format!("{:x}\r\n{}\r\n", text_frame.len(), text_frame);
            stream
                .write_all(chunk.into_bytes())
                .await
                .0
                .expect("write text start");
            // abrupt EOF: terminate the chunked stream.
            stream
                .write_all(b"0\r\n\r\n".to_vec())
                .await
                .0
                .expect("write terminator");
        });

        let relay = test_relay_service();
        let cx = test_full_context(test_route_context(format!("http://{address}")));
        let request = OpenAiPassthroughRelayRequest {
            path: "/v1/responses".to_string(),
            body: Bytes::from(integration_responses_request(true)),
            downstream_headers: HeaderMap::new(),
            cx,
        };
        let response = monoio::time::timeout(Duration::from_secs(5), relay.call(request))
            .await
            .expect("relay call should not time out")
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::OK);
        let payload = collect_response_body(response).await;
        server.await;
        let events = parse_responses_sse(&payload);
        let event_types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();
        // finish() must still emit a terminal event even without message_stop.
        assert!(
            event_types
                .iter()
                .any(|t| *t == "response.completed" || *t == "response.incomplete"),
            "missing terminal event after EOF: {event_types:?}"
        );
    }
}
