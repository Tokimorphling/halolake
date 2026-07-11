use crate::{
    PublishSnapshotRequest, SnapshotError, SnapshotPublished, SnapshotRequest, SnapshotResponse,
    UsageAck, UsageError, UsageEventBatch, UsageEventQuota, snapshot::snapshot_response,
};
use halolake_domain::UsageEvent;
use halolake_router_core::GatewaySnapshot;
use service_async::Service;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone)]
pub struct MemorySnapshotBus {
    inner: Arc<RwLock<GatewaySnapshot>>,
}

impl MemorySnapshotBus {
    pub fn new(snapshot: GatewaySnapshot) -> Self {
        Self {
            inner: Arc::new(RwLock::new(snapshot)),
        }
    }

    pub fn current_version(&self) -> Result<u64, SnapshotError> {
        self.inner
            .read()
            .map(|snapshot| snapshot.version)
            .map_err(|_| SnapshotError::Poisoned("snapshot"))
    }
}

impl Service<SnapshotRequest> for MemorySnapshotBus {
    type Response = SnapshotResponse;
    type Error = SnapshotError;

    async fn call(&self, req: SnapshotRequest) -> Result<Self::Response, Self::Error> {
        let snapshot = self
            .inner
            .read()
            .map_err(|_| SnapshotError::Poisoned("snapshot"))?
            .clone();
        Ok(snapshot_response(snapshot, req.since_version))
    }
}

impl Service<PublishSnapshotRequest> for MemorySnapshotBus {
    type Response = SnapshotPublished;
    type Error = SnapshotError;

    async fn call(&self, req: PublishSnapshotRequest) -> Result<Self::Response, Self::Error> {
        let mut current = self
            .inner
            .write()
            .map_err(|_| SnapshotError::Poisoned("snapshot"))?;
        if req.snapshot.version < current.version {
            return Err(SnapshotError::StaleVersion {
                current:   current.version,
                attempted: req.snapshot.version,
            });
        }
        let version = req.snapshot.version;
        *current = req.snapshot;
        Ok(SnapshotPublished { version })
    }
}

#[derive(Debug, Clone, Default)]
pub struct MemoryUsageEventSink {
    inner: Arc<RwLock<Vec<UsageEvent>>>,
}

impl MemoryUsageEventSink {
    pub fn events(&self) -> Result<Vec<UsageEvent>, UsageError> {
        self.inner
            .read()
            .map(|events| events.clone())
            .map_err(|_| UsageError::Poisoned("usage_events"))
    }

    pub fn delete_before_unix_seconds(&self, target_timestamp: i64) -> Result<usize, UsageError> {
        let mut removed = 0usize;
        self.inner
            .write()
            .map_err(|_| UsageError::Poisoned("usage_events"))?
            .retain(|event| {
                let keep = event.created_at_unix_ms / 1000 >= target_timestamp;
                if !keep {
                    removed = removed.saturating_add(1);
                }
                keep
            });
        Ok(removed)
    }

    pub fn apply_quotas(&self, quotas: &[UsageEventQuota]) -> Result<(), UsageError> {
        if quotas.is_empty() {
            return Ok(());
        }
        let mut events = self
            .inner
            .write()
            .map_err(|_| UsageError::Poisoned("usage_events"))?;
        for quota in quotas {
            if let Some(event) = events
                .iter_mut()
                .find(|event| event.request_id == quota.request_id)
            {
                event.quota = Some(quota.quota);
            }
        }
        Ok(())
    }
}

impl Service<UsageEventBatch> for MemoryUsageEventSink {
    type Response = UsageAck;
    type Error = UsageError;

    async fn call(&self, req: UsageEventBatch) -> Result<Self::Response, Self::Error> {
        let accepted = req.len();
        self.inner
            .write()
            .map_err(|_| UsageError::Poisoned("usage_events"))?
            .extend(req.events);
        Ok(UsageAck { accepted })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halolake_router_core::GatewaySnapshot;
    use service_async::Service;
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn snapshot(version: u64) -> GatewaySnapshot {
        GatewaySnapshot {
            version,
            tokens: Vec::new(),
            channels: Vec::new(),
            model_mappings: Vec::new(),
            channel_affinity: Default::default(),
            group_routing: Default::default(),
        }
    }

    #[test]
    fn memory_snapshot_bus_returns_not_modified_for_current_version() {
        block_on(async {
            let bus = MemorySnapshotBus::new(snapshot(7));
            let resp = bus
                .call(SnapshotRequest {
                    since_version: Some(7),
                })
                .await
                .expect("snapshot read should succeed");
            assert!(matches!(resp, SnapshotResponse::NotModified { version: 7 }));
        });
    }

    #[test]
    fn memory_snapshot_bus_publishes_newer_snapshot() {
        block_on(async {
            let bus = MemorySnapshotBus::new(snapshot(7));
            let published = bus
                .call(PublishSnapshotRequest {
                    snapshot: snapshot(8),
                })
                .await
                .expect("snapshot publish should succeed");
            assert_eq!(published.version, 8);
            assert_eq!(bus.current_version().unwrap(), 8);
        });
    }
}
