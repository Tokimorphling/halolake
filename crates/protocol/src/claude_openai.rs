use halolake_api_contract::{JsonValue, claude, openai};
use serde_json::json;
use std::{
    collections::{BTreeSet, HashMap},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use uuid::Uuid;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeOpenBlock {
    Text,
    Thinking,
    Tools,
}

pub fn openai_chat_to_claude_messages(
    req: &openai::ChatCompletionRequest,
    upstream_model: impl Into<String>,
) -> Result<claude::MessagesRequest, ProtocolError> {
    if req.messages.is_empty() {
        return Err(ProtocolError::EmptyMessages);
    }

    let normalized = normalize_messages(&req.messages);
    let mut system = Vec::new();
    let mut messages = Vec::with_capacity(normalized.len() + 1);

    for msg in &normalized {
        match msg.role.as_str() {
            "system" | "developer" => {
                if let Some(content) = &msg.content {
                    system.push(content.as_text_lossy());
                }
            }
            "assistant" => messages.push(claude::Message {
                role:    "assistant".to_string(),
                content: assistant_content_blocks(msg),
            }),
            "tool" | "function" => {
                let block = claude::ContentBlock::ToolResult {
                    tool_use_id:   msg.tool_call_id.clone().unwrap_or_default(),
                    content:       openai_content_to_claude_tool_result(msg.content.as_ref()),
                    is_error:      None,
                    cache_control: None,
                };
                if let Some(last) = messages.last_mut() {
                    if last.role == "user" {
                        last.content.push(block);
                        continue;
                    }
                }
                messages.push(claude::Message {
                    role:    "user".to_string(),
                    content: vec![block],
                });
            }
            _ => messages.push(claude::Message {
                role:    "user".to_string(),
                content: openai_content_to_claude_blocks(msg.content.as_ref()),
            }),
        }
    }

    if messages
        .first()
        .map(|message| message.role.as_str() != "user")
        .unwrap_or(false)
    {
        messages.insert(0, claude::Message {
            role:    "user".to_string(),
            content: vec![claude::ContentBlock::Text {
                text:          "...".to_string(),
                cache_control: None,
            }],
        });
    }

    let thinking =
        reasoning_to_claude_thinking(req.reasoning_effort.as_deref(), req.reasoning.as_ref());
    let mut max_tokens = req.max_tokens_value().unwrap_or(1024);
    if let Some(budget_tokens) = thinking
        .as_ref()
        .and_then(|thinking| thinking.budget_tokens)
    {
        max_tokens = max_tokens.max(budget_tokens.saturating_add(1024));
    }

    Ok(claude::MessagesRequest {
        model: upstream_model.into(),
        max_tokens,
        system: (!system.is_empty()).then(|| claude::SystemContent::Text(system.join("\n\n"))),
        cache_control: None,
        messages,
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: req.top_k,
        stream: req.stream,
        tools: req.tools.as_ref().map(|tools| convert_tools(tools)),
        tool_choice: convert_tool_choice(req.tool_choice.as_ref(), req.parallel_tool_calls),
        stop_sequences: convert_stop(req.stop.as_ref()),
        thinking,
        output_config: reasoning_to_claude_output_config(
            req.reasoning_effort.as_deref(),
            req.reasoning.as_ref(),
        ),
    })
}

pub fn claude_messages_to_openai_chat_request(
    req: &claude::MessagesRequest,
    upstream_model: impl Into<String>,
) -> Result<openai::ChatCompletionRequest, ProtocolError> {
    if req.messages.is_empty() {
        return Err(ProtocolError::EmptyMessages);
    }

    let mut messages = Vec::with_capacity(req.messages.len() + 1);
    if let Some(system) = &req.system {
        let text = system.as_text_lossy();
        if !text.is_empty() {
            messages.push(openai::ChatMessage {
                role:                "system".to_string(),
                content:             Some(openai::MessageContent::Text(text)),
                name:                None,
                tool_calls:          None,
                tool_call_id:        None,
                reasoning_content:   None,
                reasoning_signature: None,
            });
        }
    }

    for message in &req.messages {
        append_claude_message_as_openai(message, &mut messages);
    }

    Ok(openai::ChatCompletionRequest {
        model: upstream_model.into(),
        messages,
        max_tokens: Some(req.max_tokens),
        max_completion_tokens: None,
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: req.top_k,
        stream: req.stream,
        tools: req.tools.as_ref().map(|tools| {
            tools
                .iter()
                .map(|tool| openai::Tool {
                    tool_type: "function".to_string(),
                    function:  openai::FunctionTool {
                        name:        tool.name.clone(),
                        description: tool.description.clone(),
                        parameters:  Some(tool.input_schema.clone()),
                    },
                })
                .collect()
        }),
        tool_choice: convert_claude_tool_choice_to_openai(req.tool_choice.as_ref()),
        parallel_tool_calls: req.tool_choice.as_ref().and_then(|choice| {
            choice
                .get("disable_parallel_tool_use")
                .and_then(JsonValue::as_bool)
                .map(|disabled| !disabled)
        }),
        stop: (!req.stop_sequences.is_empty()).then(|| {
            if req.stop_sequences.len() == 1 {
                JsonValue::String(req.stop_sequences[0].clone())
            } else {
                JsonValue::Array(
                    req.stop_sequences
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                )
            }
        }),
        n: None,
        seed: None,
        response_format: None,
        stream_options: req
            .stream
            .unwrap_or(false)
            .then(|| json!({"include_usage": true})),
        reasoning_effort: req
            .output_config
            .as_ref()
            .and_then(|config| config.effort.clone()),
        reasoning: req.thinking.as_ref().map(|thinking| {
            json!({
                "type": thinking.thinking_type,
                "budget_tokens": thinking.budget_tokens,
                "display": thinking.display,
            })
        }),
    })
}

pub fn openai_chat_to_claude_messages_response(
    resp: openai::ChatCompletionResponse,
    requested_model: impl Into<String>,
) -> claude::MessagesResponse {
    let mut content = Vec::new();
    let mut stop_reason = None;

    for choice in resp.choices {
        stop_reason = choice.finish_reason.map(map_openai_finish_to_claude);
        match (
            choice
                .message
                .reasoning_content
                .filter(|text| !text.is_empty()),
            choice.message.reasoning_signature,
        ) {
            (Some(reasoning), signature) => content.push(claude::ContentBlock::Thinking {
                thinking: reasoning,
                signature,
                cache_control: None,
            }),
            (None, Some(data)) if !data.is_empty() => {
                content.push(claude::ContentBlock::RedactedThinking {
                    data,
                    cache_control: None,
                });
            }
            _ => {}
        }
        if let Some(message_content) = choice.message.content {
            let text = message_content.as_text_lossy();
            if !text.is_empty() || choice.message.tool_calls.as_ref().is_none_or(Vec::is_empty) {
                content.push(claude::ContentBlock::Text {
                    text,
                    cache_control: None,
                });
            }
        }
        if let Some(tool_calls) = choice.message.tool_calls {
            content.extend(
                tool_calls
                    .into_iter()
                    .map(|call| claude::ContentBlock::ToolUse {
                        id:            call.id,
                        name:          call.function.name,
                        input:         parse_tool_input_object(&call.function.arguments),
                        cache_control: None,
                    }),
            );
        }
    }

    claude::MessagesResponse {
        id: resp.id,
        response_type: "message".to_string(),
        role: "assistant".to_string(),
        model: requested_model.into(),
        content,
        stop_reason,
        usage: resp.usage.map(openai_usage_to_claude),
    }
}

pub fn claude_messages_to_openai_chat(
    resp: claude::MessagesResponse,
    requested_model: impl Into<String>,
) -> openai::ChatCompletionResponse {
    let (content, reasoning_content, reasoning_signature, tool_calls) =
        claude_content_to_openai(resp.content);
    openai::ChatCompletionResponse {
        id:      resp.id,
        object:  "chat.completion".to_string(),
        created: now_unix(),
        model:   requested_model.into(),
        choices: vec![openai::ChatChoice {
            index:         0,
            message:       openai::ChatMessage {
                role: "assistant".to_string(),
                content: Some(openai::MessageContent::Text(content)),
                name: None,
                tool_calls,
                tool_call_id: None,
                reasoning_content,
                reasoning_signature,
            },
            finish_reason: resp.stop_reason.map(map_stop_reason),
        }],
        usage:   resp.usage.map(convert_usage),
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiSseToClaudeTranslator {
    message_id:        String,
    requested_model:   String,
    started:           bool,
    done:              bool,
    block_index:       usize,
    open_block:        Option<ClaudeOpenBlock>,
    tool_base_index:   usize,
    open_tool_indices: BTreeSet<usize>,
    usage:             Option<claude::Usage>,
    finish_reason:     Option<String>,
    pending_signature: Option<String>,
}

impl OpenAiSseToClaudeTranslator {
    pub fn new(requested_model: impl Into<String>) -> Self {
        Self {
            message_id:        format!("msg_{}", Uuid::new_v4().simple()),
            requested_model:   requested_model.into(),
            started:           false,
            done:              false,
            block_index:       0,
            open_block:        None,
            tool_base_index:   0,
            open_tool_indices: BTreeSet::new(),
            usage:             None,
            finish_reason:     None,
            pending_signature: None,
        }
    }

    pub fn translate_sse_payload(&mut self, payload: &str) -> Result<Vec<String>, ProtocolError> {
        if payload == "[DONE]" {
            return self.finish_if_needed();
        }

        let chunk: openai::ChatCompletionChunk = serde_json::from_str(payload)?;
        let mut out = Vec::new();

        if !self.started {
            self.started = true;
            if !chunk.id.is_empty() {
                self.message_id = chunk.id.clone();
            }
            if !chunk.model.is_empty() {
                self.requested_model = chunk.model.clone();
            }
            out.push(self.event(json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.requested_model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": self.usage.unwrap_or_default(),
                }
            }))?);
        }

        if let Some(usage) = chunk.usage {
            self.usage = Some(openai_usage_to_claude(usage));
        }

        if chunk.choices.is_empty() {
            return Ok(out);
        }

        for choice in chunk.choices {
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }

            if let Some(signature) = choice
                .delta
                .reasoning_signature
                .as_ref()
                .filter(|signature| !signature.is_empty())
            {
                self.ensure_single_block(&mut out, ClaudeOpenBlock::Thinking)?;
                match self.pending_signature.as_mut() {
                    Some(existing) => existing.push_str(signature),
                    None => self.pending_signature = Some(signature.clone()),
                }
            }

            if let Some(tool_calls) = choice.delta.tool_calls {
                if !tool_calls.is_empty() {
                    self.ensure_tools_block(&mut out)?;
                    for (fallback_offset, call) in tool_calls.into_iter().enumerate() {
                        let offset = call.index.unwrap_or(fallback_offset as u32) as usize;
                        let index = self.tool_base_index + offset;
                        if !call.function.name.is_empty()
                            && !self.open_tool_indices.contains(&index)
                        {
                            self.open_tool_indices.insert(index);
                            out.push(self.event(json!({
                                "type": "content_block_start",
                                "index": index,
                                "content_block": {
                                    "type": "tool_use",
                                    "id": if call.id.is_empty() { format!("call_{index}") } else { call.id },
                                    "name": call.function.name,
                                    "input": {},
                                }
                            }))?);
                        }
                        if !call.function.arguments.is_empty() {
                            out.push(self.event(json!({
                                "type": "content_block_delta",
                                "index": index,
                                "delta": {
                                    "type": "input_json_delta",
                                    "partial_json": call.function.arguments,
                                }
                            }))?);
                        }
                    }
                }
            }
            if let Some(reasoning) = choice
                .delta
                .reasoning_content
                .or(choice.delta.reasoning)
                .filter(|text| !text.is_empty())
            {
                self.ensure_single_block(&mut out, ClaudeOpenBlock::Thinking)?;
                out.push(self.event(json!({
                    "type": "content_block_delta",
                    "index": self.block_index,
                    "delta": {
                        "type": "thinking_delta",
                        "thinking": reasoning,
                    }
                }))?);
            }
            if let Some(content) = choice.delta.content.filter(|text| !text.is_empty()) {
                self.ensure_single_block(&mut out, ClaudeOpenBlock::Text)?;
                out.push(self.event(json!({
                    "type": "content_block_delta",
                    "index": self.block_index,
                    "delta": {
                        "type": "text_delta",
                        "text": content,
                    }
                }))?);
            }
        }

        Ok(out)
    }

    pub fn finish(&mut self) -> Result<Vec<String>, ProtocolError> {
        self.finish_if_needed()
    }

    fn ensure_single_block(
        &mut self,
        out: &mut Vec<String>,
        kind: ClaudeOpenBlock,
    ) -> Result<(), ProtocolError> {
        if self.open_block == Some(kind) {
            return Ok(());
        }
        self.close_open_blocks(out)?;
        self.open_block = Some(kind);
        let content_block = match kind {
            ClaudeOpenBlock::Text => json!({"type": "text", "text": ""}),
            ClaudeOpenBlock::Thinking => json!({"type": "thinking", "thinking": ""}),
            ClaudeOpenBlock::Tools => unreachable!("tools use ensure_tools_block"),
        };
        out.push(self.event(json!({
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": content_block,
        }))?);
        Ok(())
    }

    fn ensure_tools_block(&mut self, out: &mut Vec<String>) -> Result<(), ProtocolError> {
        if self.open_block == Some(ClaudeOpenBlock::Tools) {
            return Ok(());
        }
        self.close_open_blocks(out)?;
        self.open_block = Some(ClaudeOpenBlock::Tools);
        self.tool_base_index = self.block_index;
        Ok(())
    }

    fn close_open_blocks(&mut self, out: &mut Vec<String>) -> Result<(), ProtocolError> {
        match self.open_block {
            Some(ClaudeOpenBlock::Text | ClaudeOpenBlock::Thinking) => {
                if self.open_block == Some(ClaudeOpenBlock::Thinking)
                    && let Some(signature) = self.pending_signature.take()
                {
                    out.push(self.event(json!({
                        "type": "content_block_delta",
                        "index": self.block_index,
                        "delta": {
                            "type": "signature_delta",
                            "signature": signature,
                        }
                    }))?);
                }
                out.push(self.event(json!({
                    "type": "content_block_stop",
                    "index": self.block_index,
                }))?);
                self.block_index += 1;
            }
            Some(ClaudeOpenBlock::Tools) => {
                for index in std::mem::take(&mut self.open_tool_indices) {
                    out.push(self.event(json!({
                        "type": "content_block_stop",
                        "index": index,
                    }))?);
                    self.block_index = self.block_index.max(index + 1);
                }
            }
            None => {}
        }
        self.open_block = None;
        Ok(())
    }

    fn finish_if_needed(&mut self) -> Result<Vec<String>, ProtocolError> {
        if self.done {
            return Ok(Vec::new());
        }
        self.done = true;

        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            out.push(self.event(json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.requested_model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": self.usage.unwrap_or_default(),
                }
            }))?);
        }
        self.close_open_blocks(&mut out)?;
        out.push(self.event(json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": map_openai_finish_to_claude(
                    self.finish_reason.clone().unwrap_or_else(|| "stop".to_string())
                ),
                "stop_sequence": null,
            },
            "usage": self.usage.unwrap_or_default(),
        }))?);
        out.push(self.event(json!({"type": "message_stop"}))?);
        Ok(out)
    }

    fn event(&self, value: JsonValue) -> Result<String, ProtocolError> {
        Ok(serde_json::to_string(&value)?)
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeSseTranslator {
    completion_id:   String,
    created:         u64,
    requested_model: String,
    emitted_role:    bool,
    usage:           Option<openai::Usage>,
    tool_indices:    HashMap<usize, u32>,
    next_tool_index: u32,
    finish_reason:   Option<String>,
    done:            bool,
}

impl ClaudeSseTranslator {
    pub fn new(requested_model: impl Into<String>) -> Self {
        Self {
            completion_id:   format!("chatcmpl-{}", Uuid::new_v4().simple()),
            created:         now_unix(),
            requested_model: requested_model.into(),
            emitted_role:    false,
            usage:           None,
            tool_indices:    HashMap::new(),
            next_tool_index: 0,
            finish_reason:   None,
            done:            false,
        }
    }

    pub fn translate_sse_payload(&mut self, payload: &str) -> Result<Vec<String>, ProtocolError> {
        if payload == "[DONE]" {
            return self.finish_if_needed();
        }
        let event: claude::StreamEvent = serde_json::from_str(payload)?;
        let mut out = Vec::new();

        match event.event_type.as_str() {
            "message_start" => {
                if let Some(message) = event.message {
                    self.usage = message.usage.map(convert_usage);
                    if !message.id.is_empty() {
                        self.completion_id = message.id;
                    }
                    if !message.model.is_empty() {
                        self.requested_model = message.model;
                    }
                }
                if !self.emitted_role {
                    self.emitted_role = true;
                    out.push(self.openai_sse(Some("assistant"), None, None, None)?);
                }
            }
            "content_block_start" => match event.content_block {
                Some(claude::ContentBlock::Text { text, .. }) if !text.is_empty() => {
                    out.push(self.openai_sse(None, Some(&text), None, None)?);
                }
                Some(claude::ContentBlock::Thinking {
                    thinking,
                    signature,
                    ..
                }) => {
                    if !thinking.is_empty() || signature.is_some() {
                        out.push(self.openai_reasoning_sse(Some(&thinking), signature.as_deref())?);
                    }
                }
                Some(claude::ContentBlock::RedactedThinking { data, .. }) => {
                    out.push(self.openai_reasoning_sse(None, Some(&data))?);
                }
                Some(claude::ContentBlock::ToolUse { id, name, .. }) => {
                    let block_index = event.index.unwrap_or_default();
                    let tool_index = self.tool_index(block_index);
                    out.push(self.openai_tool_sse(tool_index, Some(id), Some(name), "")?);
                }
                _ => {}
            },
            "content_block_delta" => {
                if let Some(delta) = event.delta {
                    if delta.get("type").and_then(JsonValue::as_str) == Some("text_delta") {
                        if let Some(text) = delta.get("text").and_then(JsonValue::as_str) {
                            out.push(self.openai_sse(None, Some(text), None, None)?);
                        }
                    } else if delta.get("type").and_then(JsonValue::as_str)
                        == Some("input_json_delta")
                    {
                        if let Some(partial_json) =
                            delta.get("partial_json").and_then(JsonValue::as_str)
                        {
                            let tool_index = self.tool_index(event.index.unwrap_or_default());
                            out.push(self.openai_tool_sse(tool_index, None, None, partial_json)?);
                        }
                    } else if delta.get("type").and_then(JsonValue::as_str)
                        == Some("thinking_delta")
                    {
                        if let Some(thinking) = delta.get("thinking").and_then(JsonValue::as_str) {
                            out.push(self.openai_reasoning_sse(Some(thinking), None)?);
                        }
                    } else if delta.get("type").and_then(JsonValue::as_str)
                        == Some("signature_delta")
                    {
                        if let Some(signature) = delta.get("signature").and_then(JsonValue::as_str)
                        {
                            out.push(self.openai_reasoning_sse(None, Some(signature))?);
                        }
                    }
                }
            }
            "message_delta" => {
                if let Some(usage) = event.usage {
                    merge_openai_usage(&mut self.usage, usage);
                }
                if let Some(delta) = event.delta {
                    if let Some(stop_reason) = delta.get("stop_reason").and_then(JsonValue::as_str)
                    {
                        self.finish_reason = Some(map_stop_reason(stop_reason.to_string()));
                    }
                }
            }
            "message_stop" => {
                out.extend(self.finish_if_needed()?);
            }
            _ => {}
        }

        Ok(out)
    }

    pub fn finish(&mut self) -> Result<Vec<String>, ProtocolError> {
        self.finish_if_needed()
    }

    fn finish_if_needed(&mut self) -> Result<Vec<String>, ProtocolError> {
        if self.done {
            return Ok(Vec::new());
        }
        self.done = true;
        let mut out = Vec::new();
        if !self.emitted_role {
            self.emitted_role = true;
            out.push(self.openai_sse(Some("assistant"), None, None, None)?);
        }
        out.push(
            self.openai_sse(
                None,
                None,
                Some(
                    self.finish_reason
                        .clone()
                        .unwrap_or_else(|| "stop".to_string()),
                ),
                self.usage,
            )?,
        );
        out.push("[DONE]".to_string());
        Ok(out)
    }

    fn openai_sse(
        &self,
        role: Option<&str>,
        content: Option<&str>,
        finish_reason: Option<String>,
        usage: Option<openai::Usage>,
    ) -> Result<String, ProtocolError> {
        let chunk = openai::ChatCompletionChunk {
            id: self.completion_id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: self.created,
            model: self.requested_model.clone(),
            choices: vec![openai::ChatChunkChoice {
                index: 0,
                delta: openai::ChatChunkDelta {
                    role:                role.map(str::to_string),
                    content:             content.map(str::to_string),
                    reasoning_content:   None,
                    reasoning:           None,
                    reasoning_signature: None,
                    tool_calls:          None,
                },
                finish_reason,
            }],
            usage,
        };
        Ok(serde_json::to_string(&chunk)?)
    }

    fn openai_tool_sse(
        &self,
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments: &str,
    ) -> Result<String, ProtocolError> {
        let chunk = openai::ChatCompletionChunk {
            id:      self.completion_id.clone(),
            object:  "chat.completion.chunk".to_string(),
            created: self.created,
            model:   self.requested_model.clone(),
            choices: vec![openai::ChatChunkChoice {
                index:         0,
                delta:         openai::ChatChunkDelta {
                    role:                None,
                    content:             None,
                    reasoning_content:   None,
                    reasoning:           None,
                    reasoning_signature: None,
                    tool_calls:          Some(vec![openai::ToolCall {
                        id:                id.unwrap_or_default(),
                        tool_type:         "function".to_string(),
                        function:          openai::ToolCallFunction {
                            name:      name.unwrap_or_default(),
                            arguments: arguments.to_string(),
                        },
                        index:             Some(index),
                        thought_signature: None,
                    }]),
                },
                finish_reason: None,
            }],
            usage:   None,
        };
        Ok(serde_json::to_string(&chunk)?)
    }

    fn openai_reasoning_sse(
        &self,
        reasoning: Option<&str>,
        signature: Option<&str>,
    ) -> Result<String, ProtocolError> {
        let chunk = openai::ChatCompletionChunk {
            id:      self.completion_id.clone(),
            object:  "chat.completion.chunk".to_string(),
            created: self.created,
            model:   self.requested_model.clone(),
            choices: vec![openai::ChatChunkChoice {
                index:         0,
                delta:         openai::ChatChunkDelta {
                    reasoning_content: reasoning.map(str::to_string),
                    reasoning_signature: signature.map(str::to_string),
                    ..openai::ChatChunkDelta::default()
                },
                finish_reason: None,
            }],
            usage:   None,
        };
        Ok(serde_json::to_string(&chunk)?)
    }

    fn tool_index(&mut self, block_index: usize) -> u32 {
        if let Some(index) = self.tool_indices.get(&block_index) {
            return *index;
        }
        let index = self.next_tool_index;
        self.next_tool_index = self.next_tool_index.saturating_add(1);
        self.tool_indices.insert(block_index, index);
        index
    }
}

fn append_claude_message_as_openai(message: &claude::Message, out: &mut Vec<openai::ChatMessage>) {
    let role = match message.role.as_str() {
        "assistant" => "assistant",
        "model" => "assistant",
        _ => "user",
    }
    .to_string();

    let mut parts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut reasoning_content = None;
    let mut reasoning_signature = None;

    for block in &message.content {
        match block {
            claude::ContentBlock::Text { text, .. } => {
                if !text.is_empty() {
                    parts.push(openai::ContentPart::Text {
                        part_type: "text".to_string(),
                        text:      text.clone(),
                    });
                }
            }
            claude::ContentBlock::Image { source, .. } => {
                if let Some(url) = claude_image_source_to_openai_url(source) {
                    parts.push(openai::ContentPart::ImageUrl {
                        part_type: "image_url".to_string(),
                        image_url: openai::ImageUrl {
                            url,
                            detail: Some("auto".to_string()),
                        },
                    });
                }
            }
            claude::ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                tool_calls.push(openai::ToolCall {
                    id:                id.clone(),
                    tool_type:         "function".to_string(),
                    function:          openai::ToolCallFunction {
                        name:      name.clone(),
                        arguments: input.to_string(),
                    },
                    index:             None,
                    thought_signature: None,
                });
            }
            claude::ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                if !parts.is_empty() || !tool_calls.is_empty() {
                    out.push(openai_message_from_parts(
                        role.clone(),
                        std::mem::take(&mut parts),
                        std::mem::take(&mut tool_calls),
                        None,
                        None,
                    ));
                }
                out.push(openai::ChatMessage {
                    role:                "tool".to_string(),
                    content:             Some(openai::MessageContent::Text(
                        content.as_text_lossy(),
                    )),
                    name:                None,
                    tool_calls:          None,
                    tool_call_id:        Some(tool_use_id.clone()),
                    reasoning_content:   None,
                    reasoning_signature: None,
                });
            }
            claude::ContentBlock::Thinking {
                thinking,
                signature,
                ..
            } => {
                reasoning_content = append_optional_string(reasoning_content, thinking);
                if signature.is_some() {
                    reasoning_signature = signature.clone();
                }
            }
            claude::ContentBlock::RedactedThinking { data, .. } => {
                if !data.is_empty() {
                    reasoning_signature = Some(data.clone());
                }
            }
            claude::ContentBlock::ServerToolUse { .. }
            | claude::ContentBlock::WebSearchToolResult { .. } => {
                // Server-side tools are executed by the Anthropic upstream and
                // have no safe Chat Completions equivalent. Preserve the
                // surrounding text/reasoning while omitting the internal
                // execution marker.
            }
        }
    }

    if !parts.is_empty()
        || !tool_calls.is_empty()
        || reasoning_content.is_some()
        || reasoning_signature.is_some()
    {
        out.push(openai_message_from_parts(
            role,
            parts,
            tool_calls,
            reasoning_content,
            reasoning_signature,
        ));
    }
}

fn append_optional_string(current: Option<String>, value: &str) -> Option<String> {
    if value.is_empty() {
        return current;
    }
    Some(match current {
        Some(mut current) => {
            current.push_str(value);
            current
        }
        None => value.to_string(),
    })
}

fn openai_message_from_parts(
    role: String,
    parts: Vec<openai::ContentPart>,
    tool_calls: Vec<openai::ToolCall>,
    reasoning_content: Option<String>,
    reasoning_signature: Option<String>,
) -> openai::ChatMessage {
    let content = if parts.is_empty() {
        None
    } else if parts.len() == 1 {
        match parts.into_iter().next().unwrap() {
            openai::ContentPart::Text { text, .. } => Some(openai::MessageContent::Text(text)),
            other => Some(openai::MessageContent::Parts(vec![other])),
        }
    } else {
        Some(openai::MessageContent::Parts(parts))
    };

    openai::ChatMessage {
        role,
        content,
        name: None,
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        tool_call_id: None,
        reasoning_content,
        reasoning_signature,
    }
}

fn claude_image_source_to_openai_url(source: &claude::ImageSource) -> Option<String> {
    if let Some(url) = &source.url {
        return Some(url.clone());
    }
    let media_type = source.media_type.as_deref()?;
    let data = source.data.as_deref()?;
    Some(format!("data:{media_type};base64,{data}"))
}

fn normalize_messages(messages: &[openai::ChatMessage]) -> Vec<openai::ChatMessage> {
    let mut out: Vec<openai::ChatMessage> = Vec::with_capacity(messages.len());

    for message in messages {
        let mut message = message.clone();
        if message.role.is_empty() {
            message.role = "user".to_string();
        }
        if message_content_is_empty(message.content.as_ref())
            && message.tool_calls.as_ref().is_none_or(Vec::is_empty)
            && message
                .reasoning_content
                .as_deref()
                .is_none_or(str::is_empty)
            && message
                .reasoning_signature
                .as_deref()
                .is_none_or(str::is_empty)
        {
            message.content = Some(openai::MessageContent::Text("...".to_string()));
        }

        if let Some(last) = out.last_mut() {
            if last.role == message.role
                && last.role != "tool"
                && last.tool_calls.is_none()
                && message.tool_calls.is_none()
                && last.reasoning_content.is_none()
                && message.reasoning_content.is_none()
                && last.reasoning_signature.is_none()
                && message.reasoning_signature.is_none()
            {
                last.content = merge_openai_message_content(last.content.take(), message.content);
                continue;
            }
        }

        out.push(message);
    }

    out
}

fn message_content_is_empty(content: Option<&openai::MessageContent>) -> bool {
    match content {
        None => true,
        Some(openai::MessageContent::Text(text)) => text.is_empty(),
        Some(openai::MessageContent::Parts(parts)) => parts.is_empty(),
    }
}

fn merge_openai_message_content(
    left: Option<openai::MessageContent>,
    right: Option<openai::MessageContent>,
) -> Option<openai::MessageContent> {
    match (left, right) {
        (None, content) | (content, None) => content,
        (
            Some(openai::MessageContent::Text(mut left)),
            Some(openai::MessageContent::Text(right)),
        ) => {
            if !left.is_empty() && !right.is_empty() {
                left.push(' ');
            }
            left.push_str(&right);
            Some(openai::MessageContent::Text(left))
        }
        (Some(left), Some(right)) => {
            let mut parts = message_content_into_parts(left);
            parts.extend(message_content_into_parts(right));
            Some(openai::MessageContent::Parts(parts))
        }
    }
}

fn message_content_into_parts(content: openai::MessageContent) -> Vec<openai::ContentPart> {
    match content {
        openai::MessageContent::Text(text) => vec![openai::ContentPart::Text {
            part_type: "text".to_string(),
            text,
        }],
        openai::MessageContent::Parts(parts) => parts,
    }
}

fn assistant_content_blocks(msg: &openai::ChatMessage) -> Vec<claude::ContentBlock> {
    let mut blocks = Vec::new();
    if msg.reasoning_content.is_some() || msg.reasoning_signature.is_some() {
        blocks.push(claude::ContentBlock::Thinking {
            thinking:      msg.reasoning_content.clone().unwrap_or_default(),
            signature:     msg.reasoning_signature.clone(),
            cache_control: None,
        });
    }
    if let Some(content) = &msg.content {
        blocks.extend(openai_content_to_claude_blocks(Some(content)));
    }
    if let Some(tool_calls) = &msg.tool_calls {
        blocks.extend(tool_calls.iter().map(|call| claude::ContentBlock::ToolUse {
            id:            call.id.clone(),
            name:          call.function.name.clone(),
            input:         parse_tool_input_object(&call.function.arguments),
            cache_control: None,
        }));
    }
    if blocks.is_empty() {
        blocks.push(claude::ContentBlock::Text {
            text:          String::new(),
            cache_control: None,
        });
    }
    blocks
}

fn parse_tool_input_object(arguments: &str) -> JsonValue {
    let Ok(value) = serde_json::from_str::<JsonValue>(arguments) else {
        return json!({});
    };
    if value.is_object() { value } else { json!({}) }
}

fn convert_tools(tools: &[openai::Tool]) -> Vec<claude::Tool> {
    tools
        .iter()
        .filter(|tool| tool.tool_type == "function")
        .map(|tool| claude::Tool {
            tool_type:     None,
            name:          tool.function.name.clone(),
            description:   tool.function.description.clone(),
            input_schema:  tool
                .function
                .parameters
                .clone()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            cache_control: None,
        })
        .collect()
}

fn convert_tool_choice(
    choice: Option<&JsonValue>,
    parallel_tool_calls: Option<bool>,
) -> Option<JsonValue> {
    let mut converted = match choice {
        Some(choice) if choice == "auto" => Some(json!({"type": "auto"})),
        Some(choice) if choice == "required" => Some(json!({"type": "any"})),
        Some(choice) if choice == "none" => Some(json!({"type": "none"})),
        Some(choice) => {
            let function_name = choice
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(JsonValue::as_str)?;
            Some(json!({"type": "tool", "name": function_name}))
        }
        None => parallel_tool_calls.map(|_| json!({"type": "auto"})),
    }?;

    if converted.get("type").and_then(JsonValue::as_str) != Some("none") {
        if let Some(parallel) = parallel_tool_calls {
            converted["disable_parallel_tool_use"] = JsonValue::Bool(!parallel);
        }
    }
    Some(converted)
}

fn convert_claude_tool_choice_to_openai(choice: Option<&JsonValue>) -> Option<JsonValue> {
    let choice = choice?;
    match choice.get("type").and_then(JsonValue::as_str) {
        Some("auto") => Some(json!("auto")),
        Some("any") => Some(json!("required")),
        Some("none") => Some(json!("none")),
        Some("tool") => choice
            .get("name")
            .and_then(JsonValue::as_str)
            .map(|name| json!({"type": "function", "function": {"name": name}})),
        _ => Some(choice.clone()),
    }
}

fn convert_stop(stop: Option<&JsonValue>) -> Vec<String> {
    match stop {
        Some(JsonValue::String(stop)) => vec![stop.clone()],
        Some(JsonValue::Array(stops)) => stops
            .iter()
            .filter_map(JsonValue::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn claude_content_to_openai(
    content: Vec<claude::ContentBlock>,
) -> (
    String,
    Option<String>,
    Option<String>,
    Option<Vec<openai::ToolCall>>,
) {
    let mut text = Vec::new();
    let mut reasoning = Vec::new();
    let mut reasoning_signature = None;
    let mut tool_calls = Vec::new();

    for block in content {
        match block {
            claude::ContentBlock::Text { text: t, .. } => text.push(t),
            claude::ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                tool_calls.push(openai::ToolCall {
                    id,
                    tool_type: "function".to_string(),
                    function: openai::ToolCallFunction {
                        name,
                        arguments: input.to_string(),
                    },
                    index: None,
                    thought_signature: None,
                });
            }
            claude::ContentBlock::Thinking {
                thinking,
                signature,
                ..
            } => {
                if !thinking.is_empty() {
                    reasoning.push(thinking);
                }
                if signature.is_some() {
                    reasoning_signature = signature;
                }
            }
            claude::ContentBlock::RedactedThinking { data, .. } => {
                if !data.is_empty() {
                    reasoning_signature = Some(data);
                }
            }
            claude::ContentBlock::Image { .. }
            | claude::ContentBlock::ToolResult { .. }
            | claude::ContentBlock::ServerToolUse { .. }
            | claude::ContentBlock::WebSearchToolResult { .. } => {}
        }
    }

    (
        text.join(""),
        (!reasoning.is_empty()).then(|| reasoning.join("")),
        reasoning_signature,
        (!tool_calls.is_empty()).then_some(tool_calls),
    )
}

fn openai_content_to_claude_blocks(
    content: Option<&openai::MessageContent>,
) -> Vec<claude::ContentBlock> {
    let mut blocks = Vec::new();
    match content {
        Some(openai::MessageContent::Text(text)) => {
            blocks.push(claude::ContentBlock::Text {
                text:          text.clone(),
                cache_control: None,
            });
        }
        Some(openai::MessageContent::Parts(parts)) => {
            for part in parts {
                match part {
                    openai::ContentPart::Text { text, .. } => {
                        blocks.push(claude::ContentBlock::Text {
                            text:          text.clone(),
                            cache_control: None,
                        });
                    }
                    openai::ContentPart::ImageUrl { image_url, .. } => {
                        if let Some(source) = openai_image_url_to_claude_source(&image_url.url) {
                            blocks.push(claude::ContentBlock::Image {
                                source,
                                cache_control: None,
                            });
                        }
                    }
                    openai::ContentPart::Other(value) => {
                        if let Some(text) = value.get("text").and_then(JsonValue::as_str) {
                            blocks.push(claude::ContentBlock::Text {
                                text:          text.to_string(),
                                cache_control: None,
                            });
                        }
                    }
                }
            }
        }
        None => {}
    }
    if blocks.is_empty() {
        blocks.push(claude::ContentBlock::Text {
            text:          String::new(),
            cache_control: None,
        });
    }
    blocks
}

fn openai_content_to_claude_tool_result(
    content: Option<&openai::MessageContent>,
) -> claude::ToolResultContent {
    let blocks = openai_content_to_claude_blocks(content);
    if blocks.len() == 1
        && let claude::ContentBlock::Text { text, .. } = &blocks[0]
    {
        return claude::ToolResultContent::Text(text.clone());
    }
    claude::ToolResultContent::Blocks(blocks)
}

fn openai_image_url_to_claude_source(url: &str) -> Option<claude::ImageSource> {
    if let Some(data_url) = url.strip_prefix("data:") {
        let (metadata, data) = data_url.split_once(',')?;
        let media_type = metadata.strip_suffix(";base64")?;
        if media_type.is_empty() || data.is_empty() {
            return None;
        }
        return Some(claude::ImageSource {
            source_type: "base64".to_string(),
            media_type:  Some(media_type.to_string()),
            data:        Some(data.to_string()),
            url:         None,
        });
    }
    (!url.is_empty()).then(|| claude::ImageSource {
        source_type: "url".to_string(),
        media_type:  None,
        data:        None,
        url:         Some(url.to_string()),
    })
}

fn reasoning_to_claude_thinking(
    effort: Option<&str>,
    reasoning: Option<&JsonValue>,
) -> Option<claude::ThinkingConfig> {
    let reasoning_type = reasoning
        .and_then(|value| value.get("type"))
        .and_then(JsonValue::as_str);
    if matches!(reasoning_type, Some("disabled" | "none")) {
        return None;
    }

    let budget_tokens = reasoning
        .and_then(|value| {
            value
                .get("max_tokens")
                .or_else(|| value.get("budget_tokens"))
        })
        .and_then(JsonValue::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    let effective_effort = effort.or_else(|| {
        reasoning
            .and_then(|value| value.get("effort"))
            .and_then(JsonValue::as_str)
    });
    if budget_tokens.is_none() && effective_effort.is_none() && reasoning_type.is_none() {
        return None;
    }

    let thinking_type = reasoning_type.unwrap_or_else(|| {
        if budget_tokens.is_some() {
            "enabled"
        } else {
            "adaptive"
        }
    });
    let budget_tokens = budget_tokens.or_else(|| match effective_effort {
        Some("low") => Some(1024),
        Some("medium") => Some(4096),
        Some("high") => Some(10240),
        Some("xhigh" | "max") => Some(32768),
        _ if thinking_type == "enabled" => Some(10240),
        _ => None,
    });

    Some(claude::ThinkingConfig {
        thinking_type: thinking_type.to_string(),
        budget_tokens,
        display: reasoning
            .and_then(|value| value.get("display"))
            .and_then(JsonValue::as_str)
            .map(str::to_string),
    })
}

fn reasoning_to_claude_output_config(
    effort: Option<&str>,
    reasoning: Option<&JsonValue>,
) -> Option<claude::OutputConfig> {
    let effort = effort
        .or_else(|| {
            reasoning
                .and_then(|value| value.get("effort"))
                .and_then(JsonValue::as_str)
        })
        .map(|value| if value == "xhigh" { "max" } else { value });
    effort.map(|effort| claude::OutputConfig {
        effort: Some(effort.to_string()),
    })
}

fn merge_openai_usage(usage: &mut Option<openai::Usage>, delta: claude::Usage) {
    let delta = convert_usage(delta);
    if let Some(current) = usage {
        current.prompt_tokens = current.prompt_tokens.max(delta.prompt_tokens);
        current.completion_tokens = current.completion_tokens.max(delta.completion_tokens);
        current.total_tokens = current
            .prompt_tokens
            .saturating_add(current.completion_tokens);
        current.cached_tokens = current.cached_tokens.max(delta.cached_tokens);
        current.prompt_tokens_details.cached_tokens = current
            .prompt_tokens_details
            .cached_tokens
            .max(delta.prompt_tokens_details.cached_tokens);
        current.prompt_tokens_details.cached_creation_tokens = current
            .prompt_tokens_details
            .cached_creation_tokens
            .max(delta.prompt_tokens_details.cached_creation_tokens);
    } else {
        *usage = Some(delta);
    }
}

fn convert_usage(usage: claude::Usage) -> openai::Usage {
    let prompt_tokens =
        usage.input_tokens + usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
    openai::Usage {
        prompt_tokens,
        completion_tokens: usage.output_tokens,
        total_tokens: prompt_tokens + usage.output_tokens,
        prompt_tokens_details: openai::TokenUsageDetails {
            cached_tokens: usage.cache_read_input_tokens,
            cached_creation_tokens: usage.cache_creation_input_tokens,
            ..openai::TokenUsageDetails::default()
        },
        input_tokens_details: None,
        cached_tokens: usage.cache_read_input_tokens,
    }
}

fn openai_usage_to_claude(usage: openai::Usage) -> claude::Usage {
    let cache_read = usage
        .prompt_tokens_details
        .cached_tokens
        .max(usage.cached_tokens)
        .max(
            usage
                .input_tokens_details
                .map(|details| details.cached_tokens)
                .unwrap_or(0),
        );
    let cache_creation = usage.prompt_tokens_details.cached_creation_tokens.max(
        usage
            .input_tokens_details
            .map(|details| details.cached_creation_tokens)
            .unwrap_or(0),
    );
    claude::Usage {
        input_tokens:                usage
            .prompt_tokens
            .saturating_sub(cache_read)
            .saturating_sub(cache_creation),
        output_tokens:               usage.completion_tokens,
        cache_creation_input_tokens: cache_creation,
        cache_read_input_tokens:     cache_read,
    }
}

fn map_stop_reason(reason: String) -> String {
    match reason.as_str() {
        "end_turn" => "stop",
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => reason.as_str(),
    }
    .to_string()
}

fn map_openai_finish_to_claude(reason: String) -> String {
    match reason.as_str() {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        "content_filter" => "refusal",
        _ => reason.as_str(),
    }
    .to_string()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> openai::ChatMessage {
        openai::ChatMessage {
            role:                role.into(),
            content:             Some(openai::MessageContent::Text(content.into())),
            name:                None,
            tool_calls:          None,
            tool_call_id:        None,
            reasoning_content:   None,
            reasoning_signature: None,
        }
    }

    #[test]
    fn converts_openai_chat_to_claude_messages() {
        let req = openai::ChatCompletionRequest {
            model:                 "gpt-4o-mini".into(),
            messages:              vec![
                openai::ChatMessage {
                    role:                "system".into(),
                    content:             Some(openai::MessageContent::Text(
                        "You are concise.".into(),
                    )),
                    name:                None,
                    tool_calls:          None,
                    tool_call_id:        None,
                    reasoning_content:   None,
                    reasoning_signature: None,
                },
                openai::ChatMessage {
                    role:                "user".into(),
                    content:             Some(openai::MessageContent::Text("Hello".into())),
                    name:                None,
                    tool_calls:          None,
                    tool_call_id:        None,
                    reasoning_content:   None,
                    reasoning_signature: None,
                },
            ],
            max_tokens:            Some(64),
            max_completion_tokens: None,
            temperature:           Some(0.2),
            top_p:                 None,
            top_k:                 None,
            stream:                Some(false),
            tools:                 None,
            tool_choice:           None,
            parallel_tool_calls:   None,
            stop:                  None,
            n:                     None,
            seed:                  None,
            response_format:       None,
            stream_options:        None,
            reasoning_effort:      None,
            reasoning:             None,
        };

        let claude = openai_chat_to_claude_messages(&req, "claude-sonnet-4").unwrap();
        assert_eq!(claude.model, "claude-sonnet-4");
        assert_eq!(
            claude
                .system
                .as_ref()
                .map(claude::SystemContent::as_text_lossy),
            Some("You are concise.".to_string())
        );
        assert_eq!(claude.messages.len(), 1);
    }

    #[test]
    fn normalizes_messages_like_new_api_claude_relay() {
        let req = openai::ChatCompletionRequest {
            model:                 "gpt-4o-mini".into(),
            messages:              vec![
                msg("assistant", "first"),
                msg("user", "hello"),
                msg("user", "again"),
            ],
            max_tokens:            None,
            max_completion_tokens: None,
            temperature:           None,
            top_p:                 None,
            top_k:                 None,
            stream:                None,
            tools:                 None,
            tool_choice:           None,
            parallel_tool_calls:   None,
            stop:                  None,
            n:                     None,
            seed:                  None,
            response_format:       None,
            stream_options:        None,
            reasoning_effort:      None,
            reasoning:             None,
        };

        let claude = openai_chat_to_claude_messages(&req, "claude").unwrap();
        assert_eq!(claude.messages[0].role, "user");
        assert!(matches!(
            claude.messages[0].content[0],
            claude::ContentBlock::Text { ref text, .. } if text == "..."
        ));
        assert_eq!(claude.messages[2].role, "user");
        assert!(matches!(
            claude.messages[2].content[0],
            claude::ContentBlock::Text { ref text, .. } if text == "hello again"
        ));
    }

    #[test]
    fn converts_tool_calls_with_object_arguments_only() {
        let req = openai::ChatCompletionRequest {
            model:                 "gpt-4o-mini".into(),
            messages:              vec![openai::ChatMessage {
                role:                "assistant".into(),
                content:             None,
                name:                None,
                tool_calls:          Some(vec![openai::ToolCall {
                    id:                "call_1".into(),
                    tool_type:         "function".into(),
                    function:          openai::ToolCallFunction {
                        name:      "lookup".into(),
                        arguments: "\"bad\"".into(),
                    },
                    index:             None,
                    thought_signature: None,
                }]),
                tool_call_id:        None,
                reasoning_content:   None,
                reasoning_signature: None,
            }],
            max_tokens:            None,
            max_completion_tokens: None,
            temperature:           None,
            top_p:                 None,
            top_k:                 None,
            stream:                None,
            tools:                 None,
            tool_choice:           None,
            parallel_tool_calls:   None,
            stop:                  None,
            n:                     None,
            seed:                  None,
            response_format:       None,
            stream_options:        None,
            reasoning_effort:      None,
            reasoning:             None,
        };

        let claude = openai_chat_to_claude_messages(&req, "claude").unwrap();
        assert!(matches!(
            claude.messages[1].content[0],
            claude::ContentBlock::ToolUse { ref input, .. } if input == &json!({})
        ));
    }

    #[test]
    fn maps_tool_choice_and_stop_reason_like_new_api() {
        assert_eq!(
            convert_tool_choice(Some(&json!("required")), Some(false)),
            Some(json!({"type": "any", "disable_parallel_tool_use": true}))
        );
        assert_eq!(map_stop_reason("refusal".into()), "content_filter");
    }

    #[test]
    fn preserves_multimodal_content_and_thinking_history() {
        let req = openai::ChatCompletionRequest {
            model:                 "gpt".into(),
            messages:              vec![
                openai::ChatMessage {
                    role:                "user".into(),
                    content:             Some(openai::MessageContent::Parts(vec![
                        openai::ContentPart::Text {
                            part_type: "text".into(),
                            text:      "look".into(),
                        },
                        openai::ContentPart::ImageUrl {
                            part_type: "image_url".into(),
                            image_url: openai::ImageUrl {
                                url:    "data:image/png;base64,AAAA".into(),
                                detail: Some("high".into()),
                            },
                        },
                    ])),
                    name:                None,
                    tool_calls:          None,
                    tool_call_id:        None,
                    reasoning_content:   None,
                    reasoning_signature: None,
                },
                openai::ChatMessage {
                    role:                "user".into(),
                    content:             Some(openai::MessageContent::Text("again".into())),
                    name:                None,
                    tool_calls:          None,
                    tool_call_id:        None,
                    reasoning_content:   None,
                    reasoning_signature: None,
                },
                openai::ChatMessage {
                    role:                "assistant".into(),
                    content:             Some(openai::MessageContent::Text("answer".into())),
                    name:                None,
                    tool_calls:          None,
                    tool_call_id:        None,
                    reasoning_content:   Some("thought".into()),
                    reasoning_signature: Some("sig".into()),
                },
            ],
            max_tokens:            Some(64),
            max_completion_tokens: None,
            temperature:           None,
            top_p:                 None,
            top_k:                 None,
            stream:                None,
            tools:                 None,
            tool_choice:           None,
            parallel_tool_calls:   None,
            stop:                  None,
            n:                     None,
            seed:                  None,
            response_format:       None,
            stream_options:        None,
            reasoning_effort:      Some("high".into()),
            reasoning:             None,
        };

        let converted = openai_chat_to_claude_messages(&req, "claude").unwrap();
        assert!(matches!(
            converted.messages[0].content[1],
            claude::ContentBlock::Image { ref source, .. } if source.data.as_deref() == Some("AAAA")
        ));
        assert!(matches!(
            converted.messages[1].content[0],
            claude::ContentBlock::Thinking { ref thinking, signature: Some(ref sig), .. }
                if thinking == "thought" && sig == "sig"
        ));
        assert_eq!(converted.max_tokens, 11264);
    }

    #[test]
    fn claude_stream_keeps_parallel_tool_indices_and_signature() {
        let mut translator = ClaudeSseTranslator::new("requested");
        let events = [
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[],"usage":{"input_tokens":2,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":null}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"why"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"one","input":{}}}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_2","name":"two","input":{}}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}),
            json!({"type":"message_stop"}),
        ];
        let mut output = Vec::new();
        for event in events {
            output.extend(
                translator
                    .translate_sse_payload(&event.to_string())
                    .unwrap(),
            );
        }
        let chunks: Vec<JsonValue> = output
            .iter()
            .filter(|payload| payload.as_str() != "[DONE]")
            .map(|payload| serde_json::from_str(payload).unwrap())
            .collect();
        assert!(
            chunks
                .iter()
                .any(|chunk| { chunk["choices"][0]["delta"]["reasoning_signature"] == "sig" })
        );
        let indices: Vec<u64> = chunks
            .iter()
            .filter_map(|chunk| chunk["choices"][0]["delta"]["tool_calls"][0]["index"].as_u64())
            .collect();
        assert_eq!(indices, vec![0, 1]);
        assert_eq!(output.last().map(String::as_str), Some("[DONE]"));
    }

    #[test]
    fn stream_translators_finalize_on_eof() {
        let mut claude_to_chat = ClaudeSseTranslator::new("requested");
        claude_to_chat
            .translate_sse_payload(
                &json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"partial"}}).to_string(),
            )
            .unwrap();
        let chat_terminal = claude_to_chat.finish().unwrap();
        assert_eq!(chat_terminal.last().map(String::as_str), Some("[DONE]"));

        let mut chat_to_claude = OpenAiSseToClaudeTranslator::new("requested");
        chat_to_claude
            .translate_sse_payload(
                &json!({"id":"chatcmpl_1","object":"chat.completion.chunk","created":1,"model":"upstream","choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":"stop"}]}).to_string(),
            )
            .unwrap();
        let claude_terminal = chat_to_claude.finish().unwrap();
        let parsed = claude_terminal
            .iter()
            .map(|event| serde_json::from_str::<JsonValue>(event).unwrap())
            .collect::<Vec<_>>();
        assert!(parsed.iter().any(|event| event["type"] == "message_delta"));
        assert_eq!(parsed.last().unwrap()["type"], "message_stop");
    }
}
