use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{
        HeaderMap, HeaderValue, StatusCode, Uri,
        header::{CACHE_CONTROL, HOST, SET_COOKIE},
    },
    response::{IntoResponse, Response},
    routing::{get, post},
};
use certain_map::ParamSet;
use halolake_control_plane::{
    AdjustUserQuotaRequest, BootstrapRootUserRequest,
    ChannelFeedbackBatch,
    ControlActor, ControlContext, ControlRequestId, CreateTokenRequest,
    CreateUserRequest, DeleteTokenRequest,
    DeleteUserRequest, GetTokenRequest, GetUserRequest,
    ListTokensRequest, ListUsersRequest, LoginUserRequest, ManageUserRequest, ManagementData,
    ManagementError, MemorySnapshotBus, PublishSnapshotRequest,
    RegisterUserRequest, RevealTokenKeyRequest, SearchTokensRequest,
    SearchUsersRequest, SettleUsageRequest, SnapshotRequest, SnapshotResponse, UpdateTokenRequest,
    UpdateUserAccessTokenRequest, UpdateUserRequest, UsageEventBatch, UsagePricing, ensure_user_password_hashed,
};
use halolake_domain::{
    ChannelRecord, PageRequest, PageResult, ROLE_ADMIN_USER, ROLE_ROOT_USER, STATUS_ENABLED,
    SearchRequest, TokenRecord, UsageEvent, UsageStatus, UserRecord,
};
use halolake_router_core::{
    ChannelAffinityConfig, ChannelAffinityRule, GatewaySnapshot, GroupRoutingConfig,
};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use service_async::Service;
use tracing::{info, warn};
use uuid::Uuid;

mod config;
mod billing;
mod catalog;
mod channel_affinity;
mod channel_feedback;
mod channel_ops;
mod channel_probe;
mod channel_special;
mod channel_task;
mod checkin;
mod compat;
mod model_sync;
mod playground;
mod prefill;
mod proxy;
mod ratio_sync;
mod security;
mod session;
mod storage;
mod system_instance;
mod store_open;
mod http_response;
mod http_auth;
mod api_channel;
mod api_web;
mod api_usage;
mod system_task;
use billing::{
    BillingStore, CompleteTopUpRequest, CreateRedemptionsRequest, DeleteInvalidRedemptionsRequest,
    DeleteRedemptionRequest, GetRedemptionRequest, ListRedemptionsRequest, ListTopUpsRequest,
    RedeemRedemptionRequest, RedemptionRecord, RollbackRedeemRedemptionRequest,
    SearchRedemptionsRequest, UpdateRedemptionRequest,
};
use catalog::{
    CatalogData, CatalogStore, CreateModelRequest, CreateVendorRequest, DeleteModelRequest,
    DeleteVendorRequest, GetModelRequest, GetVendorRequest, ListModelsRequest, ListVendorsRequest,
    ModelRecord, SearchModelsRequest, SearchVendorsRequest, UpdateModelRequest,
    UpdateVendorRequest, VendorModelCountsRequest, VendorRecord, enrich_models, missing_models,
};
use channel_affinity::{
    ChannelAffinityService, ClearChannelAffinityCacheRequest, GetChannelAffinityCacheStatsRequest,
};
use channel_feedback::ChannelFeedbackService;
use channel_task::{
    ChannelTaskSchedulerConfig, spawn_channel_task_scheduler,
};
use checkin::{CheckinStore, CreateCheckinRequest, GetCheckinStatsRequest};
use prefill::PrefillStore;
use proxy::ProxyStore;
use model_sync::{ModelSyncService, SyncUpstreamModelsRequest, SyncUpstreamPreviewRequest};
use ratio_sync::{FetchUpstreamRatiosRequest, ListSyncableChannelsRequest, RatioSyncService};
use session::{SessionSigner, SessionStore};
use security::{
    AdminDisableTwoFaRequest, AdminResetPasskeyRequest, AdminTwoFaStatsRequest,
    DeletePasskeyRequest, DisableTwoFaRequest, EnableTwoFaRequest, GetPasskeyStatusRequest,
    GetTwoFaStatusRequest, PasskeyFlow, PasskeyFlowRequest, PasskeyFlowResponse,
    PasskeyRequestContext, PasskeyUser, RegenerateTwoFaBackupCodesRequest,
    SecurityService, SecurityStore, StartTwoFaSetupRequest, UniversalVerifyRequest,
    VerificationMethod,
};
use storage::{
    ListOptionsRequest, ManagementStore, OptionStore,
    UpdateOptionRequest, UsageStore,
};
use system_instance::{
    SystemInstanceStore, spawn_system_instance_reporter,
};
use system_task::SystemTaskStore;



pub(crate) use config::{
    ensure_supported_storage_backend, storage_backend_name,
};


pub(crate) use http_response::{
    api_error_status, api_ok, api_ok_message, api_success,
    api_success_with_extra, api_success_with_message, channel_feedback_error, json_error,
    management_error, security_error, usage_error, HealthResponse,
};
pub(crate) use http_auth::{
    clear_session_cookie,
    current_user, login_payload,
    require_role, require_secure_verification, self_payload,
    session_id_from_headers, set_session_cookie, token_from_read_only_auth,
};
pub(crate) use api_channel::*;
pub(crate) use api_web::*;
pub(crate) use api_usage::*;

pub use config::{
    ControlApiConfig, InternalConfig, LogStorageBackend, ServerConfig, SessionConfig, StorageBackend,
    StorageConfig, SystemConfig, WebConfig,
};

pub(crate) const INTERNAL_KEY_HEADER: &str = "x-halolake-internal-key";
pub(crate) const SNAPSHOT_VERSION_HEADER: &str = "x-halolake-snapshot-version";
pub(crate) const NEW_API_USER_HEADER: &str = "new-api-user";
pub(crate) const SESSION_COOKIE_NAME: &str = "session";
pub(crate) const MAX_RECENT_TOKEN_LOGS: usize = 1000;
pub(crate) const TOKEN_STATUS_EXPIRED: i32 = 3;
pub(crate) const TOKEN_STATUS_EXHAUSTED: i32 = 4;
const DEFAULT_MODEL_RATIO_JSON: &str = "{}";
const DEFAULT_CHANNEL_AFFINITY_RULES_JSON: &str = r#"[{"name":"codex cli trace","model_regex":["^gpt-.*$"],"path_regex":["/v1/responses"],"key_sources":[{"type":"gjson","path":"prompt_cache_key"}],"value_regex":"","ttl_seconds":0,"param_override_template":{"operations":[{"mode":"pass_headers","value":["Originator","Session_id","User-Agent","X-Codex-Beta-Features","X-Codex-Turn-Metadata"],"keep_origin":true}]},"skip_retry_on_failure":true,"include_using_group":true,"include_rule_name":true},{"name":"claude cli trace","model_regex":["^claude-.*$"],"path_regex":["/v1/messages"],"key_sources":[{"type":"gjson","path":"metadata.user_id"}],"value_regex":"","ttl_seconds":0,"param_override_template":{"operations":[{"mode":"pass_headers","value":["X-Stainless-Arch","X-Stainless-Lang","X-Stainless-Os","X-Stainless-Package-Version","X-Stainless-Retry-Count","X-Stainless-Runtime","X-Stainless-Runtime-Version","X-Stainless-Timeout","User-Agent","X-App","Anthropic-Beta","Anthropic-Dangerous-Direct-Browser-Access","Anthropic-Version"],"keep_origin":true}]},"skip_retry_on_failure":true,"include_using_group":true,"include_rule_name":true}]"#;

#[derive(Debug, Clone, Copy)]
pub(crate) struct EmbeddedAsset {
    pub(crate) path: &'static str,
    pub(crate) bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));
// DEFAULT_WEB_ASSETS / CLASSIC_WEB_ASSETS come from include! above.

// DEFAULT_WEB_ASSETS / CLASSIC_WEB_ASSETS come from include! above.


#[derive(Debug, Clone)]
pub struct ControlApi {
    state: AppState,
}

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub(crate) snapshots: MemorySnapshotBus,
    pub(crate) management: ManagementStore,
    pub(crate) usage_events: UsageStore,
    pub(crate) catalog: CatalogStore,
    pub(crate) options: OptionStore,
    pub(crate) billing: BillingStore,
    pub(crate) checkins: CheckinStore,
    pub(crate) prefill: PrefillStore,
    pub(crate) proxies: ProxyStore,
    pub(crate) security: SecurityService,
    pub(crate) system_tasks: SystemTaskStore,
    pub(crate) system_instances: SystemInstanceStore,
    pub(crate) sessions: SessionStore,
    pub(crate) session_signer: SessionSigner,
    pub(crate) web: WebConfig,
    pub(crate) internal_secret: Option<Arc<str>>,
    pub(crate) gateway_base_url: Option<String>,
    pub(crate) start_time_unix: i64,
    pub(crate) system_name: Arc<str>,
    pub(crate) storage_backend: StorageBackend,
}


#[derive(Debug, Deserialize)]
struct SnapshotQuery {
    since_version: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct SetupPayload {
    username: String,
    password: String,
    #[serde(rename = "confirmPassword")]
    confirm_password: String,
    #[serde(default, rename = "SelfUseModeEnabled")]
    self_use_mode_enabled: bool,
    #[serde(default, rename = "DemoSiteEnabled")]
    demo_site_enabled: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(crate) struct PageQuery {
    #[serde(default = "default_page")]
    page: usize,
    #[serde(default = "default_page_size")]
    page_size: usize,
}

impl From<PageQuery> for PageRequest {
    fn from(query: PageQuery) -> Self {
        Self {
            page: query.page,
            page_size: query.page_size,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TokenSearchQuery {
    #[serde(default = "default_page")]
    page: usize,
    #[serde(default = "default_page_size")]
    page_size: usize,
    #[serde(default)]
    keyword: String,
    #[serde(default)]
    token: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ChannelSearchQuery {
    #[serde(default = "default_page")]
    page: usize,
    #[serde(default = "default_page_size")]
    page_size: usize,
    #[serde(default)]
    keyword: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct UserSearchQuery {
    #[serde(default = "default_page")]
    page: usize,
    #[serde(default = "default_page_size")]
    page_size: usize,
    #[serde(default)]
    keyword: String,
    #[serde(default)]
    group: String,
    #[serde(default)]
    role: Option<i32>,
    #[serde(default)]
    status: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TwoFaCodePayload {
    #[serde(default)]
    code: String,
}

#[derive(Debug, Clone, Deserialize)]
struct UniversalVerifyPayload {
    method: VerificationMethod,
    #[serde(default)]
    code: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RegisterPayload {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    verification_code: String,
    #[serde(default)]
    aff_code: String,
}

#[derive(Debug, Clone, Deserialize)]
struct OptionUpdatePayload {
    key: String,
    #[serde(default)]
    value: JsonValue,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct PaymentCompliancePayload {
    confirmed: bool,
}

#[derive(Debug, Clone, Copy)]
struct CheckinSetting {
    enabled: bool,
    min_quota: i64,
    max_quota: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct UserManagePayload {
    id: u64,
    action: String,
    #[serde(default)]
    value: i64,
    #[serde(default)]
    mode: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ChannelAffinityCacheQuery {
    #[serde(default)]
    all: bool,
    #[serde(default)]
    rule_name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CheckinQuery {
    #[serde(default)]
    month: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PricingQuery {
    #[serde(default)]
    group: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RankingsQuery {
    #[serde(default = "default_ranking_period")]
    period: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PerfMetricsQuery {
    #[serde(default)]
    model: String,
    #[serde(default)]
    group: String,
    #[serde(default = "default_perf_hours")]
    hours: i64,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct ModelUpdateQuery {
    #[serde(default)]
    status_only: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RedemptionSearchQuery {
    #[serde(default = "default_page")]
    page: usize,
    #[serde(default = "default_page_size")]
    page_size: usize,
    #[serde(default)]
    keyword: String,
    #[serde(default)]
    status: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RedemptionUpdateQuery {
    #[serde(default)]
    status_only: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TopUpQuery {
    #[serde(default = "default_page")]
    page: usize,
    #[serde(default = "default_page_size")]
    page_size: usize,
    #[serde(default)]
    keyword: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RedeemTopUpPayload {
    key: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CompleteTopUpPayload {
    trade_no: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BatchIds {
    ids: Vec<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct StatusUpdate {
    status: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ChannelTagPayload {
    tag: String,
    #[serde(default)]
    new_tag: Option<String>,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    weight: Option<u32>,
    #[serde(default)]
    model_mapping: Option<String>,
    #[serde(default)]
    models: Option<String>,
    #[serde(default)]
    groups: Option<String>,
    #[serde(default)]
    param_override: Option<String>,
    #[serde(default)]
    header_override: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ChannelBatchTagPayload {
    ids: Vec<u64>,
    #[serde(default)]
    tag: Option<String>,
}

impl ControlApi {
    pub async fn try_from_config(config: ControlApiConfig) -> Result<Self> {
        ensure_supported_storage_backend(&config.storage)?;
        let start_time_unix = now_unix();
        let internal_secret = config.internal.secret.as_deref().map(Arc::<str>::from);
        let gateway_base_url = config
            .internal
            .gateway_base_url
            .as_ref()
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty())
            .or_else(|| {
                std::env::var("HALOLAKE_GATEWAY_BASE_URL")
                    .ok()
                    .map(|url| url.trim().to_string())
                    .filter(|url| !url.is_empty())
            });
        if internal_secret.is_none() {
            warn!(
                "internal.secret is not configured; /internal/gateway/* endpoints will \
                 reject every request (default-deny). Set internal.secret to enable the \
                 gateway snapshot/usage/channel-feedback APIs."
            );
        }
        let system_name = Arc::from(config.system.name.as_str());
        let option_defaults = default_options(&config);
        let mut snapshot = config.snapshot();
        snapshot.channel_affinity = channel_affinity_config_from_options(&option_defaults);
        snapshot.group_routing = group_routing_config_from_options(&option_defaults);
        let _indexed = snapshot
            .clone()
            .index()
            .context("validate initial gateway snapshot")?;
        let mut management_data = ManagementData::from_snapshot(snapshot.clone());
        management_data.users = config.users;
        normalize_config_users(&mut management_data.users)?;
        let catalog_seed = CatalogData::from_management(&management_data);
        // Zero-cost monomorphized open: each store type is resolved statically.
        let management: ManagementStore =
            store_open::open_seeded_from_config(&config.storage, management_data).await?;
        let usage_events: UsageStore = store_open::open_from_config(&config.storage).await?;
        let catalog: CatalogStore =
            store_open::open_seeded_from_config(&config.storage, catalog_seed).await?;
        let options: OptionStore =
            store_open::open_seeded_from_config(&config.storage, option_defaults).await?;
        let security_store: SecurityStore = store_open::open_from_config(&config.storage).await?;
        let security = SecurityService::new(options.clone(), security_store);
        let billing: BillingStore = store_open::open_from_config(&config.storage).await?;
        let checkins: CheckinStore = store_open::open_from_config(&config.storage).await?;
        let prefill: PrefillStore = store_open::open_from_config(&config.storage).await?;
        let proxies: ProxyStore = store_open::open_from_config(&config.storage).await?;
        let system_tasks: SystemTaskStore = store_open::open_from_config(&config.storage).await?;
        let system_instances: SystemInstanceStore =
            store_open::open_from_config(&config.storage).await?;
        let sessions: SessionStore = store_open::open_from_config(&config.storage).await?;
        let session_secret = config
            .session
            .secret
            .clone()
            .or_else(|| std::env::var("SESSION_SECRET").ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if session_secret.is_none() {
            warn!(
                "session.secret / SESSION_SECRET is not set; session cookies are unsigned                  (acceptable for local memory/dev only)"
            );
        }
        let session_signer = SessionSigner::new(session_secret.unwrap_or_default());
        let snapshots = MemorySnapshotBus::new(snapshot);
        spawn_system_instance_reporter(system_instances.clone(), start_time_unix);
        if config.system.task_scheduler_enabled {
            spawn_channel_task_scheduler(
                system_tasks.clone(),
                management.clone(),
                options.clone(),
                snapshots.clone(),
                proxies.clone(),
                ChannelTaskSchedulerConfig {
                    channel_test_interval: Some(Duration::from_secs(
                        config.system.channel_test_interval_seconds,
                    )),
                    model_update_interval: Some(Duration::from_secs(
                        config.system.model_update_interval_seconds,
                    )),
                },
            );
        }

        Ok(Self {
            state: AppState {
                snapshots,
                management,
                usage_events,
                catalog,
                options,
                billing,
                checkins,
                prefill,
                proxies,
                security,
                system_tasks,
                system_instances,
                sessions,
                session_signer,
                web: config.web,
                internal_secret,
                gateway_base_url,
                start_time_unix,
                system_name,
                storage_backend: config.storage.backend,
            },
        })
    }

    pub fn router(self) -> Router {
        let router = Router::new()
            .route("/healthz", get(healthz))
            .route("/api/setup", get(api_setup).post(post_setup))
            .route("/api/setup/", get(api_setup).post(post_setup))
            .route("/api/status", get(api_status))
            .route("/api/pricing", get(api_pricing))
            .route("/api/rankings", get(api_rankings))
            .route("/api/perf-metrics", get(api_perf_metrics))
            .route("/api/perf-metrics/summary", get(api_perf_metrics_summary))
            .route("/api/option", get(get_options).put(update_option))
            .route("/api/option/", get(get_options).put(update_option))
            .route(
                "/api/option/payment_compliance",
                post(confirm_payment_compliance),
            )
            .route(
                "/api/option/channel_affinity_cache",
                get(get_channel_affinity_cache_stats).delete(clear_channel_affinity_cache),
            )
            .route("/api/option/rest_model_ratio", post(reset_model_ratio))
            .route("/api/ratio_sync/channels", get(get_syncable_channels))
            .route("/api/ratio_sync/fetch", post(fetch_upstream_ratios))
            .route(
                "/api/models",
                get(api_models)
                    .post(create_model_meta)
                    .put(update_model_meta),
            )
            .route(
                "/api/models/",
                get(list_model_meta)
                    .post(create_model_meta)
                    .put(update_model_meta),
            )
            .route("/api/models/search", get(search_model_meta))
            .route("/api/models/missing", get(get_missing_models))
            .route(
                "/api/models/sync_upstream/preview",
                get(sync_upstream_preview),
            )
            .route("/api/models/sync_upstream", post(sync_upstream_models))
            .route(
                "/api/models/{id}",
                get(get_model_meta).delete(delete_model_meta),
            )
            .route(
                "/api/vendors",
                get(list_vendors).post(create_vendor).put(update_vendor),
            )
            .route(
                "/api/vendors/",
                get(list_vendors).post(create_vendor).put(update_vendor),
            )
            .route("/api/vendors/search", get(search_vendors))
            .route("/api/vendors/{id}", get(get_vendor).delete(delete_vendor))
            .route(
                "/api/redemption",
                get(list_redemptions)
                    .post(create_redemption)
                    .put(update_redemption),
            )
            .route(
                "/api/redemption/",
                get(list_redemptions)
                    .post(create_redemption)
                    .put(update_redemption),
            )
            .route("/api/redemption/search", get(search_redemptions))
            .route(
                "/api/redemption/invalid",
                axum::routing::delete(delete_invalid_redemptions),
            )
            .route(
                "/api/redemption/{id}",
                get(get_redemption).delete(delete_redemption),
            )
            .route("/api/group", get(get_groups))
            .route("/api/group/", get(get_groups))
            .route("/api/verify", post(universal_verify))
            .route("/api/user/register", post(register_user))
            .route("/api/user/login", post(login_user))
            .route("/api/user/login/2fa", post(login_2fa))
            .route("/api/user/passkey/login/begin", post(passkey_login_begin))
            .route("/api/user/passkey/login/finish", post(passkey_login_finish))
            .route("/api/user/logout", get(logout_user))
            .route("/api/user/groups", get(user_groups))
            .route("/api/user/self/groups", get(user_groups))
            .route(
                "/api/user/passkey",
                get(passkey_status).delete(delete_passkey),
            )
            .route(
                "/api/user/passkey/register/begin",
                post(passkey_register_begin),
            )
            .route(
                "/api/user/passkey/register/finish",
                post(passkey_register_finish),
            )
            .route("/api/user/passkey/verify/begin", post(passkey_verify_begin))
            .route(
                "/api/user/passkey/verify/finish",
                post(passkey_verify_finish),
            )
            .route("/api/user/2fa/status", get(two_fa_status))
            .route("/api/user/2fa/setup", post(setup_two_fa))
            .route("/api/user/2fa/enable", post(enable_two_fa))
            .route("/api/user/2fa/disable", post(disable_two_fa))
            .route(
                "/api/user/2fa/backup_codes",
                post(regenerate_two_fa_backup_codes),
            )
            .route("/api/user/2fa/stats", get(admin_two_fa_stats))
            .route(
                "/api/user/self",
                get(get_self).put(update_self).delete(delete_self),
            )
            .route(
                "/api/user/checkin",
                get(get_checkin_status).post(do_checkin),
            )
            .route("/api/user/topup/info", get(topup_info))
            .route("/api/user/topup/self", get(list_self_topups))
            .route("/api/user/topup/complete", post(complete_topup))
            .route("/api/user/topup", get(list_all_topups).post(redeem_topup))
            .route("/api/user/topup/", get(list_all_topups).post(redeem_topup))
            .route("/api/user/models", get(user_models))
            .route("/api/user/token", get(generate_access_token))
            .route(
                "/api/user",
                get(list_users).post(create_user).put(update_user),
            )
            .route(
                "/api/user/",
                get(list_users).post(create_user).put(update_user),
            )
            .route("/api/user/search", get(search_users))
            .route("/api/user/manage", post(manage_user))
            .route(
                "/api/user/{id}/reset_passkey",
                axum::routing::delete(admin_reset_passkey),
            )
            .route(
                "/api/user/{id}/2fa",
                axum::routing::delete(admin_disable_two_fa),
            )
            .route("/api/user/{id}", get(get_user).delete(delete_user))
            .route(
                "/api/token",
                get(list_tokens).post(create_token).put(update_token),
            )
            .route(
                "/api/token/",
                get(list_tokens).post(create_token).put(update_token),
            )
            .route("/api/token/search", get(search_tokens))
            .route("/api/token/batch", post(delete_token_batch))
            .route("/api/token/batch/keys", post(reveal_token_keys_batch))
            .route("/api/token/{id}", get(get_token).delete(delete_token))
            .route("/api/token/{id}/key", post(reveal_token_key))
            .route("/api/usage/token", get(get_token_usage))
            .route("/api/usage/token/", get(get_token_usage))
            .route("/api/log", get(list_logs).delete(delete_history_logs))
            .route("/api/log/", get(list_logs).delete(delete_history_logs))
            .route("/api/log/token", get(list_token_logs))
            .route("/api/log/search", get(search_logs_deprecated))
            .route("/api/log/self", get(list_self_logs))
            .route("/api/log/self/search", get(search_logs_deprecated))
            .route("/api/log/stat", get(log_stats))
            .route("/api/log/self/stat", get(self_log_stats))
            .route(
                "/api/log/channel_affinity_usage_cache",
                get(get_channel_affinity_usage_cache_stats),
            )
            .route(
                "/api/system-task/log-cleanup",
                post(create_log_cleanup_system_task),
            )
            .route("/api/system-task/list", get(list_system_tasks))
            .route("/api/system-task/current", get(get_current_system_task))
            .route("/api/system-task/{task_id}", get(get_system_task))
            .route("/api/system-info/instances", get(list_system_instances))
            .route(
                "/api/system-info/stale-instances",
                axum::routing::delete(delete_stale_system_instances),
            )
            .route(
                "/api/system-info/instances/{node_name}",
                axum::routing::delete(delete_stale_system_instance),
            )
            .route("/api/data", get(data_all_quota))
            .route("/api/data/", get(data_all_quota))
            .route("/api/data/users", get(data_quota_by_user))
            .route("/api/data/self", get(data_self_quota))
            .route("/api/data/flow", get(data_all_flow))
            .route("/api/data/flow/self", get(data_self_flow))
            .route(
                "/api/proxy",
                get(list_proxies).post(create_proxy).put(update_proxy),
            )
            .route(
                "/api/proxy/",
                get(list_proxies).post(create_proxy).put(update_proxy),
            )
            .route("/api/proxy/{id}", get(get_proxy).delete(delete_proxy))
            .route(
                "/api/channel",
                get(list_channels).post(create_channel).put(update_channel),
            )
            .route(
                "/api/channel/",
                get(list_channels).post(create_channel).put(update_channel),
            )
            .route("/api/channel/search", get(search_channels))
            .route("/api/channel/models", get(channel_models))
            .route("/api/channel/models_enabled", get(channel_models))
            .route("/api/channel/ops", get(channel_ops))
            .route("/api/channel/test", get(test_all_channels))
            .route("/api/channel/test/{id}", get(test_channel))
            .route(
                "/api/channel/update_balance",
                get(update_all_channel_balances),
            )
            .route(
                "/api/channel/update_balance/{id}",
                get(update_channel_balance),
            )
            .route(
                "/api/channel/fetch_models",
                post(fetch_models_for_channel_payload),
            )
            .route(
                "/api/channel/fetch_models/{id}",
                get(fetch_models_for_channel),
            )
            .route(
                "/api/channel/status/batch",
                post(update_channel_status_batch),
            )
            .route(
                "/api/channel/disabled",
                axum::routing::delete(delete_disabled_channels),
            )
            .route("/api/channel/fix", post(fix_channel_abilities))
            .route("/api/channel/tag/disabled", post(disable_tag_channels))
            .route("/api/channel/tag/enabled", post(enable_tag_channels))
            .route("/api/channel/tag", axum::routing::put(edit_tag_channels))
            .route("/api/channel/tag/models", get(tag_models))
            .route("/api/channel/batch", post(delete_channel_batch))
            .route("/api/channel/batch/tag", post(batch_set_channel_tag))
            .route("/api/channel/copy/{id}", post(copy_channel))
            .route("/api/channel/multi_key/manage", post(manage_multi_keys))
            .route("/api/channel/ollama/pull", post(ollama_pull_model))
            .route(
                "/api/channel/ollama/pull/stream",
                post(ollama_pull_model_stream),
            )
            .route(
                "/api/channel/ollama/delete",
                axum::routing::delete(ollama_delete_model),
            )
            .route("/api/channel/ollama/version/{id}", get(ollama_version))
            .route(
                "/api/channel/upstream_updates/apply",
                post(apply_channel_upstream_model_updates),
            )
            .route(
                "/api/channel/upstream_updates/apply_all",
                post(apply_all_channel_upstream_model_updates),
            )
            .route(
                "/api/channel/upstream_updates/detect",
                post(detect_channel_upstream_model_updates),
            )
            .route(
                "/api/channel/upstream_updates/detect_all",
                post(detect_all_channel_upstream_model_updates),
            )
            .route(
                "/api/channel/{id}/codex/refresh",
                post(refresh_codex_channel_credential),
            )
            .route(
                "/api/channel/{id}/codex/usage",
                get(get_codex_channel_usage),
            )
            .route(
                "/api/channel/{id}/codex/usage/reset-credits",
                get(get_codex_channel_rate_limit_reset_credits),
            )
            .route(
                "/api/channel/{id}/codex/usage/reset",
                post(reset_codex_channel_usage),
            )
            .route("/api/channel/{id}", get(get_channel).delete(delete_channel))
            .route("/api/channel/{id}/key", post(reveal_channel_key))
            .route("/api/channel/{id}/status", post(update_channel_status))
            .route("/api/notice", get(api_empty_string))
            .route("/api/user-agreement", get(api_empty_string))
            .route("/api/privacy-policy", get(api_empty_string))
            .route("/api/about", get(api_empty_string))
            .route("/api/home_page_content", get(api_empty_string))
            .route("/internal/gateway/snapshot", get(gateway_snapshot))
            .route("/internal/gateway/usage", post(gateway_usage))
            .route(
                "/internal/gateway/channel-feedback",
                post(gateway_channel_feedback),
            );
        let router = playground::mount(router);
        compat::mount(router)
            .fallback(web_fallback)
            .with_state(self.state)
    }
}

pub async fn run_from_config_file(path: &str) -> Result<()> {
    let config = ControlApiConfig::load(path)?;
    let listen = config.server.listen;
    let api = ControlApi::try_from_config(config).await?;
    info!(%listen, "starting halolake control api");
    serve(listen, api).await
}

pub async fn serve(addr: SocketAddr, api: ControlApi) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("bind control-api listener")?;
    axum::serve(listener, api.router())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve control-api")
}

async fn healthz(State(state): State<AppState>) -> Response {
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

async fn gateway_snapshot(
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

async fn gateway_usage(
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

async fn gateway_channel_feedback(
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

async fn api_setup(State(state): State<AppState>) -> Response {
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

async fn post_setup(State(state): State<AppState>, Json(payload): Json<SetupPayload>) -> Response {
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

async fn api_status(State(state): State<AppState>) -> Response {
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

fn root_user_exists(state: &AppState) -> Result<bool, ManagementError> {
    Ok(state
        .management
        .current_data()?
        .users
        .iter()
        .any(|user| user.role == ROLE_ROOT_USER))
}

async fn update_setup_option(
    state: &AppState,
    key: &'static str,
    value: bool,
) -> Result<(), ManagementError> {
    state
        .options
        .call(UpdateOptionRequest {
            key: key.to_string(),
            value: bool_option_value(value).to_string(),
        })
        .await
        .map(|_| ())
}

fn bool_option_value(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

async fn api_pricing(State(state): State<AppState>, Query(query): Query<PricingQuery>) -> Response {
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

async fn api_rankings(
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

async fn api_perf_metrics(
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

async fn api_perf_metrics_summary(
    State(state): State<AppState>,
    Query(query): Query<PerfMetricsQuery>,
) -> Response {
    let events = match state.usage_events.events() {
        Ok(events) => events,
        Err(err) => return usage_error(err),
    };
    api_success(perf_metrics_summary(&events, query.hours))
}

async fn get_options(State(state): State<AppState>, headers: HeaderMap) -> Response {
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
            visible.push(storage::OptionRecord {
                key: "CompletionRatioMeta".to_string(),
                value: build_completion_ratio_meta(&option_values),
            });
            api_success(visible)
        }
        Err(err) => management_error(err),
    }
}

async fn update_option(
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

async fn reset_model_ratio(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ROOT_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .options
        .call(UpdateOptionRequest {
            key: "ModelRatio".to_string(),
            value: DEFAULT_MODEL_RATIO_JSON.to_string(),
        })
        .await
    {
        Ok(_) => api_ok(),
        Err(err) => management_error(err),
    }
}

async fn get_syncable_channels(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

async fn fetch_upstream_ratios(
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

async fn get_channel_affinity_cache_stats(
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

async fn clear_channel_affinity_cache(
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
            all: query.all,
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

async fn confirm_payment_compliance(
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

async fn api_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = current_user(&state, &headers).await {
        return resp;
    }
    model_list_response(&state).await
}

async fn get_groups(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

async fn list_redemptions(
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

async fn search_redemptions(
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
            page: PageRequest {
                page: query.page,
                page_size: query.page_size,
            },
            keyword: query.keyword,
            status: query.status,
        })
        .await
    {
        Ok(page) => api_success(page),
        Err(err) => management_error(err),
    }
}

async fn get_redemption(
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

async fn create_redemption(
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

async fn update_redemption(
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

async fn delete_redemption(
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

async fn delete_invalid_redemptions(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.billing.call(DeleteInvalidRedemptionsRequest).await {
        Ok(rows) => api_success(rows),
        Err(err) => management_error(err),
    }
}

async fn topup_info(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = current_user(&state, &headers).await {
        return resp;
    }
    let options = state.options.values().unwrap_or_default();
    api_success(topup_info_payload(&options))
}

async fn list_self_topups(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TopUpQuery>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    list_topups(
        &state,
        ListTopUpsRequest {
            user_id: Some(user.id),
            page: PageRequest {
                page: query.page,
                page_size: query.page_size,
            },
            keyword: query.keyword,
            recent_only: true,
        },
    )
    .await
}

async fn list_all_topups(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TopUpQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    list_topups(
        &state,
        ListTopUpsRequest {
            user_id: None,
            page: PageRequest {
                page: query.page,
                page_size: query.page_size,
            },
            keyword: query.keyword,
            recent_only: false,
        },
    )
    .await
}

async fn list_topups(state: &AppState, req: ListTopUpsRequest) -> Response {
    match state.billing.call(req).await {
        Ok(page) => api_success(page),
        Err(err) => management_error(err),
    }
}

async fn redeem_topup(
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
            key: payload.key,
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
            id: user.id,
            delta: redeemed.quota,
        })
        .await
    {
        Ok(_) => api_success(redeemed.quota),
        Err(err) => {
            if let Err(rollback_err) = state
                .billing
                .call(RollbackRedeemRedemptionRequest {
                    id: redeemed.id,
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

async fn complete_topup(
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
                id: completed.topup.user_id,
                delta: completed.quota,
            })
            .await
        {
            return management_error(err);
        }
    }
    api_success(JsonValue::Null)
}

async fn list_vendors(
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

async fn search_vendors(
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

async fn get_vendor(
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

async fn create_vendor(
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

async fn update_vendor(
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

async fn delete_vendor(
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

async fn list_model_meta(
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

async fn search_model_meta(
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
                page: PageRequest {
                    page: query.page,
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

async fn get_model_meta(
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

async fn create_model_meta(
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

async fn update_model_meta(
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

async fn delete_model_meta(
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

async fn get_missing_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

async fn sync_upstream_preview(
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

async fn sync_upstream_models(
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

fn enrich_model_page(state: &AppState, models: &mut [ModelRecord]) -> Result<(), Response> {
    let management = state.management.current_data().map_err(management_error)?;
    enrich_models(models, &management);
    Ok(())
}

async fn model_list_response(state: &AppState) -> Response {
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

async fn register_user(
    State(state): State<AppState>,
    Json(payload): Json<RegisterPayload>,
) -> Response {
    let _ = (
        &payload.email,
        &payload.verification_code,
        &payload.aff_code,
    );
    let options = state.options.values().unwrap_or_default();
    if !option_bool(&options, "RegisterEnabled", false) {
        return api_error_status(StatusCode::OK, "管理员关闭了新用户注册");
    }
    if !option_bool(&options, "PasswordRegisterEnabled", false) {
        return api_error_status(
            StatusCode::OK,
            "管理员关闭了通过密码进行注册，请使用第三方账户验证的形式进行注册",
        );
    }
    if option_bool(&options, "EmailVerificationEnabled", false) {
        return api_error_status(
            StatusCode::OK,
            "管理员开启了邮箱验证，请输入邮箱地址和验证码",
        );
    }

    let username = payload.username.trim();
    if username.is_empty() {
        return api_error_status(StatusCode::OK, "请求参数有误");
    }
    if username.chars().count() > 20 {
        return api_error_status(
            StatusCode::OK,
            "输入不合法 username length must be at most 20",
        );
    }
    let password_len = payload.password.chars().count();
    if !(8..=20).contains(&password_len) {
        return api_error_status(
            StatusCode::OK,
            "输入不合法 password length must be between 8 and 20",
        );
    }
    let display_name = payload.display_name.trim();
    if display_name.chars().count() > 20 {
        return api_error_status(
            StatusCode::OK,
            "输入不合法 display_name length must be at most 20",
        );
    }

    let username = username.to_string();
    let display_name = if display_name.is_empty() {
        username.clone()
    } else {
        display_name.to_string()
    };
    let now = now_unix();
    let default_token = generate_default_token_enabled(&options).then(|| TokenRecord {
        id: 0,
        snapshot_id: None,
        user_id: 0,
        snapshot_user_id: None,
        key: generate_token_key(),
        status: STATUS_ENABLED,
        name: format!("{username}的初始令牌"),
        created_time: now,
        accessed_time: now,
        expired_time: -1,
        remain_quota: 500_000,
        unlimited_quota: true,
        model_limits_enabled: false,
        model_limits: String::new(),
        allow_ips: None,
        used_quota: 0,
        group: if option_bool(&options, "DefaultUseAutoGroup", false) {
            "auto".to_string()
        } else {
            String::new()
        },
        cross_group_retry: false,
    });
    let user = UserRecord {
        id: 0,
        username,
        password: payload.password,
        access_token: None,
        display_name,
        role: halolake_domain::ROLE_COMMON_USER,
        status: STATUS_ENABLED,
        email: String::new(),
        quota: 0,
        used_quota: 0,
        group: "default".to_string(),
        setting: String::new(),
        remark: String::new(),
        created_at: now,
        last_login_at: 0,
    };

    match state
        .management
        .call(RegisterUserRequest {
            user,
            default_token,
        })
        .await
    {
        Ok(_) => match publish_management_snapshot(&state).await {
            Ok(()) => api_ok(),
            Err(err) => management_error(err),
        },
        Err(ManagementError::Duplicate) => {
            api_error_status(StatusCode::OK, "用户名已存在，或已注销")
        }
        Err(ManagementError::InvalidRequest(_)) => api_error_status(StatusCode::OK, "请求参数有误"),
        Err(err) => management_error(err),
    }
}

async fn login_user(State(state): State<AppState>, Json(req): Json<LoginRequest>) -> Response {
    let user = match state
        .management
        .call(LoginUserRequest {
            username: req.username,
            password: req.password,
        })
        .await
    {
        Ok(user) => user,
        Err(err) => return management_error(err),
    };
    let two_fa_status = match state
        .security
        .call(GetTwoFaStatusRequest { user_id: user.id })
        .await
    {
        Ok(status) => status,
        Err(err) => return security_error(err),
    };
    if two_fa_status.enabled {
        let session_id = match state.sessions.create_pending(user.id) {
            Ok(session_id) => session_id,
            Err(err) => return management_error(err),
        };
        let mut resp = api_success_with_message(
            "需要两步验证",
            json!({
                "require_2fa": true,
            }),
        );
        if let Ok(value) = HeaderValue::from_str(&set_session_cookie(&session_id, &state.session_signer)) {
            resp.headers_mut().insert(SET_COOKIE, value);
        }
        return resp;
    }
    let session_id = match state.sessions.create(user.id) {
        Ok(session_id) => session_id,
        Err(err) => return management_error(err),
    };
    let mut resp = api_success(login_payload(&user));
    if let Ok(value) = HeaderValue::from_str(&set_session_cookie(&session_id, &state.session_signer)) {
        resp.headers_mut().insert(SET_COOKIE, value);
    }
    resp
}

async fn logout_user(State(state): State<AppState>, headers: HeaderMap) -> Response {
    state.sessions.remove_from_headers(&headers, &state.session_signer);
    let mut resp = api_ok();
    if let Ok(value) = HeaderValue::from_str(&clear_session_cookie()) {
        resp.headers_mut().insert(SET_COOKIE, value);
    }
    resp
}

async fn login_2fa(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<TwoFaCodePayload>,
) -> Response {
    let user_id = match state.sessions.pending_user_id_from_headers(&headers, &state.session_signer) {
        Ok(Some(user_id)) => user_id,
        Ok(None) => return api_error_status(StatusCode::OK, "会话已过期，请重新登录"),
        Err(err) => return management_error(err),
    };
    match state
        .security
        .call(UniversalVerifyRequest {
            user_id,
            method: VerificationMethod::TwoFa,
            code: Some(payload.code),
            session_id: session_id_from_headers(&headers, &state.session_signer).map(str::to_string),
        })
        .await
    {
        Ok(_) => {}
        Err(err) => return security_error(err),
    }
    let user = match state.management.call(GetUserRequest { id: user_id }).await {
        Ok(user) => user,
        Err(err) => return management_error(err),
    };
    let session_id = match state.sessions.promote_pending_from_headers(&headers, &state.session_signer) {
        Ok(Some(session_id)) => session_id,
        Ok(None) => return api_error_status(StatusCode::OK, "会话已过期，请重新登录"),
        Err(err) => return management_error(err),
    };
    let mut resp = api_success(login_payload(&user));
    if let Ok(value) = HeaderValue::from_str(&set_session_cookie(&session_id, &state.session_signer)) {
        resp.headers_mut().insert(SET_COOKIE, value);
    }
    resp
}

async fn universal_verify(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<UniversalVerifyPayload>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .security
        .call(UniversalVerifyRequest {
            user_id: user.id,
            method: payload.method,
            code: payload.code,
            session_id: session_id_from_headers(&headers, &state.session_signer).map(str::to_string),
        })
        .await
    {
        Ok(status) => {
            // Persist step-up verification on the login session so subsequent
            // sensitive endpoints (channel key reveal) can enforce it.
            if let Some(session_id) = session_id_from_headers(&headers, &state.session_signer) {
                let _ = state.sessions.mark_secure_verified(session_id);
            }
            api_success_with_message("验证成功", status)
        }
        Err(err) => security_error(err),
    }
}

async fn two_fa_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .security
        .call(GetTwoFaStatusRequest { user_id: user.id })
        .await
    {
        Ok(status) => api_success(status),
        Err(err) => security_error(err),
    }
}

async fn setup_two_fa(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .security
        .call(StartTwoFaSetupRequest {
            user_id: user.id,
            username: user.username,
            issuer: state.system_name.to_string(),
        })
        .await
    {
        Ok(setup) => api_success_with_message(
            "2FA设置初始化成功，请使用认证器扫描二维码并输入验证码完成设置",
            setup,
        ),
        Err(err) => security_error(err),
    }
}

async fn enable_two_fa(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<TwoFaCodePayload>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .security
        .call(EnableTwoFaRequest {
            user_id: user.id,
            code: payload.code,
        })
        .await
    {
        Ok(()) => api_ok_message("两步验证启用成功"),
        Err(err) => security_error(err),
    }
}

async fn disable_two_fa(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<TwoFaCodePayload>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .security
        .call(DisableTwoFaRequest {
            user_id: user.id,
            code: payload.code,
        })
        .await
    {
        Ok(()) => api_ok_message("两步验证已禁用"),
        Err(err) => security_error(err),
    }
}

async fn regenerate_two_fa_backup_codes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<TwoFaCodePayload>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .security
        .call(RegenerateTwoFaBackupCodesRequest {
            user_id: user.id,
            code: payload.code,
        })
        .await
    {
        Ok(backup_codes) => api_success_with_message(
            "备用码重新生成成功",
            json!({ "backup_codes": backup_codes }),
        ),
        Err(err) => security_error(err),
    }
}

async fn passkey_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .security
        .call(GetPasskeyStatusRequest { user_id: user.id })
        .await
    {
        Ok(status) => api_success(status),
        Err(err) => security_error(err),
    }
}

async fn passkey_register_begin(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    passkey_user_flow(&state, &headers, &uri, PasskeyFlow::RegisterBegin, None).await
}

async fn passkey_register_finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<JsonValue>,
) -> Response {
    passkey_user_flow(
        &state,
        &headers,
        &uri,
        PasskeyFlow::RegisterFinish,
        Some(payload),
    )
    .await
}

async fn passkey_verify_begin(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    passkey_user_flow(&state, &headers, &uri, PasskeyFlow::VerifyBegin, None).await
}

async fn passkey_verify_finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<JsonValue>,
) -> Response {
    passkey_user_flow(
        &state,
        &headers,
        &uri,
        PasskeyFlow::VerifyFinish,
        Some(payload),
    )
    .await
}

async fn passkey_login_begin(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (session_id, set_cookie) = match state
        .sessions
        .passkey_session_id_from_headers(&headers, true, &state.session_signer)
    {
        Ok(Some(value)) => value,
        Ok(None) => return api_error_status(StatusCode::OK, "Passkey 会话不存在或已过期"),
        Err(err) => return management_error(err),
    };
    match state
        .security
        .call(PasskeyFlowRequest {
            user: None,
            flow: PasskeyFlow::LoginBegin,
            session_id: session_id.clone(),
            request: passkey_request_context(&headers, &uri),
            payload: None,
        })
        .await
    {
        Ok(PasskeyFlowResponse::Begin(begin)) => {
            let mut resp = api_success(begin);
            if set_cookie {
                insert_session_cookie(&mut resp, &session_id, &state.session_signer);
            }
            resp
        }
        Ok(PasskeyFlowResponse::Finished { .. }) => {
            api_error_status(StatusCode::OK, "Passkey 登录状态异常")
        }
        Err(err) => security_error(err),
    }
}

async fn passkey_login_finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<JsonValue>,
) -> Response {
    let (session_id, _) = match state
        .sessions
        .passkey_session_id_from_headers(&headers, false, &state.session_signer)
    {
        Ok(Some(value)) => value,
        Ok(None) => return api_error_status(StatusCode::OK, "Passkey 会话不存在或已过期"),
        Err(err) => return management_error(err),
    };
    match state
        .security
        .call(PasskeyFlowRequest {
            user: None,
            flow: PasskeyFlow::LoginFinish,
            session_id,
            request: passkey_request_context(&headers, &uri),
            payload: Some(payload),
        })
        .await
    {
        Ok(PasskeyFlowResponse::Finished {
            user_id: Some(user_id),
        }) => {
            let user = match state.management.call(GetUserRequest { id: user_id }).await {
                Ok(user) => user,
                Err(err) => return management_error(err),
            };
            if user.status != STATUS_ENABLED {
                return api_error_status(StatusCode::OK, "该用户已被禁用");
            }
            let session_id = match state.sessions.create(user.id) {
                Ok(session_id) => session_id,
                Err(err) => return management_error(err),
            };
            let mut resp = api_success(login_payload(&user));
            insert_session_cookie(&mut resp, &session_id, &state.session_signer);
            resp
        }
        Ok(_) => api_error_status(StatusCode::OK, "Passkey 登录状态异常"),
        Err(err) => security_error(err),
    }
}

async fn delete_passkey(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .security
        .call(DeletePasskeyRequest { user_id: user.id })
        .await
    {
        Ok(()) => api_ok_message("Passkey 已解绑"),
        Err(err) => security_error(err),
    }
}

async fn admin_two_fa_stats(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let total_users = match state.management.current_data() {
        Ok(data) => data.users.len(),
        Err(err) => return management_error(err),
    };
    match state
        .security
        .call(AdminTwoFaStatsRequest { total_users })
        .await
    {
        Ok(stats) => api_success(stats),
        Err(err) => security_error(err),
    }
}

async fn admin_reset_passkey(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let target = match state.management.call(GetUserRequest { id }).await {
        Ok(user) => user,
        Err(err) => return management_error(err),
    };
    match state
        .security
        .call(AdminResetPasskeyRequest {
            actor_role: actor.role,
            target_role: target.role,
            target_user_id: target.id,
        })
        .await
    {
        Ok(()) => api_ok_message("Passkey 已重置"),
        Err(err) => security_error(err),
    }
}

async fn admin_disable_two_fa(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let target = match state.management.call(GetUserRequest { id }).await {
        Ok(user) => user,
        Err(err) => return management_error(err),
    };
    match state
        .security
        .call(AdminDisableTwoFaRequest {
            actor_role: actor.role,
            target_role: target.role,
            target_user_id: target.id,
        })
        .await
    {
        Ok(()) => api_ok_message("用户2FA已被强制禁用"),
        Err(err) => security_error(err),
    }
}

async fn passkey_user_flow(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
    flow: PasskeyFlow,
    payload: Option<JsonValue>,
) -> Response {
    let user = match current_user(state, headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let (session_id, set_cookie) = match state.sessions.passkey_session_id_from_headers(
        headers,
        matches!(flow, PasskeyFlow::RegisterBegin | PasskeyFlow::VerifyBegin),
        &state.session_signer,
    ) {
        Ok(Some(value)) => value,
        Ok(None) => return api_error_status(StatusCode::OK, "Passkey 会话不存在或已过期"),
        Err(err) => return management_error(err),
    };
    match state
        .security
        .call(PasskeyFlowRequest {
            user: Some(PasskeyUser {
                id: user.id,
                username: user.username,
                display_name: user.display_name,
            }),
            flow,
            session_id: session_id.clone(),
            request: passkey_request_context(headers, uri),
            payload,
        })
        .await
    {
        Ok(PasskeyFlowResponse::Begin(begin)) => {
            let mut resp = api_success(begin);
            if set_cookie {
                insert_session_cookie(&mut resp, &session_id, &state.session_signer);
            }
            resp
        }
        Ok(PasskeyFlowResponse::Finished { .. }) => match flow {
            PasskeyFlow::RegisterFinish => api_ok_message("Passkey 注册成功"),
            PasskeyFlow::VerifyFinish => api_ok_message("Passkey 验证成功"),
            _ => api_ok(),
        },
        Err(err) => security_error(err),
    }
}

async fn get_self(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    api_success(self_payload(&user))
}

async fn get_checkin_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CheckinQuery>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let options = state.options.values().unwrap_or_default();
    let setting = checkin_setting(&options);
    if !setting.enabled {
        return api_error_status(StatusCode::OK, "签到功能未启用");
    }
    let today = current_utc_date();
    let month = if query.month.trim().is_empty() {
        today[..7].to_string()
    } else {
        query.month
    };
    match state
        .checkins
        .call(GetCheckinStatsRequest {
            user_id: user.id,
            month,
            today,
        })
        .await
    {
        Ok(stats) => api_success(json!({
            "enabled": setting.enabled,
            "min_quota": setting.min_quota,
            "max_quota": setting.max_quota,
            "stats": stats,
        })),
        Err(err) => management_error(err),
    }
}

async fn do_checkin(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let options = state.options.values().unwrap_or_default();
    let setting = checkin_setting(&options);
    if !setting.enabled {
        return api_error_status(StatusCode::OK, "签到功能未启用");
    }
    let quota_awarded = checkin_award_quota(setting.min_quota, setting.max_quota);
    let checkin_date = current_utc_date();
    let created_at = now_unix();
    match state
        .checkins
        .call(CreateCheckinRequest {
            user_id: user.id,
            checkin_date: checkin_date.clone(),
            quota_awarded,
            created_at,
        })
        .await
    {
        Ok(record) => match state
            .management
            .call(AdjustUserQuotaRequest {
                id: user.id,
                delta: quota_awarded,
            })
            .await
        {
            Ok(_) => api_success_with_message(
                "签到成功",
                json!({
                    "quota_awarded": record.quota_awarded,
                    "checkin_date": record.checkin_date,
                }),
            ),
            Err(err) => management_error(err),
        },
        Err(ManagementError::Duplicate) => api_error_status(StatusCode::OK, "今日已签到"),
        Err(err) => management_error(err),
    }
}

async fn update_self(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(patch): Json<UserRecord>,
) -> Response {
    let current = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    // Build the update from the current record and overlay only the fields a
    // user is allowed to change on themselves. Never trust quota/used_quota/
    // group/role/status/username from a self-service payload — accepting those
    // let any common user grant themselves unlimited quota or a privileged
    // group. `actor_role` stays ROLE_ROOT_USER only because the record is now
    // fully server-constructed and cannot carry an escalation.
    let mut user = current.clone();
    user.password = patch.password;
    if !patch.display_name.is_empty() {
        user.display_name = patch.display_name;
    }
    if !patch.email.is_empty() {
        user.email = patch.email;
    }
    if !patch.setting.is_empty() {
        user.setting = patch.setting;
    }
    if !patch.remark.is_empty() {
        user.remark = patch.remark;
    }
    match state
        .management
        .call(UpdateUserRequest {
            user,
            actor_role: ROLE_ROOT_USER,
        })
        .await
    {
        Ok(_) => api_ok(),
        Err(err) => management_error(err),
    }
}

async fn delete_self(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let current = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .management
        .call(DeleteUserRequest {
            id: current.id,
            actor_role: ROLE_ROOT_USER,
        })
        .await
    {
        Ok(()) => {
            state.sessions.remove_from_headers(&headers, &state.session_signer);
            match publish_management_snapshot(&state).await {
                Ok(()) => api_ok(),
                Err(err) => management_error(err),
            }
        }
        Err(err) => management_error(err),
    }
}

async fn user_groups(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let group = current_user(&state, &headers)
        .await
        .map(|user| user.group)
        .unwrap_or_else(|_| "default".to_string());
    api_success(json!({
        group.clone(): {
            "ratio": 1,
            "desc": group,
        }
    }))
}

async fn user_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = current_user(&state, &headers).await {
        return resp;
    }
    model_list_response(&state).await
}

async fn generate_access_token(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let access_token = Uuid::new_v4().simple().to_string();
    match state
        .management
        .call(UpdateUserAccessTokenRequest {
            id: user.id,
            access_token,
        })
        .await
    {
        Ok(access_token) => api_success(access_token),
        Err(err) => management_error(err),
    }
}

async fn list_users(
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
        .call(ListUsersRequest { page: query.into() })
        .await
    {
        Ok(mut page) => {
            page.items = page.items.into_iter().map(UserRecord::sanitized).collect();
            api_success(page)
        }
        Err(err) => management_error(err),
    }
}

async fn search_users(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UserSearchQuery>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .management
        .call(SearchUsersRequest {
            search: SearchRequest {
                page: PageRequest {
                    page: query.page,
                    page_size: query.page_size,
                },
                keyword: query.keyword,
            },
            group: query.group,
            role: query.role,
            status: query.status,
        })
        .await
    {
        Ok(mut page) => {
            page.items = page.items.into_iter().map(UserRecord::sanitized).collect();
            api_success(page)
        }
        Err(err) => management_error(err),
    }
}

async fn get_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.management.call(GetUserRequest { id }).await {
        Ok(user) => api_success(user.sanitized()),
        Err(err) => management_error(err),
    }
}

async fn create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut user): Json<UserRecord>,
) -> Response {
    let actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    fill_new_user_defaults(&mut user);
    match state
        .management
        .call(CreateUserRequest {
            user,
            actor_role: actor.role,
        })
        .await
    {
        Ok(_) => api_ok(),
        Err(err) => management_error(err),
    }
}

async fn update_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(user): Json<UserRecord>,
) -> Response {
    let actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .management
        .call(UpdateUserRequest {
            user,
            actor_role: actor.role,
        })
        .await
    {
        Ok(_) => api_ok(),
        Err(err) => management_error(err),
    }
}

async fn delete_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .management
        .call(DeleteUserRequest {
            id,
            actor_role: actor.role,
        })
        .await
    {
        Ok(()) => match publish_management_snapshot(&state).await {
            Ok(()) => api_ok(),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

async fn manage_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UserManagePayload>,
) -> Response {
    let actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .management
        .call(ManageUserRequest {
            id: req.id,
            action: req.action,
            value: req.value,
            mode: req.mode,
            actor_role: actor.role,
        })
        .await
    {
        Ok(user) => match publish_management_snapshot(&state).await {
            Ok(()) => api_success(json!({
                "role": user.role,
                "status": user.status,
            })),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

async fn list_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .management
        .call(ListTokensRequest {
            user_id: Some(user.id),
            page: query.into(),
        })
        .await
    {
        Ok(page) => api_success(page),
        Err(err) => management_error(err),
    }
}

async fn search_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TokenSearchQuery>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .management
        .call(SearchTokensRequest {
            user_id: Some(user.id),
            search: SearchRequest {
                page: PageRequest {
                    page: query.page,
                    page_size: query.page_size,
                },
                keyword: query.keyword,
            },
            token: query.token,
        })
        .await
    {
        Ok(page) => api_success(page),
        Err(err) => management_error(err),
    }
}

async fn get_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .management
        .call(GetTokenRequest {
            id,
            user_id: Some(user.id),
        })
        .await
    {
        Ok(token) => api_success(token),
        Err(err) => management_error(err),
    }
}

async fn reveal_token_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    // Token material is as sensitive as channel keys; require the same step-up.
    if let Err(resp) = require_secure_verification(&state, &headers) {
        return resp;
    }
    match state
        .management
        .call(RevealTokenKeyRequest {
            id,
            user_id: Some(user.id),
        })
        .await
    {
        Ok(key) => api_success(json!({ "key": key.key })),
        Err(err) => management_error(err),
    }
}

async fn reveal_token_keys_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<BatchIds>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_secure_verification(&state, &headers) {
        return resp;
    }
    if req.ids.len() > 100 {
        return api_error_status(StatusCode::OK, "too many ids (max 100)");
    }
    // new-api returns `{ keys: { "<id>": "<key>", ... } }` for the bulk copy UI.
    let mut keys = serde_json::Map::new();
    for id in req.ids {
        match state
            .management
            .call(RevealTokenKeyRequest {
                id,
                user_id: Some(user.id),
            })
            .await
        {
            Ok(key) => {
                keys.insert(id.to_string(), json!(key.key));
            }
            Err(err) => return management_error(err),
        }
    }
    api_success(json!({ "keys": keys }))
}

async fn create_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut token): Json<TokenRecord>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    // Self-service tokens are rebuilt server-side. Never trust ownership,
    // used_quota, or an attacker-chosen key material from the client.
    sanitize_self_service_token_create(&mut token, user.id);
    fill_new_token_defaults(&mut token, user.id);
    match state.management.call(CreateTokenRequest { token }).await {
        Ok(_) => match publish_management_snapshot(&state).await {
            Ok(()) => api_ok(),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

async fn update_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ModelUpdateQuery>,
    Json(patch): Json<TokenRecord>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    // Load the stored token first and overlay only self-service fields. Never
    // trust a full client rewrite of key/user_id/used_quota/group.
    let current = match state
        .management
        .call(GetTokenRequest {
            id: patch.id,
            user_id: Some(user.id),
        })
        .await
    {
        Ok(token) => token,
        Err(err) => return management_error(err),
    };
    // Frontend enable/disable sends `?status_only=true` with `{id, status}` only.
    // Defaults on TokenRecord would otherwise wipe remain_quota / expired_time.
    let mut token = current;
    token.user_id = user.id;
    token.key.clear();
    if query.status_only {
        if patch.status == 0 || patch.status == STATUS_ENABLED {
            token.status = patch.status;
        }
    } else {
        if !patch.name.trim().is_empty() {
            token.name = patch.name.trim().to_string();
        }
        token.expired_time = patch.expired_time;
        token.remain_quota = patch.remain_quota.max(0);
        token.unlimited_quota = patch.unlimited_quota;
        token.model_limits_enabled = patch.model_limits_enabled;
        token.model_limits = patch.model_limits;
        token.allow_ips = patch.allow_ips;
        if patch.status == 0 || patch.status == STATUS_ENABLED {
            token.status = patch.status;
        }
        // group / cross_group_retry stay server-owned (inherit user group).
        token.group.clear();
        token.cross_group_retry = false;
    }
    match state
        .management
        .call(UpdateTokenRequest {
            token,
            user_id: Some(user.id),
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

async fn delete_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .management
        .call(DeleteTokenRequest {
            id,
            user_id: Some(user.id),
        })
        .await
    {
        Ok(()) => match publish_management_snapshot(&state).await {
            Ok(()) => api_ok(),
            Err(err) => management_error(err),
        },
        Err(err) => management_error(err),
    }
}

async fn delete_token_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<BatchIds>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    for id in req.ids {
        if let Err(err) = state
            .management
            .call(DeleteTokenRequest {
                id,
                user_id: Some(user.id),
            })
            .await
        {
            return management_error(err);
        }
    }
    match publish_management_snapshot(&state).await {
        Ok(()) => api_ok(),
        Err(err) => management_error(err),
    }
}

async fn get_token_usage(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let token = match token_from_read_only_auth(&state, &headers) {
        Ok(token) => token,
        Err(resp) => return resp,
    };
    let expired_at = if token.expired_time == -1 {
        0
    } else {
        token.expired_time
    };
    let model_limits = token
        .allowed_models()
        .into_iter()
        .map(|model| (model, json!(true)))
        .collect::<serde_json::Map<_, _>>();
    api_success(json!({
        "object": "token_usage",
        "name": token.name,
        "total_granted": token.remain_quota + token.used_quota,
        "total_used": token.used_quota,
        "total_available": token.remain_quota,
        "unlimited_quota": token.unlimited_quota,
        "model_limits": model_limits,
        "model_limits_enabled": token.model_limits_enabled,
        "expires_at": expired_at,
    }))
}

fn pricing_records(
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

fn pricing_usable_groups(options: &BTreeMap<String, String>, requested_group: &str) -> JsonValue {
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

fn pricing_auto_groups(options: &BTreeMap<String, String>, requested_group: &str) -> Vec<String> {
    let usable = user_usable_groups_for_options(options, requested_group);
    parse_string_vec(options.get("AutoGroups"))
        .into_iter()
        .filter(|group| usable.contains_key(group))
        .collect()
}

fn user_usable_groups_for_options(
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

fn enabled_pricing_models(management: &ManagementData) -> Vec<String> {
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

fn catalog_model_for_name<'a>(
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

fn pricing_groups_for_model(management: &ManagementData, model_name: &str) -> Vec<String> {
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

fn insert_optional_ratio(
    record: &mut serde_json::Map<String, JsonValue>,
    key: &str,
    value: Option<&f64>,
) {
    if let Some(value) = value {
        record.insert(key.to_string(), json!(value));
    }
}

#[derive(Debug, Clone, Copy)]
struct RankingPeriodConfig {
    seconds: i64,
    bucket_size: i64,
}

fn ranking_period_config(period: &str) -> Result<RankingPeriodConfig, &'static str> {
    match period {
        "" | "week" => Ok(RankingPeriodConfig {
            seconds: 7 * 24 * 3600,
            bucket_size: 24 * 3600,
        }),
        "today" => Ok(RankingPeriodConfig {
            seconds: 24 * 3600,
            bucket_size: 3600,
        }),
        "month" => Ok(RankingPeriodConfig {
            seconds: 30 * 24 * 3600,
            bucket_size: 24 * 3600,
        }),
        "year" => Ok(RankingPeriodConfig {
            seconds: 365 * 24 * 3600,
            bucket_size: 7 * 24 * 3600,
        }),
        _ => Err("invalid ranking period"),
    }
}

fn rankings_snapshot(
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

fn usage_totals(events: &[UsageEvent], start: i64, end: i64) -> Vec<(String, u64)> {
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

fn rank_map(totals: &[(String, u64)]) -> BTreeMap<String, usize> {
    totals
        .iter()
        .enumerate()
        .map(|(idx, (model, _))| (model.clone(), idx + 1))
        .collect()
}

fn token_map(totals: &[(String, u64)]) -> BTreeMap<String, u64> {
    totals.iter().cloned().collect()
}

fn ranking_vendors(
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

fn ranking_model_history(
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

fn ranking_vendor_history(
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

fn usage_buckets(
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

fn model_vendor(catalog: &CatalogData, model_name: &str) -> (String, String) {
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

fn perf_metrics_for_model(events: &[UsageEvent], query: &PerfMetricsQuery) -> JsonValue {
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

fn perf_metrics_summary(events: &[UsageEvent], hours: i64) -> JsonValue {
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

fn perf_group_result(group: String, events: Vec<&UsageEvent>, start: i64, end: i64) -> JsonValue {
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

fn avg_latency(events: &[&UsageEvent]) -> u64 {
    if events.is_empty() {
        return 0;
    }
    events.iter().map(|event| event.latency_ms).sum::<u64>() / events.len() as u64
}

fn success_rate(events: &[&UsageEvent]) -> f64 {
    if events.is_empty() {
        return 0.0;
    }
    let successes = events
        .iter()
        .filter(|event| event.status == UsageStatus::Success)
        .count();
    successes as f64 / events.len() as f64 * 100.0
}

fn avg_tps(events: &[&UsageEvent]) -> f64 {
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

fn clamp_perf_hours(hours: i64) -> i64 {
    if hours <= 0 { 24 } else { hours.min(24 * 30) }
}

fn percent(value: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        value as f64 / total as f64 * 100.0
    }
}

fn growth_pct(current: u64, previous: u64) -> f64 {
    if previous == 0 {
        if current == 0 { 0.0 } else { 100.0 }
    } else {
        (current as f64 - previous as f64) / previous as f64 * 100.0
    }
}

fn bucket_count(start: i64, end: i64, bucket_size: i64) -> i64 {
    if bucket_size <= 0 || end <= start {
        return 0;
    }
    ((end - start) / bucket_size).saturating_add(1)
}

fn default_options(config: &ControlApiConfig) -> BTreeMap<String, String> {
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

fn toml_value_to_option_string(value: &toml::Value) -> String {
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

fn validate_option_update(state: &AppState, key: &str, value: &str) -> Result<(), Response> {
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

fn json_object_option_key(key: &str) -> bool {
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

fn validate_json_object_option(key: &str, value: &str) -> Result<(), Response> {
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

fn is_sensitive_option_key(key: &str) -> bool {
    key.ends_with("Token")
        || key.ends_with("Secret")
        || key.ends_with("Key")
        || key.ends_with("secret")
        || key.ends_with("api_key")
}

fn build_completion_ratio_meta(options: &BTreeMap<String, String>) -> String {
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

fn collect_json_object_keys(value: Option<&String>, out: &mut BTreeSet<String>) {
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

fn parse_number_map(value: Option<&String>) -> BTreeMap<String, f64> {
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

fn parse_nested_number_map(value: Option<&String>) -> BTreeMap<String, BTreeMap<String, f64>> {
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

fn parse_string_vec(value: Option<&String>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(value).unwrap_or_default()
}

fn parse_string_map(value: Option<&String>) -> HashMap<String, String> {
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

fn parse_nested_string_map(value: Option<&String>) -> HashMap<String, HashMap<String, String>> {
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

fn usage_pricing_from_options(options: &BTreeMap<String, String>) -> UsagePricing {
    UsagePricing {
        quota_per_unit: option_f64(options, "QuotaPerUnit", 500_000.0),
        model_ratio: parse_number_map(options.get("ModelRatio")),
        model_price: parse_number_map(options.get("ModelPrice")),
        completion_ratio: parse_number_map(options.get("CompletionRatio")),
        cache_ratio: parse_number_map(options.get("CacheRatio")),
        cache_creation_ratio: parse_number_map(options.get("CreateCacheRatio")),
        image_ratio: parse_number_map(options.get("ImageRatio")),
        audio_ratio: parse_number_map(options.get("AudioRatio")),
        group_ratio: parse_number_map(options.get("GroupRatio")),
        group_group_ratio: parse_nested_number_map(options.get("GroupGroupRatio")),
    }
}

fn group_routing_config_from_options(options: &BTreeMap<String, String>) -> GroupRoutingConfig {
    let group_ratio = parse_number_map(options.get("GroupRatio"));
    let group_special_usable_groups = options
        .get("GroupSpecialUsableGroup")
        .or_else(|| options.get("group_ratio_setting.group_special_usable_group"));
    GroupRoutingConfig {
        auto_groups: parse_string_vec(options.get("AutoGroups")),
        user_usable_groups: parse_string_map(options.get("UserUsableGroups")),
        group_special_usable_groups: parse_nested_string_map(group_special_usable_groups),
        known_groups: group_ratio.into_keys().collect(),
    }
}

fn checkin_setting(options: &BTreeMap<String, String>) -> CheckinSetting {
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

pub(crate) fn option_str<'a>(options: &'a BTreeMap<String, String>, key: &str, default: &'a str) -> &'a str {
    options.get(key).map(String::as_str).unwrap_or(default)
}

pub(crate) fn option_bool(options: &BTreeMap<String, String>, key: &str, default: bool) -> bool {
    options
        .get(key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn passkey_option_alias(key: &str) -> Option<&'static str> {
    match key {
        "passkey.enabled" => Some("PasskeyLoginEnabled"),
        "PasskeyLoginEnabled" => Some("passkey.enabled"),
        _ => None,
    }
}

fn generate_default_token_enabled(options: &BTreeMap<String, String>) -> bool {
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

fn channel_affinity_config_from_options(
    options: &BTreeMap<String, String>,
) -> ChannelAffinityConfig {
    ChannelAffinityConfig {
        enabled: option_bool(options, "channel_affinity_setting.enabled", true),
        switch_on_success: option_bool(options, "channel_affinity_setting.switch_on_success", true),
        keep_on_channel_disabled: option_bool(
            options,
            "channel_affinity_setting.keep_on_channel_disabled",
            false,
        ),
        max_entries: options
            .get("channel_affinity_setting.max_entries")
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(100_000),
        default_ttl_seconds: options
            .get("channel_affinity_setting.default_ttl_seconds")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(3_600),
        rules: options
            .get("channel_affinity_setting.rules")
            .and_then(|value| serde_json::from_str::<Vec<ChannelAffinityRule>>(value).ok())
            .unwrap_or_default(),
    }
}

fn option_json(options: &BTreeMap<String, String>, key: &str, default: JsonValue) -> JsonValue {
    options
        .get(key)
        .and_then(|value| serde_json::from_str::<JsonValue>(value).ok())
        .unwrap_or(default)
}

fn payment_compliance_terms_version() -> &'static str {
    "v1"
}

fn checkin_award_quota(min_quota: i64, max_quota: i64) -> i64 {
    let min_quota = min_quota.max(0);
    let max_quota = max_quota.max(min_quota);
    let span = max_quota.saturating_sub(min_quota).saturating_add(1);
    if span <= 1 {
        return min_quota;
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default();
    min_quota.saturating_add((seed % span as u64) as i64)
}

fn current_utc_date() -> String {
    date_from_unix_days(now_unix().div_euclid(86_400))
}

fn date_from_unix_days(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096).div_euclid(365);
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2).div_euclid(153);
    let day = doy - (153 * mp + 2).div_euclid(5) + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(month <= 2);
    (year as i32, month as u32, day as u32)
}

fn payment_compliance_confirmed(options: &BTreeMap<String, String>) -> bool {
    option_bool(options, "payment_setting.compliance_confirmed", false)
        && option_str(options, "payment_setting.compliance_terms_version", "")
            == payment_compliance_terms_version()
}

fn require_payment_compliance(state: &AppState) -> Result<(), Response> {
    match state.options.values() {
        Ok(options) if payment_compliance_confirmed(&options) => Ok(()),
        Ok(_) => Err(api_error_status(
            StatusCode::OK,
            "payment.compliance_required",
        )),
        Err(err) => Err(management_error(err)),
    }
}

fn topup_info_payload(options: &BTreeMap<String, String>) -> JsonValue {
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

fn push_non_empty_group(groups: &mut BTreeSet<String>, group: String) {
    let group = group.trim();
    if !group.is_empty() {
        groups.insert(group.to_string());
    }
}

fn passkey_request_context(headers: &HeaderMap, uri: &Uri) -> PasskeyRequestContext {
    PasskeyRequestContext {
        host: header_string(headers, HOST.as_str()),
        forwarded_proto: header_string(headers, "x-forwarded-proto")
            .or_else(|| header_string(headers, "x-forwarded-protocol")),
        uri_scheme: uri.scheme_str().map(str::to_string),
    }
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn insert_session_cookie(resp: &mut Response, session_id: &str, signer: &SessionSigner) {
    if let Ok(value) = HeaderValue::from_str(&set_session_cookie(session_id, signer)) {
        resp.headers_mut().insert(SET_COOKIE, value);
    }
}

pub(crate) async fn publish_management_snapshot(state: &AppState) -> Result<(), ManagementError> {
    publish_enriched_management_snapshot(
        &state.management,
        &state.options,
        &state.snapshots,
        &state.proxies,
    )
    .await
}

pub(crate) async fn publish_enriched_management_snapshot(
    management: &ManagementStore,
    options: &OptionStore,
    snapshots: &MemorySnapshotBus,
    proxies: &ProxyStore,
) -> Result<(), ManagementError> {
    // Do NOT bump here. Write paths already advanced the version via
    // `mutate()`. Options-only changes must call `bump_version()` before this
    // helper so the gateway does not treat the republish as NotModified.
    let data = management.current_data()?;
    let mut snapshot = data.build_snapshot()?;
    apply_channel_proxies(&mut snapshot, &data, proxies);
    let option_values = options.values()?;
    snapshot.channel_affinity = channel_affinity_config_from_options(&option_values);
    snapshot.group_routing = group_routing_config_from_options(&option_values);
    snapshots
        .call(PublishSnapshotRequest { snapshot })
        .await
        .map_err(ManagementError::Snapshot)?;
    Ok(())
}

fn apply_channel_proxies(
    snapshot: &mut GatewaySnapshot,
    management: &ManagementData,
    proxies: &ProxyStore,
) {
    for ch in &mut snapshot.channels {
        let Some(rec) = management.channels.iter().find(|c| {
            c.snapshot_id
                .as_deref()
                .unwrap_or(&c.id.to_string())
                == ch.id.as_str()
                || c.id.to_string() == ch.id
        }) else {
            continue;
        };
        if let Some(pid) = rec.proxy_id {
            if let Some(url) = proxies.resolve_url(Some(pid)) {
                ch.proxy = Some(url);
            }
        }
    }
}

fn fill_new_token_defaults(token: &mut TokenRecord, user_id: u64) {
    // Callers must already have pinned token.user_id to the authenticated user
    // for self-service creation; this only backfills when left unset.
    if token.user_id == 0 {
        token.user_id = user_id;
    }
    if token.key.is_empty() {
        token.key = generate_token_key();
    }
    if token.name.is_empty() {
        token.name = "default".to_string();
    }
    let now = now_unix();
    if token.created_time == 0 {
        token.created_time = now;
    }
    if token.accessed_time == 0 {
        token.accessed_time = now;
    }
}

/// Fields a self-service caller may set on create. Everything else is server-owned.
fn sanitize_self_service_token_create(token: &mut TokenRecord, user_id: u64) {
    let name = token.name.trim().to_string();
    let expired_time = token.expired_time;
    let remain_quota = token.remain_quota.max(0);
    let unlimited_quota = token.unlimited_quota;
    let model_limits_enabled = token.model_limits_enabled;
    let model_limits = token.model_limits.clone();
    let allow_ips = token.allow_ips.clone();
    let status = if token.status == 0 {
        // 0 is the common "disabled" value in the copied frontend.
        0
    } else {
        STATUS_ENABLED
    };

    *token = TokenRecord {
        id: 0,
        snapshot_id: None,
        user_id,
        snapshot_user_id: None,
        key: String::new(),
        status,
        name,
        created_time: 0,
        accessed_time: 0,
        expired_time,
        remain_quota,
        unlimited_quota,
        model_limits_enabled,
        model_limits,
        allow_ips,
        used_quota: 0,
        // Empty inherits the user's group in the published snapshot. Self-service
        // callers must not invent a privileged token_group override.
        group: String::new(),
        cross_group_retry: false,
    };
}


fn fill_new_user_defaults(user: &mut UserRecord) {
    if user.display_name.is_empty() {
        user.display_name.clone_from(&user.username);
    }
    if user.group.is_empty() {
        user.group = "default".to_string();
    }
    if user.created_at == 0 {
        user.created_at = now_unix();
    }
}

fn normalize_config_users(users: &mut [UserRecord]) -> Result<()> {
    let mut next_id = users
        .iter()
        .map(|user| user.id)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1);
    for user in users {
        if user.id == 0 {
            user.id = next_id;
            next_id = next_id.saturating_add(1);
        } else {
            next_id = next_id.max(user.id.saturating_add(1));
        }
        if user.display_name.is_empty() {
            user.display_name.clone_from(&user.username);
        }
        if user.group.is_empty() {
            user.group = "default".to_string();
        }
        if user.created_at == 0 {
            user.created_at = now_unix();
        }
        ensure_user_password_hashed(user)?;
    }
    Ok(())
}

fn generate_token_key() -> String {
    let mut key = String::with_capacity(48);
    while key.len() < 48 {
        key.push_str(&Uuid::new_v4().simple().to_string());
    }
    key.truncate(48);
    key
}

pub(crate) fn default_page() -> usize {
    1
}

pub(crate) fn default_page_size() -> usize {
    10
}

fn default_ranking_period() -> String {
    "week".to_string()
}

fn default_perf_hours() -> i64 {
    24
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        warn!(?err, "failed to install ctrl-c handler");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        infer_log_storage_backend, infer_main_storage_backend, normalize_mysql_url, StorageBackend,
        StorageConfig, LogStorageBackend, ensure_supported_storage_backend,
    };

    #[test]
    fn infers_main_storage_backend_like_new_api() {
        assert_eq!(
            infer_main_storage_backend("").expect("empty dsn should be sqlite"),
            StorageBackend::Sqlite
        );
        assert_eq!(
            infer_main_storage_backend("local").expect("local dsn should be sqlite"),
            StorageBackend::Sqlite
        );
        assert_eq!(
            infer_main_storage_backend("postgres://user:pass@localhost/db")
                .expect("postgres scheme should be postgres"),
            StorageBackend::Postgres
        );
        assert_eq!(
            infer_main_storage_backend("postgresql://user:pass@localhost/db")
                .expect("postgresql scheme should be postgres"),
            StorageBackend::Postgres
        );
        assert_eq!(
            infer_main_storage_backend("user:pass@tcp(127.0.0.1:3306)/oneapi")
                .expect("plain dsn should be mysql"),
            StorageBackend::MySql
        );

        let err = infer_main_storage_backend("clickhouse://localhost:9000/logs")
            .expect_err("clickhouse should not be accepted as main database");
        assert!(
            err.to_string()
                .contains("SQL_DSN does not support ClickHouse"),
            "{err}"
        );
    }

    #[test]
    fn infers_log_storage_backend_like_new_api() {
        assert_eq!(
            infer_log_storage_backend("local"),
            LogStorageBackend::Sqlite
        );
        assert_eq!(
            infer_log_storage_backend("postgres://user:pass@localhost/db"),
            LogStorageBackend::Postgres
        );
        assert_eq!(
            infer_log_storage_backend("user:pass@tcp(127.0.0.1:3306)/logs"),
            LogStorageBackend::MySql
        );
        assert_eq!(
            infer_log_storage_backend("clickhouse://localhost:9000/logs"),
            LogStorageBackend::ClickHouse
        );
        assert_eq!(
            infer_log_storage_backend("https://localhost:8443/logs"),
            LogStorageBackend::ClickHouse
        );
    }

    #[test]
    fn model_status_only_payload_deserializes_without_model_name() {
        // Frontend enable/disable: PUT /api/models/?status_only=true {id, status}
        let model: crate::catalog::ModelRecord =
            serde_json::from_str(r#"{"id":1,"status":0}"#).expect("status_only body");
        assert_eq!(model.id, 1);
        assert_eq!(model.status, 0);
        assert!(model.model_name.is_empty());
    }

    #[test]
    fn model_update_query_defaults_status_only_false() {
        let query: ModelUpdateQuery = serde_json::from_str("{}").expect("empty");
        assert!(!query.status_only);
        let query: ModelUpdateQuery =
            serde_json::from_str(r#"{"status_only":true}"#).expect("flag");
        assert!(query.status_only);
    }

    #[test]
    fn accepts_postgres_and_mysql_main_backend_and_rejects_clickhouse_log() {
        let storage = StorageConfig {
            backend: StorageBackend::Postgres,
            database_url: Some("postgres://localhost/halolake".into()),
            ..StorageConfig::default()
        };
        ensure_supported_storage_backend(&storage).expect("postgres main store is supported");

        let storage = StorageConfig {
            backend: StorageBackend::MySql,
            database_url: Some("mysql://localhost/halolake".into()),
            ..StorageConfig::default()
        };
        ensure_supported_storage_backend(&storage).expect("mysql main store is supported");

        let storage = StorageConfig {
            log_backend: Some(LogStorageBackend::ClickHouse),
            ..StorageConfig::default()
        };
        let err = ensure_supported_storage_backend(&storage)
            .expect_err("separate clickhouse log storage should not be implemented yet");
        assert!(
            err.to_string()
                .contains("separate log storage is not implemented yet"),
            "{err}"
        );
    }

    #[test]
    fn normalizes_go_style_mysql_dsn() {
        assert_eq!(
            normalize_mysql_url("user:pass@tcp(127.0.0.1:3306)/oneapi"),
            "mysql://user:pass@127.0.0.1:3306/oneapi"
        );
        assert_eq!(
            normalize_mysql_url("mysql://user:pass@127.0.0.1:3306/oneapi"),
            "mysql://user:pass@127.0.0.1:3306/oneapi"
        );
    }
}
