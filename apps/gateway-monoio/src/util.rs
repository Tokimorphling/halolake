use super::*;
use std::collections::HashMap;

pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    auth.strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

pub(crate) fn anthropic_version<'a>(headers: &'a HeaderMap, default: &'a str) -> &'a str {
    headers
        .get("anthropic-version")
        .and_then(|value| value.to_str().ok())
        .filter(|version| !version.is_empty())
        .unwrap_or(default)
}

pub(crate) fn upstream_uri(base_url: &str, path: &str) -> Result<Uri> {
    format!("{}{}", base_url.trim_end_matches('/'), path)
        .parse()
        .context("invalid upstream uri")
}

pub(crate) fn path_and_query<'a>(uri: &'a Uri, fallback: &'a str) -> &'a str {
    uri.path_and_query().map_or(fallback, |pq| pq.as_str())
}

pub(crate) fn authority(uri: &Uri) -> Result<HeaderValue> {
    let authority = uri.authority().context("upstream uri missing authority")?;
    HeaderValue::from_str(authority.as_str()).context("invalid upstream authority header")
}

pub(crate) async fn timeout_opt<F, T>(timeout: Option<Duration>, fut: F) -> Result<T>
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

const DNS_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
struct CachedDnsAddress {
    address: SocketAddr,
    expires_at: Instant,
}

/// Worker-local DNS cache. Blocking libc resolution runs on the shared monoio
/// blocking pool so a cache miss cannot stall every connection on a
/// thread-per-core worker.
#[derive(Debug, Clone, Default)]
pub(crate) struct LocalDnsResolver {
    entries: Rc<RefCell<HashMap<String, HashMap<u16, CachedDnsAddress>>>>,
}

impl LocalDnsResolver {
    pub(crate) async fn resolve(&self, host: &str, port: u16) -> Result<SocketAddr> {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(SocketAddr::new(ip, port));
        }
        let now = Instant::now();
        if let Some(address) = self
            .entries
            .borrow()
            .get(host)
            .and_then(|ports| ports.get(&port))
            .filter(|entry| entry.expires_at > now)
            .map(|entry| entry.address)
        {
            return Ok(address);
        }

        let host_owned = host.to_string();
        let lookup = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let address = monoio::spawn_blocking(move || {
            lookup
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| std::io::Error::other("DNS returned no addresses"))
        })
        .await
        .map_err(|err| anyhow::anyhow!("DNS worker failed: {err:?}"))?
        .with_context(|| format!("resolve {host}:{port}"))?;
        self.entries
            .borrow_mut()
            .entry(host_owned)
            .or_default()
            .insert(
                port,
                CachedDnsAddress {
                    address,
                    expires_at: Instant::now() + DNS_CACHE_TTL,
                },
            );
        Ok(address)
    }
}

pub(crate) fn debug_relay<CX>(cx: &CX, stream: bool, body: Option<&[u8]>)
where
    CX: ParamRef<RouteContext> + ParamRef<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    let route = ParamRef::<RouteContext>::param_ref(cx);
    let auth = ParamRef::<RequestAuth>::param_ref(cx);
    let request_id = ParamRef::<RequestId>::param_ref(cx);
    let peer = ParamRef::<PeerAddr>::param_ref(cx);
    // Per-request access logs stay at DEBUG. Usage accounting and aggregate
    // telemetry carry production metrics without formatting two INFO events
    // for every proxied request.
    debug!(
        request_id = %request_id.0,
        peer_addr = %peer.0,
        user_id = %auth.user_id,
        token_id = %auth.token_id,
        channel_id = %route.channel_id,
        api_key_index = ?route.api_key_index,
        provider = ?route.provider,
        using_group = %route.using_group,
        requested_model = %route.requested_model,
        upstream_model = %route.upstream_model,
        stream,
        body_bytes = body.map(|b| b.len()).unwrap_or(0),
        "relay request"
    );

    // Parsing and recursively redacting a prompt-sized JSON value is expensive.
    // Only pay that cost when a DEBUG subscriber will actually record the
    // structural summary and preview.
    if let Some(summary) = debug_body_summary(tracing::enabled!(tracing::Level::DEBUG), body) {
        debug!(
            request_id = %request_id.0,
            message_count = summary.message_count,
            max_tokens = ?summary.max_tokens,
            temperature = ?summary.temperature,
            top_p = ?summary.top_p,
            reasoning_effort = ?summary.reasoning_effort,
            thinking_budget = ?summary.thinking_budget,
            has_tools = summary.has_tools,
            has_images = summary.has_images,
            body_preview = %summary.preview,
            "relay request body summary"
        );
    }
}

#[inline]
fn debug_body_summary(enabled: bool, body: Option<&[u8]>) -> Option<RequestBodySummary> {
    if !enabled {
        return None;
    }
    Some(body.map(request_body_summary).unwrap_or_default())
}

/// Redacted structural summary of a chat-like request body.
#[derive(Debug, Default)]
pub(crate) struct RequestBodySummary {
    pub(crate) message_count: usize,
    pub(crate) max_tokens: Option<u64>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) thinking_budget: Option<u64>,
    pub(crate) has_tools: bool,
    pub(crate) has_images: bool,
    pub(crate) preview: String,
}

pub(crate) fn request_body_summary(body: &[u8]) -> RequestBodySummary {
    #[cfg(test)]
    REQUEST_BODY_SUMMARY_INVOCATIONS.with(|count| count.set(count.get() + 1));

    let Ok(value) = serde_json::from_slice::<JsonValue>(body) else {
        return RequestBodySummary {
            preview: format!("<non-json body {}B>", body.len()),
            ..Default::default()
        };
    };
    let mut summary = RequestBodySummary::default();
    if let Some(messages) = value.get("messages").and_then(JsonValue::as_array) {
        summary.message_count = messages.len();
        summary.has_images = messages.iter().any(message_has_image);
    } else if let Some(contents) = value.get("contents").and_then(JsonValue::as_array) {
        // Gemini native
        summary.message_count = contents.len();
        summary.has_images = contents.iter().any(message_has_image);
    } else if let Some(input) = value.get("input").and_then(JsonValue::as_array) {
        // OpenAI Responses API
        summary.message_count = input.len();
        summary.has_images = input.iter().any(message_has_image);
    } else if value.get("input").and_then(JsonValue::as_str).is_some() {
        summary.message_count = 1;
    }
    summary.max_tokens = value
        .get("max_tokens")
        .or_else(|| value.get("max_completion_tokens"))
        .and_then(JsonValue::as_u64);
    summary.temperature = value.get("temperature").and_then(JsonValue::as_f64);
    summary.top_p = value.get("top_p").and_then(JsonValue::as_f64);
    summary.reasoning_effort = extract_reasoning_effort(&value);
    summary.thinking_budget = extract_thinking_budget(&value);
    summary.has_tools = value
        .get("tools")
        .and_then(JsonValue::as_array)
        .is_some_and(|tools| !tools.is_empty())
        || value.get("functions").is_some()
        || value.get("tools").is_some() && value.get("tool_config").is_some();
    summary.preview = redact_json_preview(&value, 512);
    summary
}

#[cfg(test)]
std::thread_local! {
    static REQUEST_BODY_SUMMARY_INVOCATIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(test)]
fn request_body_summary_invocations() -> usize {
    REQUEST_BODY_SUMMARY_INVOCATIONS.with(std::cell::Cell::get)
}

/// Align with new-api `ForceStreamOption` / OpenAI adaptor:
/// when `stream=true`, ensure `stream_options.include_usage=true` so upstream SSE
/// includes a final usage chunk (chat completions and many OpenAI-compat APIs).
pub(crate) fn ensure_openai_stream_include_usage(value: &mut JsonValue) {
    let is_stream = value
        .get("stream")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    if !is_stream {
        return;
    }
    match value.get_mut("stream_options") {
        Some(JsonValue::Object(map)) => {
            map.insert("include_usage".to_string(), JsonValue::Bool(true));
        }
        Some(other) => {
            *other = serde_json::json!({ "include_usage": true });
        }
        None => {
            value["stream_options"] = serde_json::json!({ "include_usage": true });
        }
    }
}

fn extract_reasoning_effort(value: &JsonValue) -> Option<String> {
    // OpenAI/DeepSeek style: reasoning_effort / reasoning.effort / thinking.type
    value
        .get("reasoning_effort")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .or_else(|| {
            value
                .get("reasoning")
                .and_then(|r| r.get("effort"))
                .and_then(JsonValue::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            value
                .get("thinking")
                .and_then(|t| t.get("type").or_else(|| t.get("effort")))
                .and_then(JsonValue::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            // Claude extended thinking
            value
                .get("thinking")
                .and_then(|t| t.get("type"))
                .and_then(JsonValue::as_str)
                .map(str::to_string)
        })
}

fn extract_thinking_budget(value: &JsonValue) -> Option<u64> {
    value
        .get("thinking")
        .and_then(|t| t.get("budget_tokens").or_else(|| t.get("budget")))
        .and_then(JsonValue::as_u64)
        .or_else(|| {
            value
                .get("reasoning")
                .and_then(|r| r.get("max_tokens").or_else(|| r.get("budget_tokens")))
                .and_then(JsonValue::as_u64)
        })
}

fn message_has_image(message: &JsonValue) -> bool {
    match message.get("content") {
        Some(JsonValue::Array(parts)) => parts.iter().any(|part| {
            part.get("type")
                .and_then(JsonValue::as_str)
                .is_some_and(|t| t.contains("image") || t == "input_image")
                || part.get("inline_data").is_some()
                || part.get("image_url").is_some()
        }),
        Some(JsonValue::Object(obj)) => {
            obj.contains_key("inline_data") || obj.contains_key("image_url")
        }
        _ => false,
    }
}

/// Produce a compact redacted JSON string for logs.
/// - Truncates long strings (message content, prompts)
/// - Redacts obvious secrets / data-URLs / base64 blobs
/// - Caps total output length
pub(crate) fn redact_json_preview(value: &JsonValue, max_len: usize) -> String {
    let redacted = redact_value(value, 0);
    let rendered = redacted.to_string();
    if rendered.len() <= max_len {
        rendered
    } else {
        let mut end = max_len.saturating_sub(1);
        while end > 0 && !rendered.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &rendered[..end])
    }
}

fn redact_value(value: &JsonValue, depth: usize) -> JsonValue {
    if depth > 8 {
        return JsonValue::String("<max-depth>".into());
    }
    match value {
        JsonValue::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, child) in map {
                let key_l = key.to_ascii_lowercase();
                if is_secret_key(&key_l) {
                    out.insert(key.clone(), JsonValue::String(redact_secret_string(child)));
                } else if is_content_key(&key_l) {
                    out.insert(key.clone(), redact_content_value(child, depth + 1));
                } else {
                    out.insert(key.clone(), redact_value(child, depth + 1));
                }
            }
            JsonValue::Object(out)
        }
        JsonValue::Array(items) => {
            let mut out = Vec::with_capacity(items.len().min(32));
            for (idx, item) in items.iter().enumerate() {
                if idx >= 32 {
                    out.push(JsonValue::String(format!("<+{} more>", items.len() - 32)));
                    break;
                }
                out.push(redact_value(item, depth + 1));
            }
            JsonValue::Array(out)
        }
        JsonValue::String(s) => JsonValue::String(truncate_string(s, 120)),
        other => other.clone(),
    }
}

fn is_secret_key(key: &str) -> bool {
    matches!(
        key,
        "authorization"
            | "api_key"
            | "apikey"
            | "x-api-key"
            | "token"
            | "access_token"
            | "refresh_token"
            | "password"
            | "secret"
            | "client_secret"
    ) || key.contains("api_key")
        || key.ends_with("_key")
        || key.ends_with("_token")
        || key.ends_with("_secret")
}

fn is_content_key(key: &str) -> bool {
    matches!(
        key,
        "content"
            | "text"
            | "prompt"
            | "input"
            | "messages"
            | "system"
            | "input_text"
            | "output_text"
            | "reasoning_content"
            | "thinking"
            | "data"
            | "image_url"
            | "url"
            | "b64_json"
            | "inline_data"
    )
}

fn redact_secret_string(value: &JsonValue) -> String {
    match value {
        JsonValue::String(s) if s.chars().count() <= 8 => "***".into(),
        JsonValue::String(s) => {
            let prefix: String = s.chars().take(4).collect();
            format!("{prefix}…***")
        }
        _ => "***".into(),
    }
}

fn redact_content_value(value: &JsonValue, depth: usize) -> JsonValue {
    match value {
        JsonValue::String(s) => {
            if s.starts_with("data:") || looks_like_base64(s) {
                JsonValue::String(format!("<binary {}B>", s.len()))
            } else {
                JsonValue::String(truncate_string(s, 80))
            }
        }
        JsonValue::Array(_) | JsonValue::Object(_) => redact_value(value, depth),
        other => other.clone(),
    }
}

fn looks_like_base64(s: &str) -> bool {
    s.len() > 200
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=' || b == b'\n')
}

fn truncate_string(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{truncated}...(+{n} chars)", n = count - max_chars)
}

pub(crate) fn log_response_usage(
    request_id: &str,
    status: StatusCode,
    latency_ms: u64,
    usage: Option<ResponseUsage>,
    upstream_request_id: &str,
    is_stream: bool,
) {
    debug!(
        %request_id,
        status = status.as_u16(),
        latency_ms,
        is_stream,
        upstream_request_id = %upstream_request_id,
        prompt_tokens = ?usage.and_then(|u| u.prompt_tokens),
        completion_tokens = ?usage.and_then(|u| u.completion_tokens),
        total_tokens = ?usage.and_then(|u| u.total_tokens),
        cache_read_tokens = ?usage.and_then(|u| u.cache_read_tokens),
        cache_creation_tokens = ?usage.and_then(|u| u.cache_creation_tokens),
        image_tokens = ?usage.and_then(|u| u.image_tokens),
        audio_tokens = ?usage.and_then(|u| u.audio_tokens),
        "relay response"
    );
}

#[cfg(test)]
mod redact_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn skips_body_parse_and_redaction_when_debug_is_disabled() {
        let body = br#"{"messages":[{"role":"user","content":"large prompt"}]}"#;
        let before = request_body_summary_invocations();

        assert!(debug_body_summary(false, Some(body)).is_none());
        assert_eq!(request_body_summary_invocations(), before);

        assert!(debug_body_summary(true, Some(body)).is_some());
        assert_eq!(request_body_summary_invocations(), before + 1);
    }

    #[test]
    fn redacts_message_content_and_api_keys() {
        let value = json!({
            "model": "gpt-4o",
            "api_key": "sk-supersecret",
            "token": "密钥secret-value",
            "messages": [
                {"role": "user", "content": "hello world this is a long message that should truncate"}
            ],
            "reasoning_effort": "high",
            "max_tokens": 1024
        });
        let summary = request_body_summary(value.to_string().as_bytes());
        assert_eq!(summary.message_count, 1);
        assert_eq!(summary.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(summary.max_tokens, Some(1024));
        assert!(summary.preview.contains("sk-s…***") || summary.preview.contains("***"));
        assert!(!summary.preview.contains("supersecret"));
        assert!(!summary.preview.contains("secret-value"));
        assert!(summary.preview.contains("hello world") || summary.preview.contains("…"));
    }

    #[test]
    fn detects_images_and_tools() {
        let value = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what is this"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
                ]
            }],
            "tools": [{"type": "function"}]
        });
        let summary = request_body_summary(value.to_string().as_bytes());
        assert!(summary.has_images);
        assert!(summary.has_tools);
        assert!(summary.preview.contains("<binary") || summary.preview.contains("image"));
    }
}

pub(crate) fn is_stream_like(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/event-stream"))
}

pub(crate) fn is_event_stream_response(resp: &Response<HttpBody>) -> bool {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| {
            content_type
                .to_ascii_lowercase()
                .contains("text/event-stream")
        })
}

pub(crate) fn is_gemini_generate_content_path(path: &str) -> bool {
    parse_gemini_generate_content_path(path).is_some()
}

pub(crate) fn parse_gemini_generate_content_path(path_and_query: &str) -> Option<(String, bool)> {
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

pub(crate) fn rewrite_gemini_model_in_path(path_and_query: &str, upstream_model: &str) -> String {
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

pub(crate) fn is_forward_response_header(name: &http::HeaderName) -> bool {
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

pub(crate) fn write_sse_data(out: &mut Vec<u8>, event: &str) {
    if event == "[DONE]" {
        out.extend_from_slice(b"data: [DONE]\n\n");
    } else {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(event.as_bytes());
        out.extend_from_slice(b"\n\n");
    }
}

pub(crate) fn write_claude_sse_event(out: &mut Vec<u8>, event: &str) {
    if let Ok(value) = serde_json::from_str::<JsonValue>(event)
        && let Some(event_type) = value.get("type").and_then(JsonValue::as_str)
    {
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(event_type.as_bytes());
        out.extend_from_slice(b"\n");
    }
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(event.as_bytes());
    out.extend_from_slice(b"\n\n");
}

pub(crate) fn write_openai_image_sse_payload(
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

pub(crate) fn now_unix_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

pub(crate) fn now_unix_ms_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// Cheap prefilter for SSE frames before attempting full JSON deserialization.
///
/// Most token-stream frames only carry deltas. Usage is emitted in a small
/// terminal subset: OpenAI/Claude use `usage`, Gemini uses `usageMetadata`, and
/// the Responses API terminates with `response.completed`. A false positive is
/// harmless (the regular parser still validates the payload), while a false
/// negative would lose accounting data, so the check deliberately stays broad.
#[inline]
pub(crate) fn may_contain_response_usage(payload: &[u8]) -> bool {
    contains_json_key(payload, b"\"usage\"")
        || contains_json_key(payload, b"\"usageMetadata\"")
        || contains_bytes(payload, b"response.completed")
}

#[inline]
fn contains_json_key(payload: &[u8], quoted_key: &[u8]) -> bool {
    let mut offset = 0;
    while let Some(relative) = find_bytes(&payload[offset..], quoted_key) {
        let after_key = offset + relative + quoted_key.len();
        if payload[after_key..]
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace())
            == Some(b':')
        {
            return true;
        }
        offset = after_key;
    }
    false
}

#[inline]
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    find_bytes(haystack, needle).is_some()
}

#[inline]
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[derive(Default)]
pub(crate) struct SseBuffer {
    pending: Vec<u8>,
}

impl SseBuffer {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.push_with_done(bytes, false)
    }

    pub(crate) fn push_with_done(&mut self, bytes: &[u8], include_done: bool) -> Vec<String> {
        let mut payloads = Vec::new();
        self.for_each_payload(bytes, include_done, |payload| {
            payloads.push(String::from_utf8_lossy(payload).into_owned());
        });
        payloads
    }

    pub(crate) fn for_each_payload(
        &mut self,
        bytes: &[u8],
        include_done: bool,
        mut on_payload: impl FnMut(&[u8]),
    ) {
        // Buffer raw bytes and only decode complete events. Decoding each
        // network chunk eagerly (the previous `from_utf8_lossy` per chunk) turned
        // any multi-byte scalar split across a chunk boundary into U+FFFD, which
        // could never be reassembled since the buffer held already-decoded text.
        // Event boundaries are blank lines whose bytes (`\n`/`\r`) never appear
        // inside a UTF-8 scalar, so a complete event always holds whole scalars.
        self.pending.extend_from_slice(bytes);
        let mut consumed = 0;

        while let Some((content_len, sep_len)) = find_event_boundary(&self.pending[consumed..]) {
            let content_end = consumed + content_len;
            emit_sse_event_payload(
                &self.pending[consumed..content_end],
                include_done,
                &mut on_payload,
            );
            consumed = content_end + sep_len;
        }
        if consumed > 0 {
            // Compact once per network chunk rather than once per SSE event.
            // A chunk containing hundreds of deltas otherwise repeatedly moved
            // the same tail bytes and degraded toward quadratic work.
            self.pending.drain(..consumed);
        }
    }
}

/// Finds the first event separator (a blank line) in `buf`, returning the byte
/// offset where the event content ends and the length of the separator.
/// Recognizes LF (`\n\n`) and CRLF (`\r\n\r\n`) blank lines, plus the mixed
/// endings a CRLF/LF boundary can produce.
fn find_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == b'\n' {
            let mut j = i + 1;
            if buf.get(j) == Some(&b'\r') {
                j += 1;
            }
            if buf.get(j) == Some(&b'\n') {
                return Some((i, j + 1 - i));
            }
        }
        i += 1;
    }
    None
}

fn emit_sse_event_payload(event: &[u8], include_done: bool, on_payload: &mut impl FnMut(&[u8])) {
    let mut first: Option<&[u8]> = None;
    let mut joined = Vec::new();
    for line in event.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(payload) = line.strip_prefix(b"data:") {
            let payload = trim_ascii_start(payload);
            if let Some(previous) = first.take() {
                joined.reserve(previous.len() + payload.len() + 1);
                joined.extend_from_slice(previous);
                joined.push(b'\n');
                joined.extend_from_slice(payload);
            } else if joined.is_empty() {
                first = Some(payload);
            } else {
                joined.push(b'\n');
                joined.extend_from_slice(payload);
            }
        }
    }
    let payload = if joined.is_empty() {
        first
    } else {
        Some(joined.as_slice())
    };
    if let Some(payload) = payload
        && !payload.is_empty()
        && (include_done || payload != b"[DONE]")
    {
        on_payload(payload);
    }
}

fn trim_ascii_start(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    value
}

#[cfg(test)]
mod sse_buffer_tests {
    use super::{SseBuffer, may_contain_response_usage};

    #[test]
    fn usage_prefilter_skips_regular_provider_deltas() {
        assert!(!may_contain_response_usage(
            br#"{"choices":[{"delta":{"content":"hello"}}]}"#,
        ));
        assert!(!may_contain_response_usage(
            br#"{"type":"content_block_delta","delta":{"text":"hello"}}"#,
        ));
        assert!(!may_contain_response_usage(
            br#"{"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}"#,
        ));
    }

    #[test]
    fn usage_prefilter_keeps_all_supported_usage_shapes() {
        assert!(may_contain_response_usage(
            br#"{"usage":{"prompt_tokens":12,"completion_tokens":4}}"#,
        ));
        assert!(may_contain_response_usage(
            br#"{"type":"message_delta","usage" : {"output_tokens":5}}"#,
        ));
        assert!(may_contain_response_usage(
            br#"{"usageMetadata":{"promptTokenCount":9}}"#,
        ));
        assert!(may_contain_response_usage(
            br#"{"type":"response.completed","response":{"id":"resp_1"}}"#,
        ));
    }

    #[test]
    fn usage_prefilter_requires_usage_to_be_a_json_key() {
        assert!(!may_contain_response_usage(
            br#"{"message":"the word usage is not accounting data"}"#,
        ));
        assert!(!may_contain_response_usage(br#"{"kind":"usage"}"#));
    }

    #[test]
    fn reassembles_multibyte_scalar_split_across_chunks() {
        // "你好" is 6 UTF-8 bytes; split the first scalar across two pushes.
        let event = b"data: {\"text\":\"\xe4\xbd\xa0\xe5\xa5\xbd\"}\n\n";
        let (head, tail) = event.split_at(9); // mid-way through the first scalar

        let mut buffer = SseBuffer::default();
        assert!(buffer.push(head).is_empty());
        let payloads = buffer.push(tail);

        assert_eq!(payloads, vec!["{\"text\":\"你好\"}".to_string()]);
    }

    #[test]
    fn handles_crlf_blank_line_separators() {
        let mut buffer = SseBuffer::default();
        let payloads = buffer.push(b"data: one\r\n\r\ndata: two\r\n\r\n");
        assert_eq!(payloads, vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn joins_multiple_data_lines_without_changing_sse_semantics() {
        let mut buffer = SseBuffer::default();
        assert_eq!(
            buffer.push(b"event: message\ndata: first\ndata: second\n\n"),
            vec!["first\nsecond".to_string()]
        );
    }

    #[test]
    fn filters_done_sentinel_unless_requested() {
        let mut buffer = SseBuffer::default();
        assert!(buffer.push(b"data: [DONE]\n\n").is_empty());

        let mut buffer = SseBuffer::default();
        assert_eq!(
            buffer.push_with_done(b"data: [DONE]\n\n", true),
            vec!["[DONE]".to_string()]
        );
    }
}

#[cfg(test)]
mod stream_options_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn forces_stream_options_include_usage() {
        let mut value = json!({
            "model": "gpt-4o",
            "stream": true,
            "messages": []
        });
        ensure_openai_stream_include_usage(&mut value);
        assert_eq!(value["stream_options"]["include_usage"], json!(true));

        let mut value = json!({
            "model": "gpt-4o",
            "stream": true,
            "stream_options": { "include_usage": false, "foo": 1 }
        });
        ensure_openai_stream_include_usage(&mut value);
        assert_eq!(value["stream_options"]["include_usage"], json!(true));
        assert_eq!(value["stream_options"]["foo"], json!(1));

        let mut value = json!({ "stream": false });
        ensure_openai_stream_include_usage(&mut value);
        assert!(value.get("stream_options").is_none());
    }

    #[test]
    fn counts_responses_api_input() {
        let value = json!({
            "model": "grok-4.5",
            "stream": true,
            "tools": [{"type": "function"}],
            "input": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "yo"}
            ]
        });
        let summary = request_body_summary(value.to_string().as_bytes());
        assert_eq!(summary.message_count, 2);
        assert!(summary.has_tools);
    }
}
