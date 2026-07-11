use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use futures_util::{StreamExt, stream};
use halolake_control_plane::{ManagementData, ManagementError};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue, json};
use service_async::Service;

use crate::storage::{ManagementStore, OptionStore};

const DEFAULT_TIMEOUT_SECONDS: u64 = 10;
const DEFAULT_ENDPOINT: &str = "/api/pricing";
const MAX_CONCURRENT_FETCHES: usize = 8;
const MAX_RATIO_CONFIG_BYTES: usize = 10 << 20;
const FLOAT_EPSILON: f64 = 1e-9;
const USD_RATIO_BASE: f64 = 500.0;
const MODELS_DEV_INPUT_COST_RATIO_BASE: f64 = 1000.0;

const OFFICIAL_RATIO_PRESET_ID: i64 = -100;
const OFFICIAL_RATIO_PRESET_NAME: &str = "官方倍率预设";
const OFFICIAL_RATIO_PRESET_BASE_URL: &str = "https://basellm.github.io";
const MODELS_DEV_PRESET_ID: i64 = -101;
const MODELS_DEV_PRESET_NAME: &str = "models.dev 价格预设";
const MODELS_DEV_PRESET_BASE_URL: &str = "https://models.dev";

const PRICING_SYNC_FIELDS: [&str; 10] = [
    "model_ratio",
    "completion_ratio",
    "cache_ratio",
    "create_cache_ratio",
    "image_ratio",
    "audio_ratio",
    "audio_completion_ratio",
    "model_price",
    "billing_mode",
    "billing_expr",
];

const NUMERIC_PRICING_SYNC_FIELDS: [&str; 8] = [
    "model_ratio",
    "completion_ratio",
    "cache_ratio",
    "create_cache_ratio",
    "image_ratio",
    "audio_ratio",
    "audio_completion_ratio",
    "model_price",
];

#[derive(Debug, Clone, Copy)]
pub(crate) struct ListSyncableChannelsRequest;

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct FetchUpstreamRatiosRequest {
    #[serde(default)]
    pub(crate) channel_ids: Vec<i64>,
    #[serde(default)]
    pub(crate) upstreams: Vec<UpstreamConfig>,
    #[serde(default)]
    pub(crate) timeout: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct UpstreamConfig {
    #[serde(default)]
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) base_url: String,
    #[serde(default)]
    pub(crate) endpoint: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SyncableChannel {
    id: i64,
    name: String,
    base_url: String,
    status: i32,
    #[serde(rename = "type")]
    channel_type: i32,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct FetchUpstreamRatiosResponse {
    differences: BTreeMap<String, BTreeMap<String, DifferenceItem>>,
    test_results: Vec<TestResult>,
}

#[derive(Debug, Clone, Serialize)]
struct TestResult {
    name: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct DifferenceItem {
    current: JsonValue,
    upstreams: BTreeMap<String, JsonValue>,
    confidence: BTreeMap<String, bool>,
}

#[derive(Debug, Clone)]
struct UpstreamResult {
    name: String,
    data: Option<BTreeMap<String, JsonValue>>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct ModelsDevCandidate {
    provider: String,
    input: f64,
    output: Option<f64>,
    cache_read: Option<f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct RatioSyncService {
    management: ManagementStore,
    options: OptionStore,
    client: reqwest::Client,
}

impl RatioSyncService {
    pub(crate) fn new(management: ManagementStore, options: OptionStore) -> Self {
        Self {
            management,
            options,
            client: reqwest::Client::new(),
        }
    }
}

impl Service<ListSyncableChannelsRequest> for RatioSyncService {
    type Response = Vec<SyncableChannel>;
    type Error = ManagementError;

    async fn call(&self, _req: ListSyncableChannelsRequest) -> Result<Self::Response, Self::Error> {
        let management = self.management.current_data()?;
        Ok(syncable_channels(&management))
    }
}

impl Service<FetchUpstreamRatiosRequest> for RatioSyncService {
    type Response = FetchUpstreamRatiosResponse;
    type Error = ManagementError;

    async fn call(
        &self,
        mut req: FetchUpstreamRatiosRequest,
    ) -> Result<Self::Response, Self::Error> {
        if req.timeout == 0 {
            req.timeout = DEFAULT_TIMEOUT_SECONDS;
        }
        let management = self.management.current_data()?;
        let upstreams = upstreams_from_request(req, &management)?;
        if upstreams.is_empty() {
            return Err(ManagementError::InvalidRequest("无有效上游渠道"));
        }

        let timeout = Duration::from_secs(upstreams[0].timeout_seconds);
        let results = stream::iter(upstreams)
            .map(|upstream| {
                let service = self.clone();
                async move { service.fetch_one(upstream, timeout).await }
            })
            .buffer_unordered(MAX_CONCURRENT_FETCHES)
            .collect::<Vec<_>>()
            .await;

        let mut test_results = Vec::with_capacity(results.len());
        let mut successful = Vec::new();
        for result in results {
            match result {
                Ok(result) => {
                    if let Some(error) = result.error {
                        test_results.push(TestResult {
                            name: result.name,
                            status: "error",
                            error: Some(error),
                        });
                    } else {
                        test_results.push(TestResult {
                            name: result.name.clone(),
                            status: "success",
                            error: None,
                        });
                        if let Some(data) = result.data {
                            successful.push((result.name, data));
                        }
                    }
                }
                Err(err) => test_results.push(TestResult {
                    name: "unknown".to_string(),
                    status: "error",
                    error: Some(err.to_string()),
                }),
            }
        }

        let options = self.options.values()?;
        let local_data = local_pricing_sync_data(&options);
        Ok(FetchUpstreamRatiosResponse {
            differences: build_differences(&local_data, &successful),
            test_results,
        })
    }
}

impl RatioSyncService {
    async fn fetch_one(
        &self,
        upstream: PreparedUpstream,
        timeout: Duration,
    ) -> Result<UpstreamResult, ManagementError> {
        let full_url = upstream.full_url();
        let is_models_dev = is_models_dev_api_endpoint(&full_url);
        let mut last_error = None;

        for attempt in 0..3 {
            let mut request = self.client.get(&full_url).timeout(timeout);
            if upstream.openrouter {
                let Some(auth) = self.openrouter_auth_header(upstream.id)? else {
                    return Ok(upstream.error("OpenRouter requires a valid channel with API key"));
                };
                request = request.header(reqwest::header::AUTHORIZATION, auth);
            }

            match request.send().await {
                Ok(response) => {
                    if !response.status().is_success() {
                        return Ok(upstream.error(response.status().to_string()));
                    }
                    let body = read_limited_body(response).await?;
                    let data = parse_upstream_ratio_data(&body, upstream.openrouter, is_models_dev)
                        .map_err(ManagementError::Storage)?;
                    return Ok(UpstreamResult {
                        name: upstream.unique_name,
                        data: Some(data),
                        error: None,
                    });
                }
                Err(err) => {
                    last_error = Some(err.to_string());
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(200 * (1 << attempt))).await;
                    }
                }
            }
        }

        Ok(upstream.error(last_error.unwrap_or_else(|| "request failed".to_string())))
    }

    fn openrouter_auth_header(&self, channel_id: i64) -> Result<Option<String>, ManagementError> {
        if channel_id <= 0 {
            return Ok(None);
        }
        let management = self.management.current_data()?;
        Ok(management
            .channels
            .iter()
            .find(|channel| channel.id == channel_id as u64)
            .map(|channel| channel.key.trim())
            .filter(|key| !key.is_empty())
            .map(|key| format!("Bearer {key}")))
    }
}

#[derive(Debug, Clone)]
struct PreparedUpstream {
    id: i64,
    unique_name: String,
    base_url: String,
    endpoint: String,
    openrouter: bool,
    timeout_seconds: u64,
}

impl PreparedUpstream {
    fn from_config(config: UpstreamConfig, timeout_seconds: u64) -> Option<Self> {
        let base_url = config.base_url.trim().trim_end_matches('/').to_string();
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return None;
        }
        let unique_name = if config.id != 0 {
            format!("{}({})", config.name, config.id)
        } else {
            config.name.clone()
        };
        let openrouter = config.endpoint == "openrouter";
        Some(Self {
            id: config.id,
            unique_name,
            base_url,
            endpoint: config.endpoint,
            openrouter,
            timeout_seconds,
        })
    }

    fn full_url(&self) -> String {
        if self.openrouter {
            return format!("{}/v1/models", self.base_url);
        }
        if self.endpoint.starts_with("http://") || self.endpoint.starts_with("https://") {
            return self.endpoint.clone();
        }
        let endpoint = if self.endpoint.is_empty() {
            DEFAULT_ENDPOINT
        } else if self.endpoint.starts_with('/') {
            &self.endpoint
        } else {
            return format!("{}/{}", self.base_url, self.endpoint);
        };
        format!("{}{}", self.base_url, endpoint)
    }

    fn error(&self, error: impl Into<String>) -> UpstreamResult {
        UpstreamResult {
            name: self.unique_name.clone(),
            data: None,
            error: Some(error.into()),
        }
    }
}

fn syncable_channels(management: &ManagementData) -> Vec<SyncableChannel> {
    let mut channels = management
        .channels
        .iter()
        .filter_map(|channel| {
            let base_url = channel.base_url.as_deref()?.trim();
            if base_url.is_empty() {
                return None;
            }
            Some(SyncableChannel {
                id: channel.id as i64,
                name: channel.name.clone(),
                base_url: base_url.to_string(),
                status: channel.status,
                channel_type: channel.channel_type,
            })
        })
        .collect::<Vec<_>>();

    channels.push(SyncableChannel {
        id: OFFICIAL_RATIO_PRESET_ID,
        name: OFFICIAL_RATIO_PRESET_NAME.to_string(),
        base_url: OFFICIAL_RATIO_PRESET_BASE_URL.to_string(),
        status: 1,
        channel_type: 0,
    });
    channels.push(SyncableChannel {
        id: MODELS_DEV_PRESET_ID,
        name: MODELS_DEV_PRESET_NAME.to_string(),
        base_url: MODELS_DEV_PRESET_BASE_URL.to_string(),
        status: 1,
        channel_type: 0,
    });
    channels
}

fn upstreams_from_request(
    req: FetchUpstreamRatiosRequest,
    management: &ManagementData,
) -> Result<Vec<PreparedUpstream>, ManagementError> {
    let timeout = if req.timeout == 0 {
        DEFAULT_TIMEOUT_SECONDS
    } else {
        req.timeout
    };

    let configs = if req.upstreams.is_empty() && !req.channel_ids.is_empty() {
        req.channel_ids
            .into_iter()
            .filter_map(|id| {
                management
                    .channels
                    .iter()
                    .find(|channel| channel.id == id as u64)
                    .and_then(|channel| {
                        Some(UpstreamConfig {
                            id,
                            name: channel.name.clone(),
                            base_url: channel.base_url.clone()?,
                            endpoint: String::new(),
                        })
                    })
            })
            .collect::<Vec<_>>()
    } else {
        req.upstreams
    };

    Ok(configs
        .into_iter()
        .filter_map(|config| PreparedUpstream::from_config(config, timeout))
        .collect())
}

async fn read_limited_body(response: reqwest::Response) -> Result<Vec<u8>, ManagementError> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RATIO_CONFIG_BYTES as u64)
    {
        return Err(ManagementError::Storage(
            "ratio config too large".to_string(),
        ));
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| ManagementError::Storage(err.to_string()))?;
        if body.len().saturating_add(chunk.len()) > MAX_RATIO_CONFIG_BYTES {
            return Err(ManagementError::Storage(
                "ratio config too large".to_string(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_upstream_ratio_data(
    body: &[u8],
    openrouter: bool,
    models_dev: bool,
) -> Result<BTreeMap<String, JsonValue>, String> {
    if openrouter {
        return convert_openrouter_to_ratio_data(body);
    }
    if models_dev {
        return convert_models_dev_to_ratio_data(body);
    }

    #[derive(Deserialize)]
    struct Wrapped {
        #[serde(default)]
        success: bool,
        #[serde(default)]
        data: JsonValue,
        #[serde(default)]
        message: String,
    }

    let wrapped = serde_json::from_slice::<Wrapped>(body)
        .map_err(|err| format!("json decode failed: {err}"))?;
    if !wrapped.success {
        return Err(wrapped.message);
    }
    if let JsonValue::Object(object) = &wrapped.data
        && PRICING_SYNC_FIELDS
            .iter()
            .any(|field| object.contains_key(*field))
    {
        return Ok(object
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect());
    }
    if let JsonValue::Array(items) = wrapped.data {
        return convert_pricing_items_to_ratio_data(items);
    }
    Err("无法解析上游返回数据".to_string())
}

fn convert_pricing_items_to_ratio_data(
    items: Vec<JsonValue>,
) -> Result<BTreeMap<String, JsonValue>, String> {
    let mut model_ratio = JsonMap::new();
    let mut completion_ratio = JsonMap::new();
    let mut cache_ratio = JsonMap::new();
    let mut create_cache_ratio = JsonMap::new();
    let mut image_ratio = JsonMap::new();
    let mut audio_ratio = JsonMap::new();
    let mut audio_completion_ratio = JsonMap::new();
    let mut model_price = JsonMap::new();
    let mut billing_mode = JsonMap::new();
    let mut billing_expr = JsonMap::new();

    for value in items {
        let JsonValue::Object(item) = value else {
            continue;
        };
        let Some(model_name) = item.get("model_name").and_then(JsonValue::as_str) else {
            continue;
        };
        if model_name.is_empty() {
            continue;
        }
        if item
            .get("billing_mode")
            .and_then(JsonValue::as_str)
            .is_some_and(|mode| mode == "tiered_expr")
            && item
                .get("billing_expr")
                .and_then(JsonValue::as_str)
                .is_some_and(|expr| !expr.trim().is_empty())
        {
            billing_mode.insert(model_name.to_string(), json!("tiered_expr"));
            billing_expr.insert(
                model_name.to_string(),
                item.get("billing_expr").cloned().unwrap_or(JsonValue::Null),
            );
        }

        let quota_type = item
            .get("quota_type")
            .and_then(JsonValue::as_i64)
            .unwrap_or_default();
        if quota_type == 1 {
            if let Some(value) = numeric_json_value(item.get("model_price")) {
                model_price.insert(model_name.to_string(), value);
            }
        } else {
            if let Some(value) = numeric_json_value(item.get("model_ratio")) {
                model_ratio.insert(model_name.to_string(), value);
            }
            if let Some(value) = numeric_json_value(item.get("completion_ratio")) {
                completion_ratio.insert(model_name.to_string(), value);
            }
        }

        insert_optional_number(&mut cache_ratio, model_name, &item, "cache_ratio");
        insert_optional_number(
            &mut create_cache_ratio,
            model_name,
            &item,
            "create_cache_ratio",
        );
        insert_optional_number(&mut image_ratio, model_name, &item, "image_ratio");
        insert_optional_number(&mut audio_ratio, model_name, &item, "audio_ratio");
        insert_optional_number(
            &mut audio_completion_ratio,
            model_name,
            &item,
            "audio_completion_ratio",
        );
    }

    let mut converted = BTreeMap::new();
    insert_object_if_nonempty(&mut converted, "model_ratio", model_ratio);
    insert_object_if_nonempty(&mut converted, "completion_ratio", completion_ratio);
    insert_object_if_nonempty(&mut converted, "cache_ratio", cache_ratio);
    insert_object_if_nonempty(&mut converted, "create_cache_ratio", create_cache_ratio);
    insert_object_if_nonempty(&mut converted, "image_ratio", image_ratio);
    insert_object_if_nonempty(&mut converted, "audio_ratio", audio_ratio);
    insert_object_if_nonempty(
        &mut converted,
        "audio_completion_ratio",
        audio_completion_ratio,
    );
    insert_object_if_nonempty(&mut converted, "model_price", model_price);
    insert_object_if_nonempty(&mut converted, "billing_mode", billing_mode);
    insert_object_if_nonempty(&mut converted, "billing_expr", billing_expr);
    Ok(converted)
}

fn convert_openrouter_to_ratio_data(body: &[u8]) -> Result<BTreeMap<String, JsonValue>, String> {
    #[derive(Deserialize)]
    struct OpenRouterResponse {
        data: Vec<OpenRouterModel>,
    }
    #[derive(Deserialize)]
    struct OpenRouterModel {
        id: String,
        pricing: OpenRouterPricing,
    }
    #[derive(Deserialize)]
    struct OpenRouterPricing {
        prompt: String,
        completion: String,
        #[serde(default)]
        input_cache_read: String,
    }

    let response = serde_json::from_slice::<OpenRouterResponse>(body)
        .map_err(|err| format!("failed to decode OpenRouter response: {err}"))?;
    let mut model_ratio = JsonMap::new();
    let mut completion_ratio = JsonMap::new();
    let mut cache_ratio = JsonMap::new();

    for model in response.data {
        let prompt_price = model.pricing.prompt.parse::<f64>().unwrap_or(0.0);
        let completion_price = model.pricing.completion.parse::<f64>().unwrap_or(0.0);
        if prompt_price < 0.0 || completion_price < 0.0 {
            continue;
        }
        if prompt_price == 0.0 && completion_price == 0.0 {
            model_ratio.insert(model.id, json!(0.0));
            continue;
        }
        if prompt_price <= 0.0 {
            continue;
        }
        model_ratio.insert(
            model.id.clone(),
            json_number(round_ratio_value(prompt_price * 1000.0 * USD_RATIO_BASE)),
        );
        completion_ratio.insert(
            model.id.clone(),
            json_number(round_ratio_value(completion_price / prompt_price)),
        );
        if let Ok(cache_price) = model.pricing.input_cache_read.parse::<f64>()
            && cache_price >= 0.0
        {
            cache_ratio.insert(
                model.id,
                json_number(round_ratio_value(cache_price / prompt_price)),
            );
        }
    }

    let mut converted = BTreeMap::new();
    insert_object_if_nonempty(&mut converted, "model_ratio", model_ratio);
    insert_object_if_nonempty(&mut converted, "completion_ratio", completion_ratio);
    insert_object_if_nonempty(&mut converted, "cache_ratio", cache_ratio);
    Ok(converted)
}

fn convert_models_dev_to_ratio_data(body: &[u8]) -> Result<BTreeMap<String, JsonValue>, String> {
    #[derive(Clone, Deserialize)]
    struct Provider {
        models: BTreeMap<String, Model>,
    }
    #[derive(Clone, Deserialize)]
    struct Model {
        cost: Cost,
    }
    #[derive(Clone, Deserialize)]
    struct Cost {
        input: Option<f64>,
        output: Option<f64>,
        cache_read: Option<f64>,
    }
    let providers = serde_json::from_slice::<BTreeMap<String, Provider>>(body)
        .map_err(|err| format!("failed to decode models.dev response: {err}"))?;
    if providers.is_empty() {
        return Err("empty models.dev response".to_string());
    }

    let mut selected = BTreeMap::<String, ModelsDevCandidate>::new();
    for (provider_name, provider) in providers {
        for (model_name, model) in provider.models {
            let Some(input) = model.cost.input else {
                continue;
            };
            if !is_valid_non_negative_cost(input)
                || model
                    .cost
                    .output
                    .is_some_and(|value| !is_valid_non_negative_cost(value))
                || model
                    .cost
                    .cache_read
                    .is_some_and(|value| !is_valid_non_negative_cost(value))
            {
                continue;
            }
            if input == 0.0 && model.cost.output.is_some_and(|value| value > 0.0) {
                continue;
            }
            let candidate = ModelsDevCandidate {
                provider: provider_name.clone(),
                input,
                output: model.cost.output,
                cache_read: model.cost.cache_read,
            };
            if selected
                .get(&model_name)
                .is_none_or(|current| should_replace_models_dev_candidate(current, &candidate))
            {
                selected.insert(model_name, candidate);
            }
        }
    }
    if selected.is_empty() {
        return Err("no valid models.dev pricing entries found".to_string());
    }

    let mut model_ratio = JsonMap::new();
    let mut completion_ratio = JsonMap::new();
    let mut cache_ratio = JsonMap::new();
    for (model_name, candidate) in selected {
        if candidate.input == 0.0 {
            model_ratio.insert(model_name, json!(0.0));
            continue;
        }
        model_ratio.insert(
            model_name.clone(),
            json_number(round_ratio_value(
                candidate.input * USD_RATIO_BASE / MODELS_DEV_INPUT_COST_RATIO_BASE,
            )),
        );
        if let Some(output) = candidate.output {
            completion_ratio.insert(
                model_name.clone(),
                json_number(round_ratio_value(output / candidate.input)),
            );
        }
        if let Some(cache_read) = candidate.cache_read {
            cache_ratio.insert(
                model_name,
                json_number(round_ratio_value(cache_read / candidate.input)),
            );
        }
    }

    let mut converted = BTreeMap::new();
    insert_object_if_nonempty(&mut converted, "model_ratio", model_ratio);
    insert_object_if_nonempty(&mut converted, "completion_ratio", completion_ratio);
    insert_object_if_nonempty(&mut converted, "cache_ratio", cache_ratio);
    Ok(converted)
}

fn local_pricing_sync_data(options: &BTreeMap<String, String>) -> BTreeMap<String, JsonValue> {
    [
        ("model_ratio", "ModelRatio"),
        ("completion_ratio", "CompletionRatio"),
        ("cache_ratio", "CacheRatio"),
        ("create_cache_ratio", "CreateCacheRatio"),
        ("image_ratio", "ImageRatio"),
        ("audio_ratio", "AudioRatio"),
        ("audio_completion_ratio", "AudioCompletionRatio"),
        ("model_price", "ModelPrice"),
        ("billing_mode", "billing_setting.billing_mode"),
        ("billing_expr", "billing_setting.billing_expr"),
    ]
    .into_iter()
    .map(|(field, option_key)| {
        (
            field.to_string(),
            options
                .get(option_key)
                .and_then(|raw| serde_json::from_str::<JsonValue>(raw).ok())
                .unwrap_or_else(|| json!({})),
        )
    })
    .collect()
}

fn build_differences(
    local_data: &BTreeMap<String, JsonValue>,
    successful_channels: &[(String, BTreeMap<String, JsonValue>)],
) -> BTreeMap<String, BTreeMap<String, DifferenceItem>> {
    let mut differences = BTreeMap::<String, BTreeMap<String, DifferenceItem>>::new();
    let mut all_models = BTreeSet::new();

    for field in PRICING_SYNC_FIELDS {
        all_models.extend(value_map(local_data.get(field)).keys().cloned());
    }
    for (_, data) in successful_channels {
        for field in PRICING_SYNC_FIELDS {
            all_models.extend(value_map(data.get(field)).keys().cloned());
        }
    }

    let mut confidence_map = BTreeMap::<String, BTreeMap<String, bool>>::new();
    for (channel_name, data) in successful_channels {
        let model_ratios = value_map(data.get("model_ratio"));
        let completion_ratios = value_map(data.get("completion_ratio"));
        let mut channel_confidence = BTreeMap::new();
        for model_name in &all_models {
            let trusted = if !model_ratios.is_empty() && !completion_ratios.is_empty() {
                let model_ratio = model_ratios.get(model_name).and_then(json_as_f64);
                let completion_ratio = completion_ratios.get(model_name).and_then(json_as_f64);
                !(model_ratio.is_some_and(|value| nearly_equal(value, 37.5))
                    && completion_ratio.is_some_and(|value| nearly_equal(value, 1.0)))
            } else {
                true
            };
            channel_confidence.insert(model_name.clone(), trusted);
        }
        confidence_map.insert(channel_name.clone(), channel_confidence);
    }

    for model_name in &all_models {
        for field in PRICING_SYNC_FIELDS {
            let local_value = value_map(local_data.get(field))
                .get(model_name)
                .map(|value| normalize_sync_value(field, value.clone()))
                .unwrap_or(JsonValue::Null);

            let mut upstream_values = BTreeMap::new();
            let mut confidence_values = BTreeMap::new();
            let mut has_upstream_value = false;
            let mut has_difference = false;

            for (channel_name, data) in successful_channels {
                let mut upstream_value = value_map(data.get(field))
                    .get(model_name)
                    .map(|value| normalize_sync_value(field, value.clone()))
                    .unwrap_or(JsonValue::Null);

                if !upstream_value.is_null() {
                    has_upstream_value = true;
                    if !local_value.is_null() && !values_equal(&local_value, &upstream_value) {
                        has_difference = true;
                    } else if values_equal(&local_value, &upstream_value) {
                        upstream_value = json!("same");
                    }
                }
                if upstream_value.is_null() && local_value.is_null() {
                    upstream_value = json!("same");
                }
                if local_value.is_null() && !upstream_value.is_null() && !is_same(&upstream_value) {
                    has_difference = true;
                }

                upstream_values.insert(channel_name.clone(), upstream_value);
                confidence_values.insert(
                    channel_name.clone(),
                    confidence_map
                        .get(channel_name)
                        .and_then(|values| values.get(model_name))
                        .copied()
                        .unwrap_or(true),
                );
            }

            let should_include = if !local_value.is_null() {
                has_difference
            } else {
                has_upstream_value
            };
            if should_include {
                differences.entry(model_name.clone()).or_default().insert(
                    field.to_string(),
                    DifferenceItem {
                        current: local_value,
                        upstreams: upstream_values,
                        confidence: confidence_values,
                    },
                );
            }
        }
    }

    let mut channel_has_diff = BTreeSet::new();
    for ratio_map in differences.values() {
        for item in ratio_map.values() {
            for (channel_name, value) in &item.upstreams {
                if !value.is_null() && !is_same(value) {
                    channel_has_diff.insert(channel_name.clone());
                }
            }
        }
    }

    differences.retain(|_, ratio_map| {
        ratio_map.retain(|_, item| {
            item.upstreams
                .retain(|channel_name, _| channel_has_diff.contains(channel_name));
            item.confidence
                .retain(|channel_name, _| channel_has_diff.contains(channel_name));
            !item.upstreams.is_empty() && item.upstreams.values().any(|value| !is_same(value))
        });
        !ratio_map.is_empty()
    });
    differences
}

fn value_map(value: Option<&JsonValue>) -> BTreeMap<String, JsonValue> {
    let Some(JsonValue::Object(object)) = value else {
        return BTreeMap::new();
    };
    object
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn normalize_sync_value(field: &str, value: JsonValue) -> JsonValue {
    if NUMERIC_PRICING_SYNC_FIELDS.contains(&field)
        && let Some(value) = json_as_f64(&value)
    {
        return json_number(value);
    }
    value
}

fn values_equal(left: &JsonValue, right: &JsonValue) -> bool {
    match (json_as_f64(left), json_as_f64(right)) {
        (Some(left), Some(right)) => nearly_equal(left, right),
        _ => left == right,
    }
}

fn nearly_equal(left: f64, right: f64) -> bool {
    (left - right).abs() < FLOAT_EPSILON
}

fn json_as_f64(value: &JsonValue) -> Option<f64> {
    match value {
        JsonValue::Number(number) => number.as_f64(),
        JsonValue::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn json_number(value: f64) -> JsonValue {
    JsonNumber::from_f64(value)
        .map(JsonValue::Number)
        .unwrap_or(JsonValue::Null)
}

fn numeric_json_value(value: Option<&JsonValue>) -> Option<JsonValue> {
    value.and_then(json_as_f64).map(json_number)
}

fn insert_optional_number(
    target: &mut JsonMap<String, JsonValue>,
    model_name: &str,
    item: &JsonMap<String, JsonValue>,
    key: &str,
) {
    if let Some(value) = numeric_json_value(item.get(key)) {
        target.insert(model_name.to_string(), value);
    }
}

fn insert_object_if_nonempty(
    target: &mut BTreeMap<String, JsonValue>,
    key: &str,
    value: JsonMap<String, JsonValue>,
) {
    if !value.is_empty() {
        target.insert(key.to_string(), JsonValue::Object(value));
    }
}

fn is_same(value: &JsonValue) -> bool {
    value.as_str() == Some("same")
}

fn round_ratio_value(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

fn is_models_dev_api_endpoint(raw_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(raw_url) else {
        return false;
    };
    url.host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("models.dev"))
        && {
            let path = url.path().trim_end_matches('/');
            path == "/api.json"
        }
}

fn is_valid_non_negative_cost(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

fn should_replace_models_dev_candidate(
    current: &ModelsDevCandidate,
    next: &ModelsDevCandidate,
) -> bool {
    let current_non_zero = current.input > 0.0;
    let next_non_zero = next.input > 0.0;
    if current_non_zero != next_non_zero {
        return next_non_zero;
    }
    if next_non_zero && !nearly_equal(next.input, current.input) {
        return next.input < current.input;
    }
    next.provider < current.provider
}

#[cfg(test)]
mod tests {
    use super::*;
    use halolake_domain::{ChannelRecord, STATUS_ENABLED};

    #[test]
    fn syncable_channels_include_configured_channels_and_presets() {
        let channels = syncable_channels(&ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            vec![ChannelRecord {
                id: 7,
                channel_type: 1,
                key: "sk-test".to_string(),
                status: STATUS_ENABLED,
                name: "openai-main".to_string(),
                base_url: Some("https://example.com".to_string()),
                ..test_channel_defaults()
            }],
            Vec::new(),
        ));

        assert_eq!(channels.len(), 3);
        assert_eq!(channels[0].id, 7);
        assert_eq!(channels[1].id, OFFICIAL_RATIO_PRESET_ID);
        assert_eq!(channels[2].id, MODELS_DEV_PRESET_ID);
    }

    #[test]
    fn converts_pricing_list_to_ratio_config_shape() {
        let body = br#"{
            "success": true,
            "data": [
                {
                    "model_name": "gpt-test",
                    "quota_type": 0,
                    "model_ratio": 2,
                    "completion_ratio": 3,
                    "cache_ratio": 0.5
                },
                {
                    "model_name": "image-test",
                    "quota_type": 1,
                    "model_price": 0.04
                }
            ]
        }"#;

        let data = parse_upstream_ratio_data(body, false, false).expect("converted");
        assert_eq!(data["model_ratio"]["gpt-test"], json!(2.0));
        assert_eq!(data["completion_ratio"]["gpt-test"], json!(3.0));
        assert_eq!(data["cache_ratio"]["gpt-test"], json!(0.5));
        assert_eq!(data["model_price"]["image-test"], json!(0.04));
    }

    #[test]
    fn differences_mark_equal_values_as_same_and_include_new_values() {
        let local = BTreeMap::from([
            ("model_ratio".to_string(), json!({"gpt-a": 1.0})),
            ("completion_ratio".to_string(), json!({})),
            ("cache_ratio".to_string(), json!({})),
            ("create_cache_ratio".to_string(), json!({})),
            ("image_ratio".to_string(), json!({})),
            ("audio_ratio".to_string(), json!({})),
            ("audio_completion_ratio".to_string(), json!({})),
            ("model_price".to_string(), json!({})),
            ("billing_mode".to_string(), json!({})),
            ("billing_expr".to_string(), json!({})),
        ]);
        let upstream = vec![(
            "upstream-a".to_string(),
            BTreeMap::from([(
                "model_ratio".to_string(),
                json!({"gpt-a": 1.0, "gpt-b": 2.0}),
            )]),
        )];

        let differences = build_differences(&local, &upstream);
        assert!(!differences.contains_key("gpt-a"));
        assert_eq!(
            differences["gpt-b"]["model_ratio"].upstreams["upstream-a"],
            json!(2.0)
        );
    }

    #[test]
    fn converts_openrouter_pricing_to_ratios() {
        let body = br#"{
            "data": [{
                "id": "openrouter/model",
                "pricing": {
                    "prompt": "0.000001",
                    "completion": "0.000002",
                    "input_cache_read": "0.0000005"
                }
            }]
        }"#;

        let data = convert_openrouter_to_ratio_data(body).expect("converted");
        assert_eq!(data["model_ratio"]["openrouter/model"], json!(0.5));
        assert_eq!(data["completion_ratio"]["openrouter/model"], json!(2.0));
        assert_eq!(data["cache_ratio"]["openrouter/model"], json!(0.5));
    }

    fn test_channel_defaults() -> ChannelRecord {
        ChannelRecord {
            id: 0,
            snapshot_id: None,
            channel_type: 1,
            key: String::new(),
            status: STATUS_ENABLED,
            name: String::new(),
            weight: None,
            created_time: 0,
            test_time: 0,
            response_time: 0,
            base_url: None,
            balance: 0.0,
            balance_updated_time: 0,
            models: String::new(),
            group: "default".to_string(),
            used_quota: 0,
            model_mapping: None,
            priority: None,
            auto_ban: None,
            tag: None,
            setting: None,
            param_override: None,
            header_override: None,
            remark: None,
            proxy_id: None,
        }
    }
}
