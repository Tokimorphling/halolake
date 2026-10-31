use super::*;
/// A provider-executed tool call represented by a Responses lifecycle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostedToolStart {
    pub kind: String,
    pub id: String,
    pub name: String,
    pub action: Value,
    pub caller: Value,
    pub server_label: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostedToolResult {
    pub kind: String,
    pub id: String,
    pub result: Value,
    pub error_code: String,
    pub is_error: bool,
}
impl ResponseStreamState {
    fn responses_state(&mut self) -> Option<&mut ChatResponses> {
        self.steps.iter_mut().find_map(|s| {
            if let Step::ChatResponses(s) = s {
                Some(s)
            } else {
                None
            }
        })
    }
    /// Start a native Responses hosted-tool lifecycle outside the Chat pivot.
    pub fn start_hosted_tool(&mut self, start: &HostedToolStart) -> Result<Vec<Value>, Error> {
        let s = self.responses_state().ok_or_else(|| {
            Error::message("hosted tool requires a Chat-to-Responses stream state")
        })?;
        normalize(s.start_hosted(start)?)
    }
    /// Complete a previously started hosted-tool call.
    pub fn complete_hosted_tool(&mut self, result: &HostedToolResult) -> Result<Vec<Value>, Error> {
        let s = self.responses_state().ok_or_else(|| {
            Error::message("hosted tool requires a Chat-to-Responses stream state")
        })?;
        normalize(s.complete_hosted(result)?)
    }
}
fn normalize(v: Vec<Value>) -> Result<Vec<Value>, Error> {
    v.iter()
        .map(|v| crate::wire::normalize_stream(Format::OpenAiResponses, v))
        .collect()
}
#[derive(Debug, Default)]
struct Pending {
    kind: String,
    id: String,
    name: String,
    server: String,
    caller: Value,
    start_input: Value,
    input: String,
}
/// Consumes Claude server-tool blocks before ordinary stream conversion.
///
/// Call `convert` before `ResponseStreamState::convert_chunk`; when `handled`
/// is true, use the returned events and do not pass the source event to the pivot.
#[derive(Debug, Default)]
pub struct ClaudeHostedStreamBridge {
    pending: BTreeMap<i64, Pending>,
}
impl ClaudeHostedStreamBridge {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn convert(
        &mut self,
        event: &Value,
        state: &mut ResponseStreamState,
    ) -> Result<(Vec<Value>, bool), Error> {
        if state.responses_state().is_none() {
            return Ok((vec![], false));
        }
        let index = n(&event["index"]);
        match st(&event["type"]) {
            "content_block_start" => {
                let b = &event["content_block"];
                let typ = st(&b["type"]).trim();
                match typ {
                    "server_tool_use" | "mcp_tool_use" => {
                        if self.pending.contains_key(&index) {
                            return Err(Error::message(format!(
                                "duplicate Claude hosted-tool content block index {index}"
                            )));
                        }
                        if typ == "mcp_tool_use"
                            && (st(&b["name"]).trim().is_empty()
                                || st(&b["server_name"]).trim().is_empty())
                        {
                            return Err(Error::message(
                                "Claude MCP tool use must include name and server_name",
                            ));
                        }
                        call_type(typ, st(&b["name"]))?;
                        let input = b["input"].clone();
                        self.pending.insert(
                            index,
                            Pending {
                                kind: typ.into(),
                                id: st(&b["id"]).into(),
                                name: st(&b["name"]).into(),
                                server: st(&b["server_name"]).into(),
                                caller: b["caller"].clone(),
                                start_input: if input == json!({}) {
                                    Value::Null
                                } else {
                                    input
                                },
                                input: String::new(),
                            },
                        );
                        Ok((vec![], true))
                    }
                    "web_search_tool_result" | "mcp_tool_result" => {
                        let kind = if typ == "mcp_tool_result" {
                            "mcp_call"
                        } else {
                            "web_search_call"
                        };
                        let mut code = st(&b["error_code"]).trim();
                        if code.is_empty() && st(&b["content"]["type"]).trim().ends_with("_error") {
                            code = st(&b["content"]["error_code"]).trim();
                        }
                        let events = state.complete_hosted_tool(&HostedToolResult {
                            kind: kind.into(),
                            id: st(&b["tool_use_id"]).into(),
                            result: if kind == "mcp_call" {
                                b["content"].clone()
                            } else {
                                Value::Null
                            },
                            error_code: code.into(),
                            is_error: b["is_error"].as_bool() == Some(true),
                        })?;
                        Ok((events, true))
                    }
                    _ => Ok((vec![], false)),
                }
            }
            "content_block_delta" => {
                if let Some(p) = self.pending.get_mut(&index) {
                    if st(&event["delta"]["type"]) == "input_json_delta" {
                        p.input.push_str(st(&event["delta"]["partial_json"]));
                    }
                    Ok((vec![], true))
                } else {
                    Ok((vec![], false))
                }
            }
            "content_block_stop" => {
                let Some(p) = self.pending.remove(&index) else {
                    return Ok((vec![], false));
                };
                let action = if !p.input.is_empty() {
                    serde_json::from_str(&p.input).map_err(|e| {
                        Error::message(format!("invalid hosted-tool input JSON: {e}"))
                    })?
                } else if !p.start_input.is_null() {
                    p.start_input
                } else {
                    json!({})
                };
                let kind = call_type(&p.kind, &p.name)?;
                let events = state.start_hosted_tool(&HostedToolStart {
                    kind: kind.into(),
                    id: p.id,
                    name: p.name,
                    action,
                    caller: p.caller,
                    server_label: p.server,
                })?;
                Ok((events, true))
            }
            _ => Ok((vec![], false)),
        }
    }
}
fn call_type(typ: &str, name: &str) -> Result<&'static str, Error> {
    if typ == "mcp_tool_use" {
        return Ok("mcp_call");
    }
    match name.trim() {
        "web_search" => Ok("web_search_call"),
        "code_execution" => Err(Error::message(
            "Claude code_execution has no valid OpenAI Responses mapping without a container_id",
        )),
        "web_fetch" => Err(Error::message(
            "Claude web_fetch has no valid OpenAI Responses hosted-tool mapping",
        )),
        _ => Err(Error::message(format!(
            "unknown Claude server tool {name:?} cannot be represented as an OpenAI Responses hosted tool"
        ))),
    }
}
/// Accumulates Gemini grounding search queries and emits one complete hosted call.
#[derive(Debug, Default)]
pub struct GeminiHostedStreamBridge {
    queries: Vec<String>,
    seen: BTreeSet<String>,
}
impl GeminiHostedStreamBridge {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn observe(&mut self, response: &Value) {
        for c in arr(&response["candidates"]) {
            for q in arr(&c["groundingMetadata"]["webSearchQueries"]) {
                let q = st(q).trim();
                if !q.is_empty() && self.seen.insert(q.into()) {
                    self.queries.push(q.into());
                }
            }
        }
    }
    pub fn finalize(&mut self, state: &mut ResponseStreamState) -> Result<Vec<Value>, Error> {
        if self.queries.is_empty() {
            return Ok(vec![]);
        }
        let id = uid("ws_");
        let mut out = state.start_hosted_tool(&HostedToolStart {
            kind: "web_search_call".into(),
            id: id.clone(),
            action: json!({"type":"search","queries":self.queries}),
            ..Default::default()
        })?;
        out.extend(state.complete_hosted_tool(&HostedToolResult {
            kind: "web_search_call".into(),
            id,
            ..Default::default()
        })?);
        Ok(out)
    }
}
pub(super) fn web_action(v: &Value) -> Result<Value, Error> {
    if !v.is_object() {
        return Err(Error::message(
            "decode Responses web-search action: expected object",
        ));
    }
    let mut typ = st(&v["type"]).trim();
    let q = st(&v["query"]).trim();
    let queries = arr(&v["queries"])
        .iter()
        .map(|v| st(v).trim())
        .collect::<Vec<_>>();
    for (i, q) in queries.iter().enumerate() {
        if q.is_empty() {
            return Err(Error::message(format!(
                "Responses web-search action queries[{i}] must not be empty"
            )));
        }
    }
    if typ.is_empty() && (!q.is_empty() || !queries.is_empty()) {
        typ = "search";
    }
    let url = st(&v["url"]).trim();
    let pattern = st(&v["pattern"]).trim();
    match typ {
        "search" => {
            if q.is_empty() && queries.is_empty() {
                return Err(Error::message(
                    "Responses web-search action \"search\" requires query or queries",
                ));
            }
            let mut out = json!({"type":"search"});
            if !q.is_empty() {
                out["query"] = json!(q);
            }
            if !queries.is_empty() {
                out["queries"] = json!(queries);
            }
            if let Some(s) = v.get("sources") {
                if !s.is_array() && !s.is_null() {
                    return Err(Error::message(
                        "Responses web-search action sources must be an array",
                    ));
                }
                out["sources"] = s.clone();
            }
            Ok(out)
        }
        "open_page" => {
            if url.is_empty() {
                return Err(Error::message(
                    "Responses web-search action \"open_page\" requires url",
                ));
            }
            Ok(json!({"type":"open_page","url":url}))
        }
        "find" | "find_in_page" => {
            if url.is_empty() || pattern.is_empty() {
                return Err(Error::message(format!(
                    "Responses web-search action {typ:?} requires url and pattern"
                )));
            }
            Ok(json!({"type":"find_in_page","url":url,"pattern":pattern}))
        }
        _ => Err(Error::message(format!(
            "unsupported Responses web-search action type {typ:?}"
        ))),
    }
}
