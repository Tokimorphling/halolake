use ipnet::IpNet;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::RwLock,
    time::{Duration, Instant},
};
use thiserror::Error;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewaySnapshot {
    #[serde(default = "default_version")]
    pub version:          u64,
    #[serde(default)]
    pub tokens:           Vec<TokenConfig>,
    #[serde(default)]
    pub channels:         Vec<ChannelConfig>,
    #[serde(default)]
    pub model_mappings:   Vec<ModelMapping>,
    #[serde(default)]
    pub channel_affinity: ChannelAffinityConfig,
    #[serde(default, skip_serializing_if = "GroupRoutingConfig::is_default")]
    pub group_routing:    GroupRoutingConfig,
}

impl GatewaySnapshot {
    pub fn index(self) -> Result<IndexedSnapshot, SnapshotError> {
        IndexedSnapshot::try_from(self)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenConfig {
    #[serde(default)]
    pub id:             String,
    pub token:          String,
    pub user_id:        String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user_group:     String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token_group:    String,
    #[serde(default)]
    pub group:          String,
    #[serde(default)]
    pub enabled:        bool,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_ips:    Vec<String>,
}

impl TokenConfig {
    fn normalize_groups(&mut self) {
        self.token_group = self.token_group.trim().to_string();
        if self.group.trim().is_empty() && !self.token_group.is_empty() {
            self.group.clone_from(&self.token_group);
        }
        self.group = normalize_group(&self.group);
        self.user_group = if self.user_group.trim().is_empty() {
            self.group.clone()
        } else {
            normalize_group(&self.user_group)
        };
        if !self.token_group.is_empty() {
            self.token_group = normalize_group(&self.token_group);
            self.group.clone_from(&self.token_group);
        }
    }

    fn normalize_allowed_ips(&mut self) {
        for ip in &mut self.allowed_ips {
            *ip = ip.trim().to_string();
        }
        self.allowed_ips.retain(|ip| !ip.is_empty());
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct GroupRoutingConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_groups:                 Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub user_usable_groups:          HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub group_special_usable_groups: HashMap<String, HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_groups:                Vec<String>,
}

impl GroupRoutingConfig {
    fn is_default(&self) -> bool {
        self.auto_groups.is_empty()
            && self.user_usable_groups.is_empty()
            && self.group_special_usable_groups.is_empty()
            && self.known_groups.is_empty()
    }

    fn normalize(&mut self) {
        normalize_group_vec_preserve_order(&mut self.auto_groups);
        normalize_group_vec(&mut self.known_groups);

        let user_usable_groups = std::mem::take(&mut self.user_usable_groups);
        self.user_usable_groups = user_usable_groups
            .into_iter()
            .map(|(group, description)| (normalize_group(&group), description))
            .collect();

        let special_groups = std::mem::take(&mut self.group_special_usable_groups);
        self.group_special_usable_groups = special_groups
            .into_iter()
            .map(|(group, settings)| {
                let settings = settings
                    .into_iter()
                    .filter(|(action, _)| !action.trim().is_empty())
                    .collect();
                (normalize_group(&group), settings)
            })
            .collect();
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChannelConfig {
    pub id:              String,
    pub provider:        Provider,
    pub base_url:        String,
    #[serde(default)]
    pub api_key:         String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_keys:        Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_key_indexes: Vec<usize>,
    #[serde(default)]
    pub api_key_env:     Option<String>,
    #[serde(default)]
    pub enabled:         bool,
    #[serde(default = "default_weight")]
    pub weight:          u32,
    #[serde(default)]
    pub models:          Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups:          Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy:           Option<String>,
}

impl ChannelConfig {
    pub fn select_api_key(&self, seed: u64) -> &str {
        self.select_api_key_with_index(seed).0
    }

    pub fn select_api_key_with_index(&self, seed: u64) -> (&str, Option<usize>) {
        if self.api_keys.is_empty() {
            return (
                self.api_key.as_str(),
                (!self.api_key.is_empty()).then_some(0),
            );
        }
        let index = seed as usize % self.api_keys.len();
        let original_index = self.api_key_indexes.get(index).copied().unwrap_or(index);
        (self.api_keys[index].as_str(), Some(original_index))
    }

    fn normalize_api_keys(&mut self) {
        let api_keys = std::mem::take(&mut self.api_keys);
        let api_key_indexes = std::mem::take(&mut self.api_key_indexes);
        let mut normalized_keys = Vec::with_capacity(api_keys.len());
        let mut normalized_indexes = Vec::with_capacity(api_keys.len());
        for (idx, key) in api_keys.into_iter().enumerate() {
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            normalized_keys.push(key.to_string());
            normalized_indexes.push(api_key_indexes.get(idx).copied().unwrap_or(idx));
        }
        self.api_keys = normalized_keys;
        self.api_key_indexes = normalized_indexes;
        self.api_key = self.api_key.trim().to_string();
        if self.api_keys.is_empty() {
            if !self.api_key.is_empty() {
                self.api_keys.push(self.api_key.clone());
                self.api_key_indexes.push(0);
            }
            return;
        }
        if self.api_key.is_empty() {
            self.api_key = self.api_keys[0].clone();
        }
    }

    fn normalize_groups(&mut self) {
        if self.groups.is_empty() {
            self.groups.push("default".to_string());
            return;
        }
        for group in &mut self.groups {
            *group = normalize_group(group);
        }
        self.groups.sort();
        self.groups.dedup();
    }

    fn serves_group(&self, group: &str) -> bool {
        let group = normalize_group(group);
        self.groups.iter().any(|item| item == &group)
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    OpenAi,
    Gemini,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelMapping {
    pub requested_model: String,
    pub channel_id:      String,
    pub upstream_model:  String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChannelAffinityConfig {
    #[serde(default)]
    pub enabled:                  bool,
    #[serde(default = "default_true")]
    pub switch_on_success:        bool,
    #[serde(default)]
    pub keep_on_channel_disabled: bool,
    #[serde(default = "default_affinity_capacity")]
    pub max_entries:              usize,
    #[serde(default = "default_affinity_ttl_seconds")]
    pub default_ttl_seconds:      u64,
    #[serde(default)]
    pub rules:                    Vec<ChannelAffinityRule>,
}

impl Default for ChannelAffinityConfig {
    fn default() -> Self {
        Self {
            enabled:                  false,
            switch_on_success:        true,
            keep_on_channel_disabled: false,
            max_entries:              default_affinity_capacity(),
            default_ttl_seconds:      default_affinity_ttl_seconds(),
            rules:                    Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChannelAffinityRule {
    #[serde(default)]
    pub name:                    String,
    #[serde(default)]
    pub model_regex:             Vec<String>,
    #[serde(default)]
    pub path_regex:              Vec<String>,
    #[serde(default)]
    pub user_agent_include:      Vec<String>,
    #[serde(default)]
    pub key_sources:             Vec<ChannelAffinityKeySource>,
    #[serde(default)]
    pub value_regex:             String,
    #[serde(default)]
    pub ttl_seconds:             u64,
    #[serde(default)]
    pub param_override_template: Option<JsonValue>,
    #[serde(default)]
    pub skip_retry_on_failure:   bool,
    #[serde(default)]
    pub include_using_group:     bool,
    #[serde(default)]
    pub include_model_name:      bool,
    #[serde(default)]
    pub include_rule_name:       bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChannelAffinityKeySource {
    #[serde(rename = "type", default)]
    pub source_type: String,
    #[serde(default)]
    pub key:         String,
    #[serde(default)]
    pub path:        String,
}

#[derive(Debug, Clone)]
pub struct ChannelAffinityCandidate {
    pub cache_key:         String,
    pub ttl_seconds:       u64,
    pub cached_channel_id: Option<String>,
    pub rule_name:         String,
}

#[derive(Debug, Clone)]
struct IndexedChannelAffinity {
    config: ChannelAffinityConfig,
    rules:  Vec<IndexedChannelAffinityRule>,
}

#[derive(Debug, Clone)]
struct IndexedChannelAffinityRule {
    rule:               ChannelAffinityRule,
    model_regex:        Vec<Regex>,
    path_regex:         Vec<Regex>,
    value_regex:        Option<Regex>,
    user_agent_include: Vec<String>,
}

#[derive(Debug, Clone)]
struct ChannelAffinityCacheEntry {
    channel_id: String,
    expires_at: Instant,
}

/// Mutable channel-affinity cache, deliberately kept OUTSIDE `IndexedSnapshot`.
///
/// The snapshot is an immutable, atomically-swapped structure: a new one is
/// built and swapped in on every control-plane poll. Embedding this cache in it
/// (as an earlier version did) meant the cache was silently discarded on every
/// refresh — so any affinity TTL longer than the poll interval never took
/// effect — and put a mutable lock inside the "read-only" hot-path snapshot.
///
/// Owning it separately lets a worker keep one cache alive across snapshot
/// swaps. It is `Send + Sync` so it can also be shared behind an `Arc`.
#[derive(Debug, Default)]
pub struct ChannelAffinityCache {
    entries: RwLock<HashMap<String, ChannelAffinityCacheEntry>>,
}

impl ChannelAffinityCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self, cache_key: &str) -> Option<String> {
        let now = Instant::now();
        let mut cache = self.entries.write().ok()?;
        let entry = cache.get(cache_key)?;
        if entry.expires_at <= now {
            cache.remove(cache_key);
            return None;
        }
        Some(entry.channel_id.clone())
    }

    fn clear(&self, cache_key: &str) {
        if let Ok(mut cache) = self.entries.write() {
            cache.remove(cache_key);
        }
    }

    fn record(&self, affinity: &ChannelAffinityCandidate, channel_id: &str, max_entries: usize) {
        let ttl = Duration::from_secs(affinity.ttl_seconds.max(1));
        let mut cache = match self.entries.write() {
            Ok(cache) => cache,
            Err(_) => return,
        };
        if cache.len() >= max_entries.max(1)
            && !cache.contains_key(&affinity.cache_key)
            && let Some(expired_key) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
        {
            cache.remove(&expired_key);
        }
        cache.insert(affinity.cache_key.clone(), ChannelAffinityCacheEntry {
            channel_id: channel_id.to_string(),
            expires_at: Instant::now() + ttl,
        });
    }
}

#[derive(Debug)]
pub struct IndexedSnapshot {
    version:  u64,
    tokens:   HashMap<String, IndexedToken>,
    channels: HashMap<String, ChannelConfig>,
    mappings: HashMap<String, Vec<ModelMapping>>,
    affinity: IndexedChannelAffinity,
}

#[derive(Debug, Clone)]
pub struct IndexedToken {
    config:                   TokenConfig,
    ip_restricted:            bool,
    ip_rules:                 Vec<IpAllowRule>,
    usable_groups:            Vec<String>,
    route_groups:             Vec<String>,
    group_override_forbidden: bool,
    group_deprecated:         bool,
}

impl IndexedToken {
    fn from_config(config: TokenConfig, group_routing: &IndexedGroupRouting) -> Self {
        let ip_restricted = !config.allowed_ips.is_empty();
        let ip_rules = config
            .allowed_ips
            .iter()
            .filter_map(|rule| parse_ip_allow_rule(rule))
            .collect();
        let usable_groups = group_routing.usable_groups_for(&config.user_group);
        let has_group_override = !config.token_group.is_empty();
        let group_override_forbidden =
            has_group_override && !group_in_slice(&usable_groups, &config.group);
        let group_deprecated = has_group_override
            && config.group != "auto"
            && !group_routing.contains_known_group(&config.group);
        let route_groups = if config.group == "auto" {
            group_routing.auto_groups_for(&usable_groups)
        } else {
            vec![config.group.clone()]
        };
        Self {
            config,
            ip_restricted,
            ip_rules,
            usable_groups,
            route_groups,
            group_override_forbidden,
            group_deprecated,
        }
    }

    pub fn id(&self) -> &str {
        &self.config.id
    }

    pub fn user_id(&self) -> &str {
        &self.config.user_id
    }

    pub fn group(&self) -> &str {
        &self.config.group
    }

    pub fn user_group(&self) -> &str {
        &self.config.user_group
    }

    pub fn usable_groups(&self) -> &[String] {
        &self.usable_groups
    }

    pub fn allowed_models(&self) -> &[String] {
        &self.config.allowed_models
    }

    fn allows_ip(&self, ip: IpAddr) -> bool {
        !self.ip_restricted || self.ip_rules.iter().any(|rule| rule.contains(ip))
    }
}

#[derive(Debug, Clone)]
struct IndexedGroupRouting {
    config: GroupRoutingConfig,
}

impl IndexedGroupRouting {
    fn from_config(mut config: GroupRoutingConfig) -> Self {
        config.normalize();
        Self { config }
    }

    fn usable_groups_for(&self, user_group: &str) -> Vec<String> {
        let user_group = normalize_group(user_group);
        let mut groups = self
            .config
            .user_usable_groups
            .keys()
            .map(|group| normalize_group(group))
            .collect::<Vec<_>>();
        if let Some(settings) = self.config.group_special_usable_groups.get(&user_group) {
            for action in settings.keys() {
                let action = action.trim();
                if let Some(group) = action.strip_prefix("-:") {
                    remove_group(&mut groups, group);
                } else if let Some(group) = action.strip_prefix("+:") {
                    push_group(&mut groups, group);
                } else {
                    push_group(&mut groups, action);
                }
            }
        }
        push_group(&mut groups, &user_group);
        normalize_group_vec(&mut groups);
        groups
    }

    fn auto_groups_for(&self, usable_groups: &[String]) -> Vec<String> {
        self.config
            .auto_groups
            .iter()
            .filter(|group| group_in_slice(usable_groups, group))
            .cloned()
            .collect()
    }

    fn contains_known_group(&self, group: &str) -> bool {
        self.config.known_groups.is_empty() || group_in_slice(&self.config.known_groups, group)
    }
}

#[derive(Debug, Clone)]
enum IpAllowRule {
    Exact(IpAddr),
    Net(IpNet),
}

impl IpAllowRule {
    fn contains(&self, ip: IpAddr) -> bool {
        match self {
            Self::Exact(allowed) => *allowed == ip,
            Self::Net(net) => net.contains(&ip),
        }
    }
}

impl IndexedSnapshot {
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn authenticate(&self, bearer: &str) -> Result<AuthContext<'_>, RouteError> {
        let token = self.tokens.get(bearer).ok_or(RouteError::Unauthorized)?;
        if !token.config.enabled {
            return Err(RouteError::Unauthorized);
        }
        if token.group_override_forbidden {
            return Err(RouteError::GroupForbidden);
        }
        if token.group_deprecated {
            return Err(RouteError::GroupDeprecated);
        }
        Ok(AuthContext { token })
    }

    pub fn authorize_ip(&self, auth: &AuthContext<'_>, ip: IpAddr) -> Result<(), RouteError> {
        if auth.token.allows_ip(ip) {
            Ok(())
        } else {
            Err(RouteError::IpForbidden)
        }
    }

    pub fn route<'a>(
        &'a self,
        auth: &AuthContext<'a>,
        requested_model: &'a str,
    ) -> Result<RouteDecision<'a>, RouteError> {
        self.route_with_seed(auth, requested_model, 0)
    }

    pub fn route_with_seed<'a>(
        &'a self,
        auth: &AuthContext<'a>,
        requested_model: &'a str,
        seed: u64,
    ) -> Result<RouteDecision<'a>, RouteError> {
        // No affinity candidate, so the affinity cache is never consulted; go
        // straight to weighted routing rather than requiring a cache handle.
        if !auth.token.allowed_models().is_empty()
            && !auth
                .token
                .allowed_models()
                .iter()
                .any(|model| model == requested_model)
        {
            return Err(RouteError::ModelForbidden);
        }
        self.route_weighted(auth, requested_model, seed)
    }

    pub fn route_with_affinity_seed<'a>(
        &'a self,
        auth: &AuthContext<'a>,
        requested_model: &'a str,
        affinity: Option<&ChannelAffinityCandidate>,
        cache: &ChannelAffinityCache,
        seed: u64,
    ) -> Result<(RouteDecision<'a>, bool), RouteError> {
        if !auth.token.allowed_models().is_empty()
            && !auth
                .token
                .allowed_models()
                .iter()
                .any(|model| model == requested_model)
        {
            return Err(RouteError::ModelForbidden);
        }

        if let Some(candidate) = affinity
            && let Some(channel_id) = candidate.cached_channel_id.as_deref()
        {
            if let Some(route) = self.route_specific_channel(auth, requested_model, channel_id) {
                return Ok((route, true));
            }
            if !self.affinity.config.keep_on_channel_disabled {
                cache.clear(&candidate.cache_key);
            }
        }

        self.route_weighted(auth, requested_model, seed)
            .map(|route| (route, false))
    }

    pub fn resolve_affinity<F>(
        &self,
        requested_model: &str,
        path: &str,
        user_agent: &str,
        using_group: &str,
        body: &[u8],
        cache: &ChannelAffinityCache,
        mut header_value: F,
    ) -> Option<ChannelAffinityCandidate>
    where
        F: FnMut(&str) -> Option<String>,
    {
        if !self.affinity.config.enabled {
            return None;
        }
        for indexed in &self.affinity.rules {
            let rule = &indexed.rule;
            if indexed.model_regex.is_empty()
                || !indexed
                    .model_regex
                    .iter()
                    .any(|regex| regex.is_match(requested_model))
            {
                continue;
            }
            if !indexed.path_regex.is_empty()
                && !indexed.path_regex.iter().any(|regex| regex.is_match(path))
            {
                continue;
            }
            if !indexed.user_agent_include.is_empty() {
                let user_agent = user_agent.to_ascii_lowercase();
                if !indexed
                    .user_agent_include
                    .iter()
                    .any(|needle| user_agent.contains(needle))
                {
                    continue;
                }
            }
            let mut affinity_value = String::new();
            for source in &rule.key_sources {
                affinity_value = match source.source_type.as_str() {
                    "request_header" => header_value(&source.key).unwrap_or_default(),
                    "gjson" => json_path_string(body, &source.path).unwrap_or_default(),
                    _ => String::new(),
                };
                affinity_value = affinity_value.trim().to_string();
                if !affinity_value.is_empty() {
                    break;
                }
            }
            if affinity_value.is_empty() {
                continue;
            }
            if let Some(regex) = &indexed.value_regex
                && !regex.is_match(&affinity_value)
            {
                continue;
            }

            let ttl_seconds = if rule.ttl_seconds > 0 {
                rule.ttl_seconds
            } else {
                self.affinity.config.default_ttl_seconds.max(1)
            };
            let cache_key = affinity_cache_key(rule, requested_model, using_group, &affinity_value);
            let cached_channel_id = cache.get(&cache_key);
            return Some(ChannelAffinityCandidate {
                cache_key,
                ttl_seconds,
                cached_channel_id,
                rule_name: rule.name.clone(),
            });
        }
        None
    }

    /// Records a successful affinity decision into the caller-owned cache.
    ///
    /// The cache lives outside the snapshot (see [`ChannelAffinityCache`]) so it
    /// survives atomic snapshot swaps; the snapshot only contributes the
    /// enabled flag and the max-entries bound from its config.
    pub fn record_affinity(
        &self,
        cache: &ChannelAffinityCache,
        affinity: &ChannelAffinityCandidate,
        channel_id: &str,
    ) {
        if !self.affinity.config.enabled || channel_id.is_empty() {
            return;
        }
        cache.record(
            affinity,
            channel_id,
            self.affinity.config.max_entries.max(1),
        );
    }

    fn route_weighted<'a>(
        &'a self,
        auth: &AuthContext<'a>,
        requested_model: &'a str,
        seed: u64,
    ) -> Result<RouteDecision<'a>, RouteError> {
        let mappings = self
            .mappings
            .get(requested_model)
            .ok_or(RouteError::ModelNotFound)?;
        let mut saw_channel = false;
        let mut saw_enabled = false;
        for using_group in &auth.token.route_groups {
            let mut total_weight = 0u64;
            for mapping in mappings {
                let Some(channel) = self.channels.get(&mapping.channel_id) else {
                    continue;
                };
                saw_channel = true;
                if !channel.enabled {
                    continue;
                }
                if !channel.serves_group(using_group) {
                    continue;
                }
                saw_enabled = true;
                if !channel_serves_mapping(channel, mapping) {
                    continue;
                }
                total_weight = total_weight.saturating_add(u64::from(channel.weight.max(1)));
            }
            if total_weight == 0 {
                continue;
            }

            let mut slot = seed % total_weight;
            for mapping in mappings {
                let Some(channel) = self.channels.get(&mapping.channel_id) else {
                    continue;
                };
                if !channel.enabled
                    || !channel.serves_group(using_group)
                    || !channel_serves_mapping(channel, mapping)
                {
                    continue;
                }
                let weight = u64::from(channel.weight.max(1));
                if slot < weight {
                    return Ok(RouteDecision {
                        user_id: auth.token.user_id(),
                        channel,
                        using_group,
                        requested_model,
                        upstream_model: &mapping.upstream_model,
                    });
                }
                slot -= weight;
            }
            unreachable!("positive total route weight must select a candidate")
        }
        Err(if !saw_channel {
            RouteError::ChannelNotFound
        } else if !saw_enabled {
            RouteError::ChannelDisabled
        } else {
            RouteError::ChannelModelMismatch
        })
    }

    fn route_specific_channel<'a>(
        &'a self,
        auth: &AuthContext<'a>,
        requested_model: &'a str,
        channel_id: &str,
    ) -> Option<RouteDecision<'a>> {
        let mappings = self.mappings.get(requested_model)?;
        for mapping in mappings {
            if mapping.channel_id != channel_id {
                continue;
            }
            let channel = self.channels.get(&mapping.channel_id)?;
            let using_group = auth
                .token
                .route_groups
                .iter()
                .find(|group| channel.serves_group(group))?;
            if !channel.enabled || !channel_serves_mapping(channel, mapping) {
                return None;
            }
            return Some(RouteDecision {
                user_id: auth.token.user_id(),
                channel,
                using_group,
                requested_model,
                upstream_model: &mapping.upstream_model,
            });
        }
        None
    }
}

impl TryFrom<GatewaySnapshot> for IndexedSnapshot {
    type Error = SnapshotError;

    fn try_from(snapshot: GatewaySnapshot) -> Result<Self, Self::Error> {
        let group_routing = IndexedGroupRouting::from_config(snapshot.group_routing);
        let tokens = snapshot
            .tokens
            .into_iter()
            .map(|mut token| {
                if token.id.is_empty() {
                    token.id = token.user_id.clone();
                }
                token.normalize_groups();
                token.normalize_allowed_ips();
                let indexed = IndexedToken::from_config(token, &group_routing);
                (indexed.config.token.clone(), indexed)
            })
            .collect();
        let channels = snapshot
            .channels
            .into_iter()
            .map(|mut channel| {
                channel.normalize_api_keys();
                channel.normalize_groups();
                (channel.id.clone(), channel)
            })
            .collect();
        let mut mappings = HashMap::<String, Vec<ModelMapping>>::new();
        for mapping in snapshot.model_mappings {
            mappings
                .entry(mapping.requested_model.clone())
                .or_default()
                .push(mapping);
        }
        let affinity = IndexedChannelAffinity::from_config(snapshot.channel_affinity);

        Ok(Self {
            version: snapshot.version,
            tokens,
            channels,
            mappings,
            affinity,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AuthContext<'a> {
    pub token: &'a IndexedToken,
}

#[derive(Debug, Clone, Copy)]
pub struct RouteDecision<'a> {
    pub user_id:         &'a str,
    pub channel:         &'a ChannelConfig,
    pub using_group:     &'a str,
    pub requested_model: &'a str,
    pub upstream_model:  &'a str,
}

#[derive(Debug, Error)]
pub enum SnapshotError {}

#[derive(Debug, Error)]
pub enum RouteError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("model is not allowed for this token")]
    ModelForbidden,
    #[error("client ip is not allowed for this token")]
    IpForbidden,
    #[error("token group is not usable by this user")]
    GroupForbidden,
    #[error("token group is deprecated")]
    GroupDeprecated,
    #[error("model is not configured")]
    ModelNotFound,
    #[error("channel is not configured")]
    ChannelNotFound,
    #[error("channel is disabled")]
    ChannelDisabled,
    #[error("channel does not serve mapped upstream model")]
    ChannelModelMismatch,
}

fn default_version() -> u64 {
    1
}

fn default_weight() -> u32 {
    1
}

fn normalize_group(group: &str) -> String {
    let group = group.trim();
    if group.is_empty() {
        "default".to_string()
    } else {
        group.to_string()
    }
}

fn push_group(groups: &mut Vec<String>, group: &str) {
    let group = normalize_group(group);
    if !group_in_slice(groups, &group) {
        groups.push(group);
    }
}

fn remove_group(groups: &mut Vec<String>, group: &str) {
    let group = normalize_group(group);
    groups.retain(|item| item != &group);
}

fn normalize_group_vec(groups: &mut Vec<String>) {
    for group in groups.iter_mut() {
        *group = normalize_group(group);
    }
    groups.sort();
    groups.dedup();
}

fn normalize_group_vec_preserve_order(groups: &mut Vec<String>) {
    let mut normalized = Vec::with_capacity(groups.len());
    for group in std::mem::take(groups) {
        push_group(&mut normalized, &group);
    }
    *groups = normalized;
}

fn group_in_slice(groups: &[String], group: &str) -> bool {
    let group = normalize_group(group);
    groups.iter().any(|item| item == &group)
}

fn default_true() -> bool {
    true
}

fn default_affinity_capacity() -> usize {
    100_000
}

fn default_affinity_ttl_seconds() -> u64 {
    3_600
}

fn channel_serves_mapping(channel: &ChannelConfig, mapping: &ModelMapping) -> bool {
    channel.models.is_empty()
        || channel
            .models
            .iter()
            .any(|model| model == &mapping.upstream_model)
}

impl IndexedChannelAffinity {
    fn from_config(config: ChannelAffinityConfig) -> Self {
        let rules = config
            .rules
            .iter()
            .cloned()
            .map(IndexedChannelAffinityRule::from_rule)
            .collect();
        Self { config, rules }
    }
}

impl IndexedChannelAffinityRule {
    fn from_rule(rule: ChannelAffinityRule) -> Self {
        let model_regex = compile_regexes(&rule.model_regex);
        let path_regex = compile_regexes(&rule.path_regex);
        let value_regex = (!rule.value_regex.trim().is_empty())
            .then(|| Regex::new(rule.value_regex.trim()).ok())
            .flatten();
        let user_agent_include = rule
            .user_agent_include
            .iter()
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
            .collect();
        Self {
            rule,
            model_regex,
            path_regex,
            value_regex,
            user_agent_include,
        }
    }
}

fn compile_regexes(patterns: &[String]) -> Vec<Regex> {
    patterns
        .iter()
        .filter_map(|pattern| Regex::new(pattern.trim()).ok())
        .collect()
}

fn affinity_cache_key(
    rule: &ChannelAffinityRule,
    model: &str,
    using_group: &str,
    affinity_value: &str,
) -> String {
    let mut parts = Vec::with_capacity(4);
    if rule.include_rule_name && !rule.name.trim().is_empty() {
        parts.push(rule.name.trim());
    }
    if rule.include_model_name && !model.trim().is_empty() {
        parts.push(model.trim());
    }
    if rule.include_using_group && !using_group.trim().is_empty() {
        parts.push(using_group.trim());
    }
    parts.push(affinity_value.trim());
    parts.join(":")
}

fn json_path_string(body: &[u8], path: &str) -> Option<String> {
    if body.is_empty() || path.trim().is_empty() {
        return None;
    }
    let value = serde_json::from_slice::<JsonValue>(body).ok()?;
    let mut current = &value;
    for segment in path
        .split('.')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        current = if let Ok(index) = segment.parse::<usize>() {
            current.as_array()?.get(index)?
        } else {
            current.as_object()?.get(segment)?
        };
    }
    match current {
        JsonValue::String(value) => Some(value.trim().to_string()),
        JsonValue::Number(_) | JsonValue::Bool(_) => Some(current.to_string()),
        JsonValue::Null => None,
        JsonValue::Array(_) | JsonValue::Object(_) => Some(current.to_string()),
    }
}

fn parse_ip_allow_rule(rule: &str) -> Option<IpAllowRule> {
    let rule = rule.trim();
    if rule.is_empty() {
        return None;
    }
    if let Ok(net) = rule.parse::<IpNet>() {
        return Some(IpAllowRule::Net(net));
    }
    rule.parse::<IpAddr>().ok().map(IpAllowRule::Exact)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_snapshot_normalizes_single_api_key() {
        let snapshot = snapshot_with_channel(ChannelConfig {
            id:              "channel-a".to_string(),
            provider:        Provider::OpenAi,
            base_url:        "https://example.com".to_string(),
            api_key:         " key-a ".to_string(),
            api_keys:        Vec::new(),
            api_key_indexes: Vec::new(),
            api_key_env:     None,
            enabled:         true,
            weight:          1,
            models:          vec!["gpt-4o".to_string()],
            groups:          Vec::new(),
            proxy:           None,
        });

        let indexed = snapshot.index().expect("snapshot should index");
        let auth = indexed.authenticate("token-a").expect("auth");
        let route = indexed.route(&auth, "gpt-4o").expect("route");
        assert_eq!(route.channel.api_key, "key-a");
        assert_eq!(route.channel.api_keys, ["key-a"]);
        assert_eq!(route.channel.select_api_key(7), "key-a");
    }

    #[test]
    fn channel_selects_api_keys_by_seed() {
        let snapshot = snapshot_with_channel(ChannelConfig {
            id:              "channel-a".to_string(),
            provider:        Provider::OpenAi,
            base_url:        "https://example.com".to_string(),
            api_key:         String::new(),
            api_keys:        vec![
                " key-a ".to_string(),
                String::new(),
                "key-b".to_string(),
                "key-c".to_string(),
            ],
            api_key_indexes: Vec::new(),
            api_key_env:     None,
            enabled:         true,
            weight:          1,
            models:          vec!["gpt-4o".to_string()],
            groups:          Vec::new(),
            proxy:           None,
        });

        let indexed = snapshot.index().expect("snapshot should index");
        let auth = indexed.authenticate("token-a").expect("auth");
        let route = indexed.route(&auth, "gpt-4o").expect("route");
        assert_eq!(route.channel.api_key, "key-a");
        assert_eq!(route.channel.api_keys, ["key-a", "key-b", "key-c"]);
        assert_eq!(route.channel.api_key_indexes, [0, 2, 3]);
        assert_eq!(route.channel.select_api_key(0), "key-a");
        assert_eq!(route.channel.select_api_key(1), "key-b");
        assert_eq!(
            route.channel.select_api_key_with_index(1),
            ("key-b", Some(2))
        );
        assert_eq!(route.channel.select_api_key(3), "key-a");
    }

    #[test]
    fn route_with_seed_selects_weighted_channel_candidates() {
        let snapshot = GatewaySnapshot {
            version:          1,
            tokens:           vec![TokenConfig {
                id:             "token-a".to_string(),
                token:          "token-a".to_string(),
                user_id:        "user-a".to_string(),
                user_group:     "default".to_string(),
                token_group:    String::new(),
                group:          "default".to_string(),
                enabled:        true,
                allowed_models: Vec::new(),
                allowed_ips:    Vec::new(),
            }],
            channels:         vec![
                ChannelConfig {
                    id:              "channel-a".to_string(),
                    provider:        Provider::OpenAi,
                    base_url:        "https://a.example.com".to_string(),
                    api_key:         "key-a".to_string(),
                    api_keys:        Vec::new(),
                    api_key_indexes: Vec::new(),
                    api_key_env:     None,
                    enabled:         true,
                    weight:          1,
                    models:          vec!["gpt-4o".to_string()],
                    groups:          Vec::new(),
                    proxy:           None,
                },
                ChannelConfig {
                    id:              "channel-b".to_string(),
                    provider:        Provider::OpenAi,
                    base_url:        "https://b.example.com".to_string(),
                    api_key:         "key-b".to_string(),
                    api_keys:        Vec::new(),
                    api_key_indexes: Vec::new(),
                    api_key_env:     None,
                    enabled:         true,
                    weight:          2,
                    models:          vec!["gpt-4o".to_string()],
                    groups:          Vec::new(),
                    proxy:           None,
                },
            ],
            model_mappings:   vec![
                ModelMapping {
                    requested_model: "gpt-4o".to_string(),
                    channel_id:      "channel-a".to_string(),
                    upstream_model:  "gpt-4o".to_string(),
                },
                ModelMapping {
                    requested_model: "gpt-4o".to_string(),
                    channel_id:      "channel-b".to_string(),
                    upstream_model:  "gpt-4o".to_string(),
                },
            ],
            channel_affinity: Default::default(),
            group_routing:    Default::default(),
        };

        let indexed = snapshot.index().expect("snapshot should index");
        let auth = indexed.authenticate("token-a").expect("auth");
        assert_eq!(
            indexed
                .route_with_seed(&auth, "gpt-4o", 0)
                .expect("route")
                .channel
                .id,
            "channel-a"
        );
        assert_eq!(
            indexed
                .route_with_seed(&auth, "gpt-4o", 1)
                .expect("route")
                .channel
                .id,
            "channel-b"
        );
        assert_eq!(
            indexed
                .route_with_seed(&auth, "gpt-4o", 2)
                .expect("route")
                .channel
                .id,
            "channel-b"
        );
        assert_eq!(
            indexed
                .route_with_seed(&auth, "gpt-4o", 3)
                .expect("route")
                .channel
                .id,
            "channel-a"
        );
    }

    #[test]
    fn affinity_records_successful_channel_for_matching_request_key() {
        let snapshot = GatewaySnapshot {
            version:          1,
            tokens:           vec![TokenConfig {
                id:             "token-a".to_string(),
                token:          "token-a".to_string(),
                user_id:        "user-a".to_string(),
                user_group:     "default".to_string(),
                token_group:    String::new(),
                group:          "default".to_string(),
                enabled:        true,
                allowed_models: Vec::new(),
                allowed_ips:    Vec::new(),
            }],
            channels:         vec![
                ChannelConfig {
                    id:              "channel-a".to_string(),
                    provider:        Provider::OpenAi,
                    base_url:        "https://a.example.com".to_string(),
                    api_key:         "key-a".to_string(),
                    api_keys:        Vec::new(),
                    api_key_indexes: Vec::new(),
                    api_key_env:     None,
                    enabled:         true,
                    weight:          1,
                    models:          vec!["gpt-4o".to_string()],
                    groups:          Vec::new(),
                    proxy:           None,
                },
                ChannelConfig {
                    id:              "channel-b".to_string(),
                    provider:        Provider::OpenAi,
                    base_url:        "https://b.example.com".to_string(),
                    api_key:         "key-b".to_string(),
                    api_keys:        Vec::new(),
                    api_key_indexes: Vec::new(),
                    api_key_env:     None,
                    enabled:         true,
                    weight:          1,
                    models:          vec!["gpt-4o".to_string()],
                    groups:          Vec::new(),
                    proxy:           None,
                },
            ],
            model_mappings:   vec![
                ModelMapping {
                    requested_model: "gpt-4o".to_string(),
                    channel_id:      "channel-a".to_string(),
                    upstream_model:  "gpt-4o".to_string(),
                },
                ModelMapping {
                    requested_model: "gpt-4o".to_string(),
                    channel_id:      "channel-b".to_string(),
                    upstream_model:  "gpt-4o".to_string(),
                },
            ],
            channel_affinity: ChannelAffinityConfig {
                enabled: true,
                rules: vec![ChannelAffinityRule {
                    name: "codex cli trace".to_string(),
                    model_regex: vec!["^gpt-.*$".to_string()],
                    path_regex: vec!["/v1/responses".to_string()],
                    key_sources: vec![ChannelAffinityKeySource {
                        source_type: "gjson".to_string(),
                        key:         String::new(),
                        path:        "prompt_cache_key".to_string(),
                    }],
                    include_using_group: true,
                    include_rule_name: true,
                    ..ChannelAffinityRule::default()
                }],
                ..ChannelAffinityConfig::default()
            },
            group_routing:    Default::default(),
        };
        let snapshot_clone = snapshot.clone();
        let indexed = snapshot.index().expect("snapshot should index");
        let auth = indexed.authenticate("token-a").expect("auth");
        let cache = ChannelAffinityCache::new();
        let body = br#"{"prompt_cache_key":"session-a"}"#;
        let candidate = indexed
            .resolve_affinity(
                "gpt-4o",
                "/v1/responses",
                "",
                "default",
                body,
                &cache,
                |_| None,
            )
            .expect("affinity candidate");
        assert_eq!(candidate.cached_channel_id, None);
        let (route, hit) = indexed
            .route_with_affinity_seed(&auth, "gpt-4o", Some(&candidate), &cache, 1)
            .expect("route");
        assert!(!hit);
        assert_eq!(route.channel.id, "channel-b");
        indexed.record_affinity(&cache, &candidate, route.channel.id.as_str());

        let candidate = indexed
            .resolve_affinity(
                "gpt-4o",
                "/v1/responses",
                "",
                "default",
                body,
                &cache,
                |_| None,
            )
            .expect("affinity candidate");
        assert_eq!(candidate.cached_channel_id.as_deref(), Some("channel-b"));
        let (route, hit) = indexed
            .route_with_affinity_seed(&auth, "gpt-4o", Some(&candidate), &cache, 0)
            .expect("route");
        assert!(hit);
        assert_eq!(route.channel.id, "channel-b");

        // The cache is independent of the snapshot: re-indexing (as the gateway
        // does on every poll) must not wipe a recorded affinity.
        let reindexed = snapshot_clone.index().expect("snapshot should re-index");
        let candidate = reindexed
            .resolve_affinity(
                "gpt-4o",
                "/v1/responses",
                "",
                "default",
                body,
                &cache,
                |_| None,
            )
            .expect("affinity candidate");
        assert_eq!(candidate.cached_channel_id.as_deref(), Some("channel-b"));
    }

    #[test]
    fn token_ip_allowlist_accepts_exact_ip_and_cidr() {
        let mut snapshot = snapshot_with_channel(ChannelConfig {
            id:              "channel-a".to_string(),
            provider:        Provider::OpenAi,
            base_url:        "https://example.com".to_string(),
            api_key:         "key-a".to_string(),
            api_keys:        Vec::new(),
            api_key_indexes: Vec::new(),
            api_key_env:     None,
            enabled:         true,
            weight:          1,
            models:          vec!["gpt-4o".to_string()],
            groups:          Vec::new(),
            proxy:           None,
        });
        snapshot.tokens[0].allowed_ips =
            vec!["203.0.113.7".to_string(), "2001:db8::/32".to_string()];

        let indexed = snapshot.index().expect("snapshot should index");
        let auth = indexed.authenticate("token-a").expect("auth");
        indexed
            .authorize_ip(&auth, "203.0.113.7".parse().unwrap())
            .expect("exact ip should pass");
        indexed
            .authorize_ip(&auth, "2001:db8::1".parse().unwrap())
            .expect("cidr ip should pass");
        assert!(matches!(
            indexed.authorize_ip(&auth, "203.0.113.8".parse().unwrap()),
            Err(RouteError::IpForbidden)
        ));
    }

    #[test]
    fn token_ip_allowlist_with_only_invalid_rules_denies() {
        let mut snapshot = snapshot_with_channel(ChannelConfig {
            id:              "channel-a".to_string(),
            provider:        Provider::OpenAi,
            base_url:        "https://example.com".to_string(),
            api_key:         "key-a".to_string(),
            api_keys:        Vec::new(),
            api_key_indexes: Vec::new(),
            api_key_env:     None,
            enabled:         true,
            weight:          1,
            models:          vec!["gpt-4o".to_string()],
            groups:          Vec::new(),
            proxy:           None,
        });
        snapshot.tokens[0].allowed_ips = vec!["not-an-ip".to_string()];

        let indexed = snapshot.index().expect("snapshot should index");
        let auth = indexed.authenticate("token-a").expect("auth");
        assert!(matches!(
            indexed.authorize_ip(&auth, "127.0.0.1".parse().unwrap()),
            Err(RouteError::IpForbidden)
        ));
    }

    #[test]
    fn route_selects_only_channels_in_token_group() {
        let mut snapshot = GatewaySnapshot {
            version:          1,
            tokens:           vec![TokenConfig {
                id:             "token-a".to_string(),
                token:          "token-a".to_string(),
                user_id:        "user-a".to_string(),
                user_group:     "paid".to_string(),
                token_group:    String::new(),
                group:          "paid".to_string(),
                enabled:        true,
                allowed_models: Vec::new(),
                allowed_ips:    Vec::new(),
            }],
            channels:         vec![
                ChannelConfig {
                    id:              "default-channel".to_string(),
                    provider:        Provider::OpenAi,
                    base_url:        "https://default.example.com".to_string(),
                    api_key:         "key-a".to_string(),
                    api_keys:        Vec::new(),
                    api_key_indexes: Vec::new(),
                    api_key_env:     None,
                    enabled:         true,
                    weight:          100,
                    models:          vec!["gpt-4o".to_string()],
                    groups:          vec!["default".to_string()],
                    proxy:           None,
                },
                ChannelConfig {
                    id:              "paid-channel".to_string(),
                    provider:        Provider::OpenAi,
                    base_url:        "https://paid.example.com".to_string(),
                    api_key:         "key-b".to_string(),
                    api_keys:        Vec::new(),
                    api_key_indexes: Vec::new(),
                    api_key_env:     None,
                    enabled:         true,
                    weight:          1,
                    models:          vec!["gpt-4o".to_string()],
                    groups:          vec!["paid".to_string()],
                    proxy:           None,
                },
            ],
            model_mappings:   Vec::new(),
            channel_affinity: Default::default(),
            group_routing:    Default::default(),
        };
        snapshot.model_mappings = snapshot
            .channels
            .iter()
            .map(|channel| ModelMapping {
                requested_model: "gpt-4o".to_string(),
                channel_id:      channel.id.clone(),
                upstream_model:  "gpt-4o".to_string(),
            })
            .collect();

        let indexed = snapshot.index().expect("snapshot should index");
        let auth = indexed.authenticate("token-a").expect("auth");
        let route = indexed.route_with_seed(&auth, "gpt-4o", 0).expect("route");
        assert_eq!(route.channel.id, "paid-channel");
    }

    #[test]
    fn auto_group_routes_through_first_usable_auto_group() {
        let mut snapshot = GatewaySnapshot {
            version:          1,
            tokens:           vec![TokenConfig {
                id:             "token-a".to_string(),
                token:          "token-a".to_string(),
                user_id:        "user-a".to_string(),
                user_group:     "default".to_string(),
                token_group:    "auto".to_string(),
                group:          "auto".to_string(),
                enabled:        true,
                allowed_models: Vec::new(),
                allowed_ips:    Vec::new(),
            }],
            channels:         vec![
                ChannelConfig {
                    id:              "default-channel".to_string(),
                    provider:        Provider::OpenAi,
                    base_url:        "https://default.example.com".to_string(),
                    api_key:         "key-a".to_string(),
                    api_keys:        Vec::new(),
                    api_key_indexes: Vec::new(),
                    api_key_env:     None,
                    enabled:         true,
                    weight:          100,
                    models:          vec!["gpt-4o".to_string()],
                    groups:          vec!["default".to_string()],
                    proxy:           None,
                },
                ChannelConfig {
                    id:              "paid-channel".to_string(),
                    provider:        Provider::OpenAi,
                    base_url:        "https://paid.example.com".to_string(),
                    api_key:         "key-b".to_string(),
                    api_keys:        Vec::new(),
                    api_key_indexes: Vec::new(),
                    api_key_env:     None,
                    enabled:         true,
                    weight:          1,
                    models:          vec!["gpt-4o".to_string()],
                    groups:          vec!["paid".to_string()],
                    proxy:           None,
                },
            ],
            model_mappings:   Vec::new(),
            channel_affinity: Default::default(),
            group_routing:    GroupRoutingConfig {
                auto_groups:                 vec!["paid".to_string(), "default".to_string()],
                user_usable_groups:          HashMap::from([
                    ("auto".to_string(), "auto".to_string()),
                    ("default".to_string(), "default".to_string()),
                    ("paid".to_string(), "paid".to_string()),
                ]),
                group_special_usable_groups: HashMap::new(),
                known_groups:                vec!["default".to_string(), "paid".to_string()],
            },
        };
        snapshot.model_mappings = snapshot
            .channels
            .iter()
            .map(|channel| ModelMapping {
                requested_model: "gpt-4o".to_string(),
                channel_id:      channel.id.clone(),
                upstream_model:  "gpt-4o".to_string(),
            })
            .collect();

        let indexed = snapshot.index().expect("snapshot should index");
        let auth = indexed.authenticate("token-a").expect("auth");
        assert_eq!(auth.token.usable_groups(), ["auto", "default", "paid"]);
        let route = indexed.route_with_seed(&auth, "gpt-4o", 0).expect("route");
        assert_eq!(route.channel.id, "paid-channel");
        assert_eq!(route.using_group, "paid");
    }

    #[test]
    fn token_group_override_must_be_user_usable() {
        let mut snapshot = snapshot_with_channel(ChannelConfig {
            id:              "channel-a".to_string(),
            provider:        Provider::OpenAi,
            base_url:        "https://example.com".to_string(),
            api_key:         "key-a".to_string(),
            api_keys:        Vec::new(),
            api_key_indexes: Vec::new(),
            api_key_env:     None,
            enabled:         true,
            weight:          1,
            models:          vec!["gpt-4o".to_string()],
            groups:          vec!["paid".to_string()],
            proxy:           None,
        });
        snapshot.tokens[0].user_group = "default".to_string();
        snapshot.tokens[0].token_group = "paid".to_string();
        snapshot.tokens[0].group = "paid".to_string();
        snapshot.group_routing = GroupRoutingConfig {
            user_usable_groups: HashMap::from([("default".to_string(), "default".to_string())]),
            known_groups: vec!["default".to_string(), "paid".to_string()],
            ..GroupRoutingConfig::default()
        };

        let indexed = snapshot.index().expect("snapshot should index");
        assert!(matches!(
            indexed.authenticate("token-a"),
            Err(RouteError::GroupForbidden)
        ));
    }

    fn snapshot_with_channel(channel: ChannelConfig) -> GatewaySnapshot {
        GatewaySnapshot {
            version:          1,
            tokens:           vec![TokenConfig {
                id:             "token-a".to_string(),
                token:          "token-a".to_string(),
                user_id:        "user-a".to_string(),
                user_group:     "default".to_string(),
                token_group:    String::new(),
                group:          "default".to_string(),
                enabled:        true,
                allowed_models: Vec::new(),
                allowed_ips:    Vec::new(),
            }],
            channels:         vec![channel],
            model_mappings:   vec![ModelMapping {
                requested_model: "gpt-4o".to_string(),
                channel_id:      "channel-a".to_string(),
                upstream_model:  "gpt-4o".to_string(),
            }],
            channel_affinity: Default::default(),
            group_routing:    Default::default(),
        }
    }
}
