use halolake_domain::UsageEvent;
use serde::{Deserialize, Serialize};
use service_async::Service;
use thiserror::Error;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct UsageEventBatch {
    #[serde(default)]
    pub events: Vec<UsageEvent>,
}

impl UsageEventBatch {
    pub fn new(events: Vec<UsageEvent>) -> Self {
        Self { events }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct UsageAck {
    pub accepted: usize,
}

#[derive(Debug, Error)]
pub enum UsageError {
    #[error("usage event sink is unavailable")]
    Unavailable,
    #[error("usage event sink lock is poisoned: {0}")]
    Poisoned(&'static str),
    #[error("usage event storage error: {0}")]
    Storage(String),
    #[error("usage event transport error: {0}")]
    Transport(String),
    #[error("invalid usage event response: {0}")]
    InvalidResponse(String),
}

pub trait UsageEventSink:
    Service<UsageEventBatch, Response = UsageAck, Error = UsageError>
{
}

impl<T> UsageEventSink for T where
    T: Service<UsageEventBatch, Response = UsageAck, Error = UsageError>
{
}

#[derive(Debug, Clone, Default)]
pub struct NoopUsageEventSink;

impl Service<UsageEventBatch> for NoopUsageEventSink {
    type Response = UsageAck;
    type Error = UsageError;

    async fn call(&self, req: UsageEventBatch) -> Result<Self::Response, Self::Error> {
        Ok(UsageAck {
            accepted: req.len(),
        })
    }
}
