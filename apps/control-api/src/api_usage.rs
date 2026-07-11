//! Usage logs, data charts, and system-task read handlers.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use halolake_control_plane::{
    ManagementData, ManagementError, UsageError,
};
use halolake_domain::{
    PageRequest, ROLE_ADMIN_USER, ROLE_ROOT_USER, TokenRecord, UsageEvent, UsageStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use service_async::Service;
use axum::response::IntoResponse;
use halolake_api_contract::ApiResponse;

use crate::channel_affinity::{
    ChannelAffinityService, GetChannelAffinityUsageCacheStatsRequest,
};
use crate::http_auth::{current_user, require_role, token_from_read_only_auth};
use crate::storage::DeleteUsageBeforeRequest;
use crate::http_response::{
    api_error_status, api_success, management_error, page_items, usage_error,
};
use crate::system_instance::{
    DeleteStaleSystemInstanceRequest, DeleteStaleSystemInstancesRequest, ListSystemInstancesRequest,
};
use crate::system_task::{
    GetCurrentSystemTaskRequest, GetSystemTaskRequest,
    ListSystemTasksRequest, StartLogCleanupTaskRequest, spawn_log_cleanup_task,
};
use crate::{
    AppState, MAX_RECENT_TOKEN_LOGS, now_unix,
};

#[derive(Debug, Clone, Copy)]
pub(crate) enum QuotaGrouping {
    Model,
    User,
    UserModel,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct LogQuery {
    #[serde(default = "crate::default_page")]
    page: usize,
    #[serde(default = "crate::default_page_size")]
    page_size: usize,
    #[serde(rename = "type", default)]
    log_type: i32,
    #[serde(default)]
    start_timestamp: i64,
    #[serde(default)]
    end_timestamp: i64,
    #[serde(default)]
    model_name: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    token_name: String,
    #[serde(default)]
    channel: String,
    #[serde(default)]
    group: String,
    #[serde(default)]
    request_id: String,
    #[serde(default)]
    upstream_request_id: String,
}


#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(crate) struct DeleteLogQuery {
    #[serde(default)]
    target_timestamp: i64,
}


#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(crate) struct LogCleanupTaskQuery {
    #[serde(default)]
    target_timestamp: i64,
}


#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct CurrentSystemTaskQuery {
    #[serde(rename = "type", default)]
    task_type: String,
}


#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(crate) struct ListSystemTaskQuery {
    #[serde(default)]
    limit: usize,
}


#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ChannelAffinityUsageCacheQuery {
    #[serde(default)]
    rule_name: String,
    #[serde(default)]
    using_group: String,
    #[serde(default)]
    key_fp: String,
}


#[derive(Debug, Clone, Serialize)]
pub(crate) struct LogRecord {
    id: usize,
    user_id: String,
    created_at: i64,
    #[serde(rename = "type")]
    log_type: i32,
    content: String,
    username: String,
    token_name: String,
    model_name: String,
    quota: i64,
    prompt_tokens: u64,
    completion_tokens: u64,
    use_time: u64,
    is_stream: bool,
    channel: String,
    channel_name: String,
    token_id: String,
    group: String,
    ip: String,
    request_id: String,
    upstream_request_id: String,
    other: String,
}


#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct DataQuery {
    #[serde(default)]
    start_timestamp: i64,
    #[serde(default)]
    end_timestamp: i64,
    #[serde(default)]
    username: String,
}


#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct QuotaDataRecord {
    id: usize,
    user_id: String,
    username: String,
    model_name: String,
    created_at: i64,
    use_group: String,
    token_id: String,
    channel_id: String,
    node_name: String,
    token_used: u64,
    count: usize,
    quota: i64,
}


pub(crate) async fn list_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LogQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    usage_log_page(&state, &query, None)
}


pub(crate) async fn list_self_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LogQuery>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let user_id = user.id.to_string();
    usage_log_page(&state, &query, Some(&user_id))
}


pub(crate) async fn list_token_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LogQuery>,
) -> Response {
    let token = match token_from_read_only_auth(&state, &headers) {
        Ok(token) => token,
        Err(resp) => return resp,
    };
    match token_usage_log_records(&state, &query, &token) {
        Ok(records) => api_success(records),
        Err(err) => usage_error(err),
    }
}


pub(crate) async fn delete_history_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DeleteLogQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    if query.target_timestamp == 0 {
        return api_error_status(StatusCode::OK, "target timestamp is required");
    }
    match state
        .usage_events
        .call(DeleteUsageBeforeRequest {
            target_timestamp: query.target_timestamp,
        })
        .await
    {
        Ok(ack) => api_success(ack.deleted),
        Err(err) => usage_error(err),
    }
}


pub(crate) async fn search_logs_deprecated() -> Response {
    Json(ApiResponse::<()>::error("该接口已废弃")).into_response()
}


pub(crate) async fn get_channel_affinity_usage_cache_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ChannelAffinityUsageCacheQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelAffinityService::new(state.options.clone());
    match service
        .call(GetChannelAffinityUsageCacheStatsRequest {
            rule_name: query.rule_name,
            using_group: query.using_group,
            key_fp: query.key_fp,
        })
        .await
    {
        Ok(stats) => api_success(stats),
        Err(ManagementError::InvalidRequest(message)) => {
            api_error_status(StatusCode::BAD_REQUEST, message)
        }
        Err(err) => management_error(err),
    }
}


pub(crate) async fn create_log_cleanup_system_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LogCleanupTaskQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    if query.target_timestamp == 0 {
        return api_error_status(StatusCode::OK, "target timestamp is required");
    }

    match state
        .system_tasks
        .call(StartLogCleanupTaskRequest {
            target_timestamp: query.target_timestamp,
        })
        .await
    {
        Ok(task) => {
            spawn_log_cleanup_task(
                state.system_tasks.clone(),
                state.usage_events.clone(),
                task.task_id.clone(),
            );
            api_success(task)
        }
        Err(err) => management_error(err),
    }
}


pub(crate) async fn get_current_system_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CurrentSystemTaskQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    if query.task_type.is_empty() {
        return api_error_status(StatusCode::OK, "type is required");
    }

    match state
        .system_tasks
        .call(GetCurrentSystemTaskRequest {
            task_type: query.task_type,
        })
        .await
    {
        Ok(task) => api_success(task),
        Err(err) => management_error(err),
    }
}


pub(crate) async fn list_system_tasks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListSystemTaskQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .system_tasks
        .call(ListSystemTasksRequest { limit: query.limit })
        .await
    {
        Ok(tasks) => api_success(tasks),
        Err(err) => management_error(err),
    }
}


pub(crate) async fn get_system_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    if task_id.is_empty() {
        return api_error_status(StatusCode::OK, "task id is required");
    }
    match state
        .system_tasks
        .call(GetSystemTaskRequest { task_id })
        .await
    {
        Ok(Some(task)) => api_success(task),
        Ok(None) => api_error_status(StatusCode::NOT_FOUND, "task not found"),
        Err(err) => management_error(err),
    }
}


pub(crate) async fn list_system_instances(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .system_instances
        .call(ListSystemInstancesRequest { now: now_unix() })
        .await
    {
        Ok(instances) => api_success(instances),
        Err(err) => management_error(err),
    }
}


pub(crate) async fn delete_stale_system_instances(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .system_instances
        .call(DeleteStaleSystemInstancesRequest { now: now_unix() })
        .await
    {
        Ok(deleted) => api_success(json!({ "deleted_count": deleted })),
        Err(err) => management_error(err),
    }
}


pub(crate) async fn delete_stale_system_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(node_name): Path<String>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    if node_name.trim().is_empty() {
        return api_error_status(StatusCode::OK, "node name is required");
    }
    match state
        .system_instances
        .call(DeleteStaleSystemInstanceRequest {
            node_name,
            now: now_unix(),
        })
        .await
    {
        Ok(true) => api_success(json!({ "deleted_count": 1 })),
        Ok(false) => api_error_status(StatusCode::OK, "instance is not stale or no longer exists"),
        Err(err) => management_error(err),
    }
}


pub(crate) async fn log_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LogQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    usage_log_stats(&state, &query, None)
}


pub(crate) async fn self_log_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LogQuery>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let user_id = user.id.to_string();
    usage_log_stats(&state, &query, Some(&user_id))
}


pub(crate) async fn data_all_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DataQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    quota_data_response(&state, &query, None, QuotaGrouping::Model)
}


pub(crate) async fn data_quota_by_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DataQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    quota_data_response(&state, &query, None, QuotaGrouping::User)
}


pub(crate) async fn data_self_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DataQuery>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let user_id = user.id.to_string();
    quota_data_response(&state, &query, Some(&user_id), QuotaGrouping::UserModel)
}


pub(crate) async fn data_all_flow(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DataQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    if let Err(resp) = validate_flow_time_range(&query) {
        return resp;
    }
    quota_data_response(&state, &query, None, QuotaGrouping::UserModel)
}


pub(crate) async fn data_self_flow(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DataQuery>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    if let Err(resp) = validate_flow_time_range(&query) {
        return resp;
    }
    let user_id = user.id.to_string();
    quota_data_response(&state, &query, Some(&user_id), QuotaGrouping::UserModel)
}


pub(crate) fn usage_log_page(state: &AppState, query: &LogQuery, user_id: Option<&str>) -> Response {
    match usage_log_records(state, query, user_id) {
        Ok(records) => api_success(page_items(
            records,
            PageRequest {
                page: query.page,
                page_size: query.page_size,
            },
        )),
        Err(err) => usage_error(err),
    }
}


pub(crate) fn usage_log_stats(state: &AppState, query: &LogQuery, user_id: Option<&str>) -> Response {
    match filtered_usage_events(state, query, user_id) {
        Ok(events) => {
            let quota = events.iter().map(usage_event_quota_value).sum::<i64>();
            api_success(json!({
                "quota": quota,
                "rpm": events.len(),
                "tpm": quota,
            }))
        }
        Err(err) => usage_error(err),
    }
}


pub(crate) fn usage_log_records(
    state: &AppState,
    query: &LogQuery,
    user_id: Option<&str>,
) -> Result<Vec<LogRecord>, UsageError> {
    let events = filtered_usage_events(state, query, user_id)?;
    usage_log_records_from_events(state, events)
}


pub(crate) fn token_usage_log_records(
    state: &AppState,
    query: &LogQuery,
    token: &TokenRecord,
) -> Result<Vec<LogRecord>, UsageError> {
    let token_ids = token_usage_ids(token);
    let events = filtered_usage_events(state, query, None)?
        .into_iter()
        .filter(|event| token_ids.contains(&event.token_id))
        .take(MAX_RECENT_TOKEN_LOGS)
        .collect();
    usage_log_records_from_events(state, events)
}


pub(crate) fn usage_log_records_from_events(
    state: &AppState,
    events: Vec<UsageEvent>,
) -> Result<Vec<LogRecord>, UsageError> {
    let data = state
        .management
        .current_data()
        .map_err(|_| UsageError::Unavailable)?;
    let user_names = data
        .users
        .iter()
        .map(|user| (user.id.to_string(), user.username.clone()))
        .collect::<HashMap<_, _>>();
    let user_groups = data
        .users
        .iter()
        .map(|user| (user.id.to_string(), user.group.clone()))
        .collect::<HashMap<_, _>>();
    let token_names = data
        .tokens
        .iter()
        .flat_map(|token| {
            [
                (token.id.to_string(), token.name.clone()),
                (
                    token
                        .snapshot_id
                        .clone()
                        .unwrap_or_else(|| token.id.to_string()),
                    token.name.clone(),
                ),
            ]
        })
        .collect::<HashMap<_, _>>();
    let token_groups = data
        .tokens
        .into_iter()
        .flat_map(|token| {
            [
                (token.id.to_string(), token.group.clone()),
                (
                    token.snapshot_id.unwrap_or_else(|| token.id.to_string()),
                    token.group,
                ),
            ]
        })
        .collect::<HashMap<_, _>>();
    let channel_names = data
        .channels
        .iter()
        .flat_map(|channel| {
            [
                (channel.id.to_string(), channel.name.clone()),
                (
                    channel
                        .snapshot_id
                        .clone()
                        .unwrap_or_else(|| channel.id.to_string()),
                    channel.name.clone(),
                ),
            ]
        })
        .collect::<HashMap<_, _>>();
    let channel_groups = data
        .channels
        .into_iter()
        .flat_map(|channel| {
            let group = channel.group_list().into_iter().next().unwrap_or_default();
            [
                (channel.id.to_string(), group.clone()),
                (
                    channel
                        .snapshot_id
                        .unwrap_or_else(|| channel.id.to_string()),
                    group,
                ),
            ]
        })
        .collect::<HashMap<_, _>>();

    Ok(events
        .into_iter()
        .enumerate()
        .map(|(idx, event)| {
            usage_log_record(
                idx + 1,
                event,
                &user_names,
                &user_groups,
                &token_names,
                &token_groups,
                &channel_names,
                &channel_groups,
            )
        })
        .collect())
}


pub(crate) fn token_usage_ids(token: &TokenRecord) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    ids.insert(token.id.to_string());
    if let Some(snapshot_id) = &token.snapshot_id {
        ids.insert(snapshot_id.clone());
    }
    ids
}


pub(crate) fn filtered_usage_events(
    state: &AppState,
    query: &LogQuery,
    user_id: Option<&str>,
) -> Result<Vec<UsageEvent>, UsageError> {
    let mut events = state.usage_events.events()?;
    events.sort_by_key(|event| std::cmp::Reverse(event.created_at_unix_ms));
    let needs_group_lookup = !query.group.is_empty();
    let needs_filter_lookup =
        !query.username.is_empty() || !query.token_name.is_empty() || !query.channel.is_empty();
    let lookup_data = if needs_group_lookup || needs_filter_lookup {
        Some(
            state
                .management
                .current_data()
                .map_err(|_| UsageError::Unavailable)?,
        )
    } else {
        None
    };
    let group_lookup = lookup_data
        .as_ref()
        .filter(|_| needs_group_lookup)
        .map(UsageGroupLookup::from_data);
    let filter_lookup = lookup_data
        .as_ref()
        .filter(|_| needs_filter_lookup)
        .map(UsageFilterLookup::from_data);
    Ok(events
        .into_iter()
        .filter(|event| user_id.is_none_or(|user_id| event.user_id == user_id))
        .filter(|event| {
            let created_at = event.created_at_unix_ms / 1000;
            (query.start_timestamp == 0 || created_at >= query.start_timestamp)
                && (query.end_timestamp == 0 || created_at <= query.end_timestamp)
        })
        .filter(|event| query.log_type == 0 || usage_log_type(event.status) == query.log_type)
        .filter(|event| query.model_name.is_empty() || event.model.contains(&query.model_name))
        .filter(|event| {
            query.channel.is_empty()
                || filter_lookup
                    .as_ref()
                    .is_some_and(|lookup| lookup.matches_channel(event, &query.channel))
        })
        .filter(|event| {
            group_lookup
                .as_ref()
                .is_none_or(|lookup| lookup.group(event) == query.group)
        })
        .filter(|event| query.request_id.is_empty() || event.request_id == query.request_id)
        .filter(|event| {
            query.upstream_request_id.is_empty()
                || event.upstream_request_id == query.upstream_request_id
        })
        .filter(|event| {
            query.username.is_empty()
                || filter_lookup
                    .as_ref()
                    .is_some_and(|lookup| lookup.matches_username(event, &query.username))
        })
        .filter(|event| {
            query.token_name.is_empty()
                || filter_lookup
                    .as_ref()
                    .is_some_and(|lookup| lookup.matches_token(event, &query.token_name))
        })
        .collect())
}


pub(crate) fn usage_log_record(
    id: usize,
    event: UsageEvent,
    user_names: &HashMap<String, String>,
    user_groups: &HashMap<String, String>,
    token_names: &HashMap<String, String>,
    token_groups: &HashMap<String, String>,
    channel_names: &HashMap<String, String>,
    channel_groups: &HashMap<String, String>,
) -> LogRecord {
    let prompt_tokens = event.prompt_tokens.unwrap_or(0);
    let completion_tokens = event.completion_tokens.unwrap_or(0);
    let quota = usage_event_quota_value(&event);
    let group = usage_event_group(&event, user_groups, token_groups, channel_groups);
    let other = usage_log_other(&event);
    let content = match event.status {
        UsageStatus::Success => "consume quota".to_string(),
        UsageStatus::ClientError => "client error".to_string(),
        UsageStatus::UpstreamError => "upstream error".to_string(),
        UsageStatus::GatewayError => "gateway error".to_string(),
    };
    LogRecord {
        id,
        user_id: event.user_id.clone(),
        created_at: event.created_at_unix_ms / 1000,
        log_type: usage_log_type(event.status),
        content,
        username: user_names
            .get(&event.user_id)
            .cloned()
            .unwrap_or_else(|| event.user_id.clone()),
        token_name: token_names
            .get(&event.token_id)
            .cloned()
            .unwrap_or_else(|| event.token_id.clone()),
        model_name: event.model,
        quota,
        prompt_tokens,
        completion_tokens,
        // new-api Log.UseTime is seconds (model/log.go UseTimeSeconds).
        // Gateway stores latency_ms; convert so UI timing and t/s match new-api.
        use_time: event.latency_ms.div_ceil(1000),
        is_stream: event.is_stream,
        channel: event.channel_id.clone(),
        channel_name: channel_names
            .get(&event.channel_id)
            .cloned()
            .unwrap_or_else(|| event.channel_id.clone()),
        token_id: event.token_id,
        group,
        ip: event.ip,
        request_id: event.request_id,
        upstream_request_id: event.upstream_request_id,
        other,
    }
}


pub(crate) fn usage_log_other(event: &UsageEvent) -> String {
    let mut other = serde_json::Map::new();
    insert_optional_u64(&mut other, "total_tokens", event.total_tokens);
    insert_optional_u64(&mut other, "cache_read_tokens", event.cache_read_tokens);
    insert_optional_u64(
        &mut other,
        "cache_creation_tokens",
        event.cache_creation_tokens,
    );
    insert_optional_u64(&mut other, "image_tokens", event.image_tokens);
    insert_optional_u64(&mut other, "audio_tokens", event.audio_tokens);
    if other.is_empty() {
        String::new()
    } else {
        JsonValue::Object(other).to_string()
    }
}


pub(crate) fn insert_optional_u64(
    object: &mut serde_json::Map<String, JsonValue>,
    key: &str,
    value: Option<u64>,
) {
    if let Some(value) = value.filter(|value| *value > 0) {
        object.insert(key.to_string(), json!(value));
    }
}


pub(crate) fn usage_event_quota_value(event: &UsageEvent) -> i64 {
    event
        .quota
        .unwrap_or_else(|| event.observed_tokens().min(i64::MAX as u64) as i64)
}


pub(crate) struct UsageGroupLookup {
    user_groups: HashMap<String, String>,
    token_groups: HashMap<String, String>,
    channel_groups: HashMap<String, String>,
}


pub(crate) struct UsageFilterLookup {
    user_names: HashMap<String, String>,
    token_names: HashMap<String, String>,
    channel_names: HashMap<String, String>,
}


pub(crate) fn usage_event_group(
    event: &UsageEvent,
    user_groups: &HashMap<String, String>,
    token_groups: &HashMap<String, String>,
    channel_groups: &HashMap<String, String>,
) -> String {
    if !event.group.trim().is_empty() {
        return event.group.trim().to_string();
    }
    token_groups
        .get(&event.token_id)
        .filter(|group| !group.trim().is_empty())
        .or_else(|| {
            channel_groups
                .get(&event.channel_id)
                .filter(|group| !group.trim().is_empty())
        })
        .or_else(|| {
            user_groups
                .get(&event.user_id)
                .filter(|group| !group.trim().is_empty())
        })
        .cloned()
        .unwrap_or_else(|| "default".to_string())
}


pub(crate) fn usage_log_type(status: UsageStatus) -> i32 {
    match status {
        UsageStatus::Success => 2,
        UsageStatus::ClientError | UsageStatus::UpstreamError | UsageStatus::GatewayError => 5,
    }
}


pub(crate) fn quota_data_response(
    state: &AppState,
    query: &DataQuery,
    user_id: Option<&str>,
    grouping: QuotaGrouping,
) -> Response {
    match quota_data_records(state, query, user_id, grouping) {
        Ok(records) => api_success(records),
        Err(err) => usage_error(err),
    }
}


pub(crate) fn quota_data_records(
    state: &AppState,
    query: &DataQuery,
    user_id: Option<&str>,
    grouping: QuotaGrouping,
) -> Result<Vec<QuotaDataRecord>, UsageError> {
    let data = state
        .management
        .current_data()
        .map_err(|_| UsageError::Unavailable)?;
    let group_lookup = UsageGroupLookup::from_data(&data);
    let user_names = data
        .users
        .into_iter()
        .map(|user| (user.id.to_string(), user.username))
        .collect::<HashMap<_, _>>();
    let events = state.usage_events.events()?;
    let mut groups = BTreeMap::<(String, String, String, i64), QuotaDataRecord>::new();

    for event in events {
        if user_id.is_some_and(|user_id| event.user_id != user_id) {
            continue;
        }
        let created_at = event.created_at_unix_ms / 1000;
        if query.start_timestamp != 0 && created_at < query.start_timestamp {
            continue;
        }
        if query.end_timestamp != 0 && created_at > query.end_timestamp {
            continue;
        }
        let username = user_names
            .get(&event.user_id)
            .cloned()
            .unwrap_or_else(|| event.user_id.clone());
        if !query.username.is_empty() && username != query.username {
            continue;
        }
        let use_group = group_lookup.group(&event);

        let hour = created_at - created_at.rem_euclid(3600);
        let key = match grouping {
            QuotaGrouping::Model => (String::new(), String::new(), event.model.clone(), hour),
            QuotaGrouping::User => (event.user_id.clone(), username.clone(), String::new(), hour),
            QuotaGrouping::UserModel => (
                event.user_id.clone(),
                username.clone(),
                event.model.clone(),
                hour,
            ),
        };
        let record = groups.entry(key).or_insert_with(|| QuotaDataRecord {
            id: 0,
            user_id: match grouping {
                QuotaGrouping::Model => String::new(),
                QuotaGrouping::User | QuotaGrouping::UserModel => event.user_id.clone(),
            },
            username: match grouping {
                QuotaGrouping::Model => String::new(),
                QuotaGrouping::User | QuotaGrouping::UserModel => username.clone(),
            },
            model_name: match grouping {
                QuotaGrouping::User => String::new(),
                QuotaGrouping::Model | QuotaGrouping::UserModel => event.model.clone(),
            },
            created_at: hour,
            use_group: use_group.clone(),
            token_id: event.token_id.clone(),
            channel_id: event.channel_id.clone(),
            node_name: String::new(),
            token_used: 0,
            count: 0,
            quota: 0,
        });
        record.count = record.count.saturating_add(1);
        let tokens = event.observed_tokens();
        record.token_used = record.token_used.saturating_add(tokens);
        record.quota = record.quota.saturating_add(usage_event_quota_value(&event));
    }

    let mut records = groups.into_values().collect::<Vec<_>>();
    records.sort_by_key(|record| std::cmp::Reverse(record.created_at));
    for (idx, record) in records.iter_mut().enumerate() {
        record.id = idx + 1;
    }
    Ok(records)
}


pub(crate) fn validate_flow_time_range(query: &DataQuery) -> Result<(), Response> {
    if query.start_timestamp <= 0 {
        return Err(api_error_status(StatusCode::OK, "invalid start_timestamp"));
    }
    if query.end_timestamp <= 0 {
        return Err(api_error_status(StatusCode::OK, "invalid end_timestamp"));
    }
    if query.end_timestamp < query.start_timestamp {
        return Err(api_error_status(StatusCode::OK, "invalid time range"));
    }
    Ok(())
}


impl UsageGroupLookup {
    fn from_data(data: &ManagementData) -> Self {
        let user_groups = data
            .users
            .iter()
            .map(|user| (user.id.to_string(), user.group.clone()))
            .collect::<HashMap<_, _>>();
        let token_groups = data
            .tokens
            .iter()
            .flat_map(|token| {
                [
                    (token.id.to_string(), token.group.clone()),
                    (
                        token
                            .snapshot_id
                            .clone()
                            .unwrap_or_else(|| token.id.to_string()),
                        token.group.clone(),
                    ),
                ]
            })
            .collect::<HashMap<_, _>>();
        let channel_groups = data
            .channels
            .iter()
            .flat_map(|channel| {
                let channel_group = channel.group_list().into_iter().next().unwrap_or_default();
                [
                    (channel.id.to_string(), channel_group.clone()),
                    (
                        channel
                            .snapshot_id
                            .clone()
                            .unwrap_or_else(|| channel.id.to_string()),
                        channel_group,
                    ),
                ]
            })
            .collect::<HashMap<_, _>>();
        Self {
            user_groups,
            token_groups,
            channel_groups,
        }
    }

    fn group(&self, event: &UsageEvent) -> String {
        usage_event_group(
            event,
            &self.user_groups,
            &self.token_groups,
            &self.channel_groups,
        )
    }
}



impl UsageFilterLookup {
    fn from_data(data: &ManagementData) -> Self {
        let user_names = data
            .users
            .iter()
            .map(|user| (user.id.to_string(), user.username.clone()))
            .collect::<HashMap<_, _>>();
        let token_names = data
            .tokens
            .iter()
            .flat_map(|token| {
                [
                    (token.id.to_string(), token.name.clone()),
                    (
                        token
                            .snapshot_id
                            .clone()
                            .unwrap_or_else(|| token.id.to_string()),
                        token.name.clone(),
                    ),
                ]
            })
            .collect::<HashMap<_, _>>();
        let channel_names = data
            .channels
            .iter()
            .flat_map(|channel| {
                [
                    (channel.id.to_string(), channel.name.clone()),
                    (
                        channel
                            .snapshot_id
                            .clone()
                            .unwrap_or_else(|| channel.id.to_string()),
                        channel.name.clone(),
                    ),
                ]
            })
            .collect::<HashMap<_, _>>();
        Self {
            user_names,
            token_names,
            channel_names,
        }
    }

    fn matches_username(&self, event: &UsageEvent, query: &str) -> bool {
        event.user_id.contains(query)
            || self
                .user_names
                .get(&event.user_id)
                .is_some_and(|name| name == query || name.contains(query))
    }

    fn matches_token(&self, event: &UsageEvent, query: &str) -> bool {
        event.token_id.contains(query)
            || self
                .token_names
                .get(&event.token_id)
                .is_some_and(|name| name == query || name.contains(query))
    }

    fn matches_channel(&self, event: &UsageEvent, query: &str) -> bool {
        event.channel_id == query
            || self
                .channel_names
                .get(&event.channel_id)
                .is_some_and(|name| name == query || name.contains(query))
    }
}



