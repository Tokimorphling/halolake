//! Playground (`/pg/chat/completions`) — session-authenticated proxy to the
//! gateway data plane. Mirrors new-api's playground path: the browser talks to
//! control-api with a cookie, and we inject a gateway token for the user.

use super::*;
use axum::body::Body;
use futures_util::StreamExt;
use http::header::{AUTHORIZATION, CONTENT_TYPE};

const DEFAULT_GATEWAY_BASE_URL: &str = "http://127.0.0.1:8082";
/// Playground tokens are short-lived and bounded so a browser session cannot
/// mint a permanent unlimited data-plane credential.
const PLAYGROUND_TOKEN_TTL_SECS: i64 = 3600;
const PLAYGROUND_TOKEN_QUOTA: i64 = 200_000;
const PLAYGROUND_TOKEN_NAME: &str = "playground";

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router.route("/pg/chat/completions", post(playground_chat_completions))
}

async fn playground_chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };

    let token_key = match playground_token_key(&state, user.id).await {
        Ok(key) => key,
        Err(resp) => return resp,
    };

    let gateway_base = gateway_base_url(&state);
    let url = format!("{}/v1/chat/completions", gateway_base.trim_end_matches('/'));

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            warn!(?err, "failed to build playground http client");
            return playground_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "playground proxy unavailable",
            );
        }
    };

    let upstream = match client
        .post(&url)
        .header(AUTHORIZATION, format!("Bearer {token_key}"))
        .header(CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(err) => {
            warn!(?err, %url, "playground proxy request failed");
            return playground_error(
                StatusCode::BAD_GATEWAY,
                &format!("gateway unreachable: {err}"),
            );
        }
    };

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    // Stream SSE and buffered JSON the same way: pipe the response body.
    let stream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(|err| std::io::Error::other(err.to_string())));
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        response.headers_mut().insert(CONTENT_TYPE, value);
    }
    // Disable buffering proxies for SSE.
    if content_type.contains("text/event-stream") {
        response
            .headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        response
            .headers_mut()
            .insert("x-accel-buffering", HeaderValue::from_static("no"));
    }
    response
}

async fn playground_token_key(state: &AppState, user_id: u64) -> Result<String, Response> {
    let now = now_unix();
    let data = state.management.current_data().map_err(management_error)?;

    // Prefer any existing non-playground user token first. Playground should not
    // mint credentials when the user already has a usable key.
    if let Some(token) = data.tokens.iter().find(|token| {
        token.user_id == user_id
            && token.name != PLAYGROUND_TOKEN_NAME
            && token.status == STATUS_ENABLED
            && !token.key.trim().is_empty()
            && (token.unlimited_quota || token.remain_quota > 0)
            && (token.expired_time == -1 || token.expired_time > now)
    }) {
        return Ok(token.key.clone());
    }

    // Reuse a still-valid playground token if present.
    if let Some(token) = data.tokens.iter().find(|token| {
        token.user_id == user_id
            && token.name == PLAYGROUND_TOKEN_NAME
            && token.status == STATUS_ENABLED
            && !token.key.trim().is_empty()
            && !token.unlimited_quota
            && token.remain_quota > 0
            && token.expired_time > now
    }) {
        return Ok(token.key.clone());
    }

    // Auto-provision a short-lived, quota-bounded playground token. Never mint
    // an unlimited permanent data-plane key from a browser session.
    let key = generate_token_key();
    let token = TokenRecord {
        id: 0,
        snapshot_id: None,
        user_id,
        snapshot_user_id: None,
        key: key.clone(),
        status: STATUS_ENABLED,
        name: PLAYGROUND_TOKEN_NAME.to_string(),
        created_time: now,
        accessed_time: now,
        expired_time: now.saturating_add(PLAYGROUND_TOKEN_TTL_SECS),
        remain_quota: PLAYGROUND_TOKEN_QUOTA,
        unlimited_quota: false,
        model_limits_enabled: false,
        model_limits: String::new(),
        allow_ips: None,
        used_quota: 0,
        group: String::new(),
        cross_group_retry: false,
    };
    state
        .management
        .call(CreateTokenRequest { token })
        .await
        .map_err(management_error)?;
    publish_management_snapshot(state)
        .await
        .map_err(management_error)?;
    // Give the gateway a moment to poll the new snapshot version. The gateway
    // polls every few seconds; without a short wait the first playground
    // request after auto-create can 401.
    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(key)
}

fn gateway_base_url(state: &AppState) -> String {
    if let Some(url) = state.gateway_base_url.as_deref() {
        return url.to_string();
    }
    if let Ok(options) = state.options.values() {
        if let Some(url) = options
            .get("GatewayBaseURL")
            .or_else(|| options.get("gateway.base_url"))
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return url.to_string();
        }
    }
    DEFAULT_GATEWAY_BASE_URL.to_string()
}

fn playground_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "playground_error",
            }
        })),
    )
        .into_response()
}
