use crate::storage::{ManagementStore, OptionStore};
use halolake_control_plane::{
    ChannelFeedbackAck, ChannelFeedbackBatch, ChannelFeedbackError, ChannelFeedbackEvent,
    ChannelFeedbackReason, UpdateChannelRequest,
};
use halolake_domain::{ChannelRecord, STATUS_AUTO_DISABLED, STATUS_ENABLED};
use serde_json::{Value as JsonValue, json};
use service_async::Service;
use std::collections::BTreeMap;
use tracing::warn;

#[derive(Debug, Clone)]
pub(crate) struct ChannelFeedbackService {
    management: ManagementStore,
    options:    OptionStore,
}

impl ChannelFeedbackService {
    pub(crate) fn new(management: ManagementStore, options: OptionStore) -> Self {
        Self {
            management,
            options,
        }
    }
}

impl Service<ChannelFeedbackBatch> for ChannelFeedbackService {
    type Response = ChannelFeedbackAck;
    type Error = ChannelFeedbackError;

    async fn call(&self, req: ChannelFeedbackBatch) -> Result<Self::Response, Self::Error> {
        let options = self.options.values().map_err(feedback_storage)?;
        let policy = DisablePolicy::from_options(&options);
        let mut ack = ChannelFeedbackAck {
            accepted:          req.len(),
            disabled_channels: 0,
            disabled_keys:     0,
        };

        if !policy.enabled {
            return Ok(ack);
        }

        for event in req.events {
            if !policy.should_disable(&event) {
                continue;
            }
            let Some(mut channel) = self.find_channel(&event.channel_id)? else {
                continue;
            };
            if channel.auto_ban.unwrap_or(1) == 0 {
                continue;
            }
            let result = apply_auto_disable(&mut channel, &event);
            if !result.changed {
                continue;
            }
            self.management
                .call(UpdateChannelRequest { channel })
                .await
                .map_err(feedback_storage)?;
            ack.disabled_channels = ack
                .disabled_channels
                .saturating_add(usize::from(result.channel_disabled));
            ack.disabled_keys = ack
                .disabled_keys
                .saturating_add(usize::from(result.key_disabled));
        }

        Ok(ack)
    }
}

impl ChannelFeedbackService {
    fn find_channel(
        &self,
        channel_id: &str,
    ) -> Result<Option<ChannelRecord>, ChannelFeedbackError> {
        let data = self.management.current_data().map_err(feedback_storage)?;
        let numeric_id = channel_id.parse::<u64>().ok();
        Ok(data.channels.into_iter().find(|channel| {
            channel.snapshot_id.as_deref() == Some(channel_id)
                || numeric_id.is_some_and(|id| channel.id == id)
                || channel.id.to_string() == channel_id
        }))
    }
}

#[derive(Debug, Clone)]
struct DisablePolicy {
    enabled:       bool,
    status_ranges: Vec<StatusCodeRange>,
    keywords:      Vec<String>,
}

impl DisablePolicy {
    fn from_options(options: &BTreeMap<String, String>) -> Self {
        let status_ranges = parse_status_code_ranges(
            options
                .get("AutomaticDisableStatusCodes")
                .map_or("401", String::as_str),
        )
        .unwrap_or_else(|err| {
            warn!(
                ?err,
                "invalid AutomaticDisableStatusCodes; disabling status-code auto ban"
            );
            Vec::new()
        });
        let keywords = options
            .get("AutomaticDisableKeywords")
            .map_or("", String::as_str)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        Self {
            enabled: option_bool(options, "AutomaticDisableChannelEnabled", false),
            status_ranges,
            keywords,
        }
    }

    fn should_disable(&self, event: &ChannelFeedbackEvent) -> bool {
        match event.reason {
            ChannelFeedbackReason::Transport => return true,
            ChannelFeedbackReason::UpstreamStatus => {}
        }
        if let Some(status_code) = event.status_code
            && self
                .status_ranges
                .iter()
                .any(|range| range.contains(status_code))
        {
            return true;
        }
        if self.keywords.is_empty() || event.message.is_empty() {
            return false;
        }
        let lower = event.message.to_ascii_lowercase();
        self.keywords
            .iter()
            .any(|keyword| lower.contains(keyword.as_str()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisableResult {
    changed:          bool,
    channel_disabled: bool,
    key_disabled:     bool,
}

fn apply_auto_disable(channel: &mut ChannelRecord, event: &ChannelFeedbackEvent) -> DisableResult {
    let reason = feedback_reason(event);
    let key_count = channel_key_count(&channel.key);
    if let Some(key_index) = event.api_key_index
        && key_index < key_count
    {
        let mut state = multi_key_state(channel);
        let already_disabled = state
            .status
            .get(&key_index)
            .is_some_and(|status| *status == STATUS_AUTO_DISABLED);
        if !already_disabled {
            state.status.insert(key_index, STATUS_AUTO_DISABLED);
            state
                .disabled_time
                .insert(key_index, event.created_at_unix_ms / 1000);
            state.disabled_reason.insert(key_index, reason.clone());
            save_multi_key_state(channel, &state);
        }

        let channel_was_disabled = channel.status == STATUS_AUTO_DISABLED;
        if !has_enabled_multi_key(key_count, &state.status) {
            channel.status = STATUS_AUTO_DISABLED;
            save_channel_status_info(channel, "All keys are disabled", event.created_at_unix_ms);
        }
        return DisableResult {
            changed:          !already_disabled
                || !channel_was_disabled && channel.status == STATUS_AUTO_DISABLED,
            channel_disabled: !channel_was_disabled && channel.status == STATUS_AUTO_DISABLED,
            key_disabled:     !already_disabled,
        };
    }

    if channel.status == STATUS_AUTO_DISABLED {
        return DisableResult {
            changed:          false,
            channel_disabled: false,
            key_disabled:     false,
        };
    }
    channel.status = STATUS_AUTO_DISABLED;
    save_channel_status_info(channel, &reason, event.created_at_unix_ms);
    DisableResult {
        changed:          true,
        channel_disabled: true,
        key_disabled:     false,
    }
}

fn feedback_reason(event: &ChannelFeedbackEvent) -> String {
    let mut reason = match (event.reason, event.status_code) {
        (ChannelFeedbackReason::UpstreamStatus, Some(status)) => {
            format!("upstream status {status}")
        }
        (ChannelFeedbackReason::UpstreamStatus, None) => "upstream error".to_string(),
        (ChannelFeedbackReason::Transport, _) => "upstream transport error".to_string(),
    };
    if !event.message.trim().is_empty() {
        reason.push_str(": ");
        reason.push_str(event.message.trim());
    }
    reason
}

#[derive(Debug, Clone, Default)]
struct MultiKeyState {
    status:          BTreeMap<usize, i32>,
    disabled_time:   BTreeMap<usize, i64>,
    disabled_reason: BTreeMap<usize, String>,
}

fn multi_key_state(channel: &ChannelRecord) -> MultiKeyState {
    let value = channel_setting_json(channel);
    MultiKeyState {
        status:          json_usize_i32_map(value.get("multi_key_status_list")),
        disabled_time:   json_usize_i64_map(value.get("multi_key_disabled_time")),
        disabled_reason: json_usize_string_map(value.get("multi_key_disabled_reason")),
    }
}

fn save_multi_key_state(channel: &mut ChannelRecord, state: &MultiKeyState) {
    let mut value = channel_setting_json(channel);
    let object = value.as_object_mut().expect("setting is object");
    object.insert(
        "multi_key_status_list".to_string(),
        usize_i32_map_json(&state.status),
    );
    object.insert(
        "multi_key_disabled_time".to_string(),
        usize_i64_map_json(&state.disabled_time),
    );
    object.insert(
        "multi_key_disabled_reason".to_string(),
        usize_string_map_json(&state.disabled_reason),
    );
    channel.setting = serde_json::to_string(&value).ok();
}

fn save_channel_status_info(channel: &mut ChannelRecord, reason: &str, created_at_unix_ms: i64) {
    let mut value = channel_setting_json(channel);
    let object = value.as_object_mut().expect("setting is object");
    object.insert(
        "status_reason".to_string(),
        JsonValue::String(reason.to_string()),
    );
    object.insert(
        "status_time".to_string(),
        JsonValue::Number((created_at_unix_ms / 1000).into()),
    );
    channel.setting = serde_json::to_string(&value).ok();
}

fn channel_setting_json(channel: &ChannelRecord) -> JsonValue {
    let mut value = channel
        .setting
        .as_deref()
        .and_then(|setting| serde_json::from_str::<JsonValue>(setting).ok())
        .unwrap_or_else(|| json!({}));
    if !value.is_object() {
        value = json!({});
    }
    value
}

fn has_enabled_multi_key(len: usize, status: &BTreeMap<usize, i32>) -> bool {
    (0..len).any(|idx| {
        status
            .get(&idx)
            .is_none_or(|status| *status == STATUS_ENABLED)
    })
}

fn channel_key_count(key: &str) -> usize {
    key.lines()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .count()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatusCodeRange {
    start: u16,
    end:   u16,
}

impl StatusCodeRange {
    fn contains(self, code: u16) -> bool {
        self.start <= code && code <= self.end
    }
}

fn parse_status_code_ranges(input: &str) -> Result<Vec<StatusCodeRange>, String> {
    let input = input.trim().replace('，', ",");
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let mut ranges = Vec::new();
    let mut invalid = Vec::new();
    for segment in input
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        match parse_status_code_token(segment) {
            Ok(range) => ranges.push(range),
            Err(_) => invalid.push(segment.to_string()),
        }
    }
    if !invalid.is_empty() {
        return Err(format!(
            "invalid http status code rules: {}",
            invalid.join(", ")
        ));
    }
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged = Vec::<StatusCodeRange>::with_capacity(ranges.len());
    for range in ranges {
        let Some(last) = merged.last_mut() else {
            merged.push(range);
            continue;
        };
        if range.start <= last.end.saturating_add(1) {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    Ok(merged)
}

fn parse_status_code_token(token: &str) -> Result<StatusCodeRange, ()> {
    let token = token.trim().replace(' ', "");
    if token.is_empty() {
        return Err(());
    }
    if let Some((start, end)) = token.split_once('-') {
        if start.is_empty() || end.is_empty() {
            return Err(());
        }
        let start = parse_http_status(start)?;
        let end = parse_http_status(end)?;
        if start > end {
            return Err(());
        }
        return Ok(StatusCodeRange { start, end });
    }
    let code = parse_http_status(&token)?;
    Ok(StatusCodeRange {
        start: code,
        end:   code,
    })
}

fn parse_http_status(token: &str) -> Result<u16, ()> {
    let code = token.parse::<u16>().map_err(|_| ())?;
    if !(100..=599).contains(&code) {
        return Err(());
    }
    Ok(code)
}

fn json_usize_i32_map(value: Option<&JsonValue>) -> BTreeMap<usize, i32> {
    value
        .and_then(JsonValue::as_object)
        .into_iter()
        .flat_map(|object| object.iter())
        .filter_map(|(key, value)| {
            let key = key.parse::<usize>().ok()?;
            let value = value
                .as_i64()
                .or_else(|| value.as_str()?.parse::<i64>().ok())?;
            Some((key, value as i32))
        })
        .collect()
}

fn json_usize_i64_map(value: Option<&JsonValue>) -> BTreeMap<usize, i64> {
    value
        .and_then(JsonValue::as_object)
        .into_iter()
        .flat_map(|object| object.iter())
        .filter_map(|(key, value)| {
            let key = key.parse::<usize>().ok()?;
            let value = value
                .as_i64()
                .or_else(|| value.as_str()?.parse::<i64>().ok())?;
            Some((key, value))
        })
        .collect()
}

fn json_usize_string_map(value: Option<&JsonValue>) -> BTreeMap<usize, String> {
    value
        .and_then(JsonValue::as_object)
        .into_iter()
        .flat_map(|object| object.iter())
        .filter_map(|(key, value)| Some((key.parse::<usize>().ok()?, value.as_str()?.to_string())))
        .collect()
}

fn usize_i32_map_json(map: &BTreeMap<usize, i32>) -> JsonValue {
    json!(
        map.iter()
            .map(|(key, value)| (key.to_string(), *value))
            .collect::<BTreeMap<_, _>>()
    )
}

fn usize_i64_map_json(map: &BTreeMap<usize, i64>) -> JsonValue {
    json!(
        map.iter()
            .map(|(key, value)| (key.to_string(), *value))
            .collect::<BTreeMap<_, _>>()
    )
}

fn usize_string_map_json(map: &BTreeMap<usize, String>) -> JsonValue {
    json!(
        map.iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect::<BTreeMap<_, _>>()
    )
}

fn option_bool(options: &BTreeMap<String, String>, key: &str, default: bool) -> bool {
    options.get(key).map_or(default, |value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn feedback_storage(err: impl std::fmt::Display) -> ChannelFeedbackError {
    ChannelFeedbackError::Storage(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_merges_status_code_ranges_like_new_api() {
        assert_eq!(
            parse_status_code_ranges("500-505,504,401,403,402").unwrap(),
            vec![
                StatusCodeRange {
                    start: 401,
                    end:   403,
                },
                StatusCodeRange {
                    start: 500,
                    end:   505,
                }
            ]
        );
        assert!(parse_status_code_ranges("99,600,foo,500-400").is_err());
    }

    #[test]
    fn disables_multi_key_by_original_index_and_channel_when_all_keys_disabled() {
        let mut channel = ChannelRecord {
            id:                   7,
            snapshot_id:          Some("openai-main".to_string()),
            channel_type:         1,
            key:                  "key-a\nkey-b".to_string(),
            status:               STATUS_ENABLED,
            name:                 "openai-main".to_string(),
            weight:               Some(1),
            created_time:         0,
            test_time:            0,
            response_time:        0,
            base_url:             None,
            balance:              0.0,
            balance_updated_time: 0,
            models:               "gpt-4o".to_string(),
            group:                "default".to_string(),
            used_quota:           0,
            model_mapping:        None,
            priority:             Some(0),
            auto_ban:             Some(1),
            tag:                  None,
            setting:              Some(r#"{"multi_key_status_list":{"0":3}}"#.to_string()),
            param_override:       None,
            header_override:      None,
            remark:               None,
            proxy_id:             None,
        };
        let result = apply_auto_disable(&mut channel, &ChannelFeedbackEvent {
            request_id:         "req".to_string(),
            channel_id:         "openai-main".to_string(),
            api_key_index:      Some(1),
            status_code:        Some(401),
            reason:             ChannelFeedbackReason::UpstreamStatus,
            message:            "unauthorized".to_string(),
            created_at_unix_ms: 1_700_000_000_000,
        });
        assert!(result.changed);
        assert!(result.key_disabled);
        assert!(result.channel_disabled);
        assert_eq!(channel.status, STATUS_AUTO_DISABLED);
        let state = multi_key_state(&channel);
        assert_eq!(state.status.get(&1), Some(&STATUS_AUTO_DISABLED));
    }
}
