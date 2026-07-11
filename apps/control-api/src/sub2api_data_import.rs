//! Import sub2api backup JSON (`type: sub2api-data` / legacy `sub2api-bundle`).
//!
//! Creates proxies and channels from exported accounts. Groups are not bound
//! (same as sub2api default `skip_default_group_bind`). Existing proxies are
//! reused by fingerprint; accounts always create new channels.

use crate::{
    codex_auth_import::{
        CHANNEL_TYPE_CODEX, CodexOAuthKey, codex_key_to_json, parse_flexible_codex_key,
    },
    proxy::{CreateProxyRequest, ListProxiesRequest, ProxyRecord, ProxyStore, UpdateProxyRequest},
    storage::ManagementStore,
};
use halolake_control_plane::{CreateChannelRequest, ManagementError};
use halolake_domain::{ChannelRecord, STATUS_ENABLED};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use service_async::Service;
use std::collections::HashMap;

const DATA_TYPE: &str = "sub2api-data";
const LEGACY_DATA_TYPE: &str = "sub2api-bundle";
const DATA_VERSION: i32 = 1;

const CHANNEL_TYPE_OPENAI: i32 = 1;
const CHANNEL_TYPE_ANTHROPIC: i32 = 14;
const CHANNEL_TYPE_GEMINI: i32 = 24;
const PROXY_STATUS_ENABLED: i32 = 1;
const PROXY_STATUS_DISABLED: i32 = 0;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Sub2apiDataImportRequest {
    /// Full export object or wrapper `{ "data": { ... } }`.
    #[serde(default)]
    pub(crate) data:    Option<DataPayload>,
    /// Raw JSON file contents (string). Takes precedence when `data` is absent.
    #[serde(default)]
    pub(crate) content: String,
    /// Optional default group for created channels (manual binding still expected).
    #[serde(default)]
    pub(crate) group:   Option<String>,
    /// Optional model list applied when account has no model mapping in credentials.
    #[serde(default)]
    pub(crate) models:  Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct DataPayload {
    #[serde(default, rename = "type")]
    pub(crate) data_type:       String,
    #[serde(default)]
    pub(crate) version:         i32,
    #[serde(default)]
    pub(crate) exported_at:     String,
    #[serde(default)]
    pub(crate) proxies:         Vec<DataProxy>,
    #[serde(default)]
    pub(crate) accounts:        Vec<DataAccount>,
    #[serde(default)]
    pub(crate) skipped_shadows: i32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct DataProxy {
    #[serde(default)]
    pub(crate) proxy_key: String,
    #[serde(default)]
    pub(crate) name:      String,
    #[serde(default)]
    pub(crate) protocol:  String,
    #[serde(default)]
    pub(crate) host:      String,
    #[serde(default)]
    pub(crate) port:      i32,
    #[serde(default)]
    pub(crate) username:  String,
    #[serde(default)]
    pub(crate) password:  String,
    #[serde(default)]
    pub(crate) status:    String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct DataAccount {
    #[serde(default)]
    pub(crate) name:                  String,
    #[serde(default)]
    pub(crate) notes:                 Option<String>,
    #[serde(default)]
    pub(crate) platform:              String,
    #[serde(rename = "type", default)]
    pub(crate) account_type:          String,
    #[serde(default)]
    pub(crate) credentials:           JsonMap<String, JsonValue>,
    #[serde(default)]
    pub(crate) extra:                 JsonMap<String, JsonValue>,
    #[serde(default)]
    pub(crate) proxy_key:             Option<String>,
    #[serde(default)]
    pub(crate) concurrency:           i32,
    #[serde(default)]
    pub(crate) priority:              i32,
    #[serde(default)]
    pub(crate) rate_multiplier:       Option<f64>,
    #[serde(default)]
    pub(crate) expires_at:            Option<i64>,
    #[serde(default)]
    pub(crate) auto_pause_on_expired: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DataImportResult {
    pub(crate) proxy_created:   usize,
    pub(crate) proxy_reused:    usize,
    pub(crate) proxy_failed:    usize,
    pub(crate) account_created: usize,
    pub(crate) account_failed:  usize,
    pub(crate) errors:          Vec<DataImportError>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DataImportError {
    pub(crate) kind:      String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) name:      String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) proxy_key: String,
    pub(crate) message:   String,
}

pub(crate) async fn import_sub2api_data(
    management: &ManagementStore,
    proxies: &ProxyStore,
    req: Sub2apiDataImportRequest,
) -> Result<DataImportResult, ManagementError> {
    let payload = resolve_payload(&req)?;
    validate_header(&payload)?;

    let mut result = DataImportResult {
        proxy_created:   0,
        proxy_reused:    0,
        proxy_failed:    0,
        account_created: 0,
        account_failed:  0,
        errors:          Vec::new(),
    };

    let group = req
        .group
        .as_deref()
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .unwrap_or("default")
        .to_string();
    let default_models = req
        .models
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("gpt-5.1,gpt-5,o3,o4-mini")
        .to_string();

    // --- proxies ---
    let existing_proxies = proxies.call(ListProxiesRequest).await.unwrap_or_default();
    let mut proxy_key_to_id: HashMap<String, u64> = HashMap::new();
    let mut existing_by_url: HashMap<String, ProxyRecord> = HashMap::new();
    for proxy in &existing_proxies {
        existing_by_url.insert(proxy.url.clone(), proxy.clone());
        // also index by rebuilt key from url when possible
        if let Some(key) = fingerprint_from_url(&proxy.url) {
            proxy_key_to_id.insert(key, proxy.id);
        }
    }

    for item in &payload.proxies {
        let key = if item.proxy_key.trim().is_empty() {
            build_proxy_key(
                &item.protocol,
                &item.host,
                item.port,
                &item.username,
                &item.password,
            )
        } else {
            item.proxy_key.trim().to_string()
        };

        if let Err(message) = validate_proxy(item) {
            result.proxy_failed = result.proxy_failed.saturating_add(1);
            result.errors.push(DataImportError {
                kind: "proxy".into(),
                name: item.name.clone(),
                proxy_key: key,
                message,
            });
            continue;
        }

        let url = match build_proxy_url(item) {
            Ok(url) => url,
            Err(message) => {
                result.proxy_failed = result.proxy_failed.saturating_add(1);
                result.errors.push(DataImportError {
                    kind: "proxy".into(),
                    name: item.name.clone(),
                    proxy_key: key,
                    message,
                });
                continue;
            }
        };
        let status = normalize_proxy_status(&item.status);

        if let Some(existing) = existing_by_url.get(&url).cloned().or_else(|| {
            proxy_key_to_id
                .get(&key)
                .and_then(|id| existing_proxies.iter().find(|p| p.id == *id).cloned())
        }) {
            result.proxy_reused = result.proxy_reused.saturating_add(1);
            proxy_key_to_id.insert(key, existing.id);
            if existing.status != status {
                let mut updated = existing.clone();
                updated.status = status;
                let _ = proxies.call(UpdateProxyRequest { proxy: updated }).await;
            }
            continue;
        }

        let name = if item.name.trim().is_empty() {
            "imported-proxy".to_string()
        } else {
            item.name.trim().to_string()
        };
        match proxies
            .call(CreateProxyRequest {
                proxy: ProxyRecord {
                    id: 0,
                    name,
                    url: url.clone(),
                    status,
                    remark: "imported from sub2api-data".into(),
                },
            })
            .await
        {
            Ok(created) => {
                result.proxy_created = result.proxy_created.saturating_add(1);
                proxy_key_to_id.insert(key, created.id);
                existing_by_url.insert(created.url.clone(), created);
            }
            Err(err) => {
                result.proxy_failed = result.proxy_failed.saturating_add(1);
                result.errors.push(DataImportError {
                    kind:      "proxy".into(),
                    name:      item.name.clone(),
                    proxy_key: key,
                    message:   err.to_string(),
                });
            }
        }
    }

    // --- accounts → channels ---
    for item in &payload.accounts {
        if let Err(message) = validate_account(item) {
            result.account_failed = result.account_failed.saturating_add(1);
            result.errors.push(DataImportError {
                kind: "account".into(),
                name: item.name.clone(),
                proxy_key: String::new(),
                message,
            });
            continue;
        }

        let mut proxy_id = None;
        if let Some(pk) = item
            .proxy_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match proxy_key_to_id.get(pk) {
                Some(id) => proxy_id = Some(*id),
                None => {
                    result.account_failed = result.account_failed.saturating_add(1);
                    result.errors.push(DataImportError {
                        kind:      "account".into(),
                        name:      item.name.clone(),
                        proxy_key: pk.to_string(),
                        message:   "proxy_key not found".into(),
                    });
                    continue;
                }
            }
        }

        let mapped = match map_account_to_channel(item, &group, &default_models, proxy_id) {
            Ok(channel) => channel,
            Err(message) => {
                result.account_failed = result.account_failed.saturating_add(1);
                result.errors.push(DataImportError {
                    kind: "account".into(),
                    name: item.name.clone(),
                    proxy_key: item.proxy_key.clone().unwrap_or_default(),
                    message,
                });
                continue;
            }
        };

        match management
            .call(CreateChannelRequest { channel: mapped })
            .await
        {
            Ok(_) => {
                result.account_created = result.account_created.saturating_add(1);
            }
            Err(err) => {
                result.account_failed = result.account_failed.saturating_add(1);
                result.errors.push(DataImportError {
                    kind:      "account".into(),
                    name:      item.name.clone(),
                    proxy_key: item.proxy_key.clone().unwrap_or_default(),
                    message:   err.to_string(),
                });
            }
        }
    }

    Ok(result)
}

fn resolve_payload(req: &Sub2apiDataImportRequest) -> Result<DataPayload, ManagementError> {
    if let Some(data) = &req.data {
        return Ok(data.clone());
    }
    let content = req.content.trim();
    if content.is_empty() {
        return Err(ManagementError::InvalidRequest(
            "provide data object or content JSON string",
        ));
    }
    let value: JsonValue = serde_json::from_str(content)
        .map_err(|_| ManagementError::InvalidRequest("invalid JSON content"))?;
    // Accept either raw payload or { "data": payload }
    if let Some(inner) = value.get("data") {
        return serde_json::from_value(inner.clone())
            .map_err(|_| ManagementError::InvalidRequest("invalid data payload"));
    }
    serde_json::from_value(value)
        .map_err(|_| ManagementError::InvalidRequest("invalid data payload"))
}

fn validate_header(payload: &DataPayload) -> Result<(), ManagementError> {
    if !payload.data_type.is_empty()
        && payload.data_type != DATA_TYPE
        && payload.data_type != LEGACY_DATA_TYPE
    {
        return Err(ManagementError::InvalidRequest(
            "unsupported data type (expected sub2api-data)",
        ));
    }
    if payload.version != 0 && payload.version != DATA_VERSION {
        return Err(ManagementError::InvalidRequest(
            "unsupported data version (expected 1)",
        ));
    }
    // proxies/accounts may be empty arrays but must be present in JSON;
    // serde Default gives empty vec if missing — allow empty for proxy-only or account-only.
    Ok(())
}

fn validate_proxy(item: &DataProxy) -> Result<(), String> {
    if item.protocol.trim().is_empty() {
        return Err("proxy protocol is required".into());
    }
    if item.host.trim().is_empty() {
        return Err("proxy host is required".into());
    }
    if item.port <= 0 || item.port > 65535 {
        return Err("proxy port is invalid".into());
    }
    match item.protocol.trim().to_ascii_lowercase().as_str() {
        "http" | "https" | "socks5" | "socks5h" => {}
        other => return Err(format!("proxy protocol is invalid: {other}")),
    }
    Ok(())
}

fn validate_account(item: &DataAccount) -> Result<(), String> {
    if item.name.trim().is_empty() {
        return Err("account name is required".into());
    }
    if item.platform.trim().is_empty() {
        return Err("account platform is required".into());
    }
    if item.account_type.trim().is_empty() {
        return Err("account type is required".into());
    }
    if item.credentials.is_empty() {
        return Err("account credentials is required".into());
    }
    match item.account_type.trim().to_ascii_lowercase().as_str() {
        "oauth" | "setup-token" | "setup_token" | "apikey" | "api_key" | "api-key" | "upstream" => {
        }
        other => return Err(format!("account type is invalid: {other}")),
    }
    if item.concurrency < 0 {
        return Err("concurrency must be >= 0".into());
    }
    if item.priority < 0 {
        return Err("priority must be >= 0".into());
    }
    Ok(())
}

fn build_proxy_key(
    protocol: &str,
    host: &str,
    port: i32,
    username: &str,
    password: &str,
) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        protocol.trim(),
        host.trim(),
        port,
        username.trim(),
        password.trim()
    )
}

fn build_proxy_url(item: &DataProxy) -> Result<String, String> {
    let mut protocol = item.protocol.trim().to_ascii_lowercase();
    if protocol == "socks5" {
        protocol = "socks5h".to_string();
    }
    let host = item.host.trim();
    if host.is_empty() {
        return Err("proxy host is required".into());
    }
    let port = item.port;
    let user = item.username.trim();
    let pass = item.password.trim();
    let auth = if user.is_empty() && pass.is_empty() {
        String::new()
    } else if pass.is_empty() {
        format!("{user}@")
    } else {
        format!("{user}:{pass}@")
    };
    Ok(format!("{protocol}://{auth}{host}:{port}"))
}

fn fingerprint_from_url(url: &str) -> Option<String> {
    let uri: http::Uri = url.parse().ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let host = uri.host()?;
    let port = uri.port_u16().unwrap_or(match scheme.as_str() {
        "https" => 443,
        "socks5" | "socks5h" => 1080,
        _ => 80,
    });
    let (user, pass) = uri
        .authority()
        .and_then(|a| {
            let s = a.as_str();
            let (userinfo, _) = s.split_once('@')?;
            if let Some((u, p)) = userinfo.split_once(':') {
                Some((u.to_string(), p.to_string()))
            } else {
                Some((userinfo.to_string(), String::new()))
            }
        })
        .unwrap_or_default();
    let protocol = if scheme == "socks5h" {
        "socks5"
    } else {
        scheme.as_str()
    };
    Some(build_proxy_key(protocol, host, port as i32, &user, &pass))
}

fn normalize_proxy_status(status: &str) -> i32 {
    match status.trim().to_ascii_lowercase().as_str() {
        "" | "active" | "enabled" | "1" => PROXY_STATUS_ENABLED,
        "inactive" | "disabled" | "expired" | "0" => PROXY_STATUS_DISABLED,
        _ => PROXY_STATUS_ENABLED,
    }
}

fn map_account_to_channel(
    item: &DataAccount,
    group: &str,
    default_models: &str,
    proxy_id: Option<u64>,
) -> Result<ChannelRecord, String> {
    let platform = item.platform.trim().to_ascii_lowercase();
    let account_type = item.account_type.trim().to_ascii_lowercase();
    let (channel_type, key) = match platform.as_str() {
        "openai" => map_openai_account(&account_type, &item.credentials)?,
        "anthropic" | "claude" => map_anthropic_account(&account_type, &item.credentials)?,
        "gemini" | "google" => map_gemini_account(&account_type, &item.credentials)?,
        other => {
            return Err(format!(
                "unsupported platform for channel import: {other} (supported: openai, anthropic, \
                 gemini)"
            ));
        }
    };

    let models = cred_string(&item.credentials, "models")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_models.to_string());
    let base_url = cred_string(&item.credentials, "base_url")
        .or_else(|| cred_string(&item.extra, "base_url"))
        .filter(|s| !s.is_empty());

    let mut setting_map = JsonMap::new();
    if let Some(proxy_url) = cred_string(&item.credentials, "proxy") {
        setting_map.insert("proxy".into(), JsonValue::String(proxy_url));
    }
    // Preserve raw credentials meta for debugging (no secrets beyond key field).
    setting_map.insert(
        "import_source".into(),
        JsonValue::String("sub2api-data".into()),
    );
    setting_map.insert(
        "import_platform".into(),
        JsonValue::String(platform.clone()),
    );
    setting_map.insert(
        "import_account_type".into(),
        JsonValue::String(account_type.clone()),
    );
    if item.concurrency > 0 {
        setting_map.insert("concurrency".into(), json!(item.concurrency));
    }

    let priority = if item.priority > 0 {
        Some(i64::from(item.priority))
    } else {
        Some(0)
    };

    Ok(ChannelRecord {
        id: 0,
        snapshot_id: None,
        channel_type,
        key,
        status: STATUS_ENABLED,
        name: item.name.trim().to_string(),
        weight: Some(1),
        created_time: now_unix(),
        test_time: 0,
        response_time: 0,
        base_url,
        balance: 0.0,
        balance_updated_time: 0,
        models,
        group: group.to_string(),
        used_quota: 0,
        model_mapping: None,
        priority,
        auto_ban: Some(1),
        tag: None,
        setting: Some(serde_json::to_string(&JsonValue::Object(setting_map)).unwrap_or_default()),
        param_override: None,
        header_override: None,
        remark: item.notes.clone().or_else(|| {
            Some(format!(
                "imported from sub2api-data ({platform}/{account_type})"
            ))
        }),
        proxy_id,
    })
}

fn map_openai_account(
    account_type: &str,
    credentials: &JsonMap<String, JsonValue>,
) -> Result<(i32, String), String> {
    match account_type {
        "oauth" | "setup-token" | "setup_token" => {
            let key = credentials_to_codex_key(credentials)?;
            let json = codex_key_to_json(&key).map_err(|e| e.to_string())?;
            Ok((CHANNEL_TYPE_CODEX, json))
        }
        "apikey" | "api_key" | "api-key" | "upstream" => {
            let api_key = cred_string(credentials, "api_key")
                .or_else(|| cred_string(credentials, "access_token"))
                .or_else(|| cred_string(credentials, "token"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "openai api_key credentials missing api_key".to_string())?;
            Ok((CHANNEL_TYPE_OPENAI, api_key))
        }
        other => Err(format!("unsupported openai account type: {other}")),
    }
}

fn map_anthropic_account(
    account_type: &str,
    credentials: &JsonMap<String, JsonValue>,
) -> Result<(i32, String), String> {
    let key = cred_string(credentials, "api_key")
        .or_else(|| cred_string(credentials, "access_token"))
        .or_else(|| cred_string(credentials, "token"))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "anthropic credentials missing api_key/access_token".to_string())?;
    let _ = account_type;
    Ok((CHANNEL_TYPE_ANTHROPIC, key))
}

fn map_gemini_account(
    account_type: &str,
    credentials: &JsonMap<String, JsonValue>,
) -> Result<(i32, String), String> {
    let key = cred_string(credentials, "api_key")
        .or_else(|| cred_string(credentials, "access_token"))
        .or_else(|| cred_string(credentials, "token"))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "gemini credentials missing api_key/access_token".to_string())?;
    let _ = account_type;
    Ok((CHANNEL_TYPE_GEMINI, key))
}

fn credentials_to_codex_key(
    credentials: &JsonMap<String, JsonValue>,
) -> Result<CodexOAuthKey, String> {
    // Prefer reusing flexible parser on a JSON object.
    let as_value = JsonValue::Object(credentials.clone());
    let raw = serde_json::to_string(&as_value).map_err(|e| e.to_string())?;
    match parse_flexible_codex_key(&raw) {
        Ok(key) => Ok(key),
        Err(_) => {
            // Minimal fallback: only access_token present.
            let access = cred_string(credentials, "access_token")
                .or_else(|| cred_string(credentials, "accessToken"))
                .or_else(|| cred_string(credentials, "token"))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "oauth credentials missing access_token".to_string())?;
            Ok(CodexOAuthKey {
                id_token:      cred_string(credentials, "id_token")
                    .or_else(|| cred_string(credentials, "idToken")),
                access_token:  Some(access),
                refresh_token: cred_string(credentials, "refresh_token")
                    .or_else(|| cred_string(credentials, "refreshToken")),
                account_id:    cred_string(credentials, "chatgpt_account_id")
                    .or_else(|| cred_string(credentials, "account_id"))
                    .or_else(|| cred_string(credentials, "accountId")),
                last_refresh:  None,
                email:         cred_string(credentials, "email"),
                key_type:      Some("codex".into()),
                expired:       cred_string(credentials, "expires_at")
                    .or_else(|| cred_string(credentials, "expired")),
            })
        }
    }
}

fn cred_string(map: &JsonMap<String, JsonValue>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(|v| match v {
            JsonValue::String(s) => Some(s.trim().to_string()),
            JsonValue::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .filter(|s| !s.is_empty())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

use serde_json::json;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_proxy_url_and_upgrades_socks5() {
        let item = DataProxy {
            protocol: "socks5".into(),
            host: "1.2.3.4".into(),
            port: 1080,
            username: "u".into(),
            password: "p".into(),
            ..DataProxy::default()
        };
        assert_eq!(
            build_proxy_url(&item).unwrap(),
            "socks5h://u:p@1.2.3.4:1080"
        );
    }

    #[test]
    fn maps_openai_oauth_to_codex_channel() {
        let mut creds = JsonMap::new();
        creds.insert("access_token".into(), json!("at-1"));
        creds.insert("refresh_token".into(), json!("rt-1"));
        creds.insert("chatgpt_account_id".into(), json!("acct-1"));
        let item = DataAccount {
            name: "acc".into(),
            platform: "openai".into(),
            account_type: "oauth".into(),
            credentials: creds,
            priority: 50,
            ..DataAccount::default()
        };
        let channel = map_account_to_channel(&item, "default", "gpt-5", None).unwrap();
        assert_eq!(channel.channel_type, CHANNEL_TYPE_CODEX);
        assert!(channel.key.contains("at-1"));
        assert!(channel.key.contains("rt-1"));
    }

    #[test]
    fn maps_openai_apikey_to_type1() {
        let mut creds = JsonMap::new();
        creds.insert("api_key".into(), json!("sk-test"));
        let item = DataAccount {
            name: "api".into(),
            platform: "openai".into(),
            account_type: "apikey".into(),
            credentials: creds,
            ..DataAccount::default()
        };
        let channel = map_account_to_channel(&item, "default", "gpt-5", None).unwrap();
        assert_eq!(channel.channel_type, CHANNEL_TYPE_OPENAI);
        assert_eq!(channel.key, "sk-test");
    }

    #[test]
    fn parse_payload_from_wrapper_or_raw() {
        let raw = r#"{"type":"sub2api-data","version":1,"proxies":[],"accounts":[]}"#;
        let req = Sub2apiDataImportRequest {
            data:    None,
            content: raw.into(),
            group:   None,
            models:  None,
        };
        let payload = resolve_payload(&req).unwrap();
        validate_header(&payload).unwrap();
        assert!(payload.proxies.is_empty());
    }
}
