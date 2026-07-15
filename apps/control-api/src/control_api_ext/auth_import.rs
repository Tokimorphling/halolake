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

use super::{
    codex_auth_import::{
        self, CHANNEL_TYPE_CODEX, CodexAuthImportItem, CodexAuthImportMessage,
        CodexAuthImportRequest, CodexAuthImportResult, CodexOAuthKey, codex_key_to_json,
        collect_entry_results, find_existing_channel_id, merge_codex_oauth_key,
        parse_flexible_codex_key,
    },
    openai_oauth::{AuthMethod, expand_codex_import_blob},
    sub2api_data_import::{self, DataImportResult, Sub2apiDataImportRequest},
};
use crate::{
    channel_probe::{ChannelProbeService, FetchModelsRequest},
    channel_special::validate_xai_oauth_endpoint,
    proxy::ProxyStore,
    storage::ManagementStore,
};
use halolake_control_plane::{CreateChannelRequest, ManagementError, UpdateChannelRequest};
use halolake_domain::{
    ChannelRecord, STATUS_AUTO_DISABLED, STATUS_ENABLED, STATUS_MANUALLY_DISABLED,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use service_async::Service;
use std::collections::HashMap;

const CHANNEL_TYPE_ANTHROPIC: i32 = 14;
const CHANNEL_TYPE_GEMINI: i32 = 24;
const CHANNEL_TYPE_XAI: i32 = 48;
const CLAUDE_DEFAULT_MODELS: &str =
    "claude-haiku-4-5-20251001,claude-sonnet-4-5-20250929,claude-sonnet-4-6,claude-opus-4-6,\
     claude-opus-4-7,claude-opus-4-8,claude-sonnet-5,claude-fable-5,claude-opus-4-5-20251101,\
     claude-opus-4-1-20250805,claude-opus-4-20250514,claude-sonnet-4-20250514,\
     claude-3-7-sonnet-20250219,claude-3-5-haiku-20241022";
const CLAUDE_OAUTH_BETA: &str =
    "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
     context-management-2025-06-27,prompt-caching-scope-2026-01-05,structured-outputs-2025-12-15,\
     fast-mode-2026-02-01,redact-thinking-2026-02-12,token-efficient-tools-2026-03-28";
const CLAUDE_API_VERSION: &str = "2023-06-01";
const GEMINI_DEFAULT_MODELS: &str = "gemini-2.5-pro,gemini-2.5-flash";
const XAI_DEFAULT_API_BASE: &str = "https://api.x.ai/v1";
const XAI_DEFAULT_CLI_CHAT_BASE: &str = "https://cli-chat-proxy.grok.com/v1";
const XAI_DEFAULT_MODELS: &str = "grok-build-0.1,grok-4.5,grok-4.3,grok-4.20-0309-reasoning,\
                                  grok-4.20-0309-non-reasoning,grok-4.20-multi-agent-0309,\
                                  grok-3-mini,grok-3-mini-fast,grok-composer-2.5-fast";
const UNSUPPORTED_CLIPROXY_AUTH_TYPE: &str =
    "unsupported CLIProxyAPI auth type (supported: codex, claude, gemini, xai)";
/// Keep in sync with CLIProxyAPI `xaiClientVersionValue` / chat-proxy identity headers.
const XAI_TOKEN_AUTH_HEADER: &str = "X-XAI-Token-Auth";
const XAI_TOKEN_AUTH_VALUE: &str = "xai-grok-cli";
const XAI_CLIENT_VERSION_HEADER: &str = "x-grok-client-version";
const XAI_CLIENT_VERSION_VALUE: &str = "0.2.93";
const XAI_USER_AGENT: &str = "xai-grok-workspace/0.2.93";

/// Auto-detecting import request (JSON body).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AuthImportRequest {
    /// Format hint: `auto` (default), `cliproxy`, `codex-session`, `sub2api-data`.
    #[serde(default = "default_format")]
    pub(crate) format:          String,
    /// Paste interpretation. Omitted/`auto` preserves the legacy detector.
    #[serde(default)]
    pub(crate) auth_method:     AuthMethod,
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
        // `filenames` is aligned with `contents`. Only borrow its first entry
        // for the legacy single-content shape; otherwise doing so assigns the
        // same auth-file identity to both `content` and `contents[0]`.
        let name = if req.contents.iter().all(|content| content.trim().is_empty()) {
            req.filenames
                .first()
                .cloned()
                .unwrap_or_else(|| "content".into())
        } else {
            "content".into()
        };
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
    if req.auth_method == AuthMethod::Auto
        && blobs.len() == 1
        && (hint == "sub2api-data" || hint == "auto" || hint.is_empty())
    {
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
    let explicit_models = req
        .models
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    let models_were_explicit = explicit_models.is_some();
    let models = explicit_models
        .unwrap_or("gpt-5.1,gpt-5,o3,o4-mini")
        .to_string();
    let base_url = req
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string);
    let name_base = req.name.trim();

    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut index = 0usize;

    for (file_name, content) in &blobs {
        let detected = match req.auth_method {
            AuthMethod::Auto => match hint.as_str() {
                "cliproxy" | "cliproxyapi" | "cli-proxy" => DetectedFormat::CliProxy,
                "codex-session" | "codex" | "sub2api-session" => DetectedFormat::CodexSession,
                _ => detect_format(content),
            },
            _ => DetectedFormat::CodexSession,
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
            // Re-read after every prior file so this decision sees channels
            // created or updated earlier in the same batch.
            let existing = management.current_data()?.channels;
            match import_cliproxy_file(
                management,
                proxies,
                content,
                file_name,
                &group,
                &models,
                models_were_explicit,
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

        // Codex session / raw token / RT / PAT — expand then import.
        let proxy_url = if req.auth_method.requires_network(content) {
            match selected_proxy_url(proxies, req.proxy_id) {
                Ok(proxy_url) => proxy_url,
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
                    continue;
                }
            }
        } else {
            None
        };
        let expanded =
            match expand_codex_import_blob(content, req.auth_method, proxy_url.as_deref()).await {
                Ok(items) => items,
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
                    continue;
                }
            };

        // Process every expansion result independently. A later RT/PAT failure
        // must not discard an earlier successfully rotated credential.
        let mut file_created = 0usize;
        let mut file_updated = 0usize;
        let mut file_failed = 0usize;
        let mut last_channel_id = None;
        for expanded_item in expanded {
            let ordinal = expanded_item.ordinal;
            let expanded_content = match expanded_item.result {
                Ok(content) => content,
                Err(message) => {
                    file_failed = file_failed.saturating_add(1);
                    record_codex_item_failure(&mut aggregate, &mut index, ordinal, message);
                    continue;
                }
            };
            let codex_req = CodexAuthImportRequest {
                content:         String::new(),
                contents:        vec![expanded_content],
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
                ordinal,
                &mut seen,
                &mut index,
                &mut aggregate,
            )
            .await
            {
                Ok((result, entry_failed)) => {
                    file_created = file_created.saturating_add(result.created.unwrap_or_default());
                    file_updated = file_updated.saturating_add(result.updated.unwrap_or_default());
                    file_failed = file_failed.saturating_add(entry_failed);
                    last_channel_id = result.channel_id.or(last_channel_id);
                }
                Err(err) => {
                    file_failed = file_failed.saturating_add(1);
                    record_codex_item_failure(
                        &mut aggregate,
                        &mut index,
                        ordinal,
                        public_codex_import_error(&err),
                    );
                }
            }
        }
        file_results.push(AuthFileResult {
            name:       file_name.clone(),
            format:     "codex-session".into(),
            ok:         file_failed == 0,
            message:    format!(
                "created {file_created}, updated {file_updated}, failed {file_failed}"
            ),
            channel_id: last_channel_id,
            created:    Some(file_created),
            updated:    Some(file_updated),
        });
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

fn selected_proxy_url(
    proxies: &ProxyStore,
    proxy_id: Option<u64>,
) -> Result<Option<String>, ManagementError> {
    let Some(proxy_id) = proxy_id else {
        return Ok(None);
    };
    proxies
        .resolve_url(Some(proxy_id))
        .map(Some)
        .ok_or(ManagementError::InvalidRequest(
            "selected proxy is missing or disabled",
        ))
}

fn record_codex_item_failure(
    aggregate: &mut CodexAuthImportResult,
    index: &mut usize,
    source_ordinal: usize,
    message: String,
) {
    *index = index.saturating_add(1);
    let item_index = *index;
    let name = format!("credential {source_ordinal}");
    aggregate.failed = aggregate.failed.saturating_add(1);
    aggregate.errors.push(CodexAuthImportMessage {
        index:   item_index,
        name:    name.clone(),
        message: message.clone(),
    });
    aggregate.items.push(CodexAuthImportItem {
        index: item_index,
        name,
        action: "failed".into(),
        channel_id: None,
        message,
    });
}

fn public_codex_import_error(err: &ManagementError) -> String {
    let message = err.to_string().to_ascii_lowercase();
    if message.contains("expired") {
        "credential is invalid or expired".into()
    } else if message.contains("missing") {
        "credential is missing required fields".into()
    } else if message.contains("json") || message.contains("unsupported format") {
        "credential JSON is invalid".into()
    } else {
        "credential import failed".into()
    }
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
async fn best_effort_fetch_models(
    management: &ManagementStore,
    proxies: &ProxyStore,
    channel_id: u64,
) -> Option<String> {
    let probe = ChannelProbeService::new(management.clone(), proxies.clone());
    let models = probe
        .call(FetchModelsRequest {
            channel_id:      Some(channel_id),
            base_url:        String::new(),
            channel_type:    1,
            key:             String::new(),
            header_override: None,
            setting:         None,
            proxy_id:        None,
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
    proxies: &ProxyStore,
    content: &str,
    file_name: &str,
    group: &str,
    models: &str,
    models_were_explicit: bool,
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
            let imported_status = auth_file_status(&meta);
            // Reuse flexible codex key parser (CLIProxy flat shape is a subset).
            let key = parse_flexible_codex_key(content)?;
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

            if let Some(existing_id) =
                find_existing_cliproxy_codex_channel(existing, &item, file_name)
            {
                if update_existing {
                    let mut channel = existing
                        .iter()
                        .find(|c| c.id == existing_id)
                        .cloned()
                        .ok_or(ManagementError::NotFound)?;
                    channel.key = codex_key_to_json(&merge_codex_oauth_key(&channel.key, key))?;
                    channel.channel_type = CHANNEL_TYPE_CODEX;
                    channel.name = channel_name.clone();
                    channel.status = status_after_auth_file_import(channel.status, imported_status);
                    channel.setting = Some(merge_auth_file_identity_setting(
                        channel.setting.as_deref(),
                        "codex",
                        file_name,
                    )?);
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

            let key_json = codex_key_to_json(&key)?;
            let channel = ChannelRecord {
                id: 0,
                snapshot_id: None,
                channel_type: CHANNEL_TYPE_CODEX,
                key: key_json,
                status: imported_status,
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
                setting: Some(merge_auth_file_identity_setting(None, "codex", file_name)?),
                param_override: None,
                header_override: None,
                remark: Some(format!("imported from CLIProxyAPI ({file_name})")),
                proxy_id,
            };
            let created = management.call(CreateChannelRequest { channel }).await?;
            if models.trim().is_empty() {
                let _ = best_effort_fetch_models(management, proxies, created.id).await;
            }
            aggregate.created = aggregate.created.saturating_add(1);
            aggregate.items.push(CodexAuthImportItem {
                index:      idx,
                name:       channel_name,
                action:     "created".into(),
                channel_id: Some(created.id),
                message:    String::new(),
            });
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
            import_cliproxy_claude(
                management,
                &meta,
                file_name,
                &label,
                group,
                models,
                models_were_explicit,
                base_url,
                proxy_id,
                update_existing,
                existing,
                index,
                aggregate,
            )
            .await
        }
        "gemini" | "gemini-cli" => {
            let key = parse_cliproxy_gemini_api_key(&provider, &meta)?;
            let models = resolve_gemini_models(models, models_were_explicit);
            let imported_status = auth_file_status(&meta);
            if let Some(existing_id) = find_existing_gemini_channel(existing, &key, file_name) {
                if !update_existing {
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
                let mut channel = existing
                    .iter()
                    .find(|channel| channel.id == existing_id)
                    .cloned()
                    .ok_or(ManagementError::NotFound)?;
                channel.key = key;
                channel.channel_type = CHANNEL_TYPE_GEMINI;
                channel.name = label.clone();
                channel.status = status_after_auth_file_import(channel.status, imported_status);
                channel.models = models;
                channel.base_url = base_url.map(str::to_string);
                channel.setting = Some(merge_auth_file_identity_setting(
                    channel.setting.as_deref(),
                    "gemini",
                    file_name,
                )?);
                channel.remark = Some(format!("imported from CLIProxyAPI gemini ({file_name})"));
                if let Some(proxy_id) = proxy_id {
                    channel.proxy_id = Some(proxy_id);
                }
                let updated = management.call(UpdateChannelRequest { channel }).await?;
                aggregate.updated = aggregate.updated.saturating_add(1);
                aggregate.items.push(CodexAuthImportItem {
                    index:      idx,
                    name:       label.clone(),
                    action:     "updated".into(),
                    channel_id: Some(updated.id),
                    message:    String::new(),
                });
                return Ok(AuthFileResult {
                    name:       file_name.into(),
                    format:     "cliproxy".into(),
                    ok:         true,
                    message:    "updated gemini channel".into(),
                    channel_id: Some(updated.id),
                    created:    None,
                    updated:    Some(1),
                });
            }
            let channel = ChannelRecord {
                id: 0,
                snapshot_id: None,
                channel_type: CHANNEL_TYPE_GEMINI,
                key,
                status: imported_status,
                name: label.clone(),
                weight: Some(1),
                created_time: now_unix(),
                test_time: 0,
                response_time: 0,
                base_url: base_url.map(str::to_string),
                balance: 0.0,
                balance_updated_time: 0,
                models: models.clone(),
                group: group.to_string(),
                used_quota: 0,
                model_mapping: None,
                priority: Some(0),
                auto_ban: Some(1),
                tag: None,
                setting: Some(merge_auth_file_identity_setting(None, "gemini", file_name)?),
                param_override: None,
                header_override: None,
                remark: Some(format!("imported from CLIProxyAPI gemini ({file_name})")),
                proxy_id,
            };
            let created = management.call(CreateChannelRequest { channel }).await?;
            if models.trim().is_empty() {
                let _ = best_effort_fetch_models(management, proxies, created.id).await;
            }
            aggregate.created = aggregate.created.saturating_add(1);
            aggregate.items.push(CodexAuthImportItem {
                index:      idx,
                name:       label.clone(),
                action:     "created".into(),
                channel_id: Some(created.id),
                message:    String::new(),
            });
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
                models_were_explicit,
                base_url,
                proxy_id,
                update_existing,
                existing,
                index,
                aggregate,
            )
            .await
        }
        _ => Err(ManagementError::InvalidRequest(
            UNSUPPORTED_CLIPROXY_AUTH_TYPE,
        )),
    }
}

fn parse_cliproxy_gemini_api_key(
    provider: &str,
    meta: &JsonMap<String, JsonValue>,
) -> Result<String, ManagementError> {
    if !provider.eq_ignore_ascii_case("gemini") {
        return Err(ManagementError::InvalidRequest(
            "Gemini OAuth import is unsupported: CLIProxyAPI gemini-cli requires an external \
             PluginAuthParser",
        ));
    }
    if let Some(api_key) = str_field(meta, "api_key") {
        return Ok(api_key);
    }
    if str_field(meta, "access_token").is_some() || str_field(meta, "refresh_token").is_some() {
        return Err(ManagementError::InvalidRequest(
            "Gemini OAuth import is unsupported: CLIProxyAPI delegates OAuth auth files to an \
             external PluginAuthParser",
        ));
    }
    Err(ManagementError::InvalidRequest(
        "gemini API-key auth file missing api_key",
    ))
}

fn resolve_gemini_models(models: &str, models_were_explicit: bool) -> String {
    if models_were_explicit && !models.trim().is_empty() {
        models.trim().to_string()
    } else {
        GEMINI_DEFAULT_MODELS.to_string()
    }
}

fn auth_file_status(meta: &JsonMap<String, JsonValue>) -> i32 {
    if bool_field(meta, "disabled").unwrap_or(false) {
        STATUS_MANUALLY_DISABLED
    } else {
        STATUS_ENABLED
    }
}

fn status_after_auth_file_import(current: i32, imported: i32) -> i32 {
    if imported != STATUS_ENABLED {
        return imported;
    }
    if current == STATUS_ENABLED || current == STATUS_AUTO_DISABLED {
        STATUS_ENABLED
    } else {
        // A credential refresh may recover an auto-disabled channel, but it
        // must never undo a user's explicit/manual disable.
        current
    }
}

fn normalized_auth_file_name(file_name: &str) -> String {
    file_name
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn merge_auth_file_identity_setting(
    existing: Option<&str>,
    provider: &str,
    file_name: &str,
) -> Result<String, ManagementError> {
    let mut map = existing
        .and_then(|raw| serde_json::from_str::<JsonValue>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    map.insert(
        "import_source".into(),
        JsonValue::String("cliproxyapi".into()),
    );
    map.insert(
        "provider".into(),
        JsonValue::String(provider.to_ascii_lowercase()),
    );
    let auth_file = normalized_auth_file_name(file_name);
    if !auth_file.is_empty() {
        map.insert("auth_file".into(), JsonValue::String(auth_file));
    }
    serde_json::to_string(&JsonValue::Object(map))
        .map_err(|_| ManagementError::InvalidRequest("failed to serialize auth-file identity"))
}

fn find_existing_gemini_channel(
    existing: &[ChannelRecord],
    key: &str,
    file_name: &str,
) -> Option<u64> {
    first_conclusive_match([
        unique_channel_match(existing, |channel| {
            channel.channel_type == CHANNEL_TYPE_GEMINI && channel.key.trim() == key.trim()
        }),
        unique_channel_match(existing, |channel| {
            channel.channel_type == CHANNEL_TYPE_GEMINI
                && imported_auth_setting(channel, "gemini")
                    .is_some_and(|setting| setting_auth_file_matches(&setting, file_name))
        }),
    ])
}

fn find_existing_cliproxy_codex_channel(
    existing: &[ChannelRecord],
    item: &codex_auth_import::ParsedCodexAuth,
    file_name: &str,
) -> Option<u64> {
    let exact_access = unique_channel_match(existing, |channel| {
        channel.channel_type == CHANNEL_TYPE_CODEX
            && codex_auth_import::identity_keys_for_channel_key(&channel.key)
                .iter()
                .filter(|key| key.starts_with("access:"))
                .any(|key| item.identity_keys.iter().any(|incoming| incoming == key))
    });
    let source_file = unique_channel_match(existing, |channel| {
        channel.channel_type == CHANNEL_TYPE_CODEX
            && imported_auth_setting(channel, "codex")
                .is_some_and(|setting| setting_auth_file_matches(&setting, file_name))
    });
    match first_conclusive_match([exact_access, source_file]) {
        Some(id) => Some(id),
        None if exact_access == IdentityMatch::Ambiguous
            || source_file == IdentityMatch::Ambiguous =>
        {
            None
        }
        None => find_existing_channel_id(existing, item),
    }
}

async fn import_cliproxy_claude(
    management: &ManagementStore,
    meta: &JsonMap<String, JsonValue>,
    file_name: &str,
    label: &str,
    group: &str,
    models: &str,
    models_were_explicit: bool,
    base_url: Option<&str>,
    proxy_id: Option<u64>,
    update_existing: bool,
    existing: &[ChannelRecord],
    index: &mut usize,
    aggregate: &mut CodexAuthImportResult,
) -> Result<AuthFileResult, ManagementError> {
    let parsed = parse_cliproxy_claude_auth(meta)?;
    let channel_name = if label.is_empty() {
        file_name.trim_end_matches(".json").to_string()
    } else {
        label.to_string()
    };
    let models = resolve_claude_models(models, models_were_explicit);
    let resolved_base = base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| parsed.base_url.clone());
    let setting = build_claude_channel_setting(&parsed, file_name)?;
    let header_override = build_claude_header_override(&parsed)?;
    let status = auth_file_status(meta);

    if let Some(existing_id) = find_existing_claude_channel(existing, &parsed, file_name) {
        if update_existing {
            let mut channel = existing
                .iter()
                .find(|channel| channel.id == existing_id)
                .cloned()
                .ok_or(ManagementError::NotFound)?;
            channel.key = parsed.access_token.clone();
            channel.channel_type = CHANNEL_TYPE_ANTHROPIC;
            channel.name = channel_name.clone();
            channel.status = status_after_auth_file_import(channel.status, status);
            channel.models = models;
            channel.base_url = resolved_base;
            // Rebuild setting so stale auto-ban status_reason/status_time do not survive re-import.
            channel.setting = Some(setting);
            channel.header_override = header_override;
            channel.remark = Some(format!("imported from CLIProxyAPI claude ({file_name})"));
            if let Some(proxy_id) = proxy_id {
                channel.proxy_id = Some(proxy_id);
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
                message:    "updated claude channel".into(),
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
        channel_type: CHANNEL_TYPE_ANTHROPIC,
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
        remark: Some(format!("imported from CLIProxyAPI claude ({file_name})")),
        proxy_id,
    };
    let created = management.call(CreateChannelRequest { channel }).await?;
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
        message:    "created claude channel".into(),
        channel_id: Some(created.id),
        created:    Some(1),
        updated:    None,
    })
}

#[derive(Debug, Clone)]
struct ParsedClaudeAuth {
    access_token:    String,
    refresh_token:   Option<String>,
    email:           Option<String>,
    account_id:      Option<String>,
    organization_id: Option<String>,
    auth_kind:       String,
    is_oauth:        bool,
    base_url:        Option<String>,
    token_endpoint:  Option<String>,
    token_type:      Option<String>,
    id_token:        Option<String>,
    scope:           Option<String>,
    expired:         Option<String>,
    last_refresh:    Option<String>,
    headers:         Option<JsonMap<String, JsonValue>>,
}

fn parse_cliproxy_claude_auth(
    meta: &JsonMap<String, JsonValue>,
) -> Result<ParsedClaudeAuth, ManagementError> {
    let explicit_kind = str_field(meta, "auth_kind").map(|kind| kind.to_ascii_lowercase());
    let (auth_kind, is_oauth) = match explicit_kind.as_deref() {
        Some("oauth") => ("oauth".to_string(), true),
        Some("setup-token" | "setup_token") => ("setup-token".to_string(), true),
        Some("apikey" | "api_key" | "api-key" | "upstream") => ("api_key".to_string(), false),
        Some(_) => {
            return Err(ManagementError::InvalidRequest(
                "unsupported claude auth_kind",
            ));
        }
        None if str_field(meta, "refresh_token").is_some()
            || str_field(meta, "access_token").is_some()
            || str_field(meta, "api_key").is_some_and(|key| key.starts_with("sk-ant-oat")) =>
        {
            ("oauth".to_string(), true)
        }
        None => ("api_key".to_string(), false),
    };
    let access_token = if is_oauth {
        str_field(meta, "access_token").or_else(|| str_field(meta, "api_key"))
    } else {
        str_field(meta, "api_key").or_else(|| str_field(meta, "access_token"))
    }
    .ok_or(ManagementError::InvalidRequest(
        "claude auth file missing access_token/api_key",
    ))?;
    let base_url = str_field(meta, "base_url");
    let headers = meta.get("headers").and_then(JsonValue::as_object).cloned();
    Ok(ParsedClaudeAuth {
        access_token,
        refresh_token: str_field(meta, "refresh_token"),
        email: str_field(meta, "email"),
        account_id: str_field(meta, "account_uuid")
            .or_else(|| str_field(meta, "account_id"))
            .or_else(|| str_field(meta, "accountId")),
        organization_id: str_field(meta, "org_uuid")
            .or_else(|| str_field(meta, "organization_id"))
            .or_else(|| str_field(meta, "organizationId")),
        auth_kind,
        is_oauth,
        base_url,
        token_endpoint: str_field(meta, "token_endpoint"),
        token_type: str_field(meta, "token_type"),
        id_token: str_field(meta, "id_token").or_else(|| str_field(meta, "idToken")),
        scope: str_field(meta, "scope"),
        expired: scalar_field(meta, "expired")
            .or_else(|| scalar_field(meta, "expire"))
            .or_else(|| scalar_field(meta, "expires_at"))
            .or_else(|| scalar_field(meta, "expiry")),
        last_refresh: scalar_field(meta, "last_refresh"),
        headers,
    })
}

fn build_claude_channel_setting(
    parsed: &ParsedClaudeAuth,
    file_name: &str,
) -> Result<String, ManagementError> {
    let base = merge_auth_file_identity_setting(None, "claude", file_name)?;
    let mut map = serde_json::from_str::<JsonValue>(&base)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    map.insert(
        "auth_kind".into(),
        JsonValue::String(parsed.auth_kind.clone()),
    );
    for (key, value) in [
        ("refresh_token", parsed.refresh_token.as_ref()),
        ("email", parsed.email.as_ref()),
        ("account_id", parsed.account_id.as_ref()),
        ("organization_id", parsed.organization_id.as_ref()),
        ("token_endpoint", parsed.token_endpoint.as_ref()),
        ("token_type", parsed.token_type.as_ref()),
        ("id_token", parsed.id_token.as_ref()),
        ("scope", parsed.scope.as_ref()),
        ("expired", parsed.expired.as_ref()),
        ("last_refresh", parsed.last_refresh.as_ref()),
    ] {
        if let Some(value) = value {
            map.insert(key.into(), JsonValue::String(value.clone()));
        }
    }
    serde_json::to_string(&JsonValue::Object(map))
        .map_err(|_| ManagementError::InvalidRequest("failed to serialize claude channel setting"))
}

fn build_claude_header_override(
    parsed: &ParsedClaudeAuth,
) -> Result<Option<String>, ManagementError> {
    let mut map = JsonMap::new();
    if parsed.is_oauth {
        map.insert(
            "Authorization".into(),
            JsonValue::String("Bearer {api_key}".into()),
        );
        map.insert(
            "Anthropic-Beta".into(),
            JsonValue::String(CLAUDE_OAUTH_BETA.into()),
        );
        map.insert(
            "Anthropic-Version".into(),
            JsonValue::String(CLAUDE_API_VERSION.into()),
        );
        map.insert("X-App".into(), JsonValue::String("cli".into()));
    }
    if let Some(headers) = &parsed.headers {
        for (key, value) in headers {
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            let value = match value {
                JsonValue::String(value) if !value.trim().is_empty() => {
                    Some(JsonValue::String(value.trim().to_string()))
                }
                JsonValue::Number(value) => Some(JsonValue::String(value.to_string())),
                JsonValue::Bool(value) => Some(JsonValue::String(value.to_string())),
                _ => None,
            };
            if let Some(value) = value {
                insert_claude_header_override(&mut map, key, value);
            }
        }
    }
    if map.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(&JsonValue::Object(map))
        .map(Some)
        .map_err(|_| ManagementError::InvalidRequest("failed to serialize claude header_override"))
}

fn insert_claude_header_override(
    headers: &mut JsonMap<String, JsonValue>,
    key: &str,
    value: JsonValue,
) {
    let key = headers
        .keys()
        .find(|existing| existing.eq_ignore_ascii_case(key))
        .cloned()
        .unwrap_or_else(|| key.to_string());
    headers.insert(key, value);
}

fn resolve_claude_models(models: &str, models_were_explicit: bool) -> String {
    if models_were_explicit && !models.trim().is_empty() {
        models.trim().to_string()
    } else {
        CLAUDE_DEFAULT_MODELS.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityMatch {
    None,
    Unique(u64),
    Ambiguous,
}

fn unique_channel_match(
    existing: &[ChannelRecord],
    mut predicate: impl FnMut(&ChannelRecord) -> bool,
) -> IdentityMatch {
    let mut ids = existing
        .iter()
        .filter(|channel| predicate(channel))
        .map(|channel| channel.id);
    let Some(id) = ids.next() else {
        return IdentityMatch::None;
    };
    if ids.next().is_some() {
        IdentityMatch::Ambiguous
    } else {
        IdentityMatch::Unique(id)
    }
}

fn first_conclusive_match<const N: usize>(matches: [IdentityMatch; N]) -> Option<u64> {
    for identity_match in matches {
        match identity_match {
            IdentityMatch::None => continue,
            IdentityMatch::Unique(id) => return Some(id),
            // Never fall through from an ambiguous strong identity to a
            // weaker one (for example from duplicate tokens to an email).
            IdentityMatch::Ambiguous => return None,
        }
    }
    None
}

fn imported_auth_setting(channel: &ChannelRecord, provider: &str) -> Option<JsonValue> {
    let value = serde_json::from_str::<JsonValue>(channel.setting.as_deref()?).ok()?;
    let import_source = value.get("import_source")?.as_str()?;
    if !import_source.eq_ignore_ascii_case("cliproxyapi") {
        return None;
    }
    let stored_provider = value.get("provider")?.as_str()?;
    stored_provider
        .eq_ignore_ascii_case(provider)
        .then_some(value)
}

fn setting_auth_file_matches(setting: &JsonValue, file_name: &str) -> bool {
    let incoming = normalized_auth_file_name(file_name);
    !incoming.is_empty()
        && setting
            .get("auth_file")
            .and_then(JsonValue::as_str)
            .map(normalized_auth_file_name)
            .is_some_and(|stored| stored == incoming)
}

fn setting_string_matches(setting: &JsonValue, field: &str, expected: Option<&str>) -> bool {
    let Some(expected) = expected.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    setting
        .get(field)
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .is_some_and(|stored| stored.eq_ignore_ascii_case(expected))
}

fn find_existing_claude_channel(
    existing: &[ChannelRecord],
    parsed: &ParsedClaudeAuth,
    file_name: &str,
) -> Option<u64> {
    first_conclusive_match([
        unique_channel_match(existing, |channel| {
            channel.channel_type == CHANNEL_TYPE_ANTHROPIC
                && channel.key.trim() == parsed.access_token
        }),
        unique_channel_match(existing, |channel| {
            channel.channel_type == CHANNEL_TYPE_ANTHROPIC
                && imported_auth_setting(channel, "claude")
                    .is_some_and(|setting| setting_auth_file_matches(&setting, file_name))
        }),
        unique_channel_match(existing, |channel| {
            channel.channel_type == CHANNEL_TYPE_ANTHROPIC
                && imported_auth_setting(channel, "claude").is_some_and(|setting| {
                    setting_string_matches(&setting, "account_id", parsed.account_id.as_deref())
                })
        }),
        unique_channel_match(existing, |channel| {
            channel.channel_type == CHANNEL_TYPE_ANTHROPIC
                && imported_auth_setting(channel, "claude").is_some_and(|setting| {
                    setting_string_matches(
                        &setting,
                        "refresh_token",
                        parsed.refresh_token.as_deref(),
                    )
                })
        }),
        unique_channel_match(existing, |channel| {
            channel.channel_type == CHANNEL_TYPE_ANTHROPIC
                && imported_auth_setting(channel, "claude").is_some_and(|setting| {
                    setting_string_matches(&setting, "email", parsed.email.as_deref())
                        && setting_string_matches(
                            &setting,
                            "auth_kind",
                            Some(parsed.auth_kind.as_str()),
                        )
                })
        }),
    ])
}

async fn import_cliproxy_xai(
    management: &ManagementStore,
    meta: &JsonMap<String, JsonValue>,
    file_name: &str,
    label: &str,
    group: &str,
    models: &str,
    models_were_explicit: bool,
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
    let models = resolve_xai_models(models, models_were_explicit);
    let resolved_base = Some(resolve_xai_base_url(
        parsed.using_api,
        base_url.or(parsed.base_url.as_deref()),
    ));
    let setting = build_xai_channel_setting(&parsed, file_name)?;
    let header_override = build_xai_header_override(&parsed, resolved_base.as_deref())?;
    let status = auth_file_status(meta);

    if let Some(existing_id) = find_existing_xai_channel(existing, &parsed, file_name) {
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
            channel.status = status_after_auth_file_import(channel.status, status);
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
    id_token:       Option<String>,
    token_type:     Option<String>,
    scope:          Option<String>,
    expires_in:     Option<String>,
    email:          Option<String>,
    subject:        Option<String>,
    auth_kind:      String,
    base_url:       Option<String>,
    redirect_uri:   Option<String>,
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
    if !matches!(
        auth_kind.as_str(),
        "oauth" | "api_key" | "apikey" | "api-key" | "upstream"
    ) {
        return Err(ManagementError::InvalidRequest("unsupported xai auth_kind"));
    }
    let using_api = bool_field(meta, "using_api").unwrap_or_else(|| auth_kind != "oauth");
    let file_base = str_field(meta, "base_url");
    let base_url = Some(resolve_xai_base_url(using_api, file_base.as_deref()));
    let token_endpoint = str_field(meta, "token_endpoint")
        .map(|endpoint| validate_xai_oauth_endpoint(&endpoint))
        .transpose()?;
    let headers = meta.get("headers").and_then(|v| v.as_object()).cloned();
    Ok(ParsedXaiAuth {
        access_token,
        refresh_token: str_field(meta, "refresh_token"),
        id_token: str_field(meta, "id_token").or_else(|| str_field(meta, "idToken")),
        token_type: str_field(meta, "token_type").or_else(|| str_field(meta, "tokenType")),
        scope: str_field(meta, "scope"),
        expires_in: scalar_field(meta, "expires_in").or_else(|| scalar_field(meta, "expiresIn")),
        email: str_field(meta, "email"),
        subject: str_field(meta, "sub").or_else(|| str_field(meta, "subject")),
        auth_kind,
        base_url,
        redirect_uri: str_field(meta, "redirect_uri").or_else(|| str_field(meta, "redirectUri")),
        token_endpoint,
        expired: scalar_field(meta, "expired")
            .or_else(|| scalar_field(meta, "expire"))
            .or_else(|| scalar_field(meta, "expires_at")),
        last_refresh: scalar_field(meta, "last_refresh")
            .or_else(|| scalar_field(meta, "lastRefresh")),
        using_api,
        headers,
    })
}

fn build_xai_channel_setting(
    parsed: &ParsedXaiAuth,
    file_name: &str,
) -> Result<String, ManagementError> {
    let base = merge_auth_file_identity_setting(None, "xai", file_name)?;
    let mut map = serde_json::from_str::<JsonValue>(&base)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    map.insert(
        "auth_kind".into(),
        JsonValue::String(parsed.auth_kind.clone()),
    );
    map.insert("using_api".into(), JsonValue::Bool(parsed.using_api));
    map.insert(
        "upstream_endpoint_type".into(),
        JsonValue::String("xai-response".into()),
    );
    if let Some(email) = &parsed.email {
        map.insert("email".into(), JsonValue::String(email.clone()));
    }
    if let Some(sub) = &parsed.subject {
        map.insert("sub".into(), JsonValue::String(sub.clone()));
    }
    if let Some(refresh) = &parsed.refresh_token {
        map.insert("refresh_token".into(), JsonValue::String(refresh.clone()));
    }
    for (key, value) in [
        ("id_token", parsed.id_token.as_ref()),
        ("token_type", parsed.token_type.as_ref()),
        ("scope", parsed.scope.as_ref()),
        ("expires_in", parsed.expires_in.as_ref()),
        ("redirect_uri", parsed.redirect_uri.as_ref()),
    ] {
        if let Some(value) = value {
            map.insert(key.into(), JsonValue::String(value.clone()));
        }
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
        map.insert(
            "User-Agent".into(),
            JsonValue::String(XAI_USER_AGENT.into()),
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
                    insert_xai_header_override(
                        &mut map,
                        key,
                        JsonValue::String(s.trim().to_string()),
                    );
                }
                JsonValue::Number(n) => {
                    insert_xai_header_override(&mut map, key, JsonValue::String(n.to_string()));
                }
                JsonValue::Bool(b) => {
                    insert_xai_header_override(&mut map, key, JsonValue::String(b.to_string()));
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

fn insert_xai_header_override(
    headers: &mut JsonMap<String, JsonValue>,
    key: &str,
    value: JsonValue,
) {
    let key = headers
        .keys()
        .find(|existing| existing.eq_ignore_ascii_case(key))
        .cloned()
        .unwrap_or_else(|| key.to_string());
    headers.insert(key, value);
}

fn resolve_xai_models(models: &str, models_were_explicit: bool) -> String {
    if models_were_explicit && !models.trim().is_empty() {
        models.trim().to_string()
    } else {
        XAI_DEFAULT_MODELS.to_string()
    }
}

fn resolve_xai_base_url(using_api: bool, base_url: Option<&str>) -> String {
    let base_url = base_url
        .map(trim_xai_base_url)
        .filter(|base| !base.is_empty());
    match base_url {
        Some(base) if is_xai_default_api_base(&base) && !using_api => {
            XAI_DEFAULT_CLI_CHAT_BASE.to_string()
        }
        Some(base) if is_xai_default_api_base(&base) => XAI_DEFAULT_API_BASE.to_string(),
        Some(base) if is_xai_cli_chat_proxy_base(&base) => XAI_DEFAULT_CLI_CHAT_BASE.to_string(),
        Some(base) => base,
        None if using_api => XAI_DEFAULT_API_BASE.to_string(),
        None => XAI_DEFAULT_CLI_CHAT_BASE.to_string(),
    }
}

fn is_xai_default_api_base(base_url: &str) -> bool {
    let base_url = trim_xai_base_url(base_url);
    base_url.eq_ignore_ascii_case(XAI_DEFAULT_API_BASE)
        || base_url.eq_ignore_ascii_case("https://api.x.ai")
}

fn is_xai_cli_chat_proxy_base(base_url: &str) -> bool {
    let base_url = trim_xai_base_url(base_url);
    base_url.eq_ignore_ascii_case(XAI_DEFAULT_CLI_CHAT_BASE)
        || base_url.eq_ignore_ascii_case("https://cli-chat-proxy.grok.com")
}

fn trim_xai_base_url(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

fn find_existing_xai_channel(
    existing: &[ChannelRecord],
    parsed: &ParsedXaiAuth,
    file_name: &str,
) -> Option<u64> {
    first_conclusive_match([
        unique_channel_match(existing, |channel| {
            channel.channel_type == CHANNEL_TYPE_XAI && channel.key.trim() == parsed.access_token
        }),
        unique_channel_match(existing, |channel| {
            channel.channel_type == CHANNEL_TYPE_XAI
                && imported_auth_setting(channel, "xai")
                    .is_some_and(|setting| setting_auth_file_matches(&setting, file_name))
        }),
        unique_channel_match(existing, |channel| {
            channel.channel_type == CHANNEL_TYPE_XAI
                && imported_auth_setting(channel, "xai").is_some_and(|setting| {
                    setting_string_matches(&setting, "sub", parsed.subject.as_deref())
                })
        }),
        unique_channel_match(existing, |channel| {
            channel.channel_type == CHANNEL_TYPE_XAI
                && imported_auth_setting(channel, "xai").is_some_and(|setting| {
                    setting_string_matches(
                        &setting,
                        "refresh_token",
                        parsed.refresh_token.as_deref(),
                    )
                })
        }),
        unique_channel_match(existing, |channel| {
            channel.channel_type == CHANNEL_TYPE_XAI
                && imported_auth_setting(channel, "xai").is_some_and(|setting| {
                    setting_string_matches(&setting, "email", parsed.email.as_deref())
                        && setting_string_matches(
                            &setting,
                            "auth_kind",
                            Some(parsed.auth_kind.as_str()),
                        )
                })
        }),
    ])
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
    outer_ordinal: usize,
    seen: &mut HashMap<String, usize>,
    index: &mut usize,
    aggregate: &mut CodexAuthImportResult,
) -> Result<(AuthFileResult, usize), ManagementError> {
    let entries = collect_entry_results(req)?;
    let entry_count = entries.len();
    let mut created = 0usize;
    let mut updated = 0usize;
    let mut failed = 0usize;
    let mut last_id = None;
    let group = req.group.as_deref().unwrap_or("default").to_string();
    let models = req
        .models
        .as_deref()
        .unwrap_or("gpt-5.1,gpt-5,o3,o4-mini")
        .to_string();

    for (entry_offset, item) in entries.into_iter().enumerate() {
        let source_ordinal = if entry_count == 1 {
            outer_ordinal
        } else {
            entry_offset.saturating_add(1)
        };
        let item = match item {
            Ok(item) => item,
            Err(err) => {
                failed = failed.saturating_add(1);
                record_codex_item_failure(
                    aggregate,
                    index,
                    source_ordinal,
                    public_codex_import_error(&err),
                );
                continue;
            }
        };
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

        let account_name = if req.name.trim().is_empty() {
            item.name.clone()
        } else {
            req.name.trim().to_string()
        };

        // The store is the authoritative working set. Re-reading here makes a
        // create/update immediately visible to the next semantic entry in the
        // same JSON array or NDJSON stream.
        let existing = management.current_data()?.channels;
        if let Some(existing_id) = find_existing_channel_id(&existing, &item) {
            if req.update_existing {
                let mut channel = existing
                    .iter()
                    .find(|c| c.id == existing_id)
                    .cloned()
                    .ok_or(ManagementError::NotFound)?;
                channel.key =
                    codex_key_to_json(&merge_codex_oauth_key(&channel.key, item.key.clone()))?;
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

        let key_json = codex_key_to_json(&item.key)?;
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

    Ok((
        AuthFileResult {
            name:       file_name.into(),
            format:     "codex-session".into(),
            ok:         failed == 0,
            message:    format!("created {created}, updated {updated}, failed {failed}"),
            channel_id: last_id,
            created:    Some(created),
            updated:    Some(updated),
        },
        failed,
    ))
}

fn str_field(map: &JsonMap<String, JsonValue>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn scalar_field(map: &JsonMap<String, JsonValue>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(|value| match value {
            JsonValue::String(value) => Some(value.trim().to_string()),
            JsonValue::Number(value) => Some(value.to_string()),
            _ => None,
        })
        .filter(|value| !value.is_empty())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

// silence unused CodexOAuthKey if only used via paths
#[allow(dead_code)]
fn _codex_key_placeholder() -> CodexOAuthKey {
    CodexOAuthKey::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use halolake_control_plane::ManagementData;

    fn codex_channel(id: u64, key: &str) -> ChannelRecord {
        ChannelRecord {
            id,
            snapshot_id: None,
            channel_type: CHANNEL_TYPE_CODEX,
            key: key.to_string(),
            status: STATUS_ENABLED,
            name: "codex account".into(),
            weight: Some(1),
            created_time: 0,
            test_time: 0,
            response_time: 0,
            base_url: None,
            balance: 0.0,
            balance_updated_time: 0,
            models: "gpt-5".into(),
            group: "default".into(),
            used_quota: 0,
            model_mapping: None,
            priority: Some(0),
            auto_ban: Some(1),
            tag: None,
            setting: Some(r#"{"import_source":"codex-session"}"#.into()),
            param_override: None,
            header_override: None,
            remark: None,
            proxy_id: None,
        }
    }

    fn empty_aggregate() -> CodexAuthImportResult {
        CodexAuthImportResult {
            total:    0,
            created:  0,
            updated:  0,
            skipped:  0,
            failed:   0,
            items:    Vec::new(),
            warnings: Vec::new(),
            errors:   Vec::new(),
        }
    }

    fn codex_request(content: String) -> CodexAuthImportRequest {
        CodexAuthImportRequest {
            content,
            contents: Vec::new(),
            name: String::new(),
            group: Some("default".into()),
            models: Some("gpt-5".into()),
            base_url: None,
            proxy_id: None,
            priority: None,
            weight: None,
            update_existing: true,
        }
    }

    fn auth_import_request(contents: Vec<&str>, filenames: Vec<&str>) -> AuthImportRequest {
        AuthImportRequest {
            format:          "cliproxy".into(),
            auth_method:     AuthMethod::Auto,
            content:         String::new(),
            contents:        contents.into_iter().map(str::to_string).collect(),
            filenames:       filenames.into_iter().map(str::to_string).collect(),
            name:            "shared display name".into(),
            group:           Some("default".into()),
            models:          None,
            base_url:        None,
            proxy_id:        None,
            update_existing: true,
            data:            None,
        }
    }

    #[tokio::test]
    async fn same_display_name_cannot_merge_or_disable_distinct_claude_files() {
        let management = ManagementStore::memory(ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        let request = auth_import_request(
            vec![
                r#"{"type":"claude","auth_kind":"oauth","access_token":"claude-one","refresh_token":"refresh-one","email":"one@example.com"}"#,
                r#"{"type":"claude","auth_kind":"oauth","access_token":"claude-two","refresh_token":"refresh-two","email":"two@example.com","disabled":true}"#,
            ],
            vec!["claude-one.json", "claude-two.json"],
        );

        let result = import_auth(&management, &ProxyStore::memory(), request)
            .await
            .expect("import two independent auth files");
        let summary = result.channels.expect("channel summary");
        assert_eq!(summary.created, 2);
        assert_eq!(summary.updated, 0);

        let channels = management.current_data().unwrap().channels;
        assert_eq!(channels.len(), 2);
        let first = channels
            .iter()
            .find(|channel| channel.key == "claude-one")
            .expect("first channel survives");
        let second = channels
            .iter()
            .find(|channel| channel.key == "claude-two")
            .expect("second channel exists");
        assert_eq!(first.status, STATUS_ENABLED);
        assert_eq!(second.status, STATUS_MANUALLY_DISABLED);
        let first_setting: JsonValue =
            serde_json::from_str(first.setting.as_deref().unwrap()).unwrap();
        let second_setting: JsonValue =
            serde_json::from_str(second.setting.as_deref().unwrap()).unwrap();
        assert_eq!(first_setting["auth_file"], "claude-one.json");
        assert_eq!(second_setting["auth_file"], "claude-two.json");
    }

    #[tokio::test]
    async fn content_and_contents_do_not_share_the_first_filename_identity() {
        let management = ManagementStore::memory(ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        let mut request = auth_import_request(
            vec![r#"{"type":"claude","access_token":"from-array","auth_kind":"api_key"}"#],
            vec!["array.json"],
        );
        request.content =
            r#"{"type":"claude","access_token":"from-content","auth_kind":"api_key"}"#.into();

        let result = import_auth(&management, &ProxyStore::memory(), request)
            .await
            .expect("mixed legacy envelope");
        let summary = result.channels.unwrap();
        assert_eq!(summary.created, 2);
        assert_eq!(summary.updated, 0);
        let channels = management.current_data().unwrap().channels;
        assert_eq!(channels.len(), 2);
        assert!(channels.iter().any(|channel| channel.key == "from-content"));
        assert!(channels.iter().any(|channel| channel.key == "from-array"));
    }

    #[tokio::test]
    async fn failed_auth_file_does_not_mutate_prior_or_later_channels() {
        let management = ManagementStore::memory(ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        let seed = auth_import_request(
            vec![
                r#"{"type":"xai","auth_kind":"oauth","access_token":"xai-durable","refresh_token":"refresh-durable","sub":"subject-durable"}"#,
            ],
            vec!["durable.json"],
        );
        import_auth(&management, &ProxyStore::memory(), seed)
            .await
            .expect("seed channel");

        let request = auth_import_request(
            vec![
                r#"{"type":"xai","auth_kind":"oauth","refresh_token":"broken","disabled":true}"#,
                r#"{"type":"xai","auth_kind":"oauth","access_token":"xai-later","refresh_token":"refresh-later","sub":"subject-later"}"#,
            ],
            vec!["durable.json", "later.json"],
        );
        let result = import_auth(&management, &ProxyStore::memory(), request)
            .await
            .expect("batch failure remains item scoped");
        let summary = result.channels.expect("channel summary");
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.created, 1);

        let channels = management.current_data().unwrap().channels;
        assert_eq!(channels.len(), 2);
        let durable = channels
            .iter()
            .find(|channel| channel.key == "xai-durable")
            .expect("durable channel survives invalid replacement");
        assert_eq!(durable.status, STATUS_ENABLED);
        assert!(channels.iter().any(|channel| channel.key == "xai-later"));
    }

    #[tokio::test]
    async fn gemini_upsert_uses_auth_file_not_display_name() {
        let management = ManagementStore::memory(ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        let first = auth_import_request(vec![r#"{"type":"gemini","api_key":"gemini-one"}"#], vec![
            "gemini-one.json",
        ]);
        import_auth(&management, &ProxyStore::memory(), first)
            .await
            .expect("first gemini import");
        let second = auth_import_request(
            vec![r#"{"type":"gemini","api_key":"gemini-two","disabled":true}"#],
            vec!["gemini-two.json"],
        );
        import_auth(&management, &ProxyStore::memory(), second)
            .await
            .expect("distinct gemini file");
        assert_eq!(management.current_data().unwrap().channels.len(), 2);

        let replacement = auth_import_request(
            vec![r#"{"type":"gemini","api_key":"gemini-one-rotated"}"#],
            vec!["gemini-one.json"],
        );
        let result = import_auth(&management, &ProxyStore::memory(), replacement)
            .await
            .expect("same auth file rotates one channel");
        let summary = result.channels.unwrap();
        assert_eq!(summary.updated, 1);
        assert_eq!(summary.created, 0);

        let channels = management.current_data().unwrap().channels;
        assert_eq!(channels.len(), 2);
        assert!(
            channels
                .iter()
                .any(|channel| channel.key == "gemini-one-rotated")
        );
        let second = channels
            .iter()
            .find(|channel| channel.key == "gemini-two")
            .expect("second file is untouched");
        assert_eq!(second.status, STATUS_MANUALLY_DISABLED);
    }

    #[tokio::test]
    async fn codex_auth_file_identity_handles_token_rotation_without_stable_claims() {
        let management = ManagementStore::memory(ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        let first = auth_import_request(
            vec![r#"{"type":"codex","access_token":"codex-old","refresh_token":"refresh-old"}"#],
            vec!["codex-account.json"],
        );
        import_auth(&management, &ProxyStore::memory(), first)
            .await
            .expect("first codex import");

        let rotated = auth_import_request(
            vec![
                r#"{"type":"codex","access_token":"codex-new","refresh_token":"refresh-new","disabled":true}"#,
            ],
            vec!["codex-account.json"],
        );
        let result = import_auth(&management, &ProxyStore::memory(), rotated)
            .await
            .expect("rotate same auth file");
        let summary = result.channels.unwrap();
        assert_eq!(summary.updated, 1);
        assert_eq!(summary.created, 0);

        let channels = management.current_data().unwrap().channels;
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].status, STATUS_MANUALLY_DISABLED);
        let key = parse_flexible_codex_key(&channels[0].key).unwrap();
        assert_eq!(key.access_token.as_deref(), Some("codex-new"));
        assert_eq!(key.refresh_token.as_deref(), Some("refresh-new"));
    }

    #[test]
    fn auth_method_defaults_to_auto_for_legacy_requests() {
        let request: AuthImportRequest =
            serde_json::from_str(r#"{"format":"codex-session","content":"token"}"#)
                .expect("request");
        assert_eq!(request.auth_method, AuthMethod::Auto);

        let request: AuthImportRequest =
            serde_json::from_str(r#"{"auth_method":"mobile_refresh_token","content":"token"}"#)
                .expect("request");
        assert_eq!(request.auth_method, AuthMethod::MobileRefreshToken);
    }

    #[test]
    fn later_expansion_failure_preserves_prior_success_in_aggregate() {
        let mut aggregate = CodexAuthImportResult {
            total:    1,
            created:  1,
            updated:  0,
            skipped:  0,
            failed:   0,
            items:    vec![CodexAuthImportItem {
                index:      1,
                name:       "imported account".into(),
                action:     "created".into(),
                channel_id: Some(7),
                message:    String::new(),
            }],
            warnings: Vec::new(),
            errors:   Vec::new(),
        };
        let mut index = 1;

        record_codex_item_failure(
            &mut aggregate,
            &mut index,
            2,
            "refresh token exchange failed".into(),
        );

        assert_eq!(aggregate.created, 1);
        assert_eq!(aggregate.failed, 1);
        assert_eq!(aggregate.items.len(), 2);
        assert_eq!(aggregate.items[0].action, "created");
        assert_eq!(aggregate.items[1].action, "failed");
        assert_eq!(aggregate.items[1].name, "credential 2");
        assert_eq!(aggregate.errors.len(), 1);
    }

    #[tokio::test]
    async fn cliproxy_access_only_update_preserves_refresh_material() {
        let stored = r#"{"type":"codex","access_token":"access-old","refresh_token":"refresh-durable","client_id":"mobile-client","id_token":"id-durable","account_id":"acct-1","email":"owner@example.com","expired":"1999999999","last_refresh":"2026-07-15T00:00:00Z"}"#;
        let management = ManagementStore::memory(ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            vec![codex_channel(7, stored)],
            Vec::new(),
        ));
        let incoming = r#"{"type":"codex","access_token":"access-old","account_id":"acct-1","expired":"1999999998"}"#;
        let existing = management.current_data().unwrap().channels;
        let mut seen = HashMap::new();
        let mut index = 0;
        let mut aggregate = empty_aggregate();

        let result = import_cliproxy_file(
            &management,
            &ProxyStore::memory(),
            incoming,
            "codex.json",
            "default",
            "gpt-5",
            true,
            None,
            None,
            true,
            "",
            &existing,
            &mut seen,
            &mut index,
            &mut aggregate,
        )
        .await
        .expect("update CLIProxy Codex channel");

        assert_eq!(result.updated, Some(1));
        let channel = management.current_data().unwrap().channels.remove(0);
        let key = parse_flexible_codex_key(&channel.key).unwrap();
        assert_eq!(key.access_token.as_deref(), Some("access-old"));
        assert_eq!(key.refresh_token.as_deref(), Some("refresh-durable"));
        assert_eq!(key.client_id.as_deref(), Some("mobile-client"));
        assert_eq!(key.id_token.as_deref(), Some("id-durable"));
        assert_eq!(key.last_refresh.as_deref(), Some("2026-07-15T00:00:00Z"));
    }

    #[tokio::test]
    async fn codex_exact_token_wins_over_an_earlier_same_account_channel() {
        let stale = r#"{"type":"codex","access_token":"access-stale","refresh_token":"refresh-stale","account_id":"acct-shared","expired":"1999999900"}"#;
        let exact = r#"{"type":"codex","access_token":"access-exact","refresh_token":"refresh-old","account_id":"acct-shared","expired":"1999999900"}"#;
        let management = ManagementStore::memory(ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            vec![codex_channel(7, stale), codex_channel(8, exact)],
            Vec::new(),
        ));
        let req = codex_request(
            r#"{"access_token":"access-exact","refresh_token":"refresh-new","account_id":"acct-shared","expired":"1999999999"}"#
                .into(),
        );
        let mut seen = HashMap::new();
        let mut index = 0;
        let mut aggregate = empty_aggregate();

        import_codex_blob(
            &management,
            &req,
            "session.json",
            1,
            &mut seen,
            &mut index,
            &mut aggregate,
        )
        .await
        .expect("ranked identity update");

        let channels = management.current_data().unwrap().channels;
        let stale = channels.iter().find(|channel| channel.id == 7).unwrap();
        let exact = channels.iter().find(|channel| channel.id == 8).unwrap();
        let stale_key = parse_flexible_codex_key(&stale.key).unwrap();
        let exact_key = parse_flexible_codex_key(&exact.key).unwrap();
        assert_eq!(stale_key.refresh_token.as_deref(), Some("refresh-stale"));
        assert_eq!(exact_key.refresh_token.as_deref(), Some("refresh-new"));
    }

    #[tokio::test]
    async fn later_stale_access_entry_cannot_roll_back_rotated_codex_key() {
        let stored = r#"{"type":"codex","access_token":"access-old","refresh_token":"refresh-old","client_id":"client-old","account_id":"acct-1","expired":"1999999900"}"#;
        let management = ManagementStore::memory(ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            vec![codex_channel(7, stored)],
            Vec::new(),
        ));
        let content = r#"[
          {"access_token":"access-rotated","refresh_token":"refresh-rotated","client_id":"client-rotated","account_id":"acct-1","expired":"1999999999"},
          {"access_token":"access-stale","account_id":"acct-1","expired":"1999999998"}
        ]"#;
        let req = codex_request(content.into());
        let mut seen = HashMap::new();
        let mut index = 0;
        let mut aggregate = empty_aggregate();

        let (result, failed) = import_codex_blob(
            &management,
            &req,
            "sessions.json",
            1,
            &mut seen,
            &mut index,
            &mut aggregate,
        )
        .await
        .expect("import sessions");

        assert_eq!(failed, 0);
        assert_eq!(result.updated, Some(1));
        assert_eq!(result.created, Some(1));
        assert_eq!(aggregate.updated, 1);
        assert_eq!(aggregate.created, 1);
        assert_eq!(aggregate.skipped, 0);
        let channels = management.current_data().unwrap().channels;
        assert_eq!(channels.len(), 2);
        let channel = channels.iter().find(|channel| channel.id == 7).unwrap();
        let key = parse_flexible_codex_key(&channel.key).unwrap();
        assert_eq!(key.access_token.as_deref(), Some("access-rotated"));
        assert_eq!(key.refresh_token.as_deref(), Some("refresh-rotated"));
        assert_eq!(key.client_id.as_deref(), Some("client-rotated"));
        let stale = channels.iter().find(|channel| channel.id != 7).unwrap();
        let stale_key = parse_flexible_codex_key(&stale.key).unwrap();
        assert_eq!(stale_key.access_token.as_deref(), Some("access-stale"));
        assert!(stale_key.refresh_token.is_none());
    }

    #[tokio::test]
    async fn session_access_only_update_preserves_refresh_material() {
        let stored = r#"{"type":"codex","access_token":"access-same","refresh_token":"refresh-durable","client_id":"mobile-client","id_token":"id-durable","account_id":"acct-1","expired":"1999999900","last_refresh":"2026-07-15T00:00:00Z"}"#;
        let management = ManagementStore::memory(ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            vec![codex_channel(7, stored)],
            Vec::new(),
        ));
        let req = codex_request(
            r#"{"access_token":"access-same","account_id":"acct-1","expired":"1999999999"}"#.into(),
        );
        let mut seen = HashMap::new();
        let mut index = 0;
        let mut aggregate = empty_aggregate();

        let (result, failed) = import_codex_blob(
            &management,
            &req,
            "session.json",
            1,
            &mut seen,
            &mut index,
            &mut aggregate,
        )
        .await
        .expect("update session channel");

        assert_eq!(failed, 0);
        assert_eq!(result.updated, Some(1));
        let channel = management.current_data().unwrap().channels.remove(0);
        let key = parse_flexible_codex_key(&channel.key).unwrap();
        assert_eq!(key.refresh_token.as_deref(), Some("refresh-durable"));
        assert_eq!(key.client_id.as_deref(), Some("mobile-client"));
        assert_eq!(key.id_token.as_deref(), Some("id-durable"));
        assert_eq!(key.last_refresh.as_deref(), Some("2026-07-15T00:00:00Z"));
        assert_eq!(key.expired.as_deref(), Some("1999999999"));
    }

    #[tokio::test]
    async fn session_array_good_bad_good_persists_both_valid_entries() {
        let management = ManagementStore::memory(ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        let secret = "refresh-secret-must-not-leak";
        let content = format!(
            r#"[
              {{"access_token":"access-one","refresh_token":"refresh-one","account_id":"acct-1"}},
              {{"refresh_token":"{secret}","account_id":"acct-bad"}},
              {{"access_token":"access-two","refresh_token":"refresh-two","account_id":"acct-2"}}
            ]"#
        );
        let req = codex_request(content);
        let mut seen = HashMap::new();
        let mut index = 0;
        let mut aggregate = empty_aggregate();

        let (result, failed) = import_codex_blob(
            &management,
            &req,
            "sessions.json",
            1,
            &mut seen,
            &mut index,
            &mut aggregate,
        )
        .await
        .expect("partially import session array");

        assert_eq!(result.created, Some(2));
        assert_eq!(failed, 1);
        assert_eq!(aggregate.created, 2);
        assert_eq!(aggregate.failed, 1);
        assert_eq!(management.current_data().unwrap().channels.len(), 2);
        assert_eq!(
            aggregate
                .items
                .iter()
                .map(|item| item.action.as_str())
                .collect::<Vec<_>>(),
            vec!["created", "failed", "created"]
        );
        let public_errors = serde_json::to_string(&aggregate.errors).unwrap();
        assert!(!public_errors.contains(secret));
        assert!(public_errors.contains("credential is missing required fields"));
    }

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
    fn gemini_cliproxy_import_only_accepts_api_key_files() {
        let api_key: JsonMap<String, JsonValue> =
            serde_json::from_str(r#"{"type":"gemini","api_key":"gemini-key"}"#).unwrap();
        assert_eq!(
            parse_cliproxy_gemini_api_key("gemini", &api_key).expect("api key"),
            "gemini-key"
        );

        let oauth: JsonMap<String, JsonValue> = serde_json::from_str(
            r#"{"type":"gemini","access_token":"access","refresh_token":"refresh"}"#,
        )
        .unwrap();
        let error = parse_cliproxy_gemini_api_key("gemini", &oauth).unwrap_err();
        assert!(error.to_string().contains("PluginAuthParser"));

        let plugin_file: JsonMap<String, JsonValue> =
            serde_json::from_str(r#"{"type":"gemini-cli","api_key":"would-be-ignored"}"#).unwrap();
        let error = parse_cliproxy_gemini_api_key("gemini-cli", &plugin_file).unwrap_err();
        assert!(error.to_string().contains("PluginAuthParser"));

        assert_eq!(
            resolve_gemini_models("ignored-codex-defaults", false),
            GEMINI_DEFAULT_MODELS
        );
        assert_eq!(
            resolve_gemini_models(" custom-gemini-model ", true),
            "custom-gemini-model"
        );
    }

    #[test]
    fn parses_cliproxy_claude_oauth_and_builds_runtime_metadata() {
        let raw = r#"{
          "type":"claude",
          "access_token":"sk-ant-oat01-test",
          "refresh_token":"rt-claude",
          "email":"claude@example.com",
          "expires_at":1893456000,
          "last_refresh":"2026-07-15T01:02:03Z",
          "token_type":"Bearer",
          "idToken":"id-claude",
          "headers":{"x-app":"custom-cli","X-Custom-Number":7}
        }"#;
        let meta: JsonMap<String, JsonValue> = serde_json::from_str(raw).unwrap();
        let parsed = parse_cliproxy_claude_auth(&meta).expect("parse claude");
        assert!(parsed.is_oauth);
        assert_eq!(parsed.auth_kind, "oauth");
        assert_eq!(parsed.access_token, "sk-ant-oat01-test");
        assert_eq!(parsed.refresh_token.as_deref(), Some("rt-claude"));
        assert_eq!(parsed.id_token.as_deref(), Some("id-claude"));
        assert_eq!(parsed.expired.as_deref(), Some("1893456000"));

        let setting = build_claude_channel_setting(&parsed, "claude-user.json").expect("setting");
        let setting: JsonValue = serde_json::from_str(&setting).unwrap();
        assert_eq!(
            setting.get("refresh_token").and_then(JsonValue::as_str),
            Some("rt-claude")
        );
        assert_eq!(
            setting.get("email").and_then(JsonValue::as_str),
            Some("claude@example.com")
        );
        assert_eq!(
            setting.get("expired").and_then(JsonValue::as_str),
            Some("1893456000")
        );
        assert_eq!(
            setting.get("id_token").and_then(JsonValue::as_str),
            Some("id-claude")
        );

        let header_override = build_claude_header_override(&parsed)
            .expect("headers")
            .expect("oauth headers");
        let headers: JsonMap<String, JsonValue> = serde_json::from_str(&header_override).unwrap();
        assert_eq!(
            headers.get("Authorization").and_then(JsonValue::as_str),
            Some("Bearer {api_key}")
        );
        assert_eq!(
            headers.get("Anthropic-Beta").and_then(JsonValue::as_str),
            Some(CLAUDE_OAUTH_BETA)
        );
        assert_eq!(
            headers.get("Anthropic-Version").and_then(JsonValue::as_str),
            Some(CLAUDE_API_VERSION)
        );
        assert_eq!(
            headers.get("X-App").and_then(JsonValue::as_str),
            Some("custom-cli")
        );
        assert!(!headers.contains_key("x-app"));
        assert_eq!(
            headers.get("X-Custom-Number").and_then(JsonValue::as_str),
            Some("7")
        );
    }

    #[test]
    fn claude_api_key_keeps_x_api_key_runtime_path() {
        let raw = r#"{"type":"claude","auth_kind":"api_key","api_key":"sk-ant-api03-test"}"#;
        let meta: JsonMap<String, JsonValue> = serde_json::from_str(raw).unwrap();
        let parsed = parse_cliproxy_claude_auth(&meta).expect("parse claude api key");
        assert!(!parsed.is_oauth);
        assert_eq!(parsed.auth_kind, "api_key");
        assert_eq!(parsed.access_token, "sk-ant-api03-test");
        assert!(
            build_claude_header_override(&parsed)
                .expect("headers")
                .is_none()
        );

        let setting = build_claude_channel_setting(&parsed, "claude-api.json").expect("setting");
        let setting: JsonValue = serde_json::from_str(&setting).unwrap();
        assert_eq!(
            setting.get("auth_kind").and_then(JsonValue::as_str),
            Some("api_key")
        );
    }

    #[test]
    fn claude_default_models_do_not_inherit_codex_defaults() {
        assert_eq!(
            resolve_claude_models("gpt-5.1,gpt-5,o3,o4-mini", false),
            CLAUDE_DEFAULT_MODELS
        );
        let defaults: Vec<_> = CLAUDE_DEFAULT_MODELS.split(',').collect();
        assert_eq!(defaults.len(), 14);
        assert!(defaults.contains(&"claude-sonnet-4-6"));
        assert!(defaults.contains(&"claude-opus-4-6"));
        assert_eq!(
            resolve_claude_models(" custom-claude ", true),
            "custom-claude"
        );
    }

    #[test]
    fn parses_cliproxy_xai_oauth_file() {
        let raw = r#"{
          "type":"xai",
          "auth_kind":"oauth",
          "access_token":"at-1",
          "refresh_token":"rt-1",
          "id_token":"id-1",
          "token_type":"Bearer",
          "expires_in":3600,
          "scope":"openid profile email",
          "email":"z@example.com",
          "sub":"sub-1",
          "base_url":"https://cli-chat-proxy.grok.com/v1",
          "redirect_uri":"http://localhost:1455/auth/callback",
          "token_endpoint":"https://auth.x.ai/oauth2/token",
          "headers":{"User-Agent":"grok-shell/0.2.93"}
        }"#;
        let meta: JsonMap<String, JsonValue> = serde_json::from_str(raw).unwrap();
        let parsed = parse_cliproxy_xai_auth(&meta).expect("parse xai");
        assert_eq!(parsed.access_token, "at-1");
        assert_eq!(parsed.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(parsed.id_token.as_deref(), Some("id-1"));
        assert_eq!(parsed.token_type.as_deref(), Some("Bearer"));
        assert_eq!(parsed.expires_in.as_deref(), Some("3600"));
        assert_eq!(parsed.scope.as_deref(), Some("openid profile email"));
        assert_eq!(parsed.email.as_deref(), Some("z@example.com"));
        assert_eq!(parsed.subject.as_deref(), Some("sub-1"));
        assert_eq!(parsed.auth_kind, "oauth");
        assert!(!parsed.using_api);
        assert_eq!(parsed.base_url.as_deref(), Some(XAI_DEFAULT_CLI_CHAT_BASE));
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
        let setting = build_xai_channel_setting(&parsed, "xai-user.json").expect("setting");
        let setting: JsonValue = serde_json::from_str(&setting).unwrap();
        assert_eq!(
            setting
                .get("upstream_endpoint_type")
                .and_then(|v| v.as_str()),
            Some("xai-response")
        );
        assert_eq!(
            setting.get("id_token").and_then(JsonValue::as_str),
            Some("id-1")
        );
        assert_eq!(
            setting.get("token_type").and_then(JsonValue::as_str),
            Some("Bearer")
        );
        assert_eq!(
            setting.get("expires_in").and_then(JsonValue::as_str),
            Some("3600")
        );
        assert_eq!(
            setting.get("redirect_uri").and_then(JsonValue::as_str),
            Some("http://localhost:1455/auth/callback")
        );
    }

    #[test]
    fn cliproxy_xai_rejects_untrusted_token_endpoint_at_import() {
        for endpoint in [
            "http://auth.x.ai/oauth2/token",
            "https://evil.example/oauth2/token",
            "https://x.ai.evil.example/oauth2/token",
        ] {
            let meta: JsonMap<String, JsonValue> = serde_json::from_value(serde_json::json!({
                "type": "xai",
                "auth_kind": "oauth",
                "access_token": "at-1",
                "refresh_token": "rt-1",
                "token_endpoint": endpoint,
            }))
            .expect("metadata");

            let error = parse_cliproxy_xai_auth(&meta)
                .expect_err("untrusted xAI token endpoint must fail closed");
            assert!(error.to_string().contains("token_endpoint"));
        }

        let unsupported: JsonMap<String, JsonValue> = serde_json::from_value(serde_json::json!({
            "type": "xai",
            "auth_kind": "custom",
            "access_token": "at-1",
        }))
        .expect("metadata");
        assert!(parse_cliproxy_xai_auth(&unsupported).is_err());
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
        let setting = build_xai_channel_setting(&parsed, "xai-api.json").expect("setting");
        let setting: JsonValue = serde_json::from_str(&setting).unwrap();
        assert_eq!(
            setting
                .get("upstream_endpoint_type")
                .and_then(|v| v.as_str()),
            Some("xai-response")
        );
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
        assert_eq!(map.len(), 3);
        assert_eq!(
            map.get(XAI_CLIENT_VERSION_HEADER).and_then(|v| v.as_str()),
            Some(XAI_CLIENT_VERSION_VALUE)
        );
        assert_eq!(
            map.get("User-Agent").and_then(|v| v.as_str()),
            Some(XAI_USER_AGENT)
        );
    }

    #[test]
    fn xai_file_headers_override_cli_defaults_case_insensitively() {
        let raw = r#"{
          "type":"xai",
          "auth_kind":"oauth",
          "access_token":"at",
          "headers":{
            "user-agent":"custom-agent",
            "x-xai-token-auth":"custom-token-auth",
            "X-Grok-Client-Version":"custom-version"
          }
        }"#;
        let meta: JsonMap<String, JsonValue> = serde_json::from_str(raw).unwrap();
        let parsed = parse_cliproxy_xai_auth(&meta).expect("parse");
        let override_json = build_xai_header_override(&parsed, parsed.base_url.as_deref())
            .expect("ok")
            .expect("headers");
        let map: JsonMap<String, JsonValue> = serde_json::from_str(&override_json).unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(
            map.get("User-Agent").and_then(|v| v.as_str()),
            Some("custom-agent")
        );
        assert_eq!(
            map.get(XAI_TOKEN_AUTH_HEADER).and_then(|v| v.as_str()),
            Some("custom-token-auth")
        );
        assert_eq!(
            map.get(XAI_CLIENT_VERSION_HEADER).and_then(|v| v.as_str()),
            Some("custom-version")
        );
    }

    #[test]
    fn xai_default_models_do_not_inherit_codex_defaults() {
        assert_eq!(
            resolve_xai_models("gpt-5.1,gpt-5,o3,o4-mini", false),
            XAI_DEFAULT_MODELS
        );
        assert_eq!(XAI_DEFAULT_MODELS.split(',').count(), 9);
        assert_eq!(
            XAI_DEFAULT_MODELS,
            "grok-build-0.1,grok-4.5,grok-4.3,grok-4.20-0309-reasoning,grok-4.\
             20-0309-non-reasoning,grok-4.20-multi-agent-0309,grok-3-mini,grok-3-mini-fast,\
             grok-composer-2.5-fast"
        );
        assert_eq!(resolve_xai_models(" custom-grok ", true), "custom-grok");
    }

    #[test]
    fn resolves_xai_official_cli_and_custom_bases() {
        assert_eq!(resolve_xai_base_url(false, None), XAI_DEFAULT_CLI_CHAT_BASE);
        assert_eq!(
            resolve_xai_base_url(false, Some("https://api.x.ai/v1/")),
            XAI_DEFAULT_CLI_CHAT_BASE
        );
        assert_eq!(
            resolve_xai_base_url(false, Some("https://api.x.ai/")),
            XAI_DEFAULT_CLI_CHAT_BASE
        );
        assert_eq!(
            resolve_xai_base_url(false, Some("https://cli-chat-proxy.grok.com/")),
            XAI_DEFAULT_CLI_CHAT_BASE
        );
        assert_eq!(
            resolve_xai_base_url(false, Some(" https://gateway.example.com/foo/ ")),
            "https://gateway.example.com/foo"
        );
        assert_eq!(
            resolve_xai_base_url(false, Some("https://gateway.example.com/v1/")),
            "https://gateway.example.com/v1"
        );
        assert_eq!(resolve_xai_base_url(true, None), XAI_DEFAULT_API_BASE);
        assert_eq!(
            resolve_xai_base_url(true, Some("https://api.x.ai/")),
            XAI_DEFAULT_API_BASE
        );
        assert_eq!(
            resolve_xai_base_url(true, Some("https://api.x.ai/v1")),
            XAI_DEFAULT_API_BASE
        );
    }

    #[test]
    fn trims_xai_base_url_without_removing_v1() {
        assert_eq!(
            trim_xai_base_url(" https://gateway.example.com/v1/ "),
            "https://gateway.example.com/v1"
        );
        assert_eq!(
            trim_xai_base_url("https://gateway.example.com/foo/"),
            "https://gateway.example.com/foo"
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
