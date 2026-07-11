//! Channel and proxy management HTTP handlers.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use halolake_control_plane::{
    BatchSetChannelTagRequest, ChannelStatusUpdateRequest, ChannelTagPatch,
    CreateChannelRequest, DeleteChannelRequest, DeleteDisabledChannelsRequest, GetChannelRequest, ListChannelsRequest,
    RevealChannelKeyRequest, SearchChannelsRequest, UpdateChannelRequest,
    UpdateChannelsByTagRequest,
};
use halolake_domain::{
    ChannelRecord, PageRequest, ROLE_ADMIN_USER, ROLE_ROOT_USER, STATUS_ENABLED, SearchRequest,
};
use serde_json::{Value as JsonValue, json};
use service_async::Service;
use tracing::warn;

use crate::channel_ops::{
    ApplyAllChannelUpstreamModelUpdatesRequest, ApplyChannelUpstreamModelUpdatesRequest,
    ChannelOpsService, ChannelTestAllQuery, ChannelTestQuery, CopyChannelQuery, CopyChannelRequest,
    DetectChannelUpstreamModelUpdatesRequest, FixChannelAbilitiesRequest, TestChannelRequest,
    UpdateAllChannelBalancesRequest, UpdateChannelBalanceRequest,
};
use crate::channel_probe::{ChannelProbeService, FetchModelsRequest};
use crate::channel_special::{
    ChannelSpecialService, CodexRefreshCredentialRequest, CodexWhamKind, CodexWhamRequest,
    MultiKeyManageRequest, OllamaDeleteModelRequest, OllamaModelRequestBody,
    OllamaPullModelRequest, OllamaVersionRequest,
};
use crate::channel_task::{
    ChannelTestTaskPayload, ModelUpdateTaskPayload, SystemTaskProgressState,
    spawn_channel_test_task, spawn_model_update_task,
};
use crate::codex_auth_import::{
    CHANNEL_TYPE_CODEX, CodexAuthImportItem, CodexAuthImportMessage, CodexAuthImportRequest,
    CodexAuthImportResult, codex_key_to_json, collect_entries, find_existing_channel_id,
};
use crate::sub2api_data_import::{self, Sub2apiDataImportRequest};
use crate::http_response::{
    api_error_status, api_ok, api_success, api_success_with_extra, api_success_with_message,
    management_error, system_task_conflict,
};
use crate::proxy::{
    CreateProxyRequest, DeleteProxyRequest, GetProxyRequest, ListProxiesRequest, ProxyRecord,
    UpdateProxyRequest,
};
use crate::system_task::{
    EnqueueSystemTaskRequest, SYSTEM_TASK_TYPE_CHANNEL_TEST, SYSTEM_TASK_TYPE_MODEL_UPDATE,
};
use crate::{
    AppState, BatchIds, ChannelBatchTagPayload, ChannelSearchQuery, ChannelTagPayload, PageQuery,
    StatusUpdate, now_unix, option_f64, option_i64, publish_management_snapshot,
};
use crate::http_auth::{require_role, require_secure_verification};

pub(crate) async fn list_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .management
        .call(ListChannelsRequest { page: query.into() })
        .await
    {
        Ok(page) => api_success(page),
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
    match state
        .management
        .call(SearchChannelsRequest {
            search: SearchRequest {
                page: PageRequest {
                    page: query.page,
                    page_size: query.page_size,
                },
                keyword: query.keyword,
            },
        })
        .await
    {
        Ok(page) => api_success(page),
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
        Ok(channel) => api_success(channel),
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
    Json(channel): Json<ChannelRecord>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .management
        .call(CreateChannelRequest { channel })
        .await
    {
        Ok(_) => match publish_management_snapshot(&state).await {
            Ok(()) => api_ok(),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

/// Import Codex / sub2api-format auth files as type-57 channels.
///
/// Body: `{ "content": "<file or paste>", "contents": ["..."], "name", "group",
/// "models", "base_url", "proxy_id", "update_existing": true }`
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
        total: entries.len(),
        created: 0,
        updated: 0,
        skipped: 0,
        failed: 0,
        items: Vec::with_capacity(entries.len()),
        warnings: Vec::new(),
        errors: Vec::new(),
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
            id: 0,
            snapshot_id: None,
            channel_type: CHANNEL_TYPE_CODEX,
            key: key_json,
            status: STATUS_ENABLED,
            name: account_name.clone(),
            weight: req.weight.or(Some(1)),
            created_time: now_unix(),
            test_time: 0,
            response_time: 0,
            base_url: base_url.clone(),
            balance: 0.0,
            balance_updated_time: 0,
            models: models.clone(),
            group: group.clone(),
            used_quota: 0,
            model_mapping: None,
            priority: req.priority.or(Some(0)),
            auto_ban: Some(1),
            tag: None,
            setting: None,
            param_override: None,
            header_override: None,
            remark: Some(format!(
                "imported from codex/sub2api auth{}",
                if item.email.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", item.email)
                }
            )),
            proxy_id: req.proxy_id,
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

/// Import sub2api export JSON (`type: sub2api-data`) — proxies + accounts as channels.
///
/// Body accepts either `{ "data": { ...export... } }` or `{ "content": "<file text>" }`.
/// Groups are not auto-bound; set `group` to apply a default channel group.
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
    Json(channel): Json<ChannelRecord>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
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
            channel_id: Some(id),
            base_url: String::new(),
            channel_type: 1,
            key: String::new(),
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
            payload: Some(json!(ChannelTestTaskPayload::manual(query.stream))),
            state: Some(json!(SystemTaskProgressState::default())),
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


pub(crate) async fn fix_channel_abilities(State(state): State<AppState>, headers: HeaderMap) -> Response {
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
            payload: Some(json!(ModelUpdateTaskPayload::manual())),
            state: Some(json!(SystemTaskProgressState::default())),
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
            stream: false,
        })
        .await
    {
        Ok(data) => Json(json!({ "success": true, "message": data.message })).into_response(),
        Err(err) => api_error_status(StatusCode::OK, &format!("Failed to pull model: {err}")),
    }
}


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
            stream: true,
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


pub(crate) async fn get_codex_channel_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    codex_wham_response(&state, &headers, id, CodexWhamKind::Usage).await
}


pub(crate) async fn get_codex_channel_rate_limit_reset_credits(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    codex_wham_response(&state, &headers, id, CodexWhamKind::ResetCredits).await
}


pub(crate) async fn reset_codex_channel_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    codex_wham_response(&state, &headers, id, CodexWhamKind::ConsumeResetCredit).await
}


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


pub(crate) async fn delete_disabled_channels(State(state): State<AppState>, headers: HeaderMap) -> Response {
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
    update_channels_by_tag(
        &state,
        &headers,
        req.tag,
        ChannelTagPatch {
            status: Some(0),
            ..ChannelTagPatch::default()
        },
    )
    .await
}


pub(crate) async fn enable_tag_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChannelTagPayload>,
) -> Response {
    update_channels_by_tag(
        &state,
        &headers,
        req.tag,
        ChannelTagPatch {
            status: Some(STATUS_ENABLED),
            ..ChannelTagPatch::default()
        },
    )
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
    update_channels_by_tag(
        &state,
        &headers,
        req.tag,
        ChannelTagPatch {
            status: None,
            new_tag: req.new_tag,
            priority: req.priority,
            weight: req.weight,
            model_mapping: req.model_mapping,
            models: req.models,
            groups: req.groups,
            param_override: req.param_override,
            header_override: req.header_override,
        },
    )
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


pub(crate) fn channel_balance_price(state: &AppState) -> f64 {
    state
        .options
        .values()
        .map(|options| option_f64(&options, "Price", 7.3))
        .unwrap_or(7.3)
}


