use serde::{Deserialize, Serialize};

pub const STATUS_ENABLED: i32 = 1;
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
    pub id: u64,
    pub username: String,
    #[serde(default, skip_serializing)]
    pub password: String,
    #[serde(default, skip_serializing)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub display_name: String,
    #[serde(default = "default_common_role")]
    pub role: i32,
    #[serde(default = "default_enabled_status")]
    pub status: i32,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub quota: i64,
    #[serde(default)]
    pub used_quota: i64,
    #[serde(default = "default_group")]
    pub group: String,
    #[serde(default)]
    pub setting: String,
    #[serde(default)]
    pub remark: String,
    #[serde(default)]
    pub created_at: i64,
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
    pub id: u64,
    #[serde(default, skip)]
    pub snapshot_id: Option<String>,
    #[serde(default)]
    pub user_id: u64,
    #[serde(default, skip)]
    pub snapshot_user_id: Option<String>,
    #[serde(default)]
    pub key: String,
    #[serde(default = "default_enabled_status")]
    pub status: i32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub created_time: i64,
    #[serde(default)]
    pub accessed_time: i64,
    #[serde(default = "default_never_expires")]
    pub expired_time: i64,
    #[serde(default)]
    pub remain_quota: i64,
    #[serde(default)]
    pub unlimited_quota: bool,
    #[serde(default)]
    pub model_limits_enabled: bool,
    #[serde(default)]
    pub model_limits: String,
    #[serde(default)]
    pub allow_ips: Option<String>,
    #[serde(default)]
    pub used_quota: i64,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub cross_group_retry: bool,
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
    pub id: u64,
    #[serde(default, skip)]
    pub snapshot_id: Option<String>,
    #[serde(rename = "type", default = "default_openai_channel_type")]
    pub channel_type: i32,
    pub key: String,
    #[serde(default = "default_enabled_status")]
    pub status: i32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub weight: Option<u32>,
    #[serde(default)]
    pub created_time: i64,
    #[serde(default)]
    pub test_time: i64,
    #[serde(default)]
    pub response_time: i32,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub balance: f64,
    #[serde(default)]
    pub balance_updated_time: i64,
    #[serde(default)]
    pub models: String,
    #[serde(default = "default_group")]
    pub group: String,
    #[serde(default)]
    pub used_quota: i64,
    #[serde(default)]
    pub model_mapping: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub auto_ban: Option<i32>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub setting: Option<String>,
    #[serde(default)]
    pub param_override: Option<String>,
    #[serde(default)]
    pub header_override: Option<String>,
    #[serde(default)]
    pub remark: Option<String>,
    #[serde(default)]
    pub proxy_id: Option<u64>,
}

impl ChannelRecord {
    pub fn masked(mut self) -> Self {
        self.key.clear();
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PageRequest {
    #[serde(default = "default_page")]
    pub page: usize,
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
    pub page: PageRequest,
    #[serde(default)]
    pub keyword: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PageResult<T> {
    pub items: Vec<T>,
    pub total: usize,
    pub page: usize,
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
    pub request_id: String,
    pub user_id: String,
    pub token_id: String,
    pub channel_id: String,
    #[serde(default)]
    pub group: String,
    pub model: String,
    pub upstream_model: String,
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_tokens: Option<u64>,
    #[serde(default)]
    pub image_tokens: Option<u64>,
    #[serde(default)]
    pub audio_tokens: Option<u64>,
    #[serde(default)]
    pub quota: Option<i64>,
    pub status: UsageStatus,
    pub latency_ms: u64,
    #[serde(default)]
    pub is_stream: bool,
    #[serde(default)]
    pub ip: String,
    #[serde(default)]
    pub upstream_request_id: String,
    pub created_at_unix_ms: i64,
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

fn default_common_role() -> i32 {
    ROLE_COMMON_USER
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

    #[test]
    fn masks_token_keys_like_new_api() {
        assert_eq!(mask_token_key(""), "");
        assert_eq!(mask_token_key("abc"), "***");
        assert_eq!(mask_token_key("abcdef"), "ab****ef");
        assert_eq!(mask_token_key("abcdefghijkl"), "abcd**********ijkl");
    }
}
