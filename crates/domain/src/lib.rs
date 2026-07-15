use serde::{Deserialize, Serialize};

pub const STATUS_ENABLED: i32 = 1;
pub const STATUS_MANUALLY_DISABLED: i32 = 2;
pub const STATUS_AUTO_DISABLED: i32 = 3;
pub const ROLE_COMMON_USER: i32 = 1;
pub const ROLE_ADMIN_USER: i32 = 10;
pub const ROLE_ROOT_USER: i32 = 100;

pub const CHANNEL_TYPE_OPENAI: i32 = 1;
pub const CHANNEL_TYPE_ANTHROPIC: i32 = 14;
pub const CHANNEL_TYPE_GEMINI: i32 = 24;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct UserRecord {
    #[serde(default)]
    pub id:            u64,
    pub username:      String,
    #[serde(default, skip_serializing)]
    pub password:      String,
    #[serde(default, skip_serializing)]
    pub access_token:  Option<String>,
    #[serde(default)]
    pub display_name:  String,
    #[serde(default)]
    pub role:          i32,
    #[serde(default = "default_enabled_status")]
    pub status:        i32,
    #[serde(default)]
    pub email:         String,
    #[serde(default)]
    pub quota:         i64,
    #[serde(default)]
    pub used_quota:    i64,
    #[serde(default = "default_group")]
    pub group:         String,
    #[serde(default)]
    pub setting:       String,
    #[serde(default)]
    pub remark:        String,
    #[serde(default)]
    pub created_at:    i64,
    #[serde(default)]
    pub last_login_at: i64,
}

impl UserRecord {
    pub fn sanitized(mut self) -> Self {
        self.password.clear();
        self.access_token = None;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TokenRecord {
    #[serde(default)]
    pub id:                   u64,
    #[serde(default, skip)]
    pub snapshot_id:          Option<String>,
    #[serde(default)]
    pub user_id:              u64,
    #[serde(default, skip)]
    pub snapshot_user_id:     Option<String>,
    #[serde(default)]
    pub key:                  String,
    #[serde(default = "default_enabled_status")]
    pub status:               i32,
    #[serde(default)]
    pub name:                 String,
    #[serde(default)]
    pub created_time:         i64,
    #[serde(default)]
    pub accessed_time:        i64,
    #[serde(default = "default_never_expires")]
    pub expired_time:         i64,
    #[serde(default)]
    pub remain_quota:         i64,
    #[serde(default)]
    pub unlimited_quota:      bool,
    #[serde(default)]
    pub model_limits_enabled: bool,
    #[serde(default)]
    pub model_limits:         String,
    #[serde(default)]
    pub allow_ips:            Option<String>,
    #[serde(default)]
    pub used_quota:           i64,
    #[serde(default)]
    pub group:                String,
    #[serde(default)]
    pub cross_group_retry:    bool,
}

impl TokenRecord {
    pub fn masked(mut self) -> Self {
        self.key = mask_token_key(&self.key);
        self
    }

    pub fn allowed_models(&self) -> Vec<String> {
        if !self.model_limits_enabled || self.model_limits.is_empty() {
            return Vec::new();
        }
        self.model_limits
            .split(',')
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_string)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ChannelRecord {
    #[serde(default)]
    pub id:                   u64,
    #[serde(default, skip)]
    pub snapshot_id:          Option<String>,
    #[serde(rename = "type", default = "default_openai_channel_type")]
    pub channel_type:         i32,
    pub key:                  String,
    #[serde(default = "default_enabled_status")]
    pub status:               i32,
    #[serde(default)]
    pub name:                 String,
    #[serde(default)]
    pub weight:               Option<u32>,
    #[serde(default)]
    pub created_time:         i64,
    #[serde(default)]
    pub test_time:            i64,
    #[serde(default)]
    pub response_time:        i32,
    #[serde(default)]
    pub base_url:             Option<String>,
    #[serde(default)]
    pub balance:              f64,
    #[serde(default)]
    pub balance_updated_time: i64,
    #[serde(default)]
    pub models:               String,
    #[serde(default = "default_group")]
    pub group:                String,
    #[serde(default)]
    pub used_quota:           i64,
    #[serde(default)]
    pub model_mapping:        Option<String>,
    #[serde(default)]
    pub priority:             Option<i64>,
    #[serde(default)]
    pub auto_ban:             Option<i32>,
    #[serde(default)]
    pub tag:                  Option<String>,
    #[serde(default)]
    pub setting:              Option<String>,
    #[serde(default)]
    pub param_override:       Option<String>,
    #[serde(default)]
    pub header_override:      Option<String>,
    #[serde(default)]
    pub remark:               Option<String>,
    #[serde(default)]
    pub proxy_id:             Option<u64>,
}

impl ChannelRecord {
    pub fn masked(mut self) -> Self {
        self.key.clear();
        redact_json_option(&mut self.setting);
        redact_json_option(&mut self.param_override);
        redact_json_option(&mut self.header_override);
        self
    }

    pub fn model_list(&self) -> Vec<String> {
        self.models
            .trim_matches(',')
            .split(',')
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_string)
            .collect()
    }

    pub fn group_list(&self) -> Vec<String> {
        self.group
            .trim_matches(',')
            .split(',')
            .map(str::trim)
            .filter(|group| !group.is_empty())
            .map(str::to_string)
            .collect()
    }
}

fn redact_json_option(raw: &mut Option<String>) {
    let Some(source) = raw.as_ref() else {
        return;
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(source) else {
        // Malformed legacy JSON cannot be inspected safely. Hide the entire
        // field from API responses; the control-plane update merge preserves
        // the stored raw value when this masked field is omitted on round-trip.
        *raw = None;
        return;
    };
    if !redact_json_secrets(&mut value) {
        return;
    }
    if let Ok(redacted) = serde_json::to_string(&value) {
        *raw = Some(redacted);
    }
}

fn redact_json_secrets(value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            let mut changed = false;
            for (key, value) in object {
                if is_sensitive_json_key(key) {
                    *value = serde_json::Value::String(String::new());
                    changed = true;
                } else {
                    changed |= redact_json_secrets(value);
                }
            }
            changed
        }
        serde_json::Value::Array(values) => {
            let mut changed = false;
            for value in values {
                changed |= redact_json_secrets(value);
            }
            changed
        }
        _ => false,
    }
}

fn is_sensitive_json_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let compact: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect();

    matches!(
        compact.as_str(),
        "authorization"
            | "proxyauthorization"
            | "apikey"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "password"
            | "secret"
            | "clientsecret"
            | "privatekey"
            | "secretkey"
            | "proxy"
            | "proxyurl"
            | "cookie"
            | "setcookie"
    ) || key.ends_with("_token")
        || key.ends_with("-token")
        || key.ends_with("_password")
        || key.ends_with("-password")
        || key.ends_with("_secret")
        || key.ends_with("-secret")
        || key.ends_with("_api_key")
        || key.ends_with("-api-key")
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PageRequest {
    #[serde(default = "default_page")]
    pub page:      usize,
    #[serde(default = "default_page_size")]
    pub page_size: usize,
}

impl PageRequest {
    pub fn offset(self) -> usize {
        self.page.saturating_sub(1) * self.limit()
    }

    pub fn limit(self) -> usize {
        self.page_size.clamp(1, 100)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SearchRequest {
    #[serde(flatten)]
    pub page:    PageRequest,
    #[serde(default)]
    pub keyword: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PageResult<T> {
    pub items:     Vec<T>,
    pub total:     usize,
    pub page:      usize,
    pub page_size: usize,
}

impl<T> PageResult<T> {
    pub fn new(items: Vec<T>, total: usize, page: PageRequest) -> Self {
        Self {
            items,
            total,
            page: page.page,
            page_size: page.limit(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    Success,
    ClientError,
    UpstreamError,
    GatewayError,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct UsageEvent {
    pub request_id:            String,
    pub user_id:               String,
    pub token_id:              String,
    pub channel_id:            String,
    #[serde(default)]
    pub group:                 String,
    pub model:                 String,
    pub upstream_model:        String,
    #[serde(default)]
    pub prompt_tokens:         Option<u64>,
    #[serde(default)]
    pub completion_tokens:     Option<u64>,
    #[serde(default)]
    pub total_tokens:          Option<u64>,
    #[serde(default)]
    pub cache_read_tokens:     Option<u64>,
    #[serde(default)]
    pub cache_creation_tokens: Option<u64>,
    #[serde(default)]
    pub image_tokens:          Option<u64>,
    #[serde(default)]
    pub audio_tokens:          Option<u64>,
    #[serde(default)]
    pub quota:                 Option<i64>,
    pub status:                UsageStatus,
    pub latency_ms:            u64,
    /// Time-to-first-response in milliseconds (new-api log `other.frt`).
    /// Stream: first upstream body chunk; non-stream: often unset.
    #[serde(default)]
    pub first_response_ms:     Option<u64>,
    #[serde(default)]
    pub is_stream:             bool,
    #[serde(default)]
    pub ip:                    String,
    #[serde(default)]
    pub upstream_request_id:   String,
    pub created_at_unix_ms:    i64,
}

impl UsageEvent {
    pub fn observed_tokens(&self) -> u64 {
        self.total_tokens
            .or_else(|| {
                Some(self.prompt_tokens.unwrap_or(0) + self.completion_tokens.unwrap_or(0))
                    .filter(|tokens| *tokens > 0)
            })
            .unwrap_or(0)
    }
}

pub fn mask_token_key(key: &str) -> String {
    match key.len() {
        0 => String::new(),
        len if len <= 4 => "*".repeat(len),
        len if len <= 8 => format!("{}****{}", &key[..2], &key[len - 2..]),
        len => format!("{}**********{}", &key[..4], &key[len - 4..]),
    }
}

fn default_enabled_status() -> i32 {
    STATUS_ENABLED
}

fn default_openai_channel_type() -> i32 {
    CHANNEL_TYPE_OPENAI
}

fn default_never_expires() -> i64 {
    -1
}

fn default_group() -> String {
    "default".to_string()
}

fn default_page() -> usize {
    1
}

fn default_page_size() -> usize {
    10
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel_with_json_fields(
        setting: &str,
        param_override: &str,
        header_override: &str,
    ) -> ChannelRecord {
        serde_json::from_value(serde_json::json!({
            "key": "top-level-api-key",
            "setting": setting,
            "param_override": param_override,
            "header_override": header_override,
        }))
        .expect("channel fixture should deserialize")
    }

    #[test]
    fn masks_token_keys_like_new_api() {
        assert_eq!(mask_token_key(""), "");
        assert_eq!(mask_token_key("abc"), "***");
        assert_eq!(mask_token_key("abcdef"), "ab****ef");
        assert_eq!(mask_token_key("abcdefghijkl"), "abcd**********ijkl");
    }

    #[test]
    fn masks_nested_channel_secrets_without_mutating_stored_record() {
        let stored = channel_with_json_fields(
            r#"{
                "auth_kind":"oauth",
                "refresh_token":"xai-refresh-secret",
                "proxy":"socks5h://proxy-user:proxy-secret@127.0.0.1:1080",
                "base_url":"https://api.x.ai",
                "credentials":[{"access_token":"access-secret","id_token":"id-secret"}]
            }"#,
            r#"{
                "temperature":0.25,
                "max_tokens":1024,
                "nested":{"api_key":"api-secret","token":"token-secret","password":"password-secret","secret":"secret-secret"}
            }"#,
            r#"{"User-Agent":"halolake","Authorization":"Bearer header-secret","X-Api-Key":"header-api-secret"}"#,
        );
        let original = stored.clone();

        let masked = stored.clone().masked();

        assert!(masked.key.is_empty());
        let setting: serde_json::Value = serde_json::from_str(
            masked
                .setting
                .as_deref()
                .expect("masked setting should remain"),
        )
        .expect("masked setting should be JSON");
        assert_eq!(setting["refresh_token"], "");
        assert_eq!(setting["proxy"], "");
        assert_eq!(setting["credentials"][0]["access_token"], "");
        assert_eq!(setting["credentials"][0]["id_token"], "");
        assert_eq!(setting["auth_kind"], "oauth");
        assert_eq!(setting["base_url"], "https://api.x.ai");

        let param_override: serde_json::Value = serde_json::from_str(
            masked
                .param_override
                .as_deref()
                .expect("masked param_override should remain"),
        )
        .expect("masked param_override should be JSON");
        assert_eq!(param_override["nested"]["api_key"], "");
        assert_eq!(param_override["nested"]["token"], "");
        assert_eq!(param_override["nested"]["password"], "");
        assert_eq!(param_override["nested"]["secret"], "");
        assert_eq!(param_override["temperature"], 0.25);
        assert_eq!(param_override["max_tokens"], 1024);

        let header_override: serde_json::Value = serde_json::from_str(
            masked
                .header_override
                .as_deref()
                .expect("masked header_override should remain"),
        )
        .expect("masked header_override should be JSON");
        assert_eq!(header_override["Authorization"], "");
        assert_eq!(header_override["X-Api-Key"], "");
        assert_eq!(header_override["User-Agent"], "halolake");

        assert_eq!(stored, original);
        assert!(
            stored
                .setting
                .as_deref()
                .expect("stored setting should remain")
                .contains("xai-refresh-secret")
        );
    }

    #[test]
    fn hides_malformed_json_fields_when_masking() {
        let malformed = r#"{"refresh_token":"legacy-token"#;
        let channel = channel_with_json_fields(malformed, "not-json", "{broken");

        let masked = channel.masked();

        assert!(masked.setting.is_none());
        assert!(masked.param_override.is_none());
        assert!(masked.header_override.is_none());
    }
}
