use super::event::{EventSink, StreamEvent, ValueSink};
use super::*;
#[path = "responses_event.rs"]
mod protocol;
pub use protocol::ResponsesEvent;
use protocol::{OutputItem, ResponseSnapshot, SummaryPart};
#[derive(Debug, Default)]
struct Tool {
    id: String,
    call_id: String,
    name: String,
    args: String,
    index: usize,
    announced: bool,
    custom: bool,
    done: bool,
    namespace: String,
    restored_name: String,
    signature: String,
    search: bool,
    preserve: bool,
}
#[derive(Debug, Default)]
struct Segment {
    index: usize,
    id: String,
    text: String,
    annotations: Vec<Value>,
    done: bool,
    kind: &'static str,
    signature: String,
    redacted: bool,
    preserve: bool,
}
#[derive(Debug)]
pub(super) struct ChatResponses {
    id: String,
    model: String,
    created: i64,
    sequence: Option<u64>,
    started: bool,
    finalized: bool,
    status: String,
    incomplete: Option<Value>,
    segments: Vec<Segment>,
    text: Option<usize>,
    reasoning: Option<usize>,
    text_ordinal: usize,
    reasoning_ordinal: usize,
    tools: BTreeMap<i64, Tool>,
    output_order: Vec<(u8, usize)>,
    hosted: Vec<(Value, bool)>,
    pub usage: Option<Value>,
    pub usage_text: String,
    preserve: bool,
}
impl ChatResponses {
    pub fn new(o: &StreamOptions) -> Self {
        Self {
            id: if o.id.trim().is_empty() {
                uid("resp_")
            } else {
                o.id.trim().into()
            },
            model: o.model.trim().into(),
            created: if o.created_at == 0 {
                now()
            } else {
                o.created_at
            },
            sequence: o.emit_sequence_number.then_some(0),
            started: false,
            finalized: false,
            status: "completed".into(),
            incomplete: None,
            segments: Vec::new(),
            text: None,
            reasoning: None,
            text_ordinal: 0,
            reasoning_ordinal: 0,
            tools: BTreeMap::new(),
            output_order: Vec::new(),
            hosted: Vec::new(),
            usage: Some(crate::response::normalize_usage(&json!({}))),
            usage_text: String::new(),
            preserve: o.preserve_message_structure,
        }
    }
    fn event(&mut self, t: &str, mut p: Value) -> Value {
        p["type"] = json!(t);
        if let Some(n) = &mut self.sequence {
            p["sequence_number"] = json!(*n);
            *n += 1;
        }
        p
    }
    fn created(&mut self) -> Vec<Value> {
        if self.started {
            return vec![];
        }
        self.started = true;
        let response = json!({"id":self.id,"object":"response","created_at":self.created,"status":"in_progress","model":self.model,"output":[]});
        vec![self.event("response.created", object([("response", response)]))]
    }
    pub fn convert(&mut self, ctx: &Context, v: &Value) -> Result<Vec<Value>, Error> {
        let mut sink = ValueSink::default();
        self.convert_into(ctx, v, &mut sink)?;
        Ok(sink.values)
    }
    fn typed_event<'a>(&mut self, kind: &'static str) -> ResponsesEvent<'a> {
        let sequence = self.sequence.as_mut().map(|n| {
            let value = *n;
            *n += 1;
            value
        });
        ResponsesEvent::new(kind, sequence)
    }
    fn created_into<S: EventSink>(&mut self, sink: &mut S) -> Result<(), Error> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        let mut event = self.typed_event("response.created");
        event.response = Some(ResponseSnapshot {
            state: self,
            created: true,
        });
        sink.emit(StreamEvent::Responses(event))
    }
    pub fn convert_into<S: EventSink>(
        &mut self,
        ctx: &Context,
        v: &Value,
        sink: &mut S,
    ) -> Result<(), Error> {
        if self.id.is_empty() {
            self.id = st(&v["id"]).into();
        }
        if self.model.is_empty() {
            self.model = st(&v["model"]).into();
        }
        if self.created == 0 {
            self.created = n(&v["created"]);
        }
        if let Some(usage) = v.get("usage").filter(|u| !u.is_null()) {
            let mut usage = usage_from_chat(usage);
            if self.preserve {
                usage["output_tokens_details"] = usage["completion_tokens_details"].clone();
            }
            self.usage = Some(usage);
        }
        self.created_into(sink)?;
        for choice in arr(&v["choices"]) {
            let delta = &choice["delta"];
            let r = reasoning(delta);
            if !r.is_empty() {
                self.append_segment_into("reasoning", r, sink)?;
            }
            if self.preserve
                && let Some(signature) = delta.get("reasoning_signature")
                    .or_else(|| delta.get("reasoning_content_signature"))
                    .and_then(Value::as_str).filter(|signature| !signature.is_empty())
            {
                let index = self.start_segment_into("reasoning", sink)?;
                self.segments[index].signature.push_str(signature);
                self.segments[index].redacted |= st(&delta["reasoning_block_type"]) == "redacted_thinking";
                let mut event = self.typed_event("response.reasoning_signature.delta");
                event.output_index = Some(self.segments[index].index);
                event.item_id = &self.segments[index].id;
                event.delta = signature;
                sink.emit(StreamEvent::Responses(event))?;
            }
            let text = content(&delta["content"]);
            if !text.is_empty() {
                self.append_segment_into("message", &text, sink)?;
            }
            if !arr(&delta["annotations"]).is_empty() {
                let index = self.start_segment_into("message", sink)?;
                for annotation in arr(&delta["annotations"]) {
                    let annotation = to_response_annotation(annotation)?;
                    let mut event = self.typed_event("response.output_text.annotation.added");
                    let segment = &mut self.segments[index];
                    event.output_index = Some(segment.index);
                    event.content_index = Some(0);
                    event.annotation_index = Some(segment.annotations.len());
                    event.item_id = &segment.id;
                    segment.annotations.push(annotation);
                    event.annotation = segment.annotations.last();
                    sink.emit(StreamEvent::Responses(event))?;
                }
            }
            for tool in arr(&delta["tool_calls"]) {
                self.append_tool_into(ctx, tool, sink)?;
            }
            let finish = st(&choice["finish_reason"]).trim();
            if !finish.is_empty() {
                match finish {
                    "length" => {
                        self.status = "incomplete".into();
                        self.incomplete = Some(json!({"reason":"max_output_tokens"}));
                    }
                    "content_filter" => {
                        self.status = "incomplete".into();
                        self.incomplete = Some(json!({"reason":"content_filter"}));
                    }
                    _ => {
                        self.status = "completed".into();
                        self.incomplete = None;
                    }
                }
                self.close_items_into(sink)?;
            }
        }
        Ok(())
    }
    fn start_segment_into<S: EventSink>(
        &mut self,
        kind: &'static str,
        sink: &mut S,
    ) -> Result<usize, Error> {
        let current = if kind == "message" {
            self.text
        } else {
            self.reasoning
        };
        if let Some(index) = current {
            if !self.segments[index].done {
                return Ok(index);
            }
            if kind == "message" {
                self.text_ordinal += 1;
            } else {
                self.reasoning_ordinal += 1;
            }
        }
        let ordinal = if kind == "message" {
            self.text_ordinal
        } else {
            self.reasoning_ordinal
        };
        let id = format!(
            "{}_{}_{}",
            self.id,
            if kind == "message" {
                "msg"
            } else {
                "reasoning"
            },
            ordinal
        );
        let output_index = self.output_order.len();
        let index = self.segments.len();
        self.output_order.push((0, index));
        self.segments.push(Segment {
            index: output_index,
            id,
            kind,
            preserve: self.preserve,
            ..Default::default()
        });
        if kind == "message" {
            self.text = Some(index);
        } else {
            self.reasoning = Some(index);
        }
        let mut event = self.typed_event("response.output_item.added");
        event.output_index = Some(output_index);
        event.item = Some(OutputItem::Segment {
            segment: &self.segments[index],
            status: "in_progress",
            added: true,
        });
        sink.emit(StreamEvent::Responses(event))?;
        if kind == "reasoning" {
            let mut event = self.typed_event("response.reasoning_summary_part.added");
            event.output_index = Some(output_index);
            event.summary_index = Some(0);
            event.item_id = &self.segments[index].id;
            event.part = Some(SummaryPart {
                kind: "summary_text",
                text: "",
            });
            sink.emit(StreamEvent::Responses(event))?;
        }
        Ok(index)
    }
    fn append_segment_into<S: EventSink>(
        &mut self,
        kind: &'static str,
        delta: &str,
        sink: &mut S,
    ) -> Result<(), Error> {
        let index = self.start_segment_into(kind, sink)?;
        self.segments[index].text.push_str(delta);
        self.usage_text.push_str(delta);
        let mut event = self.typed_event(if kind == "message" {
            "response.output_text.delta"
        } else {
            "response.reasoning_summary_text.delta"
        });
        let segment = &self.segments[index];
        event.output_index = Some(segment.index);
        event.item_id = &segment.id;
        event.delta = delta;
        if kind == "message" {
            event.content_index = Some(0);
        } else {
            event.summary_index = Some(0);
        }
        sink.emit(StreamEvent::Responses(event))
    }
    fn append_tool_into<S: EventSink>(
        &mut self,
        ctx: &Context,
        v: &Value,
        sink: &mut S,
    ) -> Result<(), Error> {
        let index = n(&v["index"]);
        let id = st(&v["id"]).trim();
        let name = st(&v["function"]["name"]).trim();
        let mut tool = self.tools.remove(&index).unwrap_or_else(|| Tool {
            id: if id.is_empty() {
                format!("{}_call_{index}", self.id)
            } else {
                id.into()
            },
            call_id: id.into(),
            name: name.into(),
            ..Default::default()
        });
        let result = (|| {
            if tool.done {
                return Err(Error::message(format!(
                    "tool-call stream index {index} received data after completion"
                )));
            }
            if !id.is_empty() {
                if !tool.call_id.is_empty() && tool.call_id != id {
                    return Err(Error::message(format!(
                        "tool-call stream index {index} changed id from {:?} to {id:?}",
                        tool.call_id
                    )));
                }
                tool.call_id = id.into();
                if !tool.announced {
                    tool.id = id.into();
                }
            }
            if !name.is_empty() {
                if !tool.name.is_empty() && tool.name != name {
                    return Err(Error::message(format!(
                        "tool-call stream index {index} changed name from {:?} to {name:?}",
                        tool.name
                    )));
                }
                tool.name = name.into();
            }
            let delta = st(&v["function"]["arguments"]);
            tool.args.push_str(delta);
            tool.preserve = self.preserve;
            if self.preserve
                && let Some(signature) = v.get("thought_signature")
                    .or_else(|| v.get("thoughtSignature")).and_then(Value::as_str)
            {
                tool.signature.push_str(signature);
            }
            let newly_announced = !tool.announced && !tool.name.is_empty();
            if newly_announced {
                let tools = ctx.responses_tool_state();
                tool.custom = tools.is_custom_tool(&tool.name);
                tool.search = self.preserve && tools.tool_search && tool.name == "tool_search";
                (tool.namespace, tool.restored_name) = tools.responses_tool_name(&tool.name);
                tool.announced = true;
                tool.index = self.output_order.len();
                self.output_order.push((1, index as usize));
                let mut event = self.typed_event("response.output_item.added");
                event.output_index = Some(tool.index);
                event.item_id = &tool.id;
                event.item = Some(OutputItem::Tool {
                    tool: &tool,
                    status: "in_progress",
                    empty: true,
                });
                sink.emit(StreamEvent::Responses(event))?;
            }
            let delta = if newly_announced {
                tool.args.as_str()
            } else {
                delta
            };
            if tool.announced {
                self.usage_text.push_str(delta);
            }
            if tool.announced && !tool.custom && !tool.search && !delta.is_empty() {
                let mut event = self.typed_event("response.function_call_arguments.delta");
                event.output_index = Some(tool.index);
                event.item_id = &tool.id;
                event.delta = delta;
                sink.emit(StreamEvent::Responses(event))?;
            }
            Ok(())
        })();
        self.tools.insert(index, tool);
        result
    }
    fn close_items_into<S: EventSink>(&mut self, sink: &mut S) -> Result<(), Error> {
        for index in [self.text, self.reasoning].into_iter().flatten() {
            if self.segments[index].done {
                continue;
            }
            self.segments[index].done = true;
            let message = self.segments[index].kind == "message";
            let with_text = self.sequence.is_some();
            let mut event = self.typed_event(if message {
                "response.output_text.done"
            } else {
                "response.reasoning_summary_text.done"
            });
            let segment = &self.segments[index];
            event.output_index = Some(segment.index);
            event.item_id = &segment.id;
            if message {
                event.content_index = Some(0);
            } else {
                event.summary_index = Some(0);
            }
            if with_text {
                event.text = Some(&segment.text);
            } else if !message {
                event.part = Some(SummaryPart {
                    kind: "summary_text",
                    text: &segment.text,
                });
            }
            sink.emit(StreamEvent::Responses(event))?;
            if !message {
                let mut event = self.typed_event("response.reasoning_summary_part.done");
                let segment = &self.segments[index];
                event.output_index = Some(segment.index);
                event.item_id = &segment.id;
                event.summary_index = Some(0);
                event.part = Some(SummaryPart {
                    kind: "summary_text",
                    text: &segment.text,
                });
                sink.emit(StreamEvent::Responses(event))?;
            }
            if self.preserve && !self.segments[index].signature.is_empty() {
                let mut event = self.typed_event("response.reasoning_signature.done");
                let segment = &self.segments[index];
                event.output_index = Some(segment.index);
                event.item_id = &segment.id;
                event.delta = &segment.signature;
                sink.emit(StreamEvent::Responses(event))?;
            }
            let mut event = self.typed_event("response.output_item.done");
            let segment = &self.segments[index];
            event.output_index = Some(segment.index);
            event.item = Some(OutputItem::Segment {
                segment,
                status: self.status(),
                added: false,
            });
            sink.emit(StreamEvent::Responses(event))?;
        }
        let indexes = self.tools.keys().copied().collect::<Vec<_>>();
        for index in indexes {
            let mut tool = self.tools.remove(&index).unwrap();
            let result = (|| {
                if tool.done {
                    return Ok(());
                }
                tool.done = true;
                if !tool.announced {
                    return Ok(());
                }
                let input = tool.custom.then(|| crate::response::custom_tool_input_from_arguments(&tool.args, tool.preserve));
                if let Some(input) = input.as_deref().filter(|v| !v.is_empty()) {
                    let mut event = self.typed_event("response.custom_tool_call_input.delta");
                    event.output_index = Some(tool.index);
                    event.item_id = &tool.id;
                    event.delta = input;
                    sink.emit(StreamEvent::Responses(event))?;
                }
                if !tool.search {
                    let with_arguments = self.sequence.is_some();
                    let mut event = self.typed_event(if tool.custom {
                        "response.custom_tool_call_input.done"
                    } else {
                        "response.function_call_arguments.done"
                    });
                    event.output_index = Some(tool.index);
                    event.item_id = &tool.id;
                    if let Some(input) = input.as_deref() {
                        event.input = Some(input);
                    } else if with_arguments {
                        event.arguments = Some(&tool.args);
                        event.name = &tool.restored_name;
                    }
                    if self.preserve {
                        event.name = if tool.restored_name.is_empty() { &tool.name } else { &tool.restored_name };
                    }
                    sink.emit(StreamEvent::Responses(event))?;
                }
                let mut event = self.typed_event("response.output_item.done");
                event.output_index = Some(tool.index);
                event.item = Some(OutputItem::Tool {
                    tool: &tool,
                    status: self.status(),
                    empty: false,
                });
                sink.emit(StreamEvent::Responses(event))
            })();
            self.tools.insert(index, tool);
            result?;
        }
        // Hosted items remain bounded compatibility islands in the first typed
        // implementation; the regular items above have already been closed.
        if self.hosted.iter().any(|(_, done)| !done) {
            for event in self.close_items_legacy() {
                let event = crate::wire::normalize_stream_owned(Format::OpenAiResponses, event)?;
                sink.emit(StreamEvent::Compatibility {
                    format: Format::OpenAiResponses,
                    value: Cow::Owned(event),
                })?;
            }
        }
        Ok(())
    }
    fn tool_output(&self, t: &Tool, status: &str) -> Value {
        let mut v = json!({"type":if t.custom{"custom_tool_call"}else{"function_call"},"id":t.id,"status":status,"call_id":if t.call_id.is_empty(){&t.id}else{&t.call_id},"name":if t.restored_name.is_empty(){&t.name}else{&t.restored_name}});
        if !t.namespace.is_empty() {
            v["namespace"] = json!(t.namespace);
        }
        v[if t.custom { "input" } else { "arguments" }] = Value::String(if t.custom {
            custom_input(&t.args)
        } else {
            t.args.clone()
        });
        v
    }
    fn segment_output(&self, s: &Segment, status: &str) -> Value {
        if s.kind == "message" {
            json!({"type":"message","id":s.id,"status":status,"role":"assistant","content":[{"type":"output_text","text":s.text,"annotations":s.annotations}]})
        } else {
            json!({"type":"reasoning","id":s.id,"status":status,"summary":[{"type":"summary_text","text":s.text}]})
        }
    }
    fn status(&self) -> &str {
        if self.status == "incomplete" || self.status == "failed" {
            "incomplete"
        } else {
            "completed"
        }
    }
    fn close_items_legacy(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        for idx in [self.text, self.reasoning].into_iter().flatten() {
            if self.segments[idx].done {
                continue;
            }
            self.segments[idx].done = true;
            let s = &self.segments[idx];
            let mut p = json!({"output_index":s.index,"item_id":s.id});
            let message = s.kind == "message";
            p[if message {
                "content_index"
            } else {
                "summary_index"
            }] = json!(0);
            if self.sequence.is_some() {
                p["text"] = json!(s.text);
            } else if !message {
                p["part"] = json!({"type":"summary_text","text":s.text});
            }
            let part=(!message).then(||json!({"output_index":s.index,"item_id":s.id,"summary_index":0,"part":{"type":"summary_text","text":s.text}}));
            let done = object([
                ("output_index", s.index.into()),
                ("item", self.segment_output(s, self.status())),
            ]);
            out.push(self.event(
                if message {
                    "response.output_text.done"
                } else {
                    "response.reasoning_summary_text.done"
                },
                p,
            ));
            if let Some(p) = part {
                out.push(self.event("response.reasoning_summary_part.done", p));
            }
            out.push(self.event("response.output_item.done", done));
        }
        let indexes = self.tools.keys().copied().collect::<Vec<_>>();
        for index in indexes {
            let mut tool = self.tools.remove(&index).unwrap();
            if !tool.done {
                tool.done = true;
                if tool.announced {
                    let mut p = json!({"output_index":tool.index,"item_id":tool.id});
                    if tool.custom {
                        let input = custom_input(&tool.args);
                        if !input.is_empty() {
                            out.push(self.event(
                                "response.custom_tool_call_input.delta",
                                json!({"output_index":tool.index,"item_id":tool.id,"delta":input}),
                            ));
                        }
                        p["input"] = json!(input);
                    } else if self.sequence.is_some() {
                        p["arguments"] = json!(tool.args);
                        p["name"] = json!(tool.restored_name);
                    }
                    out.push(self.event(
                        if tool.custom {
                            "response.custom_tool_call_input.done"
                        } else {
                            "response.function_call_arguments.done"
                        },
                        p,
                    ));
                    let item = self.tool_output(&tool, self.status());
                    out.push(self.event(
                        "response.output_item.done",
                        object([("output_index", tool.index.into()), ("item", item)]),
                    ));
                }
            }
            self.tools.insert(index, tool);
        }
        for i in 0..self.hosted.len() {
            if self.hosted[i].1 {
                continue;
            }
            if self.status != "failed" {
                self.status = "incomplete".into();
            }
            self.hosted[i].1 = true;
            let failed = self.status == "failed";
            self.hosted[i].0["status"] = json!(if failed { "failed" } else { "incomplete" });
            let index = self.output_order.iter().position(|r| *r == (2, i)).unwrap();
            let id = self.hosted[i].0["id"].clone();
            if failed && self.hosted[i].0["type"] == "mcp_call" {
                self.hosted[i].0["error"] =
                    json!("provider stream failed before hosted-tool result");
                self.hosted[i].0.as_object_mut().unwrap().remove("output");
                out.push(self.event(
                    "response.mcp_call.failed",
                    json!({"output_index":index,"item_id":id}),
                ));
            }
            let item = self.hosted[i].0.clone();
            out.push(self.event(
                "response.output_item.done",
                object([
                    ("output_index", index.into()),
                    ("item_id", id),
                    ("item", item),
                ]),
            ));
        }
        out
    }
    fn response(&self) -> Value {
        let output = self
            .output_order
            .iter()
            .filter_map(|(tool, i)| {
                if *tool == 2 {
                    Some(self.hosted[*i].0.clone())
                } else if *tool == 1 {
                    self.tools
                        .get(&(*i as i64))
                        .map(|t| self.tool_output(t, self.status()))
                } else {
                    Some(self.segment_output(&self.segments[*i], self.status()))
                }
            })
            .collect::<Vec<_>>();
        let mut r = json!({"id":self.id,"object":"response","created_at":self.created,"status":self.status,"model":self.model});
        r["output"] = Value::Array(output);
        r["usage"] = self.usage.clone().unwrap_or(Value::Null);
        if let Some(d) = &self.incomplete {
            r["incomplete_details"] = d.clone();
        }
        r
    }
    pub fn start_hosted(&mut self, start: &HostedToolStart) -> Result<Vec<Value>, Error> {
        let id = start.id.trim();
        if id.is_empty() {
            return Err(Error::message("hosted-tool stream call is missing an id"));
        }
        if self.hosted.iter().any(|(v, _)| st(&v["id"]) == id) {
            return Err(Error::message(format!(
                "duplicate hosted-tool stream call id {id:?}"
            )));
        }
        if !matches!(start.kind.as_str(), "web_search_call" | "mcp_call") {
            return Err(Error::message(format!(
                "unsupported Responses hosted-tool output type {:?}",
                start.kind
            )));
        }
        if !start.caller.is_null() {
            return Err(Error::message(format!(
                "Responses {} cannot preserve Claude hosted-tool caller provenance",
                start.kind
            )));
        }
        let mut item = json!({"type":start.kind,"id":id,"status":"in_progress"});
        if start.kind == "web_search_call" {
            item["action"] = super::hosted::web_action(&start.action)?;
        } else {
            if start.name.trim().is_empty() || start.server_label.trim().is_empty() {
                return Err(Error::message(
                    "Responses MCP call requires name and server_label",
                ));
            }
            item["name"] = json!(start.name);
            item["server_label"] = json!(start.server_label);
            item["arguments"] = json!(if start.action.is_null() {
                String::new()
            } else {
                crate::json::stringify(&start.action)
            });
        }
        let index = self.output_order.len();
        let h = self.hosted.len();
        self.hosted.push((item.clone(), false));
        self.output_order.push((2, h));
        if start.kind == "mcp_call" {
            item["arguments"] = json!("");
        }
        let mut out = self.created();
        out.push(self.event(
            "response.output_item.added",
            object([
                ("output_index", index.into()),
                ("item_id", id.into()),
                ("item", item),
            ]),
        ));
        out.push(self.event(
            &format!("response.{}.in_progress", start.kind),
            json!({"output_index":index,"item_id":id}),
        ));
        if start.kind == "web_search_call" {
            out.push(self.event(
                "response.web_search_call.searching",
                json!({"output_index":index,"item_id":id}),
            ));
        } else {
            let arguments = self.hosted[h].0["arguments"].clone();
            out.push(self.event(
                "response.mcp_call_arguments.delta",
                json!({"output_index":index,"item_id":id,"delta":arguments}),
            ));
            out.push(self.event(
                "response.mcp_call_arguments.done",
                json!({"output_index":index,"item_id":id,"arguments":arguments}),
            ));
        }
        Ok(out)
    }
    pub fn complete_hosted(&mut self, result: &HostedToolResult) -> Result<Vec<Value>, Error> {
        let id = result.id.trim();
        let Some(h) = self.hosted.iter().position(|(v, _)| st(&v["id"]) == id) else {
            return Err(Error::message(format!(
                "hosted-tool result references unknown call {id:?}"
            )));
        };
        let (item, done) = &mut self.hosted[h];
        if *done {
            return Err(Error::message(format!(
                "duplicate hosted-tool result for call {id:?}"
            )));
        }
        let kind = st(&item["type"]).to_owned();
        if !result.kind.is_empty() && result.kind != kind {
            return Err(Error::message(format!(
                "hosted-tool result type {:?} does not match call type {kind:?}",
                result.kind
            )));
        }
        let failed = result.is_error || !result.error_code.trim().is_empty();
        item["status"] = json!(if failed { "failed" } else { "completed" });
        if kind == "mcp_call" {
            if failed {
                item["error"] = json!(if result.error_code.is_empty() {
                    "hosted tool execution failed"
                } else {
                    &result.error_code
                });
                item.as_object_mut().unwrap().remove("output");
            } else {
                item["output"] = json!(if result.result.is_null() {
                    String::new()
                } else if let Some(s) = result.result.as_str() {
                    s.into()
                } else {
                    crate::json::stringify(&result.result)
                });
            }
        }
        *done = true;
        let item = item.clone();
        let index = self.output_order.iter().position(|r| *r == (2, h)).unwrap();
        let mut out = Vec::new();
        if !failed || kind == "mcp_call" {
            out.push(self.event(
                &format!(
                    "response.{kind}.{}",
                    if failed { "failed" } else { "completed" }
                ),
                json!({"output_index":index,"item_id":id}),
            ));
        }
        out.push(self.event(
            "response.output_item.done",
            object([
                ("output_index", index.into()),
                ("item_id", id.into()),
                ("item", item),
            ]),
        ));
        Ok(out)
    }
    pub fn finalize(&mut self) -> Result<Vec<Value>, Error> {
        let mut sink = ValueSink::default();
        self.finalize_into(&mut sink)?;
        Ok(sink.values)
    }
    pub fn finalize_into<S: EventSink>(&mut self, sink: &mut S) -> Result<(), Error> {
        if self.finalized {
            return Ok(());
        }
        self.close_items_into(sink)?;
        self.finalized = true;
        let mut event = self.typed_event(if self.status == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        });
        event.response = Some(ResponseSnapshot {
            state: self,
            created: false,
        });
        sink.emit(StreamEvent::Responses(event))
    }
    pub fn start_hosted_into<S: EventSink>(
        &mut self,
        start: &HostedToolStart,
        sink: &mut S,
    ) -> Result<(), Error> {
        for event in self.start_hosted(start)? {
            let event = crate::wire::normalize_stream_owned(Format::OpenAiResponses, event)?;
            sink.emit(StreamEvent::Compatibility {
                format: Format::OpenAiResponses,
                value: Cow::Owned(event),
            })?;
        }
        Ok(())
    }
    pub fn complete_hosted_into<S: EventSink>(
        &mut self,
        result: &HostedToolResult,
        sink: &mut S,
    ) -> Result<(), Error> {
        for event in self.complete_hosted(result)? {
            let event = crate::wire::normalize_stream_owned(Format::OpenAiResponses, event)?;
            sink.emit(StreamEvent::Compatibility {
                format: Format::OpenAiResponses,
                value: Cow::Owned(event),
            })?;
        }
        Ok(())
    }
    pub fn fail_into<S: EventSink>(
        &mut self,
        code: &str,
        message: &str,
        param: &str,
        sink: &mut S,
    ) -> Result<(), Error> {
        for event in self.fail(code, message, param) {
            let event =
                crate::wire::normalize_stream(Format::OpenAiResponses, &event).unwrap_or(event);
            sink.emit(StreamEvent::Compatibility {
                format: Format::OpenAiResponses,
                value: Cow::Owned(event),
            })?;
        }
        Ok(())
    }
    pub fn fail(&mut self, code: &str, message: &str, param: &str) -> Vec<Value> {
        if self.finalized {
            return vec![];
        }
        let code = if code.trim().is_empty() {
            "server_error"
        } else {
            code.trim()
        };
        let message = if message.trim().is_empty() {
            "upstream response stream failed"
        } else {
            message.trim()
        };
        self.status = "failed".into();
        let mut out = self.created();
        out.extend(self.close_items_legacy());
        self.finalized = true;
        let mut p = json!({"code":code,"message":message});
        if !param.is_empty() {
            p["param"] = json!(param);
        }
        out.push(self.event("error", p));
        let mut r = self.response();
        r["error"] = json!({"code":code,"message":message});
        out.push(self.event("response.failed", object([("response", r)])));
        out
    }
}
fn custom_input(raw: &str) -> String {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("input").map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| crate::json::stringify(v))
            })
        })
        .unwrap_or_else(|| raw.into())
}
