use std::time::{SystemTime, UNIX_EPOCH};

use halolake_api_contract::{JsonValue, gemini, openai};
use serde_json::json;
use uuid::Uuid;

use crate::ProtocolError;

const GEMINI_SAFETY_CATEGORIES: &[&str] = &[
    "HARM_CATEGORY_HARASSMENT",
    "HARM_CATEGORY_HATE_SPEECH",
    "HARM_CATEGORY_SEXUALLY_EXPLICIT",
    "HARM_CATEGORY_DANGEROUS_CONTENT",
];

pub fn openai_chat_to_gemini_request(
    req: &openai::ChatCompletionRequest,
) -> Result<gemini::GeminiChatRequest, ProtocolError> {
    if req.messages.is_empty() {
        return Err(ProtocolError::EmptyMessages);
    }

    let mut out = gemini::GeminiChatRequest {
        generation_config: gemini::GeminiGenerationConfig {
            temperature: req.temperature,
            top_p: req.top_p,
            top_k: req.top_k,
            max_output_tokens: req.max_tokens_value(),
            candidate_count: req.n,
            stop_sequences: gemini_stop_sequences(req.stop.as_ref()),
            response_mime_type: gemini_response_mime_type(req.response_format.as_ref()),
            response_schema: gemini_response_schema(req.response_format.as_ref()),
            seed: req.seed,
        },
        safety_settings: GEMINI_SAFETY_CATEGORIES
            .iter()
            .map(|category| gemini::GeminiChatSafetySetting {
                category: (*category).to_string(),
                threshold: "OFF".to_string(),
            })
            .collect(),
        ..gemini::GeminiChatRequest::default()
    };

    if let Some(tools) = &req.tools {
        out.tools = openai_tools_to_gemini(tools);
        if let Some(choice) = &req.tool_choice {
            out.tool_config = openai_tool_choice_to_gemini(choice);
        }
    }

    let mut system = Vec::new();
    let mut tool_names_by_id = std::collections::HashMap::<String, String>::new();

    for message in &req.messages {
        match message.role.as_str() {
            "system" | "developer" => {
                if let Some(content) = &message.content {
                    let text = content.as_text_lossy();
                    if !text.is_empty() {
                        system.push(text);
                    }
                }
            }
            "tool" | "function" => {
                append_openai_tool_result_as_gemini(message, &tool_names_by_id, &mut out.contents)
            }
            _ => {
                let role = if message.role == "assistant" || message.role == "model" {
                    "model"
                } else {
                    "user"
                };
                let mut parts = Vec::new();

                if let Some(tool_calls) = &message.tool_calls {
                    for call in tool_calls {
                        let args = parse_json_object_or_empty(&call.function.arguments);
                        parts.push(gemini::GeminiPart {
                            function_call: Some(gemini::GeminiFunctionCall {
                                name: call.function.name.clone(),
                                args,
                            }),
                            ..gemini::GeminiPart::default()
                        });
                        tool_names_by_id.insert(call.id.clone(), call.function.name.clone());
                    }
                }

                if let Some(content) = &message.content {
                    append_openai_content_as_gemini_parts(content, &mut parts);
                }

                if !parts.is_empty() {
                    out.contents.push(gemini::GeminiChatContent {
                        role: Some(role.to_string()),
                        parts,
                    });
                }
            }
        }
    }

    if !system.is_empty() {
        out.system_instruction = Some(gemini::GeminiChatContent {
            role: None,
            parts: vec![gemini::GeminiPart::text(system.join("\n"))],
        });
    }

    Ok(out)
}

pub fn gemini_request_to_openai_chat(
    req: &gemini::GeminiChatRequest,
    upstream_model: impl Into<String>,
    stream: bool,
) -> Result<openai::ChatCompletionRequest, ProtocolError> {
    if req.contents.is_empty() {
        return Err(ProtocolError::EmptyMessages);
    }

    let mut messages = Vec::with_capacity(req.contents.len() + 1);
    if let Some(system) = &req.system_instruction {
        let text = gemini_parts_text(&system.parts);
        if !text.is_empty() {
            messages.push(openai::ChatMessage {
                role: "system".to_string(),
                content: Some(openai::MessageContent::Text(text)),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
        }
    }

    let mut next_tool_id = 1usize;
    for content in &req.contents {
        let role = match content.role.as_deref() {
            Some("model") => "assistant",
            Some("function") => "function",
            _ => "user",
        }
        .to_string();

        let mut parts = Vec::new();
        let mut tool_calls = Vec::new();
        for part in &content.parts {
            if let Some(text) = &part.text {
                if !text.is_empty() {
                    parts.push(openai::ContentPart::Text {
                        part_type: "text".to_string(),
                        text: text.clone(),
                    });
                }
            } else if let Some(inline) = &part.inline_data {
                parts.push(openai::ContentPart::ImageUrl {
                    part_type: "image_url".to_string(),
                    image_url: openai::ImageUrl {
                        url: format!("data:{};base64,{}", inline.mime_type, inline.data),
                        detail: Some("auto".to_string()),
                    },
                });
            } else if let Some(file) = &part.file_data {
                parts.push(openai::ContentPart::ImageUrl {
                    part_type: "image_url".to_string(),
                    image_url: openai::ImageUrl {
                        url: file.file_uri.clone(),
                        detail: Some("auto".to_string()),
                    },
                });
            } else if let Some(call) = &part.function_call {
                tool_calls.push(openai::ToolCall {
                    id: format!("call_{next_tool_id}"),
                    tool_type: "function".to_string(),
                    function: openai::ToolCallFunction {
                        name: call.name.clone(),
                        arguments: call.args.to_string(),
                    },
                    index: None,
                });
                next_tool_id += 1;
            } else if let Some(function_response) = &part.function_response {
                messages.push(openai::ChatMessage {
                    role: "tool".to_string(),
                    content: Some(openai::MessageContent::Text(
                        function_response.response.to_string(),
                    )),
                    name: Some(function_response.name.clone()),
                    tool_calls: None,
                    tool_call_id: Some(format!("call_{}", next_tool_id.saturating_sub(1))),
                    reasoning_content: None,
                });
            }
        }

        if !parts.is_empty() || !tool_calls.is_empty() {
            messages.push(openai_message_from_parts(role, parts, tool_calls));
        }
    }

    let tools = gemini_tools_to_openai(&req.tools);
    Ok(openai::ChatCompletionRequest {
        model: upstream_model.into(),
        messages,
        max_tokens: req.generation_config.max_output_tokens,
        max_completion_tokens: None,
        temperature: req.generation_config.temperature,
        top_p: req.generation_config.top_p,
        top_k: req.generation_config.top_k,
        stream: Some(stream),
        tools: (!tools.is_empty()).then_some(tools),
        tool_choice: None,
        parallel_tool_calls: None,
        stop: if req.generation_config.stop_sequences.is_empty() {
            None
        } else {
            Some(JsonValue::Array(
                req.generation_config
                    .stop_sequences
                    .iter()
                    .cloned()
                    .map(JsonValue::String)
                    .collect(),
            ))
        },
        n: req.generation_config.candidate_count,
        seed: req.generation_config.seed,
        response_format: None,
        stream_options: stream.then(|| json!({"include_usage": true})),
    })
}

pub fn openai_image_to_gemini_imagen_request(
    req: &openai::ImageRequest,
    upstream_model: &str,
) -> Result<gemini::GeminiImageRequest, ProtocolError> {
    if !upstream_model.starts_with("imagen") {
        return Err(ProtocolError::UnsupportedImageModel);
    }

    let mut image_size = String::new();
    if !req.quality.is_empty() {
        image_size = match req.quality.as_str() {
            "hd" | "high" | "2K" => "2K",
            "standard" | "medium" | "low" | "auto" | "1K" => "1K",
            _ => "1K",
        }
        .to_string();
    }

    Ok(gemini::GeminiImageRequest {
        instances: vec![gemini::GeminiImageInstance {
            prompt: req.prompt.clone(),
        }],
        parameters: gemini::GeminiImageParameters {
            sample_count: Some(req.n_or_one()),
            aspect_ratio: openai_image_size_to_gemini_aspect_ratio(&req.size),
            person_generation: "allow_adult".to_string(),
            image_size,
        },
    })
}

pub fn gemini_imagen_to_openai_image_response(
    resp: gemini::GeminiImageResponse,
    created: i64,
) -> Result<openai::ImageResponse, ProtocolError> {
    if resp.predictions.is_empty() {
        return Err(ProtocolError::NoImagesGenerated);
    }

    Ok(openai::ImageResponse {
        data: resp
            .predictions
            .into_iter()
            .filter(|prediction| prediction.rai_filtered_reason.is_empty())
            .map(|prediction| openai::ImageData {
                b64_json: prediction.bytes_base64_encoded,
                ..openai::ImageData::default()
            })
            .collect(),
        created,
        metadata: None,
    })
}

pub fn gemini_response_to_openai_chat(
    resp: gemini::GeminiChatResponse,
    requested_model: impl Into<String>,
) -> openai::ChatCompletionResponse {
    openai::ChatCompletionResponse {
        id: format!("chatcmpl-{}", Uuid::new_v4().simple()),
        object: "chat.completion".to_string(),
        created: now_unix(),
        model: requested_model.into(),
        choices: resp
            .candidates
            .into_iter()
            .map(gemini_candidate_to_openai_choice)
            .collect(),
        usage: Some(gemini_usage_to_openai(resp.usage_metadata)),
    }
}

pub fn openai_chat_to_gemini_response(
    resp: openai::ChatCompletionResponse,
) -> gemini::GeminiChatResponse {
    let usage = resp.usage.unwrap_or_default();
    gemini::GeminiChatResponse {
        candidates: resp
            .choices
            .into_iter()
            .map(openai_choice_to_gemini_candidate)
            .collect(),
        prompt_feedback: None,
        usage_metadata: openai_usage_to_gemini(usage),
    }
}

#[derive(Debug, Clone)]
pub struct GeminiSseToOpenAiTranslator {
    completion_id: String,
    created: u64,
    requested_model: String,
    done: bool,
}

impl GeminiSseToOpenAiTranslator {
    pub fn new(requested_model: impl Into<String>) -> Self {
        Self {
            completion_id: format!("chatcmpl-{}", Uuid::new_v4().simple()),
            created: now_unix(),
            requested_model: requested_model.into(),
            done: false,
        }
    }

    pub fn translate_sse_payload(&mut self, payload: &str) -> Result<Vec<String>, ProtocolError> {
        if self.done {
            return Ok(Vec::new());
        }
        if payload == "[DONE]" {
            self.done = true;
            return Ok(vec!["[DONE]".to_string()]);
        }

        let resp: gemini::GeminiChatResponse = serde_json::from_str(payload)?;
        let mut out = Vec::new();
        let mut finished = false;
        let usage = gemini_usage_to_openai(resp.usage_metadata);
        let choices = resp
            .candidates
            .into_iter()
            .map(|candidate| {
                // Gemini never emits a native `[DONE]`; the stream ends on any
                // finish_reason (STOP, MAX_TOKENS, SAFETY, RECITATION, ...).
                // Keying only on "STOP" left the OpenAI/Claude client blocking
                // until socket close on every other (common) terminal reason.
                if candidate.finish_reason.is_some() {
                    finished = true;
                }
                gemini_candidate_to_openai_chunk_choice(candidate)
            })
            .collect::<Vec<_>>();

        if !choices.is_empty() {
            out.push(serde_json::to_string(&openai::ChatCompletionChunk {
                id: self.completion_id.clone(),
                object: "chat.completion.chunk".to_string(),
                created: self.created,
                model: self.requested_model.clone(),
                choices,
                usage: (usage.total_tokens > 0).then_some(usage),
            })?);
        }

        if finished {
            self.done = true;
            out.push("[DONE]".to_string());
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiSseToGeminiTranslator {
    done: bool,
}

impl OpenAiSseToGeminiTranslator {
    pub fn new() -> Self {
        Self { done: false }
    }

    pub fn translate_sse_payload(&mut self, payload: &str) -> Result<Vec<String>, ProtocolError> {
        if self.done {
            return Ok(Vec::new());
        }
        if payload == "[DONE]" {
            self.done = true;
            return Ok(Vec::new());
        }

        let chunk: openai::ChatCompletionChunk = serde_json::from_str(payload)?;
        let mut response = gemini::GeminiChatResponse::default();
        response.usage_metadata = chunk.usage.map(openai_usage_to_gemini).unwrap_or_default();
        for choice in chunk.choices {
            response
                .candidates
                .push(openai_chunk_choice_to_gemini_candidate(choice));
        }
        if response.candidates.is_empty() && response.usage_metadata.total_token_count == 0 {
            return Ok(Vec::new());
        }
        Ok(vec![serde_json::to_string(&response)?])
    }
}

fn append_openai_tool_result_as_gemini(
    message: &openai::ChatMessage,
    tool_names_by_id: &std::collections::HashMap<String, String>,
    contents: &mut Vec<gemini::GeminiChatContent>,
) {
    if contents
        .last()
        .is_none_or(|content| content.role.as_deref() == Some("model"))
    {
        contents.push(gemini::GeminiChatContent {
            role: Some("user".to_string()),
            parts: Vec::new(),
        });
    }

    let name = message
        .name
        .clone()
        .or_else(|| {
            message
                .tool_call_id
                .as_ref()
                .and_then(|id| tool_names_by_id.get(id))
                .cloned()
        })
        .unwrap_or_default();
    let content = message
        .content
        .as_ref()
        .map(openai::MessageContent::as_text_lossy)
        .unwrap_or_default();
    let response =
        serde_json::from_str::<JsonValue>(&content).unwrap_or_else(|_| json!({"content": content}));

    if let Some(last) = contents.last_mut() {
        last.parts.push(gemini::GeminiPart {
            function_response: Some(gemini::GeminiFunctionResponse { name, response }),
            ..gemini::GeminiPart::default()
        });
    }
}

fn append_openai_content_as_gemini_parts(
    content: &openai::MessageContent,
    parts: &mut Vec<gemini::GeminiPart>,
) {
    match content {
        openai::MessageContent::Text(text) => {
            if !text.is_empty() {
                parts.push(gemini::GeminiPart::text(text.clone()));
            }
        }
        openai::MessageContent::Parts(content_parts) => {
            for part in content_parts {
                match part {
                    openai::ContentPart::Text { text, .. } if !text.is_empty() => {
                        parts.push(gemini::GeminiPart::text(text.clone()));
                    }
                    openai::ContentPart::ImageUrl { image_url, .. } => {
                        parts.push(openai_image_url_to_gemini_part(&image_url.url));
                    }
                    openai::ContentPart::Other(value) => {
                        if let Some(text) = value.get("text").and_then(JsonValue::as_str) {
                            if !text.is_empty() {
                                parts.push(gemini::GeminiPart::text(text.to_string()));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn openai_image_url_to_gemini_part(url: &str) -> gemini::GeminiPart {
    if let Some((mime_type, data)) = parse_data_url(url) {
        gemini::GeminiPart {
            inline_data: Some(gemini::GeminiInlineData { mime_type, data }),
            ..gemini::GeminiPart::default()
        }
    } else {
        gemini::GeminiPart {
            file_data: Some(gemini::GeminiFileData {
                mime_type: None,
                file_uri: url.to_string(),
            }),
            ..gemini::GeminiPart::default()
        }
    }
}

fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (mime_type, data) = rest.split_once(";base64,")?;
    Some((mime_type.to_string(), data.to_string()))
}

fn openai_image_size_to_gemini_aspect_ratio(size: &str) -> String {
    let size = size.trim();
    if size.contains(':') {
        return size.to_string();
    }
    match size {
        "256x256" | "512x512" | "1024x1024" => "1:1",
        "1536x1024" => "3:2",
        "1024x1536" => "2:3",
        "1024x1792" => "9:16",
        "1792x1024" => "16:9",
        _ => "1:1",
    }
    .to_string()
}

fn openai_tools_to_gemini(tools: &[openai::Tool]) -> Vec<gemini::GeminiTool> {
    let mut declarations = Vec::new();
    let mut special = Vec::new();

    for tool in tools {
        match tool.function.name.as_str() {
            "googleSearch" => special.push(gemini::GeminiTool {
                google_search: Some(json!({})),
                ..gemini::GeminiTool::default()
            }),
            "codeExecution" => special.push(gemini::GeminiTool {
                code_execution: Some(json!({})),
                ..gemini::GeminiTool::default()
            }),
            "urlContext" => special.push(gemini::GeminiTool {
                url_context: Some(json!({})),
                ..gemini::GeminiTool::default()
            }),
            _ if tool.tool_type == "function" => {
                declarations.push(gemini::GeminiFunctionDeclaration {
                    name: tool.function.name.clone(),
                    description: tool.function.description.clone(),
                    parameters: tool.function.parameters.clone(),
                });
            }
            _ => {}
        }
    }

    if !declarations.is_empty() {
        special.push(gemini::GeminiTool {
            function_declarations: declarations,
            ..gemini::GeminiTool::default()
        });
    }
    special
}

fn gemini_tools_to_openai(tools: &[gemini::GeminiTool]) -> Vec<openai::Tool> {
    tools
        .iter()
        .flat_map(|tool| tool.function_declarations.iter())
        .map(|declaration| openai::Tool {
            tool_type: "function".to_string(),
            function: openai::FunctionTool {
                name: declaration.name.clone(),
                description: declaration.description.clone(),
                parameters: declaration.parameters.clone(),
            },
        })
        .collect()
}

fn openai_tool_choice_to_gemini(choice: &JsonValue) -> Option<JsonValue> {
    if choice == "auto" {
        Some(json!({"functionCallingConfig": {"mode": "AUTO"}}))
    } else if choice == "none" {
        Some(json!({"functionCallingConfig": {"mode": "NONE"}}))
    } else if choice == "required" {
        Some(json!({"functionCallingConfig": {"mode": "ANY"}}))
    } else {
        choice
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(JsonValue::as_str)
            .map(|name| {
                json!({
                    "functionCallingConfig": {
                        "mode": "ANY",
                        "allowedFunctionNames": [name],
                    }
                })
            })
    }
}

fn gemini_stop_sequences(stop: Option<&JsonValue>) -> Vec<String> {
    let mut stops = match stop {
        Some(JsonValue::String(stop)) if !stop.is_empty() => vec![stop.clone()],
        Some(JsonValue::Array(values)) => values
            .iter()
            .filter_map(JsonValue::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    if stops.len() > 5 {
        stops.truncate(5);
    }
    stops
}

fn gemini_response_mime_type(response_format: Option<&JsonValue>) -> Option<String> {
    let format_type = response_format?.get("type")?.as_str()?;
    matches!(format_type, "json_schema" | "json_object").then(|| "application/json".to_string())
}

fn gemini_response_schema(response_format: Option<&JsonValue>) -> Option<JsonValue> {
    response_format?
        .get("json_schema")
        .and_then(|schema| schema.get("schema"))
        .cloned()
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

fn gemini_parts_text(parts: &[gemini::GeminiPart]) -> String {
    parts
        .iter()
        .filter_map(|part| part.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

fn gemini_candidate_to_openai_choice(candidate: gemini::GeminiCandidate) -> openai::ChatChoice {
    let (content, reasoning_content, tool_calls) = gemini_parts_to_openai(candidate.content.parts);
    openai::ChatChoice {
        index: candidate.index,
        message: openai::ChatMessage {
            role: "assistant".to_string(),
            content: Some(openai::MessageContent::Text(content)),
            name: None,
            tool_calls,
            tool_call_id: None,
            reasoning_content,
        },
        finish_reason: candidate.finish_reason.map(gemini_finish_to_openai),
    }
}

fn openai_choice_to_gemini_candidate(choice: openai::ChatChoice) -> gemini::GeminiCandidate {
    let mut parts = Vec::new();
    if let Some(content) = choice.message.content {
        let text = content.as_text_lossy();
        if !text.is_empty() {
            parts.push(gemini::GeminiPart::text(text));
        }
    }
    if let Some(tool_calls) = choice.message.tool_calls {
        parts.extend(tool_calls.into_iter().map(|call| gemini::GeminiPart {
            function_call: Some(gemini::GeminiFunctionCall {
                name: call.function.name,
                args: parse_json_object_or_empty(&call.function.arguments),
            }),
            ..gemini::GeminiPart::default()
        }));
    }
    gemini::GeminiCandidate {
        content: gemini::GeminiChatContent {
            role: Some("model".to_string()),
            parts,
        },
        finish_reason: choice.finish_reason.map(openai_finish_to_gemini),
        index: choice.index,
        safety_ratings: Vec::new(),
    }
}

fn gemini_candidate_to_openai_chunk_choice(
    candidate: gemini::GeminiCandidate,
) -> openai::ChatChunkChoice {
    let (content, reasoning_content, tool_calls) = gemini_parts_to_openai(candidate.content.parts);
    openai::ChatChunkChoice {
        index: candidate.index,
        delta: openai::ChatChunkDelta {
            role: None,
            content: (!content.is_empty()).then_some(content),
            reasoning_content,
            reasoning: None,
            tool_calls,
        },
        finish_reason: candidate.finish_reason.map(gemini_finish_to_openai),
    }
}

fn openai_chunk_choice_to_gemini_candidate(
    choice: openai::ChatChunkChoice,
) -> gemini::GeminiCandidate {
    let mut parts = Vec::new();
    if let Some(content) = choice.delta.content {
        if !content.is_empty() {
            parts.push(gemini::GeminiPart::text(content));
        }
    }
    if let Some(reasoning) = choice.delta.reasoning_content.or(choice.delta.reasoning) {
        if !reasoning.is_empty() {
            parts.push(gemini::GeminiPart {
                text: Some(reasoning),
                thought: Some(true),
                ..gemini::GeminiPart::default()
            });
        }
    }
    if let Some(tool_calls) = choice.delta.tool_calls {
        parts.extend(tool_calls.into_iter().map(|call| gemini::GeminiPart {
            function_call: Some(gemini::GeminiFunctionCall {
                name: call.function.name,
                args: parse_json_object_or_empty(&call.function.arguments),
            }),
            ..gemini::GeminiPart::default()
        }));
    }
    gemini::GeminiCandidate {
        content: gemini::GeminiChatContent {
            role: Some("model".to_string()),
            parts,
        },
        finish_reason: choice.finish_reason.map(openai_finish_to_gemini),
        index: choice.index,
        safety_ratings: Vec::new(),
    }
}

fn gemini_parts_to_openai(
    parts: Vec<gemini::GeminiPart>,
) -> (String, Option<String>, Option<Vec<openai::ToolCall>>) {
    let mut text = Vec::new();
    let mut reasoning = Vec::new();
    let mut tool_calls = Vec::new();
    for part in parts {
        if part.thought.unwrap_or(false) {
            if let Some(part_text) = part.text {
                reasoning.push(part_text);
            }
        } else if let Some(part_text) = part.text {
            if !part_text.is_empty() && part_text != "\n" {
                text.push(part_text);
            }
        } else if let Some(inline) = part.inline_data {
            text.push(format!(
                "![image](data:{};base64,{})",
                inline.mime_type, inline.data
            ));
        } else if let Some(call) = part.function_call {
            tool_calls.push(openai::ToolCall {
                id: format!("call_{}", tool_calls.len() + 1),
                tool_type: "function".to_string(),
                function: openai::ToolCallFunction {
                    name: call.name,
                    arguments: call.args.to_string(),
                },
                index: Some(tool_calls.len() as u32),
            });
        } else if let Some(code) = part.executable_code {
            text.push(format!("```{}\n{}\n```", code.language, code.code));
        } else if let Some(result) = part.code_execution_result {
            text.push(format!("```output\n{}\n```", result.output));
        }
    }

    (
        text.join("\n"),
        (!reasoning.is_empty()).then(|| reasoning.join("\n")),
        (!tool_calls.is_empty()).then_some(tool_calls),
    )
}

fn parse_json_object_or_empty(text: &str) -> JsonValue {
    serde_json::from_str::<JsonValue>(text)
        .ok()
        .filter(JsonValue::is_object)
        .unwrap_or_else(|| json!({}))
}

fn gemini_usage_to_openai(usage: gemini::GeminiUsageMetadata) -> openai::Usage {
    let completion = usage.candidates_token_count + usage.thoughts_token_count;
    let total = if usage.total_token_count > 0 {
        usage.total_token_count
    } else {
        usage.prompt_token_count + completion
    };
    openai::Usage {
        prompt_tokens: usage.prompt_token_count + usage.cached_content_token_count,
        completion_tokens: completion,
        total_tokens: total,
        prompt_tokens_details: openai::TokenUsageDetails {
            cached_tokens: usage.cached_content_token_count,
            ..openai::TokenUsageDetails::default()
        },
        input_tokens_details: None,
        cached_tokens: usage.cached_content_token_count,
    }
}

fn openai_usage_to_gemini(usage: openai::Usage) -> gemini::GeminiUsageMetadata {
    gemini::GeminiUsageMetadata {
        prompt_token_count: usage.prompt_tokens,
        candidates_token_count: usage.completion_tokens,
        total_token_count: usage.total_tokens,
        thoughts_token_count: 0,
        cached_content_token_count: 0,
    }
}

fn gemini_finish_to_openai(reason: String) -> String {
    match reason.as_str() {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "OTHER" => {
            "content_filter"
        }
        _ => "content_filter",
    }
    .to_string()
}

fn openai_finish_to_gemini(reason: String) -> String {
    match reason.as_str() {
        "stop" => "STOP",
        "length" => "MAX_TOKENS",
        "content_filter" => "SAFETY",
        "tool_calls" => "STOP",
        _ => "STOP",
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

    fn image_req(size: &str, quality: &str, n: Option<u32>) -> openai::ImageRequest {
        openai::ImageRequest {
            model: "gpt-image-1".to_string(),
            prompt: "a lake under stars".to_string(),
            n,
            size: size.to_string(),
            quality: quality.to_string(),
            response_format: String::new(),
            style: None,
            user: None,
            extra_fields: None,
            background: None,
            moderation: None,
            output_format: None,
            output_compression: None,
            partial_images: None,
            stream: None,
            images: None,
            mask: None,
            input_fidelity: None,
            watermark: None,
            watermark_enabled: None,
            user_id: None,
            image: None,
        }
    }

    #[test]
    fn converts_openai_tool_call_to_gemini_function_call_like_new_api() {
        let req = openai::ChatCompletionRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![openai::ChatMessage {
                role: "assistant".to_string(),
                content: Some(openai::MessageContent::Text("".to_string())),
                name: None,
                tool_calls: Some(vec![openai::ToolCall {
                    id: "call_1".to_string(),
                    tool_type: "function".to_string(),
                    function: openai::ToolCallFunction {
                        name: "lookup".to_string(),
                        arguments: "{\"q\":\"rust\"}".to_string(),
                    },
                    index: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
            }],
            max_tokens: Some(128),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stream: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            stop: None,
            n: None,
            seed: None,
            response_format: None,
            stream_options: None,
        };

        let gemini = openai_chat_to_gemini_request(&req).unwrap();
        let call = gemini.contents[0].parts[0].function_call.as_ref().unwrap();
        assert_eq!(gemini.contents[0].role.as_deref(), Some("model"));
        assert_eq!(call.name, "lookup");
        assert_eq!(call.args, json!({"q": "rust"}));
    }

    #[test]
    fn converts_gemini_function_call_to_openai_tool_calls_like_new_api() {
        let resp = gemini::GeminiChatResponse {
            candidates: vec![gemini::GeminiCandidate {
                content: gemini::GeminiChatContent {
                    role: Some("model".to_string()),
                    parts: vec![gemini::GeminiPart {
                        function_call: Some(gemini::GeminiFunctionCall {
                            name: "lookup".to_string(),
                            args: json!({"q": "rust"}),
                        }),
                        ..gemini::GeminiPart::default()
                    }],
                },
                finish_reason: Some("STOP".to_string()),
                index: 0,
                safety_ratings: Vec::new(),
            }],
            prompt_feedback: None,
            usage_metadata: gemini::GeminiUsageMetadata {
                prompt_token_count: 3,
                candidates_token_count: 4,
                total_token_count: 7,
                thoughts_token_count: 0,
                cached_content_token_count: 0,
            },
        };

        let openai = gemini_response_to_openai_chat(resp, "gemini-2.5-flash");
        let tool_calls = openai.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls[0].function.name, "lookup");
        assert_eq!(tool_calls[0].function.arguments, "{\"q\":\"rust\"}");
        assert_eq!(openai.usage.unwrap().total_tokens, 7);
    }

    #[test]
    fn converts_openai_image_request_to_gemini_imagen_like_new_api() {
        let req = image_req("1024x1792", "hd", Some(3));

        let gemini = openai_image_to_gemini_imagen_request(&req, "imagen-4.0-generate-001")
            .expect("imagen model should convert");

        assert_eq!(gemini.instances[0].prompt, "a lake under stars");
        assert_eq!(gemini.parameters.sample_count, Some(3));
        assert_eq!(gemini.parameters.aspect_ratio, "9:16");
        assert_eq!(gemini.parameters.person_generation, "allow_adult");
        assert_eq!(gemini.parameters.image_size, "2K");
    }

    #[test]
    fn maps_openai_image_size_and_quality_to_gemini_defaults_like_new_api() {
        let req = image_req("3:2", "unknown", None);

        let gemini = openai_image_to_gemini_imagen_request(&req, "imagen-4.0-generate-001")
            .expect("imagen model should convert");

        assert_eq!(gemini.parameters.sample_count, Some(1));
        assert_eq!(gemini.parameters.aspect_ratio, "3:2");
        assert_eq!(gemini.parameters.image_size, "1K");
    }

    #[test]
    fn rejects_non_imagen_image_upstream_like_new_api() {
        let req = image_req("", "", None);

        let err = openai_image_to_gemini_imagen_request(&req, "gemini-2.5-flash")
            .expect_err("non-imagen model should be rejected");

        assert!(matches!(err, ProtocolError::UnsupportedImageModel));
    }

    #[test]
    fn converts_gemini_imagen_response_to_openai_image_response_like_new_api() {
        let resp = gemini::GeminiImageResponse {
            predictions: vec![
                gemini::GeminiImagePrediction {
                    bytes_base64_encoded: "image-1".to_string(),
                    ..gemini::GeminiImagePrediction::default()
                },
                gemini::GeminiImagePrediction {
                    bytes_base64_encoded: "filtered".to_string(),
                    rai_filtered_reason: "safety".to_string(),
                    ..gemini::GeminiImagePrediction::default()
                },
            ],
        };

        let openai = gemini_imagen_to_openai_image_response(resp, 123).unwrap();

        assert_eq!(openai.created, 123);
        assert_eq!(openai.data.len(), 1);
        assert_eq!(openai.data[0].b64_json, "image-1");
    }

    #[test]
    fn gemini_stream_terminates_on_non_stop_finish_reason() {
        let mut translator = GeminiSseToOpenAiTranslator::new("gemini-1.5-pro");
        let payload = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "Hi"}]},
                "finishReason": "MAX_TOKENS"
            }]
        })
        .to_string();

        let events = translator.translate_sse_payload(&payload).expect("translate");
        // Gemini never sends a native [DONE]; a MAX_TOKENS finish must still
        // terminate the stream or the downstream client blocks until close.
        assert_eq!(events.last().map(String::as_str), Some("[DONE]"));
        let chunk: serde_json::Value = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(chunk["choices"][0]["finish_reason"], "length");

        // Further payloads after termination are ignored.
        assert!(
            translator
                .translate_sse_payload(&payload)
                .expect("translate")
                .is_empty()
        );
    }
}
