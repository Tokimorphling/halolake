//! Parse sub2api / Codex CLI auth files into channel OAuth keys.
//!
//! Compatible with `ref/sub2api` `ImportCodexSession` input shapes:
//! - raw access token (string / line)
//! - JSON object with nested `tokens.*` or flat `access_token` / `accessToken`
//! - JSON array / multi-value JSON stream / line-delimited mix
//!
//! Stored channel key matches existing Codex OAuth JSON used by `channel_special`.

use data_encoding::BASE64URL_NOPAD;
use halolake_control_plane::ManagementError;
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::{
    io::Cursor,
    time::{SystemTime, UNIX_EPOCH},
};

const CLOCK_SKEW_SECS: i64 = 120;
pub(crate) const CHANNEL_TYPE_CODEX: i32 = 57;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct CodexOAuthKey {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) id_token:      Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) access_token:  Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) account_id:    Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_refresh:  Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) email:         Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub(crate) key_type:      Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expired:       Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CodexAuthImportRequest {
    #[serde(default)]
    pub(crate) content:         String,
    #[serde(default)]
    pub(crate) contents:        Vec<String>,
    #[serde(default)]
    pub(crate) name:            String,
    #[serde(default)]
    pub(crate) group:           Option<String>,
    #[serde(default)]
    pub(crate) models:          Option<String>,
    #[serde(default)]
    pub(crate) base_url:        Option<String>,
    #[serde(default)]
    pub(crate) proxy_id:        Option<u64>,
    #[serde(default)]
    pub(crate) priority:        Option<i64>,
    #[serde(default)]
    pub(crate) weight:          Option<u32>,
    #[serde(default = "default_true")]
    pub(crate) update_existing: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CodexAuthImportResult {
    pub(crate) total:    usize,
    pub(crate) created:  usize,
    pub(crate) updated:  usize,
    pub(crate) skipped:  usize,
    pub(crate) failed:   usize,
    pub(crate) items:    Vec<CodexAuthImportItem>,
    pub(crate) warnings: Vec<CodexAuthImportMessage>,
    pub(crate) errors:   Vec<CodexAuthImportMessage>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CodexAuthImportItem {
    pub(crate) index:      usize,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) name:       String,
    pub(crate) action:     String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) channel_id: Option<u64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) message:    String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CodexAuthImportMessage {
    pub(crate) index:   usize,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) name:    String,
    pub(crate) message: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedCodexAuth {
    pub(crate) name:                  String,
    pub(crate) key:                   CodexOAuthKey,
    pub(crate) email:                 String,
    #[allow(dead_code)]
    pub(crate) account_id:            String,
    #[allow(dead_code)]
    pub(crate) user_id:               String,
    #[allow(dead_code)]
    pub(crate) access_token:          String,
    #[allow(dead_code)]
    pub(crate) refresh_token:         String,
    pub(crate) identity_keys:         Vec<String>,
    pub(crate) warnings:              Vec<String>,
    #[allow(dead_code)]
    pub(crate) token_expires_at_unix: Option<i64>,
}

pub(crate) fn collect_entries(
    req: &CodexAuthImportRequest,
) -> Result<Vec<ParsedCodexAuth>, ManagementError> {
    let mut blobs = Vec::new();
    if !req.content.trim().is_empty() {
        blobs.push(req.content.as_str());
    }
    for content in &req.contents {
        if !content.trim().is_empty() {
            blobs.push(content.as_str());
        }
    }
    if blobs.is_empty() {
        return Err(ManagementError::InvalidRequest(
            "provide accessToken or Codex session JSON in content/contents",
        ));
    }
    let mut out = Vec::new();
    for blob in blobs {
        out.extend(normalize_codex_auth_blob(blob)?);
    }
    if out.is_empty() {
        return Err(ManagementError::InvalidRequest(
            "provide accessToken or Codex session JSON in content/contents",
        ));
    }
    Ok(out)
}

pub(crate) fn normalize_codex_auth_blob(
    content: &str,
) -> Result<Vec<ParsedCodexAuth>, ManagementError> {
    let entries = parse_import_content(content)?;
    let mut out = Vec::with_capacity(entries.len());
    for (idx, value) in entries.into_iter().enumerate() {
        out.push(normalize_entry(idx + 1, value)?);
    }
    Ok(out)
}

/// Accept flat CodexOAuthKey JSON or nested sub2api auth JSON.
pub(crate) fn parse_flexible_codex_key(raw: &str) -> Result<CodexOAuthKey, ManagementError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ManagementError::InvalidRequest(
            "codex channel: empty oauth key",
        ));
    }
    if let Ok(key) = serde_json::from_str::<CodexOAuthKey>(trimmed) {
        if key
            .access_token
            .as_deref()
            .map(str::trim)
            .is_some_and(|token| !token.is_empty())
        {
            return Ok(key);
        }
    }
    let parsed = normalize_codex_auth_blob(trimmed)?;
    parsed
        .into_iter()
        .next()
        .map(|item| item.key)
        .ok_or(ManagementError::InvalidRequest(
            "codex channel: invalid oauth key json",
        ))
}

pub(crate) fn codex_key_to_json(key: &CodexOAuthKey) -> Result<String, ManagementError> {
    serde_json::to_string(key).map_err(|err| ManagementError::Storage(err.to_string()))
}

pub(crate) fn identity_keys_for_channel_key(raw_key: &str) -> Vec<String> {
    match parse_flexible_codex_key(raw_key) {
        Ok(key) => build_stored_identity_keys(
            key.account_id.as_deref().unwrap_or_default(),
            "",
            key.email.as_deref().unwrap_or_default(),
            key.access_token.as_deref().unwrap_or_default(),
        ),
        Err(_) => Vec::new(),
    }
}

pub(crate) fn find_existing_channel_id(
    channels: &[halolake_domain::ChannelRecord],
    item: &ParsedCodexAuth,
) -> Option<u64> {
    for channel in channels {
        if channel.channel_type != CHANNEL_TYPE_CODEX {
            continue;
        }
        let keys = identity_keys_for_channel_key(&channel.key);
        for key in &item.identity_keys {
            if keys.iter().any(|existing| existing == key) {
                return Some(channel.id);
            }
        }
    }
    None
}

fn parse_import_content(content: &str) -> Result<Vec<JsonValue>, ManagementError> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if looks_like_json(trimmed) {
        match decode_json_stream(trimmed) {
            Ok(values) => return Ok(flatten_values(values)),
            Err(_) => {
                if trimmed.contains('\n') {
                    if let Ok(lines) = parse_import_lines(trimmed) {
                        return Ok(lines);
                    }
                }
                return Err(ManagementError::InvalidRequest(
                    "JSON parse failed for auth content",
                ));
            }
        }
    }
    parse_import_lines(trimmed)
}

fn parse_import_lines(content: &str) -> Result<Vec<JsonValue>, ManagementError> {
    let mut values = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if looks_like_json(line) {
            let decoded = decode_json_stream(line).map_err(|_| {
                ManagementError::InvalidRequest("line JSON parse failed for auth content")
            })?;
            values.extend(flatten_values(decoded));
        } else {
            values.push(JsonValue::String(line.to_string()));
        }
    }
    Ok(values)
}

fn decode_json_stream(content: &str) -> Result<Vec<JsonValue>, serde_json::Error> {
    let mut de = serde_json::Deserializer::from_reader(Cursor::new(content.as_bytes()));
    let mut values = Vec::new();
    loop {
        match JsonValue::deserialize(&mut de) {
            Ok(value) => values.push(value),
            Err(err) if err.is_eof() && !values.is_empty() => break,
            Err(err) => return Err(err),
        }
    }
    if values.is_empty() {
        return Err(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty JSON content",
        )));
    }
    Ok(values)
}

fn flatten_values(values: Vec<JsonValue>) -> Vec<JsonValue> {
    let mut out = Vec::new();
    fn walk(value: JsonValue, out: &mut Vec<JsonValue>) {
        match value {
            JsonValue::Array(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            other => out.push(other),
        }
    }
    for value in values {
        walk(value, &mut out);
    }
    out
}

fn normalize_entry(index: usize, value: JsonValue) -> Result<ParsedCodexAuth, ManagementError> {
    let now = now_unix();
    let mut warnings = Vec::new();
    let mut email = String::new();
    let mut account_id = String::new();
    let mut user_id = String::new();
    let mut plan_type = String::new();
    let mut organization = String::new();
    let mut name_hint = String::new();
    let access_token;
    let mut refresh_token = String::new();
    let mut id_token = String::new();
    let mut token_expires_at_unix: Option<i64> = None;

    match value {
        JsonValue::String(raw) => {
            access_token = raw.trim().to_string();
        }
        JsonValue::Object(map) => {
            access_token = first_string(&map, &[
                &["tokens", "access_token"],
                &["tokens", "accessToken"],
                &["access_token"],
                &["accessToken"],
                &["token"],
            ]);
            refresh_token = first_string(&map, &[
                &["tokens", "refresh_token"],
                &["tokens", "refreshToken"],
                &["refresh_token"],
                &["refreshToken"],
            ]);
            id_token = first_string(&map, &[
                &["tokens", "id_token"],
                &["tokens", "idToken"],
                &["id_token"],
                &["idToken"],
            ]);
            email = first_string(&map, &[&["email"], &["user", "email"]]);
            account_id = first_string(&map, &[
                &["chatgpt_account_id"],
                &["chatgptAccountId"],
                &["account_id"],
                &["accountId"],
                &["account", "id"],
                &["account", "account_id"],
                &["account", "chatgpt_account_id"],
            ]);
            user_id = first_string(&map, &[
                &["chatgpt_user_id"],
                &["chatgptUserId"],
                &["user_id"],
                &["userId"],
                &["user", "id"],
            ]);
            plan_type = first_string(&map, &[
                &["plan_type"],
                &["planType"],
                &["account", "plan_type"],
                &["account", "planType"],
            ]);
            organization = first_string(&map, &[
                &["organization_id"],
                &["organizationId"],
                &["org_id"],
                &["orgId"],
            ]);
            name_hint = first_string(&map, &[&["name"], &["user", "name"]]);
            if !first_string(&map, &[&["session_token"], &["sessionToken"]]).is_empty() {
                warnings
                    .push("sessionToken ignored; not stored as OAuth refresh_token".to_string());
            }
            if let Some(exp) = first_time(&map, &[
                &["tokens", "expires_at"],
                &["tokens", "expiresAt"],
                &["expires_at"],
                &["expiresAt"],
            ]) {
                if exp <= now - CLOCK_SKEW_SECS {
                    return Err(ManagementError::InvalidRequest(
                        "access_token already expired",
                    ));
                }
                token_expires_at_unix = Some(exp);
            }
        }
        _ => {
            return Err(ManagementError::InvalidRequest(
                "auth entry has unsupported format",
            ));
        }
    }

    if access_token.is_empty() {
        return Err(ManagementError::InvalidRequest(
            "missing accessToken/access_token",
        ));
    }

    if !id_token.is_empty() {
        let _ = enrich_from_jwt(
            &id_token,
            false,
            now,
            &mut email,
            &mut account_id,
            &mut user_id,
            &mut plan_type,
            &mut organization,
            &mut token_expires_at_unix,
            &mut warnings,
        );
    }
    enrich_from_jwt(
        &access_token,
        true,
        now,
        &mut email,
        &mut account_id,
        &mut user_id,
        &mut plan_type,
        &mut organization,
        &mut token_expires_at_unix,
        &mut warnings,
    )?;

    if token_expires_at_unix.is_none() {
        warnings
            .push("could not parse access_token expiry; verify token validity after import".into());
    }
    if refresh_token.is_empty() {
        warnings.push("no refresh_token; access_token cannot be auto-renewed after expiry".into());
        if token_expires_at_unix.is_none() {
            return Err(ManagementError::InvalidRequest(
                "no refresh_token and access_token expiry unknown",
            ));
        }
    }

    let name = first_non_empty([&name_hint, &email, &account_id, &user_id])
        .unwrap_or_else(|| format!("Codex import {index}"));

    let key = CodexOAuthKey {
        id_token:      non_empty(id_token),
        access_token:  Some(access_token.clone()),
        refresh_token: non_empty(refresh_token.clone()),
        account_id:    non_empty(account_id.clone()),
        last_refresh:  None,
        email:         non_empty(email.clone()),
        key_type:      Some("codex".to_string()),
        expired:       token_expires_at_unix.map(|ts| ts.to_string()),
    };

    let identity_keys =
        build_identity_keys(&account_id, &user_id, &email, &access_token, &refresh_token);

    Ok(ParsedCodexAuth {
        name,
        key,
        email,
        account_id,
        user_id,
        access_token,
        refresh_token,
        identity_keys,
        warnings,
        token_expires_at_unix,
    })
}

fn enrich_from_jwt(
    token: &str,
    validate_expiry: bool,
    now: i64,
    email: &mut String,
    account_id: &mut String,
    user_id: &mut String,
    plan_type: &mut String,
    organization: &mut String,
    token_expires_at_unix: &mut Option<i64>,
    warnings: &mut Vec<String>,
) -> Result<(), ManagementError> {
    let Some(claims) = decode_jwt_claims(token) else {
        if validate_expiry {
            warnings
                .push("accessToken is not a parseable JWT; cannot verify expiry/identity".into());
        }
        return Ok(());
    };
    if validate_expiry {
        if let Some(exp) = claims.exp {
            if now > exp + CLOCK_SKEW_SECS {
                return Err(ManagementError::InvalidRequest(
                    "access_token already expired",
                ));
            }
            *token_expires_at_unix = Some(match *token_expires_at_unix {
                Some(cur) => cur.min(exp),
                None => exp,
            });
        }
    }
    if email.is_empty() {
        if let Some(value) = claims.email.filter(|s| !s.is_empty()) {
            *email = value;
        }
    }
    if let Some(auth) = claims.openai_auth {
        if account_id.is_empty() {
            if let Some(value) = auth.chatgpt_account_id.filter(|s| !s.is_empty()) {
                *account_id = value;
            }
        }
        if user_id.is_empty() {
            if let Some(value) = auth
                .chatgpt_user_id
                .or(auth.user_id)
                .filter(|s| !s.is_empty())
            {
                *user_id = value;
            }
        }
        if plan_type.is_empty() {
            if let Some(value) = auth.chatgpt_plan_type.filter(|s| !s.is_empty()) {
                *plan_type = value;
            }
        }
        if organization.is_empty() {
            if let Some(value) = auth.poid.filter(|s| !s.is_empty()) {
                *organization = value;
            } else if let Some(orgs) = auth.organizations {
                if let Some(default) = orgs.iter().find(|org| org.is_default) {
                    *organization = default.id.clone();
                } else if let Some(first) = orgs.first() {
                    *organization = first.id.clone();
                }
            }
        }
    }
    if user_id.is_empty() {
        if let Some(value) = claims.sub.filter(|s| !s.is_empty()) {
            *user_id = value;
        }
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
struct JwtClaims {
    #[serde(default)]
    sub:         Option<String>,
    #[serde(default)]
    email:       Option<String>,
    #[serde(default)]
    exp:         Option<i64>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    openai_auth: Option<OpenAiAuthClaims>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiAuthClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    chatgpt_user_id:    Option<String>,
    #[serde(default)]
    chatgpt_plan_type:  Option<String>,
    #[serde(default)]
    user_id:            Option<String>,
    #[serde(default)]
    poid:               Option<String>,
    #[serde(default)]
    organizations:      Option<Vec<OrgClaim>>,
}

#[derive(Debug, Default, Deserialize)]
struct OrgClaim {
    #[serde(default)]
    id:         String,
    #[serde(default)]
    is_default: bool,
}

fn decode_jwt_claims(token: &str) -> Option<JwtClaims> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let bytes = decode_jwt_segment(parts[1])?;
    serde_json::from_slice(&bytes).ok()
}

fn decode_jwt_segment(segment: &str) -> Option<Vec<u8>> {
    if let Ok(bytes) = BASE64URL_NOPAD.decode(segment.as_bytes()) {
        return Some(bytes);
    }
    let mut padded = segment.to_string();
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }
    BASE64URL_NOPAD
        .decode(padded.trim_end_matches('=').as_bytes())
        .ok()
        .or_else(|| {
            use data_encoding::BASE64URL;
            BASE64URL.decode(padded.as_bytes()).ok()
        })
}

pub(crate) fn build_identity_keys(
    account_id: &str,
    user_id: &str,
    email: &str,
    access_token: &str,
    refresh_token: &str,
) -> Vec<String> {
    let access_token = access_token.trim();
    let refresh_token = refresh_token.trim();
    if refresh_token.is_empty() && !access_token.is_empty() {
        return vec![format!("access:{}", token_fingerprint(access_token))];
    }
    build_stored_identity_keys(account_id, user_id, email, access_token)
}

pub(crate) fn build_stored_identity_keys(
    account_id: &str,
    user_id: &str,
    email: &str,
    access_token: &str,
) -> Vec<String> {
    let mut keys = Vec::new();
    let account_id = account_id.trim();
    let user_id = user_id.trim();
    let access_token = access_token.trim();
    if !user_id.is_empty() {
        keys.push(format!("user:{user_id}"));
    }
    if account_id.is_empty() && user_id.is_empty() {
        let email = email.trim().to_ascii_lowercase();
        if !email.is_empty() {
            keys.push(format!("email:{email}"));
        }
    }
    if !access_token.is_empty() {
        keys.push(format!("access:{}", token_fingerprint(access_token)));
    }
    if !account_id.is_empty() {
        keys.push(format!("account:{account_id}"));
    }
    keys
}

pub(crate) fn token_fingerprint(token: &str) -> String {
    let digest = Sha256::digest(token.trim().as_bytes());
    hex_encode(&digest)
}

fn first_string(map: &JsonMap<String, JsonValue>, paths: &[&[&str]]) -> String {
    for path in paths {
        if let Some(value) = path_value(map, path) {
            let text = string_value(value);
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

fn first_time(map: &JsonMap<String, JsonValue>, paths: &[&[&str]]) -> Option<i64> {
    for path in paths {
        if let Some(value) = path_value(map, path) {
            if let Some(ts) = parse_time_value(value) {
                return Some(ts);
            }
        }
    }
    None
}

fn path_value<'a>(map: &'a JsonMap<String, JsonValue>, path: &[&str]) -> Option<&'a JsonValue> {
    let mut current = map.get(*path.first()?)?;
    for key in path.iter().skip(1) {
        current = current.as_object()?.get(*key)?;
    }
    Some(current)
}

fn string_value(value: &JsonValue) -> String {
    match value {
        JsonValue::String(s) => s.trim().to_string(),
        JsonValue::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn parse_time_value(value: &JsonValue) -> Option<i64> {
    match value {
        JsonValue::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            if let Ok(n) = s.parse::<i64>() {
                return Some(normalize_unix(n));
            }
            parse_rfc3339_to_unix(s)
        }
        JsonValue::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .map(normalize_unix),
        _ => None,
    }
}

fn parse_rfc3339_to_unix(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T').or_else(|| s.split_once('t'))?;
    let mut date_parts = date.split('-');
    let year: i32 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    let rest = rest.trim_end_matches('Z').trim_end_matches('z');
    let (time_part, offset_secs) = split_time_and_offset(rest)?;
    let time_part = time_part.split('.').next()?;
    let mut tp = time_part.split(':');
    let hour: u32 = tp.next()?.parse().ok()?;
    let min: u32 = tp.next()?.parse().ok()?;
    let sec: u32 = tp.next().unwrap_or("0").parse().ok()?;
    let days = days_from_civil(year, month, day)?;
    let secs = days * 86400 + i64::from(hour) * 3600 + i64::from(min) * 60 + i64::from(sec);
    Some(secs - offset_secs)
}

fn split_time_and_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(idx) = rest.rfind('+') {
        let (time, off) = rest.split_at(idx);
        return Some((time, parse_offset(off)?));
    }
    if let Some(idx) = rest[1..].rfind('-').map(|i| i + 1) {
        let (time, off) = rest.split_at(idx);
        if off.starts_with('-') && off.len() >= 6 {
            return Some((time, parse_offset(off)?));
        }
    }
    Some((rest, 0))
}

fn parse_offset(off: &str) -> Option<i64> {
    let sign = if off.starts_with('+') {
        1i64
    } else if off.starts_with('-') {
        -1
    } else {
        return None;
    };
    let body = &off[1..];
    let (h, m) = if let Some((h, m)) = body.split_once(':') {
        (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)
    } else if body.len() == 4 {
        (
            body[..2].parse::<i64>().ok()?,
            body[2..].parse::<i64>().ok()?,
        )
    } else {
        return None;
    };
    Some(sign * (h * 3600 + m * 60))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || day == 0 || day > 31 {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u32;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(i64::from(era) * 146097 + i64::from(doe) - 719468)
}

fn normalize_unix(value: i64) -> i64 {
    if value > 1_000_000_000_000 {
        value / 1000
    } else {
        value
    }
}

fn looks_like_json(content: &str) -> bool {
    matches!(content.as_bytes().first(), Some(b'{') | Some(b'['))
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn first_non_empty<'a>(values: impl IntoIterator<Item = &'a String>) -> Option<String> {
    values
        .into_iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_jwt(exp_offset_secs: i64, auth: JsonValue) -> String {
        let header = BASE64URL_NOPAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let exp = now_unix() + exp_offset_secs;
        let mut payload = json!({
            "sub": "user-sub",
            "email": "claim@example.com",
            "exp": exp,
        });
        if let JsonValue::Object(map) = &mut payload {
            map.insert("https://api.openai.com/auth".into(), auth);
        }
        let payload_b64 = BASE64URL_NOPAD.encode(payload.to_string().as_bytes());
        format!("{header}.{payload_b64}.sig")
    }

    #[test]
    fn parses_nested_tokens_auth_file() {
        let access = test_jwt(
            3600,
            json!({
                "chatgpt_account_id": "acct-1",
                "chatgpt_user_id": "user-1",
                "chatgpt_plan_type": "plus",
            }),
        );
        let raw = format!(
            r#"{{"tokens":{{"access_token":"{access}","refresh_token":"rt-1"}},"email":"file@example.com","chatgpt_account_id":"acct-file"}}"#
        );
        let items = normalize_codex_auth_blob(&raw).expect("parse");
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.key.access_token.as_deref(), Some(access.as_str()));
        assert_eq!(item.key.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(item.key.account_id.as_deref(), Some("acct-file"));
        assert_eq!(item.key.email.as_deref(), Some("file@example.com"));
        assert_eq!(item.key.key_type.as_deref(), Some("codex"));
    }

    #[test]
    fn parses_line_raw_and_json() {
        let access = test_jwt(3600, json!({}));
        let content = format!("{access}\n{{\"accessToken\":\"{access}\"}}");
        let items = normalize_codex_auth_blob(&content).expect("parse");
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn flexible_key_accepts_flat_and_nested() {
        let access = test_jwt(3600, json!({"chatgpt_account_id": "a1"}));
        let flat = format!(
            r#"{{"access_token":"{access}","refresh_token":"r","account_id":"a1","type":"codex"}}"#
        );
        let key = parse_flexible_codex_key(&flat).expect("flat");
        assert_eq!(key.account_id.as_deref(), Some("a1"));

        let nested = format!(
            r#"{{"tokens":{{"access_token":"{access}","refresh_token":"r2"}},"account_id":"a2"}}"#
        );
        let key = parse_flexible_codex_key(&nested).expect("nested");
        assert_eq!(key.refresh_token.as_deref(), Some("r2"));
        assert_eq!(key.account_id.as_deref(), Some("a2"));
    }
}
