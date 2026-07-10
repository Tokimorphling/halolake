use serde::{Deserialize, Serialize};
use service_async::Service;
use thiserror::Error;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ChannelFeedbackBatch {
    #[serde(default)]
    pub events: Vec<ChannelFeedbackEvent>,
}

impl ChannelFeedbackBatch {
    pub fn new(events: Vec<ChannelFeedbackEvent>) -> Self {
        Self { events }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ChannelFeedbackEvent {
    pub request_id: String,
    pub channel_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    pub reason: ChannelFeedbackReason,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    pub created_at_unix_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelFeedbackReason {
    UpstreamStatus,
    Transport,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ChannelFeedbackAck {
    pub accepted: usize,
    pub disabled_channels: usize,
    pub disabled_keys: usize,
}

#[derive(Debug, Error)]
pub enum ChannelFeedbackError {
    #[error("channel feedback sink is unavailable")]
    Unavailable,
    #[error("channel feedback storage error: {0}")]
    Storage(String),
    #[error("channel feedback transport error: {0}")]
    Transport(String),
    #[error("invalid channel feedback response: {0}")]
    InvalidResponse(String),
}

pub trait ChannelFeedbackSink:
    Service<ChannelFeedbackBatch, Response = ChannelFeedbackAck, Error = ChannelFeedbackError>
{
}

impl<T> ChannelFeedbackSink for T where
    T: Service<ChannelFeedbackBatch, Response = ChannelFeedbackAck, Error = ChannelFeedbackError>
{
}

#[derive(Debug, Clone, Default)]
pub struct NoopChannelFeedbackSink;

impl Service<ChannelFeedbackBatch> for NoopChannelFeedbackSink {
    type Response = ChannelFeedbackAck;
    type Error = ChannelFeedbackError;

    async fn call(&self, req: ChannelFeedbackBatch) -> Result<Self::Response, Self::Error> {
        Ok(ChannelFeedbackAck {
            accepted: req.len(),
            disabled_channels: 0,
            disabled_keys: 0,
        })
    }
}
