use super::*;

pub(super) fn to_responses(input: &Value, ctx: &Context) -> Result<Value, Error> {
    model_required(input)?;
    if input["n"].as_i64().unwrap_or(1) > 1 {
        return Err(Error::message(
            "n>1 is not supported in responses compatibility mode",
        ));
    }
    let mut items = Vec::new();
    let mut instructions = Vec::new();
    for msg in arr(&input["messages"]) {
        let role = s(&msg["role"]).trim();
        if role.is_empty() {
            continue;
        }
        let content = &msg["content"];
        if matches!(role, "tool" | "function") {
            let id = s(&msg["tool_call_id"]).trim();
            let output = if ctx.options.preserve_message_structure && content.is_array() {
                Value::Array(responses_parts("user", content)?)
            } else {
                Value::String(string_value(content))
            };
            let mut item = if id.is_empty() {
                json!({"role":"user","content":format!("[tool_output_missing_call_id] {}", string_value(&output))})
            } else {
                json!({"type":"function_call_output","call_id":id,"output":output})
            };
            if ctx.options.preserve_message_structure {
                copy_fields(msg, &mut item, &["is_error", "cache_control"]);
            }
            items.push(item);
            continue;
        }
        if matches!(role, "system" | "developer") && !ctx.options.preserve_message_structure {
            let text = if content.is_string() {
                s(content).trim().to_string()
            } else {
                chat_parts(content)
                    .iter()
                    .filter(|p| s(&p["type"]) == "text" && !s(&p["text"]).trim().is_empty())
                    .map(|p| s(&p["text"]))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_string()
            };
            if !text.is_empty() {
                instructions.push(text);
            }
            continue;
        }
        if role == "assistant" && ctx.options.preserve_message_structure {
            items.extend(reasoning_blocks(msg).iter().map(reasoning_item));
        }
        let content = if ctx.options.preserve_message_structure {
            Value::Array(responses_parts(role, content)?)
        } else if content.is_null() {
            json!("")
        } else if content.is_string() {
            content.clone()
        } else {
            json!(chat_parts(content).iter().map(|p|match s(&p["type"]) {
                "text"=>json!({"type":if role=="assistant"{"output_text"}else{"input_text"},"text":p["text"]}),
                "image_url"=>json!({"type":"input_image","image_url":if !s(&p["image_url"]["url"]).is_empty(){p["image_url"]["url"].clone()}else{p["image_url"].clone()},"detail":crate::media::responses_image_detail(s(&p["image_url"]["detail"]))}),
                "input_audio"=>json!({"type":"input_audio","input_audio":p["input_audio"]}),
                "file"=>{let mut f=json!({"type":"input_file"}); for k in ["filename","file_data","file_id"]{put_nonempty(&mut f,k,s(&p["file"][k]));}f},
                "video_url"=>json!({"type":"input_video","video_url":p["video_url"]}),
                _=>json!({"type":p["type"]}),
            }).collect::<Vec<_>>())
        };
        if !ctx.options.preserve_message_structure || role != "assistant" || !arr(&content).is_empty() {
            let mut item = json!({
                "role": if ctx.options.preserve_message_structure && role == "system" { "developer" } else { role },
                "content":content,
            });
            if ctx.options.preserve_message_structure {
                item["type"] = json!("message");
            }
            items.push(item);
        }
        if role == "assistant" {
            for call in arr(&msg["tool_calls"]) {
                let name = s(&call["function"]["name"]).trim();
                if s(&call["id"]).trim().is_empty()
                    || !matches!(s(&call["type"]), "" | "function")
                    || name.is_empty()
                {
                    continue;
                }
                let mut item = json!({"type":"function_call","call_id":call["id"],"name":name,"arguments":s(&call["function"]["arguments"])});
                if ctx.options.preserve_message_structure {
                    let signature = call
                        .get("thought_signature")
                        .or_else(|| call.get("thoughtSignature"));
                    if let Some(signature) = signature {
                        item["thought_signature"] = signature.clone();
                    }
                }
                items.push(item);
            }
        }
    }
    let mut out = json!({"model":input["model"],"input":items});
    if !instructions.is_empty() {
        out["instructions"] = json!(instructions.join("\n\n"));
    }
    copy_fields(
        input,
        &mut out,
        &[
            "stream",
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
            "user",
            "parallel_tool_calls",
            "store",
            "metadata",
            "enable_thinking",
            "thinking_budget",
        ],
    );
    put_nonempty(&mut out, "prompt_cache_key", s(&input["prompt_cache_key"]));
    if !input["max_completion_tokens"].is_null() {
        out["max_output_tokens"] = input["max_completion_tokens"].clone();
    } else if !input["max_tokens"].is_null() {
        out["max_output_tokens"] = input["max_tokens"].clone();
    }
    if !input["tools"].is_null() {
        out["tools"]=json!(arr(&input["tools"]).iter().map(|t|if s(&t["type"])=="function"{json!({"type":"function","name":s(&t["function"]["name"]),"description":s(&t["function"]["description"]),"parameters":t["function"]["parameters"]})}else{t.clone()}).collect::<Vec<_>>());
    }
    if !input["tool_choice"].is_null() {
        let choice = &input["tool_choice"];
        out["tool_choice"] = if s(&choice["type"]) == "function" {
            let name = if !s(&choice["name"]).is_empty() {
                s(&choice["name"])
            } else {
                s(&choice["function"]["name"])
            };
            if name.is_empty() {
                choice.clone()
            } else {
                json!({"type":"function","name":name})
            }
        } else {
            choice.clone()
        };
    }
    let format = &input["response_format"];
    if !s(&format["type"]).trim().is_empty() {
        let mut target = json!({"type":format["type"]});
        if s(&format["type"]) == "json_schema" {
            if let Some(schema) = format["json_schema"].as_object() {
                for (k, v) in schema {
                    if k != "type" {
                        target[k] = v.clone();
                    }
                }
                if let Some(nested) = target["json_schema"].as_object().cloned() {
                    for (k, v) in nested {
                        if target.get(&k).is_none() {
                            target[&k] = v;
                        }
                    }
                    target.as_object_mut().unwrap().remove("json_schema");
                }
            }
        }
        out["text"] = json!({"format":target});
    }
    Ok(out)
}

pub(super) fn responses_parts(role: &str, content: &Value) -> Result<Vec<Value>, Error> {
    if content.is_null() {
        return Ok(vec![]);
    }
    if let Some(text) = content.as_str() {
        return Ok(if text.is_empty() {
            vec![]
        } else {
            vec![json!({
                "type": if role == "assistant" { "output_text" } else { "input_text" },
                "text":text,
            })]
        });
    }
    let parts = content
        .as_array()
        .ok_or_else(|| Error::message("Chat message content must be text or an array"))?;
    parts
        .iter()
        .map(|part| {
            let mut output = match s(&part["type"]) {
                "text" => json!({
                    "type":if role == "assistant" { "output_text" } else { "input_text" },
                    "text":s(&part["text"]),
                }),
                "image_url" if role != "assistant" => {
                    let image = &part["image_url"];
                    let mut out = json!({"type":"input_image"});
                    if let Some(url) = image.as_str() {
                        out["image_url"] = json!(url);
                    } else {
                        if let Some(url) = image.get("url") {
                            out["image_url"] = url.clone();
                        }
                        copy_fields(image, &mut out, &["file_id", "detail"]);
                    }
                    out
                }
                "file" if role != "assistant" => {
                    let mut out = json!({"type":"input_file"});
                    copy_fields(
                        &part["file"],
                        &mut out,
                        &["filename", "file_data", "file_url", "file_id"],
                    );
                    out
                }
                "input_audio" if role != "assistant" => {
                    let mut out = json!({"type":"input_audio"});
                    copy_fields(&part["input_audio"], &mut out, &["data", "format"]);
                    out
                }
                "input_text" | "output_text" | "input_image" | "input_file" => part.clone(),
                "refusal" if role == "assistant" => part.clone(),
                other => {
                    return Err(Error::message(format!(
                        "Chat content part {other:?} cannot be represented as a Responses input"
                    )));
                }
            };
            copy_content_metadata(part, &mut output);
            copy_fields(part, &mut output, &["mime_type"]);
            Ok(output)
        })
        .collect()
}

pub(super) fn to_claude(
    input: &Value,
    ctx: &Context,
    ds: &mut Vec<Diagnostic>,
) -> Result<Value, Error> {
    let mut out = json!({"model":s(&input["model"])});
    copy_fields(input, &mut out, &["temperature", "top_p", "top_k"]);
    if input["stream"] == true {
        out["stream"] = json!(true);
    }
    for key in ["max_completion_tokens", "max_tokens"] {
        if input[key].as_u64().unwrap_or(0) > 0 {
            out["max_tokens"] = input[key].clone();
            break;
        }
    }
    match &input["stop"] {
        Value::String(v) => out["stop_sequences"] = json!([v]),
        Value::Array(values) => {
            let stops: Vec<_> = values.iter().filter_map(Value::as_str).collect();
            if !stops.is_empty() {
                out["stop_sequences"] = json!(stops);
            }
        }
        _ => {}
    }
    let tools:Vec<_>=arr(&input["tools"]).iter().filter(|t|t["function"]["parameters"].is_object()||s(&t["type"])=="function").map(|t|{let mut tool=json!({"name":s(&t["function"]["name"]),"input_schema":function_schema(&t["function"]["parameters"])});put_nonempty(&mut tool,"description",s(&t["function"]["description"]));tool}).collect();
    if !tools.is_empty() {
        out["tools"] = Value::Array(tools);
    }
    if !input["tool_choice"].is_null() || !input["parallel_tool_calls"].is_null() {
        if let Some(choice) = claude_choice(&input["tool_choice"], &input["parallel_tool_calls"]) {
            out["tool_choice"] = choice;
        }
    }
    let mut formatted: Vec<Value> = vec![];
    let mut last_message_has_placeholder = false;
    for msg in arr(&input["messages"]) {
        let role = match s(&msg["role"]) {
            "developer" => "system",
            "function" | "tool" if !s(&msg["tool_call_id"]).is_empty() => "tool",
            "system" => "system",
            "assistant" => "assistant",
            _ => "user",
        };
        let mut current = json!({"role":role,"content":msg["content"]});
        if role == "system" && ctx.options.preserve_message_structure {
            preserved_system_text(&msg["content"])?;
            current["__relaykit_source_role"] = msg["role"].clone();
        }
        if role == "tool" {
            current["tool_call_id"] = msg["tool_call_id"].clone();
            if ctx.options.preserve_message_structure {
                copy_fields(msg, &mut current, &["is_error", "cache_control"]);
            }
        }
        if role == "assistant" && !msg["tool_calls"].is_null() {
            current["tool_calls"] = msg["tool_calls"].clone();
        }
        if role == "assistant" && ctx.options.preserve_message_structure {
            let blocks = reasoning_blocks(msg);
            if !blocks.is_empty() {
                current["reasoning_blocks"] = Value::Array(blocks);
            }
        }
        if let Some(last) = formatted.last() {
            if role != "tool"
                && (!ctx.options.preserve_message_structure || role != "system")
                && s(&last["role"]) == role
                && (ctx.options.preserve_message_structure
                    || last["content"].is_string() && msg["content"].is_string())
                && (!ctx.options.preserve_message_structure
                    || arr(&last["reasoning_blocks"]).is_empty()
                        && arr(&current["reasoning_blocks"]).is_empty())
            {
                let previous_calls = arr(&last["tool_calls"]);
                let current_calls = arr(&current["tool_calls"]);
                if !previous_calls.is_empty() || !current_calls.is_empty() {
                    let mut texts = Vec::with_capacity(2);
                    let is_tool_placeholder =
                        last_message_has_placeholder && !previous_calls.is_empty();
                    if !s(&last["content"]).is_empty() && !is_tool_placeholder {
                        texts.push(s(&last["content"]));
                    }
                    if !s(&msg["content"]).is_empty() {
                        texts.push(s(&msg["content"]));
                    }
                    let calls = previous_calls
                        .iter()
                        .chain(current_calls)
                        .cloned()
                        .collect();
                    current["content"] = json!(texts.join(" "));
                    current["tool_calls"] = Value::Array(calls);
                } else {
                    current["content"] = if ctx.options.preserve_message_structure {
                        merge_content(&last["content"], &msg["content"])
                    } else {
                        json!(format!("{} {}", s(&last["content"]), s(&msg["content"])))
                    };
                }
                formatted.pop();
            }
        }
        let has_placeholder = (current["content"].is_null() || current["content"] == "")
            && (!ctx.options.preserve_message_structure
                || arr(&current["tool_calls"]).is_empty()
                    && arr(&current["reasoning_blocks"]).is_empty());
        if has_placeholder {
            current["content"] = json!("...");
        }
        formatted.push(current);
        last_message_has_placeholder = has_placeholder;
    }
    let mut messages: Vec<Value> = vec![];
    let mut system = vec![];
    for msg in formatted {
        let role = s(&msg["role"]);
        if role == "system"
            && (!ctx.options.preserve_message_structure || messages.is_empty())
        {
            if msg["content"].is_string() {
                system.push(json!({"type":"text","text":msg["content"]}));
            } else {
                let parts = if ctx.options.preserve_message_structure {
                    preserved_chat_parts(&msg["content"])?
                } else {
                    chat_parts(&msg["content"])
                };
                for p in parts {
                    if s(&p["type"]) == "text" && !s(&p["text"]).is_empty() {
                        system.push(p);
                    }
                }
            }
            continue;
        }
        if role == "system" && ctx.options.preserve_message_structure {
            let source_role = s(&msg["__relaykit_source_role"]);
            append_claude(
                &mut messages,
                "user",
                json!({"type":"text","text":format!("[{source_role}]\n{}", text_content(&msg["content"]))}),
            );
            continue;
        }
        if messages.is_empty() && role != "user" {
            messages.push(json!({"role":"user","content":[{"type":"text","text":"..."}]}));
        }
        if role == "tool" {
            let content = if ctx.options.preserve_message_structure && msg["content"].is_array() {
                Value::Array(claude_parts(&msg["content"], ctx, ds)?)
            } else {
                msg["content"].clone()
            };
            let mut block =
                json!({"type":"tool_result","tool_use_id":msg["tool_call_id"],"content":content});
            if ctx.options.preserve_message_structure {
                copy_fields(&msg, &mut block, &["is_error", "cache_control"]);
            }
            append_claude(&mut messages, "user", block);
            continue;
        }
        if msg["content"].is_string()
            && msg["tool_calls"].is_null()
            && !ctx.options.preserve_message_structure
        {
            messages.push(json!({"role":role,"content":msg["content"]}));
            continue;
        }
        let mut parts = if ctx.options.preserve_message_structure {
            reasoning_blocks(&msg)
        } else {
            vec![]
        };
        parts.extend(claude_parts(&msg["content"], ctx, ds)?);
        for call in arr(&msg["tool_calls"]) {
            let args = s(&call["function"]["arguments"]);
            let parsed = if ctx.options.preserve_message_structure {
                serde_json::from_str::<Value>(args)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}))
            } else {
                serde_json::from_str::<Value>(args)
                    .ok()
                    .filter(|v| v.is_object() || v.is_null())
                    .unwrap_or(json!({}))
            };
            let mut block = json!({"type":"tool_use"});
            put_nonempty(&mut block, "id", s(&call["id"]));
            put_nonempty(&mut block, "name", s(&call["function"]["name"]));
            if !parsed.is_null() {
                block["input"] = parsed;
            }
            parts.push(block);
        }
        if parts.is_empty() {
            parts.push(json!({"type":"text","text":"..."}));
        }
        messages.push(owned_object([
            ("role", Value::String(role.into())),
            ("content", Value::Array(parts)),
        ]));
    }
    if messages.is_empty() && !system.is_empty() {
        messages.push(json!({"role":"user","content":[{"type":"text","text":"..."}]}));
    }
    if !system.is_empty() {
        out["system"] = json!(system);
    }
    if !messages.is_empty() {
        out["messages"] = Value::Array(messages);
    }
    Ok(out)
}

fn merge_content(left: &Value, right: &Value) -> Value {
    match (left, right) {
        (Value::String(left), Value::String(right)) => {
            if left.is_empty() {
                json!(right)
            } else if right.is_empty() {
                json!(left)
            } else {
                json!(format!("{left} {right}"))
            }
        }
        (Value::Null, value) => value.clone(),
        (value, Value::Null) => value.clone(),
        (Value::String(left), value) if left.is_empty() => value.clone(),
        (value, Value::String(right)) if right.is_empty() => value.clone(),
        _ => Value::Array(
            chat_parts(left)
                .into_iter()
                .chain(chat_parts(right))
                .collect(),
        ),
    }
}

fn claude_parts(
    content: &Value,
    ctx: &Context,
    ds: &mut Vec<Diagnostic>,
) -> Result<Vec<Value>, Error> {
    let mut parts = vec![];
    let input = if ctx.options.preserve_message_structure {
        preserved_chat_parts(content)?
    } else {
        chat_parts(content)
    };
    for p in input {
            let kind = s(&p["type"]);
            if kind == "text" {
                if !s(&p["text"]).is_empty() {
                    parts.push(p);
                }
                continue;
            }
            if ctx.options.preserve_message_structure
                && !matches!(kind, "image_url" | "file" | "input_audio" | "video_url")
            {
                return Err(Error::message(format!(
                    "Chat content part {kind:?} cannot be represented by Claude"
                )));
            }
            let Some((data, mime)) = source(&p, ctx.options.preserve_message_structure) else {
                if ctx.options.preserve_message_structure {
                    return Err(Error::message(format!(
                        "Chat {kind} content requires inline data or a resolvable URL"
                    )));
                }
                continue;
            };
            if ctx.options.preserve_message_structure
                && is_http_url(&data)
                && matches!(kind, "image_url" | "file")
            {
                let mut block = claude_url_part(kind, &data);
                copy_content_metadata(&p, &mut block);
                parts.push(block);
                continue;
            }
            let result = if matches!(kind, "input_audio" | "video_url") {
                Err(format!("Claude Messages cannot carry {kind} content"))
            } else {
                let (data, mime) = ctx
                    .resolve_media(&data, &mime)
                    .map_err(|e| Error::message(format!("get file data failed: {e}")))?;
                claude_media(&data, &mime, kind == "image_url")
            };
            match result {
                Ok(mut block) => {
                    if ctx.options.preserve_message_structure {
                        copy_content_metadata(&p, &mut block);
                    }
                    parts.push(block);
                }
                Err(reason) if ctx.options.preserve_message_structure => {
                    return Err(Error::message(reason));
                }
                Err(reason) => diag(
                    ds,
                    Severity::Error,
                    "unsupported_media_type",
                    "messages.content",
                    format!("{reason}; the part was omitted"),
                ),
            }
    }
    Ok(parts)
}

pub(super) fn claude_choice(choice: &Value, parallel: &Value) -> Option<Value> {
    let mut out = match choice {
        Value::String(v) => match v.as_str() {
            "auto" => json!({"type":"auto"}),
            "required" => json!({"type":"any"}),
            "none" => json!({"type":"none"}),
            _ => return None,
        },
        Value::Object(_) if s(&choice["type"]) == "function" => {
            json!({"type":"tool","name":s(&choice["function"]["name"])})
        }
        Value::Null if parallel.is_boolean() => json!({"type":"auto"}),
        _ => return None,
    };
    if let Some(parallel) = parallel.as_bool() {
        if out["type"] != "none" {
            out["disable_parallel_tool_use"] = json!(!parallel);
        }
    }
    Some(out)
}
pub(super) fn append_claude(messages: &mut Vec<Value>, role: &str, part: Value) {
    if let Some(last) = messages.last_mut() {
        if last["role"] == role {
            let mut parts = if last["content"].is_string() {
                if s(&last["content"]).is_empty() {
                    vec![]
                } else {
                    vec![json!({"type":"text","text":last["content"]})]
                }
            } else {
                arr(&last["content"]).to_vec()
            };
            parts.push(part);
            last["content"] = Value::Array(parts);
            return;
        }
    }
    messages.push(owned_object([
        ("role", Value::String(role.into())),
        ("content", Value::Array(vec![part])),
    ]));
}

pub(super) fn to_gemini(
    input: &Value,
    ctx: &Context,
    ds: &mut Vec<Diagnostic>,
) -> Result<Value, Error> {
    let mut out = json!({"contents":[],"generationConfig":{}});
    copy_renamed(
        input,
        &mut out["generationConfig"],
        &[
            ("temperature", "temperature"),
            ("imageConfig", "imageConfig"),
            ("top_p", "topP"),
            ("seed", "seed"),
        ],
    );
    let max = if !input["max_completion_tokens"].is_null() {
        &input["max_completion_tokens"]
    } else {
        &input["max_tokens"]
    };
    if !max.is_null() {
        out["generationConfig"]["maxOutputTokens"] = max.clone();
    }
    let stops: Vec<_> = if input["stop"].is_string() {
        if s(&input["stop"]).is_empty() {
            vec![]
        } else {
            vec![input["stop"].clone()]
        }
    } else {
        arr(&input["stop"])
            .iter()
            .filter(|s| s.is_string() && !s.as_str().unwrap().is_empty())
            .take(5)
            .cloned()
            .collect()
    };
    if !stops.is_empty() {
        out["generationConfig"]["stopSequences"] = json!(stops);
    }
    extra_body(input, &mut out)?;
    add_gemini_options(&mut out, input, ctx);
    apply_response_format(
        &input["response_format"],
        &mut out,
        ctx,
        ds,
        "response_format.json_schema.schema",
        s(&input["model"]),
    );
    let functions: Vec<_> = arr(&input["tools"])
        .iter()
        .map(|t| gemini_function(&t["function"]))
        .collect();
    if !functions.is_empty() {
        out["tools"] = json!([{"functionDeclarations":functions}]);
    } else if !input["tools"].is_null() {
        out["tools"] = json!([]);
    }
    if !input["tools"].is_null() {
        if let Some(choice) = gemini_choice(&input["tool_choice"]) {
            out["toolConfig"] = choice;
        }
    }
    let mut messages: Vec<Value> = vec![];
    let mut systems = vec![];
    let mut names = BTreeMap::<String, String>::new();
    let mut leading_system = true;
    let nested_media = gemini_nested_tool_media(input, ctx);
    for msg in arr(&input["messages"]) {
        let role = s(&msg["role"]);
        if matches!(role, "system" | "developer") {
            let text = if ctx.options.preserve_message_structure {
                preserved_system_text(&msg["content"])?
            } else {
                text_content(&msg["content"])
            };
            if ctx.options.preserve_message_structure && !leading_system {
                messages.push(json!({
                    "role":"user",
                    "parts":[{"text":format!("[{role}]\n{text}")}],
                }));
            } else {
                systems.push(text);
            }
            continue;
        }
        leading_system = false;
        if matches!(role, "tool" | "function") {
            if messages.last().is_none_or(|last| last["role"] == "model") {
                messages.push(json!({"role":"user","parts":[]}));
            }
            let name = msg["name"].as_str().map(str::to_owned).unwrap_or_else(|| {
                names
                    .get(s(&msg["tool_call_id"]))
                    .cloned()
                    .unwrap_or_default()
            });
            let (output, mut media) = if ctx.options.preserve_message_structure {
                let content = if msg["content"].is_array() {
                    Value::Array(responses_parts("user", &msg["content"])?)
                } else {
                    msg["content"].clone()
                };
                responses::preserved_tool_output_gemini(&content, ctx, nested_media)?
            } else {
                (response_map(&json!(text_content(&msg["content"]))), vec![])
            };
            let mut response = json!({"name":name,"response":output});
            put_nonempty(&mut response, "id", s(&msg["tool_call_id"]));
            if nested_media && !media.is_empty() {
                response["parts"] = Value::Array(std::mem::take(&mut media));
            }
            messages.last_mut().unwrap()["parts"]
                .as_array_mut()
                .unwrap()
                .push(owned_object([("functionResponse", response)]));
            messages.last_mut().unwrap()["parts"]
                .as_array_mut()
                .unwrap()
                .extend(media);
            continue;
        }
        let mut parts = vec![];
        if ctx.options.preserve_message_structure && matches!(role, "assistant" | "model") {
            for block in reasoning_blocks(msg) {
                if block["type"] == "redacted_thinking" {
                    return Err(Error::message(
                        "Gemini cannot replay Claude redacted thinking",
                    ));
                }
                let mut part = json!({"text":s(&block["thinking"]),"thought":true});
                put_nonempty(&mut part, "thoughtSignature", s(&block["signature"]));
                if !s(&part["text"]).is_empty() || !s(&part["thoughtSignature"]).is_empty() {
                    parts.push(part);
                }
            }
        }
        let attach = matches!(role, "assistant" | "model")
            && ctx.options.gemini.function_call_thought_signature_enabled;
        let mut signed = false;
        for call in arr(&msg["tool_calls"]) {
            let args = s(&call["function"]["arguments"]);
            let parsed = if args.is_empty() {
                json!({})
            } else {
                serde_json::from_str::<Value>(args)
                    .ok()
                    .filter(|v| v.is_object() || v.is_null())
                    .ok_or_else(|| {
                        Error::message(format!(
                            "invalid arguments for function {}, args: {args}",
                            s(&call["function"]["name"])
                        ))
                    })?
            };
            let mut fc = json!({"name":s(&call["function"]["name"]),"args":parsed});
            put_nonempty(&mut fc, "id", s(&call["id"]));
            let mut part = owned_object([("functionCall", fc)]);
            if ctx.options.preserve_message_structure {
                if let Some(signature) = call
                    .get("thoughtSignature")
                    .or_else(|| call.get("thought_signature"))
                {
                    part["thoughtSignature"] = signature.clone();
                    signed = true;
                }
            }
            if attach
                && !signed
                && (!s(&call["function"]["name"]).trim().is_empty()
                    || parsed.as_object().is_some_and(|m| !m.is_empty()))
            {
                attach_signature(&mut part, ctx);
                signed = true;
            }
            parts.push(part);
            names.insert(s(&call["id"]).into(), s(&call["function"]["name"]).into());
        }
        let input = if ctx.options.preserve_message_structure {
            preserved_chat_parts(&msg["content"])?
        } else {
            chat_parts(&msg["content"])
        };
        for part in input {
            if s(&part["type"]) == "text" {
                let original = s(&part["text"]);
                if original.is_empty() {
                    continue;
                }
                let mut remaining = original;
                let mut image = false;
                while let Some(start) = remaining.find("![") {
                    let Some(bracket) = remaining[start..].find("](data:").map(|i| i + start)
                    else {
                        break;
                    };
                    let Some(end) = remaining[bracket + 2..].find(')').map(|i| i + bracket + 2)
                    else {
                        break;
                    };
                    image = true;
                    if start > 0 {
                        parts.push(json!({"text":&remaining[..start]}));
                    }
                    let (mime, data) =
                        ctx.decode_media(&remaining[bracket + 2..end])
                            .map_err(|e| {
                                Error::message(format!(
                                    "decode markdown base64 image data failed: {e}"
                                ))
                            })?;
                    let mut p = json!({"inlineData":{"mimeType":mime,"data":data}});
                    if attach {
                        attach_signature(&mut p, ctx);
                    }
                    parts.push(p);
                    remaining = &remaining[end + 1..];
                }
                // Upstream intentionally discards the tail once a markdown image was found.
                if !image {
                    parts.push(json!({"text":original}));
                } else if ctx.options.preserve_message_structure && !remaining.is_empty() {
                    parts.push(json!({"text":remaining}));
                }
            } else if ctx.options.preserve_message_structure
                && !matches!(s(&part["type"]), "image_url" | "file" | "input_audio" | "video_url")
            {
                return Err(Error::message(format!(
                    "Chat content part {:?} cannot be represented by Gemini",
                    s(&part["type"])
                )));
            } else if let Some(media) = gemini_media(&part, ctx, false)? {
                parts.push(media);
            }
        }
        if attach && !signed {
            if let Some(p) = parts.iter_mut().find(|p| !s(&p["text"]).is_empty()) {
                attach_signature(p, ctx);
            }
        }
        if !parts.is_empty() {
            messages.push(owned_object([
                (
                    "role",
                    Value::String(if role == "assistant" { "model" } else { role }.into()),
                ),
                ("parts", Value::Array(parts)),
            ]));
        }
    }
    out["contents"] = Value::Array(messages);
    if !systems.is_empty() {
        out["systemInstruction"] = json!({"parts":[{"text":systems.join("\n")}]});
    }
    Ok(out)
}

fn extra_body(input: &Value, out: &mut Value) -> Result<(), Error> {
    let google = &input["extra_body"]["google"];
    if !google.is_object() {
        return Ok(());
    }
    for (bad, good) in [
        ("thinkingConfig", "thinking_config"),
        ("imageConfig", "image_config"),
    ] {
        if google.get(bad).is_some() {
            return Err(Error::message(format!(
                "extra_body.google.{bad} is not supported, use extra_body.google.{good} instead"
            )));
        }
    }
    let thinking = &google["thinking_config"];
    if thinking.get("thinkingBudget").is_some() {
        return Err(Error::message(
            "extra_body.google.thinking_config.thinkingBudget is not supported, use extra_body.google.thinking_config.thinking_budget instead",
        ));
    }
    let mut config = json!({});
    for (from, to, kind) in [
        ("thinking_budget", "thinkingBudget", "integer"),
        ("include_thoughts", "includeThoughts", "boolean"),
        ("thinking_level", "thinkingLevel", "string"),
    ] {
        if let Some(v) = thinking.get(from) {
            let valid = match kind {
                "integer" => {
                    v.as_i64().is_some()
                        || v.as_f64().is_some_and(|n| {
                            n.is_finite()
                                && n.fract() == 0.0
                                && n >= i64::MIN as f64
                                && n <= i64::MAX as f64
                        })
                }
                "boolean" => v.is_boolean(),
                _ => v.is_string(),
            };
            if !valid {
                return Err(Error::message(format!(
                    "extra_body.google.thinking_config.{from} must be {} {kind}",
                    if kind == "integer" { "an" } else { "a" }
                )));
            }
            config[to] = if kind == "integer" {
                json!(v.as_i64().unwrap_or_else(|| v.as_f64().unwrap() as i64))
            } else {
                v.clone()
            };
        }
    }
    if config.as_object().is_some_and(|m| !m.is_empty()) {
        out["generationConfig"]["thinkingConfig"] = config;
    }
    let image = &google["image_config"];
    for (bad, good) in [("aspectRatio", "aspect_ratio"), ("imageSize", "image_size")] {
        if image.get(bad).is_some() {
            return Err(Error::message(format!(
                "extra_body.google.image_config.{bad} is not supported, use extra_body.google.image_config.{good} instead"
            )));
        }
    }
    let mut config = json!({});
    copy_renamed(
        image,
        &mut config,
        &[("aspect_ratio", "aspectRatio"), ("image_size", "imageSize")],
    );
    if config.as_object().is_some_and(|m| !m.is_empty()) {
        out["generationConfig"]["imageConfig"] = config;
    }
    Ok(())
}
pub(super) fn apply_response_format(
    format: &Value,
    out: &mut Value,
    ctx: &Context,
    ds: &mut Vec<Diagnostic>,
    path: &str,
    source_model: &str,
) {
    if matches!(s(&format["type"]), "json_schema" | "json_object") {
        out["generationConfig"]["responseMimeType"] = json!("application/json");
        if !format["json_schema"].is_null() {
            if !format["json_schema"].is_object() {
                return;
            }
            let schema = &format["json_schema"]["schema"];
            let misfits = gemini_schema::misfits(schema);
            let model = if ctx.upstream_model().is_empty() {
                source_model
            } else {
                ctx.upstream_model()
            };
            let supports_json_schema = ctx
                .options
                .gemini
                .supports_json_schema
                .as_ref()
                .is_some_and(|supports| supports(model));
            if !misfits.is_empty() && supports_json_schema {
                out["generationConfig"]["responseJsonSchema"] = schema.clone();
            } else {
                let mut schema = schema.clone();
                strip_additional(&mut schema, 0);
                if !schema.is_null() {
                    out["generationConfig"]["responseSchema"] = schema;
                }
                if !misfits.is_empty() {
                    diag(
                        ds,
                        Severity::Warning,
                        "gemini_schema_keyword_dropped",
                        path,
                        format!("Gemini response schema cannot carry {}", misfits.join(", ")),
                    );
                }
            }
        }
    }
}
fn strip_additional(schema: &mut Value, depth: usize) {
    if depth >= 5 {
        return;
    }
    let Some(m) = schema.as_object_mut() else {
        return;
    };
    m.remove("title");
    m.remove("$schema");
    match m.get("type").and_then(Value::as_str) {
        Some("object") => {
            m.remove("additionalProperties");
            if let Some(Value::Object(p)) = m.get_mut("properties") {
                for v in p.values_mut() {
                    strip_additional(v, depth + 1);
                }
            }
            for field in ["allOf", "anyOf", "oneOf"] {
                if let Some(Value::Array(v)) = m.get_mut(field) {
                    for v in v {
                        strip_additional(v, depth + 1);
                    }
                }
            }
        }
        Some("array") => {
            if let Some(v) = m.get_mut("items") {
                if v.is_object() {
                    strip_additional(v, depth + 1);
                }
            }
        }
        _ => {}
    }
}
pub(super) fn gemini_function(function: &Value) -> Value {
    let mut f = json!({"name":s(&function["name"])});
    for key in ["description", "arguments"] {
        put_nonempty(&mut f, key, s(&function[key]));
    }
    if !function["strict"].is_null() {
        f["strict"] = function["strict"].clone();
    }
    if !function["parameters"].is_null()
        && !function["parameters"]["properties"]
            .as_object()
            .is_some_and(Map::is_empty)
    {
        f["parameters"] = clean_schema(&function["parameters"], 0);
    }
    f
}
pub(super) fn clean_schema(schema: &Value, depth: usize) -> Value {
    match schema {
        Value::Object(m) => {
            let mut clean: Map<String, Value> = m
                .iter()
                .filter(|(k, _)| gemini_schema::ALLOWED_FIELDS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if let Some(raw) = clean.get("type").cloned() {
                if raw.is_string() || raw.is_array() {
                    let types = if raw.is_array() {
                        arr(&raw).to_vec()
                    } else {
                        vec![raw]
                    };
                    let mut chosen = None;
                    for t in types {
                        if let Some(t) = t.as_str() {
                            let normalized = t.trim().to_lowercase();
                            if normalized == "null" {
                                clean.insert("nullable".into(), json!(true));
                            } else if chosen.is_none() {
                                chosen = Some(
                                    if matches!(
                                        normalized.as_str(),
                                        "object"
                                            | "array"
                                            | "string"
                                            | "integer"
                                            | "number"
                                            | "boolean"
                                    ) {
                                        normalized.to_uppercase()
                                    } else {
                                        t.into()
                                    },
                                );
                            }
                        }
                    }
                    clean.remove("type");
                    if let Some(chosen) = chosen {
                        clean.insert("type".into(), json!(chosen));
                    }
                }
            }
            if clean.get("enum").is_some_and(|values| {
                !values.is_array() || arr(values).iter().any(|value| !value.is_string())
            }) {
                clean.remove("enum");
            }
            if depth >= 64 {
                for key in ["properties", "items", "anyOf"] {
                    clean.remove(key);
                }
            } else {
                if let Some(Value::Object(props)) = clean.get_mut("properties") {
                    for v in props.values_mut() {
                        *v = clean_schema(v, depth + 1);
                    }
                }
                if let Some(items) = clean.get_mut("items") {
                    if items.is_object() {
                        *items = clean_schema(items, depth + 1);
                    } else if let Some(first) = items.as_array().and_then(|a| a.first()) {
                        *items = clean_schema(first, depth + 1);
                    }
                }
                if let Some(Value::Array(variants)) = clean.get_mut("anyOf") {
                    for v in variants {
                        *v = clean_schema(v, depth + 1);
                    }
                }
            }
            Value::Object(clean)
        }
        Value::Array(a) => {
            if depth >= 64 {
                json!([])
            } else {
                json!(
                    a.iter()
                        .map(|v| clean_schema(v, depth + 1))
                        .collect::<Vec<_>>()
                )
            }
        }
        _ => schema.clone(),
    }
}
pub(super) fn gemini_choice(choice: &Value) -> Option<Value> {
    let mut config = json!({});
    match choice {
        Value::String(text) => {
            config["mode"] = json!(match text.as_str() {
                "none" => "NONE",
                "required" => "ANY",
                _ => "AUTO",
            })
        }
        Value::Object(_) if choice["type"] == "function" => {
            config["mode"] = json!("ANY");
            if !s(&choice["function"]["name"]).is_empty() {
                config["allowedFunctionNames"] = json!([choice["function"]["name"]]);
            }
        }
        _ => return None,
    }
    Some(json!({"functionCallingConfig":config}))
}
