//! Options, pricing, rankings, and related helpers.

use crate::{
    AppState, DEFAULT_CHANNEL_AFFINITY_RULES_JSON, DEFAULT_MODEL_RATIO_JSON,
    api_user::CheckinSetting,
    catalog::{CatalogData, ModelRecord},
    channel_affinity::{
        ChannelAffinityService, ClearChannelAffinityCacheRequest,
        GetChannelAffinityCacheStatsRequest,
    },
    config::ControlApiConfig,
    http_auth::require_role,
    http_response::{
        api_error_status, api_ok, api_success, api_success_with_extra, management_error,
        usage_error,
    },
    now_unix, publish_management_snapshot,
    ratio_sync::{FetchUpstreamRatiosRequest, ListSyncableChannelsRequest, RatioSyncService},
    storage::{ListOptionsRequest, UpdateOptionRequest},
};
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use halolake_control_plane::{ManagementData, ManagementError, UsagePricing};
use halolake_domain::{ChannelRecord, ROLE_ROOT_USER, STATUS_ENABLED, UsageEvent, UsageStatus};
use halolake_router_core::{ChannelAffinityConfig, ChannelAffinityRule, GroupRoutingConfig};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use service_async::Service;
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OptionUpdatePayload {
    key:   String,
    #[serde(default)]
    value: JsonValue,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct PaymentCompliancePayload {
    confirmed: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ChannelAffinityCacheQuery {
    #[serde(default)]
    all:       bool,
    #[serde(default)]
    rule_name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct PricingQuery {
    #[serde(default)]
    group: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RankingsQuery {
    #[serde(default = "crate::default_ranking_period")]
    period: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct PerfMetricsQuery {
    #[serde(default)]
    model: String,
    #[serde(default)]
    group: String,
    #[serde(default = "crate::default_perf_hours")]
    hours: i64,
}

pub(crate) async fn api_pricing(
    State(state): State<AppState>,
    Query(query): Query<PricingQuery>,
) -> Response {
    let options = state.options.values().unwrap_or_default();
    let management = match state.management.current_data() {
        Ok(data) => data,
        Err(err) => return management_error(err),
    };
    let catalog = match state.catalog.current_data() {
        Ok(data) => data,
        Err(err) => return management_error(err),
    };
    let pricing = pricing_records(&management, &catalog, &options, &query.group);
    let vendors = catalog
        .vendors
        .iter()
        .filter(|vendor| vendor.status == STATUS_ENABLED)
        .map(|vendor| {
            json!({
                "id": vendor.id,
                "name": vendor.name,
                "description": vendor.description,
                "icon": vendor.icon,
            })
        })
        .collect::<Vec<_>>();
    let group_ratio =
        serde_json::from_str::<JsonValue>(option_str(&options, "GroupRatio", r#"{"default":1}"#))
            .unwrap_or_else(|_| json!({ "default": 1 }));
    api_success_with_extra(json!({
        "data": pricing,
        "vendors": vendors,
        "group_ratio": group_ratio,
        "usable_group": pricing_usable_groups(&options, &query.group),
        "supported_endpoint": serde_json::Map::<String, JsonValue>::new(),
        "auto_groups": pricing_auto_groups(&options, &query.group),
        "pricing_version": "halolake-local-v1",
    }))
}

pub(crate) async fn api_rankings(
    State(state): State<AppState>,
    Query(query): Query<RankingsQuery>,
) -> Response {
    let config = match ranking_period_config(&query.period) {
        Ok(config) => config,
        Err(message) => return api_error_status(StatusCode::BAD_REQUEST, message),
    };
    let events = match state.usage_events.events() {
        Ok(events) => events,
        Err(err) => return usage_error(err),
    };
    let catalog = match state.catalog.current_data() {
        Ok(data) => data,
        Err(err) => return management_error(err),
    };
    api_success(rankings_snapshot(&events, &catalog, config))
}

pub(crate) async fn api_perf_metrics(
    State(state): State<AppState>,
    Query(query): Query<PerfMetricsQuery>,
) -> Response {
    if query.model.is_empty() {
        return api_error_status(StatusCode::BAD_REQUEST, "model is required");
    }
    let events = match state.usage_events.events() {
        Ok(events) => events,
        Err(err) => return usage_error(err),
    };
    api_success(perf_metrics_for_model(&events, &query))
}

pub(crate) async fn api_perf_metrics_summary(
    State(state): State<AppState>,
    Query(query): Query<PerfMetricsQuery>,
) -> Response {
    let events = match state.usage_events.events() {
        Ok(events) => events,
        Err(err) => return usage_error(err),
    };
    api_success(perf_metrics_summary(&events, query.hours))
}

pub(crate) async fn get_options(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.options.call(ListOptionsRequest).await {
        Ok(options) => {
            let mut visible = Vec::with_capacity(options.len() + 1);
            let mut option_values = BTreeMap::new();
            for option in options {
                if is_sensitive_option_key(&option.key) {
                    continue;
                }
                option_values.insert(option.key.clone(), option.value.clone());
                visible.push(option);
            }
            visible.push(crate::storage::OptionRecord {
                key:   "CompletionRatioMeta".to_string(),
                value: build_completion_ratio_meta(&option_values),
            });
            api_success(visible)
        }
        Err(err) => management_error(err),
    }
}

pub(crate) async fn update_option(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<OptionUpdatePayload>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let value = option_value_to_string(payload.value);
    if let Err(resp) = validate_option_update(&state, &payload.key, &value) {
        return resp;
    }
    let key = payload.key;
    let alias_key = passkey_option_alias(&key).map(str::to_string);
    match state
        .options
        .call(UpdateOptionRequest {
            key,
            value: value.clone(),
        })
        .await
    {
        Ok(_) => {
            if let Some(key) = alias_key {
                if let Err(err) = state.options.call(UpdateOptionRequest { key, value }).await {
                    return management_error(err);
                }
            }
            // Options feed channel_affinity / group_routing in the published
            // snapshot. Bump first so the gateway poll does not treat this as
            // NotModified (options do not flow through ManagementData::mutate).
            if let Err(err) = state.management.bump_version().await {
                return management_error(err);
            }
            if let Err(err) = publish_management_snapshot(&state).await {
                return management_error(err);
            }
            api_ok()
        }
        Err(err) => management_error(err),
    }
}

pub(crate) async fn reset_model_ratio(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .options
        .call(UpdateOptionRequest {
            key:   "ModelRatio".to_string(),
            value: DEFAULT_MODEL_RATIO_JSON.to_string(),
        })
        .await
    {
        Ok(_) => api_ok(),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn get_syncable_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = RatioSyncService::new(state.management.clone(), state.options.clone());
    match service.call(ListSyncableChannelsRequest).await {
        Ok(channels) => api_success(channels),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn fetch_upstream_ratios(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<FetchUpstreamRatiosRequest>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = RatioSyncService::new(state.management.clone(), state.options.clone());
    match service.call(payload).await {
        Ok(data) => api_success(data),
        Err(ManagementError::InvalidRequest(message)) => api_error_status(StatusCode::OK, message),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn get_channel_affinity_cache_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelAffinityService::new(state.options.clone());
    match service.call(GetChannelAffinityCacheStatsRequest).await {
        Ok(stats) => api_success(stats),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn clear_channel_affinity_cache(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ChannelAffinityCacheQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ChannelAffinityService::new(state.options.clone());
    match service
        .call(ClearChannelAffinityCacheRequest {
            all:       query.all,
            rule_name: query.rule_name,
        })
        .await
    {
        Ok(ack) => api_success(ack),
        Err(ManagementError::InvalidRequest(message)) => {
            api_error_status(StatusCode::BAD_REQUEST, message)
        }
        Err(err) => management_error(err),
    }
}

pub(crate) async fn confirm_payment_compliance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<PaymentCompliancePayload>,
) -> Response {
    let actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    if !payload.confirmed {
        return api_error_status(StatusCode::OK, "please confirm payment compliance");
    }
    let now = now_unix();
    for (key, value) in [
        ("payment_setting.compliance_confirmed", "true".to_string()),
        (
            "payment_setting.compliance_terms_version",
            payment_compliance_terms_version().to_string(),
        ),
        ("payment_setting.compliance_confirmed_at", now.to_string()),
        (
            "payment_setting.compliance_confirmed_by",
            actor.id.to_string(),
        ),
    ] {
        if let Err(err) = state
            .options
            .call(UpdateOptionRequest {
                key: key.to_string(),
                value,
            })
            .await
        {
            return management_error(err);
        }
    }
    api_success(json!({
        "confirmed": true,
        "terms_version": payment_compliance_terms_version(),
        "confirmed_at": now,
        "confirmed_by": actor.id,
    }))
}

pub(crate) fn pricing_records(
    management: &ManagementData,
    catalog: &CatalogData,
    options: &BTreeMap<String, String>,
    requested_group: &str,
) -> Vec<JsonValue> {
    let model_ratio = parse_number_map(options.get("ModelRatio"));
    let model_price = parse_number_map(options.get("ModelPrice"));
    let completion_ratio = parse_number_map(options.get("CompletionRatio"));
    let cache_ratio = parse_number_map(options.get("CacheRatio"));
    let create_cache_ratio = parse_number_map(options.get("CreateCacheRatio"));
    let image_ratio = parse_number_map(options.get("ImageRatio"));
    let audio_ratio = parse_number_map(options.get("AudioRatio"));
    let audio_completion_ratio = parse_number_map(options.get("AudioCompletionRatio"));

    enabled_pricing_models(management)
        .into_iter()
        .filter_map(|model_name| {
            let meta = catalog_model_for_name(catalog, &model_name);
            if meta.is_some_and(|model| model.status != STATUS_ENABLED) {
                return None;
            }
            let groups = pricing_groups_for_model(management, &model_name);
            if !requested_group.trim().is_empty()
                && !groups.iter().any(|group| group == requested_group)
            {
                return None;
            }
            let model_price_value = model_price.get(&model_name).copied();
            let quota_type = if model_price_value.is_some() { 1 } else { 0 };
            let meta = meta.cloned();
            let mut record = serde_json::Map::new();
            record.insert("model_name".to_string(), json!(model_name));
            record.insert("quota_type".to_string(), json!(quota_type));
            record.insert(
                "model_ratio".to_string(),
                json!(model_ratio.get(&model_name).copied().unwrap_or(1.0)),
            );
            record.insert(
                "model_price".to_string(),
                json!(model_price_value.unwrap_or(0.0)),
            );
            record.insert("owner_by".to_string(), json!("halolake"));
            record.insert(
                "completion_ratio".to_string(),
                json!(completion_ratio.get(&model_name).copied().unwrap_or(1.0)),
            );
            insert_optional_ratio(&mut record, "cache_ratio", cache_ratio.get(&model_name));
            insert_optional_ratio(
                &mut record,
                "create_cache_ratio",
                create_cache_ratio.get(&model_name),
            );
            insert_optional_ratio(&mut record, "image_ratio", image_ratio.get(&model_name));
            insert_optional_ratio(&mut record, "audio_ratio", audio_ratio.get(&model_name));
            insert_optional_ratio(
                &mut record,
                "audio_completion_ratio",
                audio_completion_ratio.get(&model_name),
            );
            record.insert("enable_groups".to_string(), json!(groups));
            record.insert(
                "supported_endpoint_types".to_string(),
                JsonValue::Array(Vec::new()),
            );
            if let Some(meta) = meta {
                if !meta.description.is_empty() {
                    record.insert("description".to_string(), json!(meta.description));
                }
                if !meta.icon.is_empty() {
                    record.insert("icon".to_string(), json!(meta.icon));
                }
                if !meta.tags.is_empty() {
                    record.insert("tags".to_string(), json!(meta.tags));
                }
                if meta.vendor_id != 0 {
                    record.insert("vendor_id".to_string(), json!(meta.vendor_id));
                }
            }
            Some(JsonValue::Object(record))
        })
        .collect()
}

pub(crate) fn pricing_usable_groups(
    options: &BTreeMap<String, String>,
    requested_group: &str,
) -> JsonValue {
    let groups = user_usable_groups_for_options(options, requested_group);
    let mut groups = groups
        .into_iter()
        .map(|(group, description)| (group, JsonValue::String(description)))
        .collect::<serde_json::Map<_, _>>();
    if groups.is_empty() {
        groups.insert("default".to_string(), json!("default"));
    }
    JsonValue::Object(groups)
}

pub(crate) fn pricing_auto_groups(
    options: &BTreeMap<String, String>,
    requested_group: &str,
) -> Vec<String> {
    let usable = user_usable_groups_for_options(options, requested_group);
    parse_string_vec(options.get("AutoGroups"))
        .into_iter()
        .filter(|group| usable.contains_key(group))
        .collect()
}

pub(crate) fn user_usable_groups_for_options(
    options: &BTreeMap<String, String>,
    user_group: &str,
) -> HashMap<String, String> {
    let user_group = user_group.trim();
    let mut groups = parse_string_map(options.get("UserUsableGroups"));
    let special_groups = options
        .get("GroupSpecialUsableGroup")
        .or_else(|| options.get("group_ratio_setting.group_special_usable_group"));
    if !user_group.is_empty()
        && let Some(settings) = parse_nested_string_map(special_groups).remove(user_group)
    {
        for (action, description) in settings {
            if let Some(group) = action.strip_prefix("-:") {
                groups.remove(group.trim());
            } else if let Some(group) = action.strip_prefix("+:") {
                groups.insert(group.trim().to_string(), description);
            } else {
                groups.insert(action.trim().to_string(), description);
            }
        }
    }
    if !user_group.is_empty() && !groups.contains_key(user_group) {
        groups.insert(user_group.to_string(), "用户分组".to_string());
    }
    groups
}

pub(crate) fn enabled_pricing_models(management: &ManagementData) -> Vec<String> {
    let mut models = management
        .channels
        .iter()
        .filter(|channel| channel.status == STATUS_ENABLED)
        .flat_map(ChannelRecord::model_list)
        .chain(
            management
                .model_mappings
                .iter()
                .map(|mapping| mapping.requested_model.clone()),
        )
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    models.sort();
    models
}

pub(crate) fn catalog_model_for_name<'a>(
    catalog: &'a CatalogData,
    model_name: &str,
) -> Option<&'a ModelRecord> {
    catalog
        .models
        .iter()
        .find(|model| model.model_name == model_name)
        .or_else(|| {
            catalog.models.iter().find(|model| match model.name_rule {
                1 => model_name.starts_with(&model.model_name),
                2 => model_name.contains(&model.model_name),
                3 => model_name.ends_with(&model.model_name),
                _ => false,
            })
        })
}

pub(crate) fn pricing_groups_for_model(
    management: &ManagementData,
    model_name: &str,
) -> Vec<String> {
    let mut groups = management
        .channels
        .iter()
        .filter(|channel| channel.status == STATUS_ENABLED)
        .filter(|channel| channel.model_list().iter().any(|model| model == model_name))
        .flat_map(ChannelRecord::group_list)
        .collect::<BTreeSet<_>>();
    if groups.is_empty() {
        groups.insert("default".to_string());
    }
    groups.into_iter().collect()
}

pub(crate) fn insert_optional_ratio(
    record: &mut serde_json::Map<String, JsonValue>,
    key: &str,
    value: Option<&f64>,
) {
    if let Some(value) = value {
        record.insert(key.to_string(), json!(value));
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RankingPeriodConfig {
    seconds:     i64,
    bucket_size: i64,
}

pub(crate) fn ranking_period_config(period: &str) -> Result<RankingPeriodConfig, &'static str> {
    match period {
        "" | "week" => Ok(RankingPeriodConfig {
            seconds:     7 * 24 * 3600,
            bucket_size: 24 * 3600,
        }),
        "today" => Ok(RankingPeriodConfig {
            seconds:     24 * 3600,
            bucket_size: 3600,
        }),
        "month" => Ok(RankingPeriodConfig {
            seconds:     30 * 24 * 3600,
            bucket_size: 24 * 3600,
        }),
        "year" => Ok(RankingPeriodConfig {
            seconds:     365 * 24 * 3600,
            bucket_size: 7 * 24 * 3600,
        }),
        _ => Err("invalid ranking period"),
    }
}

pub(crate) fn rankings_snapshot(
    events: &[UsageEvent],
    catalog: &CatalogData,
    config: RankingPeriodConfig,
) -> JsonValue {
    let now = now_unix();
    let start = now.saturating_sub(config.seconds);
    let previous_start = start.saturating_sub(config.seconds);
    let current_totals = usage_totals(events, start, now);
    let previous_totals = usage_totals(events, previous_start, start.saturating_sub(1));
    let total_tokens = current_totals
        .iter()
        .map(|(_, tokens)| *tokens)
        .sum::<u64>();
    let previous_rank = rank_map(&previous_totals);
    let previous_tokens = token_map(&previous_totals);
    let mut models = current_totals
        .iter()
        .enumerate()
        .map(|(idx, (model, tokens))| {
            let vendor = model_vendor(catalog, model);
            let rank = idx + 1;
            let prev_tokens = previous_tokens.get(model).copied().unwrap_or(0);
            json!({
                "rank": rank,
                "previous_rank": previous_rank.get(model).copied(),
                "model_name": model,
                "vendor": vendor.0,
                "vendor_icon": vendor.1,
                "category": "",
                "total_tokens": tokens,
                "share": percent(*tokens, total_tokens),
                "growth_pct": growth_pct(*tokens, prev_tokens),
            })
        })
        .collect::<Vec<_>>();
    models.truncate(20);

    let vendors = ranking_vendors(&current_totals, &previous_totals, catalog, total_tokens);
    let model_history = ranking_model_history(events, catalog, start, now, config.bucket_size);
    let vendor_share_history =
        ranking_vendor_history(events, catalog, start, now, config.bucket_size);

    json!({
        "models": models,
        "vendors": vendors,
        "top_movers": Vec::<JsonValue>::new(),
        "top_droppers": Vec::<JsonValue>::new(),
        "models_history": model_history,
        "vendor_share_history": vendor_share_history,
    })
}

pub(crate) fn usage_totals(events: &[UsageEvent], start: i64, end: i64) -> Vec<(String, u64)> {
    let mut totals = BTreeMap::<String, u64>::new();
    for event in events {
        let ts = event.created_at_unix_ms / 1000;
        if event.status != UsageStatus::Success || ts < start || ts > end || event.model.is_empty()
        {
            continue;
        }
        let tokens = event.observed_tokens();
        if tokens > 0 {
            *totals.entry(event.model.clone()).or_default() += tokens;
        }
    }
    let mut totals = totals.into_iter().collect::<Vec<_>>();
    totals.sort_by_key(|(_, tokens)| std::cmp::Reverse(*tokens));
    totals
}

pub(crate) fn rank_map(totals: &[(String, u64)]) -> BTreeMap<String, usize> {
    totals
        .iter()
        .enumerate()
        .map(|(idx, (model, _))| (model.clone(), idx + 1))
        .collect()
}

pub(crate) fn token_map(totals: &[(String, u64)]) -> BTreeMap<String, u64> {
    totals.iter().cloned().collect()
}

pub(crate) fn ranking_vendors(
    current_totals: &[(String, u64)],
    previous_totals: &[(String, u64)],
    catalog: &CatalogData,
    total_tokens: u64,
) -> Vec<JsonValue> {
    let previous = previous_totals
        .iter()
        .map(|(model, tokens)| (model_vendor(catalog, model).0, *tokens))
        .fold(
            BTreeMap::<String, u64>::new(),
            |mut acc, (vendor, tokens)| {
                *acc.entry(vendor).or_default() += tokens;
                acc
            },
        );
    let mut vendors = BTreeMap::<String, (String, u64, BTreeSet<String>, String, u64)>::new();
    for (model, tokens) in current_totals {
        let (vendor, icon) = model_vendor(catalog, model);
        let entry = vendors
            .entry(vendor.clone())
            .or_insert_with(|| (icon, 0, BTreeSet::new(), String::new(), 0));
        entry.1 += *tokens;
        entry.2.insert(model.clone());
        if *tokens > entry.4 {
            entry.3.clone_from(model);
            entry.4 = *tokens;
        }
    }
    let mut vendors = vendors
        .into_iter()
        .map(|(vendor, (icon, tokens, models, top_model, _))| {
            let previous_tokens = previous.get(&vendor).copied().unwrap_or(0);
            (
                vendor,
                icon,
                tokens,
                models.len(),
                top_model,
                previous_tokens,
            )
        })
        .collect::<Vec<_>>();
    vendors.sort_by_key(|(_, _, tokens, _, _, _)| std::cmp::Reverse(*tokens));
    vendors
        .into_iter()
        .take(5)
        .enumerate()
        .map(
            |(idx, (vendor, icon, tokens, models_count, top_model, previous_tokens))| {
                json!({
                    "rank": idx + 1,
                    "vendor": vendor,
                    "vendor_icon": icon,
                    "total_tokens": tokens,
                    "share": percent(tokens, total_tokens),
                    "growth_pct": growth_pct(tokens, previous_tokens),
                    "models_count": models_count,
                    "top_model": top_model,
                })
            },
        )
        .collect()
}

pub(crate) fn ranking_model_history(
    events: &[UsageEvent],
    catalog: &CatalogData,
    start: i64,
    end: i64,
    bucket_size: i64,
) -> JsonValue {
    let buckets = usage_buckets(events, start, end, bucket_size);
    let mut totals = BTreeMap::<String, u64>::new();
    for ((model, _), tokens) in &buckets {
        *totals.entry(model.clone()).or_default() += *tokens;
    }
    let mut models = totals.into_iter().collect::<Vec<_>>();
    models.sort_by_key(|(_, tokens)| std::cmp::Reverse(*tokens));
    models.truncate(10);
    let selected = models
        .iter()
        .map(|(model, _)| model.clone())
        .collect::<BTreeSet<_>>();
    let points = buckets
        .into_iter()
        .filter(|((model, _), _)| selected.contains(model))
        .map(|((model, bucket), tokens)| {
            let vendor = model_vendor(catalog, &model).0;
            json!({
                "ts": bucket.to_string(),
                "label": bucket.to_string(),
                "model": model,
                "vendor": vendor,
                "tokens": tokens,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "points": points,
        "models": models.into_iter().map(|(model, total)| {
            let vendor = model_vendor(catalog, &model).0;
            json!({ "name": model, "vendor": vendor, "total": total })
        }).collect::<Vec<_>>(),
        "buckets": bucket_count(start, end, bucket_size),
    })
}

pub(crate) fn ranking_vendor_history(
    events: &[UsageEvent],
    catalog: &CatalogData,
    start: i64,
    end: i64,
    bucket_size: i64,
) -> JsonValue {
    let model_buckets = usage_buckets(events, start, end, bucket_size);
    let mut vendor_buckets = BTreeMap::<(String, i64), u64>::new();
    let mut vendor_totals = BTreeMap::<String, u64>::new();
    let mut total_tokens = 0u64;
    for ((model, bucket), tokens) in model_buckets {
        let vendor = model_vendor(catalog, &model).0;
        *vendor_buckets.entry((vendor.clone(), bucket)).or_default() += tokens;
        *vendor_totals.entry(vendor).or_default() += tokens;
        total_tokens += tokens;
    }
    let mut vendors = vendor_totals.into_iter().collect::<Vec<_>>();
    vendors.sort_by_key(|(_, tokens)| std::cmp::Reverse(*tokens));
    vendors.truncate(5);
    let selected = vendors
        .iter()
        .map(|(vendor, _)| vendor.clone())
        .collect::<BTreeSet<_>>();
    let points = vendor_buckets
        .into_iter()
        .filter(|((vendor, _), _)| selected.contains(vendor))
        .map(|((vendor, bucket), tokens)| {
            json!({
                "ts": bucket.to_string(),
                "label": bucket.to_string(),
                "vendor": vendor,
                "share": percent(tokens, total_tokens),
                "tokens": tokens,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "points": points,
        "vendors": vendors.into_iter().map(|(vendor, total)| {
            json!({ "name": vendor, "total": total, "share": percent(total, total_tokens) })
        }).collect::<Vec<_>>(),
        "buckets": bucket_count(start, end, bucket_size),
    })
}

pub(crate) fn usage_buckets(
    events: &[UsageEvent],
    start: i64,
    end: i64,
    bucket_size: i64,
) -> BTreeMap<(String, i64), u64> {
    let mut buckets = BTreeMap::new();
    for event in events {
        let ts = event.created_at_unix_ms / 1000;
        if event.status != UsageStatus::Success || ts < start || ts > end || event.model.is_empty()
        {
            continue;
        }
        let bucket = ts - ts.rem_euclid(bucket_size.max(1));
        *buckets.entry((event.model.clone(), bucket)).or_default() += event.observed_tokens();
    }
    buckets
}

pub(crate) fn model_vendor(catalog: &CatalogData, model_name: &str) -> (String, String) {
    let Some(model) = catalog_model_for_name(catalog, model_name) else {
        return ("Unknown".to_string(), String::new());
    };
    let Some(vendor) = catalog
        .vendors
        .iter()
        .find(|vendor| vendor.id == model.vendor_id)
    else {
        return ("Unknown".to_string(), String::new());
    };
    (vendor.name.clone(), vendor.icon.clone())
}

pub(crate) fn perf_metrics_for_model(events: &[UsageEvent], query: &PerfMetricsQuery) -> JsonValue {
    let hours = clamp_perf_hours(query.hours);
    let now = now_unix();
    let start = now.saturating_sub(hours * 3600);
    let mut groups = BTreeMap::<String, Vec<&UsageEvent>>::new();
    for event in events {
        let ts = event.created_at_unix_ms / 1000;
        if event.model != query.model || ts < start || ts > now {
            continue;
        }
        let group = "default";
        if !query.group.is_empty() && query.group != group {
            continue;
        }
        groups.entry(group.to_string()).or_default().push(event);
    }
    let groups = groups
        .into_iter()
        .map(|(group, events)| perf_group_result(group, events, start, now))
        .collect::<Vec<_>>();
    json!({
        "model_name": query.model,
        "series_schema": "halolake-usage-v1",
        "groups": groups,
    })
}

pub(crate) fn perf_metrics_summary(events: &[UsageEvent], hours: i64) -> JsonValue {
    let hours = clamp_perf_hours(hours);
    let now = now_unix();
    let start = now.saturating_sub(hours * 3600);
    let mut models = BTreeMap::<String, Vec<&UsageEvent>>::new();
    for event in events {
        let ts = event.created_at_unix_ms / 1000;
        if ts >= start && ts <= now {
            models.entry(event.model.clone()).or_default().push(event);
        }
    }
    let mut models = models
        .into_iter()
        .filter(|(model, events)| !model.is_empty() && !events.is_empty())
        .map(|(model, events)| {
            json!({
                "model_name": model,
                "avg_latency_ms": avg_latency(&events),
                "success_rate": success_rate(&events),
                "avg_tps": avg_tps(&events),
            })
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| {
        right["avg_tps"]
            .as_f64()
            .partial_cmp(&left["avg_tps"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    json!({ "models": models })
}

pub(crate) fn perf_group_result(
    group: String,
    events: Vec<&UsageEvent>,
    start: i64,
    end: i64,
) -> JsonValue {
    let mut buckets = BTreeMap::<i64, Vec<&UsageEvent>>::new();
    for event in &events {
        let ts = event.created_at_unix_ms / 1000;
        let bucket = ts - ts.rem_euclid(3600);
        buckets.entry(bucket).or_default().push(*event);
    }
    let series = buckets
        .into_iter()
        .filter(|(bucket, _)| *bucket >= start && *bucket <= end)
        .map(|(bucket, events)| {
            json!({
                "ts": bucket,
                "avg_ttft_ms": 0,
                "avg_latency_ms": avg_latency(&events),
                "success_rate": success_rate(&events),
                "avg_tps": avg_tps(&events),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "group": group,
        "avg_ttft_ms": 0,
        "avg_latency_ms": avg_latency(&events),
        "success_rate": success_rate(&events),
        "avg_tps": avg_tps(&events),
        "series": series,
    })
}

pub(crate) fn avg_latency(events: &[&UsageEvent]) -> u64 {
    if events.is_empty() {
        return 0;
    }
    events.iter().map(|event| event.latency_ms).sum::<u64>() / events.len() as u64
}

pub(crate) fn success_rate(events: &[&UsageEvent]) -> f64 {
    if events.is_empty() {
        return 0.0;
    }
    let successes = events
        .iter()
        .filter(|event| event.status == UsageStatus::Success)
        .count();
    successes as f64 / events.len() as f64 * 100.0
}

pub(crate) fn avg_tps(events: &[&UsageEvent]) -> f64 {
    let tokens = events
        .iter()
        .map(|event| event.completion_tokens.unwrap_or(0))
        .sum::<u64>();
    let latency_ms = events.iter().map(|event| event.latency_ms).sum::<u64>();
    if latency_ms == 0 {
        return 0.0;
    }
    tokens as f64 / latency_ms as f64 * 1000.0
}

pub(crate) fn clamp_perf_hours(hours: i64) -> i64 {
    if hours <= 0 { 24 } else { hours.min(24 * 30) }
}

pub(crate) fn percent(value: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        value as f64 / total as f64 * 100.0
    }
}

pub(crate) fn growth_pct(current: u64, previous: u64) -> f64 {
    if previous == 0 {
        if current == 0 { 0.0 } else { 100.0 }
    } else {
        (current as f64 - previous as f64) / previous as f64 * 100.0
    }
}

pub(crate) fn bucket_count(start: i64, end: i64, bucket_size: i64) -> i64 {
    if bucket_size <= 0 || end <= start {
        return 0;
    }
    ((end - start) / bucket_size).saturating_add(1)
}

pub(crate) fn default_options(config: &ControlApiConfig) -> BTreeMap<String, String> {
    let mut options = BTreeMap::new();
    macro_rules! option {
        ($key:literal, $value:expr) => {
            options.insert($key.to_string(), $value.to_string());
        };
    }

    option!("FileUploadPermission", "1");
    option!("FileDownloadPermission", "1");
    option!("ImageUploadPermission", "1");
    option!("ImageDownloadPermission", "1");
    option!("PasswordLoginEnabled", "true");
    option!("PasswordRegisterEnabled", "false");
    option!("EmailVerificationEnabled", "false");
    option!("RegisterEnabled", "false");
    option!("GenerateDefaultToken", "false");
    option!("CheckinEnabled", "false");
    option!("CheckinMinQuota", "1000");
    option!("CheckinMaxQuota", "10000");
    option!("AutomaticDisableChannelEnabled", "false");
    option!("AutomaticDisableStatusCodes", "401");
    option!("AutomaticEnableChannelEnabled", "false");
    option!("LogConsumeEnabled", "true");
    option!("DisplayInCurrencyEnabled", "false");
    option!("DisplayTokenStatEnabled", "true");
    option!("DrawingEnabled", "true");
    option!("TaskEnabled", "true");
    option!("DataExportEnabled", "false");
    option!("DefaultCollapseSidebar", "false");
    option!("DefaultUseAutoGroup", "false");
    option!("BatchUpdateEnabled", "true");
    option!("GitHubOAuthEnabled", "false");
    option!("LinuxDOOAuthEnabled", "false");
    option!("TelegramOAuthEnabled", "false");
    option!("WeChatAuthEnabled", "false");
    option!("TurnstileCheckEnabled", "false");
    option!("PasskeyLoginEnabled", "false");
    option!("passkey.enabled", "false");
    option!("passkey.rp_display_name", "");
    option!("passkey.rp_id", "");
    option!("passkey.origins", "");
    option!("passkey.allow_insecure_origin", "false");
    option!("passkey.user_verification", "preferred");
    option!("passkey.attachment_preference", "");
    option!("discord.enabled", "false");
    option!("oidc.enabled", "false");
    option!("theme.frontend", "default");
    option!("Notice", "");
    option!("About", "");
    option!("HomePageContent", "");
    option!("Footer", "");
    option!("SystemName", &config.system.name);
    option!("Logo", "");
    option!("ServerAddress", "");
    option!("WorkerUrl", "");
    option!("WorkerValidKey", "");
    option!("SelfUseModeEnabled", "false");
    option!("DemoSiteEnabled", "false");
    option!("PayAddress", "");
    option!("CustomCallbackAddress", "");
    option!("TopUpLink", "");
    option!("MinTopUp", "1");
    option!("StripeMinTopUp", "1");
    option!("WaffoMinTopUp", "1");
    option!("WaffoPancakeMinTopUp", "1");
    option!("PayMethods", r#"[]"#);
    option!("WaffoPayMethods", "null");
    option!("CreemProducts", "[]");
    option!("AmountOptions", "[10,20,50,100,200,500]");
    option!("AmountDiscount", "{}");
    option!("payment_setting.compliance_confirmed", "false");
    option!("payment_setting.compliance_terms_version", "");
    option!("payment_setting.compliance_confirmed_at", "0");
    option!("payment_setting.compliance_confirmed_by", "0");
    option!("CustomCurrencySymbol", "$");
    option!("CustomCurrencyExchangeRate", "1");
    option!("Price", "7.3");
    option!("QuotaDisplayType", "quota");
    option!("QuotaForNewUser", "0");
    option!("QuotaForInviter", "0");
    option!("QuotaForInvitee", "0");
    option!("QuotaRemindThreshold", "1000");
    option!("PreConsumedQuota", "500");
    option!("QuotaPerUnit", "500000");
    option!("RetryTimes", "0");
    option!("TopupGroupRatio", r#"{"default":1}"#);
    option!("AutoGroups", "[]");
    option!("PayMethods", "[]");
    option!("ModelRequestRateLimitCount", "0");
    option!("ModelRequestRateLimitDurationMinutes", "0");
    option!("ModelRequestRateLimitSuccessCount", "0");
    option!("ModelRequestRateLimitGroup", "{}");
    option!("monitor_setting.auto_test_channel_enabled", "false");
    option!("monitor_setting.auto_test_channel_minutes", "10");
    option!("ModelRatio", "{}");
    option!("ModelPrice", "{}");
    option!("CacheRatio", "{}");
    option!("CreateCacheRatio", "{}");
    option!("GroupRatio", r#"{"default":1,"vip":1,"svip":1}"#);
    option!("GroupGroupRatio", "{}");
    option!(
        "UserUsableGroups",
        r#"{"default":"默认分组","vip":"vip分组"}"#
    );
    option!("GroupSpecialUsableGroup", "{}");
    option!("CompletionRatio", "{}");
    option!("ImageRatio", "{}");
    option!("AudioRatio", "{}");
    option!("AudioCompletionRatio", "{}");
    option!("channel_affinity_setting.enabled", "true");
    option!("channel_affinity_setting.switch_on_success", "true");
    option!("channel_affinity_setting.keep_on_channel_disabled", "false");
    option!("channel_affinity_setting.max_entries", "100000");
    option!("channel_affinity_setting.default_ttl_seconds", "3600");
    option!(
        "channel_affinity_setting.rules",
        DEFAULT_CHANNEL_AFFINITY_RULES_JSON
    );
    option!("ExposeRatioEnabled", "false");
    option!("AutomaticDisableKeywords", "");
    option!("AutomaticDisableStatusCodes", "");
    option!("AutomaticRetryStatusCodes", "");
    option!("SensitiveWords", "");
    option!("CheckSensitiveEnabled", "false");
    option!("CheckSensitiveOnPromptEnabled", "false");
    option!("StopOnSensitiveEnabled", "false");

    for (key, value) in &config.options {
        options.insert(key.clone(), toml_value_to_option_string(value));
    }
    options
}

pub(crate) fn toml_value_to_option_string(value: &toml::Value) -> String {
    match value {
        toml::Value::String(value) => value.clone(),
        toml::Value::Integer(value) => value.to_string(),
        toml::Value::Float(value) => value.to_string(),
        toml::Value::Boolean(value) => value.to_string(),
        toml::Value::Datetime(value) => value.to_string(),
        toml::Value::Array(_) | toml::Value::Table(_) => {
            serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
        }
    }
}

pub(crate) fn option_value_to_string(value: JsonValue) -> String {
    match value {
        JsonValue::Null => String::new(),
        JsonValue::Bool(value) => value.to_string(),
        JsonValue::Number(value) => value.to_string(),
        JsonValue::String(value) => value,
        value @ (JsonValue::Array(_) | JsonValue::Object(_)) => {
            serde_json::to_string(&value).unwrap_or_default()
        }
    }
}

pub(crate) fn validate_option_update(
    state: &AppState,
    key: &str,
    value: &str,
) -> Result<(), Response> {
    if key.starts_with("payment_setting.compliance_") {
        return Err(api_error_status(
            StatusCode::OK,
            "合规确认字段不允许通过通用设置接口修改",
        ));
    }
    if key == "theme.frontend" && value != "default" && value != "classic" {
        return Err(api_error_status(
            StatusCode::OK,
            "无效的主题值，可选值：default（新版前端）、classic（经典前端）",
        ));
    }
    if json_object_option_key(key) {
        validate_json_object_option(key, value)?;
    }

    let options = state.options.values().unwrap_or_default();
    match key {
        "GitHubOAuthEnabled"
            if value == "true" && option_str(&options, "GitHubClientId", "").is_empty() =>
        {
            Err(api_error_status(
                StatusCode::OK,
                "无法启用 GitHub OAuth，请先填入 GitHub Client Id 以及 GitHub Client Secret！",
            ))
        }
        "discord.enabled"
            if value == "true" && option_str(&options, "discord.client_id", "").is_empty() =>
        {
            Err(api_error_status(
                StatusCode::OK,
                "无法启用 Discord OAuth，请先填入 Discord Client Id 以及 Discord Client Secret！",
            ))
        }
        "oidc.enabled"
            if value == "true" && option_str(&options, "oidc.client_id", "").is_empty() =>
        {
            Err(api_error_status(
                StatusCode::OK,
                "无法启用 OIDC 登录，请先填入 OIDC 登录相关配置！",
            ))
        }
        "LinuxDOOAuthEnabled"
            if value == "true" && option_str(&options, "LinuxDOClientId", "").is_empty() =>
        {
            Err(api_error_status(
                StatusCode::OK,
                "无法启用 LinuxDO OAuth，请先填入 LinuxDO OAuth 相关配置！",
            ))
        }
        "TelegramOAuthEnabled"
            if value == "true" && option_str(&options, "TelegramBotToken", "").is_empty() =>
        {
            Err(api_error_status(
                StatusCode::OK,
                "无法启用 Telegram OAuth，请先填入 Telegram Bot Token！",
            ))
        }
        "WeChatAuthEnabled"
            if value == "true" && option_str(&options, "WeChatServerAddress", "").is_empty() =>
        {
            Err(api_error_status(
                StatusCode::OK,
                "无法启用微信登录，请先填入微信登录相关配置信息！",
            ))
        }
        "TurnstileCheckEnabled"
            if value == "true" && option_str(&options, "TurnstileSiteKey", "").is_empty() =>
        {
            Err(api_error_status(
                StatusCode::OK,
                "无法启用 Turnstile 校验，请先填入 Turnstile 校验相关配置信息！",
            ))
        }
        _ => Ok(()),
    }
}

pub(crate) fn json_object_option_key(key: &str) -> bool {
    matches!(
        key,
        "TopupGroupRatio"
            | "ModelRequestRateLimitGroup"
            | "ModelRatio"
            | "ModelPrice"
            | "CacheRatio"
            | "CreateCacheRatio"
            | "GroupRatio"
            | "GroupGroupRatio"
            | "UserUsableGroups"
            | "GroupSpecialUsableGroup"
            | "CompletionRatio"
            | "ImageRatio"
            | "AudioRatio"
            | "AudioCompletionRatio"
    )
}

pub(crate) fn validate_json_object_option(key: &str, value: &str) -> Result<(), Response> {
    let parsed = serde_json::from_str::<JsonValue>(value).map_err(|err| {
        api_error_status(StatusCode::OK, &format!("{key} JSON 配置解析失败: {err}"))
    })?;
    let Some(object) = parsed.as_object() else {
        return Err(api_error_status(
            StatusCode::OK,
            &format!("{key} 必须是 JSON object"),
        ));
    };
    if key == "GroupRatio" {
        for (group, value) in object {
            if value.as_f64().is_none_or(|ratio| ratio <= 0.0) {
                return Err(api_error_status(
                    StatusCode::OK,
                    &format!("分组 {group} 的倍率必须大于 0"),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn is_sensitive_option_key(key: &str) -> bool {
    key.ends_with("Token")
        || key.ends_with("Secret")
        || key.ends_with("Key")
        || key.ends_with("secret")
        || key.ends_with("api_key")
}

pub(crate) fn build_completion_ratio_meta(options: &BTreeMap<String, String>) -> String {
    let mut model_names = BTreeSet::new();
    for key in [
        "ModelPrice",
        "ModelRatio",
        "CompletionRatio",
        "CacheRatio",
        "CreateCacheRatio",
        "ImageRatio",
        "AudioRatio",
        "AudioCompletionRatio",
    ] {
        collect_json_object_keys(options.get(key), &mut model_names);
    }
    let completion_ratios = parse_number_map(options.get("CompletionRatio"));
    let mut meta = serde_json::Map::with_capacity(model_names.len());
    for model in model_names {
        let ratio = completion_ratios.get(&model).copied().unwrap_or(1.0);
        meta.insert(model, json!({ "ratio": ratio, "locked": false }));
    }
    JsonValue::Object(meta).to_string()
}

pub(crate) fn collect_json_object_keys(value: Option<&String>, out: &mut BTreeSet<String>) {
    let Some(value) = value else {
        return;
    };
    let Ok(JsonValue::Object(object)) = serde_json::from_str::<JsonValue>(value) else {
        return;
    };
    out.extend(
        object
            .into_iter()
            .map(|(key, _)| key)
            .filter(|key| !key.trim().is_empty()),
    );
}

pub(crate) fn parse_number_map(value: Option<&String>) -> BTreeMap<String, f64> {
    let Some(value) = value else {
        return BTreeMap::new();
    };
    let Ok(JsonValue::Object(object)) = serde_json::from_str::<JsonValue>(value) else {
        return BTreeMap::new();
    };
    object
        .into_iter()
        .filter_map(|(key, value)| value.as_f64().map(|value| (key, value)))
        .collect()
}

pub(crate) fn parse_nested_number_map(
    value: Option<&String>,
) -> BTreeMap<String, BTreeMap<String, f64>> {
    let Some(value) = value else {
        return BTreeMap::new();
    };
    let Ok(JsonValue::Object(object)) = serde_json::from_str::<JsonValue>(value) else {
        return BTreeMap::new();
    };
    object
        .into_iter()
        .filter_map(|(outer_key, value)| {
            let JsonValue::Object(inner) = value else {
                return None;
            };
            let inner = inner
                .into_iter()
                .filter_map(|(inner_key, value)| value.as_f64().map(|value| (inner_key, value)))
                .collect::<BTreeMap<_, _>>();
            (!inner.is_empty()).then_some((outer_key, inner))
        })
        .collect()
}

pub(crate) fn parse_string_vec(value: Option<&String>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(value).unwrap_or_default()
}

pub(crate) fn parse_string_map(value: Option<&String>) -> HashMap<String, String> {
    let Some(value) = value else {
        return HashMap::new();
    };
    let Ok(JsonValue::Object(object)) = serde_json::from_str::<JsonValue>(value) else {
        return HashMap::new();
    };
    object
        .into_iter()
        .filter_map(|(key, value)| {
            let value = match value {
                JsonValue::String(value) => value,
                JsonValue::Null => String::new(),
                other => other.to_string(),
            };
            (!key.trim().is_empty()).then_some((key, value))
        })
        .collect()
}

pub(crate) fn parse_nested_string_map(
    value: Option<&String>,
) -> HashMap<String, HashMap<String, String>> {
    let Some(value) = value else {
        return HashMap::new();
    };
    let Ok(JsonValue::Object(object)) = serde_json::from_str::<JsonValue>(value) else {
        return HashMap::new();
    };
    object
        .into_iter()
        .filter_map(|(outer_key, value)| {
            let JsonValue::Object(inner) = value else {
                return None;
            };
            let inner = inner
                .into_iter()
                .filter_map(|(inner_key, value)| {
                    let value = match value {
                        JsonValue::String(value) => value,
                        JsonValue::Null => String::new(),
                        other => other.to_string(),
                    };
                    (!inner_key.trim().is_empty()).then_some((inner_key, value))
                })
                .collect::<HashMap<_, _>>();
            (!outer_key.trim().is_empty() && !inner.is_empty()).then_some((outer_key, inner))
        })
        .collect()
}

pub(crate) fn usage_pricing_from_options(options: &BTreeMap<String, String>) -> UsagePricing {
    UsagePricing {
        quota_per_unit:       option_f64(options, "QuotaPerUnit", 500_000.0),
        model_ratio:          parse_number_map(options.get("ModelRatio")),
        model_price:          parse_number_map(options.get("ModelPrice")),
        completion_ratio:     parse_number_map(options.get("CompletionRatio")),
        cache_ratio:          parse_number_map(options.get("CacheRatio")),
        cache_creation_ratio: parse_number_map(options.get("CreateCacheRatio")),
        image_ratio:          parse_number_map(options.get("ImageRatio")),
        audio_ratio:          parse_number_map(options.get("AudioRatio")),
        group_ratio:          parse_number_map(options.get("GroupRatio")),
        group_group_ratio:    parse_nested_number_map(options.get("GroupGroupRatio")),
    }
}

pub(crate) fn group_routing_config_from_options(
    options: &BTreeMap<String, String>,
) -> GroupRoutingConfig {
    let group_ratio = parse_number_map(options.get("GroupRatio"));
    let group_special_usable_groups = options
        .get("GroupSpecialUsableGroup")
        .or_else(|| options.get("group_ratio_setting.group_special_usable_group"));
    GroupRoutingConfig {
        auto_groups:                 parse_string_vec(options.get("AutoGroups")),
        user_usable_groups:          parse_string_map(options.get("UserUsableGroups")),
        group_special_usable_groups: parse_nested_string_map(group_special_usable_groups),
        known_groups:                group_ratio.into_keys().collect(),
    }
}

pub(crate) fn checkin_setting(options: &BTreeMap<String, String>) -> CheckinSetting {
    let min_quota = option_i64(
        options,
        "CheckinMinQuota",
        option_i64(options, "checkin_setting.min_quota", 1000),
    )
    .max(0);
    let max_quota = option_i64(
        options,
        "CheckinMaxQuota",
        option_i64(options, "checkin_setting.max_quota", 10000),
    )
    .max(min_quota);
    CheckinSetting {
        enabled: option_bool(
            options,
            "CheckinEnabled",
            option_bool(options, "checkin_setting.enabled", false),
        ),
        min_quota,
        max_quota,
    }
}

pub(crate) fn option_str<'a>(
    options: &'a BTreeMap<String, String>,
    key: &str,
    default: &'a str,
) -> &'a str {
    options.get(key).map(String::as_str).unwrap_or(default)
}

pub(crate) fn option_bool(options: &BTreeMap<String, String>, key: &str, default: bool) -> bool {
    options
        .get(key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

pub(crate) fn passkey_option_alias(key: &str) -> Option<&'static str> {
    match key {
        "passkey.enabled" => Some("PasskeyLoginEnabled"),
        "PasskeyLoginEnabled" => Some("passkey.enabled"),
        _ => None,
    }
}

pub(crate) fn generate_default_token_enabled(options: &BTreeMap<String, String>) -> bool {
    let env_default = std::env::var("GENERATE_DEFAULT_TOKEN")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(false);
    option_bool(options, "GenerateDefaultToken", env_default)
}

pub(crate) fn option_f64(options: &BTreeMap<String, String>, key: &str, default: f64) -> f64 {
    options
        .get(key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

pub(crate) fn option_i64(options: &BTreeMap<String, String>, key: &str, default: i64) -> i64 {
    options
        .get(key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

pub(crate) fn channel_affinity_config_from_options(
    options: &BTreeMap<String, String>,
) -> ChannelAffinityConfig {
    ChannelAffinityConfig {
        enabled:                  option_bool(options, "channel_affinity_setting.enabled", true),
        switch_on_success:        option_bool(
            options,
            "channel_affinity_setting.switch_on_success",
            true,
        ),
        keep_on_channel_disabled: option_bool(
            options,
            "channel_affinity_setting.keep_on_channel_disabled",
            false,
        ),
        max_entries:              options
            .get("channel_affinity_setting.max_entries")
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(100_000),
        default_ttl_seconds:      options
            .get("channel_affinity_setting.default_ttl_seconds")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(3_600),
        rules:                    options
            .get("channel_affinity_setting.rules")
            .and_then(|value| serde_json::from_str::<Vec<ChannelAffinityRule>>(value).ok())
            .unwrap_or_default(),
    }
}

pub(crate) fn option_json(
    options: &BTreeMap<String, String>,
    key: &str,
    default: JsonValue,
) -> JsonValue {
    options
        .get(key)
        .and_then(|value| serde_json::from_str::<JsonValue>(value).ok())
        .unwrap_or(default)
}

pub(crate) fn payment_compliance_terms_version() -> &'static str {
    "v1"
}

pub(crate) fn payment_compliance_confirmed(options: &BTreeMap<String, String>) -> bool {
    option_bool(options, "payment_setting.compliance_confirmed", false)
        && option_str(options, "payment_setting.compliance_terms_version", "")
            == payment_compliance_terms_version()
}

pub(crate) fn require_payment_compliance(state: &AppState) -> Result<(), Response> {
    match state.options.values() {
        Ok(options) if payment_compliance_confirmed(&options) => Ok(()),
        Ok(_) => Err(api_error_status(
            StatusCode::OK,
            "payment.compliance_required",
        )),
        Err(err) => Err(management_error(err)),
    }
}

pub(crate) fn topup_info_payload(options: &BTreeMap<String, String>) -> JsonValue {
    let compliance_confirmed = payment_compliance_confirmed(options);
    let pay_methods = if compliance_confirmed {
        option_json(options, "PayMethods", json!([]))
    } else {
        json!([])
    };
    json!({
        "enable_online_topup": compliance_confirmed && !option_str(options, "PayAddress", "").is_empty(),
        "enable_stripe_topup": compliance_confirmed && option_bool(options, "StripeTopupEnabled", false),
        "enable_creem_topup": compliance_confirmed && option_bool(options, "CreemTopupEnabled", false),
        "enable_waffo_topup": compliance_confirmed && option_bool(options, "WaffoTopupEnabled", false),
        "enable_waffo_pancake_topup": compliance_confirmed && option_bool(options, "WaffoPancakeTopupEnabled", false),
        "enable_redemption": compliance_confirmed,
        "payment_compliance_confirmed": compliance_confirmed,
        "payment_compliance_terms_version": payment_compliance_terms_version(),
        "waffo_pay_methods": if compliance_confirmed && option_bool(options, "WaffoTopupEnabled", false) {
            option_json(options, "WaffoPayMethods", JsonValue::Null)
        } else {
            JsonValue::Null
        },
        "creem_products": option_str(options, "CreemProducts", "[]"),
        "pay_methods": pay_methods,
        "min_topup": option_i64(options, "MinTopUp", 1),
        "stripe_min_topup": option_i64(options, "StripeMinTopUp", 1),
        "waffo_min_topup": option_i64(options, "WaffoMinTopUp", 1),
        "waffo_pancake_min_topup": option_i64(options, "WaffoPancakeMinTopUp", 1),
        "amount_options": option_json(options, "AmountOptions", json!([10, 20, 50, 100, 200, 500])),
        "discount": option_json(options, "AmountDiscount", json!({})),
        "topup_link": option_str(options, "TopUpLink", ""),
    })
}
