use crate::{PublishSnapshotRequest, SnapshotError, SnapshotPublished, SnapshotPublisher};
use bcrypt::{DEFAULT_COST, hash, verify};
use halolake_domain::{
    CHANNEL_TYPE_ANTHROPIC, CHANNEL_TYPE_GEMINI, CHANNEL_TYPE_OPENAI, ChannelRecord, PageRequest,
    PageResult, ROLE_ADMIN_USER, ROLE_COMMON_USER, ROLE_ROOT_USER, STATUS_AUTO_DISABLED,
    STATUS_ENABLED, STATUS_MANUALLY_DISABLED, SearchRequest, TokenRecord, UsageEvent, UsageStatus,
    UserRecord,
};
use halolake_router_core::{ChannelConfig, GatewaySnapshot, ModelMapping, Provider, TokenConfig};
use serde_json::Value as JsonValue;
use service_async::Service;
use sha2::{Digest, Sha256};
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

/// Allocate positive, unique management ids without letting an alias consume a
/// numeric id that another snapshot channel explicitly owns.
///
/// Older snapshots only had `ChannelConfig.id`; newer snapshots carry an
/// independent `management_id`. Pre-reserving every explicit id makes mixed
/// inputs such as `foo`, `1` deterministic instead of assigning both records
/// management id 1.
fn allocate_snapshot_channel_ids(channels: &[ChannelConfig]) -> Vec<u64> {
    let preferred = channels
        .iter()
        .map(|channel| {
            channel
                .management_id
                .filter(|id| *id > 0)
                .or_else(|| channel.id.trim().parse::<u64>().ok().filter(|id| *id > 0))
        })
        .collect::<Vec<_>>();
    let reserved_ids = preferred.iter().flatten().copied().collect::<HashSet<_>>();
    let reserved_route_ids = channels
        .iter()
        .map(|channel| channel.id.trim())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let mut used = HashSet::new();
    let mut next = 1u64;

    preferred
        .into_iter()
        .map(|preferred| {
            if let Some(id) = preferred
                && used.insert(id)
            {
                return id;
            }
            loop {
                let candidate = next;
                next = next.saturating_add(1);
                if candidate == 0
                    || used.contains(&candidate)
                    || reserved_ids.contains(&candidate)
                    || reserved_route_ids.contains(&candidate.to_string())
                {
                    continue;
                }
                used.insert(candidate);
                return candidate;
            }
        })
        .collect()
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

        let channel_ids = allocate_snapshot_channel_ids(&snapshot.channels);
        let mut route_ids = HashSet::new();
        let channels = snapshot
            .channels
            .into_iter()
            .zip(channel_ids)
            .map(|(channel, id)| {
                let key = snapshot_channel_key(&channel);
                let imported_route_id = channel.id.trim().to_string();
                let snapshot_id = (!imported_route_id.is_empty()
                    && route_ids.insert(imported_route_id.clone()))
                .then_some(imported_route_id.clone());
                let setting = channel
                    .proxy
                    .as_ref()
                    .and_then(|proxy| {
                        let proxy = proxy.trim();
                        if proxy.is_empty() {
                            None
                        } else {
                            serde_json::to_string(&serde_json::json!({ "proxy": proxy })).ok()
                        }
                    })
                    .or_else(|| {
                        channel
                            .proxy_required
                            .then(|| serde_json::json!({ "proxy_required": true }).to_string())
                    });
                ChannelRecord {
                    id,
                    snapshot_id,
                    channel_type: channel_type_from_provider(channel.provider),
                    key,
                    status: if channel.enabled { STATUS_ENABLED } else { 0 },
                    name: if imported_route_id.is_empty() {
                        id.to_string()
                    } else {
                        imported_route_id
                    },
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

        let mut channels = Vec::new();
        let mut channel_mappings = Vec::new();
        let mut valid_route_ids = HashSet::new();
        for channel in &self.channels {
            let Some(config) = channel_config(channel)? else {
                continue;
            };
            // A malformed enabled channel fails closed without preventing a
            // healthy channel from being published. Create/update/status paths
            // reject this state; this guard isolates legacy database rows.
            let derived_mappings = if channel.status == STATUS_ENABLED {
                match channel_model_mappings(channel) {
                    Ok(mappings) => mappings,
                    Err(_) => continue,
                }
            } else {
                Vec::new()
            };
            // Do not let a duplicate canonical route id be silently overwritten
            // by router-core's index. Keep the first record and fail later rows
            // closed; mutation paths prevent introducing new duplicates.
            if config.id.trim().is_empty() || !valid_route_ids.insert(config.id.clone()) {
                continue;
            }
            channels.push(config);
            channel_mappings.extend(derived_mappings);
        }

        let mut mappings = self
            .model_mappings
            .iter()
            .filter(|mapping| valid_route_ids.contains(&mapping.channel_id))
            .cloned()
            .map(|mapping| {
                (
                    (mapping.requested_model.clone(), mapping.channel_id.clone()),
                    mapping,
                )
            })
            .collect::<BTreeMap<_, _>>();
        for mapping in channel_mappings {
            mappings.insert(
                (mapping.requested_model.clone(), mapping.channel_id.clone()),
                mapping,
            );
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
    #[error("channel {0} changed while an operation was in flight")]
    StaleChannelUpdate(u64),
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
    /// Snapshot version produced by this mutation. `None` means the request
    /// contained no events and did not mutate management state.
    pub version:          Option<u64>,
    pub settled:          usize,
    pub skipped:          usize,
    pub quota:            i64,
    pub tokens_exhausted: usize,
    pub event_quotas:     Vec<UsageEventQuota>,
    /// Compact post-mutation rows used by SQL backends. Carrying these values
    /// avoids cloning and scanning the complete ManagementData after every
    /// accounting batch.
    pub users:            Vec<SettledUserState>,
    pub tokens:           Vec<SettledTokenState>,
    pub channels:         Vec<SettledChannelState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettledUserState {
    pub id:         u64,
    pub quota:      i64,
    pub used_quota: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettledTokenState {
    pub id:           u64,
    pub accessed_at:  i64,
    pub remain_quota: i64,
    pub used_quota:   i64,
    pub status:       i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettledChannelState {
    pub id:         u64,
    pub used_quota: i64,
}

/// Sparse usage mutation prepared against one management version. SQL stores
/// persist `settlement` first, then apply the indexed post-state to memory.
/// The plan owns only affected counters plus vector indexes; it never clones
/// the complete management records.
#[derive(Debug)]
pub struct PlannedUsageSettlement {
    settlement:      UsageSettlement,
    base_version:    u64,
    user_indices:    Vec<usize>,
    token_indices:   Vec<usize>,
    channel_indices: Vec<usize>,
}

impl PlannedUsageSettlement {
    pub fn settlement(&self) -> &UsageSettlement {
        &self.settlement
    }
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

#[derive(Debug, Clone)]
pub struct ListChannelsRequest {
    pub page:         PageRequest,
    pub group:        String,
    pub status:       Option<i32>,
    pub channel_type: Option<i32>,
    pub sort_by:      String,
    pub sort_order:   String,
    pub id_sort:      bool,
    pub tag_mode:     bool,
}

#[derive(Debug, Clone)]
pub struct SearchChannelsRequest {
    pub search:       SearchRequest,
    pub group:        String,
    pub model:        String,
    pub status:       Option<i32>,
    pub channel_type: Option<i32>,
    pub sort_by:      String,
    pub sort_order:   String,
    pub id_sort:      bool,
    pub tag_mode:     bool,
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

#[derive(Debug, Clone)]
pub struct DeleteChannelsBatchRequest {
    pub ids: Vec<u64>,
}

#[derive(Debug, Clone, Copy, serde::Deserialize)]
pub struct ChannelStatusUpdateRequest {
    pub id:     u64,
    pub status: i32,
}

#[derive(Debug, Clone)]
pub struct BatchUpdateChannelStatusRequest {
    pub ids:    Vec<u64>,
    pub status: i32,
}

/// Field-scoped writes used after network operations. They deliberately avoid
/// replacing a stale `ChannelRecord` captured before an `.await`.
#[derive(Debug, Clone, Copy)]
pub struct PatchChannelProbeMetricsRequest {
    pub id:            u64,
    pub test_time:     i64,
    pub response_time: i32,
}

#[derive(Debug, Clone)]
pub struct PatchChannelBalanceRequest {
    pub id:                    u64,
    pub expected_key:          String,
    pub balance:               f64,
    pub balance_updated_time:  i64,
    pub auto_disable_if_empty: bool,
}

#[derive(Debug, Clone)]
pub struct PatchChannelModelStateRequest {
    pub id:              u64,
    pub expected_models: String,
    pub models:          String,
    /// JSON object containing only model-sync setting fields.
    pub setting_patch:   Option<String>,
}

#[derive(Debug, Clone)]
pub struct RotateChannelCredentialRequest {
    pub id:            u64,
    pub expected_key:  String,
    pub new_key:       String,
    /// JSON object containing only fields changed by credential refresh.
    pub setting_patch: Option<String>,
}

/// Auto-ban a single channel by numeric id only (gateway feedback).
/// Applied inside `mutate` so other channels cannot be rewritten by a full UpdateChannel.
#[derive(Debug, Clone)]
pub struct AutoDisableChannelRequest {
    pub id:                  u64,
    pub reason:              String,
    pub api_key_index:       Option<usize>,
    pub api_key_fingerprint: Option<String>,
    pub created_at_unix_ms:  i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutoDisableChannelResult {
    pub changed:          bool,
    pub channel_disabled: bool,
    pub key_disabled:     bool,
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

#[derive(Debug)]
struct ManagementIdentityIndexes {
    users_by_id:          HashMap<u64, usize>,
    tokens_by_id:         HashMap<u64, usize>,
    tokens_by_snapshot:   HashMap<String, usize>,
    channels_by_id:       HashMap<u64, usize>,
    channels_by_snapshot: HashMap<String, usize>,
}

impl ManagementIdentityIndexes {
    fn new(data: &ManagementData) -> Self {
        Self {
            users_by_id:          data
                .users
                .iter()
                .enumerate()
                .map(|(index, user)| (user.id, index))
                .collect(),
            tokens_by_id:         data
                .tokens
                .iter()
                .enumerate()
                .map(|(index, token)| (token.id, index))
                .collect(),
            tokens_by_snapshot:   data
                .tokens
                .iter()
                .enumerate()
                .filter_map(|(index, token)| {
                    token
                        .snapshot_id
                        .as_ref()
                        .map(|snapshot_id| (snapshot_id.clone(), index))
                })
                .fold(HashMap::new(), |mut indexes, (snapshot_id, index)| {
                    // Preserve the former Vec::position semantics if legacy
                    // data contains duplicate aliases.
                    indexes.entry(snapshot_id).or_insert(index);
                    indexes
                }),
            channels_by_id:       data
                .channels
                .iter()
                .enumerate()
                .map(|(index, channel)| (channel.id, index))
                .collect(),
            channels_by_snapshot: data
                .channels
                .iter()
                .enumerate()
                .filter_map(|(index, channel)| {
                    channel
                        .snapshot_id
                        .as_ref()
                        .map(|snapshot_id| (snapshot_id.clone(), index))
                })
                .fold(HashMap::new(), |mut indexes, (snapshot_id, index)| {
                    indexes.entry(snapshot_id).or_insert(index);
                    indexes
                }),
        }
    }
}

#[derive(Debug)]
struct MemoryManagementState {
    data:     ManagementData,
    identity: ManagementIdentityIndexes,
}

impl MemoryManagementState {
    fn new(data: ManagementData) -> Self {
        let identity = ManagementIdentityIndexes::new(&data);
        Self { data, identity }
    }

    fn rebuild_identity(&mut self) {
        self.identity = ManagementIdentityIndexes::new(&self.data);
    }
}

#[derive(Debug, Clone)]
pub struct MemoryManagementStore {
    inner: Arc<RwLock<MemoryManagementState>>,
}

impl MemoryManagementStore {
    pub fn new(data: ManagementData) -> Self {
        Self {
            inner: Arc::new(RwLock::new(MemoryManagementState::new(data))),
        }
    }

    pub fn current_data(&self) -> Result<ManagementData, ManagementError> {
        self.inner
            .read()
            .map(|state| state.data.clone())
            .map_err(|_| ManagementError::Poisoned("management"))
    }

    /// Atomically replaces the published in-memory management snapshot.
    /// SQL-backed stores use this only after the candidate state commits, so
    /// failed persistence never becomes visible to readers or later writes.
    pub fn replace_data(&self, data: ManagementData) -> Result<(), ManagementError> {
        let replacement = MemoryManagementState::new(data);
        *self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("management"))? = replacement;
        Ok(())
    }

    /// Consumes an isolated store and returns its owned snapshot without an
    /// additional deep clone. SQL-backed stores use this for write candidates.
    pub fn into_data(self) -> Result<ManagementData, ManagementError> {
        match Arc::try_unwrap(self.inner) {
            Ok(inner) => inner
                .into_inner()
                .map(|state| state.data)
                .map_err(|_| ManagementError::Poisoned("management")),
            Err(inner) => inner
                .read()
                .map(|state| state.data.clone())
                .map_err(|_| ManagementError::Poisoned("management")),
        }
    }

    /// Plans a usage mutation while holding only a read lock. The returned
    /// value contains compact post-state for affected entities and indexes into
    /// the current vectors, but does not publish any mutation.
    pub fn plan_usage_settlement(
        &self,
        req: SettleUsageRequest,
    ) -> Result<PlannedUsageSettlement, ManagementError> {
        let state = self
            .inner
            .read()
            .map_err(|_| ManagementError::Poisoned("management"))?;
        Ok(plan_usage_settlement(&state.data, &state.identity, req))
    }

    /// Atomically publishes a previously persisted sparse usage plan.
    pub fn apply_planned_usage_settlement(
        &self,
        plan: PlannedUsageSettlement,
    ) -> Result<UsageSettlement, ManagementError> {
        let mut state = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("management"))?;
        apply_usage_plan(&mut state.data, plan)
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
        let mut state = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("management"))?;
        let out = f(&mut state.data);
        if out.is_ok() {
            state.data.version = state.data.version.saturating_add(1);
        }
        state.rebuild_identity();
        out
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
            // new-api: RoleGuestUser (0) means "not changing role" on PUT update.
            // Frontend omits role on update; serde then defaults to 0 (not common=1).
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
            // Always preserve role on this path (promote/demote via manage only).
            updated.role = user.role;
            // enable/disable via manage only — FE PUT omits status (defaults to enabled).
            updated.status = user.status;
            // Preserve quota/used_quota when client omits them (serde default 0).
            // new-api FE adjusts quota via /api/user/manage, not PUT body.
            if updated.quota == 0 && user.quota != 0 {
                updated.quota = user.quota;
            }
            if updated.used_quota == 0 && user.used_quota != 0 {
                updated.used_quota = user.used_quota;
            }
            if updated.created_at == 0 {
                updated.created_at = user.created_at;
            }
            if updated.last_login_at == 0 {
                updated.last_login_at = user.last_login_at;
            }
            if updated.email.is_empty() {
                updated.email.clone_from(&user.email);
            }
            if updated.setting.is_empty() {
                updated.setting.clone_from(&user.setting);
            }
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

fn plan_usage_settlement(
    data: &ManagementData,
    identity: &ManagementIdentityIndexes,
    req: SettleUsageRequest,
) -> PlannedUsageSettlement {
    let base_version = data.version;
    if req.events.is_empty() {
        return PlannedUsageSettlement {
            settlement: UsageSettlement::default(),
            base_version,
            user_indices: Vec::new(),
            token_indices: Vec::new(),
            channel_indices: Vec::new(),
        };
    }

    let SettleUsageRequest { events, pricing } = req;
    let now = now_unix();
    let mut user_states = HashMap::<usize, SettledUserState>::new();
    let mut token_states = HashMap::<usize, SettledTokenState>::new();
    let mut channel_states = HashMap::<usize, SettledChannelState>::new();
    let mut settlement = UsageSettlement::default();

    for event in events {
        let token_idx = parse_usage_entity_id(&event.token_id)
            .and_then(|id| identity.tokens_by_id.get(&id).copied())
            .or_else(|| {
                identity
                    .tokens_by_snapshot
                    .get(event.token_id.as_str())
                    .copied()
            });
        let user_id = token_idx
            .map(|idx| data.tokens[idx].user_id)
            .or_else(|| parse_usage_entity_id(&event.user_id));
        let Some(user_idx) = user_id.and_then(|id| identity.users_by_id.get(&id).copied()) else {
            settlement.skipped = settlement.skipped.saturating_add(1);
            continue;
        };
        // Channel ids deliberately keep numeric-first semantics: a numeric id
        // never falls through to a textual snapshot alias with the same bytes.
        let channel_idx = match parse_usage_entity_id(&event.channel_id) {
            Some(id) => identity.channels_by_id.get(&id).copied(),
            None => identity
                .channels_by_snapshot
                .get(event.channel_id.as_str())
                .copied(),
        };

        if let Some(index) = token_idx {
            let token = &data.tokens[index];
            token_states
                .entry(index)
                .or_insert(SettledTokenState {
                    id:           token.id,
                    accessed_at:  now,
                    remain_quota: token.remain_quota,
                    used_quota:   token.used_quota,
                    status:       token.status,
                })
                .accessed_at = now;
        }

        let token = token_idx.and_then(|index| data.tokens.get(index));
        let user = data.users.get(user_idx);
        let channel = channel_idx.and_then(|index| data.channels.get(index));
        let quota = usage_event_quota(&event, &pricing, token, user, channel);
        if quota <= 0 {
            settlement.skipped = settlement.skipped.saturating_add(1);
            continue;
        }

        settlement.event_quotas.push(UsageEventQuota {
            request_id: event.request_id.clone(),
            quota,
        });

        let user = &data.users[user_idx];
        let user_state = user_states.entry(user_idx).or_insert(SettledUserState {
            id:         user.id,
            quota:      user.quota,
            used_quota: user.used_quota,
        });
        user_state.quota = user_state.quota.saturating_sub(quota);
        user_state.used_quota = user_state.used_quota.saturating_add(quota);

        if let Some(index) = token_idx {
            let token = &data.tokens[index];
            let token_state = token_states
                .get_mut(&index)
                .expect("token post-state initialized above");
            token_state.remain_quota = token_state.remain_quota.saturating_sub(quota);
            token_state.used_quota = token_state.used_quota.saturating_add(quota);
            if !token.unlimited_quota
                && token_state.remain_quota <= 0
                && token_state.status != TOKEN_STATUS_EXHAUSTED
            {
                token_state.status = TOKEN_STATUS_EXHAUSTED;
                settlement.tokens_exhausted = settlement.tokens_exhausted.saturating_add(1);
            }
        }

        if let Some(index) = channel_idx {
            let channel = &data.channels[index];
            let channel_state = channel_states.entry(index).or_insert(SettledChannelState {
                id:         channel.id,
                used_quota: channel.used_quota,
            });
            channel_state.used_quota = channel_state.used_quota.saturating_add(quota);
        }

        settlement.settled = settlement.settled.saturating_add(1);
        settlement.quota = settlement.quota.saturating_add(quota);
    }

    let mut users = user_states.into_iter().collect::<Vec<_>>();
    let mut tokens = token_states.into_iter().collect::<Vec<_>>();
    let mut channels = channel_states.into_iter().collect::<Vec<_>>();
    users.sort_unstable_by_key(|(_, state)| state.id);
    tokens.sort_unstable_by_key(|(_, state)| state.id);
    channels.sort_unstable_by_key(|(_, state)| state.id);
    let (user_indices, settled_users) = users.into_iter().unzip();
    let (token_indices, settled_tokens) = tokens.into_iter().unzip();
    let (channel_indices, settled_channels) = channels.into_iter().unzip();
    settlement.version = Some(base_version.saturating_add(1));
    settlement.users = settled_users;
    settlement.tokens = settled_tokens;
    settlement.channels = settled_channels;

    PlannedUsageSettlement {
        settlement,
        base_version,
        user_indices,
        token_indices,
        channel_indices,
    }
}

fn apply_usage_plan(
    data: &mut ManagementData,
    plan: PlannedUsageSettlement,
) -> Result<UsageSettlement, ManagementError> {
    let PlannedUsageSettlement {
        settlement,
        base_version,
        user_indices,
        token_indices,
        channel_indices,
    } = plan;
    let Some(version) = settlement.version else {
        return Ok(settlement);
    };
    let indexes_match = data.version == base_version
        && user_indices.len() == settlement.users.len()
        && token_indices.len() == settlement.tokens.len()
        && channel_indices.len() == settlement.channels.len()
        && user_indices
            .iter()
            .zip(&settlement.users)
            .all(|(index, state)| {
                data.users
                    .get(*index)
                    .is_some_and(|user| user.id == state.id)
            })
        && token_indices
            .iter()
            .zip(&settlement.tokens)
            .all(|(index, state)| {
                data.tokens
                    .get(*index)
                    .is_some_and(|token| token.id == state.id)
            })
        && channel_indices
            .iter()
            .zip(&settlement.channels)
            .all(|(index, state)| {
                data.channels
                    .get(*index)
                    .is_some_and(|channel| channel.id == state.id)
            });
    if !indexes_match {
        return Err(ManagementError::Storage(
            "stale usage settlement plan".to_string(),
        ));
    }

    for (index, state) in user_indices.into_iter().zip(&settlement.users) {
        let user = &mut data.users[index];
        user.quota = state.quota;
        user.used_quota = state.used_quota;
    }
    for (index, state) in token_indices.into_iter().zip(&settlement.tokens) {
        let token = &mut data.tokens[index];
        token.accessed_time = state.accessed_at;
        token.remain_quota = state.remain_quota;
        token.used_quota = state.used_quota;
        token.status = state.status;
    }
    for (index, state) in channel_indices.into_iter().zip(&settlement.channels) {
        data.channels[index].used_quota = state.used_quota;
    }
    data.version = version;
    Ok(settlement)
}

impl Service<SettleUsageRequest> for MemoryManagementStore {
    type Response = UsageSettlement;
    type Error = ManagementError;

    async fn call(&self, req: SettleUsageRequest) -> Result<Self::Response, Self::Error> {
        let mut state = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("management"))?;
        let plan = plan_usage_settlement(&state.data, &state.identity, req);
        apply_usage_plan(&mut state.data, plan)
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
        let group = normalize_channel_group_filter(&req.group);
        let mut channels = data
            .channels
            .into_iter()
            .filter(|channel| {
                channel_matches_list_filters(
                    channel,
                    group.as_deref(),
                    req.status,
                    None,
                    None,
                    None,
                )
            })
            .collect::<Vec<_>>();
        if let Some(channel_type) = req.channel_type {
            channels.retain(|channel| channel.channel_type == channel_type);
        }
        sort_channels(&mut channels, &req.sort_by, &req.sort_order, req.id_sort);
        if req.tag_mode {
            return Ok(page_channels_by_tag(channels, req.page));
        }
        let channels = channels
            .into_iter()
            .map(ChannelRecord::masked)
            .collect::<Vec<_>>();
        Ok(page(channels, req.page))
    }
}

impl Service<SearchChannelsRequest> for MemoryManagementStore {
    type Response = PageResult<ChannelRecord>;
    type Error = ManagementError;

    async fn call(&self, req: SearchChannelsRequest) -> Result<Self::Response, Self::Error> {
        let data = self.current_data()?;
        let keyword = req.search.keyword.trim().to_ascii_lowercase();
        let model = req.model.trim().to_ascii_lowercase();
        let group = normalize_channel_group_filter(&req.group);
        let mut channels = data
            .channels
            .into_iter()
            .filter(|channel| {
                channel_matches_list_filters(
                    channel,
                    group.as_deref(),
                    req.status,
                    None,
                    Some(keyword.as_str()),
                    Some(model.as_str()),
                )
            })
            .collect::<Vec<_>>();
        if let Some(channel_type) = req.channel_type {
            channels.retain(|channel| channel.channel_type == channel_type);
        }
        sort_channels(&mut channels, &req.sort_by, &req.sort_order, req.id_sort);
        if req.tag_mode {
            return Ok(page_channels_by_tag(channels, req.search.page));
        }
        let channels = channels
            .into_iter()
            .map(ChannelRecord::masked)
            .collect::<Vec<_>>();
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
                channel.id = next_channel_id(&data.channels);
            }
            if channel.id == 0 {
                return Err(ManagementError::InvalidRequest(
                    "channel id must be positive",
                ));
            }
            channel_model_mappings(&channel)?;
            let route_id = channel_snapshot_id(&channel);
            if data
                .channels
                .iter()
                .any(|item| item.id == channel.id || channel_snapshot_id(item) == route_id)
            {
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
            // new-api Save/Update: empty key means "keep existing" (masked list rows).
            if updated.key.trim().is_empty() {
                updated.key.clone_from(&channel.key);
            }
            // Empty name must not wipe an existing channel name (partial / model-only saves).
            if updated.name.trim().is_empty() {
                updated.name.clone_from(&channel.name);
            }
            // Empty group must not wipe an existing assignment (partial payloads).
            if updated.group.trim().is_empty() {
                updated.group.clone_from(&channel.group);
            }
            // Merge JSON fields so sparse or masked admin saves preserve runtime
            // metadata and stored credential values.
            updated.setting =
                merge_channel_json(channel.setting.as_deref(), updated.setting.as_deref());
            updated.header_override = merge_channel_json(
                channel.header_override.as_deref(),
                updated.header_override.as_deref(),
            );
            updated.param_override = merge_channel_json(
                channel.param_override.as_deref(),
                updated.param_override.as_deref(),
            );
            if updated.proxy_id.is_none() {
                updated.proxy_id = channel.proxy_id;
            }
            // Empty Option/string fields from partial admin payloads (fetch-models
            // then save, null-stripped JSON, serde defaults) must not wipe durable
            // channel config. Intentional clear is rare; type defaults apply when
            // both sides are empty.
            preserve_nonempty_opt(&mut updated.base_url, channel.base_url.as_deref());
            preserve_nonempty_opt(&mut updated.model_mapping, channel.model_mapping.as_deref());
            preserve_nonempty_opt(&mut updated.remark, channel.remark.as_deref());
            preserve_nonempty_opt(&mut updated.tag, channel.tag.as_deref());
            // Usage / probe counters are not part of the admin form payload (serde
            // defaults them to 0). Preserve like new-api GORM Updates that only
            // touch fields present on the model instance from DB + form.
            if updated.used_quota == 0 && channel.used_quota != 0 {
                updated.used_quota = channel.used_quota;
            }
            if updated.balance == 0.0 && channel.balance != 0.0 {
                updated.balance = channel.balance;
            }
            if updated.balance_updated_time == 0 && channel.balance_updated_time != 0 {
                updated.balance_updated_time = channel.balance_updated_time;
            }
            if updated.test_time == 0 && channel.test_time != 0 {
                updated.test_time = channel.test_time;
            }
            if updated.response_time == 0 && channel.response_time != 0 {
                updated.response_time = channel.response_time;
            }
            if updated.created_time == 0 && channel.created_time != 0 {
                updated.created_time = channel.created_time;
            }
            updated.snapshot_id.clone_from(&channel.snapshot_id);
            channel_model_mappings(&updated)?;
            *channel = updated.clone();
            Ok(updated.masked())
        })
    }
}

fn preserve_nonempty_opt(target: &mut Option<String>, existing: Option<&str>) {
    let incoming_empty = target
        .as_deref()
        .map(str::trim)
        .map(|s| s.is_empty())
        .unwrap_or(true);
    if !incoming_empty {
        return;
    }
    if let Some(existing) = existing.map(str::trim).filter(|s| !s.is_empty()) {
        *target = Some(existing.to_string());
    }
}

/// Recursively overlay incoming JSON while preserving stored secrets hidden by masked reads.
fn merge_channel_json(existing: Option<&str>, incoming: Option<&str>) -> Option<String> {
    let existing_raw = existing
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let Some(incoming_raw) = incoming.filter(|value| !value.trim().is_empty()) else {
        return existing_raw;
    };
    let Ok(incoming_value) = serde_json::from_str::<JsonValue>(incoming_raw) else {
        return existing_raw;
    };
    let Some(mut existing_value) = existing_raw
        .as_deref()
        .and_then(|value| serde_json::from_str::<JsonValue>(value).ok())
    else {
        return serde_json::to_string(&incoming_value).ok();
    };
    merge_channel_json_value(&mut existing_value, incoming_value);
    serde_json::to_string(&existing_value).ok().or(existing_raw)
}

fn merge_channel_json_value(existing: &mut JsonValue, incoming: JsonValue) {
    match (existing, incoming) {
        (JsonValue::Object(base), JsonValue::Object(overlay)) => {
            for (key, value) in overlay {
                if is_masked_sensitive_json_value(&key, &value) {
                    continue;
                }
                match base.get_mut(&key) {
                    Some(existing) => merge_channel_json_value(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (JsonValue::Array(base), JsonValue::Array(overlay)) => {
            let incoming_len = overlay.len();
            for (idx, value) in overlay.into_iter().enumerate() {
                if let Some(existing) = base.get_mut(idx) {
                    merge_channel_json_value(existing, value);
                } else {
                    base.push(value);
                }
            }
            base.truncate(incoming_len);
        }
        (existing, incoming) => *existing = incoming,
    }
}

fn is_masked_sensitive_json_value(key: &str, value: &JsonValue) -> bool {
    if !value.as_str().is_some_and(|value| value.trim().is_empty()) {
        return false;
    }
    let key = key.to_ascii_lowercase();
    let compact = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>();
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

fn normalized_channel_ids(ids: Vec<u64>) -> Result<HashSet<u64>, ManagementError> {
    if ids.is_empty() || ids.contains(&0) {
        return Err(ManagementError::InvalidRequest(
            "positive channel ids are required",
        ));
    }
    Ok(ids.into_iter().collect())
}

fn validate_manual_channel_status(status: i32) -> Result<(), ManagementError> {
    if matches!(status, STATUS_ENABLED | STATUS_MANUALLY_DISABLED) {
        Ok(())
    } else {
        Err(ManagementError::InvalidRequest(
            "manual channel status must be enabled or manually disabled",
        ))
    }
}

impl Service<DeleteChannelsBatchRequest> for MemoryManagementStore {
    type Response = usize;
    type Error = ManagementError;

    async fn call(&self, req: DeleteChannelsBatchRequest) -> Result<Self::Response, Self::Error> {
        let ids = normalized_channel_ids(req.ids)?;
        self.mutate(|data| {
            if ids
                .iter()
                .any(|id| !data.channels.iter().any(|channel| channel.id == *id))
            {
                return Err(ManagementError::NotFound);
            }
            let before = data.channels.len();
            data.channels.retain(|channel| !ids.contains(&channel.id));
            Ok(before.saturating_sub(data.channels.len()))
        })
    }
}

impl Service<ChannelStatusUpdateRequest> for MemoryManagementStore {
    type Response = bool;
    type Error = ManagementError;

    async fn call(&self, req: ChannelStatusUpdateRequest) -> Result<Self::Response, Self::Error> {
        validate_manual_channel_status(req.status)?;
        self.mutate(|data| {
            let channel = data
                .channels
                .iter_mut()
                .find(|channel| channel.id == req.id)
                .ok_or(ManagementError::NotFound)?;
            if req.status == STATUS_ENABLED {
                channel_model_mappings(channel)?;
            }
            let changed = channel_status_update_needed(channel, req.status);
            if !changed {
                return Ok(false);
            }
            // Align with new-api `UpdateChannelStatus` / `handlerMultiKeyUpdate`:
            // enabling clears multi-key disabled maps and status_reason so the
            // channel is actually routable again (status flag alone is not enough).
            apply_channel_status_update(channel, req.status, "manual operation");
            Ok(true)
        })
    }
}

impl Service<BatchUpdateChannelStatusRequest> for MemoryManagementStore {
    type Response = usize;
    type Error = ManagementError;

    async fn call(
        &self,
        req: BatchUpdateChannelStatusRequest,
    ) -> Result<Self::Response, Self::Error> {
        validate_manual_channel_status(req.status)?;
        let ids = normalized_channel_ids(req.ids)?;
        self.mutate(|data| {
            if ids
                .iter()
                .any(|id| !data.channels.iter().any(|channel| channel.id == *id))
            {
                return Err(ManagementError::NotFound);
            }
            if req.status == STATUS_ENABLED {
                for channel in data
                    .channels
                    .iter()
                    .filter(|channel| ids.contains(&channel.id))
                {
                    channel_model_mappings(channel)?;
                }
            }
            let mut updated = 0usize;
            for channel in &mut data.channels {
                if ids.contains(&channel.id) && channel_status_update_needed(channel, req.status) {
                    apply_channel_status_update(channel, req.status, "manual operation");
                    updated = updated.saturating_add(1);
                }
            }
            Ok(updated)
        })
    }
}

impl Service<PatchChannelProbeMetricsRequest> for MemoryManagementStore {
    type Response = ChannelRecord;
    type Error = ManagementError;

    async fn call(
        &self,
        req: PatchChannelProbeMetricsRequest,
    ) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let channel = data
                .channels
                .iter_mut()
                .find(|channel| channel.id == req.id)
                .ok_or(ManagementError::NotFound)?;
            channel.test_time = req.test_time;
            channel.response_time = req.response_time;
            Ok(channel.clone().masked())
        })
    }
}

impl Service<PatchChannelBalanceRequest> for MemoryManagementStore {
    type Response = ChannelRecord;
    type Error = ManagementError;

    async fn call(&self, req: PatchChannelBalanceRequest) -> Result<Self::Response, Self::Error> {
        if !req.balance.is_finite() {
            return Err(ManagementError::InvalidRequest("balance must be finite"));
        }
        self.mutate(|data| {
            let channel = data
                .channels
                .iter_mut()
                .find(|channel| channel.id == req.id)
                .ok_or(ManagementError::NotFound)?;
            if channel.key != req.expected_key {
                return Err(ManagementError::StaleChannelUpdate(req.id));
            }
            channel.balance = req.balance;
            channel.balance_updated_time = req.balance_updated_time;
            if req.auto_disable_if_empty
                && req.balance <= 0.0
                && channel.status == STATUS_ENABLED
                && channel.auto_ban.unwrap_or(1) != 0
            {
                apply_channel_status_update(channel, STATUS_AUTO_DISABLED, "balance exhausted");
            }
            Ok(channel.clone().masked())
        })
    }
}

impl Service<PatchChannelModelStateRequest> for MemoryManagementStore {
    type Response = ChannelRecord;
    type Error = ManagementError;

    async fn call(
        &self,
        req: PatchChannelModelStateRequest,
    ) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let channel = data
                .channels
                .iter_mut()
                .find(|channel| channel.id == req.id)
                .ok_or(ManagementError::NotFound)?;
            if channel.models != req.expected_models {
                return Err(ManagementError::StaleChannelUpdate(req.id));
            }
            let mut updated = channel.clone();
            updated.models = req.models;
            updated.setting =
                merge_channel_json(channel.setting.as_deref(), req.setting_patch.as_deref());
            channel_model_mappings(&updated)?;
            *channel = updated.clone();
            Ok(updated.masked())
        })
    }
}

impl Service<RotateChannelCredentialRequest> for MemoryManagementStore {
    type Response = ChannelRecord;
    type Error = ManagementError;

    async fn call(
        &self,
        req: RotateChannelCredentialRequest,
    ) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let channel = data
                .channels
                .iter_mut()
                .find(|channel| channel.id == req.id)
                .ok_or(ManagementError::NotFound)?;
            if channel.key != req.expected_key {
                return Err(ManagementError::StaleChannelUpdate(req.id));
            }
            channel.key = req.new_key;
            channel.setting =
                merge_channel_json(channel.setting.as_deref(), req.setting_patch.as_deref());
            Ok(channel.clone().masked())
        })
    }
}

impl Service<AutoDisableChannelRequest> for MemoryManagementStore {
    type Response = AutoDisableChannelResult;
    type Error = ManagementError;

    async fn call(&self, req: AutoDisableChannelRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let channel = data
                .channels
                .iter_mut()
                .find(|channel| channel.id == req.id)
                .ok_or(ManagementError::NotFound)?;
            if channel.auto_ban.unwrap_or(1) == 0 {
                return Ok(AutoDisableChannelResult::default());
            }
            if let Some(expected) = req.api_key_fingerprint.as_deref() {
                let Some(key) = channel_key_for_feedback(channel, req.api_key_index) else {
                    return Ok(AutoDisableChannelResult::default());
                };
                if credential_fingerprint(&key) != expected {
                    return Ok(AutoDisableChannelResult::default());
                }
            }
            Ok(auto_disable_channel_in_place(
                channel,
                req.reason.as_str(),
                req.api_key_index,
                req.created_at_unix_ms,
            ))
        })
    }
}

fn channel_key_for_feedback(channel: &ChannelRecord, index: Option<usize>) -> Option<String> {
    let index = index.unwrap_or(0);
    if channel.channel_type == CHANNEL_TYPE_CODEX
        && index == 0
        && let Some(credential) = parse_codex_oauth_runtime_credential(channel.key.trim())
    {
        return credential.access_token;
    }
    let key = channel.key.lines().nth(index)?.trim();
    if key.is_empty() {
        return None;
    }
    if channel.channel_type == CHANNEL_TYPE_CODEX
        && let Some(credential) = parse_codex_oauth_runtime_credential(key)
    {
        return credential.access_token;
    }
    Some(key.to_string())
}

fn credential_fingerprint(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    format!("sha256:{digest:x}")
}

/// Single-channel auto-ban used by control-api feedback (in-place, no full record replace).
pub fn auto_disable_channel_in_place(
    channel: &mut ChannelRecord,
    reason: &str,
    api_key_index: Option<usize>,
    created_at_unix_ms: i64,
) -> AutoDisableChannelResult {
    let key_indexes = channel
        .key
        .lines()
        .enumerate()
        .filter_map(|(index, key)| (!key.trim().is_empty()).then_some(index))
        .collect::<Vec<_>>();
    // multi-key: disable one key when index is in range
    if let Some(key_index) = api_key_index {
        if !key_indexes.contains(&key_index) {
            // The gateway may still be reporting against an older snapshot
            // after credentials were rotated. Never turn an out-of-range key
            // report into a whole-channel disable.
            return AutoDisableChannelResult::default();
        }
        if key_indexes.len() > 1 {
            let mut setting = channel
                .setting
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .unwrap_or_else(|| serde_json::json!({}));
            if !setting.is_object() {
                setting = serde_json::json!({});
            }
            let key = key_index.to_string();
            let already = setting
                .pointer(&format!("/multi_key_status_list/{key}"))
                .and_then(|v| v.as_i64())
                == Some(STATUS_AUTO_DISABLED as i64);

            {
                let object = setting.as_object_mut().expect("object");
                let status_map = object
                    .entry("multi_key_status_list")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(map) = status_map.as_object_mut() {
                    map.insert(key.clone(), serde_json::json!(STATUS_AUTO_DISABLED));
                }
            }
            {
                let object = setting.as_object_mut().expect("object");
                let map = object
                    .entry("multi_key_disabled_reason")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(map) = map.as_object_mut() {
                    map.insert(key.clone(), serde_json::Value::String(reason.to_string()));
                }
            }
            {
                let object = setting.as_object_mut().expect("object");
                let map = object
                    .entry("multi_key_disabled_time")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(map) = map.as_object_mut() {
                    map.insert(key, serde_json::json!(created_at_unix_ms / 1000));
                }
            }

            let all_disabled = key_indexes.iter().all(|idx| {
                setting
                    .pointer(&format!("/multi_key_status_list/{idx}"))
                    .and_then(|v| v.as_i64())
                    == Some(STATUS_AUTO_DISABLED as i64)
            });
            let channel_was = channel.status == STATUS_AUTO_DISABLED;
            if all_disabled {
                channel.status = STATUS_AUTO_DISABLED;
                if let Some(object) = setting.as_object_mut() {
                    object.insert(
                        "status_reason".into(),
                        serde_json::Value::String(format!("All keys are disabled: {reason}")),
                    );
                    object.insert(
                        "status_time".into(),
                        serde_json::json!(created_at_unix_ms / 1000),
                    );
                }
            }
            channel.setting = serde_json::to_string(&setting).ok();
            return AutoDisableChannelResult {
                changed:          !already || (all_disabled && !channel_was),
                channel_disabled: all_disabled && !channel_was,
                key_disabled:     !already,
            };
        }
    }

    if channel.status == STATUS_AUTO_DISABLED {
        return AutoDisableChannelResult::default();
    }
    channel.status = STATUS_AUTO_DISABLED;
    let mut setting = channel
        .setting
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !setting.is_object() {
        setting = serde_json::json!({});
    }
    if let Some(object) = setting.as_object_mut() {
        object.insert(
            "status_reason".into(),
            serde_json::Value::String(reason.to_string()),
        );
        object.insert(
            "status_time".into(),
            serde_json::json!(created_at_unix_ms / 1000),
        );
    }
    channel.setting = serde_json::to_string(&setting).ok();
    AutoDisableChannelResult {
        changed:          true,
        channel_disabled: true,
        key_disabled:     false,
    }
}

fn apply_channel_status_update(channel: &mut ChannelRecord, status: i32, reason: &str) {
    channel.status = status;
    let mut setting = channel
        .setting
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !setting.is_object() {
        setting = serde_json::json!({});
    }
    let object = setting.as_object_mut().expect("setting object");
    if status == STATUS_ENABLED {
        // Manual enable: clear auto-ban bookkeeping (new-api EnableChannel path).
        object.remove("status_reason");
        object.remove("status_time");
        if let Some(map) = object.get_mut("multi_key_status_list") {
            if let Some(obj) = map.as_object_mut() {
                obj.clear();
            } else {
                object.insert("multi_key_status_list".into(), serde_json::json!({}));
            }
        }
        if let Some(map) = object.get_mut("multi_key_disabled_reason")
            && let Some(obj) = map.as_object_mut()
        {
            obj.clear();
        }
        if let Some(map) = object.get_mut("multi_key_disabled_time")
            && let Some(obj) = map.as_object_mut()
        {
            obj.clear();
        }
    } else {
        object.insert(
            "status_reason".into(),
            serde_json::Value::String(reason.to_string()),
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        object.insert("status_time".into(), serde_json::json!(now));
    }
    channel.setting = serde_json::to_string(&setting).ok();
}

fn channel_status_update_needed(channel: &ChannelRecord, status: i32) -> bool {
    if channel.status != status {
        return true;
    }
    if status != STATUS_ENABLED {
        return false;
    }
    let Some(setting) = channel
        .setting
        .as_deref()
        .and_then(|setting| serde_json::from_str::<JsonValue>(setting).ok())
    else {
        return false;
    };
    setting.get("status_reason").is_some()
        || setting
            .get("multi_key_status_list")
            .and_then(JsonValue::as_object)
            .is_some_and(|statuses| !statuses.is_empty())
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
            if ids.contains(&0)
                || ids
                    .iter()
                    .any(|id| !data.channels.iter().any(|channel| channel.id == *id))
            {
                return Err(ManagementError::NotFound);
            }
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
        if let Some(status) = req.patch.status {
            validate_manual_channel_status(status)?;
        }
        self.mutate(|data| {
            let mut replacements = Vec::new();
            for (idx, channel) in data.channels.iter().enumerate() {
                if channel.tag.as_deref() != Some(tag) {
                    continue;
                }
                let mut updated = channel.clone();
                apply_channel_tag_patch(&mut updated, &req.patch);
                if req.patch.status == Some(STATUS_ENABLED)
                    || req.patch.model_mapping.is_some()
                    || req.patch.models.is_some()
                {
                    channel_model_mappings(&updated)?;
                }
                replacements.push((idx, updated));
            }
            let updated = replacements.len();
            for (idx, channel) in replacements {
                data.channels[idx] = channel;
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
    let mut header_override = parse_channel_header_override(channel.header_override.as_deref());
    if channel.channel_type == CHANNEL_TYPE_CODEX
        && let Some(account_id) = channel_codex_runtime_account_id(channel)
    {
        insert_default_header(&mut header_override, "chatgpt-account-id", &account_id);
    }
    let proxy = channel_setting_proxy(channel);
    let proxy_required =
        channel.proxy_id.is_some() || proxy.is_some() || channel_setting_requires_proxy(channel);
    Ok(Some(ChannelConfig {
        id: channel_snapshot_id(channel),
        management_id: Some(channel.id),
        provider,
        base_url: channel
            .base_url
            .clone()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| channel_default_base_url(channel).to_string()),
        api_key,
        api_keys,
        api_key_indexes,
        api_key_env: None,
        enabled: channel.status == STATUS_ENABLED,
        weight: channel.weight.unwrap_or(1),
        models: channel.model_list(),
        groups: channel.group_list(),
        proxy,
        proxy_required,
        header_override,
        upstream_endpoint_type: channel_upstream_endpoint_type(channel),
    }))
}

/// Parse new-api style channel `header_override` JSON into explicit header→value map.
/// Non-string values and passthrough rule keys (`*`, `re:…`, `regex:…`) are skipped.
fn parse_channel_header_override(raw: Option<&str>) -> std::collections::BTreeMap<String, String> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return std::collections::BTreeMap::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return std::collections::BTreeMap::new();
    };
    let Some(object) = value.as_object() else {
        return std::collections::BTreeMap::new();
    };
    let mut out = std::collections::BTreeMap::new();
    for (key, entry) in object {
        let key = key.trim();
        if key.is_empty() || is_header_passthrough_rule_key(key) {
            continue;
        }
        let Some(text) = entry.as_str() else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        out.insert(key.to_string(), text.to_string());
    }
    out
}

fn is_header_passthrough_rule_key(key: &str) -> bool {
    let key = key.trim();
    if key == "*" {
        return true;
    }
    let lower = key.to_ascii_lowercase();
    lower.starts_with("re:") || lower.starts_with("regex:")
}

fn insert_default_header(
    headers: &mut std::collections::BTreeMap<String, String>,
    name: &str,
    value: &str,
) {
    if headers
        .keys()
        .any(|existing| existing.eq_ignore_ascii_case(name))
    {
        return;
    }
    headers.insert(name.to_string(), value.to_string());
}

/// Channel setting key aligned with new-api force path selection for OpenAI text APIs.
/// Values: auto | openai | openai-response | openai-response-compact | xai-response | codex
fn channel_upstream_endpoint_type(channel: &ChannelRecord) -> String {
    let raw = channel.setting.as_deref().unwrap_or("").trim();
    if raw.is_empty() {
        return codex_default_endpoint_type(channel);
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return codex_default_endpoint_type(channel);
    };
    v.get("upstream_endpoint_type")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "auto")
        .map(str::to_string)
        .unwrap_or_else(|| codex_default_endpoint_type(channel))
}

fn codex_default_endpoint_type(channel: &ChannelRecord) -> String {
    if channel_has_codex_runtime_credential(channel) {
        "codex".to_string()
    } else {
        String::new()
    }
}

fn channel_has_codex_runtime_credential(channel: &ChannelRecord) -> bool {
    channel.channel_type == CHANNEL_TYPE_CODEX
        && parse_codex_oauth_runtime_credential(channel.key.trim()).is_some()
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

fn channel_setting_requires_proxy(channel: &ChannelRecord) -> bool {
    let raw = channel.setting.as_deref().unwrap_or("").trim();
    if raw.is_empty() {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            value
                .get("proxy_required")
                .and_then(|value| value.as_bool())
        })
        .unwrap_or(false)
}

fn channel_model_mappings(channel: &ChannelRecord) -> Result<Vec<ModelMapping>, ManagementError> {
    let models = channel.model_list();
    let channel_id = channel_snapshot_id(channel);
    let mut mappings = models
        .into_iter()
        .map(|model| {
            (model.clone(), ModelMapping {
                requested_model: model.clone(),
                channel_id:      channel_id.clone(),
                upstream_model:  model,
            })
        })
        .collect::<BTreeMap<_, _>>();
    let Some(mapping) = channel
        .model_mapping
        .as_deref()
        .map(str::trim)
        .filter(|mapping| !mapping.is_empty())
    else {
        return Ok(mappings.into_values().collect());
    };

    let parsed: BTreeMap<String, String> =
        serde_json::from_str(mapping).map_err(|err| ManagementError::InvalidModelMapping {
            channel_id: channel.id,
            message:    err.to_string(),
        })?;
    for (requested_model, upstream_model) in parsed {
        let requested_model = requested_model.trim().to_string();
        let upstream_model = upstream_model.trim().to_string();
        if requested_model.is_empty() || upstream_model.is_empty() {
            return Err(ManagementError::InvalidModelMapping {
                channel_id: channel.id,
                message:    "model names must not be empty".to_string(),
            });
        }
        mappings.insert(requested_model.clone(), ModelMapping {
            requested_model,
            channel_id: channel_id.clone(),
            upstream_model,
        });
    }
    Ok(mappings.into_values().collect())
}

fn apply_channel_tag_patch(channel: &mut ChannelRecord, patch: &ChannelTagPatch) {
    if let Some(status) = patch.status {
        apply_channel_status_update(channel, status, "manual tag operation");
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
    let key = channel.key.trim();
    let codex_json_document = channel.channel_type == CHANNEL_TYPE_CODEX
        && (key.starts_with('{') || key.starts_with('['));
    if channel.channel_type == CHANNEL_TYPE_CODEX
        && let Some(credential) = parse_codex_oauth_runtime_credential(key)
    {
        if status
            .get(&0)
            .is_some_and(|status| *status != STATUS_ENABLED)
        {
            return Vec::new();
        }
        return credential
            .access_token
            .map(|access_token| vec![(0, access_token)])
            .unwrap_or_default();
    }
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
            if channel.channel_type == CHANNEL_TYPE_CODEX {
                if let Some(credential) = parse_codex_oauth_runtime_credential(key) {
                    return credential
                        .access_token
                        .map(|access_token| (idx, access_token));
                }
                if codex_json_document {
                    return None;
                }
            }
            Some((idx, key.to_string()))
        })
        .collect()
}

#[derive(Debug)]
struct CodexOAuthRuntimeCredential {
    access_token: Option<String>,
    account_id:   Option<String>,
}

fn channel_codex_runtime_account_id(channel: &ChannelRecord) -> Option<String> {
    if channel.channel_type != CHANNEL_TYPE_CODEX {
        return None;
    }
    let status = multi_key_status_list(channel.setting.as_deref());
    if let Some(credential) = parse_codex_oauth_runtime_credential(channel.key.trim()) {
        if status
            .get(&0)
            .is_some_and(|status| *status != STATUS_ENABLED)
        {
            return None;
        }
        return credential.account_id;
    }
    channel.key.lines().enumerate().find_map(|(idx, key)| {
        if status
            .get(&idx)
            .is_some_and(|status| *status != STATUS_ENABLED)
        {
            return None;
        }
        parse_codex_oauth_runtime_credential(key)?.account_id
    })
}

fn parse_codex_oauth_runtime_credential(raw: &str) -> Option<CodexOAuthRuntimeCredential> {
    let value = serde_json::from_str::<JsonValue>(raw).ok()?;
    let object = value.as_object()?;
    let tokens = object.get("tokens").and_then(JsonValue::as_object);
    let account = object.get("account").and_then(JsonValue::as_object);
    let access_token = tokens
        .and_then(|tokens| json_string_field(tokens, &["access_token", "accessToken"]))
        .or_else(|| json_string_field(object, &["access_token", "accessToken"]));
    let refresh_token = tokens
        .and_then(|tokens| json_string_field(tokens, &["refresh_token", "refreshToken"]))
        .or_else(|| json_string_field(object, &["refresh_token", "refreshToken"]));
    let id_token = tokens
        .and_then(|tokens| json_string_field(tokens, &["id_token", "idToken"]))
        .or_else(|| json_string_field(object, &["id_token", "idToken"]));
    let account_id = json_string_field(object, &[
        "chatgpt_account_id",
        "chatgptAccountId",
        "account_id",
        "accountId",
    ])
    .or_else(|| {
        account.and_then(|account| {
            json_string_field(account, &[
                "chatgpt_account_id",
                "account_id",
                "accountId",
                "id",
            ])
        })
    });
    let is_codex_type =
        json_string_field(object, &["type"]).is_some_and(|kind| kind.eq_ignore_ascii_case("codex"));
    if access_token.is_none()
        && refresh_token.is_none()
        && id_token.is_none()
        && account_id.is_none()
        && !is_codex_type
    {
        return None;
    }
    Some(CodexOAuthRuntimeCredential {
        access_token,
        account_id,
    })
}

fn json_string_field(
    object: &serde_json::Map<String, JsonValue>,
    names: &[&str],
) -> Option<String> {
    names.iter().find_map(|name| {
        object
            .get(*name)
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
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

fn channel_default_base_url(channel: &ChannelRecord) -> &'static str {
    if channel_has_codex_runtime_credential(channel) {
        "https://chatgpt.com/backend-api/codex"
    } else {
        default_base_url(channel.channel_type)
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
        CHANNEL_TYPE_SORA => "https://api.openai.com",
        CHANNEL_TYPE_CODEX => "https://api.openai.com",
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
    // Root may manage anyone (including self). Admins may only manage strictly lower roles.
    actor_role == ROLE_ROOT_USER || actor_role > target_role
}

/// Mirrors new-api ChannelSortOptions.
fn sort_channels(channels: &mut [ChannelRecord], sort_by: &str, sort_order: &str, id_sort: bool) {
    let sort_by = sort_by.trim().to_ascii_lowercase();
    let ascending = sort_order.trim().eq_ignore_ascii_case("asc");
    let column = match sort_by.as_str() {
        "id" | "name" | "priority" | "balance" | "response_time" | "test_time" => sort_by,
        _ if id_sort => "id".to_string(),
        _ => "priority".to_string(),
    };

    channels.sort_by(|a, b| {
        let ord = match column.as_str() {
            "name" => a.name.cmp(&b.name),
            "priority" => a.priority.unwrap_or(0).cmp(&b.priority.unwrap_or(0)),
            "balance" => a
                .balance
                .partial_cmp(&b.balance)
                .unwrap_or(std::cmp::Ordering::Equal),
            "response_time" => a.response_time.cmp(&b.response_time),
            "test_time" => a.test_time.cmp(&b.test_time),
            _ => a.id.cmp(&b.id),
        };
        if ascending { ord } else { ord.reverse() }
    });
}

/// new-api tag_mode: paginate distinct non-empty tags, return all channels under those tags.
/// `total` is the number of tags (not channels).
fn page_channels_by_tag(
    channels: Vec<ChannelRecord>,
    page_req: PageRequest,
) -> PageResult<ChannelRecord> {
    let mut tags = channels
        .iter()
        .filter_map(|channel| {
            channel
                .tag
                .as_deref()
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    tags.sort();
    tags.dedup();
    let total = tags.len();
    let start = page_req.offset();
    let limit = page_req.limit();
    let page_tags = tags
        .into_iter()
        .skip(start)
        .take(limit)
        .collect::<std::collections::BTreeSet<_>>();
    let items = channels
        .into_iter()
        .filter(|channel| {
            channel
                .tag
                .as_deref()
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .is_some_and(|tag| page_tags.contains(tag))
        })
        .map(ChannelRecord::masked)
        .collect::<Vec<_>>();
    PageResult::new(items, total, page_req)
}

/// Mirrors new-api `NormalizeChannelGroupFilter`.
fn normalize_channel_group_filter(group: &str) -> Option<String> {
    let group = group.trim();
    if group.is_empty() || group.eq_ignore_ascii_case("all") || group.eq_ignore_ascii_case("null") {
        None
    } else {
        Some(group.to_string())
    }
}

/// Mirrors new-api channel group membership: comma-separated list exact-token match.
fn channel_belongs_to_group(channel: &ChannelRecord, group: &str) -> bool {
    channel.group_list().iter().any(|item| item == group)
}

fn channel_matches_status_filter(channel: &ChannelRecord, status: Option<i32>) -> bool {
    match status {
        Some(status) if status == STATUS_ENABLED => channel.status == STATUS_ENABLED,
        // new-api uses 0 for "disabled" filter: any non-enabled status.
        Some(0) => channel.status != STATUS_ENABLED,
        Some(status) => channel.status == status,
        None => true,
    }
}

fn channel_matches_list_filters(
    channel: &ChannelRecord,
    group: Option<&str>,
    status: Option<i32>,
    channel_type: Option<i32>,
    keyword: Option<&str>,
    model: Option<&str>,
) -> bool {
    if let Some(group) = group
        && !channel_belongs_to_group(channel, group)
    {
        return false;
    }
    if !channel_matches_status_filter(channel, status) {
        return false;
    }
    if let Some(channel_type) = channel_type
        && channel.channel_type != channel_type
    {
        return false;
    }
    if let Some(keyword) = keyword {
        let keyword = keyword.trim();
        if !keyword.is_empty() {
            // Mirrors new-api SearchChannels:
            // id exact OR name LIKE OR key exact OR base_url LIKE, AND models LIKE (separate).
            let name_ok = channel.name.to_ascii_lowercase().contains(keyword);
            let id_ok = channel.id.to_string() == keyword;
            let key_ok = channel.key == keyword;
            let base_ok = channel
                .base_url
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase()
                .contains(keyword);
            if !(name_ok || id_ok || key_ok || base_ok) {
                return false;
            }
        }
    }
    if let Some(model) = model {
        let model = model.trim();
        if !model.is_empty() && !channel.models.to_ascii_lowercase().contains(model) {
            return false;
        }
    }
    true
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

fn next_channel_id(channels: &[ChannelRecord]) -> u64 {
    let mut candidate = next_id(channels.iter().map(|channel| channel.id));
    while candidate > 0
        && channels
            .iter()
            .any(|channel| channel_snapshot_id(channel) == candidate.to_string())
    {
        candidate = candidate.saturating_add(1);
    }
    candidate
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

#[cfg(test)]
fn channel_matches_usage_event(channel: &ChannelRecord, event_channel_id: &str) -> bool {
    match parse_usage_entity_id(event_channel_id) {
        Some(id) => channel.id == id,
        None => channel
            .snapshot_id
            .as_deref()
            .is_some_and(|snapshot_id| snapshot_id == event_channel_id),
    }
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

    fn usage_event(request_id: &str, token_id: &str, channel_id: &str) -> UsageEvent {
        UsageEvent {
            request_id:            request_id.to_string(),
            user_id:               "1".to_string(),
            token_id:              token_id.to_string(),
            channel_id:            channel_id.to_string(),
            group:                 "default".to_string(),
            model:                 "gpt-4".to_string(),
            upstream_model:        "gpt-4".to_string(),
            prompt_tokens:         Some(1),
            completion_tokens:     None,
            total_tokens:          Some(1),
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
            created_at_unix_ms:    1,
        }
    }

    #[test]
    fn usage_identity_indexes_rebuild_after_mutation_and_replace() {
        block_on(async {
            let mut root = user(1, "root", ROLE_ROOT_USER);
            root.quota = 1_000;
            let mut first_token = token(1, 1, "first-key");
            first_token.snapshot_id = Some("first-token".to_string());
            let mut kept_token = token(2, 1, "kept-key");
            kept_token.snapshot_id = Some("kept-token".to_string());
            let mut first_channel = sample_channel(1, "first", "default");
            first_channel.snapshot_id = Some("first-route".to_string());
            let mut kept_channel = sample_channel(2, "kept", "default");
            kept_channel.snapshot_id = Some("kept-route".to_string());
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                vec![root],
                vec![first_token, kept_token],
                vec![first_channel, kept_channel],
                Vec::new(),
            ));

            store
                .call(DeleteTokenRequest {
                    id:      1,
                    user_id: None,
                })
                .await
                .expect("delete first token");
            store
                .call(DeleteChannelRequest { id: 1 })
                .await
                .expect("delete first channel");
            let shifted = store
                .call(SettleUsageRequest {
                    events:  vec![usage_event("shifted", "kept-token", "kept-route")],
                    pricing: UsagePricing::default(),
                })
                .await
                .expect("settle through shifted identity indexes");
            assert_eq!(shifted.tokens[0].id, 2);
            assert_eq!(shifted.channels[0].id, 2);

            let mut replacement = store.current_data().expect("read replacement data");
            replacement.tokens[0].snapshot_id = Some("replacement-token".to_string());
            replacement.channels[0].snapshot_id = Some("replacement-route".to_string());
            store
                .replace_data(replacement)
                .expect("replace management data");
            let replaced = store
                .call(SettleUsageRequest {
                    events:  vec![usage_event(
                        "replaced",
                        "replacement-token",
                        "replacement-route",
                    )],
                    pricing: UsagePricing::default(),
                })
                .await
                .expect("settle through replacement identity indexes");
            assert_eq!(replaced.tokens[0].id, 2);
            assert_eq!(replaced.channels[0].id, 2);
        });
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
                    id:                     "openai-main".to_string(),
                    management_id:          None,
                    provider:               Provider::OpenAi,
                    base_url:               "https://example.com".to_string(),
                    api_key:                "upstream".to_string(),
                    api_keys:               vec!["upstream".to_string()],
                    api_key_indexes:        Vec::new(),
                    api_key_env:            None,
                    enabled:                true,
                    weight:                 1,
                    models:                 vec!["gpt-4o".to_string()],
                    groups:                 Vec::new(),
                    proxy:                  None,
                    proxy_required:         false,
                    header_override:        Default::default(),
                    upstream_endpoint_type: String::new(),
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
            assert_eq!(
                settlement
                    .users
                    .iter()
                    .map(|state| state.id)
                    .collect::<Vec<_>>(),
                vec![1]
            );
            assert_eq!(
                settlement
                    .tokens
                    .iter()
                    .map(|state| state.id)
                    .collect::<Vec<_>>(),
                vec![1]
            );
            assert_eq!(
                settlement
                    .channels
                    .iter()
                    .map(|state| state.id)
                    .collect::<Vec<_>>(),
                vec![1]
            );
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
            assert!(settlement.users.is_empty());
            assert_eq!(
                settlement
                    .tokens
                    .iter()
                    .map(|state| state.id)
                    .collect::<Vec<_>>(),
                vec![1]
            );
            assert!(settlement.channels.is_empty());
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
                    id:                     "openai-main".to_string(),
                    management_id:          None,
                    provider:               Provider::OpenAi,
                    base_url:               "https://example.com".to_string(),
                    api_key:                "upstream".to_string(),
                    api_keys:               vec!["upstream".to_string()],
                    api_key_indexes:        Vec::new(),
                    api_key_env:            None,
                    enabled:                true,
                    weight:                 1,
                    models:                 vec!["gpt-4o".to_string()],
                    groups:                 Vec::new(),
                    proxy:                  None,
                    proxy_required:         false,
                    header_override:        Default::default(),
                    upstream_endpoint_type: String::new(),
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
                id:                     "1".to_string(),
                management_id:          None,
                provider:               Provider::OpenAi,
                base_url:               "https://example.com".to_string(),
                api_key:                "key".to_string(),
                api_keys:               vec!["key".to_string(), "key-b".to_string()],
                api_key_indexes:        Vec::new(),
                api_key_env:            None,
                enabled:                true,
                weight:                 1,
                models:                 vec!["upstream".to_string()],
                groups:                 Vec::new(),
                proxy:                  None,
                proxy_required:         false,
                header_override:        Default::default(),
                upstream_endpoint_type: String::new(),
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
    fn preserves_unavailable_proxy_requirement_across_management_roundtrip() {
        let original = GatewaySnapshot {
            version:          9,
            tokens:           Vec::new(),
            channels:         vec![ChannelConfig {
                id:                     "proxy-required".to_string(),
                management_id:          None,
                provider:               Provider::OpenAi,
                base_url:               "https://example.com".to_string(),
                api_key:                "key".to_string(),
                api_keys:               vec!["key".to_string()],
                api_key_indexes:        Vec::new(),
                api_key_env:            None,
                enabled:                true,
                weight:                 1,
                models:                 vec!["upstream".to_string()],
                groups:                 Vec::new(),
                proxy:                  None,
                proxy_required:         true,
                header_override:        Default::default(),
                upstream_endpoint_type: String::new(),
            }],
            model_mappings:   Vec::new(),
            channel_affinity: Default::default(),
            group_routing:    Default::default(),
        };

        let rebuilt = ManagementData::from_snapshot(original)
            .build_snapshot()
            .expect("snapshot should rebuild");

        assert!(rebuilt.channels[0].enabled);
        assert!(rebuilt.channels[0].proxy_required);
        assert!(rebuilt.channels[0].proxy.is_none());
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
                id:                     "openai-main".to_string(),
                management_id:          None,
                provider:               Provider::OpenAi,
                base_url:               "https://example.com".to_string(),
                api_key:                "key".to_string(),
                api_keys:               vec!["key".to_string(), "key-b".to_string()],
                api_key_indexes:        Vec::new(),
                api_key_env:            None,
                enabled:                true,
                weight:                 1,
                models:                 vec!["deepseek-v4-pro".to_string()],
                groups:                 Vec::new(),
                proxy:                  None,
                proxy_required:         false,
                header_override:        Default::default(),
                upstream_endpoint_type: String::new(),
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

    fn sample_channel(id: u64, name: &str, group: &str) -> ChannelRecord {
        ChannelRecord {
            id,
            snapshot_id: None,
            channel_type: CHANNEL_TYPE_OPENAI,
            key: format!("key-{id}"),
            status: STATUS_ENABLED,
            name: name.to_string(),
            weight: Some(1),
            created_time: 0,
            test_time: 0,
            response_time: 0,
            base_url: Some("https://example.com".to_string()),
            balance: 0.0,
            balance_updated_time: 0,
            models: "gpt-4".to_string(),
            group: group.to_string(),
            used_quota: 0,
            model_mapping: None,
            priority: Some(0),
            auto_ban: Some(1),
            tag: None,
            setting: None,
            param_override: None,
            header_override: None,
            remark: None,
            proxy_id: None,
        }
    }

    fn sample_codex_channel(key: &str) -> ChannelRecord {
        let mut channel = sample_channel(57, "codex", "default");
        channel.channel_type = CHANNEL_TYPE_CODEX;
        channel.key = key.to_string();
        channel.base_url = None;
        channel.models = "gpt-5".to_string();
        channel
    }

    fn snapshot_channel_config(id: &str, management_id: Option<u64>) -> ChannelConfig {
        ChannelConfig {
            id: id.to_string(),
            management_id,
            provider: Provider::OpenAi,
            base_url: "https://example.com".to_string(),
            api_key: format!("key-{id}"),
            api_keys: Vec::new(),
            api_key_indexes: Vec::new(),
            api_key_env: None,
            enabled: true,
            weight: 1,
            models: vec!["gpt-4".to_string()],
            groups: vec!["default".to_string()],
            proxy: None,
            proxy_required: false,
            header_override: Default::default(),
            upstream_endpoint_type: String::new(),
        }
    }

    #[test]
    fn snapshot_import_allocates_unique_positive_management_and_route_ids() {
        let snapshot = GatewaySnapshot {
            version:          1,
            tokens:           Vec::new(),
            channels:         vec![
                snapshot_channel_config("foo", None),
                snapshot_channel_config("1", None),
                snapshot_channel_config("0", None),
                snapshot_channel_config("foo", None),
                snapshot_channel_config("explicit", Some(42)),
            ],
            model_mappings:   Vec::new(),
            channel_affinity: Default::default(),
            group_routing:    Default::default(),
        };

        let data = ManagementData::from_snapshot(snapshot);
        let management_ids = data
            .channels
            .iter()
            .map(|channel| channel.id)
            .collect::<HashSet<_>>();
        assert_eq!(management_ids.len(), data.channels.len());
        assert!(management_ids.iter().all(|id| *id > 0));
        assert_eq!(data.channels[1].id, 1);
        assert_eq!(data.channels[4].id, 42);

        let rebuilt = data.build_snapshot().expect("snapshot should build");
        let route_ids = rebuilt
            .channels
            .iter()
            .map(|channel| channel.id.clone())
            .collect::<HashSet<_>>();
        assert_eq!(route_ids.len(), rebuilt.channels.len());
        assert!(rebuilt.channels.iter().all(|channel| {
            channel
                .management_id
                .is_some_and(|id| management_ids.contains(&id))
        }));
    }

    #[test]
    fn build_snapshot_preserves_every_channel_for_the_same_model() {
        let data = ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            vec![
                sample_channel(1, "first", "default"),
                sample_channel(2, "second", "default"),
            ],
            Vec::new(),
        );

        let snapshot = data.build_snapshot().expect("snapshot should build");
        let mappings = snapshot
            .model_mappings
            .iter()
            .filter(|mapping| mapping.requested_model == "gpt-4")
            .collect::<Vec<_>>();
        assert_eq!(mappings.len(), 2);
        assert_eq!(
            mappings
                .iter()
                .map(|mapping| mapping.channel_id.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["1", "2"])
        );
    }

    #[test]
    fn partial_model_mapping_keeps_unmapped_models_as_identity() {
        let mut channel = sample_channel(1, "partial", "default");
        channel.models = "gpt-4,gpt-3.5".to_string();
        channel.model_mapping = Some(r#"{"gpt-4":"upstream-gpt-4"}"#.to_string());
        let snapshot = ManagementData::new(1, Vec::new(), Vec::new(), vec![channel], Vec::new())
            .build_snapshot()
            .expect("snapshot should build");
        let mappings = snapshot
            .model_mappings
            .iter()
            .map(|mapping| {
                (
                    mapping.requested_model.as_str(),
                    mapping.upstream_model.as_str(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(mappings.get("gpt-4"), Some(&"upstream-gpt-4"));
        assert_eq!(mappings.get("gpt-3.5"), Some(&"gpt-3.5"));
    }

    #[test]
    fn malformed_mapping_fails_only_its_channel_closed() {
        let healthy = sample_channel(1, "healthy", "default");
        let mut malformed = sample_channel(2, "malformed", "default");
        malformed.model_mapping = Some("{".to_string());
        let snapshot = ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            vec![healthy, malformed],
            Vec::new(),
        )
        .build_snapshot()
        .expect("healthy channels should still publish");

        assert_eq!(snapshot.channels.len(), 1);
        assert_eq!(snapshot.channels[0].management_id, Some(1));
        assert!(
            snapshot
                .model_mappings
                .iter()
                .all(|mapping| mapping.channel_id == "1")
        );
    }

    #[test]
    fn create_rejects_invalid_mapping_and_duplicate_route_identity() {
        block_on(async {
            let mut existing = sample_channel(1, "existing", "default");
            existing.snapshot_id = Some("legacy-route".to_string());
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                Vec::new(),
                Vec::new(),
                vec![existing],
                Vec::new(),
            ));

            let mut malformed = sample_channel(0, "malformed", "default");
            malformed.model_mapping = Some("{".to_string());
            assert!(matches!(
                store
                    .call(CreateChannelRequest { channel: malformed })
                    .await,
                Err(ManagementError::InvalidModelMapping { .. })
            ));

            let mut duplicate = sample_channel(2, "duplicate", "default");
            duplicate.snapshot_id = Some("legacy-route".to_string());
            assert!(matches!(
                store
                    .call(CreateChannelRequest { channel: duplicate })
                    .await,
                Err(ManagementError::Duplicate)
            ));
            let data = store.current_data().expect("data");
            assert_eq!(data.channels.len(), 1);
            assert_eq!(data.version, 1);
        });
    }

    #[test]
    fn batch_status_and_delete_are_atomic_and_manual_status_is_validated() {
        block_on(async {
            let store = MemoryManagementStore::new(ManagementData::new(
                7,
                Vec::new(),
                Vec::new(),
                vec![
                    sample_channel(1, "first", "default"),
                    sample_channel(2, "second", "default"),
                ],
                Vec::new(),
            ));

            assert!(matches!(
                store
                    .call(BatchUpdateChannelStatusRequest {
                        ids:    vec![1, 99],
                        status: STATUS_MANUALLY_DISABLED,
                    })
                    .await,
                Err(ManagementError::NotFound)
            ));
            let unchanged = store.current_data().expect("data");
            assert_eq!(unchanged.version, 7);
            assert!(
                unchanged
                    .channels
                    .iter()
                    .all(|channel| channel.status == STATUS_ENABLED)
            );

            assert!(matches!(
                store
                    .call(BatchUpdateChannelStatusRequest {
                        ids:    vec![1],
                        status: STATUS_AUTO_DISABLED,
                    })
                    .await,
                Err(ManagementError::InvalidRequest(_))
            ));
            let updated = store
                .call(BatchUpdateChannelStatusRequest {
                    ids:    vec![1, 1, 2],
                    status: STATUS_MANUALLY_DISABLED,
                })
                .await
                .expect("batch status");
            assert_eq!(updated, 2);
            let unchanged = store
                .call(BatchUpdateChannelStatusRequest {
                    ids:    vec![1, 2],
                    status: STATUS_MANUALLY_DISABLED,
                })
                .await
                .expect("no-op batch status");
            assert_eq!(unchanged, 0);

            assert!(matches!(
                store
                    .call(DeleteChannelsBatchRequest { ids: vec![1, 99] })
                    .await,
                Err(ManagementError::NotFound)
            ));
            assert_eq!(store.current_data().expect("data").channels.len(), 2);
            let deleted = store
                .call(DeleteChannelsBatchRequest { ids: vec![1, 2, 2] })
                .await
                .expect("batch delete");
            assert_eq!(deleted, 2);
            assert!(store.current_data().expect("data").channels.is_empty());
        });
    }

    #[test]
    fn field_scoped_network_writes_preserve_concurrent_channel_state() {
        block_on(async {
            let mut channel = sample_channel(1, "scoped", "default");
            channel.status = STATUS_MANUALLY_DISABLED;
            channel.proxy_id = Some(7);
            channel.setting = Some(
                r#"{"proxy":"http://proxy.example","status_reason":"manual","refresh_token":"old"}"#
                    .to_string(),
            );
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                Vec::new(),
                Vec::new(),
                vec![channel],
                Vec::new(),
            ));

            store
                .call(PatchChannelProbeMetricsRequest {
                    id:            1,
                    test_time:     123,
                    response_time: 45,
                })
                .await
                .expect("probe patch");
            store
                .call(RotateChannelCredentialRequest {
                    id:            1,
                    expected_key:  "key-1".to_string(),
                    new_key:       "rotated".to_string(),
                    setting_patch: Some(r#"{"refresh_token":"new"}"#.to_string()),
                })
                .await
                .expect("credential rotation");
            assert!(matches!(
                store
                    .call(PatchChannelBalanceRequest {
                        id:                    1,
                        expected_key:          "key-1".to_string(),
                        balance:               0.0,
                        balance_updated_time:  456,
                        auto_disable_if_empty: true,
                    })
                    .await,
                Err(ManagementError::StaleChannelUpdate(1))
            ));

            let stored = &store.current_data().expect("data").channels[0];
            assert_eq!(stored.status, STATUS_MANUALLY_DISABLED);
            assert_eq!(stored.proxy_id, Some(7));
            assert_eq!(stored.test_time, 123);
            assert_eq!(stored.response_time, 45);
            assert_eq!(stored.key, "rotated");
            assert_eq!(stored.balance_updated_time, 0);
            let setting: JsonValue =
                serde_json::from_str(stored.setting.as_deref().expect("setting")).expect("json");
            assert_eq!(
                setting.get("proxy").and_then(JsonValue::as_str),
                Some("http://proxy.example")
            );
            assert_eq!(
                setting.get("status_reason").and_then(JsonValue::as_str),
                Some("manual")
            );
            assert_eq!(
                setting.get("refresh_token").and_then(JsonValue::as_str),
                Some("new")
            );
        });
    }

    #[test]
    fn stale_or_out_of_range_feedback_never_disables_current_credentials() {
        block_on(async {
            let mut channel = sample_channel(1, "multi", "default");
            channel.key = "new-a\nnew-b".to_string();
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                Vec::new(),
                Vec::new(),
                vec![channel],
                Vec::new(),
            ));

            let stale = store
                .call(AutoDisableChannelRequest {
                    id:                  1,
                    reason:              "401".to_string(),
                    api_key_index:       Some(0),
                    api_key_fingerprint: Some(credential_fingerprint("old-a")),
                    created_at_unix_ms:  1,
                })
                .await
                .expect("stale feedback");
            assert_eq!(stale, AutoDisableChannelResult::default());
            let out_of_range = store
                .call(AutoDisableChannelRequest {
                    id:                  1,
                    reason:              "401".to_string(),
                    api_key_index:       Some(9),
                    api_key_fingerprint: None,
                    created_at_unix_ms:  1,
                })
                .await
                .expect("out of range feedback");
            assert_eq!(out_of_range, AutoDisableChannelResult::default());
            assert_eq!(
                store.current_data().expect("data").channels[0].status,
                STATUS_ENABLED
            );
        });
    }

    #[test]
    fn numeric_usage_identity_never_falls_through_to_snapshot_alias() {
        let mut first = sample_channel(1, "first", "default");
        first.snapshot_id = Some("route-a".to_string());
        let mut second = sample_channel(2, "second", "default");
        second.snapshot_id = Some("1".to_string());

        assert!(channel_matches_usage_event(&first, "1"));
        assert!(!channel_matches_usage_event(&second, "1"));
        assert!(channel_matches_usage_event(&second, "2"));
        assert!(channel_matches_usage_event(&first, "route-a"));
    }

    #[test]
    fn auto_disable_mutates_only_the_requested_channel() {
        block_on(async {
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                Vec::new(),
                Vec::new(),
                vec![
                    sample_channel(1, "failing-xai", "xai-private"),
                    sample_channel(2, "healthy-xai", "paid"),
                ],
                Vec::new(),
            ));

            let result = store
                .call(AutoDisableChannelRequest {
                    id:                  1,
                    reason:              "upstream status 401".to_string(),
                    api_key_index:       None,
                    api_key_fingerprint: None,
                    created_at_unix_ms:  1_700_000_000_000,
                })
                .await
                .expect("auto-disable should succeed");
            assert!(result.channel_disabled);

            let data = store.current_data().expect("data should be readable");
            let failing = data
                .channels
                .iter()
                .find(|channel| channel.id == 1)
                .unwrap();
            let healthy = data
                .channels
                .iter()
                .find(|channel| channel.id == 2)
                .unwrap();
            assert_eq!(failing.status, STATUS_AUTO_DISABLED);
            assert_eq!(healthy.status, STATUS_ENABLED);
            assert_eq!(healthy.group, "paid");
            assert_eq!(healthy.models, "gpt-4");
        });
    }

    #[test]
    fn manual_enable_clears_auto_disabled_key_state() {
        block_on(async {
            let mut channel = sample_channel(1, "multi-key", "default");
            channel.key = "key-a\nkey-b".to_string();
            channel.status = STATUS_AUTO_DISABLED;
            channel.setting = Some(
                r#"{"multi_key_status_list":{"0":3,"1":3},"multi_key_disabled_reason":{"0":"401","1":"401"},"multi_key_disabled_time":{"0":1700000000,"1":1700000000},"status_reason":"All keys are disabled","status_time":1700000000}"#
                    .to_string(),
            );
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                Vec::new(),
                Vec::new(),
                vec![channel],
                Vec::new(),
            ));

            store
                .call(ChannelStatusUpdateRequest {
                    id:     1,
                    status: STATUS_ENABLED,
                })
                .await
                .expect("manual enable should succeed");

            let data = store.current_data().expect("data should be readable");
            let channel = data.channels.first().expect("channel");
            assert_eq!(channel.status, STATUS_ENABLED);
            assert_eq!(channel_runtime_api_keys(channel).len(), 2);
            let setting: serde_json::Value =
                serde_json::from_str(channel.setting.as_deref().expect("setting")).expect("json");
            assert!(setting.get("status_reason").is_none());
            assert_eq!(
                setting
                    .get("multi_key_status_list")
                    .and_then(serde_json::Value::as_object)
                    .map(serde_json::Map::len),
                Some(0)
            );
        });
    }

    #[test]
    fn filters_channels_by_group_exact_token() {
        block_on(async {
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                Vec::new(),
                Vec::new(),
                vec![
                    sample_channel(1, "only-default", "default"),
                    sample_channel(2, "only-ttt", "ttt"),
                    sample_channel(3, "both", "ttt,default"),
                ],
                Vec::new(),
            ));

            let page = store
                .call(ListChannelsRequest {
                    page:         PageRequest {
                        page:      1,
                        page_size: 50,
                    },
                    group:        "ttt".to_string(),
                    status:       None,
                    channel_type: None,
                    sort_by:      String::new(),
                    sort_order:   String::new(),
                    id_sort:      false,
                    tag_mode:     false,
                })
                .await
                .expect("list channels");
            let ids: Vec<u64> = page.items.iter().map(|c| c.id).collect();
            assert!(ids.contains(&2), "ttt-only should match: {ids:?}");
            assert!(ids.contains(&3), "multi-group should match: {ids:?}");
            assert!(
                !ids.contains(&1),
                "default-only must be filtered out: {ids:?}"
            );
            assert_eq!(page.total, 2);
        });
    }

    #[test]
    fn empty_group_filter_returns_all_channels() {
        block_on(async {
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                Vec::new(),
                Vec::new(),
                vec![
                    sample_channel(1, "only-default", "default"),
                    sample_channel(2, "only-ttt", "ttt"),
                ],
                Vec::new(),
            ));
            let page = store
                .call(ListChannelsRequest {
                    page:         PageRequest {
                        page:      1,
                        page_size: 50,
                    },
                    group:        String::new(),
                    status:       None,
                    channel_type: None,
                    sort_by:      String::new(),
                    sort_order:   String::new(),
                    id_sort:      false,
                    tag_mode:     false,
                })
                .await
                .expect("list channels");
            assert_eq!(page.total, 2);
        });
    }

    #[test]
    fn parses_channel_header_override_map() {
        let map = parse_channel_header_override(Some(
            r#"{"User-Agent":"codex-cli/1.0","*":true,"re:^X-":true,"X-Api-Extra":"Bearer {api_key}","X-Client":"{client_header:User-Agent}"}"#,
        ));
        assert_eq!(
            map.get("User-Agent").map(String::as_str),
            Some("codex-cli/1.0")
        );
        assert_eq!(
            map.get("X-Api-Extra").map(String::as_str),
            Some("Bearer {api_key}")
        );
        assert_eq!(
            map.get("X-Client").map(String::as_str),
            Some("{client_header:User-Agent}")
        );
        assert!(!map.contains_key("*"));
        assert!(!map.keys().any(|k| k.starts_with("re:")));
    }

    #[test]
    fn build_snapshot_publishes_header_override() {
        let mut channel = sample_channel(9, "with-ua", "default");
        channel.header_override = Some(r#"{"User-Agent":"forced-ua"}"#.to_string());
        let data = ManagementData::new(1, Vec::new(), Vec::new(), vec![channel], Vec::new());
        let snapshot = data.build_snapshot().expect("snapshot");
        assert_eq!(
            snapshot.channels[0]
                .header_override
                .get("User-Agent")
                .map(String::as_str),
            Some("forced-ua")
        );
    }

    #[test]
    fn build_snapshot_normalizes_codex_oauth_runtime_config() {
        let oauth_key = r#"{
            "type": "codex",
            "access_token": "access-token",
            "refresh_token": "refresh-token-must-not-leak",
            "account_id": "account-123"
        }"#;
        let data = ManagementData::new(
            5,
            Vec::new(),
            Vec::new(),
            vec![sample_codex_channel(oauth_key)],
            Vec::new(),
        );

        let snapshot = data.build_snapshot().expect("snapshot should build");
        let codex = &snapshot.channels[0];

        assert_eq!(codex.api_key, "access-token");
        assert_eq!(codex.api_keys, ["access-token"]);
        assert_eq!(codex.api_key_indexes, [0]);
        assert_eq!(codex.base_url, "https://chatgpt.com/backend-api/codex");
        assert_eq!(codex.upstream_endpoint_type, "codex");
        assert_eq!(
            codex
                .header_override
                .get("chatgpt-account-id")
                .map(String::as_str),
            Some("account-123")
        );
        assert!(!codex.header_override.contains_key("Originator"));
        assert!(!codex.header_override.contains_key("User-Agent"));
        let published = serde_json::to_string(&snapshot).expect("snapshot should serialize");
        assert!(!published.contains("refresh-token-must-not-leak"));
    }

    #[test]
    fn build_snapshot_keeps_explicit_codex_identity_headers() {
        let mut channel = sample_codex_channel(r#"{"type":"codex","access_token":"access-token"}"#);
        channel.header_override =
            Some(r#"{"originator":"codex-tui","user-agent":"codex-tui/1.0"}"#.to_string());
        let data = ManagementData::new(5, Vec::new(), Vec::new(), vec![channel], Vec::new());

        let snapshot = data.build_snapshot().expect("snapshot should build");
        let headers = &snapshot.channels[0].header_override;

        assert_eq!(
            headers.get("originator").map(String::as_str),
            Some("codex-tui")
        );
        assert_eq!(
            headers.get("user-agent").map(String::as_str),
            Some("codex-tui/1.0")
        );
        assert_eq!(headers.len(), 2);
    }

    #[test]
    fn build_snapshot_never_publishes_refresh_only_codex_json() {
        let secret = "refresh-token-must-stay-in-control-plane";
        let oauth_key = format!(r#"{{"type":"codex","refresh_token":"{secret}"}}"#);
        let data = ManagementData::new(
            5,
            Vec::new(),
            Vec::new(),
            vec![sample_codex_channel(&oauth_key)],
            Vec::new(),
        );

        let snapshot = data.build_snapshot().expect("snapshot should build");

        assert!(snapshot.channels[0].api_key.is_empty());
        assert!(snapshot.channels[0].api_keys.is_empty());
        let published = serde_json::to_string(&snapshot).expect("snapshot should serialize");
        assert!(!published.contains(secret));
    }

    #[test]
    fn build_snapshot_preserves_plain_codex_multi_keys() {
        let data = ManagementData::new(
            6,
            Vec::new(),
            Vec::new(),
            vec![sample_codex_channel("plain-key-a\nplain-key-b")],
            Vec::new(),
        );

        let snapshot = data.build_snapshot().expect("snapshot should build");
        let codex = &snapshot.channels[0];

        assert_eq!(codex.api_key, "plain-key-a");
        assert_eq!(codex.api_keys, ["plain-key-a", "plain-key-b"]);
        assert_eq!(codex.api_key_indexes, [0, 1]);
        assert_eq!(codex.base_url, "https://api.openai.com");
        assert!(codex.upstream_endpoint_type.is_empty());
        assert!(!codex.header_override.contains_key("chatgpt-account-id"));
    }

    #[test]
    fn build_snapshot_keeps_plain_codex_api_key_openai_compatible() {
        let data = ManagementData::new(
            6,
            Vec::new(),
            Vec::new(),
            vec![sample_codex_channel("plain-api-key")],
            Vec::new(),
        );

        let snapshot = data.build_snapshot().expect("snapshot should build");
        let codex = &snapshot.channels[0];

        assert_eq!(codex.api_key, "plain-api-key");
        assert_eq!(codex.base_url, "https://api.openai.com");
        assert!(codex.upstream_endpoint_type.is_empty());
        assert!(codex.header_override.is_empty());
    }

    #[test]
    fn masked_channel_update_preserves_nested_json_secrets() {
        block_on(async {
            let mut channel = sample_channel(11, "secret-channel", "default");
            channel.setting = Some(
                r#"{"credentials":[{"refresh_token":"refresh-secret","access_token":"access-secret"}],"proxy":"socks5h://user:proxy-secret@127.0.0.1:1080","import_source":"codex"}"#.to_string(),
            );
            channel.param_override =
                Some(r#"{"auth":{"id_token":"id-secret"},"temperature":0.5}"#.to_string());
            channel.header_override = Some(
                r#"{"Authorization":"Bearer header-secret","User-Agent":"codex_cli_rs"}"#
                    .to_string(),
            );
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                Vec::new(),
                Vec::new(),
                vec![channel],
                Vec::new(),
            ));

            let mut masked = store
                .call(GetChannelRequest { id: 11 })
                .await
                .expect("channel should load masked");
            masked.name = "renamed-channel".to_string();
            store
                .call(UpdateChannelRequest { channel: masked })
                .await
                .expect("masked channel should update");

            let stored = store
                .current_data()
                .expect("data should be readable")
                .channels
                .into_iter()
                .find(|channel| channel.id == 11)
                .expect("stored channel");
            let setting: JsonValue =
                serde_json::from_str(stored.setting.as_deref().expect("stored setting"))
                    .expect("setting should remain JSON");
            let param_override: JsonValue = serde_json::from_str(
                stored
                    .param_override
                    .as_deref()
                    .expect("stored param override"),
            )
            .expect("param override should remain JSON");
            let header_override: JsonValue = serde_json::from_str(
                stored
                    .header_override
                    .as_deref()
                    .expect("stored header override"),
            )
            .expect("header override should remain JSON");

            assert_eq!(stored.name, "renamed-channel");
            assert_eq!(setting["credentials"][0]["refresh_token"], "refresh-secret");
            assert_eq!(setting["credentials"][0]["access_token"], "access-secret");
            assert_eq!(
                setting["proxy"],
                "socks5h://user:proxy-secret@127.0.0.1:1080"
            );
            assert_eq!(param_override["auth"]["id_token"], "id-secret");
            assert_eq!(header_override["Authorization"], "Bearer header-secret");
        });
    }

    #[test]
    fn merge_channel_json_preserves_malformed_existing_when_incoming_is_empty() {
        let malformed = "  {malformed-secret  ";
        assert_eq!(
            merge_channel_json(Some(malformed), None).as_deref(),
            Some(malformed)
        );
        assert_eq!(
            merge_channel_json(Some(malformed), Some("  ")).as_deref(),
            Some(malformed)
        );
        assert_eq!(
            merge_channel_json(Some(malformed), Some(r#"{"replacement":true}"#)).as_deref(),
            Some(r#"{"replacement":true}"#)
        );
    }

    #[test]
    fn masked_channel_update_roundtrips_malformed_json_without_exposing_or_losing_it() {
        block_on(async {
            let mut channel = sample_channel(12, "malformed-secret-channel", "default");
            channel.setting = Some("{setting-secret".to_string());
            channel.param_override = Some("[param-secret".to_string());
            channel.header_override = Some("{header-secret".to_string());
            let store = MemoryManagementStore::new(ManagementData::new(
                1,
                Vec::new(),
                Vec::new(),
                vec![channel],
                Vec::new(),
            ));

            let mut masked = store
                .call(GetChannelRequest { id: 12 })
                .await
                .expect("channel should load masked");
            assert!(masked.setting.is_none());
            assert!(masked.param_override.is_none());
            assert!(masked.header_override.is_none());
            masked.name = "renamed-malformed-channel".to_string();
            store
                .call(UpdateChannelRequest { channel: masked })
                .await
                .expect("masked channel should update");

            let stored = store
                .current_data()
                .expect("data should be readable")
                .channels
                .into_iter()
                .next()
                .expect("stored channel");
            assert_eq!(stored.name, "renamed-malformed-channel");
            assert_eq!(stored.setting.as_deref(), Some("{setting-secret"));
            assert_eq!(stored.param_override.as_deref(), Some("[param-secret"));
            assert_eq!(stored.header_override.as_deref(), Some("{header-secret"));
        });
    }
}
