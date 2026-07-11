use std::{
    str::FromStr,
    sync::{Arc, RwLock},
};

use halolake_control_plane::ManagementError;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use service_async::Service;
use sqlx::{
    MySqlPool, PgPool, Row, SqlitePool,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
    postgres::{PgConnectOptions, PgPoolOptions},
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use tracing::warn;
use uuid::Uuid;

use crate::storage::{DeleteUsageBeforeRequest, UsageStore};

pub(crate) const SYSTEM_TASK_TYPE_LOG_CLEANUP: &str = "log_cleanup";
pub(crate) const SYSTEM_TASK_TYPE_CHANNEL_TEST: &str = "channel_test";
pub(crate) const SYSTEM_TASK_TYPE_MODEL_UPDATE: &str = "model_update";

const LOG_CLEANUP_BATCH_SIZE: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SystemTaskStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
}

impl SystemTaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    fn is_active(self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }
}

impl FromStr for SystemTaskStatus {
    type Err = ManagementError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            _ => Err(ManagementError::Storage(format!(
                "unknown system task status: {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SystemTaskRecord {
    pub(crate) id: i64,
    pub(crate) task_id: String,
    #[serde(rename = "type")]
    pub(crate) task_type: String,
    pub(crate) status: SystemTaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) active_key: Option<String>,
    pub(crate) payload: Option<JsonValue>,
    pub(crate) state: Option<JsonValue>,
    pub(crate) result: Option<JsonValue>,
    pub(crate) error: String,
    pub(crate) locked_by: String,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct StartLogCleanupTaskRequest {
    pub(crate) target_timestamp: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct StartSystemTaskRequest {
    pub(crate) task_type: &'static str,
    pub(crate) payload: Option<JsonValue>,
    pub(crate) state: Option<JsonValue>,
}

#[derive(Debug, Clone)]
pub(crate) struct EnqueueSystemTaskRequest {
    pub(crate) task_type: &'static str,
    pub(crate) payload: Option<JsonValue>,
    pub(crate) state: Option<JsonValue>,
}

#[derive(Debug, Clone)]
pub(crate) struct EnqueueSystemTaskResponse {
    pub(crate) task: SystemTaskRecord,
    pub(crate) created: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GetCurrentSystemTaskRequest {
    pub(crate) task_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GetSystemTaskRequest {
    pub(crate) task_id: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct ListSystemTasksRequest {
    pub(crate) limit: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ClaimSystemTaskRequest {
    pub(crate) task_id: String,
    pub(crate) task_type: &'static str,
    pub(crate) runner_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct UpdateSystemTaskStateRequest {
    pub(crate) task_id: String,
    pub(crate) runner_id: String,
    pub(crate) state: JsonValue,
}

#[derive(Debug, Clone)]
pub(crate) struct FinishSystemTaskRequest {
    pub(crate) task_id: String,
    pub(crate) runner_id: String,
    pub(crate) status: SystemTaskStatus,
    pub(crate) result: Option<JsonValue>,
    pub(crate) error: String,
}

#[derive(Debug, Clone)]
struct RunLogCleanupTaskRequest {
    task_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct LogCleanupPayload {
    target_timestamp: i64,
    batch_size: usize,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct LogCleanupState {
    total: i64,
    processed: i64,
    progress: i64,
    remaining: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct LogCleanupResult {
    deleted_count: i64,
}

#[derive(Debug, Clone)]
pub(crate) enum SystemTaskStore {
    Memory(MemorySystemTaskStore),
    Sqlite(SqliteSystemTaskStore),
    MySql(MySqlSystemTaskStore),
    Postgres(PostgresSystemTaskStore),
}

impl SystemTaskStore {
    pub(crate) fn memory() -> Self {
        Self::Memory(MemorySystemTaskStore::default())
    }

    pub(crate) async fn sqlite(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Sqlite(SqliteSystemTaskStore::connect(url).await?))
    }

    pub(crate) async fn mysql(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::MySql(MySqlSystemTaskStore::connect(url).await?))
    }

    pub(crate) async fn postgres(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Postgres(PostgresSystemTaskStore::connect(url).await?))
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MemorySystemTaskStore {
    inner: Arc<RwLock<MemorySystemTaskState>>,
}

#[derive(Debug, Default)]
struct MemorySystemTaskState {
    next_id: i64,
    tasks: Vec<SystemTaskRecord>,
}

impl MemorySystemTaskStore {
    fn from_tasks(tasks: Vec<SystemTaskRecord>) -> Self {
        let next_id = tasks
            .iter()
            .map(|task| task.id)
            .max()
            .unwrap_or_default()
            .saturating_add(1);
        Self {
            inner: Arc::new(RwLock::new(MemorySystemTaskState { next_id, tasks })),
        }
    }
}

impl Service<StartLogCleanupTaskRequest> for SystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: StartLogCleanupTaskRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl Service<StartSystemTaskRequest> for SystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: StartSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl Service<EnqueueSystemTaskRequest> for SystemTaskStore {
    type Response = EnqueueSystemTaskResponse;
    type Error = ManagementError;

    async fn call(&self, req: EnqueueSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl Service<GetCurrentSystemTaskRequest> for SystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: GetCurrentSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl Service<GetSystemTaskRequest> for SystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: GetSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl Service<ListSystemTasksRequest> for SystemTaskStore {
    type Response = Vec<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListSystemTasksRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl Service<ClaimSystemTaskRequest> for SystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ClaimSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl Service<UpdateSystemTaskStateRequest> for SystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateSystemTaskStateRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl Service<FinishSystemTaskRequest> for SystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: FinishSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl Service<StartLogCleanupTaskRequest> for MemorySystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: StartLogCleanupTaskRequest) -> Result<Self::Response, Self::Error> {
        if req.target_timestamp <= 0 {
            return Err(ManagementError::InvalidRequest(
                "target timestamp is required",
            ));
        }

        let payload = LogCleanupPayload {
            target_timestamp: req.target_timestamp,
            batch_size: LOG_CLEANUP_BATCH_SIZE,
        };
        self.call(StartSystemTaskRequest {
            task_type: SYSTEM_TASK_TYPE_LOG_CLEANUP,
            payload: Some(json!(payload)),
            state: Some(json!(LogCleanupState::default())),
        })
        .await
    }
}

impl Service<StartSystemTaskRequest> for MemorySystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: StartSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        Ok(self
            .call(EnqueueSystemTaskRequest {
                task_type: req.task_type,
                payload: req.payload,
                state: req.state,
            })
            .await?
            .task)
    }
}

impl Service<EnqueueSystemTaskRequest> for MemorySystemTaskStore {
    type Response = EnqueueSystemTaskResponse;
    type Error = ManagementError;

    async fn call(&self, req: EnqueueSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        if req.task_type.trim().is_empty() {
            return Err(ManagementError::InvalidRequest(
                "system task type is required",
            ));
        }
        let mut state = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("system_tasks"))?;
        if let Some(active) = latest_active_task(&state.tasks, req.task_type) {
            return Ok(EnqueueSystemTaskResponse {
                task: active.clone(),
                created: false,
            });
        }

        let now = now_unix();
        let id = if state.next_id <= 0 { 1 } else { state.next_id };
        state.next_id = id.saturating_add(1);
        let task = SystemTaskRecord {
            id,
            task_id: format!("systask_{}", Uuid::new_v4().simple()),
            task_type: req.task_type.to_string(),
            status: SystemTaskStatus::Pending,
            active_key: Some(req.task_type.to_string()),
            payload: req.payload,
            state: req.state,
            result: None,
            error: String::new(),
            locked_by: String::new(),
            created_at: now,
            updated_at: now,
        };
        state.tasks.push(task.clone());
        Ok(EnqueueSystemTaskResponse {
            task,
            created: true,
        })
    }
}

impl Service<GetCurrentSystemTaskRequest> for MemorySystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: GetCurrentSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        let state = self
            .inner
            .read()
            .map_err(|_| ManagementError::Poisoned("system_tasks"))?;
        Ok(latest_active_task(&state.tasks, &req.task_type).cloned())
    }
}

impl Service<GetSystemTaskRequest> for MemorySystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: GetSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        let state = self
            .inner
            .read()
            .map_err(|_| ManagementError::Poisoned("system_tasks"))?;
        Ok(state
            .tasks
            .iter()
            .find(|task| task.task_id == req.task_id)
            .cloned())
    }
}

impl Service<ListSystemTasksRequest> for MemorySystemTaskStore {
    type Response = Vec<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListSystemTasksRequest) -> Result<Self::Response, Self::Error> {
        let limit = if req.limit == 0 {
            20
        } else {
            req.limit.min(100)
        };
        let state = self
            .inner
            .read()
            .map_err(|_| ManagementError::Poisoned("system_tasks"))?;
        let mut tasks = state.tasks.clone();
        tasks.sort_by(|left, right| right.id.cmp(&left.id));
        tasks.truncate(limit);
        Ok(tasks)
    }
}

impl Service<ClaimSystemTaskRequest> for MemorySystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ClaimSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        let mut state = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("system_tasks"))?;
        let Some(task) = state
            .tasks
            .iter_mut()
            .find(|task| task.task_id == req.task_id && task.task_type == req.task_type)
        else {
            return Ok(None);
        };
        if task.status != SystemTaskStatus::Pending {
            return Ok(None);
        }
        task.status = SystemTaskStatus::Running;
        task.locked_by = req.runner_id;
        task.updated_at = now_unix();
        Ok(Some(task.clone()))
    }
}

impl Service<UpdateSystemTaskStateRequest> for MemorySystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateSystemTaskStateRequest) -> Result<Self::Response, Self::Error> {
        let mut state = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("system_tasks"))?;
        let task = running_task_mut(&mut state.tasks, &req.task_id, &req.runner_id)?;
        task.state = Some(req.state);
        task.updated_at = now_unix();
        Ok(task.clone())
    }
}

impl Service<FinishSystemTaskRequest> for MemorySystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: FinishSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        let mut state = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("system_tasks"))?;
        let task = running_task_mut(&mut state.tasks, &req.task_id, &req.runner_id)?;
        task.status = req.status;
        task.active_key = None;
        task.result = req.result;
        task.error = req.error;
        task.updated_at = now_unix();
        Ok(task.clone())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SqliteSystemTaskStore {
    pool: SqlitePool,
    memory: MemorySystemTaskStore,
}

impl SqliteSystemTaskStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = SqliteConnectOptions::from_str(url)
            .map_err(storage_err)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_system_tasks(&pool).await?;
        let tasks = load_system_tasks(&pool).await?;
        Ok(Self {
            pool,
            memory: MemorySystemTaskStore::from_tasks(tasks),
        })
    }

    async fn persist_task(&self, task: &SystemTaskRecord) -> Result<(), ManagementError> {
        sqlx::query(
            "INSERT INTO system_tasks (
                id, task_id, task_type, status, active_key, payload, state, result,
                error, locked_by, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(task_id) DO UPDATE SET
                task_type = excluded.task_type,
                status = excluded.status,
                active_key = excluded.active_key,
                payload = excluded.payload,
                state = excluded.state,
                result = excluded.result,
                error = excluded.error,
                locked_by = excluded.locked_by,
                updated_at = excluded.updated_at",
        )
        .bind(task.id)
        .bind(&task.task_id)
        .bind(&task.task_type)
        .bind(task.status.as_str())
        .bind(&task.active_key)
        .bind(json_text(&task.payload)?)
        .bind(json_text(&task.state)?)
        .bind(json_text(&task.result)?)
        .bind(&task.error)
        .bind(&task.locked_by)
        .bind(task.created_at)
        .bind(task.updated_at)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }
}

impl Service<StartLogCleanupTaskRequest> for SqliteSystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: StartLogCleanupTaskRequest) -> Result<Self::Response, Self::Error> {
        let task = self.memory.call(req).await?;
        self.persist_task(&task).await?;
        Ok(task)
    }
}

impl Service<StartSystemTaskRequest> for SqliteSystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: StartSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        Ok(self
            .call(EnqueueSystemTaskRequest {
                task_type: req.task_type,
                payload: req.payload,
                state: req.state,
            })
            .await?
            .task)
    }
}

impl Service<EnqueueSystemTaskRequest> for SqliteSystemTaskStore {
    type Response = EnqueueSystemTaskResponse;
    type Error = ManagementError;

    async fn call(&self, req: EnqueueSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        let response = self.memory.call(req).await?;
        if response.created {
            self.persist_task(&response.task).await?;
        }
        Ok(response)
    }
}

impl Service<GetCurrentSystemTaskRequest> for SqliteSystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: GetCurrentSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<GetSystemTaskRequest> for SqliteSystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: GetSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<ListSystemTasksRequest> for SqliteSystemTaskStore {
    type Response = Vec<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListSystemTasksRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<ClaimSystemTaskRequest> for SqliteSystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ClaimSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        let task = self.memory.call(req).await?;
        if let Some(task) = &task {
            self.persist_task(task).await?;
        }
        Ok(task)
    }
}

impl Service<UpdateSystemTaskStateRequest> for SqliteSystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateSystemTaskStateRequest) -> Result<Self::Response, Self::Error> {
        let task = self.memory.call(req).await?;
        self.persist_task(&task).await?;
        Ok(task)
    }
}

impl Service<FinishSystemTaskRequest> for SqliteSystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: FinishSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        let task = self.memory.call(req).await?;
        self.persist_task(&task).await?;
        Ok(task)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MySqlSystemTaskStore {
    pool: MySqlPool,
    memory: MemorySystemTaskStore,
}

impl MySqlSystemTaskStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = MySqlConnectOptions::from_str(url)
            .map_err(storage_err)?;
        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_system_tasks_mysql(&pool).await?;
        let tasks = load_system_tasks_mysql(&pool).await?;
        Ok(Self {
            pool,
            memory: MemorySystemTaskStore::from_tasks(tasks),
        })
    }

    async fn persist_task(&self, task: &SystemTaskRecord) -> Result<(), ManagementError> {
        sqlx::query(
            "INSERT INTO system_tasks (
                id, task_id, task_type, status, active_key, payload, state, result,
                error, locked_by, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE task_type = VALUES(task_type),
                status = VALUES(status),
                active_key = VALUES(active_key),
                payload = VALUES(payload),
                state = VALUES(state),
                result = VALUES(result),
                error = VALUES(error),
                locked_by = VALUES(locked_by),
                updated_at = VALUES(updated_at)",
        )
        .bind(task.id)
        .bind(&task.task_id)
        .bind(&task.task_type)
        .bind(task.status.as_str())
        .bind(&task.active_key)
        .bind(json_text(&task.payload)?)
        .bind(json_text(&task.state)?)
        .bind(json_text(&task.result)?)
        .bind(&task.error)
        .bind(&task.locked_by)
        .bind(task.created_at)
        .bind(task.updated_at)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }
}

impl Service<StartLogCleanupTaskRequest> for MySqlSystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: StartLogCleanupTaskRequest) -> Result<Self::Response, Self::Error> {
        let task = self.memory.call(req).await?;
        self.persist_task(&task).await?;
        Ok(task)
    }
}

impl Service<StartSystemTaskRequest> for MySqlSystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: StartSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        Ok(self
            .call(EnqueueSystemTaskRequest {
                task_type: req.task_type,
                payload: req.payload,
                state: req.state,
            })
            .await?
            .task)
    }
}

impl Service<EnqueueSystemTaskRequest> for MySqlSystemTaskStore {
    type Response = EnqueueSystemTaskResponse;
    type Error = ManagementError;

    async fn call(&self, req: EnqueueSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        let response = self.memory.call(req).await?;
        if response.created {
            self.persist_task(&response.task).await?;
        }
        Ok(response)
    }
}

impl Service<GetCurrentSystemTaskRequest> for MySqlSystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: GetCurrentSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<GetSystemTaskRequest> for MySqlSystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: GetSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<ListSystemTasksRequest> for MySqlSystemTaskStore {
    type Response = Vec<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListSystemTasksRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<ClaimSystemTaskRequest> for MySqlSystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ClaimSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        let task = self.memory.call(req).await?;
        if let Some(task) = &task {
            self.persist_task(task).await?;
        }
        Ok(task)
    }
}

impl Service<UpdateSystemTaskStateRequest> for MySqlSystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateSystemTaskStateRequest) -> Result<Self::Response, Self::Error> {
        let task = self.memory.call(req).await?;
        self.persist_task(&task).await?;
        Ok(task)
    }
}

impl Service<FinishSystemTaskRequest> for MySqlSystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: FinishSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        let task = self.memory.call(req).await?;
        self.persist_task(&task).await?;
        Ok(task)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresSystemTaskStore {
    pool: PgPool,
    memory: MemorySystemTaskStore,
}

impl PostgresSystemTaskStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = PgConnectOptions::from_str(url).map_err(storage_err)?;
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_system_tasks_pg(&pool).await?;
        let tasks = load_system_tasks_pg(&pool).await?;
        Ok(Self {
            pool,
            memory: MemorySystemTaskStore::from_tasks(tasks),
        })
    }

    async fn persist_task(&self, task: &SystemTaskRecord) -> Result<(), ManagementError> {
        sqlx::query(
            "INSERT INTO system_tasks (
                id, task_id, task_type, status, active_key, payload, state, result,
                error, locked_by, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             ON CONFLICT(task_id) DO UPDATE SET
                task_type = excluded.task_type,
                status = excluded.status,
                active_key = excluded.active_key,
                payload = excluded.payload,
                state = excluded.state,
                result = excluded.result,
                error = excluded.error,
                locked_by = excluded.locked_by,
                updated_at = excluded.updated_at",
        )
        .bind(task.id)
        .bind(&task.task_id)
        .bind(&task.task_type)
        .bind(task.status.as_str())
        .bind(&task.active_key)
        .bind(json_text(&task.payload)?)
        .bind(json_text(&task.state)?)
        .bind(json_text(&task.result)?)
        .bind(&task.error)
        .bind(&task.locked_by)
        .bind(task.created_at)
        .bind(task.updated_at)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }
}

impl Service<StartLogCleanupTaskRequest> for PostgresSystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: StartLogCleanupTaskRequest) -> Result<Self::Response, Self::Error> {
        let task = self.memory.call(req).await?;
        self.persist_task(&task).await?;
        Ok(task)
    }
}

impl Service<StartSystemTaskRequest> for PostgresSystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: StartSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        Ok(self
            .call(EnqueueSystemTaskRequest {
                task_type: req.task_type,
                payload: req.payload,
                state: req.state,
            })
            .await?
            .task)
    }
}

impl Service<EnqueueSystemTaskRequest> for PostgresSystemTaskStore {
    type Response = EnqueueSystemTaskResponse;
    type Error = ManagementError;

    async fn call(&self, req: EnqueueSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        let response = self.memory.call(req).await?;
        if response.created {
            self.persist_task(&response.task).await?;
        }
        Ok(response)
    }
}

impl Service<GetCurrentSystemTaskRequest> for PostgresSystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: GetCurrentSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<GetSystemTaskRequest> for PostgresSystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: GetSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<ListSystemTasksRequest> for PostgresSystemTaskStore {
    type Response = Vec<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListSystemTasksRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<ClaimSystemTaskRequest> for PostgresSystemTaskStore {
    type Response = Option<SystemTaskRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ClaimSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        let task = self.memory.call(req).await?;
        if let Some(task) = &task {
            self.persist_task(task).await?;
        }
        Ok(task)
    }
}

impl Service<UpdateSystemTaskStateRequest> for PostgresSystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateSystemTaskStateRequest) -> Result<Self::Response, Self::Error> {
        let task = self.memory.call(req).await?;
        self.persist_task(&task).await?;
        Ok(task)
    }
}

impl Service<FinishSystemTaskRequest> for PostgresSystemTaskStore {
    type Response = SystemTaskRecord;
    type Error = ManagementError;

    async fn call(&self, req: FinishSystemTaskRequest) -> Result<Self::Response, Self::Error> {
        let task = self.memory.call(req).await?;
        self.persist_task(&task).await?;
        Ok(task)
    }
}

pub(crate) fn spawn_log_cleanup_task(
    tasks: SystemTaskStore,
    usage_events: UsageStore,
    task_id: String,
) {
    tokio::spawn(async move {
        let runner = LogCleanupTaskRunner::new(tasks, usage_events);
        let logged_task_id = task_id.clone();
        if let Err(err) = runner.call(RunLogCleanupTaskRequest { task_id }).await {
            warn!(%logged_task_id, ?err, "log cleanup system task failed");
        }
    });
}

#[derive(Debug, Clone)]
struct LogCleanupTaskRunner {
    tasks: SystemTaskStore,
    usage_events: UsageStore,
    runner_id: String,
}

impl LogCleanupTaskRunner {
    fn new(tasks: SystemTaskStore, usage_events: UsageStore) -> Self {
        Self {
            tasks,
            usage_events,
            runner_id: format!("halolake-{}", Uuid::new_v4().simple()),
        }
    }
}

impl Service<RunLogCleanupTaskRequest> for LogCleanupTaskRunner {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: RunLogCleanupTaskRequest) -> Result<Self::Response, Self::Error> {
        let Some(task) = self
            .tasks
            .call(ClaimSystemTaskRequest {
                task_id: req.task_id,
                task_type: SYSTEM_TASK_TYPE_LOG_CLEANUP,
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
                let _ = self.finish_failed(&err.to_string(), &task_id).await;
                Err(err)
            }
        }
    }
}

impl LogCleanupTaskRunner {
    async fn run_claimed(&self, task: SystemTaskRecord) -> Result<(), ManagementError> {
        let payload = decode_log_cleanup_payload(&task)?;
        if payload.target_timestamp <= 0 {
            self.finish_failed("target timestamp is required", &task.task_id)
                .await?;
            return Ok(());
        }

        let total = self
            .usage_events
            .count_before_unix_seconds(payload.target_timestamp)
            .map_err(usage_to_management)? as i64;
        self.update_state(
            &task.task_id,
            LogCleanupState {
                total,
                processed: 0,
                progress: log_cleanup_progress(0, total),
                remaining: total,
            },
        )
        .await?;

        let deleted = self
            .usage_events
            .call(DeleteUsageBeforeRequest {
                target_timestamp: payload.target_timestamp,
            })
            .await
            .map_err(usage_to_management)?
            .deleted as i64;
        if total > 0 && deleted == 0 {
            self.finish_failed("no log rows were deleted", &task.task_id)
                .await?;
            return Ok(());
        }

        let remaining = total.saturating_sub(deleted);
        self.update_state(
            &task.task_id,
            LogCleanupState {
                total: total.max(deleted),
                processed: deleted,
                progress: log_cleanup_progress(deleted, total.max(deleted)),
                remaining,
            },
        )
        .await?;
        self.tasks
            .call(FinishSystemTaskRequest {
                task_id: task.task_id,
                runner_id: self.runner_id.clone(),
                status: SystemTaskStatus::Succeeded,
                result: Some(json!(LogCleanupResult {
                    deleted_count: deleted,
                })),
                error: String::new(),
            })
            .await?;
        Ok(())
    }

    async fn update_state(
        &self,
        task_id: &str,
        state: LogCleanupState,
    ) -> Result<(), ManagementError> {
        self.tasks
            .call(UpdateSystemTaskStateRequest {
                task_id: task_id.to_string(),
                runner_id: self.runner_id.clone(),
                state: json!(state),
            })
            .await?;
        Ok(())
    }

    async fn finish_failed(&self, error: &str, task_id: &str) -> Result<(), ManagementError> {
        self.tasks
            .call(FinishSystemTaskRequest {
                task_id: task_id.to_string(),
                runner_id: self.runner_id.clone(),
                status: SystemTaskStatus::Failed,
                result: None,
                error: error.to_string(),
            })
            .await?;
        Ok(())
    }
}

fn latest_active_task<'a>(
    tasks: &'a [SystemTaskRecord],
    task_type: &str,
) -> Option<&'a SystemTaskRecord> {
    tasks
        .iter()
        .filter(|task| task.task_type == task_type && task.status.is_active())
        .max_by_key(|task| task.id)
}

fn running_task_mut<'a>(
    tasks: &'a mut [SystemTaskRecord],
    task_id: &str,
    runner_id: &str,
) -> Result<&'a mut SystemTaskRecord, ManagementError> {
    tasks
        .iter_mut()
        .find(|task| {
            task.task_id == task_id
                && task.status == SystemTaskStatus::Running
                && task.locked_by == runner_id
        })
        .ok_or_else(|| {
            ManagementError::Storage(format!("system task {task_id} lock lost or not running"))
        })
}

fn decode_log_cleanup_payload(
    task: &SystemTaskRecord,
) -> Result<LogCleanupPayload, ManagementError> {
    let Some(payload) = &task.payload else {
        return Err(ManagementError::InvalidRequest(
            "log cleanup payload is required",
        ));
    };
    serde_json::from_value(payload.clone()).map_err(|err| {
        ManagementError::Storage(format!(
            "invalid log cleanup payload {}: {err}",
            task.task_id
        ))
    })
}

fn log_cleanup_progress(processed: i64, total: i64) -> i64 {
    if total <= 0 {
        return 100;
    }
    if processed <= 0 {
        return 0;
    }
    if processed >= total {
        return 100;
    }
    processed.saturating_mul(100) / total
}

async fn migrate_system_tasks(pool: &SqlitePool) -> Result<(), ManagementError> {
    for stmt in [
        "CREATE TABLE IF NOT EXISTS system_tasks (
            id INTEGER PRIMARY KEY,
            task_id TEXT NOT NULL UNIQUE,
            task_type TEXT NOT NULL,
            status TEXT NOT NULL,
            active_key TEXT UNIQUE,
            payload TEXT NOT NULL DEFAULT '',
            state TEXT NOT NULL DEFAULT '',
            result TEXT NOT NULL DEFAULT '',
            error TEXT NOT NULL DEFAULT '',
            locked_by TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )",
        "CREATE INDEX IF NOT EXISTS idx_system_tasks_type_status ON system_tasks(task_type, status)",
        "CREATE INDEX IF NOT EXISTS idx_system_tasks_updated_at ON system_tasks(updated_at)",
    ] {
        sqlx::query(stmt).execute(pool).await.map_err(storage_err)?;
    }
    Ok(())
}

async fn migrate_system_tasks_mysql(pool: &MySqlPool) -> Result<(), ManagementError> {
    for stmt in [
        "CREATE TABLE IF NOT EXISTS system_tasks (
            id BIGINT PRIMARY KEY,
            task_id TEXT NOT NULL UNIQUE,
            task_type TEXT NOT NULL,
            status TEXT NOT NULL,
            active_key TEXT UNIQUE,
            payload TEXT NOT NULL DEFAULT '',
            state TEXT NOT NULL DEFAULT '',
            result TEXT NOT NULL DEFAULT '',
            error TEXT NOT NULL DEFAULT '',
            locked_by TEXT NOT NULL DEFAULT '',
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL
        )",
        "CREATE INDEX IF NOT EXISTS idx_system_tasks_type_status ON system_tasks(task_type, status)",
        "CREATE INDEX IF NOT EXISTS idx_system_tasks_updated_at ON system_tasks(updated_at)",
    ] {
        sqlx::query(stmt).execute(pool).await.map_err(storage_err)?;
    }
    Ok(())
}

async fn load_system_tasks(pool: &SqlitePool) -> Result<Vec<SystemTaskRecord>, ManagementError> {
    sqlx::query(
        "SELECT id, task_id, task_type, status, active_key, payload, state, result,
            error, locked_by, created_at, updated_at
         FROM system_tasks ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(SystemTaskRecord {
            id: i64_col_sqlite(&row, "id")?,
            task_id: string_col_sqlite(&row, "task_id")?,
            task_type: string_col_sqlite(&row, "task_type")?,
            status: SystemTaskStatus::from_str(&string_col_sqlite(&row, "status")?)?,
            active_key: opt_string_col_sqlite(&row, "active_key")?,
            payload: json_col_sqlite(&row, "payload")?,
            state: json_col_sqlite(&row, "state")?,
            result: json_col_sqlite(&row, "result")?,
            error: string_col_sqlite(&row, "error")?,
            locked_by: string_col_sqlite(&row, "locked_by")?,
            created_at: i64_col_sqlite(&row, "created_at")?,
            updated_at: i64_col_sqlite(&row, "updated_at")?,
        })
    })
    .collect()
}

async fn load_system_tasks_mysql(pool: &MySqlPool) -> Result<Vec<SystemTaskRecord>, ManagementError> {
    sqlx::query(
        "SELECT id, task_id, task_type, status, active_key, payload, state, result,
            error, locked_by, created_at, updated_at
         FROM system_tasks ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(SystemTaskRecord {
            id: i64_col_mysql(&row, "id")?,
            task_id: string_col_mysql(&row, "task_id")?,
            task_type: string_col_mysql(&row, "task_type")?,
            status: SystemTaskStatus::from_str(&string_col_mysql(&row, "status")?)?,
            active_key: opt_string_col_mysql(&row, "active_key")?,
            payload: json_col_mysql(&row, "payload")?,
            state: json_col_mysql(&row, "state")?,
            result: json_col_mysql(&row, "result")?,
            error: string_col_mysql(&row, "error")?,
            locked_by: string_col_mysql(&row, "locked_by")?,
            created_at: i64_col_mysql(&row, "created_at")?,
            updated_at: i64_col_mysql(&row, "updated_at")?,
        })
    })
    .collect()
}

async fn migrate_system_tasks_pg(pool: &PgPool) -> Result<(), ManagementError> {
    for stmt in [
        "CREATE TABLE IF NOT EXISTS system_tasks (
            id BIGINT PRIMARY KEY,
            task_id TEXT NOT NULL UNIQUE,
            task_type TEXT NOT NULL,
            status TEXT NOT NULL,
            active_key TEXT UNIQUE,
            payload TEXT NOT NULL DEFAULT '',
            state TEXT NOT NULL DEFAULT '',
            result TEXT NOT NULL DEFAULT '',
            error TEXT NOT NULL DEFAULT '',
            locked_by TEXT NOT NULL DEFAULT '',
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL
        )",
        "CREATE INDEX IF NOT EXISTS idx_system_tasks_type_status ON system_tasks(task_type, status)",
        "CREATE INDEX IF NOT EXISTS idx_system_tasks_updated_at ON system_tasks(updated_at)",
    ] {
        sqlx::query(stmt).execute(pool).await.map_err(storage_err)?;
    }
    Ok(())
}

async fn load_system_tasks_pg(pool: &PgPool) -> Result<Vec<SystemTaskRecord>, ManagementError> {
    sqlx::query(
        "SELECT id, task_id, task_type, status, active_key, payload, state, result,
            error, locked_by, created_at, updated_at
         FROM system_tasks ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(SystemTaskRecord {
            id: i64_col_pg(&row, "id")?,
            task_id: string_col_pg(&row, "task_id")?,
            task_type: string_col_pg(&row, "task_type")?,
            status: SystemTaskStatus::from_str(&string_col_pg(&row, "status")?)?,
            active_key: opt_string_col_pg(&row, "active_key")?,
            payload: json_col_pg(&row, "payload")?,
            state: json_col_pg(&row, "state")?,
            result: json_col_pg(&row, "result")?,
            error: string_col_pg(&row, "error")?,
            locked_by: string_col_pg(&row, "locked_by")?,
            created_at: i64_col_pg(&row, "created_at")?,
            updated_at: i64_col_pg(&row, "updated_at")?,
        })
    })
    .collect()
}

fn json_text(value: &Option<JsonValue>) -> Result<String, ManagementError> {
    value
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map(|value| value.unwrap_or_default())
        .map_err(storage_err)
}

fn json_col_sqlite(
    row: &sqlx::sqlite::SqliteRow,
    name: &str,
) -> Result<Option<JsonValue>, ManagementError> {
    let raw = string_col_sqlite(row, name)?;
    if raw.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|err| ManagementError::Storage(format!("invalid system task JSON {name}: {err}")))
}

fn string_col_sqlite(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<String, ManagementError> {
    row.try_get::<String, _>(name).map_err(storage_err)
}

fn opt_string_col_sqlite(
    row: &sqlx::sqlite::SqliteRow,
    name: &str,
) -> Result<Option<String>, ManagementError> {
    row.try_get::<Option<String>, _>(name).map_err(storage_err)
}

fn i64_col_sqlite(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<i64, ManagementError> {
    row.try_get::<i64, _>(name).map_err(storage_err)
}

fn json_col_mysql(
    row: &sqlx::mysql::MySqlRow,
    name: &str,
) -> Result<Option<JsonValue>, ManagementError> {
    let raw = string_col_mysql(row, name)?;
    if raw.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|err| ManagementError::Storage(format!("invalid system task JSON {name}: {err}")))
}

fn string_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<String, ManagementError> {
    row.try_get::<String, _>(name).map_err(storage_err)
}

fn opt_string_col_mysql(
    row: &sqlx::mysql::MySqlRow,
    name: &str,
) -> Result<Option<String>, ManagementError> {
    row.try_get::<Option<String>, _>(name).map_err(storage_err)
}

fn i64_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<i64, ManagementError> {
    row.try_get::<i64, _>(name).map_err(storage_err)
}

fn json_col_pg(
    row: &sqlx::postgres::PgRow,
    name: &str,
) -> Result<Option<JsonValue>, ManagementError> {
    let raw = string_col_pg(row, name)?;
    if raw.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|err| ManagementError::Storage(format!("invalid system task JSON {name}: {err}")))
}

fn string_col_pg(row: &sqlx::postgres::PgRow, name: &str) -> Result<String, ManagementError> {
    row.try_get::<String, _>(name).map_err(storage_err)
}

fn opt_string_col_pg(
    row: &sqlx::postgres::PgRow,
    name: &str,
) -> Result<Option<String>, ManagementError> {
    row.try_get::<Option<String>, _>(name).map_err(storage_err)
}

fn i64_col_pg(row: &sqlx::postgres::PgRow, name: &str) -> Result<i64, ManagementError> {
    row.try_get::<i64, _>(name).map_err(storage_err)
}

fn storage_err(err: impl std::fmt::Display) -> ManagementError {
    ManagementError::Storage(err.to_string())
}

fn usage_to_management(err: halolake_control_plane::UsageError) -> ManagementError {
    ManagementError::Storage(format!("usage store: {err}"))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use halolake_control_plane::UsageEventBatch;
    use halolake_domain::{UsageEvent, UsageStatus};

    #[tokio::test]
    async fn memory_start_deduplicates_active_log_cleanup_task() {
        let tasks = SystemTaskStore::memory();
        let first = tasks
            .call(StartLogCleanupTaskRequest {
                target_timestamp: 1000,
            })
            .await
            .expect("first task");
        let second = tasks
            .call(StartLogCleanupTaskRequest {
                target_timestamp: 2000,
            })
            .await
            .expect("active task");
        let listed = tasks
            .call(ListSystemTasksRequest { limit: 0 })
            .await
            .expect("list tasks");

        assert_eq!(first.task_id, second.task_id);
        assert_eq!(first.status, SystemTaskStatus::Pending);
        assert_eq!(listed.len(), 1);
    }

    #[tokio::test]
    async fn log_cleanup_runner_deletes_old_usage_events_and_finishes_task() {
        let tasks = SystemTaskStore::memory();
        let usage_events = UsageStore::memory();
        usage_events
            .call(UsageEventBatch::new(vec![
                usage_event("old-1", 999_000),
                usage_event("old-2", 999_500),
                usage_event("new-1", 1_001_000),
            ]))
            .await
            .expect("record usage");

        let task = tasks
            .call(StartLogCleanupTaskRequest {
                target_timestamp: 1000,
            })
            .await
            .expect("task");
        let runner = LogCleanupTaskRunner::new(tasks.clone(), usage_events.clone());
        runner
            .call(RunLogCleanupTaskRequest {
                task_id: task.task_id.clone(),
            })
            .await
            .expect("run cleanup");

        let finished = tasks
            .call(GetSystemTaskRequest {
                task_id: task.task_id,
            })
            .await
            .expect("get task")
            .expect("task exists");
        assert_eq!(finished.status, SystemTaskStatus::Succeeded);
        assert_eq!(finished.active_key, None);
        assert_eq!(
            finished.result,
            Some(json!(LogCleanupResult { deleted_count: 2 }))
        );
        assert_eq!(usage_events.events().expect("events").len(), 1);
    }

    fn usage_event(request_id: &str, created_at_unix_ms: i64) -> UsageEvent {
        UsageEvent {
            request_id: request_id.to_string(),
            user_id: "1".to_string(),
            token_id: "1".to_string(),
            channel_id: "1".to_string(),
            group: String::new(),
            model: "gpt-test".to_string(),
            upstream_model: "gpt-test".to_string(),
            prompt_tokens: Some(1),
            completion_tokens: Some(1),
            total_tokens: Some(2),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            image_tokens: None,
            audio_tokens: None,
            quota: Some(2),
            status: UsageStatus::Success,
            latency_ms: 10,
            is_stream: false,
            ip: String::new(),
            upstream_request_id: String::new(),
            created_at_unix_ms,
        }
    }
}
