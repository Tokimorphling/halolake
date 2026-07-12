use crate::storage::{ManagementStore, OptionStore};
use halolake_control_plane::{
    AutoDisableChannelRequest, ChannelFeedbackAck, ChannelFeedbackBatch, ChannelFeedbackError,
    ChannelFeedbackEvent, ChannelFeedbackReason, auto_disable_channel_in_place,
};
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
            // Gateway always sends ChannelConfig.id = numeric channel.id as string.
            // Never match snapshot aliases / empty ids — that was the cross-channel
            // disable bug when feedback targeted the wrong record.
            let Some(numeric_id) = parse_numeric_channel_id(&event.channel_id) else {
                warn!(
                    feedback_id = %event.channel_id,
                    status_code = ?event.status_code,
                    reason = %event.message,
                    "skip auto-disable: non-numeric channel_id"
                );
                continue;
            };
            let reason = feedback_reason(&event);
            let result = match self
                .management
                .call(AutoDisableChannelRequest {
                    id:                 numeric_id,
                    reason:             reason.clone(),
                    api_key_index:      event.api_key_index,
                    created_at_unix_ms: event.created_at_unix_ms,
                })
                .await
            {
                Ok(result) => result,
                Err(err) => {
                    warn!(
                        channel_id = numeric_id,
                        feedback_id = %event.channel_id,
                        ?err,
                        "auto-disable channel not found or failed"
                    );
                    continue;
                }
            };
            if !result.changed {
                continue;
            }
            // Best-effort name/group for logs (read-only after mutate).
            let (name, group) = self
                .management
                .current_data()
                .ok()
                .and_then(|data| {
                    data.channels
                        .into_iter()
                        .find(|c| c.id == numeric_id)
                        .map(|c| (c.name, c.group))
                })
                .unwrap_or_else(|| (String::new(), String::new()));
            warn!(
                channel_id = numeric_id,
                channel_name = %name,
                group = %group,
                feedback_id = %event.channel_id,
                status_code = ?event.status_code,
                reason = %event.message,
                "auto-disabling channel from gateway feedback"
            );
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

fn parse_numeric_channel_id(channel_id: &str) -> Option<u64> {
    let channel_id = channel_id.trim();
    if channel_id.is_empty() {
        return None;
    }
    // Reject non-decimal ids (snapshot aliases, names, empty). Gateway snapshot
    // uses channel.id.to_string() for ChannelConfig.id.
    if !channel_id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    channel_id.parse().ok()
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

    /// Align with new-api `ShouldDisableChannel`: status-code ranges + keyword
    /// match on the error body. Transient transport failures (timeouts, reset,
    /// TLS) alone do **not** disable a channel — that was over-aggressive and
    /// caused healthy channels to flip to status=3 after flaky network blips.
    fn should_disable(&self, event: &ChannelFeedbackEvent) -> bool {
        // Config / client-identity errors must not auto-ban channels (or unrelated ones).
        let lower_msg = event.message.to_ascii_lowercase();
        if lower_msg.contains("grok cli version")
            || lower_msg.contains("not from a valid issuer")
            || lower_msg.contains("x-grok-client-version")
        {
            return false;
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
        self.keywords.iter().any(|keyword| {
            // Ignore very short keywords that match too many error bodies.
            keyword.len() >= 4 && lower_msg.contains(keyword.as_str())
        })
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
    use halolake_domain::{ChannelRecord, STATUS_AUTO_DISABLED, STATUS_ENABLED};

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
        let result = auto_disable_channel_in_place(
            &mut channel,
            "upstream status 401: unauthorized",
            Some(1),
            1_700_000_000_000,
        );
        assert!(result.changed);
        assert!(result.key_disabled);
        assert!(result.channel_disabled);
        assert_eq!(channel.status, STATUS_AUTO_DISABLED);
        let setting: serde_json::Value =
            serde_json::from_str(channel.setting.as_deref().expect("setting")).expect("json");
        assert_eq!(
            setting
                .pointer("/multi_key_status_list/1")
                .and_then(serde_json::Value::as_i64),
            Some(STATUS_AUTO_DISABLED as i64)
        );
    }

    #[test]
    fn transport_alone_does_not_disable_like_new_api() {
        let policy = DisablePolicy {
            enabled:       true,
            status_ranges: parse_status_code_ranges("401").unwrap(),
            keywords:      vec!["invalid_api_key".into()],
        };
        let transport = ChannelFeedbackEvent {
            request_id:         "req".into(),
            channel_id:         "1".into(),
            api_key_index:      None,
            status_code:        None,
            reason:             ChannelFeedbackReason::Transport,
            message:            "connection reset by peer".into(),
            created_at_unix_ms: 0,
        };
        assert!(
            !policy.should_disable(&transport),
            "transient transport must not auto-ban"
        );

        let unauthorized = ChannelFeedbackEvent {
            status_code: Some(401),
            reason: ChannelFeedbackReason::UpstreamStatus,
            message: "unauthorized".into(),
            ..transport.clone()
        };
        assert!(policy.should_disable(&unauthorized));

        let keyword = ChannelFeedbackEvent {
            status_code: Some(400),
            reason: ChannelFeedbackReason::UpstreamStatus,
            message: "Error: invalid_api_key for account".into(),
            ..transport
        };
        assert!(policy.should_disable(&keyword));

        let grok_cli = ChannelFeedbackEvent {
            request_id:         "req".into(),
            channel_id:         "1".into(),
            api_key_index:      None,
            status_code:        Some(400),
            reason:             ChannelFeedbackReason::UpstreamStatus,
            message:            "Your Grok CLI version (none) is outdated".into(),
            created_at_unix_ms: 0,
        };
        assert!(
            !policy.should_disable(&grok_cli),
            "xAI client identity errors must not auto-ban"
        );
    }
}
