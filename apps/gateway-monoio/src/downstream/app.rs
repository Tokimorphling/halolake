//! Application routing service (business layer).

use super::super::*;
use monoio_http::common::body::HttpBody;
use service_async::{
    MakeService,
    layer::{FactoryLayer, layer_fn},
};

/// Top-level request router. Collects the body, then dispatches into the
/// existing chat/image/claude/gemini/raw service graph.
#[derive(Clone)]
pub(crate) struct GatewayAppService {
    gateway: Gateway,
}

impl GatewayAppService {
    pub(crate) fn new(gateway: Gateway) -> Self {
        Self { gateway }
    }

    pub(crate) fn layer<C>() -> impl FactoryLayer<C, Gateway, Factory = Self> {
        layer_fn(|_c: &C, gateway| Self::new(gateway))
    }
}

impl Service<(Request<HttpBody>, SocketAddr)> for GatewayAppService {
    type Response = Response<HttpBody>;
    type Error = Infallible;

    async fn call(
        &self,
        (req, peer): (Request<HttpBody>, SocketAddr),
    ) -> Result<Self::Response, Self::Error> {
        Ok(self.gateway.handle(req, peer).await)
    }
}

impl MakeService for GatewayAppService {
    type Service = Self;
    type Error = Infallible;

    fn make_via_ref(&self, _old: Option<&Self::Service>) -> Result<Self::Service, Self::Error> {
        Ok(self.clone())
    }
}

impl MakeService for Gateway {
    type Service = Self;
    type Error = Infallible;

    fn make_via_ref(&self, _old: Option<&Self::Service>) -> Result<Self::Service, Self::Error> {
        Ok(self.clone())
    }
}

impl Gateway {
    pub(crate) async fn handle(
        &self,
        req: Request<HttpBody>,
        peer: SocketAddr,
    ) -> Response<HttpBody> {
        let request_id = RequestId(Uuid::new_v4().simple().to_string());
        let span = tracing::debug_span!(
            "gateway.request",
            request_id = %request_id.0,
            peer_addr = %peer,
            method = %req.method(),
            path = %req.uri().path(),
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
                (&Method::GET, "/v1/models") => self.models_response(req.headers(), peer),
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
        req: Request<HttpBody>,
    ) -> Result<GatewayRequest, Response<HttpBody>> {
        let path = req
            .uri()
            .path_and_query()
            .map_or(req.uri().path(), |pq| pq.as_str())
            .to_string();
        let headers = req.headers().clone();
        let mut source = req.into_body();
        let mut collected = Vec::with_capacity(self.request_body_limit_bytes.min(8 * 1024));
        while let Some(chunk) = MonoioBody::next_data(&mut source).await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(err) => {
                    return Err(json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("failed to read request body: {err}"),
                    ));
                }
            };
            if chunk.len()
                > self
                    .request_body_limit_bytes
                    .saturating_sub(collected.len())
            {
                return Err(json_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "invalid_request_error",
                    "request body exceeds configured limit",
                ));
            }
            collected.extend_from_slice(&chunk);
        }
        let body = Bytes::from(collected);
        Ok(GatewayRequest {
            headers,
            path,
            body,
        })
    }

    fn models_response(&self, headers: &HeaderMap, peer: SocketAddr) -> Response<HttpBody> {
        let token = match self.extract_models_token(headers) {
            Some(token) => token,
            None => {
                return json_error(
                    StatusCode::UNAUTHORIZED,
                    "invalid_request_error",
                    "missing api key",
                );
            }
        };
        let state = self.snapshots.load();
        let auth = match state.snapshot.authenticate(&token) {
            Ok(auth) => auth,
            Err(err) => return route_error_response(err),
        };
        if let Err(err) = state.snapshot.authorize_ip(&auth, peer.ip()) {
            return route_error_response(err);
        }
        let models = state
            .snapshot
            .list_models(&auth)
            .into_iter()
            .map(|id| serde_json::json!({"id": id, "object": "model", "owned_by": "halolake"}))
            .collect::<Vec<_>>();
        json_response(
            StatusCode::OK,
            serde_json::json!({"object": "list", "data": models}),
        )
    }

    fn extract_models_token(&self, headers: &HeaderMap) -> Option<String> {
        if self.auth.accept_bearer
            && let Some(token) = bearer_token(headers)
        {
            return Some(token);
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
