use super::event::{
    ChatChoice, ChatDelta, ChatEvent, ChatFunction, ChatTool, EventSink, StreamEvent, ValueSink,
};
use super::*;
#[derive(Debug, Default)]
struct Tool {
    key: String,
    id: String,
    item_id: String,
    name: String,
    args: String,
    index: usize,
    sent: bool,
    name_sent: bool,
    sent_at: usize,
}
#[derive(Debug)]
pub(super) struct ResponsesChat {
    id: String,
    model: String,
    created: i64,
    include_usage: bool,
    started: bool,
    finalized: bool,
    text_sent: bool,
    reasoning_sent: bool,
    reasoning_break: bool,
    annotation_count: usize,
    saw_tool: bool,
    tools: Vec<Tool>,
    keys: BTreeMap<String, usize>,
    output_keys: BTreeMap<i64, usize>,
    item_keys: BTreeMap<String, usize>,
    call_keys: BTreeMap<String, usize>,
    pending: BTreeMap<String, String>,
    signatures: BTreeMap<String, String>,
    reasoning_blocks: BTreeMap<String, Value>,
    preserve: bool,
    pub usage: Option<Value>,
    pub usage_text: String,
}
impl ResponsesChat {
    pub fn new(o: &StreamOptions) -> Self {
        Self {
            id: o.id.trim().into(),
            model: o.model.trim().into(),
            created: if o.created_at == 0 {
                now()
            } else {
                o.created_at
            },
            include_usage: o.include_usage,
            started: false,
            finalized: false,
            text_sent: false,
            reasoning_sent: false,
            reasoning_break: false,
            annotation_count: 0,
            saw_tool: false,
            tools: Vec::new(),
            keys: BTreeMap::new(),
            output_keys: BTreeMap::new(),
            item_keys: BTreeMap::new(),
            call_keys: BTreeMap::new(),
            pending: BTreeMap::new(),
            signatures: BTreeMap::new(),
            reasoning_blocks: BTreeMap::new(),
            preserve: o.preserve_message_structure,
            usage: Some(crate::response::normalize_usage(&json!({}))),
            usage_text: String::new(),
        }
    }
    fn metadata(&mut self, r: &Value) {
        if !st(&r["id"]).is_empty() && self.id.is_empty() {
            self.id = st(&r["id"]).into();
        }
        if !st(&r["model"]).is_empty() && self.model != st(&r["model"]) {
            self.model = st(&r["model"]).into();
        }
        if n(&r["created_at"]) != 0 {
            self.created = n(&r["created_at"]);
        }
        if let Some(u) = r.get("usage").filter(|u| !u.is_null()) {
            merge_nonzero(&mut self.usage, &usage_from_responses(u));
        }
    }
    fn chunk_into<S: EventSink>(
        &self,
        delta: ChatDelta<'_>,
        finish: Option<&str>,
        sink: &mut S,
    ) -> Result<(), Error> {
        let choices = [ChatChoice {
            delta,
            finish_reason: finish,
            ..Default::default()
        }];
        sink.emit(StreamEvent::Chat(ChatEvent::new(
            &self.id,
            &self.model,
            self.created,
            &choices,
            None,
        )))
    }
    fn start_into<S: EventSink>(&mut self, sink: &mut S) -> Result<(), Error> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        self.chunk_into(
            ChatDelta {
                role: "assistant",
                content: Some(Cow::Borrowed("")),
                ..Default::default()
            },
            None,
            sink,
        )
    }
    fn text_into<S: EventSink>(&mut self, delta: &str, sink: &mut S) -> Result<(), Error> {
        if delta.is_empty() {
            return Ok(());
        }
        self.text_sent = true;
        self.usage_text.push_str(delta);
        self.start_into(sink)?;
        self.chunk_into(
            ChatDelta {
                content: Some(Cow::Borrowed(delta)),
                ..Default::default()
            },
            None,
            sink,
        )
    }
    fn reasoning_into<S: EventSink>(&mut self, delta: &str, sink: &mut S) -> Result<(), Error> {
        if delta.is_empty() {
            return Ok(());
        }
        let delta = if self.reasoning_break {
            self.reasoning_break = false;
            Cow::Owned(separated(delta))
        } else {
            Cow::Borrowed(delta)
        };
        self.usage_text.push_str(&delta);
        self.reasoning_sent = true;
        self.start_into(sink)?;
        self.chunk_into(
            ChatDelta {
                reasoning_content: Some(delta),
                ..Default::default()
            },
            None,
            sink,
        )
    }
    fn annotation_into<S: EventSink>(
        &mut self,
        annotation: &Value,
        sink: &mut S,
    ) -> Result<(), Error> {
        let annotation = Value::Array(vec![to_chat_annotation(annotation)?]);
        self.annotation_count += 1;
        self.start_into(sink)?;
        self.chunk_into(
            ChatDelta {
                annotations: Some(&annotation),
                ..Default::default()
            },
            None,
            sink,
        )
    }
    pub fn convert(&mut self, e: &Value) -> Result<Vec<Value>, Error> {
        let mut sink = ValueSink::default();
        self.convert_into(e, &mut sink)?;
        Ok(sink.values)
    }
    pub fn convert_into<S: EventSink>(&mut self, e: &Value, sink: &mut S) -> Result<(), Error> {
        match st(&e["type"]) {
            "response.created" => {
                self.metadata(&e["response"]);
                self.start_into(sink)
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.reasoning_into(st(&e["delta"]), sink)
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                if self.reasoning_sent {
                    self.reasoning_break = true;
                }
                Ok(())
            }
            "response.output_text.delta" => self.text_into(st(&e["delta"]), sink),
            "response.output_text.annotation.added" => {
                if e["annotation"].is_null() {
                    Ok(())
                } else {
                    self.annotation_into(&e["annotation"], sink)
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                if matches!(st(&e["item"]["type"]), "function_call" | "custom_tool_call") {
                    self.tool_item_into(e, sink)
                } else if self.preserve && st(&e["item"]["type"]) == "reasoning" {
                    self.signature_into(e, &e["item"], false, sink)
                } else {
                    Ok(())
                }
            }
            "response.reasoning_signature.delta" if self.preserve => {
                self.signature_into(e, &Value::Null, true, sink)
            }
            "response.reasoning_signature.done" if self.preserve => {
                self.signature_into(e, &Value::Null, false, sink)
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                let delta = st(&e["delta"]);
                if delta.is_empty() {
                    return Ok(());
                }
                if let Some(index) = self.find(e) {
                    self.tools[index].args.push_str(delta);
                    self.tool_delta_into(index, delta, sink)
                } else {
                    let key = Self::pending_key(e);
                    if !key.is_empty() {
                        self.pending.entry(key).or_default().push_str(delta);
                    }
                    Ok(())
                }
            }
            "response.function_call_arguments.done" | "response.custom_tool_call_input.done" => {
                if let Some(index) = self.find(e).or_else(|| self.ensure_tool(e, true)) {
                    self.tool_delta_into(index, "", sink)
                } else {
                    Ok(())
                }
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                let mut response = Cow::Borrowed(&e["response"]);
                if st(&e["type"]) == "response.incomplete" {
                    if response.is_null() {
                        response = Cow::Owned(json!({}));
                    }
                    if response["status"].is_null() {
                        response.to_mut()["status"] = json!("incomplete");
                    }
                }
                self.metadata(&response);
                self.terminal_into(&response, sink)?;
                self.finalize_into(Some(&response), sink)
            }
            "response.failed" | "response.error" => Err(Error::message(format!(
                "responses stream error: {}",
                st(&e["type"])
            ))),
            _ => Ok(()),
        }
    }
    fn terminal_into<S: EventSink>(&mut self, response: &Value, sink: &mut S) -> Result<(), Error> {
        let had_text = self.text_sent;
        let had_reasoning = self.reasoning_sent;
        let mut offset = 0;
        for item in arr(&response["output"]) {
            match st(&item["type"]) {
                "message" => {
                    if !had_text {
                        let text = arr(&item["content"])
                            .iter()
                            .filter(|p| st(&p["type"]) == "output_text")
                            .map(|p| st(&p["text"]))
                            .collect::<String>();
                        self.text_into(&text, sink)?;
                    }
                    let annotations = arr(&item["content"])
                        .iter()
                        .flat_map(|p| arr(&p["annotations"]))
                        .collect::<Vec<_>>();
                    let skip = self.annotation_count.saturating_sub(offset);
                    for annotation in annotations.iter().skip(skip) {
                        self.annotation_into(annotation, sink)?;
                    }
                    offset += annotations.len();
                }
                "reasoning" => {
                    if !had_reasoning {
                        self.reasoning_into(&reasoning_text(item), sink)?;
                    }
                    if self.preserve {
                        self.signature_into(&Value::Null, item, false, sink)?;
                    }
                }
                "function_call" | "custom_tool_call" => {
                    self.tool_item_into(&json!({"item":item}), sink)?
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn signature_into<S: EventSink>(
        &mut self,
        event: &Value,
        item: &Value,
        incremental: bool,
        sink: &mut S,
    ) -> Result<(), Error> {
        let signature = if incremental {
            st(&event["delta"])
        } else {
            item.get("encrypted_content").or_else(|| event.get("signature"))
                .or_else(|| event.get("delta")).map(st).unwrap_or("")
        };
        if signature.is_empty() {
            return Ok(());
        }
        let mut keys = Vec::new();
        if let Some(index) = event["output_index"].as_i64() {
            keys.push(format!("output:{index:020}"));
        }
        let id = item.get("id").or_else(|| event.get("item_id")).map(st).unwrap_or("");
        if !id.is_empty() {
            keys.push(format!("item:{id}"));
        }
        if keys.is_empty() {
            keys.push("reasoning".to_string());
        }
        let previous = keys.iter().find_map(|key| self.signatures.get(key)).cloned().unwrap_or_default();
        let (full, delta) = if incremental {
            (format!("{previous}{signature}"), signature.to_string())
        } else if let Some(suffix) = signature.strip_prefix(&previous) {
            (signature.to_string(), suffix.to_string())
        } else {
            return Err(Error::message("reasoning signature snapshot changed after streaming"));
        };
        for key in &keys {
            self.signatures.insert(key.clone(), full.clone());
        }
        let redacted = st(&item["claude_block_type"]) == "redacted_thinking";
        let mut blocks = Value::Null;
        if !incremental && item.is_object() {
            let block = if redacted {
                json!({"type":"redacted_thinking","data":full})
            } else {
                json!({"type":"thinking","thinking":reasoning_text(item),"signature":full})
            };
            let key = if id.is_empty() { &keys[0] } else { keys.last().unwrap() };
            self.reasoning_blocks.insert(key.clone(), block);
            blocks = Value::Array(self.reasoning_blocks.values().cloned().collect());
        }
        if delta.is_empty() && blocks.is_null() {
            return Ok(());
        }
        self.start_into(sink)?;
        self.chunk_into(
            ChatDelta {
                reasoning_signature: (!delta.is_empty()).then(|| Cow::Owned(delta)),
                reasoning_block_type: redacted.then_some("redacted_thinking"),
                claude_reasoning_blocks: (!blocks.is_null()).then_some(&blocks),
                ..Default::default()
            },
            None,
            sink,
        )
    }
    fn pending_key(e: &Value) -> String {
        if let Some(i) = e["output_index"].as_i64() {
            format!("output:{i}")
        } else {
            let id = st(&e["item_id"]).trim();
            if id.is_empty() {
                String::new()
            } else {
                format!("item:{id}")
            }
        }
    }
    fn key(e: &Value) -> String {
        if let Some(i) = e["output_index"].as_i64() {
            return format!("output:{i}");
        }
        let id = st(&e["item"]["id"]).trim();
        if !id.is_empty() {
            return format!("item:{id}");
        }
        let call = st(&e["item"]["call_id"]).trim();
        if !call.is_empty() {
            return format!("call:{call}");
        }
        Self::pending_key(e)
    }
    fn find(&self, e: &Value) -> Option<usize> {
        e["output_index"]
            .as_i64()
            .and_then(|i| self.output_keys.get(&i).copied())
            .or_else(|| self.item_keys.get(st(&e["item_id"]).trim()).copied())
            .or_else(|| self.keys.get(&Self::key(e)).copied())
    }
    fn ensure_tool(&mut self, e: &Value, fallback: bool) -> Option<usize> {
        let key = if fallback {
            Self::pending_key(e)
        } else {
            Self::key(e)
        };
        if key.is_empty() {
            return None;
        }
        let item_id = item_id(e);
        let call = st(&e["item"]["call_id"]).trim();
        let idx = self
            .keys
            .get(&key)
            .copied()
            .or_else(|| self.item_keys.get(item_id).copied())
            .or_else(|| self.call_keys.get(call).copied())
            .unwrap_or_else(|| {
                let idx = self.tools.len();
                self.tools.push(Tool {
                    key: key.clone(),
                    index: idx,
                    id: if fallback {
                        if let Some(i) = e["output_index"].as_i64() {
                            format!("call_output_{i}")
                        } else {
                            item_id.into()
                        }
                    } else {
                        String::new()
                    },
                    ..Default::default()
                });
                idx
            });
        self.keys.insert(key, idx);
        let tool = &mut self.tools[idx];
        if let Some(i) = e["output_index"].as_i64() {
            self.output_keys.insert(i, idx);
            if let Some(p) = self.pending.remove(&format!("output:{i}")) {
                tool.args.push_str(&p);
            }
        }
        if !item_id.is_empty() {
            tool.item_id = item_id.into();
            self.item_keys.insert(item_id.into(), idx);
            if let Some(p) = self.pending.remove(&format!("item:{item_id}")) {
                tool.args.push_str(&p);
            }
        }
        if !call.is_empty() {
            tool.id = call.into();
            self.call_keys.insert(call.into(), idx);
        } else if tool.id.is_empty() {
            tool.id = st(&e["item"]["id"]).trim().into();
        }
        let name = st(&e["item"]["name"]).trim();
        if !name.is_empty() {
            tool.name = name.into();
        }
        Some(idx)
    }
    fn tool_item_into<S: EventSink>(&mut self, event: &Value, sink: &mut S) -> Result<(), Error> {
        let Some(index) = self.ensure_tool(event, false) else {
            return Ok(());
        };
        let args = arguments(&event["item"]);
        if !args.is_empty() {
            self.tools[index].args = args;
        }
        self.tool_delta_into(index, "", sink)
    }
    fn tool_delta_into<S: EventSink>(
        &mut self,
        index: usize,
        explicit: &str,
        sink: &mut S,
    ) -> Result<(), Error> {
        let (send_name, start) = {
            let tool = &mut self.tools[index];
            let start = tool.sent_at;
            let args = if explicit.is_empty() {
                tool.args.get(start..).unwrap_or("")
            } else {
                explicit
            };
            if tool.sent && args.is_empty() && (tool.name.is_empty() || tool.name_sent) {
                return Ok(());
            }
            let send_name = !tool.name_sent && !tool.name.is_empty();
            if send_name {
                tool.name_sent = true;
                self.usage_text.push_str(&tool.name);
            }
            tool.sent = true;
            tool.sent_at += args.len();
            self.usage_text.push_str(args);
            (send_name, start)
        };
        self.start_into(sink)?;
        let tool = &self.tools[index];
        let args = if explicit.is_empty() {
            tool.args.get(start..).unwrap_or("")
        } else {
            explicit
        };
        let calls = [ChatTool {
            index: Some(tool.index as i64),
            id: if tool.id.is_empty() {
                &tool.key
            } else {
                &tool.id
            },
            kind: Some("function"),
            function: ChatFunction {
                name: if send_name { &tool.name } else { "" },
                arguments: Cow::Borrowed(args),
            },
        }];
        self.chunk_into(
            ChatDelta {
                tool_calls: &calls,
                ..Default::default()
            },
            None,
            sink,
        )?;
        self.saw_tool = true;
        Ok(())
    }
    pub fn finalize(&mut self, response: Option<&Value>) -> Result<Vec<Value>, Error> {
        let mut sink = ValueSink::default();
        self.finalize_into(response, &mut sink)?;
        Ok(sink.values)
    }
    pub fn finalize_into<S: EventSink>(
        &mut self,
        response: Option<&Value>,
        sink: &mut S,
    ) -> Result<(), Error> {
        if self.finalized {
            return Ok(());
        }
        self.finalized = true;
        let keys = self
            .keys
            .keys()
            .chain(self.pending.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for key in keys {
            let index = if let Some(index) = self.keys.get(&key) {
                *index
            } else {
                let index = self.tools.len();
                let id = key
                    .strip_prefix("output:")
                    .map(|value| format!("call_output_{value}"))
                    .unwrap_or_else(|| key.strip_prefix("item:").unwrap_or(&key).into());
                self.tools.push(Tool {
                    key: key.clone(),
                    index,
                    id,
                    ..Default::default()
                });
                self.keys.insert(key.clone(), index);
                index
            };
            if let Some(pending) = self.pending.remove(&key) {
                self.tools[index].args.push_str(&pending);
            }
            self.tool_delta_into(index, "", sink)?;
        }
        self.start_into(sink)?;
        let finish =
            responses_finish(response).unwrap_or(if self.saw_tool { "tool_calls" } else { "stop" });
        self.chunk_into(ChatDelta::default(), Some(finish), sink)?;
        if self.include_usage && self.usage.is_some() {
            sink.emit(StreamEvent::Chat(ChatEvent::new(
                &self.id,
                &self.model,
                self.created,
                &[],
                self.usage.as_ref(),
            )))?;
        }
        Ok(())
    }
}
