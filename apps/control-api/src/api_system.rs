//! Setup, status, health, and internal gateway control-plane endpoints.

use crate::{
    AppState, SNAPSHOT_VERSION_HEADER,
    channel_feedback::ChannelFeedbackService,
    http_response::{
        HealthResponse, api_error_status, api_ok_message, api_success, channel_feedback_error,
        json_error, management_error, usage_error,
    },
    options_util::{
        checkin_setting, option_bool, option_f64, option_str, usage_pricing_from_options,
    },
    publish_management_snapshot, security,
    storage::UpdateOptionRequest,
    storage_backend_name,
};
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use certain_map::ParamSet;
use halolake_control_plane::{
    BootstrapRootUserRequest, ChannelFeedbackBatch, ControlActor, ControlContext, ControlRequestId,
    ManagementError, SettleUsageRequest, SnapshotRequest, SnapshotResponse, UsageEventBatch,
};
use halolake_domain::ROLE_ROOT_USER;
use serde::Deserialize;
use serde_json::json;
use service_async::Service;
use tracing::warn;
use uuid::Uuid;

// storage_backend_name is reexported from config via crate root

#[derive(Debug, Deserialize)]
pub(crate) struct SnapshotQuery {
    pub(crate) since_version: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SetupPayload {
    pub(crate) username:              String,
    pub(crate) password:              String,
    #[serde(rename = "confirmPassword")]
    pub(crate) confirm_password:      String,
    #[serde(default, rename = "SelfUseModeEnabled")]
    pub(crate) self_use_mode_enabled: bool,
    #[serde(default, rename = "DemoSiteEnabled")]
    pub(crate) demo_site_enabled:     bool,
}

pub(crate) async fn healthz(State(state): State<AppState>) -> Response {
    let snapshot_version = match state.snapshots.current_version() {
        Ok(version) => version,
        Err(err) => {
            warn!(?err, "failed to read snapshot version for healthz");
            0
        }
    };
    Json(HealthResponse {
        status: "ok",
        snapshot_version,
    })
    .into_response()
}

pub(crate) async fn gateway_snapshot(
    State(state): State<AppState>,
    Query(query): Query<SnapshotQuery>,
    headers: HeaderMap,
) -> Response {
    let _cx = ControlContext::new()
        .param_set(ControlRequestId(Uuid::new_v4().simple().to_string()))
        .param_set(ControlActor::System);

    if !state.authorized(&headers) {
        return json_error(StatusCode::UNAUTHORIZED, "invalid internal key");
    }

    let req = SnapshotRequest {
        since_version: query.since_version,
    };
    match state.snapshots.call(req).await {
        Ok(SnapshotResponse::NotModified { version }) => {
            let mut resp = StatusCode::NOT_MODIFIED.into_response();
            if let Ok(value) = HeaderValue::from_str(&version.to_string()) {
                resp.headers_mut().insert(SNAPSHOT_VERSION_HEADER, value);
            }
            resp
        }
        Ok(resp @ SnapshotResponse::Updated { .. }) => Json(resp).into_response(),
        Err(err) => {
            warn!(?err, "failed to read gateway snapshot");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "snapshot unavailable")
        }
    }
}

pub(crate) async fn gateway_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(batch): Json<UsageEventBatch>,
) -> Response {
    let _cx = ControlContext::new()
        .param_set(ControlRequestId(Uuid::new_v4().simple().to_string()))
        .param_set(ControlActor::System);

    if !state.authorized(&headers) {
        return api_error_status(StatusCode::UNAUTHORIZED, "invalid internal key");
    }

    match state.usage_events.record_batch(batch).await {
        Ok(recorded) => {
            if !recorded.accepted_events.is_empty() {
                let pricing = state
                    .options
                    .values()
                    .map(|options| usage_pricing_from_options(&options))
                    .unwrap_or_default();
                match state
                    .management
                    .call(SettleUsageRequest {
                        events: recorded.accepted_events,
                        pricing,
                    })
                    .await
                {
                    Ok(settlement) => {
                        if let Err(err) = state
                            .usage_events
                            .apply_quotas(&settlement.event_quotas)
                            .await
                        {
                            return usage_error(err);
                        }
                        tracing::debug!(
                            settled = settlement.settled,
                            skipped = settlement.skipped,
                            quota = settlement.quota,
                            tokens_exhausted = settlement.tokens_exhausted,
                            "settled gateway usage batch"
                        );
                        if settlement.tokens_exhausted > 0
                            && let Err(err) = publish_management_snapshot(&state).await
                        {
                            return management_error(err);
                        }
                    }
                    Err(err) => return management_error(err),
                }
            }
            api_success(recorded.ack)
        }
        Err(err) => usage_error(err),
    }
}

pub(crate) async fn gateway_channel_feedback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(batch): Json<ChannelFeedbackBatch>,
) -> Response {
    let _cx = ControlContext::new()
        .param_set(ControlRequestId(Uuid::new_v4().simple().to_string()))
        .param_set(ControlActor::System);

    if !state.authorized(&headers) {
        return api_error_status(StatusCode::UNAUTHORIZED, "invalid internal key");
    }

    let service = ChannelFeedbackService::new(state.management.clone(), state.options.clone());
    match service.call(batch).await {
        Ok(ack) => {
            if (ack.disabled_channels > 0 || ack.disabled_keys > 0)
                && let Err(err) = publish_management_snapshot(&state).await
            {
                return management_error(err);
            }
            api_success(ack)
        }
        Err(err) => channel_feedback_error(err),
    }
}

pub(crate) async fn api_setup(State(state): State<AppState>) -> Response {
    let root_init = match root_user_exists(&state) {
        Ok(root_init) => root_init,
        Err(err) => return management_error(err),
    };
    api_success(json!({
        "status": root_init,
        "root_init": root_init,
        "database_type": storage_backend_name(state.storage_backend),
    }))
}

pub(crate) async fn post_setup(
    State(state): State<AppState>,
    Json(payload): Json<SetupPayload>,
) -> Response {
    let root_init = match root_user_exists(&state) {
        Ok(root_init) => root_init,
        Err(err) => return management_error(err),
    };
    if root_init {
        return api_error_status(StatusCode::OK, "系统已经初始化完成");
    }

    let username = payload.username.trim();
    if username.len() > 12 {
        return api_error_status(StatusCode::OK, "用户名长度不能超过12个字符");
    }
    if payload.password != payload.confirm_password {
        return api_error_status(StatusCode::OK, "两次输入的密码不一致");
    }
    if payload.password.len() < 8 {
        return api_error_status(StatusCode::OK, "密码长度至少为8个字符");
    }

    if let Err(err) = state
        .management
        .call(BootstrapRootUserRequest {
            username: username.to_string(),
            password: payload.password,
        })
        .await
    {
        return match err {
            ManagementError::Duplicate => api_error_status(StatusCode::OK, "系统已经初始化完成"),
            ManagementError::InvalidRequest(message) => api_error_status(StatusCode::OK, message),
            err => management_error(err),
        };
    }

    if let Err(err) =
        update_setup_option(&state, "SelfUseModeEnabled", payload.self_use_mode_enabled).await
    {
        return management_error(err);
    }
    if let Err(err) =
        update_setup_option(&state, "DemoSiteEnabled", payload.demo_site_enabled).await
    {
        return management_error(err);
    }
    match publish_management_snapshot(&state).await {
        Ok(()) => api_ok_message("系统初始化成功"),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn api_status(State(state): State<AppState>) -> Response {
    let option_values = state.options.values().unwrap_or_default();
    let setup = match root_user_exists(&state) {
        Ok(setup) => setup,
        Err(err) => return management_error(err),
    };
    api_success(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "start_time": state.start_time_unix,
        "setup": setup,
        "database_type": storage_backend_name(state.storage_backend),
        "system_name": option_str(&option_values, "SystemName", state.system_name.as_ref()),
        "theme": option_str(&option_values, "theme.frontend", state.web.theme.as_str()),
        "server_address": option_str(&option_values, "ServerAddress", ""),
        "quota_per_unit": option_f64(&option_values, "QuotaPerUnit", 500000.0),
        "display_in_currency": option_bool(&option_values, "DisplayInCurrencyEnabled", false),
        "quota_display_type": option_str(&option_values, "QuotaDisplayType", "quota"),
        "custom_currency_symbol": option_str(&option_values, "CustomCurrencySymbol", "$"),
        "custom_currency_exchange_rate": option_f64(&option_values, "CustomCurrencyExchangeRate", 1.0),
        "enable_batch_update": option_bool(&option_values, "BatchUpdateEnabled", true),
        "enable_drawing": option_bool(&option_values, "DrawingEnabled", true),
        "enable_task": option_bool(&option_values, "TaskEnabled", true),
        "enable_data_export": option_bool(&option_values, "DataExportEnabled", false),
        "default_collapse_sidebar": option_bool(&option_values, "DefaultCollapseSidebar", false),
        "demo_site_enabled": option_bool(&option_values, "DemoSiteEnabled", false),
        "self_use_mode_enabled": option_bool(&option_values, "SelfUseModeEnabled", false),
        "default_use_auto_group": option_bool(&option_values, "DefaultUseAutoGroup", false),
        "register_enabled": option_bool(&option_values, "RegisterEnabled", false),
        "password_login_enabled": option_bool(&option_values, "PasswordLoginEnabled", true),
        "password_register_enabled": option_bool(&option_values, "PasswordRegisterEnabled", false),
        "email_verification": option_bool(&option_values, "EmailVerificationEnabled", false),
        "github_oauth": option_bool(&option_values, "GitHubOAuthEnabled", false),
        "discord_oauth": option_bool(&option_values, "discord.enabled", false),
        "linuxdo_oauth": option_bool(&option_values, "LinuxDOOAuthEnabled", false),
        "telegram_oauth": option_bool(&option_values, "TelegramOAuthEnabled", false),
        "wechat_login": option_bool(&option_values, "WeChatAuthEnabled", false),
        "turnstile_check": option_bool(&option_values, "TurnstileCheckEnabled", false),
        "passkey_login": security::passkey_login_enabled(&option_values),
        "user_agreement_enabled": false,
        "privacy_policy_enabled": false,
        "checkin_enabled": checkin_setting(&option_values).enabled,
        "api_info_enabled": false,
        "uptime_kuma_enabled": false,
        "announcements_enabled": false,
        "faq_enabled": false,
        "HeaderNavModules": serde_json::Value::Null,
        "SidebarModulesAdmin": serde_json::Value::Null,
    }))
}

pub(crate) fn root_user_exists(state: &AppState) -> Result<bool, ManagementError> {
    Ok(state
        .management
        .current_data()?
        .users
        .iter()
        .any(|user| user.role == ROLE_ROOT_USER))
}

pub(crate) async fn update_setup_option(
    state: &AppState,
    key: &'static str,
    value: bool,
) -> Result<(), ManagementError> {
    state
        .options
        .call(UpdateOptionRequest {
            key:   key.to_string(),
            value: bool_option_value(value).to_string(),
        })
        .await
        .map(|_| ())
}

pub(crate) fn bool_option_value(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}
