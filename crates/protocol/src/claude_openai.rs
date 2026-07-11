use halolake_api_contract::{JsonValue, claude, openai};
use serde_json::json;
use std::{
    collections::BTreeSet,
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
            "system" => {
                if let Some(content) = &msg.content {
                    system.push(content.as_text_lossy());
                }
            }
            "assistant" => messages.push(claude::Message {
                role:    "assistant".to_string(),
                content: assistant_content_blocks(msg),
            }),
            "tool" => {
                let block = claude::ContentBlock::ToolResult {
                    tool_use_id: msg.tool_call_id.clone().unwrap_or_default(),
                    content:     claude::ToolResultContent::Text(
                        msg.content
                            .as_ref()
                            .map(openai::MessageContent::as_text_lossy)
                            .unwrap_or_default(),
                    ),
                    is_error:    None,
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
                content: vec![claude::ContentBlock::Text {
                    text: msg
                        .content
                        .as_ref()
                        .map(openai::MessageContent::as_text_lossy)
                        .unwrap_or_default(),
                }],
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
                text: "...".to_string(),
            }],
        });
    }

    Ok(claude::MessagesRequest {
        model: upstream_model.into(),
        max_tokens: req.max_tokens_value().unwrap_or(1024),
        system: (!system.is_empty()).then(|| claude::SystemContent::Text(system.join("\n\n"))),
        messages,
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: req.top_k,
        stream: req.stream,
        tools: req.tools.as_ref().map(|tools| convert_tools(tools)),
        tool_choice: convert_tool_choice(req.tool_choice.as_ref(), req.parallel_tool_calls),
        stop_sequences: convert_stop(req.stop.as_ref()),
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
                role:              "system".to_string(),
                content:           Some(openai::MessageContent::Text(text)),
                name:              None,
                tool_calls:        None,
                tool_call_id:      None,
                reasoning_content: None,
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
        tool_choice: None,
        parallel_tool_calls: None,
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
        if let Some(message_content) = choice.message.content {
            let text = message_content.as_text_lossy();
            if !text.is_empty() || choice.message.tool_calls.as_ref().is_none_or(Vec::is_empty) {
                content.push(claude::ContentBlock::Text { text });
            }
        }
        if let Some(tool_calls) = choice.message.tool_calls {
            content.extend(
                tool_calls
                    .into_iter()
                    .map(|call| claude::ContentBlock::ToolUse {
                        id:    call.id,
                        name:  call.function.name,
                        input: parse_tool_input_object(&call.function.arguments),
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
    let (content, tool_calls) = claude_content_to_openai(resp.content);
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
                reasoning_content: None,
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
            } else if let Some(reasoning) = choice
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
            } else if let Some(content) = choice.delta.content.filter(|text| !text.is_empty()) {
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

        if self.finish_reason.is_some() {
            out.extend(self.finish_if_needed()?);
        }

        Ok(out)
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
}

impl ClaudeSseTranslator {
    pub fn new(requested_model: impl Into<String>) -> Self {
        Self {
            completion_id:   format!("chatcmpl-{}", Uuid::new_v4().simple()),
            created:         now_unix(),
            requested_model: requested_model.into(),
            emitted_role:    false,
            usage:           None,
        }
    }

    pub fn translate_sse_payload(&mut self, payload: &str) -> Result<Vec<String>, ProtocolError> {
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
                Some(claude::ContentBlock::Text { text }) if !text.is_empty() => {
                    out.push(self.openai_sse(None, Some(&text), None, None)?);
                }
                Some(claude::ContentBlock::ToolUse { id, name, .. }) => {
                    out.push(self.openai_tool_sse(Some(id), Some(name), "")?);
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
                            out.push(self.openai_tool_sse(None, None, partial_json)?);
                        }
                    }
                }
            }
            "message_delta" => {
                if let Some(usage) = event.usage {
                    self.usage = Some(convert_usage(usage));
                }
                if let Some(delta) = event.delta {
                    if let Some(stop_reason) = delta.get("stop_reason").and_then(JsonValue::as_str)
                    {
                        out.push(self.openai_sse(
                            None,
                            None,
                            Some(map_stop_reason(stop_reason.to_string())),
                            self.usage,
                        )?);
                    }
                }
            }
            "message_stop" => {
                out.push("[DONE]".to_string());
            }
            _ => {}
        }

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
                    role:              role.map(str::to_string),
                    content:           content.map(str::to_string),
                    reasoning_content: None,
                    reasoning:         None,
                    tool_calls:        None,
                },
                finish_reason,
            }],
            usage,
        };
        Ok(serde_json::to_string(&chunk)?)
    }

    fn openai_tool_sse(
        &self,
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
                    role:              None,
                    content:           None,
                    reasoning_content: None,
                    reasoning:         None,
                    tool_calls:        Some(vec![openai::ToolCall {
                        id:        id.unwrap_or_default(),
                        tool_type: "function".to_string(),
                        function:  openai::ToolCallFunction {
                            name:      name.unwrap_or_default(),
                            arguments: arguments.to_string(),
                        },
                        index:     Some(0),
                    }]),
                },
                finish_reason: None,
            }],
            usage:   None,
        };
        Ok(serde_json::to_string(&chunk)?)
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

    for block in &message.content {
        match block {
            claude::ContentBlock::Text { text } => {
                if !text.is_empty() {
                    parts.push(openai::ContentPart::Text {
                        part_type: "text".to_string(),
                        text:      text.clone(),
                    });
                }
            }
            claude::ContentBlock::Image { source } => {
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
            claude::ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(openai::ToolCall {
                    id:        id.clone(),
                    tool_type: "function".to_string(),
                    function:  openai::ToolCallFunction {
                        name:      name.clone(),
                        arguments: input.to_string(),
                    },
                    index:     None,
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
                    ));
                }
                out.push(openai::ChatMessage {
                    role:              "tool".to_string(),
                    content:           Some(openai::MessageContent::Text(content.as_text_lossy())),
                    name:              None,
                    tool_calls:        None,
                    tool_call_id:      Some(tool_use_id.clone()),
                    reasoning_content: None,
                });
            }
            claude::ContentBlock::Thinking { thinking, .. } => {
                if !thinking.is_empty() {
                    parts.push(openai::ContentPart::Text {
                        part_type: "text".to_string(),
                        text:      thinking.clone(),
                    });
                }
            }
        }
    }

    if !parts.is_empty() || !tool_calls.is_empty() {
        out.push(openai_message_from_parts(role, parts, tool_calls));
    }
}

fn openai_message_from_parts(
    role: String,
    parts: Vec<openai::ContentPart>,
    tool_calls: Vec<openai::ToolCall>,
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
        reasoning_content: None,
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
        if message.content.is_none()
            || message
                .content
                .as_ref()
                .is_some_and(|content| content.as_text_lossy().is_empty())
        {
            message.content = Some(openai::MessageContent::Text("...".to_string()));
        }

        if let Some(last) = out.last_mut() {
            if last.role == message.role
                && last.role != "tool"
                && last.tool_calls.is_none()
                && message.tool_calls.is_none()
            {
                let merged = format!(
                    "{} {}",
                    last.content
                        .as_ref()
                        .map(openai::MessageContent::as_text_lossy)
                        .unwrap_or_default(),
                    message
                        .content
                        .as_ref()
                        .map(openai::MessageContent::as_text_lossy)
                        .unwrap_or_default()
                )
                .trim()
                .to_string();
                last.content = Some(openai::MessageContent::Text(merged));
                continue;
            }
        }

        out.push(message);
    }

    out
}

fn assistant_content_blocks(msg: &openai::ChatMessage) -> Vec<claude::ContentBlock> {
    let mut blocks = Vec::new();
    if let Some(content) = &msg.content {
        let text = content.as_text_lossy();
        if !text.is_empty() {
            blocks.push(claude::ContentBlock::Text { text });
        }
    }
    if let Some(tool_calls) = &msg.tool_calls {
        blocks.extend(tool_calls.iter().map(|call| claude::ContentBlock::ToolUse {
            id:    call.id.clone(),
            name:  call.function.name.clone(),
            input: parse_tool_input_object(&call.function.arguments),
        }));
    }
    if blocks.is_empty() {
        blocks.push(claude::ContentBlock::Text {
            text: String::new(),
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
            name:         tool.function.name.clone(),
            description:  tool.function.description.clone(),
            input_schema: tool
                .function
                .parameters
                .clone()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
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
) -> (String, Option<Vec<openai::ToolCall>>) {
    let mut text = Vec::new();
    let mut tool_calls = Vec::new();

    for block in content {
        match block {
            claude::ContentBlock::Text { text: t } => text.push(t),
            claude::ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(openai::ToolCall {
                    id,
                    tool_type: "function".to_string(),
                    function: openai::ToolCallFunction {
                        name,
                        arguments: input.to_string(),
                    },
                    index: None,
                });
            }
            claude::ContentBlock::Image { .. }
            | claude::ContentBlock::ToolResult { .. }
            | claude::ContentBlock::Thinking { .. } => {}
        }
    }

    (
        text.join(""),
        (!tool_calls.is_empty()).then_some(tool_calls),
    )
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
            role:              role.into(),
            content:           Some(openai::MessageContent::Text(content.into())),
            name:              None,
            tool_calls:        None,
            tool_call_id:      None,
            reasoning_content: None,
        }
    }

    #[test]
    fn converts_openai_chat_to_claude_messages() {
        let req = openai::ChatCompletionRequest {
            model:                 "gpt-4o-mini".into(),
            messages:              vec![
                openai::ChatMessage {
                    role:              "system".into(),
                    content:           Some(openai::MessageContent::Text(
                        "You are concise.".into(),
                    )),
                    name:              None,
                    tool_calls:        None,
                    tool_call_id:      None,
                    reasoning_content: None,
                },
                openai::ChatMessage {
                    role:              "user".into(),
                    content:           Some(openai::MessageContent::Text("Hello".into())),
                    name:              None,
                    tool_calls:        None,
                    tool_call_id:      None,
                    reasoning_content: None,
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
        };

        let claude = openai_chat_to_claude_messages(&req, "claude").unwrap();
        assert_eq!(claude.messages[0].role, "user");
        assert!(matches!(
            claude.messages[0].content[0],
            claude::ContentBlock::Text { ref text } if text == "..."
        ));
        assert_eq!(claude.messages[2].role, "user");
        assert!(matches!(
            claude.messages[2].content[0],
            claude::ContentBlock::Text { ref text } if text == "hello again"
        ));
    }

    #[test]
    fn converts_tool_calls_with_object_arguments_only() {
        let req = openai::ChatCompletionRequest {
            model:                 "gpt-4o-mini".into(),
            messages:              vec![openai::ChatMessage {
                role:              "assistant".into(),
                content:           None,
                name:              None,
                tool_calls:        Some(vec![openai::ToolCall {
                    id:        "call_1".into(),
                    tool_type: "function".into(),
                    function:  openai::ToolCallFunction {
                        name:      "lookup".into(),
                        arguments: "\"bad\"".into(),
                    },
                    index:     None,
                }]),
                tool_call_id:      None,
                reasoning_content: None,
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
        };

        let claude = openai_chat_to_claude_messages(&req, "claude").unwrap();
        assert!(matches!(
            claude.messages[1].content[1],
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
}
