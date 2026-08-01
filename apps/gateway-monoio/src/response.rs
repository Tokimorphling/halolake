use super::*;

#[derive(Debug, Clone)]
pub(crate) struct ChannelFeedbackMeta {
    pub(crate) status_code: Option<u16>,
    pub(crate) reason:      ChannelFeedbackReason,
    pub(crate) message:     String,
}

impl ChannelFeedbackMeta {
    fn upstream_status(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status_code: Some(status.as_u16()),
            reason:      ChannelFeedbackReason::UpstreamStatus,
            message:     message.into(),
        }
    }

    pub(crate) fn transport(message: impl Into<String>) -> Self {
        Self {
            status_code: None,
            reason:      ChannelFeedbackReason::Transport,
            message:     message.into(),
        }
    }
}

fn attach_channel_feedback(resp: &mut Response<GatewayBody>, meta: ChannelFeedbackMeta) {
    resp.extensions_mut().insert(meta);
}

pub(crate) async fn buffered_claude_as_openai(
    mut upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading Claude response: {err}"),
            );
        }
    };
    let claude_resp: claude::MessagesResponse = match serde_json::from_slice(&payload) {
        Ok(resp) => resp,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid Claude response: {err}"),
            );
        }
    };
    let usage = claude_resp.usage.map(ResponseUsage::from_claude);
    let mut resp = json_response(
        StatusCode::OK,
        claude_messages_to_openai_chat(claude_resp, requested_model),
    );
    attach_response_usage(&mut resp, usage);
    resp
}

pub(crate) fn stream_claude_as_openai(
    upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let body = stream_body_from_async(move |mut sender| async move {
        use futures_util::StreamExt;

        let mut stream = HttpBodyStream::from(upstream.into_body());
        let mut translator = ClaudeSseTranslator::new(requested_model);
        let mut decoder = SseBuffer::default();
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => {
                    let mut out = Vec::new();
                    for payload in decoder.push(&bytes) {
                        match translator.translate_sse_payload(&payload) {
                            Ok(events) => {
                                for event in events {
                                    write_sse_data(&mut out, &event);
                                }
                            }
                            Err(err) => warn!(?err, "failed to translate Claude SSE event"),
                        }
                    }
                    if !out.is_empty() {
                        sender.feed_data(Some(Bytes::from(out)));
                    }
                }
                Err(err) => {
                    sender.feed_error(err);
                    return;
                }
            }
        }
        let mut out = Vec::new();
        match translator.finish() {
            Ok(events) => {
                for event in events {
                    write_sse_data(&mut out, &event);
                }
            }
            Err(err) => warn!(?err, "failed to finalize Claude SSE translation"),
        }
        if !out.is_empty() {
            sender.feed_data(Some(Bytes::from(out)));
        }
        sender.feed_data(None);
    });

    response_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        })
}

pub(crate) async fn buffered_openai_as_claude(
    mut upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading OpenAI response: {err}"),
            );
        }
    };
    let openai_resp: openai::ChatCompletionResponse = match serde_json::from_slice(&payload) {
        Ok(resp) => resp,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid OpenAI response: {err}"),
            );
        }
    };
    let usage = openai_resp.usage.map(ResponseUsage::from_openai);
    let mut resp = json_response(
        StatusCode::OK,
        openai_chat_to_claude_messages_response(openai_resp, requested_model),
    );
    attach_response_usage(&mut resp, usage);
    resp
}

pub(crate) fn stream_openai_as_claude(
    upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let body = stream_body_from_async(move |mut sender| async move {
        use futures_util::StreamExt;

        let mut stream = HttpBodyStream::from(upstream.into_body());
        let mut translator = OpenAiSseToClaudeTranslator::new(requested_model);
        let mut decoder = SseBuffer::default();
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => {
                    let mut out = Vec::new();
                    for payload in decoder.push(&bytes) {
                        match translator.translate_sse_payload(&payload) {
                            Ok(events) => {
                                for event in events {
                                    write_sse_data(&mut out, &event);
                                }
                            }
                            Err(err) => {
                                warn!(?err, "failed to translate OpenAI->Claude SSE event")
                            }
                        }
                    }
                    if !out.is_empty() {
                        sender.feed_data(Some(Bytes::from(out)));
                    }
                }
                Err(err) => {
                    sender.feed_error(err);
                    return;
                }
            }
        }
        let mut out = Vec::new();
        match translator.finish() {
            Ok(events) => {
                for event in events {
                    write_sse_data(&mut out, &event);
                }
            }
            Err(err) => warn!(?err, "failed to finalize OpenAI->Claude SSE translation"),
        }
        if !out.is_empty() {
            sender.feed_data(Some(Bytes::from(out)));
        }
        sender.feed_data(None);
    });

    response_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        })
}

pub(crate) async fn buffered_responses_as_openai_chat(
    mut upstream: Response<HttpBody>,
    requested_model: String,
    tool_name_reverse: std::collections::BTreeMap<String, String>,
) -> Response<GatewayBody> {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return upstream_transport_error_response(&format!(
                "failed reading OpenAI Responses response: {err}"
            ));
        }
    };
    let translated =
        match responses_json_to_openai_chat(&payload, &requested_model, &tool_name_reverse) {
            Ok(value) => value,
            Err(err) => {
                return upstream_transport_error_response(&format!(
                    "invalid OpenAI Responses response: {err}"
                ));
            }
        };
    let usage = response_usage_from_json_bytes(&payload);
    let body = match serde_json::to_vec(&translated) {
        Ok(body) => body,
        Err(err) => {
            return upstream_transport_error_response(&format!(
                "failed serializing translated OpenAI response: {err}"
            ));
        }
    };
    let mut builder = response_builder(status).header(header::CONTENT_TYPE, "application/json");
    for (name, value) in &headers {
        if name != header::CONTENT_TYPE && is_forward_response_header(name) {
            builder = builder.header(name, value);
        }
    }
    let mut resp = builder.body(full_body(body)).unwrap_or_else(|err| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &err.to_string(),
        )
    });
    attach_response_usage(&mut resp, usage);
    resp
}

pub(crate) fn stream_responses_as_openai_chat(
    upstream: Response<HttpBody>,
    requested_model: String,
    tool_name_reverse: std::collections::BTreeMap<String, String>,
) -> Response<GatewayBody> {
    let (parts, upstream_body) = upstream.into_parts();
    let mut translator = ResponsesSseToOpenAiChat::new(requested_model, tool_name_reverse);
    let mut decoder = SseBuffer::default();
    let body = stream_body_from_async(move |sender| {
        let stream = HttpBodyStream::from(upstream_body);
        pump_http_body_stream(stream, sender, move |bytes| {
            let mut out = Vec::new();
            for payload in decoder.push_with_done(&bytes, true) {
                match translator.translate_sse_payload(&payload) {
                    Ok(events) => {
                        for event in events {
                            write_sse_data(&mut out, &event);
                        }
                    }
                    Err(err) => {
                        warn!(?err, "failed to translate OpenAI Responses SSE event");
                    }
                }
            }
            Bytes::from(out)
        })
    });

    let mut builder = response_builder(parts.status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive");
    for (name, value) in &parts.headers {
        if name != header::CONTENT_TYPE
            && name != header::CACHE_CONTROL
            && name != header::CONNECTION
            && is_forward_response_header(name)
        {
            builder = builder.header(name, value);
        }
    }
    builder.body(body).unwrap_or_else(|err| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &err.to_string(),
        )
    })
}

pub(crate) async fn buffered_openai_chat_as_responses(
    mut upstream: Response<HttpBody>,
    requested_model: String,
    tool_mapping: ResponsesToolMapping,
) -> Response<GatewayBody> {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return upstream_transport_error_response(&format!(
                "failed reading OpenAI Chat Completions response: {err}"
            ));
        }
    };
    let translated = match openai_chat_json_to_responses(&payload, &requested_model, &tool_mapping)
    {
        Ok(value) => value,
        Err(err) => {
            return upstream_transport_error_response(&format!(
                "invalid OpenAI Chat Completions response: {err}"
            ));
        }
    };
    let usage = response_usage_from_json_bytes(&payload);
    let body = match serde_json::to_vec(&translated) {
        Ok(body) => body,
        Err(err) => {
            return upstream_transport_error_response(&format!(
                "failed serializing translated Responses response: {err}"
            ));
        }
    };
    let mut builder = response_builder(status).header(header::CONTENT_TYPE, "application/json");
    for (name, value) in &headers {
        if name != header::CONTENT_TYPE && is_forward_response_header(name) {
            builder = builder.header(name, value);
        }
    }
    let mut resp = builder.body(full_body(body)).unwrap_or_else(|err| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &err.to_string(),
        )
    });
    attach_response_usage(&mut resp, usage);
    resp
}

pub(crate) fn stream_openai_chat_as_responses(
    upstream: Response<HttpBody>,
    requested_model: String,
    tool_mapping: ResponsesToolMapping,
) -> Response<GatewayBody> {
    let (parts, upstream_body) = upstream.into_parts();
    let body = stream_body_from_async(move |mut sender| async move {
        use futures_util::StreamExt;

        let mut stream = HttpBodyStream::from(upstream_body);
        let mut translator =
            OpenAiChatSseToResponses::with_tool_mapping(requested_model, tool_mapping);
        let mut decoder = SseBuffer::default();
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => {
                    let mut out = Vec::new();
                    for payload in decoder.push_with_done(&bytes, true) {
                        match translator.translate_sse_payload(&payload) {
                            Ok(events) => {
                                for event in events {
                                    write_sse_data(&mut out, &event);
                                }
                            }
                            Err(err) => warn!(
                                ?err,
                                "failed to translate OpenAI Chat SSE to Responses event"
                            ),
                        }
                    }
                    if !out.is_empty() {
                        sender.feed_data(Some(Bytes::from(out)));
                    }
                }
                Err(err) => {
                    sender.feed_error(err);
                    return;
                }
            }
        }

        // Some OpenAI-compatible servers close after finish_reason without a
        // literal [DONE]. Always finalize the typed Responses lifecycle at EOF.
        let mut out = Vec::new();
        match translator.finish() {
            Ok(events) => {
                for event in events {
                    write_sse_data(&mut out, &event);
                }
            }
            Err(err) => warn!(?err, "failed to finalize Chat to Responses SSE stream"),
        }
        if !out.is_empty() {
            sender.feed_data(Some(Bytes::from(out)));
        }
        sender.feed_data(None);
    });

    let mut builder = response_builder(parts.status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive");
    for (name, value) in &parts.headers {
        if name != header::CONTENT_TYPE
            && name != header::CACHE_CONTROL
            && name != header::CONNECTION
            && is_forward_response_header(name)
        {
            builder = builder.header(name, value);
        }
    }
    builder.body(body).unwrap_or_else(|err| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &err.to_string(),
        )
    })
}

fn openai_chat_json_to_responses(
    payload: &[u8],
    requested_model: &str,
    tool_mapping: &ResponsesToolMapping,
) -> Result<JsonValue> {
    let chat: JsonValue =
        serde_json::from_slice(payload).context("parse OpenAI Chat Completions response body")?;
    let choice = chat
        .get("choices")
        .and_then(JsonValue::as_array)
        .and_then(|choices| choices.first())
        .context("Chat Completions response has no choices")?;
    let message = choice
        .get("message")
        .context("Chat Completions choice has no message")?;
    let response_id = chat_response_id(&chat);
    let mut output = Vec::new();

    let reasoning = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    let reasoning_signature = message
        .get("reasoning_signature")
        .or_else(|| message.get("reasoningSignature"))
        .and_then(JsonValue::as_str);
    if !reasoning.is_empty() || reasoning_signature.is_some() {
        let mut item = serde_json::json!({
            "id": "rs_0",
            "type": "reasoning",
            "status": "completed",
            "summary": if reasoning.is_empty() {
                JsonValue::Array(Vec::new())
            } else {
                serde_json::json!([{"type": "summary_text", "text": reasoning}])
            },
        });
        if let Some(signature) = reasoning_signature {
            item["encrypted_content"] = JsonValue::String(signature.to_string());
        }
        output.push(item);
    }

    let text = chat_message_text(message.get("content"));
    let refusal = message.get("refusal").and_then(JsonValue::as_str);
    if !text.is_empty() || refusal.is_some() {
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(serde_json::json!({
                "type": "output_text",
                "text": text,
                "annotations": [],
                "logprobs": [],
            }));
        }
        if let Some(refusal) = refusal {
            content.push(serde_json::json!({"type": "refusal", "refusal": refusal}));
        }
        output.push(serde_json::json!({
            "id": "msg_0",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": content,
        }));
    }

    for (index, call) in message
        .get("tool_calls")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        if call.get("type").and_then(JsonValue::as_str) != Some("function") {
            continue;
        }
        let function = call.get("function").unwrap_or(&JsonValue::Null);
        let call_id = call
            .get("id")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        let mut item = serde_json::json!({
            "id": format!("fc_{index}"),
            "type": "function_call",
            "status": "completed",
            "call_id": call_id,
            "name": function.get("name").and_then(JsonValue::as_str).unwrap_or_default(),
            "arguments": function
                .get("arguments")
                .and_then(JsonValue::as_str)
                .unwrap_or_default(),
        });
        if let Some(signature) = call
            .get("thoughtSignature")
            .or_else(|| call.get("thought_signature"))
        {
            item["thought_signature"] = signature.clone();
        }
        let name = function
            .get("name")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        if tool_mapping.custom_tools.contains(name) {
            let input = function
                .get("arguments")
                .and_then(JsonValue::as_str)
                .and_then(|arguments| serde_json::from_str::<JsonValue>(arguments).ok())
                .and_then(|arguments| {
                    arguments
                        .get("input")
                        .and_then(JsonValue::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            item["type"] = JsonValue::String("custom_tool_call".to_string());
            item["input"] = JsonValue::String(input);
            if let Some(object) = item.as_object_mut() {
                object.remove("arguments");
            }
        } else if tool_mapping.tool_search && name == "tool_search" {
            let arguments = function
                .get("arguments")
                .and_then(JsonValue::as_str)
                .and_then(|arguments| serde_json::from_str::<JsonValue>(arguments).ok())
                .unwrap_or_else(|| serde_json::json!({}));
            item["type"] = JsonValue::String("tool_search_call".to_string());
            item["execution"] = JsonValue::String("client".to_string());
            item["arguments"] = arguments;
            if let Some(object) = item.as_object_mut() {
                object.remove("name");
            }
        }
        output.push(item);
    }

    let finish_reason = choice
        .get("finish_reason")
        .and_then(JsonValue::as_str)
        .unwrap_or("stop");
    let incomplete = matches!(finish_reason, "length" | "content_filter");
    let mut response = serde_json::json!({
        "id": response_id,
        "object": "response",
        "created_at": chat
            .get("created")
            .and_then(JsonValue::as_u64)
            .unwrap_or_else(|| now_unix_i64().max(0) as u64),
        "status": if incomplete { "incomplete" } else { "completed" },
        "error": JsonValue::Null,
        "incomplete_details": if incomplete {
            serde_json::json!({"reason": if finish_reason == "length" {
                "max_output_tokens"
            } else {
                "content_filter"
            }})
        } else {
            JsonValue::Null
        },
        "instructions": JsonValue::Null,
        "max_output_tokens": JsonValue::Null,
        "model": requested_model,
        "output": output,
        "parallel_tool_calls": true,
        "previous_response_id": JsonValue::Null,
        "reasoning": {"effort": JsonValue::Null, "summary": JsonValue::Null},
        "store": false,
        "temperature": JsonValue::Null,
        "text": {"format": {"type": "text"}},
        "tool_choice": "auto",
        "tools": [],
        "top_p": JsonValue::Null,
        "truncation": "disabled",
        "usage": chat.get("usage").and_then(chat_usage_to_responses),
        "user": JsonValue::Null,
        "metadata": {},
    });
    if let Some(service_tier) = chat.get("service_tier") {
        response["service_tier"] = service_tier.clone();
    }
    Ok(response)
}

fn chat_response_id(chat: &JsonValue) -> String {
    let id = chat
        .get("id")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    if let Some(suffix) = id.strip_prefix("chatcmpl-") {
        format!("resp_{suffix}")
    } else if id.is_empty() {
        format!("resp_{}", Uuid::new_v4().simple())
    } else {
        id.to_string()
    }
}

fn chat_message_text(content: Option<&JsonValue>) -> String {
    match content {
        Some(JsonValue::String(text)) => text.clone(),
        Some(JsonValue::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part {
                JsonValue::String(text) => Some(text.as_str()),
                JsonValue::Object(_) => part.get("text").and_then(JsonValue::as_str),
                _ => None,
            })
            .collect(),
        _ => String::new(),
    }
}

fn chat_usage_to_responses(usage: &JsonValue) -> Option<JsonValue> {
    let input = usage.get("prompt_tokens").and_then(JsonValue::as_u64)?;
    let output = usage
        .get("completion_tokens")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let total = usage
        .get("total_tokens")
        .and_then(JsonValue::as_u64)
        .unwrap_or_else(|| input.saturating_add(output));
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    Some(serde_json::json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": total,
    }))
}

#[derive(Debug, Default)]
struct ResponsesStreamTool {
    output_index:      u64,
    id:                String,
    call_id:           String,
    name:              String,
    arguments:         String,
    thought_signature: String,
    /// Whether `response.output_item.added` has been emitted for this tool.
    /// Announcement is delayed when custom/tool_search/namespace tools are
    /// declared and the tool name has not arrived yet, so the added item type
    /// matches the done item type.
    announced:         bool,
    is_custom:         bool,
    is_tool_search:    bool,
    /// Restored (namespace, name) for a namespace-flattened call; empty when
    /// the call is not a namespace child.
    namespace:         Option<(String, String)>,
}

struct OpenAiChatSseToResponses {
    requested_model:        String,
    response_id:            String,
    created_at:             u64,
    sequence_number:        u64,
    next_output_index:      u64,
    started:                bool,
    done:                   bool,
    finish_reason:          Option<String>,
    reasoning_output_index: Option<u64>,
    message_output_index:   Option<u64>,
    reasoning:              String,
    reasoning_signature:    String,
    text:                   String,
    tools:                  std::collections::BTreeMap<u64, ResponsesStreamTool>,
    usage:                  Option<JsonValue>,
    tool_mapping:           ResponsesToolMapping,
}

impl OpenAiChatSseToResponses {
    #[cfg(test)]
    fn new(requested_model: String) -> Self {
        Self::with_tool_mapping(requested_model, ResponsesToolMapping::default())
    }

    fn with_tool_mapping(requested_model: String, tool_mapping: ResponsesToolMapping) -> Self {
        Self {
            requested_model,
            response_id: String::new(),
            created_at: 0,
            sequence_number: 0,
            next_output_index: 0,
            started: false,
            done: false,
            finish_reason: None,
            reasoning_output_index: None,
            message_output_index: None,
            reasoning: String::new(),
            reasoning_signature: String::new(),
            text: String::new(),
            tools: std::collections::BTreeMap::new(),
            usage: None,
            tool_mapping,
        }
    }

    fn translate_sse_payload(&mut self, payload: &str) -> Result<Vec<String>> {
        if payload == "[DONE]" {
            return self.finish();
        }
        let chunk: JsonValue =
            serde_json::from_str(payload).context("parse OpenAI Chat Completions SSE payload")?;
        self.capture_meta(&chunk);
        let mut events = self.start_events()?;
        if let Some(usage) = chunk.get("usage").and_then(chat_usage_to_responses) {
            self.usage = Some(usage);
        }
        for choice in chunk
            .get("choices")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
        {
            if choice.get("index").and_then(JsonValue::as_u64).unwrap_or(0) != 0 {
                continue;
            }
            if let Some(reason) = choice.get("finish_reason").and_then(JsonValue::as_str) {
                self.finish_reason = Some(reason.to_string());
            }
            let delta = choice.get("delta").unwrap_or(&JsonValue::Null);
            if let Some(reasoning) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(JsonValue::as_str)
            {
                events.extend(self.reasoning_delta(reasoning)?);
            }
            if let Some(signature) = delta
                .get("reasoning_signature")
                .or_else(|| delta.get("reasoningSignature"))
                .or_else(|| delta.get("reasoning_content_signature"))
                .or_else(|| delta.get("encrypted_content"))
                .and_then(JsonValue::as_str)
            {
                if self.reasoning_output_index.is_none() {
                    events.extend(self.ensure_reasoning_item()?);
                }
                self.reasoning_signature.push_str(signature);
            }
            if let Some(content) = delta.get("content") {
                let text = chat_message_text(Some(content));
                if !text.is_empty() {
                    events.extend(self.text_delta(&text)?);
                }
            }
            for call in delta
                .get("tool_calls")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
            {
                events.extend(self.tool_delta(call)?);
            }
        }
        Ok(events)
    }

    fn capture_meta(&mut self, chunk: &JsonValue) {
        if self.response_id.is_empty() {
            self.response_id = chat_response_id(chunk);
        }
        if self.created_at == 0 {
            self.created_at = chunk
                .get("created")
                .and_then(JsonValue::as_u64)
                .unwrap_or_else(|| now_unix_i64().max(0) as u64);
        }
    }

    fn start_events(&mut self) -> Result<Vec<String>> {
        if self.started {
            return Ok(Vec::new());
        }
        self.started = true;
        let response = self.response_value("in_progress", Vec::new(), None);
        Ok(vec![
            self.event(serde_json::json!({"type": "response.created", "response": response}))?,
            self.event(serde_json::json!({
                "type": "response.in_progress",
                "response": self.response_value("in_progress", Vec::new(), None),
            }))?,
        ])
    }

    fn reasoning_delta(&mut self, delta: &str) -> Result<Vec<String>> {
        let mut events = self.ensure_reasoning_item()?;
        let output_index = self
            .reasoning_output_index
            .expect("reasoning item created above");
        self.reasoning.push_str(delta);
        events.push(self.event(serde_json::json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": "rs_0",
            "output_index": output_index,
            "summary_index": 0,
            "delta": delta,
        }))?);
        Ok(events)
    }

    fn ensure_reasoning_item(&mut self) -> Result<Vec<String>> {
        if self.reasoning_output_index.is_some() {
            return Ok(Vec::new());
        }
        let index = self.allocate_output_index();
        self.reasoning_output_index = Some(index);
        Ok(vec![
            self.event(serde_json::json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": {"id": "rs_0", "type": "reasoning", "status": "in_progress", "summary": []},
            }))?,
            self.event(serde_json::json!({
                "type": "response.reasoning_summary_part.added",
                "item_id": "rs_0",
                "output_index": index,
                "summary_index": 0,
                "part": {"type": "summary_text", "text": ""},
            }))?,
        ])
    }

    fn text_delta(&mut self, delta: &str) -> Result<Vec<String>> {
        let mut events = Vec::new();
        let output_index = if let Some(index) = self.message_output_index {
            index
        } else {
            let index = self.allocate_output_index();
            self.message_output_index = Some(index);
            events.push(self.event(serde_json::json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": {"id": "msg_0", "type": "message", "status": "in_progress", "role": "assistant", "content": []},
            }))?);
            events.push(self.event(serde_json::json!({
                "type": "response.content_part.added",
                "item_id": "msg_0",
                "output_index": index,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": [], "logprobs": []},
            }))?);
            index
        };
        self.text.push_str(delta);
        events.push(self.event(serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "msg_0",
            "output_index": output_index,
            "content_index": 0,
            "delta": delta,
            "logprobs": [],
        }))?);
        Ok(events)
    }

    fn tool_delta(&mut self, call: &JsonValue) -> Result<Vec<String>> {
        let chat_index = call.get("index").and_then(JsonValue::as_u64).unwrap_or(0);
        let mut events = Vec::new();
        if !self.tools.contains_key(&chat_index) {
            let output_index = self.allocate_output_index();
            let id = format!("fc_{chat_index}");
            let call_id = call
                .get("id")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string();
            let function = call.get("function").unwrap_or(&JsonValue::Null);
            let name = function
                .get("name")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string();
            self.tools.insert(chat_index, ResponsesStreamTool {
                output_index,
                id,
                call_id,
                name,
                arguments: String::new(),
                thought_signature: String::new(),
                announced: false,
                is_custom: false,
                is_tool_search: false,
                namespace: None,
            });
        }
        let function = call.get("function").unwrap_or(&JsonValue::Null);
        let arguments_delta = function
            .get("arguments")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        let tool = self
            .tools
            .get_mut(&chat_index)
            .expect("tool inserted above");
        if let Some(id) = call.get("id").and_then(JsonValue::as_str) {
            if tool.call_id.is_empty() {
                tool.call_id.push_str(id);
            } else if tool.call_id != id && !tool.call_id.ends_with(id) {
                tool.call_id.push_str(id);
            }
        }
        if let Some(name) = function.get("name").and_then(JsonValue::as_str) {
            if tool.name.is_empty() {
                tool.name.push_str(name);
            } else if !tool.name.ends_with(name) {
                tool.name.push_str(name);
            }
        }
        if let Some(signature) = call
            .get("thoughtSignature")
            .or_else(|| call.get("thought_signature"))
            .and_then(JsonValue::as_str)
        {
            tool.thought_signature.push_str(signature);
        }
        tool.arguments.push_str(arguments_delta);
        events.extend(self.announce_tool(chat_index)?);
        let tool = self.tools.get(&chat_index).expect("tool inserted above");
        // custom/tool_search arguments are buffered and emitted wholesale at
        // close time (see finish); only plain function_call emits per-delta.
        if !arguments_delta.is_empty() && tool.announced && !tool.is_custom && !tool.is_tool_search
        {
            let output_index = tool.output_index;
            let item_id = tool.id.clone();
            let call_id = tool.call_id.clone();
            let name = tool.name.clone();
            events.push(self.event(serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "item_id": item_id,
                "output_index": output_index,
                "call_id": call_id,
                "name": name,
                "delta": arguments_delta,
            }))?);
        }
        Ok(events)
    }

    /// Emits `response.output_item.added` for a tool once its type can be
    /// classified. When custom/tool_search/namespace tools are declared and the
    /// name has not arrived yet, the announcement is deferred so the added item
    /// type matches the done item type. Backfills buffered argument deltas for
    /// plain function_call tools when the announcement was delayed.
    fn announce_tool(&mut self, chat_index: u64) -> Result<Vec<String>> {
        let defer = {
            let tool = self.tools.get(&chat_index).expect("tool index exists");
            if tool.announced {
                return Ok(Vec::new());
            }
            let needs_name = !self.tool_mapping.custom_tools.is_empty()
                || self.tool_mapping.tool_search
                || !self.tool_mapping.namespace_tools.is_empty();
            needs_name && tool.name.is_empty()
        };
        if defer {
            return Ok(Vec::new());
        }
        let tool = self.tools.get_mut(&chat_index).expect("tool index exists");
        tool.announced = true;
        tool.is_custom = self.tool_mapping.custom_tools.contains(&tool.name);
        tool.is_tool_search =
            !tool.is_custom && self.tool_mapping.tool_search && tool.name == "tool_search";
        let namespace = if !tool.is_custom && !tool.is_tool_search {
            self.tool_mapping
                .namespace_entry(&tool.name)
                .map(|entry| (entry.namespace.clone(), entry.name.clone()))
        } else {
            None
        };
        tool.namespace = namespace.clone();
        let item_type = if tool.is_custom {
            "custom_tool_call"
        } else if tool.is_tool_search {
            "tool_search_call"
        } else {
            "function_call"
        };
        let output_index = tool.output_index;
        let id = tool.id.clone();
        let call_id = tool.call_id.clone();
        let name = tool.name.clone();
        let (item_name, item_namespace) = match &namespace {
            Some((ns, child)) => (child.clone(), Some(ns.clone())),
            None => (name.clone(), None),
        };
        let mut item = serde_json::json!({
            "id": id,
            "type": item_type,
            "status": "in_progress",
            "call_id": call_id,
        });
        if item_type == "custom_tool_call" {
            item["name"] = JsonValue::String(item_name.clone());
            item["input"] = JsonValue::String(String::new());
        } else if item_type == "tool_search_call" {
            item["execution"] = JsonValue::String("client".to_string());
            item["arguments"] = serde_json::json!({});
        } else {
            item["name"] = JsonValue::String(item_name.clone());
            item["arguments"] = JsonValue::String(String::new());
            if let Some(ns) = item_namespace {
                item["namespace"] = JsonValue::String(ns);
            }
        }
        let mut events = vec![self.event(serde_json::json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": item,
        }))?];
        // Backfill argument deltas accumulated before a late announcement.
        // custom/tool_search input is emitted wholesale at close, not here.
        let tool = self.tools.get(&chat_index).expect("tool index exists");
        if !tool.arguments.is_empty() && !tool.is_custom && !tool.is_tool_search {
            let item_id = tool.id.clone();
            let call_id = tool.call_id.clone();
            let name = tool.name.clone();
            let arguments = tool.arguments.clone();
            events.push(self.event(serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "item_id": item_id,
                "output_index": output_index,
                "call_id": call_id,
                "name": name,
                "delta": arguments,
            }))?);
        }
        Ok(events)
    }

    fn allocate_output_index(&mut self) -> u64 {
        let index = self.next_output_index;
        self.next_output_index = self.next_output_index.saturating_add(1);
        index
    }

    fn finish(&mut self) -> Result<Vec<String>> {
        if self.done {
            return Ok(Vec::new());
        }
        self.done = true;
        let mut events = if self.started {
            Vec::new()
        } else {
            self.start_events()?
        };
        let mut output = Vec::new();
        if let Some(output_index) = self.reasoning_output_index {
            let mut item = serde_json::json!({
                "id": "rs_0",
                "type": "reasoning",
                "status": "completed",
                "summary": if self.reasoning.is_empty() {
                    JsonValue::Array(Vec::new())
                } else {
                    serde_json::json!([{"type": "summary_text", "text": self.reasoning}])
                },
            });
            if !self.reasoning_signature.is_empty() {
                item["encrypted_content"] = JsonValue::String(self.reasoning_signature.clone());
            }
            events.push(self.event(serde_json::json!({
                "type": "response.reasoning_summary_text.done",
                "item_id": "rs_0",
                "output_index": output_index,
                "summary_index": 0,
                "text": self.reasoning,
            }))?);
            events.push(self.event(serde_json::json!({
                "type": "response.reasoning_summary_part.done",
                "item_id": "rs_0",
                "output_index": output_index,
                "summary_index": 0,
                "part": {"type": "summary_text", "text": self.reasoning},
            }))?);
            events.push(self.event(serde_json::json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item.clone(),
            }))?);
            output.push((output_index, item));
        }
        if let Some(output_index) = self.message_output_index {
            let item = serde_json::json!({
                "id": "msg_0",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": self.text, "annotations": [], "logprobs": []}],
            });
            events.push(self.event(serde_json::json!({
                "type": "response.output_text.done",
                "item_id": "msg_0",
                "output_index": output_index,
                "content_index": 0,
                "text": self.text,
                "logprobs": [],
            }))?);
            events.push(self.event(serde_json::json!({
                "type": "response.content_part.done",
                "item_id": "msg_0",
                "output_index": output_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": self.text, "annotations": [], "logprobs": []},
            }))?);
            events.push(self.event(serde_json::json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item.clone(),
            }))?);
            output.push((output_index, item));
        }
        let tool_indices: Vec<u64> = self.tools.keys().copied().collect();
        for chat_index in tool_indices {
            // Force-announce tools whose name never arrived so their added/done
            // types stay consistent (function_call fallback).
            events.extend(self.announce_tool(chat_index)?);
            let (
                output_index,
                item_id,
                call_id,
                item_name,
                item_namespace,
                item_type,
                arguments,
                thought_signature,
            ) = {
                let tool = self.tools.get(&chat_index).expect("tool index exists");
                let arguments = if tool.arguments.trim().is_empty() {
                    "{}".to_string()
                } else {
                    tool.arguments.clone()
                };
                let (item_name, item_namespace) = match &tool.namespace {
                    Some((ns, child)) => (child.clone(), Some(ns.clone())),
                    None => (tool.name.clone(), None),
                };
                let item_type = if tool.is_custom {
                    "custom_tool_call"
                } else if tool.is_tool_search {
                    "tool_search_call"
                } else {
                    "function_call"
                };
                (
                    tool.output_index,
                    tool.id.clone(),
                    tool.call_id.clone(),
                    item_name,
                    item_namespace,
                    item_type,
                    arguments,
                    tool.thought_signature.clone(),
                )
            };
            let mut item = serde_json::json!({
                "id": item_id.clone(),
                "type": item_type,
                "status": "completed",
                "call_id": call_id.clone(),
            });
            if item_type == "custom_tool_call" {
                let input = extract_custom_tool_input(&arguments);
                if !input.is_empty() {
                    events.push(self.event(serde_json::json!({
                        "type": "response.custom_tool_call_input.delta",
                        "item_id": item_id,
                        "output_index": output_index,
                        "delta": input,
                    }))?);
                }
                events.push(self.event(serde_json::json!({
                    "type": "response.custom_tool_call_input.done",
                    "item_id": item_id,
                    "output_index": output_index,
                    "call_id": call_id,
                    "name": item_name,
                    "input": input,
                }))?);
                item["name"] = JsonValue::String(item_name);
                item["input"] = JsonValue::String(input);
            } else if item_type == "tool_search_call" {
                let parsed =
                    serde_json::from_str(&arguments).unwrap_or_else(|_| serde_json::json!({}));
                item["execution"] = JsonValue::String("client".to_string());
                item["arguments"] = parsed;
            } else {
                events.push(self.event(serde_json::json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": item_id,
                    "output_index": output_index,
                    "call_id": call_id,
                    "name": item_name,
                    "arguments": arguments,
                }))?);
                item["name"] = JsonValue::String(item_name);
                item["arguments"] = JsonValue::String(arguments);
                if let Some(ns) = item_namespace {
                    item["namespace"] = JsonValue::String(ns);
                }
            }
            if !thought_signature.is_empty() {
                item["thought_signature"] = JsonValue::String(thought_signature);
            }
            events.push(self.event(serde_json::json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item.clone(),
            }))?);
            output.push((output_index, item));
        }
        output.sort_by_key(|(index, _)| *index);
        let output = output.into_iter().map(|(_, item)| item).collect();
        let incomplete = matches!(
            self.finish_reason.as_deref(),
            Some("length" | "content_filter")
        );
        let status = if incomplete {
            "incomplete"
        } else {
            "completed"
        };
        let response = self.response_value(status, output, self.usage.clone());
        events.push(self.event(serde_json::json!({
            "type": if incomplete { "response.incomplete" } else { "response.completed" },
            "response": response,
        }))?);
        events.push("[DONE]".to_string());
        Ok(events)
    }

    fn response_value(
        &self,
        status: &str,
        output: Vec<JsonValue>,
        usage: Option<JsonValue>,
    ) -> JsonValue {
        let incomplete = status == "incomplete";
        serde_json::json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "error": JsonValue::Null,
            "incomplete_details": if incomplete {
                serde_json::json!({"reason": if self.finish_reason.as_deref() == Some("length") {
                    "max_output_tokens"
                } else {
                    "content_filter"
                }})
            } else {
                JsonValue::Null
            },
            "instructions": JsonValue::Null,
            "max_output_tokens": JsonValue::Null,
            "model": self.requested_model,
            "output": output,
            "parallel_tool_calls": true,
            "previous_response_id": JsonValue::Null,
            "reasoning": {"effort": JsonValue::Null, "summary": JsonValue::Null},
            "store": false,
            "temperature": JsonValue::Null,
            "text": {"format": {"type": "text"}},
            "tool_choice": "auto",
            "tools": [],
            "top_p": JsonValue::Null,
            "truncation": "disabled",
            "usage": usage,
            "user": JsonValue::Null,
            "metadata": {},
        })
    }

    fn event(&mut self, mut event: JsonValue) -> Result<String> {
        event["sequence_number"] = JsonValue::Number(self.sequence_number.into());
        self.sequence_number = self.sequence_number.saturating_add(1);
        serde_json::to_string(&event).context("serialize OpenAI Responses SSE event")
    }
}

/// Extracts the free-form `input` from a custom tool call's lowered function
/// arguments. Mirrors sub2api's `extractCustomToolCallInput`: prefer the
/// `{"input": "..."}` field; if the model emitted non-schema JSON, return it
/// verbatim so the client can validate; empty `{}` or empty input yields "".
fn extract_custom_tool_input(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let Ok(value) = serde_json::from_str::<JsonValue>(trimmed) else {
        return trimmed.to_string();
    };
    let Some(obj) = value.as_object() else {
        return trimmed.to_string();
    };
    if let Some(input) = obj.get("input") {
        if let Some(text) = input.as_str() {
            return text.to_string();
        }
        return trimmed.to_string();
    }
    if obj.is_empty() {
        return String::new();
    }
    trimmed.to_string()
}

fn responses_json_to_openai_chat(
    payload: &[u8],
    requested_model: &str,
    tool_name_reverse: &std::collections::BTreeMap<String, String>,
) -> Result<JsonValue> {
    let root: JsonValue =
        serde_json::from_slice(payload).context("parse OpenAI Responses response body")?;
    let response = root
        .get("response")
        .filter(|_| {
            matches!(
                root.get("type").and_then(JsonValue::as_str),
                Some(
                    "response.completed"
                        | "response.incomplete"
                        | "response.failed"
                        | "response.done"
                )
            )
        })
        .unwrap_or(&root);

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut reasoning_signature = String::new();
    let mut tool_calls = Vec::new();
    for item in response
        .get("output")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(JsonValue::as_str) {
            Some("message") => append_response_message_text(item, &mut content),
            Some("reasoning") => {
                append_response_reasoning_text(item, &mut reasoning);
                append_response_reasoning_signature(item, &mut reasoning_signature);
            }
            Some("function_call") | Some("custom_tool_call") => {
                tool_calls.push(response_function_call_to_chat(
                    item,
                    tool_calls.len() as u64,
                    tool_name_reverse,
                ));
            }
            _ => {}
        }
    }
    if content.is_empty()
        && let Some(output_text) = response.get("output_text").and_then(JsonValue::as_str)
    {
        content.push_str(output_text);
    }

    let mut message = serde_json::Map::new();
    message.insert("role".into(), JsonValue::String("assistant".into()));
    message.insert(
        "content".into(),
        if content.is_empty() {
            JsonValue::Null
        } else {
            JsonValue::String(content)
        },
    );
    if !reasoning.is_empty() {
        message.insert("reasoning_content".into(), JsonValue::String(reasoning));
    }
    if !reasoning_signature.is_empty() {
        message.insert(
            "reasoning_signature".into(),
            JsonValue::String(reasoning_signature),
        );
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), JsonValue::Array(tool_calls));
    }

    let finish_reason = if message.contains_key("tool_calls") {
        "tool_calls"
    } else if response
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(JsonValue::as_str)
        == Some("max_output_tokens")
    {
        "length"
    } else {
        "stop"
    };
    let mut out = serde_json::json!({
        "id": response.get("id").and_then(JsonValue::as_str).unwrap_or_default(),
        "object": "chat.completion",
        "created": response
            .get("created_at")
            .and_then(JsonValue::as_u64)
            .unwrap_or_else(|| now_unix_i64().max(0) as u64),
        "model": requested_model,
        "choices": [{
            "index": 0,
            "message": JsonValue::Object(message),
            "finish_reason": finish_reason,
        }],
    });
    if let Some(usage) = response
        .get("usage")
        .and_then(responses_usage_to_openai_chat)
    {
        out["usage"] = usage;
    }
    Ok(out)
}

fn append_response_message_text(item: &JsonValue, out: &mut String) {
    for part in item
        .get("content")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        match part.get("type").and_then(JsonValue::as_str) {
            Some("output_text") | Some("text") => {
                if let Some(text) = part.get("text").and_then(JsonValue::as_str) {
                    out.push_str(text);
                }
            }
            Some("refusal") => {
                if let Some(text) = part
                    .get("refusal")
                    .or_else(|| part.get("text"))
                    .and_then(JsonValue::as_str)
                {
                    out.push_str(text);
                }
            }
            _ => {}
        }
    }
}

fn append_response_reasoning_text(item: &JsonValue, out: &mut String) {
    for part in item
        .get("summary")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        if matches!(
            part.get("type").and_then(JsonValue::as_str),
            Some("summary_text") | Some("output_text") | Some("text")
        ) && let Some(text) = part.get("text").and_then(JsonValue::as_str)
        {
            out.push_str(text);
        }
    }
}

fn append_response_reasoning_signature(item: &JsonValue, out: &mut String) {
    for key in [
        "encrypted_content",
        "reasoning_signature",
        "reasoning_content_signature",
        "signature",
    ] {
        if let Some(value) = item.get(key).and_then(JsonValue::as_str) {
            out.push_str(value);
            return;
        }
    }
}

fn response_function_call_to_chat(
    item: &JsonValue,
    index: u64,
    tool_name_reverse: &std::collections::BTreeMap<String, String>,
) -> JsonValue {
    let name = item
        .get("name")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    serde_json::json!({
        "index": index,
        "id": item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(JsonValue::as_str)
            .unwrap_or_default(),
        "type": "function",
        "function": {
            "name": tool_name_reverse.get(name).map(String::as_str).unwrap_or(name),
            "arguments": item
                .get("arguments")
                .or_else(|| item.get("input"))
                .and_then(JsonValue::as_str)
                .unwrap_or_default(),
        },
    })
}

fn responses_usage_to_openai_chat(usage: &JsonValue) -> Option<JsonValue> {
    let input = usage.get("input_tokens").and_then(JsonValue::as_u64)?;
    let output = usage
        .get("output_tokens")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let total = usage
        .get("total_tokens")
        .and_then(JsonValue::as_u64)
        .unwrap_or_else(|| input.saturating_add(output));
    let mut out = serde_json::json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": total,
    });
    let cached = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let cache_write = usage
        .get("input_tokens_details")
        .and_then(|details| {
            details
                .get("cache_write_tokens")
                .or_else(|| details.get("cached_creation_tokens"))
        })
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    if cached > 0 || cache_write > 0 {
        out["prompt_tokens_details"] = serde_json::json!({
            "cached_tokens": cached,
            "cached_creation_tokens": cache_write,
        });
    }
    Some(out)
}

struct ResponsesSseToOpenAiChat {
    requested_model:          String,
    response_id:              String,
    created_at:               u64,
    next_tool_index:          u64,
    last_tool_index:          u64,
    tool_indices:             std::collections::BTreeMap<u64, u64>,
    announced_tool_items:     std::collections::BTreeSet<u64>,
    arguments_delta_items:    std::collections::BTreeSet<u64>,
    content_seen:             bool,
    reasoning_seen:           bool,
    reasoning_signature_seen: bool,
    tool_calls_seen:          bool,
    done:                     bool,
    tool_name_reverse:        std::collections::BTreeMap<String, String>,
}

impl ResponsesSseToOpenAiChat {
    fn new(
        requested_model: String,
        tool_name_reverse: std::collections::BTreeMap<String, String>,
    ) -> Self {
        Self {
            requested_model,
            response_id: String::new(),
            created_at: 0,
            next_tool_index: 0,
            last_tool_index: 0,
            tool_indices: std::collections::BTreeMap::new(),
            announced_tool_items: std::collections::BTreeSet::new(),
            arguments_delta_items: std::collections::BTreeSet::new(),
            content_seen: false,
            reasoning_seen: false,
            reasoning_signature_seen: false,
            tool_calls_seen: false,
            done: false,
            tool_name_reverse,
        }
    }

    fn translate_sse_payload(&mut self, payload: &str) -> Result<Vec<String>> {
        if payload == "[DONE]" {
            return self.finish(None);
        }
        let event: JsonValue =
            serde_json::from_str(payload).context("parse OpenAI Responses SSE payload")?;
        let event_type = event
            .get("type")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        match event_type {
            "response.created" | "response.in_progress" => {
                if let Some(response) = event.get("response") {
                    self.capture_response_meta(response);
                }
                Ok(Vec::new())
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let Some(delta) = event.get("delta").and_then(JsonValue::as_str) else {
                    return Ok(Vec::new());
                };
                self.content_seen = true;
                self.chunk(
                    serde_json::json!({"role": "assistant", "content": delta}),
                    None,
                    None,
                )
                .map(|chunk| vec![chunk])
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let Some(delta) = event.get("delta").and_then(JsonValue::as_str) else {
                    return Ok(Vec::new());
                };
                self.reasoning_seen = true;
                self.chunk(
                    serde_json::json!({"role": "assistant", "reasoning_content": delta}),
                    None,
                    None,
                )
                .map(|chunk| vec![chunk])
            }
            "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.done" => Ok(Vec::new()),
            "response.output_item.added" => self.output_item_added(&event),
            "response.function_call_arguments.delta" => self.function_arguments_delta(&event),
            "response.function_call_arguments.done" => self.function_arguments_done(&event),
            "response.output_item.done" => self.output_item_done(&event),
            "response.completed" | "response.done" | "response.incomplete" => {
                let response = event.get("response").unwrap_or(&event);
                self.capture_response_meta(response);
                let mut chunks = self.completed_fallback_chunks(response)?;
                let finish_reason =
                    responses_terminal_finish_reason(response, self.tool_calls_seen);
                chunks.extend(self.finish_with_reason(response.get("usage"), finish_reason)?);
                Ok(chunks)
            }
            "error" | "response.failed" => {
                if self.done {
                    return Ok(Vec::new());
                }
                let message = event
                    .get("error")
                    .or_else(|| {
                        event
                            .get("response")
                            .and_then(|response| response.get("error"))
                    })
                    .and_then(|error| error.get("message"))
                    .and_then(JsonValue::as_str)
                    .unwrap_or("OpenAI Responses stream failed");
                self.content_seen = true;
                let mut chunks = vec![self.chunk(
                    serde_json::json!({"role": "assistant", "content": message}),
                    None,
                    None,
                )?];
                chunks.extend(
                    self.finish_with_reason(
                        event
                            .get("response")
                            .and_then(|response| response.get("usage"))
                            .or_else(|| event.get("usage")),
                        "stop",
                    )?,
                );
                Ok(chunks)
            }
            _ => Ok(Vec::new()),
        }
    }

    fn capture_response_meta(&mut self, response: &JsonValue) {
        if let Some(id) = response.get("id").and_then(JsonValue::as_str) {
            self.response_id = id.to_string();
        }
        if let Some(created_at) = response.get("created_at").and_then(JsonValue::as_u64) {
            self.created_at = created_at;
        }
    }

    fn output_item_added(&mut self, event: &JsonValue) -> Result<Vec<String>> {
        let Some(item) = event.get("item") else {
            return Ok(Vec::new());
        };
        if !matches!(
            item.get("type").and_then(JsonValue::as_str),
            Some("function_call") | Some("custom_tool_call")
        ) {
            return Ok(Vec::new());
        }
        let output_index = event
            .get("output_index")
            .and_then(JsonValue::as_u64)
            .unwrap_or(self.next_tool_index);
        let tool_index = self.tool_index(output_index);
        self.announced_tool_items.insert(output_index);
        self.tool_calls_seen = true;
        let mut tool_call =
            response_function_call_to_chat(item, tool_index, &self.tool_name_reverse);
        tool_call["function"]["arguments"] = JsonValue::String(String::new());
        self.chunk(
            serde_json::json!({"role": "assistant", "tool_calls": [tool_call]}),
            None,
            None,
        )
        .map(|chunk| vec![chunk])
    }

    fn function_arguments_delta(&mut self, event: &JsonValue) -> Result<Vec<String>> {
        let Some(delta) = event.get("delta").and_then(JsonValue::as_str) else {
            return Ok(Vec::new());
        };
        let output_index = event
            .get("output_index")
            .and_then(JsonValue::as_u64)
            .unwrap_or(self.last_tool_index);
        let tool_index = self.tool_index(output_index);
        self.arguments_delta_items.insert(output_index);
        self.tool_calls_seen = true;
        self.chunk(
            serde_json::json!({
                "tool_calls": [{
                    "index": tool_index,
                    "function": {"arguments": delta},
                }]
            }),
            None,
            None,
        )
        .map(|chunk| vec![chunk])
    }

    fn function_arguments_done(&mut self, event: &JsonValue) -> Result<Vec<String>> {
        let output_index = event
            .get("output_index")
            .and_then(JsonValue::as_u64)
            .unwrap_or(self.last_tool_index);
        if self.arguments_delta_items.contains(&output_index) {
            return Ok(Vec::new());
        }
        let Some(arguments) = event.get("arguments").and_then(JsonValue::as_str) else {
            return Ok(Vec::new());
        };
        let tool_index = self.tool_index(output_index);
        self.tool_calls_seen = true;
        self.chunk(
            serde_json::json!({
                "tool_calls": [{
                    "index": tool_index,
                    "function": {"arguments": arguments},
                }]
            }),
            None,
            None,
        )
        .map(|chunk| vec![chunk])
    }

    fn output_item_done(&mut self, event: &JsonValue) -> Result<Vec<String>> {
        let Some(item) = event.get("item") else {
            return Ok(Vec::new());
        };
        if item.get("type").and_then(JsonValue::as_str) == Some("reasoning") {
            let mut signature = String::new();
            append_response_reasoning_signature(item, &mut signature);
            if !signature.is_empty() && !self.reasoning_signature_seen {
                self.reasoning_signature_seen = true;
                return self
                    .chunk(
                        serde_json::json!({
                            "role": "assistant",
                            "reasoning_signature": signature,
                        }),
                        None,
                        None,
                    )
                    .map(|chunk| vec![chunk]);
            }
            return Ok(Vec::new());
        }
        if !matches!(
            item.get("type").and_then(JsonValue::as_str),
            Some("function_call") | Some("custom_tool_call")
        ) {
            return Ok(Vec::new());
        }
        let output_index = event
            .get("output_index")
            .and_then(JsonValue::as_u64)
            .unwrap_or(self.next_tool_index);
        if self.announced_tool_items.contains(&output_index) {
            return Ok(Vec::new());
        }
        let tool_index = self.tool_index(output_index);
        self.announced_tool_items.insert(output_index);
        self.tool_calls_seen = true;
        let tool_call = response_function_call_to_chat(item, tool_index, &self.tool_name_reverse);
        self.chunk(
            serde_json::json!({"role": "assistant", "tool_calls": [tool_call]}),
            None,
            None,
        )
        .map(|chunk| vec![chunk])
    }

    fn completed_fallback_chunks(&mut self, response: &JsonValue) -> Result<Vec<String>> {
        let mut chunks = Vec::new();
        let output = response
            .get("output")
            .and_then(JsonValue::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if !self.content_seen {
            let mut text = String::new();
            for item in output {
                if item.get("type").and_then(JsonValue::as_str) == Some("message") {
                    append_response_message_text(item, &mut text);
                }
            }
            if !text.is_empty() {
                self.content_seen = true;
                chunks.push(self.chunk(
                    serde_json::json!({"role": "assistant", "content": text}),
                    None,
                    None,
                )?);
            }
        }
        if !self.reasoning_seen || !self.reasoning_signature_seen {
            let mut reasoning = String::new();
            let mut signature = String::new();
            for item in output {
                if item.get("type").and_then(JsonValue::as_str) == Some("reasoning") {
                    append_response_reasoning_text(item, &mut reasoning);
                    append_response_reasoning_signature(item, &mut signature);
                }
            }
            let mut delta = serde_json::Map::new();
            if !self.reasoning_seen && !reasoning.is_empty() {
                self.reasoning_seen = true;
                delta.insert("reasoning_content".into(), JsonValue::String(reasoning));
            }
            if !self.reasoning_signature_seen && !signature.is_empty() {
                self.reasoning_signature_seen = true;
                delta.insert("reasoning_signature".into(), JsonValue::String(signature));
            }
            if !delta.is_empty() {
                chunks.push(self.chunk(JsonValue::Object(delta), None, None)?);
            }
        }
        for (output_index, item) in output.iter().enumerate() {
            match item.get("type").and_then(JsonValue::as_str) {
                Some("function_call") | Some("custom_tool_call")
                    if !self.announced_tool_items.contains(&(output_index as u64)) =>
                {
                    let output_index = output_index as u64;
                    let tool_index = self.tool_index(output_index);
                    self.announced_tool_items.insert(output_index);
                    self.tool_calls_seen = true;
                    chunks.push(self.chunk(
                        serde_json::json!({
                            "role": "assistant",
                            "tool_calls": [response_function_call_to_chat(
                                item,
                                tool_index,
                                &self.tool_name_reverse,
                            )],
                        }),
                        None,
                        None,
                    )?);
                }
                _ => {}
            }
        }
        Ok(chunks)
    }

    fn finish(&mut self, usage: Option<&JsonValue>) -> Result<Vec<String>> {
        let finish_reason = if self.tool_calls_seen {
            "tool_calls"
        } else {
            "stop"
        };
        self.finish_with_reason(usage, finish_reason)
    }

    fn finish_with_reason(
        &mut self,
        usage: Option<&JsonValue>,
        finish_reason: &str,
    ) -> Result<Vec<String>> {
        if self.done {
            return Ok(Vec::new());
        }
        self.done = true;
        Ok(vec![
            self.chunk(
                serde_json::json!({}),
                Some(finish_reason),
                usage.and_then(responses_usage_to_openai_chat),
            )?,
            "[DONE]".to_string(),
        ])
    }

    fn tool_index(&mut self, output_index: u64) -> u64 {
        if let Some(index) = self.tool_indices.get(&output_index) {
            self.last_tool_index = output_index;
            return *index;
        }
        let index = self.next_tool_index;
        self.next_tool_index = self.next_tool_index.saturating_add(1);
        self.last_tool_index = output_index;
        self.tool_indices.insert(output_index, index);
        index
    }

    fn chunk(
        &self,
        delta: JsonValue,
        finish_reason: Option<&str>,
        usage: Option<JsonValue>,
    ) -> Result<String> {
        let mut value = serde_json::json!({
            "id": self.response_id,
            "object": "chat.completion.chunk",
            "created": self.created_at,
            "model": self.requested_model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }],
        });
        if let Some(usage) = usage {
            value["usage"] = usage;
        }
        serde_json::to_string(&value).context("serialize OpenAI chat SSE chunk")
    }
}

fn responses_terminal_finish_reason(response: &JsonValue, tool_calls_seen: bool) -> &'static str {
    if response.get("status").and_then(JsonValue::as_str) == Some("incomplete") {
        return match response
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(JsonValue::as_str)
        {
            Some("content_filter") => "content_filter",
            _ => "length",
        };
    }
    if tool_calls_seen {
        "tool_calls"
    } else {
        "stop"
    }
}

pub(crate) async fn buffered_gemini_as_openai(
    mut upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading Gemini response: {err}"),
            );
        }
    };
    let gemini_resp: gemini::GeminiChatResponse = match serde_json::from_slice(&payload) {
        Ok(resp) => resp,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid Gemini response: {err}"),
            );
        }
    };
    let usage = ResponseUsage::from_gemini(gemini_resp.usage_metadata);
    let mut resp = json_response(
        StatusCode::OK,
        gemini_response_to_openai_chat(gemini_resp, requested_model),
    );
    attach_response_usage(&mut resp, Some(usage));
    resp
}

pub(crate) fn stream_gemini_as_openai(
    upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let mut translator = GeminiSseToOpenAiTranslator::new(requested_model);
    let mut decoder = SseBuffer::default();
    let body = stream_body_from_async(move |sender| {
        let stream = HttpBodyStream::from(upstream.into_body());
        pump_http_body_stream(stream, sender, move |bytes| {
            let mut out = Vec::new();
            for payload in decoder.push_with_done(&bytes, true) {
                match translator.translate_sse_payload(&payload) {
                    Ok(events) => {
                        for event in events {
                            write_sse_data(&mut out, &event);
                        }
                    }
                    Err(err) => {
                        warn!(?err, "failed to translate Gemini SSE event");
                    }
                }
            }
            Bytes::from(out)
        })
    });

    response_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        })
}

pub(crate) async fn buffered_gemini_as_claude(
    mut upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading Gemini response: {err}"),
            );
        }
    };
    let gemini_resp: gemini::GeminiChatResponse = match serde_json::from_slice(&payload) {
        Ok(resp) => resp,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid Gemini response: {err}"),
            );
        }
    };
    let openai_resp = gemini_response_to_openai_chat(gemini_resp, requested_model.clone());
    let usage = openai_resp.usage.map(ResponseUsage::from_openai);
    let mut resp = json_response(
        StatusCode::OK,
        openai_chat_to_claude_messages_response(openai_resp, requested_model),
    );
    attach_response_usage(&mut resp, usage);
    resp
}

pub(crate) fn stream_gemini_as_claude(
    upstream: Response<HttpBody>,
    requested_model: String,
) -> Response<GatewayBody> {
    let mut gemini_to_openai = GeminiSseToOpenAiTranslator::new(requested_model.clone());
    let mut openai_to_claude = OpenAiSseToClaudeTranslator::new(requested_model);
    let mut decoder = SseBuffer::default();
    let body = stream_body_from_async(move |sender| {
        let stream = HttpBodyStream::from(upstream.into_body());
        pump_http_body_stream(stream, sender, move |bytes| {
            let mut out = Vec::new();
            for payload in decoder.push_with_done(&bytes, true) {
                match gemini_to_openai.translate_sse_payload(&payload) {
                    Ok(openai_events) => {
                        for openai_event in openai_events {
                            match openai_to_claude.translate_sse_payload(&openai_event) {
                                Ok(claude_events) => {
                                    for event in claude_events {
                                        write_claude_sse_event(&mut out, &event);
                                    }
                                }
                                Err(err) => {
                                    warn!(?err, "failed to translate chained OpenAI SSE event");
                                }
                            }
                        }
                    }
                    Err(err) => {
                        warn!(?err, "failed to translate Gemini SSE event");
                    }
                }
            }
            Bytes::from(out)
        })
    });

    response_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        })
}

pub(crate) async fn buffered_openai_as_gemini(
    mut upstream: Response<HttpBody>,
) -> Response<GatewayBody> {
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading OpenAI response: {err}"),
            );
        }
    };
    let openai_resp: openai::ChatCompletionResponse = match serde_json::from_slice(&payload) {
        Ok(resp) => resp,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid OpenAI response: {err}"),
            );
        }
    };
    let usage = openai_resp.usage.map(ResponseUsage::from_openai);
    let mut resp = json_response(StatusCode::OK, openai_chat_to_gemini_response(openai_resp));
    attach_response_usage(&mut resp, usage);
    resp
}

pub(crate) fn stream_openai_as_gemini(upstream: Response<HttpBody>) -> Response<GatewayBody> {
    let mut translator = OpenAiSseToGeminiTranslator::new();
    let mut decoder = SseBuffer::default();
    let body = stream_body_from_async(move |sender| {
        let stream = HttpBodyStream::from(upstream.into_body());
        pump_http_body_stream(stream, sender, move |bytes| {
            let mut out = Vec::new();
            for payload in decoder.push_with_done(&bytes, true) {
                match translator.translate_sse_payload(&payload) {
                    Ok(events) => {
                        for event in events {
                            write_sse_data(&mut out, &event);
                        }
                    }
                    Err(err) => {
                        warn!(?err, "failed to translate OpenAI SSE event");
                    }
                }
            }
            Bytes::from(out)
        })
    });

    response_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        })
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
    match gemini_imagen_to_openai_image_response(gemini_resp, now_unix_i64()) {
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

    response_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(full_body(out))
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
            message:    message.to_string(),
            error_type: error_type.to_string(),
            code:       None,
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
        prompt_tokens:         (prompt > 0).then_some(prompt),
        completion_tokens:     (completion > 0).then_some(completion),
        total_tokens:          (total > 0).then_some(total),
        cache_read_tokens:     (cached_content > 0).then_some(cached_content),
        cache_creation_tokens: None,
        image_tokens:          (image_tokens > 0).then_some(image_tokens),
        audio_tokens:          (audio_tokens > 0).then_some(audio_tokens),
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

        assert_eq!(usage, ResponseUsage {
            prompt_tokens:         Some(11),
            completion_tokens:     Some(7),
            total_tokens:          Some(18),
            cache_read_tokens:     None,
            cache_creation_tokens: None,
            image_tokens:          None,
            audio_tokens:          None,
        });
    }

    #[test]
    fn extracts_openai_cached_usage_from_json() {
        let usage = response_usage_from_json_bytes(
            br#"{"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,"prompt_tokens_details":{"cached_tokens":4,"cached_creation_tokens":2,"image_tokens":3,"audio_tokens":1}}}"#,
        )
        .unwrap();

        assert_eq!(usage, ResponseUsage {
            prompt_tokens:         Some(11),
            completion_tokens:     Some(7),
            total_tokens:          Some(18),
            cache_read_tokens:     Some(4),
            cache_creation_tokens: Some(2),
            image_tokens:          Some(3),
            audio_tokens:          Some(1),
        });
    }

    #[test]
    fn extracts_responses_style_usage_from_json() {
        let usage =
            response_usage_from_json_bytes(br#"{"usage":{"input_tokens":13,"output_tokens":5}}"#)
                .unwrap();

        assert_eq!(usage, ResponseUsage {
            prompt_tokens:         Some(13),
            completion_tokens:     Some(5),
            total_tokens:          Some(18),
            cache_read_tokens:     None,
            cache_creation_tokens: None,
            image_tokens:          None,
            audio_tokens:          None,
        });
    }

    #[test]
    fn extracts_claude_usage_with_cache_tokens_from_json() {
        let usage = response_usage_from_json_bytes(
            br#"{"usage":{"input_tokens":10,"cache_creation_input_tokens":3,"cache_read_input_tokens":2,"output_tokens":8}}"#,
        )
        .unwrap();

        assert_eq!(usage, ResponseUsage {
            prompt_tokens:         Some(15),
            completion_tokens:     Some(8),
            total_tokens:          Some(23),
            cache_read_tokens:     Some(2),
            cache_creation_tokens: Some(3),
            image_tokens:          None,
            audio_tokens:          None,
        });
    }

    #[test]
    fn extracts_gemini_usage_metadata_from_json() {
        let usage = response_usage_from_json_bytes(
            br#"{"usageMetadata":{"promptTokenCount":9,"cachedContentTokenCount":4,"candidatesTokenCount":6,"thoughtsTokenCount":2,"totalTokenCount":30,"promptTokensDetails":[{"modality":"IMAGE","tokenCount":3},{"modality":"AUDIO","tokenCount":2}]}}"#,
        )
        .unwrap();

        assert_eq!(usage, ResponseUsage {
            prompt_tokens:         Some(9),
            completion_tokens:     Some(8),
            total_tokens:          Some(30),
            cache_read_tokens:     Some(4),
            cache_creation_tokens: None,
            image_tokens:          Some(3),
            audio_tokens:          Some(2),
        });
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
        assert_eq!(usage, ResponseUsage {
            prompt_tokens:         Some(21),
            completion_tokens:     Some(9),
            total_tokens:          Some(30),
            cache_read_tokens:     None,
            cache_creation_tokens: None,
            image_tokens:          None,
            audio_tokens:          None,
        });
    }

    #[test]
    fn responses_usage_does_not_double_count_cache_tokens() {
        let usage = response_usage_from_json_bytes(
            br#"{"type":"response.completed","response":{"usage":{"input_tokens":15,"input_tokens_details":{"cached_tokens":2},"cache_creation_input_tokens":3,"output_tokens":5,"total_tokens":20}}}"#,
        )
        .unwrap();

        assert_eq!(usage, ResponseUsage {
            prompt_tokens:         Some(15),
            completion_tokens:     Some(5),
            total_tokens:          Some(20),
            cache_read_tokens:     Some(2),
            cache_creation_tokens: Some(3),
            image_tokens:          None,
            audio_tokens:          None,
        });
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
        let mut translator = OpenAiChatSseToResponses::new("requested-model".to_string());
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
        let mut translator = OpenAiChatSseToResponses::new("requested-model".to_string());
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
        let mut translator = OpenAiChatSseToResponses::new("requested-model".to_string());
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
        let mut translator = ResponsesSseToOpenAiChat::new("requested-model".to_string(), reverse);
        assert!(translator
            .translate_sse_payload(
                r#"{"type":"response.created","response":{"id":"resp_1","created_at":1700000000}}"#,
            )
            .expect("created event should parse")
            .is_empty());

        let text = translator
            .translate_sse_payload(r#"{"type":"response.output_text.delta","delta":"hi"}"#)
            .expect("text event should parse");
        let text: JsonValue = serde_json::from_str(&text[0]).expect("chat chunk should be JSON");
        assert_eq!(text["choices"][0]["delta"]["content"], "hi");

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
        let final_chunk = parsed.last().expect("finish chunk should exist");
        assert_eq!(final_chunk["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(final_chunk["usage"]["prompt_tokens"], 9);
        assert_eq!(final_chunk["usage"]["completion_tokens"], 4);
    }

    #[test]
    fn responses_sse_incomplete_maps_reason_and_failed_is_idempotent() {
        let mut incomplete =
            ResponsesSseToOpenAiChat::new("requested-model".to_string(), Default::default());
        let terminal = incomplete
            .translate_sse_payload(
                r#"{"type":"response.incomplete","response":{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#,
            )
            .unwrap();
        let finish: JsonValue = serde_json::from_str(&terminal[0]).unwrap();
        assert_eq!(finish["choices"][0]["finish_reason"], "length");
        assert_eq!(finish["usage"]["total_tokens"], 5);
        assert_eq!(terminal.last().map(String::as_str), Some("[DONE]"));
        assert!(incomplete.finish(None).unwrap().is_empty());

        let mut failed =
            ResponsesSseToOpenAiChat::new("requested-model".to_string(), Default::default());
        let terminal = failed
            .translate_sse_payload(
                r#"{"type":"response.failed","response":{"error":{"message":"upstream failed"}}}"#,
            )
            .unwrap();
        assert_eq!(terminal.last().map(String::as_str), Some("[DONE]"));
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
            OpenAiChatSseToResponses::with_tool_mapping("model".to_string(), mapping);
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
            OpenAiChatSseToResponses::with_tool_mapping("model".to_string(), mapping);
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
            OpenAiChatSseToResponses::with_tool_mapping("model".to_string(), mapping);
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
            OpenAiChatSseToResponses::with_tool_mapping("model".to_string(), mapping);
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
    fn extract_custom_tool_input_handles_schema_and_raw_fallback() {
        assert_eq!(extract_custom_tool_input(r#"{"input":"dir"}"#), "dir");
        assert_eq!(
            extract_custom_tool_input("console.log(1)"),
            "console.log(1)"
        );
        assert_eq!(
            extract_custom_tool_input(r#"{"other":"x"}"#),
            r#"{"other":"x"}"#
        );
        assert_eq!(extract_custom_tool_input("{}"), "");
        assert_eq!(extract_custom_tool_input(""), "");
    }

    #[test]
    fn responses_sse_emits_reasoning_signature_delta_before_terminal() {
        let mut translator =
            ResponsesSseToOpenAiChat::new("requested-model".to_string(), Default::default());
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
        let final_chunk = parsed.last().unwrap();
        assert_eq!(final_chunk["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn responses_sse_redacted_reasoning_with_only_signature_emits_signature_delta() {
        let mut translator =
            ResponsesSseToOpenAiChat::new("requested-model".to_string(), Default::default());
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
            id:            "msg_1".to_string(),
            response_type: "message".to_string(),
            role:          "assistant".to_string(),
            model:         "claude".to_string(),
            content:       vec![
                claude::ContentBlock::Thinking {
                    thinking:  "why".to_string(),
                    signature: Some("cipher".to_string()),
                },
                claude::ContentBlock::RedactedThinking {
                    data: "redacted".to_string(),
                },
            ],
            stop_reason:   Some("end_turn".to_string()),
            usage:         None,
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
