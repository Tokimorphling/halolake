use super::*;

#[derive(Debug, Clone)]
pub(crate) struct ChannelFeedbackMeta {
    pub(crate) status_code: Option<u16>,
    pub(crate) reason: ChannelFeedbackReason,
    pub(crate) message: String,
}

impl ChannelFeedbackMeta {
    fn upstream_status(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status_code: Some(status.as_u16()),
            reason: ChannelFeedbackReason::UpstreamStatus,
            message: message.into(),
        }
    }

    pub(crate) fn transport(message: impl Into<String>) -> Self {
        Self {
            status_code: None,
            reason: ChannelFeedbackReason::Transport,
            message: message.into(),
        }
    }
}

fn attach_channel_feedback(resp: &mut Response<GatewayBody>, meta: ChannelFeedbackMeta) {
    resp.extensions_mut().insert(meta);
}

#[cfg(test)]
use halolake_protocol::convert_response_json;
use halolake_protocol::{
    RelaySseTranslator, convert_response_json_with_options, relaykit::Format as RelayFormat,
};

pub(crate) async fn buffered_protocol_response(
    mut upstream: Response<HttpBody>,
    from: RelayFormat,
    to: RelayFormat,
    requested_model: FastStr,
    tools: ResponsesToolMapping,
    reverse_names: std::collections::BTreeMap<String, String>,
    include_encrypted_reasoning: bool,
) -> Response<GatewayBody> {
    if !upstream.status().is_success() {
        return upstream_error_response(upstream).await;
    }
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return upstream_transport_error_response(&format!(
                "failed reading {from} response: {err}"
            ));
        }
    };
    let input: JsonValue = match serde_json::from_slice(&payload) {
        Ok(input) => input,
        Err(err) => {
            return upstream_transport_error_response(&format!("invalid {from} response: {err}"));
        }
    };
    let usage = response_usage_from_json(&input);
    let mut output = match convert_response_json_with_options(
        from,
        to,
        input,
        requested_model.as_str(),
        &tools,
        include_encrypted_reasoning,
    ) {
        Ok(output) => output,
        Err(err) => {
            return upstream_transport_error_response(&format!(
                "failed converting {from} response to {to}: {err}"
            ));
        }
    };
    restore_chat_tool_names(&mut output, &reverse_names);
    let body = match serde_json::to_vec(&output) {
        Ok(body) => body,
        Err(err) => {
            return upstream_transport_error_response(&format!(
                "failed serializing {to} response: {err}"
            ));
        }
    };
    let mut response = protocol_response_builder(status, &headers, false)
        .body(full_body(body))
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        });
    attach_response_usage(&mut response, usage);
    response
}

pub(crate) fn stream_protocol_response(
    upstream: Response<HttpBody>,
    from: RelayFormat,
    to: RelayFormat,
    requested_model: FastStr,
    tools: ResponsesToolMapping,
    reverse_names: std::collections::BTreeMap<String, String>,
    include_encrypted_reasoning: bool,
) -> Response<GatewayBody> {
    let (parts, upstream_body) = upstream.into_parts();
    let body = stream_body_from_async(move |mut sender| async move {
        use futures_util::StreamExt;

        let mut stream = HttpBodyStream::from(upstream_body);
        let mut decoder = SseBuffer::default();
        let mut translator = RelaySseTranslator::new(from, to, requested_model, tools)
            .with_encrypted_reasoning(include_encrypted_reasoning);
        let mut sent_done = false;
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => {
                    let mut out = Vec::new();
                    for payload in decoder.push_with_done(&bytes, true) {
                        let events = match translator.translate_sse_payload(&payload) {
                            Ok(events) => events,
                            Err(err) => {
                                warn!(?err, %from, %to, "invalid upstream protocol event");
                                translator.fail("invalid_upstream_event", err.to_string())
                            }
                        };
                        write_protocol_events(&mut out, to, events, &reverse_names);
                        append_protocol_done(&mut out, from, to, &translator, &mut sent_done);
                        if translator.is_failed() {
                            break;
                        }
                    }
                    if !out.is_empty() {
                        sender.feed_data(Some(Bytes::from(out)));
                    }
                    if translator.is_failed() {
                        sender.feed_data(None);
                        return;
                    }
                }
                Err(err) => {
                    let events = translator.fail(
                        "upstream_transport_error",
                        format!("failed reading {from} stream: {err}"),
                    );
                    let mut out = Vec::new();
                    write_protocol_events(&mut out, to, events, &reverse_names);
                    if !out.is_empty() {
                        sender.feed_data(Some(Bytes::from(out)));
                    }
                    sender.feed_data(None);
                    return;
                }
            }
        }
        let events = match translator.finish() {
            Ok(events) => events,
            Err(err) => {
                warn!(?err, %from, %to, "failed finalizing protocol stream");
                translator.fail("invalid_upstream_event", err.to_string())
            }
        };
        let mut out = Vec::new();
        write_protocol_events(&mut out, to, events, &reverse_names);
        append_protocol_done(&mut out, from, to, &translator, &mut sent_done);
        if !out.is_empty() {
            sender.feed_data(Some(Bytes::from(out)));
        }
        sender.feed_data(None);
    });
    protocol_response_builder(parts.status, &parts.headers, true)
        .body(body)
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        })
}

fn protocol_response_builder(
    status: StatusCode,
    headers: &HeaderMap,
    stream: bool,
) -> http::response::Builder {
    let mut builder = response_builder(status).header(
        header::CONTENT_TYPE,
        if stream {
            "text/event-stream"
        } else {
            "application/json"
        },
    );
    if stream {
        builder = builder
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::CONNECTION, "keep-alive");
    }
    for (name, value) in headers {
        if name != header::CONTENT_TYPE
            && (!stream || name != header::CACHE_CONTROL && name != header::CONNECTION)
            && is_forward_response_header(name)
        {
            builder = builder.header(name, value);
        }
    }
    builder
}

pub(crate) fn write_protocol_sse_event(out: &mut Vec<u8>, to: RelayFormat, event: &JsonValue) {
    let data = event.to_string();
    if matches!(to, RelayFormat::Claude | RelayFormat::OpenAiResponses) {
        write_claude_sse_event(out, &data);
    } else {
        write_sse_data(out, &data);
    }
}

fn write_protocol_events(
    out: &mut Vec<u8>,
    to: RelayFormat,
    events: Vec<JsonValue>,
    reverse_names: &std::collections::BTreeMap<String, String>,
) {
    for mut event in events {
        restore_chat_tool_names(&mut event, reverse_names);
        write_protocol_sse_event(out, to, &event);
    }
}

fn append_protocol_done(
    out: &mut Vec<u8>,
    from: RelayFormat,
    to: RelayFormat,
    translator: &RelaySseTranslator,
    sent: &mut bool,
) {
    let uses_done = to == RelayFormat::OpenAi
        || from == RelayFormat::OpenAi && to == RelayFormat::OpenAiResponses;
    if uses_done && translator.is_complete() && !translator.is_failed() && !*sent {
        write_sse_data(out, "[DONE]");
        *sent = true;
    }
}

// Codex tool-name shortening is channel policy, independent of protocol conversion.
fn restore_chat_tool_names(
    value: &mut JsonValue,
    reverse_names: &std::collections::BTreeMap<String, String>,
) {
    if reverse_names.is_empty() {
        return;
    }
    let Some(choices) = value.get_mut("choices").and_then(JsonValue::as_array_mut) else {
        return;
    };
    for choice in choices {
        for key in ["message", "delta"] {
            if let Some(calls) = choice
                .get_mut(key)
                .and_then(|message| message.get_mut("tool_calls"))
                .and_then(JsonValue::as_array_mut)
            {
                for call in calls {
                    for key in ["function", "custom"] {
                        if let Some(name) = call
                            .get_mut(key)
                            .and_then(|function| function.get_mut("name"))
                            && let Some(original) =
                                name.as_str().and_then(|name| reverse_names.get(name))
                        {
                            *name = JsonValue::String(original.clone());
                        }
                    }
                }
            }
        }
    }
}

pub(crate) async fn buffered_claude_as_openai(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
) -> Response<GatewayBody> {
    buffered_protocol_response(
        upstream,
        RelayFormat::Claude,
        RelayFormat::OpenAi,
        requested_model,
        Default::default(),
        Default::default(),
        true,
    )
    .await
}

pub(crate) fn stream_claude_as_openai(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
) -> Response<GatewayBody> {
    stream_protocol_response(
        upstream,
        RelayFormat::Claude,
        RelayFormat::OpenAi,
        requested_model,
        Default::default(),
        Default::default(),
        true,
    )
}

pub(crate) async fn buffered_openai_as_claude(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
) -> Response<GatewayBody> {
    buffered_protocol_response(
        upstream,
        RelayFormat::OpenAi,
        RelayFormat::Claude,
        requested_model,
        Default::default(),
        Default::default(),
        true,
    )
    .await
}

pub(crate) fn stream_openai_as_claude(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
) -> Response<GatewayBody> {
    stream_protocol_response(
        upstream,
        RelayFormat::OpenAi,
        RelayFormat::Claude,
        requested_model,
        Default::default(),
        Default::default(),
        true,
    )
}

pub(crate) async fn buffered_responses_as_openai_chat(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
    tool_name_reverse: std::collections::BTreeMap<String, String>,
) -> Response<GatewayBody> {
    buffered_protocol_response(
        upstream,
        RelayFormat::OpenAiResponses,
        RelayFormat::OpenAi,
        requested_model,
        Default::default(),
        tool_name_reverse,
        true,
    )
    .await
}

pub(crate) fn stream_responses_as_openai_chat(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
    tool_name_reverse: std::collections::BTreeMap<String, String>,
) -> Response<GatewayBody> {
    stream_protocol_response(
        upstream,
        RelayFormat::OpenAiResponses,
        RelayFormat::OpenAi,
        requested_model,
        Default::default(),
        tool_name_reverse,
        true,
    )
}

pub(crate) async fn buffered_openai_chat_as_responses(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
    tools: ResponsesToolMapping,
) -> Response<GatewayBody> {
    buffered_protocol_response(
        upstream,
        RelayFormat::OpenAi,
        RelayFormat::OpenAiResponses,
        requested_model,
        tools,
        Default::default(),
        true,
    )
    .await
}

pub(crate) fn stream_openai_chat_as_responses(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
    tools: ResponsesToolMapping,
) -> Response<GatewayBody> {
    stream_protocol_response(
        upstream,
        RelayFormat::OpenAi,
        RelayFormat::OpenAiResponses,
        requested_model,
        tools,
        Default::default(),
        true,
    )
}

#[cfg(test)]
fn openai_chat_json_to_responses(
    payload: &[u8],
    requested_model: &str,
    tools: &ResponsesToolMapping,
) -> Result<JsonValue> {
    Ok(convert_response_json(
        RelayFormat::OpenAi,
        RelayFormat::OpenAiResponses,
        serde_json::from_slice(payload)?,
        requested_model,
        tools,
    )?)
}

#[cfg(test)]
fn responses_json_to_openai_chat(
    payload: &[u8],
    requested_model: &str,
    reverse_names: &std::collections::BTreeMap<String, String>,
) -> Result<JsonValue> {
    let mut value = convert_response_json(
        RelayFormat::OpenAiResponses,
        RelayFormat::OpenAi,
        serde_json::from_slice(payload)?,
        requested_model,
        &Default::default(),
    )?;
    restore_chat_tool_names(&mut value, reverse_names);
    Ok(value)
}

#[cfg(test)]
struct OpenAiChatSseToResponses {
    inner: RelaySseTranslator,
    done_sent: bool,
}

#[cfg(test)]
impl OpenAiChatSseToResponses {
    fn new(model: FastStr) -> Self {
        Self::with_tool_mapping(model, Default::default())
    }
    fn with_tool_mapping(model: FastStr, tools: ResponsesToolMapping) -> Self {
        Self {
            inner: RelaySseTranslator::new(
                RelayFormat::OpenAi,
                RelayFormat::OpenAiResponses,
                model,
                tools,
            ),
            done_sent: false,
        }
    }
    fn translate_sse_payload(&mut self, payload: &str) -> Result<Vec<String>> {
        let events = self.inner.translate_sse_payload(payload)?;
        Ok(self.encode(events))
    }
    fn finish(&mut self) -> Result<Vec<String>> {
        let events = self.inner.finish()?;
        Ok(self.encode(events))
    }
    fn encode(&mut self, events: Vec<JsonValue>) -> Vec<String> {
        let mut out = events
            .into_iter()
            .map(|event| event.to_string())
            .collect::<Vec<_>>();
        if self.inner.is_complete() && !self.inner.is_failed() && !self.done_sent {
            out.push("[DONE]".into());
            self.done_sent = true;
        }
        out
    }
}

#[cfg(test)]
struct ResponsesSseToOpenAiChat {
    inner: RelaySseTranslator,
    reverse_names: std::collections::BTreeMap<String, String>,
    done_sent: bool,
}

#[cfg(test)]
impl ResponsesSseToOpenAiChat {
    fn new(model: FastStr, reverse_names: std::collections::BTreeMap<String, String>) -> Self {
        Self {
            inner: RelaySseTranslator::new(
                RelayFormat::OpenAiResponses,
                RelayFormat::OpenAi,
                model,
                Default::default(),
            ),
            reverse_names,
            done_sent: false,
        }
    }
    fn translate_sse_payload(&mut self, payload: &str) -> Result<Vec<String>> {
        let events = self.inner.translate_sse_payload(payload)?;
        Ok(self.encode(events))
    }
    fn finish(&mut self, _usage: Option<&JsonValue>) -> Result<Vec<String>> {
        let events = self.inner.finish()?;
        Ok(self.encode(events))
    }
    fn encode(&mut self, events: Vec<JsonValue>) -> Vec<String> {
        let mut out = events
            .into_iter()
            .map(|mut event| {
                restore_chat_tool_names(&mut event, &self.reverse_names);
                event.to_string()
            })
            .collect::<Vec<_>>();
        if self.inner.is_complete() && !self.inner.is_failed() && !self.done_sent {
            out.push("[DONE]".into());
            self.done_sent = true;
        }
        out
    }
}

pub(crate) async fn buffered_gemini_as_openai(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
) -> Response<GatewayBody> {
    buffered_protocol_response(
        upstream,
        RelayFormat::Gemini,
        RelayFormat::OpenAi,
        requested_model,
        Default::default(),
        Default::default(),
        true,
    )
    .await
}

pub(crate) fn stream_gemini_as_openai(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
) -> Response<GatewayBody> {
    stream_protocol_response(
        upstream,
        RelayFormat::Gemini,
        RelayFormat::OpenAi,
        requested_model,
        Default::default(),
        Default::default(),
        true,
    )
}

pub(crate) async fn buffered_gemini_as_claude(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
) -> Response<GatewayBody> {
    buffered_protocol_response(
        upstream,
        RelayFormat::Gemini,
        RelayFormat::Claude,
        requested_model,
        Default::default(),
        Default::default(),
        true,
    )
    .await
}

pub(crate) fn stream_gemini_as_claude(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
) -> Response<GatewayBody> {
    stream_protocol_response(
        upstream,
        RelayFormat::Gemini,
        RelayFormat::Claude,
        requested_model,
        Default::default(),
        Default::default(),
        true,
    )
}

pub(crate) async fn buffered_openai_as_gemini(
    upstream: Response<HttpBody>,
) -> Response<GatewayBody> {
    buffered_protocol_response(
        upstream,
        RelayFormat::OpenAi,
        RelayFormat::Gemini,
        FastStr::default(),
        Default::default(),
        Default::default(),
        true,
    )
    .await
}

pub(crate) fn stream_openai_as_gemini(upstream: Response<HttpBody>) -> Response<GatewayBody> {
    stream_protocol_response(
        upstream,
        RelayFormat::OpenAi,
        RelayFormat::Gemini,
        FastStr::default(),
        Default::default(),
        Default::default(),
        true,
    )
}

pub(crate) async fn buffered_gemini_imagen_as_openai(
    mut upstream: Response<HttpBody>,
) -> Response<GatewayBody> {
    let status = upstream.status();
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading Gemini image response: {err}"),
            );
        }
    };
    let gemini_resp: gemini::GeminiImageResponse = match serde_json::from_slice(&payload) {
        Ok(resp) => resp,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid Gemini image response: {err}"),
            );
        }
    };
    match gemini_imagen_to_openai_image_response(
        gemini_resp,
        u64::try_from(now_unix_i64()).unwrap_or_default(),
    ) {
        Ok(resp) => json_response(status, resp),
        Err(err) => json_error(StatusCode::BAD_GATEWAY, "bad_gateway", &err.to_string()),
    }
}

pub(crate) async fn openai_image_json_as_stream(
    mut upstream: Response<HttpBody>,
) -> Response<GatewayBody> {
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading OpenAI image response: {err}"),
            );
        }
    };
    let image_resp: openai::ImageResponse = match serde_json::from_slice(&payload) {
        Ok(resp) => resp,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid OpenAI image response: {err}"),
            );
        }
    };

    let created = if image_resp.created == 0 {
        now_unix_i64()
    } else {
        image_resp.created
    };
    let mut out = Vec::new();
    for image in image_resp.data {
        let mut payload = serde_json::Map::new();
        payload.insert(
            "type".to_string(),
            JsonValue::String("image_generation.completed".to_string()),
        );
        payload.insert(
            "created_at".to_string(),
            JsonValue::Number(serde_json::Number::from(created)),
        );
        if !image.url.is_empty() {
            payload.insert("url".to_string(), JsonValue::String(image.url));
        }
        if !image.b64_json.is_empty() {
            payload.insert("b64_json".to_string(), JsonValue::String(image.b64_json));
        }
        if !image.revised_prompt.is_empty() {
            payload.insert(
                "revised_prompt".to_string(),
                JsonValue::String(image.revised_prompt),
            );
        }
        if let Err(err) = write_openai_image_sse_payload(
            &mut out,
            "image_generation.completed",
            &JsonValue::Object(payload),
        ) {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            );
        }
    }
    write_sse_data(&mut out, "[DONE]");

    let body: Bytes = out.into();
    response_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(full_body(body))
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        })
}

pub(crate) fn upstream_to_response(upstream: Response<HttpBody>) -> Response<GatewayBody> {
    let (parts, body) = upstream.into_parts();
    let status = parts.status;
    // Already a monoio-http body; keep streaming as-is.

    let mut builder = response_builder(status);
    for (name, value) in &parts.headers {
        if is_forward_response_header(name) {
            builder = builder.header(name, value);
        }
    }
    let mut resp = builder.body(body).unwrap_or_else(|err| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &err.to_string(),
        )
    });
    if !status.is_success() {
        attach_channel_feedback(&mut resp, ChannelFeedbackMeta::upstream_status(status, ""));
    }
    resp
}

pub(crate) async fn upstream_to_response_with_usage(
    mut upstream: Response<HttpBody>,
) -> Response<GatewayBody> {
    if is_event_stream_response(&upstream) {
        return upstream_to_response(upstream);
    }

    let status = upstream.status();
    let headers = upstream.headers().clone();
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading upstream response body: {err}"),
            );
        }
    };
    let usage = response_usage_from_json_bytes(&payload);
    let mut builder = response_builder(status);
    for (name, value) in &headers {
        if is_forward_response_header(name) {
            builder = builder.header(name, value);
        }
    }
    let message = (!status.is_success()).then(|| String::from_utf8_lossy(&payload).into_owned());
    let mut resp = builder.body(full_body(payload)).unwrap_or_else(|err| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &err.to_string(),
        )
    });
    attach_response_usage(&mut resp, usage);
    if let Some(message) = message {
        attach_channel_feedback(
            &mut resp,
            ChannelFeedbackMeta::upstream_status(status, message),
        );
    }
    resp
}

pub(crate) async fn upstream_error_response(
    mut upstream: Response<HttpBody>,
) -> Response<GatewayBody> {
    let status = upstream.status();
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => Bytes::from(format!("failed reading upstream error body: {err}")),
    };
    let message = String::from_utf8_lossy(&payload);
    let mut resp = json_error(status, "upstream_error", &message);
    attach_channel_feedback(
        &mut resp,
        ChannelFeedbackMeta::upstream_status(status, message.into_owned()),
    );
    resp
}

pub(crate) fn upstream_transport_error_response(message: &str) -> Response<GatewayBody> {
    let mut resp = json_error(StatusCode::BAD_GATEWAY, "bad_gateway", message);
    attach_channel_feedback(
        &mut resp,
        ChannelFeedbackMeta::transport(message.to_string()),
    );
    resp
}

pub(crate) fn route_error_response(err: RouteError) -> Response<GatewayBody> {
    let status = match err {
        RouteError::Unauthorized => StatusCode::UNAUTHORIZED,
        RouteError::ModelForbidden
        | RouteError::IpForbidden
        | RouteError::GroupForbidden
        | RouteError::GroupDeprecated => StatusCode::FORBIDDEN,
        RouteError::ModelNotFound => StatusCode::NOT_FOUND,
        RouteError::ChannelNotFound
        | RouteError::ChannelDisabled
        | RouteError::ChannelModelMismatch => StatusCode::BAD_GATEWAY,
    };
    json_error(status, "routing_error", &err.to_string())
}

pub(crate) fn json_response<T: Serialize>(status: StatusCode, value: T) -> Response<GatewayBody> {
    let body = match serde_json::to_vec(&value) {
        Ok(body) => body,
        Err(err) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            );
        }
    };
    response_builder(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full_body(body))
        .unwrap()
}

pub(crate) fn json_error(
    status: StatusCode,
    error_type: &str,
    message: &str,
) -> Response<GatewayBody> {
    let value = openai::ErrorResponse {
        error: openai::ErrorBody {
            message: message.to_string(),
            error_type: error_type.to_string(),
            code: None,
        },
    };
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| {
        b"{\"error\":{\"message\":\"internal error\",\"type\":\"internal_error\"}}".to_vec()
    });
    response_builder(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full_body(body))
        .unwrap()
}

pub(crate) fn response_builder(status: StatusCode) -> http::response::Builder {
    Response::builder().status(status)
}

pub(crate) fn full_body(body: impl Into<Bytes>) -> GatewayBody {
    HttpBody::fixed_body(Some(body.into()))
}

/// Bridge an async chunk producer into a monoio-http streaming body.
pub(crate) fn stream_body_from_async<F, Fut>(producer: F) -> GatewayBody
where
    F: FnOnce(StreamPayloadSender) -> Fut + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let (payload, sender) = stream_payload_pair::<Bytes, HttpError>();
    monoio::spawn(async move {
        producer(sender).await;
    });
    HttpBody::H1(Payload::Stream(payload))
}

pub(crate) type StreamPayloadSender =
    monoio_http::h1::payload::StreamPayloadSender<Bytes, HttpError>;

/// Pump a monoio HttpBodyStream into a stream payload sender, mapping each
/// chunk through `map`. Used by SSE translators.
pub(crate) async fn pump_http_body_stream<F>(
    mut stream: HttpBodyStream,
    mut sender: StreamPayloadSender,
    mut map: F,
) where
    F: FnMut(Bytes) -> Bytes + 'static,
{
    use futures_util::StreamExt;
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => {
                let out = map(bytes);
                if !out.is_empty() {
                    sender.feed_data(Some(out));
                }
            }
            Err(err) => {
                sender.feed_error(err);
                return;
            }
        }
    }
    sender.feed_data(None);
}

pub(crate) fn attach_response_usage(
    resp: &mut Response<GatewayBody>,
    usage: Option<ResponseUsage>,
) {
    if let Some(usage) = usage.filter(|usage| !usage.is_empty()) {
        resp.extensions_mut().insert(usage);
    }
}

pub(crate) fn response_usage_from_json_bytes(payload: &[u8]) -> Option<ResponseUsage> {
    serde_json::from_slice::<JsonValue>(payload)
        .ok()
        .and_then(|value| response_usage_from_json(&value))
}

fn response_usage_from_json(value: &JsonValue) -> Option<ResponseUsage> {
    let responses_usage = value.get("object").and_then(JsonValue::as_str) == Some("response")
        || value
            .get("type")
            .and_then(JsonValue::as_str)
            .is_some_and(|event_type| event_type.starts_with("response."));
    let native_claude_cache = value
        .get("usage")
        .is_some_and(usage_has_native_claude_cache_fields);
    value
        .get("usage")
        .and_then(|usage| {
            response_usage_from_usage_object(usage, responses_usage || !native_claude_cache)
        })
        // OpenAI Responses API SSE: {"type":"response.completed","response":{"usage":{...}}}
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("usage"))
                .and_then(|usage| response_usage_from_usage_object(usage, true))
        })
        .or_else(|| {
            value
                .get("usageMetadata")
                .and_then(response_usage_from_gemini_usage_object)
        })
}

fn usage_has_native_claude_cache_fields(value: &JsonValue) -> bool {
    value.get("cache_read_input_tokens").is_some()
        || value.get("cache_creation_input_tokens").is_some()
}

fn response_usage_from_usage_object(
    value: &JsonValue,
    input_tokens_include_cache: bool,
) -> Option<ResponseUsage> {
    let cache_read_tokens = json_path_u64(value, &["prompt_tokens_details", "cached_tokens"])
        .or_else(|| json_path_u64(value, &["input_tokens_details", "cached_tokens"]))
        .or_else(|| json_u64(value, "cached_tokens"))
        .or_else(|| json_u64(value, "prompt_cache_hit_tokens"))
        .or_else(|| json_u64(value, "cache_read_input_tokens"))
        .and_then(nonzero_u64);
    let cache_creation_tokens =
        json_path_u64(value, &["prompt_tokens_details", "cached_creation_tokens"])
            .or_else(|| json_path_u64(value, &["input_tokens_details", "cached_creation_tokens"]))
            .or_else(|| json_u64(value, "cache_creation_input_tokens"))
            .and_then(nonzero_u64);
    let image_tokens = json_path_u64(value, &["prompt_tokens_details", "image_tokens"])
        .or_else(|| json_path_u64(value, &["input_tokens_details", "image_tokens"]))
        .and_then(nonzero_u64);
    let audio_tokens = json_path_u64(value, &["prompt_tokens_details", "audio_tokens"])
        .or_else(|| json_path_u64(value, &["input_tokens_details", "audio_tokens"]))
        .and_then(nonzero_u64);
    let prompt_tokens = json_u64(value, "prompt_tokens")
        .and_then(nonzero_u64)
        .or_else(|| {
            let input_tokens = json_u64(value, "input_tokens");
            let has_claude_prompt = input_tokens.is_some()
                || cache_creation_tokens.is_some()
                || cache_read_tokens.is_some();
            has_claude_prompt
                .then(|| {
                    let input_tokens = input_tokens.unwrap_or(0);
                    if input_tokens_include_cache {
                        input_tokens
                    } else {
                        input_tokens
                            .saturating_add(cache_creation_tokens.unwrap_or(0))
                            .saturating_add(cache_read_tokens.unwrap_or(0))
                    }
                })
                .and_then(nonzero_u64)
        });
    let completion_tokens = json_u64(value, "completion_tokens")
        .and_then(nonzero_u64)
        .or_else(|| json_u64(value, "output_tokens").and_then(nonzero_u64));
    let total_tokens = json_u64(value, "total_tokens")
        .and_then(nonzero_u64)
        .or_else(|| {
            let total = prompt_tokens
                .unwrap_or(0)
                .saturating_add(completion_tokens.unwrap_or(0));
            (total > 0).then_some(total)
        });
    let usage = ResponseUsage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        image_tokens,
        audio_tokens,
    };
    (!usage.is_empty()).then_some(usage)
}

fn response_usage_from_gemini_usage_object(value: &JsonValue) -> Option<ResponseUsage> {
    let cached_content = json_u64(value, "cachedContentTokenCount").unwrap_or(0);
    let (image_tokens, audio_tokens) = gemini_prompt_detail_tokens(value);
    // Gemini promptTokenCount already includes cachedContentTokenCount.
    let prompt = json_u64(value, "promptTokenCount").unwrap_or(0);
    let completion = json_u64(value, "candidatesTokenCount")
        .unwrap_or(0)
        .saturating_add(json_u64(value, "thoughtsTokenCount").unwrap_or(0));
    let total =
        json_u64(value, "totalTokenCount").unwrap_or_else(|| prompt.saturating_add(completion));
    let usage = ResponseUsage {
        prompt_tokens: (prompt > 0).then_some(prompt),
        completion_tokens: (completion > 0).then_some(completion),
        total_tokens: (total > 0).then_some(total),
        cache_read_tokens: (cached_content > 0).then_some(cached_content),
        cache_creation_tokens: None,
        image_tokens: (image_tokens > 0).then_some(image_tokens),
        audio_tokens: (audio_tokens > 0).then_some(audio_tokens),
    };
    (!usage.is_empty()).then_some(usage)
}

fn gemini_prompt_detail_tokens(value: &JsonValue) -> (u64, u64) {
    let mut image = 0u64;
    let mut audio = 0u64;
    for key in ["promptTokensDetails", "toolUsePromptTokensDetails"] {
        let Some(details) = value.get(key).and_then(JsonValue::as_array) else {
            continue;
        };
        for detail in details {
            let modality = detail
                .get("modality")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let tokens = detail
                .get("tokenCount")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0);
            if modality == "image" {
                image = image.saturating_add(tokens);
            } else if modality == "audio" {
                audio = audio.saturating_add(tokens);
            }
        }
    }
    (image, audio)
}

fn json_path_u64(value: &JsonValue, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for segment in path {
        current = current.as_object()?.get(*segment)?;
    }
    current.as_u64()
}

fn json_u64(value: &JsonValue, key: &str) -> Option<u64> {
    value.get(key).and_then(JsonValue::as_u64)
}

fn nonzero_u64(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_openai_usage_from_json() {
        let usage = response_usage_from_json_bytes(
            br#"{"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}"#,
        )
        .unwrap();

        assert_eq!(
            usage,
            ResponseUsage {
                prompt_tokens: Some(11),
                completion_tokens: Some(7),
                total_tokens: Some(18),
                cache_read_tokens: None,
                cache_creation_tokens: None,
                image_tokens: None,
                audio_tokens: None,
            }
        );
    }

    #[test]
    fn extracts_openai_cached_usage_from_json() {
        let usage = response_usage_from_json_bytes(
            br#"{"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,"prompt_tokens_details":{"cached_tokens":4,"cached_creation_tokens":2,"image_tokens":3,"audio_tokens":1}}}"#,
        )
        .unwrap();

        assert_eq!(
            usage,
            ResponseUsage {
                prompt_tokens: Some(11),
                completion_tokens: Some(7),
                total_tokens: Some(18),
                cache_read_tokens: Some(4),
                cache_creation_tokens: Some(2),
                image_tokens: Some(3),
                audio_tokens: Some(1),
            }
        );
    }

    #[test]
    fn extracts_responses_style_usage_from_json() {
        let usage =
            response_usage_from_json_bytes(br#"{"usage":{"input_tokens":13,"output_tokens":5}}"#)
                .unwrap();

        assert_eq!(
            usage,
            ResponseUsage {
                prompt_tokens: Some(13),
                completion_tokens: Some(5),
                total_tokens: Some(18),
                cache_read_tokens: None,
                cache_creation_tokens: None,
                image_tokens: None,
                audio_tokens: None,
            }
        );
    }

    #[test]
    fn extracts_claude_usage_with_cache_tokens_from_json() {
        let usage = response_usage_from_json_bytes(
            br#"{"usage":{"input_tokens":10,"cache_creation_input_tokens":3,"cache_read_input_tokens":2,"output_tokens":8}}"#,
        )
        .unwrap();

        assert_eq!(
            usage,
            ResponseUsage {
                prompt_tokens: Some(15),
                completion_tokens: Some(8),
                total_tokens: Some(23),
                cache_read_tokens: Some(2),
                cache_creation_tokens: Some(3),
                image_tokens: None,
                audio_tokens: None,
            }
        );
    }

    #[test]
    fn extracts_gemini_usage_metadata_from_json() {
        let usage = response_usage_from_json_bytes(
            br#"{"usageMetadata":{"promptTokenCount":9,"cachedContentTokenCount":4,"candidatesTokenCount":6,"thoughtsTokenCount":2,"totalTokenCount":30,"promptTokensDetails":[{"modality":"IMAGE","tokenCount":3},{"modality":"AUDIO","tokenCount":2}]}}"#,
        )
        .unwrap();

        assert_eq!(
            usage,
            ResponseUsage {
                prompt_tokens: Some(9),
                completion_tokens: Some(8),
                total_tokens: Some(30),
                cache_read_tokens: Some(4),
                cache_creation_tokens: None,
                image_tokens: Some(3),
                audio_tokens: Some(2),
            }
        );
    }

    #[test]
    fn skips_empty_usage_from_json() {
        assert_eq!(
            response_usage_from_json_bytes(br#"{"usage":{"prompt_tokens":0}}"#),
            None
        );
        assert_eq!(
            response_usage_from_json_bytes(br#"{"id":"chatcmpl"}"#),
            None
        );
    }

    #[test]
    fn extracts_responses_api_nested_usage() {
        let usage = response_usage_from_json_bytes(
            br#"{"type":"response.completed","response":{"usage":{"input_tokens":21,"output_tokens":9,"total_tokens":30}}}"#,
        )
        .unwrap();
        assert_eq!(
            usage,
            ResponseUsage {
                prompt_tokens: Some(21),
                completion_tokens: Some(9),
                total_tokens: Some(30),
                cache_read_tokens: None,
                cache_creation_tokens: None,
                image_tokens: None,
                audio_tokens: None,
            }
        );
    }

    #[test]
    fn responses_usage_does_not_double_count_cache_tokens() {
        let usage = response_usage_from_json_bytes(
            br#"{"type":"response.completed","response":{"usage":{"input_tokens":15,"input_tokens_details":{"cached_tokens":2},"cache_creation_input_tokens":3,"output_tokens":5,"total_tokens":20}}}"#,
        )
        .unwrap();

        assert_eq!(
            usage,
            ResponseUsage {
                prompt_tokens: Some(15),
                completion_tokens: Some(5),
                total_tokens: Some(20),
                cache_read_tokens: Some(2),
                cache_creation_tokens: Some(3),
                image_tokens: None,
                audio_tokens: None,
            }
        );
    }

    #[test]
    fn converts_buffered_responses_event_to_chat_completion() {
        let reverse = std::collections::BTreeMap::from([(
            "mcp__forecast".to_string(),
            "mcp__weather__forecast_with_a_very_long_original_name".to_string(),
        )]);
        let payload = br#"{
            "type":"response.completed",
            "response":{
                "id":"resp_1",
                "created_at":1700000000,
                "status":"completed",
                "output":[
                    {"type":"reasoning","summary":[{"type":"summary_text","text":"check"}]},
                    {"type":"message","content":[{"type":"output_text","text":"hello"}]},
                    {"type":"function_call","call_id":"call_1","name":"mcp__forecast","arguments":"{\"q\":1}"}
                ],
                "usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18,"input_tokens_details":{"cached_tokens":3}}
            }
        }"#;

        let value = responses_json_to_openai_chat(payload, "requested-model", &reverse)
            .expect("Responses payload should translate");
        let parsed: openai::ChatCompletionResponse =
            serde_json::from_value(value.clone()).expect("translated JSON should parse as chat");

        assert_eq!(parsed.model, "requested-model");
        assert_eq!(value["choices"][0]["message"]["content"], "hello");
        assert_eq!(value["choices"][0]["message"]["reasoning_content"], "check");
        assert_eq!(
            value["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "mcp__weather__forecast_with_a_very_long_original_name"
        );
        assert_eq!(value["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(value["usage"]["prompt_tokens"], 11);
        assert_eq!(value["usage"]["prompt_tokens_details"]["cached_tokens"], 3);
    }

    #[test]
    fn converts_buffered_chat_completion_to_responses_with_reasoning_and_tools() {
        let payload = br#"{
            "id":"chatcmpl-abc",
            "object":"chat.completion",
            "created":1700000000,
            "model":"upstream-model",
            "choices":[{
                "index":0,
                "message":{
                    "role":"assistant",
                    "content":"hello",
                    "reasoning_content":"inspect first",
                    "reasoning_signature":"encrypted-reasoning",
                    "tool_calls":[{
                        "id":"call_1",
                        "type":"function",
                        "function":{"name":"lookup","arguments":"{\"q\":1}"},
                        "thoughtSignature":"gemini-signature"
                    }]
                },
                "finish_reason":"tool_calls"
            }],
            "usage":{
                "prompt_tokens":11,
                "completion_tokens":7,
                "total_tokens":18,
                "prompt_tokens_details":{"cached_tokens":3},
                "completion_tokens_details":{"reasoning_tokens":4}
            }
        }"#;
        let value = openai_chat_json_to_responses(
            payload,
            "requested-model",
            &ResponsesToolMapping::default(),
        )
        .expect("Chat payload should translate");
        assert_eq!(value["id"], "resp_abc");
        assert_eq!(value["object"], "response");
        assert_eq!(value["model"], "requested-model");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["output"][0]["type"], "reasoning");
        assert_eq!(value["output"][0]["summary"][0]["text"], "inspect first");
        assert_eq!(
            value["output"][0]["encrypted_content"],
            "encrypted-reasoning"
        );
        assert_eq!(value["output"][1]["content"][0]["text"], "hello");
        assert_eq!(value["output"][2]["type"], "function_call");
        assert_eq!(value["output"][2]["call_id"], "call_1");
        assert_eq!(value["output"][2]["thought_signature"], "gemini-signature");
        assert_eq!(value["usage"]["input_tokens"], 11);
        assert_eq!(value["usage"]["input_tokens_details"]["cached_tokens"], 3);
        assert_eq!(
            value["usage"]["output_tokens_details"]["reasoning_tokens"],
            4
        );
    }

    #[test]
    fn translates_chat_sse_to_responses_text_reasoning_tools_and_usage() {
        let mut translator =
            OpenAiChatSseToResponses::new(FastStr::from_static_str("requested-model"));
        let first = translator
            .translate_sse_payload(
                r#"{"id":"chatcmpl-abc","created":1700000000,"choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"think ","content":"hi","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":"}}]},"finish_reason":null}]}"#,
            )
            .expect("first Chat SSE chunk should translate");
        let first: Vec<JsonValue> = first
            .iter()
            .map(|event| serde_json::from_str(event).expect("Responses event should be JSON"))
            .collect();
        assert_eq!(first[0]["type"], "response.created");
        assert!(
            first
                .iter()
                .any(|event| event["type"] == "response.reasoning_summary_text.delta")
        );
        assert!(
            first
                .iter()
                .any(|event| event["type"] == "response.output_text.delta")
        );
        assert!(
            first
                .iter()
                .any(|event| event["type"] == "response.function_call_arguments.delta")
        );

        let second = translator
            .translate_sse_payload(
                r#"{"id":"chatcmpl-abc","created":1700000000,"choices":[{"index":0,"delta":{"reasoning_content":"done","tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":9,"completion_tokens":4,"total_tokens":13}}"#,
            )
            .expect("second Chat SSE chunk should translate");
        assert!(second.iter().any(|event| {
            serde_json::from_str::<JsonValue>(event)
                .is_ok_and(|event| event["type"] == "response.function_call_arguments.delta")
        }));
        let second_values: Vec<JsonValue> = second
            .iter()
            .map(|event| serde_json::from_str(event).unwrap())
            .collect();
        let arguments: String = first
            .iter()
            .chain(&second_values)
            .filter(|event| event["type"] == "response.function_call_arguments.delta")
            .filter_map(|event| event["delta"].as_str())
            .collect();
        assert_eq!(arguments, "{\"q\":1}");
        let completed = translator
            .translate_sse_payload("[DONE]")
            .expect("done marker should complete Responses stream");
        assert_eq!(completed.last().map(String::as_str), Some("[DONE]"));
        let terminal: Vec<JsonValue> = completed[..completed.len() - 1]
            .iter()
            .map(|event| serde_json::from_str(event).expect("terminal event should be JSON"))
            .collect();
        let completed = terminal
            .iter()
            .find(|event| event["type"] == "response.completed")
            .expect("Responses stream should have response.completed");
        assert_eq!(completed["response"]["status"], "completed");
        assert_eq!(
            completed["response"]["output"][0]["summary"][0]["text"],
            "think done"
        );
        assert_eq!(
            completed["response"]["output"][1]["content"][0]["text"],
            "hi"
        );
        assert_eq!(completed["response"]["output"][2]["call_id"], "call_1");
        assert_eq!(completed["response"]["output"][2]["arguments"], "{\"q\":1}");
        assert_eq!(completed["response"]["usage"]["input_tokens"], 9);
        assert_eq!(completed["response"]["usage"]["output_tokens"], 4);
    }

    #[test]
    fn chat_sse_length_finish_emits_response_incomplete() {
        let mut translator =
            OpenAiChatSseToResponses::new(FastStr::from_static_str("requested-model"));
        translator
            .translate_sse_payload(
                r#"{"id":"chatcmpl-abc","created":1700000000,"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":"length"}]}"#,
            )
            .unwrap();
        let terminal = translator.translate_sse_payload("[DONE]").unwrap();
        let event: JsonValue = terminal[..terminal.len() - 1]
            .iter()
            .filter_map(|event| serde_json::from_str(event).ok())
            .find(|event: &JsonValue| event["type"] == "response.incomplete")
            .expect("length termination should emit response.incomplete");
        assert_eq!(event["response"]["status"], "incomplete");
        assert_eq!(
            event["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }

    #[test]
    fn chat_sse_eof_keeps_signature_only_reasoning_and_tool_signature() {
        let mut translator =
            OpenAiChatSseToResponses::new(FastStr::from_static_str("requested-model"));
        translator
            .translate_sse_payload(
                r#"{"id":"chatcmpl-abc","created":1700000000,"choices":[{"index":0,"delta":{"reasoning_signature":"encrypted-reasoning","tool_calls":[{"index":0,"id":"call_1","type":"function","thoughtSignature":"tool-signature","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
            )
            .unwrap();

        let terminal = translator.finish().unwrap();
        assert_eq!(terminal.last().map(String::as_str), Some("[DONE]"));
        let completed = terminal
            .iter()
            .filter_map(|event| serde_json::from_str::<JsonValue>(event).ok())
            .find(|event| event["type"] == "response.completed")
            .unwrap();
        assert_eq!(
            completed["response"]["output"][0]["encrypted_content"],
            "encrypted-reasoning"
        );
        assert_eq!(
            completed["response"]["output"][1]["thought_signature"],
            "tool-signature"
        );
    }

    #[test]
    fn translates_responses_sse_text_tools_reasoning_and_usage() {
        let reverse = std::collections::BTreeMap::from([(
            "short_tool".to_string(),
            "original_tool_name".to_string(),
        )]);
        let mut translator =
            ResponsesSseToOpenAiChat::new(FastStr::from_static_str("requested-model"), reverse);
        let created = translator
            .translate_sse_payload(
                r#"{"type":"response.created","response":{"id":"resp_1","created_at":1700000000}}"#,
            )
            .expect("created event should parse");
        assert_eq!(created.len(), 1);
        let created: JsonValue = serde_json::from_str(&created[0]).unwrap();
        assert_eq!(created["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(created["choices"][0]["delta"]["content"], "");
        assert!(created["choices"][0]["finish_reason"].is_null());
        assert_eq!(created["id"], "resp_1");
        assert_eq!(created["model"], "requested-model");
        assert_eq!(created["created"], 1700000000);

        let text = translator
            .translate_sse_payload(r#"{"type":"response.output_text.delta","delta":"hi"}"#)
            .expect("text event should parse");
        assert!(text.iter().any(|chunk| {
            serde_json::from_str::<JsonValue>(chunk)
                .is_ok_and(|chunk| chunk["choices"][0]["delta"]["content"] == "hi")
        }));

        let first_tool = translator
            .translate_sse_payload(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"short_tool","arguments":""}}"#,
            )
            .expect("tool event should parse");
        let first_tool: JsonValue =
            serde_json::from_str(&first_tool[0]).expect("tool chunk should be JSON");
        assert_eq!(
            first_tool["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "original_tool_name"
        );

        let completed = translator
            .translate_sse_payload(
                r#"{"type":"response.completed","response":{"id":"resp_1","created_at":1700000000,"output":[{"type":"function_call","call_id":"call_1","name":"short_tool","arguments":"{}"},{"type":"function_call","call_id":"call_2","name":"other_tool","arguments":"{\"x\":1}"},{"type":"reasoning","summary":[{"type":"summary_text","text":"thought"}]}],"usage":{"input_tokens":9,"output_tokens":4,"total_tokens":13}}}"#,
            )
            .expect("completed event should parse");

        assert_eq!(completed.last().map(String::as_str), Some("[DONE]"));
        let parsed: Vec<JsonValue> = completed[..completed.len() - 1]
            .iter()
            .map(|chunk| serde_json::from_str(chunk).expect("chat chunk should be JSON"))
            .collect();
        assert!(
            parsed
                .iter()
                .any(|chunk| { chunk["choices"][0]["delta"]["tool_calls"][0]["id"] == "call_2" })
        );
        assert!(
            parsed
                .iter()
                .any(|chunk| { chunk["choices"][0]["delta"]["reasoning_content"] == "thought" })
        );
        let final_chunk = parsed
            .iter()
            .find(|chunk| chunk["choices"][0]["finish_reason"] == "tool_calls")
            .expect("finish chunk should exist");
        assert_eq!(final_chunk["choices"][0]["finish_reason"], "tool_calls");
        let usage_chunk = parsed
            .iter()
            .find(|chunk| chunk["usage"].is_object())
            .expect("usage should be present before DONE");
        assert_eq!(usage_chunk["usage"]["prompt_tokens"], 9);
        assert_eq!(usage_chunk["usage"]["completion_tokens"], 4);
    }

    #[test]
    fn responses_sse_incomplete_maps_reason_and_failed_is_idempotent() {
        let mut incomplete = ResponsesSseToOpenAiChat::new(
            FastStr::from_static_str("requested-model"),
            Default::default(),
        );
        let terminal = incomplete
            .translate_sse_payload(
                r#"{"type":"response.incomplete","response":{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#,
            )
            .unwrap();
        let events: Vec<JsonValue> = terminal
            .iter()
            .filter_map(|event| serde_json::from_str(event).ok())
            .collect();
        let finish = events
            .iter()
            .find(|event| event["choices"][0]["finish_reason"] == "length")
            .expect("incomplete stream should finish with length");
        assert_eq!(finish["choices"][0]["finish_reason"], "length");
        assert!(
            events
                .iter()
                .any(|event| event["usage"]["total_tokens"] == 5)
        );
        assert_eq!(terminal.last().map(String::as_str), Some("[DONE]"));
        assert!(incomplete.finish(None).unwrap().is_empty());

        let mut failed = ResponsesSseToOpenAiChat::new(
            FastStr::from_static_str("requested-model"),
            Default::default(),
        );
        let terminal = failed
            .translate_sse_payload(
                r#"{"type":"response.failed","response":{"error":{"message":"upstream failed"}}}"#,
            )
            .unwrap();
        assert!(terminal.iter().any(|event| {
            serde_json::from_str::<JsonValue>(event).is_ok_and(|event| event.get("error").is_some())
        }));
        assert!(!terminal.iter().any(|event| event == "[DONE]"));
        assert!(!terminal.iter().any(|event| {
            serde_json::from_str::<JsonValue>(event)
                .is_ok_and(|event| event["choices"][0]["finish_reason"] == "stop")
        }));
        assert!(failed.finish(None).unwrap().is_empty());
    }

    fn responses_mapping(payload: &serde_json::Value) -> ResponsesToolMapping {
        halolake_protocol::responses_tool_mapping(&serde_json::to_vec(payload).unwrap()).unwrap()
    }

    #[test]
    fn chat_sse_custom_tool_emits_full_lifecycle_and_skips_function_arguments() {
        let mapping = responses_mapping(&serde_json::json!({
            "tools":[{"type":"custom","name":"exec"}],
            "input":"hi"
        }));
        let mut translator =
            OpenAiChatSseToResponses::with_tool_mapping(FastStr::from_static_str("model"), mapping);
        let mut all_events: Vec<JsonValue> = Vec::new();
        for chunk in [
            r#"{"id":"chatcmpl-abc","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"exec","arguments":"{\"input\":"}}]},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-abc","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ] {
            for event in translator.translate_sse_payload(chunk).unwrap() {
                if event != "[DONE]" {
                    all_events.push(serde_json::from_str(&event).unwrap());
                }
            }
        }
        for event in translator.translate_sse_payload("[DONE]").unwrap() {
            if event != "[DONE]" {
                all_events.push(serde_json::from_str(&event).unwrap());
            }
        }
        let added = all_events
            .iter()
            .find(|event| event["type"] == "response.output_item.added")
            .unwrap();
        assert_eq!(added["item"]["type"], "custom_tool_call");
        let done = all_events
            .iter()
            .find(|event| event["type"] == "response.custom_tool_call_input.done")
            .unwrap();
        assert_eq!(done["input"], "ls");
        assert_eq!(done["name"], "exec");
        assert!(
            !all_events
                .iter()
                .any(|event| event["type"] == "response.function_call_arguments.done")
        );
        let item_done = all_events
            .iter()
            .find(|event| event["type"] == "response.output_item.done")
            .unwrap();
        assert_eq!(item_done["item"]["type"], "custom_tool_call");
        assert_eq!(item_done["item"]["input"], "ls");
    }

    #[test]
    fn chat_sse_tool_search_emits_only_output_item_done_with_client_execution() {
        let mapping = responses_mapping(&serde_json::json!({
            "tools":[{"type":"tool_search"}],
            "input":"hi"
        }));
        let mut translator =
            OpenAiChatSseToResponses::with_tool_mapping(FastStr::from_static_str("model"), mapping);
        let mut all_events: Vec<JsonValue> = Vec::new();
        for chunk in [
            r#"{"id":"chatcmpl-abc","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"tool_search","arguments":"{\"query\":"}}]},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-abc","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"files\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ] {
            for event in translator.translate_sse_payload(chunk).unwrap() {
                if event != "[DONE]" {
                    all_events.push(serde_json::from_str(&event).unwrap());
                }
            }
        }
        for event in translator.translate_sse_payload("[DONE]").unwrap() {
            if event != "[DONE]" {
                all_events.push(serde_json::from_str(&event).unwrap());
            }
        }
        let added = all_events
            .iter()
            .find(|event| event["type"] == "response.output_item.added")
            .unwrap();
        assert_eq!(added["item"]["type"], "tool_search_call");
        assert_eq!(added["item"]["execution"], "client");
        let item_done = all_events
            .iter()
            .find(|event| event["type"] == "response.output_item.done")
            .unwrap();
        assert_eq!(item_done["item"]["type"], "tool_search_call");
        assert_eq!(item_done["item"]["execution"], "client");
        assert_eq!(item_done["item"]["arguments"]["query"], "files");
        assert!(
            !all_events
                .iter()
                .any(|event| event["type"] == "response.function_call_arguments.done")
        );
    }

    #[test]
    fn chat_sse_late_tool_name_defers_announcement_and_backfills_arguments() {
        let mapping = responses_mapping(&serde_json::json!({
            "tools":[{"type":"custom","name":"exec"}],
            "input":"hi"
        }));
        let mut translator =
            OpenAiChatSseToResponses::with_tool_mapping(FastStr::from_static_str("model"), mapping);
        // First chunk: id + arguments arrive, but name is empty.
        let first = translator
            .translate_sse_payload(
                r#"{"id":"chatcmpl-abc","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"arguments":"{\"input\":"}}]},"finish_reason":null}]}"#,
            )
            .unwrap();
        let first_events: Vec<JsonValue> = first
            .iter()
            .map(|event| serde_json::from_str(event).unwrap())
            .collect();
        assert!(
            !first_events
                .iter()
                .any(|event| event["type"] == "response.output_item.added")
        );
        // Second chunk: name arrives, announcement happens with correct type.
        let second = translator
            .translate_sse_payload(
                r#"{"id":"chatcmpl-abc","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"exec","arguments":"\"ls\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            )
            .unwrap();
        let second_events: Vec<JsonValue> = second
            .iter()
            .map(|event| serde_json::from_str(event).unwrap())
            .collect();
        let added = second_events
            .iter()
            .find(|event| event["type"] == "response.output_item.added")
            .expect("announcement deferred until name arrives");
        assert_eq!(added["item"]["type"], "custom_tool_call");
    }

    #[test]
    fn chat_sse_namespace_tool_restores_name_and_namespace_on_added_and_done() {
        let mapping = responses_mapping(&serde_json::json!({
            "tools":[{"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
            ]}],
            "input":"hi"
        }));
        let mut translator =
            OpenAiChatSseToResponses::with_tool_mapping(FastStr::from_static_str("model"), mapping);
        let mut all_events: Vec<JsonValue> = Vec::new();
        for chunk in [
            r#"{"id":"chatcmpl-abc","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"collaboration__spawn_agent","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        ] {
            for event in translator.translate_sse_payload(chunk).unwrap() {
                if event != "[DONE]" {
                    all_events.push(serde_json::from_str(&event).unwrap());
                }
            }
        }
        for event in translator.translate_sse_payload("[DONE]").unwrap() {
            if event != "[DONE]" {
                all_events.push(serde_json::from_str(&event).unwrap());
            }
        }
        let added = all_events
            .iter()
            .find(|event| event["type"] == "response.output_item.added")
            .unwrap();
        assert_eq!(added["item"]["type"], "function_call");
        assert_eq!(added["item"]["name"], "spawn_agent");
        assert_eq!(added["item"]["namespace"], "collaboration");
        let item_done = all_events
            .iter()
            .find(|event| event["type"] == "response.output_item.done")
            .unwrap();
        assert_eq!(item_done["item"]["name"], "spawn_agent");
        assert_eq!(item_done["item"]["namespace"], "collaboration");
        let args_done = all_events
            .iter()
            .find(|event| event["type"] == "response.function_call_arguments.done")
            .unwrap();
        assert_eq!(args_done["name"], "spawn_agent");
    }

    #[test]
    fn relaykit_restores_buffered_custom_tool_input() {
        let mapping = responses_mapping(&serde_json::json!({
            "tools":[{"type":"custom","name":"exec"}],
            "input":"hi"
        }));
        let input = serde_json::json!({
            "id":"chatcmpl-custom",
            "choices":[{
                "index":0,
                "message":{"role":"assistant","tool_calls":[{
                    "id":"call_1","type":"function",
                    "function":{"name":"exec","arguments":"{\"input\":\"dir\"}"}
                }]},
                "finish_reason":"tool_calls"
            }]
        });
        let output = openai_chat_json_to_responses(
            &serde_json::to_vec(&input).unwrap(),
            "requested",
            &mapping,
        )
        .unwrap();
        assert_eq!(output["output"][0]["type"], "custom_tool_call");
        assert_eq!(output["output"][0]["input"], "dir");
    }

    #[test]
    fn relaykit_preserves_buffered_chat_refusal_content() {
        let input = serde_json::json!({
            "id":"chatcmpl-refusal","model":"upstream",
            "choices":[{
                "index":0,
                "message":{"role":"assistant","content":null,"refusal":"I cannot help."},
                "finish_reason":"stop"
            }]
        });
        let output = openai_chat_json_to_responses(
            &serde_json::to_vec(&input).unwrap(),
            "requested",
            &Default::default(),
        )
        .unwrap();
        let refusal = output["output"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|item| item["content"].as_array().into_iter().flatten())
            .find(|part| part["type"] == "refusal")
            .expect("buffered refusal must not become an empty successful response");
        assert_eq!(refusal["refusal"], "I cannot help.");
    }

    #[test]
    fn responses_sse_emits_reasoning_signature_delta_before_terminal() {
        let mut translator = ResponsesSseToOpenAiChat::new(
            FastStr::from_static_str("requested-model"),
            Default::default(),
        );
        translator
            .translate_sse_payload(
                r#"{"type":"response.created","response":{"id":"resp_1","created_at":1700000000}}"#,
            )
            .unwrap();
        translator
            .translate_sse_payload(
                r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
            )
            .unwrap();
        let sig = translator
            .translate_sse_payload(
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_0","summary":[{"type":"summary_text","text":"thinking"}],"encrypted_content":"cipher"}}"#,
            )
            .unwrap();
        let sig_chunk: JsonValue = serde_json::from_str(&sig[0]).unwrap();
        assert_eq!(
            sig_chunk["choices"][0]["delta"]["reasoning_signature"],
            "cipher"
        );
        let terminal = translator
            .translate_sse_payload(
                r#"{"type":"response.completed","response":{"id":"resp_1","created_at":1700000000,"output":[{"type":"reasoning","id":"rs_0","summary":[{"type":"summary_text","text":"thinking"}],"encrypted_content":"cipher"}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#,
            )
            .unwrap();
        let parsed: Vec<JsonValue> = terminal[..terminal.len() - 1]
            .iter()
            .map(|c| serde_json::from_str(c).unwrap())
            .collect();
        let final_chunk = parsed
            .iter()
            .find(|event| event["choices"][0]["finish_reason"] == "stop")
            .unwrap();
        assert_eq!(final_chunk["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn responses_sse_redacted_reasoning_with_only_signature_emits_signature_delta() {
        let mut translator = ResponsesSseToOpenAiChat::new(
            FastStr::from_static_str("requested-model"),
            Default::default(),
        );
        translator
            .translate_sse_payload(
                r#"{"type":"response.created","response":{"id":"resp_1","created_at":1700000000}}"#,
            )
            .unwrap();
        let sig = translator
            .translate_sse_payload(
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_0","summary":[],"encrypted_content":"redacted-sig"}}"#,
            )
            .unwrap();
        let sig_chunk: JsonValue = serde_json::from_str(&sig[0]).unwrap();
        assert_eq!(
            sig_chunk["choices"][0]["delta"]["reasoning_signature"],
            "redacted-sig"
        );
    }

    #[test]
    fn relaykit_keeps_argument_only_chat_deltas_in_claude_tools() {
        let mut translator = RelaySseTranslator::new(
            RelayFormat::OpenAi,
            RelayFormat::Claude,
            "requested",
            Default::default(),
        );
        let mut events = Vec::new();
        for input in [
            r#"{"id":"chatcmpl-1","model":"upstream","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"weather","arguments":""}}]},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","model":"upstream","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","model":"upstream","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ] {
            events.extend(translator.translate_sse_payload(input).unwrap());
        }
        let arguments: String = events
            .iter()
            .filter(|event| event["delta"]["type"] == "input_json_delta")
            .filter_map(|event| event["delta"]["partial_json"].as_str())
            .collect();
        assert_eq!(arguments, "{\"city\":\"Paris\"}");
        assert!(events.iter().any(|event| {
            event["type"] == "message_delta" && event["delta"]["stop_reason"] == "tool_use"
        }));
    }

    #[test]
    fn relaykit_keeps_custom_tool_input_over_the_full_responses_lifecycle() {
        let mut translator = ResponsesSseToOpenAiChat::new(
            FastStr::from_static_str("requested"),
            Default::default(),
        );
        let mut chunks = Vec::new();
        for input in [
            serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","name":"exec","input":""}}),
            serde_json::json!({"type":"response.custom_tool_call_input.delta","output_index":0,"item_id":"ctc_1","delta":"hello"}),
            serde_json::json!({"type":"response.custom_tool_call_input.done","output_index":0,"item_id":"ctc_1","input":"hello"}),
            serde_json::json!({"type":"response.output_item.done","output_index":0,"item":{"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","name":"exec","input":"hello"}}),
            serde_json::json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","name":"exec","input":"hello"}]}}),
        ] {
            chunks.extend(
                translator
                    .translate_sse_payload(&input.to_string())
                    .unwrap(),
            );
        }
        let arguments: String = chunks
            .iter()
            .filter_map(|chunk| serde_json::from_str::<JsonValue>(chunk).ok())
            .filter_map(|chunk| {
                chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(arguments, "hello");
        assert_eq!(chunks.last().map(String::as_str), Some("[DONE]"));
    }

    #[test]
    fn relaykit_buffered_incomplete_status_precedes_tool_call_finish() {
        for (details, output, expected) in [
            ("content_filter", serde_json::json!([]), "content_filter"),
            (
                "max_output_tokens",
                serde_json::json!([{
                    "type":"function_call","call_id":"call_1",
                    "name":"exec","arguments":"{\"x\":"
                }]),
                "length",
            ),
        ] {
            let input = serde_json::json!({
                "id":"resp_1","status":"incomplete",
                "incomplete_details":{"reason":details},
                "output":output
            });
            let result = responses_json_to_openai_chat(
                &serde_json::to_vec(&input).unwrap(),
                "requested",
                &Default::default(),
            )
            .unwrap();
            assert_eq!(result["choices"][0]["finish_reason"], expected);
        }
    }

    #[test]
    fn premature_claude_eof_does_not_emit_a_completed_response() {
        let mut translator = RelaySseTranslator::new(
            RelayFormat::Claude,
            RelayFormat::OpenAiResponses,
            "requested",
            Default::default(),
        );
        for input in [
            serde_json::json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial answer"}}),
        ] {
            translator
                .translate_sse_payload(&input.to_string())
                .unwrap();
        }
        let terminal = translator.finish().unwrap();
        assert!(
            terminal
                .iter()
                .any(|event| event["type"] == "response.failed")
        );
        assert!(
            !terminal
                .iter()
                .any(|event| event["type"] == "response.completed")
        );
        assert!(translator.is_failed());
    }

    #[monoio::test_all(enable_timer = true)]
    async fn protocol_transport_failure_emits_failure_without_success_framing() {
        use futures_util::StreamExt;

        for (from, to, inputs) in [
            (
                RelayFormat::Claude,
                RelayFormat::OpenAiResponses,
                vec![
                    serde_json::json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
                    serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"exec","input":{}}}),
                    serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}),
                ],
            ),
            (
                RelayFormat::OpenAi,
                RelayFormat::Claude,
                vec![
                    serde_json::json!({"id":"chatcmpl-1","model":"upstream","choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":"stop"}]}),
                ],
            ),
        ] {
            let (payload, mut upstream_sender) = stream_payload_pair::<Bytes, HttpError>();
            for input in inputs {
                upstream_sender.feed_data(Some(Bytes::from(format!("data: {input}\n\n"))));
            }
            upstream_sender.feed_error(
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "simulated upstream failure")
                    .into(),
            );
            let upstream = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(HttpBody::H1(Payload::Stream(payload)))
                .unwrap();
            let response = stream_protocol_response(
                upstream,
                from,
                to,
                FastStr::from_static_str("requested"),
                Default::default(),
                Default::default(),
                true,
            );
            let mut body = HttpBodyStream::from(response.into_body());
            let mut decoder = SseBuffer::default();
            let mut payloads = Vec::new();
            while let Some(chunk) = body.next().await {
                payloads.extend(decoder.push_with_done(
                    &chunk.expect("transport failure must be rendered as protocol SSE"),
                    true,
                ));
            }
            assert!(payloads.iter().all(|payload| payload != "[DONE]"));
            let events: Vec<JsonValue> = payloads
                .iter()
                .map(|payload| serde_json::from_str(payload).unwrap())
                .collect();
            assert!(events.iter().any(|event| event["type"] == "error"));
            assert!(!events.iter().any(|event| {
                matches!(
                    event["type"].as_str(),
                    Some(
                        "response.completed"
                            | "response.output_item.done"
                            | "response.function_call_arguments.done"
                            | "message_stop"
                    )
                )
            }));
            if to == RelayFormat::OpenAiResponses {
                assert!(
                    events
                        .iter()
                        .any(|event| event["type"] == "response.failed")
                );
            }
        }
    }

    #[test]
    fn protocol_response_headers_keep_request_ids_and_override_framing() {
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("upstream-1"));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("42"));
        let response = protocol_response_builder(StatusCode::OK, &headers, true)
            .body(full_body(Bytes::new()))
            .unwrap();
        assert_eq!(response.headers()["x-request-id"], "upstream-1");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
    }

    #[test]
    fn claude_sse_reasoning_summary_part_ordering_is_correct() {
        let mut translator = ClaudeSseToResponsesTranslator::new("claude");
        let created = translator
            .translate_sse_payload(
                &serde_json::json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}).to_string(),
            )
            .unwrap();
        assert!(created.iter().any(|e| e["type"] == "response.created"));
        let block_start = translator
            .translate_sse_payload(
                &serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":null}}).to_string(),
            )
            .unwrap();
        let added = block_start
            .iter()
            .find(|e| e["type"] == "response.output_item.added")
            .unwrap();
        assert_eq!(added["item"]["type"], "reasoning");
        let part_added = block_start
            .iter()
            .find(|e| e["type"] == "response.reasoning_summary_part.added")
            .unwrap();
        assert_eq!(part_added["part"]["type"], "summary_text");
        let delta = translator
            .translate_sse_payload(
                &serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"why"}}).to_string(),
            )
            .unwrap();
        let text_delta = delta
            .iter()
            .find(|e| e["type"] == "response.reasoning_summary_text.delta")
            .unwrap();
        assert_eq!(text_delta["delta"], "why");
        let stop = translator
            .translate_sse_payload(
                &serde_json::json!({"type":"content_block_stop","index":0}).to_string(),
            )
            .unwrap();
        let order: Vec<&str> = stop
            .iter()
            .map(|e| e["type"].as_str().unwrap_or(""))
            .collect();
        let text_done_pos = order
            .iter()
            .position(|t| *t == "response.reasoning_summary_text.done")
            .unwrap();
        let part_done_pos = order
            .iter()
            .position(|t| *t == "response.reasoning_summary_part.done")
            .unwrap();
        let item_done_pos = order
            .iter()
            .position(|t| *t == "response.output_item.done")
            .unwrap();
        assert!(text_done_pos < part_done_pos);
        assert!(part_done_pos < item_done_pos);
    }

    #[test]
    fn include_encrypted_reasoning_controls_buffered_and_sse_output() {
        let response = claude::MessagesResponse {
            id: "msg_1".to_string(),
            response_type: "message".to_string(),
            role: "assistant".to_string(),
            model: "claude".to_string(),
            content: vec![
                claude::ContentBlock::Thinking {
                    thinking: "why".to_string(),
                    signature: Some("cipher".to_string()),
                    cache_control: None,
                },
                claude::ContentBlock::RedactedThinking {
                    data: "redacted".to_string(),
                    cache_control: None,
                },
            ],
            stop_reason: Some("end_turn".to_string()),
            usage: None,
        };
        let with = claude_messages_to_openai_responses_with_options_and_tools(
            response.clone(),
            "requested",
            true,
            &ResponsesToolMapping::default(),
        );
        assert_eq!(with["output"][0]["encrypted_content"], "cipher");
        assert_eq!(with["output"][1]["encrypted_content"], "redacted");

        let without = claude_messages_to_openai_responses_with_options_and_tools(
            response,
            "requested",
            false,
            &ResponsesToolMapping::default(),
        );
        assert!(without["output"][0].get("encrypted_content").is_none());
        assert!(without["output"][1].get("encrypted_content").is_none());
    }
}
