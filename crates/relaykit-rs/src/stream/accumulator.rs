use crate::response::{append_separated, arr, n, reasoning_output, response_output, s, stringify};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Reconstructs a Responses output array when the terminal SSE response omits
/// it. Item identity takes precedence over event arrival order; pending tool
/// arguments can arrive before their output-item announcement.
#[derive(Default, Debug)]
pub struct ResponsesBufferedAccumulator {
    items: Vec<Item>,
    tools: Vec<Tool>,
    item_output: BTreeMap<i64, usize>,
    item_ids: BTreeMap<String, usize>,
    last_unindexed: Option<usize>,
    tool_output: BTreeMap<i64, usize>,
    tool_ids: BTreeMap<String, usize>,
    pending_output: BTreeMap<i64, String>,
    pending_ids: BTreeMap<String, String>,
}
#[derive(Default, Debug)]
struct Item {
    kind: String,
    id: String,
    text: String,
    annotations: Vec<Value>,
    tool: Option<usize>,
    reasoning_break: bool,
}
#[derive(Default, Debug)]
struct Tool {
    call_id: String,
    item_id: String,
    name: String,
    arguments: String,
}
fn item_id(v: &Value) -> &str {
    let id = s(&v["item"]["id"]).trim();
    if id.is_empty() {
        s(&v["item_id"]).trim()
    } else {
        id
    }
}
impl ResponsesBufferedAccumulator {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn process_event(&mut self, event: &Value) {
        match s(&event["type"]) {
            "response.output_text.delta" => {
                let idx = self.ensure_item(event, "message");
                self.items[idx].text.push_str(s(&event["delta"]));
            }
            "response.output_text.annotation.added" => {
                let idx = self.ensure_item(event, "message");
                if !event["annotation"].is_null() {
                    self.items[idx]
                        .annotations
                        .push(event["annotation"].clone());
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let idx = self.ensure_item(event, "reasoning");
                let item = &mut self.items[idx];
                if item.reasoning_break {
                    append_separated(&mut item.text, s(&event["delta"]));
                    item.reasoning_break = false;
                } else {
                    item.text.push_str(s(&event["delta"]));
                }
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                let idx = self.ensure_item(event, "reasoning");
                let item = &mut self.items[idx];
                if item.text.is_empty() {
                    item.text.push_str(s(&event["text"]));
                }
                if !item.text.is_empty() {
                    item.reasoning_break = true;
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                let kind = s(&event["item"]["type"]);
                match kind {
                    "reasoning" => {
                        let idx = self.ensure_item(event, kind);
                        if self.items[idx].text.is_empty() {
                            self.items[idx].text = reasoning_output(&event["item"]);
                        }
                    }
                    "message" => {
                        let idx = self.ensure_item(event, kind);
                        let item = &mut self.items[idx];
                        let seed_text = item.text.is_empty();
                        let seed_annotations = item.annotations.is_empty();
                        for content in arr(&event["item"]["content"]) {
                            if s(&content["type"]) == "output_text" {
                                if seed_text {
                                    item.text.push_str(s(&content["text"]));
                                }
                                if seed_annotations {
                                    item.annotations
                                        .extend(arr(&content["annotations"]).iter().cloned());
                                }
                            }
                        }
                    }
                    "function_call" | "custom_tool_call" => {
                        let idx = self.ensure_tool(event);
                        let args = stringify(&event["item"]["arguments"]);
                        if !args.is_empty() {
                            self.tools[idx].arguments = args;
                        }
                    }
                    _ => {}
                }
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                let delta = s(&event["delta"]);
                if let Some(idx) = self.find_tool(event) {
                    self.tools[idx].arguments.push_str(delta);
                } else if !event["output_index"].is_null() {
                    self.pending_output
                        .entry(n(&event["output_index"]))
                        .or_default()
                        .push_str(delta);
                } else if !s(&event["item_id"]).trim().is_empty() {
                    self.pending_ids
                        .entry(s(&event["item_id"]).trim().into())
                        .or_default()
                        .push_str(delta);
                }
            }
            _ => {}
        }
    }
    pub fn supplement_response_output(&self, response: &mut Value) {
        if response.is_object() && arr(&response["output"]).is_empty() {
            response["output"] = Value::Array(self.build_output());
        }
    }
    pub fn build_output(&self) -> Vec<Value> {
        let mut out = vec![];
        for item in &self.items {
            let mut value = response_output(&item.kind, &item.id, "");
            match item.kind.as_str() {
                "reasoning" => {
                    if item.text.is_empty() {
                        continue;
                    }
                    value["summary"] = json!([{"type":"summary_text","text":item.text}]);
                }
                "message" => {
                    if item.text.is_empty() {
                        continue;
                    }
                    value["role"] = json!("assistant");
                    value["content"] = json!([{"type":"output_text","text":item.text,"annotations":if item.annotations.is_empty(){Value::Null}else{json!(item.annotations)}}]);
                }
                "function_call" | "custom_tool_call" => {
                    let Some(tool) = item.tool.and_then(|idx| self.tools.get(idx)) else {
                        continue;
                    };
                    value["id"] = json!(tool.item_id);
                    if !tool.call_id.is_empty() {
                        value["call_id"] = json!(tool.call_id);
                    }
                    if !tool.name.is_empty() {
                        value["name"] = json!(tool.name);
                    }
                    value["arguments"] = json!(tool.arguments);
                    // The upstream accumulator stores custom inputs in Arguments;
                    // the custom variant's serializer consequently emits empty input.
                    if item.kind == "custom_tool_call" {
                        value = json!({"type":"custom_tool_call","call_id":tool.call_id,"name":tool.name,"input":""});
                        if !tool.item_id.is_empty() {
                            value["id"] = json!(tool.item_id);
                        }
                    }
                }
                _ => continue,
            }
            out.push(value);
        }
        out
    }
    fn find_item(&self, event: &Value) -> Option<usize> {
        if !event["output_index"].is_null() {
            if let Some(idx) = self.item_output.get(&n(&event["output_index"])) {
                return Some(*idx);
            }
        }
        self.item_ids.get(item_id(event)).copied()
    }
    fn ensure_item(&mut self, event: &Value, kind: &str) -> usize {
        let idx = if let Some(idx) = self.find_item(event) {
            if self.items[idx].kind.is_empty() {
                self.items[idx].kind = kind.into();
            }
            idx
        } else {
            if event["output_index"].is_null() && item_id(event).is_empty() {
                if let Some(idx) = self.last_unindexed {
                    if self.items[idx].kind == kind {
                        return idx;
                    }
                }
            }
            let idx = self.items.len();
            self.items.push(Item {
                kind: kind.into(),
                ..Default::default()
            });
            self.last_unindexed = Some(idx);
            idx
        };
        if !event["output_index"].is_null() {
            self.item_output.insert(n(&event["output_index"]), idx);
        }
        let id = item_id(event);
        if !id.is_empty() {
            if self.items[idx].id != id {
                self.items[idx].id = id.into();
            }
            if self.item_ids.get(id) != Some(&idx) {
                self.item_ids.insert(id.into(), idx);
            }
        }
        idx
    }
    fn find_tool(&self, event: &Value) -> Option<usize> {
        if !event["output_index"].is_null() {
            if let Some(idx) = self.tool_output.get(&n(&event["output_index"])) {
                return Some(*idx);
            }
        }
        let id = if s(&event["item_id"]).trim().is_empty() {
            s(&event["item"]["id"]).trim()
        } else {
            s(&event["item_id"]).trim()
        };
        self.tool_ids.get(id).copied()
    }
    fn apply_tool(tool: &mut Tool, event: &Value) {
        let v = &event["item"];
        let id = s(&v["id"]).trim();
        if !id.is_empty() && tool.item_id != id {
            tool.item_id = id.into();
        }
        if !s(&v["call_id"]).trim().is_empty() && tool.call_id != s(&v["call_id"]).trim() {
            tool.call_id = s(&v["call_id"]).trim().into();
        } else if tool.call_id.is_empty() {
            tool.call_id = id.into();
        }
        if !s(&v["name"]).trim().is_empty() && tool.name != s(&v["name"]).trim() {
            tool.name = s(&v["name"]).trim().into();
        }
    }
    fn ensure_tool(&mut self, event: &Value) -> usize {
        if let Some(idx) = self.find_tool(event) {
            Self::apply_tool(&mut self.tools[idx], event);
            let item = self.ensure_item(event, s(&event["item"]["type"]));
            self.items[item].tool = Some(idx);
            return idx;
        }
        let mut tool = Tool::default();
        Self::apply_tool(&mut tool, event);
        let idx = self.tools.len();
        let item = self.ensure_item(event, s(&event["item"]["type"]));
        self.items[item].tool = Some(idx);
        if !event["output_index"].is_null() {
            let oi = n(&event["output_index"]);
            self.tool_output.insert(oi, idx);
            if let Some(pending) = self.pending_output.remove(&oi) {
                tool.arguments.push_str(&pending);
            }
        }
        if !tool.item_id.is_empty() {
            self.tool_ids.insert(tool.item_id.clone(), idx);
            if let Some(pending) = self.pending_ids.remove(&tool.item_id) {
                tool.arguments.push_str(&pending);
            }
        }
        self.tools.push(tool);
        idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_arguments_and_reasoning_boundaries() {
        let mut a = ResponsesBufferedAccumulator::new();
        a.process_event(&json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"x\":"}));
        a.process_event(&json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"i","call_id":"c","name":"f"}}));
        a.process_event(
            &json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"1}"}),
        );
        a.process_event(&json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"first"}));
        a.process_event(
            &json!({"type":"response.reasoning_summary_text.done","output_index":0,"text":"first"}),
        );
        a.process_event(&json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"second"}));
        let out = a.build_output();
        assert_eq!(out[0]["arguments"], "{\"x\":1}");
        assert_eq!(out[1]["summary"][0]["text"], "first\n\nsecond");
    }
}
