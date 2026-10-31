use super::event::{EventSink, StreamEvent, ValueSink};
use super::*;

/// The generated Gemini wire event. Text borrows the input chunk; arguments and
/// the small immutable tool metadata are owned only when a call is completed.
#[derive(Debug, Serialize)]
pub(super) struct GeminiEvent<'a> {
    candidates: Vec<GeminiCandidate<'a>>,
    #[serde(rename = "usageMetadata")]
    usage: GeminiStreamUsage<'a>,
}
#[derive(Debug, Serialize)]
struct GeminiCandidate<'a> {
    index: i64,
    content: GeminiContent<'a>,
    #[serde(rename = "finishReason", skip_serializing_if = "Option::is_none")]
    finish_reason: Option<&'static str>,
    #[serde(rename = "safetyRatings")]
    safety_ratings: [(); 0],
}
#[derive(Debug, Serialize)]
struct GeminiContent<'a> {
    role: &'static str,
    parts: Vec<GeminiPart<'a>>,
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum GeminiPart<'a> {
    Text {
        text: Cow<'a, str>,
    },
    Call {
        #[serde(rename = "functionCall")]
        call: GeminiFunction,
        #[serde(rename = "thoughtSignature", skip_serializing_if = "String::is_empty")]
        thought_signature: String,
    },
}
#[derive(Debug, Serialize)]
struct GeminiFunction {
    #[serde(skip_serializing_if = "String::is_empty")]
    id: String,
    name: String,
    #[serde(serialize_with = "serialize_arguments")]
    args: Value,
}
fn serialize_arguments<S: serde::Serializer>(
    args: &Value,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    crate::wire::normalized_view("any", args).serialize(serializer)
}
#[derive(Debug)]
enum GeminiStreamUsage<'a> {
    Source(&'a Value),
    Estimated(i64),
}
impl Serialize for GeminiStreamUsage<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Source(usage) => {
                let mut native = gemini_usage(usage);
                let billing = &usage["billing_usage"];
                if billing["gemini_usage_metadata"].is_null()
                    || (st(&billing["source"]) != "gemini_chat"
                        && st(&billing["semantic"]) != "gemini")
                {
                    // Streaming uses the current wire counter verbatim. The
                    // non-stream response renderer fills a missing total.
                    native["totalTokenCount"] = n(&usage["total_tokens"]).into();
                }
                crate::wire::normalized_view("GeminiUsageMetadata", &native).serialize(serializer)
            }
            Self::Estimated(prompt) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(9))?;
                map.serialize_entry("promptTokenCount", prompt)?;
                map.serialize_entry("toolUsePromptTokenCount", &0)?;
                map.serialize_entry("candidatesTokenCount", &0)?;
                map.serialize_entry("totalTokenCount", prompt)?;
                map.serialize_entry("thoughtsTokenCount", &0)?;
                map.serialize_entry("cachedContentTokenCount", &0)?;
                for key in [
                    "promptTokensDetails",
                    "toolUsePromptTokensDetails",
                    "candidatesTokensDetails",
                ] {
                    map.serialize_entry(key, &Option::<()>::None)?;
                }
                map.end()
            }
        }
    }
}
#[derive(Debug, Default)]
struct ChatTool {
    id: String,
    name: String,
    args: String,
    thought_signature: String,
    emitted: bool,
    choice: i64,
}
#[derive(Debug, Default)]
pub(super) struct ChatGemini {
    tools: Vec<ChatTool>,
    by_index: BTreeMap<(i64, i64), usize>,
    by_id: BTreeMap<i64, BTreeMap<String, usize>>,
    finished: BTreeSet<i64>,
    seen: BTreeSet<i64>,
    usage_emitted: bool,
    finalized: bool,
    pub usage: Option<Value>,
}
fn function_arguments(s: &str) -> Value {
    if s.trim().is_empty() || s.trim() == "null" {
        return json!({});
    }
    serde_json::from_str::<Value>(s)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"arguments":s}))
}
fn candidate<'a>(
    index: i64,
    parts: Vec<GeminiPart<'a>>,
    finish: Option<&'static str>,
) -> GeminiCandidate<'a> {
    GeminiCandidate {
        index,
        content: GeminiContent {
            role: "model",
            parts,
        },
        finish_reason: finish,
        safety_ratings: [],
    }
}
fn gemini_finish(s: &str) -> &'static str {
    match s.trim() {
        "length" => "MAX_TOKENS",
        "content_filter" => "SAFETY",
        _ => "STOP",
    }
}
fn gemini_response<'a>(
    ctx: &Context,
    candidates: Vec<GeminiCandidate<'a>>,
    usage: Option<&'a Value>,
) -> GeminiEvent<'a> {
    GeminiEvent {
        candidates,
        usage: usage
            .map(GeminiStreamUsage::Source)
            .unwrap_or(GeminiStreamUsage::Estimated(ctx.estimate_prompt_tokens)),
    }
}
impl ChatGemini {
    fn append(&mut self, choice: i64, v: &Value) -> Result<(), Error> {
        let index = n(&v["index"]);
        if index < 0 {
            return Err(Error::message(format!(
                "OpenAI chat choice {choice} has negative tool-call index {index}"
            )));
        }
        let id = st(&v["id"]).trim();
        let mut t = self
            .by_id
            .get(&choice)
            .and_then(|tools| tools.get(id))
            .copied()
            .or_else(|| self.by_index.get(&(choice, index)).copied());
        if t.is_some_and(|i| {
            !id.is_empty() && !self.tools[i].id.is_empty() && self.tools[i].id != id
        }) {
            t = None;
        }
        let i = t.unwrap_or_else(|| {
            let i = self.tools.len();
            self.tools.push(ChatTool {
                choice,
                ..Default::default()
            });
            i
        });
        self.by_index.insert((choice, index), i);
        let t = &mut self.tools[i];
        if t.emitted {
            return Err(Error::message(format!(
                "OpenAI chat choice {choice} tool-call index {index} received data after completion"
            )));
        }
        if !id.is_empty() {
            if t.id != id {
                t.id = id.into();
            }
            let ids = self.by_id.entry(choice).or_default();
            if ids.get(id) != Some(&i) {
                ids.insert(id.into(), i);
            }
        }
        let name = st(&v["function"]["name"]).trim();
        if !name.is_empty() {
            if !t.name.is_empty() && t.name != name {
                return Err(Error::message(format!(
                    "OpenAI chat choice {choice} tool-call index {index} changed name from {:?} to {name:?}",
                    t.name
                )));
            }
            if t.name != name {
                t.name = name.into();
            }
        }
        if let Some(signature) = v
            .get("thought_signature")
            .or_else(|| v.get("thoughtSignature"))
            .and_then(Value::as_str)
        {
            t.thought_signature.clear();
            t.thought_signature.push_str(signature);
        }
        t.args.push_str(st(&v["function"]["arguments"]));
        Ok(())
    }
    fn finish<'a>(
        &mut self,
        choice: i64,
        preserve_message_structure: bool,
    ) -> Result<Vec<GeminiPart<'a>>, Error> {
        let mut out = Vec::new();
        for t in self
            .tools
            .iter()
            .filter(|t| t.choice == choice && !t.emitted)
        {
            if t.name.is_empty() {
                return Err(Error::message(format!(
                    "OpenAI chat choice {choice} has a tool call without a function name"
                )));
            }
            out.push(GeminiPart::Call {
                call: GeminiFunction {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    args: function_arguments(&t.args),
                },
                thought_signature: if !preserve_message_structure {
                    String::new()
                } else if t.thought_signature.is_empty() {
                    "skip_thought_signature_validator".into()
                } else {
                    t.thought_signature.clone()
                },
            });
        }
        for t in self.tools.iter_mut().filter(|t| t.choice == choice) {
            t.emitted = true;
        }
        Ok(out)
    }
    pub fn convert(&mut self, ctx: &Context, v: &Value) -> Result<Vec<Value>, Error> {
        let mut sink = ValueSink::default();
        self.convert_into(ctx, v, &mut sink)?;
        Ok(sink.values)
    }
    pub fn convert_into(
        &mut self,
        ctx: &Context,
        v: &Value,
        sink: &mut dyn EventSink,
    ) -> Result<(), Error> {
        if self.finalized {
            return Err(Error::message(
                "OpenAI chat to Gemini stream received data after finalization",
            ));
        }
        let incoming = v.get("usage").filter(|v| !v.is_null());
        if let Some(u) = incoming {
            self.usage = Some(usage_from_chat(u));
        }
        let mut candidates = Vec::new();
        for c in arr(&v["choices"]) {
            let index = n(&c["index"]);
            self.seen.insert(index);
            let text = content(&c["delta"]["content"]);
            let tools = arr(&c["delta"]["tool_calls"]);
            let finish = st(&c["finish_reason"]).trim();
            if self.finished.contains(&index) {
                if !text.is_empty() || !tools.is_empty() {
                    return Err(Error::message(format!(
                        "OpenAI chat choice {index} received data after completion"
                    )));
                }
                continue;
            }
            for t in tools {
                self.append(index, t)?;
            }
            let mut parts = Vec::new();
            if !text.is_empty() {
                parts.push(GeminiPart::Text { text });
            }
            let f = if !finish.is_empty() {
                parts.extend(self.finish(index, ctx.options.preserve_message_structure)?);
                self.finished.insert(index);
                Some(gemini_finish(finish))
            } else {
                None
            };
            if !parts.is_empty() || f.is_some() {
                candidates.push(candidate(index, parts, f));
            }
        }
        if candidates.is_empty() {
            if incoming.is_some() && !self.finished.is_empty() {
                self.usage_emitted = true;
                return sink.emit(StreamEvent::Gemini(gemini_response(
                    ctx,
                    vec![],
                    self.usage.as_ref(),
                )));
            }
            return Ok(());
        }
        if incoming.is_some() {
            self.usage_emitted = true;
        }
        sink.emit(StreamEvent::Gemini(gemini_response(
            ctx, candidates, incoming,
        )))
    }
    pub fn finalize(&mut self, ctx: &Context) -> Result<Vec<Value>, Error> {
        let mut sink = ValueSink::default();
        self.finalize_into(ctx, &mut sink)?;
        Ok(sink.values)
    }
    pub fn finalize_into(&mut self, ctx: &Context, sink: &mut dyn EventSink) -> Result<(), Error> {
        if self.finalized {
            return Ok(());
        }
        let choices = self
            .seen
            .iter()
            .filter(|i| !self.finished.contains(i))
            .copied()
            .chain(self.tools.iter().filter(|t| !t.emitted).map(|t| t.choice))
            .collect::<BTreeSet<_>>();
        let mut candidates = Vec::new();
        for i in choices {
            candidates.push(candidate(
                i,
                self.finish(i, ctx.options.preserve_message_structure)?,
                Some("STOP"),
            ));
        }
        self.finalized = true;
        if candidates.is_empty() && (self.usage.is_none() || self.usage_emitted) {
            return Ok(());
        }
        self.usage_emitted = self.usage.is_some();
        sink.emit(StreamEvent::Gemini(gemini_response(
            ctx,
            candidates,
            self.usage.as_ref(),
        )))
    }
}
#[derive(Debug, Default)]
struct Partial {
    id: String,
    name: String,
    args: Value,
}
#[derive(Debug, Default)]
struct Grounding {
    rendered: String,
    parts: BTreeMap<usize, GroundingPart>,
    chunks: Vec<Value>,
}
#[derive(Debug, Default)]
struct GroundingPart {
    text: String,
    spans: Vec<(usize, usize, usize)>,
}
impl Grounding {
    fn append(&mut self, parts: &[Value], rendered: &str) {
        let mut cursor = 0;
        let base = self.rendered.len();
        for (index, p) in parts.iter().enumerate() {
            let text = st(&p["text"]);
            if text.is_empty() || p["thought"].as_bool() == Some(true) {
                continue;
            }
            let located = rendered
                .get(cursor..)
                .and_then(|s| s.find(text))
                .map(|offset| cursor + offset);
            if let Some(start) = located {
                cursor = start + text.len();
            }
            let part = self.parts.entry(index).or_default();
            let start = part.text.len();
            part.text.push_str(text);
            if text == "\n" {
                continue;
            }
            if let Some(pos) = located {
                part.spans.push((start, start + text.len(), base + pos));
            }
        }
        self.rendered.push_str(rendered);
    }
    fn annotations(
        &mut self,
        metadata: &Value,
        candidate: i64,
        seen: &mut BTreeSet<String>,
    ) -> Vec<Value> {
        self.chunks
            .extend(arr(&metadata["groundingChunks"]).iter().cloned());
        if self.chunks.is_empty() {
            return vec![];
        }
        let mut out = Vec::new();
        for support in arr(&metadata["groundingSupports"]) {
            let seg = &support["segment"];
            let idx = if let Some(i) = seg["partIndex"].as_u64() {
                Some(i as usize)
            } else {
                let indexes = self
                    .parts
                    .iter()
                    .filter(|(_, p)| !p.spans.is_empty())
                    .map(|(i, _)| *i)
                    .collect::<Vec<_>>();
                if indexes.len() == 1 {
                    Some(indexes[0])
                } else {
                    None
                }
            };
            let Some(part) = idx.and_then(|i| self.parts.get(&i)) else {
                continue;
            };
            let start = n(&seg["startIndex"]);
            let end = n(&seg["endIndex"]);
            if start < 0 || end <= start {
                continue;
            }
            let (start, end) = (start as usize, end as usize);
            let Some(source) = part.text.get(start..end) else {
                continue;
            };
            if !st(&seg["text"]).is_empty() && source != st(&seg["text"]) {
                continue;
            }
            let mut rendered_start = None;
            let mut rendered_end = None;
            for (a, b, r) in &part.spans {
                if rendered_start.is_none() && start >= *a && start < *b {
                    rendered_start = Some(r + start - a);
                }
                if end > *a && end <= *b {
                    rendered_end = Some(r + end - a);
                }
            }
            let (Some(a), Some(b)) = (rendered_start, rendered_end) else {
                continue;
            };
            if a >= b || self.rendered.get(a..b) != Some(source) {
                continue;
            }
            let (Some(prefix_a), Some(prefix_b)) = (self.rendered.get(..a), self.rendered.get(..b))
            else {
                continue;
            };
            let (a, b) = (prefix_a.chars().count(), prefix_b.chars().count());
            for index in arr(&support["groundingChunkIndices"]) {
                let Some(chunk) = index.as_u64().and_then(|i| self.chunks.get(i as usize)) else {
                    continue;
                };
                let source = if !chunk["web"].is_null() {
                    &chunk["web"]
                } else {
                    &chunk["retrievedContext"]
                };
                let url = st(&source["uri"]);
                if url.is_empty() || !seen.insert(format!("{candidate}:{a}:{b}:{url}")) {
                    continue;
                }
                out.push(json!({"type":"url_citation","url_citation":{"start_index":a,"end_index":b,"url":url,"title":st(&source["title"])}}));
            }
        }
        out
    }
}
#[derive(Debug)]
pub(super) struct GeminiChat {
    id: String,
    created: i64,
    saw_tool: bool,
    finished: bool,
    next: BTreeMap<i64, i64>,
    by_id: BTreeMap<i64, BTreeMap<String, i64>>,
    partial: BTreeMap<i64, Partial>,
    grounding: BTreeMap<i64, Grounding>,
    sent_annotations: BTreeSet<String>,
    pub usage: Option<Value>,
}
impl GeminiChat {
    pub fn new(o: &StreamOptions) -> Self {
        Self {
            id: if o.id.trim().is_empty() {
                uid("chatcmpl-")
            } else {
                o.id.trim().into()
            },
            created: if o.created_at == 0 {
                now()
            } else {
                o.created_at
            },
            saw_tool: false,
            finished: false,
            next: BTreeMap::new(),
            by_id: BTreeMap::new(),
            partial: BTreeMap::new(),
            grounding: BTreeMap::new(),
            sent_annotations: BTreeSet::new(),
            usage: None,
        }
    }
    fn model(ctx: &Context) -> &str {
        if ctx.channel_meta_attached {
            &ctx.upstream_model_name
        } else {
            ""
        }
    }
    fn terminal(&mut self, ctx: &Context) -> Value {
        self.finished = true;
        let mut v = chat_chunk(
            &self.id,
            Self::model(ctx),
            self.created,
            json!({}),
            Some(if self.saw_tool { "tool_calls" } else { "stop" }),
        );
        if let Some(u) = &self.usage {
            v["usage"] = u.clone();
        }
        v
    }
    pub fn finalize(&mut self, ctx: &Context) -> Result<Vec<Value>, Error> {
        if let Some((i, p)) = self.partial.first_key_value() {
            return Err(Error::message(format!(
                "Gemini stream ended with an incomplete function call for candidate {i} (id {:?}, name {:?})",
                p.id, p.name
            )));
        }
        if self.finished {
            Ok(vec![])
        } else {
            Ok(vec![self.terminal(ctx)])
        }
    }
    fn prepare(&mut self, index: i64, parts: &[Value]) -> Result<Vec<Value>, Error> {
        let mut out = Vec::new();
        for p in parts {
            let call = &p["functionCall"];
            if call.is_null()
                || (!self.partial.contains_key(&index)
                    && call.get("willContinue").is_none()
                    && arr(&call["partialArgs"]).is_empty())
            {
                out.push(p.clone());
                continue;
            }
            let current = self.partial.entry(index).or_insert_with(|| Partial {
                args: json!({}),
                ..Default::default()
            });
            let id = st(&call["id"]).trim();
            let name = st(&call["name"]).trim();
            if !id.is_empty() {
                if !current.id.is_empty() && current.id != id {
                    return Err(Error::message(format!(
                        "candidate {index} function call changed id from {:?} to {id:?}",
                        current.id
                    )));
                }
                current.id = id.into();
            }
            if !name.is_empty() {
                if !current.name.is_empty() && current.name != name {
                    return Err(Error::message(format!(
                        "candidate {index} function call changed name from {:?} to {name:?}",
                        current.name
                    )));
                }
                current.name = name.into();
            }
            for part in arr(&call["partialArgs"]) {
                let path = parse_path(st(&part["jsonPath"]))?;
                let value = ["stringValue", "numberValue", "boolValue", "nullValue"]
                    .into_iter()
                    .find_map(|k| {
                        part.get(k).map(|v| {
                            (
                                if k == "nullValue" {
                                    Value::Null
                                } else {
                                    v.clone()
                                },
                                k == "stringValue",
                            )
                        })
                    });
                if let Some((value, append)) = value {
                    set_path(&mut current.args, &path, value, append)?;
                }
            }
            if call["willContinue"].as_bool() == Some(true) {
                continue;
            }
            if current.name.is_empty() {
                return Err(Error::message(format!(
                    "candidate {index} completed a partial function call without a name"
                )));
            }
            let current = self.partial.remove(&index).unwrap();
            let mut p = p.clone();
            p["functionCall"] = json!({"id":current.id,"name":current.name,"args":current.args});
            out.push(p);
        }
        Ok(out)
    }
    pub fn convert(&mut self, ctx: &Context, v: &Value) -> Result<Vec<Value>, Error> {
        let incoming = usage_from_gemini(&v["usageMetadata"], ctx.estimate_prompt_tokens);
        let mut choices = Vec::new();
        let mut is_stop = false;
        let has_nonstop = arr(&v["candidates"])
            .iter()
            .any(|c| !st(&c["finishReason"]).is_empty() && st(&c["finishReason"]) != "STOP");
        let mut any_tool = false;
        for c in arr(&v["candidates"]) {
            let index = n(&c["index"]);
            let parts = self.prepare(index, arr(&c["content"]["parts"]))?;
            let mut text = Vec::new();
            let mut tools = Vec::new();
            let mut thought = false;
            let mut thought_signature = None;
            let mut finish = match c["finishReason"].as_str() {
                Some("STOP") => {
                    is_stop = true;
                    (self.saw_tool && ctx.options.preserve_message_structure)
                        .then_some("tool_calls")
                }
                Some("MAX_TOKENS") => Some("length"),
                Some(_) => Some("content_filter"),
                None => None,
            };
            for p in &parts {
                if !p["inlineData"].is_null() {
                    let mime = st(&p["inlineData"]["mimeType"]);
                    if mime.starts_with("image") {
                        text.push(format!(
                            "![image](data:{mime};base64,{})",
                            st(&p["inlineData"]["data"])
                        ));
                    }
                } else if !p["functionCall"].is_null() {
                    let call = &p["functionCall"];
                    let id = st(&call["id"]).trim();
                    let id = if id.is_empty() {
                        uid("call_")
                    } else {
                        id.into()
                    };
                    let ids = self.by_id.entry(index).or_default();
                    let stable = if let Some(i) = ids.get(id.as_str()) {
                        *i
                    } else {
                        let i = self.next.entry(index).or_default();
                        let n = *i;
                        *i += 1;
                        ids.insert(id.clone(), n);
                        n
                    };
                    let mut tool = json!({"index":stable,"id":id,"type":"function","function":{"name":st(&call["name"]),"arguments":crate::json::stringify(call.get("args").unwrap_or(&Value::Null))}});
                    if !st(&p["thoughtSignature"]).is_empty() {
                        tool["thoughtSignature"] = p["thoughtSignature"].clone();
                    }
                    tools.push(tool);
                } else if p["thought"].as_bool() == Some(true) {
                    thought = true;
                    text.push(st(&p["text"]).into());
                    if !st(&p["thoughtSignature"]).is_empty() {
                        thought_signature = Some(st(&p["thoughtSignature"]));
                    }
                } else if !p["executableCode"].is_null() {
                    text.push(format!(
                        "```{}\n{}\n```\n",
                        st(&p["executableCode"]["language"]),
                        st(&p["executableCode"]["code"])
                    ));
                } else if !p["codeExecutionResult"].is_null() {
                    text.push(format!(
                        "```output\n{}\n```\n",
                        st(&p["codeExecutionResult"]["output"])
                    ));
                } else if st(&p["text"]) != "\n" {
                    text.push(st(&p["text"]).into());
                }
            }
            let text = text.join("\n");
            let mut delta = json!({});
            delta[if thought {
                "reasoning_content"
            } else {
                "content"
            }] = json!(text);
            if let Some(signature) = thought_signature {
                delta["reasoning_signature"] = json!(signature);
            }
            if !tools.is_empty() {
                any_tool = true;
                finish = Some("tool_calls");
                delta["tool_calls"] = Value::Array(tools);
            }
            let g = self.grounding.entry(index).or_default();
            g.append(&parts, if thought { "" } else { &text });
            let annotations =
                g.annotations(&c["groundingMetadata"], index, &mut self.sent_annotations);
            if !annotations.is_empty() {
                delta["annotations"] = Value::Array(annotations);
            }
            choices.push(object([
                ("index", index.into()),
                ("delta", delta),
                (
                    "finish_reason",
                    finish.map(Value::from).unwrap_or(Value::Null),
                ),
            ]));
        }
        if any_tool {
            self.saw_tool = true;
            if !has_nonstop {
                for c in &mut choices {
                    if c["finish_reason"] == "tool_calls" {
                        c["finish_reason"] = Value::Null;
                    }
                }
            }
        }
        if let Some(u) = &incoming {
            self.usage = Some(u.clone());
        }
        if choices.iter().any(|c| !st(&c["finish_reason"]).is_empty()) {
            self.finished = true;
        }
        let mut response = json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":Self::model(ctx)});
        response["choices"] = Value::Array(choices);
        if let Some(u) = incoming {
            response["usage"] = u;
        }
        let mut out = vec![response];
        if is_stop && !self.finished {
            out.push(self.terminal(ctx));
        }
        Ok(out)
    }
}
#[derive(Debug)]
enum Path {
    Member(String),
    Index(usize),
}
fn parse_path(input: &str) -> Result<Vec<Path>, Error> {
    let path = input.trim();
    let bytes = path.as_bytes();
    let err = || {
        Error::message(format!(
            "unsupported Gemini partial argument path {input:?}"
        ))
    };
    if bytes.first() != Some(&b'$') {
        return Err(err());
    }
    let mut out = Vec::new();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                    i += 1;
                }
                let member = &path[start..i];
                if member.is_empty() || member.contains([']', '*', '?']) {
                    return Err(err());
                }
                out.push(Path::Member(member.into()));
            }
            b'[' => {
                i += 1;
                if i >= bytes.len() {
                    return Err(err());
                }
                if bytes[i] == b'\'' || bytes[i] == b'"' {
                    let quote = bytes[i];
                    let start = i;
                    i += 1;
                    while i < bytes.len() && bytes[i] != quote {
                        if bytes[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    if i >= bytes.len() {
                        return Err(err());
                    }
                    let raw = &path[start..=i];
                    let raw = if quote == b'\'' {
                        format!(
                            "\"{}\"",
                            raw[1..raw.len() - 1]
                                .replace('"', "\\\"")
                                .replace("\\'", "'")
                        )
                    } else {
                        raw.into()
                    };
                    let member: String = serde_json::from_str(&raw).map_err(|_| err())?;
                    i += 1;
                    if bytes.get(i) != Some(&b']') {
                        return Err(err());
                    }
                    i += 1;
                    out.push(Path::Member(member));
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    if start == i || bytes.get(i) != Some(&b']') {
                        return Err(err());
                    }
                    let index: usize = path[start..i].parse().map_err(|_| err())?;
                    if index > 4095 {
                        return Err(Error::message(format!(
                            "array index {index} exceeds Gemini partial argument materialization limit 4095"
                        )));
                    }
                    out.push(Path::Index(index));
                    i += 1;
                }
            }
            _ => return Err(err()),
        }
    }
    if out.is_empty() {
        return Err(Error::message(format!(
            "Gemini partial argument path {input:?} targets the arguments root"
        )));
    }
    Ok(out)
}
fn set_path(current: &mut Value, path: &[Path], value: Value, append: bool) -> Result<(), Error> {
    let Some((head, tail)) = path.split_first() else {
        if append {
            if let (Some(a), Some(b)) = (current.as_str(), value.as_str()) {
                *current = json!(format!("{a}{b}"));
                return Ok(());
            }
        }
        *current = value;
        return Ok(());
    };
    match head {
        Path::Member(k) => {
            if current.is_null() {
                *current = json!({});
            }
            let obj = current
                .as_object_mut()
                .ok_or_else(|| Error::message(format!("member {k:?} traverses non-object")))?;
            set_path(obj.entry(k).or_insert(Value::Null), tail, value, append)
        }
        Path::Index(i) => {
            if current.is_null() {
                *current = json!([]);
            }
            let arr = current
                .as_array_mut()
                .ok_or_else(|| Error::message(format!("array index {i} traverses non-array")))?;
            if arr.len() <= *i {
                arr.resize(i + 1, Value::Null);
            }
            set_path(&mut arr[*i], tail, value, append)
        }
    }
}
