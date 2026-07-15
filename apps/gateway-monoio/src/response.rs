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
    let mut translator = ClaudeSseTranslator::new(requested_model);
    let mut decoder = SseBuffer::default();
    let body = stream_body_from_async(move |sender| {
        let stream = HttpBodyStream::from(upstream.into_body());
        pump_http_body_stream(stream, sender, move |bytes| {
            let mut out = Vec::new();
            for payload in decoder.push(&bytes) {
                match translator.translate_sse_payload(&payload) {
                    Ok(events) => {
                        for event in events {
                            write_sse_data(&mut out, &event);
                        }
                    }
                    Err(err) => {
                        warn!(?err, "failed to translate Claude SSE event");
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
    let mut translator = OpenAiSseToClaudeTranslator::new(requested_model);
    let mut decoder = SseBuffer::default();
    let body = stream_body_from_async(move |sender| {
        let stream = HttpBodyStream::from(upstream.into_body());
        pump_http_body_stream(stream, sender, move |bytes| {
            let mut out = Vec::new();
            for payload in decoder.push(&bytes) {
                match translator.translate_sse_payload(&payload) {
                    Ok(events) => {
                        for event in events {
                            write_sse_data(&mut out, &event);
                        }
                    }
                    Err(err) => {
                        warn!(?err, "failed to translate OpenAI->Claude SSE event");
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

fn responses_json_to_openai_chat(
    payload: &[u8],
    requested_model: &str,
    tool_name_reverse: &std::collections::BTreeMap<String, String>,
) -> Result<JsonValue> {
    let root: JsonValue =
        serde_json::from_slice(payload).context("parse OpenAI Responses response body")?;
    let response = root
        .get("response")
        .filter(|_| root.get("type").and_then(JsonValue::as_str) == Some("response.completed"))
        .unwrap_or(&root);

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for item in response
        .get("output")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(JsonValue::as_str) {
            Some("message") => append_response_message_text(item, &mut content),
            Some("reasoning") => append_response_reasoning_text(item, &mut reasoning),
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
    requested_model:       String,
    response_id:           String,
    created_at:            u64,
    next_tool_index:       u64,
    last_tool_index:       u64,
    tool_indices:          std::collections::BTreeMap<u64, u64>,
    announced_tool_items:  std::collections::BTreeSet<u64>,
    arguments_delta_items: std::collections::BTreeSet<u64>,
    content_seen:          bool,
    reasoning_seen:        bool,
    tool_calls_seen:       bool,
    done:                  bool,
    tool_name_reverse:     std::collections::BTreeMap<String, String>,
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
            "response.output_item.added" => self.output_item_added(&event),
            "response.function_call_arguments.delta" => self.function_arguments_delta(&event),
            "response.function_call_arguments.done" => self.function_arguments_done(&event),
            "response.output_item.done" => self.output_item_done(&event),
            "response.completed" => {
                let response = event.get("response").unwrap_or(&event);
                self.capture_response_meta(response);
                let mut chunks = self.completed_fallback_chunks(response)?;
                chunks.extend(self.finish(response.get("usage"))?);
                Ok(chunks)
            }
            "error" | "response.failed" => {
                let message = event
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(JsonValue::as_str)
                    .unwrap_or("OpenAI Responses stream failed");
                self.content_seen = true;
                self.chunk(
                    serde_json::json!({"role": "assistant", "content": message}),
                    Some("stop"),
                    None,
                )
                .map(|chunk| vec![chunk, "[DONE]".to_string()])
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
        if !self.reasoning_seen {
            let mut reasoning = String::new();
            for item in output {
                if item.get("type").and_then(JsonValue::as_str) == Some("reasoning") {
                    append_response_reasoning_text(item, &mut reasoning);
                }
            }
            if !reasoning.is_empty() {
                self.reasoning_seen = true;
                chunks.push(self.chunk(
                    serde_json::json!({
                        "role": "assistant",
                        "reasoning_content": reasoning,
                    }),
                    None,
                    None,
                )?);
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
        if self.done {
            return Ok(Vec::new());
        }
        self.done = true;
        let finish_reason = if self.tool_calls_seen {
            "tool_calls"
        } else {
            "stop"
        };
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
    value
        .get("usage")
        .and_then(response_usage_from_usage_object)
        // OpenAI Responses API SSE: {"type":"response.completed","response":{"usage":{...}}}
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("usage"))
                .and_then(response_usage_from_usage_object)
        })
        .or_else(|| {
            value
                .get("usageMetadata")
                .and_then(response_usage_from_gemini_usage_object)
        })
}

fn response_usage_from_usage_object(value: &JsonValue) -> Option<ResponseUsage> {
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
                    input_tokens
                        .unwrap_or(0)
                        .saturating_add(cache_creation_tokens.unwrap_or(0))
                        .saturating_add(cache_read_tokens.unwrap_or(0))
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
    let prompt = json_u64(value, "promptTokenCount")
        .unwrap_or(0)
        .saturating_add(cached_content);
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
            prompt_tokens:         Some(13),
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
}
