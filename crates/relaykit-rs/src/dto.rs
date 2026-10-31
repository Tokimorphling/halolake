//! Protocol DTOs generated from the pinned Go field definitions.
//! `Option<Vec<_>>` preserves the distinction between nil and empty slices.
pub use crate::dto_values::{BoolValue, IntValue, StringValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdvancedCustomConfig {
    #[serde(rename = "advanced_routes", skip_serializing_if = "option_empty")]
    pub routes: Option<Vec<AdvancedCustomRoute>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdvancedCustomRoute {
    #[serde(
        rename = "incoming_path",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub incoming_path: String,
    #[serde(
        rename = "upstream_path",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub upstream_path: String,
    #[serde(
        rename = "converter",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub converter: String,
    #[serde(rename = "models", skip_serializing_if = "option_empty")]
    pub models: Option<Vec<String>>,
    #[serde(rename = "auth", skip_serializing_if = "Option::is_none")]
    pub auth: Option<AdvancedCustomRouteAuth>,
    #[serde(
        rename = "pass_through_body_enabled",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub pass_through_body_enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdvancedCustomRouteAuth {
    #[serde(
        rename = "type",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub r#type: String,
    #[serde(
        rename = "name",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub name: String,
    #[serde(
        rename = "value",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlphaSearchRequest {
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(
        rename = "id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub id: String,
    #[serde(rename = "stream", skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnthropicModel {
    #[serde(rename = "id", deserialize_with = "null_default")]
    pub id: String,
    #[serde(rename = "created_at", deserialize_with = "null_default")]
    pub created_at: String,
    #[serde(rename = "display_name", deserialize_with = "null_default")]
    pub display_name: String,
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioRequest {
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(rename = "input", deserialize_with = "null_default")]
    pub input: String,
    #[serde(rename = "voice", deserialize_with = "null_default")]
    pub voice: String,
    #[serde(
        rename = "instructions",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub instructions: String,
    #[serde(
        rename = "response_format",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub response_format: String,
    #[serde(rename = "speed", skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    #[serde(
        rename = "stream_format",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub stream_format: String,
    #[serde(
        rename = "metadata",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub metadata: Option<Value>,
    #[serde(
        rename = "task_type",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub task_type: Option<Value>,
    #[serde(
        rename = "language",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub language: Option<Value>,
    #[serde(
        rename = "ref_audio",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub ref_audio: Option<Value>,
    #[serde(
        rename = "ref_text",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub ref_text: Option<Value>,
    #[serde(
        rename = "x_vector_only_mode",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub x_vector_only_mode: Option<Value>,
    #[serde(
        rename = "max_new_tokens",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub max_new_tokens: Option<Value>,
    #[serde(
        rename = "initial_codec_chunk_frames",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub initial_codec_chunk_frames: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioResponse {
    #[serde(rename = "text", deserialize_with = "null_default")]
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BaseRequest {}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BillingUsage {
    #[serde(
        rename = "source",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub source: String,
    #[serde(
        rename = "semantic",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub semantic: String,
    #[serde(
        rename = "estimated",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub estimated: bool,
    #[serde(rename = "openai_usage", skip_serializing_if = "Option::is_none")]
    pub open_ai_usage: Option<Box<Usage>>,
    #[serde(rename = "claude_usage", skip_serializing_if = "Option::is_none")]
    pub claude_usage: Option<ClaudeUsage>,
    #[serde(
        rename = "gemini_usage_metadata",
        skip_serializing_if = "Option::is_none"
    )]
    pub gemini_usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheWriteTokenDetails {
    #[serde(rename = "ttl_30m_tokens", deserialize_with = "null_default")]
    pub ttl30m_tokens: i64,
    #[serde(rename = "ttl_12h_tokens", deserialize_with = "null_default")]
    pub ttl12h_tokens: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CachedTokenDetails {
    #[serde(rename = "text_tokens", skip_serializing_if = "Option::is_none")]
    pub text_tokens: Option<i64>,
    #[serde(rename = "image_tokens", skip_serializing_if = "Option::is_none")]
    pub image_tokens: Option<i64>,
    #[serde(rename = "audio_tokens", skip_serializing_if = "Option::is_none")]
    pub audio_tokens: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelOtherSettings {
    #[serde(
        rename = "azure_responses_version",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub azure_responses_version: String,
    #[serde(rename = "vertex_key_type", skip_serializing_if = "Value::is_null")]
    pub vertex_key_type: Value,
    #[serde(
        rename = "vertex_project_id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub vertex_project_id: String,
    #[serde(
        rename = "openrouter_enterprise",
        skip_serializing_if = "Option::is_none"
    )]
    pub open_router_enterprise: Option<bool>,
    #[serde(
        rename = "claude_beta_query",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub claude_beta_query: bool,
    #[serde(
        rename = "allow_service_tier",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub allow_service_tier: bool,
    #[serde(
        rename = "allow_inference_geo",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub allow_inference_geo: bool,
    #[serde(
        rename = "allow_speed",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub allow_speed: bool,
    #[serde(
        rename = "allow_safety_identifier",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub allow_safety_identifier: bool,
    #[serde(
        rename = "disable_store",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub disable_store: bool,
    #[serde(
        rename = "allow_include_obfuscation",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub allow_include_obfuscation: bool,
    #[serde(
        rename = "disable_task_polling_sleep",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub disable_task_polling_sleep: bool,
    #[serde(rename = "aws_key_type", skip_serializing_if = "Value::is_null")]
    pub aws_key_type: Value,
    #[serde(
        rename = "aws_api_type",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub aws_api_type: String,
    #[serde(
        rename = "upstream_model_update_check_enabled",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub upstream_model_update_check_enabled: bool,
    #[serde(
        rename = "upstream_model_update_auto_sync_enabled",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub upstream_model_update_auto_sync_enabled: bool,
    #[serde(
        rename = "upstream_model_update_last_check_time",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub upstream_model_update_last_check_time: i64,
    #[serde(
        rename = "upstream_model_update_last_detected_models",
        skip_serializing_if = "option_empty"
    )]
    pub upstream_model_update_last_detected_models: Option<Vec<String>>,
    #[serde(
        rename = "upstream_model_update_last_removed_models",
        skip_serializing_if = "option_empty"
    )]
    pub upstream_model_update_last_removed_models: Option<Vec<String>>,
    #[serde(
        rename = "upstream_model_update_ignored_models",
        skip_serializing_if = "option_empty"
    )]
    pub upstream_model_update_ignored_models: Option<Vec<String>>,
    #[serde(rename = "advanced_custom", skip_serializing_if = "Option::is_none")]
    pub advanced_custom: Option<AdvancedCustomConfig>,
    #[serde(
        rename = "ollama_openai_chat",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub ollama_open_ai_chat: bool,
    #[serde(
        rename = "tool_loss_policy",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub tool_loss_policy: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelSettings {
    #[serde(
        rename = "task_plugin_key",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub task_plugin_key: String,
    #[serde(
        rename = "force_format",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub force_format: bool,
    #[serde(
        rename = "thinking_to_content",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub thinking_to_content: bool,
    #[serde(rename = "proxy", deserialize_with = "null_default")]
    pub proxy: String,
    #[serde(
        rename = "pass_through_body_enabled",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub pass_through_body_enabled: bool,
    #[serde(
        rename = "responses_websocket_enabled",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub responses_web_socket_enabled: bool,
    #[serde(
        rename = "system_prompt",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub system_prompt: String,
    #[serde(
        rename = "system_prompt_override",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub system_prompt_override: bool,
    #[serde(
        rename = "task_extend_plugin_keys",
        skip_serializing_if = "option_empty"
    )]
    pub task_extend_plugin_keys: Option<Vec<String>>,
    #[serde(
        rename = "http_protocol",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub http_protocol: String,
    #[serde(
        rename = "http2_connection_shards",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub http2_connection_shards: i64,
    #[serde(
        rename = "tls_insecure_skip_verify",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub tls_insecure_skip_verify: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatCompletionsStreamResponse {
    #[serde(rename = "id", deserialize_with = "null_default")]
    pub id: String,
    #[serde(rename = "object", deserialize_with = "null_default")]
    pub object: String,
    #[serde(rename = "created", deserialize_with = "null_default")]
    pub created: i64,
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(rename = "system_fingerprint")]
    pub system_fingerprint: Option<String>,
    #[serde(rename = "choices")]
    pub choices: Option<Vec<ChatCompletionsStreamResponseChoice>>,
    #[serde(rename = "usage")]
    pub usage: Option<Box<Usage>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatCompletionsStreamResponseChoice {
    #[serde(rename = "delta", deserialize_with = "null_default")]
    pub delta: ChatCompletionsStreamResponseChoiceDelta,
    #[serde(rename = "logprobs")]
    pub logprobs: Option<Value>,
    #[serde(rename = "finish_reason")]
    pub finish_reason: Option<String>,
    #[serde(rename = "index", deserialize_with = "null_default")]
    pub index: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatCompletionsStreamResponseChoiceDelta {
    #[serde(rename = "content", skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(rename = "reasoning_content", skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(rename = "reasoning", skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(
        rename = "role",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub role: String,
    #[serde(rename = "tool_calls", skip_serializing_if = "option_empty")]
    pub tool_calls: Option<Vec<ToolCallResponse>>,
    #[serde(
        rename = "annotations",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub annotations: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatCompletionsStreamResponseSimple {
    #[serde(rename = "choices")]
    pub choices: Option<Vec<ChatCompletionsStreamResponseChoice>>,
    #[serde(rename = "usage")]
    pub usage: Option<Box<Usage>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeCacheCreationUsage {
    #[serde(
        rename = "ephemeral_5m_input_tokens",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub ephemeral5m_input_tokens: i64,
    #[serde(
        rename = "ephemeral_1h_input_tokens",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub ephemeral1h_input_tokens: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeErrorWithStatusCode {
    #[serde(rename = "error")]
    pub error: Value,
    #[serde(rename = "status_code", deserialize_with = "null_default")]
    pub status_code: i64,
    #[serde(rename = "LocalError", deserialize_with = "null_default")]
    pub local_error: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeMediaMessage {
    #[serde(
        rename = "type",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub r#type: String,
    #[serde(rename = "text", skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(
        rename = "model",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub model: String,
    #[serde(rename = "source", skip_serializing_if = "Option::is_none")]
    pub source: Option<ClaudeMessageSource>,
    #[serde(rename = "usage", skip_serializing_if = "Option::is_none")]
    pub usage: Option<ClaudeUsage>,
    #[serde(rename = "stop_reason", skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(rename = "partial_json", skip_serializing_if = "Option::is_none")]
    pub partial_json: Option<String>,
    #[serde(
        rename = "role",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub role: String,
    #[serde(rename = "thinking", skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(
        rename = "data",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub data: String,
    #[serde(
        rename = "signature",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub signature: String,
    #[serde(
        rename = "delta",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub delta: String,
    #[serde(
        rename = "cache_control",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub cache_control: Option<Value>,
    #[serde(
        rename = "citations",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub citations: Option<Value>,
    #[serde(
        rename = "citation",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub citation: Option<Value>,
    #[serde(
        rename = "caller",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub caller: Option<Value>,
    #[serde(
        rename = "server_name",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub server_name: String,
    #[serde(rename = "is_error", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(
        rename = "error_code",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub error_code: String,
    #[serde(
        rename = "id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub id: String,
    #[serde(
        rename = "name",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub name: String,
    #[serde(rename = "input", skip_serializing_if = "Value::is_null")]
    pub input: Value,
    #[serde(rename = "content", skip_serializing_if = "Value::is_null")]
    pub content: Value,
    #[serde(
        rename = "tool_use_id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub tool_use_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeMessage {
    #[serde(rename = "role", deserialize_with = "null_default")]
    pub role: String,
    #[serde(rename = "content")]
    pub content: Value,
    #[serde(
        rename = "output_config",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub output_config: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeMessageSource {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(
        rename = "media_type",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub media_type: String,
    #[serde(rename = "data", skip_serializing_if = "Value::is_null")]
    pub data: Value,
    #[serde(
        rename = "url",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeMetadata {
    #[serde(rename = "user_id", deserialize_with = "null_default")]
    pub user_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeRequest {
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(
        rename = "prompt",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub prompt: String,
    #[serde(rename = "system", skip_serializing_if = "Value::is_null")]
    pub system: Value,
    #[serde(rename = "messages", skip_serializing_if = "option_empty")]
    pub messages: Option<Vec<ClaudeMessage>>,
    #[serde(
        rename = "cache_control",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub cache_control: Option<Value>,
    #[serde(
        rename = "inference_geo",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub inference_geo: String,
    #[serde(rename = "max_tokens", skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(
        rename = "max_tokens_to_sample",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_tokens_to_sample: Option<u64>,
    #[serde(rename = "stop_sequences", skip_serializing_if = "option_empty")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(rename = "temperature", skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(rename = "top_p", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(rename = "top_k", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i64>,
    #[serde(rename = "stream", skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(rename = "tools", skip_serializing_if = "Value::is_null")]
    pub tools: Value,
    #[serde(
        rename = "safeguards",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub safeguards: Option<Value>,
    #[serde(
        rename = "context_management",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub context_management: Option<Value>,
    #[serde(
        rename = "output_config",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub output_config: Option<Value>,
    #[serde(
        rename = "output_format",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub output_format: Option<Value>,
    #[serde(
        rename = "container",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub container: Option<Value>,
    #[serde(rename = "tool_choice", skip_serializing_if = "Value::is_null")]
    pub tool_choice: Value,
    #[serde(rename = "thinking", skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    #[serde(
        rename = "mcp_servers",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub mcp_servers: Option<Value>,
    #[serde(
        rename = "metadata",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub metadata: Option<Value>,
    #[serde(
        rename = "chat_template_kwargs",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub chat_template_kwargs: Option<Value>,
    #[serde(
        rename = "speed",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub speed: Option<Value>,
    #[serde(
        rename = "service_tier",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub service_tier: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeResponse {
    #[serde(
        rename = "id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub id: String,
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(
        rename = "role",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub role: String,
    #[serde(rename = "content", skip_serializing_if = "option_empty")]
    pub content: Option<Vec<ClaudeMediaMessage>>,
    #[serde(
        rename = "completion",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub completion: String,
    #[serde(
        rename = "stop_reason",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub stop_reason: String,
    #[serde(
        rename = "model",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub model: String,
    #[serde(rename = "error", skip_serializing_if = "Value::is_null")]
    pub error: Value,
    #[serde(rename = "usage", skip_serializing_if = "Option::is_none")]
    pub usage: Option<ClaudeUsage>,
    #[serde(rename = "index", skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    #[serde(rename = "content_block", skip_serializing_if = "Option::is_none")]
    pub content_block: Option<ClaudeMediaMessage>,
    #[serde(rename = "delta", skip_serializing_if = "Option::is_none")]
    pub delta: Option<ClaudeMediaMessage>,
    #[serde(rename = "message", skip_serializing_if = "Option::is_none")]
    pub message: Option<ClaudeMediaMessage>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeServerToolUse {
    #[serde(
        rename = "web_search_requests",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub web_search_requests: i64,
    #[serde(
        rename = "web_fetch_requests",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub web_fetch_requests: i64,
    #[serde(
        rename = "code_execution_requests",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub code_execution_requests: i64,
    #[serde(
        rename = "tool_search_requests",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub tool_search_requests: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeToolChoice {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(
        rename = "name",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub name: String,
    #[serde(
        rename = "disable_parallel_tool_use",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub disable_parallel_tool_use: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeUsage {
    #[serde(rename = "input_tokens", deserialize_with = "null_default")]
    pub input_tokens: i64,
    #[serde(
        rename = "cache_creation_input_tokens",
        deserialize_with = "null_default"
    )]
    pub cache_creation_input_tokens: i64,
    #[serde(rename = "cache_read_input_tokens", deserialize_with = "null_default")]
    pub cache_read_input_tokens: i64,
    #[serde(rename = "output_tokens", deserialize_with = "null_default")]
    pub output_tokens: i64,
    #[serde(rename = "cache_creation", skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<ClaudeCacheCreationUsage>,
    #[serde(
        rename = "claude_cache_creation_5_m_tokens",
        deserialize_with = "null_default"
    )]
    pub claude_cache_creation5m_tokens: i64,
    #[serde(
        rename = "claude_cache_creation_1_h_tokens",
        deserialize_with = "null_default"
    )]
    pub claude_cache_creation1h_tokens: i64,
    #[serde(rename = "server_tool_use", skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ClaudeServerToolUse>,
    #[serde(rename = "billing_usage", skip_serializing_if = "Option::is_none")]
    pub billing_usage: Option<Box<BillingUsage>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeWebSearchTool {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(rename = "name", deserialize_with = "null_default")]
    pub name: String,
    #[serde(
        rename = "max_uses",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub max_uses: i64,
    #[serde(rename = "allowed_domains", skip_serializing_if = "option_empty")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(rename = "blocked_domains", skip_serializing_if = "option_empty")]
    pub blocked_domains: Option<Vec<String>>,
    #[serde(rename = "allowed_callers", skip_serializing_if = "option_empty")]
    pub allowed_callers: Option<Vec<String>>,
    #[serde(
        rename = "response_inclusion",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub response_inclusion: String,
    #[serde(rename = "user_location", skip_serializing_if = "Option::is_none")]
    pub user_location: Option<ClaudeWebSearchUserLocation>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeWebSearchUserLocation {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(
        rename = "timezone",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub timezone: String,
    #[serde(
        rename = "country",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub country: String,
    #[serde(
        rename = "region",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub region: String,
    #[serde(
        rename = "city",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub city: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompletionsStreamResponse {
    #[serde(rename = "choices")]
    pub choices: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContentEmbedding {
    #[serde(rename = "values")]
    pub values: Option<Vec<f64>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeepSeekToolsRequest {
    #[serde(
        rename = "model",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DifferenceItem {
    #[serde(rename = "current")]
    pub current: Value,
    #[serde(rename = "upstreams")]
    pub upstreams: Option<std::collections::BTreeMap<String, Value>>,
    #[serde(rename = "confidence")]
    pub confidence: Option<std::collections::BTreeMap<String, bool>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingOptions {
    #[serde(
        rename = "seed",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub seed: i64,
    #[serde(rename = "temperature", skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(
        rename = "top_k",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub top_k: i64,
    #[serde(rename = "top_p", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(rename = "frequency_penalty", skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(rename = "presence_penalty", skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(
        rename = "num_predict",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub num_predict: i64,
    #[serde(
        rename = "num_ctx",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub num_ctx: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingRequest {
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(rename = "input")]
    pub input: Value,
    #[serde(
        rename = "encoding_format",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub encoding_format: String,
    #[serde(rename = "dimensions", skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<i64>,
    #[serde(
        rename = "user",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub user: String,
    #[serde(rename = "seed", skip_serializing_if = "Option::is_none")]
    pub seed: Option<f64>,
    #[serde(rename = "temperature", skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(rename = "top_p", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(rename = "frequency_penalty", skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(rename = "presence_penalty", skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingResponse {
    #[serde(rename = "object", deserialize_with = "null_default")]
    pub object: String,
    #[serde(rename = "data")]
    pub data: Option<Vec<EmbeddingResponseItem>>,
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(rename = "usage", deserialize_with = "null_default")]
    pub usage: Usage,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingResponseItem {
    #[serde(rename = "object", deserialize_with = "null_default")]
    pub object: String,
    #[serde(rename = "index", deserialize_with = "null_default")]
    pub index: i64,
    #[serde(rename = "embedding")]
    pub embedding: Option<Vec<f64>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlexibleEmbeddingResponse {
    #[serde(rename = "object", deserialize_with = "null_default")]
    pub object: String,
    #[serde(rename = "data")]
    pub data: Option<Vec<FlexibleEmbeddingResponseItem>>,
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(rename = "usage", deserialize_with = "null_default")]
    pub usage: Usage,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlexibleEmbeddingResponseItem {
    #[serde(rename = "object", deserialize_with = "null_default")]
    pub object: String,
    #[serde(rename = "index", deserialize_with = "null_default")]
    pub index: i64,
    #[serde(rename = "embedding")]
    pub embedding: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormatJsonSchema {
    #[serde(
        rename = "description",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub description: String,
    #[serde(rename = "name", deserialize_with = "null_default")]
    pub name: String,
    #[serde(rename = "schema", skip_serializing_if = "Value::is_null")]
    pub schema: Value,
    #[serde(
        rename = "strict",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub strict: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FunctionCall {
    #[serde(
        rename = "id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub id: String,
    #[serde(rename = "name", deserialize_with = "null_default")]
    pub function_name: String,
    #[serde(rename = "args")]
    pub arguments: Value,
    #[serde(rename = "partialArgs", skip_serializing_if = "option_empty")]
    pub partial_args: Option<Vec<GeminiPartialArg>>,
    #[serde(rename = "willContinue", skip_serializing_if = "Option::is_none")]
    pub will_continue: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FunctionCallingConfig {
    #[serde(
        rename = "mode",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub mode: String,
    #[serde(rename = "allowedFunctionNames", skip_serializing_if = "option_empty")]
    pub allowed_function_names: Option<Vec<String>>,
    #[serde(
        rename = "streamFunctionCallArguments",
        skip_serializing_if = "Option::is_none"
    )]
    pub stream_function_call_arguments: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FunctionRequest {
    #[serde(
        rename = "description",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub description: String,
    #[serde(rename = "name", deserialize_with = "null_default")]
    pub name: String,
    #[serde(
        rename = "parameters",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "nullable_raw_message"
    )]
    pub parameters: Option<Value>,
    #[serde(
        rename = "arguments",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub arguments: String,
    #[serde(rename = "strict", skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FunctionResponse {
    #[serde(
        rename = "description",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub description: String,
    #[serde(
        rename = "name",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub name: String,
    #[serde(rename = "parameters", skip_serializing_if = "Value::is_null")]
    pub parameters: Value,
    #[serde(rename = "arguments", deserialize_with = "null_default")]
    pub arguments: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiBatchEmbeddingRequest {
    #[serde(rename = "requests")]
    pub requests: Option<Vec<Option<GeminiEmbeddingRequest>>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiBatchEmbeddingResponse {
    #[serde(rename = "embeddings")]
    pub embeddings: Option<Vec<Option<ContentEmbedding>>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiChatCandidate {
    #[serde(rename = "content", deserialize_with = "null_default")]
    pub content: GeminiChatContent,
    #[serde(rename = "finishReason")]
    pub finish_reason: Option<String>,
    #[serde(rename = "index", deserialize_with = "null_default")]
    pub index: i64,
    #[serde(rename = "safetyRatings")]
    pub safety_ratings: Option<Vec<GeminiChatSafetyRating>>,
    #[serde(rename = "groundingMetadata", skip_serializing_if = "Option::is_none")]
    pub grounding_metadata: Option<GeminiGroundingMetadata>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiChatContent {
    #[serde(
        rename = "role",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub role: String,
    #[serde(rename = "parts")]
    pub parts: Option<Vec<GeminiPart>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(default)]
pub struct GeminiChatGenerationConfig {
    #[serde(rename = "temperature", skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(rename = "topK", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<f64>,
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(rename = "candidateCount", skip_serializing_if = "Option::is_none")]
    pub candidate_count: Option<i64>,
    #[serde(rename = "stopSequences", skip_serializing_if = "option_empty")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(
        rename = "responseMimeType",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub response_mime_type: String,
    #[serde(rename = "responseSchema", skip_serializing_if = "Value::is_null")]
    pub response_schema: Value,
    #[serde(
        rename = "responseJsonSchema",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub response_json_schema: Option<Value>,
    #[serde(rename = "presencePenalty", skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(rename = "frequencyPenalty", skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(rename = "responseLogprobs", skip_serializing_if = "Option::is_none")]
    pub response_logprobs: Option<bool>,
    #[serde(rename = "logprobs", skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<i32>,
    #[serde(
        rename = "enableEnhancedCivicAnswers",
        skip_serializing_if = "Option::is_none"
    )]
    pub enable_enhanced_civic_answers: Option<bool>,
    #[serde(
        rename = "mediaResolution",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub media_resolution: String,
    #[serde(rename = "seed", skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(rename = "responseModalities", skip_serializing_if = "option_empty")]
    pub response_modalities: Option<Vec<String>>,
    #[serde(rename = "thinkingConfig", skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<GeminiThinkingConfig>,
    #[serde(
        rename = "speechConfig",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub speech_config: Option<Value>,
    #[serde(
        rename = "imageConfig",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub image_config: Option<Value>,
}

impl<'de> Deserialize<'de> for GeminiChatGenerationConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let mut object = crate::wire::normalize_named("GeminiChatGenerationConfig", &value)
            .map_err(serde::de::Error::custom)?
            .as_object()
            .cloned()
            .unwrap_or_default();
        Ok(Self {
            temperature: object
                .remove("temperature")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            top_p: object
                .remove("topP")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            top_k: object
                .remove("topK")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            max_output_tokens: object
                .remove("maxOutputTokens")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            candidate_count: object
                .remove("candidateCount")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            stop_sequences: object
                .remove("stopSequences")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            response_mime_type: object
                .remove("responseMimeType")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            response_schema: object
                .remove("responseSchema")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            response_json_schema: object
                .remove("responseJsonSchema")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            presence_penalty: object
                .remove("presencePenalty")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            frequency_penalty: object
                .remove("frequencyPenalty")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            response_logprobs: object
                .remove("responseLogprobs")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            logprobs: object
                .remove("logprobs")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            enable_enhanced_civic_answers: object
                .remove("enableEnhancedCivicAnswers")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            media_resolution: object
                .remove("mediaResolution")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            seed: object
                .remove("seed")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            response_modalities: object
                .remove("responseModalities")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            thinking_config: object
                .remove("thinkingConfig")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            speech_config: object
                .remove("speechConfig")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            image_config: object
                .remove("imageConfig")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
        })
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiChatPromptFeedback {
    #[serde(rename = "safetyRatings")]
    pub safety_ratings: Option<Vec<GeminiChatSafetyRating>>,
    #[serde(rename = "blockReason", skip_serializing_if = "Option::is_none")]
    pub block_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(default)]
pub struct GeminiChatRequest {
    #[serde(rename = "requests", skip_serializing_if = "option_empty")]
    pub requests: Option<Vec<GeminiChatRequest>>,
    #[serde(rename = "contents")]
    pub contents: Option<Vec<GeminiChatContent>>,
    #[serde(rename = "safetySettings", skip_serializing_if = "option_empty")]
    pub safety_settings: Option<Vec<GeminiChatSafetySettings>>,
    #[serde(rename = "generationConfig", deserialize_with = "null_default")]
    pub generation_config: GeminiChatGenerationConfig,
    #[serde(
        rename = "tools",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub tools: Option<Value>,
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<ToolConfig>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    pub system_instructions: Option<GeminiChatContent>,
    #[serde(
        rename = "cachedContent",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub cached_content: String,
}

impl<'de> Deserialize<'de> for GeminiChatRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let mut object = crate::wire::normalize_named("GeminiChatRequest", &value)
            .map_err(serde::de::Error::custom)?
            .as_object()
            .cloned()
            .unwrap_or_default();
        Ok(Self {
            requests: object
                .remove("requests")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            contents: object
                .remove("contents")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            safety_settings: object
                .remove("safetySettings")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            generation_config: object
                .remove("generationConfig")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            tools: object
                .remove("tools")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            tool_config: object
                .remove("toolConfig")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            system_instructions: object
                .remove("systemInstruction")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            cached_content: object
                .remove("cachedContent")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
        })
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiChatResponse {
    #[serde(rename = "candidates")]
    pub candidates: Option<Vec<GeminiChatCandidate>>,
    #[serde(rename = "promptFeedback", skip_serializing_if = "Option::is_none")]
    pub prompt_feedback: Option<GeminiChatPromptFeedback>,
    #[serde(rename = "usageMetadata", deserialize_with = "null_default")]
    pub usage_metadata: GeminiUsageMetadata,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiChatSafetyRating {
    #[serde(rename = "category", deserialize_with = "null_default")]
    pub category: String,
    #[serde(rename = "probability", deserialize_with = "null_default")]
    pub probability: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiChatSafetySettings {
    #[serde(rename = "category", deserialize_with = "null_default")]
    pub category: String,
    #[serde(rename = "threshold", deserialize_with = "null_default")]
    pub threshold: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiChatTool {
    #[serde(rename = "googleSearch", skip_serializing_if = "Value::is_null")]
    pub google_search: Value,
    #[serde(
        rename = "googleSearchRetrieval",
        skip_serializing_if = "Value::is_null"
    )]
    pub google_search_retrieval: Value,
    #[serde(
        rename = "googleMaps",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub google_maps: Option<Value>,
    #[serde(
        rename = "enterpriseWebSearch",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub enterprise_web_search: Option<Value>,
    #[serde(rename = "codeExecution", skip_serializing_if = "Value::is_null")]
    pub code_execution: Value,
    #[serde(
        rename = "functionDeclarations",
        skip_serializing_if = "Value::is_null"
    )]
    pub function_declarations: Value,
    #[serde(rename = "urlContext", skip_serializing_if = "Value::is_null")]
    pub url_context: Value,
    #[serde(
        rename = "fileSearch",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub file_search: Option<Value>,
    #[serde(
        rename = "computerUse",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub computer_use: Option<Value>,
    #[serde(
        rename = "retrieval",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub retrieval: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiEmbeddingRequest {
    #[serde(
        rename = "model",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub model: String,
    #[serde(rename = "content", deserialize_with = "null_default")]
    pub content: GeminiChatContent,
    #[serde(
        rename = "taskType",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub task_type: String,
    #[serde(
        rename = "title",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub title: String,
    #[serde(
        rename = "outputDimensionality",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub output_dimensionality: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiEmbeddingResponse {
    #[serde(rename = "embedding", deserialize_with = "null_default")]
    pub embedding: ContentEmbedding,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiFileData {
    #[serde(
        rename = "mimeType",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub mime_type: String,
    #[serde(
        rename = "fileUri",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub file_uri: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiFunctionResponse {
    #[serde(rename = "name", deserialize_with = "null_default")]
    pub name: String,
    #[serde(rename = "response")]
    pub response: Option<std::collections::BTreeMap<String, Value>>,
    #[serde(
        rename = "willContinue",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub will_continue: Option<Value>,
    #[serde(
        rename = "scheduling",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub scheduling: Option<Value>,
    #[serde(
        rename = "parts",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub parts: Option<Value>,
    #[serde(
        rename = "id",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub id: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiGroundingMetadata {
    #[serde(rename = "webSearchQueries", skip_serializing_if = "option_empty")]
    pub web_search_queries: Option<Vec<String>>,
    #[serde(rename = "imageSearchQueries", skip_serializing_if = "option_empty")]
    pub image_search_queries: Option<Vec<String>>,
    #[serde(rename = "retrievalQueries", skip_serializing_if = "option_empty")]
    pub retrieval_queries: Option<Vec<String>>,
    #[serde(
        rename = "groundingChunks",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub grounding_chunks: Option<Value>,
    #[serde(
        rename = "groundingSupports",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub grounding_supports: Option<Value>,
    #[serde(
        rename = "searchEntryPoint",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub search_entry_point: Option<Value>,
    #[serde(
        rename = "retrievalMetadata",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub retrieval_metadata: Option<Value>,
    #[serde(
        rename = "sourceFlaggingUris",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub source_flagging_uris: Option<Value>,
    #[serde(
        rename = "googleMapsWidgetContextToken",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub google_maps_widget_context_token: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiImageInstance {
    #[serde(rename = "prompt", deserialize_with = "null_default")]
    pub prompt: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiImageParameters {
    #[serde(
        rename = "sampleCount",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub sample_count: i64,
    #[serde(
        rename = "aspectRatio",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub aspect_ratio: String,
    #[serde(
        rename = "personGeneration",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub person_generation: String,
    #[serde(
        rename = "imageSize",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub image_size: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiImagePrediction {
    #[serde(rename = "mimeType", deserialize_with = "null_default")]
    pub mime_type: String,
    #[serde(rename = "bytesBase64Encoded", deserialize_with = "null_default")]
    pub bytes_base64_encoded: String,
    #[serde(
        rename = "raiFilteredReason",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub rai_filtered_reason: String,
    #[serde(rename = "safetyAttributes", skip_serializing_if = "Value::is_null")]
    pub safety_attributes: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiImageRequest {
    #[serde(rename = "instances")]
    pub instances: Option<Vec<GeminiImageInstance>>,
    #[serde(rename = "parameters", deserialize_with = "null_default")]
    pub parameters: GeminiImageParameters,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiImageResponse {
    #[serde(rename = "predictions")]
    pub predictions: Option<Vec<GeminiImagePrediction>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(default)]
pub struct GeminiInlineData {
    #[serde(rename = "mimeType", deserialize_with = "null_default")]
    pub mime_type: String,
    #[serde(rename = "data", deserialize_with = "null_default")]
    pub data: String,
}

impl<'de> Deserialize<'de> for GeminiInlineData {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let mut object = crate::wire::normalize_named("GeminiInlineData", &value)
            .map_err(serde::de::Error::custom)?
            .as_object()
            .cloned()
            .unwrap_or_default();
        Ok(Self {
            mime_type: object
                .remove("mimeType")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            data: object
                .remove("data")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
        })
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiModel {
    #[serde(rename = "name")]
    pub name: Value,
    #[serde(rename = "baseModelId")]
    pub base_model_id: Value,
    #[serde(rename = "version")]
    pub version: Value,
    #[serde(rename = "displayName")]
    pub display_name: Value,
    #[serde(rename = "description")]
    pub description: Value,
    #[serde(rename = "inputTokenLimit")]
    pub input_token_limit: Value,
    #[serde(rename = "outputTokenLimit")]
    pub output_token_limit: Value,
    #[serde(rename = "supportedGenerationMethods")]
    pub supported_generation_methods: Option<Vec<Value>>,
    #[serde(rename = "thinking")]
    pub thinking: Value,
    #[serde(rename = "temperature")]
    pub temperature: Value,
    #[serde(rename = "maxTemperature")]
    pub max_temperature: Value,
    #[serde(rename = "topP")]
    pub top_p: Value,
    #[serde(rename = "topK")]
    pub top_k: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(default)]
pub struct GeminiPart {
    #[serde(
        rename = "text",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub text: String,
    #[serde(
        rename = "thought",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub thought: bool,
    #[serde(rename = "inlineData", skip_serializing_if = "Option::is_none")]
    pub inline_data: Option<GeminiInlineData>,
    #[serde(rename = "functionCall", skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCall>,
    #[serde(
        rename = "thoughtSignature",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub thought_signature: Option<Value>,
    #[serde(rename = "functionResponse", skip_serializing_if = "Option::is_none")]
    pub function_response: Option<GeminiFunctionResponse>,
    #[serde(
        rename = "mediaResolution",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub media_resolution: Option<Value>,
    #[serde(
        rename = "videoMetadata",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub video_metadata: Option<Value>,
    #[serde(rename = "fileData", skip_serializing_if = "Option::is_none")]
    pub file_data: Option<GeminiFileData>,
    #[serde(rename = "executableCode", skip_serializing_if = "Option::is_none")]
    pub executable_code: Option<GeminiPartExecutableCode>,
    #[serde(
        rename = "codeExecutionResult",
        skip_serializing_if = "Option::is_none"
    )]
    pub code_execution_result: Option<GeminiPartCodeExecutionResult>,
}

impl<'de> Deserialize<'de> for GeminiPart {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let mut object = crate::wire::normalize_named("GeminiPart", &value)
            .map_err(serde::de::Error::custom)?
            .as_object()
            .cloned()
            .unwrap_or_default();
        Ok(Self {
            text: object
                .remove("text")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            thought: object
                .remove("thought")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            inline_data: object
                .remove("inlineData")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            function_call: object
                .remove("functionCall")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            thought_signature: object
                .remove("thoughtSignature")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            function_response: object
                .remove("functionResponse")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            media_resolution: object
                .remove("mediaResolution")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            video_metadata: object
                .remove("videoMetadata")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            file_data: object
                .remove("fileData")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            executable_code: object
                .remove("executableCode")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            code_execution_result: object
                .remove("codeExecutionResult")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
        })
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiPartCodeExecutionResult {
    #[serde(
        rename = "outcome",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub outcome: String,
    #[serde(
        rename = "output",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub output: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiPartExecutableCode {
    #[serde(
        rename = "language",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub language: String,
    #[serde(
        rename = "code",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub code: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiPartialArg {
    #[serde(rename = "jsonPath", deserialize_with = "null_default")]
    pub json_path: String,
    #[serde(rename = "numberValue", skip_serializing_if = "Option::is_none")]
    pub number_value: Option<f64>,
    #[serde(rename = "stringValue", skip_serializing_if = "Option::is_none")]
    pub string_value: Option<String>,
    #[serde(rename = "boolValue", skip_serializing_if = "Option::is_none")]
    pub bool_value: Option<bool>,
    #[serde(
        rename = "nullValue",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub null_value: Option<Value>,
    #[serde(rename = "willContinue", skip_serializing_if = "Option::is_none")]
    pub will_continue: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiPromptTokensDetails {
    #[serde(rename = "modality", deserialize_with = "null_default")]
    pub modality: String,
    #[serde(rename = "tokenCount", deserialize_with = "null_default")]
    pub token_count: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(default)]
pub struct GeminiThinkingConfig {
    #[serde(rename = "includeThoughts", skip_serializing_if = "Option::is_none")]
    pub include_thoughts: Option<bool>,
    #[serde(rename = "thinkingBudget", skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<i64>,
    #[serde(
        rename = "thinkingLevel",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub thinking_level: String,
}

impl<'de> Deserialize<'de> for GeminiThinkingConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let mut object = crate::wire::normalize_named("GeminiThinkingConfig", &value)
            .map_err(serde::de::Error::custom)?
            .as_object()
            .cloned()
            .unwrap_or_default();
        Ok(Self {
            include_thoughts: object
                .remove("includeThoughts")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            thinking_budget: object
                .remove("thinkingBudget")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            thinking_level: object
                .remove("thinkingLevel")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
        })
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiUsageMetadata {
    #[serde(rename = "promptTokenCount", deserialize_with = "null_default")]
    pub prompt_token_count: i64,
    #[serde(rename = "toolUsePromptTokenCount", deserialize_with = "null_default")]
    pub tool_use_prompt_token_count: i64,
    #[serde(rename = "candidatesTokenCount", deserialize_with = "null_default")]
    pub candidates_token_count: i64,
    #[serde(rename = "totalTokenCount", deserialize_with = "null_default")]
    pub total_token_count: i64,
    #[serde(rename = "thoughtsTokenCount", deserialize_with = "null_default")]
    pub thoughts_token_count: i64,
    #[serde(rename = "cachedContentTokenCount", deserialize_with = "null_default")]
    pub cached_content_token_count: i64,
    #[serde(rename = "promptTokensDetails")]
    pub prompt_tokens_details: Option<Vec<GeminiPromptTokensDetails>>,
    #[serde(rename = "toolUsePromptTokensDetails")]
    pub tool_use_prompt_tokens_details: Option<Vec<GeminiPromptTokensDetails>>,
    #[serde(rename = "candidatesTokensDetails")]
    pub candidates_tokens_details: Option<Vec<GeminiPromptTokensDetails>>,
    #[serde(rename = "billing_usage", skip_serializing_if = "Option::is_none")]
    pub billing_usage: Option<Box<BillingUsage>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralErrorResponse {
    #[serde(rename = "error", deserialize_with = "raw_message")]
    pub error: Option<Value>,
    #[serde(rename = "message", deserialize_with = "null_default")]
    pub message: String,
    #[serde(rename = "msg", deserialize_with = "null_default")]
    pub msg: String,
    #[serde(rename = "err", deserialize_with = "null_default")]
    pub err: String,
    #[serde(rename = "error_msg", deserialize_with = "null_default")]
    pub error_msg: String,
    #[serde(
        rename = "metadata",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub metadata: Option<Value>,
    #[serde(
        rename = "detail",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub detail: String,
    #[serde(rename = "header")]
    pub header: Value,
    #[serde(rename = "response")]
    pub response: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct GeneralOpenAIRequest {
    #[serde(
        rename = "model",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub model: String,
    #[serde(rename = "messages", skip_serializing_if = "option_empty")]
    pub messages: Option<Vec<Message>>,
    #[serde(rename = "prompt", skip_serializing_if = "Value::is_null")]
    pub prompt: Value,
    #[serde(rename = "prefix", skip_serializing_if = "Value::is_null")]
    pub prefix: Value,
    #[serde(rename = "suffix", skip_serializing_if = "Value::is_null")]
    pub suffix: Value,
    #[serde(rename = "stream", skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(rename = "stream_options", skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(rename = "max_tokens", skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(
        rename = "max_completion_tokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_completion_tokens: Option<u64>,
    #[serde(
        rename = "reasoning_effort",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub reasoning_effort: String,
    #[serde(
        rename = "verbosity",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub verbosity: Option<Value>,
    #[serde(rename = "temperature", skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(rename = "top_p", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(rename = "top_k", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i64>,
    #[serde(rename = "stop", skip_serializing_if = "Value::is_null")]
    pub stop: Value,
    #[serde(rename = "n", skip_serializing_if = "Option::is_none")]
    pub n: Option<i64>,
    #[serde(rename = "input", skip_serializing_if = "Value::is_null")]
    pub input: Value,
    #[serde(
        rename = "instruction",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub instruction: String,
    #[serde(
        rename = "size",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub size: String,
    #[serde(
        rename = "functions",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub functions: Option<Value>,
    #[serde(rename = "frequency_penalty", skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(rename = "presence_penalty", skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(rename = "response_format", skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(
        rename = "encoding_format",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub encoding_format: Option<Value>,
    #[serde(rename = "seed", skip_serializing_if = "Option::is_none")]
    pub seed: Option<f64>,
    #[serde(
        rename = "parallel_tool_calls",
        skip_serializing_if = "Option::is_none"
    )]
    pub parallel_too_calls: Option<bool>,
    #[serde(rename = "tools", skip_serializing_if = "option_empty")]
    pub tools: Option<Vec<ToolCallRequest>>,
    #[serde(rename = "tool_choice", skip_serializing_if = "Value::is_null")]
    pub tool_choice: Value,
    #[serde(
        rename = "function_call",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub function_call: Option<Value>,
    #[serde(
        rename = "user",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub user: Option<Value>,
    #[serde(
        rename = "service_tier",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub service_tier: Option<Value>,
    #[serde(rename = "logprobs", skip_serializing_if = "Option::is_none")]
    pub log_probs: Option<bool>,
    #[serde(rename = "top_logprobs", skip_serializing_if = "Option::is_none")]
    pub top_log_probs: Option<i64>,
    #[serde(rename = "dimensions", skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<i64>,
    #[serde(
        rename = "modalities",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub modalities: Option<Value>,
    #[serde(
        rename = "audio",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub audio: Option<Value>,
    #[serde(
        rename = "safety_identifier",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub safety_identifier: Option<Value>,
    #[serde(
        rename = "store",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub store: Option<Value>,
    #[serde(
        rename = "prompt_cache_key",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub prompt_cache_key: String,
    #[serde(
        rename = "prompt_cache_options",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub prompt_cache_options: Option<Value>,
    #[serde(
        rename = "prompt_cache_retention",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub prompt_cache_retention: Option<Value>,
    #[serde(
        rename = "logit_bias",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub logit_bias: Option<Value>,
    #[serde(
        rename = "metadata",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub metadata: Option<Value>,
    #[serde(
        rename = "prediction",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub prediction: Option<Value>,
    #[serde(
        rename = "extra_body",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub extra_body: Option<Value>,
    #[serde(
        rename = "imageConfig",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub image_config: Option<Value>,
    #[serde(
        rename = "search_parameters",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub search_parameters: Option<Value>,
    #[serde(rename = "web_search_options", skip_serializing_if = "Option::is_none")]
    pub web_search_options: Option<WebSearchOptions>,
    #[serde(
        rename = "usage",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub usage: Option<Value>,
    #[serde(
        rename = "reasoning",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub reasoning: Option<Value>,
    #[serde(
        rename = "vl_high_resolution_images",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub vl_high_resolution_images: Option<Value>,
    #[serde(
        rename = "enable_thinking",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub enable_thinking: Option<Value>,
    #[serde(
        rename = "thinking_budget",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub thinking_budget: Option<Value>,
    #[serde(
        rename = "chat_template_kwargs",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub chat_template_kwargs: Option<Value>,
    #[serde(
        rename = "enable_search",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub enable_search: Option<Value>,
    #[serde(
        rename = "search_options",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub search_options: Option<Value>,
    #[serde(
        rename = "think",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub think: Option<Value>,
    #[serde(
        rename = "web_search",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub web_search: Option<Value>,
    #[serde(
        rename = "thinking",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub thinking: Option<Value>,
    #[serde(
        rename = "search_domain_filter",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub search_domain_filter: Option<Value>,
    #[serde(
        rename = "search_recency_filter",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub search_recency_filter: Option<Value>,
    #[serde(rename = "return_images", skip_serializing_if = "Option::is_none")]
    pub return_images: Option<bool>,
    #[serde(
        rename = "return_related_questions",
        skip_serializing_if = "Option::is_none"
    )]
    pub return_related_questions: Option<bool>,
    #[serde(
        rename = "search_mode",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub search_mode: Option<Value>,
    #[serde(
        rename = "reasoning_split",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub reasoning_split: Option<Value>,
    #[serde(
        rename = "thinking_token_budget",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub thinking_token_budget: Option<Value>,
    #[serde(
        rename = "include_reasoning",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub include_reasoning: Option<Value>,
    #[serde(
        rename = "min_p",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub min_p: Option<Value>,
    #[serde(
        rename = "repetition_penalty",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub repetition_penalty: Option<Value>,
    #[serde(
        rename = "structured_outputs",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub structured_outputs: Option<Value>,
    #[serde(
        rename = "return_token_ids",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub return_token_ids: Option<Value>,
    #[serde(rename = "min_tokens", skip_serializing_if = "Option::is_none")]
    pub min_tokens: Option<u64>,
    #[serde(
        rename = "separate_reasoning",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub separate_reasoning: Option<Value>,
    #[serde(
        rename = "stream_reasoning",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub stream_reasoning: Option<Value>,
    #[serde(
        rename = "regex",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub regex: Option<Value>,
    #[serde(
        rename = "ebnf",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub ebnf: Option<Value>,
    #[serde(
        rename = "stop_token_ids",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub stop_token_i_ds: Option<Value>,
    #[serde(
        rename = "stop_regex",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub stop_regex: Option<Value>,
    #[serde(
        rename = "no_stop_trim",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub no_stop_trim: Option<Value>,
    #[serde(
        rename = "ignore_eos",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub ignore_eos: Option<Value>,
    #[serde(
        rename = "skip_special_tokens",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub skip_special_tokens: Option<Value>,
    #[serde(
        rename = "continue_final_message",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub continue_final_message: Option<Value>,
    #[serde(
        rename = "cache_salt",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub cache_salt: Option<Value>,
}

impl Serialize for GeneralOpenAIRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut object = serde_json::Map::new();
        if !self.model.is_empty() {
            object.insert(
                "model".into(),
                serde_json::to_value(&self.model).map_err(serde::ser::Error::custom)?,
            );
        }
        if !option_empty(&self.messages) {
            object.insert(
                "messages".into(),
                serde_json::to_value(&self.messages).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.prompt.is_null() {
            object.insert(
                "prompt".into(),
                serde_json::to_value(&self.prompt).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.prefix.is_null() {
            object.insert(
                "prefix".into(),
                serde_json::to_value(&self.prefix).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.suffix.is_null() {
            object.insert(
                "suffix".into(),
                serde_json::to_value(&self.suffix).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.stream.is_some() {
            object.insert(
                "stream".into(),
                serde_json::to_value(self.stream).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.stream_options.is_some() {
            object.insert(
                "stream_options".into(),
                serde_json::to_value(&self.stream_options).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.max_tokens.is_some() {
            object.insert(
                "max_tokens".into(),
                serde_json::to_value(self.max_tokens).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.max_completion_tokens.is_some() {
            object.insert(
                "max_completion_tokens".into(),
                serde_json::to_value(self.max_completion_tokens)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.reasoning_effort.is_empty() {
            object.insert(
                "reasoning_effort".into(),
                serde_json::to_value(&self.reasoning_effort).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.verbosity.is_some() {
            object.insert(
                "verbosity".into(),
                serde_json::to_value(&self.verbosity).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.temperature.is_some() {
            object.insert(
                "temperature".into(),
                serde_json::to_value(self.temperature).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.top_p.is_some() {
            object.insert(
                "top_p".into(),
                serde_json::to_value(self.top_p).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.top_k.is_some() {
            object.insert(
                "top_k".into(),
                serde_json::to_value(self.top_k).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.stop.is_null() {
            object.insert(
                "stop".into(),
                serde_json::to_value(&self.stop).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.n.is_some() {
            object.insert(
                "n".into(),
                serde_json::to_value(self.n).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.input.is_null() {
            object.insert(
                "input".into(),
                serde_json::to_value(&self.input).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.instruction.is_empty() {
            object.insert(
                "instruction".into(),
                serde_json::to_value(&self.instruction).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.size.is_empty() {
            object.insert(
                "size".into(),
                serde_json::to_value(&self.size).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.functions.is_some() {
            object.insert(
                "functions".into(),
                serde_json::to_value(&self.functions).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.frequency_penalty.is_some() {
            object.insert(
                "frequency_penalty".into(),
                serde_json::to_value(self.frequency_penalty).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.presence_penalty.is_some() {
            object.insert(
                "presence_penalty".into(),
                serde_json::to_value(self.presence_penalty).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.response_format.is_some() {
            object.insert(
                "response_format".into(),
                serde_json::to_value(&self.response_format).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.encoding_format.is_some() {
            object.insert(
                "encoding_format".into(),
                serde_json::to_value(&self.encoding_format).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.seed.is_some() {
            object.insert(
                "seed".into(),
                serde_json::to_value(self.seed).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.parallel_too_calls.is_some() {
            object.insert(
                "parallel_tool_calls".into(),
                serde_json::to_value(self.parallel_too_calls).map_err(serde::ser::Error::custom)?,
            );
        }
        if !option_empty(&self.tools) {
            object.insert(
                "tools".into(),
                serde_json::to_value(&self.tools).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.tool_choice.is_null() {
            object.insert(
                "tool_choice".into(),
                serde_json::to_value(&self.tool_choice).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.function_call.is_some() {
            object.insert(
                "function_call".into(),
                serde_json::to_value(&self.function_call).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.user.is_some() {
            object.insert(
                "user".into(),
                serde_json::to_value(&self.user).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.service_tier.is_some() {
            object.insert(
                "service_tier".into(),
                serde_json::to_value(&self.service_tier).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.log_probs.is_some() {
            object.insert(
                "logprobs".into(),
                serde_json::to_value(self.log_probs).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.top_log_probs.is_some() {
            object.insert(
                "top_logprobs".into(),
                serde_json::to_value(self.top_log_probs).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.dimensions.is_some() {
            object.insert(
                "dimensions".into(),
                serde_json::to_value(self.dimensions).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.modalities.is_some() {
            object.insert(
                "modalities".into(),
                serde_json::to_value(&self.modalities).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.audio.is_some() {
            object.insert(
                "audio".into(),
                serde_json::to_value(&self.audio).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.safety_identifier.is_some() {
            object.insert(
                "safety_identifier".into(),
                serde_json::to_value(&self.safety_identifier).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.store.is_some() {
            object.insert(
                "store".into(),
                serde_json::to_value(&self.store).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.prompt_cache_key.is_empty() {
            object.insert(
                "prompt_cache_key".into(),
                serde_json::to_value(&self.prompt_cache_key).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.prompt_cache_options.is_some() {
            object.insert(
                "prompt_cache_options".into(),
                serde_json::to_value(&self.prompt_cache_options)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.prompt_cache_retention.is_some() {
            object.insert(
                "prompt_cache_retention".into(),
                serde_json::to_value(&self.prompt_cache_retention)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.logit_bias.is_some() {
            object.insert(
                "logit_bias".into(),
                serde_json::to_value(&self.logit_bias).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.metadata.is_some() {
            object.insert(
                "metadata".into(),
                serde_json::to_value(&self.metadata).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.prediction.is_some() {
            object.insert(
                "prediction".into(),
                serde_json::to_value(&self.prediction).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.extra_body.is_some() {
            object.insert(
                "extra_body".into(),
                serde_json::to_value(&self.extra_body).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.image_config.is_some() {
            object.insert(
                "imageConfig".into(),
                serde_json::to_value(&self.image_config).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.search_parameters.is_some() {
            object.insert(
                "search_parameters".into(),
                serde_json::to_value(&self.search_parameters).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.web_search_options.is_some() {
            object.insert(
                "web_search_options".into(),
                serde_json::to_value(&self.web_search_options)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.usage.is_some() {
            object.insert(
                "usage".into(),
                serde_json::to_value(&self.usage).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.reasoning.is_some() {
            object.insert(
                "reasoning".into(),
                serde_json::to_value(&self.reasoning).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.vl_high_resolution_images.is_some() {
            object.insert(
                "vl_high_resolution_images".into(),
                serde_json::to_value(&self.vl_high_resolution_images)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.enable_thinking.is_some() {
            object.insert(
                "enable_thinking".into(),
                serde_json::to_value(&self.enable_thinking).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.thinking_budget.is_some() {
            object.insert(
                "thinking_budget".into(),
                serde_json::to_value(&self.thinking_budget).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.chat_template_kwargs.is_some() {
            object.insert(
                "chat_template_kwargs".into(),
                serde_json::to_value(&self.chat_template_kwargs)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.enable_search.is_some() {
            object.insert(
                "enable_search".into(),
                serde_json::to_value(&self.enable_search).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.search_options.is_some() {
            object.insert(
                "search_options".into(),
                serde_json::to_value(&self.search_options).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.think.is_some() {
            object.insert(
                "think".into(),
                serde_json::to_value(&self.think).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.web_search.is_some() {
            object.insert(
                "web_search".into(),
                serde_json::to_value(&self.web_search).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.thinking.is_some() {
            object.insert(
                "thinking".into(),
                serde_json::to_value(&self.thinking).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.search_domain_filter.is_some() {
            object.insert(
                "search_domain_filter".into(),
                serde_json::to_value(&self.search_domain_filter)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.search_recency_filter.is_some() {
            object.insert(
                "search_recency_filter".into(),
                serde_json::to_value(&self.search_recency_filter)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.return_images.is_some() {
            object.insert(
                "return_images".into(),
                serde_json::to_value(self.return_images).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.return_related_questions.is_some() {
            object.insert(
                "return_related_questions".into(),
                serde_json::to_value(self.return_related_questions)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.search_mode.is_some() {
            object.insert(
                "search_mode".into(),
                serde_json::to_value(&self.search_mode).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.reasoning_split.is_some() {
            object.insert(
                "reasoning_split".into(),
                serde_json::to_value(&self.reasoning_split).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.thinking_token_budget.is_some() {
            object.insert(
                "thinking_token_budget".into(),
                serde_json::to_value(&self.thinking_token_budget)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.include_reasoning.is_some() {
            object.insert(
                "include_reasoning".into(),
                serde_json::to_value(&self.include_reasoning).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.min_p.is_some() {
            object.insert(
                "min_p".into(),
                serde_json::to_value(&self.min_p).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.repetition_penalty.is_some() {
            object.insert(
                "repetition_penalty".into(),
                serde_json::to_value(&self.repetition_penalty)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.structured_outputs.is_some() {
            object.insert(
                "structured_outputs".into(),
                serde_json::to_value(&self.structured_outputs)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.return_token_ids.is_some() {
            object.insert(
                "return_token_ids".into(),
                serde_json::to_value(&self.return_token_ids).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.min_tokens.is_some() {
            object.insert(
                "min_tokens".into(),
                serde_json::to_value(self.min_tokens).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.separate_reasoning.is_some() {
            object.insert(
                "separate_reasoning".into(),
                serde_json::to_value(&self.separate_reasoning)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.stream_reasoning.is_some() {
            object.insert(
                "stream_reasoning".into(),
                serde_json::to_value(&self.stream_reasoning).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.regex.is_some() {
            object.insert(
                "regex".into(),
                serde_json::to_value(&self.regex).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.ebnf.is_some() {
            object.insert(
                "ebnf".into(),
                serde_json::to_value(&self.ebnf).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.stop_token_i_ds.is_some() {
            object.insert(
                "stop_token_ids".into(),
                serde_json::to_value(&self.stop_token_i_ds).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.stop_regex.is_some() {
            object.insert(
                "stop_regex".into(),
                serde_json::to_value(&self.stop_regex).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.no_stop_trim.is_some() {
            object.insert(
                "no_stop_trim".into(),
                serde_json::to_value(&self.no_stop_trim).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.ignore_eos.is_some() {
            object.insert(
                "ignore_eos".into(),
                serde_json::to_value(&self.ignore_eos).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.skip_special_tokens.is_some() {
            object.insert(
                "skip_special_tokens".into(),
                serde_json::to_value(&self.skip_special_tokens)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.continue_final_message.is_some() {
            object.insert(
                "continue_final_message".into(),
                serde_json::to_value(&self.continue_final_message)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.cache_salt.is_some() {
            object.insert(
                "cache_salt".into(),
                serde_json::to_value(&self.cache_salt).map_err(serde::ser::Error::custom)?,
            );
        }
        crate::wire::normalize_named("GeneralOpenAIRequest", &Value::Object(object))
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageBillingParameters {
    #[serde(rename = "n", skip_serializing_if = "Option::is_none")]
    pub n: Option<u64>,
    #[serde(rename = "prompt_extend", skip_serializing_if = "Option::is_none")]
    pub prompt_extend: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageData {
    #[serde(rename = "url", deserialize_with = "null_default")]
    pub url: String,
    #[serde(rename = "b64_json", deserialize_with = "null_default")]
    pub b64_json: String,
    #[serde(rename = "revised_prompt", deserialize_with = "null_default")]
    pub revised_prompt: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageRequest {
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(rename = "prompt", deserialize_with = "null_default")]
    pub prompt: String,
    #[serde(rename = "n", skip_serializing_if = "Option::is_none")]
    pub n: Option<u64>,
    #[serde(
        rename = "size",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub size: String,
    #[serde(
        rename = "quality",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub quality: String,
    #[serde(
        rename = "response_format",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub response_format: String,
    #[serde(
        rename = "style",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub style: Option<Value>,
    #[serde(
        rename = "user",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub user: Option<Value>,
    #[serde(
        rename = "extra_fields",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub extra_fields: Option<Value>,
    #[serde(
        rename = "background",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub background: Option<Value>,
    #[serde(
        rename = "moderation",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub moderation: Option<Value>,
    #[serde(
        rename = "output_format",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub output_format: Option<Value>,
    #[serde(
        rename = "output_compression",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub output_compression: Option<Value>,
    #[serde(
        rename = "partial_images",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub partial_images: Option<Value>,
    #[serde(rename = "stream", skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(
        rename = "images",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub images: Option<Value>,
    #[serde(
        rename = "mask",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub mask: Option<Value>,
    #[serde(
        rename = "input_fidelity",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub input_fidelity: Option<Value>,
    #[serde(rename = "watermark", skip_serializing_if = "Option::is_none")]
    pub watermark: Option<bool>,
    #[serde(
        rename = "watermark_enabled",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub watermark_enabled: Option<Value>,
    #[serde(
        rename = "user_id",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub user_id: Option<Value>,
    #[serde(
        rename = "image",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub image: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageResponse {
    #[serde(rename = "data")]
    pub data: Option<Vec<ImageData>>,
    #[serde(rename = "created", deserialize_with = "null_default")]
    pub created: i64,
    #[serde(
        rename = "metadata",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IncompleteDetails {
    #[serde(rename = "reason", deserialize_with = "null_default")]
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Input {
    #[serde(
        rename = "type",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub r#type: String,
    #[serde(
        rename = "role",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub role: String,
    #[serde(
        rename = "content",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub content: Option<Value>,
    #[serde(
        rename = "name",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub name: String,
    #[serde(
        rename = "arguments",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub arguments: Option<Value>,
    #[serde(
        rename = "input",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub tool_input: Option<Value>,
    #[serde(
        rename = "output",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub output: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputAudioTranscription {
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputSchema {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(rename = "properties", skip_serializing_if = "Value::is_null")]
    pub properties: Value,
    #[serde(rename = "required", skip_serializing_if = "Value::is_null")]
    pub required: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputTokenDetails {
    #[serde(rename = "cached_tokens", deserialize_with = "null_default")]
    pub cached_tokens: i64,
    #[serde(
        rename = "cached_tokens_details",
        skip_serializing_if = "Option::is_none"
    )]
    pub cached_tokens_details: Option<CachedTokenDetails>,
    #[serde(
        rename = "cached_creation_tokens",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub cached_creation_tokens: i64,
    #[serde(
        rename = "cache_write_tokens",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub cache_write_tokens: i64,
    #[serde(
        rename = "cache_write_tokens_details",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_write_tokens_details: Option<CacheWriteTokenDetails>,
    #[serde(rename = "text_tokens", deserialize_with = "null_default")]
    pub text_tokens: i64,
    #[serde(rename = "audio_tokens", deserialize_with = "null_default")]
    pub audio_tokens: i64,
    #[serde(rename = "image_tokens", deserialize_with = "null_default")]
    pub image_tokens: i64,
    #[serde(
        rename = "cache_creation_input_tokens",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub cache_creation_input_tokens: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LatLng {
    #[serde(rename = "latitude", skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    #[serde(rename = "longitude", skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaContent {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(
        rename = "text",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub text: String,
    #[serde(rename = "image_url", skip_serializing_if = "Value::is_null")]
    pub image_url: Value,
    #[serde(rename = "input_audio", skip_serializing_if = "Value::is_null")]
    pub input_audio: Value,
    #[serde(rename = "file", skip_serializing_if = "Value::is_null")]
    pub file: Value,
    #[serde(rename = "video_url", skip_serializing_if = "Value::is_null")]
    pub video_url: Value,
    #[serde(
        rename = "cache_control",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub cache_control: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaInput {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(
        rename = "text",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub text: String,
    #[serde(
        rename = "file_url",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub file_url: String,
    #[serde(
        rename = "image_url",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub image_url: String,
    #[serde(
        rename = "detail",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Message {
    #[serde(rename = "role", deserialize_with = "null_default")]
    pub role: String,
    #[serde(rename = "content")]
    pub content: Value,
    #[serde(rename = "name", skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "prefix", skip_serializing_if = "Option::is_none")]
    pub prefix: Option<bool>,
    #[serde(rename = "reasoning_content", skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(rename = "reasoning", skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(
        rename = "tool_calls",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub tool_calls: Option<Value>,
    #[serde(
        rename = "tool_call_id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub tool_call_id: String,
    #[serde(
        rename = "tools",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub tools: Option<Value>,
    #[serde(
        rename = "annotations",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub annotations: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageFile {
    #[serde(
        rename = "filename",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub file_name: String,
    #[serde(
        rename = "file_data",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub file_data: String,
    #[serde(
        rename = "file_id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub file_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageImageUrl {
    #[serde(rename = "url", deserialize_with = "null_default")]
    pub url: String,
    #[serde(
        rename = "detail",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageInputAudio {
    #[serde(rename = "data", deserialize_with = "null_default")]
    pub data: String,
    #[serde(rename = "format", deserialize_with = "null_default")]
    pub format: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageVideoUrl {
    #[serde(rename = "url", deserialize_with = "null_default")]
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notify {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(rename = "title", deserialize_with = "null_default")]
    pub title: String,
    #[serde(rename = "content", deserialize_with = "null_default")]
    pub content: String,
    #[serde(rename = "values")]
    pub values: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAIChatCapabilities {
    #[serde(rename = "UseMaxCompletionTokens", deserialize_with = "null_default")]
    pub use_max_completion_tokens: bool,
    #[serde(rename = "UseDeveloperRole", deserialize_with = "null_default")]
    pub use_developer_role: bool,
    #[serde(rename = "SupportsTemperature", deserialize_with = "null_default")]
    pub supports_temperature: bool,
    #[serde(rename = "SupportsTopP", deserialize_with = "null_default")]
    pub supports_top_p: bool,
    #[serde(rename = "SupportsLogProbs", deserialize_with = "null_default")]
    pub supports_log_probs: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAIEmbeddingResponse {
    #[serde(rename = "object", deserialize_with = "null_default")]
    pub object: String,
    #[serde(rename = "data")]
    pub data: Option<Vec<OpenAIEmbeddingResponseItem>>,
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(rename = "usage", deserialize_with = "null_default")]
    pub usage: Usage,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAIEmbeddingResponseItem {
    #[serde(rename = "object", deserialize_with = "null_default")]
    pub object: String,
    #[serde(rename = "index", deserialize_with = "null_default")]
    pub index: i64,
    #[serde(rename = "embedding")]
    pub embedding: Option<Vec<f64>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAIErrorWithStatusCode {
    #[serde(rename = "error")]
    pub error: Value,
    #[serde(rename = "status_code", deserialize_with = "null_default")]
    pub status_code: i64,
    #[serde(rename = "LocalError", deserialize_with = "null_default")]
    pub local_error: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAIModels {
    #[serde(rename = "id", deserialize_with = "null_default")]
    pub id: String,
    #[serde(rename = "object", deserialize_with = "null_default")]
    pub object: String,
    #[serde(rename = "created", deserialize_with = "null_default")]
    pub created: i64,
    #[serde(rename = "owned_by", deserialize_with = "null_default")]
    pub owned_by: String,
    #[serde(rename = "supported_endpoint_types")]
    pub supported_endpoint_types: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAIResponsesCompactionRequest {
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(
        rename = "input",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub input: Option<Value>,
    #[serde(
        rename = "instructions",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub instructions: Option<Value>,
    #[serde(
        rename = "previous_response_id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub previous_response_id: String,
    #[serde(
        rename = "tools",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub tools: Option<Value>,
    #[serde(
        rename = "parallel_tool_calls",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub parallel_tool_calls: Option<Value>,
    #[serde(rename = "reasoning", skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    #[serde(
        rename = "service_tier",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub service_tier: String,
    #[serde(
        rename = "prompt_cache_key",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub prompt_cache_key: Option<Value>,
    #[serde(
        rename = "prompt_cache_options",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub prompt_cache_options: Option<Value>,
    #[serde(
        rename = "prompt_cache_retention",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub prompt_cache_retention: Option<Value>,
    #[serde(
        rename = "text",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub text: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAIResponsesCompactionResponse {
    #[serde(rename = "id", deserialize_with = "null_default")]
    pub id: String,
    #[serde(rename = "object", deserialize_with = "null_default")]
    pub object: String,
    #[serde(rename = "created_at", deserialize_with = "null_default")]
    pub created_at: IntValue,
    #[serde(rename = "output", deserialize_with = "raw_message")]
    pub output: Option<Value>,
    #[serde(rename = "usage")]
    pub usage: Option<Box<Usage>>,
    #[serde(rename = "error", skip_serializing_if = "Value::is_null")]
    pub error: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct OpenAIResponsesRequest {
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(
        rename = "input",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub input: Option<Value>,
    #[serde(
        rename = "include",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub include: Option<Value>,
    #[serde(
        rename = "conversation",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub conversation: Option<Value>,
    #[serde(
        rename = "context_management",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub context_management: Option<Value>,
    #[serde(
        rename = "instructions",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub instructions: Option<Value>,
    #[serde(rename = "max_output_tokens", skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(rename = "top_logprobs", skip_serializing_if = "Option::is_none")]
    pub top_log_probs: Option<i64>,
    #[serde(
        rename = "metadata",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub metadata: Option<Value>,
    #[serde(
        rename = "moderation",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub moderation: Option<Value>,
    #[serde(
        rename = "parallel_tool_calls",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub parallel_tool_calls: Option<Value>,
    #[serde(
        rename = "frequency_penalty",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub frequency_penalty: Option<Value>,
    #[serde(
        rename = "presence_penalty",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub presence_penalty: Option<Value>,
    #[serde(
        rename = "previous_response_id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub previous_response_id: String,
    #[serde(rename = "reasoning", skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    #[serde(
        rename = "service_tier",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub service_tier: String,
    #[serde(
        rename = "store",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub store: Option<Value>,
    #[serde(
        rename = "prompt_cache_key",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub prompt_cache_key: Option<Value>,
    #[serde(
        rename = "prompt_cache_options",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub prompt_cache_options: Option<Value>,
    #[serde(
        rename = "prompt_cache_retention",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub prompt_cache_retention: Option<Value>,
    #[serde(
        rename = "safety_identifier",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub safety_identifier: Option<Value>,
    #[serde(rename = "stream", skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(rename = "stream_options", skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(rename = "temperature", skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(
        rename = "text",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub text: Option<Value>,
    #[serde(
        rename = "tool_choice",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub tool_choice: Option<Value>,
    #[serde(
        rename = "tools",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub tools: Option<Value>,
    #[serde(rename = "top_p", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(
        rename = "truncation",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub truncation: Option<Value>,
    #[serde(
        rename = "user",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub user: Option<Value>,
    #[serde(rename = "max_tool_calls", skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u64>,
    #[serde(
        rename = "prompt",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub prompt: Option<Value>,
    #[serde(
        rename = "client_metadata",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub client_metadata: Option<Value>,
    #[serde(
        rename = "enable_thinking",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub enable_thinking: Option<Value>,
    #[serde(
        rename = "thinking_budget",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub thinking_budget: Option<Value>,
    #[serde(
        rename = "chat_template_kwargs",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub chat_template_kwargs: Option<Value>,
    #[serde(
        rename = "top_k",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub top_k: Option<Value>,
    #[serde(
        rename = "min_p",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub min_p: Option<Value>,
    #[serde(
        rename = "repetition_penalty",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub repetition_penalty: Option<Value>,
    #[serde(
        rename = "stop",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub stop: Option<Value>,
    #[serde(
        rename = "cache_salt",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub cache_salt: Option<Value>,
    #[serde(
        rename = "preset",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub preset: Option<Value>,
}

impl Serialize for OpenAIResponsesRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut object = serde_json::Map::new();
        if true {
            object.insert(
                "model".into(),
                serde_json::to_value(&self.model).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.input.is_some() {
            object.insert(
                "input".into(),
                serde_json::to_value(&self.input).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.include.is_some() {
            object.insert(
                "include".into(),
                serde_json::to_value(&self.include).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.conversation.is_some() {
            object.insert(
                "conversation".into(),
                serde_json::to_value(&self.conversation).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.context_management.is_some() {
            object.insert(
                "context_management".into(),
                serde_json::to_value(&self.context_management)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.instructions.is_some() {
            object.insert(
                "instructions".into(),
                serde_json::to_value(&self.instructions).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.max_output_tokens.is_some() {
            object.insert(
                "max_output_tokens".into(),
                serde_json::to_value(self.max_output_tokens).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.top_log_probs.is_some() {
            object.insert(
                "top_logprobs".into(),
                serde_json::to_value(self.top_log_probs).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.metadata.is_some() {
            object.insert(
                "metadata".into(),
                serde_json::to_value(&self.metadata).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.moderation.is_some() {
            object.insert(
                "moderation".into(),
                serde_json::to_value(&self.moderation).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.parallel_tool_calls.is_some() {
            object.insert(
                "parallel_tool_calls".into(),
                serde_json::to_value(&self.parallel_tool_calls)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.frequency_penalty.is_some() {
            object.insert(
                "frequency_penalty".into(),
                serde_json::to_value(&self.frequency_penalty).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.presence_penalty.is_some() {
            object.insert(
                "presence_penalty".into(),
                serde_json::to_value(&self.presence_penalty).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.previous_response_id.is_empty() {
            object.insert(
                "previous_response_id".into(),
                serde_json::to_value(&self.previous_response_id)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.reasoning.is_some() {
            object.insert(
                "reasoning".into(),
                serde_json::to_value(&self.reasoning).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.service_tier.is_empty() {
            object.insert(
                "service_tier".into(),
                serde_json::to_value(&self.service_tier).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.store.is_some() {
            object.insert(
                "store".into(),
                serde_json::to_value(&self.store).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.prompt_cache_key.is_some() {
            object.insert(
                "prompt_cache_key".into(),
                serde_json::to_value(&self.prompt_cache_key).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.prompt_cache_options.is_some() {
            object.insert(
                "prompt_cache_options".into(),
                serde_json::to_value(&self.prompt_cache_options)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.prompt_cache_retention.is_some() {
            object.insert(
                "prompt_cache_retention".into(),
                serde_json::to_value(&self.prompt_cache_retention)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.safety_identifier.is_some() {
            object.insert(
                "safety_identifier".into(),
                serde_json::to_value(&self.safety_identifier).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.stream.is_some() {
            object.insert(
                "stream".into(),
                serde_json::to_value(self.stream).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.stream_options.is_some() {
            object.insert(
                "stream_options".into(),
                serde_json::to_value(&self.stream_options).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.temperature.is_some() {
            object.insert(
                "temperature".into(),
                serde_json::to_value(self.temperature).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.text.is_some() {
            object.insert(
                "text".into(),
                serde_json::to_value(&self.text).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.tool_choice.is_some() {
            object.insert(
                "tool_choice".into(),
                serde_json::to_value(&self.tool_choice).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.tools.is_some() {
            object.insert(
                "tools".into(),
                serde_json::to_value(&self.tools).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.top_p.is_some() {
            object.insert(
                "top_p".into(),
                serde_json::to_value(self.top_p).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.truncation.is_some() {
            object.insert(
                "truncation".into(),
                serde_json::to_value(&self.truncation).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.user.is_some() {
            object.insert(
                "user".into(),
                serde_json::to_value(&self.user).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.max_tool_calls.is_some() {
            object.insert(
                "max_tool_calls".into(),
                serde_json::to_value(self.max_tool_calls).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.prompt.is_some() {
            object.insert(
                "prompt".into(),
                serde_json::to_value(&self.prompt).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.client_metadata.is_some() {
            object.insert(
                "client_metadata".into(),
                serde_json::to_value(&self.client_metadata).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.enable_thinking.is_some() {
            object.insert(
                "enable_thinking".into(),
                serde_json::to_value(&self.enable_thinking).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.thinking_budget.is_some() {
            object.insert(
                "thinking_budget".into(),
                serde_json::to_value(&self.thinking_budget).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.chat_template_kwargs.is_some() {
            object.insert(
                "chat_template_kwargs".into(),
                serde_json::to_value(&self.chat_template_kwargs)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.top_k.is_some() {
            object.insert(
                "top_k".into(),
                serde_json::to_value(&self.top_k).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.min_p.is_some() {
            object.insert(
                "min_p".into(),
                serde_json::to_value(&self.min_p).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.repetition_penalty.is_some() {
            object.insert(
                "repetition_penalty".into(),
                serde_json::to_value(&self.repetition_penalty)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.stop.is_some() {
            object.insert(
                "stop".into(),
                serde_json::to_value(&self.stop).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.cache_salt.is_some() {
            object.insert(
                "cache_salt".into(),
                serde_json::to_value(&self.cache_salt).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.preset.is_some() {
            object.insert(
                "preset".into(),
                serde_json::to_value(&self.preset).map_err(serde::ser::Error::custom)?,
            );
        }
        crate::wire::normalize_named("OpenAIResponsesRequest", &Value::Object(object))
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAIResponsesResponse {
    #[serde(rename = "id", deserialize_with = "null_default")]
    pub id: String,
    #[serde(rename = "object", deserialize_with = "null_default")]
    pub object: String,
    #[serde(rename = "created_at", deserialize_with = "null_default")]
    pub created_at: IntValue,
    #[serde(rename = "status", deserialize_with = "raw_message")]
    pub status: Option<Value>,
    #[serde(rename = "error", skip_serializing_if = "Value::is_null")]
    pub error: Value,
    #[serde(rename = "incomplete_details", skip_serializing_if = "Option::is_none")]
    pub incomplete_details: Option<IncompleteDetails>,
    #[serde(rename = "instructions", deserialize_with = "raw_message")]
    pub instructions: Option<Value>,
    #[serde(rename = "max_output_tokens", deserialize_with = "null_default")]
    pub max_output_tokens: i64,
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(rename = "output")]
    pub output: Option<Vec<ResponsesOutput>>,
    #[serde(rename = "parallel_tool_calls", deserialize_with = "null_default")]
    pub parallel_tool_calls: bool,
    #[serde(rename = "previous_response_id", deserialize_with = "raw_message")]
    pub previous_response_id: Option<Value>,
    #[serde(rename = "reasoning")]
    pub reasoning: Option<Reasoning>,
    #[serde(rename = "store", deserialize_with = "null_default")]
    pub store: bool,
    #[serde(rename = "temperature", deserialize_with = "null_default")]
    pub temperature: f64,
    #[serde(rename = "tool_choice", deserialize_with = "raw_message")]
    pub tool_choice: Option<Value>,
    #[serde(rename = "tools")]
    pub tools: Option<Vec<Option<std::collections::BTreeMap<String, Value>>>>,
    #[serde(rename = "top_p", deserialize_with = "null_default")]
    pub top_p: f64,
    #[serde(rename = "truncation", deserialize_with = "raw_message")]
    pub truncation: Option<Value>,
    #[serde(rename = "usage")]
    pub usage: Option<Box<Usage>>,
    #[serde(rename = "user", deserialize_with = "raw_message")]
    pub user: Option<Value>,
    #[serde(rename = "metadata", deserialize_with = "raw_message")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAITextResponse {
    #[serde(rename = "id", deserialize_with = "null_default")]
    pub id: String,
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(rename = "object", deserialize_with = "null_default")]
    pub object: String,
    #[serde(rename = "created")]
    pub created: Value,
    #[serde(rename = "choices")]
    pub choices: Option<Vec<OpenAITextResponseChoice>>,
    #[serde(rename = "error", skip_serializing_if = "Value::is_null")]
    pub error: Value,
    #[serde(rename = "usage", deserialize_with = "null_default")]
    pub usage: Usage,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAITextResponseChoice {
    #[serde(rename = "index", deserialize_with = "null_default")]
    pub index: i64,
    #[serde(rename = "message", deserialize_with = "null_default")]
    pub message: Message,
    #[serde(rename = "finish_reason", deserialize_with = "null_default")]
    pub finish_reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAIVideo {
    #[serde(rename = "id", deserialize_with = "null_default")]
    pub id: String,
    #[serde(
        rename = "task_id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub task_id: String,
    #[serde(rename = "object", deserialize_with = "null_default")]
    pub object: String,
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(rename = "status", deserialize_with = "null_default")]
    pub status: String,
    #[serde(rename = "progress", deserialize_with = "null_default")]
    pub progress: i64,
    #[serde(rename = "created_at", deserialize_with = "null_default")]
    pub created_at: i64,
    #[serde(
        rename = "completed_at",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub completed_at: i64,
    #[serde(
        rename = "expires_at",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub expires_at: i64,
    #[serde(
        rename = "seconds",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub seconds: String,
    #[serde(
        rename = "size",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub size: String,
    #[serde(
        rename = "remixed_from_video_id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub remixed_from_video_id: String,
    #[serde(rename = "error", skip_serializing_if = "Option::is_none")]
    pub error: Option<OpenAIVideoError>,
    #[serde(rename = "metadata", skip_serializing_if = "option_empty")]
    pub metadata: Option<std::collections::BTreeMap<String, Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAIVideoError {
    #[serde(rename = "message", deserialize_with = "null_default")]
    pub message: String,
    #[serde(rename = "code", deserialize_with = "null_default")]
    pub code: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAIVideoResponse {
    #[serde(rename = "id", deserialize_with = "null_default")]
    pub id: String,
    #[serde(rename = "object", deserialize_with = "null_default")]
    pub object: String,
    #[serde(rename = "bytes", deserialize_with = "null_default")]
    pub bytes: i64,
    #[serde(rename = "created_at", deserialize_with = "null_default")]
    pub created_at: i64,
    #[serde(rename = "expires_at", deserialize_with = "null_default")]
    pub expires_at: i64,
    #[serde(rename = "filename", deserialize_with = "null_default")]
    pub filename: String,
    #[serde(rename = "purpose", deserialize_with = "null_default")]
    pub purpose: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputConfigForEffort {
    #[serde(
        rename = "effort",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub effort: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputTokenDetails {
    #[serde(rename = "text_tokens", deserialize_with = "null_default")]
    pub text_tokens: i64,
    #[serde(rename = "audio_tokens", deserialize_with = "null_default")]
    pub audio_tokens: i64,
    #[serde(rename = "image_tokens", deserialize_with = "null_default")]
    pub image_tokens: i64,
    #[serde(rename = "reasoning_tokens", deserialize_with = "null_default")]
    pub reasoning_tokens: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayGroundRequest {
    #[serde(
        rename = "model",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub model: String,
    #[serde(
        rename = "group",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub group: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RealTimeTool {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(rename = "name", deserialize_with = "null_default")]
    pub name: String,
    #[serde(rename = "description", deserialize_with = "null_default")]
    pub description: String,
    #[serde(rename = "parameters")]
    pub parameters: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RealtimeContent {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(
        rename = "text",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub text: String,
    #[serde(
        rename = "audio",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub audio: String,
    #[serde(
        rename = "transcript",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub transcript: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RealtimeEvent {
    #[serde(rename = "event_id", deserialize_with = "null_default")]
    pub event_id: String,
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(rename = "session", skip_serializing_if = "Option::is_none")]
    pub session: Option<RealtimeSession>,
    #[serde(rename = "item", skip_serializing_if = "Option::is_none")]
    pub item: Option<RealtimeItem>,
    #[serde(rename = "error", skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    #[serde(rename = "response", skip_serializing_if = "Option::is_none")]
    pub response: Option<RealtimeResponse>,
    #[serde(
        rename = "delta",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub delta: String,
    #[serde(
        rename = "audio",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub audio: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RealtimeItem {
    #[serde(rename = "id", deserialize_with = "null_default")]
    pub id: String,
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(rename = "status", deserialize_with = "null_default")]
    pub status: String,
    #[serde(rename = "role", deserialize_with = "null_default")]
    pub role: String,
    #[serde(rename = "content")]
    pub content: Option<Vec<RealtimeContent>>,
    #[serde(rename = "name", skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "tool_calls", skip_serializing_if = "Value::is_null")]
    pub tool_calls: Value,
    #[serde(
        rename = "call_id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub call_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RealtimeResponse {
    #[serde(rename = "usage")]
    pub usage: Option<RealtimeUsage>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RealtimeSession {
    #[serde(rename = "modalities")]
    pub modalities: Option<Vec<String>>,
    #[serde(rename = "instructions", deserialize_with = "null_default")]
    pub instructions: String,
    #[serde(rename = "voice", deserialize_with = "null_default")]
    pub voice: String,
    #[serde(rename = "input_audio_format", deserialize_with = "null_default")]
    pub input_audio_format: String,
    #[serde(rename = "output_audio_format", deserialize_with = "null_default")]
    pub output_audio_format: String,
    #[serde(
        rename = "input_audio_transcription",
        deserialize_with = "null_default"
    )]
    pub input_audio_transcription: InputAudioTranscription,
    #[serde(rename = "turn_detection")]
    pub turn_detection: Value,
    #[serde(rename = "tools")]
    pub tools: Option<Vec<RealTimeTool>>,
    #[serde(rename = "tool_choice", deserialize_with = "null_default")]
    pub tool_choice: String,
    #[serde(rename = "temperature", deserialize_with = "null_default")]
    pub temperature: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RealtimeUsage {
    #[serde(rename = "total_tokens", deserialize_with = "null_default")]
    pub total_tokens: i64,
    #[serde(rename = "input_tokens", deserialize_with = "null_default")]
    pub input_tokens: i64,
    #[serde(rename = "output_tokens", deserialize_with = "null_default")]
    pub output_tokens: i64,
    #[serde(rename = "input_token_details", deserialize_with = "null_default")]
    pub input_token_details: InputTokenDetails,
    #[serde(rename = "output_token_details", deserialize_with = "null_default")]
    pub output_token_details: OutputTokenDetails,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reasoning {
    #[serde(
        rename = "effort",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub effort: String,
    #[serde(
        rename = "summary",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub summary: String,
    #[serde(
        rename = "mode",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub mode: Option<Value>,
    #[serde(
        rename = "context",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub context: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReasoningConversionState {
    #[serde(rename = "Mode", deserialize_with = "null_default")]
    pub mode: String,
    #[serde(rename = "Effort", deserialize_with = "null_default")]
    pub effort: String,
    #[serde(rename = "BudgetTokens")]
    pub budget_tokens: Option<i64>,
    #[serde(rename = "IncludeThoughts")]
    pub include_thoughts: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RerankDocument {
    #[serde(rename = "text")]
    pub text: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RerankRequest {
    #[serde(rename = "documents")]
    pub documents: Option<Vec<Value>>,
    #[serde(rename = "query", deserialize_with = "null_default")]
    pub query: String,
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(rename = "top_n", skip_serializing_if = "Option::is_none")]
    pub top_n: Option<i64>,
    #[serde(rename = "return_documents", skip_serializing_if = "Option::is_none")]
    pub return_documents: Option<bool>,
    #[serde(rename = "max_chunk_per_doc", skip_serializing_if = "Option::is_none")]
    pub max_chunk_per_doc: Option<i64>,
    #[serde(rename = "overlap_tokens", skip_serializing_if = "Option::is_none")]
    pub over_lap_tokens: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RerankResponse {
    #[serde(rename = "results")]
    pub results: Option<Vec<RerankResponseResult>>,
    #[serde(rename = "usage", deserialize_with = "null_default")]
    pub usage: Usage,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RerankResponseResult {
    #[serde(rename = "document", skip_serializing_if = "Value::is_null")]
    pub document: Value,
    #[serde(rename = "index", deserialize_with = "null_default")]
    pub index: i64,
    #[serde(rename = "relevance_score", deserialize_with = "null_default")]
    pub relevance_score: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResponseFormat {
    #[serde(
        rename = "type",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub r#type: String,
    #[serde(
        rename = "json_schema",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub json_schema: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ResponsesOutput {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(rename = "id", deserialize_with = "null_default")]
    pub id: String,
    #[serde(rename = "status", deserialize_with = "null_default")]
    pub status: String,
    #[serde(rename = "role", deserialize_with = "null_default")]
    pub role: String,
    #[serde(rename = "content")]
    pub content: Option<Vec<ResponsesOutputContent>>,
    #[serde(rename = "summary", skip_serializing_if = "option_empty")]
    pub summary: Option<Vec<ResponsesReasoningSummaryPart>>,
    #[serde(rename = "quality", deserialize_with = "null_default")]
    pub quality: String,
    #[serde(rename = "size", deserialize_with = "null_default")]
    pub size: String,
    #[serde(
        rename = "result",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub result: String,
    #[serde(
        rename = "call_id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub call_id: String,
    #[serde(
        rename = "name",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub name: String,
    #[serde(
        rename = "namespace",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub namespace: String,
    #[serde(
        rename = "arguments",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub arguments: Option<Value>,
    #[serde(
        rename = "input",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub input: Option<Value>,
    #[serde(
        rename = "action",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub action: Option<Value>,
    #[serde(
        rename = "queries",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub queries: Option<Value>,
    #[serde(
        rename = "results",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub results: Option<Value>,
    #[serde(
        rename = "sources",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub sources: Option<Value>,
    #[serde(
        rename = "code",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub code: Option<Value>,
    #[serde(
        rename = "outputs",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub outputs: Option<Value>,
    #[serde(
        rename = "container_id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub container_id: String,
    #[serde(
        rename = "pending_safety_checks",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub pending_safety_checks: Option<Value>,
    #[serde(
        rename = "caller",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub caller: Option<Value>,
    #[serde(
        rename = "server_label",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub server_label: String,
    #[serde(
        rename = "output",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub output: Option<Value>,
    #[serde(
        rename = "error",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub item_error: Option<Value>,
    #[serde(
        rename = "approval_request_id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub approval_request_id: String,
    #[serde(
        rename = "tools",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub mcp_tools: Option<Value>,
}

impl Serialize for ResponsesOutput {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut object = serde_json::Map::new();
        if true {
            object.insert(
                "type".into(),
                serde_json::to_value(&self.r#type).map_err(serde::ser::Error::custom)?,
            );
        }
        if true {
            object.insert(
                "id".into(),
                serde_json::to_value(&self.id).map_err(serde::ser::Error::custom)?,
            );
        }
        if true {
            object.insert(
                "status".into(),
                serde_json::to_value(&self.status).map_err(serde::ser::Error::custom)?,
            );
        }
        if true {
            object.insert(
                "role".into(),
                serde_json::to_value(&self.role).map_err(serde::ser::Error::custom)?,
            );
        }
        if true {
            object.insert(
                "content".into(),
                serde_json::to_value(&self.content).map_err(serde::ser::Error::custom)?,
            );
        }
        if !option_empty(&self.summary) {
            object.insert(
                "summary".into(),
                serde_json::to_value(&self.summary).map_err(serde::ser::Error::custom)?,
            );
        }
        if true {
            object.insert(
                "quality".into(),
                serde_json::to_value(&self.quality).map_err(serde::ser::Error::custom)?,
            );
        }
        if true {
            object.insert(
                "size".into(),
                serde_json::to_value(&self.size).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.result.is_empty() {
            object.insert(
                "result".into(),
                serde_json::to_value(&self.result).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.call_id.is_empty() {
            object.insert(
                "call_id".into(),
                serde_json::to_value(&self.call_id).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.name.is_empty() {
            object.insert(
                "name".into(),
                serde_json::to_value(&self.name).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.namespace.is_empty() {
            object.insert(
                "namespace".into(),
                serde_json::to_value(&self.namespace).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.arguments.is_some() {
            object.insert(
                "arguments".into(),
                serde_json::to_value(&self.arguments).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.input.is_some() {
            object.insert(
                "input".into(),
                serde_json::to_value(&self.input).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.action.is_some() {
            object.insert(
                "action".into(),
                serde_json::to_value(&self.action).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.queries.is_some() {
            object.insert(
                "queries".into(),
                serde_json::to_value(&self.queries).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.results.is_some() {
            object.insert(
                "results".into(),
                serde_json::to_value(&self.results).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.sources.is_some() {
            object.insert(
                "sources".into(),
                serde_json::to_value(&self.sources).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.code.is_some() {
            object.insert(
                "code".into(),
                serde_json::to_value(&self.code).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.outputs.is_some() {
            object.insert(
                "outputs".into(),
                serde_json::to_value(&self.outputs).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.container_id.is_empty() {
            object.insert(
                "container_id".into(),
                serde_json::to_value(&self.container_id).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.pending_safety_checks.is_some() {
            object.insert(
                "pending_safety_checks".into(),
                serde_json::to_value(&self.pending_safety_checks)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.caller.is_some() {
            object.insert(
                "caller".into(),
                serde_json::to_value(&self.caller).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.server_label.is_empty() {
            object.insert(
                "server_label".into(),
                serde_json::to_value(&self.server_label).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.output.is_some() {
            object.insert(
                "output".into(),
                serde_json::to_value(&self.output).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.item_error.is_some() {
            object.insert(
                "error".into(),
                serde_json::to_value(&self.item_error).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.approval_request_id.is_empty() {
            object.insert(
                "approval_request_id".into(),
                serde_json::to_value(&self.approval_request_id)
                    .map_err(serde::ser::Error::custom)?,
            );
        }
        if self.mcp_tools.is_some() {
            object.insert(
                "tools".into(),
                serde_json::to_value(&self.mcp_tools).map_err(serde::ser::Error::custom)?,
            );
        }
        crate::wire::normalize_named("ResponsesOutput", &Value::Object(object))
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResponsesOutputContent {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(rename = "text", deserialize_with = "null_default")]
    pub text: String,
    #[serde(rename = "annotations")]
    pub annotations: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResponsesReasoningSummaryPart {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(rename = "text", deserialize_with = "null_default")]
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResponsesStreamResponse {
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(rename = "response", skip_serializing_if = "Option::is_none")]
    pub response: Option<OpenAIResponsesResponse>,
    #[serde(
        rename = "code",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub code: String,
    #[serde(
        rename = "message",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub message: String,
    #[serde(
        rename = "param",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub param: String,
    #[serde(
        rename = "delta",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub delta: String,
    #[serde(rename = "arguments", skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(rename = "input", skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(
        rename = "name",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub name: String,
    #[serde(rename = "text", skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(rename = "item", skip_serializing_if = "Option::is_none")]
    pub item: Option<ResponsesOutput>,
    #[serde(rename = "sequence_number", skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<i64>,
    #[serde(
        rename = "annotation",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub annotation: Option<Value>,
    #[serde(rename = "annotation_index", skip_serializing_if = "Option::is_none")]
    pub annotation_index: Option<i64>,
    #[serde(
        rename = "obfuscation",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub obfuscation: String,
    #[serde(rename = "output_index", skip_serializing_if = "Option::is_none")]
    pub output_index: Option<i64>,
    #[serde(rename = "content_index", skip_serializing_if = "Option::is_none")]
    pub content_index: Option<i64>,
    #[serde(rename = "summary_index", skip_serializing_if = "Option::is_none")]
    pub summary_index: Option<i64>,
    #[serde(
        rename = "item_id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub item_id: String,
    #[serde(rename = "part", skip_serializing_if = "Option::is_none")]
    pub part: Option<ResponsesReasoningSummaryPart>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrievalConfig {
    #[serde(rename = "latLng", skip_serializing_if = "Option::is_none")]
    pub lat_lng: Option<LatLng>,
    #[serde(
        rename = "languageCode",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub language_code: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Segment {
    #[serde(rename = "id", deserialize_with = "null_default")]
    pub id: i64,
    #[serde(rename = "seek", deserialize_with = "null_default")]
    pub seek: i64,
    #[serde(rename = "start", deserialize_with = "null_default")]
    pub start: f64,
    #[serde(rename = "end", deserialize_with = "null_default")]
    pub end: f64,
    #[serde(rename = "text", deserialize_with = "null_default")]
    pub text: String,
    #[serde(rename = "tokens")]
    pub tokens: Option<Vec<i64>>,
    #[serde(rename = "temperature", deserialize_with = "null_default")]
    pub temperature: f64,
    #[serde(rename = "avg_logprob", deserialize_with = "null_default")]
    pub avg_logprob: f64,
    #[serde(rename = "compression_ratio", deserialize_with = "null_default")]
    pub compression_ratio: f64,
    #[serde(rename = "no_speech_prob", deserialize_with = "null_default")]
    pub no_speech_prob: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SensitiveResponse {
    #[serde(rename = "sensitive_words")]
    pub sensitive_words: Option<Vec<String>>,
    #[serde(rename = "content", deserialize_with = "null_default")]
    pub content: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SimpleResponse {
    #[serde(rename = "usage", deserialize_with = "null_default")]
    pub usage: Usage,
    #[serde(rename = "error")]
    pub error: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamOptions {
    #[serde(
        rename = "include_usage",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub include_usage: bool,
    #[serde(
        rename = "include_obfuscation",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub include_obfuscation: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncableChannel {
    #[serde(rename = "id", deserialize_with = "null_default")]
    pub id: i64,
    #[serde(rename = "name", deserialize_with = "null_default")]
    pub name: String,
    #[serde(rename = "base_url", deserialize_with = "null_default")]
    pub base_url: String,
    #[serde(rename = "status", deserialize_with = "null_default")]
    pub status: i64,
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TestResult {
    #[serde(rename = "name", deserialize_with = "null_default")]
    pub name: String,
    #[serde(rename = "status", deserialize_with = "null_default")]
    pub status: String,
    #[serde(
        rename = "error",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextResponse {
    #[serde(rename = "id", deserialize_with = "null_default")]
    pub id: String,
    #[serde(rename = "object", deserialize_with = "null_default")]
    pub object: String,
    #[serde(rename = "created", deserialize_with = "null_default")]
    pub created: i64,
    #[serde(rename = "model", deserialize_with = "null_default")]
    pub model: String,
    #[serde(rename = "choices")]
    pub choices: Option<Vec<OpenAITextResponseChoice>>,
    #[serde(rename = "usage", deserialize_with = "null_default")]
    pub usage: Usage,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Thinking {
    pub r#type: String,
    pub budget_tokens: Option<i64>,
    pub display: String,
    pub block_binding: Option<Value>,
    /// Unmodeled thinking controls, forwarded verbatim like the Go DTO.
    pub extra: std::collections::BTreeMap<String, Value>,
}

impl Serialize for Thinking {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut object = serde_json::Map::new();
        if !self.r#type.is_empty() {
            object.insert(
                "type".into(),
                serde_json::to_value(&self.r#type).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.budget_tokens.is_some() {
            object.insert(
                "budget_tokens".into(),
                serde_json::to_value(self.budget_tokens).map_err(serde::ser::Error::custom)?,
            );
        }
        if !self.display.is_empty() {
            object.insert(
                "display".into(),
                serde_json::to_value(&self.display).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.block_binding.is_some() {
            object.insert(
                "block_binding".into(),
                serde_json::to_value(&self.block_binding).map_err(serde::ser::Error::custom)?,
            );
        }
        for (key, value) in &self.extra {
            object.entry(key.clone()).or_insert_with(|| value.clone());
        }
        Value::Object(object).serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Thinking {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let mut object = crate::wire::normalize_named("Thinking", &value)
            .map_err(serde::de::Error::custom)?
            .as_object()
            .cloned()
            .unwrap_or_default();
        Ok(Self {
            r#type: object
                .remove("type")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            budget_tokens: object
                .remove("budget_tokens")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            display: object
                .remove("display")
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?
                .unwrap_or_default(),
            block_binding: object.remove("block_binding"),
            extra: object.into_iter().collect(),
        })
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tool {
    #[serde(rename = "name", deserialize_with = "null_default")]
    pub name: String,
    #[serde(
        rename = "description",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub description: String,
    #[serde(rename = "input_schema", deserialize_with = "raw_message")]
    pub input_schema: Option<Value>,
    #[serde(rename = "strict", skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ToolCallRequest {
    #[serde(
        rename = "id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub id: String,
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(rename = "function", deserialize_with = "null_default")]
    pub function: FunctionRequest,
    #[serde(
        rename = "custom",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub custom: Option<Value>,
    /// Host-only complete tool definition, emitted verbatim when present.
    #[serde(skip)]
    pub native: Option<Value>,
}

impl Serialize for ToolCallRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let Some(native) = &self.native {
            return native.serialize(serializer);
        }
        let mut object = serde_json::Map::new();
        if !self.id.is_empty() {
            object.insert(
                "id".into(),
                serde_json::to_value(&self.id).map_err(serde::ser::Error::custom)?,
            );
        }
        if true {
            object.insert(
                "type".into(),
                serde_json::to_value(&self.r#type).map_err(serde::ser::Error::custom)?,
            );
        }
        if true {
            object.insert(
                "function".into(),
                serde_json::to_value(&self.function).map_err(serde::ser::Error::custom)?,
            );
        }
        if self.custom.is_some() {
            object.insert(
                "custom".into(),
                serde_json::to_value(&self.custom).map_err(serde::ser::Error::custom)?,
            );
        }
        crate::wire::normalize_named("ToolCallRequest", &Value::Object(object))
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolCallResponse {
    #[serde(rename = "index", skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    #[serde(
        rename = "id",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: Value,
    #[serde(rename = "function", deserialize_with = "null_default")]
    pub function: FunctionResponse,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolConfig {
    #[serde(
        rename = "functionCallingConfig",
        skip_serializing_if = "Option::is_none"
    )]
    pub function_calling_config: Option<FunctionCallingConfig>,
    #[serde(rename = "retrievalConfig", skip_serializing_if = "Option::is_none")]
    pub retrieval_config: Option<RetrievalConfig>,
    #[serde(
        rename = "includeServerSideToolInvocations",
        skip_serializing_if = "Option::is_none"
    )]
    pub include_server_side_tool_invocations: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpstreamDTO {
    #[serde(
        rename = "id",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub id: i64,
    #[serde(rename = "name", deserialize_with = "null_default")]
    pub name: String,
    #[serde(rename = "base_url", deserialize_with = "null_default")]
    pub base_url: String,
    #[serde(rename = "endpoint", deserialize_with = "null_default")]
    pub endpoint: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpstreamRequest {
    #[serde(rename = "channel_ids")]
    pub channel_i_ds: Option<Vec<i64>>,
    #[serde(rename = "upstreams")]
    pub upstreams: Option<Vec<UpstreamDTO>>,
    #[serde(rename = "timeout", deserialize_with = "null_default")]
    pub timeout: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    #[serde(rename = "prompt_tokens", deserialize_with = "null_default")]
    pub prompt_tokens: i64,
    #[serde(rename = "completion_tokens", deserialize_with = "null_default")]
    pub completion_tokens: i64,
    #[serde(rename = "total_tokens", deserialize_with = "null_default")]
    pub total_tokens: i64,
    #[serde(
        rename = "prompt_cache_hit_tokens",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub prompt_cache_hit_tokens: i64,
    #[serde(
        rename = "usage_semantic",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub usage_semantic: String,
    #[serde(
        rename = "usage_source",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub usage_source: String,
    #[serde(rename = "billing_usage", skip_serializing_if = "Option::is_none")]
    pub billing_usage: Option<Box<BillingUsage>>,
    #[serde(rename = "prompt_tokens_details", deserialize_with = "null_default")]
    pub prompt_tokens_details: InputTokenDetails,
    #[serde(
        rename = "completion_tokens_details",
        deserialize_with = "null_default"
    )]
    pub completion_token_details: OutputTokenDetails,
    #[serde(rename = "input_tokens", deserialize_with = "null_default")]
    pub input_tokens: i64,
    #[serde(rename = "output_tokens", deserialize_with = "null_default")]
    pub output_tokens: i64,
    #[serde(rename = "input_tokens_details")]
    pub input_tokens_details: Option<InputTokenDetails>,
    #[serde(
        rename = "output_tokens_details",
        skip_serializing_if = "Option::is_none"
    )]
    pub output_tokens_details: Option<OutputTokenDetails>,
    #[serde(
        rename = "claude_cache_creation_5_m_tokens",
        deserialize_with = "null_default"
    )]
    pub claude_cache_creation5m_tokens: i64,
    #[serde(
        rename = "claude_cache_creation_1_h_tokens",
        deserialize_with = "null_default"
    )]
    pub claude_cache_creation1h_tokens: i64,
    #[serde(rename = "cost", skip_serializing_if = "Value::is_null")]
    pub cost: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserSetting {
    #[serde(
        rename = "notify_type",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub notify_type: String,
    #[serde(
        rename = "quota_warning_threshold",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub quota_warning_threshold: f64,
    #[serde(
        rename = "quota_warning_enabled",
        skip_serializing_if = "Option::is_none"
    )]
    pub quota_warning_enabled: Option<bool>,
    #[serde(
        rename = "webhook_url",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub webhook_url: String,
    #[serde(
        rename = "webhook_secret",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub webhook_secret: String,
    #[serde(
        rename = "notification_email",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub notification_email: String,
    #[serde(
        rename = "bark_url",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub bark_url: String,
    #[serde(
        rename = "gotify_url",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub gotify_url: String,
    #[serde(
        rename = "gotify_token",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub gotify_token: String,
    #[serde(rename = "gotify_priority", deserialize_with = "null_default")]
    pub gotify_priority: i64,
    #[serde(
        rename = "upstream_model_update_notify_enabled",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub upstream_model_update_notify_enabled: bool,
    #[serde(
        rename = "channel_notify_enabled",
        skip_serializing_if = "Option::is_none"
    )]
    pub channel_notify_enabled: Option<bool>,
    #[serde(
        rename = "accept_unset_model_ratio_model",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub accept_unset_ratio_model: bool,
    #[serde(
        rename = "record_ip_log",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub record_ip_log: bool,
    #[serde(
        rename = "sidebar_modules",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub sidebar_modules: String,
    #[serde(
        rename = "billing_preference",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub billing_preference: String,
    #[serde(
        rename = "language",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub language: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSearchOptions {
    #[serde(
        rename = "search_context_size",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub search_context_size: String,
    #[serde(
        rename = "user_location",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "raw_message"
    )]
    pub user_location: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WhisperVerboseJSONResponse {
    #[serde(
        rename = "task",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub task: String,
    #[serde(
        rename = "language",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub language: String,
    #[serde(
        rename = "duration",
        skip_serializing_if = "is_default",
        deserialize_with = "null_default"
    )]
    pub duration: f64,
    #[serde(
        rename = "text",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "null_default"
    )]
    pub text: String,
    #[serde(rename = "segments", skip_serializing_if = "option_empty")]
    pub segments: Option<Vec<Segment>>,
}

fn raw_message<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}
fn nullable_raw_message<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Option::<Value>::deserialize(d)
}
fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}
trait Empty {
    fn empty(&self) -> bool;
}
impl<T> Empty for Vec<T> {
    fn empty(&self) -> bool {
        self.is_empty()
    }
}
impl<K, V> Empty for std::collections::BTreeMap<K, V> {
    fn empty(&self) -> bool {
        self.is_empty()
    }
}
fn option_empty<T: Empty>(v: &Option<T>) -> bool {
    v.as_ref().is_none_or(Empty::empty)
}
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}
