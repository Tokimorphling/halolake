use super::*;

pub(super) fn to_chat(
    input: &Value,
    ctx: &Context,
    ds: &mut Vec<Diagnostic>,
) -> Result<Value, Error> {
    let mut out = json!({"model":s(&input["model"])});
    copy_fields(
        input,
        &mut out,
        &["temperature", "max_tokens", "top_p", "top_k", "stream"],
    );
    if ctx.options.preserve_message_structure {
        if let Some(max_tokens) = out.as_object_mut().unwrap().remove("max_tokens") {
            out["max_completion_tokens"] = max_tokens;
        }
        let format = input["output_config"]
            .get("format")
            .or_else(|| input.get("output_format"));
        if let Some(format) = format {
            if format["type"] == "json_schema" {
                out["response_format"] = json!({
                    "type":"json_schema",
                    "json_schema":{"name":"response","strict":true,"schema":format["schema"]},
                });
            }
        }
    }
    preserve_thinking_suffix(&mut out, ctx);
    let stops = arr(&input["stop_sequences"]);
    if stops.len() == 1 {
        out["stop"] = stops[0].clone();
    } else if !stops.is_empty() {
        out["stop"] = json!(stops);
        out["__relaykit_stop_is_string_slice"] = json!(true);
    }
    let tools: Vec<_> = arr(&input["tools"])
        .iter()
        .map(|t| {
            let mut f = json!({"name":s(&t["name"])});
            put_nonempty(&mut f, "description", s(&t["description"]));
            if !t["input_schema"].is_null() {
                f["parameters"] = t["input_schema"].clone();
            }
            owned_object([("type", Value::String("function".into())), ("function", f)])
        })
        .collect();
    out["tools"] = Value::Array(tools);
    let mut messages = vec![];
    let system = &input["system"];
    if system.is_string() && !s(system).is_empty() {
        messages.push(json!({"role":"system","content":system}));
    } else if !arr(system).is_empty() {
        let content = if ctx.options.open_router_dialect
            && ctx.upstream_model().starts_with("anthropic/claude")
        {
            json!(
                arr(system)
                    .iter()
                    .map(|p| {
                        let mut p2 = json!({"type":"text"});
                        put_nonempty(&mut p2, "text", s(&p["text"]));
                        copy_fields(p, &mut p2, &["cache_control"]);
                        p2
                    })
                    .collect::<Vec<_>>()
            )
        } else {
            json!(
                arr(system)
                    .iter()
                    .map(|p| s(&p["text"]))
                    .collect::<String>()
            )
        };
        messages.push(owned_object([
            ("role", Value::String("system".into())),
            ("content", content),
        ]));
    }
    let mut names = BTreeMap::<String, String>::new();
    let mut unnamed = vec![];
    for (mi, msg) in arr(&input["messages"]).iter().enumerate() {
        let mut message = json!({"role":s(&msg["role"]),"content":null});
        if msg["content"].is_string() {
            message["content"] = msg["content"].clone();
        } else {
            let blocks = parse_blocks(&msg["content"])?;
            let mut calls = vec![];
            let mut media = vec![];
            for (bi, block) in blocks.iter().enumerate() {
                names
                    .entry(s(&block["id"]).into())
                    .or_insert_with(|| s(&block["name"]).into());
                if ctx.options.preserve_message_structure
                    && matches!(s(&block["type"]), "text" | "input_text" | "image" | "document")
                {
                    media.push(preserved_block_chat(block)?);
                    continue;
                }
                match s(&block["type"]) {
                    "thinking" | "redacted_thinking" if ctx.options.preserve_message_structure => {
                        add_reasoning_message(&mut message, block);
                    }
                    "text" | "input_text" => {
                        let mut p = json!({"type":"text"});
                        put_nonempty(&mut p, "text", s(&block["text"]));
                        copy_fields(block, &mut p, &["cache_control"]);
                        media.push(p);
                    }
                    "image" => {
                        let url = image_url(&block["source"]);
                        if url.is_empty() {
                            diag(
                                ds,
                                Severity::Error,
                                "unsupported_media_source",
                                format!("messages[{mi}].content[{bi}].source"),
                                format!(
                                    "OpenAI Chat Completions cannot reference a Claude image with source type {:?}; the image was omitted",
                                    s(&block["source"]["type"])
                                ),
                            );
                        } else {
                            media.push(json!({"type":"image_url","image_url":{"url":url}}));
                        }
                    }
                    "document" => {
                        let src = &block["source"];
                        if src["type"] == "text" {
                            let mut p = json!({"type":"text"});
                            put_nonempty(&mut p, "text", &string_value(&src["data"]));
                            media.push(p);
                            continue;
                        }
                        if src["type"] != "base64" || src["media_type"] != "application/pdf" {
                            diag(
                                ds,
                                Severity::Error,
                                "unsupported_media_source",
                                format!("messages[{mi}].content[{bi}].source"),
                                format!(
                                    "OpenAI Chat Completions carries only base64 PDF and plain-text documents, not a {:?} source of type {:?}; the document was omitted",
                                    s(&src["type"]),
                                    s(&src["media_type"])
                                ),
                            );
                            continue;
                        }
                        media.push(json!({"type":"file","file":{"filename":"document.pdf","file_data":format!("data:application/pdf;base64,{}",string_value(&src["data"]))}}));
                    }
                    "tool_use" => {
                        let mut f = json!({});
                        put_nonempty(&mut f, "name", s(&block["name"]));
                        f["arguments"] = json!(stringify(&block["input"]));
                        let mut call = owned_object([
                            ("type", Value::String("function".into())),
                            ("function", f),
                        ]);
                        put_nonempty(&mut call, "id", s(&block["id"]));
                        calls.push(call);
                    }
                    "tool_result" => {
                        let name = s(&block["name"]);
                        if name.is_empty() {
                            unnamed.push((messages.len(), s(&block["tool_use_id"]).to_owned()));
                        }
                        let content = if ctx.options.preserve_message_structure {
                            let (content, images) = split_chat_tool_content(
                                preserved_tool_result_chat(&block["content"])?
                            );
                            media.extend(images);
                            content
                        } else if block["content"].is_string() {
                            block["content"].clone()
                        } else {
                            let (text, images) = tool_result_chat(&block["content"]);
                            media.extend(images);
                            Value::String(text)
                        };
                        let mut tool = owned_object([
                            ("role", Value::String("tool".into())),
                            ("name", Value::String(name.into())),
                            ("content", content),
                        ]);
                        put_nonempty(&mut tool, "tool_call_id", s(&block["tool_use_id"]));
                        if ctx.options.preserve_message_structure {
                            copy_fields(block, &mut tool, &["is_error", "cache_control"]);
                        }
                        messages.push(tool);
                    }
                    other if ctx.options.preserve_message_structure => {
                        return Err(Error::message(format!(
                            "Claude content block {other:?} cannot be represented as a Chat message"
                        )));
                    }
                    _ => {}
                }
            }
            if !calls.is_empty() {
                message["tool_calls"] = Value::Array(calls);
            }
            if !media.is_empty() {
                message["content"] = Value::Array(media);
            }
        }
        if !chat_parts(&message["content"]).is_empty()
            || !arr(&message["tool_calls"]).is_empty()
            || ctx.options.preserve_message_structure && !reasoning_blocks(&message).is_empty()
        {
            messages.push(message);
        }
    }
    for (index, id) in unnamed {
        messages[index]["name"] = json!(names.get(&id).cloned().unwrap_or_default());
    }
    if !messages.is_empty() {
        if ctx.options.preserve_message_structure {
            normalize_chat_tool_turns(&mut messages);
        }
        out["messages"] = Value::Array(messages);
    }
    Ok(out)
}
fn parse_blocks(content: &Value) -> Result<&[Value], Error> {
    if content.is_null() {
        Ok(&[])
    } else if content.is_array() {
        Ok(arr(content))
    } else {
        Err(Error::message(
            "invalid Claude message content: expected array",
        ))
    }
}
fn image_url(src: &Value) -> String {
    if src.is_null() || src["type"] == "file" {
        return String::new();
    }
    if !s(&src["url"]).is_empty() {
        return s(&src["url"]).into();
    }
    let data = string_value(&src["data"]);
    if data.is_empty() {
        String::new()
    } else {
        format!("data:{};base64,{data}", s(&src["media_type"]))
    }
}
fn source_url(src: &Value) -> String {
    if !s(&src["url"]).trim().is_empty() {
        return s(&src["url"]).into();
    }
    let data = string_value(&src["data"]);
    if data.is_empty() {
        String::new()
    } else if data.starts_with("data:") {
        data
    } else {
        format!("data:{};base64,{data}", s(&src["media_type"]))
    }
}
pub(super) fn tool_result_chat(content: &Value) -> (String, Vec<Value>) {
    let blocks = arr(content);
    let mut texts = vec![];
    let mut media = vec![];
    for b in blocks {
        match s(&b["type"]) {
            "text" | "input_text" => {
                if !s(&b["text"]).is_empty() {
                    texts.push(s(&b["text"]));
                }
            }
            "image" => {
                let url = image_url(&b["source"]);
                if url.is_empty() {
                    return (stringify(content), vec![]);
                }
                media.push(json!({"type":"image_url","image_url":{"url":url}}));
            }
            _ => return (stringify(content), vec![]),
        }
    }
    if texts.is_empty() && media.is_empty() {
        (stringify(content), vec![])
    } else if texts.is_empty() {
        ("[image]".into(), media)
    } else {
        (texts.join("\n"), media)
    }
}
fn preserved_block_chat(block: &Value) -> Result<Value, Error> {
    let mut part = match s(&block["type"]) {
        "text" | "input_text" => json!({"type":"text","text":s(&block["text"])}),
        "image" => {
            let url = image_url(&block["source"]);
            if url.is_empty() {
                return Err(Error::message("Claude image source cannot be represented by Chat"));
            }
            json!({"type":"image_url","image_url":{"url":url}})
        }
        "document" => {
            let source = &block["source"];
            match s(&source["type"]) {
                "url" if !s(&source["url"]).is_empty() => {
                    json!({"type":"file","file":{"file_url":source["url"]}})
                }
                "base64" if source["media_type"] == "application/pdf" => {
                    json!({"type":"file","file":{
                        "filename":"document.pdf",
                        "file_data":source_url(source),
                    }})
                }
                "text" => json!({"type":"text","text":string_value(&source["data"])}),
                _ => {
                    return Err(Error::message(
                        "Claude document source cannot be represented by Chat",
                    ));
                }
            }
        }
        other => {
            return Err(Error::message(format!(
                "Claude tool-result content block {other:?} cannot be represented by Chat"
            )));
        }
    };
    copy_content_metadata(block, &mut part);
    Ok(part)
}
fn preserved_tool_result_chat(content: &Value) -> Result<Value, Error> {
    match content {
        Value::Null => Ok(json!("")),
        Value::String(_) => Ok(content.clone()),
        Value::Array(parts) => parts
            .iter()
            .map(preserved_block_chat)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        _ => Err(Error::message(
            "Claude tool-result content must be text or an array",
        )),
    }
}
fn preserved_tool_result_responses(content: &Value) -> Result<Value, Error> {
    let content = preserved_tool_result_chat(content)?;
    if content.is_string() {
        Ok(content)
    } else {
        chat::responses_parts("user", &content).map(Value::Array)
    }
}
fn preserve_thinking_suffix(out: &mut Value, ctx: &Context) {
    if !ctx.options.open_router_dialect
        && ctx.origin_model_name.ends_with("-thinking")
        && !s(&out["model"]).ends_with("-thinking")
    {
        out["model"] = json!(format!("{}-thinking", s(&out["model"])));
    }
}

pub(super) fn to_responses(input: &Value, ctx: &Context) -> Result<Value, Error> {
    if s(&input["model"]).trim().is_empty() {
        return Err(Error::message("model is required"));
    }
    let mut out = json!({"model":s(&input["model"])});
    copy_fields(
        input,
        &mut out,
        &["metadata", "service_tier", "stream", "temperature", "top_p"],
    );
    if ctx.options.preserve_message_structure {
        let format = input["output_config"]
            .get("format")
            .or_else(|| input.get("output_format"));
        if let Some(format) = format {
            if format["type"] == "json_schema" {
                out["text"]["format"] = json!({
                    "type":"json_schema",
                    "name":"response",
                    "strict":true,
                    "schema":format["schema"],
                });
            }
        }
    }
    preserve_thinking_suffix(&mut out, ctx);
    if !input["max_tokens"].is_null() {
        out["max_output_tokens"] = input["max_tokens"].clone();
    } else if !input["max_tokens_to_sample"].is_null() {
        out["max_output_tokens"] = input["max_tokens_to_sample"].clone();
    }
    let system = &input["system"];
    if system.is_string() {
        out["instructions"] = system.clone();
    } else if !system.is_null() {
        if !system.is_array() {
            return Err(Error::message(
                "invalid Claude system content: expected array",
            ));
        }
        let text = arr(system)
            .iter()
            .filter(|p| matches!(s(&p["type"]), "" | "text" | "input_text"))
            .map(|p| s(&p["text"]))
            .collect::<String>();
        if !text.is_empty() {
            out["instructions"] = Value::String(text);
        }
    }
    if !input["tools"].is_null() {
        if !input["tools"].is_array() {
            return Err(Error::message("invalid Claude tools: expected array"));
        }
        out["tools"]=json!(arr(&input["tools"]).iter().map(|t|{let mut f=json!({"type":"function","name":s(&t["name"]),"description":s(&t["description"]),"parameters":t["input_schema"]});copy_fields(t,&mut f,&["strict"]);f}).collect::<Vec<_>>());
    }
    if !input["tool_choice"].is_null() {
        let c = &input["tool_choice"];
        out["tool_choice"] = match s(&c["type"]) {
            "" | "auto" => json!("auto"),
            "any" => json!("required"),
            "none" => json!("none"),
            "tool" => json!({"type":"function","name":s(&c["name"])}),
            other => {
                return Err(Error::message(format!(
                    "unsupported Claude tool_choice type {other:?}"
                )));
            }
        };
        if c["disable_parallel_tool_use"] == true && c["type"] != "none" {
            out["parallel_tool_calls"] = json!(false);
        }
    }
    let mut items = vec![];
    for (mi, msg) in arr(&input["messages"]).iter().enumerate() {
        let role = s(&msg["role"]).trim();
        if role.is_empty() {
            continue;
        }
        if msg["content"].is_string() {
            if ctx.options.preserve_message_structure {
                items.push(json!({
                    "type":"message",
                    "role":if role == "system" { "developer" } else { role },
                    "content":[{
                        "type":if role == "assistant" { "output_text" } else { "input_text" },
                        "text":msg["content"],
                    }],
                }));
            } else {
                items.push(json!({"role":role,"content":msg["content"]}));
            }
            continue;
        }
        let blocks = parse_blocks(&msg["content"])
            .map_err(|e| Error::message(format!("messages[{mi}].content: {e}")))?;
        let mut parts = vec![];
        for b in blocks {
            if ctx.options.preserve_message_structure
                && matches!(s(&b["type"]), "thinking" | "redacted_thinking")
            {
                flush_preserved(&mut items, &mut parts, role, true);
                items.push(reasoning_item(b));
                continue;
            }
            if ctx.options.preserve_message_structure {
                match s(&b["type"]) {
                    "text" | "input_text" | "image" | "document" => {
                        let part = preserved_block_chat(b)?;
                        parts.extend(chat::responses_parts(role, &json!([part]))?);
                        continue;
                    }
                    "tool_use" => {
                        flush_preserved(&mut items, &mut parts, role, true);
                        items.push(json!({
                            "type":"function_call",
                            "call_id":s(&b["id"]),
                            "name":s(&b["name"]),
                            "arguments":if b["input"].is_null() {"{}".into()} else {stringify(&b["input"])},
                        }));
                        continue;
                    }
                    "tool_result" => {
                        flush_preserved(&mut items, &mut parts, role, true);
                        let mut item = json!({
                            "type":"function_call_output",
                            "call_id":s(&b["tool_use_id"]),
                            "output":preserved_tool_result_responses(&b["content"])?,
                        });
                        copy_fields(b, &mut item, &["is_error", "cache_control"]);
                        items.push(item);
                        continue;
                    }
                    other => {
                        return Err(Error::message(format!(
                            "Claude content block {other:?} cannot be represented as a Responses input"
                        )));
                    }
                }
            }
            match s(&b["type"]){
                "text"|"input_text"=>parts.push(json!({"type":if role=="assistant"{"output_text"}else{"input_text"},"text":s(&b["text"])})),
                "image"|"document"=>{let source=source_url(&b["source"]);if !source.is_empty(){parts.push(if b["type"]=="image"{json!({"type":"input_image","image_url":source,"detail":crate::media::DEFAULT_RESPONSES_IMAGE_DETAIL})}else{json!({"type":"input_file","file_data":source})});}},
                "tool_use"=>{flush(&mut items,&mut parts,role);let args=if b["input"].is_null(){"{}".into()}else{stringify(&b["input"])};items.push(json!({"type":"function_call","call_id":s(&b["id"]),"name":s(&b["name"]),"arguments":args}));},
                "tool_result"=>{flush(&mut items,&mut parts,role);items.push(json!({"type":"function_call_output","call_id":s(&b["tool_use_id"]),"output":tool_result_responses(&b["content"])}));},_=>{}
            }
        }
        flush_preserved(&mut items, &mut parts, role, ctx.options.preserve_message_structure);
    }
    out["input"] = Value::Array(items);
    Ok(out)
}
fn flush(items: &mut Vec<Value>, parts: &mut Vec<Value>, role: &str) {
    if !parts.is_empty() {
        items.push(owned_object([
            ("role", Value::String(role.into())),
            ("content", Value::Array(std::mem::take(parts))),
        ]));
    }
}
fn flush_preserved(items: &mut Vec<Value>, parts: &mut Vec<Value>, role: &str, preserve: bool) {
    flush(items, parts, role);
    if preserve {
        if let Some(item) = items.last_mut() {
            if item.get("role").is_some() {
                item["type"] = json!("message");
                if item["role"] == "system" {
                    item["role"] = json!("developer");
                }
            }
        }
    }
}
pub(super) fn tool_result_responses(content: &Value) -> Value {
    if content.is_null() {
        return json!("");
    }
    if !content.is_array() {
        return content.clone();
    }
    let mut parts = vec![];
    for b in arr(content) {
        match s(&b["type"]) {
            "text" | "input_text" => parts.push(json!({"type":"input_text","text":s(&b["text"])})),
            "image" | "document" => {
                let source = source_url(&b["source"]);
                if !source.is_empty() {
                    parts.push(if b["type"] == "image" {
                        json!({"type":"input_image","image_url":source,"detail":crate::media::DEFAULT_RESPONSES_IMAGE_DETAIL})
                    } else {
                        json!({"type":"input_file","file_data":source})
                    });
                }
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        content.clone()
    } else {
        Value::Array(parts)
    }
}
