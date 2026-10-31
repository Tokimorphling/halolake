//! Request conversion implementation. Each direct edge follows the upstream
//! converter independently; in particular Responses media never passes through
//! the less expressive Chat Completions content model.
use crate::{Context, Diagnostic, Error, Format, Severity};
use serde_json::{Map, Value, json};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

mod chat;
mod claude;
mod gemini;
mod gemini_schema;
mod responses;
mod tools;
pub(crate) mod typed;

pub fn responses_tool_state(input: &Value) -> Result<crate::ResponsesToolState, Error> {
    tools::responses_tool_state(input)
}

pub(crate) fn preserve_request_extensions(format: Format, source: &Value, target: &mut Value) {
    if format != Format::OpenAi {
        return;
    }
    if let (Some(source), Some(target)) = (
        source["messages"].as_array(),
        target["messages"].as_array_mut(),
    ) {
        for (source, target) in source.iter().zip(target) {
            copy_fields(
                source,
                target,
                &[
                    "reasoning_signature",
                    "reasoning_content_signature",
                    "reasoningSignature",
                    "encrypted_content",
                    "reasoning_block_type",
                    "reasoning_blocks",
                    "claude_reasoning_blocks",
                    "is_error",
                    "cache_control",
                    "content",
                    "tool_calls",
                ],
            );
        }
    }
    if source["tools"].is_array() {
        target["tools"] = source["tools"].clone();
    }
}

pub(crate) fn convert(
    from: Format,
    to: Format,
    input: &Value,
    ctx: &Context,
) -> Result<(Value, Vec<Diagnostic>), Error> {
    if from == to {
        return Ok((input.clone(), vec![]));
    }
    let path = crate::conversion_path(from, to, false);
    convert_via(from, &path[1..], input, ctx)
}

pub(crate) fn convert_via(
    from: Format,
    path: &[Format],
    input: &Value,
    ctx: &Context,
) -> Result<(Value, Vec<Diagnostic>), Error> {
    if path.is_empty() {
        return Ok((input.clone(), vec![]));
    }
    let mut normalized = crate::wire::normalize_request(from, input)?;
    if ctx.options.preserve_message_structure {
        preserve_request_extensions(from, input, &mut normalized);
    }
    convert_normalized_via(from, path, &normalized, ctx)
}

// Prepared callers have already validated and normalized this source. Keep
// that work outside repeated conversions while retaining the tool pipeline.
pub(super) fn convert_normalized_via(
    from: Format,
    path: &[Format],
    input: &Value,
    ctx: &Context,
) -> Result<(Value, Vec<Diagnostic>), Error> {
    if path.is_empty() {
        return Ok((input.clone(), vec![]));
    }
    let (mut value, set) = tools::extract(from, input, ctx)?;
    let mut current = from;
    let mut diagnostics = vec![];
    for &target in path {
        let (next, ds) = convert_direct(current, target, &value, ctx)?;
        value = Cow::Owned(next);
        diagnostics.extend(ds);
        current = target;
    }
    let tool_diagnostics = tools::attach(current, value.to_mut(), set, ctx)?;
    crate::core::reject_loss(ctx.options.tool_loss_policy, &tool_diagnostics)?;
    diagnostics.extend(tool_diagnostics);
    for diagnostic in &mut diagnostics {
        diagnostic.from = from;
        diagnostic.to = current;
    }
    Ok((value.into_owned(), diagnostics))
}

pub(crate) fn convert_direct(
    from: Format,
    to: Format,
    input: &Value,
    ctx: &Context,
) -> Result<(Value, Vec<Diagnostic>), Error> {
    let mut diagnostics = Vec::new();
    let mut out = convert_body_direct(from, to, input, ctx, &mut diagnostics)?;
    finish_direct(from, to, input, &mut out, ctx, &mut diagnostics)?;
    Ok((out, diagnostics))
}
fn convert_body_direct(
    from: Format,
    to: Format,
    input: &Value,
    ctx: &Context,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Value, Error> {
    Ok(match (from, to) {
        (Format::OpenAi, Format::OpenAiResponses) => chat::to_responses(input, ctx)?,
        (Format::OpenAi, Format::Claude) => chat::to_claude(input, ctx, diagnostics)?,
        (Format::OpenAi, Format::Gemini) => chat::to_gemini(input, ctx, diagnostics)?,
        (Format::Claude, Format::OpenAi) => claude::to_chat(input, ctx, diagnostics)?,
        (Format::Claude, Format::OpenAiResponses) => claude::to_responses(input, ctx)?,
        (Format::Gemini, Format::OpenAi) => gemini::to_chat(input, ctx, diagnostics)?,
        (Format::OpenAiResponses, Format::OpenAi) => responses::to_chat(input, ctx, diagnostics)?,
        (Format::OpenAiResponses, Format::Claude) => responses::to_claude(input, ctx, diagnostics)?,
        (Format::OpenAiResponses, Format::Gemini) => responses::to_gemini(input, ctx, diagnostics)?,
        _ if from == to => input.clone(),
        _ => {
            return Err(Error::message(
                "no direct request converter for this format pair",
            ));
        }
    })
}
fn finish_direct(
    from: Format,
    to: Format,
    input: &Value,
    out: &mut Value,
    ctx: &Context,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), Error> {
    crate::reasoning::convert_request(from, to, input, out, ctx, diagnostics)?;
    if to == Format::Claude && from != to && out["max_tokens"].is_null() {
        if let Some(default) = &ctx.options.claude.default_max_tokens {
            let tokens = default(s(&out["model"]));
            if tokens > 0 {
                out["max_tokens"] = json!(tokens);
            }
        }
        if out["max_tokens"].is_null() {
            return Err(Error::message(
                "claude messages request requires max_tokens: set max_tokens on the request or configure Options.Claude.DefaultMaxTokens",
            ));
        }
    }
    for diagnostic in diagnostics {
        diagnostic.from = from;
        diagnostic.to = to;
    }
    Ok(())
}

fn s(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}
fn arr(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}
pub(crate) fn stringify(value: &Value) -> String {
    crate::json::stringify(value)
}
fn string_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(v) => v.clone(),
        _ => stringify(value),
    }
}
// Responses content text is dynamically decoded and uses kitutil.Interface2String,
// whose array/map fallback is Go's display format rather than JSON encoding.
fn response_text_value(value: &Value) -> Cow<'_, str> {
    crate::response::citations::go_interface_string(value)
}
fn copy_fields(source: &Value, target: &mut Value, keys: &[&str]) {
    for key in keys {
        if !source[*key].is_null() {
            target[*key] = source[*key].clone();
        }
    }
}
fn copy_renamed(source: &Value, target: &mut Value, pairs: &[(&str, &str)]) {
    for (from, to) in pairs {
        if !source[*from].is_null() {
            target[*to] = source[*from].clone();
        }
    }
}
fn owned_object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}
fn put_nonempty(target: &mut Value, key: &str, value: &str) {
    if !value.is_empty() {
        target[key] = json!(value);
    }
}
fn diag(
    ds: &mut Vec<Diagnostic>,
    severity: Severity,
    code: &str,
    path: impl Into<String>,
    message: impl Into<String>,
) {
    ds.push(Diagnostic {
        code: code.into(),
        path: path.into(),
        message: message.into(),
        severity,
        from: Format::OpenAi,
        to: Format::OpenAi,
    });
}
fn model_required(input: &Value) -> Result<(), Error> {
    if s(&input["model"]).is_empty() {
        Err(Error::message("model is required"))
    } else {
        Ok(())
    }
}
fn text_content(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.into();
    }
    arr(value)
        .iter()
        .filter(|p| s(&p["type"]) == "text")
        .map(|p| s(&p["text"]))
        .collect()
}
fn chat_parts(value: &Value) -> Vec<Value> {
    if let Some(text) = value.as_str() {
        return vec![json!({"type":"text","text":text})];
    }
    arr(value)
        .iter()
        .filter_map(|part| {
            let p = match s(&part["type"]) {
                "text" if part["text"].is_string() => json!({"type":"text","text":part["text"]}),
                "image_url" => {
                    let url = if part["image_url"].is_string() {
                        s(&part["image_url"])
                    } else {
                        s(&part["image_url"]["url"])
                    };
                    let mut image = json!({"url":url});
                    put_nonempty(&mut image, "detail", s(&part["image_url"]["detail"]));
                    json!({"type":"image_url","image_url":image})
                }
                "input_audio"
                    if part["input_audio"]["data"].is_string()
                        && part["input_audio"]["format"].is_string() =>
                {
                    json!({"type":"input_audio","input_audio":part["input_audio"]})
                }
                "file" if part["file"]["file_id"].is_string() => {
                    json!({"type":"file","file":{"file_id":part["file"]["file_id"]}})
                }
                "file"
                    if part["file"]["filename"].is_string()
                        && part["file"]["file_data"].is_string() =>
                {
                    json!({"type":"file","file":part["file"]})
                }
                "video_url" if part["video_url"].is_string() => {
                    json!({"type":"video_url","video_url":{"url":part["video_url"]}})
                }
                _ => return None,
            };
            Some(p)
        })
        .collect()
}
fn preserved_chat_parts(value: &Value) -> Result<Vec<Value>, Error> {
    match value {
        Value::Null => Ok(vec![]),
        Value::String(text) => Ok(vec![json!({"type":"text","text":text})]),
        Value::Array(parts) if parts.iter().all(Value::is_object) => Ok(parts.clone()),
        _ => Err(Error::message(
            "Chat message content must be text or an array of content objects",
        )),
    }
}
fn split_chat_tool_content(content: Value) -> (Value, Vec<Value>) {
    let Value::Array(parts) = content else {
        return (content, Vec::new());
    };
    let mut text = Vec::new();
    let mut media = Vec::new();
    for part in parts {
        if part["type"] == "text" {
            text.push(part);
        } else {
            text.push(json!({
                "type":"text",
                "text": if part["type"] == "image_url" { "[image]" } else { "[file]" }
            }));
            media.push(part);
        }
    }
    (Value::Array(text), media)
}
fn copy_content_metadata(source: &Value, target: &mut Value) {
    copy_fields(source, target, &["cache_control", "title", "context", "citations"]);
}
fn preserved_system_text(value: &Value) -> Result<String, Error> {
    let parts = preserved_chat_parts(value)?;
    if parts.iter().any(|part| part["type"] != "text") {
        return Err(Error::message("system instructions can contain only text"));
    }
    Ok(parts.iter().map(|part| s(&part["text"])).collect())
}
fn content_parts(value: &Value) -> Vec<Value> {
    match value {
        Value::Null => vec![],
        Value::String(text) => vec![json!({"type":"input_text","text":text})],
        Value::Array(values) => values.iter().map(|p| if p.is_object() { p.clone() } else { json!({"type":"input_text","text":if p.is_string(){string_value(p)}else{stringify(p)}}) }).collect(),
        _ => vec![json!({"type":"input_text","text":stringify(value)})],
    }
}
fn input_items(input: &Value) -> Result<Cow<'_, [Value]>, Error> {
    match input {
        Value::Null => Ok(Cow::Borrowed(&[])),
        Value::String(text) => Ok(Cow::Owned(vec![json!({"role":"user","content":text})])),
        Value::Array(items) if items.iter().all(Value::is_object) => Ok(Cow::Borrowed(items)),
        Value::Array(_) => Err(Error::message("invalid input array: expected object items")),
        _ => Err(Error::message(format!(
            "unsupported responses input type {:?}",
            if input.is_object() {
                "object"
            } else if input.is_boolean() {
                "boolean"
            } else {
                "number"
            }
        ))),
    }
}
// JSON embedded in a string is decoded into Go's interface{} numbers, not a
// RawMessage. In particular float64 overflow makes ObjectValue/ResponseMap
// retain the original string instead of accepting an arbitrary-precision tree.
fn parse_interface_json(text: &str) -> Result<Value, Error> {
    fn finite_numbers(value: &Value) -> bool {
        match value {
            Value::Number(number) => number.as_f64().is_some_and(f64::is_finite),
            Value::Array(values) => values.iter().all(finite_numbers),
            Value::Object(values) => values.values().all(finite_numbers),
            _ => true,
        }
    }
    let value: Value = serde_json::from_str(text)?;
    if !finite_numbers(&value) {
        return Err(Error::message("JSON number is outside Go float64 range"));
    }
    Ok(crate::wire::go_interface_value(&value))
}
fn object_value(value: &Value, fallback: &str) -> Value {
    match value {
        Value::Null => json!({}),
        Value::Object(_) => value.clone(),
        Value::String(text) => match parse_interface_json(text) {
            Ok(Value::Null) => Value::Null,
            Ok(v @ Value::Object(_)) => v,
            Ok(v @ Value::Array(_)) => json!({fallback:v}),
            _ => json!({fallback:text}),
        },
        _ => json!({fallback:value}),
    }
}
fn response_map(value: &Value) -> Value {
    match value {
        Value::Null => json!({}),
        Value::Object(_) => value.clone(),
        Value::Array(_) => json!({"result":value}),
        Value::String(text) => match parse_interface_json(text) {
            Ok(Value::Null) => Value::Null,
            Ok(v @ Value::Object(_)) => v,
            Ok(v @ Value::Array(_)) => json!({"result":v}),
            _ => json!({"content":text}),
        },
        _ => json!({"content":value}),
    }
}
fn call_id(item: &Value) -> &str {
    let id = s(&item["call_id"]).trim();
    if id.is_empty() {
        s(&item["id"]).trim()
    } else {
        id
    }
}
fn call_name(item: &Value) -> String {
    let ns = s(&item["namespace"]).trim();
    let name = s(&item["name"]).trim();
    if ns.is_empty() || ns == "functions" || name.is_empty() {
        name.into()
    } else {
        format!("{ns}__{name}")
    }
}
fn resolved_call_name(item: &Value, ctx: &Context) -> String {
    if !ctx.options.preserve_message_structure {
        return call_name(item);
    }
    if s(&item["type"]) == "tool_search_call" {
        return "tool_search".into();
    }
    let state = ctx.responses_tool_state();
    let namespace = s(&item["namespace"]).trim();
    let name = s(&item["name"]).trim();
    if namespace.is_empty() || namespace == "functions" {
        return name.to_owned();
    }
    state
        .flatten_declared(namespace, name)
        .map(str::to_owned)
        .unwrap_or_else(|| name.to_owned())
}

fn reasoning_blocks(message: &Value) -> Vec<Value> {
    for key in ["claude_reasoning_blocks", "reasoning_blocks"] {
        if let Some(blocks) = message[key].as_array() {
            return blocks.clone();
        }
    }
    let text = s(&message["reasoning_content"]);
    let text = if text.is_empty() { s(&message["reasoning"]) } else { text };
    let signature = [
        "reasoning_signature",
        "reasoning_content_signature",
        "reasoningSignature",
        "encrypted_content",
    ]
    .into_iter()
    .map(|key| s(&message[key]))
    .find(|value| !value.is_empty())
    .unwrap_or("");
    if text.is_empty() && signature.is_empty() {
        return vec![];
    }
    if message["reasoning_block_type"] == "redacted_thinking" {
        return vec![json!({"type":"redacted_thinking","data":signature})];
    }
    let mut block = json!({"type":"thinking","thinking":text});
    put_nonempty(&mut block, "signature", signature);
    vec![block]
}

fn reasoning_item(block: &Value) -> Value {
    let redacted = block["type"] == "redacted_thinking";
    let mut item = json!({
        "type": "reasoning",
        "summary": if redacted || s(&block["thinking"]).is_empty() {
            vec![]
        } else {
            vec![json!({"type":"summary_text","text":block["thinking"]})]
        },
    });
    put_nonempty(
        &mut item,
        "encrypted_content",
        s(&block[if redacted { "data" } else { "signature" }]),
    );
    if redacted {
        item["claude_block_type"] = json!("redacted_thinking");
    }
    item
}

fn reasoning_from_item(item: &Value) -> Value {
    let text = arr(&item["summary"])
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect::<String>();
    let signature = s(&item["encrypted_content"]);
    if item["claude_block_type"] == "redacted_thinking" {
        return json!({"type":"redacted_thinking","data":signature});
    }
    let mut block = json!({"type":"thinking","thinking":text});
    put_nonempty(&mut block, "signature", signature);
    block
}

fn add_reasoning_message(message: &mut Value, block: &Value) {
    if !message["reasoning_blocks"].is_array() {
        message["reasoning_blocks"] = json!([]);
    }
    message["reasoning_blocks"].as_array_mut().unwrap().push(block.clone());
    if block["type"] == "redacted_thinking" {
        message["reasoning_block_type"] = json!("redacted_thinking");
        put_nonempty(message, "reasoning_signature", s(&block["data"]));
    } else {
        let previous = s(&message["reasoning_content"]);
        let text = format!("{previous}{}", s(&block["thinking"]));
        put_nonempty(message, "reasoning_content", &text);
        put_nonempty(message, "reasoning_signature", s(&block["signature"]));
    }
}

fn checked_tool_arguments(value: &Value) -> Result<Value, Error> {
    let result = if let Some(value) = value.as_str() {
        serde_json::from_str(value)
            .map_err(|error| Error::message(format!("invalid tool arguments: {error}")))?
    } else if value.is_null() {
        json!({})
    } else {
        value.clone()
    };
    if result.is_object() {
        Ok(result)
    } else {
        Err(Error::message("tool arguments must be a JSON object"))
    }
}

fn is_http_url(value: &str) -> bool {
    value.starts_with("https://") || value.starts_with("http://")
}

fn claude_url_part(kind: &str, url: &str) -> Value {
    json!({
        "type": if matches!(kind, "input_image" | "image_url") { "image" } else { "document" },
        "source": {"type":"url", "url":url},
    })
}

fn claude_message_parts(message: &Value) -> Vec<Value> {
    match &message["content"] {
        Value::String(text) => vec![json!({"type":"text","text":text})],
        Value::Array(parts) => parts.clone(),
        _ => vec![],
    }
}

fn normalize_claude_tool_turns(messages: &mut Vec<Value>) {
    let mut replies = BTreeMap::new();
    for message in messages.iter() {
        for part in claude_message_parts(message) {
            if part["type"] == "tool_result" && !s(&part["tool_use_id"]).is_empty() {
                replies.entry(s(&part["tool_use_id"]).to_owned()).or_insert(part);
            }
        }
    }
    let mut ordinary = Vec::<Value>::new();
    for mut message in std::mem::take(messages) {
        let parts = claude_message_parts(&message)
            .into_iter()
            .filter(|part| part["type"] != "tool_result")
            .collect::<Vec<_>>();
        if !parts.is_empty() {
            message["content"] = Value::Array(parts);
            ordinary.push(message);
        }
    }
    let mut emitted = BTreeSet::new();
    for mut message in ordinary {
        if message["role"] != "assistant" {
            for part in claude_message_parts(&message) {
                chat::append_claude(messages, s(&message["role"]), part);
            }
            continue;
        }
        let mut kept = Vec::new();
        let parts = claude_message_parts(&message)
            .into_iter()
            .filter(|part| {
                if part["type"] != "tool_use" {
                    return true;
                }
                let id = s(&part["id"]);
                if !id.is_empty() && replies.contains_key(id) && emitted.insert(id.to_owned()) {
                    kept.push(id.to_owned());
                    true
                } else {
                    false
                }
            })
            .collect::<Vec<_>>();
        if parts.is_empty() {
            continue;
        }
        message["content"] = Value::Array(parts);
        messages.push(message);
        if !kept.is_empty() {
            let reply_parts = kept
                .into_iter()
                .map(|id| replies[&id].clone())
                .collect::<Vec<_>>();
            if let Some(last) = messages.last_mut()
                && last["role"] == "user"
            {
                let mut parts = arr(&last["content"]).to_vec();
                parts.extend(reply_parts);
                last["content"] = Value::Array(parts);
            } else {
                messages.push(owned_object([
                    ("role", Value::String("user".into())),
                    ("content", Value::Array(reply_parts)),
                ]));
            }
        }
    }
}

fn normalize_chat_tool_turns(messages: &mut Vec<Value>) {
    let replies = messages
        .iter()
        .filter(|message| message["role"] == "tool" && !s(&message["tool_call_id"]).is_empty())
        .map(|message| (s(&message["tool_call_id"]).to_owned(), message.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut ordinary = Vec::<Value>::new();
    let mut adjacent_assistant = false;
    for message in std::mem::take(messages) {
        if message["role"] == "tool" {
            // Removing replies must not join reasoning from different turns.
            adjacent_assistant = false;
            continue;
        }
        if message["role"] == "assistant"
            && adjacent_assistant
            && ordinary.last().is_some_and(|last| last["role"] == "assistant")
        {
            let last = ordinary.last_mut().unwrap();
            let calls = arr(&last["tool_calls"])
                .iter()
                .chain(arr(&message["tool_calls"]))
                .cloned()
                .collect::<Vec<_>>();
            if !calls.is_empty() {
                last["tool_calls"] = Value::Array(calls);
            }
            if !message["content"].is_null() && message["content"] != "" {
                last["content"] = match (&last["content"], &message["content"]) {
                    (Value::String(left), Value::String(right)) => json!(format!("{left}{right}")),
                    (Value::Null, _) => message["content"].clone(),
                    _ => Value::Array(
                        chat_parts(&last["content"])
                            .into_iter()
                            .chain(chat_parts(&message["content"]))
                            .collect(),
                    ),
                };
            }
            for block in arr(&message["reasoning_blocks"]) {
                add_reasoning_message(last, block);
            }
        } else {
            adjacent_assistant = message["role"] == "assistant";
            ordinary.push(message);
        }
    }
    let mut emitted = BTreeSet::new();
    for mut message in ordinary {
        let calls = arr(&message["tool_calls"])
            .iter()
            .filter(|call| {
                let id = s(&call["id"]);
                !id.is_empty() && replies.contains_key(id) && emitted.insert(id.to_owned())
            })
            .cloned()
            .collect::<Vec<_>>();
        if message["role"] == "assistant" {
            if calls.is_empty() {
                message.as_object_mut().unwrap().remove("tool_calls");
                if chat_parts(&message["content"]).is_empty()
                    && reasoning_blocks(&message).is_empty()
                {
                    continue;
                }
            } else {
                message["tool_calls"] = Value::Array(calls.clone());
            }
        }
        messages.push(message);
        for call in calls {
            messages.push(replies[s(&call["id"])].clone());
        }
    }
}
fn function_schema(value: &Value) -> Value {
    let mut schema = value.as_object().cloned().unwrap_or_default();
    schema.entry("type").or_insert(json!("object"));
    schema.entry("properties").or_insert(json!({}));
    Value::Object(schema)
}
fn source(part: &Value, responses: bool) -> Option<(String, String)> {
    let mut mime = if responses {
        s(&part["mime_type"]).trim().to_string()
    } else {
        String::new()
    };
    let keys: &[&str] = match s(&part["type"]) {
        "input_image" => &["image_url", "url"],
        "input_file" => &["file", "file_data", "file_url", "url"],
        "input_audio" => &["input_audio", "data", "url"],
        "input_video" => &["video_url", "url"],
        "image_url" => &["image_url"],
        "file" => &["file"],
        "video_url" => &["video_url"],
        _ => return None,
    };
    for key in keys {
        let value = &part[*key];
        let data = if let Some(text) = value.as_str() {
            text.to_string()
        } else {
            if mime.is_empty() {
                mime = s(&value["mime_type"]).trim().into();
            }
            let nested: &[&str] = if !responses && s(&part["type"]) == "file" {
                &["file_data"]
            } else {
                &["url", "file_data", "file_url", "data"]
            };
            nested
                .iter()
                .map(|k| s(&value[*k]).trim())
                .find(|s| !s.is_empty())
                .unwrap_or("")
                .into()
        };
        if !data.is_empty() {
            if mime.is_empty()
                && s(&part["type"]) == "input_audio"
                && !s(&part["input_audio"]["format"]).is_empty()
            {
                mime = format!("audio/{}", s(&part["input_audio"]["format"]));
            }
            return Some((data, mime));
        }
    }
    None
}
fn is_text_mime(mime: &str) -> bool {
    let m = mime.split(';').next().unwrap_or("").trim().to_lowercase();
    m.starts_with("text/") || m == "application/json"
}
fn image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg" | "image/png" | "image/webp" | "image/gif"
    )
}
fn normalized_mime(mime: &str) -> String {
    let m = mime.split(';').next().unwrap_or("").trim().to_lowercase();
    if m == "image/jpg" {
        "image/jpeg".into()
    } else {
        m
    }
}
fn claude_media(data: &str, mime: &str, declared_image: bool) -> Result<Value, String> {
    let m = normalized_mime(mime);
    if m == "application/pdf" {
        return Ok(
            json!({"type":"document","source":{"type":"base64","media_type":m,"data":data}}),
        );
    }
    if is_text_mime(&m) {
        let text = crate::media::decode_text(data).ok_or_else(|| {
            format!("text file of MIME type {mime:?} could not be decoded as UTF-8 text")
        })?;
        return Ok(
            json!({"type":"document","source":{"type":"text","media_type":"text/plain","data":text}}),
        );
    }
    if image_mime(&m) || (m.is_empty() && declared_image) {
        let mut src = json!({"type":"base64"});
        put_nonempty(&mut src, "media_type", &m);
        put_nonempty(&mut src, "data", data);
        return Ok(json!({"type":"image","source":src}));
    }
    Err(if m == "application/octet-stream" && declared_image {
        "the image format could not be identified".into()
    } else if m.starts_with("image/") {
        format!("Claude accepts only JPEG, PNG, GIF, and WebP images, not {mime:?}")
    } else {
        format!("Claude Messages cannot carry content of MIME type {mime:?}")
    })
}
fn gemini_mime(mime: &str) -> bool {
    let m = mime.to_lowercase();
    m.starts_with("audio/")
        || m.starts_with("video/")
        || matches!(
            m.as_str(),
            "image/png"
                | "image/jpeg"
                | "image/jpg"
                | "image/webp"
                | "image/heic"
                | "image/heif"
                | "image/gif"
                | "image/avif"
                | "text/plain"
                | "text/html"
                | "text/css"
                | "text/javascript"
                | "text/x-typescript"
                | "text/csv"
                | "text/markdown"
                | "text/x-python"
                | "text/xml"
                | "text/rtf"
                | "application/x-javascript"
                | "application/x-typescript"
                | "application/x-python-code"
                | "application/json"
                | "application/x-ipynb+json"
                | "application/rtf"
                | "application/pdf"
        )
}
fn gemini_media(part: &Value, ctx: &Context, responses: bool) -> Result<Option<Value>, Error> {
    let Some((data, mime)) = source(part, responses || ctx.options.preserve_message_structure) else {
        if ctx.options.preserve_message_structure {
            return Err(Error::message(
                "Gemini media content requires inline data or a resolvable URL",
            ));
        }
        return Ok(None);
    };
    if ctx.options.preserve_message_structure && is_http_url(&data) {
        let mime = if mime.is_empty() {
            if matches!(s(&part["type"]), "input_image" | "image_url") {
                "image/png"
            } else {
                "application/pdf"
            }
        } else {
            &mime
        };
        return Ok(Some(json!({"fileData":{"mimeType":mime,"fileUri":data}})));
    }
    let (base64, mime) = ctx
        .resolve_media(&data, &mime)
        .map_err(|e| Error::message(format!("get file data from '{data}' failed: {e}")))?;
    if !gemini_mime(&mime) {
        return Err(Error::message(format!(
            "mime type is not supported by Gemini: '{mime}', url: '{data}'"
        )));
    }
    Ok(Some(json!({"inlineData":{"mimeType":mime,"data":base64}})))
}
const SIGNATURE: &str = "context_engineering_is_the_way_to_go";
fn attach_signature(part: &mut Value, ctx: &Context) {
    if ctx.options.gemini.function_call_thought_signature_enabled
        && part["thoughtSignature"].is_null()
    {
        part["thoughtSignature"] = json!(SIGNATURE);
    }
}
fn add_gemini_options(out: &mut Value, input: &Value, ctx: &Context) {
    let model = if ctx.upstream_model().is_empty() {
        s(&input["model"])
    } else {
        ctx.upstream_model()
    };
    if ctx
        .options
        .gemini
        .supports_imagine
        .as_ref()
        .is_some_and(|f| f(model))
    {
        out["generationConfig"]["responseModalities"] = json!(["TEXT", "IMAGE"]);
    }
    if let Some(setting) = &ctx.options.gemini.safety_setting {
        let settings: Vec<_> = [
            "HARM_CATEGORY_HARASSMENT",
            "HARM_CATEGORY_HATE_SPEECH",
            "HARM_CATEGORY_SEXUALLY_EXPLICIT",
            "HARM_CATEGORY_DANGEROUS_CONTENT",
        ]
        .into_iter()
        .filter_map(|category| {
            let threshold = setting(category);
            if threshold.is_empty() {
                None
            } else {
                Some(json!({"category":category,"threshold":threshold}))
            }
        })
        .collect();
        if !settings.is_empty() {
            out["safetySettings"] = json!(settings);
        }
    }
}
fn gemini_nested_tool_media(input: &Value, ctx: &Context) -> bool {
    let model = if ctx.upstream_model().is_empty() {
        s(&input["model"])
    } else {
        ctx.upstream_model()
    }
    .to_lowercase();
    let model = model.trim_start_matches("models/");
    !model.starts_with("gemini-1.") && !model.starts_with("gemini-2.")
}
fn omit_trailing(messages: &mut Vec<Value>, gemini: bool, ds: &mut Vec<Diagnostic>) {
    let mut end = messages.len();
    while end > 0 {
        let msg = &messages[end - 1];
        let role = if gemini { "model" } else { "assistant" };
        let has_tool = arr(&msg[if gemini { "parts" } else { "content" }])
            .iter()
            .any(|p| {
                if gemini {
                    !p["functionCall"].is_null()
                } else {
                    s(&p["type"]) == "tool_use"
                }
            });
        if s(&msg["role"]) != role || has_tool {
            break;
        }
        end -= 1;
    }
    if end == 0 || end == messages.len() || s(&messages[end - 1]["role"]) != "user" {
        return;
    }
    messages.truncate(end);
    diag(
        ds,
        Severity::Warning,
        "trailing_assistant_omitted",
        "input",
        "assistant output after the last user message or tool result was omitted; the target protocol would continue it as a prefill",
    );
}
