//! Borrowed wire views for events emitted by the Chat→Responses state machine.
use super::{ChatResponses, Segment, Tool};
use serde::{
    Serialize, Serializer,
    ser::{SerializeMap, SerializeSeq},
};
use serde_json::Value;
use std::borrow::Cow;

/// A Responses event consumed synchronously while its state remains borrowed.
#[derive(Debug, Serialize)]
pub struct ResponsesEvent<'a> {
    #[serde(rename = "type")]
    pub(super) kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) response: Option<ResponseSnapshot<'a>>,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub(super) delta: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) arguments: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) input: Option<&'a str>,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub(super) name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) item: Option<OutputItem<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) sequence_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) annotation: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) annotation_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) output_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) content_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) summary_index: Option<usize>,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub(super) item_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) part: Option<SummaryPart<'a>>,
}
impl ResponsesEvent<'_> {
    pub fn event_type(&self) -> &'static str {
        self.kind
    }
}
impl<'a> ResponsesEvent<'a> {
    pub(super) fn new(kind: &'static str, sequence_number: Option<u64>) -> Self {
        Self {
            kind,
            response: None,
            delta: "",
            arguments: None,
            input: None,
            name: "",
            text: None,
            item: None,
            sequence_number,
            annotation: None,
            annotation_index: None,
            output_index: None,
            content_index: None,
            summary_index: None,
            item_id: "",
            part: None,
        }
    }
}
#[derive(Debug, Serialize)]
pub(super) struct SummaryPart<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: &'a str,
}
#[derive(Debug)]
pub(super) enum OutputItem<'a> {
    Segment {
        segment: &'a Segment,
        status: &'a str,
        added: bool,
    },
    Tool {
        tool: &'a Tool,
        status: &'a str,
        empty: bool,
    },
    Hosted(&'a Value),
}
impl Serialize for OutputItem<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Hosted(value) => {
                crate::wire::normalized_view("ResponsesOutput", value).serialize(serializer)
            }
            Self::Segment {
                segment: s,
                status,
                added,
            } => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("type", s.kind)?;
                map.serialize_entry("id", &s.id)?;
                map.serialize_entry("status", status)?;
                map.serialize_entry("role", if s.kind == "message" { "assistant" } else { "" })?;
                if s.kind == "message" {
                    #[derive(Serialize)]
                    struct Content<'a> {
                        #[serde(rename = "type")]
                        kind: &'static str,
                        text: &'a str,
                        annotations: AnnotationList<'a>,
                    }
                    if *added {
                        map.serialize_entry("content", &[] as &[Content<'_>])?;
                    } else {
                        map.serialize_entry(
                            "content",
                            &[Content {
                                kind: "output_text",
                                text: &s.text,
                                annotations: AnnotationList(&s.annotations),
                            }],
                        )?;
                    }
                } else {
                    map.serialize_entry("content", &Option::<()>::None)?;
                    if *added || s.preserve && s.text.is_empty() {
                        map.serialize_entry("summary", &[] as &[SummaryPart<'_>])?;
                    } else {
                        map.serialize_entry(
                            "summary",
                            &[SummaryPart {
                                kind: "summary_text",
                                text: &s.text,
                            }],
                        )?;
                    }
                    if !s.signature.is_empty() {
                        map.serialize_entry("encrypted_content", &s.signature)?;
                    }
                    if s.redacted {
                        map.serialize_entry("claude_block_type", "redacted_thinking")?;
                    }
                }
                map.serialize_entry("quality", "")?;
                map.serialize_entry("size", "")?;
                map.end()
            }
            Self::Tool {
                tool: t,
                status,
                empty,
            } => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry(
                    "type",
                    if t.search {
                        "tool_search_call"
                    } else if t.custom {
                        "custom_tool_call"
                    } else {
                        "function_call"
                    },
                )?;
                map.serialize_entry("id", &t.id)?;
                map.serialize_entry("status", status)?;
                if !t.custom && !t.search {
                    map.serialize_entry("role", "")?;
                    map.serialize_entry("content", &Option::<()>::None)?;
                    map.serialize_entry("quality", "")?;
                    map.serialize_entry("size", "")?;
                }
                let call_id = if t.call_id.is_empty() {
                    &t.id
                } else {
                    &t.call_id
                };
                if !call_id.is_empty() {
                    map.serialize_entry("call_id", call_id)?;
                }
                let name = if t.restored_name.is_empty() {
                    &t.name
                } else {
                    &t.restored_name
                };
                if !name.is_empty() && !t.search {
                    map.serialize_entry("name", name)?;
                }
                if !t.namespace.is_empty() {
                    map.serialize_entry("namespace", &t.namespace)?;
                }
                if !t.signature.is_empty() {
                    map.serialize_entry("thought_signature", &t.signature)?;
                }
                if t.search {
                    map.serialize_entry("execution", "client")?;
                    let arguments = if *empty { serde_json::json!({}) } else {
                        serde_json::from_str::<Value>(&t.args)
                            .map_err(serde::ser::Error::custom)?
                    };
                    map.serialize_entry("arguments", &arguments)?;
                    return map.end();
                }
                let args = if *empty {
                    Cow::Borrowed("")
                } else if t.custom {
                    Cow::Owned(crate::response::custom_tool_input_from_arguments(&t.args, t.preserve))
                } else {
                    Cow::Borrowed(t.args.as_str())
                };
                map.serialize_entry(if t.custom { "input" } else { "arguments" }, &args)?;
                map.end()
            }
        }
    }
}
// The terminal content list is Go's []any, whereas annotation.added carries
// RawMessage. Normalize this uncommon island without copying its surrounding text.
#[derive(Debug)]
struct AnnotationList<'a>(&'a [Value]);
impl Serialize for AnnotationList<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for annotation in self.0 {
            seq.serialize_element(&crate::wire::normalized_view("any", annotation))?;
        }
        seq.end()
    }
}
#[derive(Debug)]
pub(super) struct ResponseSnapshot<'a> {
    pub state: &'a ChatResponses,
    pub created: bool,
}
struct Outputs<'a>(&'a ChatResponses);
impl Serialize for Outputs<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let state = self.0;
        let mut seq = serializer.serialize_seq(Some(state.output_order.len()))?;
        for (kind, index) in &state.output_order {
            let item = if *kind == 2 {
                OutputItem::Hosted(&state.hosted[*index].0)
            } else if *kind == 1 {
                let Some(tool) = state.tools.get(&(*index as i64)) else {
                    continue;
                };
                OutputItem::Tool {
                    tool,
                    status: state.status(),
                    empty: false,
                }
            } else {
                OutputItem::Segment {
                    segment: &state.segments[*index],
                    status: state.status(),
                    added: false,
                }
            };
            seq.serialize_element(&item)?;
        }
        seq.end()
    }
}
impl Serialize for ResponseSnapshot<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let s = self.state;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("id", &s.id)?;
        map.serialize_entry("object", "response")?;
        map.serialize_entry("created_at", &s.created)?;
        map.serialize_entry(
            "status",
            if self.created {
                "in_progress"
            } else {
                &s.status
            },
        )?;
        if !self.created {
            if let Some(details) = &s.incomplete {
                map.serialize_entry(
                    "incomplete_details",
                    &crate::wire::normalized_view("IncompleteDetails", details),
                )?;
            }
        }
        map.serialize_entry("instructions", &Option::<()>::None)?;
        map.serialize_entry("max_output_tokens", &0)?;
        map.serialize_entry("model", &s.model)?;
        if self.created {
            map.serialize_entry("output", &[] as &[Value])?;
        } else {
            map.serialize_entry("output", &Outputs(s))?;
        }
        map.serialize_entry("parallel_tool_calls", &false)?;
        map.serialize_entry("previous_response_id", &Option::<()>::None)?;
        map.serialize_entry("reasoning", &Option::<()>::None)?;
        map.serialize_entry("store", &false)?;
        map.serialize_entry("temperature", &0)?;
        map.serialize_entry("tool_choice", &Option::<()>::None)?;
        map.serialize_entry("tools", &Option::<()>::None)?;
        map.serialize_entry("top_p", &0)?;
        map.serialize_entry("truncation", &Option::<()>::None)?;
        if !self.created {
            if let Some(usage) = &s.usage {
                map.serialize_entry("usage", &crate::wire::normalized_view("Usage", usage))?;
            } else {
                map.serialize_entry("usage", &Option::<()>::None)?;
            }
        } else {
            map.serialize_entry("usage", &Option::<()>::None)?;
        }
        map.serialize_entry("user", &Option::<()>::None)?;
        map.serialize_entry("metadata", &Option::<()>::None)?;
        map.end()
    }
}
