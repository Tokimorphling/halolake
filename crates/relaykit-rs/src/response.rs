//! Protocol response conversion. Provider usage snapshots are preserved independently
//! of the presentation format, including on routes through Chat Completions.
use crate::{Context, Diagnostic, Error, Format};
use serde_json::{Value, json};
fn owned_object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}
pub(crate) mod citations;
pub(crate) mod hosted;
pub(crate) mod typed;
pub(crate) mod typed_claude;
pub(crate) mod typed_usage;
mod usage;
pub(crate) use usage::{
    claude_usage, gemini_usage, normalize_usage, usage_from_chat, usage_from_claude,
    usage_from_gemini, usage_from_responses,
};

pub(crate) fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
pub(crate) fn n(v: &Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_f64().map(|n| n as i64))
        .unwrap_or(0)
}
pub(crate) fn arr(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
pub(crate) fn stringify(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        s.to_owned()
    } else if v.is_null() {
        String::new()
    } else {
        v.to_string()
    }
}
pub(crate) fn go_json(v: &Value) -> String {
    crate::json::stringify(v)
}
pub(crate) fn custom_tool_input_from_arguments(raw: &str, preserve: bool) -> String {
    if preserve && matches!(raw.trim(), "" | "{}" | "null") {
        return String::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    if let Some(input) = value.get("input") {
        return input.as_str().map(str::to_string).unwrap_or_else(|| {
            if preserve { go_json(&value) } else { go_json(input) }
        });
    }
    if preserve {
        return value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string());
    }
    raw.to_string()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub(crate) use crate::new_id;
pub(crate) fn text_content(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter(|x| s(&x["type"]) == "text")
            .map(|x| s(&x["text"]))
            .collect(),
        _ => String::new(),
    }
}
pub(crate) fn reasoning(v: &Value) -> &str {
    v.get("reasoning_content")
        .filter(|x| !x.is_null())
        .map(s)
        .unwrap_or_else(|| s(&v["reasoning"]))
}
pub(crate) fn claude_stop(reason: &str) -> String {
    match reason.to_lowercase().as_str() {
        "stop" => "end_turn",
        "stop_sequence" => "stop_sequence",
        "length" | "max_tokens" => "max_tokens",
        "content_filter" => "refusal",
        "tool_calls" => "tool_use",
        _ => reason,
    }
    .to_owned()
}
pub(crate) fn chat_stop(reason: &str) -> String {
    match reason.to_lowercase().as_str() {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" | "pause_turn" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => reason,
    }
    .to_owned()
}
pub(crate) fn canonical_usage(format: Format, value: &Value, fallback: i64) -> Option<Value> {
    match format {
        Format::OpenAi => {
            if s(&value["object"]) == "chat.completion.chunk" && value["usage"].is_null() {
                None
            } else {
                Some(usage_from_chat(&value["usage"]))
            }
        }
        Format::OpenAiResponses => {
            if s(&value["type"]).starts_with("response.") {
                (!value["response"].is_null())
                    .then(|| usage_from_responses(&value["response"]["usage"]))
            } else {
                Some(usage_from_responses(&value["usage"]))
            }
        }
        Format::Claude => {
            if !value["usage"].is_null() {
                Some(usage_from_claude(&value["usage"]))
            } else if !value["message"]["usage"].is_null() {
                Some(usage_from_claude(&value["message"]["usage"]))
            } else {
                None
            }
        }
        Format::Gemini => usage_from_gemini(&value["usageMetadata"], fallback),
    }
}
pub(crate) fn conversion_usage(
    from: Format,
    to: Format,
    input: &Value,
    ctx: &Context,
) -> Option<Value> {
    let u = canonical_usage(
        from,
        input,
        if from == to {
            0
        } else {
            ctx.estimate_prompt_tokens
        },
    );
    if from == to
        || to == Format::OpenAi
        || from == Format::OpenAi
        || (from == Format::OpenAiResponses && to == Format::Claude)
    {
        u
    } else {
        Some(usage_from_chat(&u.unwrap_or(Value::Null)))
    }
}
pub(crate) fn convert(
    from: Format,
    to: Format,
    input: &Value,
    ctx: &Context,
) -> Result<(Value, Vec<Diagnostic>, Option<Value>), Error> {
    if ctx.options.preserve_message_structure
        && let Some((code, message, _)) = crate::stream::protocol_error(input)
    {
        return Err(Error::message(format!("{code}: {message}")));
    }
    if from == to {
        return Ok((
            input.clone(),
            vec![],
            conversion_usage(from, to, input, ctx),
        ));
    }
    if ctx.options.preserve_message_structure
        && from == Format::Claude
        && to == Format::OpenAiResponses
    {
        let output = crate::stream::claude_responses::buffered(ctx, input)?;
        return Ok((output, Vec::new(), conversion_usage(from, to, input, ctx)));
    }
    let mut normalized = if ctx.options.preserve_message_structure {
        input.clone()
    } else {
        crate::wire::normalize_response(from, input)?
    };
    // Gemini keeps an internal presence bit even though its serializer exposes
    // a zero-valued usageMetadata object. An absent/null input is not a report.
    if from == Format::Gemini && input["usageMetadata"].is_null() {
        normalized.as_object_mut().unwrap().remove("usageMetadata");
    }
    // Claude's prepared DTO has already applied field-name folding and the
    // numeric rules for native billing snapshots. Use that same view for the
    // returned accounting, including a case-folded Usage or Message.Usage.
    let usage = conversion_usage(
        from,
        to,
        if from == Format::Claude {
            &normalized
        } else {
            input
        },
        ctx,
    );
    let input = &normalized;
    let mut diagnostics = hosted::inspect(from, to, input);
    let (stripped, set) = hosted::extract(from, input);
    let mut output = if from != Format::OpenAi
        && to != Format::OpenAi
        && !(from == Format::OpenAiResponses && to == Format::Claude)
    {
        let (pivot, _) = convert_direct(from, Format::OpenAi, &stripped, ctx)?;
        convert_direct(Format::OpenAi, to, &pivot, ctx)?.0
    } else {
        convert_direct(from, to, &stripped, ctx)?.0
    };
    diagnostics.extend(hosted::attach(from, to, &mut output, set)?);
    Ok((output, diagnostics, usage))
}
pub(crate) fn convert_direct(
    from: Format,
    to: Format,
    input: &Value,
    ctx: &Context,
) -> Result<(Value, Vec<Diagnostic>), Error> {
    let value = match (from, to) {
        (Format::OpenAi, Format::Claude) => chat_to_claude(input, ctx),
        (Format::OpenAi, Format::Gemini) => chat_to_gemini(input),
        (Format::OpenAi, Format::OpenAiResponses) => chat_to_responses(input, ctx)?,
        (Format::OpenAiResponses, Format::OpenAi) => responses_to_chat(input, ctx)?,
        (Format::OpenAiResponses, Format::Claude) => responses_to_claude(input),
        (Format::Claude, Format::OpenAi) => claude_to_chat(input, ctx),
        (Format::Gemini, Format::OpenAi) => gemini_to_chat(input, ctx),
        _ if from == to => input.clone(),
        _ => {
            return Err(Error::message(
                "response route requires an OpenAI chat pivot",
            ));
        }
    };
    Ok((value, vec![]))
}
fn chat_to_claude(v: &Value, ctx: &Context) -> Value {
    let mut blocks = vec![];
    let mut stop = String::new();
    for choice in arr(&v["choices"]) {
        stop = claude_stop(s(&choice["finish_reason"]));
        let m = &choice["message"];
        let mut text = text_content(&m["content"]);
        if ctx.options.preserve_message_structure && text.is_empty() {
            text = s(&m["refusal"]).to_string();
        }
        let r = reasoning(m);
        let calls = arr(&m["tool_calls"]);
        let preserved = arr(&m["claude_reasoning_blocks"]);
        let signature = m.get("reasoning_signature")
            .or_else(|| m.get("reasoning_content_signature"))
            .map(s).unwrap_or("");
        if ctx.options.preserve_message_structure && !preserved.is_empty() {
            blocks.extend(preserved.iter().cloned());
        } else if ctx.options.preserve_message_structure
            && s(&m["reasoning_block_type"]) == "redacted_thinking"
        {
            blocks.push(json!({"type":"redacted_thinking","data":signature}));
        } else if ctx.options.preserve_message_structure && (!r.is_empty() || !signature.is_empty()) {
            let mut block = json!({"type":"thinking","thinking":r});
            if !signature.is_empty() {
                block["signature"] = json!(signature);
            }
            blocks.push(block);
        } else if !r.is_empty() {
            blocks.push(json!({"type":"thinking","thinking":r}));
        }
        if !text.is_empty() || (r.is_empty() && calls.is_empty()) {
            let mut b = json!({"type":"text","text":text});
            let cs = citations::to_claude(&m["annotations"], &text);
            if !cs.is_empty() {
                b["citations"] = json!(cs);
            }
            blocks.push(b);
        }
        for call in calls {
            let mut input = serde_json::from_str::<Value>(s(&call["function"]["arguments"]))
                .ok()
                .filter(Value::is_object)
                .unwrap_or(json!({}));
            input.as_object_mut().unwrap().sort_keys();
            blocks.push(json!({"type":"tool_use","id":s(&call["id"]),"name":s(&call["function"]["name"]),"input":input}));
        }
    }
    json!({"id":s(&v["id"]),"type":"message","role":"assistant","model":s(&v["model"]),"content":blocks,"stop_reason":stop,"usage":claude_usage(&v["usage"])})
}
pub(crate) fn gemini_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() || raw.trim() == "null" {
        return json!({});
    }
    serde_json::from_str::<Value>(raw)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"arguments":raw}))
}
fn chat_to_gemini(v: &Value) -> Value {
    let mut candidates = vec![];
    for c in arr(&v["choices"]) {
        let m = &c["message"];
        let mut parts = vec![];
        let text = text_content(&m["content"]);
        if !text.is_empty() {
            parts.push(owned_object([("text", Value::String(text))]));
        }
        for call in arr(&m["tool_calls"]) {
            let mut f = owned_object([
                ("name", json!(s(&call["function"]["name"]))),
                ("args", gemini_arguments(s(&call["function"]["arguments"]))),
            ]);
            if !s(&call["id"]).is_empty() {
                f["id"] = call["id"].clone();
            }
            parts.push(owned_object([("functionCall", f)]));
        }
        let finish = match s(&c["finish_reason"]) {
            "length" => "MAX_TOKENS",
            "content_filter" => "SAFETY",
            _ => "STOP",
        };
        candidates.push(owned_object([
            ("index", json!(n(&c["index"]))),
            (
                "content",
                owned_object([("role", json!("model")), ("parts", Value::Array(parts))]),
            ),
            ("finishReason", json!(finish)),
            ("safetyRatings", Value::Array(vec![])),
        ]));
    }
    owned_object([
        ("candidates", Value::Array(candidates)),
        ("usageMetadata", gemini_usage(&v["usage"])),
    ])
}
pub(crate) fn response_output(typ: &str, id: &str, status: &str) -> Value {
    json!({"type":typ,"id":id,"status":status,"role":"","content":null,"quality":"","size":""})
}
pub(crate) fn response_base(
    id: &str,
    model: &str,
    created: i64,
    status: &str,
    output: Vec<Value>,
    usage: Value,
) -> Value {
    owned_object([
        ("id", json!(id)),
        ("object", json!("response")),
        ("created_at", json!(created)),
        ("status", json!(status)),
        ("instructions", Value::Null),
        ("max_output_tokens", json!(0)),
        ("model", json!(model)),
        ("output", Value::Array(output)),
        ("parallel_tool_calls", json!(false)),
        ("previous_response_id", Value::Null),
        ("reasoning", Value::Null),
        ("store", json!(false)),
        ("temperature", json!(0)),
        ("tool_choice", Value::Null),
        ("tools", Value::Null),
        ("top_p", json!(0)),
        ("truncation", Value::Null),
        ("usage", usage),
        ("user", Value::Null),
        ("metadata", Value::Null),
    ])
}
fn chat_to_responses(v: &Value, ctx: &Context) -> Result<Value, Error> {
    let mut id = if s(&v["id"]).trim().is_empty() {
        new_id("resp_")
    } else {
        s(&v["id"]).trim().to_owned()
    };
    if ctx.options.preserve_message_structure {
        if let Some(suffix) = id.strip_prefix("chatcmpl-").or_else(|| id.strip_prefix("chatcmpl_")) {
            id = format!("resp_{suffix}");
        }
    }
    let created = if v["created"].is_number() {
        n(&v["created"])
    } else {
        s(&v["created"])
            .parse::<i64>()
            .ok()
            .filter(|n| *n != 0)
            .unwrap_or_else(now)
    };
    let choice = arr(&v["choices"]).first().unwrap_or(&Value::Null);
    let m = &choice["message"];
    let reason = match s(&choice["finish_reason"]).trim() {
        "length" => "max_output_tokens",
        "content_filter" => "content_filter",
        _ => "",
    };
    let status = if reason.is_empty() {
        "completed"
    } else {
        "incomplete"
    };
    let mut outputs = vec![];
    let r = reasoning(m);
    let signature = m.get("reasoning_signature")
        .or_else(|| m.get("reasoning_content_signature"))
        .map(s).unwrap_or("");
    let preserved = arr(&m["claude_reasoning_blocks"]);
    if ctx.options.preserve_message_structure && !preserved.is_empty() {
        for (index, block) in preserved.iter().enumerate() {
            let mut item = response_output("reasoning", &format!("{id}_reasoning_{index}"), status);
            let text = s(&block["thinking"]);
            item["summary"] = if text.is_empty() { json!([]) }
                else { json!([{"type":"summary_text","text":text}]) };
            let redacted = s(&block["type"]) == "redacted_thinking";
            let signature = s(&block[if redacted { "data" } else { "signature" }]);
            if !signature.is_empty() {
                item["encrypted_content"] = json!(signature);
            }
            if redacted {
                item["claude_block_type"] = json!("redacted_thinking");
            }
            outputs.push(item);
        }
    } else if !r.is_empty() || ctx.options.preserve_message_structure && !signature.is_empty() {
        let mut o = response_output("reasoning", &format!("{id}_reasoning_0"), status);
        o["summary"] = if r.is_empty() { json!([]) }
            else { json!([{"type":"summary_text","text":r}]) };
        if ctx.options.preserve_message_structure && !signature.is_empty() {
            o["encrypted_content"] = json!(signature);
            if s(&m["reasoning_block_type"]) == "redacted_thinking" {
                o["claude_block_type"] = json!("redacted_thinking");
            }
        }
        outputs.push(o);
    }
    let text = text_content(&m["content"]);
    if !text.is_empty() {
        let mut o = response_output("message", &format!("{id}_msg_0"), status);
        o["role"] = json!("assistant");
        o["content"] = Value::Array(vec![owned_object([
            ("type", json!("output_text")),
            ("text", Value::String(text)),
            (
                "annotations",
                Value::Array(citations::to_responses(&m["annotations"])?),
            ),
        ])]);
        outputs.push(o);
    }
    if ctx.options.preserve_message_structure && !s(&m["refusal"]).is_empty() {
        let part = json!({"type":"refusal","refusal":s(&m["refusal"])});
        if let Some(message) = outputs.iter_mut().find(|o| s(&o["type"]) == "message") {
            message["content"].as_array_mut().unwrap().push(part);
        } else {
            let mut message = response_output("message", &format!("{id}_msg_0"), status);
            message["role"] = json!("assistant");
            message["content"] = json!([part]);
            outputs.push(message);
        }
    }
    let tools = ctx.responses_tool_state();
    for (i, c) in arr(&m["tool_calls"]).iter().enumerate() {
        let typ = s(&c["type"]);
        let name = s(&c["function"]["name"]);
        if (typ.is_empty() || typ == "function") && name.trim().is_empty() {
            continue;
        }
        let callid = if s(&c["id"]).trim().is_empty() {
            format!("{id}_call_{i}")
        } else {
            s(&c["id"]).trim().to_owned()
        };
        let mut o = response_output(
            if typ.is_empty() || typ == "function" {
                "function_call"
            } else {
                typ
            },
            &callid,
            status,
        );
        o["call_id"] = Value::String(callid);
        if typ.is_empty() || typ == "function" {
            let (namespace, name) = tools.responses_tool_name(name);
            o["name"] = Value::String(name);
            if !namespace.is_empty() {
                o["namespace"] = Value::String(namespace);
            }
            let args = s(&c["function"]["arguments"]);
            if tools.is_custom_tool(s(&c["function"]["name"])) {
                o["type"] = json!("custom_tool_call");
                let input = custom_tool_input_from_arguments(args, ctx.options.preserve_message_structure);
                o["input"] = Value::String(input);
                for k in ["role", "content", "quality", "size"] {
                    o.as_object_mut().unwrap().remove(k);
                }
            } else if ctx.options.preserve_message_structure
                && tools.tool_search
                && s(&c["function"]["name"]) == "tool_search"
            {
                o["type"] = json!("tool_search_call");
                o["execution"] = json!("client");
                o["arguments"] = serde_json::from_str::<Value>(args)?;
                o.as_object_mut().unwrap().remove("name");
            } else {
                o["arguments"] = json!(args);
            }
        } else if !c["custom"].is_null() {
            o["arguments"] = c["custom"].clone();
        }
        if ctx.options.preserve_message_structure
            && let Some(signature) = c.get("thought_signature").or_else(|| c.get("thoughtSignature"))
        {
            o["thought_signature"] = signature.clone();
        }
        outputs.push(o);
    }
    let mut out = response_base(
        &id,
        s(&v["model"]),
        created,
        status,
        outputs,
        usage_from_chat(&v["usage"]),
    );
    if ctx.options.preserve_message_structure {
        out["usage"]["output_tokens_details"] = out["usage"]["completion_tokens_details"].clone();
    }
    if !reason.is_empty() {
        out["incomplete_details"] = json!({"reason":reason});
    }
    Ok(out)
}
pub(crate) fn append_separated(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !out.is_empty() {
        let trailing = out.chars().rev().take_while(|c| *c == '\n').take(2).count();
        let leading = text.chars().take_while(|c| *c == '\n').take(2).count();
        for _ in 0..2usize.saturating_sub(trailing + leading) {
            out.push('\n');
        }
    }
    out.push_str(text);
}
pub(crate) fn reasoning_output(v: &Value) -> String {
    let mut out = String::new();
    let content = arr(&v["content"]);
    let parts = if content.iter().any(|p| !s(&p["text"]).is_empty()) {
        content
    } else {
        arr(&v["summary"])
    };
    for p in parts {
        append_separated(&mut out, s(&p["text"]));
    }
    out
}
pub(crate) fn response_text(v: &Value) -> String {
    let mut out = String::new();
    for o in arr(&v["output"]) {
        if s(&o["type"]) == "message" && (s(&o["role"]).is_empty() || s(&o["role"]) == "assistant")
        {
            let t: String = arr(&o["content"])
                .iter()
                .filter(|p| s(&p["type"]) == "output_text")
                .map(|p| s(&p["text"]))
                .collect();
            append_separated(&mut out, &t);
        }
    }
    if out.is_empty() {
        for o in arr(&v["output"]) {
            let t: String = arr(&o["content"]).iter().map(|p| s(&p["text"])).collect();
            append_separated(&mut out, &t);
        }
    }
    out
}
pub(crate) fn responses_finish(v: &Value, tools: bool) -> &'static str {
    if s(&v["status"]).trim() == "incomplete" {
        if s(&v["incomplete_details"]["reason"]).trim() == "content_filter" {
            "content_filter"
        } else {
            "length"
        }
    } else if tools {
        "tool_calls"
    } else {
        "stop"
    }
}
fn tool_call(id: &str, name: &str, args: String) -> Value {
    let mut o = owned_object([
        ("type", json!("function")),
        (
            "function",
            owned_object([("arguments", Value::String(args))]),
        ),
    ]);
    if !id.is_empty() {
        o["id"] = json!(id);
    }
    if !name.is_empty() {
        o["function"]["name"] = json!(name);
    }
    o
}
fn responses_to_chat(v: &Value, _ctx: &Context) -> Result<Value, Error> {
    let mut m = json!({"role":"assistant","content":response_text(v)});
    let mut r = String::new();
    let mut calls = vec![];
    let mut annotations = vec![];
    for o in arr(&v["output"]) {
        match s(&o["type"]) {
            "reasoning" => append_separated(&mut r, &reasoning_output(o)),
            "function_call" | "custom_tool_call" => {
                if !s(&o["name"]).trim().is_empty() {
                    let id = if s(&o["call_id"]).trim().is_empty() {
                        s(&o["id"]).trim()
                    } else {
                        s(&o["call_id"]).trim()
                    };
                    calls.push(tool_call(
                        id,
                        s(&o["name"]).trim(),
                        stringify(&o["arguments"]),
                    ));
                }
            }
            "message" => {
                for c in arr(&o["content"]) {
                    for a in arr(&c["annotations"]) {
                        annotations.push(citations::response_annotation_to_chat(a)?);
                    }
                }
            }
            _ => {}
        }
    }
    let finish = responses_finish(v, !calls.is_empty());
    if !calls.is_empty() {
        m["tool_calls"] = json!(calls);
    }
    if !r.is_empty() {
        m["reasoning_content"] = json!(r);
    }
    if !annotations.is_empty() {
        m["annotations"] = json!(annotations);
    }
    let id = if s(&v["id"]).trim().is_empty() {
        new_id("chatcmpl-")
    } else {
        s(&v["id"]).trim().to_owned()
    };
    Ok(
        json!({"id":id,"object":"chat.completion","model":s(&v["model"]),"created":n(&v["created_at"]),"choices":[{"index":0,"message":m,"finish_reason":finish}],"usage":usage_from_responses(&v["usage"])}),
    )
}
fn responses_to_claude(v: &Value) -> Value {
    let mut blocks = vec![];
    let mut saw_tool = false;
    for o in arr(&v["output"]) {
        match s(&o["type"]) {
            "reasoning" => {
                let r = reasoning_output(o);
                if !r.is_empty() {
                    blocks.push(json!({"type":"thinking","thinking":r}));
                }
            }
            "message" => {
                if !s(&o["role"]).is_empty() && s(&o["role"]) != "assistant" {
                    continue;
                }
                for p in arr(&o["content"]) {
                    if s(&p["type"]) != "output_text" {
                        continue;
                    }
                    let mut b = json!({"type":"text","text":s(&p["text"])});
                    let cs = citations::to_claude(&p["annotations"], s(&p["text"]));
                    if !cs.is_empty() {
                        b["citations"] = json!(cs);
                    }
                    blocks.push(b);
                }
            }
            "function_call" | "custom_tool_call" => {
                saw_tool = true;
                let id = if s(&o["call_id"]).trim().is_empty() {
                    s(&o["id"]).trim()
                } else {
                    s(&o["call_id"]).trim()
                };
                let args = stringify(&o["arguments"]);
                let input = if args.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str::<Value>(&args)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({"input":args}))
                };
                blocks.push(json!({"type":"tool_use","id":id,"name":s(&o["name"]),"input":input}));
            }
            _ => {}
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({"type":"text","text":""}));
    }
    json!({"id":s(&v["id"]),"type":"message","role":"assistant","model":s(&v["model"]),"content":blocks,"stop_reason":claude_stop(responses_finish(v,saw_tool)),"usage":claude_usage(&usage_from_responses(&v["usage"]))})
}
fn claude_to_chat(v: &Value, ctx: &Context) -> Value {
    let mut text = String::new();
    let mut thinking = "";
    let mut calls = vec![];
    let mut annotations = vec![];
    for b in arr(&v["content"]) {
        match s(&b["type"]) {
            "text" => {
                let offset = text.chars().count();
                let t = s(&b["text"]);
                annotations.extend(citations::claude_to_chat(&b["citations"], t, offset));
                text.push_str(t);
            }
            "thinking" => {
                if !b["thinking"].is_null() {
                    thinking = s(&b["thinking"]);
                }
            }
            "tool_use" => calls.push(tool_call(
                s(&b["id"]),
                s(&b["name"]),
                crate::json::stringify_interface(&b["input"]),
            )),
            _ => {}
        }
    }
    // Go's choice embeds Message: its first-block reasoning fallback is a
    // message field, and an empty final thinking block does not clear it.
    if thinking.is_empty() {
        thinking = arr(&v["content"])
            .first()
            .map(|block| s(&block["thinking"]))
            .unwrap_or("");
    }
    let mut m = owned_object([
        ("role", json!("assistant")),
        ("content", Value::String(text)),
    ]);
    if !thinking.is_empty() {
        m["reasoning_content"] = json!(thinking);
    }
    if ctx.options.preserve_message_structure {
        let preserved = arr(&v["content"]).iter()
            .filter(|b| matches!(s(&b["type"]), "thinking" | "redacted_thinking"))
            .cloned().collect::<Vec<_>>();
        if !preserved.is_empty() {
            m["reasoning_content"] = json!(preserved.iter().map(|b| s(&b["thinking"])).collect::<String>());
            if let Some(last) = preserved.last() {
                let redacted = s(&last["type"]) == "redacted_thinking";
                let signature = s(&last[if redacted { "data" } else { "signature" }]);
                if !signature.is_empty() {
                    m["reasoning_signature"] = json!(signature);
                }
                if redacted {
                    m["reasoning_block_type"] = json!("redacted_thinking");
                }
            }
            m["claude_reasoning_blocks"] = Value::Array(preserved);
        }
    }
    if !annotations.is_empty() {
        m["annotations"] = Value::Array(annotations);
    }
    if !calls.is_empty() {
        m["tool_calls"] = Value::Array(calls);
    }
    let choice = owned_object([
        ("index", json!(0)),
        ("message", m),
        (
            "finish_reason",
            Value::String(chat_stop(s(&v["stop_reason"]))),
        ),
    ]);
    owned_object([
        ("id", json!(s(&v["id"]))),
        ("object", json!("chat.completion")),
        ("model", json!(s(&v["model"]))),
        (
            "created",
            json!(if ctx.created == 0 { now() } else { ctx.created }),
        ),
        ("choices", Value::Array(vec![choice])),
        (
            "usage",
            canonical_usage(Format::Claude, v, 0).unwrap_or_else(|| normalize_usage(&Value::Null)),
        ),
    ])
}
pub(crate) fn render_gemini_parts(
    parts: &Value,
    preserve_message_structure: bool,
) -> (String, Vec<Value>, Option<String>, Option<String>) {
    let mut pieces = vec![];
    let mut calls = vec![];
    let mut reason = None;
    let mut reasoning_signature = None;
    for p in arr(parts) {
        if !p["inlineData"].is_null() {
            let media = &p["inlineData"];
            pieces.push(format!(
                "{}(data:{};base64,{})",
                if s(&media["mimeType"]).starts_with("image") {
                    "![image]"
                } else {
                    "[media]"
                },
                s(&media["mimeType"]),
                s(&media["data"])
            ));
        } else if !p["functionCall"].is_null() {
            let f = &p["functionCall"];
            let id = if s(&f["id"]).trim().is_empty() {
                new_id("call_")
            } else {
                s(&f["id"]).trim().to_owned()
            };
            let mut call = tool_call(&id, s(&f["name"]), go_json(&f["args"]));
            if preserve_message_structure && !s(&p["thoughtSignature"]).is_empty() {
                call["thoughtSignature"] = p["thoughtSignature"].clone();
            }
            calls.push(call);
        } else if p["thought"] == true {
            reason = Some(s(&p["text"]).to_owned());
            if preserve_message_structure && !s(&p["thoughtSignature"]).is_empty() {
                reasoning_signature = Some(s(&p["thoughtSignature"]).to_owned());
            }
        } else if !p["executableCode"].is_null() {
            pieces.push(format!(
                "```{}\n{}\n```",
                s(&p["executableCode"]["language"]),
                s(&p["executableCode"]["code"])
            ));
        } else if !p["codeExecutionResult"].is_null() {
            pieces.push(format!(
                "```output\n{}\n```",
                s(&p["codeExecutionResult"]["output"])
            ));
        } else if s(&p["text"]) != "\n" {
            pieces.push(s(&p["text"]).to_owned());
        }
    }
    (pieces.join("\n"), calls, reason, reasoning_signature)
}
fn gemini_to_chat(v: &Value, ctx: &Context) -> Value {
    let mut choices = vec![];
    let mut has_calls = false;
    for c in arr(&v["candidates"]) {
        let (text, calls, reason, reasoning_signature) = render_gemini_parts(
            &c["content"]["parts"],
            ctx.options.preserve_message_structure,
        );
        let mut m = json!({"role":"assistant","content":text});
        if !calls.is_empty() {
            has_calls = true;
            m["tool_calls"] = json!(calls);
        }
        if let Some(reason) = reason {
            m["reasoning_content"] = json!(reason);
        }
        if let Some(signature) = reasoning_signature {
            m["reasoning_signature"] = json!(signature);
        }
        let annotations = citations::gemini_to_chat(&c["groundingMetadata"], &c["content"], &text);
        if !annotations.is_empty() {
            m["annotations"] = json!(annotations);
        }
        let finish = if has_calls {
            "tool_calls"
        } else if c["finishReason"].is_null() {
            "stop"
        } else {
            match s(&c["finishReason"]) {
                "STOP" => "stop",
                "MAX_TOKENS" => "length",
                _ => "content_filter",
            }
        };
        choices.push(json!({"index":n(&c["index"]),"message":m,"finish_reason":finish}));
    }
    json!({"id":if ctx.response_id.is_empty(){new_id("chatcmpl-")}else{ctx.response_id.clone()},"object":"chat.completion","model":if ctx.channel_meta_attached{ctx.upstream_model_name.as_str()}else{""},"created":if ctx.created==0{now()}else{ctx.created},"choices":choices,"usage":usage_from_gemini(&v["usageMetadata"],ctx.estimate_prompt_tokens).unwrap_or_else(||normalize_usage(&Value::Null))})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_billing_snapshot_survives_three_protocols() {
        let source = json!({"id":"m","model":"claude","content":[],"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":7,"cache_read_input_tokens":3,"cache_creation":{"ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":5}}});
        let ctx = Context::default();
        let chat = convert(Format::Claude, Format::OpenAi, &source, &ctx)
            .unwrap()
            .0;
        assert_eq!(chat["usage"]["prompt_tokens"], 20);
        assert_eq!(
            chat["usage"]["prompt_tokens_details"]["cache_write_tokens"],
            7
        );
        let responses = convert(Format::OpenAi, Format::OpenAiResponses, &chat, &ctx)
            .unwrap()
            .0;
        let restored = convert(Format::OpenAiResponses, Format::Claude, &responses, &ctx)
            .unwrap()
            .0;
        assert_eq!(restored["usage"]["input_tokens"], 10);
        assert_eq!(
            restored["usage"]["cache_creation"]["ephemeral_1h_input_tokens"],
            5
        );
        assert_eq!(
            restored["usage"]["billing_usage"],
            chat["usage"]["billing_usage"]
        );
    }

    #[test]
    fn grounded_unicode_offsets_are_utf8_bytes_on_input_and_scalars_on_output() {
        let metadata = json!({"groundingChunks":[{"web":{"uri":"https://example.test","title":"Source"}}],"groundingSupports":[{"segment":{"startIndex":3,"endIndex":6,"text":"文"},"groundingChunkIndices":[0]}]});
        let content = json!({"parts":[{"text":"中文答案"}]});
        let a = citations::gemini_to_chat(&metadata, &content, "中文答案");
        assert_eq!(a[0]["url_citation"]["start_index"], 1);
        assert_eq!(a[0]["url_citation"]["end_index"], 2);
        let restored = citations::to_claude(&json!(a), "中文答案");
        assert_eq!(restored[0]["cited_text"], "文");
    }

    #[test]
    fn hosted_mcp_preserves_order_and_does_not_become_a_function() {
        let source = json!({"id":"m","content":[{"type":"text","text":"before"},{"type":"mcp_tool_use","id":"h","name":"lookup","server_name":"s","input":{"q":"x"}},{"type":"mcp_tool_result","tool_use_id":"h","content":"found"},{"type":"text","text":"after"}],"stop_reason":"end_turn"});
        let (out, ds, _) = convert(
            Format::Claude,
            Format::OpenAiResponses,
            &source,
            &Context::default(),
        )
        .unwrap();
        assert_eq!(out["output"][0]["type"], "mcp_call");
        assert_eq!(out["output"][0]["arguments"], "{\"q\":\"x\"}");
        assert_eq!(out["output"][0]["output"], "found");
        assert_eq!(out["output"][0]["status"], "completed");
        assert!(
            ds.iter()
                .any(|d| d.code == "hosted_tool_order_unrepresentable")
        );
        assert!(
            !arr(&out["output"])
                .iter()
                .any(|v| v["type"] == "function_call")
        );
    }

    #[test]
    fn responses_direct_claude_retains_message_boundaries() {
        let source = json!({"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"one"},{"type":"output_text","text":"two"}]},{"type":"function_call","id":"c","name":"f","arguments":"bad JSON"}],"status":"incomplete","incomplete_details":{"reason":"content_filter"}});
        let (out, _, _) = convert(
            Format::OpenAiResponses,
            Format::Claude,
            &source,
            &Context::default(),
        )
        .unwrap();
        assert_eq!(out["content"][0]["text"], "one");
        assert_eq!(out["content"][1]["text"], "two");
        assert_eq!(out["content"][2]["input"], json!({"input":"bad JSON"}));
        assert_eq!(out["stop_reason"], "refusal");
    }
}
