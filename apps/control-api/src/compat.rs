//! Frontend-compat routes that close the remaining new-api API surface.
//!
//! External integrations (OAuth providers, Stripe/Creem/Waffo/Epay, io.net
//! deployments, Uptime Kuma, SMTP) return empty data or a clear "not
//! configured" error so the copied new-api frontend no longer 404s. Local
//! product logic (prefill groups, user setting, aff code, ratio_config,
//! authz catalog, empty MJ/task pages) is implemented for real.

use super::*;
use axum::routing::{delete, put};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static PREFILL_NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Default)]
struct PrefillStore {
    inner: Arc<RwLock<Vec<PrefillGroup>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct PrefillGroup {
    #[serde(default)]
    id: u64,
    name: String,
    #[serde(rename = "type")]
    group_type: String,
    #[serde(default)]
    items: JsonValue,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PrefillListQuery {
    #[serde(default, rename = "type")]
    group_type: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CompatPageQuery {
    #[serde(default = "default_page", alias = "p")]
    page: usize,
    #[serde(default = "default_page_size")]
    page_size: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct EmailQuery {
    #[serde(default)]
    email: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PasswordResetBody {
    #[serde(default)]
    email: String,
    #[serde(default)]
    token: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AmountBody {
    #[serde(default)]
    amount: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AffTransferBody {
    #[serde(default)]
    quota: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct UserSettingBody {
    #[serde(default)]
    notify_type: Option<String>,
    #[serde(default)]
    quota_warning_threshold: Option<f64>,
    #[serde(default)]
    webhook_url: Option<String>,
    #[serde(default)]
    webhook_secret: Option<String>,
    #[serde(default)]
    notification_email: Option<String>,
    #[serde(default)]
    bark_url: Option<String>,
    #[serde(default)]
    gotify_url: Option<String>,
    #[serde(default)]
    gotify_token: Option<String>,
    #[serde(default)]
    gotify_priority: Option<i64>,
    #[serde(default)]
    upstream_model_update_notify_enabled: Option<bool>,
    #[serde(default)]
    accept_unset_model_ratio_model: Option<bool>,
    #[serde(default)]
    record_ip_log: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SubscriptionPreferenceBody {
    #[serde(default)]
    billing_preference: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
struct SubscriptionPlanBody {
    #[serde(default)]
    plan: JsonValue,
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
struct CustomOAuthDiscoveryBody {
    #[serde(default)]
    well_known_url: String,
    #[serde(default)]
    issuer_url: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
struct PerformanceLogsQuery {
    #[serde(default)]
    mode: String,
    #[serde(default)]
    value: i64,
}

static PREFILL: OnceLock<PrefillStore> = OnceLock::new();

fn prefill_store() -> &'static PrefillStore {
    PREFILL.get_or_init(PrefillStore::default)
}

impl PrefillStore {
    fn list(&self, group_type: &str) -> Result<Vec<PrefillGroup>, ManagementError> {
        let groups = self
            .inner
            .read()
            .map_err(|_| ManagementError::Poisoned("prefill"))?
            .clone();
        if group_type.is_empty() {
            return Ok(groups);
        }
        Ok(groups
            .into_iter()
            .filter(|group| group.group_type == group_type)
            .collect())
    }

    fn create(&self, mut group: PrefillGroup) -> Result<PrefillGroup, ManagementError> {
        group.name = group.name.trim().to_string();
        group.group_type = group.group_type.trim().to_string();
        if group.name.is_empty() || group.group_type.is_empty() {
            return Err(ManagementError::InvalidRequest(
                "name and type are required",
            ));
        }
        if group.id == 0 {
            group.id = PREFILL_NEXT_ID.fetch_add(1, Ordering::Relaxed);
        }
        let mut groups = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("prefill"))?;
        groups.push(group.clone());
        Ok(group)
    }

    fn update(&self, group: PrefillGroup) -> Result<PrefillGroup, ManagementError> {
        if group.id == 0 {
            return Err(ManagementError::InvalidRequest("id is required"));
        }
        let mut groups = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("prefill"))?;
        let current = groups
            .iter_mut()
            .find(|item| item.id == group.id)
            .ok_or(ManagementError::NotFound)?;
        if !group.name.trim().is_empty() {
            current.name = group.name.trim().to_string();
        }
        if !group.group_type.trim().is_empty() {
            current.group_type = group.group_type.trim().to_string();
        }
        if !group.items.is_null() {
            current.items = group.items;
        }
        if !group.description.is_empty() {
            current.description = group.description;
        }
        Ok(current.clone())
    }

    fn delete(&self, id: u64) -> Result<(), ManagementError> {
        let mut groups = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("prefill"))?;
        let before = groups.len();
        groups.retain(|group| group.id != id);
        if before == groups.len() {
            return Err(ManagementError::NotFound);
        }
        Ok(())
    }
}

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/uptime/status", get(uptime_status))
        .route("/api/verification", get(send_verification))
        .route("/api/reset_password", get(send_reset_password))
        .route("/api/user/reset", post(reset_password))
        .route("/api/ratio_config", get(ratio_config))
        .route("/api/authz/catalog", get(authz_catalog))
        .route(
            "/api/prefill_group",
            get(list_prefill_groups)
                .post(create_prefill_group)
                .put(update_prefill_group),
        )
        .route(
            "/api/prefill_group/",
            get(list_prefill_groups)
                .post(create_prefill_group)
                .put(update_prefill_group),
        )
        .route("/api/prefill_group/{id}", delete(delete_prefill_group))
        .route("/api/mj/", get(list_mj_admin))
        .route("/api/mj/self", get(list_mj_self))
        .route("/api/mj/self/", get(list_mj_self))
        .route("/api/task/", get(list_task_admin))
        .route("/api/task/self", get(list_task_self))
        .route("/api/oauth/state", get(oauth_state))
        .route("/api/oauth/email/bind", post(oauth_not_configured_post))
        .route("/api/oauth/wechat", get(oauth_not_configured_get))
        .route(
            "/api/oauth/wechat/bind",
            get(oauth_not_configured_get).post(oauth_not_configured_post),
        )
        .route("/api/oauth/telegram/login", get(oauth_not_configured_get))
        .route("/api/oauth/telegram/bind", get(oauth_not_configured_get))
        .route("/api/oauth/{provider}", get(oauth_not_configured_get))
        .route(
            "/api/custom-oauth-provider/",
            get(empty_list).post(oauth_not_configured_post),
        )
        .route(
            "/api/custom-oauth-provider/discovery",
            post(custom_oauth_discovery),
        )
        .route(
            "/api/custom-oauth-provider/{id}",
            get(custom_oauth_not_found)
                .put(oauth_not_configured_post)
                .delete(oauth_not_configured_delete),
        )
        .route("/api/subscription/plans", get(subscription_plans))
        .route("/api/subscription/self", get(subscription_self))
        .route(
            "/api/subscription/self/preference",
            put(subscription_preference),
        )
        .route("/api/subscription/balance/pay", post(payment_not_configured))
        .route("/api/subscription/epay/pay", post(payment_not_configured))
        .route("/api/subscription/stripe/pay", post(payment_not_configured))
        .route("/api/subscription/creem/pay", post(payment_not_configured))
        .route(
            "/api/subscription/waffo-pancake/pay",
            post(payment_not_configured),
        )
        .route(
            "/api/subscription/admin/plans",
            get(empty_list).post(subscription_admin_create_plan),
        )
        .route(
            "/api/subscription/admin/plans/{id}",
            put(subscription_admin_update_plan).patch(subscription_admin_patch_plan),
        )
        .route(
            "/api/subscription/admin/plans/{id}/subscriptions/reset",
            post(subscription_admin_reset),
        )
        .route(
            "/api/subscription/admin/users/{id}/subscriptions",
            get(empty_list).post(subscription_admin_bind),
        )
        .route(
            "/api/subscription/admin/users/{id}/subscriptions/reset",
            post(subscription_admin_reset),
        )
        .route(
            "/api/subscription/admin/user_subscriptions/{id}",
            delete(subscription_admin_delete),
        )
        .route(
            "/api/subscription/admin/user_subscriptions/{id}/invalidate",
            post(subscription_admin_invalidate),
        )
        .route("/api/subscription/admin/bind", post(subscription_admin_bind))
        .route("/api/user/amount", post(user_amount))
        .route("/api/user/pay", post(payment_not_configured))
        .route("/api/user/stripe/amount", post(user_amount))
        .route("/api/user/stripe/pay", post(payment_not_configured))
        .route("/api/user/creem/pay", post(payment_not_configured))
        .route("/api/user/waffo/amount", post(user_amount))
        .route("/api/user/waffo/pay", post(payment_not_configured))
        .route("/api/user/waffo-pancake/amount", post(user_amount))
        .route("/api/user/waffo-pancake/pay", post(payment_not_configured))
        .route("/api/user/aff", get(user_aff))
        .route("/api/user/aff_transfer", post(user_aff_transfer))
        .route("/api/user/setting", put(update_user_setting))
        .route(
            "/api/user/oauth/bindings",
            get(empty_list).delete(oauth_not_configured_delete),
        )
        .route(
            "/api/user/{id}/oauth/bindings",
            get(empty_list),
        )
        .route(
            "/api/user/{id}/oauth/bindings/{provider_id}",
            delete(oauth_not_configured_delete),
        )
        .route(
            "/api/user/oauth/bindings/{provider_id}",
            delete(oauth_not_configured_delete),
        )
        .route("/api/performance/stats", get(performance_stats))
        .route(
            "/api/performance/disk_cache",
            delete(performance_clear_disk_cache),
        )
        .route("/api/performance/reset_stats", post(performance_reset_stats))
        .route("/api/performance/gc", post(performance_gc))
        .route(
            "/api/performance/logs",
            get(performance_logs).delete(performance_cleanup_logs),
        )
        .route("/api/deployments/settings", get(deployment_settings))
        .route(
            "/api/deployments/settings/test-connection",
            post(deployment_not_configured),
        )
        .route(
            "/api/deployments/",
            get(deployment_list).post(deployment_not_configured),
        )
        .route("/api/deployments/search", get(deployment_list))
        .route("/api/deployments/hardware-types", get(deployment_hardware))
        .route("/api/deployments/locations", get(deployment_locations))
        .route(
            "/api/deployments/available-replicas",
            get(deployment_replicas),
        )
        .route(
            "/api/deployments/price-estimation",
            post(deployment_not_configured),
        )
        .route("/api/deployments/check-name", get(deployment_check_name))
        .route(
            "/api/deployments/batch_delete",
            post(deployment_not_configured),
        )
        .route(
            "/api/deployments/{id}",
            get(deployment_not_found)
                .put(deployment_not_configured)
                .delete(deployment_not_configured),
        )
        .route("/api/deployments/{id}/name", put(deployment_not_configured))
        .route("/api/deployments/{id}/logs", get(empty_list))
        .route("/api/deployments/{id}/containers", get(empty_list))
        .route(
            "/api/deployments/{id}/containers/{container_id}",
            get(deployment_not_found),
        )
        .route(
            "/api/deployments/{id}/extend",
            post(deployment_not_configured),
        )
        .route(
            "/api/deployments/{id}/restart",
            post(deployment_not_configured),
        )
        .route(
            "/api/deployments/{id}/start",
            post(deployment_not_configured),
        )
        .route("/api/stripe/webhook", post(webhook_ok))
        .route("/api/creem/webhook", post(webhook_ok))
        .route("/api/waffo/webhook", post(webhook_ok))
        .route("/api/waffo-pancake/webhook/{env}", post(webhook_ok))
        .route(
            "/api/user/epay/notify",
            get(webhook_ok).post(webhook_ok),
        )
}

async fn uptime_status() -> Response {
    api_success(Vec::<JsonValue>::new())
}

async fn send_verification(Query(query): Query<EmailQuery>) -> Response {
    let _ = query.email;
    // Anti-enumeration: always succeed. Real SMTP is not configured yet.
    api_ok()
}

async fn send_reset_password(Query(query): Query<EmailQuery>) -> Response {
    let _ = query.email;
    api_ok()
}

async fn reset_password(Json(body): Json<PasswordResetBody>) -> Response {
    let _ = (body.email, body.token);
    api_error_status(
        StatusCode::OK,
        "password reset by email is not configured; ask an admin to reset your password",
    )
}

async fn ratio_config(State(state): State<AppState>) -> Response {
    let options = state.options.values().unwrap_or_default();
    if !option_bool(&options, "ExposeRatioEnabled", false) {
        return api_error_status(StatusCode::FORBIDDEN, "倍率配置接口未启用");
    }
    api_success(json!({
        "model_ratio": option_json_object(&options, "ModelRatio"),
        "completion_ratio": option_json_object(&options, "CompletionRatio"),
        "cache_ratio": option_json_object(&options, "CacheRatio"),
        "create_cache_ratio": option_json_object(&options, "CreateCacheRatio"),
        "model_price": option_json_object(&options, "ModelPrice"),
    }))
}

async fn authz_catalog(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_success(static_authz_catalog())
}

fn static_authz_catalog() -> JsonValue {
    let channel_actions = [
        ("read", "permission.channel.read", "permission.channel.read.desc"),
        (
            "operate",
            "permission.channel.operate",
            "permission.channel.operate.desc",
        ),
        (
            "write",
            "permission.channel.write",
            "permission.channel.write.desc",
        ),
        (
            "sensitive_write",
            "permission.channel.sensitive_write",
            "permission.channel.sensitive_write.desc",
        ),
        (
            "secret_view",
            "permission.channel.secret_view",
            "permission.channel.secret_view.desc",
        ),
    ]
    .into_iter()
    .map(|(action, label_key, description_key)| {
        json!({
            "action": action,
            "label_key": label_key,
            "description_key": description_key,
        })
    })
    .collect::<Vec<_>>();

    let admin_grants = json!({
        "channel": {
            "read": true,
            "operate": true,
            "write": true,
            "sensitive_write": true,
            "secret_view": false,
        }
    });
    let root_grants = json!({
        "channel": {
            "read": true,
            "operate": true,
            "write": true,
            "sensitive_write": true,
            "secret_view": true,
        }
    });

    json!({
        "resources": [{
            "resource": "channel",
            "label_key": "permission.channel",
            "actions": channel_actions,
        }],
        "roles": [
            {
                "key": "admin",
                "name": "Admin",
                "built_in": true,
                "superuser": false,
                "grants": admin_grants,
            },
            {
                "key": "root",
                "name": "Root",
                "built_in": true,
                "superuser": true,
                "grants": root_grants,
            }
        ]
    })
}

async fn list_prefill_groups(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PrefillListQuery>,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    match prefill_store().list(&query.group_type) {
        Ok(groups) => api_success(groups),
        Err(err) => management_error(err),
    }
}

async fn create_prefill_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(group): Json<PrefillGroup>,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    match prefill_store().create(group) {
        Ok(group) => api_success(group),
        Err(err) => management_error(err),
    }
}

async fn update_prefill_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(group): Json<PrefillGroup>,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    match prefill_store().update(group) {
        Ok(group) => api_success(group),
        Err(err) => management_error(err),
    }
}

async fn delete_prefill_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    match prefill_store().delete(id) {
        Ok(()) => api_ok(),
        Err(err) => management_error(err),
    }
}

async fn list_mj_admin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CompatPageQuery>,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_success(empty_page(query))
}

async fn list_mj_self(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CompatPageQuery>,
) -> Response {
    if let Err(resp) = current_user(&state, &headers).await {
        return resp;
    }
    api_success(empty_page(query))
}

async fn list_task_admin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CompatPageQuery>,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_success(empty_page(query))
}

async fn list_task_self(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CompatPageQuery>,
) -> Response {
    if let Err(resp) = current_user(&state, &headers).await {
        return resp;
    }
    api_success(empty_page(query))
}

async fn oauth_state() -> Response {
    let state = &Uuid::new_v4().simple().to_string()[..12];
    api_success(state.to_string())
}

async fn oauth_not_configured_get() -> Response {
    api_error_status(StatusCode::OK, "OAuth is not configured")
}

async fn oauth_not_configured_post() -> Response {
    api_error_status(StatusCode::OK, "OAuth is not configured")
}

async fn oauth_not_configured_delete() -> Response {
    api_error_status(StatusCode::OK, "OAuth is not configured")
}

async fn custom_oauth_discovery(Json(body): Json<CustomOAuthDiscoveryBody>) -> Response {
    let _ = body;
    api_error_status(StatusCode::OK, "custom OAuth discovery is not configured")
}

async fn custom_oauth_not_found() -> Response {
    api_error_status(StatusCode::NOT_FOUND, "custom OAuth provider not found")
}

async fn empty_list() -> Response {
    api_success(Vec::<JsonValue>::new())
}

async fn subscription_plans(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = current_user(&state, &headers).await {
        return resp;
    }
    api_success(Vec::<JsonValue>::new())
}

async fn subscription_self(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = current_user(&state, &headers).await {
        return resp;
    }
    api_success(json!({
        "billing_preference": "subscription_first",
        "subscriptions": [],
        "all_subscriptions": [],
    }))
}

async fn subscription_preference(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SubscriptionPreferenceBody>,
) -> Response {
    if let Err(resp) = current_user(&state, &headers).await {
        return resp;
    }
    let _ = body.billing_preference;
    api_ok()
}

async fn subscription_admin_create_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SubscriptionPlanBody>,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    let _ = body;
    api_error_status(StatusCode::OK, "subscription plans storage is not enabled yet")
}

async fn subscription_admin_update_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(_id): Path<u64>,
    Json(body): Json<SubscriptionPlanBody>,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    let _ = body;
    api_error_status(StatusCode::OK, "subscription plans storage is not enabled yet")
}

async fn subscription_admin_patch_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(_id): Path<u64>,
    Json(body): Json<SubscriptionPlanBody>,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    let _ = body;
    api_error_status(StatusCode::OK, "subscription plans storage is not enabled yet")
}

async fn subscription_admin_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_success(json!({
        "plan_id": 0,
        "matched_count": 0,
        "reset_count": 0,
        "user_count": 0,
        "advance_reset_time": false,
    }))
}

async fn subscription_admin_bind(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_error_status(StatusCode::OK, "subscription binding is not enabled yet")
}

async fn subscription_admin_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(_id): Path<u64>,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_ok()
}

async fn subscription_admin_invalidate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(_id): Path<u64>,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_ok()
}

async fn payment_not_configured() -> Response {
    api_error_status(StatusCode::OK, "online payment is not configured")
}

async fn user_amount(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AmountBody>,
) -> Response {
    if let Err(resp) = current_user(&state, &headers).await {
        return resp;
    }
    let _ = body.amount;
    // Legacy epay-style envelope some wallet pages still parse.
    Json(json!({
        "message": "success",
        "data": "0",
        "success": false,
    }))
    .into_response()
}

async fn user_aff(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    api_success(aff_code_for_user(user.id))
}

async fn user_aff_transfer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AffTransferBody>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let _ = (user, body.quota);
    api_error_status(StatusCode::OK, "no affiliate quota available")
}

async fn update_user_setting(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<UserSettingBody>,
) -> Response {
    let current = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let mut setting = if current.setting.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&current.setting).unwrap_or_else(|_| json!({}))
    };
    if let Some(obj) = setting.as_object_mut() {
        if let Some(value) = body.notify_type {
            obj.insert("notify_type".into(), json!(value));
        }
        if let Some(value) = body.quota_warning_threshold {
            obj.insert("quota_warning_threshold".into(), json!(value));
        }
        if let Some(value) = body.webhook_url {
            obj.insert("webhook_url".into(), json!(value));
        }
        if let Some(value) = body.webhook_secret {
            obj.insert("webhook_secret".into(), json!(value));
        }
        if let Some(value) = body.notification_email {
            obj.insert("notification_email".into(), json!(value));
        }
        if let Some(value) = body.bark_url {
            obj.insert("bark_url".into(), json!(value));
        }
        if let Some(value) = body.gotify_url {
            obj.insert("gotify_url".into(), json!(value));
        }
        if let Some(value) = body.gotify_token {
            obj.insert("gotify_token".into(), json!(value));
        }
        if let Some(value) = body.gotify_priority {
            obj.insert("gotify_priority".into(), json!(value));
        }
        if let Some(value) = body.upstream_model_update_notify_enabled {
            obj.insert(
                "upstream_model_update_notify_enabled".into(),
                json!(value),
            );
        }
        if let Some(value) = body.accept_unset_model_ratio_model {
            obj.insert("accept_unset_model_ratio_model".into(), json!(value));
        }
        if let Some(value) = body.record_ip_log {
            obj.insert("record_ip_log".into(), json!(value));
        }
    }
    let mut user = current.clone();
    user.setting = setting.to_string();
    match state
        .management
        .call(UpdateUserRequest {
            user,
            actor_role: ROLE_ROOT_USER,
        })
        .await
    {
        Ok(_) => api_ok_message("settings updated"),
        Err(err) => management_error(err),
    }
}

async fn performance_stats(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ROOT_USER).await {
        return resp;
    }
    api_success(json!({
        "cache_stats": {},
        "memory_stats": {
            "alloc": 0,
            "total_alloc": 0,
            "sys": 0,
            "num_gc": 0,
            "num_goroutine": 0,
        },
        "disk_cache_info": {
            "path": "",
            "exists": false,
            "file_count": 0,
            "total_size": 0,
        },
        "disk_space_info": { "used_percent": 0 },
        "config": {
            "disk_cache_enabled": false,
            "disk_cache_threshold_mb": 0,
            "disk_cache_max_size_mb": 0,
            "disk_cache_path": "",
            "is_running_in_container": false,
            "monitor_enabled": false,
            "monitor_cpu_threshold": 0,
            "monitor_memory_threshold": 0,
            "monitor_disk_threshold": 0,
        }
    }))
}

async fn performance_clear_disk_cache(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ROOT_USER).await {
        return resp;
    }
    api_ok_message("disk cache cleared")
}

async fn performance_reset_stats(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ROOT_USER).await {
        return resp;
    }
    api_ok_message("stats reset")
}

async fn performance_gc(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ROOT_USER).await {
        return resp;
    }
    api_ok_message("gc requested")
}

async fn performance_logs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ROOT_USER).await {
        return resp;
    }
    api_success(json!({
        "log_dir": "",
        "enabled": false,
        "file_count": 0,
        "total_size": 0,
        "files": [],
    }))
}

async fn performance_cleanup_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PerformanceLogsQuery>,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ROOT_USER).await {
        return resp;
    }
    let _ = query;
    api_ok_message("log cleanup complete")
}

async fn deployment_settings(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_success(json!({
        "enabled": false,
        "configured": false,
    }))
}

async fn deployment_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CompatPageQuery>,
) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_success(empty_page(query))
}

async fn deployment_hardware(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_success(json!({ "hardware_types": [] }))
}

async fn deployment_locations(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_success(json!({ "locations": [] }))
}

async fn deployment_replicas(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_success(json!({ "replicas": [] }))
}

async fn deployment_check_name(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_success(json!({ "available": true }))
}

async fn deployment_not_configured(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_error_status(StatusCode::OK, "deployment integration is not configured")
}

async fn deployment_not_found(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return resp;
    }
    api_error_status(StatusCode::NOT_FOUND, "deployment not found")
}

async fn webhook_ok() -> Response {
    // Payment integrations are not configured. Returning success would let a
    // public caller fake settlement against any frontend/admin path that treats
    // a 2xx webhook as paid. Reject clearly instead.
    api_error_status(
        StatusCode::SERVICE_UNAVAILABLE,
        "payment webhook integration is not configured",
    )
}

fn empty_page(query: CompatPageQuery) -> PageResult<JsonValue> {
    PageResult::new(
        Vec::new(),
        0,
        PageRequest {
            page: query.page.max(1),
            page_size: query.page_size.max(1),
        },
    )
}

fn aff_code_for_user(user_id: u64) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut n = user_id.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA5A5_A5A5_A5A5_A5A5;
    let mut out = [b'A'; 4];
    for slot in &mut out {
        *slot = ALPHABET[(n as usize) % ALPHABET.len()];
        n /= ALPHABET.len() as u64;
        if n == 0 {
            n = user_id.saturating_add(1);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn option_json_object(options: &BTreeMap<String, String>, key: &str) -> JsonValue {
    options
        .get(key)
        .and_then(|value| serde_json::from_str(value).ok())
        .unwrap_or_else(|| json!({}))
}
