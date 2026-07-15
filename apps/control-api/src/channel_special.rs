use crate::{channel_http::ChannelHttpClientFactory, proxy::ProxyStore, storage::ManagementStore};
use axum::body::Body;
use futures_util::lock::Mutex as AsyncMutex;
use halolake_control_plane::{
    ManagementError, RotateChannelCredentialRequest, UpdateChannelRequest,
};
use halolake_domain::ChannelRecord;
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use service_async::Service;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const CHANNEL_TYPE_OLLAMA: i32 = 4;
const CHANNEL_TYPE_CLAUDE: i32 = 14;
const CHANNEL_TYPE_XAI: i32 = 48;
const CHANNEL_TYPE_CODEX: i32 = 57;
const CODEX_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CLAUDE_OAUTH_TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const XAI_OAUTH_DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
const XAI_OAUTH_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const CHATGPT_WEB_BASE_URL: &str = "https://chatgpt.com";
const RECENT_OAUTH_REFRESH_WINDOW_SECS: i64 = 30;

type OAuthRefreshLock = AsyncMutex<()>;
static OAUTH_REFRESH_LOCKS: OnceLock<Mutex<HashMap<u64, Weak<OAuthRefreshLock>>>> = OnceLock::new();

#[derive(Debug, Clone)]
pub(crate) struct ChannelSpecialService {
    management: ManagementStore,
    http:       ChannelHttpClientFactory,
}

impl ChannelSpecialService {
    pub(crate) fn new(management: ManagementStore, proxies: ProxyStore) -> Self {
        Self {
            management,
            http: ChannelHttpClientFactory::new(proxies),
        }
    }

    fn client_for_channel(
        &self,
        channel: &ChannelRecord,
    ) -> Result<reqwest::Client, ManagementError> {
        self.http.client_for_channel(channel)
    }

    fn full_channel(&self, id: u64) -> Result<ChannelRecord, ManagementError> {
        self.management
            .current_data()?
            .channels
            .into_iter()
            .find(|channel| channel.id == id)
            .ok_or(ManagementError::NotFound)
    }

    async fn update_channel(
        &self,
        channel: ChannelRecord,
    ) -> Result<ChannelRecord, ManagementError> {
        self.management.call(UpdateChannelRequest { channel }).await
    }

    pub(crate) async fn refresh_imported_oauth_channel(
        &self,
        id: u64,
    ) -> Result<bool, ManagementError> {
        // Feedback requests can arrive concurrently. Keep exactly one refresh in flight per
        // channel, then re-read storage after acquiring the lock so queued callers observe the
        // rotated refresh token written by the winner. Weak entries are pruned on every lookup.
        let refresh_lock = oauth_refresh_lock(id);
        let _refresh_guard = refresh_lock.lock().await;
        let mut channel = self.full_channel(id)?;
        if channel.channel_type == CHANNEL_TYPE_CODEX {
            if is_multi_key(&channel.key) {
                return Ok(false);
            }
            let Ok(key) = parse_codex_key(&channel.key) else {
                return Ok(false);
            };
            if key
                .refresh_token
                .as_deref()
                .is_none_or(|token| token.trim().is_empty())
            {
                return Ok(false);
            }
            if last_refresh_is_recent(key.last_refresh.as_deref(), now_unix()) {
                return Ok(true);
            }
            self.refresh_codex_oauth_key(channel).await?;
            return Ok(true);
        }
        if !matches!(channel.channel_type, CHANNEL_TYPE_CLAUDE | CHANNEL_TYPE_XAI) {
            return Ok(false);
        }
        let Some(mut credential) = imported_oauth_refresh_credential(&channel) else {
            return Ok(false);
        };
        let expected_key = channel.key.clone();
        let original_setting = credential.setting.clone();
        if setting_last_refresh_is_recent(&credential.setting, now_unix()) {
            return Ok(true);
        }
        let client = self.client_for_channel(&channel)?;
        let (payload, token_endpoint) = match channel.channel_type {
            CHANNEL_TYPE_CLAUDE => (
                refresh_claude_oauth(&client, &credential.refresh_token).await?,
                None,
            ),
            CHANNEL_TYPE_XAI => {
                let token_endpoint = match credential
                    .setting
                    .get("token_endpoint")
                    .and_then(JsonValue::as_str)
                    .map(str::trim)
                    .filter(|endpoint| !endpoint.is_empty())
                {
                    Some(endpoint) => validate_xai_oauth_endpoint(endpoint)?,
                    None => discover_xai_token_endpoint(&client).await?,
                };
                let payload =
                    refresh_xai_oauth(&client, &token_endpoint, &credential.refresh_token).await?;
                (payload, Some(token_endpoint))
            }
            _ => return Ok(false),
        };
        let tokens = parse_oauth_refresh_payload(&payload, &credential.refresh_token)?;
        apply_oauth_refresh(
            &mut channel,
            &mut credential.setting,
            tokens,
            token_endpoint.as_deref(),
            now_unix(),
        )?;
        self.management
            .call(RotateChannelCredentialRequest {
                id: channel.id,
                expected_key,
                new_key: channel.key,
                setting_patch: Some(changed_setting_patch(
                    &original_setting,
                    &credential.setting,
                )?),
            })
            .await?;
        Ok(true)
    }

    fn ollama_channel(&self, id: u64) -> Result<ChannelRecord, ManagementError> {
        let channel = self.full_channel(id)?;
        if channel.channel_type != CHANNEL_TYPE_OLLAMA {
            return Err(ManagementError::InvalidRequest(
                "This operation is only supported for Ollama channels",
            ));
        }
        Ok(channel)
    }

    fn codex_channel(&self, id: u64) -> Result<ChannelRecord, ManagementError> {
        let channel = self.full_channel(id)?;
        if channel.channel_type != CHANNEL_TYPE_CODEX {
            return Err(ManagementError::InvalidRequest("channel type is not Codex"));
        }
        if is_multi_key(&channel.key) {
            return Err(ManagementError::InvalidRequest(
                "multi-key channel is not supported",
            ));
        }
        Ok(channel)
    }

    async fn ollama_json(
        &self,
        channel: &ChannelRecord,
        method: reqwest::Method,
        path: &str,
        body: Option<JsonValue>,
    ) -> Result<JsonValue, ManagementError> {
        let url = format!("{}{}", channel_base_url(channel), path);
        let client = self.client_for_channel(channel)?;
        let mut request = client.request(method, url);
        let key = first_key(&channel.key);
        if !key.is_empty() {
            request = request.bearer_auth(key);
        }
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(serde_json::to_vec(&body).map_err(storage_err)?);
        }
        let response = request.send().await.map_err(storage_err)?;
        response_json(response).await
    }

    async fn refresh_codex_oauth_key(
        &self,
        mut channel: ChannelRecord,
    ) -> Result<CodexOAuthKey, ManagementError> {
        let expected_key = channel.key.clone();
        let mut key = parse_codex_key(&channel.key)?;
        let refresh_token = key
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_string)
            .ok_or(ManagementError::InvalidRequest(
                "codex channel: refresh_token is required to refresh credential",
            ))?;
        let client_id = key
            .client_id
            .as_deref()
            .map(str::trim)
            .filter(|client_id| !client_id.is_empty())
            .unwrap_or(CODEX_OAUTH_CLIENT_ID)
            .to_string();
        let body = format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}&scope={}",
            percent_encode(&refresh_token),
            percent_encode(&client_id),
            percent_encode("openid profile email")
        );
        let response = self
            .client_for_channel(&channel)?
            .post(CODEX_OAUTH_TOKEN_URL)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(storage_err)?;
        let payload = oauth_response_json(response, "codex").await?;
        let access_token = json_str(&payload, &["access_token"])
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                ManagementError::Storage("codex oauth refresh response missing access_token".into())
            })?;
        let new_refresh_token = json_str(&payload, &["refresh_token"])
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(refresh_token);
        let expires_in = json_i64(&payload, &["expires_in"]).unwrap_or_default();
        key.access_token = Some(access_token);
        key.refresh_token = Some(new_refresh_token);
        key.client_id = Some(client_id);
        if let Some(id_token) =
            json_str(&payload, &["id_token"]).filter(|value| !value.trim().is_empty())
        {
            key.id_token = Some(id_token);
        }
        key.last_refresh = Some(now_unix().to_string());
        if expires_in > 0 {
            key.expired = Some(now_unix().saturating_add(expires_in).to_string());
        }
        if key
            .key_type
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            key.key_type = Some("codex".to_string());
        }
        channel.key = serde_json::to_string(&key).map_err(storage_err)?;
        self.management
            .call(RotateChannelCredentialRequest {
                id: channel.id,
                expected_key,
                new_key: channel.key,
                setting_patch: None,
            })
            .await?;
        Ok(key)
    }

    /// Serialize every Codex refresh entry point, including manual refresh and
    /// WHAM's 401 retry. Re-read under the lock so a waiter never reuses a
    /// rotating refresh token that another request has already consumed.
    async fn refresh_codex_oauth_key_singleflight(
        &self,
        channel_id: u64,
        observed_access_token: Option<&str>,
    ) -> Result<CodexOAuthKey, ManagementError> {
        let refresh_lock = oauth_refresh_lock(channel_id);
        let _refresh_guard = refresh_lock.lock().await;
        let channel = self.codex_channel(channel_id)?;
        let key = parse_codex_key(&channel.key)?;
        if observed_access_token.is_some_and(|observed| {
            key.access_token
                .as_deref()
                .is_some_and(|current| current.trim() != observed.trim())
        }) {
            return Ok(key);
        }
        self.refresh_codex_oauth_key(channel).await
    }

    async fn fetch_codex_wham(
        &self,
        mut channel: ChannelRecord,
        kind: CodexWhamKind,
    ) -> Result<CodexWhamResponse, ManagementError> {
        let mut key = parse_codex_key(&channel.key)?;
        let mut response = self.codex_wham_once(&channel, &key, kind).await?;
        if matches!(response.upstream_status, 401 | 403)
            && key
                .refresh_token
                .as_deref()
                .is_some_and(|token| !token.trim().is_empty())
            && let Ok(new_key) = self
                .refresh_codex_oauth_key_singleflight(channel.id, key.access_token.as_deref())
                .await
        {
            key = new_key;
            channel.key = serde_json::to_string(&key).map_err(storage_err)?;
            response = self.codex_wham_once(&channel, &key, kind).await?;
        }
        Ok(response)
    }

    async fn codex_wham_once(
        &self,
        channel: &ChannelRecord,
        key: &CodexOAuthKey,
        kind: CodexWhamKind,
    ) -> Result<CodexWhamResponse, ManagementError> {
        let access_token = key
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or(ManagementError::InvalidRequest(
                "codex channel: access_token is required",
            ))?;
        let account_id = key
            .account_id
            .as_deref()
            .map(str::trim)
            .filter(|account_id| !account_id.is_empty())
            .ok_or(ManagementError::InvalidRequest(
                "codex channel: account_id is required",
            ))?;
        let base = codex_wham_base_url(channel);
        let (method, path, body) = match kind {
            CodexWhamKind::Usage => (reqwest::Method::GET, "/backend-api/wham/usage", None),
            CodexWhamKind::ResetCredits => (
                reqwest::Method::GET,
                "/backend-api/wham/rate-limit-reset-credits",
                None,
            ),
            CodexWhamKind::ConsumeResetCredit => (
                reqwest::Method::POST,
                "/backend-api/wham/rate-limit-reset-credits/consume",
                Some(json!({ "redeem_request_id": Uuid::new_v4().to_string() })),
            ),
        };
        let mut request = self
            .client_for_channel(channel)?
            .request(method, format!("{base}{path}"))
            .bearer_auth(access_token)
            .header("chatgpt-account-id", account_id)
            .header("accept", "application/json")
            .header("openai-beta", "codex-1")
            .header("oai-language", "zh-CN")
            .header("originator", "Codex Desktop")
            .header("sec-fetch-site", "none")
            .header("sec-fetch-mode", "no-cors")
            .header("sec-fetch-dest", "empty")
            .header("priority", "u=4, i");
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(serde_json::to_vec(&body).map_err(storage_err)?);
        }
        let response = request.send().await.map_err(storage_err)?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(storage_err)?;
        let data = serde_json::from_slice::<JsonValue>(&bytes)
            .unwrap_or_else(|_| JsonValue::String(String::from_utf8_lossy(&bytes).to_string()));
        Ok(CodexWhamResponse {
            success: (200..300).contains(&status),
            upstream_status: status,
            data,
        })
    }
}

impl Service<OllamaPullModelRequest> for ChannelSpecialService {
    type Response = OllamaPullModelResponse;
    type Error = ManagementError;

    async fn call(&self, req: OllamaPullModelRequest) -> Result<Self::Response, Self::Error> {
        if req.channel_id == 0 || req.model_name.trim().is_empty() {
            return Err(ManagementError::InvalidRequest(
                "Channel ID and model name are required",
            ));
        }
        let channel = self.ollama_channel(req.channel_id)?;
        let body = json!({
            "name": req.model_name.trim(),
            "stream": req.stream,
        });
        let value = self
            .ollama_json(&channel, reqwest::Method::POST, "/api/pull", Some(body))
            .await?;
        Ok(OllamaPullModelResponse {
            message: format!("Model {} pulled successfully", req.model_name.trim()),
            event:   value,
        })
    }
}

impl Service<OllamaDeleteModelRequest> for ChannelSpecialService {
    type Response = String;
    type Error = ManagementError;

    async fn call(&self, req: OllamaDeleteModelRequest) -> Result<Self::Response, Self::Error> {
        if req.channel_id == 0 || req.model_name.trim().is_empty() {
            return Err(ManagementError::InvalidRequest(
                "Channel ID and model name are required",
            ));
        }
        let channel = self.ollama_channel(req.channel_id)?;
        self.ollama_json(
            &channel,
            reqwest::Method::DELETE,
            "/api/delete",
            Some(json!({ "name": req.model_name.trim() })),
        )
        .await?;
        Ok(format!(
            "Model {} deleted successfully",
            req.model_name.trim()
        ))
    }
}

impl Service<OllamaVersionRequest> for ChannelSpecialService {
    type Response = OllamaVersionResponse;
    type Error = ManagementError;

    async fn call(&self, req: OllamaVersionRequest) -> Result<Self::Response, Self::Error> {
        let channel = self.ollama_channel(req.id)?;
        let value = self
            .ollama_json(&channel, reqwest::Method::GET, "/api/version", None)
            .await?;
        Ok(OllamaVersionResponse {
            version: json_str(&value, &["version"]).unwrap_or_default(),
        })
    }
}

impl Service<MultiKeyManageRequest> for ChannelSpecialService {
    type Response = MultiKeyManageResponse;
    type Error = ManagementError;

    async fn call(&self, req: MultiKeyManageRequest) -> Result<Self::Response, Self::Error> {
        let mut channel = self.full_channel(req.channel_id)?;
        let keys = channel_keys(&channel.key);
        if keys.len() <= 1 {
            return Ok(MultiKeyManageResponse::message(
                false,
                "该渠道不是多密钥模式",
            ));
        }
        let action = req.action.trim();
        if action == "get_key_status" {
            return Ok(multi_key_status_response(&channel, &req, &keys));
        }

        let mut state = multi_key_state(&channel);
        let message = match action {
            "disable_key" => {
                let idx = checked_key_index(&req, keys.len())?;
                state.status.insert(idx, 2);
                state.disabled_time.insert(idx, now_unix());
                state
                    .disabled_reason
                    .insert(idx, "manual disabled".to_string());
                "密钥已禁用".to_string()
            }
            "enable_key" => {
                let idx = checked_key_index(&req, keys.len())?;
                state.status.remove(&idx);
                state.disabled_time.remove(&idx);
                state.disabled_reason.remove(&idx);
                "密钥已启用".to_string()
            }
            "enable_all_keys" => {
                let enabled = state.status.len();
                state = MultiKeyState::default();
                format!("已启用 {enabled} 个密钥")
            }
            "disable_all_keys" => {
                let mut disabled = 0usize;
                for idx in 0..keys.len() {
                    if state.status.get(&idx).copied().unwrap_or(1) == 1 {
                        state.status.insert(idx, 2);
                        state.disabled_time.insert(idx, now_unix());
                        state
                            .disabled_reason
                            .insert(idx, "manual disabled".to_string());
                        disabled = disabled.saturating_add(1);
                    }
                }
                if disabled == 0 {
                    return Ok(MultiKeyManageResponse::message(false, "没有可禁用的密钥"));
                }
                format!("已禁用 {disabled} 个密钥")
            }
            "delete_key" => {
                let idx = checked_key_index(&req, keys.len())?;
                if keys.len() == 1 {
                    return Ok(MultiKeyManageResponse::message(
                        false,
                        "不能删除最后一个密钥",
                    ));
                }
                let remaining = keys
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != idx)
                    .map(|(_, key)| key.clone())
                    .collect::<Vec<_>>();
                state = reindex_multi_key_state(&state, keys.len(), |i| i != idx);
                channel.key = remaining.join("\n");
                "密钥已删除".to_string()
            }
            "delete_disabled_keys" => {
                let mut deleted = 0usize;
                let remaining = keys
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, key)| {
                        if state.status.get(&idx).copied().unwrap_or(1) == 3 {
                            deleted = deleted.saturating_add(1);
                            None
                        } else {
                            Some(key.clone())
                        }
                    })
                    .collect::<Vec<_>>();
                if deleted == 0 {
                    return Ok(MultiKeyManageResponse::message(
                        false,
                        "没有需要删除的自动禁用密钥",
                    ));
                }
                state = reindex_multi_key_state(&state, keys.len(), |idx| {
                    state.status.get(&idx).copied().unwrap_or(1) != 3
                });
                channel.key = remaining.join("\n");
                save_multi_key_state(&mut channel, &state)?;
                self.update_channel(channel).await?;
                return Ok(MultiKeyManageResponse::WithData {
                    success: true,
                    message: format!("已删除 {deleted} 个自动禁用的密钥"),
                    data:    json!(deleted),
                });
            }
            _ => return Ok(MultiKeyManageResponse::message(false, "不支持的操作")),
        };
        save_multi_key_state(&mut channel, &state)?;
        self.update_channel(channel).await?;
        Ok(MultiKeyManageResponse::message(true, message))
    }
}

impl Service<CodexRefreshCredentialRequest> for ChannelSpecialService {
    type Response = CodexRefreshCredentialResponse;
    type Error = ManagementError;

    async fn call(
        &self,
        req: CodexRefreshCredentialRequest,
    ) -> Result<Self::Response, Self::Error> {
        let channel = self.codex_channel(req.id)?;
        let observed_access_token = parse_codex_key(&channel.key)?.access_token;
        let key = self
            .refresh_codex_oauth_key_singleflight(channel.id, observed_access_token.as_deref())
            .await?;
        Ok(CodexRefreshCredentialResponse {
            expires_at:   key.expired.unwrap_or_default(),
            last_refresh: key.last_refresh.unwrap_or_default(),
            account_id:   key.account_id.unwrap_or_default(),
            email:        key.email.unwrap_or_default(),
            channel_id:   channel.id,
            channel_type: channel.channel_type,
            channel_name: channel.name,
        })
    }
}

impl Service<CodexWhamRequest> for ChannelSpecialService {
    type Response = CodexWhamResponse;
    type Error = ManagementError;

    async fn call(&self, req: CodexWhamRequest) -> Result<Self::Response, Self::Error> {
        let channel = self.codex_channel(req.id)?;
        self.fetch_codex_wham(channel, req.kind).await
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OllamaModelRequestBody {
    pub(crate) channel_id: u64,
    pub(crate) model_name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct OllamaPullModelRequest {
    pub(crate) channel_id: u64,
    pub(crate) model_name: String,
    pub(crate) stream:     bool,
}

#[derive(Debug, Clone)]
pub(crate) struct OllamaDeleteModelRequest {
    pub(crate) channel_id: u64,
    pub(crate) model_name: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OllamaVersionRequest {
    pub(crate) id: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct OllamaPullModelResponse {
    pub(crate) message: String,
    pub(crate) event:   JsonValue,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct OllamaVersionResponse {
    pub(crate) version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MultiKeyManageRequest {
    pub(crate) channel_id: u64,
    pub(crate) action:     String,
    #[serde(default)]
    pub(crate) key_index:  Option<usize>,
    #[serde(default)]
    pub(crate) page:       usize,
    #[serde(default)]
    pub(crate) page_size:  usize,
    #[serde(default)]
    pub(crate) status:     Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum MultiKeyManageResponse {
    Message {
        success: bool,
        message: String,
    },
    Status {
        success: bool,
        message: String,
        data:    MultiKeyStatusResponse,
    },
    WithData {
        success: bool,
        message: String,
        data:    JsonValue,
    },
}

impl MultiKeyManageResponse {
    fn message(success: bool, message: impl Into<String>) -> Self {
        Self::Message {
            success,
            message: message.into(),
        }
    }

    fn status(data: MultiKeyStatusResponse) -> Self {
        Self::Status {
            success: true,
            message: String::new(),
            data,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MultiKeyStatusResponse {
    pub(crate) keys:                  Vec<KeyStatus>,
    pub(crate) total:                 usize,
    pub(crate) page:                  usize,
    pub(crate) page_size:             usize,
    pub(crate) total_pages:           usize,
    pub(crate) enabled_count:         usize,
    pub(crate) manual_disabled_count: usize,
    pub(crate) auto_disabled_count:   usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct KeyStatus {
    pub(crate) index:         usize,
    pub(crate) status:        i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) disabled_time: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason:        Option<String>,
    pub(crate) key_preview:   String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CodexRefreshCredentialRequest {
    pub(crate) id: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CodexWhamRequest {
    pub(crate) id:   u64,
    pub(crate) kind: CodexWhamKind,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum CodexWhamKind {
    Usage,
    ResetCredits,
    ConsumeResetCredit,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CodexRefreshCredentialResponse {
    pub(crate) expires_at:   String,
    pub(crate) last_refresh: String,
    pub(crate) account_id:   String,
    pub(crate) email:        String,
    pub(crate) channel_id:   u64,
    pub(crate) channel_type: i32,
    pub(crate) channel_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CodexWhamResponse {
    pub(crate) success:         bool,
    pub(crate) upstream_status: u16,
    pub(crate) data:            JsonValue,
}

impl OllamaPullModelResponse {
    pub(crate) fn event_stream_body(&self) -> Body {
        let event = serde_json::to_string(&self.event).unwrap_or_else(|_| "{}".to_string());
        let done = serde_json::to_string(&json!({ "message": self.message }))
            .unwrap_or_else(|_| "{}".to_string());
        Body::from(format!("data: {event}\n\ndata: {done}\n\ndata: [DONE]\n\n"))
    }
}

#[derive(Debug, Clone, Default)]
struct MultiKeyState {
    status:          BTreeMap<usize, i32>,
    disabled_time:   BTreeMap<usize, i64>,
    disabled_reason: BTreeMap<usize, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CodexOAuthKey {
    #[serde(default)]
    id_token:      Option<String>,
    #[serde(default)]
    access_token:  Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    client_id:     Option<String>,
    #[serde(default)]
    account_id:    Option<String>,
    #[serde(default)]
    last_refresh:  Option<String>,
    #[serde(default)]
    email:         Option<String>,
    #[serde(rename = "type", default)]
    key_type:      Option<String>,
    #[serde(default)]
    expired:       Option<String>,
}

#[derive(Debug, Clone)]
struct ImportedOAuthRefreshCredential {
    setting:       JsonValue,
    refresh_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OAuthRefreshTokens {
    access_token:  String,
    refresh_token: String,
    id_token:      Option<String>,
    expires_in:    i64,
}

fn multi_key_status_response(
    channel: &ChannelRecord,
    req: &MultiKeyManageRequest,
    keys: &[String],
) -> MultiKeyManageResponse {
    let state = multi_key_state(channel);
    let mut enabled_count = 0usize;
    let mut manual_disabled_count = 0usize;
    let mut auto_disabled_count = 0usize;
    let mut all = Vec::with_capacity(keys.len());
    for (idx, key) in keys.iter().enumerate() {
        let status = state.status.get(&idx).copied().unwrap_or(1);
        match status {
            1 => enabled_count = enabled_count.saturating_add(1),
            2 => manual_disabled_count = manual_disabled_count.saturating_add(1),
            3 => auto_disabled_count = auto_disabled_count.saturating_add(1),
            _ => {}
        }
        all.push(KeyStatus {
            index: idx,
            status,
            disabled_time: (status != 1)
                .then(|| state.disabled_time.get(&idx).copied())
                .flatten(),
            reason: (status != 1)
                .then(|| state.disabled_reason.get(&idx).cloned())
                .flatten(),
            key_preview: key_preview(key),
        });
    }
    let filtered = all
        .into_iter()
        .filter(|item| req.status.is_none_or(|status| item.status == status))
        .collect::<Vec<_>>();
    let total = filtered.len();
    let page_size = req.page_size.max(1);
    let total_pages = total.div_ceil(page_size).max(1);
    let page = req.page.max(1).min(total_pages);
    let start = (page - 1) * page_size;
    let keys = filtered.into_iter().skip(start).take(page_size).collect();
    MultiKeyManageResponse::status(MultiKeyStatusResponse {
        keys,
        total,
        page,
        page_size,
        total_pages,
        enabled_count,
        manual_disabled_count,
        auto_disabled_count,
    })
}

fn multi_key_state(channel: &ChannelRecord) -> MultiKeyState {
    let value = channel
        .setting
        .as_deref()
        .and_then(|setting| serde_json::from_str::<JsonValue>(setting).ok())
        .unwrap_or_else(|| json!({}));
    MultiKeyState {
        status:          json_usize_i32_map(value.get("multi_key_status_list")),
        disabled_time:   json_usize_i64_map(value.get("multi_key_disabled_time")),
        disabled_reason: json_usize_string_map(value.get("multi_key_disabled_reason")),
    }
}

fn save_multi_key_state(
    channel: &mut ChannelRecord,
    state: &MultiKeyState,
) -> Result<(), ManagementError> {
    let mut value = channel
        .setting
        .as_deref()
        .and_then(|setting| serde_json::from_str::<JsonValue>(setting).ok())
        .unwrap_or_else(|| json!({}));
    if !value.is_object() {
        value = json!({});
    }
    let object = value.as_object_mut().expect("setting is object");
    object.insert(
        "multi_key_status_list".to_string(),
        usize_i32_map_json(&state.status),
    );
    object.insert(
        "multi_key_disabled_time".to_string(),
        usize_i64_map_json(&state.disabled_time),
    );
    object.insert(
        "multi_key_disabled_reason".to_string(),
        usize_string_map_json(&state.disabled_reason),
    );
    channel.setting = Some(serde_json::to_string(&value).map_err(storage_err)?);
    Ok(())
}

fn reindex_multi_key_state(
    state: &MultiKeyState,
    len: usize,
    keep: impl Fn(usize) -> bool,
) -> MultiKeyState {
    let mut next = MultiKeyState::default();
    let mut next_idx = 0usize;
    for idx in 0..len {
        if !keep(idx) {
            continue;
        }
        if let Some(status) = state.status.get(&idx).copied()
            && status != 1
        {
            next.status.insert(next_idx, status);
        }
        if let Some(value) = state.disabled_time.get(&idx).copied() {
            next.disabled_time.insert(next_idx, value);
        }
        if let Some(value) = state.disabled_reason.get(&idx).cloned() {
            next.disabled_reason.insert(next_idx, value);
        }
        next_idx = next_idx.saturating_add(1);
    }
    next
}

fn checked_key_index(req: &MultiKeyManageRequest, len: usize) -> Result<usize, ManagementError> {
    let idx = req
        .key_index
        .ok_or(ManagementError::InvalidRequest("未指定密钥索引"))?;
    if idx >= len {
        return Err(ManagementError::InvalidRequest("密钥索引超出范围"));
    }
    Ok(idx)
}

fn json_usize_i32_map(value: Option<&JsonValue>) -> BTreeMap<usize, i32> {
    value
        .and_then(JsonValue::as_object)
        .into_iter()
        .flat_map(JsonMap::iter)
        .filter_map(|(key, value)| Some((key.parse().ok()?, json_i64_value(value)? as i32)))
        .collect()
}

fn json_usize_i64_map(value: Option<&JsonValue>) -> BTreeMap<usize, i64> {
    value
        .and_then(JsonValue::as_object)
        .into_iter()
        .flat_map(JsonMap::iter)
        .filter_map(|(key, value)| Some((key.parse().ok()?, json_i64_value(value)?)))
        .collect()
}

fn json_usize_string_map(value: Option<&JsonValue>) -> BTreeMap<usize, String> {
    value
        .and_then(JsonValue::as_object)
        .into_iter()
        .flat_map(JsonMap::iter)
        .filter_map(|(key, value)| Some((key.parse().ok()?, value.as_str()?.to_string())))
        .collect()
}

fn usize_i32_map_json(map: &BTreeMap<usize, i32>) -> JsonValue {
    JsonValue::Object(
        map.iter()
            .map(|(key, value)| (key.to_string(), json!(value)))
            .collect(),
    )
}

fn usize_i64_map_json(map: &BTreeMap<usize, i64>) -> JsonValue {
    JsonValue::Object(
        map.iter()
            .map(|(key, value)| (key.to_string(), json!(value)))
            .collect(),
    )
}

fn usize_string_map_json(map: &BTreeMap<usize, String>) -> JsonValue {
    JsonValue::Object(
        map.iter()
            .map(|(key, value)| (key.to_string(), json!(value)))
            .collect(),
    )
}

fn channel_keys(key: &str) -> Vec<String> {
    key.lines()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .collect()
}

fn key_preview(key: &str) -> String {
    let mut chars = key.chars();
    let preview = chars.by_ref().take(10).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

fn parse_codex_key(raw: &str) -> Result<CodexOAuthKey, ManagementError> {
    let flexible = crate::control_api_ext::parse_flexible_codex_key(raw)?;
    Ok(CodexOAuthKey {
        id_token:      flexible.id_token,
        access_token:  flexible.access_token,
        refresh_token: flexible.refresh_token,
        client_id:     flexible.client_id,
        account_id:    flexible.account_id,
        last_refresh:  flexible.last_refresh,
        email:         flexible.email,
        key_type:      flexible.key_type,
        expired:       flexible.expired,
    })
}

fn oauth_refresh_lock(channel_id: u64) -> Arc<OAuthRefreshLock> {
    let registry = OAUTH_REFRESH_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&channel_id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(AsyncMutex::new(()));
    locks.insert(channel_id, Arc::downgrade(&lock));
    lock
}

fn last_refresh_is_recent(last_refresh: Option<&str>, now: i64) -> bool {
    last_refresh
        .map(str::trim)
        .and_then(|value| value.parse::<i64>().ok())
        .is_some_and(|refreshed_at| refresh_timestamp_is_recent(refreshed_at, now))
}

fn refresh_timestamp_is_recent(refreshed_at: i64, now: i64) -> bool {
    refreshed_at >= now.saturating_sub(RECENT_OAUTH_REFRESH_WINDOW_SECS)
        && refreshed_at <= now.saturating_add(RECENT_OAUTH_REFRESH_WINDOW_SECS)
}

fn setting_last_refresh_is_recent(setting: &JsonValue, now: i64) -> bool {
    let Some(last_refresh) = setting.get("last_refresh") else {
        return false;
    };
    match last_refresh {
        JsonValue::String(value) => last_refresh_is_recent(Some(value), now),
        JsonValue::Number(value) => value
            .as_i64()
            .is_some_and(|refreshed_at| refresh_timestamp_is_recent(refreshed_at, now)),
        _ => false,
    }
}

fn imported_oauth_refresh_credential(
    channel: &ChannelRecord,
) -> Option<ImportedOAuthRefreshCredential> {
    let setting = serde_json::from_str::<JsonValue>(channel.setting.as_deref()?.trim()).ok()?;
    let object = setting.as_object()?;
    let auth_kind = object
        .get("auth_kind")?
        .as_str()?
        .trim()
        .to_ascii_lowercase();
    if !matches!(auth_kind.as_str(), "oauth" | "setup-token" | "setup_token") {
        return None;
    }
    let refresh_token = object.get("refresh_token")?.as_str()?.trim().to_string();
    if refresh_token.is_empty() {
        return None;
    }
    Some(ImportedOAuthRefreshCredential {
        setting,
        refresh_token,
    })
}

fn parse_oauth_refresh_payload(
    payload: &JsonValue,
    previous_refresh_token: &str,
) -> Result<OAuthRefreshTokens, ManagementError> {
    let access_token = json_str(payload, &["access_token"])
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            ManagementError::Storage("oauth refresh response missing access_token".into())
        })?;
    let refresh_token = json_str(payload, &["refresh_token"])
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .unwrap_or_else(|| previous_refresh_token.to_string());
    let id_token = json_str(payload, &["id_token"])
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty());
    Ok(OAuthRefreshTokens {
        access_token,
        refresh_token,
        id_token,
        expires_in: json_i64(payload, &["expires_in"]).unwrap_or_default(),
    })
}

fn apply_oauth_refresh(
    channel: &mut ChannelRecord,
    setting: &mut JsonValue,
    tokens: OAuthRefreshTokens,
    token_endpoint: Option<&str>,
    refreshed_at: i64,
) -> Result<(), ManagementError> {
    let object = setting
        .as_object_mut()
        .ok_or(ManagementError::InvalidRequest(
            "oauth channel setting must be an object",
        ))?;
    channel.key = tokens.access_token;
    object.insert(
        "refresh_token".to_string(),
        JsonValue::String(tokens.refresh_token),
    );
    if let Some(id_token) = tokens.id_token {
        object.insert("id_token".to_string(), JsonValue::String(id_token));
    }
    if tokens.expires_in > 0 {
        object.insert(
            "expired".to_string(),
            JsonValue::String(refreshed_at.saturating_add(tokens.expires_in).to_string()),
        );
    }
    object.insert(
        "last_refresh".to_string(),
        JsonValue::String(refreshed_at.to_string()),
    );
    if let Some(token_endpoint) = token_endpoint
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
    {
        object.insert(
            "token_endpoint".to_string(),
            JsonValue::String(token_endpoint.to_string()),
        );
    }
    channel.setting = Some(serde_json::to_string(setting).map_err(storage_err)?);
    Ok(())
}

/// Produce a sparse JSON object containing only fields changed by the refresh.
/// It is merged into the current record under the management-store lock, so a
/// concurrent manual status/proxy edit is never replaced by a stale clone.
fn changed_setting_patch(before: &JsonValue, after: &JsonValue) -> Result<String, ManagementError> {
    let before = before.as_object().ok_or(ManagementError::InvalidRequest(
        "oauth channel setting must be an object",
    ))?;
    let after = after.as_object().ok_or(ManagementError::InvalidRequest(
        "oauth channel setting must be an object",
    ))?;
    let changed = after
        .iter()
        .filter(|(key, value)| before.get(*key) != Some(*value))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<JsonMap<_, _>>();
    serde_json::to_string(&JsonValue::Object(changed)).map_err(storage_err)
}

async fn refresh_claude_oauth(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<JsonValue, ManagementError> {
    let body = serde_json::to_vec(&json!({
        "client_id": CLAUDE_OAUTH_CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
    }))
    .map_err(storage_err)?;
    let response = client
        .post(CLAUDE_OAUTH_TOKEN_URL)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|_| ManagementError::Storage("claude oauth refresh request failed".into()))?;
    oauth_response_json(response, "claude").await
}

async fn discover_xai_token_endpoint(client: &reqwest::Client) -> Result<String, ManagementError> {
    let response = client
        .get(XAI_OAUTH_DISCOVERY_URL)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|_| ManagementError::Storage("xai oauth discovery request failed".into()))?;
    let payload = oauth_response_json(response, "xai discovery").await?;
    let endpoint = json_str(&payload, &["token_endpoint"])
        .map(|endpoint| endpoint.trim().to_string())
        .filter(|endpoint| !endpoint.is_empty())
        .ok_or_else(|| {
            ManagementError::Storage("xai oauth discovery missing token_endpoint".into())
        })?;
    validate_xai_oauth_endpoint(&endpoint)
}

pub(crate) fn validate_xai_oauth_endpoint(raw: &str) -> Result<String, ManagementError> {
    let endpoint = raw.trim();
    let parsed = reqwest::Url::parse(endpoint)
        .map_err(|_| ManagementError::InvalidRequest("xai oauth token_endpoint is invalid"))?;
    if parsed.scheme() != "https" {
        return Err(ManagementError::InvalidRequest(
            "xai oauth token_endpoint must use https",
        ));
    }
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if host != "x.ai" && !host.ends_with(".x.ai") {
        return Err(ManagementError::InvalidRequest(
            "xai oauth token_endpoint must be hosted on x.ai",
        ));
    }
    Ok(endpoint.to_string())
}

async fn refresh_xai_oauth(
    client: &reqwest::Client,
    token_endpoint: &str,
    refresh_token: &str,
) -> Result<JsonValue, ManagementError> {
    let body = format!(
        "grant_type=refresh_token&client_id={}&refresh_token={}",
        percent_encode(XAI_OAUTH_CLIENT_ID),
        percent_encode(refresh_token)
    );
    let response = client
        .post(token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|_| ManagementError::Storage("xai oauth refresh request failed".into()))?;
    oauth_response_json(response, "xai").await
}

async fn oauth_response_json(
    response: reqwest::Response,
    provider: &'static str,
) -> Result<JsonValue, ManagementError> {
    let status = response.status();
    if !status.is_success() {
        return Err(ManagementError::Storage(format!(
            "{provider} oauth request failed: status={status}"
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| ManagementError::Storage(format!("{provider} oauth response read failed")))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| ManagementError::Storage(format!("{provider} oauth response is invalid")))
}

async fn response_json(response: reqwest::Response) -> Result<JsonValue, ManagementError> {
    let status = response.status();
    let bytes = response.bytes().await.map_err(storage_err)?;
    if !status.is_success() {
        let message = serde_json::from_slice::<JsonValue>(&bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|error| error.get("message").or(Some(error)))
                    .and_then(JsonValue::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| status.to_string());
        return Err(ManagementError::Storage(message));
    }
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes).map_err(storage_err)
}

fn channel_base_url(channel: &ChannelRecord) -> String {
    channel
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or(match channel.channel_type {
            CHANNEL_TYPE_OLLAMA => "http://localhost:11434",
            _ => "https://api.openai.com",
        })
        .trim_end_matches('/')
        .to_string()
}

fn codex_wham_base_url(channel: &ChannelRecord) -> String {
    let configured = channel
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or(CHATGPT_WEB_BASE_URL)
        .trim_end_matches('/');
    configured
        .strip_suffix("/backend-api/codex")
        .or_else(|| configured.strip_suffix("/backend-api"))
        .unwrap_or(configured)
        .to_string()
}

fn first_key(key: &str) -> String {
    key.trim()
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn is_multi_key(key: &str) -> bool {
    channel_keys(key).len() > 1
}

fn json_str(value: &JsonValue, path: &[&str]) -> Option<String> {
    json_at(value, path)?.as_str().map(str::to_string)
}

fn json_i64(value: &JsonValue, path: &[&str]) -> Option<i64> {
    json_i64_value(json_at(value, path)?)
}

fn json_i64_value(value: &JsonValue) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().map(|value| value as i64))
        .or_else(|| value.as_str()?.parse().ok())
}

fn json_at<'a>(value: &'a JsonValue, path: &[&str]) -> Option<&'a JsonValue> {
    path.iter().try_fold(value, |value, key| value.get(key))
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn storage_err(err: impl std::fmt::Display) -> ManagementError {
    ManagementError::Storage(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use halolake_control_plane::ManagementData;

    #[test]
    fn previews_long_keys() {
        assert_eq!(key_preview("1234567890abcdef"), "1234567890...");
        assert_eq!(key_preview("short"), "short");
    }

    #[test]
    fn reindexes_multi_key_state_after_delete() {
        let mut state = MultiKeyState::default();
        state.status.insert(1, 2);
        state.status.insert(2, 3);
        let next = reindex_multi_key_state(&state, 3, |idx| idx != 1);
        assert_eq!(next.status.get(&1), Some(&3));
        assert!(!next.status.contains_key(&0));
    }

    #[test]
    fn codex_wham_base_strips_codex_api_suffix() {
        let mut channel = test_channel();
        assert_eq!(codex_wham_base_url(&channel), CHATGPT_WEB_BASE_URL);
        channel.base_url = Some("https://chatgpt.com/backend-api/codex/".into());
        assert_eq!(codex_wham_base_url(&channel), CHATGPT_WEB_BASE_URL);
        channel.base_url = Some("http://127.0.0.1:1234/test".into());
        assert_eq!(codex_wham_base_url(&channel), "http://127.0.0.1:1234/test");
    }

    #[test]
    fn imported_oauth_refresh_requires_oauth_setting_and_refresh_token() {
        let mut channel = test_channel();
        channel.channel_type = CHANNEL_TYPE_CLAUDE;
        channel.setting = Some(r#"{"auth_kind":"oauth","refresh_token":"refresh-old"}"#.into());
        let credential = imported_oauth_refresh_credential(&channel).expect("oauth credential");
        assert_eq!(credential.refresh_token, "refresh-old");

        channel.setting = Some(r#"{"auth_kind":"api_key","refresh_token":"refresh-old"}"#.into());
        assert!(imported_oauth_refresh_credential(&channel).is_none());
        channel.setting =
            Some(r#"{"auth_kind":"setup-token","refresh_token":"refresh-old"}"#.into());
        assert!(imported_oauth_refresh_credential(&channel).is_some());
        channel.setting = Some(r#"{"auth_kind":"oauth","refresh_token":""}"#.into());
        assert!(imported_oauth_refresh_credential(&channel).is_none());
    }

    #[test]
    fn oauth_refresh_update_preserves_unrotated_refresh_token() {
        let mut channel = test_channel();
        channel.channel_type = CHANNEL_TYPE_XAI;
        channel.setting = Some(
            r#"{"auth_kind":"oauth","refresh_token":"refresh-old","id_token":"id-old"}"#.into(),
        );
        let mut credential = imported_oauth_refresh_credential(&channel).expect("oauth credential");
        let tokens = parse_oauth_refresh_payload(
            &json!({
                "access_token": "access-new",
                "id_token": "id-new",
                "expires_in": 3600
            }),
            &credential.refresh_token,
        )
        .expect("token payload");

        apply_oauth_refresh(
            &mut channel,
            &mut credential.setting,
            tokens,
            Some("https://auth.x.ai/oauth2/token"),
            1_700_000_000,
        )
        .expect("refresh should apply");

        let setting: JsonValue =
            serde_json::from_str(channel.setting.as_deref().expect("setting")).expect("json");
        assert_eq!(channel.key, "access-new");
        assert_eq!(setting["refresh_token"], "refresh-old");
        assert_eq!(setting["id_token"], "id-new");
        assert_eq!(setting["expired"], "1700003600");
        assert_eq!(setting["last_refresh"], "1700000000");
        assert_eq!(setting["token_endpoint"], "https://auth.x.ai/oauth2/token");
    }

    #[test]
    fn oauth_refresh_lock_reuses_active_lock_and_drops_stale_entry() {
        let first = oauth_refresh_lock(u64::MAX - 1);
        let second = oauth_refresh_lock(u64::MAX - 1);
        assert!(Arc::ptr_eq(&first, &second));
        let stale = Arc::downgrade(&first);
        drop(first);
        drop(second);
        assert!(stale.upgrade().is_none());

        let replacement = oauth_refresh_lock(u64::MAX - 1);
        assert_eq!(Arc::strong_count(&replacement), 1);
    }

    #[test]
    fn recent_oauth_refresh_window_accepts_strings_and_numbers() {
        let now = 1_700_000_000;
        assert!(last_refresh_is_recent(Some("1699999970"), now));
        assert!(!last_refresh_is_recent(Some("1699999969"), now));
        assert!(!last_refresh_is_recent(Some("not-a-timestamp"), now));
        assert!(setting_last_refresh_is_recent(
            &json!({ "last_refresh": now }),
            now
        ));
        assert!(!setting_last_refresh_is_recent(
            &json!({ "last_refresh": now + 31 }),
            now
        ));
    }

    #[test]
    fn xai_oauth_endpoint_is_https_and_scoped_to_xai_hosts() {
        assert_eq!(
            validate_xai_oauth_endpoint(" https://auth.x.ai/oauth2/token ")
                .expect("official endpoint"),
            "https://auth.x.ai/oauth2/token"
        );
        assert_eq!(
            validate_xai_oauth_endpoint("https://x.ai/oauth/token").expect("apex endpoint"),
            "https://x.ai/oauth/token"
        );

        for endpoint in [
            "http://auth.x.ai/oauth2/token",
            "https://evil.example/oauth/token",
            "https://auth.x.ai.evil.example/oauth/token",
            "not-a-url",
        ] {
            assert!(
                validate_xai_oauth_endpoint(endpoint).is_err(),
                "endpoint must fail closed: {endpoint}"
            );
        }
    }

    #[tokio::test]
    async fn codex_feedback_refresh_rejects_plain_or_non_refreshable_credentials() {
        let mut plain = test_channel();
        plain.id = u64::MAX - 3;
        plain.key = "plain-api-key".to_string();
        let mut access_only = test_channel();
        access_only.id = u64::MAX - 2;
        access_only.key = r#"{"type":"codex","access_token":"access-only"}"#.to_string();
        let service = ChannelSpecialService::new(
            ManagementStore::memory(ManagementData::new(
                1,
                Vec::new(),
                Vec::new(),
                vec![plain, access_only],
                Vec::new(),
            )),
            ProxyStore::memory(),
        );

        assert!(
            !service
                .refresh_imported_oauth_channel(u64::MAX - 3)
                .await
                .expect("plain Codex key should be ignored")
        );
        assert!(
            !service
                .refresh_imported_oauth_channel(u64::MAX - 2)
                .await
                .expect("access-only Codex key should be ignored")
        );
    }

    fn test_channel() -> ChannelRecord {
        ChannelRecord {
            id:                   1,
            snapshot_id:          None,
            channel_type:         CHANNEL_TYPE_CODEX,
            key:                  String::new(),
            status:               1,
            name:                 "test".into(),
            weight:               Some(1),
            created_time:         0,
            test_time:            0,
            response_time:        0,
            base_url:             None,
            balance:              0.0,
            balance_updated_time: 0,
            models:               String::new(),
            group:                "default".into(),
            used_quota:           0,
            model_mapping:        None,
            priority:             None,
            auto_ban:             None,
            tag:                  None,
            setting:              None,
            param_override:       None,
            header_override:      None,
            remark:               None,
            proxy_id:             None,
        }
    }
}
