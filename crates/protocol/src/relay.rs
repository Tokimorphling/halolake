use halolake_api_contract::JsonValue;
use relaykit::{Context, Format, Request, Response, ResponseStreamState, StreamOptions};
use serde_json::json;
use std::sync::Arc;
use thiserror::Error;

pub type ResponsesToolMapping = relaykit::ResponsesToolState;
pub use relaykit::ResponsesNamespaceTool;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("request contains no messages")]
    EmptyMessages,
    #[error("not supported model for image generation, only imagen models are supported")]
    UnsupportedImageModel,
    #[error("no images generated")]
    NoImagesGenerated,
    #[error("unsupported request state: {0}")]
    UnsupportedRequest(String),
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Relay(relaykit::Error),
}

impl From<relaykit::Error> for ProtocolError {
    fn from(error: relaykit::Error) -> Self {
        match error {
            relaykit::Error::Json(error) => Self::Json(error),
            relaykit::Error::Message(message) => Self::UnsupportedRequest(message),
            error => Self::UnsupportedRequest(error.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptCacheMode {
    Automatic,
    Explicit,
}

fn media_data(source: &str, fallback_mime: &str) -> Result<(String, String), relaykit::Error> {
    if !source.starts_with("data:")
        && !source.starts_with("http://")
        && !source.starts_with("https://")
        && !source.is_empty()
        && !fallback_mime.is_empty()
    {
        return Ok((source.to_string(), fallback_mime.to_string()));
    }
    let Some((header, data)) = source.strip_prefix("data:").and_then(|v| v.split_once(',')) else {
        return Err(relaykit::Error::message(
            "media URL requires a host resolver; only inline data is decoded by the protocol facade",
        ));
    };
    let Some(mime) = header.strip_suffix(";base64") else {
        return Err(relaykit::Error::message("media data URL must use base64"));
    };
    if data.is_empty() {
        return Err(relaykit::Error::message("media data URL is empty"));
    }
    Ok((
        data.to_string(),
        if mime.is_empty() { fallback_mime } else { mime }.to_string(),
    ))
}

fn context(model: &str, default_max_tokens: i64) -> Context {
    let mut ctx = Context {
        origin_model_name: model.to_string(),
        upstream_model_name: model.to_string(),
        channel_meta_attached: true,
        ..Context::default()
    };
    ctx.options.preserve_message_structure = true;
    ctx.options.allow_unpaired_hosted_search = true;
    ctx.options.tool_loss_policy = relaykit::LossPolicy::Safe;
    ctx.options.gemini.function_call_thought_signature_enabled = true;
    ctx.options.claude.default_max_tokens = Some(Arc::new(move |_| default_max_tokens));
    ctx.media.get_base64_data = Some(Arc::new(media_data));
    ctx.media.decode_base64_file_data =
        Some(Arc::new(|source| media_data(source, "application/octet-stream")));
    ctx
}

pub fn responses_tool_mapping(raw: &[u8]) -> Result<ResponsesToolMapping, ProtocolError> {
    Ok(relaykit::responses_tool_state(&serde_json::from_slice(raw)?)?)
}

pub fn convert_request_json(
    from: Format,
    to: Format,
    mut input: JsonValue,
    upstream_model: &str,
) -> Result<(JsonValue, ResponsesToolMapping), ProtocolError> {
    if !input.is_object() {
        return Err(ProtocolError::UnsupportedRequest("request must be a JSON object".into()));
    }
    input["model"] = json!(upstream_model);
    let default_max_tokens = if from == Format::OpenAiResponses
        && matches!(
            input.pointer("/reasoning/effort").and_then(JsonValue::as_str),
            Some("medium" | "high" | "xhigh" | "max" | "ultra")
        )
    {
        64_000
    } else if from == Format::OpenAiResponses {
        8192
    } else {
        4096
    };
    let mut ctx = context(upstream_model, default_max_tokens);
    ctx.is_stream = input.get("stream").and_then(JsonValue::as_bool).unwrap_or(false);
    let request = Request::from_value(from, input)?;
    let result = relaykit::convert_request(&ctx, to, &request)?;
    Ok((result.value.into_value(), ctx.responses_tool_state()))
}

pub fn convert_response_json(
    from: Format,
    to: Format,
    input: JsonValue,
    requested_model: &str,
    tools: &ResponsesToolMapping,
) -> Result<JsonValue, ProtocolError> {
    convert_response_json_with_options(from, to, input, requested_model, tools, true)
}

pub fn convert_response_json_with_options(
    from: Format,
    to: Format,
    mut input: JsonValue,
    requested_model: &str,
    tools: &ResponsesToolMapping,
    include_encrypted_reasoning: bool,
) -> Result<JsonValue, ProtocolError> {
    if from == Format::OpenAiResponses
        && matches!(
            input.get("type").and_then(JsonValue::as_str),
            Some("response.completed" | "response.incomplete" | "response.failed")
        )
        && let Some(response) = input.get_mut("response").filter(|value| value.is_object())
    {
        input = response.take();
    }
    let ctx = context(requested_model, 4096);
    ctx.set_responses_tool_state(tools.clone());
    let response = Response::from_value(from, input)?;
    let mut result = relaykit::convert_response(&ctx, to, &response)?.value.into_value();
    if result.is_object() && to != Format::Gemini && !requested_model.is_empty() {
        result["model"] = json!(requested_model);
    }
    filter_encrypted_reasoning(&mut result, include_encrypted_reasoning);
    Ok(result)
}

pub fn filter_encrypted_reasoning(value: &mut JsonValue, include: bool) {
    if include {
        return;
    }
    match value {
        JsonValue::Object(object) => {
            for key in ["encrypted_content", "reasoning_signature", "reasoning_content_signature"] {
                object.remove(key);
            }
            for child in object.values_mut() {
                filter_encrypted_reasoning(child, false);
            }
        }
        JsonValue::Array(values) => {
            for child in values {
                filter_encrypted_reasoning(child, false);
            }
        }
        _ => {}
    }
}

/// Keeps transport completion checks outside the pure conversion state machine.
pub struct RelaySseTranslator {
    inner: ResponseStreamState,
    ctx: Context,
    from: Format,
    terminal_seen: bool,
    closed: bool,
    failed: bool,
    include_encrypted_reasoning: bool,
}

impl RelaySseTranslator {
    pub fn new(
        from: Format,
        to: Format,
        requested_model: impl Into<String>,
        tools: ResponsesToolMapping,
    ) -> Self {
        let model = requested_model.into();
        let ctx = context(&model, 4096);
        ctx.set_responses_tool_state(tools);
        let inner = ResponseStreamState::new(from, to, StreamOptions {
            model,
            include_usage: true,
            emit_sequence_number: true,
            preserve_message_structure: true,
            allow_unpaired_hosted_search: true,
            ..StreamOptions::default()
        })
        .expect("all built-in protocol pairs have a stream converter");
        Self {
            inner,
            ctx,
            from,
            terminal_seen: false,
            closed: false,
            failed: false,
            include_encrypted_reasoning: true,
        }
    }

    pub fn with_encrypted_reasoning(mut self, include: bool) -> Self {
        self.include_encrypted_reasoning = include;
        self
    }

    pub fn translate_sse_payload(
        &mut self,
        payload: &str,
    ) -> Result<Vec<JsonValue>, ProtocolError> {
        if self.closed || self.failed || payload.trim().is_empty() {
            return Ok(Vec::new());
        }
        if payload.trim() == "[DONE]" {
            self.terminal_seen = true;
            return self.finish();
        }
        let value: JsonValue = serde_json::from_str(payload)?;
        self.terminal_seen |= match self.from {
            Format::OpenAi => value
                .get("choices")
                .and_then(JsonValue::as_array)
                .is_some_and(|choices| {
                    choices.iter().any(|choice| {
                        choice.get("finish_reason").is_some_and(|reason| !reason.is_null())
                    })
                }),
            Format::OpenAiResponses => matches!(
                value.get("type").and_then(JsonValue::as_str),
                Some("response.completed" | "response.incomplete" | "response.failed" | "error")
            ),
            Format::Claude => {
                value.get("type").and_then(JsonValue::as_str) == Some("message_stop")
                    || value
                        .pointer("/delta/stop_reason")
                        .is_some_and(|reason| !reason.is_null())
            }
            Format::Gemini => value
                .get("candidates")
                .and_then(JsonValue::as_array)
                .is_some_and(|candidates| {
                    candidates.iter().any(|candidate| {
                        candidate.get("finishReason").is_some_and(|reason| !reason.is_null())
                    })
                }),
        };
        let mut events = self.inner.convert_chunk(&self.ctx, &value)?;
        self.failed |= self.inner.is_failed();
        self.filter(&mut events);
        let explicit_terminal = match self.from {
            Format::OpenAiResponses => matches!(
                value.get("type").and_then(JsonValue::as_str),
                Some("response.completed" | "response.incomplete")
            ),
            Format::Claude => value.get("type").and_then(JsonValue::as_str) == Some("message_stop"),
            Format::Gemini => self.terminal_seen,
            Format::OpenAi => false,
        };
        if explicit_terminal && !self.failed {
            events.extend(self.finish()?);
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<JsonValue>, ProtocolError> {
        if self.closed || self.failed {
            return Ok(Vec::new());
        }
        if !self.terminal_seen {
            return Ok(self.fail(
                "incomplete_stream",
                "upstream stream ended before a terminal protocol event",
            ));
        }
        let mut events = self.inner.finalize(&self.ctx)?;
        self.closed = true;
        if self.inner.to == Format::Claude
            && !events.iter().any(|event| event["type"] == "message_delta")
        {
            let stop_reason = events
                .iter()
                .find_map(|event| event.get("stop_reason").and_then(JsonValue::as_str))
                .unwrap_or("end_turn");
            let terminal = json!({
                "type":"message_delta",
                "delta":{"stop_reason":stop_reason},
                "usage":{"output_tokens":0}
            });
            let position = events
                .iter()
                .position(|event| event["type"] == "message_stop")
                .unwrap_or(events.len());
            events.insert(position, terminal);
        }
        self.filter(&mut events);
        Ok(events)
    }

    /// Legacy Chat Completions facades historically synthesized a terminal
    /// chunk when Anthropic closed without `message_stop`. Keep that behavior
    /// at the facade boundary; the lower-level response stream remains strict.
    pub fn finish_at_eof(&mut self) -> Result<Vec<JsonValue>, ProtocolError> {
        if self.from == Format::Claude && !self.terminal_seen {
            return self.translate_sse_payload(r#"{"type":"message_stop"}"#);
        }
        self.finish()
    }

    pub fn fail(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Vec<JsonValue> {
        if self.failed || self.closed {
            return Vec::new();
        }
        self.failed = true;
        self.closed = true;
        let mut events = self.inner.fail_stream(&code.into(), &message.into(), "");
        self.filter(&mut events);
        events
    }

    pub fn is_complete(&self) -> bool {
        self.closed && !self.failed
    }

    pub fn is_failed(&self) -> bool {
        self.failed
    }

    pub fn from_format(&self) -> Format {
        self.from
    }

    pub fn usage(&self) -> Option<&JsonValue> {
        self.inner.usage()
    }

    fn filter(&self, events: &mut Vec<JsonValue>) {
        if !self.include_encrypted_reasoning {
            events.retain(|event| {
                !matches!(
                    event.get("type").and_then(JsonValue::as_str),
                    Some("response.reasoning_signature.delta" | "response.reasoning_signature.done")
                )
            });
        }
        for event in events {
            filter_encrypted_reasoning(event, self.include_encrypted_reasoning);
        }
    }
}

/// Channel cache policy, not protocol conversion.
pub fn apply_claude_prompt_cache(
    converted: &mut JsonValue,
    original: &JsonValue,
    mode: PromptCacheMode,
) {
    let mut cache = original
        .get("cache_control")
        .filter(|value| value.get("type").and_then(JsonValue::as_str) == Some("ephemeral"))
        .cloned()
        .unwrap_or_else(|| json!({"type":"ephemeral"}));
    cache["type"] = json!("ephemeral");
    if mode == PromptCacheMode::Automatic {
        converted["cache_control"] = cache.clone();
    } else if let Some(object) = converted.as_object_mut() {
        object.remove("cache_control");
    }
    if let Some(system) = converted.get_mut("system") {
        if let Some(text) = system.as_str() {
            *system = json!([{"type":"text","text":text}]);
        }
        if let Some(blocks) = system.as_array_mut() {
            for block in blocks.iter_mut() {
                if let Some(object) = block.as_object_mut() {
                    object.remove("cache_control");
                }
            }
            if let Some(last) = blocks.last_mut() {
                last["cache_control"] = cache.clone();
            }
        }
    }
    if let Some(tools) = converted.get_mut("tools").and_then(JsonValue::as_array_mut) {
        for tool in tools.iter_mut() {
            if let Some(object) = tool.as_object_mut() {
                object.remove("cache_control");
            }
        }
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = cache.clone();
        }
    }
    let reserved = usize::from(
        converted.get("system").and_then(JsonValue::as_array).is_some_and(|v| !v.is_empty()),
    ) + usize::from(
        converted.get("tools").and_then(JsonValue::as_array).is_some_and(|v| !v.is_empty()),
    );
    let Some(messages) = converted.get_mut("messages").and_then(JsonValue::as_array_mut) else {
        return;
    };
    for message in messages.iter_mut() {
        if let Some(blocks) = message.get_mut("content").and_then(JsonValue::as_array_mut) {
            for block in blocks {
                if let Some(object) = block.as_object_mut() {
                    object.remove("cache_control");
                }
            }
        }
    }
    if mode == PromptCacheMode::Automatic {
        return;
    }
    let last = messages.len().checked_sub(1);
    let penultimate_user = messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| message.get("role").and_then(JsonValue::as_str) == Some("user"))
        .nth(1)
        .map(|(index, _)| index);
    let mut marked = Vec::new();
    for index in [last, penultimate_user].into_iter().flatten() {
        if marked.len() >= 4usize.saturating_sub(reserved) || marked.contains(&index) {
            continue;
        }
        if let Some(blocks) = messages[index].get_mut("content").and_then(JsonValue::as_array_mut) {
            let text_index = blocks
                .iter()
                .rposition(|block| block.get("type").and_then(JsonValue::as_str) == Some("text"))
                .or_else(|| blocks.len().checked_sub(1));
            if let Some(block_index) = text_index {
                blocks[block_index]["cache_control"] = cache.clone();
                marked.push(index);
            }
        }
    }
}
