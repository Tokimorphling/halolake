//! Channel and proxy management HTTP handlers.

use crate::{
    AppState, BatchIds, ChannelBatchTagPayload, ChannelListQuery, ChannelSearchQuery,
    ChannelTagPayload, StatusUpdate,
    channel_ops::{
        ApplyAllChannelUpstreamModelUpdatesRequest, ApplyChannelUpstreamModelUpdatesRequest,
        ChannelOpsService, ChannelTestAllQuery, ChannelTestQuery, CopyChannelQuery,
        CopyChannelRequest, DetectChannelUpstreamModelUpdatesRequest, FixChannelAbilitiesRequest,
        TestChannelRequest, UpdateAllChannelBalancesRequest, UpdateChannelBalanceRequest,
    },
    channel_probe::{ChannelProbeService, FetchModelsRequest},
    channel_task::{
        ChannelTestTaskPayload, ModelUpdateTaskPayload, SystemTaskProgressState,
        spawn_channel_test_task, spawn_model_update_task,
    },
    http_auth::{require_role, require_secure_verification},
    http_response::{
        api_error_status, api_ok, api_success, api_success_with_extra, api_success_with_message,
        management_error, system_task_conflict,
    },
    now_unix, option_f64, option_i64,
    proxy::{
        CreateProxyRequest, DeleteProxyRequest, GetProxyRequest, ListProxiesRequest, ProxyRecord,
        UpdateProxyRequest,
    },
    publish_management_snapshot,
    system_task::{
        EnqueueSystemTaskRequest, SYSTEM_TASK_TYPE_CHANNEL_TEST, SYSTEM_TASK_TYPE_MODEL_UPDATE,
    },
};
#[cfg(feature = "admin-extras")]
use crate::{
    auth_import::{self, AuthImportRequest},
    channel_special::{
        ChannelSpecialService, CodexRefreshCredentialRequest, CodexWhamKind, CodexWhamRequest,
        MultiKeyManageRequest, OllamaDeleteModelRequest, OllamaModelRequestBody,
        OllamaPullModelRequest, OllamaVersionRequest,
    },
    codex_auth_import::{
        CHANNEL_TYPE_CODEX, CodexAuthImportItem, CodexAuthImportMessage, CodexAuthImportRequest,
        CodexAuthImportResult, codex_key_to_json, collect_entries, find_existing_channel_id,
    },
    sub2api_data_import::{self, Sub2apiDataImportRequest},
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use halolake_control_plane::{
    BatchSetChannelTagRequest, ChannelStatusUpdateRequest, ChannelTagPatch, CreateChannelRequest,
    DeleteChannelRequest, DeleteDisabledChannelsRequest, GetChannelRequest, ListChannelsRequest,
    RevealChannelKeyRequest, SearchChannelsRequest, UpdateChannelRequest,
    UpdateChannelsByTagRequest,
};
use halolake_domain::{
    ChannelRecord, PageRequest, ROLE_ADMIN_USER, ROLE_ROOT_USER, STATUS_ENABLED, SearchRequest,
};
use serde_json::{Value as JsonValue, json};
use service_async::Service;
use std::collections::HashMap;
use tracing::warn;

fn normalize_group_filter(group: &str) -> Option<&str> {
    let group = group.trim();
    if group.is_empty() || group.eq_ignore_ascii_case("all") || group.eq_ignore_ascii_case("null") {
        None
    } else {
        Some(group)
    }
}

fn channel_in_group(channel: &ChannelRecord, group: &str) -> bool {
    channel.group_list().iter().any(|item| item == group)
}

fn channel_matches_status(channel: &ChannelRecord, status: Option<i32>) -> bool {
    match status {
        Some(status) if status == STATUS_ENABLED => channel.status == STATUS_ENABLED,
        Some(0) => channel.status != STATUS_ENABLED,
        Some(status) => channel.status == status,
        None => true,
    }
}

/// new-api type_counts: counts by type under group+status filters (type filter excluded).
fn type_counts_for_filters(
    channels: &[ChannelRecord],
    group: Option<&str>,
    status: Option<i32>,
) -> serde_json::Map<String, JsonValue> {
    let mut counts = serde_json::Map::new();
    for channel in channels {
        if let Some(group) = group {
            if !channel_in_group(channel, group) {
                continue;
            }
        }
        if !channel_matches_status(channel, status) {
            continue;
        }
        let key = channel.channel_type.to_string();
        let entry = counts.entry(key).or_insert(json!(0));
        if let Some(n) = entry.as_i64() {
            *entry = json!(n + 1);
        }
    }
    counts
}

/// Parse new-api status filter query: enabled/1, disabled/0, else all.
fn parse_channel_status_filter(status: &str) -> Option<i32> {
    match status.trim().to_ascii_lowercase().as_str() {
        "enabled" | "1" => Some(STATUS_ENABLED),
        "disabled" | "0" => Some(0),
        _ => None,
    }
}

pub(crate) async fn list_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ChannelListQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let group = query.group.clone();
    let status = parse_channel_status_filter(&query.status);
    match state
        .management
        .call(ListChannelsRequest {
            page: query.page_request(),
            group: query.group,
            status,
            channel_type: query.channel_type,
            sort_by: query.sort_by,
            sort_order: query.sort_order,
            id_sort: query.id_sort,
            tag_mode: query.tag_mode,
        })
        .await
    {
        Ok(page) => {
            let items: Vec<JsonValue> = page.items.iter().map(channel_to_api_json).collect();
            let type_counts = match state.management.current_data() {
                Ok(data) => {
                    type_counts_for_filters(&data.channels, normalize_group_filter(&group), status)
                }
                Err(_) => channel_type_counts(&items),
            };
            api_success(json!({
                "items": items,
                "total": page.total,
                "page": page.page,
                "page_size": page.page_size,
                "type_counts": type_counts,
            }))
        }
        Err(err) => management_error(err),
    }
}

pub(crate) async fn search_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ChannelSearchQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let group = query.group.clone();
    let status = parse_channel_status_filter(&query.status);
    match state
        .management
        .call(SearchChannelsRequest {
            search: SearchRequest {
                page:    PageRequest {
                    page:      query.page,
                    page_size: query.page_size,
                },
                keyword: query.keyword,
            },
            group: query.group,
            model: query.model,
            status,
            channel_type: query.channel_type,
            sort_by: query.sort_by,
            sort_order: query.sort_order,
            id_sort: query.id_sort,
            tag_mode: query.tag_mode,
        })
        .await
    {
        Ok(page) => {
            let items: Vec<JsonValue> = page.items.iter().map(channel_to_api_json).collect();
            let type_counts = match state.management.current_data() {
                Ok(data) => {
                    type_counts_for_filters(&data.channels, normalize_group_filter(&group), status)
                }
                Err(_) => channel_type_counts(&items),
            };
            api_success(json!({
                "items": items,
                "total": page.total,
                "page": page.page,
                "page_size": page.page_size,
                "type_counts": type_counts,
            }))
        }
        Err(err) => management_error(err),
    }
}

pub(crate) async fn get_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.management.call(GetChannelRequest { id }).await {
        Ok(channel) => api_success(channel_to_api_json(&channel)),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn reveal_channel_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    // new-api: RootAuth + SecureVerificationRequired before returning plaintext key.
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_secure_verification(&state, &headers) {
        return resp;
    }
    match state.management.call(RevealChannelKeyRequest { id }).await {
        Ok(key) => api_success(json!({ "key": key.key })),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn create_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<JsonValue>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };

    let channels = match expand_create_channel_body(body) {
        Ok(items) => items,
        Err(msg) => return api_error_status(StatusCode::BAD_REQUEST, &msg),
    };
    if channels.is_empty() {
        return api_error_status(StatusCode::BAD_REQUEST, "no channel to create");
    }

    let mut created = 0usize;
    for channel in channels {
        match state
            .management
            .call(CreateChannelRequest { channel })
            .await
        {
            Ok(_) => created += 1,
            Err(err) => return management_error(err),
        }
    }

    match publish_management_snapshot(&state).await {
        Ok(()) => {
            if created > 1 {
                api_success_with_message("created", json!({ "created": created }))
            } else {
                api_ok()
            }
        }
        Err(err) => management_error(err),
    }
}

/// Mirrors `ref/new-api/controller/channel.go` `AddChannel` body:
/// ```json
/// { "mode": "single|batch|multi_to_single", "multi_key_mode": "random|polling",
///   "batch_add_set_key_prefix_2_name": bool, "channel": { ... } }
/// ```
/// Also accepts a flat channel object (API convenience).
fn expand_create_channel_body(body: JsonValue) -> Result<Vec<ChannelRecord>, String> {
    let mode = body
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("single")
        .trim()
        .to_ascii_lowercase();
    let multi_key_mode = body
        .get("multi_key_mode")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let batch_prefix = body
        .get("batch_add_set_key_prefix_2_name")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let channel_json = if let Some(ch) = body.get("channel").cloned() {
        if ch.is_null() {
            return Err("channel is required".into());
        }
        ch
    } else {
        let mut flat = body;
        if let Some(obj) = flat.as_object_mut() {
            obj.remove("mode");
            obj.remove("multi_key_mode");
            obj.remove("batch_add_set_key_prefix_2_name");
        }
        flat
    };
    let mut channel = channel_record_from_json(channel_json)?;
    if channel.created_time == 0 {
        channel.created_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
    }

    // new-api: switch mode → build keys slice → BatchInsertChannels
    match mode.as_str() {
        "single" => {
            if channel.key.trim().is_empty() {
                return Err("key is empty".into());
            }
            Ok(vec![channel])
        }
        "batch" => {
            // new-api: keys = strings.Split(key, "\n"); skip empty; optional name prefix
            let keys = split_channel_keys(&channel.key);
            if keys.is_empty() {
                return Err("key is empty".into());
            }
            let base_name = channel.name.clone();
            let many = keys.len() > 1;
            Ok(keys
                .into_iter()
                .map(|key| {
                    let mut item = channel.clone();
                    item.id = 0;
                    item.key = key.clone();
                    // new-api: if BatchAddSetKeyPrefix2Name && len(keys) > 1 {
                    //   Name = fmt.Sprintf("%s %s", Name, keyPrefix[:8])
                    // }
                    if batch_prefix && many {
                        let prefix: String = key.chars().take(8).collect();
                        item.name = format!("{base_name} {prefix}");
                    } else {
                        item.name = base_name.clone();
                    }
                    item
                })
                .collect())
        }
        "multi_to_single" => {
            // new-api: IsMultiKey=true, MultiKeyMode=..., Key=join(cleanKeys, "\n")
            let keys = split_channel_keys(&channel.key);
            if keys.is_empty() {
                return Err("key is empty".into());
            }
            channel.key = keys.join("\n");
            let mode_value = multi_key_mode
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .unwrap_or("random");
            // Persist multi-key metadata in `setting` (Halolake equivalent of channel_info)
            channel.setting = Some(merge_setting_json(channel.setting.as_deref(), &[
                ("is_multi_key", json!(true)),
                ("multi_key_mode", json!(mode_value)),
                ("multi_key_size", json!(keys.len())),
            ]));
            Ok(vec![channel])
        }
        other => Err(format!("不支持的添加模式: {other}")),
    }
}

/// new-api UI turns empty strings into JSON null; strip nulls so serde defaults apply.
fn channel_record_from_json(mut value: JsonValue) -> Result<ChannelRecord, String> {
    if let Some(obj) = value.as_object_mut() {
        // Drop unknown new-api-only fields that are not on ChannelRecord
        // (serde ignores unknown by default; nulls would fail on String fields).
        obj.retain(|_, v| !v.is_null());
        for drop_key in [
            "openai_organization",
            "test_model",
            "status_code_mapping",
            "settings",
            "other",
            "other_info",
            "channel_info",
            "keys",
            "max_input_tokens",
            "balance",
            "balance_updated_time",
            "used_quota",
            "created_time",
            "test_time",
            "response_time",
        ] {
            obj.remove(drop_key);
        }
        if let Some(w) = obj.get("weight").cloned() {
            if let Some(f) = w.as_f64() {
                obj.insert("weight".into(), json!(f as u64));
            }
        }
        // Map form setting.proxy (URL string) is already in setting JSON.
        // If client sends numeric proxy pool id as "proxy", ignore (not on ChannelRecord).
        obj.remove("proxy");
        if !obj.contains_key("key") {
            obj.insert("key".into(), json!(""));
        }
        // id may arrive as number or string
        if let Some(id) = obj.get("id").cloned() {
            if let Some(s) = id.as_str() {
                if let Ok(n) = s.parse::<u64>() {
                    obj.insert("id".into(), json!(n));
                }
            }
        }
    }
    serde_json::from_value(value).map_err(|err| format!("invalid channel payload: {err}"))
}

fn channel_type_counts(items: &[JsonValue]) -> serde_json::Map<String, JsonValue> {
    let mut counts = serde_json::Map::new();
    for item in items {
        let ty = item.get("type").and_then(|v| v.as_i64()).unwrap_or(0);
        let key = ty.to_string();
        let entry = counts.entry(key).or_insert(json!(0));
        if let Some(n) = entry.as_i64() {
            *entry = json!(n + 1);
        }
    }
    counts
}

/// Enrich channel JSON for new-api web (requires channel_info.multi_key_mode etc.).
fn channel_to_api_json(channel: &ChannelRecord) -> JsonValue {
    let mut value = serde_json::to_value(channel).unwrap_or_else(|_| json!({}));
    if let Some(obj) = value.as_object_mut() {
        let info = channel_info_from_setting(channel.setting.as_deref(), &channel.key);
        obj.insert("channel_info".into(), info);
        // new-api list/detail often include these as empty strings
        obj.entry("openai_organization".to_string())
            .or_insert(json!(""));
        obj.entry("test_model".to_string()).or_insert(json!(""));
        obj.entry("status_code_mapping".to_string())
            .or_insert(json!(""));
        obj.entry("other".to_string()).or_insert(json!(""));
        obj.entry("other_info".to_string()).or_insert(json!(""));
        obj.entry("settings".to_string()).or_insert(json!("{}"));
    }
    value
}

fn channel_info_from_setting(setting: Option<&str>, key: &str) -> JsonValue {
    let parsed = setting
        .and_then(|s| serde_json::from_str::<JsonValue>(s).ok())
        .unwrap_or_else(|| json!({}));
    let key_lines = key.lines().map(str::trim).filter(|l| !l.is_empty()).count();
    let is_multi = parsed
        .get("is_multi_key")
        .and_then(|v| v.as_bool())
        .unwrap_or(key_lines > 1);
    let multi_key_size = parsed
        .get("multi_key_size")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(if is_multi { key_lines } else { 0 });
    let multi_key_mode = parsed
        .get("multi_key_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("random");
    let multi_key_mode = if multi_key_mode == "polling" {
        "polling"
    } else {
        "random"
    };
    json!({
        "is_multi_key": is_multi,
        "multi_key_size": multi_key_size,
        "multi_key_status_list": parsed.get("multi_key_status_list").cloned().unwrap_or(json!({})),
        "multi_key_disabled_reason": parsed.get("multi_key_disabled_reason").cloned().unwrap_or(json!({})),
        "multi_key_disabled_time": parsed.get("multi_key_disabled_time").cloned().unwrap_or(json!({})),
        "multi_key_polling_index": parsed.get("multi_key_polling_index").and_then(|v| v.as_u64()).unwrap_or(0),
        "multi_key_mode": multi_key_mode,
    })
}

fn split_channel_keys(raw: &str) -> Vec<String> {
    // new-api: strings.Split + TrimSpace + skip empty
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn merge_setting_json(existing: Option<&str>, pairs: &[(&str, JsonValue)]) -> String {
    let mut map = existing
        .and_then(|s| serde_json::from_str::<JsonValue>(s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    for (k, v) in pairs {
        map.insert((*k).to_string(), v.clone());
    }
    JsonValue::Object(map).to_string()
}

#[cfg(test)]
mod create_channel_tests {
    use super::*;

    #[test]
    fn parses_new_api_wrapped_single() {
        let body = json!({
            "mode": "single",
            "channel": {
                "name": "demo",
                "type": 1,
                "key": "sk-test",
                "models": "gpt-4o",
                "group": "default",
                "base_url": null,
                "weight": null,
                "priority": null
            }
        });
        let channels = expand_create_channel_body(body).expect("parse");
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].key, "sk-test");
        assert_eq!(channels[0].name, "demo");
        assert_eq!(channels[0].channel_type, 1);
    }

    #[test]
    fn batch_splits_keys_and_prefixes_name() {
        let body = json!({
            "mode": "batch",
            "batch_add_set_key_prefix_2_name": true,
            "channel": {
                "name": "pool",
                "type": 1,
                "key": "sk-aaa\nsk-bbb\n",
                "models": "gpt-4o",
                "group": "default"
            }
        });
        let channels = expand_create_channel_body(body).expect("parse");
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].key, "sk-aaa");
        assert_eq!(channels[1].key, "sk-bbb");
        assert_eq!(channels[0].name, "pool sk-aaa");
        assert_eq!(channels[1].name, "pool sk-bbb");
    }

    #[test]
    fn multi_to_single_joins_keys() {
        let body = json!({
            "mode": "multi_to_single",
            "multi_key_mode": "polling",
            "channel": {
                "name": "mk",
                "type": 1,
                "key": "k1\nk2",
                "models": "gpt-4o",
                "group": "default",
                "setting": null
            }
        });
        let channels = expand_create_channel_body(body).expect("parse");
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].key, "k1\nk2");
        let setting = channels[0].setting.as_deref().unwrap_or("");
        assert!(setting.contains("is_multi_key"));
        assert!(setting.contains("polling"));
    }
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
                channel.key = key_json;
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
    match auth_import::import_auth(&state.management, &state.proxies, req).await {
        Ok(result) => {
            let mutated = result
                .channels
                .as_ref()
                .is_some_and(|c| c.created > 0 || c.updated > 0)
                || result.data.as_ref().is_some_and(|d| {
                    d.proxy_created > 0 || d.account_created > 0 || d.proxy_reused > 0
                });
            if mutated {
                if let Err(err) = publish_management_snapshot(&state).await {
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
/// - `group`, `models`, `name`, `update_existing` (optional)
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
    let mut group = None;
    let mut models = None;
    let mut name = String::new();
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
            "group" => group = Some(text.trim().to_string()).filter(|s| !s.is_empty()),
            "models" => models = Some(text.trim().to_string()).filter(|s| !s.is_empty()),
            "name" => name = text.trim().to_string(),
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
        content: String::new(),
        contents,
        filenames,
        name,
        group,
        models,
        base_url: None,
        proxy_id: None,
        update_existing,
        data: None,
    };

    match auth_import::import_auth(&state.management, &state.proxies, req).await {
        Ok(result) => {
            let mutated = result
                .channels
                .as_ref()
                .is_some_and(|c| c.created > 0 || c.updated > 0)
                || result.data.as_ref().is_some_and(|d| {
                    d.proxy_created > 0 || d.account_created > 0 || d.proxy_reused > 0
                });
            if mutated {
                if let Err(err) = publish_management_snapshot(&state).await {
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
    match sub2api_data_import::import_sub2api_data(&state.management, &state.proxies, req).await {
        Ok(result) => {
            if result.proxy_created > 0 || result.account_created > 0 || result.proxy_reused > 0 {
                if let Err(err) = publish_management_snapshot(&state).await {
                    return management_error(err);
                }
            }
            api_success(result)
        }
        Err(err) => management_error(err),
    }
}

pub(crate) async fn update_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<JsonValue>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    // new-api PUT body is a flat channel-like object with extra fields + nulls.
    // Track which keys the client actually sent so serde defaults do not wipe
    // status / counters that the admin form never includes.
    let body_has_status = body
        .as_object()
        .is_some_and(|obj| obj.contains_key("status") && !obj["status"].is_null());
    let mut channel = match channel_record_from_json(body) {
        Ok(c) => c,
        Err(msg) => return api_error_status(StatusCode::BAD_REQUEST, &msg),
    };
    if channel.id == 0 {
        return api_error_status(StatusCode::BAD_REQUEST, "channel id is required");
    }
    if !body_has_status {
        // Frontend update payload omits status; keep existing enable/disable state.
        if let Ok(data) = state.management.current_data() {
            if let Some(existing) = data.channels.iter().find(|c| c.id == channel.id) {
                channel.status = existing.status;
            }
        }
    }
    match state
        .management
        .call(UpdateChannelRequest { channel })
        .await
    {
        Ok(_) => match publish_management_snapshot(&state).await {
            Ok(()) => api_ok(),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

pub(crate) async fn delete_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.management.call(DeleteChannelRequest { id }).await {
        Ok(()) => match publish_management_snapshot(&state).await {
            Ok(()) => api_ok(),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

pub(crate) async fn delete_channel_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<BatchIds>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    for id in req.ids {
        if let Err(err) = state.management.call(DeleteChannelRequest { id }).await {
            return management_error(err);
        }
    }
    match publish_management_snapshot(&state).await {
        Ok(()) => api_ok(),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn update_channel_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
    Json(req): Json<StatusUpdate>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .management
        .call(ChannelStatusUpdateRequest {
            id,
            status: req.status,
        })
        .await
    {
        Ok(_) => match publish_management_snapshot(&state).await {
            Ok(()) => api_ok(),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

pub(crate) async fn update_channel_status_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<Vec<ChannelStatusUpdateRequest>>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    for item in req {
        if let Err(err) = state.management.call(item).await {
            return management_error(err);
        }
    }
    match publish_management_snapshot(&state).await {
        Ok(()) => api_ok(),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn channel_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let data = match state.management.current_data() {
        Ok(data) => data,
        Err(err) => return management_error(err),
    };
    let mut models = data
        .channels
        .into_iter()
        .flat_map(|channel| channel.model_list())
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();
    api_success(models)
}

pub(crate) async fn channel_ops(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let options = state.options.values().unwrap_or_default();
    api_success(json!({
        "retry_times": option_i64(&options, "RetryTimes", 0),
    }))
}

pub(crate) async fn fetch_models_for_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelProbeService::new(state.management.clone());
    match service
        .call(FetchModelsRequest {
            channel_id:      Some(id),
            base_url:        String::new(),
            channel_type:    1,
            key:             String::new(),
            header_override: None,
        })
        .await
    {
        Ok(models) => api_success(models),
        Err(err) => api_error_status(StatusCode::OK, &format!("获取模型列表失败: {err}")),
    }
}

pub(crate) async fn fetch_models_for_channel_payload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<FetchModelsRequest>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelProbeService::new(state.management.clone());
    match service.call(payload).await {
        Ok(models) => api_success(models),
        Err(err) => api_error_status(StatusCode::OK, &err.to_string()),
    }
}

pub(crate) async fn copy_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
    Query(query): Query<CopyChannelQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelOpsService::new(state.management.clone());
    match service
        .call(CopyChannelRequest {
            id,
            suffix: query.suffix,
            reset_balance: query.reset_balance,
        })
        .await
    {
        Ok(data) => match publish_management_snapshot(&state).await {
            Ok(()) => api_success(data),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

pub(crate) async fn update_channel_balance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelOpsService::new(state.management.clone());
    match service
        .call(UpdateChannelBalanceRequest {
            id,
            price: channel_balance_price(&state),
        })
        .await
    {
        Ok(balance) => match publish_management_snapshot(&state).await {
            Ok(()) => api_success_with_extra(json!({ "balance": balance })),
            Err(err) => management_error(err),
        },
        Err(err) => api_error_status(StatusCode::OK, &err.to_string()),
    }
}

pub(crate) async fn update_all_channel_balances(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelOpsService::new(state.management.clone());
    match service
        .call(UpdateAllChannelBalancesRequest {
            price: channel_balance_price(&state),
        })
        .await
    {
        Ok(data) => match publish_management_snapshot(&state).await {
            Ok(()) => api_success(data),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

pub(crate) async fn test_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
    Query(query): Query<ChannelTestQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelOpsService::new(state.management.clone());
    match service
        .call(TestChannelRequest {
            id,
            model: query.model,
            endpoint_type: query.endpoint_type,
            stream: query.stream,
        })
        .await
    {
        Ok(data) => match publish_management_snapshot(&state).await {
            Ok(()) => api_success_with_extra(json!({ "time": data.time })),
            Err(err) => management_error(err),
        },
        Err(err) => Json(json!({
            "success": false,
            "message": err.to_string(),
            "time": 0.0,
        }))
        .into_response(),
    }
}

pub(crate) async fn test_all_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ChannelTestAllQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .system_tasks
        .call(EnqueueSystemTaskRequest {
            task_type: SYSTEM_TASK_TYPE_CHANNEL_TEST,
            payload:   Some(json!(ChannelTestTaskPayload::manual(query.stream))),
            state:     Some(json!(SystemTaskProgressState::default())),
        })
        .await
    {
        Ok(enqueued) if enqueued.created => {
            let task = enqueued.task;
            spawn_channel_test_task(
                state.system_tasks.clone(),
                state.management.clone(),
                state.options.clone(),
                state.snapshots.clone(),
                state.proxies.clone(),
                task.task_id.clone(),
            );
            api_success(json!({
                "task_id": task.task_id,
                "status": task.status,
            }))
        }
        Ok(enqueued) => system_task_conflict(
            &enqueued.task,
            "已有通道测试任务正在运行或等待中，不能启动本次手动任务",
        ),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn fix_channel_abilities(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelOpsService::new(state.management.clone());
    match service.call(FixChannelAbilitiesRequest).await {
        Ok(data) => match publish_management_snapshot(&state).await {
            Ok(()) => api_success(data),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn detect_channel_upstream_model_updates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DetectChannelUpstreamModelUpdatesRequest>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelOpsService::new(state.management.clone());
    match service.call(req).await {
        Ok(data) => match publish_management_snapshot(&state).await {
            Ok(()) => api_success(data),
            Err(err) => management_error(err),
        },
        Err(err) => api_error_status(StatusCode::OK, &err.to_string()),
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn detect_all_channel_upstream_model_updates(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .system_tasks
        .call(EnqueueSystemTaskRequest {
            task_type: SYSTEM_TASK_TYPE_MODEL_UPDATE,
            payload:   Some(json!(ModelUpdateTaskPayload::manual())),
            state:     Some(json!(SystemTaskProgressState::default())),
        })
        .await
    {
        Ok(enqueued) if enqueued.created => {
            let task = enqueued.task;
            spawn_model_update_task(
                state.system_tasks.clone(),
                state.management.clone(),
                state.options.clone(),
                state.snapshots.clone(),
                state.proxies.clone(),
                task.task_id.clone(),
            );
            api_success(json!({
                "task_id": task.task_id,
                "status": task.status,
            }))
        }
        Ok(enqueued) => system_task_conflict(
            &enqueued.task,
            "已有模型更新任务正在运行或等待中，不能启动本次手动任务",
        ),
        Err(err) => management_error(err),
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn apply_channel_upstream_model_updates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ApplyChannelUpstreamModelUpdatesRequest>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelOpsService::new(state.management.clone());
    match service.call(req).await {
        Ok(data) => match publish_management_snapshot(&state).await {
            Ok(()) => api_success(data),
            Err(err) => management_error(err),
        },
        Err(err) => api_error_status(StatusCode::OK, &err.to_string()),
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn apply_all_channel_upstream_model_updates(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelOpsService::new(state.management.clone());
    match service
        .call(ApplyAllChannelUpstreamModelUpdatesRequest)
        .await
    {
        Ok(data) => match publish_management_snapshot(&state).await {
            Ok(()) => api_success(data),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn manage_multi_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<MultiKeyManageRequest>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelSpecialService::new(state.management.clone());
    match service.call(req).await {
        Ok(data) => match publish_management_snapshot(&state).await {
            Ok(()) => Json(data).into_response(),
            Err(err) => management_error(err),
        },
        Err(err) => api_error_status(StatusCode::OK, &err.to_string()),
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn ollama_pull_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OllamaModelRequestBody>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelSpecialService::new(state.management.clone());
    match service
        .call(OllamaPullModelRequest {
            channel_id: req.channel_id,
            model_name: req.model_name,
            stream:     false,
        })
        .await
    {
        Ok(data) => Json(json!({ "success": true, "message": data.message })).into_response(),
        Err(err) => api_error_status(StatusCode::OK, &format!("Failed to pull model: {err}")),
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn ollama_pull_model_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OllamaModelRequestBody>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelSpecialService::new(state.management.clone());
    match service
        .call(OllamaPullModelRequest {
            channel_id: req.channel_id,
            model_name: req.model_name,
            stream:     true,
        })
        .await
    {
        Ok(data) => (
            [
                ("content-type", "text/event-stream"),
                ("cache-control", "no-cache"),
                ("connection", "keep-alive"),
                ("access-control-allow-origin", "*"),
            ],
            data.event_stream_body(),
        )
            .into_response(),
        Err(err) => (
            [("content-type", "text/event-stream")],
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({ "error": err.to_string() })
            ),
        )
            .into_response(),
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn ollama_delete_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OllamaModelRequestBody>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelSpecialService::new(state.management.clone());
    match service
        .call(OllamaDeleteModelRequest {
            channel_id: req.channel_id,
            model_name: req.model_name,
        })
        .await
    {
        Ok(message) => Json(json!({ "success": true, "message": message })).into_response(),
        Err(err) => api_error_status(StatusCode::OK, &format!("Failed to delete model: {err}")),
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn ollama_version(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelSpecialService::new(state.management.clone());
    match service.call(OllamaVersionRequest { id }).await {
        Ok(data) => api_success(data),
        Err(err) => api_error_status(StatusCode::OK, &format!("获取Ollama版本失败: {err}")),
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn refresh_codex_channel_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelSpecialService::new(state.management.clone());
    match service.call(CodexRefreshCredentialRequest { id }).await {
        Ok(data) => match publish_management_snapshot(&state).await {
            Ok(()) => api_success_with_message("refreshed", data),
            Err(err) => management_error(err),
        },
        Err(err) => {
            warn!(%err, "failed to refresh codex channel credential");
            api_error_status(StatusCode::OK, "刷新凭证失败，请稍后重试")
        }
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn get_codex_channel_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    codex_wham_response(&state, &headers, id, CodexWhamKind::Usage).await
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn get_codex_channel_rate_limit_reset_credits(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    codex_wham_response(&state, &headers, id, CodexWhamKind::ResetCredits).await
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn reset_codex_channel_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    codex_wham_response(&state, &headers, id, CodexWhamKind::ConsumeResetCredit).await
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn codex_wham_response(
    state: &AppState,
    headers: &HeaderMap,
    id: u64,
    kind: CodexWhamKind,
) -> Response {
    let _actor = match require_role(state, headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelSpecialService::new(state.management.clone());
    match service.call(CodexWhamRequest { id, kind }).await {
        Ok(data) => {
            let _ = publish_management_snapshot(state).await;
            let message = if data.success {
                String::new()
            } else {
                format!("upstream status: {}", data.upstream_status)
            };
            Json(json!({
                "success": data.success,
                "message": message,
                "upstream_status": data.upstream_status,
                "data": data.data,
            }))
            .into_response()
        }
        Err(err) => api_error_status(StatusCode::OK, &err.to_string()),
    }
}

pub(crate) async fn delete_disabled_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.management.call(DeleteDisabledChannelsRequest).await {
        Ok(rows) => match publish_management_snapshot(&state).await {
            Ok(()) => api_success(rows),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

pub(crate) async fn batch_set_channel_tag(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChannelBatchTagPayload>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    if req.ids.is_empty() {
        return api_error_status(StatusCode::OK, "参数错误");
    }
    match state
        .management
        .call(BatchSetChannelTagRequest {
            ids: req.ids,
            tag: req.tag,
        })
        .await
    {
        Ok(rows) => match publish_management_snapshot(&state).await {
            Ok(()) => api_success(rows),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

pub(crate) async fn disable_tag_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChannelTagPayload>,
) -> Response {
    update_channels_by_tag(&state, &headers, req.tag, ChannelTagPatch {
        status: Some(0),
        ..ChannelTagPatch::default()
    })
    .await
}

pub(crate) async fn enable_tag_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChannelTagPayload>,
) -> Response {
    update_channels_by_tag(&state, &headers, req.tag, ChannelTagPatch {
        status: Some(STATUS_ENABLED),
        ..ChannelTagPatch::default()
    })
    .await
}

pub(crate) async fn edit_tag_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChannelTagPayload>,
) -> Response {
    if req.tag.trim().is_empty() {
        return api_error_status(StatusCode::OK, "tag不能为空");
    }
    if let Err(resp) = validate_json_override("参数覆盖", req.param_override.as_deref()) {
        return resp;
    }
    if let Err(resp) = validate_json_override("请求头覆盖", req.header_override.as_deref()) {
        return resp;
    }
    update_channels_by_tag(&state, &headers, req.tag, ChannelTagPatch {
        status:          None,
        new_tag:         req.new_tag,
        priority:        req.priority,
        weight:          req.weight,
        model_mapping:   req.model_mapping,
        models:          req.models,
        groups:          req.groups,
        param_override:  req.param_override,
        header_override: req.header_override,
    })
    .await
}

pub(crate) async fn tag_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let tag = query.get("tag").map(String::as_str).unwrap_or_default();
    if tag.is_empty() {
        return api_error_status(StatusCode::BAD_REQUEST, "tag不能为空");
    }
    let data = match state.management.current_data() {
        Ok(data) => data,
        Err(err) => return management_error(err),
    };
    let mut longest_models = String::new();
    let mut max_len = 0usize;
    for channel in data.channels {
        if channel.tag.as_deref() != Some(tag) || channel.models.is_empty() {
            continue;
        }
        let len = channel.model_list().len();
        if len > max_len {
            max_len = len;
            longest_models = channel.models;
        }
    }
    api_success(longest_models)
}

pub(crate) async fn update_channels_by_tag(
    state: &AppState,
    headers: &HeaderMap,
    tag: String,
    patch: ChannelTagPatch,
) -> Response {
    let _actor = match require_role(state, headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    if tag.trim().is_empty() {
        return api_error_status(StatusCode::OK, "参数错误");
    }
    match state
        .management
        .call(UpdateChannelsByTagRequest { tag, patch })
        .await
    {
        Ok(_) => match publish_management_snapshot(state).await {
            Ok(()) => api_ok(),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

pub(crate) fn validate_json_override(label: &str, value: Option<&str>) -> Result<(), Response> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    if serde_json::from_str::<JsonValue>(value).is_err() {
        return Err(api_error_status(
            StatusCode::OK,
            &format!("{label}必须是合法的 JSON 格式"),
        ));
    }
    Ok(())
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn list_proxies(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.proxies.call(ListProxiesRequest).await {
        Ok(items) => api_success(items),
        Err(err) => management_error(err),
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn get_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.proxies.call(GetProxyRequest { id }).await {
        Ok(item) => api_success(item),
        Err(err) => management_error(err),
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn create_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(proxy): Json<ProxyRecord>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.proxies.call(CreateProxyRequest { proxy }).await {
        Ok(item) => {
            if let Err(err) = publish_management_snapshot(&state).await {
                return management_error(err);
            }
            api_success(item)
        }
        Err(err) => management_error(err),
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn update_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(proxy): Json<ProxyRecord>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.proxies.call(UpdateProxyRequest { proxy }).await {
        Ok(item) => {
            if let Err(err) = publish_management_snapshot(&state).await {
                return management_error(err);
            }
            api_success(item)
        }
        Err(err) => management_error(err),
    }
}

#[cfg(feature = "admin-extras")]
pub(crate) async fn delete_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.proxies.call(DeleteProxyRequest { id }).await {
        Ok(()) => {
            if let Err(err) = publish_management_snapshot(&state).await {
                return management_error(err);
            }
            api_ok()
        }
        Err(err) => management_error(err),
    }
}

/// Sub2API-style connectivity test: exit IP + latency via the proxy.
#[cfg(feature = "admin-extras")]
pub(crate) async fn test_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match crate::proxy_probe::test_proxy(&state.proxies, id).await {
        Ok(result) => api_success(result),
        Err(err) => management_error(err),
    }
}

/// Sub2API-style quality check: base connectivity + AI API reachability.
#[cfg(feature = "admin-extras")]
pub(crate) async fn quality_check_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match crate::proxy_probe::check_proxy_quality(&state.proxies, id).await {
        Ok(result) => api_success(result),
        Err(err) => management_error(err),
    }
}

pub(crate) fn channel_balance_price(state: &AppState) -> f64 {
    state
        .options
        .values()
        .map(|options| option_f64(&options, "Price", 7.3))
        .unwrap_or(7.3)
}
