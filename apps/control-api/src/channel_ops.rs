use crate::{
    channel_probe::{ChannelProbeService, FetchModelsRequest},
    storage::ManagementStore,
};
use halolake_control_plane::{CreateChannelRequest, ManagementError, UpdateChannelRequest};
use halolake_domain::{
    CHANNEL_TYPE_ANTHROPIC, CHANNEL_TYPE_GEMINI, CHANNEL_TYPE_OPENAI, ChannelRecord, STATUS_ENABLED,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use service_async::Service;
use std::{
    collections::{BTreeMap, HashSet},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const CHANNEL_TYPE_MIDJOURNEY: i32 = 2;
const CHANNEL_TYPE_OLLAMA: i32 = 4;
const CHANNEL_TYPE_MIDJOURNEY_PLUS: i32 = 5;
const CHANNEL_TYPE_CUSTOM: i32 = 8;
const CHANNEL_TYPE_AI_PROXY: i32 = 10;
const CHANNEL_TYPE_API2GPT: i32 = 12;
const CHANNEL_TYPE_AIGC2D: i32 = 13;
const CHANNEL_TYPE_ALI: i32 = 17;
const CHANNEL_TYPE_OPENROUTER: i32 = 20;
const CHANNEL_TYPE_MOONSHOT: i32 = 25;
const CHANNEL_TYPE_ZHIPU_V4: i32 = 26;
const CHANNEL_TYPE_SUNO_API: i32 = 36;
const CHANNEL_TYPE_SILICON_FLOW: i32 = 40;
const CHANNEL_TYPE_DEEPSEEK: i32 = 43;
const CHANNEL_TYPE_MOKA_AI: i32 = 44;
const CHANNEL_TYPE_VOLC_ENGINE: i32 = 45;
const CHANNEL_TYPE_XAI: i32 = 48;
const CHANNEL_TYPE_KLING: i32 = 50;
const CHANNEL_TYPE_JIMENG: i32 = 51;
const CHANNEL_TYPE_VIDU: i32 = 52;
const CHANNEL_TYPE_DOUBAO_VIDEO: i32 = 54;
const CHANNEL_TYPE_CODEX: i32 = 57;

const ENDPOINT_OPENAI: &str = "openai";
const ENDPOINT_OPENAI_RESPONSE: &str = "openai-response";
const ENDPOINT_OPENAI_RESPONSE_COMPACT: &str = "openai-response-compact";
const ENDPOINT_ANTHROPIC: &str = "anthropic";
const ENDPOINT_GEMINI: &str = "gemini";
const ENDPOINT_JINA_RERANK: &str = "jina-rerank";
const ENDPOINT_IMAGE_GENERATION: &str = "image-generation";
const ENDPOINT_EMBEDDINGS: &str = "embeddings";

#[allow(async_fn_in_trait)]
pub(crate) trait ChannelOpsProgress {
    async fn report(&mut self, processed: usize, total: usize) -> Result<(), ManagementError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NoopChannelOpsProgress;

impl ChannelOpsProgress for NoopChannelOpsProgress {
    async fn report(&mut self, _processed: usize, _total: usize) -> Result<(), ManagementError> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChannelOpsService {
    management: ManagementStore,
    client:     reqwest::Client,
}

impl ChannelOpsService {
    pub(crate) fn new(management: ManagementStore) -> Self {
        Self {
            management,
            client: reqwest::Client::new(),
        }
    }

    fn full_channel(&self, id: u64) -> Result<ChannelRecord, ManagementError> {
        self.management
            .current_data()?
            .channels
            .into_iter()
            .find(|channel| channel.id == id)
            .ok_or(ManagementError::NotFound)
    }

    async fn update_channel(
        &self,
        channel: ChannelRecord,
    ) -> Result<ChannelRecord, ManagementError> {
        self.management.call(UpdateChannelRequest { channel }).await
    }

    async fn create_channel(
        &self,
        channel: ChannelRecord,
    ) -> Result<ChannelRecord, ManagementError> {
        self.management.call(CreateChannelRequest { channel }).await
    }

    async fn update_channel_balance(
        &self,
        mut channel: ChannelRecord,
        price: f64,
        disable_empty: bool,
    ) -> Result<f64, ManagementError> {
        if is_multi_key(&channel.key) {
            return Err(ManagementError::InvalidRequest(
                "multi-key channel balance query is not supported",
            ));
        }
        let balance = self.query_channel_balance(&channel, price).await?;
        channel.balance = balance;
        channel.balance_updated_time = now_unix();
        if disable_empty && balance <= 0.0 && channel.auto_ban.unwrap_or(1) != 0 {
            channel.status = 0;
        }
        self.update_channel(channel).await?;
        Ok(balance)
    }

    async fn query_channel_balance(
        &self,
        channel: &ChannelRecord,
        price: f64,
    ) -> Result<f64, ManagementError> {
        let key = first_key(&channel.key);
        if key.is_empty() {
            return Err(ManagementError::InvalidRequest("channel key is required"));
        }
        match channel.channel_type {
            CHANNEL_TYPE_OPENAI | CHANNEL_TYPE_CUSTOM => {
                self.query_openai_balance(channel, &key).await
            }
            CHANNEL_TYPE_AI_PROXY => self.query_aiproxy_balance(&key).await,
            CHANNEL_TYPE_API2GPT => {
                self.query_credit_grants_balance(
                    "https://api.api2gpt.com/dashboard/billing/credit_grants",
                    &key,
                    "total_remaining",
                )
                .await
            }
            CHANNEL_TYPE_AIGC2D => {
                self.query_credit_grants_balance(
                    "https://api.aigc2d.com/dashboard/billing/credit_grants",
                    &key,
                    "total_available",
                )
                .await
            }
            CHANNEL_TYPE_SILICON_FLOW => self.query_siliconflow_balance(&key).await,
            CHANNEL_TYPE_DEEPSEEK => self.query_deepseek_balance(&key).await,
            CHANNEL_TYPE_OPENROUTER => self.query_openrouter_balance(&key).await,
            CHANNEL_TYPE_MOONSHOT => self.query_moonshot_balance(&key, price).await,
            _ => Err(ManagementError::InvalidRequest("尚未实现")),
        }
    }

    async fn query_openai_balance(
        &self,
        channel: &ChannelRecord,
        key: &str,
    ) -> Result<f64, ManagementError> {
        let base_url = channel_base_url(channel);
        let subscription = self
            .get_json_bearer(
                &format!("{}/v1/dashboard/billing/subscription", base_url),
                key,
            )
            .await?;
        let has_payment_method =
            json_bool(&subscription, &["has_payment_method"]).unwrap_or_default();
        let hard_limit = json_f64(&subscription, &["hard_limit_usd"])
            .or_else(|| json_f64(&subscription, &["system_hard_limit_usd"]))
            .unwrap_or_default();

        let today_days = unix_days();
        let (_, _, day) = civil_from_days(today_days);
        let start_days = if has_payment_method {
            today_days.saturating_sub(i64::from(day.saturating_sub(1)))
        } else {
            today_days.saturating_sub(100)
        };
        let start_date = ymd_string_from_days(start_days);
        let end_date = ymd_string_from_days(today_days);
        let usage = self
            .get_json_bearer(
                &format!(
                    "{}/v1/dashboard/billing/usage?start_date={start_date}&end_date={end_date}",
                    base_url
                ),
                key,
            )
            .await?;
        let total_usage = json_f64(&usage, &["total_usage"]).unwrap_or_default();
        Ok(hard_limit - total_usage / 100.0)
    }

    async fn query_aiproxy_balance(&self, key: &str) -> Result<f64, ManagementError> {
        let value = self
            .client
            .get("https://aiproxy.io/api/report/getUserOverview")
            .header("Api-Key", key)
            .send()
            .await
            .map_err(storage_err)?;
        let value = response_json(value).await?;
        if !json_bool(&value, &["success"]).unwrap_or_default() {
            let code = json_i64(&value, &["error_code"]).unwrap_or_default();
            let message = json_str(&value, &["message"]).unwrap_or_default();
            return Err(ManagementError::Storage(format!(
                "code: {code}, message: {message}"
            )));
        }
        json_f64(&value, &["data", "totalPoints"])
            .ok_or_else(|| ManagementError::Storage("missing totalPoints".to_string()))
    }

    async fn query_credit_grants_balance(
        &self,
        url: &str,
        key: &str,
        field: &str,
    ) -> Result<f64, ManagementError> {
        let value = self.get_json_bearer(url, key).await?;
        json_f64(&value, &[field])
            .ok_or_else(|| ManagementError::Storage(format!("missing {field}")))
    }

    async fn query_siliconflow_balance(&self, key: &str) -> Result<f64, ManagementError> {
        let value = self
            .get_json_bearer("https://api.siliconflow.cn/v1/user/info", key)
            .await?;
        if json_i64(&value, &["code"]) != Some(20000) {
            return Err(ManagementError::Storage(format!(
                "code: {}, message: {}",
                json_i64(&value, &["code"]).unwrap_or_default(),
                json_str(&value, &["message"]).unwrap_or_default()
            )));
        }
        json_f64(&value, &["data", "totalBalance"])
            .ok_or_else(|| ManagementError::Storage("missing totalBalance".to_string()))
    }

    async fn query_deepseek_balance(&self, key: &str) -> Result<f64, ManagementError> {
        let value = self
            .get_json_bearer("https://api.deepseek.com/user/balance", key)
            .await?;
        let Some(items) = value.get("balance_infos").and_then(JsonValue::as_array) else {
            return Err(ManagementError::Storage(
                "missing balance_infos".to_string(),
            ));
        };
        items
            .iter()
            .find(|item| json_str(item, &["currency"]).as_deref() == Some("CNY"))
            .and_then(|item| json_f64(item, &["total_balance"]))
            .ok_or_else(|| ManagementError::Storage("currency CNY not found".to_string()))
    }

    async fn query_openrouter_balance(&self, key: &str) -> Result<f64, ManagementError> {
        let value = self
            .get_json_bearer("https://openrouter.ai/api/v1/credits", key)
            .await?;
        let total_credits = json_f64(&value, &["data", "total_credits"]).unwrap_or_default();
        let total_usage = json_f64(&value, &["data", "total_usage"]).unwrap_or_default();
        Ok(total_credits - total_usage)
    }

    async fn query_moonshot_balance(&self, key: &str, price: f64) -> Result<f64, ManagementError> {
        let value = self
            .get_json_bearer("https://api.moonshot.cn/v1/users/me/balance", key)
            .await?;
        let status = json_bool(&value, &["status"]).unwrap_or_default();
        let code = json_i64(&value, &["code"]).unwrap_or_default();
        if !status || code != 0 {
            return Err(ManagementError::Storage(format!(
                "failed to update moonshot balance, status: {status}, code: {code}, scode: {}",
                json_str(&value, &["scode"]).unwrap_or_default()
            )));
        }
        let cny = json_f64(&value, &["data", "available_balance"])
            .ok_or_else(|| ManagementError::Storage("missing available_balance".to_string()))?;
        if price <= 0.0 {
            return Ok(cny);
        }
        Ok(cny / price)
    }

    async fn get_json_bearer(&self, url: &str, key: &str) -> Result<JsonValue, ManagementError> {
        let response = self
            .client
            .get(url)
            .bearer_auth(key)
            .send()
            .await
            .map_err(storage_err)?;
        response_json(response).await
    }

    async fn test_channel(
        &self,
        mut channel: ChannelRecord,
        req: TestChannelRequest,
    ) -> Result<TestChannelResponse, ManagementError> {
        if unsupported_channel_test_type(channel.channel_type) {
            return Err(ManagementError::Storage(format!(
                "{} channel test is not supported",
                channel_type_name(channel.channel_type)
            )));
        }
        let model = select_test_model(&channel, &req.model);
        let endpoint = normalize_test_endpoint(&channel, &model, &req.endpoint_type);
        let started = Instant::now();
        self.send_test_request(&channel, &model, &endpoint, req.stream)
            .await?;
        let elapsed = started.elapsed();
        channel.response_time = elapsed.as_millis().min(i32::MAX as u128) as i32;
        channel.test_time = now_unix();
        self.update_channel(channel).await?;
        Ok(TestChannelResponse {
            time: elapsed.as_secs_f64(),
        })
    }

    async fn send_test_request(
        &self,
        channel: &ChannelRecord,
        model: &str,
        endpoint: &str,
        stream: bool,
    ) -> Result<(), ManagementError> {
        let key = first_key(&channel.key);
        let base_url = channel_base_url(channel);
        let (url, body, auth) =
            build_test_http_request(channel, &base_url, &key, model, endpoint, stream);
        let request_body = serde_json::to_vec(&body).map_err(storage_err)?;
        let client = http_client_for_channel(channel)?;
        let mut request = client
            .post(url)
            .header("content-type", "application/json")
            .body(request_body);
        match auth {
            TestAuth::Bearer => {
                request = request.bearer_auth(&key);
            }
            TestAuth::Claude => {
                request = request
                    .header("x-api-key", &key)
                    .header("anthropic-version", "2023-06-01");
            }
            TestAuth::GeminiQuery => {}
        }
        let response = request.send().await.map_err(storage_err)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(storage_err)?;
        if !status.is_success() {
            let message = detect_error_message(&bytes).unwrap_or_else(|| status.to_string());
            return Err(ManagementError::Storage(format!(
                "upstream error: {message}"
            )));
        }
        if let Some(message) = detect_error_message(&bytes) {
            return Err(ManagementError::Storage(format!(
                "upstream error: {message}"
            )));
        }
        if stream && !stream_body_has_event(&bytes) {
            return Err(ManagementError::Storage(
                "stream response body does not contain a valid stream event".to_string(),
            ));
        }
        Ok(())
    }

    async fn detect_upstream_updates(
        &self,
        channel_id: u64,
        allow_auto_apply: bool,
    ) -> Result<DetectChannelUpstreamModelUpdatesResponse, ManagementError> {
        let mut channel = self.full_channel(channel_id)?;
        let mut settings = channel_settings(&channel);
        let upstream_models = ChannelProbeService::new(self.management.clone())
            .call(FetchModelsRequest {
                channel_id:   Some(channel_id),
                base_url:     String::new(),
                channel_type: CHANNEL_TYPE_OPENAI,
                key:          String::new(),
            })
            .await?;
        let (pending_add, pending_remove) = collect_pending_upstream_model_changes(
            channel.model_list(),
            upstream_models,
            &settings.upstream_model_update_ignored_models,
            normalize_channel_model_mapping(&channel),
        );
        let mut auto_added_models = 0usize;
        if allow_auto_apply
            && settings.upstream_model_update_auto_sync_enabled
            && !pending_add.is_empty()
        {
            let origin_models = normalize_model_names(channel.model_list());
            let merged_models = merge_model_names(origin_models.clone(), pending_add);
            auto_added_models = merged_models.len().saturating_sub(origin_models.len());
            channel.models = merged_models.join(",");
            settings.upstream_model_update_last_detected_models = Vec::new();
        } else {
            settings.upstream_model_update_last_detected_models = pending_add;
        }
        settings.upstream_model_update_last_removed_models = pending_remove;
        settings.upstream_model_update_last_check_time = now_unix();
        channel.setting = Some(serde_json::to_string(&settings).map_err(storage_err)?);
        self.update_channel(channel.clone()).await?;
        Ok(DetectChannelUpstreamModelUpdatesResponse {
            channel_id: channel.id,
            channel_name: channel.name,
            add_models: settings.upstream_model_update_last_detected_models,
            remove_models: settings.upstream_model_update_last_removed_models,
            last_check_time: settings.upstream_model_update_last_check_time,
            auto_added_models,
        })
    }

    pub(crate) async fn test_all_channels_with_progress<P>(
        &self,
        req: TestAllChannelsRequest,
        progress: &mut P,
    ) -> Result<TestAllChannelsResponse, ManagementError>
    where
        P: ChannelOpsProgress,
    {
        let channels = self.management.current_data()?.channels;
        let total = channels
            .iter()
            .filter(|channel| {
                channel.status == STATUS_ENABLED
                    && !unsupported_channel_test_type(channel.channel_type)
            })
            .count();
        let mut processed = 0usize;
        let mut response = TestAllChannelsResponse::default();
        progress.report(0, total).await?;
        for channel in channels {
            if channel.status != STATUS_ENABLED
                || unsupported_channel_test_type(channel.channel_type)
            {
                continue;
            }
            let test_req = TestChannelRequest {
                id:            channel.id,
                model:         String::new(),
                endpoint_type: String::new(),
                stream:        req.stream || channel.channel_type == CHANNEL_TYPE_CODEX,
            };
            response.tested = response.tested.saturating_add(1);
            match self.test_channel(channel, test_req).await {
                Ok(_) => response.succeeded = response.succeeded.saturating_add(1),
                Err(_) => response.failed = response.failed.saturating_add(1),
            }
            processed = processed.saturating_add(1);
            progress.report(processed, total).await?;
        }
        if processed != total {
            progress.report(total, total).await?;
        }
        Ok(response)
    }

    pub(crate) async fn detect_all_upstream_model_updates_with_progress<P>(
        &self,
        _req: DetectAllChannelUpstreamModelUpdatesRequest,
        progress: &mut P,
    ) -> Result<DetectAllChannelUpstreamModelUpdatesResponse, ManagementError>
    where
        P: ChannelOpsProgress,
    {
        let channels = self.management.current_data()?.channels;
        let total = channels
            .iter()
            .filter(|channel| channel.status == STATUS_ENABLED)
            .count();
        let mut processed = 0usize;
        let mut response = DetectAllChannelUpstreamModelUpdatesResponse::default();
        progress.report(0, total).await?;
        for channel in channels {
            if channel.status != STATUS_ENABLED {
                continue;
            }
            processed = processed.saturating_add(1);
            progress.report(processed, total).await?;

            let settings = channel_settings(&channel);
            if !settings.upstream_model_update_check_enabled {
                continue;
            }
            response.checked_channels = response.checked_channels.saturating_add(1);
            match self.detect_upstream_updates(channel.id, false).await {
                Ok(result) => {
                    let add = result.add_models.len();
                    let remove = result.remove_models.len();
                    response.detected_add_models = response.detected_add_models.saturating_add(add);
                    response.detected_remove_models =
                        response.detected_remove_models.saturating_add(remove);
                    response.auto_added_models = response
                        .auto_added_models
                        .saturating_add(result.auto_added_models);
                    if add > 0 || remove > 0 {
                        response.changed_channels = response.changed_channels.saturating_add(1);
                    }
                }
                Err(_) => response.failed_channels = response.failed_channels.saturating_add(1),
            }
        }
        if processed != total {
            progress.report(total, total).await?;
        }
        Ok(response)
    }

    async fn apply_upstream_updates(
        &self,
        req: ApplyChannelUpstreamModelUpdatesRequest,
    ) -> Result<ApplyChannelUpstreamModelUpdatesResponse, ManagementError> {
        if req.id == 0 {
            return Err(ManagementError::InvalidRequest("invalid channel id"));
        }
        let mut channel = self.full_channel(req.id)?;
        let before_settings = channel_settings(&channel);
        let ignored_models = intersect_model_names(
            req.ignore_models.clone(),
            before_settings
                .upstream_model_update_last_detected_models
                .clone(),
        );
        let (added_models, removed_models, remaining_models, remaining_remove_models) =
            apply_channel_upstream_model_updates(&mut channel, req)?;
        let settings = channel_settings(&channel);
        self.update_channel(channel.clone()).await?;
        Ok(ApplyChannelUpstreamModelUpdatesResponse {
            id: channel.id,
            added_models,
            removed_models,
            ignored_models,
            remaining_models,
            remaining_remove_models,
            models: channel.models,
            settings,
        })
    }
}

impl Service<CopyChannelRequest> for ChannelOpsService {
    type Response = CopyChannelResponse;
    type Error = ManagementError;

    async fn call(&self, req: CopyChannelRequest) -> Result<Self::Response, Self::Error> {
        let mut clone = self.full_channel(req.id)?;
        clone.id = 0;
        clone.snapshot_id = None;
        clone.created_time = now_unix();
        clone.name.push_str(&req.suffix);
        clone.test_time = 0;
        clone.response_time = 0;
        if req.reset_balance {
            clone.balance = 0.0;
            clone.balance_updated_time = 0;
            clone.used_quota = 0;
        }
        let created = self.create_channel(clone).await?;
        Ok(CopyChannelResponse { id: created.id })
    }
}

impl Service<UpdateChannelBalanceRequest> for ChannelOpsService {
    type Response = f64;
    type Error = ManagementError;

    async fn call(&self, req: UpdateChannelBalanceRequest) -> Result<Self::Response, Self::Error> {
        let channel = self.full_channel(req.id)?;
        self.update_channel_balance(channel, req.price, false).await
    }
}

impl Service<UpdateAllChannelBalancesRequest> for ChannelOpsService {
    type Response = UpdateAllChannelBalancesResponse;
    type Error = ManagementError;

    async fn call(
        &self,
        req: UpdateAllChannelBalancesRequest,
    ) -> Result<Self::Response, Self::Error> {
        let channels = self.management.current_data()?.channels;
        let mut response = UpdateAllChannelBalancesResponse::default();
        for channel in channels {
            if channel.status != STATUS_ENABLED || is_multi_key(&channel.key) {
                continue;
            }
            match self.update_channel_balance(channel, req.price, true).await {
                Ok(balance) => {
                    response.updated = response.updated.saturating_add(1);
                    if balance <= 0.0 {
                        response.disabled = response.disabled.saturating_add(1);
                    }
                }
                Err(_) => {
                    response.failed = response.failed.saturating_add(1);
                }
            }
        }
        Ok(response)
    }
}

impl Service<TestChannelRequest> for ChannelOpsService {
    type Response = TestChannelResponse;
    type Error = ManagementError;

    async fn call(&self, req: TestChannelRequest) -> Result<Self::Response, Self::Error> {
        let channel = self.full_channel(req.id)?;
        self.test_channel(channel, req).await
    }
}

impl Service<TestAllChannelsRequest> for ChannelOpsService {
    type Response = TestAllChannelsResponse;
    type Error = ManagementError;

    async fn call(&self, req: TestAllChannelsRequest) -> Result<Self::Response, Self::Error> {
        self.test_all_channels_with_progress(req, &mut NoopChannelOpsProgress)
            .await
    }
}

impl Service<FixChannelAbilitiesRequest> for ChannelOpsService {
    type Response = FixChannelAbilitiesResponse;
    type Error = ManagementError;

    async fn call(&self, _req: FixChannelAbilitiesRequest) -> Result<Self::Response, Self::Error> {
        let data = self.management.current_data()?;
        let mut success = 0usize;
        let mut fails = 0usize;
        for channel in data.channels {
            if !snapshot_supported_channel_type(channel.channel_type) {
                continue;
            }
            if normalize_channel_model_mapping_result(&channel).is_err() {
                fails = fails.saturating_add(1);
                continue;
            }
            success = success.saturating_add(1);
        }
        Ok(FixChannelAbilitiesResponse { success, fails })
    }
}

impl Service<DetectChannelUpstreamModelUpdatesRequest> for ChannelOpsService {
    type Response = DetectChannelUpstreamModelUpdatesResponse;
    type Error = ManagementError;

    async fn call(
        &self,
        req: DetectChannelUpstreamModelUpdatesRequest,
    ) -> Result<Self::Response, Self::Error> {
        self.detect_upstream_updates(req.id, false).await
    }
}

impl Service<ApplyChannelUpstreamModelUpdatesRequest> for ChannelOpsService {
    type Response = ApplyChannelUpstreamModelUpdatesResponse;
    type Error = ManagementError;

    async fn call(
        &self,
        req: ApplyChannelUpstreamModelUpdatesRequest,
    ) -> Result<Self::Response, Self::Error> {
        self.apply_upstream_updates(req).await
    }
}

impl Service<DetectAllChannelUpstreamModelUpdatesRequest> for ChannelOpsService {
    type Response = DetectAllChannelUpstreamModelUpdatesResponse;
    type Error = ManagementError;

    async fn call(
        &self,
        req: DetectAllChannelUpstreamModelUpdatesRequest,
    ) -> Result<Self::Response, Self::Error> {
        self.detect_all_upstream_model_updates_with_progress(req, &mut NoopChannelOpsProgress)
            .await
    }
}

impl Service<ApplyAllChannelUpstreamModelUpdatesRequest> for ChannelOpsService {
    type Response = ApplyAllChannelUpstreamModelUpdatesResponse;
    type Error = ManagementError;

    async fn call(
        &self,
        _req: ApplyAllChannelUpstreamModelUpdatesRequest,
    ) -> Result<Self::Response, Self::Error> {
        let channels = self.management.current_data()?.channels;
        let mut response = ApplyAllChannelUpstreamModelUpdatesResponse::default();
        for channel in channels {
            if channel.status != STATUS_ENABLED {
                continue;
            }
            let settings = channel_settings(&channel);
            if !settings.upstream_model_update_check_enabled
                || (settings
                    .upstream_model_update_last_detected_models
                    .is_empty()
                    && settings
                        .upstream_model_update_last_removed_models
                        .is_empty())
            {
                continue;
            }
            let req = ApplyChannelUpstreamModelUpdatesRequest {
                id:            channel.id,
                add_models:    settings.upstream_model_update_last_detected_models,
                remove_models: settings.upstream_model_update_last_removed_models,
                ignore_models: Vec::new(),
            };
            match self.apply_upstream_updates(req).await {
                Ok(result) => {
                    response.processed_channels = response.processed_channels.saturating_add(1);
                    response.added_models = response
                        .added_models
                        .saturating_add(result.added_models.len());
                    response.removed_models = response
                        .removed_models
                        .saturating_add(result.removed_models.len());
                    response.results.push(ApplyAllChannelResult {
                        channel_id:              result.id,
                        channel_name:            channel.name,
                        added_models:            result.added_models,
                        removed_models:          result.removed_models,
                        remaining_models:        result.remaining_models,
                        remaining_remove_models: result.remaining_remove_models,
                    });
                }
                Err(_) => response.failed_channel_ids.push(channel.id),
            }
        }
        Ok(response)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CopyChannelRequest {
    pub(crate) id:            u64,
    pub(crate) suffix:        String,
    pub(crate) reset_balance: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CopyChannelResponse {
    pub(crate) id: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct UpdateChannelBalanceRequest {
    pub(crate) id:    u64,
    pub(crate) price: f64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct UpdateAllChannelBalancesRequest {
    pub(crate) price: f64,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub(crate) struct UpdateAllChannelBalancesResponse {
    pub(crate) updated:  usize,
    pub(crate) failed:   usize,
    pub(crate) disabled: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct TestChannelRequest {
    pub(crate) id:            u64,
    pub(crate) model:         String,
    pub(crate) endpoint_type: String,
    pub(crate) stream:        bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct TestChannelResponse {
    pub(crate) time: f64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TestAllChannelsRequest {
    pub(crate) stream: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub(crate) struct TestAllChannelsResponse {
    pub(crate) tested:    usize,
    pub(crate) succeeded: usize,
    pub(crate) failed:    usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FixChannelAbilitiesRequest;

#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct FixChannelAbilitiesResponse {
    pub(crate) success: usize,
    pub(crate) fails:   usize,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct DetectChannelUpstreamModelUpdatesRequest {
    pub(crate) id: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DetectAllChannelUpstreamModelUpdatesRequest;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ApplyChannelUpstreamModelUpdatesRequest {
    pub(crate) id:            u64,
    #[serde(default)]
    pub(crate) add_models:    Vec<String>,
    #[serde(default)]
    pub(crate) remove_models: Vec<String>,
    #[serde(default)]
    pub(crate) ignore_models: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ApplyAllChannelUpstreamModelUpdatesRequest;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DetectChannelUpstreamModelUpdatesResponse {
    pub(crate) channel_id:        u64,
    pub(crate) channel_name:      String,
    pub(crate) add_models:        Vec<String>,
    pub(crate) remove_models:     Vec<String>,
    pub(crate) last_check_time:   i64,
    pub(crate) auto_added_models: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct DetectAllChannelUpstreamModelUpdatesResponse {
    pub(crate) checked_channels:       usize,
    pub(crate) changed_channels:       usize,
    pub(crate) detected_add_models:    usize,
    pub(crate) detected_remove_models: usize,
    pub(crate) failed_channels:        usize,
    pub(crate) auto_added_models:      usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ApplyChannelUpstreamModelUpdatesResponse {
    pub(crate) id: u64,
    pub(crate) added_models: Vec<String>,
    pub(crate) removed_models: Vec<String>,
    pub(crate) ignored_models: Vec<String>,
    pub(crate) remaining_models: Vec<String>,
    pub(crate) remaining_remove_models: Vec<String>,
    pub(crate) models: String,
    pub(crate) settings: ChannelOtherSettings,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct ApplyAllChannelUpstreamModelUpdatesResponse {
    pub(crate) processed_channels: usize,
    pub(crate) added_models:       usize,
    pub(crate) removed_models:     usize,
    pub(crate) failed_channel_ids: Vec<u64>,
    pub(crate) results:            Vec<ApplyAllChannelResult>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ApplyAllChannelResult {
    pub(crate) channel_id:              u64,
    pub(crate) channel_name:            String,
    pub(crate) added_models:            Vec<String>,
    pub(crate) removed_models:          Vec<String>,
    pub(crate) remaining_models:        Vec<String>,
    pub(crate) remaining_remove_models: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ChannelOtherSettings {
    #[serde(default)]
    pub(crate) upstream_model_update_check_enabled: bool,
    #[serde(default)]
    pub(crate) upstream_model_update_auto_sync_enabled: bool,
    #[serde(default)]
    pub(crate) upstream_model_update_last_check_time: i64,
    #[serde(default)]
    pub(crate) upstream_model_update_last_detected_models: Vec<String>,
    #[serde(default)]
    pub(crate) upstream_model_update_last_removed_models: Vec<String>,
    #[serde(default)]
    pub(crate) upstream_model_update_ignored_models: Vec<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct CopyChannelQuery {
    #[serde(default = "default_copy_suffix")]
    pub(crate) suffix:        String,
    #[serde(default = "default_true")]
    pub(crate) reset_balance: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ChannelTestQuery {
    #[serde(default)]
    pub(crate) model:         String,
    #[serde(default)]
    pub(crate) endpoint_type: String,
    #[serde(default)]
    pub(crate) stream:        bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(crate) struct ChannelTestAllQuery {
    #[serde(default)]
    pub(crate) stream: bool,
}

#[derive(Debug, Clone, Copy)]
enum TestAuth {
    Bearer,
    Claude,
    GeminiQuery,
}

fn apply_channel_upstream_model_updates(
    channel: &mut ChannelRecord,
    req: ApplyChannelUpstreamModelUpdatesRequest,
) -> Result<(Vec<String>, Vec<String>, Vec<String>, Vec<String>), ManagementError> {
    let mut settings = channel_settings(channel);
    let pending_add = normalize_model_names(settings.upstream_model_update_last_detected_models);
    let pending_remove = normalize_model_names(settings.upstream_model_update_last_removed_models);
    let add_models = intersect_model_names(req.add_models, pending_add.clone());
    let ignore_models = intersect_model_names(req.ignore_models, pending_add.clone());
    let mut remove_models = intersect_model_names(req.remove_models, pending_remove.clone());
    remove_models = subtract_model_names(remove_models, add_models.clone());

    let origin_models = normalize_model_names(channel.model_list());
    let next_models = apply_selected_model_changes(
        origin_models.clone(),
        add_models.clone(),
        remove_models.clone(),
    );
    if origin_models != next_models {
        channel.models = next_models.join(",");
    }

    settings.upstream_model_update_ignored_models = merge_model_names(
        settings.upstream_model_update_ignored_models,
        ignore_models.clone(),
    );
    if !add_models.is_empty() {
        settings.upstream_model_update_ignored_models = subtract_model_names(
            settings.upstream_model_update_ignored_models,
            add_models.clone(),
        );
    }
    let consumed_add = merge_model_names(add_models.clone(), ignore_models);
    let remaining_models = subtract_model_names(pending_add, consumed_add);
    let remaining_remove_models = subtract_model_names(pending_remove, remove_models.clone());
    settings.upstream_model_update_last_detected_models = remaining_models.clone();
    settings.upstream_model_update_last_removed_models = remaining_remove_models.clone();
    settings.upstream_model_update_last_check_time = now_unix();
    channel.setting = Some(serde_json::to_string(&settings).map_err(storage_err)?);
    Ok((
        add_models,
        remove_models,
        remaining_models,
        remaining_remove_models,
    ))
}

fn collect_pending_upstream_model_changes(
    local_models: Vec<String>,
    upstream_models: Vec<String>,
    ignored_models: &[String],
    model_mapping: BTreeMap<String, String>,
) -> (Vec<String>, Vec<String>) {
    let local_models = normalize_model_names(local_models);
    let upstream_models = normalize_model_names(upstream_models);
    let local_set = local_models.iter().cloned().collect::<HashSet<_>>();
    let upstream_set = upstream_models.iter().cloned().collect::<HashSet<_>>();
    let redirect_source_set = model_mapping.keys().cloned().collect::<HashSet<_>>();
    let redirect_target_set = model_mapping.values().cloned().collect::<HashSet<_>>();
    let mut covered_upstream_set = local_set.clone();
    covered_upstream_set.extend(redirect_target_set);

    let pending_add = upstream_models
        .into_iter()
        .filter(|model| {
            !covered_upstream_set.contains(model) && !ignored_model_matches(ignored_models, model)
        })
        .collect::<Vec<_>>();
    let pending_remove = local_models
        .into_iter()
        .filter(|model| !redirect_source_set.contains(model) && !upstream_set.contains(model))
        .collect::<Vec<_>>();
    (
        normalize_model_names(pending_add),
        normalize_model_names(pending_remove),
    )
}

fn ignored_model_matches(ignored_models: &[String], model: &str) -> bool {
    ignored_models.iter().any(|ignored| {
        let ignored = ignored.trim();
        if let Some(pattern) = ignored.strip_prefix("regex:") {
            return Regex::new(pattern.trim())
                .map(|regex| regex.is_match(model))
                .unwrap_or(false);
        }
        ignored == model
    })
}

fn normalize_channel_model_mapping(channel: &ChannelRecord) -> BTreeMap<String, String> {
    normalize_channel_model_mapping_result(channel).unwrap_or_default()
}

fn normalize_channel_model_mapping_result(
    channel: &ChannelRecord,
) -> Result<BTreeMap<String, String>, ManagementError> {
    let Some(mapping) = channel.model_mapping.as_deref().map(str::trim) else {
        return Ok(BTreeMap::new());
    };
    if mapping.is_empty() || mapping == "{}" {
        return Ok(BTreeMap::new());
    }
    let parsed = serde_json::from_str::<BTreeMap<String, String>>(mapping).map_err(storage_err)?;
    Ok(parsed
        .into_iter()
        .filter_map(|(source, target)| {
            let source = source.trim();
            let target = target.trim();
            (!source.is_empty() && !target.is_empty())
                .then(|| (source.to_string(), target.to_string()))
        })
        .collect())
}

fn channel_settings(channel: &ChannelRecord) -> ChannelOtherSettings {
    channel
        .setting
        .as_deref()
        .map(str::trim)
        .filter(|setting| !setting.is_empty())
        .and_then(|setting| serde_json::from_str(setting).ok())
        .unwrap_or_default()
}

fn normalize_model_names(models: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::with_capacity(models.len());
    let mut normalized = Vec::with_capacity(models.len());
    for model in models {
        let model = model.trim();
        if model.is_empty() || !seen.insert(model.to_string()) {
            continue;
        }
        normalized.push(model.to_string());
    }
    normalized
}

fn merge_model_names(base: Vec<String>, appended: Vec<String>) -> Vec<String> {
    let mut merged = normalize_model_names(base);
    let mut seen = merged.iter().cloned().collect::<HashSet<_>>();
    for model in normalize_model_names(appended) {
        if seen.insert(model.clone()) {
            merged.push(model);
        }
    }
    merged
}

fn subtract_model_names(base: Vec<String>, removed: Vec<String>) -> Vec<String> {
    let removed = normalize_model_names(removed)
        .into_iter()
        .collect::<HashSet<_>>();
    normalize_model_names(base)
        .into_iter()
        .filter(|model| !removed.contains(model))
        .collect()
}

fn intersect_model_names(base: Vec<String>, allowed: Vec<String>) -> Vec<String> {
    let allowed = normalize_model_names(allowed)
        .into_iter()
        .collect::<HashSet<_>>();
    normalize_model_names(base)
        .into_iter()
        .filter(|model| allowed.contains(model))
        .collect()
}

fn apply_selected_model_changes(
    origin_models: Vec<String>,
    add_models: Vec<String>,
    remove_models: Vec<String>,
) -> Vec<String> {
    let normalized_add = normalize_model_names(add_models);
    let normalized_remove =
        subtract_model_names(normalize_model_names(remove_models), normalized_add.clone());
    subtract_model_names(
        merge_model_names(origin_models, normalized_add),
        normalized_remove,
    )
}

fn build_test_http_request(
    channel: &ChannelRecord,
    base_url: &str,
    key: &str,
    model: &str,
    endpoint: &str,
    stream: bool,
) -> (String, JsonValue, TestAuth) {
    match endpoint {
        ENDPOINT_ANTHROPIC => (
            format!("{base_url}/v1/messages"),
            json!({
                "model": model,
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}],
                "stream": stream
            }),
            TestAuth::Claude,
        ),
        ENDPOINT_GEMINI => (
            format!(
                "{}/v1beta/models/{}:generateContent?key={}",
                base_url, model, key
            ),
            json!({
                "contents": [{"role": "user", "parts": [{"text": "hi"}]}]
            }),
            TestAuth::GeminiQuery,
        ),
        ENDPOINT_EMBEDDINGS => (
            format!("{base_url}/v1/embeddings"),
            json!({"model": model, "input": ["hello world"]}),
            TestAuth::Bearer,
        ),
        ENDPOINT_IMAGE_GENERATION => (
            format!("{base_url}/v1/images/generations"),
            json!({"model": model, "prompt": "a cute cat", "n": 1, "size": "1024x1024"}),
            TestAuth::Bearer,
        ),
        ENDPOINT_JINA_RERANK => (
            format!("{base_url}/v1/rerank"),
            json!({
                "model": model,
                "query": "What is Deep Learning?",
                "documents": [
                    "Deep Learning is a subset of machine learning.",
                    "Machine learning is a field of artificial intelligence."
                ],
                "top_n": 2
            }),
            TestAuth::Bearer,
        ),
        ENDPOINT_OPENAI_RESPONSE => (
            format!("{base_url}/v1/responses"),
            json!({
                "model": model,
                "input": [{"role": "user", "content": "hi"}],
                "stream": stream
            }),
            TestAuth::Bearer,
        ),
        ENDPOINT_OPENAI_RESPONSE_COMPACT => (
            format!("{base_url}/v1/responses/compact"),
            json!({"model": model, "input": [{"role": "user", "content": "hi"}]}),
            TestAuth::Bearer,
        ),
        _ => {
            let mut body = json!({
                "model": model,
                "messages": [{"role": "user", "content": "hi"}],
                "stream": stream
            });
            if model_is_reasoning_o_model(model) {
                body["max_completion_tokens"] = json!(16);
            } else if model.contains("gemini") {
                body["max_tokens"] = json!(3000);
            } else {
                body["max_tokens"] = json!(16);
            }
            if stream {
                body["stream_options"] = json!({"include_usage": true});
            }
            let auth = if channel.channel_type == CHANNEL_TYPE_ANTHROPIC {
                TestAuth::Claude
            } else {
                TestAuth::Bearer
            };
            (format!("{base_url}/v1/chat/completions"), body, auth)
        }
    }
}

fn normalize_test_endpoint(channel: &ChannelRecord, model: &str, requested: &str) -> String {
    let requested = requested.trim();
    if !requested.is_empty() {
        return requested.to_string();
    }
    let lower = model.to_ascii_lowercase();
    if channel.channel_type == CHANNEL_TYPE_ANTHROPIC {
        return ENDPOINT_ANTHROPIC.to_string();
    }
    if channel.channel_type == CHANNEL_TYPE_GEMINI {
        return ENDPOINT_GEMINI.to_string();
    }
    if lower.contains("rerank") {
        return ENDPOINT_JINA_RERANK.to_string();
    }
    if lower.contains("embedding")
        || lower.contains("embed")
        || model.starts_with("m3e")
        || model.contains("bge-")
        || channel.channel_type == CHANNEL_TYPE_MOKA_AI
    {
        return ENDPOINT_EMBEDDINGS.to_string();
    }
    if channel.channel_type == CHANNEL_TYPE_VOLC_ENGINE && lower.contains("seedream") {
        return ENDPOINT_IMAGE_GENERATION.to_string();
    }
    if lower.contains("codex") || channel.channel_type == CHANNEL_TYPE_CODEX {
        return ENDPOINT_OPENAI_RESPONSE.to_string();
    }
    ENDPOINT_OPENAI.to_string()
}

fn select_test_model(channel: &ChannelRecord, requested: &str) -> String {
    let requested = requested.trim();
    if !requested.is_empty() {
        return requested.to_string();
    }
    channel
        .model_list()
        .into_iter()
        .next()
        .unwrap_or_else(|| "gpt-4o-mini".to_string())
}

fn model_is_reasoning_o_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("o1") || model.starts_with("o3") || model.starts_with("o4")
}

fn channel_base_url(channel: &ChannelRecord) -> String {
    channel
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| default_channel_base_url(channel.channel_type))
        .trim_end_matches('/')
        .to_string()
}

/// Build an HTTP client that honors channel `setting.proxy` (same URL the gateway uses).
fn http_client_for_channel(channel: &ChannelRecord) -> Result<reqwest::Client, ManagementError> {
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .connect_timeout(std::time::Duration::from_secs(15));
    if let Some(proxy_url) = channel_setting_proxy_url(channel) {
        let proxy = reqwest::Proxy::all(&proxy_url)
            .map_err(|err| ManagementError::Storage(format!("invalid channel proxy URL: {err}")))?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(storage_err)
}

fn channel_setting_proxy_url(channel: &ChannelRecord) -> Option<String> {
    let raw = channel.setting.as_deref()?.trim();
    if raw.is_empty() {
        return None;
    }
    let v: JsonValue = serde_json::from_str(raw).ok()?;
    v.get("proxy")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn default_channel_base_url(channel_type: i32) -> &'static str {
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
        CHANNEL_TYPE_SILICON_FLOW => "https://api.siliconflow.cn",
        CHANNEL_TYPE_DEEPSEEK => "https://api.deepseek.com",
        CHANNEL_TYPE_VOLC_ENGINE => "https://ark.cn-beijing.volces.com",
        CHANNEL_TYPE_XAI => "https://api.x.ai",
        CHANNEL_TYPE_CODEX | CHANNEL_TYPE_OPENAI => "https://api.openai.com",
        _ => "https://api.openai.com",
    }
}

fn unsupported_channel_test_type(channel_type: i32) -> bool {
    matches!(
        channel_type,
        CHANNEL_TYPE_MIDJOURNEY
            | CHANNEL_TYPE_MIDJOURNEY_PLUS
            | CHANNEL_TYPE_SUNO_API
            | CHANNEL_TYPE_KLING
            | CHANNEL_TYPE_JIMENG
            | CHANNEL_TYPE_DOUBAO_VIDEO
            | CHANNEL_TYPE_VIDU
    )
}

fn snapshot_supported_channel_type(channel_type: i32) -> bool {
    matches!(
        channel_type,
        CHANNEL_TYPE_OPENAI
            | CHANNEL_TYPE_OLLAMA
            | CHANNEL_TYPE_CUSTOM
            | CHANNEL_TYPE_AI_PROXY
            | CHANNEL_TYPE_API2GPT
            | CHANNEL_TYPE_AIGC2D
            | CHANNEL_TYPE_ANTHROPIC
            | CHANNEL_TYPE_ALI
            | CHANNEL_TYPE_OPENROUTER
            | CHANNEL_TYPE_GEMINI
            | CHANNEL_TYPE_MOONSHOT
            | CHANNEL_TYPE_ZHIPU_V4
            | CHANNEL_TYPE_SILICON_FLOW
            | CHANNEL_TYPE_DEEPSEEK
            | CHANNEL_TYPE_MOKA_AI
            | CHANNEL_TYPE_VOLC_ENGINE
            | CHANNEL_TYPE_XAI
            | CHANNEL_TYPE_CODEX
    )
}

fn channel_type_name(channel_type: i32) -> &'static str {
    match channel_type {
        CHANNEL_TYPE_MIDJOURNEY => "Midjourney",
        CHANNEL_TYPE_MIDJOURNEY_PLUS => "MidjourneyPlus",
        CHANNEL_TYPE_SUNO_API => "SunoAPI",
        CHANNEL_TYPE_KLING => "Kling",
        CHANNEL_TYPE_JIMENG => "Jimeng",
        CHANNEL_TYPE_DOUBAO_VIDEO => "DoubaoVideo",
        CHANNEL_TYPE_VIDU => "Vidu",
        _ => "Unknown",
    }
}

fn detect_error_message(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<JsonValue>(text)
        && let Some(message) = error_message_from_json(&value)
    {
        return Some(message);
    }
    for line in text.lines() {
        let line = line.trim();
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<JsonValue>(payload)
            && let Some(message) = error_message_from_json(&value)
        {
            return Some(message);
        }
    }
    None
}

fn error_message_from_json(value: &JsonValue) -> Option<String> {
    let error = value.get("error")?;
    if error.is_null() {
        return None;
    }
    if let Some(message) = error
        .get("message")
        .and_then(JsonValue::as_str)
        .or_else(|| {
            error
                .get("error")
                .and_then(|value| value.get("message"))
                .and_then(JsonValue::as_str)
        })
    {
        let message = message.trim();
        if !message.is_empty() {
            return Some(message.to_string());
        }
    }
    if let Some(message) = error
        .as_str()
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        return Some(message.to_string());
    }
    Some("upstream returned error payload".to_string())
}

fn stream_body_has_event(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|text| {
        text.lines().any(|line| {
            let Some(payload) = line.trim().strip_prefix("data:") else {
                return false;
            };
            let payload = payload.trim();
            !payload.is_empty() && payload != "[DONE]"
        })
    })
}

async fn response_json(response: reqwest::Response) -> Result<JsonValue, ManagementError> {
    let status = response.status();
    let bytes = response.bytes().await.map_err(storage_err)?;
    if !status.is_success() {
        let message = detect_error_message(&bytes).unwrap_or_else(|| status.to_string());
        return Err(ManagementError::Storage(message));
    }
    serde_json::from_slice(&bytes).map_err(storage_err)
}

fn first_key(key: &str) -> String {
    key.trim()
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn is_multi_key(key: &str) -> bool {
    key.lines()
        .filter(|line| !line.trim().is_empty())
        .take(2)
        .count()
        > 1
}

fn json_f64(value: &JsonValue, path: &[&str]) -> Option<f64> {
    let value = json_at(value, path)?;
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|value| value as f64))
        .or_else(|| value.as_u64().map(|value| value as f64))
        .or_else(|| value.as_str()?.parse().ok())
}

fn json_i64(value: &JsonValue, path: &[&str]) -> Option<i64> {
    let value = json_at(value, path)?;
    value
        .as_i64()
        .or_else(|| value.as_u64().map(|value| value as i64))
        .or_else(|| value.as_str()?.parse().ok())
}

fn json_bool(value: &JsonValue, path: &[&str]) -> Option<bool> {
    let value = json_at(value, path)?;
    value.as_bool().or_else(|| value.as_str()?.parse().ok())
}

fn json_str(value: &JsonValue, path: &[&str]) -> Option<String> {
    json_at(value, path)?
        .as_str()
        .map(str::to_string)
        .or_else(|| Some(json_at(value, path)?.to_string()))
}

fn json_at<'a>(value: &'a JsonValue, path: &[&str]) -> Option<&'a JsonValue> {
    path.iter().try_fold(value, |value, key| value.get(key))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn unix_days() -> i64 {
    now_unix() / 86_400
}

fn ymd_string_from_days(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    y += i64::from(m <= 2);
    (y as i32, m as u32, d as u32)
}

fn storage_err(err: impl std::fmt::Display) -> ManagementError {
    ManagementError::Storage(err.to_string())
}

fn default_copy_suffix() -> String {
    "_复制".to_string()
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_change_apply_adds_and_removes_with_add_wins() {
        let origin = vec!["a".to_string(), "b".to_string()];
        let next =
            apply_selected_model_changes(origin, vec!["c".to_string(), "b".to_string()], vec![
                "a".to_string(),
                "c".to_string(),
            ]);
        assert_eq!(next, vec!["b", "c"]);
    }

    #[test]
    fn pending_changes_respect_mapping_and_regex_ignored_models() {
        let mapping = BTreeMap::from([("alias".to_string(), "upstream".to_string())]);
        let (add, remove) = collect_pending_upstream_model_changes(
            vec!["alias".to_string(), "old".to_string()],
            vec![
                "upstream".to_string(),
                "new-a".to_string(),
                "skip-1".to_string(),
            ],
            &["regex:^skip-".to_string()],
            mapping,
        );
        assert_eq!(add, vec!["new-a"]);
        assert_eq!(remove, vec!["old"]);
    }

    #[test]
    fn civil_date_formats_unix_epoch() {
        assert_eq!(ymd_string_from_days(0), "1970-01-01");
        assert_eq!(ymd_string_from_days(20_643), "2026-07-09");
    }
}
