use super::StreamOptions;
use crate::{Context, Error, ResponsesToolState};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
struct Block {
    output_index: usize,
    id: String,
    kind: BlockKind,
    source_closed: bool,
    closed: bool,
    status: &'static str,
}

#[derive(Debug)]
enum BlockKind {
    Text {
        text: String,
        citations: Vec<Value>,
    },
    Reasoning {
        text: String,
        signature: String,
        redacted: bool,
    },
    Tool {
        call_id: String,
        name: String,
        namespace: String,
        arguments: String,
        custom: bool,
        search: bool,
    },
    WebSearch {
        arguments: String,
    },
}

impl Block {
    fn item(&self, status: &str, announced: bool) -> Value {
        match &self.kind {
            BlockKind::Text { text, citations } => json!({
                "type":"message", "id":self.id, "status":status, "role":"assistant",
                "content":if announced { Vec::new() } else { vec![text_part(text, citations)] },
            }),
            BlockKind::Reasoning {
                text,
                signature,
                redacted,
            } => {
                let mut item = json!({
                    "type":"reasoning", "id":self.id, "status":status,
                    "summary":if announced || text.is_empty() {
                        Vec::new()
                    } else {
                        vec![json!({"type":"summary_text","text":text})]
                    },
                });
                if !signature.is_empty() {
                    item["encrypted_content"] = json!(signature);
                }
                if *redacted {
                    item["claude_block_type"] = json!("redacted_thinking");
                }
                item
            }
            BlockKind::Tool {
                call_id,
                name,
                namespace,
                arguments,
                custom,
                search,
            } => {
                let mut item = json!({
                    "type":if *custom {
                        "custom_tool_call"
                    } else if *search {
                        "tool_search_call"
                    } else {
                        "function_call"
                    },
                    "id":self.id, "status":status, "call_id":call_id,
                });
                if *search {
                    item["execution"] = json!("client");
                    item["arguments"] = if announced {
                        json!({})
                    } else {
                        tool_input(arguments)
                    };
                } else {
                    item["name"] = json!(name);
                    if !namespace.is_empty() {
                        item["namespace"] = json!(namespace);
                    }
                    if *custom {
                        item["input"] = json!(if announced {
                            String::new()
                        } else {
                            crate::response::custom_tool_input_from_arguments(arguments, true)
                        });
                    } else {
                        item["arguments"] = json!(if announced {
                            ""
                        } else if arguments.is_empty() {
                            "{}"
                        } else {
                            arguments
                        });
                    }
                }
                item
            }
            BlockKind::WebSearch { arguments } => json!({
                "type":"web_search_call", "id":self.id, "status":status,
                "action":super::hosted::web_action(&tool_input(arguments))
                    .unwrap_or_else(|_| json!({"type":"search","query":""})),
            }),
        }
    }
}

/// The native opt-in route retains Claude block boundaries and opaque reasoning.
#[derive(Debug)]
pub(super) struct ClaudeResponses {
    id: String,
    id_fixed: bool,
    model: String,
    created_at: i64,
    sequence: Option<u64>,
    created: bool,
    finalized: bool,
    allow_unpaired_hosted_search: bool,
    mapping_ready: bool,
    tools: ResponsesToolState,
    blocks: BTreeMap<u64, Block>,
    ignored: BTreeSet<u64>,
    next_output: usize,
    stop_reason: String,
    native_usage: Value,
    error: Option<Value>,
    pub usage: Option<Value>,
    pub usage_text: String,
}

impl ClaudeResponses {
    pub fn new(options: &StreamOptions) -> Self {
        Self {
            id: if options.id.is_empty() {
                crate::new_id("resp_")
            } else {
                options.id.clone()
            },
            id_fixed: !options.id.is_empty(),
            model: options.model.clone(),
            created_at: if options.created_at == 0 {
                super::now()
            } else {
                options.created_at
            },
            sequence: options.emit_sequence_number.then_some(0),
            created: false,
            finalized: false,
            allow_unpaired_hosted_search: options.allow_unpaired_hosted_search,
            mapping_ready: false,
            tools: ResponsesToolState::default(),
            blocks: BTreeMap::new(),
            ignored: BTreeSet::new(),
            next_output: 0,
            stop_reason: String::new(),
            native_usage: json!({}),
            error: None,
            usage: None,
            usage_text: String::new(),
        }
    }

    pub fn convert(&mut self, ctx: &Context, input: &Value) -> Result<Vec<Value>, Error> {
        if self.finalized {
            return Ok(Vec::new());
        }
        self.configure(ctx);
        match string(&input["type"]) {
            "message_start" => {
                let message = &input["message"];
                if !self.created && !self.id_fixed && !string(&message["id"]).is_empty() {
                    self.id = string(&message["id"]).to_string();
                }
                if self.model.is_empty() {
                    self.model = string(&message["model"]).to_string();
                }
                self.merge_usage(&message["usage"]);
                Ok(self.ensure_created())
            }
            "content_block_start" => self.start_block(index(input), &input["content_block"]),
            "content_block_delta" => self.delta(index(input), &input["delta"]),
            "content_block_stop" => self.close_block(index(input)),
            "message_delta" => {
                self.merge_usage(&input["usage"]);
                if let Some(reason) = input.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = reason.to_string();
                }
                Ok(Vec::new())
            }
            "message_stop" => self.finalize(ctx),
            "error" => {
                let error = input.get("error").unwrap_or(input);
                Ok(self.fail(
                    error
                        .get("code")
                        .or_else(|| error.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("upstream_error"),
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Anthropic stream failed"),
                    error.get("param").and_then(Value::as_str).unwrap_or(""),
                ))
            }
            _ => Ok(Vec::new()),
        }
    }

    fn configure(&mut self, ctx: &Context) {
        if !self.mapping_ready {
            self.mapping_ready = true;
            self.tools = ctx.responses_tool_state();
        }
        if self.model.is_empty() && !ctx.origin_model_name.is_empty() {
            self.model = ctx.origin_model_name.clone();
        }
    }

    fn ensure_created(&mut self) -> Vec<Value> {
        if self.created {
            return Vec::new();
        }
        self.created = true;
        let response = self.snapshot("in_progress");
        vec![self.event("response.created", json!({"response":response}))]
    }

    fn start_block(&mut self, source_index: u64, input: &Value) -> Result<Vec<Value>, Error> {
        if self.blocks.contains_key(&source_index) || self.ignored.contains(&source_index) {
            return Err(Error::message("duplicate Anthropic content block index"));
        }
        if string(&input["type"]) == "web_search_tool_result" {
            let events = self.complete_web_search(input)?;
            self.ignored.insert(source_index);
            return Ok(events);
        }
        let kind = match string(&input["type"]) {
            "text" => BlockKind::Text {
                text: String::new(),
                citations: Vec::new(),
            },
            "thinking" => BlockKind::Reasoning {
                text: String::new(),
                signature: string(&input["signature"]).to_string(),
                redacted: false,
            },
            "redacted_thinking" => BlockKind::Reasoning {
                text: String::new(),
                signature: string(&input["data"]).to_string(),
                redacted: true,
            },
            "tool_use" => {
                let upstream_name = string(&input["name"]);
                let (namespace, name) = self.tools.responses_tool_name(upstream_name);
                BlockKind::Tool {
                    call_id: string(&input["id"]).to_string(),
                    name,
                    namespace,
                    arguments: String::new(),
                    custom: self.tools.is_custom_tool(upstream_name),
                    search: self.tools.tool_search && upstream_name == "tool_search",
                }
            }
            "server_tool_use" if string(&input["name"]) == "web_search" => {
                let id = string(&input["id"]);
                if id.is_empty()
                    || self.blocks.values().any(|block| {
                        matches!(block.kind, BlockKind::WebSearch { .. }) && block.id == id
                    })
                {
                    return Err(Error::message(
                        "Anthropic hosted web search requires a unique call id",
                    ));
                }
                if !input["caller"].is_null() {
                    return Err(Error::message(
                        "Responses web search cannot preserve Anthropic caller provenance",
                    ));
                }
                BlockKind::WebSearch {
                    arguments: String::new(),
                }
            }
            _ => {
                self.ignored.insert(source_index);
                return Ok(Vec::new());
            }
        };
        let prefix = match &kind {
            BlockKind::Text { .. } => "msg_",
            BlockKind::Reasoning { .. } => "rs_",
            BlockKind::Tool { custom: true, .. } => "ct_",
            BlockKind::Tool { search: true, .. } => "ts_",
            BlockKind::Tool { .. } => "fc_",
            BlockKind::WebSearch { .. } => "ws_",
        };
        let id = if matches!(kind, BlockKind::WebSearch { .. }) && !string(&input["id"]).is_empty()
        {
            string(&input["id"]).to_string()
        } else {
            crate::new_id(prefix)
        };
        let block = Block {
            output_index: self.next_output,
            id,
            kind,
            source_closed: false,
            closed: false,
            status: "in_progress",
        };
        self.next_output += 1;
        let item = block.item("in_progress", true);
        let output_index = block.output_index;
        let id = block.id.clone();
        let mut events = self.ensure_created();
        events.push(self.event(
            "response.output_item.added",
            json!({
                "output_index":output_index, "item":item,
            }),
        ));
        match &block.kind {
            BlockKind::Text { .. } => events.push(self.event(
                "response.content_part.added",
                json!({
                    "output_index":output_index, "content_index":0, "item_id":id,
                    "part":{"type":"output_text","text":"","annotations":[]},
                }),
            )),
            BlockKind::Reasoning { .. } => {
                events.push(self.event(
                    "response.reasoning_summary_part.added",
                    json!({
                        "output_index":output_index, "summary_index":0, "item_id":id,
                        "part":{"type":"summary_text","text":""},
                    }),
                ));
            }
            BlockKind::WebSearch { .. } => {
                events.push(self.event(
                    "response.web_search_call.in_progress",
                    json!({"output_index":output_index,"item_id":id}),
                ));
                events.push(self.event(
                    "response.web_search_call.searching",
                    json!({"output_index":output_index,"item_id":id}),
                ));
            }
            _ => {}
        }
        self.blocks.insert(source_index, block);
        let initial = match string(&input["type"]) {
            "text" => Some(json!({"type":"text_delta","text":input["text"]})),
            "thinking" => Some(json!({"type":"thinking_delta","thinking":input["thinking"]})),
            "tool_use" | "server_tool_use"
                if !input["input"].is_null() && input["input"] != json!({}) =>
            {
                Some(json!({"type":"input_json_delta","partial_json":input["input"].to_string()}))
            }
            _ => None,
        };
        if let Some(initial) = initial {
            events.extend(self.delta(source_index, &initial)?);
        }
        if let Some(citations) = input["citations"].as_array() {
            for citation in citations {
                events.extend(self.delta(
                    source_index,
                    &json!({
                        "type":"citations_delta","citation":citation,
                    }),
                )?);
            }
        }
        Ok(events)
    }

    fn delta(&mut self, source_index: u64, delta: &Value) -> Result<Vec<Value>, Error> {
        if self.ignored.contains(&source_index) {
            return Ok(Vec::new());
        }
        let Some(block) = self.blocks.get_mut(&source_index) else {
            return Err(Error::message(
                "Anthropic content delta has no matching block start",
            ));
        };
        if block.source_closed {
            return Err(Error::message(
                "Anthropic content delta arrived after block stop",
            ));
        }
        let output_index = block.output_index;
        let id = block.id.clone();
        let event = match (&mut block.kind, string(&delta["type"])) {
            (BlockKind::Text { text, .. }, "text_delta") => {
                let fragment = string(&delta["text"]);
                if fragment.is_empty() {
                    return Ok(Vec::new());
                }
                text.push_str(fragment);
                self.usage_text.push_str(fragment);
                Some((
                    "response.output_text.delta",
                    json!({
                        "output_index":output_index, "content_index":0, "item_id":id, "delta":fragment,
                    }),
                ))
            }
            (BlockKind::Text { text, citations }, "citations_delta") => {
                let citation = delta.get("citation").cloned().unwrap_or(Value::Null);
                let before = annotations(text, citations).len();
                citations.push(citation);
                let added = annotations(text, citations);
                added.get(before).cloned().map(|annotation| {
                    (
                        "response.output_text.annotation.added",
                        json!({
                            "output_index":output_index, "content_index":0, "item_id":id,
                            "annotation_index":before, "annotation":annotation,
                        }),
                    )
                })
            }
            (
                BlockKind::Reasoning {
                    text,
                    redacted: false,
                    ..
                },
                "thinking_delta",
            ) => {
                let fragment = string(&delta["thinking"]);
                if fragment.is_empty() {
                    return Ok(Vec::new());
                }
                text.push_str(fragment);
                self.usage_text.push_str(fragment);
                Some((
                    "response.reasoning_summary_text.delta",
                    json!({
                        "output_index":output_index, "summary_index":0, "item_id":id, "delta":fragment,
                    }),
                ))
            }
            (BlockKind::Reasoning { signature, .. }, "signature_delta") => {
                signature.push_str(string(&delta["signature"]));
                None
            }
            (
                BlockKind::Tool {
                    call_id,
                    name,
                    namespace,
                    arguments,
                    custom,
                    search,
                },
                "input_json_delta",
            ) => {
                let fragment = string(&delta["partial_json"]);
                arguments.push_str(fragment);
                if *custom || *search || fragment.is_empty() {
                    None
                } else {
                    let mut fields = tool_fields(output_index, &id, call_id, name, namespace);
                    fields["delta"] = json!(fragment);
                    Some(("response.function_call_arguments.delta", fields))
                }
            }
            (BlockKind::WebSearch { arguments }, "input_json_delta") => {
                arguments.push_str(string(&delta["partial_json"]));
                None
            }
            _ => None,
        };
        Ok(event
            .map(|(kind, fields)| vec![self.event(kind, fields)])
            .unwrap_or_default())
    }

    fn close_block(&mut self, source_index: u64) -> Result<Vec<Value>, Error> {
        if self.ignored.contains(&source_index) {
            return Ok(Vec::new());
        }
        let Some(block) = self.blocks.get_mut(&source_index) else {
            return Err(Error::message(
                "Anthropic content block stop has no matching start",
            ));
        };
        if block.closed {
            return Ok(Vec::new());
        }
        block.source_closed = true;
        if let BlockKind::WebSearch { arguments } = &block.kind {
            let action = if arguments.is_empty() {
                json!({})
            } else {
                serde_json::from_str(arguments).map_err(|error| {
                    Error::message(format!("invalid Anthropic web search input: {error}"))
                })?
            };
            super::hosted::web_action(&action)?;
            // Ending the input block starts execution, not its successful completion.
            return Ok(Vec::new());
        }
        block.closed = true;
        block.status = "completed";
        let index = block.output_index;
        let id = block.id.clone();
        let item = block.item("completed", false);
        let mut pending = Vec::new();
        match &block.kind {
            BlockKind::Text { text, citations } => {
                pending.push((
                    "response.output_text.done",
                    json!({
                        "output_index":index,"content_index":0,"item_id":id,"text":text,
                    }),
                ));
                pending.push((
                    "response.content_part.done",
                    json!({
                        "output_index":index,"content_index":0,"item_id":id,
                        "part":text_part(text, citations),
                    }),
                ));
            }
            BlockKind::Reasoning { text, .. } => {
                pending.push((
                    "response.reasoning_summary_text.done",
                    json!({
                        "output_index":index,"summary_index":0,"item_id":id,"text":text,
                    }),
                ));
                pending.push((
                    "response.reasoning_summary_part.done",
                    json!({
                        "output_index":index,"summary_index":0,"item_id":id,
                        "part":{"type":"summary_text","text":text},
                    }),
                ));
            }
            BlockKind::Tool {
                custom,
                search,
                name,
                namespace,
                call_id,
                arguments,
            } => {
                let mut fields = tool_fields(index, &id, call_id, name, namespace);
                if *custom {
                    let input = crate::response::custom_tool_input_from_arguments(arguments, true);
                    if !input.is_empty() {
                        let mut delta = fields.clone();
                        delta["delta"] = json!(input);
                        pending.push(("response.custom_tool_call_input.delta", delta));
                    }
                    fields["input"] = json!(input);
                    pending.push(("response.custom_tool_call_input.done", fields));
                } else if !*search {
                    fields["arguments"] = json!(if arguments.is_empty() {
                        "{}"
                    } else {
                        arguments
                    });
                    pending.push(("response.function_call_arguments.done", fields));
                }
            }
            BlockKind::WebSearch { .. } => unreachable!(),
        }
        pending.push((
            "response.output_item.done",
            json!({"output_index":index,"item":item}),
        ));
        Ok(pending
            .into_iter()
            .map(|(kind, fields)| self.event(kind, fields))
            .collect())
    }

    fn complete_web_search(&mut self, input: &Value) -> Result<Vec<Value>, Error> {
        let call_id = string(&input["tool_use_id"]);
        let Some(block) = self
            .blocks
            .values_mut()
            .find(|block| matches!(block.kind, BlockKind::WebSearch { .. }) && block.id == call_id)
        else {
            return Err(Error::message(
                "Anthropic web search result has no matching call",
            ));
        };
        if block.closed || !block.source_closed {
            return Err(Error::message(
                "Anthropic web search result is duplicated or precedes its input stop",
            ));
        }
        let failed = input["is_error"] == true
            || !string(&input["error_code"]).is_empty()
            || string(&input["content"]["type"]).ends_with("_error");
        block.closed = true;
        block.status = if failed { "failed" } else { "completed" };
        let output_index = block.output_index;
        let id = block.id.clone();
        let item = block.item(block.status, false);
        let mut events = Vec::new();
        if !failed {
            events.push(self.event(
                "response.web_search_call.completed",
                json!({"output_index":output_index,"item_id":id}),
            ));
        }
        events.push(self.event(
            "response.output_item.done",
            json!({"output_index":output_index,"item":item}),
        ));
        Ok(events)
    }

    pub fn finalize(&mut self, ctx: &Context) -> Result<Vec<Value>, Error> {
        if self.finalized {
            return Ok(Vec::new());
        }
        let unpaired: Vec<_> = self
            .blocks
            .iter()
            .filter(|(_, block)| matches!(block.kind, BlockKind::WebSearch { .. }) && !block.closed)
            .map(|(index, _)| *index)
            .collect();
        if !unpaired.is_empty() {
            if !self.allow_unpaired_hosted_search {
                return Err(Error::message(
                    "Anthropic response ended before a hosted web search result",
                ));
            }
        }
        self.configure(ctx);
        let mut events = self.ensure_created();
        for index in unpaired {
            let Some(block) = self.blocks.get_mut(&index) else {
                continue;
            };
            block.source_closed = true;
            block.closed = true;
            block.status = "completed";
            let output_index = block.output_index;
            let id = block.id.clone();
            let item = block.item("completed", false);
            events.push(self.event(
                "response.web_search_call.completed",
                json!({"output_index":output_index,"item_id":id}),
            ));
            events.push(self.event(
                "response.output_item.done",
                json!({"output_index":output_index,"item":item}),
            ));
        }
        let keys: Vec<_> = self
            .blocks
            .iter()
            .filter(|(_, block)| !block.closed)
            .map(|(index, _)| *index)
            .collect();
        for index in keys {
            events.extend(self.close_block(index)?);
        }
        self.finalized = true;
        let status = match self.stop_reason.as_str() {
            "max_tokens" | "pause_turn" | "model_context_window_exceeded" => "incomplete",
            "refusal" => "incomplete",
            _ => "completed",
        };
        let response = self.snapshot(status);
        events.push(self.event(
            if status == "incomplete" {
                "response.incomplete"
            } else {
                "response.completed"
            },
            json!({"response":response}),
        ));
        Ok(events)
    }

    pub fn fail(&mut self, code: &str, message: &str, param: &str) -> Vec<Value> {
        if self.finalized {
            return Vec::new();
        }
        let mut events = self.ensure_created();
        self.error = Some(json!({"code":code,"message":message}));
        self.finalized = true;
        events.push(self.event(
            "error",
            json!({
                "code":code,"message":message,
                "param":if param.is_empty() { Value::Null } else { json!(param) },
            }),
        ));
        let response = self.snapshot("failed");
        events.push(self.event("response.failed", json!({"response":response})));
        events
    }

    fn snapshot(&self, status: &str) -> Value {
        let mut blocks: Vec<_> = self.blocks.values().collect();
        blocks.sort_unstable_by_key(|block| block.output_index);
        let output: Vec<_> = blocks
            .into_iter()
            .map(|block| {
                block.item(
                    if block.closed {
                        block.status
                    } else {
                        if status == "failed" {
                            "incomplete"
                        } else {
                            "in_progress"
                        }
                    },
                    false,
                )
            })
            .collect();
        let canonical = self
            .usage
            .clone()
            .unwrap_or_else(|| crate::response::usage_from_claude(&Value::Null));
        let input = number(&canonical["prompt_tokens"]);
        let output_tokens = number(&canonical["completion_tokens"]);
        let cache_creation = number(&self.native_usage["cache_creation_input_tokens"]).max(
            number(&self.native_usage["cache_creation"]["ephemeral_5m_input_tokens"])
                .saturating_add(number(
                    &self.native_usage["cache_creation"]["ephemeral_1h_input_tokens"],
                )),
        );
        let mut response = json!({
            "id":self.id,"object":"response","created_at":self.created_at,
            "status":status,"model":self.model,"output":output,"parallel_tool_calls":true,
            "usage":if status == "in_progress" { Value::Null } else { json!({
                "input_tokens":input,"input_tokens_details":{"cached_tokens":number(&self.native_usage["cache_read_input_tokens"])},
                "output_tokens":output_tokens,"output_tokens_details":{"reasoning_tokens":0},
                "total_tokens":input.saturating_add(output_tokens),
                "cache_creation_input_tokens":cache_creation,
            }) },
            "error":self.error,
        });
        if status == "incomplete" {
            response["incomplete_details"] = json!({"reason":if self.stop_reason == "refusal" {
                "content_filter"
            } else {
                "max_output_tokens"
            }});
        }
        response
    }

    fn merge_usage(&mut self, usage: &Value) {
        if !usage.is_object() {
            return;
        }
        merge_cumulative(&mut self.native_usage, usage);
        self.usage = Some(crate::response::usage_from_claude(&self.native_usage));
    }

    fn event(&mut self, kind: &str, mut fields: Value) -> Value {
        fields["type"] = json!(kind);
        if let Some(sequence) = &mut self.sequence {
            fields["sequence_number"] = json!(*sequence);
            *sequence = sequence.saturating_add(1);
        }
        fields
    }
}

pub(crate) fn buffered(ctx: &Context, input: &Value) -> Result<Value, Error> {
    let mut state = ClaudeResponses::new(&StreamOptions {
        id: string(&input["id"]).to_string(),
        model: if ctx.origin_model_name.is_empty() {
            string(&input["model"]).to_string()
        } else {
            ctx.origin_model_name.clone()
        },
        created_at: if ctx.created == 0 {
            super::now()
        } else {
            ctx.created
        },
        preserve_message_structure: true,
        allow_unpaired_hosted_search: ctx.options.allow_unpaired_hosted_search,
        ..StreamOptions::default()
    });
    state.convert(ctx, &json!({"type":"message_start","message":input}))?;
    // Buffered and streaming conversion share the same block/lifecycle machinery.
    if let Some(content) = input["content"].as_array() {
        for (index, block) in content.iter().enumerate() {
            state.convert(
                ctx,
                &json!({"type":"content_block_start","index":index,"content_block":block}),
            )?;
            state.convert(ctx, &json!({"type":"content_block_stop","index":index}))?;
        }
    }
    state.convert(ctx, &json!({
        "type":"message_delta","delta":{"stop_reason":input["stop_reason"]},"usage":input["usage"],
    }))?;
    state
        .finalize(ctx)?
        .into_iter()
        .find_map(|event| {
            event
                .get("response")
                .filter(|response| {
                    matches!(
                        response["status"].as_str(),
                        Some("completed" | "incomplete")
                    )
                })
                .cloned()
        })
        .ok_or_else(|| Error::message("Anthropic response did not produce a terminal response"))
}

fn string(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

fn number(value: &Value) -> u64 {
    value.as_u64().unwrap_or(0)
}

fn index(value: &Value) -> u64 {
    number(&value["index"])
}

fn tool_input(arguments: &str) -> Value {
    if arguments.is_empty() {
        json!({})
    } else {
        serde_json::from_str(arguments).unwrap_or_else(|_| json!(arguments))
    }
}

fn tool_fields(index: usize, id: &str, call_id: &str, name: &str, namespace: &str) -> Value {
    let mut fields = json!({
        "output_index":index,"item_id":id,"call_id":call_id,"name":name,
    });
    if !namespace.is_empty() {
        fields["namespace"] = json!(namespace);
    }
    fields
}

fn annotations(text: &str, citations: &[Value]) -> Vec<Value> {
    crate::response::citations::to_responses(&json!(crate::response::citations::claude_to_chat(
        &json!(citations),
        text,
        0
    )))
    .unwrap_or_default()
}

fn text_part(text: &str, citations: &[Value]) -> Value {
    json!({"type":"output_text","text":text,"annotations":annotations(text, citations)})
}

pub(super) fn merge_cumulative(target: &mut Value, incoming: &Value) {
    let Some(fields) = incoming.as_object() else {
        return;
    };
    for (key, value) in fields {
        if value.is_object() {
            if !target[key].is_object() {
                target[key] = json!({});
            }
            merge_cumulative(&mut target[key], value);
        } else if value.is_u64() {
            target[key] = json!(number(&target[key]).max(number(value)));
        } else if !value.is_null() {
            target[key] = value.clone();
        }
    }
}
