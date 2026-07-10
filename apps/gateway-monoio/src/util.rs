use super::*;

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

pub(crate) fn debug_relay<CX>(cx: &CX, stream: bool)
where
    CX: ParamRef<RouteContext> + ParamRef<RequestAuth> + ParamRef<RequestId> + ParamRef<PeerAddr>,
{
    let route = ParamRef::<RouteContext>::param_ref(cx);
    let auth = ParamRef::<RequestAuth>::param_ref(cx);
    let request_id = ParamRef::<RequestId>::param_ref(cx);
    let peer = ParamRef::<PeerAddr>::param_ref(cx);
    debug!(
        request_id = %request_id.0,
        peer_addr = %peer.0,
        user_id = %auth.user_id,
        token_id = %auth.token_id,
        channel_id = %route.channel_id,
        api_key_index = ?route.api_key_index,
        provider = ?route.provider,
        requested_model = %route.requested_model,
        upstream_model = %route.upstream_model,
        stream,
        "relay request"
    );
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
    if let Ok(value) = serde_json::from_str::<JsonValue>(event) {
        if let Some(event_type) = value.get("type").and_then(JsonValue::as_str) {
            out.extend_from_slice(b"event: ");
            out.extend_from_slice(event_type.as_bytes());
            out.extend_from_slice(b"\n");
        }
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

#[derive(Default)]
pub(crate) struct SseBuffer {
    pending: Vec<u8>,
}

impl SseBuffer {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.push_with_done(bytes, false)
    }

    pub(crate) fn push_with_done(&mut self, bytes: &[u8], include_done: bool) -> Vec<String> {
        // Buffer raw bytes and only decode complete events. Decoding each
        // network chunk eagerly (the previous `from_utf8_lossy` per chunk) turned
        // any multi-byte scalar split across a chunk boundary into U+FFFD, which
        // could never be reassembled since the buffer held already-decoded text.
        // Event boundaries are blank lines whose bytes (`\n`/`\r`) never appear
        // inside a UTF-8 scalar, so a complete event always holds whole scalars.
        self.pending.extend_from_slice(bytes);
        let mut payloads = Vec::new();

        while let Some((content_end, sep_len)) = find_event_boundary(&self.pending) {
            let event: Vec<u8> = self.pending.drain(..content_end + sep_len).collect();
            let event = String::from_utf8_lossy(&event[..content_end]);
            for payload in sse_event_payloads(&event) {
                if include_done || payload != "[DONE]" {
                    payloads.push(payload);
                }
            }
        }

        payloads
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

fn sse_event_payloads(event: &str) -> Vec<String> {
    let mut data = Vec::new();
    for line in event.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            // Trim a trailing CR: `str::lines()` strips interior `\r\n`, but the
            // event's final line keeps its `\r` when the separator was `\r\n\r\n`.
            data.push(payload.trim_start().trim_end_matches('\r'));
        }
    }
    if data.is_empty() {
        Vec::new()
    } else {
        vec![data.join("\n")]
    }
}

#[cfg(test)]
mod sse_buffer_tests {
    use super::SseBuffer;

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
