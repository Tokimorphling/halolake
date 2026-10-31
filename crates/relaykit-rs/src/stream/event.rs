//! Synchronously consumed protocol event views.
//!
//! Event views may borrow the current input and stream state. A sink must finish
//! consuming the view before returning; retain encoded bytes or `to_value()` if
//! the event needs to outlive that call.
use crate::{Error, Format};
use serde::{Serialize, Serializer, ser::SerializeMap};
use serde_json::Value;
use std::{borrow::Cow, io::Write};

#[derive(Debug)]
pub enum StreamEvent<'a> {
    Chat(ChatEvent<'a>),
    Claude(super::claude::ClaudeEvent<'a>),
    Responses(super::chat_responses::ResponsesEvent<'a>),
    Gemini(super::gemini::GeminiEvent<'a>),
    /// An event whose opaque fields still use the compatibility representation.
    Compatibility {
        format: Format,
        value: Cow<'a, Value>,
    },
}
impl StreamEvent<'_> {
    pub fn format(&self) -> Format {
        match self {
            Self::Chat(_) => Format::OpenAi,
            Self::Claude(_) => Format::Claude,
            Self::Responses(_) => Format::OpenAiResponses,
            Self::Gemini(_) => Format::Gemini,
            Self::Compatibility { format, .. } => *format,
        }
    }
    pub fn is_typed(&self) -> bool {
        !matches!(self, Self::Compatibility { .. })
    }
    pub fn to_value(&self) -> Result<Value, Error> {
        Ok(serde_json::to_value(self)?)
    }
    pub fn to_vec(&self) -> Result<Vec<u8>, Error> {
        Ok(serde_json::to_vec(self)?)
    }
    pub fn write_to<W: Write>(&self, writer: W) -> Result<(), Error> {
        Ok(serde_json::to_writer(writer, self)?)
    }
}
impl Serialize for StreamEvent<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Chat(event) => event.serialize(s),
            Self::Claude(event) => event.serialize(s),
            Self::Responses(event) => event.serialize(s),
            Self::Gemini(event) => event.serialize(s),
            Self::Compatibility { format, value } => crate::wire::normalized_view(
                match format {
                    Format::OpenAi => "ChatCompletionsStreamResponse",
                    Format::OpenAiResponses => "ResponsesStreamResponse",
                    Format::Claude => "ClaudeResponse",
                    Format::Gemini => "GeminiChatResponse",
                },
                value,
            )
            .serialize(s),
        }
    }
}
/// A synchronous consumer. Return after serialization or materialization is done.
pub trait EventSink {
    fn emit(&mut self, event: StreamEvent<'_>) -> Result<(), Error>;
}
impl<F> EventSink for F
where
    F: for<'a> FnMut(StreamEvent<'a>) -> Result<(), Error>,
{
    fn emit(&mut self, event: StreamEvent<'_>) -> Result<(), Error> {
        self(event)
    }
}
#[derive(Default)]
pub(crate) struct ValueSink {
    pub values: Vec<Value>,
}
impl EventSink for ValueSink {
    fn emit(&mut self, event: StreamEvent<'_>) -> Result<(), Error> {
        self.values.push(event.to_value()?);
        Ok(())
    }
}

/// Exact Chat chunk wire contract, including required null/default fields.
#[derive(Debug)]
pub struct ChatEvent<'a> {
    pub id: &'a str,
    pub model: &'a str,
    pub created: i64,
    pub choices: &'a [ChatChoice<'a>],
    pub usage: Option<&'a Value>,
}
// Manual wrapper adds the two fixed protocol fields without owning strings.
impl<'a> ChatEvent<'a> {
    pub fn new(
        id: &'a str,
        model: &'a str,
        created: i64,
        choices: &'a [ChatChoice<'a>],
        usage: Option<&'a Value>,
    ) -> Self {
        Self {
            id,
            model,
            created,
            choices,
            usage,
        }
    }
}
impl Serialize for ChatEvent<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(7))?;
        map.serialize_entry("id", self.id)?;
        map.serialize_entry("object", "chat.completion.chunk")?;
        map.serialize_entry("created", &self.created)?;
        map.serialize_entry("model", self.model)?;
        map.serialize_entry("system_fingerprint", &Option::<()>::None)?;
        map.serialize_entry("choices", self.choices)?;
        map.serialize_entry(
            "usage",
            &self.usage.map(|v| crate::wire::normalized_view("Usage", v)),
        )?;
        map.end()
    }
}
#[derive(Debug, Default, Serialize)]
pub struct ChatChoice<'a> {
    pub delta: ChatDelta<'a>,
    pub index: i64,
    pub finish_reason: Option<&'a str>,
    pub logprobs: Option<&'a Value>,
}
#[derive(Debug, Default, Serialize)]
pub struct ChatDelta<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_signature: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_block_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_reasoning_blocks: Option<&'a Value>,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub role: &'a str,
    #[serde(skip_serializing_if = "<[ChatTool<'_>]>::is_empty")]
    pub tool_calls: &'a [ChatTool<'a>],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<&'a Value>,
}
#[derive(Debug, Default, Serialize)]
pub struct ChatTool<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub id: &'a str,
    #[serde(rename = "type")]
    pub kind: Option<&'a str>,
    pub function: ChatFunction<'a>,
}
#[derive(Debug, Default, Serialize)]
pub struct ChatFunction<'a> {
    #[serde(skip_serializing_if = "str::is_empty")]
    pub name: &'a str,
    pub arguments: Cow<'a, str>,
}
