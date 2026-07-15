//! OpenAI / Codex OAuth helpers for credential import.
//!
//! Supports:
//! - refresh_token → session JSON (manual RT / mobile RT)
//! - Codex `at-*` personal access token whoami validation
//! - expanding mixed paste blobs (RT lines, PAT lines, JSON sessions)

use halolake_control_plane::ManagementError;
use serde::Deserialize;
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CODEX_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_MOBILE_OAUTH_CLIENT_ID: &str = "app_LlGpXReQgckcGGUo2JrYvtJK";
const CODEX_PAT_WHOAMI_URL: &str =
    "https://auth.openai.com/api/accounts/v1/user-auth-credential/whoami";
const CODEX_CLI_USER_AGENT: &str = "codex_cli_rs";
const CODEX_REFRESH_SCOPES: &str = "openid profile email";
const OAUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const OAUTH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Explicit interpretation for pasted OpenAI/Codex credentials.
///
/// `auto` preserves the legacy import behavior: JSON is passed through, `at-*`
/// lines are validated as PATs, and all remaining lines are exchanged as RTs.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthMethod {
    #[default]
    Auto,
    RefreshToken,
    MobileRefreshToken,
    CodexSession,
    CodexPat,
}

impl AuthMethod {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Some(Self::Auto),
            "refresh_token" => Some(Self::RefreshToken),
            "mobile_refresh_token" => Some(Self::MobileRefreshToken),
            "codex_session" => Some(Self::CodexSession),
            "codex_pat" => Some(Self::CodexPat),
            _ => None,
        }
    }

    pub(crate) fn requires_network(self, content: &str) -> bool {
        match self {
            Self::Auto => auto_blob_requires_network(content),
            Self::RefreshToken | Self::MobileRefreshToken | Self::CodexPat => true,
            Self::CodexSession => false,
        }
    }

    fn refresh_client_id(self) -> Option<&'static str> {
        match self {
            Self::RefreshToken => Some(CODEX_OAUTH_CLIENT_ID),
            Self::MobileRefreshToken => Some(CODEX_MOBILE_OAUTH_CLIENT_ID),
            Self::Auto | Self::CodexSession | Self::CodexPat => None,
        }
    }
}

/// One credential expansion result, kept in source order.
///
/// Failures intentionally retain only a sanitized message: the original RT/PAT
/// must never be copied into API results or logs by the import aggregator.
#[derive(Debug)]
pub(crate) struct CodexExpansionItem {
    pub(crate) ordinal: usize,
    pub(crate) result:  Result<String, String>,
}

/// Exchange a Codex/OpenAI refresh token for a session-shaped JSON string.
async fn exchange_codex_refresh_token(
    client: &reqwest::Client,
    refresh_token: &str,
    client_id: &str,
) -> Result<String, ManagementError> {
    let refresh_token = refresh_token.trim();
    if refresh_token.is_empty() {
        return Err(ManagementError::InvalidRequest("refresh_token is empty"));
    }
    // note: dynamic upstream errors use Storage (InvalidRequest is &'static)
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        percent_encode(refresh_token),
        percent_encode(client_id)
    );
    let body = format!("{body}&scope={}", percent_encode(CODEX_REFRESH_SCOPES));
    let response = client
        .post(CODEX_OAUTH_TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .header("user-agent", CODEX_CLI_USER_AGENT)
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
    build_refresh_session(&payload, refresh_token, client_id)
}

fn build_refresh_session(
    payload: &JsonValue,
    refresh_token: &str,
    client_id: &str,
) -> Result<String, ManagementError> {
    let access_token = payload
        .get("access_token")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ManagementError::Storage("oauth response missing access_token".into()))?;
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
    out.insert("client_id".into(), json!(client_id));
    out.insert("tokens".into(), JsonValue::Object(tokens));
    serde_json::to_string(&JsonValue::Object(out)).map_err(storage_err)
}

/// Validate Codex `at-*` PAT via whoami; returns enriched session JSON (access_token only).
async fn validate_codex_personal_access_token(
    client: &reqwest::Client,
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
    tokens.insert(
        "expires_at".into(),
        json!(now_unix().saturating_add(365 * 24 * 3600)),
    );

    let mut out = JsonMap::new();
    out.insert("type".into(), json!("codex"));
    out.insert("email".into(), json!(email));
    out.insert("chatgpt_account_id".into(), json!(account_id));
    out.insert("chatgpt_user_id".into(), json!(user_id));
    if !plan_type.is_empty() {
        out.insert("plan_type".into(), json!(plan_type));
    }
    out.insert("tokens".into(), JsonValue::Object(tokens));
    serde_json::to_string(&JsonValue::Object(out)).map_err(storage_err)
}

/// Expand paste content into importable Codex session JSON blobs.
///
/// Explicit methods force one interpretation; `auto` keeps the legacy mixed
/// JSON/PAT/RT detection behavior.
pub(crate) async fn expand_codex_import_blob(
    content: &str,
    auth_method: AuthMethod,
    proxy_url: Option<&str>,
) -> Result<Vec<CodexExpansionItem>, ManagementError> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    if auth_method == AuthMethod::CodexSession {
        return Ok(vec![CodexExpansionItem {
            ordinal: 1,
            result:  Ok(trimmed.to_string()),
        }]);
    }

    // Whole JSON blobs remain compatible with the legacy auto path.
    if auth_method == AuthMethod::Auto && (trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return Ok(vec![CodexExpansionItem {
            ordinal: 1,
            result:  Ok(trimmed.to_string()),
        }]);
    }

    // Client construction is shared, but its failure is recorded per credential
    // so one batch-level setup error cannot erase already expanded entries.
    let client = oauth_client(proxy_url).map_err(|err| err.to_string());

    let mut out = Vec::new();
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let ordinal = out.len().saturating_add(1);
        if auth_method == AuthMethod::Auto && (line.starts_with('{') || line.starts_with('[')) {
            out.push(CodexExpansionItem {
                ordinal,
                result: Ok(line.to_string()),
            });
            continue;
        }
        let result = match &client {
            Ok(client) => match auth_method {
                AuthMethod::CodexPat => validate_codex_personal_access_token(client, line).await,
                AuthMethod::Auto if line.starts_with("at-") => {
                    validate_codex_personal_access_token(client, line).await
                }
                AuthMethod::RefreshToken | AuthMethod::MobileRefreshToken => {
                    let client_id = auth_method
                        .refresh_client_id()
                        .expect("refresh methods always have a client id");
                    exchange_codex_refresh_token(client, line, client_id).await
                }
                AuthMethod::Auto => {
                    exchange_codex_refresh_token(client, line, CODEX_OAUTH_CLIENT_ID).await
                }
                AuthMethod::CodexSession => unreachable!("handled before line expansion"),
            }
            .map_err(|err| sanitize_credential_error(&err.to_string(), line)),
            Err(err) => Err(sanitize_credential_error(err, line)),
        };
        out.push(CodexExpansionItem { ordinal, result });
    }
    if out.is_empty() {
        return Err(ManagementError::InvalidRequest(
            "no refresh tokens, PAT (at-*), or session JSON found",
        ));
    }
    // ok
    Ok(out)
}

fn sanitize_credential_error(message: &str, credential: &str) -> String {
    let credential = credential.trim();
    let mut sanitized = message.to_string();
    if !credential.is_empty() {
        sanitized = sanitized.replace(credential, "[redacted]");
        let encoded = percent_encode(credential);
        if encoded != credential {
            sanitized = sanitized.replace(&encoded, "[redacted]");
        }
    }
    if sanitized.trim().is_empty() {
        "credential processing failed".to_string()
    } else {
        sanitized
    }
}

fn auto_blob_requires_network(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() || trimmed.starts_with('{') || trimmed.starts_with('[') {
        return false;
    }
    trimmed.lines().map(str::trim).any(|line| {
        !line.is_empty()
            && !line.starts_with('#')
            && !line.starts_with('{')
            && !line.starts_with('[')
    })
}

fn oauth_client(proxy_url: Option<&str>) -> Result<reqwest::Client, ManagementError> {
    let mut builder = reqwest::Client::builder()
        .timeout(OAUTH_REQUEST_TIMEOUT)
        .connect_timeout(OAUTH_CONNECT_TIMEOUT);
    if let Some(proxy_url) = proxy_url.map(str::trim).filter(|url| !url.is_empty()) {
        let proxy = reqwest::Proxy::all(proxy_url)
            .map_err(|_| ManagementError::InvalidRequest("selected proxy URL is invalid"))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|_| ManagementError::Storage("failed to build OAuth HTTP client".into()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_method_selects_expected_client_id() {
        assert_eq!(
            AuthMethod::RefreshToken.refresh_client_id(),
            Some(CODEX_OAUTH_CLIENT_ID)
        );
        assert_eq!(
            AuthMethod::MobileRefreshToken.refresh_client_id(),
            Some(CODEX_MOBILE_OAUTH_CLIENT_ID)
        );
        assert_eq!(AuthMethod::CodexSession.refresh_client_id(), None);
        assert_eq!(AuthMethod::CodexPat.refresh_client_id(), None);
    }

    #[test]
    fn refresh_session_persists_client_id_and_rotated_tokens() {
        let session = build_refresh_session(
            &json!({
                "access_token": "at-new",
                "refresh_token": "rt-new",
                "id_token": "id-new",
                "expires_in": 3600
            }),
            "rt-old",
            CODEX_MOBILE_OAUTH_CLIENT_ID,
        )
        .expect("session");
        let value: JsonValue = serde_json::from_str(&session).expect("json");
        assert_eq!(
            value.get("client_id").and_then(JsonValue::as_str),
            Some(CODEX_MOBILE_OAUTH_CLIENT_ID)
        );
        assert_eq!(
            value
                .pointer("/tokens/access_token")
                .and_then(JsonValue::as_str),
            Some("at-new")
        );
        assert_eq!(
            value
                .pointer("/tokens/refresh_token")
                .and_then(JsonValue::as_str),
            Some("rt-new")
        );
        let key = crate::control_api_ext::parse_flexible_codex_key(&session).expect("key");
        assert_eq!(key.client_id.as_deref(), Some(CODEX_MOBILE_OAUTH_CLIENT_ID));
    }

    #[test]
    fn explicit_session_avoids_network_while_auto_tokens_require_it() {
        assert!(!AuthMethod::CodexSession.requires_network("raw-access-token"));
        assert!(!AuthMethod::Auto.requires_network(r#"{"tokens":{}}"#));
        assert!(AuthMethod::Auto.requires_network("rt-one\nrt-two"));
        assert!(AuthMethod::CodexPat.requires_network("at-token"));
    }

    #[test]
    fn credential_errors_are_redacted_without_dropping_other_results() {
        let secret = "rt-secret-value";
        let items = [
            CodexExpansionItem {
                ordinal: 1,
                result:  Ok("session-json".into()),
            },
            CodexExpansionItem {
                ordinal: 2,
                result:  Err(sanitize_credential_error(
                    &format!("exchange rejected {secret}"),
                    secret,
                )),
            },
        ];

        assert!(items[0].result.is_ok());
        let error = items[1].result.as_ref().expect_err("second item fails");
        assert!(!error.contains(secret));
        assert!(error.contains("[redacted]"));
    }
}
