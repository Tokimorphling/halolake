//! Application routing service (business layer).

use super::super::*;
use monoio_http::common::body::{BodyExt, HttpBody};
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
        req: Request<HttpBody>,
    ) -> Result<GatewayRequest, Response<HttpBody>> {
        let path = req
            .uri()
            .path_and_query()
            .map_or(req.uri().path(), |pq| pq.as_str())
            .to_string();
        let headers = req.headers().clone();
        let body = match req.into_body().bytes().await {
            Ok(bytes) => bytes,
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

    fn models_response(&self) -> Response<HttpBody> {
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
