//! Auth / session helpers for control-api handlers.

use axum::{
    Json,
    http::{HeaderMap, StatusCode, header::COOKIE},
    response::{IntoResponse, Response},
};
use halolake_control_plane::{GetUserRequest, ValidateUserAccessTokenRequest};
use halolake_domain::{
    ROLE_ADMIN_USER, ROLE_ROOT_USER, STATUS_ENABLED, TokenRecord, UserRecord,
};
use serde_json::json;
use service_async::Service;

use crate::http_response::{api_error_status, management_error};
use crate::session::{SecureVerificationError, SessionSigner};
use crate::{
    AppState, INTERNAL_KEY_HEADER, NEW_API_USER_HEADER, SESSION_COOKIE_NAME, TOKEN_STATUS_EXHAUSTED,
    TOKEN_STATUS_EXPIRED,
};

/// Length-independent, byte-by-byte comparison that avoids the early-exit
/// timing signal of `==`. Not a substitute for a constant-time-length
/// primitive, but adequate for a shared-secret header check.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub(crate) fn require_secure_verification(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(), Response> {
    let Some(session_id) = session_id_from_headers(headers, &state.session_signer) else {
        return Err(secure_verification_response(SecureVerificationError::Required));
    };
    match state.sessions.require_secure_verified(session_id) {
        Ok(()) => Ok(()),
        Err(err) => Err(secure_verification_response(err)),
    }
}

pub(crate) fn secure_verification_response(err: SecureVerificationError) -> Response {
    // Frontend `isVerificationRequiredError` reads `response.data.code` at the
    // top level of the JSON body (not nested under `data`).
    let (message, code) = match err {
        SecureVerificationError::Required => ("需要安全验证", "VERIFICATION_REQUIRED"),
        SecureVerificationError::Expired => ("验证已过期，请重新验证", "VERIFICATION_EXPIRED"),
        SecureVerificationError::Unavailable => {
            return api_error_status(StatusCode::INTERNAL_SERVER_ERROR, "session unavailable");
        }
    };
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "success": false,
            "message": message,
            "code": code,
        })),
    )
        .into_response()
}

pub(crate) async fn current_user(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<UserRecord, Response> {
    let session_user_id =
        match state.sessions.user_id_from_headers(headers, &state.session_signer) {
            Ok(user_id) => user_id,
            Err(err) => return Err(management_error(err)),
        };
    let access_token = if session_user_id.is_none() {
        access_token_from_headers(headers)
    } else {
        None
    };
    let user_id = if let Some(user_id) = session_user_id {
        user_id
    } else if let Some(access_token) = access_token {
        let user = match state
            .management
            .call(ValidateUserAccessTokenRequest { access_token })
            .await
        {
            Ok(user) => user,
            Err(err) => return Err(management_error(err)),
        };
        user.id
    } else {
        return Err(api_error_status(StatusCode::UNAUTHORIZED, "not logged in"));
    };
    if let Some(header_user_id) = new_api_user_id(headers) {
        match header_user_id {
            Ok(id) if id == user_id => {}
            Ok(_) => {
                return Err(api_error_status(
                    StatusCode::UNAUTHORIZED,
                    "user id mismatch",
                ));
            }
            Err(()) => {
                return Err(api_error_status(
                    StatusCode::UNAUTHORIZED,
                    "invalid New-Api-User header",
                ));
            }
        }
    }
    let user = match state.management.call(GetUserRequest { id: user_id }).await {
        Ok(user) => user,
        Err(err) => return Err(management_error(err)),
    };
    if user.status != STATUS_ENABLED {
        return Err(api_error_status(StatusCode::OK, "user is disabled"));
    }
    Ok(user)
}

pub(crate) fn token_from_read_only_auth(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<TokenRecord, Response> {
    let keys = authorization_token_keys(headers)?;
    let data = state.management.current_data().map_err(management_error)?;
    let token = data
        .tokens
        .iter()
        .find(|token| keys.matches(&token.key))
        .cloned()
        .ok_or_else(|| api_error_status(StatusCode::UNAUTHORIZED, "token is invalid"))?;
    if !read_only_token_status_allowed(token.status) {
        return Err(api_error_status(
            StatusCode::UNAUTHORIZED,
            "token status is unavailable",
        ));
    }
    let user_enabled = data
        .users
        .iter()
        .find(|user| user.id == token.user_id)
        .is_none_or(|user| user.status == STATUS_ENABLED);
    if !user_enabled {
        return Err(api_error_status(StatusCode::FORBIDDEN, "user is disabled"));
    }
    Ok(token)
}

pub(crate) struct AuthorizationTokenKeys {
    exact: String,
    split_prefix: Option<String>,
}

impl AuthorizationTokenKeys {
    pub(crate) fn matches(&self, token_key: &str) -> bool {
        token_key == self.exact
            || self
                .split_prefix
                .as_deref()
                .is_some_and(|candidate| token_key == candidate)
    }
}

pub(crate) fn authorization_token_keys(headers: &HeaderMap) -> Result<AuthorizationTokenKeys, Response> {
    let Some(raw) = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return Err(api_error_status(
            StatusCode::UNAUTHORIZED,
            "token is required",
        ));
    };
    let token = raw
        .trim()
        .strip_prefix("Bearer ")
        .or_else(|| raw.trim().strip_prefix("bearer "))
        .unwrap_or(raw.trim())
        .trim()
        .strip_prefix("sk-")
        .unwrap_or_else(|| {
            raw.trim()
                .strip_prefix("Bearer ")
                .or_else(|| raw.trim().strip_prefix("bearer "))
                .unwrap_or(raw.trim())
                .trim()
        });
    if token.is_empty() {
        return Err(api_error_status(
            StatusCode::UNAUTHORIZED,
            "token is required",
        ));
    }
    let split_prefix = token
        .split_once('-')
        .map(|(prefix, _)| prefix.trim())
        .filter(|prefix| !prefix.is_empty() && *prefix != token)
        .map(str::to_string);
    Ok(AuthorizationTokenKeys {
        exact: token.to_string(),
        split_prefix,
    })
}

pub(crate) fn read_only_token_status_allowed(status: i32) -> bool {
    matches!(
        status,
        STATUS_ENABLED | TOKEN_STATUS_EXPIRED | TOKEN_STATUS_EXHAUSTED
    )
}

pub(crate) async fn require_role(
    state: &AppState,
    headers: &HeaderMap,
    min_role: i32,
) -> Result<UserRecord, Response> {
    let user = current_user(state, headers).await?;
    if user.role < min_role {
        return Err(api_error_status(
            StatusCode::FORBIDDEN,
            "insufficient privilege",
        ));
    }
    Ok(user)
}

pub(crate) fn login_payload(user: &UserRecord) -> serde_json::Value {
    json!({
        "id": user.id,
        "username": user.username,
        "display_name": user.display_name,
        "role": user.role,
        "status": user.status,
        "group": user.group,
    })
}

pub(crate) fn self_payload(user: &UserRecord) -> serde_json::Value {
    json!({
        "id": user.id,
        "username": user.username,
        "display_name": user.display_name,
        "role": user.role,
        "status": user.status,
        "email": user.email,
        "github_id": "",
        "discord_id": "",
        "oidc_id": "",
        "wechat_id": "",
        "telegram_id": "",
        "group": user.group,
        "quota": user.quota,
        "used_quota": user.used_quota,
        "request_count": 0,
        "aff_code": "",
        "aff_count": 0,
        "aff_quota": 0,
        "aff_history_quota": 0,
        "inviter_id": 0,
        "linux_do_id": "",
        "setting": user.setting,
        "stripe_customer": "",
        "sidebar_modules": serde_json::Value::Null,
        "permissions": user_permissions(user.role),
    })
}

pub(crate) fn user_permissions(role: i32) -> serde_json::Value {
    if role == ROLE_ROOT_USER {
        json!({
            "sidebar_settings": false,
            "sidebar_modules": {},
            "admin_permissions": {},
        })
    } else if role >= ROLE_ADMIN_USER {
        json!({
            "sidebar_settings": true,
            "sidebar_modules": {
                "admin": {
                    "setting": false,
                }
            },
            "admin_permissions": {},
        })
    } else {
        json!({
            "sidebar_settings": true,
            "sidebar_modules": {
                "admin": false,
            },
            "admin_permissions": {},
        })
    }
}

pub(crate) fn session_id_from_headers<'a>(
    headers: &'a HeaderMap,
    signer: &SessionSigner,
) -> Option<&'a str> {
    let cookie = headers.get(COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        if name != SESSION_COOKIE_NAME || value.is_empty() {
            return None;
        }
        signer.verify_cookie_value(value)
    })
}

pub(crate) fn access_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let token = headers
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .trim();
    let token = token
        .strip_prefix("Bearer ")
        .or_else(|| token.strip_prefix("bearer "))
        .unwrap_or(token)
        .trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

pub(crate) fn new_api_user_id(headers: &HeaderMap) -> Option<Result<u64, ()>> {
    headers.get(NEW_API_USER_HEADER).map(|value| {
        value
            .to_str()
            .map_err(|_| ())
            .and_then(|value| value.parse().map_err(|_| ()))
    })
}

pub(crate) fn set_session_cookie(session_id: &str, signer: &SessionSigner) -> String {
    let value = signer.sign(session_id);
    format!(
        "{SESSION_COOKIE_NAME}={value}; Path=/; Max-Age=2592000; HttpOnly; SameSite=Strict"
    )
}

pub(crate) fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE_NAME}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict")
}

impl AppState {
    pub(crate) fn authorized(&self, headers: &HeaderMap) -> bool {
        // Default-deny: an unset internal secret must never expose the
        // `/internal/*` endpoints, which return plaintext channel keys and
        // gateway tokens. Startup logs a loud warning in this case.
        let Some(secret) = &self.internal_secret else {
            return false;
        };
        headers
            .get(INTERNAL_KEY_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| constant_time_eq(value.as_bytes(), secret.as_bytes()))
    }
}
