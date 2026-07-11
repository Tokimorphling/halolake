use crate::{PublishSnapshotRequest, SnapshotError, SnapshotPublished, SnapshotPublisher};
use bcrypt::{DEFAULT_COST, hash, verify};
use halolake_domain::{
    CHANNEL_TYPE_ANTHROPIC, CHANNEL_TYPE_GEMINI, CHANNEL_TYPE_OPENAI, ChannelRecord, PageRequest,
    PageResult, ROLE_ADMIN_USER, ROLE_COMMON_USER, ROLE_ROOT_USER, STATUS_ENABLED, SearchRequest,
    TokenRecord, UsageEvent, UsageStatus, UserRecord,
};
use halolake_router_core::{ChannelConfig, GatewaySnapshot, ModelMapping, Provider, TokenConfig};
use serde_json::Value as JsonValue;
use service_async::Service;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, RwLock},
};
use thiserror::Error;

const CHANNEL_TYPE_OLLAMA: i32 = 4;
const CHANNEL_TYPE_CUSTOM: i32 = 8;
const CHANNEL_TYPE_AI_PROXY: i32 = 10;
const CHANNEL_TYPE_API2GPT: i32 = 12;
const CHANNEL_TYPE_AIGC2D: i32 = 13;
const CHANNEL_TYPE_ALI: i32 = 17;
const CHANNEL_TYPE_OPENROUTER: i32 = 20;
const CHANNEL_TYPE_MOONSHOT: i32 = 25;
const CHANNEL_TYPE_ZHIPU_V4: i32 = 26;
const CHANNEL_TYPE_PERPLEXITY: i32 = 27;
const CHANNEL_TYPE_LINGYI_WANWU: i32 = 31;
const CHANNEL_TYPE_COHERE: i32 = 34;
const CHANNEL_TYPE_MINIMAX: i32 = 35;
const CHANNEL_TYPE_JINA: i32 = 38;
const CHANNEL_TYPE_SILICON_FLOW: i32 = 40;
const CHANNEL_TYPE_MISTRAL: i32 = 42;
const CHANNEL_TYPE_DEEPSEEK: i32 = 43;
const CHANNEL_TYPE_MOKA_AI: i32 = 44;
const CHANNEL_TYPE_VOLC_ENGINE: i32 = 45;
const CHANNEL_TYPE_XAI: i32 = 48;
const CHANNEL_TYPE_SUBMODEL: i32 = 53;
const CHANNEL_TYPE_SORA: i32 = 55;
const CHANNEL_TYPE_CODEX: i32 = 57;
const TOKEN_STATUS_EXHAUSTED: i32 = 4;

#[derive(Debug, Clone)]
pub struct ManagementData {
    pub version:        u64,
    pub users:          Vec<UserRecord>,
    pub tokens:         Vec<TokenRecord>,
    pub channels:       Vec<ChannelRecord>,
    pub model_mappings: Vec<ModelMapping>,
}

impl ManagementData {
    pub fn new(
        version: u64,
        users: Vec<UserRecord>,
        tokens: Vec<TokenRecord>,
        channels: Vec<ChannelRecord>,
        model_mappings: Vec<ModelMapping>,
    ) -> Self {
        Self {
            version,
            users,
            tokens,
            channels,
            model_mappings,
        }
    }

    pub fn from_snapshot(snapshot: GatewaySnapshot) -> Self {
        let version = snapshot.version;
        let model_mappings = snapshot.model_mappings;
        let tokens = snapshot
            .tokens
            .into_iter()
            .enumerate()
            .map(|(idx, token)| TokenRecord {
                id:                   token.id.parse().unwrap_or((idx + 1) as u64),
                snapshot_id:          Some(token.id.clone()),
                user_id:              token.user_id.parse().unwrap_or(1),
                snapshot_user_id:     Some(token.user_id),
                key:                  token.token,
                status:               if token.enabled { STATUS_ENABLED } else { 0 },
                name:                 token.id,
                created_time:         0,
                accessed_time:        0,
                expired_time:         -1,
                remain_quota:         0,
                unlimited_quota:      true,
                model_limits_enabled: !token.allowed_models.is_empty(),
                model_limits:         token.allowed_models.join(","),
                allow_ips:            (!token.allowed_ips.is_empty())
                    .then(|| token.allowed_ips.join("\n")),
                used_quota:           0,
                group:                if token.token_group.trim().is_empty() {
                    token.group
                } else {
                    token.token_group
                },
                cross_group_retry:    false,
            })
            .collect::<Vec<_>>();

        let channels = snapshot
            .channels
            .into_iter()
            .enumerate()
            .map(|(idx, channel)| {
                let key = snapshot_channel_key(&channel);
                let setting = channel.proxy.as_ref().and_then(|proxy| {
                    let proxy = proxy.trim();
                    if proxy.is_empty() {
                        None
                    } else {
                        serde_json::to_string(&serde_json::json!({ "proxy": proxy })).ok()
                    }
                });
                ChannelRecord {
                    id: channel.id.parse().unwrap_or((idx + 1) as u64),
                    snapshot_id: Some(channel.id.clone()),
                    channel_type: channel_type_from_provider(channel.provider),
                    key,
                    status: if channel.enabled { STATUS_ENABLED } else { 0 },
                    name: channel.id,
                    weight: Some(channel.weight),
                    created_time: 0,
                    test_time: 0,
                    response_time: 0,
                    base_url: Some(channel.base_url),
                    balance: 0.0,
                    balance_updated_time: 0,
                    models: channel.models.join(","),
                    group: if channel.groups.is_empty() {
                        "default".to_string()
                    } else {
                        channel.groups.join(",")
                    },
                    used_quota: 0,
                    model_mapping: None,
                    priority: Some(0),
                    auto_ban: Some(1),
                    tag: None,
                    setting,
                    param_override: None,
                    header_override: None,
                    remark: None,
                    proxy_id: None,
                }
            })
            .collect::<Vec<_>>();

        Self {
            version,
            users: Vec::new(),
            tokens,
            channels,
            model_mappings,
        }
    }

    pub fn build_snapshot(&self) -> Result<GatewaySnapshot, ManagementError> {
        let now = now_unix();
        let enabled_user_ids = self
            .users
            .iter()
            .filter(|user| user.status == STATUS_ENABLED)
            .map(|user| user.id)
            .collect::<HashSet<_>>();
        let tokens = self
            .tokens
            .iter()
            .map(|token| {
                let user_group = token_user_group(token, self.users.as_slice());
                let token_group = token.group.trim().to_string();
                let group = if token_group.is_empty() {
                    user_group.clone()
                } else {
                    token_group.clone()
                };
                TokenConfig {
                    id: token_snapshot_id(token),
                    token: token.key.clone(),
                    user_id: token_snapshot_user_id(token),
                    user_group,
                    token_group,
                    group,
                    enabled: token_runtime_enabled(token, &enabled_user_ids, now),
                    allowed_models: token.allowed_models(),
                    allowed_ips: token_allowed_ips(token.allow_ips.as_deref()),
                }
            })
            .collect::<Vec<_>>();

        let channels = self
            .channels
            .iter()
            .filter_map(|channel| match channel_config(channel) {
                Ok(Some(channel)) => Some(Ok(channel)),
                Ok(None) => None,
                Err(err) => Some(Err(err)),
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut mappings = self
            .model_mappings
            .iter()
            .cloned()
            .map(|mapping| (mapping.requested_model.clone(), mapping))
            .collect::<BTreeMap<_, _>>();
        for channel in &self.channels {
            if channel.status != STATUS_ENABLED
                || provider_from_channel_type(channel.channel_type).is_none()
            {
                continue;
            }
            for mapping in channel_model_mappings(channel)? {
                mappings.insert(mapping.requested_model.clone(), mapping);
            }
        }

        Ok(GatewaySnapshot {
            version: self.version,
            tokens,
            channels,
            model_mappings: mappings.into_values().collect(),
            channel_affinity: Default::default(),
            group_routing: Default::default(),
        })
    }
}

#[derive(Debug, Error)]
pub enum ManagementError {
    #[error("management store lock is poisoned: {0}")]
    Poisoned(&'static str),
    #[error("record not found")]
    NotFound,
    #[error("duplicate record")]
    Duplicate,
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
    #[error("password hash failed: {0}")]
    PasswordHash(String),
    #[error("permission denied")]
    PermissionDenied,
    #[error("storage error: {0}")]
    Storage(String),
    #[error("unsupported channel type: {0}")]
    UnsupportedChannelType(i32),
    #[error("invalid model mapping for channel {channel_id}: {message}")]
    InvalidModelMapping { channel_id: u64, message: String },
    #[error("snapshot publish failed: {0}")]
    Snapshot(#[from] SnapshotError),
}

#[derive(Debug, Clone)]
pub struct LoginUserRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ListUsersRequest {
    pub page: PageRequest,
}

#[derive(Debug, Clone)]
pub struct SearchUsersRequest {
    pub search: SearchRequest,
    pub group:  String,
    pub role:   Option<i32>,
    pub status: Option<i32>,
}

#[derive(Debug, Clone, Copy)]
pub struct GetUserRequest {
    pub id: u64,
}

#[derive(Debug, Clone)]
pub struct ValidateUserAccessTokenRequest {
    pub access_token: String,
}

#[derive(Debug, Clone)]
pub struct UpdateUserAccessTokenRequest {
    pub id:           u64,
    pub access_token: String,
}

#[derive(Debug, Clone)]
pub struct BootstrapRootUserRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct RegisterUserRequest {
    pub user:          UserRecord,
    pub default_token: Option<TokenRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredUser {
    pub user:          UserRecord,
    pub default_token: Option<TokenRecord>,
}

#[derive(Debug, Clone)]
pub struct CreateUserRequest {
    pub user:       UserRecord,
    pub actor_role: i32,
}

#[derive(Debug, Clone)]
pub struct UpdateUserRequest {
    pub user:       UserRecord,
    pub actor_role: i32,
}

#[derive(Debug, Clone, Copy)]
pub struct DeleteUserRequest {
    pub id:         u64,
    pub actor_role: i32,
}

#[derive(Debug, Clone)]
pub struct ManageUserRequest {
    pub id:         u64,
    pub action:     String,
    pub value:      i64,
    pub mode:       String,
    pub actor_role: i32,
}

#[derive(Debug, Clone, Copy)]
pub struct AdjustUserQuotaRequest {
    pub id:    u64,
    pub delta: i64,
}

#[derive(Debug, Clone)]
pub struct SettleUsageRequest {
    pub events:  Vec<UsageEvent>,
    pub pricing: UsagePricing,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageSettlement {
    pub settled:          usize,
    pub skipped:          usize,
    pub quota:            i64,
    pub tokens_exhausted: usize,
    pub event_quotas:     Vec<UsageEventQuota>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEventQuota {
    pub request_id: String,
    pub quota:      i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsagePricing {
    pub quota_per_unit:       f64,
    pub model_ratio:          BTreeMap<String, f64>,
    pub model_price:          BTreeMap<String, f64>,
    pub completion_ratio:     BTreeMap<String, f64>,
    pub cache_ratio:          BTreeMap<String, f64>,
    pub cache_creation_ratio: BTreeMap<String, f64>,
    pub image_ratio:          BTreeMap<String, f64>,
    pub audio_ratio:          BTreeMap<String, f64>,
    pub group_ratio:          BTreeMap<String, f64>,
    pub group_group_ratio:    BTreeMap<String, BTreeMap<String, f64>>,
}

impl Default for UsagePricing {
    fn default() -> Self {
        Self {
            quota_per_unit:       500_000.0,
            model_ratio:          BTreeMap::new(),
            model_price:          BTreeMap::new(),
            completion_ratio:     BTreeMap::new(),
            cache_ratio:          BTreeMap::new(),
            cache_creation_ratio: BTreeMap::new(),
            image_ratio:          BTreeMap::new(),
            audio_ratio:          BTreeMap::new(),
            group_ratio:          BTreeMap::from([("default".to_string(), 1.0)]),
            group_group_ratio:    BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ListTokensRequest {
    pub user_id: Option<u64>,
    pub page:    PageRequest,
}

#[derive(Debug, Clone)]
pub struct SearchTokensRequest {
    pub user_id: Option<u64>,
    pub search:  SearchRequest,
    pub token:   String,
}

#[derive(Debug, Clone, Copy)]
pub struct GetTokenRequest {
    pub id:      u64,
    pub user_id: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct RevealTokenKeyRequest {
    pub id:      u64,
    pub user_id: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RevealedTokenKey {
    pub key: String,
}

#[derive(Debug, Clone)]
pub struct CreateTokenRequest {
    pub token: TokenRecord,
}

#[derive(Debug, Clone)]
pub struct UpdateTokenRequest {
    pub token:   TokenRecord,
    pub user_id: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct DeleteTokenRequest {
    pub id:      u64,
    pub user_id: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct ListChannelsRequest {
    pub page: PageRequest,
}

#[derive(Debug, Clone)]
pub struct SearchChannelsRequest {
    pub search: SearchRequest,
}

#[derive(Debug, Clone, Copy)]
pub struct GetChannelRequest {
    pub id: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct RevealChannelKeyRequest {
    pub id: u64,
}

#[derive(Debug, Clone)]
pub struct RevealedChannelKey {
    pub key: String,
}

#[derive(Debug, Clone)]
pub struct CreateChannelRequest {
    pub channel: ChannelRecord,
}

#[derive(Debug, Clone)]
pub struct UpdateChannelRequest {
    pub channel: ChannelRecord,
}

#[derive(Debug, Clone, Copy)]
pub struct DeleteChannelRequest {
    pub id: u64,
}

#[derive(Debug, Clone, Copy, serde::Deserialize)]
pub struct ChannelStatusUpdateRequest {
    pub id:     u64,
    pub status: i32,
}

#[derive(Debug, Clone, Copy)]
pub struct DeleteDisabledChannelsRequest;

#[derive(Debug, Clone)]
pub struct BatchSetChannelTagRequest {
    pub ids: Vec<u64>,
    pub tag: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ChannelTagPatch {
    pub status:          Option<i32>,
    pub new_tag:         Option<String>,
    pub priority:        Option<i64>,
    pub weight:          Option<u32>,
    pub model_mapping:   Option<String>,
    pub models:          Option<String>,
    pub groups:          Option<String>,
    pub param_override:  Option<String>,
    pub header_override: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpdateChannelsByTagRequest {
    pub tag:   String,
    pub patch: ChannelTagPatch,
}

#[derive(Debug, Clone)]
pub struct PublishManagementSnapshotRequest<P> {
    pub publisher: P,
}

#[derive(Debug, Clone)]
pub struct MemoryManagementStore {
    inner: Arc<RwLock<ManagementData>>,
}

impl MemoryManagementStore {
    pub fn new(data: ManagementData) -> Self {
        Self {
            inner: Arc::new(RwLock::new(data)),
        }
    }

    pub fn current_data(&self) -> Result<ManagementData, ManagementError> {
        self.inner
            .read()
            .map(|data| data.clone())
            .map_err(|_| ManagementError::Poisoned("management"))
    }

    /// Advances the snapshot version without any other change. Options-derived
    /// config (channel_affinity / group_routing) is enriched onto the snapshot
    /// at publish time and does not flow through `mutate`, so an options-only
    /// change would otherwise republish an identical version and the gateway's
    /// `since_version >= version` poll would treat it as NotModified.
    pub fn bump_version(&self) -> Result<u64, ManagementError> {
        self.mutate(|data| Ok(data.version))
    }

    fn mutate<F, T>(&self, f: F) -> Result<T, ManagementError>
    where
        F: FnOnce(&mut ManagementData) -> Result<T, ManagementError>,
    {
        let mut data = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("management"))?;
        let out = f(&mut data)?;
        data.version = data.version.saturating_add(1);
        Ok(out)
    }
}

impl Service<LoginUserRequest> for MemoryManagementStore {
    type Response = UserRecord;
    type Error = ManagementError;

    async fn call(&self, req: LoginUserRequest) -> Result<Self::Response, Self::Error> {
        if req.username.trim().is_empty() || req.password.is_empty() {
            return Err(ManagementError::InvalidRequest(
                "username and password are required",
            ));
        }
        self.current_data()?
            .users
            .into_iter()
            .find(|user| {
                user.username == req.username && verify_user_password(&user.password, &req.password)
            })
            .filter(|user| user.status == STATUS_ENABLED)
            .map(UserRecord::sanitized)
            .ok_or(ManagementError::InvalidCredentials)
    }
}

impl Service<ListUsersRequest> for MemoryManagementStore {
    type Response = PageResult<UserRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListUsersRequest) -> Result<Self::Response, Self::Error> {
        let mut users = self
            .current_data()?
            .users
            .into_iter()
            .map(UserRecord::sanitized)
            .collect::<Vec<_>>();
        users.sort_by_key(|user| std::cmp::Reverse(user.id));
        Ok(page(users, req.page))
    }
}

impl Service<SearchUsersRequest> for MemoryManagementStore {
    type Response = PageResult<UserRecord>;
    type Error = ManagementError;

    async fn call(&self, req: SearchUsersRequest) -> Result<Self::Response, Self::Error> {
        let keyword = req.search.keyword.trim().to_ascii_lowercase();
        let group = req.group.trim();
        let mut users = self
            .current_data()?
            .users
            .into_iter()
            .filter(|user| {
                keyword.is_empty()
                    || user
                        .username
                        .to_ascii_lowercase()
                        .contains(keyword.as_str())
                    || user
                        .display_name
                        .to_ascii_lowercase()
                        .contains(keyword.as_str())
                    || user.email.to_ascii_lowercase().contains(keyword.as_str())
            })
            .filter(|user| group.is_empty() || user.group == group)
            .filter(|user| req.role.is_none_or(|role| user.role == role))
            .filter(|user| req.status.is_none_or(|status| user.status == status))
            .map(UserRecord::sanitized)
            .collect::<Vec<_>>();
        users.sort_by_key(|user| std::cmp::Reverse(user.id));
        Ok(page(users, req.search.page))
    }
}

impl Service<GetUserRequest> for MemoryManagementStore {
    type Response = UserRecord;
    type Error = ManagementError;

    async fn call(&self, req: GetUserRequest) -> Result<Self::Response, Self::Error> {
        self.current_data()?
            .users
            .into_iter()
            .find(|user| user.id == req.id)
            .map(UserRecord::sanitized)
            .ok_or(ManagementError::NotFound)
    }
}

impl Service<ValidateUserAccessTokenRequest> for MemoryManagementStore {
    type Response = UserRecord;
    type Error = ManagementError;

    async fn call(
        &self,
        req: ValidateUserAccessTokenRequest,
    ) -> Result<Self::Response, Self::Error> {
        let access_token = req.access_token.trim();
        if access_token.is_empty() {
            return Err(ManagementError::InvalidCredentials);
        }
        self.current_data()?
            .users
            .into_iter()
            .find(|user| {
                user.status == STATUS_ENABLED
                    && user
                        .access_token
                        .as_deref()
                        .is_some_and(|token| token == access_token)
            })
            .map(UserRecord::sanitized)
            .ok_or(ManagementError::InvalidCredentials)
    }
}

impl Service<UpdateUserAccessTokenRequest> for MemoryManagementStore {
    type Response = String;
    type Error = ManagementError;

    async fn call(&self, req: UpdateUserAccessTokenRequest) -> Result<Self::Response, Self::Error> {
        if req.access_token.trim().is_empty() {
            return Err(ManagementError::InvalidRequest("access token is required"));
        }
        self.mutate(|data| {
            let user = data
                .users
                .iter_mut()
                .find(|user| user.id == req.id)
                .ok_or(ManagementError::NotFound)?;
            user.access_token = Some(req.access_token.clone());
            Ok(req.access_token)
        })
    }
}

impl Service<BootstrapRootUserRequest> for MemoryManagementStore {
    type Response = UserRecord;
    type Error = ManagementError;

    async fn call(&self, req: BootstrapRootUserRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            if data.users.iter().any(|user| user.role == ROLE_ROOT_USER) {
                return Err(ManagementError::Duplicate);
            }
            let mut user = UserRecord {
                id:            next_id(data.users.iter().map(|user| user.id)),
                username:      req.username.trim().to_string(),
                password:      req.password,
                access_token:  None,
                display_name:  "Root User".to_string(),
                role:          ROLE_ROOT_USER,
                status:        STATUS_ENABLED,
                email:         String::new(),
                quota:         100_000_000,
                used_quota:    0,
                group:         "default".to_string(),
                setting:       String::new(),
                remark:        String::new(),
                created_at:    now_unix(),
                last_login_at: 0,
            };
            validate_user_for_write(&user)?;
            if user.password.is_empty() {
                return Err(ManagementError::InvalidRequest("password is required"));
            }
            ensure_user_password_hashed(&mut user)?;
            if data
                .users
                .iter()
                .any(|item| item.id == user.id || item.username == user.username)
            {
                return Err(ManagementError::Duplicate);
            }
            data.users.push(user.clone());
            Ok(user.sanitized())
        })
    }
}

impl Service<CreateUserRequest> for MemoryManagementStore {
    type Response = UserRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateUserRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let mut user = req.user;
            validate_user_for_write(&user)?;
            if user.password.is_empty() {
                return Err(ManagementError::InvalidRequest("password is required"));
            }
            ensure_user_password_hashed(&mut user)?;
            if user.role == 0 {
                user.role = ROLE_COMMON_USER;
            }
            if user.role >= req.actor_role {
                return Err(ManagementError::PermissionDenied);
            }
            if user.id == 0 {
                user.id = next_id(data.users.iter().map(|user| user.id));
            }
            if user.display_name.is_empty() {
                user.display_name.clone_from(&user.username);
            }
            if user.group.is_empty() {
                user.group = "default".to_string();
            }
            if data
                .users
                .iter()
                .any(|item| item.id == user.id || item.username == user.username)
            {
                return Err(ManagementError::Duplicate);
            }
            data.users.push(user.clone());
            Ok(user.sanitized())
        })
    }
}

impl Service<RegisterUserRequest> for MemoryManagementStore {
    type Response = RegisteredUser;
    type Error = ManagementError;

    async fn call(&self, req: RegisterUserRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let mut user = req.user;
            validate_user_for_write(&user)?;
            if user.password.is_empty() {
                return Err(ManagementError::InvalidRequest("password is required"));
            }
            ensure_user_password_hashed(&mut user)?;
            user.id = next_id(data.users.iter().map(|user| user.id));
            user.role = ROLE_COMMON_USER;
            user.status = STATUS_ENABLED;
            user.access_token = None;
            user.quota = 0;
            user.used_quota = 0;
            if user.display_name.is_empty() {
                user.display_name.clone_from(&user.username);
            }
            if user.group.is_empty() {
                user.group = "default".to_string();
            }
            if user.created_at == 0 {
                user.created_at = now_unix();
            }
            if data.users.iter().any(|item| item.username == user.username) {
                return Err(ManagementError::Duplicate);
            }

            let mut default_token = req.default_token;
            if let Some(token) = &mut default_token {
                if token.key.trim().is_empty() {
                    return Err(ManagementError::InvalidRequest("token key is required"));
                }
                token.id = next_id(data.tokens.iter().map(|token| token.id));
                token.user_id = user.id;
                token.snapshot_user_id = None;
                if token.status == 0 {
                    token.status = STATUS_ENABLED;
                }
                if token.name.is_empty() {
                    token.name = user.username.clone();
                }
                let now = now_unix();
                if token.created_time == 0 {
                    token.created_time = now;
                }
                if token.accessed_time == 0 {
                    token.accessed_time = now;
                }
                if data
                    .tokens
                    .iter()
                    .any(|item| item.id == token.id || item.key == token.key)
                {
                    return Err(ManagementError::Duplicate);
                }
            }

            data.users.push(user.clone());
            if let Some(token) = &default_token {
                data.tokens.push(token.clone());
            }
            Ok(RegisteredUser {
                user:          user.sanitized(),
                default_token: default_token.map(TokenRecord::masked),
            })
        })
    }
}

impl Service<UpdateUserRequest> for MemoryManagementStore {
    type Response = UserRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateUserRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let user_idx = data
                .users
                .iter()
                .position(|user| user.id == req.user.id)
                .ok_or(ManagementError::NotFound)?;
            let user = &data.users[user_idx];
            if !can_manage_target_role(req.actor_role, user.role) {
                return Err(ManagementError::PermissionDenied);
            }
            let mut updated = req.user;
            validate_user_for_write(&updated)?;
            if updated.role != 0 && updated.role != user.role {
                return Err(ManagementError::InvalidRequest(
                    "role changes must use manage",
                ));
            }
            if updated.password.is_empty() {
                updated.password.clone_from(&user.password);
            } else {
                ensure_user_password_hashed(&mut updated)?;
            }
            if updated.display_name.is_empty() {
                updated.display_name.clone_from(&updated.username);
            }
            if updated.group.is_empty() {
                updated.group = "default".to_string();
            }
            updated.role = user.role;
            if data
                .users
                .iter()
                .any(|item| item.id != updated.id && item.username == updated.username)
            {
                return Err(ManagementError::Duplicate);
            }
            data.users[user_idx] = updated.clone();
            Ok(updated.sanitized())
        })
    }
}

impl Service<DeleteUserRequest> for MemoryManagementStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeleteUserRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let user = data
                .users
                .iter()
                .find(|user| user.id == req.id)
                .ok_or(ManagementError::NotFound)?;
            if user.role == ROLE_ROOT_USER || !can_manage_target_role(req.actor_role, user.role) {
                return Err(ManagementError::PermissionDenied);
            }
            data.users.retain(|user| user.id != req.id);
            data.tokens.retain(|token| token.user_id != req.id);
            Ok(())
        })
    }
}

impl Service<ManageUserRequest> for MemoryManagementStore {
    type Response = UserRecord;
    type Error = ManagementError;

    async fn call(&self, req: ManageUserRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let user = data
                .users
                .iter_mut()
                .find(|user| user.id == req.id)
                .ok_or(ManagementError::NotFound)?;
            if !can_manage_target_role(req.actor_role, user.role) {
                return Err(ManagementError::PermissionDenied);
            }

            match req.action.as_str() {
                "disable" => {
                    if user.role == ROLE_ROOT_USER {
                        return Err(ManagementError::PermissionDenied);
                    }
                    user.status = 0;
                }
                "enable" => user.status = STATUS_ENABLED,
                "delete" => {
                    if user.role == ROLE_ROOT_USER {
                        return Err(ManagementError::PermissionDenied);
                    }
                    let id = user.id;
                    data.users.retain(|user| user.id != id);
                    data.tokens.retain(|token| token.user_id != id);
                    return Ok(UserRecord {
                        id,
                        username: String::new(),
                        password: String::new(),
                        access_token: None,
                        display_name: String::new(),
                        role: ROLE_COMMON_USER,
                        status: 0,
                        email: String::new(),
                        quota: 0,
                        used_quota: 0,
                        group: String::new(),
                        setting: String::new(),
                        remark: String::new(),
                        created_at: 0,
                        last_login_at: 0,
                    });
                }
                "promote" => {
                    if req.actor_role != ROLE_ROOT_USER || user.role >= ROLE_ADMIN_USER {
                        return Err(ManagementError::PermissionDenied);
                    }
                    user.role = ROLE_ADMIN_USER;
                }
                "demote" => {
                    if user.role == ROLE_ROOT_USER || user.role == ROLE_COMMON_USER {
                        return Err(ManagementError::PermissionDenied);
                    }
                    user.role = ROLE_COMMON_USER;
                }
                "add_quota" => match req.mode.as_str() {
                    "add" if req.value > 0 => user.quota = user.quota.saturating_add(req.value),
                    "subtract" if req.value > 0 => {
                        user.quota = user.quota.saturating_sub(req.value)
                    }
                    "override" => user.quota = req.value,
                    _ => return Err(ManagementError::InvalidRequest("invalid quota mode")),
                },
                _ => return Err(ManagementError::InvalidRequest("unknown user action")),
            }

            Ok(user.clone().sanitized())
        })
    }
}

impl Service<AdjustUserQuotaRequest> for MemoryManagementStore {
    type Response = UserRecord;
    type Error = ManagementError;

    async fn call(&self, req: AdjustUserQuotaRequest) -> Result<Self::Response, Self::Error> {
        if req.delta == 0 {
            return Err(ManagementError::InvalidRequest("quota delta is required"));
        }
        self.mutate(|data| {
            let user = data
                .users
                .iter_mut()
                .find(|user| user.id == req.id)
                .ok_or(ManagementError::NotFound)?;
            if req.delta > 0 {
                user.quota = user.quota.saturating_add(req.delta);
            } else {
                user.quota = user.quota.saturating_sub(req.delta.saturating_abs());
            }
            Ok(user.clone().sanitized())
        })
    }
}

impl Service<SettleUsageRequest> for MemoryManagementStore {
    type Response = UsageSettlement;
    type Error = ManagementError;

    async fn call(&self, req: SettleUsageRequest) -> Result<Self::Response, Self::Error> {
        if req.events.is_empty() {
            return Ok(UsageSettlement::default());
        }
        self.mutate(|data| {
            let now = now_unix();
            let mut settlement = UsageSettlement::default();
            for event in req.events {
                let token_idx = data
                    .tokens
                    .iter()
                    .position(|token| token_matches_usage_event(token, &event.token_id));
                let user_id = token_idx
                    .map(|idx| data.tokens[idx].user_id)
                    .or_else(|| parse_usage_entity_id(&event.user_id));
                let Some(user_idx) =
                    user_id.and_then(|id| data.users.iter().position(|user| user.id == id))
                else {
                    settlement.skipped = settlement.skipped.saturating_add(1);
                    continue;
                };
                let channel_idx = data
                    .channels
                    .iter()
                    .position(|channel| channel_matches_usage_event(channel, &event.channel_id));
                if let Some(idx) = token_idx {
                    data.tokens[idx].accessed_time = now;
                }
                let token = token_idx.and_then(|idx| data.tokens.get(idx));
                let user = data.users.get(user_idx);
                let channel = channel_idx.and_then(|idx| data.channels.get(idx));
                let quota = usage_event_quota(&event, &req.pricing, token, user, channel);
                if quota <= 0 {
                    settlement.skipped = settlement.skipped.saturating_add(1);
                    continue;
                }

                settlement.event_quotas.push(UsageEventQuota {
                    request_id: event.request_id.clone(),
                    quota,
                });

                let user = &mut data.users[user_idx];
                user.quota = user.quota.saturating_sub(quota);
                user.used_quota = user.used_quota.saturating_add(quota);

                if let Some(idx) = token_idx {
                    let token = &mut data.tokens[idx];
                    token.remain_quota = token.remain_quota.saturating_sub(quota);
                    token.used_quota = token.used_quota.saturating_add(quota);
                    if !token.unlimited_quota
                        && token.remain_quota <= 0
                        && token.status != TOKEN_STATUS_EXHAUSTED
                    {
                        token.status = TOKEN_STATUS_EXHAUSTED;
                        settlement.tokens_exhausted = settlement.tokens_exhausted.saturating_add(1);
                    }
                }

                if let Some(idx) = channel_idx {
                    data.channels[idx].used_quota =
                        data.channels[idx].used_quota.saturating_add(quota);
                }

                settlement.settled = settlement.settled.saturating_add(1);
                settlement.quota = settlement.quota.saturating_add(quota);
            }
            Ok(settlement)
        })
    }
}

impl Service<ListTokensRequest> for MemoryManagementStore {
    type Response = PageResult<TokenRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListTokensRequest) -> Result<Self::Response, Self::Error> {
        let data = self.current_data()?;
        let mut tokens = data
            .tokens
            .into_iter()
            .filter(|token| req.user_id.is_none_or(|user_id| token.user_id == user_id))
            .map(TokenRecord::masked)
            .collect::<Vec<_>>();
        tokens.sort_by_key(|token| std::cmp::Reverse(token.id));
        Ok(page(tokens, req.page))
    }
}

impl Service<SearchTokensRequest> for MemoryManagementStore {
    type Response = PageResult<TokenRecord>;
    type Error = ManagementError;

    async fn call(&self, req: SearchTokensRequest) -> Result<Self::Response, Self::Error> {
        let data = self.current_data()?;
        let keyword = req.search.keyword.trim().to_ascii_lowercase();
        let token_keyword = req
            .token
            .trim()
            .trim_start_matches("sk-")
            .to_ascii_lowercase();
        let mut tokens = data
            .tokens
            .into_iter()
            .filter(|token| req.user_id.is_none_or(|user_id| token.user_id == user_id))
            .filter(|token| {
                keyword.is_empty() || token.name.to_ascii_lowercase().contains(keyword.as_str())
            })
            .filter(|token| {
                token_keyword.is_empty()
                    || token
                        .key
                        .to_ascii_lowercase()
                        .contains(token_keyword.as_str())
            })
            .map(TokenRecord::masked)
            .collect::<Vec<_>>();
        tokens.sort_by_key(|token| std::cmp::Reverse(token.id));
        Ok(page(tokens, req.search.page))
    }
}

impl Service<GetTokenRequest> for MemoryManagementStore {
    type Response = TokenRecord;
    type Error = ManagementError;

    async fn call(&self, req: GetTokenRequest) -> Result<Self::Response, Self::Error> {
        self.current_data()?
            .tokens
            .into_iter()
            .find(|token| token.id == req.id && req.user_id.is_none_or(|id| token.user_id == id))
            .map(TokenRecord::masked)
            .ok_or(ManagementError::NotFound)
    }
}

impl Service<RevealTokenKeyRequest> for MemoryManagementStore {
    type Response = RevealedTokenKey;
    type Error = ManagementError;

    async fn call(&self, req: RevealTokenKeyRequest) -> Result<Self::Response, Self::Error> {
        let key = self
            .current_data()?
            .tokens
            .into_iter()
            .find(|token| token.id == req.id && req.user_id.is_none_or(|id| token.user_id == id))
            .map(|token| token.key)
            .ok_or(ManagementError::NotFound)?;
        Ok(RevealedTokenKey { key })
    }
}

impl Service<CreateTokenRequest> for MemoryManagementStore {
    type Response = TokenRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateTokenRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let mut token = req.token;
            if token.id == 0 {
                token.id = next_id(data.tokens.iter().map(|token| token.id));
            }
            if data
                .tokens
                .iter()
                .any(|item| item.id == token.id || item.key == token.key)
            {
                return Err(ManagementError::Duplicate);
            }
            data.tokens.push(token.clone());
            Ok(token.masked())
        })
    }
}

impl Service<UpdateTokenRequest> for MemoryManagementStore {
    type Response = TokenRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateTokenRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let token = data
                .tokens
                .iter_mut()
                .find(|token| {
                    token.id == req.token.id && req.user_id.is_none_or(|id| token.user_id == id)
                })
                .ok_or(ManagementError::NotFound)?;
            let mut updated = req.token;
            // Ownership and spend counters are server-owned. Self-service (and
            // any caller that supplies user_id) cannot reassign a token or reset
            // used_quota by rewriting the record.
            updated.user_id = token.user_id;
            updated.used_quota = token.used_quota;
            if updated.key.is_empty() {
                updated.key.clone_from(&token.key);
            }
            updated.snapshot_id.clone_from(&token.snapshot_id);
            updated.snapshot_user_id.clone_from(&token.snapshot_user_id);
            if updated.remain_quota < 0 {
                updated.remain_quota = 0;
            }
            *token = updated.clone();
            Ok(updated.masked())
        })
    }
}

impl Service<DeleteTokenRequest> for MemoryManagementStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeleteTokenRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let before = data.tokens.len();
            data.tokens.retain(|token| {
                !(token.id == req.id && req.user_id.is_none_or(|id| token.user_id == id))
            });
            if data.tokens.len() == before {
                return Err(ManagementError::NotFound);
            }
            Ok(())
        })
    }
}

impl Service<ListChannelsRequest> for MemoryManagementStore {
    type Response = PageResult<ChannelRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListChannelsRequest) -> Result<Self::Response, Self::Error> {
        let data = self.current_data()?;
        let mut channels = data
            .channels
            .into_iter()
            .map(ChannelRecord::masked)
            .collect::<Vec<_>>();
        channels.sort_by_key(|channel| std::cmp::Reverse(channel.id));
        Ok(page(channels, req.page))
    }
}

impl Service<SearchChannelsRequest> for MemoryManagementStore {
    type Response = PageResult<ChannelRecord>;
    type Error = ManagementError;

    async fn call(&self, req: SearchChannelsRequest) -> Result<Self::Response, Self::Error> {
        let data = self.current_data()?;
        let keyword = req.search.keyword.trim().to_ascii_lowercase();
        let mut channels = data
            .channels
            .into_iter()
            .filter(|channel| {
                keyword.is_empty()
                    || channel.name.to_ascii_lowercase().contains(keyword.as_str())
                    || channel
                        .models
                        .to_ascii_lowercase()
                        .contains(keyword.as_str())
            })
            .map(ChannelRecord::masked)
            .collect::<Vec<_>>();
        channels.sort_by_key(|channel| std::cmp::Reverse(channel.id));
        Ok(page(channels, req.search.page))
    }
}

impl Service<GetChannelRequest> for MemoryManagementStore {
    type Response = ChannelRecord;
    type Error = ManagementError;

    async fn call(&self, req: GetChannelRequest) -> Result<Self::Response, Self::Error> {
        self.current_data()?
            .channels
            .into_iter()
            .find(|channel| channel.id == req.id)
            .map(ChannelRecord::masked)
            .ok_or(ManagementError::NotFound)
    }
}

impl Service<RevealChannelKeyRequest> for MemoryManagementStore {
    type Response = RevealedChannelKey;
    type Error = ManagementError;

    async fn call(&self, req: RevealChannelKeyRequest) -> Result<Self::Response, Self::Error> {
        let key = self
            .current_data()?
            .channels
            .into_iter()
            .find(|channel| channel.id == req.id)
            .map(|channel| channel.key)
            .ok_or(ManagementError::NotFound)?;
        Ok(RevealedChannelKey { key })
    }
}

impl Service<CreateChannelRequest> for MemoryManagementStore {
    type Response = ChannelRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateChannelRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let mut channel = req.channel;
            if channel.id == 0 {
                channel.id = next_id(data.channels.iter().map(|channel| channel.id));
            }
            if data.channels.iter().any(|item| item.id == channel.id) {
                return Err(ManagementError::Duplicate);
            }
            data.channels.push(channel.clone());
            Ok(channel.masked())
        })
    }
}

impl Service<UpdateChannelRequest> for MemoryManagementStore {
    type Response = ChannelRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateChannelRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let channel = data
                .channels
                .iter_mut()
                .find(|channel| channel.id == req.channel.id)
                .ok_or(ManagementError::NotFound)?;
            let mut updated = req.channel;
            if updated.key.is_empty() {
                updated.key.clone_from(&channel.key);
            }
            updated.snapshot_id.clone_from(&channel.snapshot_id);
            *channel = updated.clone();
            Ok(updated.masked())
        })
    }
}

impl Service<DeleteChannelRequest> for MemoryManagementStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeleteChannelRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let before = data.channels.len();
            data.channels.retain(|channel| channel.id != req.id);
            if data.channels.len() == before {
                return Err(ManagementError::NotFound);
            }
            Ok(())
        })
    }
}

impl Service<ChannelStatusUpdateRequest> for MemoryManagementStore {
    type Response = ChannelRecord;
    type Error = ManagementError;

    async fn call(&self, req: ChannelStatusUpdateRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let channel = data
                .channels
                .iter_mut()
                .find(|channel| channel.id == req.id)
                .ok_or(ManagementError::NotFound)?;
            channel.status = req.status;
            Ok(channel.clone().masked())
        })
    }
}

impl Service<DeleteDisabledChannelsRequest> for MemoryManagementStore {
    type Response = usize;
    type Error = ManagementError;

    async fn call(
        &self,
        _req: DeleteDisabledChannelsRequest,
    ) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let before = data.channels.len();
            data.channels
                .retain(|channel| channel.status == STATUS_ENABLED);
            Ok(before.saturating_sub(data.channels.len()))
        })
    }
}

impl Service<BatchSetChannelTagRequest> for MemoryManagementStore {
    type Response = usize;
    type Error = ManagementError;

    async fn call(&self, req: BatchSetChannelTagRequest) -> Result<Self::Response, Self::Error> {
        if req.ids.is_empty() {
            return Err(ManagementError::InvalidRequest("channel ids are required"));
        }
        self.mutate(|data| {
            let ids = req.ids.into_iter().collect::<HashSet<_>>();
            let mut updated = 0usize;
            for channel in &mut data.channels {
                if ids.contains(&channel.id) {
                    channel.tag.clone_from(&req.tag);
                    updated = updated.saturating_add(1);
                }
            }
            Ok(updated)
        })
    }
}

impl Service<UpdateChannelsByTagRequest> for MemoryManagementStore {
    type Response = usize;
    type Error = ManagementError;

    async fn call(&self, req: UpdateChannelsByTagRequest) -> Result<Self::Response, Self::Error> {
        let tag = req.tag.trim();
        if tag.is_empty() {
            return Err(ManagementError::InvalidRequest("tag is required"));
        }
        self.mutate(|data| {
            let mut updated = 0usize;
            for channel in &mut data.channels {
                if channel.tag.as_deref() != Some(tag) {
                    continue;
                }
                apply_channel_tag_patch(channel, &req.patch);
                updated = updated.saturating_add(1);
            }
            Ok(updated)
        })
    }
}

impl<P> Service<PublishManagementSnapshotRequest<P>> for MemoryManagementStore
where
    P: SnapshotPublisher,
{
    type Response = SnapshotPublished;
    type Error = ManagementError;

    async fn call(
        &self,
        req: PublishManagementSnapshotRequest<P>,
    ) -> Result<Self::Response, Self::Error> {
        // Publish the current version as-is. Write paths already advanced the
        // version via `mutate()`. Options-only publishers must call
        // `bump_version()` before this so the gateway does not see NotModified.
        let snapshot = self.current_data()?.build_snapshot()?;
        req.publisher
            .call(PublishSnapshotRequest { snapshot })
            .await
            .map_err(ManagementError::Snapshot)
    }
}

pub fn ensure_user_password_hashed(user: &mut UserRecord) -> Result<(), ManagementError> {
    if user.password.is_empty() || is_bcrypt_hash(&user.password) {
        return Ok(());
    }
    user.password = hash_user_password(&user.password)?;
    Ok(())
}

pub fn hash_user_password(password: &str) -> Result<String, ManagementError> {
    hash(password, DEFAULT_COST).map_err(|err| ManagementError::PasswordHash(err.to_string()))
}

fn channel_config(channel: &ChannelRecord) -> Result<Option<ChannelConfig>, ManagementError> {
    let Some(provider) = provider_from_channel_type(channel.channel_type) else {
        return Ok(None);
    };
    let runtime_keys = channel_runtime_api_keys(channel);
    let api_key = runtime_keys
        .first()
        .map(|(_, key)| key.clone())
        .unwrap_or_default();
    let mut api_keys = Vec::with_capacity(runtime_keys.len());
    let mut api_key_indexes = Vec::with_capacity(runtime_keys.len());
    for (idx, key) in runtime_keys {
        api_key_indexes.push(idx);
        api_keys.push(key);
    }
    Ok(Some(ChannelConfig {
        id: channel_snapshot_id(channel),
        provider,
        base_url: channel
            .base_url
            .clone()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| default_base_url(channel.channel_type).to_string()),
        api_key,
        api_keys,
        api_key_indexes,
        api_key_env: None,
        enabled: channel.status == STATUS_ENABLED,
        weight: channel.weight.unwrap_or(1),
        models: channel.model_list(),
        groups: channel.group_list(),
        proxy: channel_setting_proxy(channel),
    }))
}

fn channel_setting_proxy(channel: &ChannelRecord) -> Option<String> {
    let raw = channel.setting.as_deref()?.trim();
    if raw.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    v.get("proxy")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn channel_model_mappings(channel: &ChannelRecord) -> Result<Vec<ModelMapping>, ManagementError> {
    let models = channel.model_list();
    let Some(mapping) = channel
        .model_mapping
        .as_deref()
        .map(str::trim)
        .filter(|mapping| !mapping.is_empty())
    else {
        return Ok(models
            .into_iter()
            .map(|model| ModelMapping {
                requested_model: model.clone(),
                channel_id:      channel_snapshot_id(channel),
                upstream_model:  model,
            })
            .collect());
    };

    let parsed: HashMap<String, String> =
        serde_json::from_str(mapping).map_err(|err| ManagementError::InvalidModelMapping {
            channel_id: channel.id,
            message:    err.to_string(),
        })?;
    Ok(parsed
        .into_iter()
        .map(|(requested_model, upstream_model)| ModelMapping {
            requested_model,
            channel_id: channel_snapshot_id(channel),
            upstream_model,
        })
        .collect())
}

fn apply_channel_tag_patch(channel: &mut ChannelRecord, patch: &ChannelTagPatch) {
    if let Some(status) = patch.status {
        channel.status = status;
    }
    if let Some(new_tag) = &patch.new_tag {
        channel.tag = (!new_tag.trim().is_empty()).then(|| new_tag.trim().to_string());
    }
    if let Some(priority) = patch.priority {
        channel.priority = Some(priority);
    }
    if let Some(weight) = patch.weight {
        channel.weight = Some(weight);
    }
    if let Some(model_mapping) = &patch.model_mapping {
        channel.model_mapping = Some(model_mapping.clone());
    }
    if let Some(models) = &patch.models {
        channel.models.clone_from(models);
    }
    if let Some(groups) = &patch.groups {
        channel.group.clone_from(groups);
    }
    if let Some(param_override) = &patch.param_override {
        channel.param_override = Some(param_override.clone());
    }
    if let Some(header_override) = &patch.header_override {
        channel.header_override = Some(header_override.clone());
    }
}

fn token_snapshot_id(token: &TokenRecord) -> String {
    token
        .snapshot_id
        .clone()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| token.id.to_string())
}

fn token_snapshot_user_id(token: &TokenRecord) -> String {
    token
        .snapshot_user_id
        .clone()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| token.user_id.to_string())
}

fn token_runtime_enabled(token: &TokenRecord, enabled_user_ids: &HashSet<u64>, now: i64) -> bool {
    if token.status != STATUS_ENABLED {
        return false;
    }
    if token.expired_time != -1 && token.expired_time < now {
        return false;
    }
    if !token.unlimited_quota && token.remain_quota <= 0 {
        return false;
    }
    if !enabled_user_ids.is_empty() && !enabled_user_ids.contains(&token.user_id) {
        return false;
    }
    true
}

fn token_user_group(token: &TokenRecord, users: &[UserRecord]) -> String {
    users
        .iter()
        .find(|user| user.id == token.user_id)
        .map(|user| user.group.trim())
        .filter(|group| !group.is_empty())
        .unwrap_or("default")
        .to_string()
}

fn token_allowed_ips(allow_ips: Option<&str>) -> Vec<String> {
    let Some(allow_ips) = allow_ips else {
        return Vec::new();
    };
    let clean_ips = allow_ips.replace(' ', "");
    if clean_ips.is_empty() {
        return Vec::new();
    }
    clean_ips
        .lines()
        .map(str::trim)
        .map(|ip| ip.replace(',', ""))
        .filter(|ip| !ip.is_empty())
        .collect()
}

fn channel_snapshot_id(channel: &ChannelRecord) -> String {
    channel
        .snapshot_id
        .clone()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| channel.id.to_string())
}

fn snapshot_channel_key(channel: &ChannelConfig) -> String {
    let keys = channel
        .api_keys
        .iter()
        .map(|key| key.trim())
        .filter(|key| !key.is_empty())
        .collect::<Vec<_>>();
    if keys.is_empty() {
        return channel.api_key.trim().to_string();
    }
    keys.join("\n")
}

fn channel_runtime_api_keys(channel: &ChannelRecord) -> Vec<(usize, String)> {
    let status = multi_key_status_list(channel.setting.as_deref());
    channel
        .key
        .lines()
        .enumerate()
        .filter_map(|(idx, key)| {
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            if status
                .get(&idx)
                .is_some_and(|status| *status != STATUS_ENABLED)
            {
                return None;
            }
            Some((idx, key.to_string()))
        })
        .collect()
}

fn multi_key_status_list(setting: Option<&str>) -> HashMap<usize, i32> {
    let Some(value) = setting.and_then(|setting| serde_json::from_str::<JsonValue>(setting).ok())
    else {
        return HashMap::new();
    };
    let Some(status) = value
        .get("multi_key_status_list")
        .and_then(JsonValue::as_object)
    else {
        return HashMap::new();
    };
    status
        .iter()
        .filter_map(|(idx, value)| {
            let idx = idx.parse::<usize>().ok()?;
            let status = value
                .as_i64()
                .or_else(|| value.as_str()?.parse::<i64>().ok())?;
            Some((idx, status as i32))
        })
        .collect()
}

fn provider_from_channel_type(channel_type: i32) -> Option<Provider> {
    match channel_type {
        CHANNEL_TYPE_OPENAI
        | CHANNEL_TYPE_OLLAMA
        | CHANNEL_TYPE_CUSTOM
        | CHANNEL_TYPE_AI_PROXY
        | CHANNEL_TYPE_API2GPT
        | CHANNEL_TYPE_AIGC2D
        | CHANNEL_TYPE_ALI
        | CHANNEL_TYPE_OPENROUTER
        | CHANNEL_TYPE_MOONSHOT
        | CHANNEL_TYPE_ZHIPU_V4
        | CHANNEL_TYPE_PERPLEXITY
        | CHANNEL_TYPE_LINGYI_WANWU
        | CHANNEL_TYPE_COHERE
        | CHANNEL_TYPE_MINIMAX
        | CHANNEL_TYPE_JINA
        | CHANNEL_TYPE_SILICON_FLOW
        | CHANNEL_TYPE_MISTRAL
        | CHANNEL_TYPE_DEEPSEEK
        | CHANNEL_TYPE_MOKA_AI
        | CHANNEL_TYPE_VOLC_ENGINE
        | CHANNEL_TYPE_XAI
        | CHANNEL_TYPE_SUBMODEL
        | CHANNEL_TYPE_SORA
        | CHANNEL_TYPE_CODEX => Some(Provider::OpenAi),
        CHANNEL_TYPE_ANTHROPIC => Some(Provider::Claude),
        CHANNEL_TYPE_GEMINI => Some(Provider::Gemini),
        _ => None,
    }
}

fn channel_type_from_provider(provider: Provider) -> i32 {
    match provider {
        Provider::OpenAi => CHANNEL_TYPE_OPENAI,
        Provider::Claude => CHANNEL_TYPE_ANTHROPIC,
        Provider::Gemini => CHANNEL_TYPE_GEMINI,
    }
}

fn default_base_url(channel_type: i32) -> &'static str {
    match channel_type {
        CHANNEL_TYPE_OLLAMA => "http://localhost:11434",
        CHANNEL_TYPE_AI_PROXY => "https://api.aiproxy.io",
        CHANNEL_TYPE_API2GPT => "https://api.api2gpt.com",
        CHANNEL_TYPE_AIGC2D => "https://api.aigc2d.com",
        CHANNEL_TYPE_ANTHROPIC => "https://api.anthropic.com",
        CHANNEL_TYPE_ALI => "https://dashscope.aliyuncs.com",
        CHANNEL_TYPE_OPENROUTER => "https://openrouter.ai/api",
        CHANNEL_TYPE_GEMINI => "https://generativelanguage.googleapis.com",
        CHANNEL_TYPE_MOONSHOT => "https://api.moonshot.cn",
        CHANNEL_TYPE_ZHIPU_V4 => "https://open.bigmodel.cn",
        CHANNEL_TYPE_PERPLEXITY => "https://api.perplexity.ai",
        CHANNEL_TYPE_LINGYI_WANWU => "https://api.lingyiwanwu.com",
        CHANNEL_TYPE_COHERE => "https://api.cohere.ai",
        CHANNEL_TYPE_MINIMAX => "https://api.minimax.chat",
        CHANNEL_TYPE_JINA => "https://api.jina.ai",
        CHANNEL_TYPE_SILICON_FLOW => "https://api.siliconflow.cn",
        CHANNEL_TYPE_MISTRAL => "https://api.mistral.ai",
        CHANNEL_TYPE_DEEPSEEK => "https://api.deepseek.com",
        CHANNEL_TYPE_MOKA_AI => "https://api.moka.ai",
        CHANNEL_TYPE_VOLC_ENGINE => "https://ark.cn-beijing.volces.com",
        CHANNEL_TYPE_XAI => "https://api.x.ai",
        CHANNEL_TYPE_SUBMODEL => "https://llm.submodel.ai",
        CHANNEL_TYPE_SORA | CHANNEL_TYPE_CODEX => "https://api.openai.com",
        _ => "https://api.openai.com",
    }
}

fn validate_user_for_write(user: &UserRecord) -> Result<(), ManagementError> {
    if user.username.trim().is_empty() {
        return Err(ManagementError::InvalidRequest("username is required"));
    }
    Ok(())
}

fn verify_user_password(hash_value: &str, password: &str) -> bool {
    is_bcrypt_hash(hash_value) && verify(password, hash_value).unwrap_or(false)
}

fn is_bcrypt_hash(value: &str) -> bool {
    value.starts_with("$2a$") || value.starts_with("$2b$") || value.starts_with("$2y$")
}

fn can_manage_target_role(actor_role: i32, target_role: i32) -> bool {
    actor_role == ROLE_ROOT_USER || actor_role > target_role
}

fn page<T>(items: Vec<T>, page: PageRequest) -> PageResult<T> {
    let total = items.len();
    let start = page.offset();
    let limit = page.limit();
    let items = items.into_iter().skip(start).take(limit).collect();
    PageResult::new(items, total, page)
}

fn next_id(ids: impl Iterator<Item = u64>) -> u64 {
    ids.max().unwrap_or(0).saturating_add(1)
}

fn usage_event_quota(
    event: &UsageEvent,
    pricing: &UsagePricing,
    token: Option<&TokenRecord>,
    user: Option<&UserRecord>,
    channel: Option<&ChannelRecord>,
) -> i64 {
    if event.status != UsageStatus::Success {
        return 0;
    }
    let total_tokens = event.observed_tokens();
    if total_tokens == 0 {
        return 0;
    }

    let model_name = event.model.trim();
    let upstream_model_name = event.upstream_model.trim();
    let using_group = usage_pricing_group(event, token, user, channel);
    let user_group = user
        .map(|user| user.group.trim())
        .filter(|group| !group.is_empty())
        .unwrap_or("default");
    let group_ratio = pricing_group_ratio(pricing, user_group, &using_group);

    if let Some(model_price) =
        pricing_value_for_model(&pricing.model_price, model_name, upstream_model_name)
    {
        return quota_from_f64(model_price * pricing.quota_per_unit * group_ratio);
    }

    let prompt_tokens = usage_prompt_tokens(event);
    let completion_tokens = event.completion_tokens.unwrap_or(0);
    let completion_ratio =
        pricing_value_for_model(&pricing.completion_ratio, model_name, upstream_model_name)
            .unwrap_or(1.0);
    let cache_ratio =
        pricing_value_for_model(&pricing.cache_ratio, model_name, upstream_model_name)
            .unwrap_or(1.0);
    let cache_creation_ratio = pricing_value_for_model(
        &pricing.cache_creation_ratio,
        model_name,
        upstream_model_name,
    )
    .unwrap_or(1.0);
    let image_ratio =
        pricing_value_for_model(&pricing.image_ratio, model_name, upstream_model_name)
            .unwrap_or(1.0);
    let audio_ratio =
        pricing_value_for_model(&pricing.audio_ratio, model_name, upstream_model_name)
            .unwrap_or(1.0);
    let model_ratio =
        pricing_value_for_model(&pricing.model_ratio, model_name, upstream_model_name)
            .unwrap_or(1.0);
    let ratio = model_ratio * group_ratio;
    let cache_read_tokens = event.cache_read_tokens.unwrap_or(0).min(prompt_tokens);
    let cache_creation_tokens = event
        .cache_creation_tokens
        .unwrap_or(0)
        .min(prompt_tokens.saturating_sub(cache_read_tokens));
    let image_tokens = event.image_tokens.unwrap_or(0).min(
        prompt_tokens
            .saturating_sub(cache_read_tokens)
            .saturating_sub(cache_creation_tokens),
    );
    let audio_tokens = event.audio_tokens.unwrap_or(0).min(
        prompt_tokens
            .saturating_sub(cache_read_tokens)
            .saturating_sub(cache_creation_tokens)
            .saturating_sub(image_tokens),
    );
    let base_prompt_tokens = prompt_tokens
        .saturating_sub(cache_read_tokens)
        .saturating_sub(cache_creation_tokens)
        .saturating_sub(image_tokens)
        .saturating_sub(audio_tokens);
    let quota = quota_from_f64(
        ((base_prompt_tokens as f64)
            + (cache_read_tokens as f64 * cache_ratio)
            + (cache_creation_tokens as f64 * cache_creation_ratio)
            + (image_tokens as f64 * image_ratio)
            + (audio_tokens as f64 * audio_ratio)
            + (completion_tokens as f64 * completion_ratio))
            * ratio,
    );
    if ratio != 0.0 && quota == 0 { 1 } else { quota }
}

fn usage_prompt_tokens(event: &UsageEvent) -> u64 {
    match (
        event.prompt_tokens,
        event.completion_tokens,
        event.total_tokens,
    ) {
        (Some(prompt), _, _) => prompt,
        (None, Some(completion), Some(total)) => total.saturating_sub(completion),
        (None, _, Some(total)) => total,
        (None, _, None) => 0,
    }
}

fn pricing_value_for_model(
    values: &BTreeMap<String, f64>,
    model_name: &str,
    upstream_model_name: &str,
) -> Option<f64> {
    values
        .get(model_name)
        .copied()
        .or_else(|| values.get(upstream_model_name).copied())
}

fn usage_pricing_group(
    event: &UsageEvent,
    token: Option<&TokenRecord>,
    user: Option<&UserRecord>,
    channel: Option<&ChannelRecord>,
) -> String {
    if !event.group.trim().is_empty() {
        return event.group.trim().to_string();
    }
    if let Some(group) = token
        .map(|token| token.group.trim())
        .filter(|group| !group.is_empty())
    {
        return group.to_string();
    }
    if let Some(group) = channel.and_then(|channel| channel.group_list().into_iter().next()) {
        return group;
    }
    user.map(|user| user.group.trim())
        .filter(|group| !group.is_empty())
        .unwrap_or("default")
        .to_string()
}

fn pricing_group_ratio(pricing: &UsagePricing, user_group: &str, using_group: &str) -> f64 {
    pricing
        .group_group_ratio
        .get(user_group)
        .and_then(|groups| groups.get(using_group))
        .copied()
        .or_else(|| pricing.group_ratio.get(using_group).copied())
        .unwrap_or(1.0)
}

fn quota_from_f64(value: f64) -> i64 {
    if !value.is_finite() {
        return 0;
    }
    let rounded = value.round();
    if rounded >= i64::MAX as f64 {
        i64::MAX
    } else if rounded <= i64::MIN as f64 {
        i64::MIN
    } else {
        rounded as i64
    }
}

fn token_matches_usage_event(token: &TokenRecord, event_token_id: &str) -> bool {
    entity_id_matches(event_token_id, token.id, token.snapshot_id.as_deref())
}

fn channel_matches_usage_event(channel: &ChannelRecord, event_channel_id: &str) -> bool {
    entity_id_matches(event_channel_id, channel.id, channel.snapshot_id.as_deref())
}

fn entity_id_matches(event_id: &str, numeric_id: u64, snapshot_id: Option<&str>) -> bool {
    parse_usage_entity_id(event_id).is_some_and(|id| id == numeric_id)
        || snapshot_id.is_some_and(|snapshot_id| snapshot_id == event_id)
}

fn parse_usage_entity_id(value: &str) -> Option<u64> {
    value.parse::<u64>().ok()
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use halolake_domain::CHANNEL_TYPE_OPENAI;
    use service_async::Service;
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn user(id: u64, username: &str, role: i32) -> UserRecord {
        UserRecord {
            id,
            username: username.to_string(),
            password: hash_user_password("password").expect("test password should hash"),
            access_token: None,
            display_name: username.to_string(),
            role,
            status: STATUS_ENABLED,
            email: String::new(),
            quota: 0,
            used_quota: 0,
            group: "default".to_string(),
            setting: String::new(),
            remark: String::new(),
            created_at: 0,
            last_login_at: 0,
        }
    }

    fn token(id: u64, user_id: u64, key: &str) -> TokenRecord {
        TokenRecord {
            id,
            snapshot_id: None,
            user_id,
            snapshot_user_id: None,
            key: key.to_string(),
            status: STATUS_ENABLED,
            name: key.to_string(),
            created_time: 0,
            accessed_time: 0,
            expired_time: -1,
            remain_quota: 1000,
            unlimited_quota: false,
            model_limits_enabled: false,
            model_limits: String::new(),
            allow_ips: None,
            used_quota: 0,
            group: String::new(),
            cross_group_retry: false,
        }
    }

    #[test]
    fn settles_usage_by_snapshot_ids() {
        block_on(async {
            let snapshot = GatewaySnapshot {
                version:          1,
                tokens:           vec![TokenConfig {
                    id:             "dev-token".to_string(),
                    token:          "secret".to_string(),
                    user_id:        "1".to_string(),
                    user_group:     "default".to_string(),
                    token_group:    String::new(),
                    group:          "default".to_string(),
                    enabled:        true,
                    allowed_models: Vec::new(),
                    allowed_ips:    Vec::new(),
                }],
                channels:         vec![ChannelConfig {
                    id:              "openai-main".to_string(),
                    provider:        Provider::OpenAi,
                    base_url:        "https://example.com".to_string(),
                    api_key:         "upstream".to_string(),
                    api_keys:        vec!["upstream".to_string()],
                    api_key_indexes: Vec::new(),
                    api_key_env:     None,
                    enabled:         true,
                    weight:          1,
                    models:          vec!["gpt-4o".to_string()],
                    groups:          Vec::new(),
                    proxy:           None,
                }],
                model_mappings:   vec![ModelMapping {
                    requested_model: "gpt-4o".to_string(),
                    channel_id:      "openai-main".to_string(),
                    upstream_model:  "gpt-4o".to_string(),
                }],
                channel_affinity: Default::default(),
                group_routing:    Default::default(),
            };
            let mut data = ManagementData::from_snapshot(snapshot);
            let mut root = user(1, "root", ROLE_ROOT_USER);
            root.quota = 1000;
            data.users = vec![root];
            data.tokens[0].remain_quota = 1000;

            let store = MemoryManagementStore::new(data);
            let settlement = store
                .call(SettleUsageRequest {
                    pricing: UsagePricing::default(),
                    events:  vec![UsageEvent {
                        request_id:            "req-1".to_string(),
                        user_id:               "1".to_string(),
                        token_id:              "dev-token".to_string(),
                        channel_id:            "openai-main".to_string(),
                        group:                 String::new(),
                        model:                 "gpt-4o".to_string(),
                        upstream_model:        "gpt-4o".to_string(),
                        prompt_tokens:         Some(10),
                        completion_tokens:     Some(32),
                        total_tokens:          None,
                        cache_read_tokens:     None,
                        cache_creation_tokens: None,
                        image_tokens:          None,
                        audio_tokens:          None,
                        quota:                 None,
                        status:                UsageStatus::Success,
                        latency_ms:            123,
                        first_response_ms:     None,
                        is_stream:             false,
                        ip:                    String::new(),
                        upstream_request_id:   String::new(),
                        created_at_unix_ms:    1_700_000_000_000,
                    }],
                })
                .await
                .expect("usage should settle");

            assert_eq!(settlement.settled, 1);
            assert_eq!(settlement.quota, 42);
            let data = store.current_data().expect("data should be readable");
            assert_eq!(data.users[0].quota, 958);
            assert_eq!(data.users[0].used_quota, 42);
            assert_eq!(data.tokens[0].remain_quota, 958);
            assert_eq!(data.tokens[0].used_quota, 42);
            assert_eq!(data.channels[0].used_quota, 42);
        });
    }

    #[test]
    fn settle_usage_marks_token_exhausted() {
        block_on(async {
            let mut data = ManagementData::new(
                1,
                vec![user(1, "root", ROLE_ROOT_USER)],
                vec![token(1, 1, "dev-token")],
                Vec::new(),
                Vec::new(),
            );
            data.users[0].quota = 1000;
            data.tokens[0].remain_quota = 1;

            let store = MemoryManagementStore::new(data);
            let settlement = store
                .call(SettleUsageRequest {
                    pricing: UsagePricing::default(),
                    events:  vec![UsageEvent {
                        request_id:            "req-1".to_string(),
                        user_id:               "1".to_string(),
                        token_id:              "1".to_string(),
                        channel_id:            String::new(),
                        group:                 String::new(),
                        model:                 "gpt-4o".to_string(),
                        upstream_model:        "gpt-4o".to_string(),
                        prompt_tokens:         Some(2),
                        completion_tokens:     None,
                        total_tokens:          None,
                        cache_read_tokens:     None,
                        cache_creation_tokens: None,
                        image_tokens:          None,
                        audio_tokens:          None,
                        quota:                 None,
                        status:                UsageStatus::Success,
                        latency_ms:            1,
                        first_response_ms:     None,
                        is_stream:             false,
                        ip:                    String::new(),
                        upstream_request_id:   String::new(),
                        created_at_unix_ms:    1_700_000_000_000,
                    }],
                })
                .await
                .expect("usage should settle");

            assert_eq!(settlement.settled, 1);
            assert_eq!(settlement.tokens_exhausted, 1);
            let data = store.current_data().expect("data should be readable");
            assert_eq!(data.tokens[0].status, TOKEN_STATUS_EXHAUSTED);
        });
    }

    #[test]
    fn settle_usage_updates_token_access_time_even_without_quota() {
        block_on(async {
            let mut data = ManagementData::new(
                1,
                vec![user(1, "root", ROLE_ROOT_USER)],
                vec![token(1, 1, "dev-token")],
                Vec::new(),
                Vec::new(),
            );
            data.users[0].quota = 1000;
            data.tokens[0].accessed_time = 0;
            data.tokens[0].remain_quota = 1000;

            let store = MemoryManagementStore::new(data);
            let settlement = store
                .call(SettleUsageRequest {
                    pricing: UsagePricing::default(),
                    events:  vec![UsageEvent {
                        request_id:            "req-1".to_string(),
                        user_id:               "1".to_string(),
                        token_id:              "1".to_string(),
                        channel_id:            String::new(),
                        group:                 String::new(),
                        model:                 "gpt-4o".to_string(),
                        upstream_model:        "gpt-4o".to_string(),
                        prompt_tokens:         None,
                        completion_tokens:     None,
                        total_tokens:          None,
                        cache_read_tokens:     None,
                        cache_creation_tokens: None,
                        image_tokens:          None,
                        audio_tokens:          None,
                        quota:                 None,
                        status:                UsageStatus::UpstreamError,
                        latency_ms:            1,
                        first_response_ms:     None,
                        is_stream:             false,
                        ip:                    String::new(),
                        upstream_request_id:   String::new(),
                        created_at_unix_ms:    1_700_000_000_000,
                    }],
                })
                .await
                .expect("usage should settle");

            assert_eq!(settlement.settled, 0);
            assert_eq!(settlement.skipped, 1);
            let data = store.current_data().expect("data should be readable");
            assert!(data.tokens[0].accessed_time > 0);
            assert_eq!(data.tokens[0].remain_quota, 1000);
            assert_eq!(data.tokens[0].used_quota, 0);
        });
    }

    #[test]
    fn settles_usage_with_model_and_group_ratios() {
        block_on(async {
            let snapshot = GatewaySnapshot {
                version:          1,
                tokens:           vec![TokenConfig {
                    id:             "dev-token".to_string(),
                    token:          "secret".to_string(),
                    user_id:        "1".to_string(),
                    user_group:     "default".to_string(),
                    token_group:    String::new(),
                    group:          "default".to_string(),
                    enabled:        true,
                    allowed_models: Vec::new(),
                    allowed_ips:    Vec::new(),
                }],
                channels:         vec![ChannelConfig {
                    id:              "openai-main".to_string(),
                    provider:        Provider::OpenAi,
                    base_url:        "https://example.com".to_string(),
                    api_key:         "upstream".to_string(),
                    api_keys:        vec!["upstream".to_string()],
                    api_key_indexes: Vec::new(),
                    api_key_env:     None,
                    enabled:         true,
                    weight:          1,
                    models:          vec!["gpt-4o".to_string()],
                    groups:          Vec::new(),
                    proxy:           None,
                }],
                model_mappings:   vec![ModelMapping {
                    requested_model: "gpt-4o".to_string(),
                    channel_id:      "openai-main".to_string(),
                    upstream_model:  "gpt-4o".to_string(),
                }],
                channel_affinity: Default::default(),
                group_routing:    Default::default(),
            };
            let mut data = ManagementData::from_snapshot(snapshot);
            let mut root = user(1, "root", ROLE_ROOT_USER);
            root.quota = 1000;
            root.group = "vip".to_string();
            data.users = vec![root];
            data.tokens[0].remain_quota = 1000;
            data.tokens[0].group = "auto".to_string();

            let store = MemoryManagementStore::new(data);
            let settlement = store
                .call(SettleUsageRequest {
                    pricing: UsagePricing {
                        model_ratio: BTreeMap::from([("gpt-4o".to_string(), 4.0)]),
                        completion_ratio: BTreeMap::from([("gpt-4o".to_string(), 2.0)]),
                        group_group_ratio: BTreeMap::from([(
                            "vip".to_string(),
                            BTreeMap::from([("paid".to_string(), 0.25)]),
                        )]),
                        ..UsagePricing::default()
                    },
                    events:  vec![UsageEvent {
                        request_id:            "req-1".to_string(),
                        user_id:               "1".to_string(),
                        token_id:              "dev-token".to_string(),
                        channel_id:            "openai-main".to_string(),
                        group:                 "paid".to_string(),
                        model:                 "gpt-4o".to_string(),
                        upstream_model:        "gpt-4o".to_string(),
                        prompt_tokens:         Some(100),
                        completion_tokens:     Some(50),
                        total_tokens:          Some(150),
                        cache_read_tokens:     None,
                        cache_creation_tokens: None,
                        image_tokens:          None,
                        audio_tokens:          None,
                        quota:                 None,
                        status:                UsageStatus::Success,
                        latency_ms:            123,
                        first_response_ms:     None,
                        is_stream:             false,
                        ip:                    String::new(),
                        upstream_request_id:   String::new(),
                        created_at_unix_ms:    1_700_000_000_000,
                    }],
                })
                .await
                .expect("usage should settle");

            assert_eq!(settlement.settled, 1);
            assert_eq!(settlement.quota, 200);
            let data = store.current_data().expect("data should be readable");
            assert_eq!(data.users[0].quota, 800);
            assert_eq!(data.users[0].used_quota, 200);
            assert_eq!(data.tokens[0].remain_quota, 800);
            assert_eq!(data.tokens[0].used_quota, 200);
            assert_eq!(data.channels[0].used_quota, 200);
        });
    }

    #[test]
    fn settles_usage_with_cache_token_ratios() {
        block_on(async {
            let mut data = ManagementData::new(
                1,
                vec![user(1, "root", ROLE_ROOT_USER)],
                vec![token(1, 1, "dev-token")],
                Vec::new(),
                Vec::new(),
            );
            data.users[0].quota = 1000;
            data.tokens[0].remain_quota = 1000;

            let store = MemoryManagementStore::new(data);
            let settlement = store
                .call(SettleUsageRequest {
                    pricing: UsagePricing {
                        completion_ratio: BTreeMap::from([("gpt-4o".to_string(), 2.0)]),
                        cache_ratio: BTreeMap::from([("gpt-4o".to_string(), 0.5)]),
                        cache_creation_ratio: BTreeMap::from([("gpt-4o".to_string(), 1.25)]),
                        image_ratio: BTreeMap::from([("gpt-4o".to_string(), 2.0)]),
                        audio_ratio: BTreeMap::from([("gpt-4o".to_string(), 3.0)]),
                        ..UsagePricing::default()
                    },
                    events:  vec![UsageEvent {
                        request_id:            "req-cache".to_string(),
                        user_id:               "1".to_string(),
                        token_id:              "1".to_string(),
                        channel_id:            String::new(),
                        group:                 String::new(),
                        model:                 "gpt-4o".to_string(),
                        upstream_model:        "gpt-4o".to_string(),
                        prompt_tokens:         Some(130),
                        completion_tokens:     Some(10),
                        total_tokens:          Some(140),
                        cache_read_tokens:     Some(40),
                        cache_creation_tokens: Some(20),
                        image_tokens:          Some(20),
                        audio_tokens:          Some(10),
                        quota:                 None,
                        status:                UsageStatus::Success,
                        latency_ms:            1,
                        first_response_ms:     None,
                        is_stream:             false,
                        ip:                    String::new(),
                        upstream_request_id:   String::new(),
                        created_at_unix_ms:    1_700_000_000_000,
                    }],
                })
                .await
                .expect("usage should settle");

            assert_eq!(settlement.settled, 1);
            assert_eq!(settlement.quota, 175);
            let data = store.current_data().expect("data should be readable");
            assert_eq!(data.users[0].quota, 825);
            assert_eq!(data.tokens[0].remain_quota, 825);
        });
    }

    #[test]
    fn builds_snapshot_from_management_data() {
        let mut data = ManagementData::new(
            3,
            Vec::new(),
            vec![TokenRecord {
                id:                   1,
                snapshot_id:          None,
                user_id:              7,
                snapshot_user_id:     None,
                key:                  "dev-token".to_string(),
                status:               STATUS_ENABLED,
                name:                 "Dev".to_string(),
                created_time:         0,
                accessed_time:        0,
                expired_time:         -1,
                remain_quota:         0,
                unlimited_quota:      true,
                model_limits_enabled: true,
                model_limits:         "gpt-4o,claude".to_string(),
                allow_ips:            None,
                used_quota:           0,
                group:                String::new(),
                cross_group_retry:    false,
            }],
            vec![ChannelRecord {
                id:                   2,
                snapshot_id:          None,
                channel_type:         CHANNEL_TYPE_OPENAI,
                key:                  "upstream".to_string(),
                status:               STATUS_ENABLED,
                name:                 "OpenAI".to_string(),
                weight:               Some(1),
                created_time:         0,
                test_time:            0,
                response_time:        0,
                base_url:             Some("https://example.com".to_string()),
                balance:              0.0,
                balance_updated_time: 0,
                models:               "gpt-4o".to_string(),
                group:                "default".to_string(),
                used_quota:           0,
                model_mapping:        Some(r#"{"gpt-4o":"upstream-gpt"}"#.to_string()),
                priority:             Some(0),
                auto_ban:             Some(1),
                tag:                  None,
                setting:              None,
                param_override:       None,
                header_override:      None,
                remark:               None,
                proxy_id:             None,
            }],
            Vec::new(),
        );
        data.tokens[0].allow_ips = Some(" 127.0.0.1\n10.0.0.0/8, ".to_string());
        data.tokens[0].group = "paid".to_string();
        data.channels[0].group = "paid,default".to_string();

        let snapshot = data.build_snapshot().expect("snapshot should build");
        assert_eq!(snapshot.version, 3);
        assert_eq!(snapshot.tokens[0].allowed_models, ["gpt-4o", "claude"]);
        assert_eq!(snapshot.tokens[0].allowed_ips, ["127.0.0.1", "10.0.0.0/8"]);
        assert_eq!(snapshot.tokens[0].user_group, "default");
        assert_eq!(snapshot.tokens[0].token_group, "paid");
        assert_eq!(snapshot.tokens[0].group, "paid");
        assert_eq!(snapshot.channels[0].api_keys, ["upstream"]);
        assert_eq!(snapshot.channels[0].groups, ["paid", "default"]);
        assert_eq!(snapshot.model_mappings[0].requested_model, "gpt-4o");
        assert_eq!(snapshot.model_mappings[0].upstream_model, "upstream-gpt");
    }

    #[test]
    fn build_snapshot_marks_unavailable_tokens_disabled() {
        let mut disabled_user = user(2, "disabled", ROLE_COMMON_USER);
        disabled_user.status = 0;
        let mut expired = token(2, 1, "expired-token");
        expired.expired_time = now_unix().saturating_sub(1);
        let mut exhausted = token(3, 1, "exhausted-token");
        exhausted.remain_quota = 0;
        let data = ManagementData::new(
            3,
            vec![user(1, "root", ROLE_ROOT_USER), disabled_user],
            vec![
                token(1, 1, "active-token"),
                expired,
                exhausted,
                token(4, 2, "disabled-user-token"),
            ],
            Vec::new(),
            Vec::new(),
        );

        let snapshot = data.build_snapshot().expect("snapshot should build");
        let enabled = snapshot
            .tokens
            .iter()
            .map(|token| (token.token.as_str(), token.enabled))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(enabled.get("active-token"), Some(&true));
        assert_eq!(enabled.get("expired-token"), Some(&false));
        assert_eq!(enabled.get("exhausted-token"), Some(&false));
        assert_eq!(enabled.get("disabled-user-token"), Some(&false));
    }

    #[test]
    fn build_snapshot_publishes_enabled_multi_keys() {
        let data = ManagementData::new(
            4,
            Vec::new(),
            Vec::new(),
            vec![ChannelRecord {
                id:                   2,
                snapshot_id:          None,
                channel_type:         CHANNEL_TYPE_OPENAI,
                key:                  "key-a\nkey-b\nkey-c".to_string(),
                status:               STATUS_ENABLED,
                name:                 "OpenAI".to_string(),
                weight:               Some(1),
                created_time:         0,
                test_time:            0,
                response_time:        0,
                base_url:             Some("https://example.com".to_string()),
                balance:              0.0,
                balance_updated_time: 0,
                models:               "gpt-4o".to_string(),
                group:                "default".to_string(),
                used_quota:           0,
                model_mapping:        None,
                priority:             Some(0),
                auto_ban:             Some(1),
                tag:                  None,
                setting:              Some(r#"{"multi_key_status_list":{"1":2}}"#.to_string()),
                param_override:       None,
                header_override:      None,
                remark:               None,
                proxy_id:             None,
            }],
            Vec::new(),
        );

        let snapshot = data.build_snapshot().expect("snapshot should build");
        assert_eq!(snapshot.channels[0].api_key, "key-a");
        assert_eq!(snapshot.channels[0].api_keys, ["key-a", "key-c"]);
    }

    #[test]
    fn keeps_explicit_snapshot_model_mappings() {
        let original = GatewaySnapshot {
            version:          9,
            tokens:           Vec::new(),
            channels:         vec![ChannelConfig {
                id:              "1".to_string(),
                provider:        Provider::OpenAi,
                base_url:        "https://example.com".to_string(),
                api_key:         "key".to_string(),
                api_keys:        vec!["key".to_string(), "key-b".to_string()],
                api_key_indexes: Vec::new(),
                api_key_env:     None,
                enabled:         true,
                weight:          1,
                models:          vec!["upstream".to_string()],
                groups:          Vec::new(),
                proxy:           None,
            }],
            model_mappings:   vec![ModelMapping {
                requested_model: "requested".to_string(),
                channel_id:      "1".to_string(),
                upstream_model:  "upstream".to_string(),
            }],
            channel_affinity: Default::default(),
            group_routing:    Default::default(),
        };

        let rebuilt = ManagementData::from_snapshot(original)
            .build_snapshot()
            .expect("snapshot should rebuild");
        assert!(rebuilt.model_mappings.iter().any(|mapping| {
            mapping.requested_model == "requested" && mapping.upstream_model == "upstream"
        }));
    }

    #[test]
    fn preserves_non_numeric_snapshot_ids() {
        let original = GatewaySnapshot {
            version:          9,
            tokens:           vec![TokenConfig {
                id:             "dev-token-id".to_string(),
                token:          "dev-token".to_string(),
                user_id:        "dev-user".to_string(),
                user_group:     "default".to_string(),
                token_group:    String::new(),
                group:          "default".to_string(),
                enabled:        true,
                allowed_models: vec!["deepseek-v4-pro".to_string()],
                allowed_ips:    Vec::new(),
            }],
            channels:         vec![ChannelConfig {
                id:              "openai-main".to_string(),
                provider:        Provider::OpenAi,
                base_url:        "https://example.com".to_string(),
                api_key:         "key".to_string(),
                api_keys:        vec!["key".to_string(), "key-b".to_string()],
                api_key_indexes: Vec::new(),
                api_key_env:     None,
                enabled:         true,
                weight:          1,
                models:          vec!["deepseek-v4-pro".to_string()],
                groups:          Vec::new(),
                proxy:           None,
            }],
            model_mappings:   vec![ModelMapping {
                requested_model: "deepseek-v4-pro".to_string(),
                channel_id:      "openai-main".to_string(),
                upstream_model:  "deepseek-v4-pro".to_string(),
            }],
            channel_affinity: Default::default(),
            group_routing:    Default::default(),
        };

        let rebuilt = ManagementData::from_snapshot(original)
            .build_snapshot()
            .expect("snapshot should rebuild");

        assert_eq!(rebuilt.tokens[0].id, "dev-token-id");
        assert_eq!(rebuilt.tokens[0].user_id, "dev-user");
        assert_eq!(rebuilt.channels[0].id, "openai-main");
        assert_eq!(rebuilt.channels[0].api_keys, ["key", "key-b"]);
        assert!(rebuilt.model_mappings.iter().any(|mapping| {
            mapping.requested_model == "deepseek-v4-pro"
                && mapping.channel_id == "openai-main"
                && mapping.upstream_model == "deepseek-v4-pro"
        }));
    }

    #[test]
    fn authenticates_configured_user() {
        block_on(async {
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                vec![user(1, "root", ROLE_ROOT_USER)],
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ));

            let user = store
                .call(LoginUserRequest {
                    username: "root".to_string(),
                    password: "password".to_string(),
                })
                .await
                .expect("login should succeed");

            assert_eq!(user.id, 1);
            assert!(user.password.is_empty());
        });
    }

    #[test]
    fn bootstraps_single_root_user() {
        block_on(async {
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ));

            let created = store
                .call(BootstrapRootUserRequest {
                    username: "root".to_string(),
                    password: "password".to_string(),
                })
                .await
                .expect("root bootstrap should succeed");
            assert_eq!(created.username, "root");
            assert_eq!(created.display_name, "Root User");
            assert_eq!(created.role, ROLE_ROOT_USER);
            assert_eq!(created.status, STATUS_ENABLED);
            assert_eq!(created.quota, 100_000_000);
            assert!(created.password.is_empty());

            let data = store.current_data().expect("data should be readable");
            assert_eq!(data.users.len(), 1);
            assert!(verify_user_password(&data.users[0].password, "password"));

            let err = store
                .call(BootstrapRootUserRequest {
                    username: "another".to_string(),
                    password: "password".to_string(),
                })
                .await
                .expect_err("second root bootstrap should be rejected");
            assert!(matches!(err, ManagementError::Duplicate));
        });
    }

    #[test]
    fn registers_common_user_with_optional_default_token() {
        block_on(async {
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ));
            let mut alice = user(42, "alice", ROLE_ROOT_USER);
            alice.password = "password".to_string();
            alice.status = 0;
            alice.quota = 123_456;
            let mut initial_token = token(0, 0, "initial-token");
            initial_token.group = "auto".to_string();

            let registered = store
                .call(RegisterUserRequest {
                    user:          alice,
                    default_token: Some(initial_token),
                })
                .await
                .expect("registration should succeed");

            assert_eq!(registered.user.id, 1);
            assert_eq!(registered.user.username, "alice");
            assert_eq!(registered.user.role, ROLE_COMMON_USER);
            assert_eq!(registered.user.status, STATUS_ENABLED);
            assert_eq!(registered.user.quota, 0);
            assert!(registered.user.password.is_empty());
            assert!(registered.default_token.is_some());

            let data = store.current_data().expect("data should be readable");
            assert_eq!(data.users.len(), 1);
            assert_eq!(data.tokens.len(), 1);
            assert!(verify_user_password(&data.users[0].password, "password"));
            assert_eq!(data.tokens[0].user_id, data.users[0].id);
            assert_eq!(data.tokens[0].key, "initial-token");
            assert_eq!(data.tokens[0].group, "auto");

            let mut duplicate = user(0, "alice", ROLE_COMMON_USER);
            duplicate.password = "password".to_string();
            let err = store
                .call(RegisterUserRequest {
                    user:          duplicate,
                    default_token: None,
                })
                .await
                .expect_err("duplicate username should be rejected");
            assert!(matches!(err, ManagementError::Duplicate));
        });
    }

    #[test]
    fn root_can_create_common_user_but_not_peer_root() {
        block_on(async {
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                vec![user(1, "root", ROLE_ROOT_USER)],
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ));

            let created = store
                .call(CreateUserRequest {
                    user:       user(0, "alice", ROLE_COMMON_USER),
                    actor_role: ROLE_ROOT_USER,
                })
                .await
                .expect("root should create common user");
            assert_eq!(created.username, "alice");

            let err = store
                .call(CreateUserRequest {
                    user:       user(0, "peer-root", ROLE_ROOT_USER),
                    actor_role: ROLE_ROOT_USER,
                })
                .await
                .expect_err("root should not create peer root");
            assert!(matches!(err, ManagementError::PermissionDenied));
        });
    }

    #[test]
    fn validates_user_access_token() {
        block_on(async {
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                vec![user(1, "root", ROLE_ROOT_USER)],
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ));

            let token = store
                .call(UpdateUserAccessTokenRequest {
                    id:           1,
                    access_token: "access-token".to_string(),
                })
                .await
                .expect("access token should update");
            assert_eq!(token, "access-token");

            let user = store
                .call(ValidateUserAccessTokenRequest {
                    access_token: "access-token".to_string(),
                })
                .await
                .expect("access token should authenticate");
            assert_eq!(user.id, 1);
            assert!(user.access_token.is_none());
        });
    }
}
