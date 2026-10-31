//! Hosted tools are carried alongside the message pivot, never reclassified as
//! client-executed function calls.
use super::{arr, go_json, n, new_id, s};
use crate::{Diagnostic, Error, Format, Severity};
use serde_json::{Value, json};
use std::{borrow::Cow, collections::BTreeMap};

pub(crate) fn diagnostic(
    from: Format,
    to: Format,
    path: impl Into<String>,
    code: &str,
    message: impl Into<String>,
    warning: bool,
) -> Diagnostic {
    Diagnostic {
        from,
        to,
        path: path.into(),
        code: code.into(),
        message: message.into(),
        severity: if warning {
            Severity::Warning
        } else {
            Severity::Error
        },
    }
}
fn claude_hosted(t: &str) -> bool {
    t == "server_tool_use" || t == "mcp_tool_use" || t.ends_with("_tool_result")
}
fn response_hosted(t: &str) -> bool {
    !matches!(
        t.trim(),
        "" | "message" | "reasoning" | "function_call" | "custom_tool_call"
    )
}
fn kind(from: Format, v: &Value) -> &str {
    let t = s(&v["type"]).trim();
    if from == Format::Claude {
        if t == "mcp_tool_use" {
            "mcp"
        } else if t == "server_tool_use" {
            s(&v["name"]).trim()
        } else {
            t.strip_suffix("_tool_result").unwrap_or(t)
        }
    } else {
        t.strip_suffix("_output")
            .unwrap_or(t)
            .strip_suffix("_call")
            .unwrap_or(t.strip_suffix("_output").unwrap_or(t))
    }
}
fn result_failure(v: &Value) -> (bool, String) {
    let explicit = s(&v["error_code"]).trim();
    if v["is_error"] == true || !explicit.is_empty() {
        return (true, explicit.into());
    }
    let c = &v["content"];
    let error = s(&c["error_code"]).trim();
    if s(&v["type"]).trim().ends_with("_tool_result")
        && c.is_object()
        && (s(&c["type"]).trim().ends_with("_error") || !error.is_empty())
    {
        return (true, error.into());
    }
    (false, String::new())
}
pub(crate) fn normalize_web_action(v: &Value) -> Result<Value, Error> {
    if !v.is_object() && !v.is_null() {
        return Err(Error::message(
            "decode Responses web-search action: expected object",
        ));
    }
    let mut typ = s(&v["type"]).trim();
    let query = s(&v["query"]).trim();
    let url = s(&v["url"]).trim();
    let pattern = s(&v["pattern"]).trim();
    let mut queries = vec![];
    for (i, q) in arr(&v["queries"]).iter().enumerate() {
        if s(q).trim().is_empty() {
            return Err(Error::message(format!(
                "Responses web-search action queries[{i}] must not be empty"
            )));
        }
        queries.push(s(q).trim());
    }
    if typ.is_empty() && (!query.is_empty() || !queries.is_empty()) {
        typ = "search";
    }
    Ok(match typ {
        "search" => {
            if query.is_empty() && queries.is_empty() {
                return Err(Error::message(
                    "Responses web-search action \"search\" requires query or queries",
                ));
            }
            if !v["sources"].is_null() && !v["sources"].is_array() {
                return Err(Error::message(
                    "Responses web-search action sources must be an array",
                ));
            }
            let mut out = json!({"type":"search"});
            if !query.is_empty() {
                out["query"] = json!(query);
            }
            if !queries.is_empty() {
                out["queries"] = json!(queries);
            }
            if v.get("sources").is_some() {
                out["sources"] = v["sources"].clone();
            }
            out
        }
        "open_page" => {
            if url.is_empty() {
                return Err(Error::message(
                    "Responses web-search action \"open_page\" requires url",
                ));
            }
            json!({"type":"open_page","url":url})
        }
        "find" | "find_in_page" => {
            if url.is_empty() || pattern.is_empty() {
                return Err(Error::message(format!(
                    "Responses web-search action {typ:?} requires url and pattern"
                )));
            }
            json!({"type":"find_in_page","url":url,"pattern":pattern})
        }
        _ => {
            return Err(Error::message(format!(
                "unsupported Responses web-search action type {typ:?}"
            )));
        }
    })
}
fn web_input(v: &Value) -> Result<Value, Error> {
    let a = normalize_web_action(v)?;
    let typ = s(&a["type"]);
    if typ != "search" {
        return Err(Error::message(format!(
            "Responses web-search action {typ:?} has no Claude equivalent"
        )));
    }
    let mut queries: Vec<_> = arr(&a["queries"])
        .iter()
        .map(|v| s(v).trim())
        .filter(|s| !s.is_empty())
        .collect();
    let query = s(&a["query"]).trim();
    if queries.is_empty() && !query.is_empty() {
        queries.push(query);
    } else if !query.is_empty() && (queries.len() != 1 || queries[0] != query) {
        return Err(Error::message(
            "Responses web-search action contains conflicting query and queries fields",
        ));
    }
    if queries.len() != 1 {
        return Err(Error::message(format!(
            "Claude web search requires exactly one query, got {}",
            queries.len()
        )));
    }
    Ok(json!({"query":queries[0]}))
}
fn mcp_input(v: &Value) -> Result<Value, Error> {
    let Some(raw) = v.as_str() else {
        return Err(Error::message(
            "Responses MCP arguments must be a JSON string",
        ));
    };
    let o = serde_json::from_str::<Value>(raw)
        .map_err(|_| Error::message("Responses MCP arguments must contain a JSON object"))?;
    if !o.is_object() {
        return Err(Error::message(
            "Responses MCP arguments must contain a JSON object",
        ));
    }
    Ok(o)
}
pub(super) fn mcp_output(v: &Value, code: &str) -> Result<(Value, bool), Error> {
    if !code.is_empty() {
        return Ok((json!(code), false));
    }
    if v.is_string() {
        return Ok((v.clone(), false));
    }
    if let Some(a) = v.as_array() {
        if a.is_empty() {
            return Ok((json!(""), true));
        }
        if a.len() != 1 {
            return Err(Error::message(format!(
                "Responses MCP output cannot preserve {} Claude content blocks",
                a.len()
            )));
        }
        if s(&a[0]["type"]) != "text" {
            return Err(Error::message(
                "Responses MCP output can only preserve a Claude text result block",
            ));
        }
        if let Some(t) = a[0]["text"].as_str() {
            return Ok((json!(t), true));
        }
        return Err(Error::message(
            "decode Claude MCP text result: expected string",
        ));
    }
    let typ = match v {
        Value::Null => "null",
        Value::Object(_) => "object",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        _ => "unknown",
    };
    Err(Error::message(format!(
        "Responses MCP output cannot preserve Claude result type {typ:?}"
    )))
}
pub(crate) fn inspect(from: Format, to: Format, v: &Value) -> Vec<Diagnostic> {
    if from == to {
        return vec![];
    }
    let mut ds = vec![];
    let mut add = |path: String, code: &str, msg: String, w: bool| {
        ds.push(diagnostic(from, to, path, code, msg, w))
    };
    if from == Format::Claude {
        if to != Format::OpenAiResponses
            && (s(&v["stop_reason"]) == "pause_turn"
                || s(&v["delta"]["stop_reason"]) == "pause_turn")
        {
            add("stop_reason".into(),"continuation_state_lost","Claude pause_turn requires protocol-native continuation state that the target response cannot preserve".into(),false);
        }
        let mut blocks: Vec<_> = arr(&v["content"])
            .iter()
            .enumerate()
            .map(|(i, b)| (format!("content[{i}]"), b, false))
            .collect();
        if !v["content_block"].is_null() {
            blocks.push(("content_block".into(), &v["content_block"], true));
        }
        for (path, b, stream) in blocks {
            let typ = s(&b["type"]).trim();
            let k = kind(from, b);
            if claude_hosted(typ) {
                if to != Format::OpenAiResponses || !matches!(k, "web_search" | "mcp") {
                    add(
                        path.clone(),
                        "hosted_tool_unrepresentable",
                        format!(
                            "{to} cannot losslessly represent Claude hosted-tool response block {typ:?}"
                        ),
                        false,
                    );
                } else if typ == "server_tool_use" || typ == "mcp_tool_use" {
                    if s(&b["id"]).is_empty() {
                        add(
                            format!("{path}.id"),
                            "hosted_tool_id_missing",
                            "Claude hosted-tool call has no id for pairing it with its result"
                                .into(),
                            false,
                        );
                    }
                    if k == "mcp" && (s(&b["name"]).is_empty() || s(&b["server_name"]).is_empty()) {
                        add(path.clone(),"mcp_identity_missing","Claude MCP tool use must include both name and server_name for Responses MCP mapping".into(),false);
                    }
                    if !b["caller"].is_null() {
                        add(format!("{path}.caller"),"hosted_tool_caller_unrepresentable","OpenAI Responses web_search_call and mcp_call items cannot preserve Claude's hosted-tool caller provenance".into(),false);
                    }
                    if !stream {
                        if k == "mcp" && !b["input"].is_object() {
                            add(
                                format!("{path}.input"),
                                "mcp_arguments_unrepresentable",
                                "Claude MCP input must be a JSON object".into(),
                                false,
                            );
                        } else if k == "web_search" {
                            if let Err(e) = normalize_web_action(&b["input"]) {
                                add(
                                    format!("{path}.input"),
                                    "web_search_action_unrepresentable",
                                    e.to_string(),
                                    false,
                                );
                            }
                        }
                    }
                } else if s(&b["tool_use_id"]).is_empty() {
                    add(
                        format!("{path}.tool_use_id"),
                        "hosted_tool_id_missing",
                        "Claude hosted-tool result has no tool_use_id for pairing it with its call"
                            .into(),
                        false,
                    );
                }
                if k == "mcp" && typ == "mcp_tool_result" {
                    let (failed, code) = result_failure(b);
                    match mcp_output(&b["content"],if failed{&code}else{""}){Err(e)=>add(format!("{path}.content"),"mcp_result_unrepresentable",e.to_string(),false),Ok((_,true))=>add(format!("{path}.content"),"mcp_text_result_normalized","Claude's single MCP text block is normalized to a Responses output string".into(),true),_=>{}}
                }
            }
            if typ == "redacted_thinking" && !s(&b["data"]).is_empty() {
                add(
                    format!("{path}.data"),
                    "continuation_state_lost",
                    "Claude encrypted thinking state cannot be represented by the target response"
                        .into(),
                    false,
                );
            }
        }
    } else if from == Format::OpenAiResponses {
        for (i, o) in arr(&v["output"]).iter().enumerate() {
            let typ = s(&o["type"]);
            if !response_hosted(typ) {
                continue;
            }
            let path = format!("output[{i}]");
            let k = kind(from, o);
            if to != Format::Claude || !matches!(k, "web_search" | "mcp") {
                add(
                    path,
                    "hosted_tool_unrepresentable",
                    format!(
                        "{to} cannot losslessly represent OpenAI Responses hosted-tool output {typ:?}"
                    ),
                    false,
                );
                continue;
            }
            if s(&o["id"]).is_empty() {
                add(
                    format!("{path}.id"),
                    "hosted_tool_id_missing",
                    "hosted-tool output has no id for pairing the call with its result".into(),
                    false,
                );
            }
            if k == "mcp" {
                if s(&o["name"]).is_empty() || s(&o["server_label"]).is_empty() {
                    add(path.clone(),"mcp_identity_missing","Responses MCP output must include both name and server_label for Claude MCP mapping".into(),false);
                }
                if !s(&o["approval_request_id"]).is_empty() {
                    add(format!("{path}.approval_request_id"),"mcp_approval_state_unrepresentable","Claude MCP response blocks cannot preserve a Responses approval_request_id".into(),false);
                }
                if let Err(e) = mcp_input(&o["arguments"]) {
                    add(
                        format!("{path}.arguments"),
                        "mcp_arguments_unrepresentable",
                        e.to_string(),
                        false,
                    );
                }
                if !o["output"].is_null() && !o["error"].is_null() {
                    add(
                        path.clone(),
                        "mcp_result_ambiguous",
                        "Responses MCP output contains both output and error".into(),
                        false,
                    );
                }
                for field in ["output", "error"] {
                    if !o[field].is_null() && !o[field].is_string() {
                        add(
                            format!("{path}.{field}"),
                            "mcp_result_unrepresentable",
                            "Responses MCP output/error must be a JSON string".into(),
                            false,
                        );
                    }
                }
            } else if let Err(e) = web_input(&o["action"]) {
                add(
                    format!("{path}.action"),
                    "web_search_action_unrepresentable",
                    e.to_string(),
                    false,
                );
            }
            let status = s(&o["status"]);
            if !matches!(status, "" | "in_progress" | "completed" | "failed") {
                add(
                    format!("{path}.status"),
                    "hosted_tool_status_unrepresentable",
                    format!("Claude cannot preserve hosted-tool status {status:?}"),
                    false,
                );
            }
            if status == "failed"
                && o["error"].is_null()
                && o["output"].is_null()
                && o["results"].is_null()
            {
                add(
                    format!("{path}.status"),
                    "hosted_tool_error_missing",
                    "failed hosted-tool output has no error or output that Claude can preserve"
                        .into(),
                    false,
                );
            }
        }
    } else if from == Format::Gemini {
        for (i, c) in arr(&v["candidates"]).iter().enumerate() {
            let m = &c["groundingMetadata"];
            if m.is_null() {
                continue;
            }
            let path = format!("candidates[{i}].groundingMetadata");
            if !arr(&m["webSearchQueries"]).is_empty() && to != Format::OpenAiResponses {
                add(format!("{path}.webSearchQueries"),"web_search_call_unrepresentable","Gemini grounding confirms a hosted web search, but the target converter cannot produce an OpenAI Responses web_search_call item".into(),false);
            }
            inspect_grounding(&c["content"], m, &path, &mut add);
            if [
                "webSearchQueries",
                "retrievalQueries",
                "searchEntryPoint",
                "retrievalMetadata",
                "sourceFlaggingUris",
                "googleMapsWidgetContextToken",
            ]
            .iter()
            .any(|key| {
                m.get(key)
                    .is_some_and(|v| !v.is_null() && v != "" && v != &json!([]))
            }) {
                add(path,"hosted_tool_metadata_reduced","Gemini grounding citations are preserved, but provider-specific search metadata has no target-protocol equivalent".into(),true);
            }
        }
    }
    ds
}
fn inspect_grounding(
    content: &Value,
    m: &Value,
    path: &str,
    add: &mut impl FnMut(String, &str, String, bool),
) {
    if m["groundingSupports"].is_null() {
        return;
    }
    let Some(chunks) = m["groundingChunks"].as_array() else {
        add(
            format!("{path}.groundingChunks"),
            "grounding_source_invalid",
            "Gemini grounding chunks are missing or cannot be decoded".into(),
            false,
        );
        return;
    };
    let Some(supports) = m["groundingSupports"].as_array() else {
        add(
            format!("{path}.groundingSupports"),
            "grounding_citation_invalid",
            "Gemini grounding supports cannot be decoded: expected array".into(),
            false,
        );
        return;
    };
    let parts = arr(&content["parts"]);
    let texts: Vec<_> = parts
        .iter()
        .enumerate()
        .filter(|(_, p)| !s(&p["text"]).is_empty() && p["thought"] != true)
        .collect();
    for (i, support) in supports.iter().enumerate() {
        let seg = &support["segment"];
        let sp = format!("{path}.groundingSupports[{i}].segment");
        let valid = arr(&support["groundingChunkIndices"]).iter().any(|idx| {
            chunks.get(n(idx) as usize).is_some_and(|c| {
                let src = if c["web"].is_null() {
                    &c["retrievedContext"]
                } else {
                    &c["web"]
                };
                !s(&src["uri"]).is_empty()
            })
        });
        if !valid {
            add(
                format!("{path}.groundingSupports[{i}].groundingChunkIndices"),
                "grounding_source_invalid",
                "Gemini grounding support does not reference a valid source URI".into(),
                false,
            );
            continue;
        }
        let pi = if !seg["partIndex"].is_null() {
            n(&seg["partIndex"])
        } else if texts.len() == 1 {
            texts[0].0 as i64
        } else {
            add(format!("{sp}.partIndex"),"grounding_part_ambiguous","Gemini grounding omitted partIndex while multiple text parts are present, so citation placement is ambiguous".into(),false);
            continue;
        };
        let Some(p) = parts
            .get(pi as usize)
            .filter(|p| !s(&p["text"]).is_empty() && p["thought"] != true)
        else {
            add(
                format!("{sp}.partIndex"),
                "grounding_part_invalid",
                format!("Gemini grounding references non-text part {pi}"),
                false,
            );
            continue;
        };
        let text = s(&p["text"]);
        let start = n(&seg["startIndex"]);
        let end = n(&seg["endIndex"]);
        if start < 0
            || end <= start
            || end as usize > text.len()
            || !text.is_char_boundary(start as usize)
            || !text.is_char_boundary(end as usize)
        {
            add(sp,"grounding_offset_invalid","Gemini grounding byte offsets do not identify valid UTF-8 boundaries in the referenced part".into(),false);
            continue;
        }
        if !s(&seg["text"]).is_empty() && text[start as usize..end as usize] != *s(&seg["text"]) {
            add(
                format!("{sp}.text"),
                "grounding_text_mismatch",
                "Gemini grounding segment text does not match the referenced part range".into(),
                false,
            );
        }
    }
}
pub(crate) struct Set {
    items: Vec<(usize, Value)>,
    regular: Vec<usize>,
    length: usize,
}
pub(crate) fn extract(from: Format, v: &Value) -> (Cow<'_, Value>, Set) {
    let mut set = Set {
        items: vec![],
        regular: vec![],
        length: 0,
    };
    let field = match from {
        Format::Claude => "content",
        Format::OpenAiResponses => "output",
        _ => "",
    };
    if !field.is_empty() {
        let is_hosted = |item: &Value| {
            if from == Format::Claude {
                claude_hosted(s(&item["type"]).trim())
            } else {
                response_hosted(s(&item["type"]))
            }
        };
        if let Some(items) = v[field].as_array() {
            set.length = items.len();
            if !items.iter().any(is_hosted) {
                // attach returns immediately for an empty hosted set, so
                // ordinary items need no copy or position vector here.
                return (Cow::Borrowed(v), set);
            }
        }
        let mut out = v.clone();
        // Non-array/missing content keeps the old extraction semantics: an
        // owned response whose content/output is replaced with an empty array.
        let items = out[field]
            .take()
            .as_array_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        let mut regular = vec![];
        for (i, item) in items.into_iter().enumerate() {
            if is_hosted(&item) {
                set.items.push((i, item));
            } else {
                set.regular.push(i);
                regular.push(item);
            }
        }
        out[field] = Value::Array(regular);
        return (Cow::Owned(out), set);
    } else if from == Format::Gemini {
        let mut queries = vec![];
        for c in arr(&v["candidates"]) {
            for q in arr(&c["groundingMetadata"]["webSearchQueries"]) {
                let q = s(q).trim();
                if !q.is_empty() && !queries.contains(&q) {
                    queries.push(q);
                }
            }
        }
        if !queries.is_empty() {
            set.length = 2;
            set.regular = vec![0];
            set.items.push((1,json!({"type":"googleSearch","id":new_id("ws_"),"status":"completed","action":{"type":"search","queries":queries}})));
        }
        return (Cow::Borrowed(v), set);
    }
    (Cow::Owned(v.clone()), set)
}
pub(crate) fn attach(
    from: Format,
    to: Format,
    out: &mut Value,
    set: Set,
) -> Result<Vec<Diagnostic>, Error> {
    let mut ds = vec![];
    if set.items.is_empty() {
        return Ok(ds);
    }
    let mut add = |path: String, code: &str, msg: String, w: bool| {
        ds.push(diagnostic(from, to, path, code, msg, w))
    };
    let mut hosted: Vec<(usize, Vec<Value>)> = vec![];
    let mut by_id = BTreeMap::<String, usize>::new();
    for (i, (pos, item)) in set.items.iter().enumerate() {
        let path = format!("hosted_tools[{i}]");
        let native = s(&item["type"]);
        let k = if from == Format::Gemini {
            "web_search"
        } else {
            kind(from, item)
        };
        if !matches!(to, Format::OpenAiResponses | Format::Claude) {
            add(
                path,
                "hosted_tool_event_omitted",
                format!("{to} cannot represent hosted-tool response {native:?}"),
                true,
            );
            continue;
        }
        if from == to {
            hosted.push((*pos, vec![item.clone()]));
            continue;
        }
        if to == Format::OpenAiResponses {
            let typ = match k {
                "web_search" => "web_search_call",
                "mcp" => "mcp_call",
                _ => {
                    add(
                        path,
                        "hosted_tool_unrepresentable",
                        format!("OpenAI Responses has no lossless response mapping for {native:?}"),
                        false,
                    );
                    continue;
                }
            };
            if native.ends_with("_tool_result") {
                let id = s(&item["tool_use_id"]);
                let Some(index) = by_id.get(id).copied() else {
                    add(
                        format!("{path}.tool_use_id"),
                        "hosted_tool_result_orphaned",
                        format!("hosted-tool result references unknown call {id:?}"),
                        false,
                    );
                    continue;
                };
                let target = &mut hosted[index].1[0];
                let (failed, code) = result_failure(item);
                target["status"] = json!(if failed { "failed" } else { "completed" });
                if k == "mcp" {
                    match mcp_output(&item["content"], if failed { &code } else { "" }) {
                        Err(e) => {
                            add(
                                format!("{path}.content"),
                                "mcp_result_unrepresentable",
                                e.to_string(),
                                false,
                            );
                            continue;
                        }
                        Ok((value, normalized)) => {
                            target[if failed { "error" } else { "output" }] = value;
                            target.as_object_mut().unwrap().remove(if failed {
                                "output"
                            } else {
                                "error"
                            });
                            if normalized {
                                add(format!("{path}.content"),"mcp_text_result_normalized","Claude's single MCP text block was normalized to a Responses output string".into(),true);
                            }
                        }
                    }
                } else if !item["content"].is_null() {
                    add(format!("{path}.content"),"web_search_result_omitted","Claude web-search result content is provider-private and has no field on an OpenAI Responses web_search_call; completion status and citations remain available".into(),true);
                }
                continue;
            }
            let status = if from == Format::Gemini {
                "completed"
            } else {
                "in_progress"
            };
            let mut target = json!({"type":typ,"id":s(&item["id"]),"status":status});
            if k == "web_search" {
                let action = if from == Format::Claude {
                    &item["input"]
                } else {
                    &item["action"]
                };
                target["action"] = normalize_web_action(action)
                    .map_err(|e| Error::message(format!("{path}.action: {e}")))?;
            } else {
                if s(&item["name"]).trim().is_empty() || s(&item["server_name"]).trim().is_empty() {
                    add(path,"mcp_identity_missing","Claude MCP output requires both name and server_name for Responses mapping".into(),false);
                    continue;
                }
                if !item["input"].is_object() {
                    add(
                        format!("{path}.input"),
                        "mcp_arguments_unrepresentable",
                        "Claude MCP input must be a JSON object".into(),
                        false,
                    );
                    continue;
                }
                target["name"] = item["name"].clone();
                target["server_label"] = item["server_name"].clone();
                target["arguments"] = json!(go_json(&item["input"]));
            }
            if !s(&item["id"]).is_empty() {
                by_id.insert(s(&item["id"]).into(), hosted.len());
            }
            hosted.push((*pos, vec![target]));
            add(path,"hosted_tool_result_approximated","hosted-tool execution is preserved, but provider-specific result fields may differ".into(),true);
        } else {
            if from == Format::OpenAiResponses && k == "web_search" {
                add(path,"web_search_response_unrepresentable","Responses web-search execution cannot reconstruct Claude's required encrypted web_search_tool_result continuation state".into(),false);
                continue;
            }
            let name = match k {
                "web_search" => "web_search",
                "web_fetch" => "web_fetch",
                "mcp" => s(&item["name"]),
                _ => "",
            };
            if name.is_empty() {
                add(
                    path,
                    "hosted_tool_unrepresentable",
                    format!("Claude has no lossless response mapping for {native:?}"),
                    false,
                );
                continue;
            }
            let input = if k == "web_search" {
                match web_input(&item["action"]) {
                    Ok(v) => v,
                    Err(e) => {
                        add(
                            format!("{path}.action"),
                            "web_search_action_unrepresentable",
                            e.to_string(),
                            false,
                        );
                        continue;
                    }
                }
            } else if k == "mcp" {
                match mcp_input(&item["arguments"]) {
                    Ok(v) => v,
                    Err(e) => {
                        add(
                            format!("{path}.arguments"),
                            "mcp_arguments_unrepresentable",
                            e.to_string(),
                            false,
                        );
                        continue;
                    }
                }
            } else {
                item.get("action").cloned().unwrap_or(json!({}))
            };
            let mut call = json!({"type":if k=="mcp"{"mcp_tool_use"}else{"server_tool_use"},"id":s(&item["id"]),"name":name,"input":input});
            if !s(&item["server_label"]).is_empty() {
                call["server_name"] = item["server_label"].clone();
            }
            if !item["caller"].is_null() {
                call["caller"] = item["caller"].clone();
            }
            let mut blocks = vec![call];
            let failed = s(&item["status"]) == "failed" || !item["error"].is_null();
            let result = if k == "mcp" {
                if failed && !item["error"].is_null() {
                    &item["error"]
                } else {
                    &item["output"]
                }
            } else if !item["results"].is_null() {
                &item["results"]
            } else {
                &item["sources"]
            };
            if !result.is_null() {
                if k == "mcp" && !result.is_string() {
                    add(
                        format!("{path}.output"),
                        "mcp_result_unrepresentable",
                        "Responses MCP output/error must be a JSON string".into(),
                        false,
                    );
                    continue;
                }
                blocks.push(json!({"type":format!("{k}_tool_result"),"tool_use_id":s(&item["id"]),"content":result,"is_error":failed}));
            } else if k == "mcp" && matches!(s(&item["status"]), "completed" | "failed") {
                add(
                    path.clone(),
                    "mcp_result_missing",
                    format!(
                        "Responses MCP output has status {:?} but no output or error",
                        s(&item["status"])
                    ),
                    false,
                );
            }
            hosted.push((*pos, blocks));
            add(path,"hosted_tool_result_approximated","hosted-tool execution is preserved, but provider-specific result fields may differ".into(),true);
        }
    }
    if hosted.is_empty() {
        return Ok(ds);
    }
    let field = if to == Format::Claude {
        "content"
    } else {
        "output"
    };
    let regular = arr(&out[field]);
    let mut merged = vec![];
    if regular.len() == set.regular.len() {
        for p in 0..set.length {
            for (_, blocks) in hosted.iter().filter(|(pos, _)| *pos == p) {
                merged.extend(blocks.iter().cloned());
            }
            if let Some(i) = set.regular.iter().position(|x| *x == p) {
                merged.push(regular[i].clone());
            }
        }
    } else {
        let first = set.regular.first().copied().unwrap_or(usize::MAX);
        let last = set.regular.last().copied().unwrap_or(usize::MAX);
        let exact = set.regular.is_empty() || hosted.iter().all(|(p, _)| *p < first || *p > last);
        if exact {
            for (p, b) in &hosted {
                if set.regular.is_empty() || *p < first {
                    merged.extend(b.iter().cloned());
                }
            }
            merged.extend(regular.iter().cloned());
            for (p, b) in &hosted {
                if !set.regular.is_empty() && *p > last {
                    merged.extend(b.iter().cloned());
                }
            }
        } else {
            for (_, b) in &hosted {
                merged.extend(b.iter().cloned());
            }
            merged.extend(regular.iter().cloned());
            add(
                field.into(),
                "hosted_tool_order_unrepresentable",
                format!(
                    "hosted-tool {} were interleaved with content that the target converter coalesced, so their original order cannot be reconstructed",
                    if to == Format::Claude {
                        "blocks"
                    } else {
                        "items"
                    }
                ),
                false,
            );
        }
    }
    out[field] = json!(merged);
    Ok(ds)
}
