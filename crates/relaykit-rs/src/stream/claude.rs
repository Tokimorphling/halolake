use super::event::{EventSink, StreamEvent, ValueSink};
use super::*;
use serde::{Serialize, Serializer};

/// A Claude wire event consumed synchronously by an event sink.
/// Borrowed fields may reference either the prepared chunk or stream state;
/// callers retaining an event must materialize it before `emit` returns.
#[derive(Debug)]
pub enum ClaudeEvent<'a> {
    MessageStart {
        id: &'a str,
        model: &'a str,
        usage: &'a Value,
    },
    BlockStart {
        index: i64,
        block: ClaudeContentBlock<'a>,
    },
    BlockDelta {
        index: i64,
        delta: ClaudeDelta<'a>,
    },
    BlockStop {
        index: i64,
    },
    MessageDelta {
        stop_reason: &'a str,
        usage: Option<&'a Value>,
    },
    MessageStop,
}
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ClaudeContentBlock<'a> {
    #[serde(rename = "text")]
    Text { text: &'a str },
    #[serde(rename = "thinking")]
    Thinking { thinking: &'a str },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: &'a str },
    #[serde(rename = "tool_use")]
    ToolUse {
        #[serde(skip_serializing_if = "str::is_empty")]
        id: &'a str,
        #[serde(skip_serializing_if = "str::is_empty")]
        name: &'a str,
        input: EmptyObject,
    },
}
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ClaudeDelta<'a> {
    #[serde(rename = "text_delta")]
    Text { text: &'a str },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: &'a str },
    #[serde(rename = "signature_delta")]
    Signature { signature: &'a str },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: &'a str },
    #[serde(rename = "citations_delta")]
    Citation { citation: &'a Value },
}
#[derive(Debug, Clone, Copy)]
pub struct EmptyObject;
impl Serialize for EmptyObject {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        serializer.serialize_map(Some(0))?.end()
    }
}
struct ClaudeUsageView<'a>(&'a Value);
impl Serialize for ClaudeUsageView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        crate::wire::normalized_view("ClaudeUsage", self.0).serialize(serializer)
    }
}
impl Serialize for ClaudeEvent<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::MessageStart { id, model, usage } => {
                #[derive(Serialize)]
                struct Message<'a> {
                    #[serde(skip_serializing_if = "str::is_empty")]
                    id: &'a str,
                    #[serde(rename = "type")]
                    kind: &'static str,
                    role: &'static str,
                    #[serde(skip_serializing_if = "str::is_empty")]
                    model: &'a str,
                    usage: ClaudeUsageView<'a>,
                    content: [(); 0],
                }
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "message_start")?;
                map.serialize_entry(
                    "message",
                    &Message {
                        id,
                        model,
                        kind: "message",
                        role: "assistant",
                        usage: ClaudeUsageView(usage),
                        content: [],
                    },
                )?;
                map.end()
            }
            Self::BlockStart { index, block } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "content_block_start")?;
                map.serialize_entry("index", index)?;
                map.serialize_entry("content_block", block)?;
                map.end()
            }
            Self::BlockDelta { index, delta } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "content_block_delta")?;
                map.serialize_entry("index", index)?;
                map.serialize_entry("delta", delta)?;
                map.end()
            }
            Self::BlockStop { index } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "content_block_stop")?;
                map.serialize_entry("index", index)?;
                map.end()
            }
            Self::MessageDelta { stop_reason, usage } => {
                #[derive(Serialize)]
                struct Stop<'a> {
                    stop_reason: &'a str,
                }
                let mut map = serializer.serialize_map(Some(2 + usize::from(usage.is_some())))?;
                map.serialize_entry("type", "message_delta")?;
                map.serialize_entry("delta", &Stop { stop_reason })?;
                if let Some(usage) = usage {
                    map.serialize_entry("usage", &ClaudeUsageView(usage))?;
                }
                map.end()
            }
            Self::MessageStop => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("type", "message_stop")?;
                map.end()
            }
        }
    }
}
fn emit(sink: &mut dyn EventSink, event: ClaudeEvent<'_>) -> Result<(), Error> {
    sink.emit(StreamEvent::Claude(event))
}

#[derive(Debug, Default)]
pub(super) struct ClaudeChat {
    // Only hosted classification survives a chunk; block type strings are not
    // needed by the dense client-tool index mapping.
    blocks: BTreeMap<i64, bool>,
    tools: BTreeMap<i64, i64>,
    next: i64,
    preserve_message_structure: bool,
    id: String,
    model: String,
    created: i64,
    message_stopped: bool,
    reasoning_blocks: BTreeMap<i64, Value>,
    native_usage: Value,
    pub usage: Option<Value>,
}
impl ClaudeChat {
    pub fn new(options: &StreamOptions) -> Self {
        Self {
            preserve_message_structure: options.preserve_message_structure,
            id: options.id.clone(),
            model: options.model.clone(),
            created: if options.created_at == 0 {
                now()
            } else {
                options.created_at
            },
            ..Self::default()
        }
    }
    pub fn convert(&mut self, input: &Value) -> Result<Vec<Value>, Error> {
        if self.preserve_message_structure && self.message_stopped {
            return Ok(Vec::new());
        }
        let preserved = if self.preserve_message_structure {
            self.preserve_input(input)?
        } else {
            None
        };
        let mut sink = ValueSink::default();
        self.convert_into(input, &mut sink)?;
        if self.preserve_message_structure {
            if self.native_usage.is_object() {
                self.usage = Some(usage_from_claude(&self.native_usage));
            }
            if sink.values.is_empty() {
                if let Some(delta) = preserved.as_ref() {
                    sink.values.push(json!({
                        "id":self.id,"object":"chat.completion.chunk","created":self.created,
                        "model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":null}],
                        "usage":null,
                    }));
                }
            } else if let Some(preserved) = preserved.as_ref().and_then(Value::as_object) {
                for (key, value) in preserved {
                    sink.values[0]["choices"][0]["delta"][key] = value.clone();
                }
            }
            for value in &mut sink.values {
                value["id"] = json!(self.id);
                value["model"] = json!(self.model);
                value["created"] = json!(self.created);
                if !value["usage"].is_null() {
                    value["usage"] = self.usage.clone().unwrap_or(Value::Null);
                }
                if st(&input["type"]) == "content_block_delta"
                    && st(&input["delta"]["type"]) == "signature_delta"
                {
                    value["choices"][0]["delta"]
                        .as_object_mut()
                        .unwrap()
                        .remove("reasoning_content");
                }
                if st(&input["type"]) == "content_block_start"
                    && st(&input["content_block"]["type"]) == "tool_use"
                    && !input["content_block"]["input"].is_null()
                    && input["content_block"]["input"] != json!({})
                {
                    value["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"] =
                        json!(input["content_block"]["input"].to_string());
                }
            }
        }
        Ok(sink.values)
    }
    fn preserve_input(&mut self, input: &Value) -> Result<Option<Value>, Error> {
        let index = n(&input["index"]);
        let usage = if st(&input["type"]) == "message_start" {
            &input["message"]["usage"]
        } else {
            &input["usage"]
        };
        super::claude_responses::merge_cumulative(&mut self.native_usage, usage);
        match st(&input["type"]) {
            "message_start" => {
                if self.id.is_empty() {
                    self.id = st(&input["message"]["id"]).into();
                }
                if self.model.is_empty() {
                    self.model = st(&input["message"]["model"]).into();
                }
                Ok(None)
            }
            "content_block_start" => {
                if self.blocks.contains_key(&index) {
                    return Err(Error::message(
                        "duplicate Claude content block stream index",
                    ));
                }
                let block = &input["content_block"];
                match st(&block["type"]) {
                    "thinking" => {
                        self.reasoning_blocks.insert(index, block.clone());
                        let mut delta = json!({"reasoning_block_type":"thinking"});
                        if !st(&block["thinking"]).is_empty() {
                            delta["reasoning_content"] = block["thinking"].clone();
                        }
                        if !st(&block["signature"]).is_empty() {
                            delta["reasoning_signature"] = block["signature"].clone();
                        }
                        Ok(Some(delta))
                    }
                    "redacted_thinking" => {
                        self.reasoning_blocks.insert(index, block.clone());
                        Ok(Some(json!({
                            "reasoning_signature":block["data"],
                            "reasoning_block_type":"redacted_thinking",
                        })))
                    }
                    _ => Ok(None),
                }
            }
            "content_block_delta" => {
                if !self.blocks.contains_key(&index) {
                    return Err(Error::message(
                        "Claude stream delta has no open content block",
                    ));
                }
                let delta = &input["delta"];
                match st(&delta["type"]) {
                    "thinking_delta" | "signature_delta" => {
                        let Some(block) = self.reasoning_blocks.get_mut(&index) else {
                            return Err(Error::message(
                                "Claude reasoning delta has no matching reasoning block",
                            ));
                        };
                        if st(&block["type"]) != "thinking" {
                            return Err(Error::message(
                                "Claude redacted reasoning cannot contain plaintext or signature deltas",
                            ));
                        }
                        let key = if st(&delta["type"]) == "thinking_delta" {
                            "thinking"
                        } else {
                            "signature"
                        };
                        let text = format!("{}{}", st(&block[key]), st(&delta[key]));
                        block[key] = json!(text);
                        Ok(Some(if key == "signature" {
                            json!({"reasoning_signature":delta[key],"reasoning_block_type":"thinking"})
                        } else {
                            json!({"reasoning_block_type":"thinking"})
                        }))
                    }
                    _ => Ok(None),
                }
            }
            "content_block_stop" => Ok(self
                .reasoning_blocks
                .remove(&index)
                .map(|block| json!({"claude_reasoning_blocks":[block]}))),
            "message_stop" => {
                self.message_stopped = true;
                let blocks: Vec<_> = std::mem::take(&mut self.reasoning_blocks)
                    .into_values()
                    .collect();
                Ok((!blocks.is_empty()).then(|| json!({"claude_reasoning_blocks":blocks})))
            }
            _ => Ok(None),
        }
    }
    pub fn convert_into(&mut self, v: &Value, sink: &mut dyn EventSink) -> Result<(), Error> {
        use super::event::{ChatChoice, ChatDelta, ChatEvent, ChatFunction, ChatTool};
        self.usage = canonical(Format::Claude, v, 0);
        let mut index = n(&v["index"]);
        let mut delta = ChatDelta::default();
        let mut tool = None;
        let mut annotations = None;
        let mut finish = None;
        let mut id = "";
        let mut model = st(&v["model"]);
        match st(&v["type"]) {
            "message_start" => {
                id = st(&v["message"]["id"]);
                model = st(&v["message"]["model"]);
                delta.role = "assistant";
                delta.content = Some(Cow::Borrowed(""));
            }
            "content_block_start" => {
                let block = &v["content_block"];
                if block.is_null() {
                    return Ok(());
                }
                let kind = st(&block["type"]).trim();
                if !kind.is_empty() {
                    if v.get("index").is_none() {
                        return Err(Error::message(
                            "Claude content block stream start is missing index",
                        ));
                    }
                    let hosted = hosted(kind);
                    self.blocks.insert(index, hosted);
                    if hosted {
                        return Ok(());
                    }
                    if kind == "tool_use" {
                        index = *self.tools.entry(index).or_insert_with(|| {
                            let index = self.next;
                            self.next += 1;
                            index
                        });
                    }
                }
                if kind == "text" && !block["text"].is_null() {
                    delta.content = Some(Cow::Borrowed(st(&block["text"])));
                    let citations = crate::response::citations::claude_to_chat(
                        &block["citations"],
                        st(&block["text"]),
                        0,
                    );
                    if !citations.is_empty() {
                        annotations = Some(Value::Array(citations));
                    }
                }
                if kind == "tool_use" {
                    tool = Some(ChatTool {
                        index: Some(index),
                        id: st(&block["id"]),
                        kind: Some("function"),
                        function: ChatFunction {
                            name: st(&block["name"]),
                            arguments: Cow::Borrowed(""),
                        },
                    });
                }
            }
            "content_block_delta" => {
                let source = &v["delta"];
                if !source["text"].is_null() {
                    delta.content = Some(Cow::Borrowed(st(&source["text"])));
                }
                match st(&source["type"]) {
                    "input_json_delta" => {
                        if v.get("index").is_none() {
                            return Err(Error::message(
                                "Claude tool-use stream delta is missing content block index",
                            ));
                        }
                        if source["partial_json"].is_null() {
                            return Err(Error::message(
                                "Claude tool-use stream delta is missing partial JSON",
                            ));
                        }
                        let Some(index) = self.tools.get(&index) else {
                            if self.blocks.get(&index).copied().unwrap_or(false) {
                                return Ok(());
                            }
                            return Err(Error::message(format!(
                                "Claude tool-use stream delta references unknown content block index {index}"
                            )));
                        };
                        delta = ChatDelta::default();
                        tool = Some(ChatTool {
                            index: Some(*index),
                            kind: Some("function"),
                            function: ChatFunction {
                                arguments: Cow::Borrowed(st(&source["partial_json"])),
                                ..Default::default()
                            },
                            ..Default::default()
                        });
                    }
                    "signature_delta" => delta.reasoning_content = Some(Cow::Borrowed("\n")),
                    "thinking_delta" => {
                        if !source["thinking"].is_null() {
                            delta.reasoning_content = Some(Cow::Borrowed(st(&source["thinking"])));
                        }
                    }
                    "citations_delta" => {
                        let citation = Value::Array(vec![source["citation"].clone()]);
                        let citations =
                            crate::response::citations::claude_to_chat(&citation, "", 0);
                        if !citations.is_empty() {
                            annotations = Some(Value::Array(citations));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = v["delta"]["stop_reason"].as_str() {
                    finish = Some(chat_stop(reason));
                }
            }
            "content_block_stop" => {
                self.blocks.remove(&index);
                return Ok(());
            }
            _ => return Ok(()),
        }
        delta.tool_calls = tool.as_slice();
        delta.annotations = annotations.as_ref();
        let choices = [ChatChoice {
            delta,
            finish_reason: finish,
            ..Default::default()
        }];
        sink.emit(StreamEvent::Chat(ChatEvent::new(
            id,
            model,
            0,
            &choices,
            self.usage.as_ref(),
        )))
    }
}

fn hosted(s: &str) -> bool {
    matches!(
        s,
        "server_tool_use"
            | "mcp_tool_use"
            | "web_search_tool_result"
            | "mcp_tool_result"
            | "code_execution_tool_result"
            | "web_fetch_tool_result"
    )
}
#[derive(Debug, Default)]
struct ChatTool {
    index: i64,
    id: String,
    name: String,
    pending: String,
    started: bool,
}
#[derive(Debug, Default)]
pub(super) struct ChatClaude {
    started: bool,
    done: bool,
    preserve_message_structure: bool,
    model: String,
    finalizing: bool,
    reasoning_index: Option<i64>,
    open_reasoning: BTreeSet<i64>,
    reasoning_kind: String,
    pending_redacted: String,
    index: i64,
    kind: String,
    finish: String,
    base: i64,
    tools: Vec<ChatTool>,
    by_index: BTreeMap<i64, usize>,
    by_id: BTreeMap<String, usize>,
    pub usage: Option<Value>,
}

impl ChatClaude {
    pub fn new(options: &StreamOptions) -> Self {
        Self {
            preserve_message_structure: options.preserve_message_structure,
            model: options.model.clone(),
            ..Self::default()
        }
    }
    fn pending_into(&mut self, sink: &mut dyn EventSink) -> Result<(), Error> {
        for tool in &mut self.tools {
            if tool.started || tool.name.is_empty() {
                continue;
            }
            if tool.id.is_empty() {
                tool.id = uid("toolu_");
            }
            emit(
                sink,
                ClaudeEvent::BlockStart {
                    index: tool.index,
                    block: ClaudeContentBlock::ToolUse {
                        id: &tool.id,
                        name: &tool.name,
                        input: EmptyObject,
                    },
                },
            )?;
            tool.started = true;
            if !tool.pending.is_empty() {
                emit(
                    sink,
                    ClaudeEvent::BlockDelta {
                        index: tool.index,
                        delta: ClaudeDelta::InputJson {
                            partial_json: &tool.pending,
                        },
                    },
                )?;
                tool.pending.clear();
            }
        }
        Ok(())
    }
    fn stop_into(&mut self, sink: &mut dyn EventSink) -> Result<(), Error> {
        self.pending_into(sink)?;
        if self.kind == "tools" {
            for tool in self.tools.iter().filter(|tool| tool.started) {
                emit(sink, ClaudeEvent::BlockStop { index: tool.index })?;
            }
        } else if self.kind == "thinking" && self.preserve_message_structure && !self.finalizing {
            return Ok(());
        } else if !self.kind.is_empty() {
            emit(sink, ClaudeEvent::BlockStop { index: self.index })?;
            self.open_reasoning.remove(&self.index);
        }
        Ok(())
    }
    fn switch_into(&mut self, kind: &str, sink: &mut dyn EventSink) -> Result<(), Error> {
        if self.kind == kind {
            return Ok(());
        }
        self.stop_into(sink)?;
        if self.kind == "tools" {
            self.index = self.base + self.tools.len() as i64;
            self.tools.clear();
            self.by_id.clear();
            self.by_index.clear();
        } else if !self.kind.is_empty() {
            self.index += 1;
        }
        self.kind.clear();
        self.kind.push_str(kind);
        Ok(())
    }
    fn start_block_into(&mut self, kind: &str, sink: &mut dyn EventSink) -> Result<(), Error> {
        if self.kind == kind {
            return Ok(());
        }
        self.switch_into(kind, sink)?;
        if kind == "thinking" && self.preserve_message_structure {
            self.reasoning_index = Some(self.index);
            self.open_reasoning.insert(self.index);
        }
        emit(
            sink,
            ClaudeEvent::BlockStart {
                index: self.index,
                block: if kind == "text" {
                    ClaudeContentBlock::Text { text: "" }
                } else {
                    ClaudeContentBlock::Thinking { thinking: "" }
                },
            },
        )
    }
    fn flush_redacted_into(&mut self, sink: &mut dyn EventSink) -> Result<(), Error> {
        if self.pending_redacted.is_empty() {
            return Ok(());
        }
        if self.kind == "redacted_thinking" {
            self.switch_into("", sink)?;
        }
        self.switch_into("redacted_thinking", sink)?;
        let data = std::mem::take(&mut self.pending_redacted);
        emit(
            sink,
            ClaudeEvent::BlockStart {
                index: self.index,
                block: ClaudeContentBlock::RedactedThinking { data: &data },
            },
        )
    }
    fn preserved_reasoning_into(
        &mut self,
        delta: &Value,
        sink: &mut dyn EventSink,
    ) -> Result<(), Error> {
        let thinking = reasoning(delta);
        let signature = delta
            .get("reasoning_signature")
            .or_else(|| delta.get("reasoning_content_signature"))
            .or_else(|| delta.get("encrypted_content"))
            .map(st)
            .unwrap_or("");
        let kind = st(&delta["reasoning_block_type"]);
        if !kind.is_empty() {
            self.reasoning_kind = kind.into();
        } else if !thinking.is_empty() {
            self.reasoning_kind = "thinking".into();
        }
        if self.reasoning_kind == "redacted_thinking" {
            self.pending_redacted.push_str(signature);
            return Ok(());
        }
        self.flush_redacted_into(sink)?;
        if !thinking.is_empty() {
            self.start_block_into("thinking", sink)?;
            emit(
                sink,
                ClaudeEvent::BlockDelta {
                    index: self.index,
                    delta: ClaudeDelta::Thinking { thinking },
                },
            )?;
        }
        if !signature.is_empty() {
            if self.reasoning_index.is_none() {
                self.start_block_into("thinking", sink)?;
            }
            emit(
                sink,
                ClaudeEvent::BlockDelta {
                    index: self.reasoning_index.unwrap(),
                    delta: ClaudeDelta::Signature { signature },
                },
            )?;
        }
        Ok(())
    }
    fn visible_usage(&mut self, u: Option<&Value>) -> Option<Value> {
        let prior = self.usage.as_ref().map(claude_usage);
        if let Some(u) = u {
            let converted = claude_usage(u);
            merge_nonzero(&mut self.usage, u);
            let mut out = prior;
            let billing = converted
                .get("billing_usage")
                .filter(|v| !v.is_null())
                .cloned();
            let cache = converted
                .get("cache_creation")
                .filter(|v| !v.is_null())
                .cloned();
            merge_nonzero(&mut out, &converted);
            if let Some(v) = &mut out {
                if let Some(b) = billing {
                    v["billing_usage"] = b;
                }
                if let Some(c) = cache {
                    v["claude_cache_creation_5_m_tokens"] = c["ephemeral_5m_input_tokens"].clone();
                    v["claude_cache_creation_1_h_tokens"] = c["ephemeral_1h_input_tokens"].clone();
                }
            }
            out
        } else {
            prior
        }
    }
    pub fn convert(&mut self, ctx: &Context, input: &Value) -> Result<Vec<Value>, Error> {
        let mut sink = ValueSink::default();
        self.convert_into(ctx, input, &mut sink)?;
        Ok(sink.values)
    }
    pub fn convert_into(
        &mut self,
        ctx: &Context,
        v: &Value,
        sink: &mut dyn EventSink,
    ) -> Result<(), Error> {
        if self.done {
            return Ok(());
        }
        let incoming = v.get("usage").filter(|usage| !usage.is_null());
        if !self.started {
            self.started = true;
            let mut usage = json!({"input_tokens":ctx.estimate_prompt_tokens,"output_tokens":0});
            if let Some(incoming) = incoming {
                if has_usage_tokens(incoming) {
                    usage = claude_usage(incoming);
                    merge_nonzero(&mut self.usage, incoming);
                }
            }
            emit(
                sink,
                ClaudeEvent::MessageStart {
                    id: st(&v["id"]),
                    model: if self.preserve_message_structure && !self.model.is_empty() {
                        &self.model
                    } else {
                        st(&v["model"])
                    },
                    usage: &usage,
                },
            )?;
        }
        let choices = arr(&v["choices"]);
        if choices.is_empty() {
            if self.preserve_message_structure {
                self.visible_usage(incoming);
                return Ok(());
            }
            if let Some(usage) = self.visible_usage(incoming) {
                self.stop_into(sink)?;
                let reason = claude_stop(&self.finish);
                emit(
                    sink,
                    ClaudeEvent::MessageDelta {
                        stop_reason: if reason.is_empty() {
                            "end_turn"
                        } else {
                            reason
                        },
                        usage: Some(&usage),
                    },
                )?;
                emit(sink, ClaudeEvent::MessageStop)?;
                self.done = true;
            }
            return Ok(());
        }
        let choice = &choices[0];
        let delta = &choice["delta"];
        let finished = !st(&choice["finish_reason"]).is_empty();
        if finished {
            self.finish = st(&choice["finish_reason"]).into();
        }
        if self.preserve_message_structure {
            self.preserved_reasoning_into(delta, sink)?;
            let text = content(&delta["content"]);
            if !text.is_empty() || !arr(&delta["tool_calls"]).is_empty() {
                self.flush_redacted_into(sink)?;
            }
            if !text.is_empty() {
                self.start_block_into("text", sink)?;
                emit(
                    sink,
                    ClaudeEvent::BlockDelta {
                        index: self.index,
                        delta: ClaudeDelta::Text { text: &text },
                    },
                )?;
            }
            self.visible_usage(incoming);
        }
        if !arr(&delta["tool_calls"]).is_empty() {
            if self.kind != "tools" {
                self.switch_into("tools", sink)?;
                self.base = self.index;
            }
            for (position, call) in arr(&delta["tool_calls"]).iter().enumerate() {
                let index = call["index"].as_i64().unwrap_or(position as i64);
                let id = st(&call["id"]).trim();
                let mut selected = self
                    .by_id
                    .get(id)
                    .copied()
                    .or_else(|| self.by_index.get(&index).copied());
                if selected.is_some_and(|i| {
                    !id.is_empty() && !self.tools[i].id.is_empty() && self.tools[i].id != id
                }) {
                    selected = None;
                }
                let selected = selected.unwrap_or_else(|| {
                    let i = self.tools.len();
                    self.tools.push(ChatTool {
                        index: self.base + i as i64,
                        ..Default::default()
                    });
                    i
                });
                self.by_index.insert(index, selected);
                let tool = &mut self.tools[selected];
                if tool.id.is_empty() && !id.is_empty() {
                    tool.id = id.into();
                    self.by_id.insert(id.into(), selected);
                }
                if tool.name.is_empty() {
                    tool.name = st(&call["function"]["name"]).trim().into();
                }
                let args = st(&call["function"]["arguments"]);
                if !tool.started && !tool.id.is_empty() && !tool.name.is_empty() {
                    emit(
                        sink,
                        ClaudeEvent::BlockStart {
                            index: tool.index,
                            block: ClaudeContentBlock::ToolUse {
                                id: &tool.id,
                                name: &tool.name,
                                input: EmptyObject,
                            },
                        },
                    )?;
                    tool.started = true;
                    if tool.pending.is_empty() {
                        if !args.is_empty() {
                            emit(
                                sink,
                                ClaudeEvent::BlockDelta {
                                    index: tool.index,
                                    delta: ClaudeDelta::InputJson { partial_json: args },
                                },
                            )?;
                        }
                    } else {
                        tool.pending.push_str(args);
                        emit(
                            sink,
                            ClaudeEvent::BlockDelta {
                                index: tool.index,
                                delta: ClaudeDelta::InputJson {
                                    partial_json: &tool.pending,
                                },
                            },
                        )?;
                        tool.pending.clear();
                    }
                } else if tool.started {
                    if !args.is_empty() {
                        emit(
                            sink,
                            ClaudeEvent::BlockDelta {
                                index: tool.index,
                                delta: ClaudeDelta::InputJson { partial_json: args },
                            },
                        )?;
                    }
                } else {
                    tool.pending.push_str(args);
                }
            }
            if !self.tools.is_empty() {
                self.index = self.base + self.tools.len() as i64 - 1;
            }
        } else if !self.preserve_message_structure {
            let text = content(&delta["content"]);
            for (kind, value) in [("thinking", reasoning(delta)), ("text", text.as_ref())] {
                if !value.is_empty() {
                    self.start_block_into(kind, sink)?;
                    emit(
                        sink,
                        ClaudeEvent::BlockDelta {
                            index: self.index,
                            delta: if kind == "text" {
                                ClaudeDelta::Text { text: value }
                            } else {
                                ClaudeDelta::Thinking { thinking: value }
                            },
                        },
                    )?;
                }
            }
        }
        let citations = crate::response::citations::to_claude(&delta["annotations"], "");
        if !citations.is_empty() {
            self.start_block_into("text", sink)?;
            for citation in citations {
                emit(
                    sink,
                    ClaudeEvent::BlockDelta {
                        index: self.index,
                        delta: ClaudeDelta::Citation {
                            citation: &citation,
                        },
                    },
                )?;
            }
        }
        if finished && !self.preserve_message_structure {
            if let Some(usage) = self.visible_usage(incoming) {
                self.stop_into(sink)?;
                emit(
                    sink,
                    ClaudeEvent::MessageDelta {
                        stop_reason: claude_stop(&self.finish),
                        usage: Some(&usage),
                    },
                )?;
                emit(sink, ClaudeEvent::MessageStop)?;
                self.done = true;
            }
        }
        Ok(())
    }
    pub fn finalize(&mut self) -> Result<Vec<Value>, Error> {
        let mut sink = ValueSink::default();
        self.finalize_into(&mut sink)?;
        Ok(sink.values)
    }
    pub fn finalize_into(&mut self, sink: &mut dyn EventSink) -> Result<(), Error> {
        if self.done {
            return Ok(());
        }
        if self.preserve_message_structure {
            self.flush_redacted_into(sink)?;
            self.finalizing = true;
            let deferred: Vec<_> = self
                .open_reasoning
                .iter()
                .copied()
                .filter(|index| self.kind != "thinking" || *index != self.index)
                .collect();
            for index in deferred {
                emit(sink, ClaudeEvent::BlockStop { index })?;
                self.open_reasoning.remove(&index);
            }
        }
        self.stop_into(sink)?;
        let usage = self.visible_usage(None);
        let reason = claude_stop(&self.finish);
        emit(
            sink,
            ClaudeEvent::MessageDelta {
                stop_reason: if reason.is_empty() {
                    "end_turn"
                } else {
                    reason
                },
                usage: usage.as_ref(),
            },
        )?;
        emit(sink, ClaudeEvent::MessageStop)?;
        self.done = true;
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Block {
    kind: String,
    item_id: String,
    call_id: String,
    name: String,
    started: bool,
    stopped: bool,
    value: String,
    sent: usize,
    annotations: usize,
    reasoning_break: bool,
}
#[derive(Debug)]
pub(super) struct ResponsesClaude {
    id: String,
    model: String,
    started: bool,
    done: bool,
    saw_tool: bool,
    blocks: Vec<Block>,
    by_output: BTreeMap<i64, usize>,
    by_item: BTreeMap<String, usize>,
    last: BTreeMap<String, usize>,
    pub usage: Option<Value>,
    pub usage_text: String,
}

impl ResponsesClaude {
    pub fn new(o: &StreamOptions) -> Self {
        Self {
            id: o.id.trim().into(),
            model: o.model.trim().into(),
            started: false,
            done: false,
            saw_tool: false,
            blocks: Vec::new(),
            by_output: BTreeMap::new(),
            by_item: BTreeMap::new(),
            last: BTreeMap::new(),
            usage: None,
            usage_text: String::new(),
        }
    }
    fn metadata(&mut self, r: &Value) {
        if !st(&r["id"]).is_empty() {
            self.id = st(&r["id"]).into();
        }
        if !st(&r["model"]).is_empty() {
            self.model = st(&r["model"]).into();
        }
        if let Some(u) = r.get("usage").filter(|u| !u.is_null()) {
            merge_nonzero(&mut self.usage, &usage_from_responses(u));
        }
    }
    fn start_into(&mut self, ctx: &Context, sink: &mut dyn EventSink) -> Result<(), Error> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        let input = self
            .usage
            .as_ref()
            .map(|usage| n(&claude_usage(usage)["input_tokens"]))
            .unwrap_or(ctx.estimate_prompt_tokens);
        let usage = json!({"input_tokens":input,"output_tokens":0});
        emit(
            sink,
            ClaudeEvent::MessageStart {
                id: &self.id,
                model: &self.model,
                usage: &usage,
            },
        )
    }
    fn ensure(&mut self, e: &Value, kind: &str) -> Result<usize, Error> {
        self.ensure_parts(
            e["output_index"].as_i64(),
            e.get("output_index").is_some(),
            item_id(e),
            &e["item"],
            kind,
        )
    }
    fn ensure_parts(
        &mut self,
        output_index: Option<i64>,
        has_output_index: bool,
        id: &str,
        item: &Value,
        kind: &str,
    ) -> Result<usize, Error> {
        let index = output_index
            .and_then(|i| self.by_output.get(&i).copied())
            .or_else(|| self.by_item.get(id).copied())
            .or_else(|| {
                if !has_output_index && id.is_empty() {
                    self.last
                        .get(kind)
                        .copied()
                        .filter(|i| !self.blocks[*i].stopped)
                } else {
                    None
                }
            });
        let index = index.unwrap_or_else(|| {
            let i = self.blocks.len();
            self.blocks.push(Block {
                kind: kind.into(),
                ..Default::default()
            });
            i
        });
        let b = &mut self.blocks[index];
        if b.kind != kind {
            return Err(Error::message(format!(
                "Responses output item changed from {} to {kind}",
                b.kind
            )));
        }
        if let Some(i) = output_index {
            self.by_output.insert(i, index);
        }
        if !id.is_empty() {
            b.item_id = id.into();
            self.by_item.insert(id.into(), index);
        }
        let call = st(&item["call_id"]).trim();
        if !call.is_empty() {
            b.call_id = call.into();
        } else if b.call_id.is_empty() {
            b.call_id = st(&item["id"]).trim().into();
        }
        let name = st(&item["name"]).trim();
        if !name.is_empty() {
            b.name = name.into();
        }
        self.last.insert(kind.into(), index);
        Ok(index)
    }
    fn start_block_into(
        &mut self,
        ctx: &Context,
        i: usize,
        sink: &mut dyn EventSink,
    ) -> Result<(), Error> {
        let block = &mut self.blocks[i];
        if block.started || block.stopped {
            return Ok(());
        }
        match block.kind.as_str() {
            "text" | "thinking" => {}
            "tool_use" => {
                if block.name.is_empty() {
                    return Ok(());
                }
                self.saw_tool = true;
            }
            _ => return Ok(()),
        }
        block.started = true;
        self.start_into(ctx, sink)?;
        let block = &self.blocks[i];
        let content = match block.kind.as_str() {
            "text" => ClaudeContentBlock::Text { text: "" },
            "thinking" => ClaudeContentBlock::Thinking { thinking: "" },
            _ => ClaudeContentBlock::ToolUse {
                id: if block.call_id.is_empty() {
                    &block.item_id
                } else {
                    &block.call_id
                },
                name: &block.name,
                input: EmptyObject,
            },
        };
        emit(
            sink,
            ClaudeEvent::BlockStart {
                index: i as i64,
                block: content,
            },
        )
    }
    fn flush_into(
        &mut self,
        ctx: &Context,
        i: usize,
        sink: &mut dyn EventSink,
    ) -> Result<(), Error> {
        if self.blocks[i].stopped {
            return Ok(());
        }
        self.start_block_into(ctx, i, sink)?;
        let block = &mut self.blocks[i];
        if !block.started || block.sent >= block.value.len() {
            return Ok(());
        }
        let text = &block.value[block.sent..];
        self.usage_text.push_str(text);
        block.sent = block.value.len();
        let delta = match block.kind.as_str() {
            "text" => ClaudeDelta::Text { text },
            "thinking" => ClaudeDelta::Thinking { thinking: text },
            _ => ClaudeDelta::InputJson { partial_json: text },
        };
        emit(
            sink,
            ClaudeEvent::BlockDelta {
                index: i as i64,
                delta,
            },
        )
    }
    fn append_into(
        &mut self,
        ctx: &Context,
        i: usize,
        text: &str,
        sink: &mut dyn EventSink,
    ) -> Result<(), Error> {
        if self.blocks[i].stopped || text.is_empty() {
            return Ok(());
        }
        self.blocks[i].value.push_str(text);
        self.flush_into(ctx, i, sink)
    }
    fn merge_into(
        &mut self,
        ctx: &Context,
        i: usize,
        text: &str,
        sink: &mut dyn EventSink,
    ) -> Result<(), Error> {
        let block = &mut self.blocks[i];
        if block.stopped {
            return Ok(());
        }
        if block.value.is_empty() || text.starts_with(&block.value) {
            block.value.clear();
            block.value.push_str(text);
        }
        self.flush_into(ctx, i, sink)
    }
    fn stop_into(
        &mut self,
        ctx: &Context,
        i: usize,
        sink: &mut dyn EventSink,
    ) -> Result<(), Error> {
        if self.blocks[i].stopped {
            return Ok(());
        }
        self.flush_into(ctx, i, sink)?;
        self.start_block_into(ctx, i, sink)?;
        if self.blocks[i].started {
            self.blocks[i].stopped = true;
            emit(sink, ClaudeEvent::BlockStop { index: i as i64 })?;
        }
        Ok(())
    }
    fn annotations_into(
        &mut self,
        ctx: &Context,
        i: usize,
        annotations: &[Value],
        snapshot: bool,
        sink: &mut dyn EventSink,
    ) -> Result<(), Error> {
        let block = &mut self.blocks[i];
        if block.kind != "text" || block.stopped || annotations.is_empty() {
            return Ok(());
        }
        let skip = if snapshot { block.annotations } else { 0 };
        if snapshot {
            block.annotations = annotations.len().max(block.annotations);
        } else {
            block.annotations += annotations.len();
        }
        let source = Value::Array(annotations.iter().skip(skip).cloned().collect());
        let citations = crate::response::citations::to_claude(&source, &block.value);
        if citations.is_empty() {
            return Ok(());
        }
        self.start_block_into(ctx, i, sink)?;
        for citation in citations {
            emit(
                sink,
                ClaudeEvent::BlockDelta {
                    index: i as i64,
                    delta: ClaudeDelta::Citation {
                        citation: &citation,
                    },
                },
            )?;
        }
        Ok(())
    }
    fn item_into(
        &mut self,
        ctx: &Context,
        event: &Value,
        stop: bool,
        sink: &mut dyn EventSink,
    ) -> Result<(), Error> {
        self.item_parts_into(
            ctx,
            event["output_index"].as_i64(),
            event.get("output_index").is_some(),
            item_id(event),
            &event["item"],
            stop,
            sink,
        )
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "keep terminal snapshot items borrowed instead of rebuilding an event tree"
    )]
    fn item_parts_into(
        &mut self,
        ctx: &Context,
        output_index: Option<i64>,
        has_output_index: bool,
        id: &str,
        item: &Value,
        stop: bool,
        sink: &mut dyn EventSink,
    ) -> Result<(), Error> {
        let kind = match st(&item["type"]) {
            "reasoning" => "thinking",
            "message" => {
                if !st(&item["role"]).is_empty() && st(&item["role"]) != "assistant" {
                    return Ok(());
                }
                "text"
            }
            "function_call" | "custom_tool_call" => "tool_use",
            _ => return Ok(()),
        };
        let i = self.ensure_parts(output_index, has_output_index, id, item, kind)?;
        match kind {
            "thinking" => self.merge_into(ctx, i, &reasoning_text(item), sink)?,
            "tool_use" => {
                let text = if let Some(text) = item["arguments"].as_str() {
                    Cow::Borrowed(text)
                } else {
                    Cow::Owned(arguments(item))
                };
                self.merge_into(ctx, i, &text, sink)?;
            }
            _ => {
                let mut texts = arr(&item["content"])
                    .iter()
                    .filter(|part| st(&part["type"]) == "output_text")
                    .map(|part| st(&part["text"]))
                    .filter(|text| !text.is_empty());
                let first = texts.next().unwrap_or("");
                let text = if let Some(next) = texts.next() {
                    let mut text = String::with_capacity(first.len() + next.len());
                    text.push_str(first);
                    text.push_str(next);
                    for part in texts {
                        text.push_str(part);
                    }
                    Cow::Owned(text)
                } else {
                    Cow::Borrowed(first)
                };
                let annotations = arr(&item["content"])
                    .iter()
                    .filter(|part| st(&part["type"]) == "output_text")
                    .flat_map(|part| arr(&part["annotations"]).iter().cloned())
                    .collect::<Vec<_>>();
                self.merge_into(ctx, i, &text, sink)?;
                self.annotations_into(ctx, i, &annotations, true, sink)?;
            }
        }
        if stop {
            self.stop_into(ctx, i, sink)?;
        }
        Ok(())
    }
    pub fn convert(&mut self, ctx: &Context, event: &Value) -> Result<Vec<Value>, Error> {
        let mut sink = ValueSink::default();
        self.convert_into(ctx, event, &mut sink)?;
        Ok(sink.values)
    }
    pub fn convert_into(
        &mut self,
        ctx: &Context,
        event: &Value,
        sink: &mut dyn EventSink,
    ) -> Result<(), Error> {
        if self.done {
            return Ok(());
        }
        self.metadata(&event["response"]);
        let kind = st(&event["type"]);
        match kind {
            "response.created" => self.start_into(ctx, sink),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let i = self.ensure(event, "thinking")?;
                let text = st(&event["delta"]);
                if self.blocks[i].reasoning_break && !text.is_empty() {
                    self.blocks[i].reasoning_break = false;
                    if text.starts_with("\n\n") {
                        self.append_into(ctx, i, text, sink)
                    } else {
                        self.append_into(ctx, i, &separated(text), sink)
                    }
                } else {
                    self.append_into(ctx, i, text, sink)
                }
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                let i = self.ensure(event, "thinking")?;
                if let Some(text) = event["text"].as_str() {
                    self.merge_into(ctx, i, text, sink)?;
                }
                if !self.blocks[i].value.is_empty() {
                    self.blocks[i].reasoning_break = true;
                }
                Ok(())
            }
            "response.output_text.delta" => {
                let i = self.ensure(event, "text")?;
                self.append_into(ctx, i, st(&event["delta"]), sink)
            }
            "response.output_text.done" => {
                let i = self.ensure(event, "text")?;
                if let Some(text) = event["text"].as_str() {
                    self.merge_into(ctx, i, text, sink)?;
                }
                Ok(())
            }
            "response.output_text.annotation.added" => {
                let i = self.ensure(event, "text")?;
                self.annotations_into(
                    ctx,
                    i,
                    std::slice::from_ref(&event["annotation"]),
                    false,
                    sink,
                )
            }
            "response.output_item.added" | "response.output_item.done" => {
                self.item_into(ctx, event, kind == "response.output_item.done", sink)
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                let i = self.ensure(event, "tool_use")?;
                self.append_into(ctx, i, st(&event["delta"]), sink)
            }
            "response.function_call_arguments.done" | "response.custom_tool_call_input.done" => {
                let i = self.ensure(event, "tool_use")?;
                if let Some(text) = event["arguments"].as_str() {
                    self.merge_into(ctx, i, text, sink)?;
                }
                Ok(())
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                self.finish_into(ctx, Some(&event["response"]), sink)
            }
            "response.failed" | "response.error" => Err(Error::message(format!(
                "responses stream error: {}",
                if st(&event["message"]).trim().is_empty() {
                    kind
                } else {
                    st(&event["message"]).trim()
                }
            ))),
            _ => Ok(()),
        }
    }
    pub fn finish(&mut self, ctx: &Context, response: Option<&Value>) -> Result<Vec<Value>, Error> {
        let mut sink = ValueSink::default();
        self.finish_into(ctx, response, &mut sink)?;
        Ok(sink.values)
    }
    pub fn finish_into(
        &mut self,
        ctx: &Context,
        response: Option<&Value>,
        sink: &mut dyn EventSink,
    ) -> Result<(), Error> {
        if self.done {
            return Ok(());
        }
        if let Some(response) = response {
            self.metadata(response);
            for (i, item) in arr(&response["output"]).iter().enumerate() {
                self.item_parts_into(
                    ctx,
                    Some(i as i64),
                    true,
                    st(&item["id"]).trim(),
                    item,
                    true,
                    sink,
                )?;
            }
        }
        for i in 0..self.blocks.len() {
            self.stop_into(ctx, i, sink)?;
        }
        self.start_into(ctx, sink)?;
        let reason = responses_finish(response)
            .map(claude_stop)
            .unwrap_or(if self.saw_tool {
                "tool_use"
            } else {
                "end_turn"
            });
        let usage = self.usage.as_ref().map(claude_usage);
        emit(
            sink,
            ClaudeEvent::MessageDelta {
                stop_reason: reason,
                usage: usage.as_ref(),
            },
        )?;
        emit(sink, ClaudeEvent::MessageStop)?;
        self.done = true;
        Ok(())
    }
}

fn has_usage_tokens(u: &Value) -> bool {
    fn details(v: &Value) -> bool {
        v.as_object().is_some_and(|o| {
            o.iter().any(|(k, v)| {
                k != "billing_usage"
                    && (v.as_i64().is_some_and(|v| v != 0) || v.is_object() && details(v))
            })
        })
    }
    [
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "input_tokens",
        "output_tokens",
        "prompt_cache_hit_tokens",
        "claude_cache_creation_5_m_tokens",
        "claude_cache_creation_1_h_tokens",
    ]
    .iter()
    .any(|k| n(&u[*k]) != 0)
        || [
            "prompt_tokens_details",
            "completion_tokens_details",
            "input_tokens_details",
        ]
        .iter()
        .any(|k| details(&u[*k]))
}
