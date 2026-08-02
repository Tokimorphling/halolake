use crate::claude_openai::ProtocolError;
use halolake_api_contract::{JsonValue, claude};
use serde_json::{Map, json};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

const DEFAULT_MAX_TOKENS: u32 = 8192;

fn ephemeral_cache_control() -> JsonValue {
    json!({"type": "ephemeral"})
}

/// Controls whether a converted Messages request uses Anthropic's newer
/// top-level automatic breakpoint or only explicit block-level breakpoints.
/// Explicit mode is the safe default for Bedrock and older compatible APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptCacheMode {
    Explicit,
    Automatic,
}

fn request_cache_control(request: &JsonValue) -> JsonValue {
    request
        .get("cache_control")
        .filter(|value| {
            value
                .get("type")
                .and_then(JsonValue::as_str)
                == Some("ephemeral")
        })
        .cloned()
        .unwrap_or_else(ephemeral_cache_control)
}

/// The `tool_search` proxy keeps its wire name: Codex's model side calls the
/// tool by this exact name, so it cannot be renamed to avoid collisions.
const TOOL_SEARCH_NAME: &str = "tool_search";

/// Anthropic, like Chat Completions, caps tool names at 64 bytes.
const TOOL_NAME_MAX_LEN: usize = 64;

/// Free-form `custom` tools carry raw text, which Anthropic's JSON-schema tool
/// input cannot express; they are lowered to a single `input` string property
/// and restored from it on the way back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponsesToolMapping {
    pub custom_tools:    BTreeSet<String>,
    pub tool_search:     bool,
    pub namespace_tools: BTreeMap<String, ResponsesNamespaceTool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsesNamespaceTool {
    pub namespace: String,
    pub name:      String,
}

impl ResponsesToolMapping {
    /// Restores the Codex-facing identity of a flattened namespace tool name.
    pub fn namespace_entry(&self, flat: &str) -> Option<&ResponsesNamespaceTool> {
        self.namespace_tools.get(flat)
    }

    /// Flattened name for a namespace-qualified call, but only when the pair was
    /// actually declared in this request. An unknown pair must stay untouched so
    /// the request fails loudly upstream instead of silently calling the wrong
    /// tool.
    pub fn flatten_declared(&self, namespace: &str, name: &str) -> Option<&str> {
        let flat = flatten_namespace_tool_name(namespace, name);
        let entry = self.namespace_tools.get_key_value(&flat)?;
        (entry.1.namespace == namespace && entry.1.name == name).then(|| entry.0.as_str())
    }
}

/// Collects the reversible tool lowering metadata from a Responses request.
/// `additional_tools` is part of the input history, so it is included here as
/// well as top-level `tools`.
///
/// Collection is deliberately two-phase: every top-level `function`/`custom`
/// name is gathered before any namespace child is flattened, so a namespace
/// declared *before* a conflicting top-level tool is still rejected.
pub fn responses_tool_mapping(raw: &[u8]) -> Result<ResponsesToolMapping, ProtocolError> {
    let request: JsonValue = serde_json::from_slice(raw)?;
    responses_tool_mapping_from_value(&request)
}

fn responses_tool_mapping_from_value(
    request: &JsonValue,
) -> Result<ResponsesToolMapping, ProtocolError> {
    let mut mapping = ResponsesToolMapping::default();
    let tools = collect_declared_tools(request);

    // Phase 1: every top-level declaration, regardless of ordering.
    let mut function_names = BTreeSet::new();
    for tool in &tools {
        let name = tool
            .get("name")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        match tool.get("type").and_then(JsonValue::as_str) {
            Some("function") if !name.is_empty() => {
                function_names.insert(name.to_string());
            }
            Some("custom") if !name.is_empty() => {
                mapping.custom_tools.insert(name.to_string());
            }
            Some("tool_search") => mapping.tool_search = true,
            _ => {}
        }
    }
    if let Some(name) = mapping
        .custom_tools
        .iter()
        .find(|name| function_names.contains(name.as_str()))
    {
        return Err(ProtocolError::UnsupportedRequest(format!(
            "custom tool {name:?} conflicts with a function tool of the same name; rename one of \
             the tools"
        )));
    }
    let mut top_level_tools = function_names;
    top_level_tools.extend(mapping.custom_tools.iter().cloned());
    if mapping.tool_search && top_level_tools.contains(TOOL_SEARCH_NAME) {
        return Err(ProtocolError::UnsupportedRequest(format!(
            "built-in tool_search conflicts with a declared tool named {TOOL_SEARCH_NAME:?}; \
             rename the tool"
        )));
    }

    // Phase 2: flatten namespace children against the complete top-level set.
    for tool in &tools {
        if tool.get("type").and_then(JsonValue::as_str) == Some("namespace") {
            collect_namespace_tools(tool, &top_level_tools, &mut mapping)?;
        }
    }
    if mapping.tool_search && mapping.namespace_tools.contains_key(TOOL_SEARCH_NAME) {
        return Err(ProtocolError::UnsupportedRequest(format!(
            "built-in tool_search conflicts with a namespace tool flattened as \
             {TOOL_SEARCH_NAME:?}; rename the tool"
        )));
    }
    Ok(mapping)
}

/// Top-level `tools` plus any `additional_tools` carried inside the input
/// history; both declare tools the model may call.
fn collect_declared_tools(request: &JsonValue) -> Vec<JsonValue> {
    let mut tools = request
        .get("tools")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();
    for item in request
        .get("input")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        if item.get("type").and_then(JsonValue::as_str) == Some("additional_tools")
            && let Some(extra) = item.get("tools").and_then(JsonValue::as_array)
        {
            tools.extend(extra.iter().cloned());
        }
    }
    tools
}

fn collect_namespace_tools(
    tool: &JsonValue,
    top_level_tools: &BTreeSet<String>,
    mapping: &mut ResponsesToolMapping,
) -> Result<(), ProtocolError> {
    let Some(namespace) = tool
        .get("name")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        return Ok(());
    };
    for child in namespace_children(tool) {
        if child.get("type").and_then(JsonValue::as_str) != Some("function") {
            continue;
        }
        let Some(name) = child
            .get("name")
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let flat = flatten_namespace_tool_name(namespace, name);
        if top_level_tools.contains(&flat) {
            return Err(ProtocolError::UnsupportedRequest(format!(
                "namespace tool {namespace:?}/{name:?} flattens to {flat:?}, which conflicts with \
                 a top-level tool of the same name; rename one of the tools"
            )));
        }
        let entry = ResponsesNamespaceTool {
            namespace: namespace.to_string(),
            name:      name.to_string(),
        };
        if let Some(previous) = mapping.namespace_tools.get(&flat) {
            if previous == &entry {
                continue;
            }
            return Err(ProtocolError::UnsupportedRequest(format!(
                "namespace tools {:?}/{:?} and {namespace:?}/{name:?} both flatten to {flat:?}; \
                 rename one of the tools",
                previous.namespace, previous.name
            )));
        }
        mapping.namespace_tools.insert(flat, entry);
    }
    Ok(())
}

fn namespace_children(tool: &JsonValue) -> impl Iterator<Item = &JsonValue> {
    tool.get("tools")
        .and_then(JsonValue::as_array)
        .filter(|children| !children.is_empty())
        .or_else(|| tool.get("children").and_then(JsonValue::as_array))
        .into_iter()
        .flatten()
}

fn flatten_namespace_tool_name(namespace: &str, name: &str) -> String {
    let full = format!("{namespace}__{name}");
    if full.len() <= TOOL_NAME_MAX_LEN {
        return full;
    }
    // FNV-1a keeps this protocol crate dependency-free while producing a
    // stable suffix for reversible request-scoped namespace lowering.
    let hash = full
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    let suffix = format!("__{:08x}", hash as u32);
    let prefix = truncate_utf8_bytes(&full, TOOL_NAME_MAX_LEN - suffix.len());
    format!("{prefix}{suffix}")
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Resolves the Anthropic-facing tool name for a Responses `function_call` or
/// `tool_search_call` history item.
///
/// Namespace-qualified calls (`name` + `namespace`) are flattened to the
/// `<namespace>__<name>` form, but only when that pair was actually declared in
/// the request; an unknown pair is left untouched so Anthropic rejects it
/// loudly instead of silently calling the wrong tool.
fn resolve_function_call_name(
    item: &JsonValue,
    item_type: &str,
    tool_mapping: &ResponsesToolMapping,
) -> String {
    let namespace = item
        .get("namespace")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    let name = item
        .get("name")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    if item_type == "tool_search_call" && name.is_empty() {
        return "tool_search".to_string();
    }
    if !namespace.is_empty()
        && !name.is_empty()
        && let Some(flat) = tool_mapping.flatten_declared(namespace, name)
    {
        return flat.to_string();
    }
    name.to_string()
}

/// Converts an OpenAI Responses request into an Anthropic Messages request.
///
/// The Responses API is intentionally handled as JSON here: its input item set
/// evolves independently from Chat Completions, while the Anthropic side has a
/// stable typed contract in `halolake-api-contract`.
///
/// `tool_mapping` must be collected from the same request via
/// [`responses_tool_mapping`]; the gateway already does this so it can pass the
/// mapping to the response translator as well. Standalone callers can use
/// [`openai_responses_to_claude_messages`], which computes the mapping itself.
pub fn openai_responses_to_claude_messages_with_tools(
    raw: &[u8],
    upstream_model: impl Into<String>,
    tool_mapping: &ResponsesToolMapping,
) -> Result<claude::MessagesRequest, ProtocolError> {
    openai_responses_to_claude_messages_with_tools_and_cache_mode(
        raw,
        upstream_model,
        tool_mapping,
        PromptCacheMode::Explicit,
    )
}

/// Same as [`openai_responses_to_claude_messages_with_tools`], with an
/// explicit cache compatibility choice for the target Anthropic endpoint.
pub fn openai_responses_to_claude_messages_with_tools_and_cache_mode(
    raw: &[u8],
    upstream_model: impl Into<String>,
    tool_mapping: &ResponsesToolMapping,
    cache_mode: PromptCacheMode,
) -> Result<claude::MessagesRequest, ProtocolError> {
    let request: JsonValue = serde_json::from_slice(raw)?;
    if request
        .get("previous_response_id")
        .is_some_and(|value| !value.is_null())
    {
        return Err(ProtocolError::UnsupportedRequest(
            "previous_response_id requires Responses state; include the full input history when \
             using an Anthropic channel"
                .to_string(),
        ));
    }
    let cache_control = request_cache_control(&request);
    let mut system = Vec::new();
    if let Some(instructions) = request.get("instructions").and_then(JsonValue::as_str)
        && !instructions.trim().is_empty()
    {
        system.push(instructions.trim().to_string());
    }

    let mut messages = Vec::new();
    match request.get("input") {
        Some(JsonValue::String(text)) => {
            push_message(&mut messages, "user", vec![claude::ContentBlock::Text {
                text: text.clone(),
                cache_control: None,
            }])
        }
        Some(JsonValue::Array(items)) => {
            let mut leading_system = true;
            for item in items {
                if item.get("type").and_then(JsonValue::as_str) == Some("additional_tools") {
                    continue;
                }
                let is_system = matches!(
                    item.get("role").and_then(JsonValue::as_str),
                    Some("system" | "developer")
                );
                if !is_system {
                    leading_system = false;
                }
                convert_input_item(
                    item,
                    tool_mapping,
                    &mut system,
                    &mut messages,
                    leading_system,
                )?;
            }
        }
        Some(value) if !value.is_null() => {
            push_message(&mut messages, "user", vec![claude::ContentBlock::Text {
                text: value.to_string(),
                cache_control: None,
            }])
        }
        _ => {}
    }

    merge_consecutive_messages(&mut messages);
    // Responses emits each function_call as an individual item. Group
    // adjacent assistant items before pairing so parallel calls and their
    // results become one Anthropic assistant/user turn pair.
    normalize_tool_history(&mut messages);
    merge_consecutive_messages(&mut messages);
    if messages.is_empty() {
        return Err(ProtocolError::EmptyMessages);
    }

    let reasoning_effort = request
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(JsonValue::as_str);
    let anthropic_effort = reasoning_effort.map(|effort| {
        if effort == "xhigh" {
            "max".to_string()
        } else {
            effort.to_string()
        }
    });
    let thinking = anthropic_effort.as_deref().and_then(|effort| {
        (effort != "low").then(|| claude::ThinkingConfig {
            thinking_type: "enabled".to_string(),
            budget_tokens: Some(match effort {
                "medium" => 4096,
                "high" => 10240,
                "max" => 32768,
                _ => 4096,
            }),
            display:       None,
        })
    });

    let requested_max_tokens = request
        .get("max_output_tokens")
        .and_then(JsonValue::as_u64)
        .and_then(|tokens| u32::try_from(tokens).ok())
        .filter(|tokens| *tokens > 0)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    // Anthropic requires max_tokens to exceed the thinking budget; keep room
    // for visible output even when Responses asks for high/xhigh reasoning.
    let max_tokens = thinking
        .as_ref()
        .and_then(|thinking| thinking.budget_tokens)
        .map(|budget| requested_max_tokens.max(budget.saturating_add(1024)))
        .unwrap_or(requested_max_tokens);

    let effective_tools = effective_tools(&request);
    let mut tools = convert_tools(effective_tools.as_ref())?;
    // Cache the complete tool-definition prefix explicitly. In automatic mode
    // the top-level marker handles the growing conversation; in explicit mode
    // the message breakpoints below cover that history for compatible APIs
    // that do not implement automatic caching.
    if let Some(last_tool) = tools.as_mut().and_then(|tools| tools.last_mut()) {
        last_tool.cache_control = Some(cache_control.clone());
    }
    let mut tool_choice = request
        .get("tool_choice")
        .and_then(|choice| convert_tool_choice(choice, tool_mapping));
    if tools.is_some()
        && let Some(parallel) = request
            .get("parallel_tool_calls")
            .and_then(JsonValue::as_bool)
    {
        if tool_choice.is_none() {
            tool_choice = Some(json!({"type":"auto"}));
        }
        if let Some(choice) = tool_choice.as_mut().and_then(JsonValue::as_object_mut)
            && choice.get("type").and_then(JsonValue::as_str) != Some("none")
        {
            choice.insert(
                "disable_parallel_tool_use".to_string(),
                JsonValue::Bool(!parallel),
            );
        }
    }

    if cache_mode == PromptCacheMode::Explicit {
        apply_explicit_message_cache_breakpoints(&mut messages, &cache_control);
    }

    let system = (!system.is_empty()).then(|| {
        let mut extra = Map::new();
        extra.insert("cache_control".to_string(), cache_control.clone());
        claude::SystemContent::Blocks(vec![claude::SystemBlock {
            block_type: "text".to_string(),
            text:       Some(system.join("\n\n")),
            extra,
        }])
    });

    Ok(claude::MessagesRequest {
        model: upstream_model.into(),
        max_tokens,
        // Automatic mode is opt-in because Bedrock and older compatible APIs
        // may reject the top-level cache_control field.
        cache_control: (cache_mode == PromptCacheMode::Automatic)
            .then_some(cache_control),
        system,
        messages,
        temperature: request
            .get("temperature")
            .and_then(JsonValue::as_f64)
            .map(|value| value as f32),
        top_p: request
            .get("top_p")
            .and_then(JsonValue::as_f64)
            .map(|value| value as f32),
        top_k: None,
        stream: request.get("stream").and_then(JsonValue::as_bool),
        tools,
        tool_choice,
        stop_sequences: request.get("stop").map(string_list).unwrap_or_default(),
        thinking,
        output_config: anthropic_effort.map(|effort| claude::OutputConfig {
            effort: Some(effort),
        }),
    })
}

/// Standalone entry point that collects the reversible tool mapping from the
/// request and then delegates to [`openai_responses_to_claude_messages_with_tools`].
pub fn openai_responses_to_claude_messages(
    raw: &[u8],
    upstream_model: impl Into<String>,
) -> Result<claude::MessagesRequest, ProtocolError> {
    let mapping = responses_tool_mapping(raw)?;
    openai_responses_to_claude_messages_with_tools(raw, upstream_model, &mapping)
}

/// Standalone Responses converter with an explicit prompt-cache mode.
pub fn openai_responses_to_claude_messages_with_cache_mode(
    raw: &[u8],
    upstream_model: impl Into<String>,
    cache_mode: PromptCacheMode,
) -> Result<claude::MessagesRequest, ProtocolError> {
    let mapping = responses_tool_mapping(raw)?;
    openai_responses_to_claude_messages_with_tools_and_cache_mode(
        raw,
        upstream_model,
        &mapping,
        cache_mode,
    )
}

pub fn claude_messages_to_openai_responses(
    response: claude::MessagesResponse,
    requested_model: impl Into<String>,
) -> JsonValue {
    claude_messages_to_openai_responses_with_options(response, requested_model, true)
}

pub fn claude_messages_to_openai_responses_with_options(
    response: claude::MessagesResponse,
    requested_model: impl Into<String>,
    include_encrypted_reasoning: bool,
) -> JsonValue {
    claude_messages_to_openai_responses_with_options_and_tools(
        response,
        requested_model,
        include_encrypted_reasoning,
        &ResponsesToolMapping::default(),
    )
}

pub fn claude_messages_to_openai_responses_with_options_and_tools(
    response: claude::MessagesResponse,
    requested_model: impl Into<String>,
    include_encrypted_reasoning: bool,
    tool_mapping: &ResponsesToolMapping,
) -> JsonValue {
    let model = requested_model.into();
    let mut output = Vec::new();
    let mut message_parts = Vec::new();
    for block in response.content {
        match block {
            claude::ContentBlock::Thinking {
                thinking,
                signature,
                ..
            } => {
                flush_message_parts(&mut output, &mut message_parts);
                let mut item = json!({
                    "type": "reasoning",
                    "id": item_id("rs"),
                    "summary": [{"type": "summary_text", "text": thinking}],
                });
                if include_encrypted_reasoning
                    && let Some(signature) = signature.filter(|value| !value.is_empty())
                {
                    item["encrypted_content"] = JsonValue::String(signature);
                }
                output.push(item);
            }
            claude::ContentBlock::RedactedThinking { data, .. } => {
                flush_message_parts(&mut output, &mut message_parts);
                let mut item = json!({
                    "type": "reasoning",
                    "id": item_id("rs"),
                    "summary": [],
                });
                if include_encrypted_reasoning {
                    item["encrypted_content"] = JsonValue::String(data);
                }
                output.push(item);
            }
            claude::ContentBlock::Text { text, .. } => {
                message_parts.push(json!({"type": "output_text", "text": text, "annotations": []}));
            }
            claude::ContentBlock::ToolUse { id, name, input, .. } => {
                flush_message_parts(&mut output, &mut message_parts);
                output.push(tool_use_to_responses_item(id, name, input, tool_mapping));
            }
            claude::ContentBlock::ServerToolUse { id, name, input, .. } => {
                flush_message_parts(&mut output, &mut message_parts);
                output.push(server_tool_use_to_responses_item(id, name, input));
            }
            claude::ContentBlock::WebSearchToolResult { .. }
            | claude::ContentBlock::Image { .. }
            | claude::ContentBlock::ToolResult { .. } => {}
        }
    }
    flush_message_parts(&mut output, &mut message_parts);
    if output.is_empty() {
        output.push(json!({
            "type": "message",
            "id": item_id("msg"),
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "", "annotations": []}],
        }));
    }

    let status = if response.stop_reason.as_deref() == Some("max_tokens") {
        "incomplete"
    } else {
        "completed"
    };
    let usage = response.usage.unwrap_or_default();
    let input_tokens = usage
        .input_tokens
        .saturating_add(usage.cache_read_input_tokens)
        .saturating_add(usage.cache_creation_input_tokens);
    let mut result = json!({
        "id": if response.id.is_empty() { response_id() } else { response.id },
        "object": "response",
        "created_at": now_unix(),
        "status": status,
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "usage": {
            "input_tokens": input_tokens,
            "input_tokens_details": {"cached_tokens": usage.cache_read_input_tokens},
            "output_tokens": usage.output_tokens,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": input_tokens.saturating_add(usage.output_tokens),
            "cache_creation_input_tokens": usage.cache_creation_input_tokens,
        },
        "error": null,
    });
    if status == "incomplete" {
        result["incomplete_details"] = json!({"reason": "max_output_tokens"});
    }
    result
}

fn tool_use_to_responses_item(
    id: String,
    name: String,
    input: JsonValue,
    mapping: &ResponsesToolMapping,
) -> JsonValue {
    if mapping.custom_tools.contains(&name) {
        let input = extract_custom_tool_input_from_json(&input);
        return json!({
            "type": "custom_tool_call",
            "id": item_id("ct"),
            "call_id": id,
            "name": name,
            "input": input,
            "status": "completed",
        });
    }
    if mapping.tool_search && name == "tool_search" {
        return json!({
            "type": "tool_search_call",
            "id": item_id("ts"),
            "call_id": id,
            "execution": "client",
            "arguments": input,
            "status": "completed",
        });
    }
    if let Some(entry) = mapping.namespace_entry(&name) {
        return json!({
            "type": "function_call",
            "id": item_id("fc"),
            "call_id": id,
            "name": entry.name,
            "namespace": entry.namespace,
            "arguments": input.to_string(),
            "status": "completed",
        });
    }
    json!({
        "type": "function_call",
        "id": item_id("fc"),
        "call_id": id,
        "name": name,
        "arguments": input.to_string(),
        "status": "completed",
    })
}

fn server_tool_use_to_responses_item(id: String, name: String, input: JsonValue) -> JsonValue {
    let query = input
        .get("query")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    if name == "web_search" {
        return json!({
            "type": "web_search_call",
            "id": id,
            "status": "completed",
            "action": {"type":"search", "query":query},
        });
    }
    json!({
        "type": "function_call",
        "id": item_id("fc"),
        "call_id": id,
        "name": name,
        "arguments": input.to_string(),
        "status": "completed",
    })
}

#[derive(Debug)]
pub struct ClaudeSseToResponsesTranslator {
    response_id: String,
    model: String,
    created_at: u64,
    sequence: u64,
    output_index: usize,
    current: Option<StreamItem>,
    output: Vec<JsonValue>,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_creation_tokens: u32,
    stop_reason: Option<String>,
    error: Option<JsonValue>,
    created: bool,
    terminal: bool,
    include_encrypted_reasoning: bool,
    tool_mapping: ResponsesToolMapping,
}

#[derive(Debug)]
enum StreamItem {
    Message {
        id:            String,
        parts:         Vec<JsonValue>,
        text:          String,
        content_index: usize,
    },
    Reasoning {
        id:        String,
        summary:   String,
        signature: String,
    },
    ServerTool {
        id:    String,
        name:  String,
        input: String,
    },
    Function {
        id:        String,
        call_id:   String,
        name:      String,
        arguments: String,
    },
}

impl ClaudeSseToResponsesTranslator {
    pub fn new(requested_model: impl Into<String>) -> Self {
        Self::with_encrypted_reasoning(requested_model, true)
    }

    pub fn with_encrypted_reasoning(
        requested_model: impl Into<String>,
        include_encrypted_reasoning: bool,
    ) -> Self {
        Self::with_options(
            requested_model,
            include_encrypted_reasoning,
            ResponsesToolMapping::default(),
        )
    }

    pub fn with_options(
        requested_model: impl Into<String>,
        include_encrypted_reasoning: bool,
        tool_mapping: ResponsesToolMapping,
    ) -> Self {
        Self {
            response_id: response_id(),
            model: requested_model.into(),
            created_at: now_unix(),
            sequence: 0,
            output_index: 0,
            current: None,
            output: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            stop_reason: None,
            error: None,
            created: false,
            terminal: false,
            include_encrypted_reasoning,
            tool_mapping,
        }
    }

    pub fn translate_sse_payload(
        &mut self,
        payload: &str,
    ) -> Result<Vec<JsonValue>, ProtocolError> {
        if payload == "[DONE]" {
            return Ok(self.finish());
        }
        let raw_event: JsonValue = serde_json::from_str(payload)?;
        if raw_event.get("type").and_then(JsonValue::as_str) == Some("error") {
            return Ok(self.fail_from_claude_error(&raw_event));
        }
        let event: claude::StreamEvent = serde_json::from_value(raw_event)?;
        let mut out = Vec::new();
        match event.event_type.as_str() {
            "message_start" => {
                if let Some(message) = event.message {
                    if !message.id.is_empty() {
                        self.response_id = message.id;
                    }
                    if let Some(usage) = message.usage {
                        self.merge_usage(usage);
                    }
                }
                if !self.created {
                    self.created = true;
                    let response = self.response_value("in_progress", Vec::new());
                    out.push(self.event("response.created", json!({"response": response})));
                }
            }
            "content_block_start" => {
                out.extend(self.close_current());
                match event.content_block {
                    Some(claude::ContentBlock::Text { text, .. }) => {
                        let id = item_id("msg");
                        out.push(self.event(
                            "response.output_item.added",
                            json!({
                                "output_index": self.output_index,
                                "item": {"type":"message","id":id,"status":"in_progress","role":"assistant","content":[]}
                            }),
                        ));
                        out.push(self.event(
                            "response.content_part.added",
                            json!({
                                "output_index": self.output_index,
                                "content_index": 0,
                                "item_id": id,
                                "part": {"type":"output_text","text":"","annotations":[]}
                            }),
                        ));
                        self.current = Some(StreamItem::Message {
                            id,
                            parts: Vec::new(),
                            text: String::new(),
                            content_index: 0,
                        });
                        if !text.is_empty() {
                            out.extend(self.text_delta(&text));
                        }
                    }
                    Some(claude::ContentBlock::Thinking {
                        thinking,
                        signature,
                        ..
                    }) => {
                        let id = item_id("rs");
                        let output_index = self.output_index;
                        out.push(self.event(
                            "response.output_item.added",
                            json!({"output_index":output_index,"item":{"type":"reasoning","id":id,"summary":[]}}),
                        ));
                        out.push(self.event(
                            "response.reasoning_summary_part.added",
                            json!({
                                "output_index":output_index,
                                "summary_index":0,
                                "item_id":id,
                                "part":{"type":"summary_text","text":""}
                            }),
                        ));
                        self.current = Some(StreamItem::Reasoning {
                            id,
                            summary: String::new(),
                            signature: signature.unwrap_or_default(),
                        });
                        if !thinking.is_empty() {
                            out.extend(self.reasoning_delta(&thinking));
                        }
                    }
                    Some(claude::ContentBlock::RedactedThinking { data, .. }) => {
                        let id = item_id("rs");
                        let output_index = self.output_index;
                        let mut item = json!({"type":"reasoning","id":id,"summary":[]});
                        if self.include_encrypted_reasoning {
                            item["encrypted_content"] = JsonValue::String(data.clone());
                        }
                        out.push(self.event(
                            "response.output_item.added",
                            json!({"output_index":output_index,"item":item}),
                        ));
                        out.push(self.event(
                            "response.reasoning_summary_part.added",
                            json!({
                                "output_index":output_index,
                                "summary_index":0,
                                "item_id":id,
                                "part":{"type":"summary_text","text":""}
                            }),
                        ));
                        self.current = Some(StreamItem::Reasoning {
                            id,
                            summary: String::new(),
                            signature: data,
                        });
                    }
                    Some(claude::ContentBlock::ToolUse {
                        id: call_id,
                        name,
                        input,
                        ..
                    }) => {
                        let item_type = self.tool_item_type(&name);
                        let id = item_id(if item_type == "custom_tool_call" {
                            "ct"
                        } else if item_type == "tool_search_call" {
                            "ts"
                        } else {
                            "fc"
                        });
                        let mut item = json!({
                            "type":item_type,
                            "id":id,
                            "call_id":call_id,
                            "status":"in_progress"
                        });
                        if item_type == "custom_tool_call" {
                            item["name"] = JsonValue::String(name.clone());
                            item["input"] = JsonValue::String(String::new());
                        } else if item_type == "tool_search_call" {
                            item["execution"] = JsonValue::String("client".to_string());
                            item["arguments"] = json!({});
                        } else {
                            item["name"] = JsonValue::String(name.clone());
                            item["arguments"] = JsonValue::String(String::new());
                        }
                        out.push(self.event(
                            "response.output_item.added",
                            json!({"output_index":self.output_index,"item":item}),
                        ));
                        self.current = Some(StreamItem::Function {
                            id,
                            call_id,
                            name,
                            arguments: if input.is_null() || input == json!({}) {
                                String::new()
                            } else {
                                input.to_string()
                            },
                        });
                    }
                    Some(claude::ContentBlock::ServerToolUse { id, name, input, .. }) => {
                        let output_index = self.output_index;
                        out.push(self.event(
                            "response.output_item.added",
                            json!({
                                "output_index": output_index,
                                "item": {
                                    "type":"web_search_call",
                                    "id":id,
                                    "status":"in_progress",
                                    "action":{"type":"search","query":input.get("query").and_then(JsonValue::as_str).unwrap_or_default()}
                                }
                            }),
                        ));
                        self.current = Some(StreamItem::ServerTool {
                            id,
                            name,
                            input: if input.is_null() || input == json!({}) {
                                String::new()
                            } else {
                                input.to_string()
                            },
                        });
                    }
                    Some(claude::ContentBlock::WebSearchToolResult { .. }) => {}
                    _ => {}
                }
            }
            "content_block_delta" => {
                if let Some(delta) = event.delta {
                    match delta.get("type").and_then(JsonValue::as_str) {
                        Some("text_delta") => {
                            if let Some(text) = delta.get("text").and_then(JsonValue::as_str) {
                                out.extend(self.text_delta(text));
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(text) = delta.get("thinking").and_then(JsonValue::as_str) {
                                out.extend(self.reasoning_delta(text));
                            }
                        }
                        Some("signature_delta") => {
                            if let Some(signature) =
                                delta.get("signature").and_then(JsonValue::as_str)
                                && let Some(StreamItem::Reasoning {
                                    signature: stored, ..
                                }) = self.current.as_mut()
                            {
                                stored.push_str(signature);
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(arguments) =
                                delta.get("partial_json").and_then(JsonValue::as_str)
                            {
                                out.extend(self.arguments_delta(arguments));
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => out.extend(self.close_current()),
            "message_delta" => {
                if let Some(usage) = event.usage {
                    self.merge_usage(usage);
                }
                self.stop_reason = event
                    .delta
                    .as_ref()
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(JsonValue::as_str)
                    .map(str::to_string)
                    .or(self.stop_reason.take());
            }
            "message_stop" => out.extend(self.finish()),
            _ => {}
        }
        Ok(out)
    }

    pub fn finish(&mut self) -> Vec<JsonValue> {
        if self.terminal {
            return Vec::new();
        }
        let mut out = self.close_current();
        if !self.created {
            self.created = true;
            let response = self.response_value("in_progress", Vec::new());
            out.push(self.event("response.created", json!({"response": response})));
        }
        self.terminal = true;
        let status = if self.error.is_some() {
            "failed"
        } else if self.stop_reason.as_deref() == Some("max_tokens") {
            "incomplete"
        } else {
            "completed"
        };
        let response = self.response_value(status, self.output.clone());
        let event_type = match status {
            "failed" => "response.failed",
            "incomplete" => "response.incomplete",
            _ => "response.completed",
        };
        out.push(self.event(event_type, json!({"response": response})));
        out
    }

    pub fn fail(&mut self, code: impl Into<String>, message: impl Into<String>) -> Vec<JsonValue> {
        self.fail_from_claude_error(&json!({
            "error": {
                "type": code.into(),
                "message": message.into(),
            }
        }))
    }

    fn text_delta(&mut self, delta: &str) -> Vec<JsonValue> {
        let Some(StreamItem::Message {
            id,
            text,
            content_index,
            ..
        }) = self.current.as_mut()
        else {
            return Vec::new();
        };
        text.push_str(delta);
        let id = id.clone();
        let content_index = *content_index;
        vec![self.event(
            "response.output_text.delta",
            json!({"output_index":self.output_index,"content_index":content_index,"item_id":id,"delta":delta}),
        )]
    }

    fn reasoning_delta(&mut self, delta: &str) -> Vec<JsonValue> {
        let Some(StreamItem::Reasoning { id, summary, .. }) = self.current.as_mut() else {
            return Vec::new();
        };
        summary.push_str(delta);
        let id = id.clone();
        vec![self.event(
            "response.reasoning_summary_text.delta",
            json!({"output_index":self.output_index,"summary_index":0,"item_id":id,"delta":delta}),
        )]
    }

    fn arguments_delta(&mut self, delta: &str) -> Vec<JsonValue> {
        match self.current.as_mut() {
            Some(StreamItem::Function {
                id,
                call_id,
                name,
                arguments,
            }) => {
                arguments.push_str(delta);
                let (id, call_id, name) = (id.clone(), call_id.clone(), name.clone());
                if self.tool_item_type(&name) == "function_call" {
                    vec![self.event(
                        "response.function_call_arguments.delta",
                        json!({"output_index":self.output_index,"item_id":id,"call_id":call_id,"name":name,"delta":delta}),
                    )]
                } else {
                    Vec::new()
                }
            }
            // Server tool input arrives as input_json_delta fragments; accumulate
            // them so close_current can reconstruct the full query.
            Some(StreamItem::ServerTool { input, .. }) => {
                input.push_str(delta);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn close_current(&mut self) -> Vec<JsonValue> {
        let Some(current) = self.current.take() else {
            return Vec::new();
        };
        let mut events = Vec::new();
        let item = match current {
            StreamItem::Message {
                id,
                mut parts,
                text,
                content_index,
            } => {
                let part = json!({"type":"output_text","text":text,"annotations":[]});
                events.push(self.event(
                    "response.output_text.done",
                    json!({"output_index":self.output_index,"content_index":content_index,"item_id":id,"text":text}),
                ));
                events.push(self.event(
                    "response.content_part.done",
                    json!({"output_index":self.output_index,"content_index":content_index,"item_id":id,"part":part}),
                ));
                parts.push(part);
                json!({"type":"message","id":id,"status":"completed","role":"assistant","content":parts})
            }
            StreamItem::Reasoning {
                id,
                summary,
                signature,
            } => {
                events.push(self.event(
                    "response.reasoning_summary_text.done",
                    json!({"output_index":self.output_index,"summary_index":0,"item_id":id,"text":summary}),
                ));
                events.push(self.event(
                    "response.reasoning_summary_part.done",
                    json!({
                        "output_index":self.output_index,
                        "summary_index":0,
                        "item_id":id,
                        "part":{"type":"summary_text","text":summary}
                    }),
                ));
                let mut item = json!({
                    "type":"reasoning",
                    "id":id,
                    "summary": if summary.is_empty() { Vec::<JsonValue>::new() } else { vec![json!({"type":"summary_text","text":summary})] },
                });
                if self.include_encrypted_reasoning && !signature.is_empty() {
                    item["encrypted_content"] = JsonValue::String(signature);
                }
                item
            }
            StreamItem::Function {
                id,
                call_id,
                name,
                arguments,
            } => {
                let arguments = if arguments.is_empty() {
                    "{}".to_string()
                } else {
                    arguments
                };
                let input: JsonValue =
                    serde_json::from_str(&arguments).unwrap_or_else(|_| json!({}));
                let item = tool_use_to_responses_item(
                    call_id.clone(),
                    name.clone(),
                    input,
                    &self.tool_mapping,
                );
                match item.get("type").and_then(JsonValue::as_str) {
                    Some("custom_tool_call") => {
                        let input = item.get("input").and_then(JsonValue::as_str).unwrap_or_default();
                        if !input.is_empty() {
                            events.push(self.event(
                                "response.custom_tool_call_input.delta",
                                json!({"output_index":self.output_index,"item_id":id,"delta":input}),
                            ));
                        }
                        events.push(self.event(
                            "response.custom_tool_call_input.done",
                            json!({"output_index":self.output_index,"item_id":id,"call_id":call_id,"name":name,"input":input}),
                        ));
                    }
                    Some("function_call") => events.push(self.event(
                        "response.function_call_arguments.done",
                        json!({"output_index":self.output_index,"item_id":id,"call_id":call_id,"name":name,"arguments":arguments}),
                    )),
                    _ => {}
                }
                let mut item = item;
                item["id"] = JsonValue::String(id);
                item
            }
            StreamItem::ServerTool { id, name, input } => {
                let input: JsonValue = serde_json::from_str(&input).unwrap_or_else(|_| json!({}));
                server_tool_use_to_responses_item(id, name, input)
            }
        };
        events.push(self.event(
            "response.output_item.done",
            json!({"output_index":self.output_index,"item":item}),
        ));
        self.output.push(item);
        self.output_index += 1;
        events
    }

    fn response_value(&self, status: &str, output: Vec<JsonValue>) -> JsonValue {
        let input_tokens = self
            .input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_creation_tokens);
        let mut response = json!({
            "id":self.response_id,
            "object":"response",
            "created_at":self.created_at,
            "status":status,
            "model":self.model,
            "output":output,
            "parallel_tool_calls":true,
            "usage": if status == "in_progress" { JsonValue::Null } else { json!({
                "input_tokens":input_tokens,
                "input_tokens_details":{"cached_tokens":self.cache_read_tokens},
                "output_tokens":self.output_tokens,
                "output_tokens_details":{"reasoning_tokens":0},
                "total_tokens":input_tokens.saturating_add(self.output_tokens),
                "cache_creation_input_tokens":self.cache_creation_tokens,
            }) },
            "error": if status == "failed" { self.error.clone().unwrap_or(JsonValue::Null) } else { JsonValue::Null },
        });
        if status == "incomplete" {
            response["incomplete_details"] = json!({"reason":"max_output_tokens"});
        }
        response
    }

    fn tool_item_type(&self, name: &str) -> &'static str {
        if self.tool_mapping.custom_tools.contains(name) {
            "custom_tool_call"
        } else if self.tool_mapping.tool_search && name == "tool_search" {
            "tool_search_call"
        } else {
            "function_call"
        }
    }

    fn fail_from_claude_error(&mut self, event: &JsonValue) -> Vec<JsonValue> {
        if self.terminal {
            return Vec::new();
        }

        let upstream = event.get("error").unwrap_or(event);
        let code = upstream
            .get("code")
            .or_else(|| upstream.get("type"))
            .and_then(JsonValue::as_str)
            .unwrap_or("upstream_error")
            .to_string();
        let message = upstream
            .get("message")
            .and_then(JsonValue::as_str)
            .unwrap_or("Anthropic stream failed")
            .to_string();
        let param = upstream.get("param").cloned().unwrap_or(JsonValue::Null);
        self.error = Some(json!({"code":code,"message":message}));

        let mut out = self.close_current();
        if !self.created {
            self.created = true;
            let response = self.response_value("in_progress", Vec::new());
            out.push(self.event("response.created", json!({"response": response})));
        }
        out.push(self.event(
            "error",
            json!({"code":code,"message":message,"param":param}),
        ));
        self.terminal = true;
        let response = self.response_value("failed", self.output.clone());
        out.push(self.event("response.failed", json!({"response":response})));
        out
    }

    fn event(&mut self, event_type: &str, fields: JsonValue) -> JsonValue {
        let mut object = fields.as_object().cloned().unwrap_or_default();
        object.insert(
            "type".to_string(),
            JsonValue::String(event_type.to_string()),
        );
        object.insert("sequence_number".to_string(), json!(self.sequence));
        self.sequence = self.sequence.saturating_add(1);
        JsonValue::Object(object)
    }

    fn merge_usage(&mut self, usage: claude::Usage) {
        self.input_tokens = self.input_tokens.max(usage.input_tokens);
        self.output_tokens = self.output_tokens.max(usage.output_tokens);
        self.cache_read_tokens = self.cache_read_tokens.max(usage.cache_read_input_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .max(usage.cache_creation_input_tokens);
    }
}

fn convert_input_item(
    item: &JsonValue,
    tool_mapping: &ResponsesToolMapping,
    system: &mut Vec<String>,
    messages: &mut Vec<claude::Message>,
    hoist_system: bool,
) -> Result<(), ProtocolError> {
    let item_type = item
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    let role = item
        .get("role")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    if hoist_system && matches!(role, "system" | "developer") {
        let text = content_text(item.get("content"))?;
        if !text.is_empty() {
            system.push(text);
        }
        return Ok(());
    }
    match item_type {
        "reasoning" => {
            let signature = item
                .get("encrypted_content")
                .and_then(JsonValue::as_str)
                .unwrap_or_default();
            let thinking = item
                .get("summary")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("text").and_then(JsonValue::as_str))
                .collect::<Vec<_>>()
                .join("");
            if !thinking.is_empty() || !signature.is_empty() {
                push_message(messages, "assistant", vec![
                    claude::ContentBlock::Thinking {
                        thinking,
                        signature: (!signature.is_empty()).then(|| signature.to_string()),
                        cache_control: None,
                    },
                ]);
            }
        }
        "function_call" | "tool_search_call" => {
            let arguments = item.get("arguments").unwrap_or(&JsonValue::Null);
            let arguments: JsonValue = match arguments {
                JsonValue::String(raw) => serde_json::from_str(raw).map_err(|error| {
                    ProtocolError::UnsupportedRequest(format!(
                        "function_call.arguments must be valid JSON: {error}"
                    ))
                })?,
                JsonValue::Null => json!({}),
                value => value.clone(),
            };
            if !arguments.is_object() {
                return Err(ProtocolError::UnsupportedRequest(
                    "function_call.arguments must decode to a JSON object for Anthropic"
                        .to_string(),
                ));
            }
            push_message(messages, "assistant", vec![claude::ContentBlock::ToolUse {
                id:    anthropic_call_id(
                    item.get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default(),
                ),
                name:  resolve_function_call_name(item, item_type, tool_mapping),
                input: arguments,
                cache_control: None,
            }]);
        }
        "custom_tool_call" => {
            let input = item
                .get("input")
                .and_then(JsonValue::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    item.get("input")
                        .filter(|value| !value.is_null())
                        .map(JsonValue::to_string)
                        .unwrap_or_default()
                });
            push_message(messages, "assistant", vec![claude::ContentBlock::ToolUse {
                id:    anthropic_call_id(
                    item.get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default(),
                ),
                name:  item
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
                input: json!({"input": input}),
                cache_control: None,
            }]);
        }
        "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
            let content = tool_result_content(item.get("output"))?;
            push_message(messages, "user", vec![claude::ContentBlock::ToolResult {
                tool_use_id: anthropic_call_id(
                    item.get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default(),
                ),
                content,
                is_error: (item.get("status").and_then(JsonValue::as_str) == Some("failed"))
                    .then_some(true),
                cache_control: None,
            }]);
        }
        _ if role == "assistant" => {
            let blocks = content_to_claude(item.get("content"), false)?;
            if !blocks.is_empty() {
                push_message(messages, "assistant", blocks);
            }
        }
        "item_reference" => {
            return Err(ProtocolError::UnsupportedRequest(
                "item_reference requires Responses server state; inline the full item instead"
                    .to_string(),
            ));
        }
        "message" | "" => {
            let blocks = content_to_claude(item.get("content"), true)?;
            if !blocks.is_empty() {
                push_message(messages, "user", blocks);
            }
        }
        other => {
            return Err(ProtocolError::UnsupportedRequest(format!(
                "Responses input item type {other:?} cannot be represented by Anthropic Messages"
            )));
        }
    }
    Ok(())
}

fn flush_message_parts(output: &mut Vec<JsonValue>, parts: &mut Vec<JsonValue>) {
    if parts.is_empty() {
        return;
    }
    output.push(json!({
        "type": "message",
        "id": item_id("msg"),
        "status": "completed",
        "role": "assistant",
        "content": std::mem::take(parts),
    }));
}

fn content_to_claude(
    content: Option<&JsonValue>,
    allow_images: bool,
) -> Result<Vec<claude::ContentBlock>, ProtocolError> {
    match content {
        Some(JsonValue::String(text)) => {
            Ok(vec![claude::ContentBlock::Text {
                text: text.clone(),
                cache_control: None,
            }])
        }
        Some(JsonValue::Array(parts)) => {
            let mut blocks = Vec::with_capacity(parts.len());
            for part in parts {
                match part.get("type").and_then(JsonValue::as_str) {
                    Some("input_text" | "output_text" | "text") => {
                        if let Some(text) = part.get("text").and_then(JsonValue::as_str) {
                            blocks.push(claude::ContentBlock::Text {
                                text: text.to_string(),
                                cache_control: None,
                            });
                        }
                    }
                    Some("input_image") if allow_images => {
                        let url = part
                            .get("image_url")
                            .or_else(|| part.get("url"))
                            .and_then(JsonValue::as_str)
                            .ok_or_else(|| {
                                ProtocolError::UnsupportedRequest(
                                    "input_image requires image_url for Anthropic".to_string(),
                                )
                            })?;
                        blocks.push(image_block(url).ok_or_else(|| {
                            ProtocolError::UnsupportedRequest(
                                "input_image data URL is invalid for Anthropic".to_string(),
                            )
                        })?);
                    }
                    Some("input_image") => {
                        return Err(ProtocolError::UnsupportedRequest(
                            "assistant input_image cannot be represented by Anthropic Messages"
                                .to_string(),
                        ));
                    }
                    Some(kind) => {
                        return Err(ProtocolError::UnsupportedRequest(format!(
                            "Responses content part {kind} cannot be represented by Anthropic \
                             Messages"
                        )));
                    }
                    None => {
                        return Err(ProtocolError::UnsupportedRequest(
                            "Responses content part is missing type".to_string(),
                        ));
                    }
                }
            }
            Ok(blocks)
        }
        Some(value) if !value.is_null() => Ok(vec![claude::ContentBlock::Text {
            text: value.to_string(),
            cache_control: None,
        }]),
        _ => Ok(Vec::new()),
    }
}

fn image_block(url: &str) -> Option<claude::ContentBlock> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (media_type, data) = rest.split_once(";base64,")?;
        return Some(claude::ContentBlock::Image {
            source: claude::ImageSource {
                source_type: "base64".to_string(),
                media_type:  Some(media_type.to_string()),
                data:        Some(data.to_string()),
                url:         None,
            },
            cache_control: None,
        });
    }
    Some(claude::ContentBlock::Image {
        source: claude::ImageSource {
            source_type: "url".to_string(),
            media_type:  None,
            data:        None,
            url:         Some(url.to_string()),
        },
        cache_control: None,
    })
}

fn tool_result_content(
    output: Option<&JsonValue>,
) -> Result<claude::ToolResultContent, ProtocolError> {
    Ok(match output {
        Some(JsonValue::String(text)) => claude::ToolResultContent::Text(text.clone()),
        Some(JsonValue::Array(parts)) => {
            let blocks = content_to_claude(Some(&JsonValue::Array(parts.clone())), true)?;
            if !blocks.is_empty() {
                claude::ToolResultContent::Blocks(blocks)
            } else {
                claude::ToolResultContent::Text("(empty)".to_string())
            }
        }
        Some(value) if !value.is_null() => claude::ToolResultContent::Json(value.clone()),
        _ => claude::ToolResultContent::Text("(empty)".to_string()),
    })
}

fn convert_tools(tools: Option<&JsonValue>) -> Result<Option<Vec<claude::Tool>>, ProtocolError> {
    let tools = match tools {
        Some(value) => value.as_array(),
        None => None,
    };
    let tools = match tools {
        Some(tools) => tools,
        None => return Ok(None),
    };
    let mut converted = Vec::new();
    let mut seen_flat = BTreeSet::new();
    for tool in tools {
        let tool_type = tool
            .get("type")
            .and_then(JsonValue::as_str)
            .unwrap_or("function");
        if matches!(
            tool_type,
            "web_search" | "web_search_preview" | "google_search" | "web_search_20250305"
        ) {
            converted.push(claude::Tool {
                tool_type:    Some("web_search_20250305".to_string()),
                name:         "web_search".to_string(),
                description:  None,
                input_schema: JsonValue::Null,
                cache_control: None,
            });
            continue;
        }
        if matches!(
            tool_type,
            "file_search" | "computer" | "image_generation" | "code_interpreter" | "mcp"
        ) {
            return Err(ProtocolError::UnsupportedRequest(format!(
                "Responses server tool {tool_type:?} cannot be represented on an Anthropic \
                 Messages upstream; use a channel that supports it or remove the tool"
            )));
        }
        if tool_type == "tool_search" {
            converted.push(claude::Tool {
                tool_type:    None,
                name:         "tool_search".to_string(),
                description:  Some(
                    "Search and load Codex tools, plugins, connectors, and MCP \
                     namespaces for the current task."
                        .to_string(),
                ),
                input_schema: json!({
                    "type":"object",
                    "properties": {
                        "query": {"type":"string","description":"Search query for tools or connectors to load."},
                        "limit": {"type":"integer","description":"Maximum number of tool groups to return."}
                    },
                    "required":["query"]
                }),
                cache_control: None,
            });
            continue;
        }
        if tool_type == "namespace" {
            for child in namespace_children(tool) {
                if child.get("type").and_then(JsonValue::as_str) != Some("function") {
                    continue;
                }
                let Some(child_name) = child
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                else {
                    continue;
                };
                let Some(namespace) = tool
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                else {
                    continue;
                };
                let flat = flatten_namespace_tool_name(namespace, child_name);
                if !seen_flat.insert(flat.clone()) {
                    continue;
                }
                let mut schema = child
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object","properties":{}}));
                normalize_schema(&mut schema);
                converted.push(claude::Tool {
                    tool_type:    None,
                    name:         flat,
                    description:  child
                        .get("description")
                        .and_then(JsonValue::as_str)
                        .map(str::to_string),
                    input_schema: schema,
                    cache_control: None,
                });
            }
            continue;
        }
        let function = tool.get("function").unwrap_or(tool);
        let Some(name) = function.get("name").and_then(JsonValue::as_str) else {
            continue;
        };
        let mut schema = function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type":"object","properties":{}}));
        if tool_type == "custom" {
            // custom tools are lowered to function tools whose input is the
            // verbatim free-text the client sent. Force the {"input": string}
            // schema so the model produces arguments we can round-trip back to
            // custom_tool_call.input (mirrors sub2api's customToolInputSchema).
            schema = json!({
                "type":"object",
                "properties":{"input":{"type":"string","description":"The raw input for this tool, passed through verbatim."}},
                "required":["input"]
            });
        }
        normalize_schema(&mut schema);
        converted.push(claude::Tool {
            tool_type:    None,
            name:         name.to_string(),
            description:  function
                .get("description")
                .and_then(JsonValue::as_str)
                .map(str::to_string),
            input_schema: schema,
            cache_control: None,
        });
    }
    Ok((!converted.is_empty()).then_some(converted))
}

fn effective_tools(request: &JsonValue) -> Option<JsonValue> {
    let mut tools = request
        .get("tools")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();
    for item in request
        .get("input")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        if item.get("type").and_then(JsonValue::as_str) == Some("additional_tools")
            && let Some(additional) = item.get("tools").and_then(JsonValue::as_array)
        {
            tools.extend(additional.iter().cloned());
        }
    }
    (!tools.is_empty()).then_some(JsonValue::Array(tools))
}

fn convert_tool_choice(
    choice: &JsonValue,
    tool_mapping: &ResponsesToolMapping,
) -> Option<JsonValue> {
    match choice {
        JsonValue::String(value) => Some(match value.as_str() {
            "required" => json!({"type":"any"}),
            "auto" => json!({"type":"auto"}),
            "none" => json!({"type":"none"}),
            _ => choice.clone(),
        }),
        JsonValue::Object(object)
            if matches!(
                object.get("type").and_then(JsonValue::as_str),
                Some("function") | Some("custom") | Some("tool_search") | Some("web_search")
            ) =>
        {
            let choice_type = object
                .get("type")
                .and_then(JsonValue::as_str)
                .unwrap_or_default();
            let namespace = object
                .get("namespace")
                .and_then(JsonValue::as_str)
                .unwrap_or_default();
            let base_name = object
                .get("name")
                .or_else(|| object.get("function").and_then(|f| f.get("name")))
                .and_then(JsonValue::as_str)
                .or_else(|| (choice_type == "tool_search").then_some("tool_search"))
                .or_else(|| (choice_type == "web_search").then_some("web_search"));
            let name = match (namespace, base_name) {
                (namespace, Some(name)) if !namespace.is_empty() => tool_mapping
                    .flatten_declared(namespace, name)
                    .map(str::to_string)
                    .unwrap_or_else(|| name.to_string()),
                (_, Some(name)) => name.to_string(),
                _ => return Some(json!({"type":"auto"})),
            };
            Some(json!({"type":"tool","name":name}))
        }
        JsonValue::Object(object)
            if object.get("type").and_then(JsonValue::as_str) == Some("namespace") =>
        {
            // Anthropic cannot pin a tool choice to a whole namespace group;
            // fall back to auto so the model can still pick any flattened child.
            Some(json!({"type":"auto"}))
        }
        _ => Some(choice.clone()),
    }
}

fn normalize_schema(schema: &mut JsonValue) {
    if !schema.is_object() {
        *schema = json!({"type":"object","properties":{}});
        return;
    }
    let object = schema.as_object_mut().expect("checked object");
    if object.get("type").and_then(JsonValue::as_str) != Some("object") {
        *schema = json!({"type":"object","properties":{}});
        return;
    }
    object
        .entry("type".to_string())
        .or_insert_with(|| JsonValue::String("object".to_string()));
    object
        .entry("properties".to_string())
        .or_insert_with(|| JsonValue::Object(Map::new()));
}

fn mark_last_content_block(message: &mut claude::Message, cache_control: &JsonValue) {
    if let Some(block) = message.content.last_mut() {
        *block.cache_control_mut() = Some(cache_control.clone());
    }
}

/// Applies sub2api-style explicit message breakpoints:
///
/// - clear all old message markers first, making repeated conversion
///   deterministic;
/// - mark the last message;
/// - when there is enough history, also mark the second-last user message.
///
/// Together with the system and last-tool markers this stays within
/// Anthropic's four explicit breakpoint limit and preserves a reusable
/// message prefix as the conversation grows.
fn apply_explicit_message_cache_breakpoints(
    messages: &mut [claude::Message],
    cache_control: &JsonValue,
) {
    for message in messages.iter_mut() {
        for block in &mut message.content {
            *block.cache_control_mut() = None;
        }
    }

    if let Some(last) = messages.last_mut() {
        mark_last_content_block(last, cache_control);
    }

    if messages.len() < 4 {
        return;
    }

    let mut user_count = 0;
    let second_last_user = messages.iter().enumerate().rev().find_map(|(index, message)| {
        if message.role != "user" {
            return None;
        }
        user_count += 1;
        (user_count == 2).then_some(index)
    });
    if let Some(index) = second_last_user {
        mark_last_content_block(&mut messages[index], cache_control);
    }
}

fn normalize_tool_history(messages: &mut Vec<claude::Message>) {
    let mut results = BTreeMap::new();
    for message in messages.iter() {
        if message.role == "user" {
            for block in &message.content {
                if let claude::ContentBlock::ToolResult { tool_use_id, .. } = block {
                    results.insert(tool_use_id.clone(), block.clone());
                }
            }
        }
    }
    let mut emitted = BTreeSet::new();
    let mut out = Vec::new();
    for message in messages.drain(..) {
        if message.role == "assistant" {
            let mut answered = Vec::new();
            let mut other = Vec::new();
            for block in message.content {
                match &block {
                    claude::ContentBlock::ToolUse { id, .. }
                        if results.contains_key(id) && !emitted.contains(id) =>
                    {
                        answered.push(block)
                    }
                    // Anthropic requires every tool_use in an assistant turn to
                    // have a matching tool_result in the immediately following
                    // user turn. Drop unanswered parallel siblings (and duplicate
                    // tool_use IDs) rather than forwarding an invalid transcript.
                    claude::ContentBlock::ToolUse { .. } => {}
                    _ => other.push(block),
                }
            }
            if !answered.is_empty() {
                other.extend(answered.iter().cloned());
                out.push(claude::Message {
                    role:    "assistant".to_string(),
                    content: other,
                });
                let mut result_blocks = Vec::new();
                for call in answered {
                    if let claude::ContentBlock::ToolUse { id, .. } = call
                        && let Some(result) = results.get(&id)
                    {
                        emitted.insert(id);
                        result_blocks.push(result.clone());
                    }
                }
                out.push(claude::Message {
                    role:    "user".to_string(),
                    content: result_blocks,
                });
            } else if !other.is_empty() {
                out.push(claude::Message {
                    role:    "assistant".to_string(),
                    content: other,
                });
            }
        } else {
            let content = message
                .content
                .into_iter()
                .filter(|block| match block {
                    // Matched results were re-emitted directly after their
                    // tool_use. Orphan results cannot form a valid Anthropic
                    // transcript and must not be forwarded upstream.
                    claude::ContentBlock::ToolResult { .. } => false,
                    _ => true,
                })
                .collect::<Vec<_>>();
            if !content.is_empty() {
                out.push(claude::Message {
                    role: message.role,
                    content,
                });
            }
        }
    }
    *messages = out;
}

fn merge_consecutive_messages(messages: &mut Vec<claude::Message>) {
    let mut merged: Vec<claude::Message> = Vec::new();
    for message in messages.drain(..) {
        if let Some(last) = merged.last_mut()
            && last.role == message.role
        {
            last.content.extend(message.content);
            continue;
        }
        merged.push(message);
    }
    *messages = merged;
}

fn push_message(
    messages: &mut Vec<claude::Message>,
    role: &str,
    content: Vec<claude::ContentBlock>,
) {
    if !content.is_empty() {
        messages.push(claude::Message {
            role: role.to_string(),
            content,
        });
    }
}

fn content_text(content: Option<&JsonValue>) -> Result<String, ProtocolError> {
    match content {
        Some(JsonValue::String(text)) => Ok(text.clone()),
        Some(JsonValue::Array(parts)) => {
            let mut text = Vec::new();
            for part in parts {
                match part.get("type").and_then(JsonValue::as_str) {
                    Some("input_text" | "output_text" | "text") => {
                        if let Some(value) = part.get("text").and_then(JsonValue::as_str) {
                            text.push(value);
                        }
                    }
                    Some(kind) => {
                        return Err(ProtocolError::UnsupportedRequest(format!(
                            "system content part {kind} cannot be represented by Anthropic system \
                             text"
                        )));
                    }
                    None => {
                        return Err(ProtocolError::UnsupportedRequest(
                            "system content part is missing type".to_string(),
                        ));
                    }
                }
            }
            Ok(text.join("\n\n"))
        }
        _ => Ok(String::new()),
    }
}

fn string_list(value: &JsonValue) -> Vec<String> {
    match value {
        JsonValue::String(value) => vec![value.clone()],
        JsonValue::Array(values) => values
            .iter()
            .filter_map(JsonValue::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn anthropic_call_id(id: &str) -> String {
    if id.starts_with("toolu_") || id.starts_with("call_") {
        id.to_string()
    } else if let Some(rest) = id.strip_prefix("fc_")
        && (rest.starts_with("toolu_") || rest.starts_with("call_"))
    {
        rest.to_string()
    } else {
        format!("toolu_{id}")
    }
}

fn response_id() -> String {
    format!("resp_{}", Uuid::new_v4().simple())
}

fn item_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

/// Restore a custom tool's free-text input from the lowered function-call
/// arguments. Mirrors sub2api's `extractCustomToolCallInput`:
///
/// - `{"input": "..."}` → the inner string
/// - `{"input": <non-string>}` → original arguments verbatim (client validates)
/// - `{}` (empty object) → empty string
/// - any other JSON object (no `input` key) → original arguments verbatim
/// - non-JSON / null → empty string
///
/// When the model does not follow the lowered schema, returning the raw
/// arguments lets the client surface the error and request a retry instead of
/// silently dropping the content.
fn extract_custom_tool_input_from_json(input: &JsonValue) -> String {
    let Some(obj) = input.as_object() else {
        return String::new();
    };
    if let Some(value) = obj.get("input") {
        if let Some(text) = value.as_str() {
            return text.to_string();
        }
        return input.to_string();
    }
    if obj.is_empty() {
        return String::new();
    }
    input.to_string()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_cache_controls(value: &mut JsonValue) {
        match value {
            JsonValue::Object(object) => {
                object.remove("cache_control");
                for child in object.values_mut() {
                    strip_cache_controls(child);
                }
            }
            JsonValue::Array(values) => {
                for child in values {
                    strip_cache_controls(child);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn converts_typed_history_images_reasoning_and_tools() {
        let request = json!({
            "model":"gpt-5",
            "instructions":"system prompt",
            "input":[
                {"type":"reasoning","summary":[{"type":"summary_text","text":"thought"}],"encrypted_content":"sig"},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"calling"}]},
                {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":1}"},
                {"type":"function_call_output","call_id":"call_1","output":[{"type":"input_text","text":"ok"}]},
                {"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]}
            ],
            "tools":[{"type":"function","name":"lookup","parameters":{"properties":{"q":{"type":"number"}}}}],
            "reasoning":{"effort":"xhigh"},
            "stream":true
        });
        let converted = openai_responses_to_claude_messages_with_cache_mode(
            &serde_json::to_vec(&request).unwrap(),
            "claude-sonnet",
            PromptCacheMode::Automatic,
        )
        .unwrap();
        assert_eq!(converted.model, "claude-sonnet");
        let system = converted.system.as_ref().unwrap();
        assert_eq!(system.as_text_lossy(), "system prompt");
        assert!(matches!(
            system,
            claude::SystemContent::Blocks(blocks)
                if blocks.len() == 1
                    && blocks[0].extra.get("cache_control")
                        == Some(&json!({"type":"ephemeral"}))
        ));
        assert_eq!(
            converted.cache_control,
            Some(json!({"type":"ephemeral"}))
        );
        assert_eq!(
            converted
                .tools
                .as_ref()
                .and_then(|tools| tools.last())
                .and_then(|tool| tool.cache_control.as_ref()),
            Some(&json!({"type":"ephemeral"}))
        );
        let wire = serde_json::to_value(&converted).unwrap();
        assert_eq!(wire["cache_control"], json!({"type":"ephemeral"}));
        assert_eq!(
            wire["system"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(
            wire["tools"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(
            converted.output_config.unwrap().effort.as_deref(),
            Some("max")
        );
        assert_eq!(converted.thinking.unwrap().budget_tokens, Some(32768));
        assert_eq!(converted.max_tokens, 33792);
        assert!(converted.messages.iter().any(|message| message.content.iter().any(
            |block| matches!(block, claude::ContentBlock::Thinking { signature: Some(sig), .. } if sig == "sig")
        )));
        assert!(converted.messages.iter().any(|message| message.content.iter().any(
            |block| matches!(block, claude::ContentBlock::Image { source, .. } if source.data.as_deref() == Some("AAAA"))
        )));
    }

    #[test]
    fn enables_automatic_prompt_cache_without_system_or_tools() {
        let request = json!({"input":"hello"});
        let converted = openai_responses_to_claude_messages_with_cache_mode(
            &serde_json::to_vec(&request).unwrap(),
            "claude",
            PromptCacheMode::Automatic,
        )
        .unwrap();

        assert_eq!(
            converted.cache_control,
            Some(json!({"type":"ephemeral"}))
        );
        assert!(converted.system.is_none());
        assert!(converted.tools.is_none());
    }

    #[test]
    fn content_blocks_round_trip_cache_control_and_ttl() {
        let raw = json!({
            "type":"text",
            "text":"stable context",
            "cache_control":{"type":"ephemeral","ttl":"1h"}
        });
        let block: claude::ContentBlock = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(block).unwrap(), raw);
    }

    #[test]
    fn explicit_cache_mode_preserves_requested_ttl() {
        let request = json!({
            "cache_control":{"type":"ephemeral","ttl":"1h"},
            "input":"hello"
        });
        let converted = openai_responses_to_claude_messages(
            &serde_json::to_vec(&request).unwrap(),
            "claude",
        )
        .unwrap();
        assert!(converted.cache_control.is_none());
        let wire = serde_json::to_value(converted).unwrap();
        assert_eq!(
            wire["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
    }

    #[test]
    fn keeps_mid_conversation_developer_item_out_of_system_prefix() {
        let request = json!({
            "instructions":"initial instructions",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]},
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"mid developer"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"second"}]}
            ]
        });
        let converted = openai_responses_to_claude_messages(
            &serde_json::to_vec(&request).unwrap(),
            "claude",
        )
        .unwrap();
        assert_eq!(
            converted.system.as_ref().unwrap().as_text_lossy(),
            "initial instructions"
        );
        let wire = serde_json::to_value(converted).unwrap();
        assert_eq!(wire["messages"][2]["content"][0]["text"], "mid developer");
    }

    #[test]
    fn keeps_stable_prefix_when_responses_history_is_appended() {
        let base = json!({
            "instructions":"stable instructions",
            "tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer 1"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"second"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer 2"}]}
            ]
        });
        let mut next = base.clone();
        next["input"].as_array_mut().unwrap().push(json!({
            "type":"message",
            "role":"user",
            "content":[{"type":"input_text","text":"follow-up"}]
        }));

        let first = openai_responses_to_claude_messages(
            &serde_json::to_vec(&base).unwrap(),
            "claude",
        )
        .unwrap();
        let second = openai_responses_to_claude_messages(
            &serde_json::to_vec(&next).unwrap(),
            "claude",
        )
        .unwrap();
        let first_wire = serde_json::to_value(first).unwrap();
        let second_wire = serde_json::to_value(second).unwrap();

        assert_eq!(first_wire["cache_control"], second_wire["cache_control"]);
        assert_eq!(first_wire["system"], second_wire["system"]);
        assert_eq!(first_wire["tools"], second_wire["tools"]);
        assert_eq!(
            first_wire["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert!(first_wire["cache_control"].is_null());
        assert_eq!(
            first_wire["system"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(
            first_wire["tools"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(
            second_wire["messages"][2]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        let mut first_without_markers = first_wire.clone();
        let mut second_without_markers = second_wire.clone();
        strip_cache_controls(&mut first_without_markers);
        strip_cache_controls(&mut second_without_markers);
        let first_messages = first_without_markers["messages"].as_array().unwrap();
        let second_messages = second_without_markers["messages"].as_array().unwrap();
        assert_eq!(&first_messages[..], &second_messages[..first_messages.len()]);
        let first_messages = first_wire["messages"].as_array().unwrap();
        let second_messages = second_wire["messages"].as_array().unwrap();
        assert_eq!(
            first_messages[3]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert!(second_messages[3]["content"][0]
            .get("cache_control")
            .is_none());
    }

    #[test]
    fn groups_parallel_calls_with_their_results() {
        let request = json!({
            "input":[
                {"type":"function_call","call_id":"call_1","name":"one","arguments":"{}"},
                {"type":"function_call","call_id":"call_2","name":"two","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"a"},
                {"type":"function_call_output","call_id":"call_2","output":"b"}
            ]
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert_eq!(converted.messages.len(), 2);
        assert_eq!(converted.messages[0].role, "assistant");
        assert_eq!(converted.messages[1].role, "user");
        assert_eq!(converted.messages[0].content.len(), 2);
        assert_eq!(converted.messages[1].content.len(), 2);
    }

    #[test]
    fn drops_unanswered_parallel_siblings_and_orphan_results() {
        let request = json!({
            "input":[
                {"type":"function_call","call_id":"call_1","name":"one","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"a"},
                {"type":"function_call","call_id":"call_2","name":"two","arguments":"{}"},
                {"type":"function_call_output","call_id":"orphan","output":"ignored"}
            ]
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert!(converted.messages.iter().any(|message| {
            message.content.iter().any(
                |block| matches!(block, claude::ContentBlock::ToolUse { id, .. } if id == "call_1"),
            )
        }));
        assert!(!converted.messages.iter().any(|message| {
            message.content.iter().any(
                |block| matches!(block, claude::ContentBlock::ToolUse { id, .. } if id == "call_2"),
            )
        }));
        assert!(!converted.messages.iter().any(|message| message.content.iter().any(
            |block| matches!(block, claude::ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "toolu_orphan")
        )));
    }

    #[test]
    fn drops_dangling_tool_call_when_no_results_exist() {
        let request = json!({
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"start"}]},
                {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"}
            ]
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert_eq!(converted.messages.len(), 1);
        assert_eq!(converted.messages[0].role, "user");
        assert!(!converted.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, claude::ContentBlock::ToolUse { .. }))
        }));
    }

    #[test]
    fn rejects_stateful_reference_and_maps_parallel_tool_policy() {
        let request = json!({
            "input":"hello",
            "tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],
            "parallel_tool_calls":false
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert_eq!(
            converted
                .tool_choice
                .as_ref()
                .and_then(|choice| choice.get("disable_parallel_tool_use")),
            Some(&JsonValue::Bool(true))
        );

        let request = json!({"previous_response_id":"resp_1","input":"continue"});
        let error =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap_err();
        assert!(matches!(error, ProtocolError::UnsupportedRequest(_)));
    }

    #[test]
    fn rejects_malformed_tool_arguments_and_unsupported_content() {
        let malformed = json!({
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]},
                {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"not-json"},
                {"type":"function_call_output","call_id":"call_1","output":"ok"}
            ]
        });
        let error =
            openai_responses_to_claude_messages(&serde_json::to_vec(&malformed).unwrap(), "claude")
                .unwrap_err();
        assert!(matches!(error, ProtocolError::UnsupportedRequest(_)));

        let unsupported = json!({
            "input":[{"type":"message","role":"user","content":[
                {"type":"input_text","text":"keep"},
                {"type":"input_audio","data":"AAAA","format":"wav"}
            ]}]
        });
        let error = openai_responses_to_claude_messages(
            &serde_json::to_vec(&unsupported).unwrap(),
            "claude",
        )
        .unwrap_err();
        assert!(matches!(error, ProtocolError::UnsupportedRequest(_)));
    }

    #[test]
    fn stream_keeps_requested_model_and_surfaces_claude_error() {
        let mut translator = ClaudeSseToResponsesTranslator::new("requested-model");
        let created = translator
            .translate_sse_payload(
                &json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"upstream-model","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}).to_string(),
            )
            .unwrap();
        assert_eq!(created[0]["response"]["model"], "requested-model");

        let events = translator
            .translate_sse_payload(
                &json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}})
                    .to_string(),
            )
            .unwrap();
        assert_eq!(events[0]["type"], "error");
        assert_eq!(events[0]["code"], "overloaded_error");
        let terminal = events.last().unwrap();
        assert_eq!(terminal["type"], "response.failed");
        assert_eq!(terminal["response"]["status"], "failed");
        assert_eq!(terminal["response"]["model"], "requested-model");
        assert!(translator.finish().is_empty());
    }

    #[test]
    fn explicit_translation_failure_emits_failed_terminal() {
        let mut translator = ClaudeSseToResponsesTranslator::new("requested-model");
        let events = translator.fail("invalid_upstream_event", "bad event");
        assert_eq!(events[0]["type"], "response.created");
        assert_eq!(events[1]["type"], "error");
        assert_eq!(events[1]["code"], "invalid_upstream_event");
        assert_eq!(events[2]["type"], "response.failed");
        assert_eq!(events[2]["response"]["status"], "failed");
        assert!(translator.finish().is_empty());
    }

    #[test]
    fn converts_buffered_thinking_signature_and_usage() {
        let response = claude::MessagesResponse {
            id:            "msg_1".to_string(),
            response_type: "message".to_string(),
            role:          "assistant".to_string(),
            model:         "claude".to_string(),
            content:       vec![
                claude::ContentBlock::Thinking {
                    thinking:  "why".to_string(),
                    signature: Some("cipher".to_string()),
                    cache_control: None,
                },
                claude::ContentBlock::ToolUse {
                    id:    "toolu_1".to_string(),
                    name:  "lookup".to_string(),
                    input: json!({"q":1}),
                    cache_control: None,
                },
            ],
            stop_reason:   Some("tool_use".to_string()),
            usage:         Some(claude::Usage {
                input_tokens:                10,
                output_tokens:               4,
                cache_creation_input_tokens: 2,
                cache_read_input_tokens:     3,
            }),
        };
        let converted = claude_messages_to_openai_responses(response, "requested");
        assert_eq!(converted["output"][0]["encrypted_content"], "cipher");
        assert_eq!(converted["output"][1]["call_id"], "toolu_1");
        assert_eq!(converted["usage"]["input_tokens"], 15);
    }

    #[test]
    fn omits_encrypted_reasoning_when_not_requested() {
        let response = claude::MessagesResponse {
            id:            "msg_1".to_string(),
            response_type: "message".to_string(),
            role:          "assistant".to_string(),
            model:         "claude".to_string(),
            content:       vec![claude::ContentBlock::Thinking {
                thinking:  "why".to_string(),
                signature: Some("cipher".to_string()),
                cache_control: None,
            }],
            stop_reason:   Some("end_turn".to_string()),
            usage:         None,
        };
        let converted =
            claude_messages_to_openai_responses_with_options(response, "requested", false);
        assert!(converted["output"][0].get("encrypted_content").is_none());
    }

    #[test]
    fn buffered_output_keeps_claude_block_order() {
        let response = claude::MessagesResponse {
            id:            "msg_order".to_string(),
            response_type: "message".to_string(),
            role:          "assistant".to_string(),
            model:         "claude".to_string(),
            content:       vec![
                claude::ContentBlock::Text {
                    text: "before".to_string(),
                    cache_control: None,
                },
                claude::ContentBlock::ToolUse {
                    id:    "toolu_1".to_string(),
                    name:  "lookup".to_string(),
                    input: json!({}),
                    cache_control: None,
                },
                claude::ContentBlock::Text {
                    text: "after".to_string(),
                    cache_control: None,
                },
            ],
            stop_reason:   Some("end_turn".to_string()),
            usage:         None,
        };
        let converted = claude_messages_to_openai_responses(response, "requested");
        assert_eq!(converted["output"][0]["type"], "message");
        assert_eq!(converted["output"][1]["type"], "function_call");
        assert_eq!(converted["output"][2]["type"], "message");
    }

    #[test]
    fn stream_emits_signature_on_reasoning_item_and_terminal_output() {
        let mut translator = ClaudeSseToResponsesTranslator::new("claude");
        let events = [
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[],"stop_reason":null,"usage":{"input_tokens":2,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":null}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"why"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
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
        let item_done = output
            .iter()
            .find(|event| event["type"] == "response.output_item.done")
            .unwrap();
        assert_eq!(item_done["item"]["encrypted_content"], "sig");
        let terminal = output.last().unwrap();
        assert_eq!(terminal["type"], "response.completed");
        assert_eq!(
            terminal["response"]["output"][0]["summary"][0]["text"],
            "why"
        );
    }

    #[test]
    fn namespace_declaration_flattens_to_function_tools() {
        let request = json!({
            "model":"gpt-5",
            "tools":[
                {"type":"function","name":"plain","description":"keep"},
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","description":"spawn","parameters":{"type":"object"}}
                ]}
            ],
            "input":"hi"
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        let tools = converted.tools.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "plain");
        assert_eq!(tools[1].name, "collaboration__spawn_agent");
        assert_eq!(tools[1].description.as_deref(), Some("spawn"));
    }

    #[test]
    fn namespace_qualified_history_and_choice_rewrite_to_flat_name() {
        let request = json!({
            "tools":[{"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
            ]}],
            "tool_choice":{"type":"function","name":"spawn_agent","namespace":"collaboration"},
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]},
                {"type":"function_call","call_id":"call_1","name":"spawn_agent","namespace":"collaboration","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"ok"}
            ]
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert_eq!(
            converted.tool_choice.as_ref().unwrap()["name"],
            "collaboration__spawn_agent"
        );
        assert!(
            converted
                .tool_choice
                .as_ref()
                .unwrap()
                .get("namespace")
                .is_none()
        );
        assert!(converted.messages.iter().any(|message| message.content.iter().any(
            |block| matches!(block, claude::ContentBlock::ToolUse { name, .. } if name == "collaboration__spawn_agent")
        )));
    }

    #[test]
    fn namespace_group_choice_falls_back_to_auto() {
        let request = json!({
            "tools":[{"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"spawn_agent","parameters":{"type":"object"}},
                {"type":"function","name":"send_message","parameters":{"type":"object"}}
            ]}],
            "tool_choice":{"type":"namespace","name":"collaboration"},
            "input":"hi"
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert_eq!(converted.tool_choice.as_ref().unwrap()["type"], "auto");
    }

    #[test]
    fn namespace_rejects_top_level_flat_name_collision() {
        let request = json!({
            "tools":[
                {"type":"function","name":"collaboration__spawn_agent"},
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent"}
                ]}
            ],
            "input":"hi"
        });
        let error =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap_err();
        assert!(
            matches!(error, ProtocolError::UnsupportedRequest(message) if message.contains("conflicts with a top-level tool"))
        );
    }

    #[test]
    fn namespace_rejects_cross_namespace_collision() {
        let request = json!({
            "tools":[
                {"type":"namespace","name":"a","tools":[{"type":"function","name":"b__c"}]},
                {"type":"namespace","name":"a__b","tools":[{"type":"function","name":"c"}]}
            ],
            "input":"hi"
        });
        let error =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap_err();
        assert!(
            matches!(error, ProtocolError::UnsupportedRequest(message) if message.contains("both flatten"))
        );
    }

    #[test]
    fn namespace_rejects_duplicate_equivalent_declaration() {
        let request = json!({
            "tools":[
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
                ]},
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
                ]}
            ],
            "input":"hi"
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        let tools = converted.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "collaboration__spawn_agent");
    }

    #[test]
    fn namespace_buffered_response_restores_name_and_namespace() {
        let mapping = responses_tool_mapping(
            &serde_json::to_vec(&json!({
                "tools":[{"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
                ]}],
                "input":"hi"
            }))
            .unwrap(),
        )
        .unwrap();
        let response = claude::MessagesResponse {
            id:            "msg_1".to_string(),
            response_type: "message".to_string(),
            role:          "assistant".to_string(),
            model:         "claude".to_string(),
            content:       vec![claude::ContentBlock::ToolUse {
                id:    "toolu_1".to_string(),
                name:  "collaboration__spawn_agent".to_string(),
                input: json!({}),
                cache_control: None,
            }],
            stop_reason:   Some("tool_use".to_string()),
            usage:         None,
        };
        let converted = claude_messages_to_openai_responses_with_options_and_tools(
            response,
            "requested",
            true,
            &mapping,
        );
        assert_eq!(converted["output"][0]["type"], "function_call");
        assert_eq!(converted["output"][0]["name"], "spawn_agent");
        assert_eq!(converted["output"][0]["namespace"], "collaboration");
    }

    #[test]
    fn namespace_sse_response_restores_identity_on_done_and_terminal() {
        let mapping = responses_tool_mapping(
            &serde_json::to_vec(&json!({
                "tools":[{"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
                ]}],
                "input":"hi"
            }))
            .unwrap(),
        )
        .unwrap();
        let mut translator = ClaudeSseToResponsesTranslator::with_options("claude", true, mapping);
        let events = [
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[],"stop_reason":null,"usage":{"input_tokens":2,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"collaboration__spawn_agent","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            json!({"type":"content_block_stop","index":0}),
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
        let item_done = output
            .iter()
            .find(|event| event["type"] == "response.output_item.done")
            .unwrap();
        assert_eq!(item_done["item"]["type"], "function_call");
        assert_eq!(item_done["item"]["name"], "spawn_agent");
        assert_eq!(item_done["item"]["namespace"], "collaboration");
        let terminal = output.last().unwrap();
        assert_eq!(terminal["type"], "response.completed");
        assert_eq!(terminal["response"]["output"][0]["name"], "spawn_agent");
        assert_eq!(
            terminal["response"]["output"][0]["namespace"],
            "collaboration"
        );
    }

    #[test]
    fn namespace_unknown_qualified_history_is_left_untouched() {
        let request = json!({
            "tools":[{"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
            ]}],
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]},
                {"type":"function_call","call_id":"call_1","name":"other","namespace":"unknown","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"ok"}
            ]
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert!(converted.messages.iter().any(|message| {
            message.content.iter().any(
            |block| matches!(block, claude::ContentBlock::ToolUse { name, .. } if name == "other")
        )
        }));
    }

    #[test]
    fn namespace_long_name_uses_stable_hash_suffix() {
        let long_namespace = "a".repeat(40);
        let long_name = "b".repeat(40);
        let request = json!({
            "tools":[{"type":"namespace","name":long_namespace,"tools":[
                {"type":"function","name":long_name,"parameters":{"type":"object"}}
            ]}],
            "input":"hi"
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        let tools = converted.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert!(tools[0].name.len() <= TOOL_NAME_MAX_LEN);
        assert!(tools[0].name.contains("__"));
        assert!(tools[0].name.ends_with(|c: char| c.is_ascii_hexdigit()));
    }

    #[test]
    fn rejects_unsupported_server_tools_at_request_stage() {
        for tool_type in [
            "file_search",
            "computer",
            "image_generation",
            "code_interpreter",
        ] {
            let request = json!({
                "tools":[{"type":tool_type,"name":"x"}],
                "input":"hi"
            });
            let error = openai_responses_to_claude_messages(
                &serde_json::to_vec(&request).unwrap(),
                "claude",
            )
            .unwrap_err();
            assert!(
                matches!(error, ProtocolError::UnsupportedRequest(msg) if msg.contains(tool_type)),
                "{tool_type} should be rejected"
            );
        }
    }

    #[test]
    fn web_search_declaration_maps_to_anthropic_web_search_tool() {
        let request = json!({
            "tools":[{"type":"web_search"}],
            "input":"hi"
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        let tools = converted.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_type.as_deref(), Some("web_search_20250305"));
        assert_eq!(tools[0].name, "web_search");
    }

    #[test]
    fn sse_server_tool_accumulates_input_json_delta_fragments() {
        let mut translator = ClaudeSseToResponsesTranslator::new("claude");
        let events = [
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[],"stop_reason":null,"usage":{"input_tokens":2,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvu_1","name":"web_search","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"quer"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"y\":\"rust\"}"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
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
        let item_done = output
            .iter()
            .find(|event| event["type"] == "response.output_item.done")
            .unwrap();
        assert_eq!(item_done["item"]["type"], "web_search_call");
        assert_eq!(item_done["item"]["action"]["query"], "rust");
        let terminal = output.last().unwrap();
        assert_eq!(terminal["type"], "response.completed");
        assert_eq!(terminal["response"]["output"][0]["action"]["query"], "rust");
    }

    #[test]
    fn buffered_server_tool_use_maps_to_web_search_call() {
        let response = claude::MessagesResponse {
            id:            "msg_1".to_string(),
            response_type: "message".to_string(),
            role:          "assistant".to_string(),
            model:         "claude".to_string(),
            content:       vec![claude::ContentBlock::ServerToolUse {
                id:    "srvu_1".to_string(),
                name:  "web_search".to_string(),
                input: json!({"query":"halolake"}),
                cache_control: None,
            }],
            stop_reason:   Some("end_turn".to_string()),
            usage:         None,
        };
        let converted = claude_messages_to_openai_responses(response, "requested");
        assert_eq!(converted["output"][0]["type"], "web_search_call");
        assert_eq!(converted["output"][0]["action"]["query"], "halolake");
    }

    #[test]
    fn rejects_item_reference_and_unknown_input_item_types() {
        let request = json!({
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"item_reference","id":"msg_abc"}
            ]
        });
        let error =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap_err();
        assert!(
            matches!(error, ProtocolError::UnsupportedRequest(msg) if msg.contains("item_reference"))
        );

        let request = json!({
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"unknown_future_type","content":"x"}
            ]
        });
        let error =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap_err();
        assert!(
            matches!(error, ProtocolError::UnsupportedRequest(msg) if msg.contains("unknown_future_type"))
        );
    }

    #[test]
    fn previous_response_id_rejected_without_requesting_upstream() {
        let request = json!({
            "previous_response_id":"resp_123",
            "input":"continue"
        });
        let error =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap_err();
        assert!(
            matches!(error, ProtocolError::UnsupportedRequest(msg) if msg.contains("previous_response_id"))
        );
    }

    #[test]
    fn custom_tool_input_restoration_mirrors_sub2api_fallback() {
        // Mirrors sub2api extractCustomToolCallInput test cases:
        //   {"input":"dir"}          → "dir"
        //   {"input":<non-string>}   → original arguments verbatim
        //   {}                       → ""
        //   {"other":"x"}            → original arguments verbatim
        //   non-JSON / null          → ""
        let mapping = ResponsesToolMapping {
            custom_tools: ["lookup".to_string()].into_iter().collect(),
            ..Default::default()
        };
        let mk = |input: &str| -> JsonValue {
            let response = claude::MessagesResponse {
                id:            "msg".to_string(),
                response_type: "message".to_string(),
                role:          "assistant".to_string(),
                model:         "claude".to_string(),
                content:       vec![claude::ContentBlock::ToolUse {
                    id:    "toolu_1".to_string(),
                    name:  "lookup".to_string(),
                    input: serde_json::from_str(input).unwrap_or(json!({})),
                    cache_control: None,
                }],
                stop_reason:   Some("end_turn".to_string()),
                usage:         None,
            };
            let converted = claude_messages_to_openai_responses_with_options_and_tools(
                response,
                "requested",
                true,
                &mapping,
            );
            converted["output"][0]["input"].clone()
        };
        assert_eq!(mk(r#"{"input":"dir"}"#), "dir");
        assert_eq!(mk(r#"{"input":42}"#), r#"{"input":42}"#);
        assert_eq!(mk(r#"{}"#), "");
        assert_eq!(mk(r#"{"other":"x"}"#), r#"{"other":"x"}"#);
        assert_eq!(mk(r#"null"#), "");
    }

    #[test]
    fn custom_tool_always_uses_input_string_schema() {
        // sub2api forces the {"input":string} schema for custom tools even
        // when the caller supplies its own parameters.
        let request = json!({
            "model":"gpt-5",
            "tools":[{"type":"custom","name":"lookup","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}],
            "input":"hi"
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        let tools = converted.tools.unwrap();
        assert_eq!(tools[0].name, "lookup");
        let schema = &tools[0].input_schema;
        assert_eq!(schema["properties"]["input"]["type"], "string");
        assert_eq!(schema["required"][0], "input");
    }
}
