use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use halolake_router_core::{
    ChannelAffinityCache, ChannelAffinityCandidate, ChannelAffinityConfig,
    ChannelAffinityKeySource, ChannelAffinityRequest, ChannelAffinityRule, ChannelConfig,
    GatewaySnapshot, GroupRoutingConfig, ModelMapping, Provider, TokenConfig,
};
use std::{collections::HashMap, time::Duration};

const MODEL: &str = "gpt-benchmark";
const GROUP: &str = "benchmark";
const TOKEN: &str = "benchmark-token";

fn snapshot(channel_count: usize, allowed_model_count: usize) -> GatewaySnapshot {
    let mut allowed_models = (0..allowed_model_count.saturating_sub(1))
        .map(|index| format!("unused-model-{index}"))
        .collect::<Vec<_>>();
    if allowed_model_count > 0 {
        allowed_models.push(MODEL.to_string());
    }

    let channels = (0..channel_count)
        .map(|index| ChannelConfig {
            id: format!("channel-{index}"),
            management_id: Some(index as u64 + 1),
            provider: Provider::OpenAi,
            base_url: "https://upstream.example".to_string(),
            api_key: "benchmark-key".to_string(),
            api_keys: Vec::new(),
            api_key_indexes: Vec::new(),
            api_key_env: None,
            enabled: true,
            weight: (index % 100 + 1) as u32,
            models: vec![MODEL.to_string()],
            groups: vec![GROUP.to_string()],
            proxy: None,
            proxy_required: false,
            header_override: Default::default(),
            upstream_endpoint_type: String::new(),
        })
        .collect::<Vec<_>>();
    let model_mappings = channels
        .iter()
        .map(|channel| ModelMapping {
            requested_model: MODEL.to_string(),
            channel_id: channel.id.clone(),
            upstream_model: MODEL.to_string(),
        })
        .collect();

    GatewaySnapshot {
        version: 1,
        tokens: vec![TokenConfig {
            id: "benchmark-token-id".to_string(),
            token: TOKEN.to_string(),
            user_id: "benchmark-user".to_string(),
            user_group: GROUP.to_string(),
            token_group: GROUP.to_string(),
            group: GROUP.to_string(),
            enabled: true,
            allowed_models,
            allowed_ips: Vec::new(),
        }],
        channels,
        model_mappings,
        channel_affinity: ChannelAffinityConfig::default(),
        group_routing: GroupRoutingConfig::default(),
    }
}

/// The routing algorithm used before `IndexedSnapshot` precomputed eligible
/// candidates and cumulative weights. Keeping it here gives the benchmark a
/// stable regression baseline without putting the old implementation back on
/// the production path.
struct LegacyRoutes {
    channels: HashMap<String, ChannelConfig>,
    mappings: HashMap<String, Vec<ModelMapping>>,
}

impl LegacyRoutes {
    fn from_snapshot(snapshot: &GatewaySnapshot) -> Self {
        Self {
            channels: snapshot
                .channels
                .iter()
                .cloned()
                .map(|channel| (channel.id.clone(), channel))
                .collect(),
            mappings: snapshot.model_mappings.iter().cloned().fold(
                HashMap::<String, Vec<ModelMapping>>::new(),
                |mut mappings, mapping| {
                    mappings
                        .entry(mapping.requested_model.clone())
                        .or_default()
                        .push(mapping);
                    mappings
                },
            ),
        }
    }

    fn route(&self, requested_model: &str, using_group: &str, seed: u64) -> &ChannelConfig {
        let mappings = self
            .mappings
            .get(requested_model)
            .expect("benchmark model must exist");
        let mut total_weight = 0u64;
        for mapping in mappings {
            let Some(channel) = self.channels.get(&mapping.channel_id) else {
                continue;
            };
            if !channel.enabled
                || !legacy_serves_group(channel, using_group)
                || !channel_serves_mapping(channel, mapping)
            {
                continue;
            }
            total_weight = total_weight.saturating_add(u64::from(channel.weight.max(1)));
        }
        assert!(total_weight > 0, "benchmark snapshot must have a route");

        let mut slot = seed % total_weight;
        for mapping in mappings {
            let Some(channel) = self.channels.get(&mapping.channel_id) else {
                continue;
            };
            if !channel.enabled
                || !legacy_serves_group(channel, using_group)
                || !channel_serves_mapping(channel, mapping)
            {
                continue;
            }
            let weight = u64::from(channel.weight.max(1));
            if slot < weight {
                return channel;
            }
            slot -= weight;
        }
        unreachable!("positive total route weight must select a candidate")
    }
}

fn legacy_serves_group(channel: &ChannelConfig, group: &str) -> bool {
    // Match the former hot path, including normalization into a fresh String
    // on every candidate scan.
    let group = if group.trim().is_empty() {
        "default".to_string()
    } else {
        group.trim().to_string()
    };
    channel.groups.iter().any(|candidate| candidate == &group)
}

fn channel_serves_mapping(channel: &ChannelConfig, mapping: &ModelMapping) -> bool {
    channel.models.is_empty()
        || channel
            .models
            .iter()
            .any(|model| model == &mapping.upstream_model)
}

fn weighted_routing(c: &mut Criterion) {
    let mut group = c.benchmark_group("weighted_routing");
    for channel_count in [1, 64, 1_024, 8_192] {
        let snapshot = snapshot(channel_count, 0);
        let legacy = LegacyRoutes::from_snapshot(&snapshot);
        let indexed = snapshot.index().expect("valid snapshot");
        let auth = indexed.authenticate(TOKEN).expect("valid token");
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("legacy_scan", channel_count),
            &channel_count,
            |bencher, _| {
                let mut seed = 0u64;
                bencher.iter(|| {
                    seed = seed.wrapping_add(1);
                    let channel = legacy.route(MODEL, GROUP, std::hint::black_box(seed));
                    std::hint::black_box(channel.id.as_str());
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("indexed", channel_count),
            &channel_count,
            |bencher, _| {
                let mut seed = 0u64;
                bencher.iter(|| {
                    seed = seed.wrapping_add(1);
                    let route = indexed
                        .route_with_seed(&auth, MODEL, std::hint::black_box(seed))
                        .expect("route");
                    std::hint::black_box(route.channel.id.as_str());
                });
            },
        );
    }
    group.finish();
}

fn specific_channel_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("specific_channel_lookup");
    let cache = ChannelAffinityCache::new();
    for channel_count in [64, 1_024, 8_192] {
        let indexed = snapshot(channel_count, 0).index().expect("valid snapshot");
        let auth = indexed.authenticate(TOKEN).expect("valid token");
        let affinity = ChannelAffinityCandidate {
            cache_key: "benchmark-affinity".to_string(),
            ttl_seconds: 60,
            cached_channel_id: Some(format!("channel-{}", channel_count - 1)),
            rule_name: "benchmark".to_string(),
        };
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::from_parameter(channel_count),
            &channel_count,
            |bencher, _| {
                bencher.iter(|| {
                    let (route, hit) = indexed
                        .route_with_affinity_seed(
                            &auth,
                            MODEL,
                            Some(std::hint::black_box(&affinity)),
                            &cache,
                            0,
                        )
                        .expect("route");
                    std::hint::black_box((route.channel.id.as_str(), hit));
                });
            },
        );
    }
    group.finish();
}

fn affinity_cache_hit(c: &mut Criterion) {
    let mut group = c.benchmark_group("affinity_cache_hit");
    for channel_count in [64, 1_024, 8_192] {
        let mut source = snapshot(channel_count, 0);
        source.channel_affinity = ChannelAffinityConfig {
            enabled: true,
            rules: vec![ChannelAffinityRule {
                name: "benchmark".to_string(),
                model_regex: vec![format!("^{MODEL}$")],
                key_sources: vec![ChannelAffinityKeySource {
                    source_type: "request_header".to_string(),
                    key: "x-session-id".to_string(),
                    path: String::new(),
                }],
                ..ChannelAffinityRule::default()
            }],
            ..ChannelAffinityConfig::default()
        };
        let indexed = source.index().expect("valid affinity snapshot");
        let auth = indexed.authenticate(TOKEN).expect("valid token");
        let cache = ChannelAffinityCache::new();
        let initial = indexed
            .resolve_affinity(
                ChannelAffinityRequest {
                    requested_model: MODEL,
                    path: "/v1/chat/completions",
                    user_agent: "",
                    using_group: GROUP,
                    body: b"",
                },
                &cache,
                |_| Some("benchmark-session".to_string()),
            )
            .expect("matching affinity rule");
        indexed.record_affinity(&cache, &initial, &format!("channel-{}", channel_count - 1));

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::from_parameter(channel_count),
            &channel_count,
            |bencher, _| {
                bencher.iter(|| {
                    let affinity = indexed
                        .resolve_affinity(
                            ChannelAffinityRequest {
                                requested_model: MODEL,
                                path: "/v1/chat/completions",
                                user_agent: "",
                                using_group: GROUP,
                                body: b"",
                            },
                            &cache,
                            |_| Some("benchmark-session".to_string()),
                        )
                        .expect("cached affinity");
                    let (route, hit) = indexed
                        .route_with_affinity_seed(&auth, MODEL, Some(&affinity), &cache, 0)
                        .expect("affinity route");
                    std::hint::black_box((route.channel.id.as_str(), hit));
                });
            },
        );
    }
    group.finish();
}

fn allowed_model_check(c: &mut Criterion) {
    let mut group = c.benchmark_group("allowed_model_check");
    for allowed_model_count in [1, 64, 1_024, 8_192] {
        let indexed = snapshot(1, allowed_model_count)
            .index()
            .expect("valid snapshot");
        let auth = indexed.authenticate(TOKEN).expect("valid token");
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::from_parameter(allowed_model_count),
            &allowed_model_count,
            |bencher, _| {
                bencher.iter(|| {
                    let route = indexed
                        .route_with_seed(&auth, std::hint::black_box(MODEL), 0)
                        .expect("route");
                    std::hint::black_box(route.channel.id.as_str());
                });
            },
        );
    }
    group.finish();
}

fn criterion() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .sample_size(40)
}

criterion_group! {
    name = routing;
    config = criterion();
    targets = weighted_routing, specific_channel_lookup, affinity_cache_hit, allowed_model_check
}
criterion_main!(routing);
