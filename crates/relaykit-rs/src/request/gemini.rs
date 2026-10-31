use super::*;

pub(super) fn to_chat(
    input: &Value,
    ctx: &Context,
    ds: &mut Vec<Diagnostic>,
) -> Result<Value, Error> {
    let mut out = json!({"model":ctx.upstream_model(),"stream":ctx.is_stream});
    let mut messages = vec![];
    let contents = arr(&input["contents"]);
    let mut reserved = BTreeSet::<String>::new();
    let mut pending = vec![];
    let mut next = 1;
    for c in contents {
        for p in arr(&c["parts"]) {
            for k in ["functionCall", "functionResponse"] {
                let id = string_value(&p[k]["id"]);
                if !id.is_empty() {
                    reserved.insert(id);
                }
            }
        }
    }
    for (ci, content) in contents.iter().enumerate() {
        let role = match s(&content["role"]) {
            "model" => "assistant",
            "function" => "function",
            _ => "user",
        };
        let mut message = json!({"role":role,"content":null});
        let mut media = vec![];
        let mut calls = vec![];
        let mut thoughts = vec![];
        for (pi, p) in arr(&content["parts"]).iter().enumerate() {
            if ctx.options.preserve_message_structure
                && p["functionCall"].is_null()
                && !s(&p["thoughtSignature"]).is_empty()
            {
                message["reasoning_signature"] = p["thoughtSignature"].clone();
            }
            if !s(&p["text"]).is_empty() {
                if p["thought"] == true {
                    if ctx.options.preserve_message_structure {
                        let mut block = json!({"type":"thinking","thinking":p["text"]});
                        put_nonempty(&mut block, "signature", s(&p["thoughtSignature"]));
                        add_reasoning_message(&mut message, &block);
                    } else {
                        thoughts.push(s(&p["text"]));
                    }
                } else {
                    media.push(json!({"type":"text","text":p["text"]}));
                }
            } else if !p["inlineData"].is_null() || !p["fileData"].is_null() {
                let result = if ctx.options.preserve_message_structure {
                    preserved_media_to_chat(p)
                } else {
                    media_to_chat(p)
                };
                match result {
                    Ok(v) => media.push(v),
                    Err(reason) if ctx.options.preserve_message_structure => {
                        return Err(Error::message(reason));
                    }
                    Err(reason) => diag(
                        ds,
                        Severity::Error,
                        "unsupported_media_type",
                        format!(
                            "contents[{ci}].parts[{pi}].{}",
                            if !p["inlineData"].is_null() {
                                "inlineData"
                            } else {
                                "fileData"
                            }
                        ),
                        format!("{reason}; the part was omitted"),
                    ),
                }
            } else if !p["functionCall"].is_null() {
                let fc = &p["functionCall"];
                let mut id = s(&fc["id"]).to_owned();
                if id.is_empty() {
                    id = fallback_id(&mut reserved, &mut next);
                }
                pending.push((id.clone(), s(&fc["name"]).to_owned()));
                let mut function = json!({"arguments":if fc["args"].is_null(){"null".into()}else{stringify(&fc["args"])}});
                put_nonempty(&mut function, "name", s(&fc["name"]));
                let mut call = json!({"id":id,"type":"function","function":function});
                if ctx.options.preserve_message_structure && !s(&p["thoughtSignature"]).is_empty() {
                    call["thoughtSignature"] = p["thoughtSignature"].clone();
                }
                calls.push(call);
            } else if !p["functionResponse"].is_null() {
                let response = &p["functionResponse"];
                let mut id = string_value(&response["id"]);
                let found = if !id.is_empty() {
                    pending.iter().position(|(p, _)| p == &id)
                } else {
                    pending.iter().position(|(_, name)| {
                        s(&response["name"]).is_empty() || name == s(&response["name"])
                    })
                };
                if let Some(index) = found {
                    let (call, _) = pending.remove(index);
                    if id.is_empty() {
                        id = call;
                    }
                }
                if id.is_empty() {
                    id = fallback_id(&mut reserved, &mut next);
                }
                let mut output = json!(stringify(&response["response"]));
                if ctx.options.preserve_message_structure && response["parts"].is_array() {
                    let mut parts = vec![];
                    let payload = &response["response"];
                    if payload.as_object().is_none_or(|value| !value.is_empty()) {
                        let text = if payload.as_object().is_some_and(|value| value.len() == 1)
                            && payload["content"].is_string()
                        {
                            s(&payload["content"]).to_owned()
                        } else {
                            stringify(payload)
                        };
                        parts.push(json!({"type":"text","text":text}));
                    }
                    for part in arr(&response["parts"]) {
                        if part["text"].is_string() {
                            parts.push(json!({"type":"text","text":part["text"]}));
                        } else if !part["inlineData"].is_null() || !part["fileData"].is_null() {
                            parts.push(preserved_media_to_chat(part).map_err(Error::message)?);
                        } else {
                            return Err(Error::message(
                                "Gemini function-response content cannot be represented by Chat",
                            ));
                        }
                    }
                    output = Value::Array(parts);
                }
                if ctx.options.preserve_message_structure {
                    let (text, attachments) = split_chat_tool_content(output);
                    output = text;
                    media.extend(attachments);
                }
                messages.push(json!({"role":"tool","tool_call_id":id,"content":output}));
            } else if ctx.options.preserve_message_structure
                && p["thought"] == true
                && !s(&p["thoughtSignature"]).is_empty()
            {
                add_reasoning_message(
                    &mut message,
                    &json!({"type":"thinking","thinking":"","signature":p["thoughtSignature"]}),
                );
            }
        }
        if !calls.is_empty() {
            message["tool_calls"] = Value::Array(calls);
        }
        if media.len() == 1 && media[0]["type"] == "text" {
            message["content"] = json!(s(&media[0]["text"]));
        } else if !media.is_empty() {
            message["content"] = Value::Array(media);
        }
        if !thoughts.is_empty() {
            message["reasoning_content"] = json!(thoughts.join("\n"));
        }
        if ctx.options.preserve_message_structure
            && !s(&message["reasoning_signature"]).is_empty()
            && !message["reasoning_blocks"].is_array()
        {
            message["reasoning_blocks"] = json!([{
                "type":"thinking",
                "thinking":s(&message["reasoning_content"]),
                "signature":message["reasoning_signature"],
            }]);
        }
        if !chat_parts(&message["content"]).is_empty()
            || !arr(&message["tool_calls"]).is_empty()
            || !message["reasoning_content"].is_null()
            || ctx.options.preserve_message_structure && !s(&message["reasoning_signature"]).is_empty()
        {
            messages.push(message);
        }
    }
    if !input["systemInstruction"].is_null() {
        let text = arr(&input["systemInstruction"]["parts"])
            .iter()
            .map(|p| s(&p["text"]))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        messages.insert(0, json!({"role":"system","content":text}));
    }
    if !messages.is_empty() {
        out["messages"] = Value::Array(messages);
    }
    let cfg = &input["generationConfig"];
    copy_renamed(
        cfg,
        &mut out,
        &[
            ("temperature", "temperature"),
            ("topP", "top_p"),
            ("maxOutputTokens", "max_tokens"),
            ("candidateCount", "n"),
        ],
    );
    if ctx.options.preserve_message_structure {
        if let Some(max_tokens) = out.as_object_mut().unwrap().remove("max_tokens") {
            out["max_completion_tokens"] = max_tokens;
        }
    }
    if let Some(top_k) = cfg["topK"].as_f64() {
        out["top_k"] = json!(top_k as i64);
    }
    let stops = arr(&cfg["stopSequences"]);
    if !stops.is_empty() {
        out["stop"] = json!(&stops[..stops.len().min(4)]);
        out["__relaykit_stop_is_string_slice"] = json!(true);
    }
    let mut tools = vec![];
    for tool in arr(&input["tools"]) {
        for function in arr(&tool["functionDeclarations"]) {
            let mut f = json!({});
            put_nonempty(&mut f, "name", s(&function["name"]));
            put_nonempty(&mut f, "description", s(&function["description"]));
            if !function["parameters"].is_null() {
                f["parameters"] = function["parameters"].clone();
            }
            tools.push(json!({"type":"function","function":f}));
        }
    }
    if !tools.is_empty() {
        out["tools"] = Value::Array(tools);
    }
    Ok(out)
}
fn fallback_id(reserved: &mut BTreeSet<String>, next: &mut u64) -> String {
    loop {
        let id = format!("call_{next}");
        *next += 1;
        if reserved.insert(id.clone()) {
            return id;
        }
    }
}
fn media_to_chat(part: &Value) -> Result<Value, String> {
    let inline = &part["inlineData"];
    let file = &part["fileData"];
    if inline.is_null() {
        let mime = normalized_mime(s(&file["mimeType"]));
        if !mime.is_empty() && !image_mime(&mime) {
            return Err(format!(
                "Chat Completions content cannot reference a Gemini file of MIME type {:?}",
                s(&file["mimeType"])
            ));
        }
        return Ok(
            json!({"type":"image_url","image_url":{"url":s(&file["fileUri"]),"detail":"auto"}}),
        );
    }
    let raw = s(&inline["mimeType"]);
    let mime = normalized_mime(raw);
    let data = s(&inline["data"]);
    if image_mime(&mime) {
        return Ok(
            json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{data}"),"detail":"auto"}}),
        );
    }
    if mime.starts_with("image/") {
        return Err(format!(
            "Chat Completions content accepts only PNG, JPEG, WebP, and GIF images, not {raw:?}"
        ));
    }
    if mime == "application/pdf" {
        return Ok(
            json!({"type":"file","file":{"filename":"document.pdf","file_data":format!("data:application/pdf;base64,{data}")}}),
        );
    }
    if is_text_mime(&mime) {
        let text = crate::media::decode_text(data).ok_or_else(|| {
            format!("Gemini inlineData of MIME type {raw:?} could not be decoded as UTF-8 text")
        })?;
        let mut part = json!({"type":"text"});
        put_nonempty(&mut part, "text", &text);
        return Ok(part);
    }
    let format = match mime.as_str() {
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/mp3" | "audio/mpeg" => "mp3",
        _ => {
            return Err(format!(
                "Gemini inlineData of MIME type {raw:?} has no Chat Completions content part"
            ));
        }
    };
    Ok(json!({"type":"input_audio","input_audio":{"data":data,"format":format}}))
}
fn preserved_media_to_chat(part: &Value) -> Result<Value, String> {
    let file = &part["fileData"];
    if part["inlineData"].is_null() && !file.is_null() {
        let mime = normalized_mime(s(&file["mimeType"]));
        if mime == "application/pdf" || is_text_mime(&mime) {
            let url = s(&file["fileUri"]);
            if url.is_empty() {
                return Err("Gemini fileData requires a fileUri".into());
            }
            return Ok(json!({
                "type":"file",
                "mime_type":mime,
                "file":{"file_url":url},
            }));
        }
    }
    media_to_chat(part)
}
