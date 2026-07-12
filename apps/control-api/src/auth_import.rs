//! Unified auth credential import for Halolake channels.
//!
//! Accepts multiple upstream ecosystems and normalizes them into channels:
//!
//! | Source | Typical input | Result |
//! |--------|---------------|--------|
//! | **CLIProxyAPI** | `{ "type":"codex"\|"claude"\|"gemini"\|"xai", "access_token", ... }.json` | type 57 / 14 / 24 / 48 channel |
//! | **sub2api Codex session** | nested `tokens.*` or raw JWT paste | type 57 Codex |
//! | **sub2api-data export** | `{ "type":"sub2api-data", "proxies", "accounts" }` | proxies + channels |
//!
//! Entry points:
//! - JSON: [`AuthImportRequest`] via `/api/channel/import/auth`
//! - Multipart files: same handler with form fields

use crate::{
    channel_probe::{ChannelProbeService, FetchModelsRequest},
    codex_auth_import::{
        self, CHANNEL_TYPE_CODEX, CodexAuthImportItem, CodexAuthImportMessage,
        CodexAuthImportRequest, CodexAuthImportResult, CodexOAuthKey, codex_key_to_json,
        collect_entries, find_existing_channel_id, parse_flexible_codex_key,
    },
    proxy::ProxyStore,
    storage::ManagementStore,
    sub2api_data_import::{self, DataImportResult, Sub2apiDataImportRequest},
};
use halolake_control_plane::{CreateChannelRequest, ManagementError, UpdateChannelRequest};
use halolake_domain::{ChannelRecord, STATUS_AUTO_DISABLED, STATUS_ENABLED};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use service_async::Service;
use std::collections::HashMap;

const CHANNEL_TYPE_ANTHROPIC: i32 = 14;
const CHANNEL_TYPE_GEMINI: i32 = 24;
const CHANNEL_TYPE_XAI: i32 = 48;
const XAI_DEFAULT_API_BASE: &str = "https://api.x.ai";
const XAI_DEFAULT_CLI_CHAT_BASE: &str = "https://cli-chat-proxy.grok.com";
const XAI_DEFAULT_MODELS: &str = "grok-4,grok-4-latest,grok-3,grok-3-mini,grok-2,grok-2-latest";
/// Keep in sync with CLIProxyAPI `xaiClientVersionValue` / chat-proxy identity headers.
const XAI_TOKEN_AUTH_HEADER: &str = "X-XAI-Token-Auth";
const XAI_TOKEN_AUTH_VALUE: &str = "xai-grok-cli";
const XAI_CLIENT_VERSION_HEADER: &str = "x-grok-client-version";
const XAI_CLIENT_VERSION_VALUE: &str = "0.2.93";

/// Auto-detecting import request (JSON body).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AuthImportRequest {
    /// Format hint: `auto` (default), `cliproxy`, `codex-session`, `sub2api-data`.
    #[serde(default = "default_format")]
    pub(crate) format:          String,
    /// Single file / paste content.
    #[serde(default)]
    pub(crate) content:         String,
    /// Multiple file contents (batch API upload).
    #[serde(default)]
    pub(crate) contents:        Vec<String>,
    /// Optional original filenames (same order as `contents`); used for labels.
    #[serde(default)]
    pub(crate) filenames:       Vec<String>,
    #[serde(default)]
    pub(crate) name:            String,
    #[serde(default)]
    pub(crate) group:           Option<String>,
    #[serde(default)]
    pub(crate) models:          Option<String>,
    #[serde(default)]
    pub(crate) base_url:        Option<String>,
    #[serde(default)]
    pub(crate) proxy_id:        Option<u64>,
    #[serde(default = "default_true")]
    pub(crate) update_existing: bool,
    /// For sub2api-data: optional structured payload instead of `content`.
    #[serde(default)]
    pub(crate) data:            Option<sub2api_data_import::DataPayload>,
}

fn default_format() -> String {
    "auto".into()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthImportResult {
    pub(crate) format:       String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) channels:     Option<CodexAuthImportResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) data:         Option<DataImportResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) file_results: Vec<AuthFileResult>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthFileResult {
    pub(crate) name:       String,
    pub(crate) format:     String,
    pub(crate) ok:         bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) message:    String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) channel_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) created:    Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) updated:    Option<usize>,
}

/// Detect content kind for a single blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectedFormat {
    Sub2apiData,
    CliProxy,
    CodexSession,
}

pub(crate) async fn import_auth(
    management: &ManagementStore,
    proxies: &ProxyStore,
    req: AuthImportRequest,
) -> Result<AuthImportResult, ManagementError> {
    let hint = req.format.trim().to_ascii_lowercase();
    let has_data = req.data.is_some();
    let mut blobs: Vec<(String, String)> = Vec::new();

    if !req.content.trim().is_empty() {
        let name = req
            .filenames
            .first()
            .cloned()
            .unwrap_or_else(|| "content".into());
        blobs.push((name, req.content.clone()));
    }
    for (i, content) in req.contents.iter().enumerate() {
        if content.trim().is_empty() {
            continue;
        }
        let name = req
            .filenames
            .get(i)
            .cloned()
            .unwrap_or_else(|| format!("file-{i}"));
        blobs.push((name, content.clone()));
    }

    if has_data && blobs.is_empty() {
        // structured sub2api-data only
        let data_req = Sub2apiDataImportRequest {
            data:    req.data.clone(),
            content: String::new(),
            group:   req.group.clone(),
            models:  req.models.clone(),
        };
        let data = sub2api_data_import::import_sub2api_data(management, proxies, data_req).await?;
        return Ok(AuthImportResult {
            format:       "sub2api-data".into(),
            channels:     None,
            data:         Some(data),
            file_results: Vec::new(),
        });
    }

    if blobs.is_empty() {
        return Err(ManagementError::InvalidRequest(
            "provide content, contents[], or data for auth import",
        ));
    }

    // Single-blob path with explicit or detected format.
    if blobs.len() == 1 && (hint == "sub2api-data" || hint == "auto" || hint.is_empty()) {
        let (_, content) = &blobs[0];
        let detected = if hint == "sub2api-data" {
            DetectedFormat::Sub2apiData
        } else {
            detect_format(content)
        };
        if detected == DetectedFormat::Sub2apiData {
            let data_req = Sub2apiDataImportRequest {
                data:    req.data.clone(),
                content: content.clone(),
                group:   req.group.clone(),
                models:  req.models.clone(),
            };
            let data =
                sub2api_data_import::import_sub2api_data(management, proxies, data_req).await?;
            let file_result = AuthFileResult {
                name:       blobs[0].0.clone(),
                format:     "sub2api-data".into(),
                ok:         data.account_failed == 0 && data.proxy_failed == 0,
                message:    format!(
                    "proxies +{}/reuse {}, accounts +{}/fail {}",
                    data.proxy_created,
                    data.proxy_reused,
                    data.account_created,
                    data.account_failed
                ),
                channel_id: None,
                created:    Some(data.account_created),
                updated:    None,
            };
            return Ok(AuthImportResult {
                format:       "sub2api-data".into(),
                channels:     None,
                data:         Some(data),
                file_results: vec![file_result],
            });
        }
    }

    // Treat remaining as per-file auth credentials (CLIProxyAPI and/or codex session).
    let mut aggregate = CodexAuthImportResult {
        total:    0,
        created:  0,
        updated:  0,
        skipped:  0,
        failed:   0,
        items:    Vec::new(),
        warnings: Vec::new(),
        errors:   Vec::new(),
    };
    let mut file_results = Vec::new();
    let group = req
        .group
        .as_deref()
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .unwrap_or("default")
        .to_string();
    let models = req
        .models
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("gpt-5.1,gpt-5,o3,o4-mini")
        .to_string();
    let base_url = req
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string);
    let name_base = req.name.trim();

    let existing = management.current_data()?.channels;
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut index = 0usize;

    for (file_name, content) in &blobs {
        let detected = match hint.as_str() {
            "cliproxy" | "cliproxyapi" | "cli-proxy" => DetectedFormat::CliProxy,
            "codex-session" | "codex" | "sub2api-session" => DetectedFormat::CodexSession,
            _ => detect_format(content),
        };

        if detected == DetectedFormat::Sub2apiData {
            // Nested export inside multi-file batch
            match sub2api_data_import::import_sub2api_data(
                management,
                proxies,
                Sub2apiDataImportRequest {
                    data:    None,
                    content: content.clone(),
                    group:   Some(group.clone()),
                    models:  Some(models.clone()),
                },
            )
            .await
            {
                Ok(data) => {
                    aggregate.created = aggregate.created.saturating_add(data.account_created);
                    aggregate.failed = aggregate.failed.saturating_add(data.account_failed);
                    file_results.push(AuthFileResult {
                        name:       file_name.clone(),
                        format:     "sub2api-data".into(),
                        ok:         data.account_failed == 0 && data.proxy_failed == 0,
                        message:    format!(
                            "proxies +{}/{}, accounts +{}/{}",
                            data.proxy_created,
                            data.proxy_reused,
                            data.account_created,
                            data.account_failed
                        ),
                        channel_id: None,
                        created:    Some(data.account_created),
                        updated:    None,
                    });
                }
                Err(err) => {
                    aggregate.failed = aggregate.failed.saturating_add(1);
                    file_results.push(AuthFileResult {
                        name:       file_name.clone(),
                        format:     "sub2api-data".into(),
                        ok:         false,
                        message:    err.to_string(),
                        channel_id: None,
                        created:    None,
                        updated:    None,
                    });
                }
            }
            continue;
        }

        // CLIProxy typed files may map to non-Codex channels.
        if detected == DetectedFormat::CliProxy {
            match import_cliproxy_file(
                management,
                content,
                file_name,
                &group,
                &models,
                base_url.as_deref(),
                req.proxy_id,
                req.update_existing,
                name_base,
                &existing,
                &mut seen,
                &mut index,
                &mut aggregate,
            )
            .await
            {
                Ok(fr) => file_results.push(fr),
                Err(err) => {
                    aggregate.failed = aggregate.failed.saturating_add(1);
                    file_results.push(AuthFileResult {
                        name:       file_name.clone(),
                        format:     "cliproxy".into(),
                        ok:         false,
                        message:    err.to_string(),
                        channel_id: None,
                        created:    None,
                        updated:    None,
                    });
                }
            }
            continue;
        }

        // Codex session / raw token — reuse codex_auth_import pipeline for this blob.
        let codex_req = CodexAuthImportRequest {
            content:         content.clone(),
            contents:        Vec::new(),
            name:            if name_base.is_empty() {
                String::new()
            } else {
                name_base.to_string()
            },
            group:           Some(group.clone()),
            models:          Some(models.clone()),
            base_url:        base_url.clone(),
            proxy_id:        req.proxy_id,
            priority:        None,
            weight:          None,
            update_existing: req.update_existing,
        };
        match import_codex_blob(
            management,
            &codex_req,
            file_name,
            &existing,
            &mut seen,
            &mut index,
            &mut aggregate,
        )
        .await
        {
            Ok(fr) => file_results.push(fr),
            Err(err) => {
                aggregate.failed = aggregate.failed.saturating_add(1);
                file_results.push(AuthFileResult {
                    name:       file_name.clone(),
                    format:     "codex-session".into(),
                    ok:         false,
                    message:    err.to_string(),
                    channel_id: None,
                    created:    None,
                    updated:    None,
                });
            }
        }
    }

    aggregate.total = aggregate.created + aggregate.updated + aggregate.skipped + aggregate.failed;

    Ok(AuthImportResult {
        format: if file_results.iter().all(|f| f.format == "cliproxy") {
            "cliproxy".into()
        } else if file_results.iter().all(|f| f.format == "codex-session") {
            "codex-session".into()
        } else {
            "mixed".into()
        },
        channels: Some(aggregate),
        data: None,
        file_results,
    })
}

fn detect_format(content: &str) -> DetectedFormat {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return DetectedFormat::CodexSession;
    }
    // sub2api-data markers
    if let Ok(v) = serde_json::from_str::<JsonValue>(trimmed) {
        if let Some(t) = v.get("type").and_then(|x| x.as_str()) {
            if t == "sub2api-data" || t == "sub2api-bundle" {
                return DetectedFormat::Sub2apiData;
            }
            // CLIProxyAPI auth file requires "type" provider string
            if matches!(
                t.to_ascii_lowercase().as_str(),
                "codex"
                    | "claude"
                    | "gemini"
                    | "gemini-cli"
                    | "antigravity"
                    | "qwen"
                    | "iflow"
                    | "kimi"
                    | "xai"
                    | "openai"
            ) {
                return DetectedFormat::CliProxy;
            }
        }
        if v.get("data").is_some()
            && (v.get("proxies").is_some()
                || v.get("data").and_then(|d| d.get("proxies")).is_some())
        {
            return DetectedFormat::Sub2apiData;
        }
        if v.get("proxies").is_some() && v.get("accounts").is_some() {
            return DetectedFormat::Sub2apiData;
        }
        // CLIProxy flat codex without relying only on type
        if v.get("account_id").is_some()
            && v.get("access_token").is_some()
            && v.get("tokens").is_none()
        {
            return DetectedFormat::CliProxy;
        }
    }
    DetectedFormat::CodexSession
}

/// Best-effort `/models` pull after import. Never fails the import; keeps defaults on error.
/// Only runs when the client did not supply an explicit models list.
async fn best_effort_fetch_models(management: &ManagementStore, channel_id: u64) -> Option<String> {
    let probe = ChannelProbeService::new(management.clone());
    let models = probe
        .call(FetchModelsRequest {
            channel_id:      Some(channel_id),
            base_url:        String::new(),
            channel_type:    1,
            key:             String::new(),
            header_override: None,
        })
        .await
        .ok()?;
    if models.is_empty() {
        return None;
    }
    let joined = models.join(",");
    let mut channel = management
        .current_data()
        .ok()?
        .channels
        .into_iter()
        .find(|c| c.id == channel_id)?;
    if channel.models == joined {
        return Some(joined);
    }
    channel.models = joined.clone();
    management
        .call(UpdateChannelRequest { channel })
        .await
        .ok()?;
    Some(joined)
}

async fn import_cliproxy_file(
    management: &ManagementStore,
    content: &str,
    file_name: &str,
    group: &str,
    models: &str,
    base_url: Option<&str>,
    proxy_id: Option<u64>,
    update_existing: bool,
    name_base: &str,
    existing: &[ChannelRecord],
    seen: &mut HashMap<String, usize>,
    index: &mut usize,
    aggregate: &mut CodexAuthImportResult,
) -> Result<AuthFileResult, ManagementError> {
    let meta: JsonMap<String, JsonValue> = serde_json::from_str(content.trim())
        .map_err(|_| ManagementError::InvalidRequest("invalid CLIProxyAPI auth JSON"))?;
    let provider = meta
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_ascii_lowercase();
    let email = meta
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let label = if !name_base.is_empty() {
        name_base.to_string()
    } else if !email.is_empty() {
        email.clone()
    } else {
        file_name.trim_end_matches(".json").to_string()
    };

    *index = index.saturating_add(1);
    let idx = *index;

    match provider.as_str() {
        "codex" | "openai" => {
            // Reuse flexible codex key parser (CLIProxy flat shape is a subset).
            let key = parse_flexible_codex_key(content)?;
            let key_json = codex_key_to_json(&key)?;
            let identity = codex_auth_import::identity_keys_for_channel_key(&key_json);
            // Build a pseudo ParsedCodexAuth via collect path: import as single entry.
            let parsed_list = codex_auth_import::normalize_codex_auth_blob(content)?;
            let item = parsed_list
                .into_iter()
                .next()
                .ok_or(ManagementError::InvalidRequest(
                    "empty codex auth after parse",
                ))?;

            if let Some(prev) = item.identity_keys.iter().find_map(|k| seen.get(k).copied()) {
                aggregate.skipped = aggregate.skipped.saturating_add(1);
                return Ok(AuthFileResult {
                    name:       file_name.into(),
                    format:     "cliproxy".into(),
                    ok:         true,
                    message:    format!("duplicate of entry {prev}; skipped"),
                    channel_id: None,
                    created:    None,
                    updated:    None,
                });
            }
            for k in &item.identity_keys {
                seen.insert(k.clone(), idx);
            }

            let channel_name = if label.is_empty() {
                item.name.clone()
            } else {
                label.clone()
            };

            if let Some(existing_id) = find_existing_channel_id(existing, &item) {
                if update_existing {
                    let mut channel = existing
                        .iter()
                        .find(|c| c.id == existing_id)
                        .cloned()
                        .ok_or(ManagementError::NotFound)?;
                    channel.key = key_json;
                    channel.channel_type = CHANNEL_TYPE_CODEX;
                    channel.name = channel_name.clone();
                    if let Some(pid) = proxy_id {
                        channel.proxy_id = Some(pid);
                    }
                    let updated = management.call(UpdateChannelRequest { channel }).await?;
                    aggregate.updated = aggregate.updated.saturating_add(1);
                    aggregate.items.push(CodexAuthImportItem {
                        index:      idx,
                        name:       channel_name,
                        action:     "updated".into(),
                        channel_id: Some(updated.id),
                        message:    String::new(),
                    });
                    return Ok(AuthFileResult {
                        name:       file_name.into(),
                        format:     "cliproxy".into(),
                        ok:         true,
                        message:    "updated".into(),
                        channel_id: Some(updated.id),
                        created:    None,
                        updated:    Some(1),
                    });
                }
                aggregate.skipped = aggregate.skipped.saturating_add(1);
                return Ok(AuthFileResult {
                    name:       file_name.into(),
                    format:     "cliproxy".into(),
                    ok:         true,
                    message:    "exists; update_existing=false".into(),
                    channel_id: Some(existing_id),
                    created:    None,
                    updated:    None,
                });
            }

            let channel = ChannelRecord {
                id: 0,
                snapshot_id: None,
                channel_type: CHANNEL_TYPE_CODEX,
                key: key_json,
                status: STATUS_ENABLED,
                name: channel_name.clone(),
                weight: Some(1),
                created_time: now_unix(),
                test_time: 0,
                response_time: 0,
                base_url: base_url.map(str::to_string),
                balance: 0.0,
                balance_updated_time: 0,
                models: models.to_string(),
                group: group.to_string(),
                used_quota: 0,
                model_mapping: None,
                priority: Some(0),
                auto_ban: Some(1),
                tag: None,
                setting: Some(r#"{"import_source":"cliproxyapi"}"#.into()),
                param_override: None,
                header_override: None,
                remark: Some(format!("imported from CLIProxyAPI ({file_name})")),
                proxy_id,
            };
            let created = management.call(CreateChannelRequest { channel }).await?;
            if models.trim().is_empty() {
                let _ = best_effort_fetch_models(management, created.id).await;
            }
            aggregate.created = aggregate.created.saturating_add(1);
            aggregate.items.push(CodexAuthImportItem {
                index:      idx,
                name:       channel_name,
                action:     "created".into(),
                channel_id: Some(created.id),
                message:    String::new(),
            });
            let _ = identity;
            Ok(AuthFileResult {
                name:       file_name.into(),
                format:     "cliproxy".into(),
                ok:         true,
                message:    "created".into(),
                channel_id: Some(created.id),
                created:    Some(1),
                updated:    None,
            })
        }
        "claude" => {
            let access = str_field(&meta, "access_token")
                .or_else(|| str_field(&meta, "api_key"))
                .ok_or(ManagementError::InvalidRequest(
                    "claude auth file missing access_token",
                ))?;
            // Prefer storing structured JSON for refresh support later; gateway uses plain key lines.
            // For Claude type 14, new-api uses the access token as key string.
            let key = access;
            let channel = ChannelRecord {
                id: 0,
                snapshot_id: None,
                channel_type: CHANNEL_TYPE_ANTHROPIC,
                key,
                status: STATUS_ENABLED,
                name: label.clone(),
                weight: Some(1),
                created_time: now_unix(),
                test_time: 0,
                response_time: 0,
                base_url: base_url.map(str::to_string),
                balance: 0.0,
                balance_updated_time: 0,
                models: "claude-sonnet-4-5,claude-opus-4-5".into(),
                group: group.to_string(),
                used_quota: 0,
                model_mapping: None,
                priority: Some(0),
                auto_ban: Some(1),
                tag: None,
                setting: Some(r#"{"import_source":"cliproxyapi","provider":"claude"}"#.into()),
                param_override: None,
                header_override: None,
                remark: Some(format!("imported from CLIProxyAPI claude ({file_name})")),
                proxy_id,
            };
            let created = management.call(CreateChannelRequest { channel }).await?;
            if models.trim().is_empty() {
                let _ = best_effort_fetch_models(management, created.id).await;
            }
            aggregate.created = aggregate.created.saturating_add(1);
            Ok(AuthFileResult {
                name:       file_name.into(),
                format:     "cliproxy".into(),
                ok:         true,
                message:    "created claude channel".into(),
                channel_id: Some(created.id),
                created:    Some(1),
                updated:    None,
            })
        }
        "gemini" | "gemini-cli" => {
            let key = str_field(&meta, "api_key")
                .or_else(|| str_field(&meta, "access_token"))
                .ok_or(ManagementError::InvalidRequest(
                    "gemini auth file missing api_key/access_token",
                ))?;
            let channel = ChannelRecord {
                id: 0,
                snapshot_id: None,
                channel_type: CHANNEL_TYPE_GEMINI,
                key,
                status: STATUS_ENABLED,
                name: label.clone(),
                weight: Some(1),
                created_time: now_unix(),
                test_time: 0,
                response_time: 0,
                base_url: base_url.map(str::to_string),
                balance: 0.0,
                balance_updated_time: 0,
                models: "gemini-2.5-pro,gemini-2.5-flash".into(),
                group: group.to_string(),
                used_quota: 0,
                model_mapping: None,
                priority: Some(0),
                auto_ban: Some(1),
                tag: None,
                setting: Some(r#"{"import_source":"cliproxyapi","provider":"gemini"}"#.into()),
                param_override: None,
                header_override: None,
                remark: Some(format!("imported from CLIProxyAPI gemini ({file_name})")),
                proxy_id,
            };
            let created = management.call(CreateChannelRequest { channel }).await?;
            if models.trim().is_empty() {
                let _ = best_effort_fetch_models(management, created.id).await;
            }
            aggregate.created = aggregate.created.saturating_add(1);
            Ok(AuthFileResult {
                name:       file_name.into(),
                format:     "cliproxy".into(),
                ok:         true,
                message:    "created gemini channel".into(),
                channel_id: Some(created.id),
                created:    Some(1),
                updated:    None,
            })
        }
        "xai" | "x-ai" | "x.ai" | "grok" => {
            import_cliproxy_xai(
                management,
                &meta,
                file_name,
                &label,
                group,
                models,
                base_url,
                proxy_id,
                update_existing,
                existing,
                index,
                aggregate,
            )
            .await
        }
        other => Err(ManagementError::InvalidRequest(leak(format!(
            "unsupported CLIProxyAPI auth type: {other} (supported: codex, claude, gemini, xai)"
        )))),
    }
}

async fn import_cliproxy_xai(
    management: &ManagementStore,
    meta: &JsonMap<String, JsonValue>,
    file_name: &str,
    label: &str,
    group: &str,
    models: &str,
    base_url: Option<&str>,
    proxy_id: Option<u64>,
    update_existing: bool,
    existing: &[ChannelRecord],
    index: &mut usize,
    aggregate: &mut CodexAuthImportResult,
) -> Result<AuthFileResult, ManagementError> {
    let parsed = parse_cliproxy_xai_auth(meta)?;
    let channel_name = if label.is_empty() {
        file_name.trim_end_matches(".json").to_string()
    } else {
        label.to_string()
    };
    let models_from_req_empty = models.trim().is_empty();
    let models = if models_from_req_empty {
        XAI_DEFAULT_MODELS.to_string()
    } else {
        models.to_string()
    };
    let resolved_base = base_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(normalize_openai_compatible_base_url)
        .or_else(|| parsed.base_url.clone());
    let setting = build_xai_channel_setting(&parsed)?;
    let header_override = build_xai_header_override(&parsed, resolved_base.as_deref())?;
    let status = if meta
        .get("disabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        // new-api convention: 2 = manually disabled
        2
    } else {
        STATUS_ENABLED
    };

    if let Some(existing_id) = find_existing_xai_channel(existing, &parsed, &channel_name) {
        if update_existing {
            let mut channel = existing
                .iter()
                .find(|c| c.id == existing_id)
                .cloned()
                .ok_or(ManagementError::NotFound)?;
            channel.key = parsed.access_token.clone();
            channel.channel_type = CHANNEL_TYPE_XAI;
            channel.name = channel_name.clone();
            // Re-import recovers auto-disabled channels so users need not delete/recreate
            // after fixing headers or tokens. Keep manual disable (status 2).
            if status == STATUS_ENABLED {
                if channel.status == STATUS_AUTO_DISABLED || channel.status == STATUS_ENABLED {
                    channel.status = STATUS_ENABLED;
                }
            } else {
                channel.status = status;
            }
            channel.models = models;
            channel.base_url = resolved_base.clone();
            // Rebuild setting drops auto-ban status_reason/status_time.
            channel.setting = Some(setting);
            channel.header_override = header_override;
            channel.remark = Some(format!("imported from CLIProxyAPI xai ({file_name})"));
            if let Some(pid) = proxy_id {
                channel.proxy_id = Some(pid);
            }
            let updated = management.call(UpdateChannelRequest { channel }).await?;
            aggregate.updated = aggregate.updated.saturating_add(1);
            aggregate.items.push(CodexAuthImportItem {
                index:      *index,
                name:       channel_name,
                action:     "updated".into(),
                channel_id: Some(updated.id),
                message:    String::new(),
            });
            return Ok(AuthFileResult {
                name:       file_name.into(),
                format:     "cliproxy".into(),
                ok:         true,
                message:    "updated xai channel".into(),
                channel_id: Some(updated.id),
                created:    None,
                updated:    Some(1),
            });
        }
        aggregate.skipped = aggregate.skipped.saturating_add(1);
        return Ok(AuthFileResult {
            name:       file_name.into(),
            format:     "cliproxy".into(),
            ok:         true,
            message:    "exists; update_existing=false".into(),
            channel_id: Some(existing_id),
            created:    None,
            updated:    None,
        });
    }

    let channel = ChannelRecord {
        id: 0,
        snapshot_id: None,
        channel_type: CHANNEL_TYPE_XAI,
        key: parsed.access_token,
        status,
        name: channel_name.clone(),
        weight: Some(1),
        created_time: now_unix(),
        test_time: 0,
        response_time: 0,
        base_url: resolved_base,
        balance: 0.0,
        balance_updated_time: 0,
        models,
        group: group.to_string(),
        used_quota: 0,
        model_mapping: None,
        priority: Some(0),
        auto_ban: Some(1),
        tag: None,
        setting: Some(setting),
        param_override: None,
        header_override,
        remark: Some(format!("imported from CLIProxyAPI xai ({file_name})")),
        proxy_id,
    };
    let created = management.call(CreateChannelRequest { channel }).await?;
    if models_from_req_empty {
        let _ = best_effort_fetch_models(management, created.id).await;
    }
    aggregate.created = aggregate.created.saturating_add(1);
    aggregate.items.push(CodexAuthImportItem {
        index:      *index,
        name:       channel_name,
        action:     "created".into(),
        channel_id: Some(created.id),
        message:    String::new(),
    });
    Ok(AuthFileResult {
        name:       file_name.into(),
        format:     "cliproxy".into(),
        ok:         true,
        message:    "created xai channel".into(),
        channel_id: Some(created.id),
        created:    Some(1),
        updated:    None,
    })
}

#[derive(Debug, Clone)]
struct ParsedXaiAuth {
    access_token:   String,
    refresh_token:  Option<String>,
    email:          Option<String>,
    subject:        Option<String>,
    auth_kind:      String,
    base_url:       Option<String>,
    token_endpoint: Option<String>,
    expired:        Option<String>,
    last_refresh:   Option<String>,
    using_api:      bool,
    headers:        Option<JsonMap<String, JsonValue>>,
}

fn parse_cliproxy_xai_auth(
    meta: &JsonMap<String, JsonValue>,
) -> Result<ParsedXaiAuth, ManagementError> {
    let access_token = str_field(meta, "access_token")
        .or_else(|| str_field(meta, "api_key"))
        .ok_or(ManagementError::InvalidRequest(
            "xai auth file missing access_token/api_key",
        ))?;
    let auth_kind = str_field(meta, "auth_kind")
        .unwrap_or_else(|| {
            if str_field(meta, "refresh_token").is_some() {
                "oauth".into()
            } else {
                "api_key".into()
            }
        })
        .to_ascii_lowercase();
    let using_api = bool_field(meta, "using_api").unwrap_or_else(|| auth_kind != "oauth");
    let file_base = str_field(meta, "base_url").map(|u| normalize_openai_compatible_base_url(&u));
    let base_url = file_base.or_else(|| {
        Some(if using_api {
            XAI_DEFAULT_API_BASE.to_string()
        } else {
            XAI_DEFAULT_CLI_CHAT_BASE.to_string()
        })
    });
    let headers = meta.get("headers").and_then(|v| v.as_object()).cloned();
    Ok(ParsedXaiAuth {
        access_token,
        refresh_token: str_field(meta, "refresh_token"),
        email: str_field(meta, "email"),
        subject: str_field(meta, "sub").or_else(|| str_field(meta, "subject")),
        auth_kind,
        base_url,
        token_endpoint: str_field(meta, "token_endpoint"),
        expired: str_field(meta, "expired").or_else(|| str_field(meta, "expire")),
        last_refresh: str_field(meta, "last_refresh"),
        using_api,
        headers,
    })
}

fn build_xai_channel_setting(parsed: &ParsedXaiAuth) -> Result<String, ManagementError> {
    let mut map = JsonMap::new();
    map.insert(
        "import_source".into(),
        JsonValue::String("cliproxyapi".into()),
    );
    map.insert("provider".into(), JsonValue::String("xai".into()));
    map.insert(
        "auth_kind".into(),
        JsonValue::String(parsed.auth_kind.clone()),
    );
    map.insert("using_api".into(), JsonValue::Bool(parsed.using_api));
    if let Some(email) = &parsed.email {
        map.insert("email".into(), JsonValue::String(email.clone()));
    }
    if let Some(sub) = &parsed.subject {
        map.insert("sub".into(), JsonValue::String(sub.clone()));
    }
    if let Some(refresh) = &parsed.refresh_token {
        map.insert("refresh_token".into(), JsonValue::String(refresh.clone()));
    }
    if let Some(endpoint) = &parsed.token_endpoint {
        map.insert("token_endpoint".into(), JsonValue::String(endpoint.clone()));
    }
    if let Some(expired) = &parsed.expired {
        map.insert("expired".into(), JsonValue::String(expired.clone()));
    }
    if let Some(last_refresh) = &parsed.last_refresh {
        map.insert(
            "last_refresh".into(),
            JsonValue::String(last_refresh.clone()),
        );
    }
    serde_json::to_string(&JsonValue::Object(map))
        .map_err(|_| ManagementError::InvalidRequest("failed to serialize xai channel setting"))
}

/// Match CLIProxyAPI `applyXAIChatHeaders`: for OAuth/chat-proxy channels inject
/// identity headers; auth-file `headers` override defaults (same order as
/// applyXAICustomHeaders after defaults).
fn build_xai_header_override(
    parsed: &ParsedXaiAuth,
    resolved_base: Option<&str>,
) -> Result<Option<String>, ManagementError> {
    let mut map = JsonMap::new();
    let base = resolved_base
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(parsed
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()));
    if !parsed.using_api && base.is_some_and(is_xai_cli_chat_proxy_base) {
        map.insert(
            XAI_TOKEN_AUTH_HEADER.into(),
            JsonValue::String(XAI_TOKEN_AUTH_VALUE.into()),
        );
        map.insert(
            XAI_CLIENT_VERSION_HEADER.into(),
            JsonValue::String(XAI_CLIENT_VERSION_VALUE.into()),
        );
    }
    if let Some(headers) = &parsed.headers {
        for (key, value) in headers {
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            match value {
                JsonValue::String(s) if !s.trim().is_empty() => {
                    map.insert(key.to_string(), JsonValue::String(s.trim().to_string()));
                }
                JsonValue::Number(n) => {
                    map.insert(key.to_string(), JsonValue::String(n.to_string()));
                }
                JsonValue::Bool(b) => {
                    map.insert(key.to_string(), JsonValue::String(b.to_string()));
                }
                _ => {}
            }
        }
    }
    if map.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(&JsonValue::Object(map))
        .map(Some)
        .map_err(|_| ManagementError::InvalidRequest("failed to serialize xai header_override"))
}

fn is_xai_cli_chat_proxy_base(base_url: &str) -> bool {
    let normalized = normalize_openai_compatible_base_url(base_url).to_ascii_lowercase();
    normalized == XAI_DEFAULT_CLI_CHAT_BASE
        || normalized
            .trim_end_matches('/')
            .ends_with("cli-chat-proxy.grok.com")
}

fn find_existing_xai_channel(
    existing: &[ChannelRecord],
    parsed: &ParsedXaiAuth,
    channel_name: &str,
) -> Option<u64> {
    existing.iter().find_map(|channel| {
        if channel.channel_type != CHANNEL_TYPE_XAI {
            return None;
        }
        if channel.key.trim() == parsed.access_token {
            return Some(channel.id);
        }
        if !channel_name.is_empty() && channel.name.eq_ignore_ascii_case(channel_name) {
            return Some(channel.id);
        }
        let setting = channel.setting.as_deref().unwrap_or("");
        if setting.is_empty() {
            return None;
        }
        let Ok(value) = serde_json::from_str::<JsonValue>(setting) else {
            return None;
        };
        if value
            .get("provider")
            .and_then(|v| v.as_str())
            .is_some_and(|p| !p.eq_ignore_ascii_case("xai"))
        {
            return None;
        }
        if let Some(email) = &parsed.email {
            if value
                .get("email")
                .and_then(|v| v.as_str())
                .is_some_and(|e| e.eq_ignore_ascii_case(email))
            {
                return Some(channel.id);
            }
        }
        if let Some(sub) = &parsed.subject {
            if value
                .get("sub")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s == sub)
            {
                return Some(channel.id);
            }
        }
        None
    })
}

/// Gateway joins `{base_url}{path}` where path starts with `/v1/...`.
/// CLIProxy stores bases with trailing `/v1`; strip it for OpenAI-compat channels.
fn normalize_openai_compatible_base_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    let lower = trimmed.to_ascii_lowercase();
    if lower.ends_with("/v1") {
        trimmed[..trimmed.len().saturating_sub(3)]
            .trim_end_matches('/')
            .to_string()
    } else {
        trimmed.to_string()
    }
}

fn bool_field(map: &JsonMap<String, JsonValue>, key: &str) -> Option<bool> {
    let value = map.get(key)?;
    if let Some(b) = value.as_bool() {
        return Some(b);
    }
    if let Some(s) = value.as_str() {
        return match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        };
    }
    None
}

async fn import_codex_blob(
    management: &ManagementStore,
    req: &CodexAuthImportRequest,
    file_name: &str,
    existing: &[ChannelRecord],
    seen: &mut HashMap<String, usize>,
    index: &mut usize,
    aggregate: &mut CodexAuthImportResult,
) -> Result<AuthFileResult, ManagementError> {
    let entries = collect_entries(req)?;
    let mut created = 0usize;
    let mut updated = 0usize;
    let mut last_id = None;
    let group = req.group.as_deref().unwrap_or("default").to_string();
    let models = req
        .models
        .as_deref()
        .unwrap_or("gpt-5.1,gpt-5,o3,o4-mini")
        .to_string();

    for item in entries {
        *index = index.saturating_add(1);
        let idx = *index;
        aggregate.total = aggregate.total.saturating_add(1);

        for w in &item.warnings {
            aggregate.warnings.push(CodexAuthImportMessage {
                index:   idx,
                name:    item.name.clone(),
                message: w.clone(),
            });
        }

        if let Some(prev) = item.identity_keys.iter().find_map(|k| seen.get(k).copied()) {
            aggregate.skipped = aggregate.skipped.saturating_add(1);
            aggregate.items.push(CodexAuthImportItem {
                index:      idx,
                name:       item.name.clone(),
                action:     "skipped".into(),
                channel_id: None,
                message:    format!("duplicate of entry {prev}"),
            });
            continue;
        }
        for k in &item.identity_keys {
            seen.insert(k.clone(), idx);
        }

        let key_json = codex_key_to_json(&item.key)?;
        let account_name = if req.name.trim().is_empty() {
            item.name.clone()
        } else {
            req.name.trim().to_string()
        };

        if let Some(existing_id) = find_existing_channel_id(existing, &item) {
            if req.update_existing {
                let mut channel = existing
                    .iter()
                    .find(|c| c.id == existing_id)
                    .cloned()
                    .ok_or(ManagementError::NotFound)?;
                channel.key = key_json;
                channel.channel_type = CHANNEL_TYPE_CODEX;
                channel.name = account_name.clone();
                if let Some(pid) = req.proxy_id {
                    channel.proxy_id = Some(pid);
                }
                let ch = management.call(UpdateChannelRequest { channel }).await?;
                updated = updated.saturating_add(1);
                aggregate.updated = aggregate.updated.saturating_add(1);
                last_id = Some(ch.id);
                aggregate.items.push(CodexAuthImportItem {
                    index:      idx,
                    name:       account_name,
                    action:     "updated".into(),
                    channel_id: Some(ch.id),
                    message:    String::new(),
                });
            } else {
                aggregate.skipped = aggregate.skipped.saturating_add(1);
            }
            continue;
        }

        let channel = ChannelRecord {
            id:                   0,
            snapshot_id:          None,
            channel_type:         CHANNEL_TYPE_CODEX,
            key:                  key_json,
            status:               STATUS_ENABLED,
            name:                 account_name.clone(),
            weight:               Some(1),
            created_time:         now_unix(),
            test_time:            0,
            response_time:        0,
            base_url:             req.base_url.clone(),
            balance:              0.0,
            balance_updated_time: 0,
            models:               models.clone(),
            group:                group.clone(),
            used_quota:           0,
            model_mapping:        None,
            priority:             Some(0),
            auto_ban:             Some(1),
            tag:                  None,
            setting:              Some(r#"{"import_source":"codex-session"}"#.into()),
            param_override:       None,
            header_override:      None,
            remark:               Some(format!("imported from {file_name}")),
            proxy_id:             req.proxy_id,
        };
        let ch = management.call(CreateChannelRequest { channel }).await?;
        created = created.saturating_add(1);
        aggregate.created = aggregate.created.saturating_add(1);
        last_id = Some(ch.id);
        aggregate.items.push(CodexAuthImportItem {
            index:      idx,
            name:       account_name,
            action:     "created".into(),
            channel_id: Some(ch.id),
            message:    String::new(),
        });
    }

    Ok(AuthFileResult {
        name:       file_name.into(),
        format:     "codex-session".into(),
        ok:         true,
        message:    format!("created {created}, updated {updated}"),
        channel_id: last_id,
        created:    Some(created),
        updated:    Some(updated),
    })
}

fn str_field(map: &JsonMap<String, JsonValue>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

// silence unused CodexOAuthKey if only used via paths
#[allow(dead_code)]
fn _codex_key_placeholder() -> CodexOAuthKey {
    CodexOAuthKey::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cliproxy_codex_file() {
        let raw = r#"{"type":"codex","email":"a@b.com","access_token":"at","refresh_token":"rt","account_id":"acc"}"#;
        assert_eq!(detect_format(raw), DetectedFormat::CliProxy);
    }

    #[test]
    fn detects_cliproxy_xai_file() {
        let raw = r#"{"type":"xai","auth_kind":"oauth","access_token":"at","refresh_token":"rt","email":"u@x.ai"}"#;
        assert_eq!(detect_format(raw), DetectedFormat::CliProxy);
    }

    #[test]
    fn parses_cliproxy_xai_oauth_file() {
        let raw = r#"{
          "type":"xai",
          "auth_kind":"oauth",
          "access_token":"at-1",
          "refresh_token":"rt-1",
          "email":"z@example.com",
          "sub":"sub-1",
          "base_url":"https://cli-chat-proxy.grok.com/v1",
          "token_endpoint":"https://auth.x.ai/oauth2/token",
          "headers":{"User-Agent":"grok-shell/0.2.93"}
        }"#;
        let meta: JsonMap<String, JsonValue> = serde_json::from_str(raw).unwrap();
        let parsed = parse_cliproxy_xai_auth(&meta).expect("parse xai");
        assert_eq!(parsed.access_token, "at-1");
        assert_eq!(parsed.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(parsed.email.as_deref(), Some("z@example.com"));
        assert_eq!(parsed.subject.as_deref(), Some("sub-1"));
        assert_eq!(parsed.auth_kind, "oauth");
        assert!(!parsed.using_api);
        assert_eq!(
            parsed.base_url.as_deref(),
            Some("https://cli-chat-proxy.grok.com")
        );
        assert!(parsed.headers.is_some());
        let override_json = build_xai_header_override(&parsed, parsed.base_url.as_deref())
            .expect("headers")
            .expect("some");
        let map: JsonMap<String, JsonValue> = serde_json::from_str(&override_json).unwrap();
        assert_eq!(
            map.get(XAI_TOKEN_AUTH_HEADER).and_then(|v| v.as_str()),
            Some(XAI_TOKEN_AUTH_VALUE)
        );
        assert_eq!(
            map.get(XAI_CLIENT_VERSION_HEADER).and_then(|v| v.as_str()),
            Some(XAI_CLIENT_VERSION_VALUE)
        );
        assert_eq!(
            map.get("User-Agent").and_then(|v| v.as_str()),
            Some("grok-shell/0.2.93")
        );
    }

    #[test]
    fn xai_api_key_path_skips_cli_identity_headers() {
        let raw = r#"{"type":"xai","auth_kind":"api_key","access_token":"sk","using_api":true}"#;
        let meta: JsonMap<String, JsonValue> = serde_json::from_str(raw).unwrap();
        let parsed = parse_cliproxy_xai_auth(&meta).expect("parse");
        assert!(parsed.using_api);
        let override_json =
            build_xai_header_override(&parsed, Some(XAI_DEFAULT_API_BASE)).expect("ok");
        assert!(override_json.is_none());
    }

    #[test]
    fn xai_cli_headers_without_file_headers() {
        let raw = r#"{"type":"xai","auth_kind":"oauth","access_token":"at","refresh_token":"rt"}"#;
        let meta: JsonMap<String, JsonValue> = serde_json::from_str(raw).unwrap();
        let parsed = parse_cliproxy_xai_auth(&meta).expect("parse");
        let override_json = build_xai_header_override(&parsed, parsed.base_url.as_deref())
            .expect("ok")
            .expect("cli headers");
        let map: JsonMap<String, JsonValue> = serde_json::from_str(&override_json).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get(XAI_CLIENT_VERSION_HEADER).and_then(|v| v.as_str()),
            Some(XAI_CLIENT_VERSION_VALUE)
        );
    }

    #[test]
    fn normalizes_xai_base_url_trailing_v1() {
        assert_eq!(
            normalize_openai_compatible_base_url("https://api.x.ai/v1/"),
            "https://api.x.ai"
        );
        assert_eq!(
            normalize_openai_compatible_base_url("https://api.x.ai"),
            "https://api.x.ai"
        );
    }

    #[test]
    fn detects_sub2api_data() {
        let raw = r#"{"type":"sub2api-data","version":1,"proxies":[],"accounts":[]}"#;
        assert_eq!(detect_format(raw), DetectedFormat::Sub2apiData);
    }

    #[test]
    fn detects_nested_session_as_codex() {
        let raw = r#"{"tokens":{"access_token":"at","refresh_token":"rt"},"email":"x@y.com"}"#;
        assert_eq!(detect_format(raw), DetectedFormat::CodexSession);
    }

    #[test]
    fn cliproxy_codex_parses_via_flexible_key() {
        let raw = r#"{"type":"codex","access_token":"at-1","refresh_token":"rt-1","account_id":"acct","email":"e@x.com","expired":"9999999999"}"#;
        let key = parse_flexible_codex_key(raw).expect("parse");
        assert_eq!(key.access_token.as_deref(), Some("at-1"));
        assert_eq!(key.account_id.as_deref(), Some("acct"));
    }
}
