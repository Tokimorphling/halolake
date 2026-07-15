use crate::{
    PublishSnapshotRequest, SnapshotError, SnapshotPublished, SnapshotRequest, SnapshotResponse,
    UsageAck, UsageError, UsageEventBatch, UsageEventQuota,
};
use halolake_domain::UsageEvent;
use halolake_router_core::GatewaySnapshot;
use service_async::Service;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
};

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
            .map_err(|_| SnapshotError::Poisoned("snapshot"))?;
        let version = snapshot.version;
        if req.since_version.is_some_and(|since| since >= version) {
            return Ok(SnapshotResponse::NotModified { version });
        }
        Ok(SnapshotResponse::Updated {
            snapshot: snapshot.clone(),
        })
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

#[derive(Debug, Default)]
struct MemoryUsageState {
    events:      Vec<UsageEvent>,
    request_ids: HashSet<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MemoryUsageEventSink {
    inner: Arc<RwLock<MemoryUsageState>>,
}

impl MemoryUsageEventSink {
    pub fn events(&self) -> Result<Vec<UsageEvent>, UsageError> {
        self.inner
            .read()
            .map(|state| state.events.clone())
            .map_err(|_| UsageError::Poisoned("usage_events"))
    }

    /// Records only request ids that are not already present. The membership
    /// check and append intentionally share one write lock so concurrent retry
    /// batches cannot both be acknowledged and settled.
    pub fn record_unique(&self, events: Vec<UsageEvent>) -> Result<Vec<UsageEvent>, UsageError> {
        let mut state = self
            .inner
            .write()
            .map_err(|_| UsageError::Poisoned("usage_events"))?;
        let mut accepted = Vec::with_capacity(events.len());
        for event in events {
            if state.request_ids.insert(event.request_id.clone()) {
                accepted.push(event.clone());
                state.events.push(event);
            }
        }
        Ok(accepted)
    }

    pub fn delete_before_unix_seconds(&self, target_timestamp: i64) -> Result<usize, UsageError> {
        let mut removed = 0usize;
        let mut state = self
            .inner
            .write()
            .map_err(|_| UsageError::Poisoned("usage_events"))?;
        state.events.retain(|event| {
            let keep = event.created_at_unix_ms / 1000 >= target_timestamp;
            if !keep {
                removed = removed.saturating_add(1);
            }
            keep
        });
        state.request_ids = state
            .events
            .iter()
            .map(|event| event.request_id.clone())
            .collect();
        Ok(removed)
    }

    pub fn apply_quotas(&self, quotas: &[UsageEventQuota]) -> Result<(), UsageError> {
        if quotas.is_empty() {
            return Ok(());
        }
        let mut state = self
            .inner
            .write()
            .map_err(|_| UsageError::Poisoned("usage_events"))?;
        let quota_by_request = quotas
            .iter()
            .map(|quota| (quota.request_id.as_str(), quota.quota))
            .collect::<HashMap<_, _>>();
        for event in &mut state.events {
            if let Some(quota) = quota_by_request.get(event.request_id.as_str()) {
                event.quota = Some(*quota);
            }
        }
        Ok(())
    }
}

impl Service<UsageEventBatch> for MemoryUsageEventSink {
    type Response = UsageAck;
    type Error = UsageError;

    async fn call(&self, req: UsageEventBatch) -> Result<Self::Response, Self::Error> {
        let accepted = self.record_unique(req.events)?.len();
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
