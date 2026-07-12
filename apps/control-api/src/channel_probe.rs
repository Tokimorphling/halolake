use crate::storage::ManagementStore;
use halolake_control_plane::ManagementError;
use halolake_domain::{CHANNEL_TYPE_ANTHROPIC, CHANNEL_TYPE_GEMINI, CHANNEL_TYPE_OPENAI};
use serde::Deserialize;
use service_async::Service;

const CHANNEL_TYPE_OLLAMA: i32 = 4;
const CHANNEL_TYPE_ALI: i32 = 17;
const CHANNEL_TYPE_OPENROUTER: i32 = 20;
const CHANNEL_TYPE_ZHIPU_V4: i32 = 26;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FetchModelsRequest {
    #[serde(default)]
    pub(crate) channel_id:      Option<u64>,
    #[serde(default)]
    pub(crate) base_url:        String,
    #[serde(rename = "type", default = "default_channel_type")]
    pub(crate) channel_type:    i32,
    #[serde(default)]
    pub(crate) key:             String,
    /// Channel-level header overrides (e.g. xAI chat-proxy CLI identity).
    #[serde(default)]
    pub(crate) header_override: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ChannelProbeService {
    management: ManagementStore,
    client:     reqwest::Client,
}

impl ChannelProbeService {
    pub(crate) fn new(management: ManagementStore) -> Self {
        Self {
            management,
            client: reqwest::Client::new(),
        }
    }
}

impl Service<FetchModelsRequest> for ChannelProbeService {
    type Response = Vec<String>;
    type Error = ManagementError;

    async fn call(&self, req: FetchModelsRequest) -> Result<Self::Response, Self::Error> {
        let request = if let Some(channel_id) = req.channel_id {
            let channel = self
                .management
                .current_data()?
                .channels
                .into_iter()
                .find(|channel| channel.id == channel_id)
                .ok_or(ManagementError::NotFound)?;
            FetchModelsRequest {
                channel_id:      Some(channel_id),
                base_url:        channel.base_url.clone().unwrap_or_default(),
                channel_type:    channel.channel_type,
                key:             channel.key.clone(),
                header_override: channel.header_override.clone(),
            }
        } else {
            req
        };

        let base_url = resolve_base_url(request.channel_type, &request.base_url)?;
        let key = first_key(&request.key);
        let headers = parse_header_override_map(request.header_override.as_deref());
        let models = match request.channel_type {
            CHANNEL_TYPE_OLLAMA => self.fetch_ollama_models(&base_url, &key, &headers).await,
            CHANNEL_TYPE_GEMINI => self.fetch_gemini_models(&base_url, &key, &headers).await,
            _ => {
                self.fetch_openai_compatible_models(request.channel_type, &base_url, &key, &headers)
                    .await
            }
        }?;
        Ok(normalize_model_names(models))
    }
}

impl ChannelProbeService {
    async fn fetch_openai_compatible_models(
        &self,
        channel_type: i32,
        base_url: &str,
        key: &str,
        headers: &[(String, String)],
    ) -> Result<Vec<String>, ManagementError> {
        let url = openai_compatible_models_url(channel_type, base_url);
        let mut request = self.client.get(url);
        if !key.is_empty() {
            if channel_type == CHANNEL_TYPE_ANTHROPIC {
                request = request
                    .header("x-api-key", key)
                    .header("anthropic-version", "2023-06-01");
            } else {
                request = request.bearer_auth(key);
            }
        }
        request = apply_header_overrides(request, headers, key);
        let response = request.send().await.map_err(storage_err)?;
        if !response.status().is_success() {
            return Err(ManagementError::Storage(format!(
                "failed to fetch models: {}",
                response.status()
            )));
        }
        let value = response_json(response).await?;
        Ok(parse_openai_models(&value))
    }

    async fn fetch_ollama_models(
        &self,
        base_url: &str,
        key: &str,
        headers: &[(String, String)],
    ) -> Result<Vec<String>, ManagementError> {
        let mut request = self
            .client
            .get(format!("{}/api/tags", base_url.trim_end_matches('/')));
        if !key.is_empty() {
            request = request.bearer_auth(key);
        }
        request = apply_header_overrides(request, headers, key);
        let response = request.send().await.map_err(storage_err)?;
        if !response.status().is_success() {
            return Err(ManagementError::Storage(format!(
                "failed to fetch Ollama models: {}",
                response.status()
            )));
        }
        let value = response_json(response).await?;
        Ok(parse_ollama_models(&value))
    }

    async fn fetch_gemini_models(
        &self,
        base_url: &str,
        key: &str,
        headers: &[(String, String)],
    ) -> Result<Vec<String>, ManagementError> {
        let mut url = format!("{}/v1beta/models", base_url.trim_end_matches('/'));
        if !key.is_empty() {
            url.push_str("?key=");
            url.push_str(key);
        }
        let mut request = self.client.get(url);
        request = apply_header_overrides(request, headers, key);
        let response = request.send().await.map_err(storage_err)?;
        if !response.status().is_success() {
            return Err(ManagementError::Storage(format!(
                "failed to fetch Gemini models: {}",
                response.status()
            )));
        }
        let value = response_json(response).await?;
        Ok(parse_gemini_models(&value))
    }
}

fn resolve_base_url(channel_type: i32, configured: &str) -> Result<String, ManagementError> {
    let base_url = configured.trim().trim_end_matches('/');
    if !base_url.is_empty() {
        return Ok(base_url.to_string());
    }
    channel_type_default_base_url(channel_type)
        .map(str::to_string)
        .ok_or(ManagementError::InvalidRequest("base_url is required"))
}

fn channel_type_default_base_url(channel_type: i32) -> Option<&'static str> {
    match channel_type {
        CHANNEL_TYPE_OPENAI => Some("https://api.openai.com"),
        CHANNEL_TYPE_ANTHROPIC => Some("https://api.anthropic.com"),
        CHANNEL_TYPE_GEMINI => Some("https://generativelanguage.googleapis.com"),
        CHANNEL_TYPE_OLLAMA => Some("http://localhost:11434"),
        CHANNEL_TYPE_OPENROUTER => Some("https://openrouter.ai/api"),
        _ => None,
    }
}

fn openai_compatible_models_url(channel_type: i32, base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    match channel_type {
        CHANNEL_TYPE_ALI => format!("{base_url}/compatible-mode/v1/models"),
        CHANNEL_TYPE_ZHIPU_V4 => format!("{base_url}/api/paas/v4/models"),
        _ => format!("{base_url}/v1/models"),
    }
}

fn parse_openai_models(value: &serde_json::Value) -> Vec<String> {
    value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("id").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect()
}

fn parse_ollama_models(value: &serde_json::Value) -> Vec<String> {
    value
        .get("models")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("name").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect()
}

fn parse_gemini_models(value: &serde_json::Value) -> Vec<String> {
    value
        .get("models")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.get("name")
                .and_then(serde_json::Value::as_str)
                .or_else(|| item.get("id").and_then(serde_json::Value::as_str))
        })
        .map(|name| name.strip_prefix("models/").unwrap_or(name).to_string())
        .collect()
}

fn normalize_model_names(models: Vec<String>) -> Vec<String> {
    let mut models = models
        .into_iter()
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty())
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();
    models
}

fn parse_header_override_map(raw: Option<&str>) -> Vec<(String, String)> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, entry) in object {
        let key = key.trim();
        if key.is_empty() || key == "*" {
            continue;
        }
        let lower = key.to_ascii_lowercase();
        if lower.starts_with("re:") || lower.starts_with("regex:") {
            continue;
        }
        let Some(text) = entry.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        out.push((key.to_string(), text.to_string()));
    }
    out
}

fn resolve_header_template(template: &str, api_key: &str) -> Option<String> {
    let value = template.replace("{api_key}", api_key);
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn apply_header_overrides(
    mut request: reqwest::RequestBuilder,
    headers: &[(String, String)],
    api_key: &str,
) -> reqwest::RequestBuilder {
    for (name, template) in headers {
        let Some(value) = resolve_header_template(template, api_key) else {
            continue;
        };
        request = request.header(name.as_str(), value);
    }
    request
}

fn first_key(key: &str) -> String {
    key.trim()
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

async fn response_json(response: reqwest::Response) -> Result<serde_json::Value, ManagementError> {
    let bytes = response.bytes().await.map_err(storage_err)?;
    serde_json::from_slice(&bytes).map_err(storage_err)
}

fn storage_err(err: impl std::fmt::Display) -> ManagementError {
    ManagementError::Storage(err.to_string())
}

fn default_channel_type() -> i32 {
    CHANNEL_TYPE_OPENAI
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_and_normalizes_openai_model_ids() {
        let models = normalize_model_names(parse_openai_models(&json!({
            "data": [
                {"id": "gpt-b"},
                {"id": "gpt-a"},
                {"id": "gpt-a"}
            ]
        })));
        assert_eq!(models, ["gpt-a", "gpt-b"]);
    }

    #[test]
    fn parses_gemini_model_names_without_prefix() {
        let models = parse_gemini_models(&json!({
            "models": [
                {"name": "models/gemini-2.5-pro"},
                {"name": "gemini-2.5-flash"}
            ]
        }));
        assert_eq!(models, ["gemini-2.5-pro", "gemini-2.5-flash"]);
    }

    #[test]
    fn builds_provider_specific_model_urls() {
        assert_eq!(
            openai_compatible_models_url(CHANNEL_TYPE_ALI, "https://dashscope.aliyuncs.com"),
            "https://dashscope.aliyuncs.com/compatible-mode/v1/models"
        );
        assert_eq!(
            openai_compatible_models_url(CHANNEL_TYPE_OPENAI, "https://api.openai.com"),
            "https://api.openai.com/v1/models"
        );
    }
}
