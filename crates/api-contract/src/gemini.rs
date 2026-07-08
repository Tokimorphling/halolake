use serde::{Deserialize, Serialize};

use crate::JsonValue;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GeminiChatRequest {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contents: Vec<GeminiChatContent>,
    #[serde(
        default,
        rename = "safetySettings",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub safety_settings: Vec<GeminiChatSafetySetting>,
    #[serde(
        default,
        rename = "generationConfig",
        skip_serializing_if = "GeminiGenerationConfig::is_empty"
    )]
    pub generation_config: GeminiGenerationConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<GeminiTool>,
    #[serde(
        default,
        rename = "toolConfig",
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_config: Option<JsonValue>,
    #[serde(
        default,
        rename = "systemInstruction",
        skip_serializing_if = "Option::is_none"
    )]
    pub system_instruction: Option<GeminiChatContent>,
    #[serde(
        default,
        rename = "cachedContent",
        skip_serializing_if = "Option::is_none"
    )]
    pub cached_content: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GeminiChatContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default)]
    pub parts: Vec<GeminiPart>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GeminiPart {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,
    #[serde(
        default,
        rename = "inlineData",
        skip_serializing_if = "Option::is_none"
    )]
    pub inline_data: Option<GeminiInlineData>,
    #[serde(default, rename = "fileData", skip_serializing_if = "Option::is_none")]
    pub file_data: Option<GeminiFileData>,
    #[serde(
        default,
        rename = "functionCall",
        skip_serializing_if = "Option::is_none"
    )]
    pub function_call: Option<GeminiFunctionCall>,
    #[serde(
        default,
        rename = "functionResponse",
        skip_serializing_if = "Option::is_none"
    )]
    pub function_response: Option<GeminiFunctionResponse>,
    #[serde(
        default,
        rename = "executableCode",
        skip_serializing_if = "Option::is_none"
    )]
    pub executable_code: Option<GeminiExecutableCode>,
    #[serde(
        default,
        rename = "codeExecutionResult",
        skip_serializing_if = "Option::is_none"
    )]
    pub code_execution_result: Option<GeminiCodeExecutionResult>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, JsonValue>,
}

impl GeminiPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GeminiInlineData {
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GeminiFileData {
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(rename = "fileUri")]
    pub file_uri: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GeminiFunctionCall {
    pub name: String,
    #[serde(default)]
    pub args: JsonValue,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GeminiFunctionResponse {
    pub name: String,
    #[serde(default)]
    pub response: JsonValue,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GeminiExecutableCode {
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub code: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GeminiCodeExecutionResult {
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub output: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GeminiChatSafetySetting {
    pub category: String,
    pub threshold: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GeminiGenerationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, rename = "topK", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i32>,
    #[serde(
        default,
        rename = "maxOutputTokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_output_tokens: Option<u32>,
    #[serde(
        default,
        rename = "candidateCount",
        skip_serializing_if = "Option::is_none"
    )]
    pub candidate_count: Option<u32>,
    #[serde(
        default,
        rename = "stopSequences",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub stop_sequences: Vec<String>,
    #[serde(
        default,
        rename = "responseMimeType",
        skip_serializing_if = "Option::is_none"
    )]
    pub response_mime_type: Option<String>,
    #[serde(
        default,
        rename = "responseSchema",
        skip_serializing_if = "Option::is_none"
    )]
    pub response_schema: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
}

impl GeminiGenerationConfig {
    fn is_empty(&self) -> bool {
        self.temperature.is_none()
            && self.top_p.is_none()
            && self.top_k.is_none()
            && self.max_output_tokens.is_none()
            && self.candidate_count.is_none()
            && self.stop_sequences.is_empty()
            && self.response_mime_type.is_none()
            && self.response_schema.is_none()
            && self.seed.is_none()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GeminiTool {
    #[serde(
        default,
        rename = "functionDeclarations",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub function_declarations: Vec<GeminiFunctionDeclaration>,
    #[serde(
        default,
        rename = "googleSearch",
        skip_serializing_if = "Option::is_none"
    )]
    pub google_search: Option<JsonValue>,
    #[serde(
        default,
        rename = "codeExecution",
        skip_serializing_if = "Option::is_none"
    )]
    pub code_execution: Option<JsonValue>,
    #[serde(
        default,
        rename = "urlContext",
        skip_serializing_if = "Option::is_none"
    )]
    pub url_context: Option<JsonValue>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GeminiFunctionDeclaration {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<JsonValue>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GeminiChatResponse {
    #[serde(default)]
    pub candidates: Vec<GeminiCandidate>,
    #[serde(
        default,
        rename = "promptFeedback",
        skip_serializing_if = "Option::is_none"
    )]
    pub prompt_feedback: Option<JsonValue>,
    #[serde(default, rename = "usageMetadata")]
    pub usage_metadata: GeminiUsageMetadata,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GeminiCandidate {
    #[serde(default)]
    pub content: GeminiChatContent,
    #[serde(
        default,
        rename = "finishReason",
        skip_serializing_if = "Option::is_none"
    )]
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub index: u32,
    #[serde(default, rename = "safetyRatings")]
    pub safety_ratings: Vec<JsonValue>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub struct GeminiUsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    pub prompt_token_count: u32,
    #[serde(default, rename = "candidatesTokenCount")]
    pub candidates_token_count: u32,
    #[serde(default, rename = "totalTokenCount")]
    pub total_token_count: u32,
    #[serde(default, rename = "thoughtsTokenCount")]
    pub thoughts_token_count: u32,
    #[serde(default, rename = "cachedContentTokenCount")]
    pub cached_content_token_count: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GeminiImageRequest {
    pub instances: Vec<GeminiImageInstance>,
    pub parameters: GeminiImageParameters,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GeminiImageInstance {
    pub prompt: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GeminiImageParameters {
    #[serde(
        default,
        rename = "sampleCount",
        skip_serializing_if = "Option::is_none"
    )]
    pub sample_count: Option<u32>,
    #[serde(
        default,
        rename = "aspectRatio",
        skip_serializing_if = "String::is_empty"
    )]
    pub aspect_ratio: String,
    #[serde(
        default,
        rename = "personGeneration",
        skip_serializing_if = "String::is_empty"
    )]
    pub person_generation: String,
    #[serde(
        default,
        rename = "imageSize",
        skip_serializing_if = "String::is_empty"
    )]
    pub image_size: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GeminiImageResponse {
    #[serde(default)]
    pub predictions: Vec<GeminiImagePrediction>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GeminiImagePrediction {
    #[serde(default, rename = "mimeType")]
    pub mime_type: String,
    #[serde(default, rename = "bytesBase64Encoded")]
    pub bytes_base64_encoded: String,
    #[serde(default, rename = "raiFilteredReason")]
    pub rai_filtered_reason: String,
    #[serde(
        default,
        rename = "safetyAttributes",
        skip_serializing_if = "Option::is_none"
    )]
    pub safety_attributes: Option<JsonValue>,
}
