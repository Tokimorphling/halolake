//! User, auth, checkin, and token HTTP handlers.

use crate::{
    AppState, BatchIds, ModelUpdateQuery, PageQuery,
    checkin::{CreateCheckinRequest, GetCheckinStatsRequest},
    checkin_setting, generate_default_token_enabled,
    http_auth::{
        clear_session_cookie, current_user, login_payload, require_role, self_payload,
        session_id_from_headers, set_session_cookie, token_from_read_only_auth,
    },
    http_response::{
        api_error_status, api_ok, api_ok_message, api_success, api_success_with_message,
        management_error, security_error,
    },
    model_list_response, now_unix, option_bool, publish_management_snapshot,
    security::{
        AdminDisableTwoFaRequest, AdminResetPasskeyRequest, AdminTwoFaStatsRequest,
        DeletePasskeyRequest, DisableTwoFaRequest, EnableTwoFaRequest, GetPasskeyStatusRequest,
        GetTwoFaStatusRequest, PasskeyFlow, PasskeyFlowRequest, PasskeyFlowResponse,
        PasskeyRequestContext, PasskeyUser, RegenerateTwoFaBackupCodesRequest,
        StartTwoFaSetupRequest, UniversalVerifyRequest, VerificationMethod,
    },
    session::SessionSigner,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{
        HeaderMap, HeaderValue, StatusCode, Uri,
        header::{HOST, SET_COOKIE},
    },
    response::Response,
};
use halolake_control_plane::{
    AdjustUserQuotaRequest, CreateTokenRequest, CreateUserRequest, DeleteTokenRequest,
    DeleteUserRequest, GetTokenRequest, GetUserRequest, ListTokensRequest, ListUsersRequest,
    LoginUserRequest, ManageUserRequest, ManagementError, RegisterUserRequest,
    RevealTokenKeyRequest, SearchTokensRequest, SearchUsersRequest, UpdateTokenRequest,
    UpdateUserAccessTokenRequest, UpdateUserRequest,
};
use halolake_domain::{
    PageRequest, ROLE_ADMIN_USER, ROLE_ROOT_USER, STATUS_ENABLED, SearchRequest, TokenRecord,
    UserRecord,
};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use service_async::Service;
use uuid::Uuid;

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct TokenSearchQuery {
    #[serde(default = "crate::default_page")]
    pub(crate) page:      usize,
    #[serde(default = "crate::default_page_size")]
    pub(crate) page_size: usize,
    #[serde(default)]
    pub(crate) keyword:   String,
    #[serde(default)]
    pub(crate) token:     String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct UserSearchQuery {
    #[serde(default = "crate::default_page", alias = "p")]
    pub(crate) page:      usize,
    #[serde(default = "crate::default_page_size", alias = "size")]
    pub(crate) page_size: usize,
    #[serde(default)]
    pub(crate) keyword:   String,
    #[serde(default)]
    pub(crate) group:     String,
    #[serde(default)]
    pub(crate) role:      Option<i32>,
    #[serde(default)]
    pub(crate) status:    Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LoginRequest {
    pub(crate) username: String,
    pub(crate) password: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct TwoFaCodePayload {
    #[serde(default)]
    pub(crate) code: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UniversalVerifyPayload {
    pub(crate) method: VerificationMethod,
    #[serde(default)]
    pub(crate) code:   Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RegisterPayload {
    #[serde(default)]
    pub(crate) username:          String,
    #[serde(default)]
    pub(crate) password:          String,
    #[serde(default)]
    pub(crate) display_name:      String,
    #[serde(default)]
    pub(crate) email:             String,
    #[serde(default)]
    pub(crate) verification_code: String,
    #[serde(default)]
    pub(crate) aff_code:          String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CheckinSetting {
    pub(crate) enabled:   bool,
    pub(crate) min_quota: i64,
    pub(crate) max_quota: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserManagePayload {
    pub(crate) id:     u64,
    pub(crate) action: String,
    #[serde(default)]
    pub(crate) value:  i64,
    #[serde(default)]
    pub(crate) mode:   String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct CheckinQuery {
    #[serde(default)]
    pub(crate) month: String,
}

pub(crate) async fn register_user(
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
        id:                   0,
        snapshot_id:          None,
        user_id:              0,
        snapshot_user_id:     None,
        key:                  generate_token_key(),
        status:               STATUS_ENABLED,
        name:                 format!("{username}的初始令牌"),
        created_time:         now,
        accessed_time:        now,
        expired_time:         -1,
        remain_quota:         500_000,
        unlimited_quota:      true,
        model_limits_enabled: false,
        model_limits:         String::new(),
        allow_ips:            None,
        used_quota:           0,
        group:                if option_bool(&options, "DefaultUseAutoGroup", false) {
            "auto".to_string()
        } else {
            String::new()
        },
        cross_group_retry:    false,
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

pub(crate) async fn login_user(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Response {
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
        if let Ok(value) =
            HeaderValue::from_str(&set_session_cookie(&session_id, &state.session_signer))
        {
            resp.headers_mut().insert(SET_COOKIE, value);
        }
        return resp;
    }
    let session_id = match state.sessions.create(user.id) {
        Ok(session_id) => session_id,
        Err(err) => return management_error(err),
    };
    let mut resp = api_success(login_payload(&user));
    if let Ok(value) =
        HeaderValue::from_str(&set_session_cookie(&session_id, &state.session_signer))
    {
        resp.headers_mut().insert(SET_COOKIE, value);
    }
    resp
}

pub(crate) async fn logout_user(State(state): State<AppState>, headers: HeaderMap) -> Response {
    state
        .sessions
        .remove_from_headers(&headers, &state.session_signer);
    let mut resp = api_ok();
    if let Ok(value) = HeaderValue::from_str(&clear_session_cookie()) {
        resp.headers_mut().insert(SET_COOKIE, value);
    }
    resp
}

pub(crate) async fn login_2fa(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<TwoFaCodePayload>,
) -> Response {
    let user_id = match state
        .sessions
        .pending_user_id_from_headers(&headers, &state.session_signer)
    {
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
            session_id: session_id_from_headers(&headers, &state.session_signer)
                .map(str::to_string),
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
    let session_id = match state
        .sessions
        .promote_pending_from_headers(&headers, &state.session_signer)
    {
        Ok(Some(session_id)) => session_id,
        Ok(None) => return api_error_status(StatusCode::OK, "会话已过期，请重新登录"),
        Err(err) => return management_error(err),
    };
    let mut resp = api_success(login_payload(&user));
    if let Ok(value) =
        HeaderValue::from_str(&set_session_cookie(&session_id, &state.session_signer))
    {
        resp.headers_mut().insert(SET_COOKIE, value);
    }
    resp
}

pub(crate) async fn universal_verify(
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
            user_id:    user.id,
            method:     payload.method,
            code:       payload.code,
            session_id: session_id_from_headers(&headers, &state.session_signer)
                .map(str::to_string),
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

pub(crate) async fn two_fa_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

pub(crate) async fn setup_two_fa(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .security
        .call(StartTwoFaSetupRequest {
            user_id:  user.id,
            username: user.username,
            issuer:   state.system_name.to_string(),
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

pub(crate) async fn enable_two_fa(
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
            code:    payload.code,
        })
        .await
    {
        Ok(()) => api_ok_message("两步验证启用成功"),
        Err(err) => security_error(err),
    }
}

pub(crate) async fn disable_two_fa(
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
            code:    payload.code,
        })
        .await
    {
        Ok(()) => api_ok_message("两步验证已禁用"),
        Err(err) => security_error(err),
    }
}

pub(crate) async fn regenerate_two_fa_backup_codes(
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
            code:    payload.code,
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

pub(crate) async fn passkey_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

pub(crate) async fn passkey_register_begin(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    passkey_user_flow(&state, &headers, &uri, PasskeyFlow::RegisterBegin, None).await
}

pub(crate) async fn passkey_register_finish(
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

pub(crate) async fn passkey_verify_begin(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    passkey_user_flow(&state, &headers, &uri, PasskeyFlow::VerifyBegin, None).await
}

pub(crate) async fn passkey_verify_finish(
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

pub(crate) async fn passkey_login_begin(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (session_id, set_cookie) =
        match state
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
            user:       None,
            flow:       PasskeyFlow::LoginBegin,
            session_id: session_id.clone(),
            request:    passkey_request_context(&headers, &uri),
            payload:    None,
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

pub(crate) async fn passkey_login_finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<JsonValue>,
) -> Response {
    let (session_id, _) =
        match state
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

pub(crate) async fn delete_passkey(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

pub(crate) async fn admin_two_fa_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
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

pub(crate) async fn admin_reset_passkey(
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
            actor_role:     actor.role,
            target_role:    target.role,
            target_user_id: target.id,
        })
        .await
    {
        Ok(()) => api_ok_message("Passkey 已重置"),
        Err(err) => security_error(err),
    }
}

pub(crate) async fn admin_disable_two_fa(
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
            actor_role:     actor.role,
            target_role:    target.role,
            target_user_id: target.id,
        })
        .await
    {
        Ok(()) => api_ok_message("用户2FA已被强制禁用"),
        Err(err) => security_error(err),
    }
}

pub(crate) async fn passkey_user_flow(
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
                id:           user.id,
                username:     user.username,
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

pub(crate) async fn get_self(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    api_success(self_payload(&user))
}

pub(crate) async fn get_checkin_status(
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

pub(crate) async fn do_checkin(State(state): State<AppState>, headers: HeaderMap) -> Response {
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
                id:    user.id,
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

pub(crate) async fn update_self(
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

pub(crate) async fn delete_self(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let current = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state
        .management
        .call(DeleteUserRequest {
            id:         current.id,
            actor_role: ROLE_ROOT_USER,
        })
        .await
    {
        Ok(()) => {
            state
                .sessions
                .remove_from_headers(&headers, &state.session_signer);
            match publish_management_snapshot(&state).await {
                Ok(()) => api_ok(),
                Err(err) => management_error(err),
            }
        }
        Err(err) => management_error(err),
    }
}

pub(crate) async fn user_groups(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

pub(crate) async fn user_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = current_user(&state, &headers).await {
        return resp;
    }
    model_list_response(&state).await
}

pub(crate) async fn generate_access_token(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
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

pub(crate) async fn list_users(
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
        Ok(page) => {
            let items: Vec<JsonValue> = page
                .items
                .into_iter()
                .map(|u| user_to_api_json(u.sanitized()))
                .collect();
            api_success(json!({
                "items": items,
                "total": page.total,
                "page": page.page,
                "page_size": page.page_size,
            }))
        }
        Err(err) => management_error(err),
    }
}

pub(crate) async fn search_users(
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
                page:    PageRequest {
                    page:      query.page,
                    page_size: query.page_size,
                },
                keyword: query.keyword,
            },
            group:  query.group,
            role:   query.role,
            status: query.status,
        })
        .await
    {
        Ok(page) => {
            let items: Vec<JsonValue> = page
                .items
                .into_iter()
                .map(|u| user_to_api_json(u.sanitized()))
                .collect();
            api_success(json!({
                "items": items,
                "total": page.total,
                "page": page.page,
                "page_size": page.page_size,
            }))
        }
        Err(err) => management_error(err),
    }
}

pub(crate) async fn get_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let _actor = match require_role(&state, &headers, ROLE_ADMIN_USER).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    match state.management.call(GetUserRequest { id }).await {
        Ok(user) => api_success(user_to_api_json(user.sanitized())),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn create_user(
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

pub(crate) async fn update_user(
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

pub(crate) async fn delete_user(
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

pub(crate) async fn manage_user(
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
            id:         req.id,
            action:     req.action.clone(),
            value:      req.value,
            mode:       req.mode.clone(),
            actor_role: actor.role,
        })
        .await
    {
        Ok(user) => {
            // Quota / role / status changes affect gateway settle & UI; republish.
            if let Err(err) = publish_management_snapshot(&state).await {
                return management_error(err);
            }
            // new-api returns { role, status } for enable/disable/promote/demote.
            // For add_quota also return quota so the admin UI can refresh remaining balance.
            if req.action == "add_quota" {
                api_success(json!({
                    "id": user.id,
                    "role": user.role,
                    "status": user.status,
                    "quota": user.quota,
                    "used_quota": user.used_quota,
                }))
            } else {
                api_success(json!({
                    "role": user.role,
                    "status": user.status,
                }))
            }
        }
        Err(err) => management_error(err),
    }
}

pub(crate) async fn list_tokens(
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
            page:    query.into(),
        })
        .await
    {
        Ok(page) => api_success(page),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn search_tokens(
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
            search:  SearchRequest {
                page:    PageRequest {
                    page:      query.page,
                    page_size: query.page_size,
                },
                keyword: query.keyword,
            },
            token:   query.token,
        })
        .await
    {
        Ok(page) => api_success(page),
        Err(err) => management_error(err),
    }
}

pub(crate) async fn get_token(
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

pub(crate) async fn reveal_token_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    // Match new-api GetTokenKey: session auth only (no step-up 2FA/passkey).
    // Channel key reveal still uses require_secure_verification.
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

pub(crate) async fn reveal_token_keys_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<BatchIds>,
) -> Response {
    let user = match current_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    // Match new-api GetTokenKeysBatch: session auth only.
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

pub(crate) async fn create_token(
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

pub(crate) async fn update_token(
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
            id:      patch.id,
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

pub(crate) async fn delete_token(
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

pub(crate) async fn delete_token_batch(
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

pub(crate) async fn get_token_usage(State(state): State<AppState>, headers: HeaderMap) -> Response {
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

/// Enrich user JSON for new-api admin UI (request_count / aff fields optional).
fn user_to_api_json(user: UserRecord) -> JsonValue {
    let mut value = serde_json::to_value(&user).unwrap_or_else(|_| json!({}));
    if let Some(obj) = value.as_object_mut() {
        obj.entry("request_count".to_string()).or_insert(json!(0));
        obj.entry("aff_code".to_string()).or_insert(json!(""));
        obj.entry("aff_count".to_string()).or_insert(json!(0));
        obj.entry("aff_quota".to_string()).or_insert(json!(0));
        obj.entry("aff_history_quota".to_string())
            .or_insert(json!(0));
        obj.entry("inviter_id".to_string()).or_insert(json!(0));
        obj.entry("github_id".to_string()).or_insert(json!(""));
        obj.entry("oidc_id".to_string()).or_insert(json!(""));
        obj.entry("wechat_id".to_string()).or_insert(json!(""));
        obj.entry("telegram_id".to_string()).or_insert(json!(""));
        obj.entry("linux_do_id".to_string()).or_insert(json!(""));
        obj.entry("DeletedAt".to_string())
            .or_insert(JsonValue::Null);
    }
    value
}

pub(crate) fn checkin_award_quota(min_quota: i64, max_quota: i64) -> i64 {
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

pub(crate) fn current_utc_date() -> String {
    date_from_unix_days(now_unix().div_euclid(86_400))
}

pub(crate) fn date_from_unix_days(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

pub(crate) fn civil_from_days(days: i64) -> (i32, u32, u32) {
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

pub(crate) fn passkey_request_context(headers: &HeaderMap, uri: &Uri) -> PasskeyRequestContext {
    PasskeyRequestContext {
        host:            header_string(headers, HOST.as_str()),
        forwarded_proto: header_string(headers, "x-forwarded-proto")
            .or_else(|| header_string(headers, "x-forwarded-protocol")),
        uri_scheme:      uri.scheme_str().map(str::to_string),
    }
}

pub(crate) fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(crate) fn insert_session_cookie(resp: &mut Response, session_id: &str, signer: &SessionSigner) {
    if let Ok(value) = HeaderValue::from_str(&set_session_cookie(session_id, signer)) {
        resp.headers_mut().insert(SET_COOKIE, value);
    }
}

pub(crate) fn fill_new_token_defaults(token: &mut TokenRecord, user_id: u64) {
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
pub(crate) fn sanitize_self_service_token_create(token: &mut TokenRecord, user_id: u64) {
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

pub(crate) fn fill_new_user_defaults(user: &mut UserRecord) {
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

pub(crate) fn generate_token_key() -> String {
    let mut key = String::with_capacity(48);
    while key.len() < 48 {
        key.push_str(&Uuid::new_v4().simple().to_string());
    }
    key.truncate(48);
    key
}
