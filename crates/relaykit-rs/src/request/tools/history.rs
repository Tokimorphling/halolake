//! Server-side tool continuation history. Kept separate from client function
//! calls because replaying hosted results as user text changes their meaning.
use super::super::*;

#[derive(Clone, Debug)]
pub(super) struct HistoryItem {
    kind: String,
    native: String,
    role: String,
    index: usize,
    mixed: bool,
    id: String,
    call_id: String,
    name: String,
    server: String,
    status: String,
    action: Option<Value>,
    results: Option<Value>,
    caller: Option<Value>,
    raw: Value,
}
// Validate the same message shapes as extraction, but avoid rebuilding ordinary
// tool histories when no hosted block or removed empty message is present.
pub(super) fn needs_extraction(source: Format, input: &Value) -> Result<bool, Error> {
    let mut changed = false;
    match source {
        Format::OpenAiResponses => {
            for (index, item) in arr(&input["input"]).iter().enumerate() {
                if !item.is_object() {
                    return Err(Error::message(format!("input[{index}]: expected object")));
                }
                changed |= responses_hosted(s(&item["type"]).trim());
            }
        }
        Format::Claude => {
            for (index, msg) in arr(&input["messages"]).iter().enumerate() {
                if msg["content"].is_string() {
                    continue;
                }
                if !msg["content"].is_array() && !msg["content"].is_null() {
                    return Err(Error::message(format!(
                        "messages[{index}].content: expected array"
                    )));
                }
                changed |= arr(&msg["content"]).is_empty();
                for (bi, b) in arr(&msg["content"]).iter().enumerate() {
                    if !b.is_object() {
                        return Err(Error::message(format!(
                            "messages[{index}].content[{bi}]: expected object"
                        )));
                    }
                    let native = s(&b["type"]).trim();
                    changed |= matches!(native, "server_tool_use" | "mcp_tool_use")
                        || native.ends_with("_tool_result");
                }
            }
        }
        _ => {}
    }
    Ok(changed)
}
pub(in crate::request) fn responses_hosted(native: &str) -> bool {
    matches!(
        native,
        "web_search_call"
            | "file_search_call"
            | "code_interpreter_call"
            | "computer_call"
            | "computer_call_output"
            | "image_generation_call"
            | "local_shell_call"
            | "local_shell_call_output"
            | "apply_patch_call"
            | "apply_patch_call_output"
            | "mcp_call"
            | "mcp_list_tools"
            | "mcp_approval_request"
            | "mcp_approval_response"
    )
}
pub(super) fn extract(source: Format, input: &mut Value) -> Result<Vec<HistoryItem>, Error> {
    let mut history = vec![];
    match source {
        Format::OpenAiResponses => {
            if !input["input"].is_array() {
                return Ok(history);
            }
            let mut filtered = vec![];
            for (index, item) in arr(&input["input"]).iter().enumerate() {
                if !item.is_object() {
                    return Err(Error::message(format!("input[{index}]: expected object")));
                }
                let native = s(&item["type"]).trim();
                if !responses_hosted(native) {
                    filtered.push(item.clone());
                    continue;
                }
                let mut status = s(&item["status"]).trim().to_owned();
                let mut action = item.get("action").cloned();
                let mut results = ["results", "sources", "output"]
                    .into_iter()
                    .find_map(|k| item.get(k).cloned());
                if native == "mcp_call" {
                    action = item.get("arguments").cloned();
                    results = item.get("output").cloned();
                    if !item["error"].is_null() {
                        results = item.get("error").cloned();
                        status = "failed".into();
                    }
                }
                let kind = match native.trim_end_matches("_output").trim_end_matches("_call") {
                    "web_search" => "web_search",
                    "file_search" => "file_search",
                    "code_interpreter" | "local_shell" => "code_execution",
                    "computer" => "computer_use",
                    "image_generation" => "image_generation",
                    "mcp" => "mcp",
                    _ => "native",
                };
                history.push(HistoryItem {
                    kind: kind.into(),
                    native: native.into(),
                    role: s(&item["role"]).trim().into(),
                    index,
                    mixed: false,
                    id: s(&item["id"]).trim().into(),
                    call_id: s(&item["call_id"]).trim().into(),
                    name: s(&item["name"]).trim().into(),
                    server: s(&item["server_label"]).trim().into(),
                    status,
                    action,
                    results,
                    caller: item.get("caller").cloned(),
                    raw: item.clone(),
                });
            }
            if !history.is_empty() {
                input["input"] = Value::Array(filtered);
            }
        }
        Format::Claude => {
            let mut messages = vec![];
            for (index, msg) in arr(&input["messages"]).iter().enumerate() {
                if msg["content"].is_string() {
                    messages.push(msg.clone());
                    continue;
                }
                if !msg["content"].is_array() && !msg["content"].is_null() {
                    return Err(Error::message(format!(
                        "messages[{index}].content: expected array"
                    )));
                }
                let mut filtered = vec![];
                let start = history.len();
                for (bi, b) in arr(&msg["content"]).iter().enumerate() {
                    if !b.is_object() {
                        return Err(Error::message(format!(
                            "messages[{index}].content[{bi}]: expected object"
                        )));
                    }
                    let native = s(&b["type"]).trim();
                    if !matches!(native, "server_tool_use" | "mcp_tool_use")
                        && !native.ends_with("_tool_result")
                    {
                        filtered.push(b.clone());
                        continue;
                    }
                    let name = s(&b["name"]).trim();
                    let result = native.ends_with("_tool_result");
                    let kind = if native == "mcp_tool_use" {
                        "mcp"
                    } else if result {
                        match native.trim_end_matches("_tool_result") {
                            "web_search" => "web_search",
                            "web_fetch" => "web_fetch",
                            "code_execution" => "code_execution",
                            "mcp" => "mcp",
                            _ => "native",
                        }
                    } else {
                        match name {
                            "web_search" => "web_search",
                            "web_fetch" => "web_fetch",
                            "code_execution" => "code_execution",
                            _ => "native",
                        }
                    };
                    let failed = b["is_error"] == true
                        || !s(&b["error_code"]).trim().is_empty()
                        || result
                            && b["content"].is_object()
                            && (s(&b["content"]["type"]).trim().ends_with("_error")
                                || !s(&b["content"]["error_code"]).trim().is_empty());
                    history.push(HistoryItem {
                        kind: kind.into(),
                        native: native.into(),
                        role: s(&msg["role"]).into(),
                        index,
                        mixed: false,
                        id: s(&b["id"]).trim().into(),
                        call_id: s(&b["tool_use_id"]).trim().into(),
                        name: name.into(),
                        server: s(&b["server_name"]).trim().into(),
                        status: if result {
                            if failed { "failed" } else { "completed" }
                        } else {
                            "in_progress"
                        }
                        .into(),
                        action: b.get("input").cloned(),
                        results: b.get("content").cloned(),
                        caller: b.get("caller").cloned(),
                        raw: b.clone(),
                    });
                }
                if !filtered.is_empty() {
                    for h in &mut history[start..] {
                        h.mixed = true;
                    }
                    let mut msg = msg.clone();
                    msg["content"] = Value::Array(filtered);
                    messages.push(msg);
                }
            }
            input["messages"] = Value::Array(messages);
        }
        _ => {}
    }
    Ok(history)
}
pub(super) fn attach(
    source: Format,
    target: Format,
    input: &mut Value,
    history: Vec<HistoryItem>,
) -> Result<Vec<Diagnostic>, Error> {
    if history.is_empty() {
        return Ok(vec![]);
    }
    match target {
        Format::OpenAiResponses => attach_responses(source, input, &history),
        Format::Claude => attach_claude(source, input, &history),
        _ => Ok(unsupported(target, &history)),
    }
}
fn unsupported(target: Format, history: &[HistoryItem]) -> Vec<Diagnostic> {
    let mut ds = vec![];
    for (i, h) in history.iter().enumerate() {
        unsupported_one(target, i, h, &mut ds);
    }
    ds
}
fn unsupported_one(target: Format, i: usize, h: &HistoryItem, ds: &mut Vec<Diagnostic>) {
    diag(
        ds,
        Severity::Error,
        "hosted_tool_history_unsupported",
        format!("hosted_history[{i}]"),
        format!(
            "{target} cannot preserve hosted-tool continuation item {:?}",
            h.native
        ),
    );
}
fn attach_responses(
    source: Format,
    target: &mut Value,
    history: &[HistoryItem],
) -> Result<Vec<Diagnostic>, Error> {
    let ordinary = input_items(&target["input"])?;
    let mut ds = vec![];
    let mut groups = BTreeMap::<usize, Vec<usize>>::new();
    let mut flat = Vec::<Value>::new();
    let mut by_id = BTreeMap::<String, usize>::new();
    let mixed: BTreeSet<_> = history
        .iter()
        .filter(|h| h.mixed)
        .map(|h| h.index)
        .collect();
    for (i, h) in history.iter().enumerate() {
        let path = format!("hosted_history[{i}]");
        if source == Format::OpenAiResponses {
            groups.entry(h.index).or_default().push(flat.len());
            flat.push(h.raw.clone());
            continue;
        }
        let kind = match h.kind.as_str() {
            "web_search" => "web_search_call",
            "mcp" => "mcp_call",
            _ => {
                unsupported_one(Format::OpenAiResponses, i, h, &mut ds);
                continue;
            }
        };
        let id = if h.id.is_empty() { &h.call_id } else { &h.id };
        if h.native.ends_with("_tool_result") {
            if let Some(index) = by_id.get(&h.call_id).copied() {
                let call = &mut flat[index];
                let failed = h.status == "failed";
                call["status"] = json!(if failed { "failed" } else { "completed" });
                if h.kind == "mcp" {
                    if let Some(result) = &h.results {
                        match mcp_result_string(result) {
                            Ok((value, normalized)) => {
                                call[if failed { "error" } else { "output" }] = json!(value);
                                if normalized {
                                    normalized_diag(i, &mut ds);
                                }
                            }
                            Err(e) => {
                                diag(
                                    &mut ds,
                                    Severity::Error,
                                    "mcp_result_unrepresentable",
                                    format!("{path}.content"),
                                    e,
                                );
                                continue;
                            }
                        }
                    }
                } else if h.kind == "web_search" && h.results.is_some() {
                    diag(
                        &mut ds,
                        Severity::Warning,
                        "web_search_result_omitted",
                        format!("{path}.content"),
                        "Claude web-search result content has no field on a Responses web_search_call",
                    );
                }
                continue;
            }
        }
        let mut item = json!({"type":kind,"id":id,"status":if h.status.is_empty(){"in_progress"}else{&h.status}});
        if h.kind == "web_search" {
            item["action"] = normalize_search(h.action.as_ref().unwrap_or(&Value::Null))
                .map_err(|e| Error::message(format!("{path}.action: {e}")))?;
        } else if h.kind == "mcp" {
            if h.name.trim().is_empty() || h.server.trim().is_empty() {
                diag(
                    &mut ds,
                    Severity::Error,
                    "mcp_identity_missing",
                    &path,
                    "Claude MCP history requires both name and server_name for Responses mapping",
                );
                continue;
            }
            item["name"] = json!(h.name);
            item["server_label"] = json!(h.server);
            if let Some(action) = &h.action {
                if !action.is_object() {
                    diag(
                        &mut ds,
                        Severity::Error,
                        "mcp_arguments_unrepresentable",
                        format!("{path}.input"),
                        "Claude MCP input must be a JSON object",
                    );
                    continue;
                }
                item["arguments"] = json!(stringify(action));
            } else {
                diag(
                    &mut ds,
                    Severity::Error,
                    "mcp_arguments_missing",
                    format!("{path}.input"),
                    "Claude MCP history has no input object",
                );
                continue;
            }
            if let Some(result) = &h.results {
                match mcp_result_string(result) {
                    Ok((value, normalized)) => {
                        let failed = h.status == "failed";
                        item[if failed { "error" } else { "output" }] = json!(value);
                        item["status"] = json!(if failed { "failed" } else { "completed" });
                        if normalized {
                            normalized_diag(i, &mut ds);
                        }
                    }
                    Err(e) => {
                        diag(
                            &mut ds,
                            Severity::Error,
                            "mcp_result_unrepresentable",
                            format!("{path}.content"),
                            e,
                        );
                        continue;
                    }
                }
            }
            if let Some(caller) = &h.caller {
                item["caller"] = caller.clone();
            }
        }
        if !id.is_empty() {
            by_id.insert(id.clone(), flat.len());
        }
        groups.entry(h.index).or_default().push(flat.len());
        flat.push(item);
    }
    if groups.is_empty() {
        return Ok(if ds.is_empty() {
            unsupported(Format::OpenAiResponses, history)
        } else {
            ds
        });
    }
    let mut merged = vec![];
    let mut pos = 0;
    let max = history.iter().map(|h| h.index).max().unwrap_or(0);
    let mut source_index = 0;
    while source_index <= max || pos < ordinary.len() {
        if let Some(indices) = groups.get(&source_index) {
            for &i in indices {
                merged.push(flat[i].clone());
            }
            if mixed.contains(&source_index) {
                if pos < ordinary.len() {
                    merged.push(ordinary[pos].clone());
                    pos += 1;
                }
                diag(
                    &mut ds,
                    Severity::Error,
                    "hosted_tool_order_unrepresentable",
                    format!("hosted_history[{source_index}]"),
                    "hosted and ordinary blocks share one source message, but Responses represents hosted calls as separate input items",
                );
            }
        } else if pos < ordinary.len() {
            merged.push(ordinary[pos].clone());
            pos += 1;
        }
        source_index += 1;
    }
    target["input"] = Value::Array(merged);
    approximate("input", &mut ds);
    Ok(ds)
}
fn attach_claude(
    source: Format,
    target: &mut Value,
    history: &[HistoryItem],
) -> Result<Vec<Diagnostic>, Error> {
    let mut ds = vec![];
    let mut groups = BTreeMap::<usize, Vec<Value>>::new();
    let mixed: BTreeSet<_> = history
        .iter()
        .filter(|h| h.mixed)
        .map(|h| h.index)
        .collect();
    for (i, h) in history.iter().enumerate() {
        let path = format!("hosted_history[{i}]");
        if source == Format::Claude {
            groups.entry(h.index).or_default().push(h.raw.clone());
            continue;
        }
        if source == Format::OpenAiResponses && h.kind == "web_search" {
            diag(
                &mut ds,
                Severity::Error,
                "web_search_continuation_unrepresentable",
                &path,
                "Responses web-search history cannot reconstruct Claude's encrypted web_search_tool_result continuation state",
            );
            continue;
        }
        let id = if h.id.is_empty() { &h.call_id } else { &h.id };
        let mut blocks = vec![];
        if h.kind == "mcp" {
            if h.name.is_empty() || h.server.is_empty() {
                diag(
                    &mut ds,
                    Severity::Error,
                    "mcp_identity_missing",
                    &path,
                    "Responses MCP history requires both name and server_label for Claude mapping",
                );
                continue;
            }
            if id.is_empty() {
                diag(
                    &mut ds,
                    Severity::Error,
                    "hosted_tool_id_missing",
                    format!("{path}.id"),
                    "Responses MCP history has no id for pairing the call with its result",
                );
                continue;
            }
            if !matches!(
                h.status.as_str(),
                "" | "in_progress" | "completed" | "failed"
            ) {
                diag(
                    &mut ds,
                    Severity::Error,
                    "hosted_tool_status_unrepresentable",
                    format!("{path}.status"),
                    format!("Claude cannot preserve Responses MCP status {:?}", h.status),
                );
            }
            let mut result = h.results.as_ref();
            let mut failed = h.status == "failed";
            if !s(&h.raw["approval_request_id"]).trim().is_empty() {
                diag(
                    &mut ds,
                    Severity::Error,
                    "mcp_approval_state_unrepresentable",
                    format!("{path}.approval_request_id"),
                    "Claude MCP history cannot preserve a Responses approval_request_id",
                );
            }
            if !h.raw["output"].is_null() && !h.raw["error"].is_null() {
                diag(
                    &mut ds,
                    Severity::Error,
                    "mcp_result_ambiguous",
                    &path,
                    "Responses MCP history contains both output and error",
                );
            }
            if !h.raw["error"].is_null() {
                result = h.raw.get("error");
                failed = true;
            } else if !h.raw["output"].is_null() {
                result = h.raw.get("output");
            }
            let input = match mcp_input(h.action.as_ref().unwrap_or(&Value::Null)) {
                Ok(v) => v,
                Err(e) => {
                    diag(
                        &mut ds,
                        Severity::Error,
                        "mcp_arguments_unrepresentable",
                        format!("{path}.arguments"),
                        e,
                    );
                    continue;
                }
            };
            let mut call = json!({"type":"mcp_tool_use","id":id,"name":h.name,"server_name":h.server,"input":input});
            if let Some(caller) = &h.caller {
                call["caller"] = caller.clone();
            }
            blocks.push(call);
            if let Some(result) = result.filter(|v| !v.is_null()) {
                if !result.is_string() {
                    diag(
                        &mut ds,
                        Severity::Error,
                        "mcp_result_unrepresentable",
                        format!("{path}.output"),
                        "Responses MCP output/error must be a JSON string",
                    );
                    continue;
                }
                let mut result =
                    json!({"type":"mcp_tool_result","tool_use_id":id,"content":result});
                if failed {
                    result["is_error"] = json!(true);
                }
                blocks.push(result);
            } else if h.status == "completed" || failed {
                diag(
                    &mut ds,
                    Severity::Error,
                    "mcp_result_missing",
                    &path,
                    format!(
                        "Responses MCP history has status {:?} but no output or error",
                        h.status
                    ),
                );
            }
            groups.entry(h.index).or_default().extend(blocks);
            continue;
        }
        let name = match h.kind.as_str() {
            "web_search" => "web_search",
            "web_fetch" => "web_fetch",
            _ => {
                unsupported_one(Format::Claude, i, h, &mut ds);
                continue;
            }
        };
        let action = h.action.clone().unwrap_or(json!({}));
        let mut call = json!({"type":"server_tool_use","id":id,"name":name,"input":action});
        if let Some(caller) = &h.caller {
            call["caller"] = caller.clone();
        }
        blocks.push(call);
        if let Some(result) = &h.results {
            blocks.push(
                json!({"type":format!("{name}_tool_result"),"tool_use_id":id,"content":result}),
            );
        }
        groups.entry(h.index).or_default().extend(blocks);
    }
    if groups.is_empty() {
        return Ok(if ds.is_empty() {
            unsupported(Format::Claude, history)
        } else {
            ds
        });
    }
    let ordinary = arr(&target["messages"]);
    let mut merged = vec![];
    let mut pos = 0;
    let max = history.iter().map(|h| h.index).max().unwrap_or(0);
    let mut index = 0;
    while index <= max || pos < ordinary.len() {
        if let Some(blocks) = groups.get(&index) {
            let role = history
                .iter()
                .find(|h| h.index == index && !h.role.is_empty())
                .map(|h| h.role.as_str())
                .unwrap_or("assistant");
            merged.push(json!({"role":role,"content":blocks}));
            if mixed.contains(&index) {
                if pos < ordinary.len() {
                    merged.push(ordinary[pos].clone());
                    pos += 1;
                }
                diag(
                    &mut ds,
                    Severity::Error,
                    "hosted_tool_order_unrepresentable",
                    format!("hosted_history[{index}]"),
                    "hosted and ordinary content cannot be merged after the intermediate converter coalesced source blocks",
                );
            }
        } else if pos < ordinary.len() {
            merged.push(ordinary[pos].clone());
            pos += 1;
        }
        index += 1;
    }
    target["messages"] = Value::Array(merged);
    approximate("messages", &mut ds);
    Ok(ds)
}
fn mcp_input(value: &Value) -> Result<Value, String> {
    let Some(text) = value.as_str() else {
        return Err("Responses MCP arguments must be a JSON string".into());
    };
    if !text.trim().starts_with('{') {
        return Err("Responses MCP arguments must contain a JSON object".into());
    }
    serde_json::from_str::<Value>(text)
        .map_err(|e| format!("decode Responses MCP arguments object: {e}"))
        .and_then(|v| {
            if v.is_object() {
                Ok(v)
            } else {
                Err("Responses MCP arguments must contain a JSON object".into())
            }
        })
}
fn mcp_result_string(value: &Value) -> Result<(String, bool), String> {
    match value {
        Value::String(s) => Ok((s.clone(), false)),
        Value::Array(blocks) => {
            if blocks.is_empty() {
                return Ok((String::new(), true));
            }
            if blocks.len() != 1 {
                return Err(format!(
                    "Responses MCP output cannot preserve {} Claude content blocks",
                    blocks.len()
                ));
            }
            if blocks[0]["type"] != "text" {
                return Err(
                    "Responses MCP output can only preserve a Claude text result block".into(),
                );
            }
            let Some(text) = blocks[0]["text"].as_str() else {
                return Err("decode Claude MCP text result: expected string".into());
            };
            Ok((text.into(), true))
        }
        _ => Err(format!(
            "Responses MCP output cannot preserve Claude result type {:?}",
            match value {
                Value::Null => "null",
                Value::Bool(_) => "boolean",
                Value::Number(_) => "number",
                _ => "object",
            }
        )),
    }
}
fn normalized_diag(i: usize, ds: &mut Vec<Diagnostic>) {
    diag(
        ds,
        Severity::Warning,
        "mcp_text_result_normalized",
        format!("hosted_history[{i}].content"),
        "Claude's single MCP text block was normalized to a Responses output string",
    );
}
fn approximate(path: &str, ds: &mut Vec<Diagnostic>) {
    diag(
        ds,
        Severity::Warning,
        "hosted_tool_history_approximated",
        path,
        "hosted-tool continuation state is preserved, but provider-specific item fields may differ",
    );
}
fn normalize_search(value: &Value) -> Result<Value, String> {
    if !value.is_object() && !value.is_null() {
        return Err("decode Responses web-search action: expected object".into());
    }
    let mut kind = s(&value["type"]).trim();
    let query = s(&value["query"]).trim();
    let url = s(&value["url"]).trim();
    let pattern = s(&value["pattern"]).trim();
    let mut queries = vec![];
    for (i, q) in arr(&value["queries"]).iter().enumerate() {
        let q = s(q).trim();
        if q.is_empty() {
            return Err(format!(
                "Responses web-search action queries[{i}] must not be empty"
            ));
        }
        queries.push(q);
    }
    if kind.is_empty() && (!query.is_empty() || !queries.is_empty()) {
        kind = "search";
    }
    match kind {
        "search" => {
            if query.is_empty() && queries.is_empty() {
                return Err(
                    "Responses web-search action \"search\" requires query or queries".into(),
                );
            }
            if !value["sources"].is_null() && !value["sources"].is_array() {
                return Err("Responses web-search action sources must be an array".into());
            }
            let mut result = json!({"type":"search"});
            put_nonempty(&mut result, "query", query);
            if !queries.is_empty() {
                result["queries"] = json!(queries);
            }
            if let Some(v) = value.get("sources") {
                result["sources"] = v.clone();
            }
            Ok(result)
        }
        "open_page" => {
            if url.is_empty() {
                return Err("Responses web-search action \"open_page\" requires url".into());
            }
            Ok(json!({"type":"open_page","url":url}))
        }
        "find" | "find_in_page" => {
            if url.is_empty() || pattern.is_empty() {
                return Err(format!(
                    "Responses web-search action {kind:?} requires url and pattern"
                ));
            }
            Ok(json!({"type":"find_in_page","url":url,"pattern":pattern}))
        }
        _ => Err(format!(
            "unsupported Responses web-search action type {kind:?}"
        )),
    }
}
