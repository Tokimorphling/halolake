//! HTTP handlers + route mount for credential import (control-api-ext).

use crate::{
    AppState,
    control_api_ext::{
        AuthImportRequest, AuthMethod, CHANNEL_TYPE_CODEX, CodexAuthImportItem,
        CodexAuthImportMessage, CodexAuthImportRequest, CodexAuthImportResult,
        Sub2apiDataImportRequest, codex_key_to_json, collect_entries, find_existing_channel_id,
        import_auth, merge_codex_oauth_key, run_sub2api_data_import,
    },
    http_auth::require_role,
    http_response::{api_error_status, api_success, management_error},
    now_unix, publish_management_snapshot,
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
};
use halolake_control_plane::{CreateChannelRequest, ManagementError, UpdateChannelRequest};
use halolake_domain::{ChannelRecord, ROLE_ADMIN_USER, STATUS_ENABLED};
use service_async::Service;
use std::collections::HashMap;

async fn bump_and_publish_import_snapshot(state: &AppState) -> Result<(), ManagementError> {
    // Auth imports may mutate only ProxyStore. Advance the management version
    // explicitly so existing gateway workers do not reject that snapshot as
    // NotModified. Channel mutations may already have bumped it; an extra
    // monotonic increment is harmless.
    state.management.bump_version().await?;
    publish_management_snapshot(state).await
}

/// Mount import routes under admin-extras.
pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/channel/import/auth", post(import_auth_json))
        .route("/api/channel/import/auth/", post(import_auth_json))
        .route(
            "/api/channel/import/auth/upload",
            post(import_auth_multipart),
        )
        .route(
            "/api/channel/import/auth/upload/",
            post(import_auth_multipart),
        )
        .route("/api/channel/import/codex-auth", post(import_codex_auth))
        .route("/api/channel/import/codex-auth/", post(import_codex_auth))
        .route(
            "/api/channel/import/sub2api-data",
            post(import_sub2api_data),
        )
        .route(
            "/api/channel/import/sub2api-data/",
            post(import_sub2api_data),
        )
}

/// Import Codex / sub2api-format auth files as type-57 channels.
///
/// Body: `{ "content": "<file or paste>", "contents": ["..."], "name", "group",
/// "models", "base_url", "proxy_id", "update_existing": true }`
#[cfg(feature = "admin-extras")]
pub(crate) async fn import_codex_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CodexAuthImportRequest>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };

    let entries = match collect_entries(&req) {
        Ok(entries) => entries,
        Err(err) => return management_error(err),
    };

    let mut result = CodexAuthImportResult {
        total:    entries.len(),
        created:  0,
        updated:  0,
        skipped:  0,
        failed:   0,
        items:    Vec::with_capacity(entries.len()),
        warnings: Vec::new(),
        errors:   Vec::new(),
    };

    let existing = match state.management.current_data() {
        Ok(data) => data.channels,
        Err(err) => return management_error(err),
    };

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

    let mut seen: HashMap<String, usize> = HashMap::new();

    for (offset, item) in entries.into_iter().enumerate() {
        let index = offset + 1;
        let account_name = if name_base.is_empty() {
            item.name.clone()
        } else if result.total > 1 {
            format!("{name_base} #{index}")
        } else {
            name_base.to_string()
        };

        for warning in &item.warnings {
            result.warnings.push(CodexAuthImportMessage {
                index,
                name: account_name.clone(),
                message: warning.clone(),
            });
        }

        if let Some(prev) = item
            .identity_keys
            .iter()
            .find_map(|key| seen.get(key).copied())
        {
            result.skipped = result.skipped.saturating_add(1);
            let message = format!("duplicate of import entry {prev}; skipped");
            result.items.push(CodexAuthImportItem {
                index,
                name: account_name.clone(),
                action: "skipped".into(),
                channel_id: None,
                message: message.clone(),
            });
            result.warnings.push(CodexAuthImportMessage {
                index,
                name: account_name,
                message,
            });
            continue;
        }
        for key in &item.identity_keys {
            seen.insert(key.clone(), index);
        }

        let key_json = match codex_key_to_json(&item.key) {
            Ok(json) => json,
            Err(err) => {
                result.failed = result.failed.saturating_add(1);
                let message = err.to_string();
                result.items.push(CodexAuthImportItem {
                    index,
                    name: account_name.clone(),
                    action: "failed".into(),
                    channel_id: None,
                    message: message.clone(),
                });
                result.errors.push(CodexAuthImportMessage {
                    index,
                    name: account_name,
                    message,
                });
                continue;
            }
        };

        if let Some(existing_id) = find_existing_channel_id(&existing, &item) {
            if req.update_existing {
                let mut channel = match existing.iter().find(|c| c.id == existing_id).cloned() {
                    Some(channel) => channel,
                    None => {
                        result.failed = result.failed.saturating_add(1);
                        result.items.push(CodexAuthImportItem {
                            index,
                            name: account_name.clone(),
                            action: "failed".into(),
                            channel_id: None,
                            message: "existing channel disappeared".into(),
                        });
                        continue;
                    }
                };
                channel.key =
                    match codex_key_to_json(&merge_codex_oauth_key(&channel.key, item.key.clone()))
                    {
                        Ok(key) => key,
                        Err(err) => {
                            result.failed = result.failed.saturating_add(1);
                            let message = err.to_string();
                            result.items.push(CodexAuthImportItem {
                                index,
                                name: account_name.clone(),
                                action: "failed".into(),
                                channel_id: None,
                                message: message.clone(),
                            });
                            result.errors.push(CodexAuthImportMessage {
                                index,
                                name: account_name,
                                message,
                            });
                            continue;
                        }
                    };
                channel.channel_type = CHANNEL_TYPE_CODEX;
                if !account_name.is_empty() {
                    channel.name = account_name.clone();
                }
                if let Some(pid) = req.proxy_id {
                    channel.proxy_id = Some(pid);
                }
                match state
                    .management
                    .call(UpdateChannelRequest { channel })
                    .await
                {
                    Ok(updated) => {
                        result.updated = result.updated.saturating_add(1);
                        result.items.push(CodexAuthImportItem {
                            index,
                            name: account_name,
                            action: "updated".into(),
                            channel_id: Some(updated.id),
                            message: String::new(),
                        });
                    }
                    Err(err) => {
                        result.failed = result.failed.saturating_add(1);
                        let message = err.to_string();
                        result.items.push(CodexAuthImportItem {
                            index,
                            name: account_name.clone(),
                            action: "failed".into(),
                            channel_id: None,
                            message: message.clone(),
                        });
                        result.errors.push(CodexAuthImportMessage {
                            index,
                            name: account_name,
                            message,
                        });
                    }
                }
            } else {
                result.skipped = result.skipped.saturating_add(1);
                result.items.push(CodexAuthImportItem {
                    index,
                    name: account_name,
                    action: "skipped".into(),
                    channel_id: Some(existing_id),
                    message: "matching channel exists; update_existing=false".into(),
                });
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
            weight:               req.weight.or(Some(1)),
            created_time:         now_unix(),
            test_time:            0,
            response_time:        0,
            base_url:             base_url.clone(),
            balance:              0.0,
            balance_updated_time: 0,
            models:               models.clone(),
            group:                group.clone(),
            used_quota:           0,
            model_mapping:        None,
            priority:             req.priority.or(Some(0)),
            auto_ban:             Some(1),
            tag:                  None,
            setting:              None,
            param_override:       None,
            header_override:      None,
            remark:               Some(format!(
                "imported from codex/sub2api auth{}",
                if item.email.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", item.email)
                }
            )),
            proxy_id:             req.proxy_id,
        };

        match state
            .management
            .call(CreateChannelRequest { channel })
            .await
        {
            Ok(created) => {
                result.created = result.created.saturating_add(1);
                result.items.push(CodexAuthImportItem {
                    index,
                    name: account_name,
                    action: "created".into(),
                    channel_id: Some(created.id),
                    message: String::new(),
                });
            }
            Err(err) => {
                result.failed = result.failed.saturating_add(1);
                let message = err.to_string();
                result.items.push(CodexAuthImportItem {
                    index,
                    name: account_name.clone(),
                    action: "failed".into(),
                    channel_id: None,
                    message: message.clone(),
                });
                result.errors.push(CodexAuthImportMessage {
                    index,
                    name: account_name,
                    message,
                });
            }
        }
    }

    if result.created > 0 || result.updated > 0 {
        if let Err(err) = publish_management_snapshot(&state).await {
            return management_error(err);
        }
    }
    api_success(result)
}

/// Unified auth import: CLIProxyAPI auth JSON, Codex session, or sub2api-data.
///
/// JSON body: `{ "format":"auto", "content"|"contents[]", "filenames[]", "group", ... }`
#[cfg(feature = "admin-extras")]
pub(crate) async fn import_auth_json(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AuthImportRequest>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match import_auth(&state.management, &state.proxies, req).await {
        Ok(result) => {
            let mutated = result
                .channels
                .as_ref()
                .is_some_and(|c| c.created > 0 || c.updated > 0)
                || result.data.as_ref().is_some_and(|d| {
                    d.proxy_created > 0 || d.account_created > 0 || d.proxy_reused > 0
                });
            if mutated {
                if let Err(err) = bump_and_publish_import_snapshot(&state).await {
                    return management_error(err);
                }
            }
            api_success(result)
        }
        Err(err) => management_error(err),
    }
}

/// Multipart batch upload of auth files (CLIProxyAPI-style).
///
/// Form fields:
/// - `file` / `files` / any file parts: one or more `.json` auth files
/// - `format` (optional): `auto` | `cliproxy` | `codex-session` | `sub2api-data`
/// - `auth_method`, `group`, `models`, `name`, `proxy_id`, `update_existing` (optional)
#[cfg(feature = "admin-extras")]
pub(crate) async fn import_auth_multipart(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: axum::extract::Multipart,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };

    let mut contents = Vec::new();
    let mut filenames = Vec::new();
    let mut format = "auto".to_string();
    let mut auth_method = AuthMethod::Auto;
    let mut group = None;
    let mut models = None;
    let mut name = String::new();
    let mut proxy_id = None;
    let mut update_existing = true;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(err) => {
                return api_error_status(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid multipart: {err}"),
                );
            }
        };
        let field_name = field.name().unwrap_or("").to_string();
        let file_name = field.file_name().map(|s| s.to_string());
        let data = match field.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                return api_error_status(
                    StatusCode::BAD_REQUEST,
                    &format!("failed to read multipart field: {err}"),
                );
            }
        };
        let text = String::from_utf8_lossy(&data).to_string();
        if file_name.is_some() || matches!(field_name.as_str(), "file" | "files" | "auth" | "auths")
        {
            let fname = file_name.unwrap_or_else(|| field_name.clone());
            if !text.trim().is_empty() {
                filenames.push(fname);
                contents.push(text);
            }
            continue;
        }
        match field_name.as_str() {
            "format" => format = text.trim().to_string(),
            "auth_method" => {
                auth_method = match AuthMethod::parse(&text) {
                    Some(method) => method,
                    None => {
                        return api_error_status(StatusCode::BAD_REQUEST, "invalid auth_method");
                    }
                };
            }
            "group" => group = Some(text.trim().to_string()).filter(|s| !s.is_empty()),
            "models" => models = Some(text.trim().to_string()).filter(|s| !s.is_empty()),
            "name" => name = text.trim().to_string(),
            "proxy_id" => {
                let value = text.trim();
                if !value.is_empty() {
                    proxy_id = match value.parse::<u64>() {
                        Ok(id) if id > 0 => Some(id),
                        _ => {
                            return api_error_status(StatusCode::BAD_REQUEST, "invalid proxy_id");
                        }
                    };
                }
            }
            "update_existing" => {
                update_existing = matches!(
                    text.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                );
            }
            "content" => {
                if !text.trim().is_empty() {
                    filenames.push("content".into());
                    contents.push(text);
                }
            }
            _ => {}
        }
    }

    if contents.is_empty() {
        return api_error_status(StatusCode::BAD_REQUEST, "no files uploaded");
    }

    let req = AuthImportRequest {
        format,
        auth_method,
        content: String::new(),
        contents,
        filenames,
        name,
        group,
        models,
        base_url: None,
        proxy_id,
        update_existing,
        data: None,
    };

    match import_auth(&state.management, &state.proxies, req).await {
        Ok(result) => {
            let mutated = result
                .channels
                .as_ref()
                .is_some_and(|c| c.created > 0 || c.updated > 0)
                || result.data.as_ref().is_some_and(|d| {
                    d.proxy_created > 0 || d.account_created > 0 || d.proxy_reused > 0
                });
            if mutated {
                if let Err(err) = bump_and_publish_import_snapshot(&state).await {
                    return management_error(err);
                }
            }
            api_success(result)
        }
        Err(err) => management_error(err),
    }
}

/// Import sub2api export JSON (`type: sub2api-data`) — proxies + accounts as channels.
///
/// Body accepts either `{ "data": { ...export... } }` or `{ "content": "<file text>" }`.
/// Groups are not auto-bound; set `group` to apply a default channel group.
#[cfg(feature = "admin-extras")]
pub(crate) async fn import_sub2api_data(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<Sub2apiDataImportRequest>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match run_sub2api_data_import(&state.management, &state.proxies, req).await {
        Ok(result) => {
            if result.proxy_created > 0 || result.account_created > 0 || result.proxy_reused > 0 {
                if let Err(err) = bump_and_publish_import_snapshot(&state).await {
                    return management_error(err);
                }
            }
            api_success(result)
        }
        Err(err) => management_error(err),
    }
}
