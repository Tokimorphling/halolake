use super::*;
use crate::gateway::{
    ClaudeVersion, ConnectTimeout, GeminiApiVersion, PassAnthropicBeta, UpstreamReadTimeout,
};

#[derive(Clone)]
pub(crate) struct RelayService {
    claude_version: Arc<str>,
    pass_anthropic_beta: bool,
    gemini_api_version: Arc<str>,
    connect_timeout: Option<Duration>,
    http: HttpUpstream,
    https: HttpsUpstream,
}

impl RelayService {
    pub(crate) fn from_params<C>(params: &C) -> Self
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

pub(crate) struct OpenAiChatRelayRequest<CX> {
    pub(crate) request: openai::ChatCompletionRequest,
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
                        return Ok(upstream_transport_error_response(&err.to_string()));
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
                        return Ok(upstream_transport_error_response(&err.to_string()));
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
            Ok(resp) => Ok(upstream_to_response_with_usage(resp).await),
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
