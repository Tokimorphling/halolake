use serde::{Deserialize, Serialize};

use crate::JsonValue;

pub const MAX_IMAGE_N: u32 = 128;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub top_k: Option<i32>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub tools: Option<Vec<Tool>>,
    #[serde(default)]
    pub tool_choice: Option<JsonValue>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub stop: Option<JsonValue>,
    #[serde(default)]
    pub n: Option<u32>,
    #[serde(default)]
    pub seed: Option<i64>,
    #[serde(default)]
    pub response_format: Option<JsonValue>,
    #[serde(default)]
    pub stream_options: Option<JsonValue>,
}

impl ChatCompletionRequest {
    pub fn is_stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }

    pub fn max_tokens_value(&self) -> Option<u32> {
        self.max_tokens.or(self.max_completion_tokens)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImageRequest {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub size: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub quality: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub response_format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_fields: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moderation: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_format: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_compression: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_images: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_fidelity: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark_enabled: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<JsonValue>,
}

impl ImageRequest {
    pub fn is_stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }

    pub fn n_or_one(&self) -> u32 {
        self.n.unwrap_or(1)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<MessageContent>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl MessageContent {
    pub fn as_text_lossy(&self) -> String {
        match self {
            MessageContent::Text(text) => text.clone(),
            MessageContent::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text, .. } => Some(text.as_str()),
                    ContentPart::ImageUrl { .. } => None,
                    ContentPart::Other(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ContentPart {
    Text {
        #[serde(rename = "type")]
        part_type: String,
        text: String,
    },
    ImageUrl {
        #[serde(rename = "type")]
        part_type: String,
        image_url: ImageUrl,
    },
    Other(JsonValue),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionTool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FunctionTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<JsonValue>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: ToolCallFunction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatChoice {
    pub index: u32,
    pub message: ChatMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatChunkChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatChunkChoice {
    pub index: u32,
    pub delta: ChatChunkDelta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChatChunkDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub struct TokenUsageDetails {
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub cached_tokens: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub cached_creation_tokens: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub audio_tokens: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub image_tokens: u32,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default, skip_serializing_if = "TokenUsageDetails::is_empty")]
    pub prompt_tokens_details: TokenUsageDetails,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<TokenUsageDetails>,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub cached_tokens: u32,
}

impl TokenUsageDetails {
    pub fn is_empty(&self) -> bool {
        self.cached_tokens == 0
            && self.cached_creation_tokens == 0
            && self.audio_tokens == 0
            && self.image_tokens == 0
    }
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImageResponse {
    pub data: Vec<ImageData>,
    pub created: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonValue>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ImageData {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub b64_json: String,
    #[serde(default)]
    pub revised_prompt: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ErrorBody {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}
