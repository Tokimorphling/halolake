use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use halolake_control_plane::{
    CreateTokenRequest, ManagementData, MemorySnapshotBus, UpdateUserRequest,
    ensure_user_password_hashed,
};
use halolake_domain::{
    PageRequest, PageResult, ROLE_ADMIN_USER, ROLE_ROOT_USER, STATUS_ENABLED, TokenRecord,
    UserRecord,
};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use service_async::Service;
use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};
use tracing::{info, warn};
use uuid::Uuid;

#[cfg(feature = "admin-extras")]
mod admin_extras;
mod api_catalog;
mod api_channel;
mod api_system;
mod api_usage;
mod api_user;
mod api_web;
mod billing;
mod bootstrap_credentials;
mod catalog;
mod channel_affinity;
mod channel_feedback;
mod channel_http;
mod channel_ops;
mod channel_probe;
#[cfg(feature = "admin-extras")]
mod channel_special;
mod channel_task;
mod checkin;
#[cfg(feature = "compat-stubs")]
mod compat;
mod config;
#[cfg(feature = "admin-extras")]
mod control_api_ext;
mod http_auth;
mod http_response;
#[cfg(feature = "admin-extras")]
mod model_sync;
mod options_util;
#[cfg(feature = "admin-extras")]
mod playground;
mod prefill;
mod process_metrics;
mod proxy;
#[cfg(feature = "admin-extras")]
mod proxy_probe;
#[cfg(feature = "admin-extras")]
mod ratio_sync;
mod security;
mod session;
mod snapshot_publish;
mod storage;
mod store_open;
mod system_instance;
mod system_task;
pub(crate) use api_catalog::*;
pub(crate) use api_channel::*;
pub(crate) use api_system::*;
pub(crate) use api_usage::*;
pub(crate) use api_user::*;
pub(crate) use api_web::*;
use billing::BillingStore;
use catalog::{CatalogData, CatalogStore};
use channel_task::{ChannelTaskSchedulerConfig, spawn_channel_task_scheduler};
use checkin::CheckinStore;
pub use config::{
    ControlApiConfig, InternalConfig, LogStorageBackend, ServerConfig, SessionConfig,
    StorageBackend, StorageConfig, SystemConfig, WebConfig,
};
pub(crate) use config::{ensure_supported_storage_backend, storage_backend_name};
pub(crate) use http_auth::{current_user, require_role};
pub(crate) use http_response::{
    api_error_status, api_ok, api_ok_message, api_success, management_error,
};
pub(crate) use options_util::*;
use prefill::PrefillStore;
use proxy::ProxyStore;
use security::{SecurityService, SecurityStore};
use session::{SessionSigner, SessionStore};
pub(crate) use snapshot_publish::{
    publish_enriched_management_snapshot, publish_management_snapshot,
};
use storage::{ManagementStore, OptionStore, UsageStore};
use system_instance::{SystemInstanceStore, spawn_system_instance_reporter};
use system_task::SystemTaskStore;

pub(crate) const INTERNAL_KEY_HEADER: &str = "x-halolake-internal-key";
pub(crate) const SNAPSHOT_VERSION_HEADER: &str = "x-halolake-snapshot-version";
pub(crate) const NEW_API_USER_HEADER: &str = "new-api-user";
pub(crate) const SESSION_COOKIE_NAME: &str = "session";
pub(crate) const MAX_RECENT_TOKEN_LOGS: usize = 1000;
pub(crate) const TOKEN_STATUS_EXPIRED: i32 = 3;
pub(crate) const TOKEN_STATUS_EXHAUSTED: i32 = 4;
pub(crate) const DEFAULT_MODEL_RATIO_JSON: &str = "{}";
pub(crate) const DEFAULT_CHANNEL_AFFINITY_RULES_JSON: &str = r#"[{"name":"codex cli trace","model_regex":["^gpt-.*$"],"path_regex":["/v1/responses"],"key_sources":[{"type":"gjson","path":"prompt_cache_key"}],"value_regex":"","ttl_seconds":0,"param_override_template":{"operations":[{"mode":"pass_headers","value":["Originator","Session_id","User-Agent","X-Codex-Beta-Features","X-Codex-Turn-Metadata"],"keep_origin":true}]},"skip_retry_on_failure":true,"include_using_group":true,"include_rule_name":true},{"name":"claude cli trace","model_regex":["^claude-.*$"],"path_regex":["/v1/messages"],"key_sources":[{"type":"gjson","path":"metadata.user_id"}],"value_regex":"","ttl_seconds":0,"param_override_template":{"operations":[{"mode":"pass_headers","value":["X-Stainless-Arch","X-Stainless-Lang","X-Stainless-Os","X-Stainless-Package-Version","X-Stainless-Retry-Count","X-Stainless-Runtime","X-Stainless-Runtime-Version","X-Stainless-Timeout","User-Agent","X-App","Anthropic-Beta","Anthropic-Dangerous-Direct-Browser-Access","Anthropic-Version"],"keep_origin":true}]},"skip_retry_on_failure":true,"include_using_group":true,"include_rule_name":true}]"#;

#[derive(Debug, Clone, Copy)]
pub(crate) struct EmbeddedAsset {
    pub(crate) path:  &'static str,
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
    pub(crate) snapshots:        MemorySnapshotBus,
    pub(crate) management:       ManagementStore,
    pub(crate) usage_events:     UsageStore,
    pub(crate) catalog:          CatalogStore,
    pub(crate) options:          OptionStore,
    pub(crate) billing:          BillingStore,
    pub(crate) checkins:         CheckinStore,
    pub(crate) prefill:          PrefillStore,
    pub(crate) proxies:          ProxyStore,
    pub(crate) security:         SecurityService,
    pub(crate) system_tasks:     SystemTaskStore,
    pub(crate) system_instances: SystemInstanceStore,
    pub(crate) sessions:         SessionStore,
    pub(crate) session_signer:   SessionSigner,
    pub(crate) web:              WebConfig,
    pub(crate) internal_secret:  Option<Arc<str>>,
    pub(crate) gateway_base_url: Option<String>,
    pub(crate) start_time_unix:  i64,
    pub(crate) system_name:      Arc<str>,
    pub(crate) storage_backend:  StorageBackend,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(crate) struct PageQuery {
    #[serde(default = "default_page", alias = "p")]
    page:      usize,
    #[serde(default = "default_page_size", alias = "size")]
    page_size: usize,
}

impl From<PageQuery> for PageRequest {
    fn from(query: PageQuery) -> Self {
        Self {
            page:      query.page,
            page_size: query.page_size,
        }
    }
}

/// List channels query, compatible with new-api web `/api/channel`.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ChannelListQuery {
    #[serde(default = "default_page", alias = "p")]
    page:         usize,
    #[serde(default = "default_page_size", alias = "size")]
    page_size:    usize,
    #[serde(default)]
    group:        String,
    /// `enabled` / `1`, `disabled` / `0`, empty = all.
    #[serde(default)]
    status:       String,
    #[serde(default, rename = "type")]
    channel_type: Option<i32>,
    #[serde(default)]
    sort_by:      String,
    #[serde(default)]
    sort_order:   String,
    #[serde(default)]
    id_sort:      bool,
    #[serde(default)]
    tag_mode:     bool,
}

impl ChannelListQuery {
    pub(crate) fn page_request(&self) -> PageRequest {
        PageRequest {
            page:      self.page,
            page_size: self.page_size,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ChannelSearchQuery {
    #[serde(default = "default_page", alias = "p")]
    page:         usize,
    #[serde(default = "default_page_size", alias = "size")]
    page_size:    usize,
    #[serde(default)]
    keyword:      String,
    #[serde(default)]
    group:        String,
    #[serde(default)]
    model:        String,
    /// `enabled` / `1`, `disabled` / `0`, empty = all.
    #[serde(default)]
    status:       String,
    #[serde(default, rename = "type")]
    channel_type: Option<i32>,
    #[serde(default)]
    sort_by:      String,
    #[serde(default)]
    sort_order:   String,
    #[serde(default)]
    id_sort:      bool,
    #[serde(default)]
    tag_mode:     bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(crate) struct ModelUpdateQuery {
    #[serde(default)]
    status_only: bool,
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
    tag:             String,
    #[serde(default)]
    new_tag:         Option<String>,
    #[serde(default)]
    priority:        Option<i64>,
    #[serde(default)]
    weight:          Option<u32>,
    #[serde(default)]
    model_mapping:   Option<String>,
    #[serde(default)]
    models:          Option<String>,
    #[serde(default)]
    groups:          Option<String>,
    #[serde(default)]
    param_override:  Option<String>,
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

        // Generate missing session/internal secrets and persist to credentials file.
        let secrets = bootstrap_credentials::ensure_runtime_secrets(
            config.session.secret.clone(),
            config.internal.secret.clone(),
        )
        .context("bootstrap runtime secrets")?;

        let internal_secret = secrets.internal_secret.as_deref().map(Arc::<str>::from);
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
                "internal.secret is not configured; /internal/gateway/* endpoints will reject \
                 every request (default-deny). Set internal.secret to enable the gateway \
                 snapshot/usage/channel-feedback APIs."
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
        // Do not seed weak/placeholder users from config on first boot.
        // Root is created via setup UI or auto-bootstrap when DB has no root.
        let mut management_data = ManagementData::from_snapshot(snapshot.clone());
        let auto_bootstrap = std::env::var("HALOLAKE_AUTO_BOOTSTRAP")
            .map(|v| {
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(true);
        if auto_bootstrap {
            management_data.users.clear();
        } else {
            management_data.users = config.users;
            normalize_config_users(&mut management_data.users)?;
        }
        let catalog_seed = CatalogData::from_management(&management_data);
        // Zero-cost monomorphized open: each store type is resolved statically.
        let management: ManagementStore =
            store_open::open_seeded_from_config(&config.storage, management_data).await?;

        if auto_bootstrap {
            bootstrap_credentials::ensure_root_admin(&management)
                .await
                .context("bootstrap root admin")?;
        }

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
        let session_secret = secrets.session_secret.clone().unwrap_or_default();
        if session_secret.is_empty() {
            warn!(
                "session.secret / SESSION_SECRET is not set; session cookies are unsigned \
                 (acceptable for local memory/dev only)"
            );
        }
        let session_signer = SessionSigner::new(session_secret);
        // Config snapshot is only a seed for empty DB. Memory bus must start at
        // version 0 so the first publish of DB-backed management always applies
        // (otherwise gateway can boot empty: /v1/models=[], unauthorized tokens).
        snapshot.version = 0;
        let snapshots = MemorySnapshotBus::new(snapshot);
        publish_enriched_management_snapshot(&management, &options, &snapshots, &proxies)
            .await
            .context("publish initial management snapshot to gateway bus")?;
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
            .route("/api/option", get(get_options).put(update_option))
            .route("/api/option/", get(get_options).put(update_option))
            .route(
                "/api/option/payment_compliance",
                post(confirm_payment_compliance),
            )
            .route("/api/option/rest_model_ratio", post(reset_model_ratio))
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
            .route("/api/token/{id}/", get(get_token).delete(delete_token))
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
            .route("/api/channel/{id}", get(get_channel).delete(delete_channel))
            .route("/api/channel/{id}/key", post(reveal_channel_key))
            .route("/api/channel/{id}/status", post(update_channel_status))
            .route("/internal/gateway/snapshot", get(gateway_snapshot))
            .route("/internal/gateway/usage", post(gateway_usage))
            .route(
                "/internal/gateway/channel-feedback",
                post(gateway_channel_feedback),
            )
            .route(
                "/internal/gateway/system-instance",
                post(gateway_system_instance),
            );
        #[cfg(feature = "admin-extras")]
        let router = admin_extras::mount(router);
        #[cfg(feature = "admin-extras")]
        let router = playground::mount(router);
        #[cfg(feature = "compat-stubs")]
        let router = compat::mount(router);
        router.fallback(web_fallback).with_state(self.state)
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

pub(crate) fn default_page() -> usize {
    1
}

pub(crate) fn default_page_size() -> usize {
    10
}

pub(crate) fn default_ranking_period() -> String {
    "week".to_string()
}

pub(crate) fn default_perf_hours() -> i64 {
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
        LogStorageBackend, StorageBackend, StorageConfig, ensure_supported_storage_backend,
        infer_log_storage_backend, infer_main_storage_backend, normalize_mysql_url,
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
