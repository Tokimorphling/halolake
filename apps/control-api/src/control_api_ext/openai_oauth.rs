//! OpenAI / Codex OAuth helpers for credential import.
//!
//! Supports:
//! - refresh_token → session JSON (manual RT / mobile RT)
//! - Codex `at-*` personal access token whoami validation
//! - expanding mixed paste blobs (RT lines, PAT lines, JSON sessions)

use halolake_control_plane::ManagementError;
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use std::time::{SystemTime, UNIX_EPOCH};

const CODEX_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_PAT_WHOAMI_URL: &str =
    "https://auth.openai.com/api/accounts/v1/user-auth-credential/whoami";
const CODEX_CLI_USER_AGENT: &str = "codex_cli_rs";

/// Exchange a Codex/OpenAI refresh token for a session-shaped JSON string.
pub(crate) async fn exchange_codex_refresh_token(
    refresh_token: &str,
) -> Result<String, ManagementError> {
    let refresh_token = refresh_token.trim();
    if refresh_token.is_empty() {
        return Err(ManagementError::InvalidRequest("refresh_token is empty"));
    }
    // note: dynamic upstream errors use Storage (InvalidRequest is &'static)
    let client = reqwest::Client::new();
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        percent_encode(refresh_token),
        percent_encode(CODEX_OAUTH_CLIENT_ID)
    );
    let response = client
        .post(CODEX_OAUTH_TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body(body)
        .send()
        .await
        .map_err(storage_err)?;
    let status = response.status();
    let payload: JsonValue = response.json().await.map_err(storage_err)?;
    if !status.is_success() {
        let msg = payload
            .get("error_description")
            .or_else(|| payload.get("error"))
            .and_then(JsonValue::as_str)
            .unwrap_or("oauth refresh failed");
        return Err(ManagementError::Storage(format!(
            "codex refresh_token exchange failed ({status}): {msg}"
        )));
    }
    let access_token = payload
        .get("access_token")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ManagementError::Storage("oauth response missing access_token".into())
        })?;
    let new_refresh = payload
        .get("refresh_token")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(refresh_token);
    let id_token = payload
        .get("id_token")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_string();
    let expires_in = payload
        .get("expires_in")
        .and_then(|v| v.as_i64().or_else(|| v.as_u64().map(|n| n as i64)))
        .unwrap_or(0);
    let expires_at = if expires_in > 0 {
        Some(now_unix().saturating_add(expires_in))
    } else {
        None
    };

    let mut tokens = JsonMap::new();
    tokens.insert("access_token".into(), json!(access_token));
    tokens.insert("refresh_token".into(), json!(new_refresh));
    if !id_token.is_empty() {
        tokens.insert("id_token".into(), json!(id_token));
    }
    if let Some(exp) = expires_at {
        tokens.insert("expires_at".into(), json!(exp));
    }

    let mut out = JsonMap::new();
    out.insert("type".into(), json!("codex"));
    out.insert("tokens".into(), JsonValue::Object(tokens));
    Ok(serde_json::to_string(&JsonValue::Object(out)).map_err(storage_err)?)
}

/// Validate Codex `at-*` PAT via whoami; returns enriched session JSON (access_token only).
pub(crate) async fn validate_codex_personal_access_token(
    access_token: &str,
) -> Result<String, ManagementError> {
    let access_token = access_token.trim();
    if access_token.is_empty() {
        return Err(ManagementError::InvalidRequest("access token is empty"));
    }
    if !access_token.starts_with("at-") {
        return Err(ManagementError::InvalidRequest(
            "Codex personal access token must start with at-",
        ));
    }
    // dynamic errors below use Storage
    let client = reqwest::Client::new();
    let response = client
        .get(CODEX_PAT_WHOAMI_URL)
        .header("authorization", format!("Bearer {access_token}"))
        .header("accept", "application/json")
        .header("originator", "codex_cli_rs")
        .header("user-agent", CODEX_CLI_USER_AGENT)
        .send()
        .await
        .map_err(storage_err)?;
    let status = response.status();
    let payload: JsonValue = response.json().await.map_err(storage_err)?;
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(ManagementError::InvalidRequest(
            "Codex personal access token is invalid or expired",
        ));
    }
    if !status.is_success() {
        let msg = payload
            .get("error")
            .and_then(|e| e.get("message").or(Some(e)))
            .and_then(JsonValue::as_str)
            .unwrap_or("whoami failed");
        return Err(ManagementError::Storage(format!(
            "Codex PAT validation failed ({status}): {msg}"
        )));
    }

    let email = payload
        .get("email")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let account_id = payload
        .get("chatgpt_account_id")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let user_id = payload
        .get("chatgpt_user_id")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let plan_type = payload
        .get("chatgpt_plan_type")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if email.is_empty() || account_id.is_empty() || user_id.is_empty() {
        return Err(ManagementError::Storage(
            "Codex PAT whoami response missing account identity fields".into(),
        ));
    }

    let mut tokens = JsonMap::new();
    tokens.insert("access_token".into(), json!(access_token));
    // far-future expiry placeholder: PAT does not rotate; import pipeline needs expiry or RT
    tokens.insert("expires_at".into(), json!(now_unix().saturating_add(365 * 24 * 3600)));

    let mut out = JsonMap::new();
    out.insert("type".into(), json!("codex"));
    out.insert("email".into(), json!(email));
    out.insert("chatgpt_account_id".into(), json!(account_id));
    out.insert("chatgpt_user_id".into(), json!(user_id));
    if !plan_type.is_empty() {
        out.insert("plan_type".into(), json!(plan_type));
    }
    out.insert("tokens".into(), JsonValue::Object(tokens));
    Ok(serde_json::to_string(&JsonValue::Object(out)).map_err(storage_err)?)
}

/// Expand paste content into importable Codex session JSON blobs.
///
/// - JSON object/array → passed through (possibly split NDJSON lines of JSON)
/// - lines starting with `at-` → whoami-validated PAT sessions
/// - other non-empty lines treated as refresh tokens → exchanged
pub(crate) async fn expand_codex_import_blob(
    content: &str,
) -> Result<Vec<String>, ManagementError> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    // Whole blob is JSON
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Ok(vec![trimmed.to_string()]);
    }

    let mut out = Vec::new();
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('{') || line.starts_with('[') {
            out.push(line.to_string());
            continue;
        }
        if line.starts_with("at-") {
            out.push(validate_codex_personal_access_token(line).await?);
            continue;
        }
        // refresh token (manual RT / mobile RT)
        out.push(exchange_codex_refresh_token(line).await?);
    }
    if out.is_empty() {
        return Err(ManagementError::InvalidRequest(
            "no refresh tokens, PAT (at-*), or session JSON found",
        ));
    }
    // ok
    Ok(out)
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn storage_err(err: impl std::fmt::Display) -> ManagementError {
    ManagementError::Storage(err.to_string())
}
