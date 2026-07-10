use halolake_router_core::GatewaySnapshot;
use serde::{Deserialize, Serialize};
use service_async::Service;
use thiserror::Error;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SnapshotRequest {
    #[serde(default)]
    pub since_version: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SnapshotResponse {
    NotModified { version: u64 },
    Updated { snapshot: GatewaySnapshot },
}

impl SnapshotResponse {
    pub fn version(&self) -> u64 {
        match self {
            Self::NotModified { version } => *version,
            Self::Updated { snapshot } => snapshot.version,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PublishSnapshotRequest {
    pub snapshot: GatewaySnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct SnapshotPublished {
    pub version: u64,
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("snapshot store is unavailable")]
    Unavailable,
    #[error("snapshot store lock is poisoned: {0}")]
    Poisoned(&'static str),
    #[error("snapshot transport error: {0}")]
    Transport(String),
    #[error("invalid snapshot response: {0}")]
    InvalidResponse(String),
    #[error("stale snapshot version: current={current}, attempted={attempted}")]
    StaleVersion { current: u64, attempted: u64 },
}

pub trait SnapshotSource:
    Service<SnapshotRequest, Response = SnapshotResponse, Error = SnapshotError>
{
}

impl<T> SnapshotSource for T where
    T: Service<SnapshotRequest, Response = SnapshotResponse, Error = SnapshotError>
{
}

pub trait SnapshotPublisher:
    Service<PublishSnapshotRequest, Response = SnapshotPublished, Error = SnapshotError>
{
}

impl<T> SnapshotPublisher for T where
    T: Service<PublishSnapshotRequest, Response = SnapshotPublished, Error = SnapshotError>
{
}

#[derive(Debug, Clone)]
pub struct StaticSnapshotSource {
    snapshot: GatewaySnapshot,
}

impl StaticSnapshotSource {
    pub fn new(snapshot: GatewaySnapshot) -> Self {
        Self { snapshot }
    }

    pub fn version(&self) -> u64 {
        self.snapshot.version
    }
}

impl Service<SnapshotRequest> for StaticSnapshotSource {
    type Response = SnapshotResponse;
    type Error = SnapshotError;

    async fn call(&self, req: SnapshotRequest) -> Result<Self::Response, Self::Error> {
        Ok(snapshot_response(self.snapshot.clone(), req.since_version))
    }
}

pub(crate) fn snapshot_response(
    snapshot: GatewaySnapshot,
    since_version: Option<u64>,
) -> SnapshotResponse {
    let version = snapshot.version;
    if since_version.is_some_and(|since| since >= version) {
        SnapshotResponse::NotModified { version }
    } else {
        SnapshotResponse::Updated { snapshot }
    }
}
