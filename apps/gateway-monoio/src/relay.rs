use super::*;
use crate::gateway::{
    ClaudeVersion, ConnectTimeout, GeminiApiVersion, PassAnthropicBeta, UpstreamReadTimeout,
};

#[derive(Clone)]
pub(crate) struct RelayService {
    claude_version:      Arc<str>,
    pass_anthropic_beta: bool,
    gemini_api_version:  Arc<str>,
    connect_timeout:     Option<Duration>,
    http:                HttpUpstream,
    https:               HttpsUpstream,
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

    async fn send_upstream(
        &self,
        uri: Uri,
        req: Request<HttpBody>,
        proxy: Option<&str>,
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
            proxy = proxy.unwrap_or(""),
            connect_timeout_ms = ?self.connect_timeout.map(|d| d.as_millis()),
            "upstream request prepared"
        );

        if let Some(proxy_url) = proxy.map(str::trim).filter(|p| !p.is_empty()) {
            return self.send_upstream_via_proxy(uri, req, proxy_url).await;
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

    async fn send_upstream_via_proxy(
        &self,
        uri: Uri,
        req: Request<HttpBody>,
        proxy_url: &str,
    ) -> Result<Response<HttpBody>> {
        let proxy = crate::upstream_proxy::parse_proxy_endpoint(proxy_url)?;
        let target_host = uri.host().context("upstream uri missing host")?.to_string();
        let target_port =
            uri.port_u16()
                .unwrap_or(if uri.scheme() == Some(&http::uri::Scheme::HTTPS) {
                    443
                } else {
                    80
                });

        debug!(
            proxy = %proxy.canonical,
            %target_host,
            target_port,
            "connecting upstream via proxy"
        );

        let stream = crate::upstream_proxy::dial_via_proxy(
            &proxy,
            &target_host,
            target_port,
            self.connect_timeout,
        )
        .await?;

        let path = req.uri().to_string();
        if uri.scheme() == Some(&http::uri::Scheme::HTTPS) {
            let native = native_tls::TlsConnector::new().context("build native-tls connector")?;
            let tls_connector = monoio_native_tls::TlsConnector::from(native);
            let tls_stream = tls_connector
                .connect(&target_host, stream)
                .await
                .context("TLS handshake through proxy")?;
            proxy_send_request(tls_stream, req)
                .await
                .with_context(|| format!("send https via proxy {target_host}{path}"))
        } else {
            proxy_send_request(stream, req)
                .await
                .with_context(|| format!("send http via proxy {target_host}{path}"))
        }
    }
}

async fn proxy_send_request<IO>(stream: IO, req: Request<HttpBody>) -> Result<Response<HttpBody>>
where
    IO: AsyncReadRent + AsyncWriteRent + monoio::io::Split + Unpin + 'static,
{
    use monoio_http::{
        common::body::Body as MonoioBodyTrait,
        h1::{
            codec::decoder::PayloadDecoder,
            payload::{Payload, fixed_payload_pair, stream_payload_pair},
        },
    };

    let mut codec = ClientCodec::new(stream);
    if let Err(err) = codec.send_and_flush(req).await {
        anyhow::bail!("send upstream request via proxy: {err:?}");
    }
    match codec.next().await {
        Some(Ok(resp)) => {
            let (parts, payload_decoder) = resp.into_parts();
            let body = match payload_decoder {
                PayloadDecoder::None => HttpBody::from(Payload::None),
                PayloadDecoder::Fixed(_) => {
                    let mut framed_payload = payload_decoder.with_io(&mut codec);
                    let (payload, payload_sender) = fixed_payload_pair();
                    if let Some(data) = MonoioBodyTrait::next_data(&mut framed_payload).await {
                        payload_sender.feed(data);
                    }
                    HttpBody::from(Payload::Fixed(payload))
                }
                PayloadDecoder::Streamed(_) => {
                    let mut framed_payload = payload_decoder.with_io(&mut codec);
                    let (payload, mut payload_sender) = stream_payload_pair();
                    loop {
                        match MonoioBodyTrait::next_data(&mut framed_payload).await {
                            Some(Ok(data)) => payload_sender.feed_data(Some(data)),
                            Some(Err(err)) => {
                                anyhow::bail!("decode streamed upstream body via proxy: {err}");
                            }
                            None => {
                                payload_sender.feed_data(None);
                                break;
                            }
                        }
                    }
                    HttpBody::from(Payload::Stream(payload))
                }
            };
            Ok(Response::from_parts(parts, body))
        }
        Some(Err(err)) => anyhow::bail!("decode upstream response via proxy: {err}"),
        None => anyhow::bail!("proxy upstream closed without response"),
    }
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
                let body =
                    match rewrite_openai_chat_model_body(&req.raw_body, &route.upstream_model) {
                        Ok(body) => body,
                        Err(err) => {
                            return Ok(json_error(
                                StatusCode::BAD_REQUEST,
                                "invalid_request_error",
                                &format!("failed to rewrite OpenAI chat request: {err}"),
                            ));
                        }
                    };
                self.openai_passthrough(
                    route,
                    "/v1/chat/completions",
                    body,
                    &req.downstream_headers,
                )
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
                let upstream = match self
                    .send_openai_json(
                        &route,
                        &req.downstream_headers,
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
                let upstream = match self
                    .send_openai_json(
                        &route,
                        &req.downstream_headers,
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

        let req = builder
            .body(HttpBody::fixed_body(Some(body)))
            .context("build Claude upstream request")?;
        self.send_upstream(uri, req, route.proxy.as_deref()).await
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
        // Preserve client identity headers. new-api does not rewrite User-Agent by
        // default; affinity rules and Codex/Claude CLIs also rely on the original UA.
        if let Some(user_agent) = downstream_headers.get(header::USER_AGENT) {
            builder = builder.header(header::USER_AGENT, user_agent);
        }
        if let Some(beta) = downstream_headers.get("OpenAI-Beta") {
            builder = builder.header("OpenAI-Beta", beta);
        }
        // Common Codex / OpenAI client headers (allowlisted; credentials never forwarded).
        for name in [
            "Originator",
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
        let req = builder
            .body(HttpBody::fixed_body(Some(body)))
            .context("build OpenAI upstream request")?;
        self.send_upstream(uri, req, route.proxy.as_deref()).await
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
        if let Some(user_agent) = downstream_headers.get(header::USER_AGENT) {
            builder = builder.header(header::USER_AGENT, user_agent);
        }
        let req = builder
            .body(HttpBody::fixed_body(Some(body)))
            .context("build Gemini upstream request")?;
        self.send_upstream(uri, req, route.proxy.as_deref()).await
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

fn rewrite_openai_chat_model_body(raw_body: &[u8], upstream_model: &str) -> Result<Bytes> {
    let mut value: JsonValue =
        serde_json::from_slice(raw_body).context("parse OpenAI chat request body")?;
    value["model"] = JsonValue::String(upstream_model.to_string());
    ensure_openai_stream_include_usage(&mut value);
    Ok(Bytes::from(
        serde_json::to_vec(&value).context("serialize OpenAI chat request body")?,
    ))
}
