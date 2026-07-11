//! Catalog, redemption, top-up, and group HTTP handlers.

use crate::{
    AppState, ModelUpdateQuery, PageQuery,
    api_user::TokenSearchQuery,
    billing::{
        CompleteTopUpRequest, CreateRedemptionsRequest, DeleteInvalidRedemptionsRequest,
        DeleteRedemptionRequest, GetRedemptionRequest, ListRedemptionsRequest, ListTopUpsRequest,
        RedeemRedemptionRequest, RedemptionRecord, RollbackRedeemRedemptionRequest,
        SearchRedemptionsRequest, UpdateRedemptionRequest,
    },
    catalog::{
        CreateModelRequest, CreateVendorRequest, DeleteModelRequest, DeleteVendorRequest,
        GetModelRequest, GetVendorRequest, ListModelsRequest, ListVendorsRequest, ModelRecord,
        SearchModelsRequest, SearchVendorsRequest, UpdateModelRequest, UpdateVendorRequest,
        VendorModelCountsRequest, VendorRecord, enrich_models, missing_models,
    },
    http_auth::{current_user, require_role},
    http_response::{api_error_status, api_ok, api_success, json_error, management_error},
    model_sync::{ModelSyncService, SyncUpstreamModelsRequest, SyncUpstreamPreviewRequest},
    options_util::{
        collect_json_object_keys, option_f64, require_payment_compliance, topup_info_payload,
    },
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use halolake_control_plane::{AdjustUserQuotaRequest, SnapshotRequest, SnapshotResponse};
use halolake_domain::{PageRequest, ROLE_ADMIN_USER, SearchRequest};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use service_async::Service;
use std::collections::BTreeSet;
use tracing::warn;

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RedemptionSearchQuery {
    #[serde(default = "crate::default_page")]
    pub(crate) page:      usize,
    #[serde(default = "crate::default_page_size")]
    pub(crate) page_size: usize,
    #[serde(default)]
    pub(crate) keyword:   String,
    #[serde(default)]
    pub(crate) status:    String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RedemptionUpdateQuery {
    #[serde(default)]
    pub(crate) status_only: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct TopUpQuery {
    #[serde(default = "crate::default_page")]
    pub(crate) page:      usize,
    #[serde(default = "crate::default_page_size")]
    pub(crate) page_size: usize,
    #[serde(default)]
    pub(crate) keyword:   String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RedeemTopUpPayload {
    pub(crate) key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CompleteTopUpPayload {
    pub(crate) trade_no: String,
}

pub(crate) async fn api_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = current_user(&state, &headers).await {
        return resp;
    }
    model_list_response(&state).await
}

pub(crate) async fn get_groups(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let mut groups = BTreeSet::new();
    groups.insert("default".to_string());
    if let Ok(options) = state.options.values() {
        collect_json_object_keys(options.get("GroupRatio"), &mut groups);
    }
    let data = match state.management.current_data() {
        Ok(data) => data,
        Err(err) => return management_error(err),
    };
    for user in data.users {
        push_non_empty_group(&mut groups, user.group);
    }
    for token in data.tokens {
        push_non_empty_group(&mut groups, token.group);
    }
    for channel in data.channels {
        for group in channel.group_list() {
            push_non_empty_group(&mut groups, group);
        }
    }
    api_success(groups.into_iter().collect::<Vec<_>>())
}

pub(crate) async fn list_redemptions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .billing
        .call(ListRedemptionsRequest { page: query.into() })
        .await
    {
        Ok(page) => api_success(page),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn search_redemptions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RedemptionSearchQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .billing
        .call(SearchRedemptionsRequest {
            page:    PageRequest {
                page:      query.page,
                page_size: query.page_size,
            },
            keyword: query.keyword,
            status:  query.status,
        })
        .await
    {
        Ok(page) => api_success(page),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn get_redemption(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.billing.call(GetRedemptionRequest { id }).await {
        Ok(redemption) => api_success(redemption),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn create_redemption(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(redemption): Json<RedemptionRecord>,
) -> Response {
    let actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_payment_compliance(&state) {
        return resp;
    }
    match state
        .billing
        .call(CreateRedemptionsRequest {
            redemption,
            user_id: actor.id,
        })
        .await
    {
        Ok(keys) => api_success(keys),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn update_redemption(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RedemptionUpdateQuery>,
    Json(redemption): Json<RedemptionRecord>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .billing
        .call(UpdateRedemptionRequest {
            redemption,
            status_only: query
                .status_only
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
        })
        .await
    {
        Ok(redemption) => api_success(redemption),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn delete_redemption(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.billing.call(DeleteRedemptionRequest { id }).await {
        Ok(()) => api_ok(),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn delete_invalid_redemptions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.billing.call(DeleteInvalidRedemptionsRequest).await {
        Ok(rows) => api_success(rows),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn topup_info(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = current_user(&state, &headers).await {
        return resp;
    }
    let options = state.options.values().unwrap_or_default();
    api_success(topup_info_payload(&options))
}

pub(crate) async fn list_self_topups(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TopUpQuery>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    list_topups(&state, ListTopUpsRequest {
        user_id:     Some(user.id),
        page:        PageRequest {
            page:      query.page,
            page_size: query.page_size,
        },
        keyword:     query.keyword,
        recent_only: true,
    })
    .await
}

pub(crate) async fn list_all_topups(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TopUpQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    list_topups(&state, ListTopUpsRequest {
        user_id:     None,
        page:        PageRequest {
            page:      query.page,
            page_size: query.page_size,
        },
        keyword:     query.keyword,
        recent_only: false,
    })
    .await
}

pub(crate) async fn list_topups(state: &AppState, req: ListTopUpsRequest) -> Response {
    match state.billing.call(req).await {
        Ok(page) => api_success(page),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn redeem_topup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<RedeemTopUpPayload>,
) -> Response {
    if let Err(resp) = require_payment_compliance(&state) {
        return resp;
    }
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let redeemed = match state
        .billing
        .call(RedeemRedemptionRequest {
            key:     payload.key,
            user_id: user.id,
        })
        .await
    {
        Ok(redeemed) => redeemed,
        Err(err) => {
            warn!(?err, user_id = user.id, "failed to redeem redemption key");
            return api_error_status(StatusCode::OK, "redeem.failed");
        }
    };
    match state
        .management
        .call(AdjustUserQuotaRequest {
            id:    user.id,
            delta: redeemed.quota,
        })
        .await
    {
        Ok(_) => api_success(redeemed.quota),
        Err(err) => {
            if let Err(rollback_err) = state
                .billing
                .call(RollbackRedeemRedemptionRequest {
                    id:      redeemed.id,
                    user_id: user.id,
                })
                .await
            {
                warn!(
                    ?rollback_err,
                    redemption_id = redeemed.id,
                    user_id = user.id,
                    "failed to roll back redeemed code after quota update error"
                );
            }
            management_error(err)
        }
    }
}

pub(crate) async fn complete_topup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CompleteTopUpPayload>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let quota_per_unit = state
        .options
        .values()
        .map(|options| option_f64(&options, "QuotaPerUnit", 500000.0))
        .unwrap_or(500000.0);
    let completed = match state
        .billing
        .call(CompleteTopUpRequest {
            trade_no: payload.trade_no,
            quota_per_unit,
        })
        .await
    {
        Ok(completed) => completed,
        Err(err) => return management_error(err),
    };
    if completed.quota > 0 {
        if let Err(err) = state
            .management
            .call(AdjustUserQuotaRequest {
                id:    completed.topup.user_id,
                delta: completed.quota,
            })
            .await
        {
            return management_error(err);
        }
    }
    api_success(JsonValue::Null)
}

pub(crate) async fn list_vendors(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .catalog
        .call(ListVendorsRequest { page: query.into() })
        .await
    {
        Ok(page) => api_success(page),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn search_vendors(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TokenSearchQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .catalog
        .call(SearchVendorsRequest {
            search: SearchRequest {
                page:    PageRequest {
                    page:      query.page,
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

pub(crate) async fn get_vendor(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.catalog.call(GetVendorRequest { id }).await {
        Ok(vendor) => api_success(vendor),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn create_vendor(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(vendor): Json<VendorRecord>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.catalog.call(CreateVendorRequest { vendor }).await {
        Ok(vendor) => api_success(vendor),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn update_vendor(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(vendor): Json<VendorRecord>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.catalog.call(UpdateVendorRequest { vendor }).await {
        Ok(vendor) => api_success(vendor),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn delete_vendor(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.catalog.call(DeleteVendorRequest { id }).await {
        Ok(()) => api_success(()),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn list_model_meta(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let mut page = match state
        .catalog
        .call(ListModelsRequest { page: query.into() })
        .await
    {
        Ok(page) => page,
        Err(err) => return management_error(err),
    };
    if let Err(resp) = enrich_model_page(&state, &mut page.items) {
        return resp;
    }
    let vendor_counts = match state.catalog.call(VendorModelCountsRequest).await {
        Ok(counts) => counts,
        Err(err) => return management_error(err),
    };
    api_success(json!({
        "items": page.items,
        "total": page.total,
        "page": page.page,
        "page_size": page.page_size,
        "vendor_counts": vendor_counts,
    }))
}

pub(crate) async fn search_model_meta(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TokenSearchQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let mut page = match state
        .catalog
        .call(SearchModelsRequest {
            search: SearchRequest {
                page:    PageRequest {
                    page:      query.page,
                    page_size: query.page_size,
                },
                keyword: query.keyword,
            },
            vendor: query.token,
        })
        .await
    {
        Ok(page) => page,
        Err(err) => return management_error(err),
    };
    if let Err(resp) = enrich_model_page(&state, &mut page.items) {
        return resp;
    }
    api_success(page)
}

pub(crate) async fn get_model_meta(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let mut model = match state.catalog.call(GetModelRequest { id }).await {
        Ok(model) => model,
        Err(err) => return management_error(err),
    };
    if let Err(resp) = enrich_model_page(&state, std::slice::from_mut(&mut model)) {
        return resp;
    }
    api_success(model)
}

pub(crate) async fn create_model_meta(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(model): Json<ModelRecord>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.catalog.call(CreateModelRequest { model }).await {
        Ok(model) => api_success(model),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn update_model_meta(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ModelUpdateQuery>,
    Json(model): Json<ModelRecord>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .catalog
        .call(UpdateModelRequest {
            model,
            status_only: query.status_only,
        })
        .await
    {
        Ok(model) => api_success(model),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn delete_model_meta(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.catalog.call(DeleteModelRequest { id }).await {
        Ok(()) => api_success(()),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn get_missing_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let management = match state.management.current_data() {
        Ok(data) => data,
        Err(err) => return management_error(err),
    };
    let catalog = match state.catalog.current_data() {
        Ok(data) => data,
        Err(err) => return management_error(err),
    };
    api_success(missing_models(&management, &catalog))
}

pub(crate) async fn sync_upstream_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SyncUpstreamPreviewRequest>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ModelSyncService::new(state.management.clone(), state.catalog.clone());
    match service.call(query).await {
        Ok(data) => api_success(data),
        Err(err) => api_error_status(StatusCode::OK, &format!("获取上游模型失败: {err}")),
    }
}

pub(crate) async fn sync_upstream_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SyncUpstreamModelsRequest>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let service = ModelSyncService::new(state.management.clone(), state.catalog.clone());
    match service.call(req).await {
        Ok(data) => api_success(data),
        Err(err) => api_error_status(StatusCode::OK, &format!("获取上游模型失败: {err}")),
    }
}

pub(crate) fn enrich_model_page(
    state: &AppState,
    models: &mut [ModelRecord],
) -> Result<(), Response> {
    let management = state.management.current_data().map_err(management_error)?;
    enrich_models(models, &management);
    Ok(())
}

pub(crate) async fn model_list_response(state: &AppState) -> Response {
    match state
        .snapshots
        .call(SnapshotRequest {
            since_version: None,
        })
        .await
    {
        Ok(SnapshotResponse::Updated { snapshot }) => {
            let models = snapshot
                .model_mappings
                .into_iter()
                .map(|mapping| mapping.requested_model)
                .collect::<Vec<_>>();
            api_success(models)
        }
        Ok(SnapshotResponse::NotModified { .. }) => api_success(Vec::<String>::new()),
        Err(err) => {
            warn!(?err, "failed to read models from snapshot");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "snapshot unavailable")
        }
    }
}

pub(crate) fn push_non_empty_group(groups: &mut BTreeSet<String>, group: String) {
    let group = group.trim();
    if !group.is_empty() {
        groups.insert(group.to_string());
    }
}
