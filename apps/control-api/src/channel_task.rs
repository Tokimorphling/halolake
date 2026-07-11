use crate::{
    channel_ops::{
        ChannelOpsProgress, ChannelOpsService, DetectAllChannelUpstreamModelUpdatesRequest,
        TestAllChannelsRequest,
    },
    proxy::ProxyStore,
    publish_enriched_management_snapshot,
    storage::{ManagementStore, OptionStore},
    system_task::{
        ClaimSystemTaskRequest, EnqueueSystemTaskRequest, FinishSystemTaskRequest,
        SYSTEM_TASK_TYPE_CHANNEL_TEST, SYSTEM_TASK_TYPE_MODEL_UPDATE, SystemTaskRecord,
        SystemTaskStatus, SystemTaskStore, UpdateSystemTaskStateRequest,
    },
};
use halolake_control_plane::{ManagementError, MemorySnapshotBus};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use service_async::Service;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use uuid::Uuid;

const PROGRESS_WRITE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChannelTestTaskPayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) mode:   String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) notify: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) stream: bool,
}

impl ChannelTestTaskPayload {
    pub(crate) fn manual(stream: bool) -> Self {
        Self {
            mode: "manual".to_string(),
            notify: false,
            stream,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ModelUpdateTaskPayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) mode: String,
}

impl ModelUpdateTaskPayload {
    pub(crate) fn manual() -> Self {
        Self {
            mode: "manual".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct SystemTaskProgressState {
    pub(crate) total:     usize,
    pub(crate) processed: usize,
    pub(crate) progress:  usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ChannelTaskSchedulerConfig {
    pub(crate) channel_test_interval: Option<Duration>,
    pub(crate) model_update_interval: Option<Duration>,
}

pub(crate) fn spawn_channel_task_scheduler(
    tasks: SystemTaskStore,
    management: ManagementStore,
    options: OptionStore,
    snapshots: MemorySnapshotBus,
    proxies: ProxyStore,
    config: ChannelTaskSchedulerConfig,
) {
    if let Some(interval) = config
        .channel_test_interval
        .filter(|interval| !interval.is_zero())
    {
        spawn_periodic_channel_test_task(
            tasks.clone(),
            management.clone(),
            options.clone(),
            snapshots.clone(),
            proxies.clone(),
            interval,
        );
    }
    if let Some(interval) = config
        .model_update_interval
        .filter(|interval| !interval.is_zero())
    {
        spawn_periodic_model_update_task(tasks, management, options, snapshots, proxies, interval);
    }
}

pub(crate) fn spawn_channel_test_task(
    tasks: SystemTaskStore,
    management: ManagementStore,
    options: OptionStore,
    snapshots: MemorySnapshotBus,
    proxies: ProxyStore,
    task_id: String,
) {
    tokio::spawn(async move {
        let logged_task_id = task_id.clone();
        let runner = ChannelTestTaskRunner::new(tasks, management, options, snapshots, proxies);
        if let Err(err) = runner.call(RunChannelTestTaskRequest { task_id }).await {
            warn!(%logged_task_id, ?err, "channel test system task failed");
        }
    });
}

fn spawn_periodic_channel_test_task(
    tasks: SystemTaskStore,
    management: ManagementStore,
    options: OptionStore,
    snapshots: MemorySnapshotBus,
    proxies: ProxyStore,
    period: Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        info!(?period, "channel_test scheduler started");
        loop {
            ticker.tick().await;
            match enqueue_channel_test_task(&tasks, ChannelTestTaskPayload::default()).await {
                Ok(Some(task_id)) => {
                    spawn_channel_test_task(
                        tasks.clone(),
                        management.clone(),
                        options.clone(),
                        snapshots.clone(),
                        proxies.clone(),
                        task_id,
                    );
                }
                Ok(None) => debug!("channel_test scheduler skipped active task"),
                Err(err) => warn!(?err, "channel_test scheduler enqueue failed"),
            }
        }
    });
}

pub(crate) fn spawn_model_update_task(
    tasks: SystemTaskStore,
    management: ManagementStore,
    options: OptionStore,
    snapshots: MemorySnapshotBus,
    proxies: ProxyStore,
    task_id: String,
) {
    tokio::spawn(async move {
        let logged_task_id = task_id.clone();
        let runner = ModelUpdateTaskRunner::new(tasks, management, options, snapshots, proxies);
        if let Err(err) = runner.call(RunModelUpdateTaskRequest { task_id }).await {
            warn!(%logged_task_id, ?err, "model update system task failed");
        }
    });
}

fn spawn_periodic_model_update_task(
    tasks: SystemTaskStore,
    management: ManagementStore,
    options: OptionStore,
    snapshots: MemorySnapshotBus,
    proxies: ProxyStore,
    period: Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        info!(?period, "model_update scheduler started");
        loop {
            ticker.tick().await;
            match enqueue_model_update_task(&tasks, ModelUpdateTaskPayload::default()).await {
                Ok(Some(task_id)) => {
                    spawn_model_update_task(
                        tasks.clone(),
                        management.clone(),
                        options.clone(),
                        snapshots.clone(),
                        proxies.clone(),
                        task_id,
                    );
                }
                Ok(None) => debug!("model_update scheduler skipped active task"),
                Err(err) => warn!(?err, "model_update scheduler enqueue failed"),
            }
        }
    });
}

async fn enqueue_channel_test_task(
    tasks: &SystemTaskStore,
    payload: ChannelTestTaskPayload,
) -> Result<Option<String>, ManagementError> {
    let enqueued = tasks
        .call(EnqueueSystemTaskRequest {
            task_type: SYSTEM_TASK_TYPE_CHANNEL_TEST,
            payload:   Some(json!(payload)),
            state:     Some(json!(SystemTaskProgressState::default())),
        })
        .await?;
    if enqueued.created || enqueued.task.status == SystemTaskStatus::Pending {
        return Ok(Some(enqueued.task.task_id));
    }
    Ok(None)
}

async fn enqueue_model_update_task(
    tasks: &SystemTaskStore,
    payload: ModelUpdateTaskPayload,
) -> Result<Option<String>, ManagementError> {
    let enqueued = tasks
        .call(EnqueueSystemTaskRequest {
            task_type: SYSTEM_TASK_TYPE_MODEL_UPDATE,
            payload:   Some(json!(payload)),
            state:     Some(json!(SystemTaskProgressState::default())),
        })
        .await?;
    if enqueued.created || enqueued.task.status == SystemTaskStatus::Pending {
        return Ok(Some(enqueued.task.task_id));
    }
    Ok(None)
}

#[derive(Debug, Clone)]
struct RunChannelTestTaskRequest {
    task_id: String,
}

#[derive(Debug, Clone)]
struct RunModelUpdateTaskRequest {
    task_id: String,
}

#[derive(Debug, Clone)]
struct ChannelTestTaskRunner {
    tasks:      SystemTaskStore,
    management: ManagementStore,
    options:    OptionStore,
    snapshots:  MemorySnapshotBus,
    proxies:    ProxyStore,
    runner_id:  String,
}

impl ChannelTestTaskRunner {
    fn new(
        tasks: SystemTaskStore,
        management: ManagementStore,
        options: OptionStore,
        snapshots: MemorySnapshotBus,
        proxies: ProxyStore,
    ) -> Self {
        Self {
            tasks,
            management,
            options,
            snapshots,
            proxies,
            runner_id: format!("halolake-{}", Uuid::new_v4().simple()),
        }
    }
}

impl Service<RunChannelTestTaskRequest> for ChannelTestTaskRunner {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: RunChannelTestTaskRequest) -> Result<Self::Response, Self::Error> {
        let Some(task) = self
            .tasks
            .call(ClaimSystemTaskRequest {
                task_id:   req.task_id,
                task_type: SYSTEM_TASK_TYPE_CHANNEL_TEST,
                runner_id: self.runner_id.clone(),
            })
            .await?
        else {
            return Ok(());
        };

        let task_id = task.task_id.clone();
        match self.run_claimed(task).await {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = self.finish_failed(&task_id, &err.to_string()).await;
                Err(err)
            }
        }
    }
}

impl ChannelTestTaskRunner {
    async fn run_claimed(&self, task: SystemTaskRecord) -> Result<(), ManagementError> {
        let payload = decode_payload::<ChannelTestTaskPayload>(&task)?.unwrap_or_default();
        let mut progress = SystemTaskProgressReporter::new(
            self.tasks.clone(),
            task.task_id.clone(),
            self.runner_id.clone(),
        );
        let service = ChannelOpsService::new(self.management.clone());
        let result = service
            .test_all_channels_with_progress(
                TestAllChannelsRequest {
                    stream: payload.stream,
                },
                &mut progress,
            )
            .await?;
        publish_enriched_management_snapshot(
            &self.management,
            &self.options,
            &self.snapshots,
            &self.proxies,
        )
        .await?;
        self.finish_succeeded(&task.task_id, json!(result)).await?;
        Ok(())
    }

    async fn finish_succeeded(
        &self,
        task_id: &str,
        result: JsonValue,
    ) -> Result<(), ManagementError> {
        self.tasks
            .call(FinishSystemTaskRequest {
                task_id:   task_id.to_string(),
                runner_id: self.runner_id.clone(),
                status:    SystemTaskStatus::Succeeded,
                result:    Some(result),
                error:     String::new(),
            })
            .await?;
        Ok(())
    }

    async fn finish_failed(&self, task_id: &str, error: &str) -> Result<(), ManagementError> {
        self.tasks
            .call(FinishSystemTaskRequest {
                task_id:   task_id.to_string(),
                runner_id: self.runner_id.clone(),
                status:    SystemTaskStatus::Failed,
                result:    None,
                error:     error.to_string(),
            })
            .await?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ModelUpdateTaskRunner {
    tasks:      SystemTaskStore,
    management: ManagementStore,
    options:    OptionStore,
    snapshots:  MemorySnapshotBus,
    proxies:    ProxyStore,
    runner_id:  String,
}

impl ModelUpdateTaskRunner {
    fn new(
        tasks: SystemTaskStore,
        management: ManagementStore,
        options: OptionStore,
        snapshots: MemorySnapshotBus,
        proxies: ProxyStore,
    ) -> Self {
        Self {
            tasks,
            management,
            options,
            snapshots,
            proxies,
            runner_id: format!("halolake-{}", Uuid::new_v4().simple()),
        }
    }
}

impl Service<RunModelUpdateTaskRequest> for ModelUpdateTaskRunner {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: RunModelUpdateTaskRequest) -> Result<Self::Response, Self::Error> {
        let Some(task) = self
            .tasks
            .call(ClaimSystemTaskRequest {
                task_id:   req.task_id,
                task_type: SYSTEM_TASK_TYPE_MODEL_UPDATE,
                runner_id: self.runner_id.clone(),
            })
            .await?
        else {
            return Ok(());
        };

        let task_id = task.task_id.clone();
        match self.run_claimed(task).await {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = self.finish_failed(&task_id, &err.to_string()).await;
                Err(err)
            }
        }
    }
}

impl ModelUpdateTaskRunner {
    async fn run_claimed(&self, task: SystemTaskRecord) -> Result<(), ManagementError> {
        let _payload = decode_payload::<ModelUpdateTaskPayload>(&task)?.unwrap_or_default();
        let mut progress = SystemTaskProgressReporter::new(
            self.tasks.clone(),
            task.task_id.clone(),
            self.runner_id.clone(),
        );
        let service = ChannelOpsService::new(self.management.clone());
        let result = service
            .detect_all_upstream_model_updates_with_progress(
                DetectAllChannelUpstreamModelUpdatesRequest,
                &mut progress,
            )
            .await?;
        publish_enriched_management_snapshot(
            &self.management,
            &self.options,
            &self.snapshots,
            &self.proxies,
        )
        .await?;
        self.finish_succeeded(&task.task_id, json!(result)).await?;
        Ok(())
    }

    async fn finish_succeeded(
        &self,
        task_id: &str,
        result: JsonValue,
    ) -> Result<(), ManagementError> {
        self.tasks
            .call(FinishSystemTaskRequest {
                task_id:   task_id.to_string(),
                runner_id: self.runner_id.clone(),
                status:    SystemTaskStatus::Succeeded,
                result:    Some(result),
                error:     String::new(),
            })
            .await?;
        Ok(())
    }

    async fn finish_failed(&self, task_id: &str, error: &str) -> Result<(), ManagementError> {
        self.tasks
            .call(FinishSystemTaskRequest {
                task_id:   task_id.to_string(),
                runner_id: self.runner_id.clone(),
                status:    SystemTaskStatus::Failed,
                result:    None,
                error:     error.to_string(),
            })
            .await?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct SystemTaskProgressReporter {
    tasks:         SystemTaskStore,
    task_id:       String,
    runner_id:     String,
    last_write_at: Option<Instant>,
    last_progress: Option<usize>,
}

impl SystemTaskProgressReporter {
    fn new(tasks: SystemTaskStore, task_id: String, runner_id: String) -> Self {
        Self {
            tasks,
            task_id,
            runner_id,
            last_write_at: None,
            last_progress: None,
        }
    }
}

impl ChannelOpsProgress for SystemTaskProgressReporter {
    async fn report(&mut self, processed: usize, total: usize) -> Result<(), ManagementError> {
        let progress = progress_percent(processed, total);
        if progress < 100 {
            if self.last_progress == Some(progress) {
                return Ok(());
            }
            if self
                .last_write_at
                .is_some_and(|last_write_at| last_write_at.elapsed() < PROGRESS_WRITE_INTERVAL)
            {
                return Ok(());
            }
        }

        self.last_progress = Some(progress);
        self.last_write_at = Some(Instant::now());
        self.tasks
            .call(UpdateSystemTaskStateRequest {
                task_id:   self.task_id.clone(),
                runner_id: self.runner_id.clone(),
                state:     json!(SystemTaskProgressState {
                    total,
                    processed,
                    progress,
                }),
            })
            .await?;
        Ok(())
    }
}

fn progress_percent(processed: usize, total: usize) -> usize {
    if total == 0 {
        return 100;
    }
    processed.saturating_mul(100).min(total.saturating_mul(100)) / total
}

fn decode_payload<T>(task: &SystemTaskRecord) -> Result<Option<T>, ManagementError>
where
    T: for<'de> Deserialize<'de>,
{
    task.payload
        .clone()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|err| {
            ManagementError::Storage(format!(
                "invalid system task payload {}: {err}",
                task.task_id
            ))
        })
}

impl Default for ChannelTestTaskPayload {
    fn default() -> Self {
        Self {
            mode:   String::new(),
            notify: false,
            stream: false,
        }
    }
}

impl Default for ModelUpdateTaskPayload {
    fn default() -> Self {
        Self {
            mode: String::new(),
        }
    }
}
