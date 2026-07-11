use crate::JsonValue;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessagesRequest {
    pub model:          String,
    pub max_tokens:     u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system:         Option<SystemContent>,
    pub messages:       Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature:    Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p:          Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k:          Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream:         Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools:          Option<Vec<Tool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice:    Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SystemContent {
    Text(String),
    Blocks(Vec<SystemBlock>),
    Other(JsonValue),
}

impl SystemContent {
    pub fn as_text_lossy(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| block.text.as_deref())
                .collect::<Vec<_>>()
                .join(""),
            Self::Other(value) => value.as_str().unwrap_or_default().to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text:       Option<String>,
    #[serde(flatten)]
    pub extra:      serde_json::Map<String, JsonValue>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub role:    String,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: ImageSource },
    #[serde(rename = "tool_use")]
    ToolUse {
        id:    String,
        name:  String,
        #[serde(default)]
        input: JsonValue,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content:     ToolResultContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error:    Option<bool>,
    },
    #[serde(rename = "thinking")]
    Thinking {
        thinking:  String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type:  Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data:        Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url:         Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
    Json(JsonValue),
}

impl Default for ToolResultContent {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

impl ToolResultContent {
    pub fn as_text_lossy(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Json(value) => value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string()),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tool {
    pub name:         String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description:  Option<String>,
    pub input_schema: JsonValue,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessagesResponse {
    pub id:            String,
    #[serde(rename = "type")]
    pub response_type: String,
    pub role:          String,
    pub model:         String,
    #[serde(default)]
    pub content:       Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason:   Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage:         Option<Usage>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens:                u32,
    #[serde(default)]
    pub output_tokens:               u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens:     u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StreamEvent {
    #[serde(rename = "type")]
    pub event_type:    String,
    #[serde(default)]
    pub message:       Option<MessagesResponse>,
    #[serde(default)]
    pub delta:         Option<JsonValue>,
    #[serde(default)]
    pub content_block: Option<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index:         Option<usize>,
    #[serde(default)]
    pub usage:         Option<Usage>,
}
