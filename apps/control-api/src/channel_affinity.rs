use crate::storage::OptionStore;
use halolake_control_plane::ManagementError;
use serde::{Deserialize, Serialize};
use service_async::Service;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub(crate) struct GetChannelAffinityCacheStatsRequest;

#[derive(Debug, Clone)]
pub(crate) struct ClearChannelAffinityCacheRequest {
    pub(crate) all:       bool,
    pub(crate) rule_name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct GetChannelAffinityUsageCacheStatsRequest {
    pub(crate) rule_name:   String,
    pub(crate) using_group: String,
    pub(crate) key_fp:      String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ChannelAffinityCacheStats {
    enabled:        bool,
    total:          usize,
    unknown:        usize,
    by_rule_name:   BTreeMap<String, usize>,
    cache_capacity: usize,
    cache_algo:     &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ChannelAffinityClearAck {
    deleted: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ChannelAffinityUsageCacheStats {
    rule_name:               String,
    using_group:             String,
    key_fp:                  String,
    cached_token_rate_mode:  String,
    hit:                     i64,
    total:                   i64,
    window_seconds:          i64,
    prompt_tokens:           i64,
    completion_tokens:       i64,
    total_tokens:            i64,
    cached_tokens:           i64,
    prompt_cache_hit_tokens: i64,
    last_seen_at:            i64,
}

#[derive(Debug, Clone)]
pub(crate) struct ChannelAffinityService {
    options: OptionStore,
}

impl ChannelAffinityService {
    pub(crate) fn new(options: OptionStore) -> Self {
        Self { options }
    }
}

impl Service<GetChannelAffinityCacheStatsRequest> for ChannelAffinityService {
    type Response = ChannelAffinityCacheStats;
    type Error = ManagementError;

    async fn call(
        &self,
        _req: GetChannelAffinityCacheStatsRequest,
    ) -> Result<Self::Response, Self::Error> {
        let options = self.options.values()?;
        let rules = channel_affinity_rules(&options);
        let by_rule_name = rules
            .iter()
            .filter(|rule| rule.include_rule_name)
            .filter_map(|rule| {
                let name = rule.name.trim();
                (!name.is_empty()).then_some((name.to_string(), 0usize))
            })
            .collect::<BTreeMap<_, _>>();
        Ok(ChannelAffinityCacheStats {
            enabled: option_bool(&options, "channel_affinity_setting.enabled", true),
            total: 0,
            unknown: 0,
            by_rule_name,
            cache_capacity: option_usize(&options, "channel_affinity_setting.max_entries", 100_000),
            cache_algo: "LRU",
        })
    }
}

impl Service<ClearChannelAffinityCacheRequest> for ChannelAffinityService {
    type Response = ChannelAffinityClearAck;
    type Error = ManagementError;

    async fn call(
        &self,
        req: ClearChannelAffinityCacheRequest,
    ) -> Result<Self::Response, Self::Error> {
        if req.all {
            return Ok(ChannelAffinityClearAck { deleted: 0 });
        }
        let rule_name = req.rule_name.trim();
        if rule_name.is_empty() {
            return Err(ManagementError::InvalidRequest(
                "缺少参数：rule_name，或使用 all=true 清空全部",
            ));
        }
        let options = self.options.values()?;
        let Some(rule) = channel_affinity_rules(&options)
            .into_iter()
            .find(|rule| rule.name.trim() == rule_name)
        else {
            return Err(ManagementError::InvalidRequest("未知规则名称"));
        };
        if !rule.include_rule_name {
            return Err(ManagementError::InvalidRequest(
                "该规则未启用 include_rule_name，无法按规则清空缓存",
            ));
        }
        Ok(ChannelAffinityClearAck { deleted: 0 })
    }
}

impl Service<GetChannelAffinityUsageCacheStatsRequest> for ChannelAffinityService {
    type Response = ChannelAffinityUsageCacheStats;
    type Error = ManagementError;

    async fn call(
        &self,
        req: GetChannelAffinityUsageCacheStatsRequest,
    ) -> Result<Self::Response, Self::Error> {
        let rule_name = req.rule_name.trim();
        if rule_name.is_empty() {
            return Err(ManagementError::InvalidRequest("missing param: rule_name"));
        }
        let key_fp = req.key_fp.trim();
        if key_fp.is_empty() {
            return Err(ManagementError::InvalidRequest("missing param: key_fp"));
        }
        Ok(ChannelAffinityUsageCacheStats {
            rule_name:               rule_name.to_string(),
            using_group:             req.using_group.trim().to_string(),
            key_fp:                  key_fp.to_string(),
            cached_token_rate_mode:  String::new(),
            hit:                     0,
            total:                   0,
            window_seconds:          0,
            prompt_tokens:           0,
            completion_tokens:       0,
            total_tokens:            0,
            cached_tokens:           0,
            prompt_cache_hit_tokens: 0,
            last_seen_at:            0,
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ChannelAffinityRule {
    #[serde(default)]
    name:              String,
    #[serde(default)]
    include_rule_name: bool,
}

fn channel_affinity_rules(options: &BTreeMap<String, String>) -> Vec<ChannelAffinityRule> {
    options
        .get("channel_affinity_setting.rules")
        .and_then(|value| serde_json::from_str::<Vec<ChannelAffinityRule>>(value).ok())
        .unwrap_or_default()
}

fn option_bool(options: &BTreeMap<String, String>, key: &str, default: bool) -> bool {
    options
        .get(key)
        .and_then(|value| value.parse::<bool>().ok())
        .unwrap_or(default)
}

fn option_usize(options: &BTreeMap<String, String>, key: &str, default: usize) -> usize {
    options
        .get(key)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::OptionStore;

    #[tokio::test]
    async fn stats_include_rule_names_from_options() {
        let service = ChannelAffinityService::new(OptionStore::memory(BTreeMap::from([
            ("channel_affinity_setting.enabled".to_string(), "true".to_string()),
            (
                "channel_affinity_setting.max_entries".to_string(),
                "123".to_string(),
            ),
            (
                "channel_affinity_setting.rules".to_string(),
                r#"[{"name":"codex","include_rule_name":true},{"name":"ignored","include_rule_name":false}]"#
                    .to_string(),
            ),
        ])));

        let stats = service
            .call(GetChannelAffinityCacheStatsRequest)
            .await
            .expect("stats");
        assert!(stats.enabled);
        assert_eq!(stats.cache_capacity, 123);
        assert_eq!(stats.by_rule_name.get("codex"), Some(&0));
        assert!(!stats.by_rule_name.contains_key("ignored"));
    }
}
