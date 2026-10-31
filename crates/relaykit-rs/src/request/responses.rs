use super::*;

fn validate(input: &Value) -> Result<(), Error> {
    model_required(input)?;
    let unsupported: [&str; 4] = [
        "conversation",
        "previous_response_id",
        "prompt",
        "context_management",
    ];
    let present: Vec<_> = unsupported
        .into_iter()
        .filter(|k| {
            if *k == "previous_response_id" {
                !s(&input[*k]).trim().is_empty()
            } else {
                !input[*k].is_null()
            }
        })
        .collect();
    if !present.is_empty() {
        return Err(Error::message(format!(
            "responses to chat conversion does not support stateful fields: {}",
            present.join(", ")
        )));
    }
    Ok(())
}
pub(super) fn to_chat(
    input: &Value,
    ctx: &Context,
    ds: &mut Vec<Diagnostic>,
) -> Result<Value, Error> {
    validate(input)?;
    let mut out = json!({"model":s(&input["model"])});
    copy_fields(
        input,
        &mut out,
        &[
            "stream",
            "stream_options",
            "temperature",
            "top_p",
            "top_logprobs",
            "user",
            "store",
            "metadata",
            "safety_identifier",
            "prompt_cache_retention",
            "enable_thinking",
            "thinking_budget",
        ],
    );
    copy_renamed(
        input,
        &mut out,
        &[("max_output_tokens", "max_completion_tokens")],
    );
    for k in ["frequency_penalty", "presence_penalty"] {
        if !input[k].is_null() {
            if !input[k].is_number() {
                return Err(Error::message(format!("invalid {k}: expected number")));
            }
            out[k] = input[k].clone();
        }
    }
    put_nonempty(&mut out, "service_tier", s(&input["service_tier"]));
    if input["parallel_tool_calls"].is_boolean() {
        out["parallel_tool_calls"] = input["parallel_tool_calls"].clone();
    }
    put_nonempty(&mut out, "prompt_cache_key", s(&input["prompt_cache_key"]));
    if let Some(format) = response_format(&input["text"])? {
        out["response_format"] = format;
    }
    if !input["tool_choice"].is_null() {
        out["tool_choice"] = chat_choice(&input["tool_choice"])?;
    }
    if !input["tools"].is_null() {
        if !input["tools"].is_array() {
            return Err(Error::message("invalid tools: expected array"));
        }
        let tools: Vec<_> = arr(&input["tools"])
            .iter()
            .map(|t| {
                if s(&t["type"]).trim() == "function" {
                    let mut f = json!({});
                    put_nonempty(&mut f, "name", s(&t["name"]).trim());
                    put_nonempty(&mut f, "description", s(&t["description"]));
                    if !t["parameters"].is_null() {
                        f["parameters"] = t["parameters"].clone();
                    }
                    owned_object([("type", Value::String("function".into())), ("function", f)])
                } else {
                    json!({"type":s(&t["type"]).trim(),"custom":t})
                }
            })
            .collect();
        if !tools.is_empty() {
            out["tools"] = Value::Array(tools);
        }
    }
    let mut messages = vec![];
    let mut pending = vec![];
    if !input["instructions"].is_null() {
        let text = string_value(&input["instructions"]);
        if !text.trim().is_empty() {
            messages.push(owned_object([
                ("role", Value::String("system".into())),
                ("content", Value::String(text)),
            ]));
        }
    }
    for item in input_items(&input["input"])?.iter() {
        let kind = s(&item["type"]).trim();
        if !pending.is_empty()
            && !matches!(kind, "function_call_output" | "custom_tool_call_output")
        {
            messages.push(owned_object([
                ("role", Value::String("user".into())),
                ("content", Value::Array(std::mem::take(&mut pending))),
            ]));
        }
        match kind {
            "function_call" | "custom_tool_call" | "tool_search_call" => {
                let name = resolved_call_name(item, ctx);
                if name.is_empty() {
                    return Err(Error::message(format!("{kind} item is missing name")));
                }
                let args = if kind == "custom_tool_call" {
                    stringify(&json!({"input":string_value(&item["input"])}))
                } else {
                    string_value(&item["arguments"])
                };
                let mut function = json!({"name":name});
                put_nonempty(&mut function, "arguments", &args);
                let mut call = owned_object([
                    ("type", Value::String("function".into())),
                    ("function", function),
                ]);
                put_nonempty(&mut call, "id", call_id(item));
                if ctx.options.preserve_message_structure {
                    if let Some(signature) = item
                        .get("thought_signature")
                        .or_else(|| item.get("thoughtSignature"))
                    {
                        call["thoughtSignature"] = signature.clone();
                    }
                }
                if messages.last().is_none_or(|m| m["role"] != "assistant") {
                    messages.push(json!({"role":"assistant","content":null,"tool_calls":[]}));
                }
                let last = messages.last_mut().unwrap();
                if !last["tool_calls"].is_array() {
                    last["tool_calls"] = json!([]);
                }
                last["tool_calls"].as_array_mut().unwrap().push(call);
            }
            "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
                let (content, media) = if ctx.options.preserve_message_structure {
                    split_chat_tool_content(
                        if item["output"].is_array() {
                            preserved_chat_content(&item["output"], ctx, ds)?
                        } else {
                            json!(string_value(&item["output"]))
                        },
                    )
                } else {
                    tool_output_chat(&item["output"], ctx, ds)
                };
                let mut msg =
                    owned_object([("role", Value::String("tool".into())), ("content", content)]);
                put_nonempty(&mut msg, "tool_call_id", s(&item["call_id"]).trim());
                if ctx.options.preserve_message_structure {
                    copy_fields(item, &mut msg, &["is_error", "cache_control"]);
                }
                messages.push(msg);
                pending.extend(media);
            }
            "reasoning" if ctx.options.preserve_message_structure => {
                if messages.last().is_none_or(|message| message["role"] != "assistant") {
                    messages.push(json!({"role":"assistant","content":null}));
                }
                add_reasoning_message(messages.last_mut().unwrap(), &reasoning_from_item(item));
            }
            "item_reference" if ctx.options.preserve_message_structure => {
                return Err(Error::message(
                    "Responses item_reference cannot be resolved for a stateless upstream",
                ));
            }
            _ => {
                if ctx.options.preserve_message_structure
                    && !matches!(kind, "" | "message")
                {
                    return Err(Error::message(format!(
                        "Responses input item {kind:?} cannot be represented as a Chat message"
                    )));
                }
                let role = s(&item["role"]).trim();
                let content = if ctx.options.preserve_message_structure {
                    preserved_chat_content(&item["content"], ctx, ds)?
                } else {
                    chat_content(&item["content"], ctx, ds)
                };
                messages.push(owned_object([
                    (
                        "role",
                        Value::String(if role.is_empty() { "user" } else { role }.into()),
                    ),
                    ("content", content),
                ]));
            }
        }
    }
    if !pending.is_empty() {
        messages.push(owned_object([
            ("role", Value::String("user".into())),
            ("content", Value::Array(pending)),
        ]));
    }
    if !messages.is_empty() {
        if ctx.options.preserve_message_structure {
            normalize_chat_tool_turns(&mut messages);
        }
        out["messages"] = Value::Array(messages);
    }
    Ok(out)
}
fn preserved_chat_content(
    content: &Value,
    ctx: &Context,
    ds: &mut Vec<Diagnostic>,
) -> Result<Value, Error> {
    if content.is_null() {
        return Ok(json!(""));
    }
    if content.is_string() {
        return Ok(content.clone());
    }
    let input = content
        .as_array()
        .ok_or_else(|| Error::message("Responses message content must be text or an array"))?;
    let mut output = Vec::with_capacity(input.len());
    for part in input {
        let kind = s(&part["type"]).trim();
        if !matches!(
            kind,
            "input_text" | "output_text" | "text" | "input_image" | "input_file"
                | "input_audio" | "input_video" | "refusal"
        ) {
            return Err(Error::message(format!(
                "Responses content part {kind:?} cannot be represented by Chat"
            )));
        }
        let mut converted = if kind == "input_file" {
            file_chat(part, ctx).map_err(Error::message)?
        } else if kind == "refusal" {
            part.clone()
        } else {
            let converted = chat_content(&json!([part]), ctx, ds);
            if let Some(parts) = converted.as_array() {
                parts.first().cloned().ok_or_else(|| {
                    Error::message("Responses content part could not be represented by Chat")
                })?
            } else {
                json!({"type":"text","text":converted})
            }
        };
        copy_content_metadata(part, &mut converted);
        copy_fields(part, &mut converted, &["mime_type"]);
        output.push(converted);
    }
    Ok(Value::Array(output))
}
fn chat_content(content: &Value, ctx: &Context, ds: &mut Vec<Diagnostic>) -> Value {
    if content.is_null() {
        return json!("");
    }
    if !content.is_array() {
        return content.clone();
    }
    let mut parts = vec![];
    let mut text = String::new();
    let mut only_text = true;
    let mut dropped_file = false;
    for p in arr(content) {
        if !p.is_object() {
            only_text = false;
            parts.push(p.clone());
            continue;
        }
        match s(&p["type"]).trim() {
            "input_text" | "output_text" | "text" => {
                let t = response_text_value(&p["text"]).into_owned();
                text.push_str(&t);
                parts.push(json!({"type":"text","text":t}));
            }
            "input_image" => {
                only_text = false;
                let image = if p.get("image_url").is_some() {
                    if p["image_url"].is_string() {
                        json!({"url":p["image_url"]})
                    } else {
                        p["image_url"].clone()
                    }
                } else {
                    let mut image = json!({});
                    copy_fields(p, &mut image, &["url", "file_id", "detail"]);
                    if image.as_object().unwrap().is_empty() {
                        p.clone()
                    } else {
                        image
                    }
                };
                parts.push(json!({"type":"image_url","image_url":image}));
            }
            "input_file" => {
                only_text = false;
                match file_chat(p, ctx) {
                    Ok(p) => parts.push(p),
                    Err(reason) => {
                        diag(
                            ds,
                            Severity::Error,
                            "unsupported_media_type",
                            "input.content",
                            format!("{reason}; the file was omitted"),
                        );
                        dropped_file = true;
                    }
                }
            }
            "input_audio" => {
                only_text = false;
                parts.push(
                    json!({"type":"input_audio","input_audio":part_payload(p,"input_audio")}),
                );
            }
            "input_video" => {
                only_text = false;
                let video = if p.get("video_url").is_some() {
                    let v = &p["video_url"];
                    if v.is_object() && !s(&v["url"]).is_empty() {
                        v["url"].clone()
                    } else {
                        v.clone()
                    }
                } else if !s(&p["url"]).is_empty() {
                    p["url"].clone()
                } else {
                    part_payload(p, "video_url")
                };
                parts.push(json!({"type":"video_url","video_url":video}));
            }
            _ => {
                only_text = false;
                parts.push(p.clone());
            }
        }
    }
    if only_text {
        json!(text)
    } else if parts.is_empty() && dropped_file {
        json!("[file]")
    } else {
        Value::Array(parts)
    }
}
fn part_payload(part: &Value, key: &str) -> Value {
    if let Some(v) = part.get(key) {
        return v.clone();
    }
    let mut p = part.clone();
    p.as_object_mut().unwrap().remove("type");
    p
}
fn file_chat(part: &Value, ctx: &Context) -> Result<Value, String> {
    let fields = if part["file"].is_object() {
        &part["file"]
    } else {
        part
    };
    let mut filename = s(&fields["filename"]).trim().to_owned();
    let Some((data, mime)) = source(part, true) else {
        let id = s(&fields["file_id"]).trim();
        if id.is_empty() {
            return Err("input_file carries no file_data, file_url, or file_id".into());
        }
        let mut f = json!({"file_id":id});
        put_nonempty(&mut f, "filename", &filename);
        return Ok(json!({"type":"file","file":f}));
    };
    if ctx.options.preserve_message_structure && is_http_url(&data) {
        let mut file = json!({"file_url":data});
        put_nonempty(&mut file, "filename", &filename);
        return Ok(json!({"type":"file","file":file}));
    }
    let (data, mime) = ctx
        .resolve_media(&data, &mime)
        .map_err(|e| format!("input_file could not be loaded: {e}"))?;
    let media = normalized_mime(&mime);
    if media == "application/pdf" {
        if filename.is_empty() {
            filename = "document.pdf".into();
        }
        return Ok(
            json!({"type":"file","file":{"filename":filename,"file_data":format!("data:application/pdf;base64,{data}")}}),
        );
    }
    if is_text_mime(&media) {
        let text = crate::media::decode_text(&data).ok_or_else(|| {
            format!("input_file of MIME type {mime:?} could not be decoded as UTF-8 text")
        })?;
        return Ok(json!({"type":"text","text":text}));
    }
    Err(format!(
        "OpenAI Chat Completions file parts accept only PDF, not {mime:?}"
    ))
}
fn tool_output_chat(value: &Value, ctx: &Context, ds: &mut Vec<Diagnostic>) -> (Value, Vec<Value>) {
    if arr(value).is_empty() {
        return (json!(string_value(value)), vec![]);
    }
    let mut texts = vec![];
    let mut media = vec![];
    let mut labels = vec![];
    for p in arr(value) {
        match s(&p["type"]).trim() {
            "input_text" | "output_text" | "text" => {
                let text = response_text_value(&p["text"]).into_owned();
                if !text.is_empty() {
                    texts.push(text);
                }
            }
            "input_image" | "input_file" | "input_audio" | "input_video" => {
                media.push(p.clone());
                let label = format!("[{}]", s(&p["type"]).trim().trim_start_matches("input_"));
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
            _ => return (json!(string_value(value)), vec![]),
        }
    }
    if media.is_empty() {
        return (json!(texts.join("\n")), vec![]);
    }
    let converted = chat_content(&Value::Array(media), ctx, ds);
    (
        json!(if texts.is_empty() {
            labels.join(" ")
        } else {
            texts.join("\n")
        }),
        arr(&converted).to_vec(),
    )
}
fn chat_choice(choice: &Value) -> Result<Value, Error> {
    if choice.is_null() || choice.is_string() {
        return Ok(choice.clone());
    }
    if !choice.is_object() {
        return Err(Error::message("invalid tool_choice: expected object"));
    }
    let name = s(&choice["name"]).trim();
    Ok(if choice["type"] == "function" && !name.is_empty() {
        json!({"type":"function","function":{"name":name}})
    } else {
        choice.clone()
    })
}
fn response_format(text: &Value) -> Result<Option<Value>, Error> {
    if text.is_null() {
        return Ok(None);
    }
    if !text.is_object() {
        return Err(Error::message("invalid text config: expected object"));
    }
    let format = &text["format"];
    let kind = s(&format["type"]).trim();
    if !format.is_object() || kind.is_empty() {
        return Ok(None);
    }
    let mut out = json!({"type":kind});
    if kind == "json_schema" {
        out["json_schema"] = format.clone();
    }
    Ok(Some(out))
}
fn functions(input: &Value) -> Result<Vec<Value>, Error> {
    if input["tools"].is_null() {
        return Ok(vec![]);
    }
    if !input["tools"].is_array() {
        return Err(Error::message("invalid tools: expected array"));
    }
    Ok(arr(&input["tools"])
        .iter()
        .filter(|t| s(&t["type"]).trim() == "function" && !s(&t["name"]).trim().is_empty())
        .map(|t| {
            let mut f = json!({"name":s(&t["name"]).trim()});
            put_nonempty(&mut f, "description", s(&t["description"]));
            if !t["parameters"].is_null() {
                f["parameters"] = t["parameters"].clone();
            }
            f
        })
        .collect())
}

pub(super) fn to_claude(
    input: &Value,
    ctx: &Context,
    ds: &mut Vec<Diagnostic>,
) -> Result<Value, Error> {
    validate(input)?;
    let mut out = json!({"model":s(&input["model"])});
    if ctx.options.preserve_message_structure
        && let Some(effort) = input["reasoning"]["effort"].as_str()
    {
        let effort = match effort {
            "xhigh" | "ultra" => "max",
            "minimal" | "none" => "low",
            other => other,
        };
        out["output_config"] = json!({"effort": effort});
    }
    copy_fields(input, &mut out, &["temperature", "top_p", "stream"]);
    if input["max_output_tokens"].as_u64().unwrap_or(0) > 0 {
        out["max_tokens"] = input["max_output_tokens"].clone();
    }
    if ctx.options.preserve_message_structure {
        if let Some(format) = response_format(&input["text"])? {
            match s(&format["type"]) {
                "json_schema" => {
                    out["output_config"]["format"] = json!({
                        "type":"json_schema",
                        "schema":format["json_schema"]["schema"],
                    });
                }
                "json_object" => {
                    out["output_config"]["format"] = json!({
                        "type":"json_schema",
                        "schema":{"type":"object"},
                    });
                }
                "text" => {}
                other => {
                    return Err(Error::message(format!(
                        "Responses text format {other:?} cannot be represented by Claude"
                    )));
                }
            }
        }
    }
    let tools: Vec<_> = functions(input)?
        .iter()
        .map(|f| {
            let mut t = json!({"name":f["name"],"input_schema":function_schema(&f["parameters"])});
            put_nonempty(&mut t, "description", s(&f["description"]));
            t
        })
        .collect();
    if !tools.is_empty() {
        out["tools"] = Value::Array(tools);
    }
    let choice = chat_choice(&input["tool_choice"])?;
    if let Some(choice) = chat::claude_choice(&choice, &input["parallel_tool_calls"]) {
        out["tool_choice"] = choice;
    }
    let mut system = vec![];
    let mut messages = vec![];
    if !input["instructions"].is_null() {
        let text = string_value(&input["instructions"]);
        if !text.trim().is_empty() {
            system.push(json!({"type":"text","text":text}));
        }
    }
    let mut leading_system = true;
    for item in input_items(&input["input"])?.iter() {
        if !matches!(s(&item["role"]), "system" | "developer") {
            leading_system = false;
        }
        match s(&item["type"]).trim() {
            "function_call" | "custom_tool_call" | "tool_search_call" => {
                let name = resolved_call_name(item, ctx);
                let custom = item["type"] == "custom_tool_call";
                if custom && name.is_empty() {
                    return Err(Error::message("custom_tool_call item is missing name"));
                }
                let mut block = json!({"type":"tool_use"});
                put_nonempty(&mut block, "id", call_id(item));
                put_nonempty(&mut block, "name", &name);
                let args = if custom {
                    json!({"input":string_value(&item["input"])})
                } else {
                    let arguments = item
                        .get("arguments")
                        .or_else(|| item.get("input"))
                        .unwrap_or(&Value::Null);
                    if ctx.options.preserve_message_structure {
                        checked_tool_arguments(arguments)?
                    } else {
                        object_value(arguments, "arguments")
                    }
                };
                if !args.is_null() {
                    block["input"] = args;
                }
                chat::append_claude(&mut messages, "assistant", block);
            }
            "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
                let mut block = json!({"type":"tool_result"});
                put_nonempty(&mut block, "tool_use_id", call_id(item));
                let output = tool_output_claude(&item["output"], ctx, ds)?;
                if !output.is_null() {
                    block["content"] = output;
                }
                if ctx.options.preserve_message_structure {
                    copy_fields(item, &mut block, &["is_error", "cache_control"]);
                }
                chat::append_claude(&mut messages, "user", block);
            }
            "reasoning" if ctx.options.preserve_message_structure => {
                chat::append_claude(&mut messages, "assistant", reasoning_from_item(item));
            }
            "item_reference" if ctx.options.preserve_message_structure => {
                return Err(Error::message(
                    "Responses item_reference cannot be resolved for a stateless upstream",
                ));
            }
            _ => {
                let kind = s(&item["type"]).trim();
                if ctx.options.preserve_message_structure
                    && !matches!(kind, "" | "message")
                {
                    return Err(Error::message(format!(
                        "Responses input item {kind:?} cannot be represented by Claude"
                    )));
                }
                let source_role = s(&item["role"]).trim();
                let role = match source_role {
                    "assistant" => "assistant",
                    "system" | "developer" => "system",
                    _ => "user",
                };
                let mut parts = claude_content(&item["content"], ctx, ds)?;
                if source_role.is_empty() && parts.is_empty() {
                    continue;
                }
                if role == "system" {
                    if !ctx.options.preserve_message_structure || leading_system {
                        system.extend(parts.into_iter().filter(|p| p["type"] == "text"));
                    } else {
                        for part in parts {
                            chat::append_claude(&mut messages, "user", part);
                        }
                    }
                    continue;
                }
                if parts.is_empty() {
                    parts.push(json!({"type":"text","text":"..."}));
                }
                messages.push(owned_object([
                    ("role", Value::String(role.into())),
                    ("content", Value::Array(parts)),
                ]));
            }
        }
    }
    if !ctx.options.preserve_message_structure {
        omit_trailing(&mut messages, false, ds);
    }
    if ctx.options.preserve_message_structure {
        normalize_claude_tool_turns(&mut messages);
    }
    if !ctx.options.preserve_message_structure
        && (!messages.is_empty() || !system.is_empty())
        && messages.first().is_none_or(|m| m["role"] != "user")
    {
        messages.insert(
            0,
            json!({"role":"user","content":[{"type":"text","text":"..."}]}),
        );
    }
    if !messages.is_empty() {
        out["messages"] = Value::Array(messages);
    }
    if !system.is_empty() {
        out["system"] = json!(system);
    }
    Ok(out)
}
fn claude_content(
    content: &Value,
    ctx: &Context,
    ds: &mut Vec<Diagnostic>,
) -> Result<Vec<Value>, Error> {
    let mut out = vec![];
    for p in content_parts(content) {
        let kind = s(&p["type"]).trim();
        match kind {
            "input_text" | "output_text" | "text" => {
                let text = response_text_value(&p["text"]).into_owned();
                if !text.is_empty() {
                    let mut block = json!({"type":"text","text":text});
                    if ctx.options.preserve_message_structure {
                        copy_content_metadata(&p, &mut block);
                    }
                    out.push(block);
                }
            }
            "input_image" | "input_file" | "input_audio" | "input_video" => {
                let Some((data, mime)) = source(&p, true) else {
                    if ctx.options.preserve_message_structure {
                        return Err(Error::message(format!(
                            "Responses {kind} content requires an inline source or resolvable URL"
                        )));
                    }
                    continue;
                };
                if ctx.options.preserve_message_structure
                    && is_http_url(&data)
                    && matches!(kind, "input_image" | "input_file")
                {
                    let mut block = claude_url_part(kind, &data);
                    copy_content_metadata(&p, &mut block);
                    out.push(block);
                    continue;
                }
                let result = if matches!(kind, "input_image" | "input_file") {
                    let (data, mime) = ctx
                        .resolve_media(&data, &mime)
                        .map_err(|e| Error::message(format!("get file data failed: {e}")))?;
                    claude_media(&data, &mime, kind == "input_image")
                } else {
                    Err(format!("Claude Messages cannot carry {kind} content"))
                };
                match result {
                        Ok(mut block) => {
                            if ctx.options.preserve_message_structure {
                                copy_content_metadata(&p, &mut block);
                            }
                            out.push(block);
                        }
                    Err(reason) if ctx.options.preserve_message_structure => {
                        return Err(Error::message(reason));
                    }
                    Err(reason) => diag(
                        ds,
                        Severity::Error,
                        "unsupported_media_type",
                        "input.content",
                        format!("{reason}; the part was omitted"),
                    ),
                }
            }
            other if ctx.options.preserve_message_structure => {
                return Err(Error::message(format!(
                    "Responses content part {other:?} cannot be represented by Claude"
                )));
            }
            _ => {}
        }
    }
    Ok(out)
}
fn tool_output_claude(
    value: &Value,
    ctx: &Context,
    ds: &mut Vec<Diagnostic>,
) -> Result<Value, Error> {
    if arr(value).is_empty() {
        return Ok(if value.is_null() {
            json!("")
        } else if ctx.options.preserve_message_structure && !value.is_string() {
            json!(string_value(value))
        } else {
            value.clone()
        });
    }
    let mut labels = vec![];
    for p in arr(value) {
        match s(&p["type"]).trim() {
            "input_text" | "output_text" | "text" => {}
            "input_image" | "input_file" | "input_audio" | "input_video" => {
                let label = format!("[{}]", s(&p["type"]).trim().trim_start_matches("input_"));
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
            _ if ctx.options.preserve_message_structure => {
                return Err(Error::message(
                    "Claude tool-result output contains an unsupported content part",
                ));
            }
            _ => return Ok(json!(string_value(value))),
        }
    }
    if ctx.options.preserve_message_structure {
        return Ok(Value::Array(claude_content(value, ctx, ds)?));
    }
    Ok(match claude_content(value, ctx, ds) {
        Ok(parts) if parts.is_empty() => json!(labels.join(" ")),
        Ok(parts) => Value::Array(parts),
        Err(_) => json!(string_value(value)),
    })
}

pub(super) fn to_gemini(
    input: &Value,
    ctx: &Context,
    ds: &mut Vec<Diagnostic>,
) -> Result<Value, Error> {
    validate(input)?;
    let mut out = json!({"generationConfig":{}});
    copy_renamed(
        input,
        &mut out["generationConfig"],
        &[
            ("temperature", "temperature"),
            ("top_p", "topP"),
            ("max_output_tokens", "maxOutputTokens"),
        ],
    );
    add_gemini_options(&mut out, input, ctx);
    if let Some(format) = response_format(&input["text"])? {
        chat::apply_response_format(
            &format,
            &mut out,
            ctx,
            ds,
            "text.format.schema",
            s(&input["model"]),
        );
    }
    let functions: Vec<_> = functions(input)?
        .iter()
        .map(chat::gemini_function)
        .collect();
    if !functions.is_empty() {
        out["tools"] = json!([{"functionDeclarations":functions}]);
    }
    if let Some(choice) = chat::gemini_choice(&chat_choice(&input["tool_choice"])?) {
        out["toolConfig"] = choice;
    }
    let mut systems = vec![];
    if !input["instructions"].is_null() {
        let text = string_value(&input["instructions"]);
        if !text.trim().is_empty() {
            systems.push(text);
        }
    }
    let nested_media = gemini_nested_tool_media(input, ctx);
    let mut names = BTreeMap::<String, String>::new();
    let mut contents = vec![];
    let mut leading_system = true;
    for item in input_items(&input["input"])?.iter() {
        let kind = s(&item["type"]).trim();
        if !matches!(s(&item["role"]), "system" | "developer") {
            leading_system = false;
        }
        match kind {
            "function_call" | "custom_tool_call" | "tool_search_call"
                if kind != "tool_search_call" || ctx.options.preserve_message_structure => {
                let name = resolved_call_name(item, ctx);
                if name.is_empty() {
                    return Err(Error::message(format!("{kind} item is missing name")));
                }
                let id = call_id(item);
                let args = if kind == "custom_tool_call" {
                    json!({"input":string_value(&item["input"])})
                } else if ctx.options.preserve_message_structure {
                    checked_tool_arguments(&item["arguments"])?
                } else {
                    object_value(&item["arguments"], "arguments")
                };
                let mut call =
                    owned_object([("name", Value::String(name.clone())), ("args", args)]);
                put_nonempty(&mut call, "id", id);
                let mut p = owned_object([("functionCall", call)]);
                if ctx.options.preserve_message_structure {
                    if let Some(signature) = item
                        .get("thought_signature")
                        .or_else(|| item.get("thoughtSignature"))
                    {
                        p["thoughtSignature"] = signature.clone();
                    }
                }
                attach_signature(&mut p, ctx);
                if !id.is_empty() {
                    names.insert(id.into(), name);
                }
                append_gemini(&mut contents, "model", p);
            }
            "function_call_output" | "custom_tool_call_output" | "tool_search_output"
                if kind != "tool_search_output" || ctx.options.preserve_message_structure => {
                let id = call_id(item);
                let name = if s(&item["name"]).trim().is_empty() {
                    names.get(id).cloned().unwrap_or_default()
                } else {
                    s(&item["name"]).trim().into()
                };
                let (output, mut media) = if ctx.options.preserve_message_structure {
                    preserved_tool_output_gemini(&item["output"], ctx, nested_media)?
                } else {
                    tool_output_gemini(&item["output"], ctx, nested_media, ds)
                };
                let mut response =
                    owned_object([("name", Value::String(name)), ("response", output)]);
                put_nonempty(&mut response, "id", id);
                if nested_media && !media.is_empty() {
                    response["parts"] = Value::Array(std::mem::take(&mut media));
                }
                append_gemini(
                    &mut contents,
                    "user",
                    owned_object([("functionResponse", response)]),
                );
                contents.last_mut().unwrap()["parts"]
                    .as_array_mut()
                    .unwrap()
                    .extend(media);
            }
            "reasoning" if ctx.options.preserve_message_structure => {
                let block = reasoning_from_item(item);
                if block["type"] == "redacted_thinking" {
                    return Err(Error::message(
                        "Gemini cannot replay Claude redacted thinking",
                    ));
                }
                let mut part = json!({"thought":true,"text":s(&block["thinking"])});
                put_nonempty(&mut part, "thoughtSignature", s(&block["signature"]));
                append_gemini(&mut contents, "model", part);
            }
            "item_reference" if ctx.options.preserve_message_structure => {
                return Err(Error::message(
                    "Responses item_reference cannot be resolved for a stateless upstream",
                ));
            }
            _ => {
                if ctx.options.preserve_message_structure && !matches!(kind, "" | "message") {
                    return Err(Error::message(format!(
                        "Responses input item {kind:?} cannot be represented by Gemini"
                    )));
                }
                let role = match s(&item["role"]).trim() {
                    "assistant" | "model" => "model",
                    "system" | "developer" => "system",
                    _ => "user",
                };
                let parts = gemini_content(&item["content"], ctx)?;
                if role == "system" {
                    if ctx.options.preserve_message_structure
                        && parts.iter().any(|part| !part["text"].is_string())
                    {
                        return Err(Error::message("system instructions can contain only text"));
                    }
                    if ctx.options.preserve_message_structure && !leading_system {
                        contents.push(json!({
                            "role":"user",
                            "parts":[{"text":format!(
                                "[{}]\n{}",
                                s(&item["role"]),
                                parts.iter().map(|part| s(&part["text"])).collect::<String>(),
                            )}],
                        }));
                    } else {
                        systems.extend(
                            parts
                                .iter()
                                .filter_map(|p| p["text"].as_str())
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned),
                        );
                    }
                } else if !parts.is_empty() {
                    contents.push(owned_object([
                        ("role", Value::String(role.into())),
                        ("parts", Value::Array(parts)),
                    ]));
                }
            }
        }
    }
    if !ctx.options.preserve_message_structure {
        omit_trailing(&mut contents, true, ds);
    }
    out["contents"] = if contents.is_empty() {
        Value::Null
    } else {
        Value::Array(contents)
    };
    if !systems.is_empty() {
        out["systemInstruction"] = json!({"parts":[{"text":systems.join("\n")}]});
    }
    Ok(out)
}
fn gemini_content(content: &Value, ctx: &Context) -> Result<Vec<Value>, Error> {
    let mut parts = vec![];
    for p in content_parts(content) {
        match s(&p["type"]).trim() {
            "input_text" | "output_text" | "text" => {
                let text = response_text_value(&p["text"]).into_owned();
                if !text.is_empty() {
                    parts.push(json!({"text":text}));
                }
            }
            "input_image" | "input_file" | "input_audio" | "input_video" => {
                if let Some(p) = gemini_media(&p, ctx, true)? {
                    parts.push(p);
                }
            }
            other if ctx.options.preserve_message_structure => {
                return Err(Error::message(format!(
                    "Responses content part {other:?} cannot be represented by Gemini"
                )));
            }
            _ => {}
        }
    }
    Ok(parts)
}
fn append_gemini(contents: &mut Vec<Value>, role: &str, part: Value) {
    if let Some(last) = contents.last_mut() {
        if last["role"] == role {
            let parts = last["parts"].as_array_mut().unwrap();
            let mut at = parts.len();
            if role == "model" && !part["functionCall"].is_null() {
                at = parts
                    .iter()
                    .take_while(|p| !p["functionCall"].is_null())
                    .count();
            } else if !part["functionResponse"].is_null() {
                if let Some(index) = parts.iter().rposition(|p| !p["functionResponse"].is_null()) {
                    at = index + 1;
                }
            }
            parts.insert(at, part);
            return;
        }
    }
    contents.push(owned_object([
        ("role", Value::String(role.into())),
        ("parts", Value::Array(vec![part])),
    ]));
}
fn tool_output_gemini(
    value: &Value,
    ctx: &Context,
    nested: bool,
    ds: &mut Vec<Diagnostic>,
) -> (Value, Vec<Value>) {
    let parts = arr(value);
    if parts.is_empty()
        || parts.iter().any(|p| {
            !matches!(
                s(&p["type"]).trim(),
                "input_text"
                    | "output_text"
                    | "text"
                    | "input_image"
                    | "input_file"
                    | "input_audio"
                    | "input_video"
            )
        })
    {
        return (response_map(value), vec![]);
    }
    let mut texts = vec![];
    let mut labels = vec![];
    let mut media = vec![];
    for p in parts {
        let kind = s(&p["type"]).trim();
        if matches!(kind, "input_text" | "output_text" | "text") {
            let text = response_text_value(&p["text"]).into_owned();
            if !text.is_empty() {
                texts.push(text);
            }
            continue;
        }
        let kind = kind.trim_start_matches("input_");
        let label = format!("[{kind}]");
        if !labels.contains(&label) {
            labels.push(label);
        }
        let Some((data, mime)) = source(p, true) else {
            continue;
        };
        let mut reason = format!("Gemini function responses cannot carry {kind} content");
        if !nested || matches!(kind, "image" | "file") {
            match ctx.resolve_media(&data, &mime) {
                Err(_) => reason = format!("the {kind} could not be loaded"),
                Ok((data, mime)) => {
                    let top = mime.split('/').next().unwrap_or("").to_lowercase();
                    if gemini_mime(&mime) && (!nested || !matches!(top.as_str(), "audio" | "video"))
                    {
                        media.push(json!({"inlineData":{"mimeType":mime,"data":data}}));
                        continue;
                    }
                    reason = format!("Gemini function responses cannot carry {mime} content");
                }
            }
        }
        diag(
            ds,
            Severity::Error,
            "unsupported_media_type",
            "input.output",
            format!("{reason}; the part was omitted"),
        );
    }
    if !texts.is_empty() {
        (response_map(&json!(texts.join("\n"))), media)
    } else if !media.is_empty() {
        (json!({}), media)
    } else {
        (response_map(&json!(labels.join(" "))), vec![])
    }
}
pub(super) fn preserved_tool_output_gemini(
    value: &Value,
    ctx: &Context,
    nested: bool,
) -> Result<(Value, Vec<Value>), Error> {
    if !value.is_array() {
        return Ok((response_map(value), vec![]));
    }
    let mut texts = vec![];
    let mut media = vec![];
    for part in arr(value) {
        let kind = s(&part["type"]).trim();
        match kind {
            "input_text" | "output_text" | "text" => {
                let text = response_text_value(&part["text"]).into_owned();
                if !text.is_empty() {
                    texts.push(text);
                }
            }
            "input_image" | "input_file" | "input_audio" | "input_video" => {
                if nested && matches!(kind, "input_audio" | "input_video") {
                    return Err(Error::message(format!(
                        "Gemini function responses cannot carry {kind} content"
                    )));
                }
                if let Some(part) = gemini_media(part, ctx, true)? {
                    media.push(part);
                }
            }
            other => {
                return Err(Error::message(format!(
                    "Responses tool-output content part {other:?} cannot be represented by Gemini"
                )));
            }
        }
    }
    Ok((
        if texts.is_empty() {
            json!({})
        } else {
            response_map(&json!(texts.join("\n")))
        },
        media,
    ))
}
