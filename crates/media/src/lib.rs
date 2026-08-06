//! Persistent media control-plane state for Tokiame/ComfyUI jobs.
//!
//! Mutations use a candidate-state transaction: validate and update an owned
//! snapshot, durably persist it, then publish it to readers. This mirrors the
//! control-plane store's state-preserving service boundary and prevents a
//! failed database write from leaking partially applied capacity or quota
//! reservations into memory.

use halolake_tokiame_protocol::{JobEvent, JobEventKind, WorkerCapabilities};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use service_async::Service;
use sqlx::{
    MySqlPool, PgPool, Row, SqlitePool,
    mysql::MySqlPoolOptions,
    postgres::PgPoolOptions,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{cmp::Reverse, collections::BTreeMap, str::FromStr, sync::Arc};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

const STATE_ROW_ID: i32 = 1;
const MAX_JOB_EVENTS: usize = 200;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("media record not found")]
    NotFound,
    #[error("media record already exists")]
    Duplicate,
    #[error("media authentication failed")]
    Unauthorized,
    #[error("media state conflict: {0}")]
    Conflict(String),
    #[error("invalid media request: {0}")]
    InvalidRequest(String),
    #[error("media storage error: {0}")]
    Storage(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Offline,
    Online,
    Draining,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub id:                     String,
    pub namespace:              String,
    pub node_name:              String,
    pub token_hash:             String,
    pub group:                  String,
    pub channel_id:             u64,
    pub enabled:                bool,
    pub status:                 WorkerStatus,
    pub session_id:             Option<String>,
    pub protocol_version:       u16,
    pub agent_version:          String,
    pub capabilities:           WorkerCapabilities,
    pub active_jobs:            u16,
    pub queued_jobs:            u16,
    pub heartbeat_sequence:     u64,
    pub connected_at_unix_ms:   Option<i64>,
    pub last_heartbeat_unix_ms: Option<i64>,
    pub created_at_unix_ms:     i64,
    pub updated_at_unix_ms:     i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRole {
    Input,
    Output,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactState {
    PendingUpload,
    Ready,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub id:                 String,
    pub job_id:             Option<String>,
    pub user_id:            u64,
    pub role:               ArtifactRole,
    pub state:              ArtifactState,
    pub object_key:         String,
    pub name:               String,
    pub content_type:       String,
    pub size_bytes:         u64,
    pub sha256:             String,
    pub expires_at_unix_ms: Option<i64>,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Dispatched,
    Running,
    Uploading,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
    Orphaned,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobEventRecord {
    pub sequence: u64,
    pub kind:     JobEventKind,
    pub progress: Option<f32>,
    pub message:  String,
    pub unix_ms:  i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaJobRecord {
    pub id:                  String,
    pub user_id:             u64,
    pub token_id:            u64,
    pub group:               String,
    pub model:               String,
    pub workflow_id:         String,
    pub workflow_version:    String,
    pub params:              JsonValue,
    pub input_artifact_ids:  Vec<String>,
    pub output_artifact_ids: Vec<String>,
    pub worker_id:           String,
    pub channel_id:          u64,
    pub session_id:          Option<String>,
    pub idempotency_key:     Option<String>,
    pub state:               JobState,
    pub progress:            Option<f32>,
    pub prompt_id:           Option<String>,
    pub attempt:             u32,
    pub last_event_sequence: u64,
    pub reserved_quota:      i64,
    pub charged_quota:       i64,
    pub quota_settled:       bool,
    pub error:               Option<String>,
    pub events:              Vec<JobEventRecord>,
    pub created_at_unix_ms:  i64,
    pub updated_at_unix_ms:  i64,
    pub finished_at_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaData {
    #[serde(default)]
    pub workers:   BTreeMap<String, WorkerRecord>,
    #[serde(default)]
    pub artifacts: BTreeMap<String, ArtifactRecord>,
    #[serde(default)]
    pub jobs:      BTreeMap<String, MediaJobRecord>,
}

#[derive(Debug, Clone)]
enum Backend {
    Memory,
    Sqlite(SqlitePool),
    MySql(MySqlPool),
    Postgres(PgPool),
}

#[derive(Debug, Clone)]
pub struct MediaStore {
    backend:    Backend,
    data:       Arc<RwLock<MediaData>>,
    write_lock: Arc<Mutex<()>>,
}

impl MediaStore {
    pub fn memory() -> Self {
        Self {
            backend:    Backend::Memory,
            data:       Arc::new(RwLock::new(MediaData::default())),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub async fn sqlite(url: &str) -> Result<Self, MediaError> {
        let options = SqliteConnectOptions::from_str(url)
            .map_err(storage_error)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_error)?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS tokiame_media_state (id INTEGER PRIMARY KEY, payload TEXT \
             NOT NULL, updated_at BIGINT NOT NULL)",
        )
        .execute(&pool)
        .await
        .map_err(storage_error)?;
        Self::from_backend(Backend::Sqlite(pool)).await
    }

    pub async fn mysql(url: &str) -> Result<Self, MediaError> {
        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await
            .map_err(storage_error)?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS tokiame_media_state (id INT PRIMARY KEY, payload LONGTEXT \
             NOT NULL, updated_at BIGINT NOT NULL)",
        )
        .execute(&pool)
        .await
        .map_err(storage_error)?;
        Self::from_backend(Backend::MySql(pool)).await
    }

    pub async fn postgres(url: &str) -> Result<Self, MediaError> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await
            .map_err(storage_error)?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS tokiame_media_state (id INTEGER PRIMARY KEY, payload TEXT \
             NOT NULL, updated_at BIGINT NOT NULL)",
        )
        .execute(&pool)
        .await
        .map_err(storage_error)?;
        Self::from_backend(Backend::Postgres(pool)).await
    }

    async fn from_backend(backend: Backend) -> Result<Self, MediaError> {
        let data = load_data(&backend).await?;
        Ok(Self {
            backend,
            data: Arc::new(RwLock::new(data)),
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    pub async fn current_data(&self) -> MediaData {
        self.data.read().await.clone()
    }

    async fn mutate<T>(
        &self,
        f: impl FnOnce(&mut MediaData) -> Result<T, MediaError>,
    ) -> Result<T, MediaError> {
        let _guard = self.write_lock.lock().await;
        let mut candidate = self.data.read().await.clone();
        let value = f(&mut candidate)?;
        save_data(&self.backend, &candidate).await?;
        *self.data.write().await = candidate;
        Ok(value)
    }
}

#[derive(Debug, Clone)]
pub struct CreateWorkerRequest {
    pub namespace:  String,
    pub node_name:  String,
    pub token_hash: String,
    pub group:      String,
    pub channel_id: u64,
}

#[derive(Debug, Clone)]
pub struct RotateWorkerTokenRequest {
    pub worker_id:  String,
    pub token_hash: String,
}

#[derive(Debug, Clone)]
pub struct AuthenticateWorkerRequest {
    pub token_hash: String,
}

#[derive(Debug, Clone)]
pub struct RegisterWorkerRequest {
    pub worker_id:        String,
    pub namespace:        String,
    pub node_name:        String,
    pub session_id:       String,
    pub protocol_version: u16,
    pub agent_version:    String,
    pub capabilities:     WorkerCapabilities,
}

#[derive(Debug, Clone)]
pub struct WorkerHeartbeatRequest {
    pub worker_id:   String,
    pub session_id:  String,
    pub sequence:    u64,
    pub active_jobs: u16,
    pub queued_jobs: u16,
    pub unix_ms:     i64,
}

#[derive(Debug, Clone)]
pub struct DisconnectWorkerRequest {
    pub worker_id:  String,
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub struct GetWorkerRequest {
    pub worker_id: String,
}

#[derive(Debug, Clone)]
pub struct SetWorkerEnabledRequest {
    pub worker_id: String,
    pub enabled:   bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ListWorkersRequest;

#[derive(Debug, Clone)]
pub struct CreateArtifactRequest {
    pub job_id:             Option<String>,
    pub user_id:            u64,
    pub role:               ArtifactRole,
    pub object_key:         String,
    pub name:               String,
    pub content_type:       String,
    pub size_bytes:         u64,
    pub sha256:             String,
    pub expires_at_unix_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CompleteArtifactRequest {
    pub artifact_id: String,
    pub size_bytes:  u64,
    pub sha256:      String,
}

#[derive(Debug, Clone)]
pub struct GetArtifactRequest {
    pub artifact_id: String,
}

#[derive(Debug, Clone)]
pub struct CreateJobRequest {
    pub user_id:               u64,
    pub token_id:              u64,
    pub group:                 String,
    pub model:                 String,
    pub workflow_id:           String,
    pub workflow_version:      String,
    pub params:                JsonValue,
    pub input_artifact_ids:    Vec<String>,
    pub idempotency_key:       Option<String>,
    pub reserved_quota:        i64,
    pub token_remaining_quota: i64,
    pub token_unlimited_quota: bool,
}

#[derive(Debug, Clone)]
pub struct GetJobRequest {
    pub job_id: String,
}

#[derive(Debug, Clone)]
pub struct ListUserJobsRequest {
    pub user_id: u64,
    pub limit:   usize,
}

#[derive(Debug, Clone)]
pub struct BindJobSessionRequest {
    pub job_id:     String,
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub struct MarkDispatchFailedRequest {
    pub job_id:  String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ApplyJobEventRequest {
    pub worker_id:  String,
    pub session_id: String,
    pub event:      JobEvent,
}

#[derive(Debug, Clone)]
pub struct ApplyJobEventResponse {
    pub job:                    MediaJobRecord,
    pub changed:                bool,
    pub needs_quota_settlement: bool,
}

#[derive(Debug, Clone)]
pub struct MarkJobQuotaSettledRequest {
    pub job_id:        String,
    pub charged_quota: i64,
}

#[derive(Debug, Clone)]
pub struct RequestJobCancellation {
    pub job_id:  String,
    pub user_id: u64,
}

impl Service<CreateWorkerRequest> for MediaStore {
    type Response = WorkerRecord;
    type Error = MediaError;

    async fn call(&self, req: CreateWorkerRequest) -> Result<Self::Response, Self::Error> {
        validate_name("namespace", &req.namespace)?;
        validate_name("node_name", &req.node_name)?;
        if req.token_hash.trim().is_empty() {
            return Err(MediaError::InvalidRequest("token hash is required".into()));
        }
        self.mutate(|data| {
            if data.workers.values().any(|worker| {
                worker.namespace == req.namespace || worker.token_hash == req.token_hash
            }) {
                return Err(MediaError::Duplicate);
            }
            let now = now_unix_ms();
            let worker = WorkerRecord {
                id:                     Uuid::new_v4().to_string(),
                namespace:              req.namespace,
                node_name:              req.node_name,
                token_hash:             req.token_hash,
                group:                  normalized_group(&req.group),
                channel_id:             req.channel_id,
                enabled:                true,
                status:                 WorkerStatus::Offline,
                session_id:             None,
                protocol_version:       0,
                agent_version:          String::new(),
                capabilities:           WorkerCapabilities::default(),
                active_jobs:            0,
                queued_jobs:            0,
                heartbeat_sequence:     0,
                connected_at_unix_ms:   None,
                last_heartbeat_unix_ms: None,
                created_at_unix_ms:     now,
                updated_at_unix_ms:     now,
            };
            data.workers.insert(worker.id.clone(), worker.clone());
            Ok(worker)
        })
        .await
    }
}

impl Service<RotateWorkerTokenRequest> for MediaStore {
    type Response = WorkerRecord;
    type Error = MediaError;

    async fn call(&self, req: RotateWorkerTokenRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            if data
                .workers
                .values()
                .any(|worker| worker.id != req.worker_id && worker.token_hash == req.token_hash)
            {
                return Err(MediaError::Duplicate);
            }
            let worker = data
                .workers
                .get_mut(&req.worker_id)
                .ok_or(MediaError::NotFound)?;
            worker.token_hash = req.token_hash;
            worker.status = WorkerStatus::Offline;
            worker.session_id = None;
            worker.updated_at_unix_ms = now_unix_ms();
            Ok(worker.clone())
        })
        .await
    }
}

impl Service<AuthenticateWorkerRequest> for MediaStore {
    type Response = WorkerRecord;
    type Error = MediaError;

    async fn call(&self, req: AuthenticateWorkerRequest) -> Result<Self::Response, Self::Error> {
        self.data
            .read()
            .await
            .workers
            .values()
            .find(|worker| worker.enabled && worker.token_hash == req.token_hash)
            .cloned()
            .ok_or(MediaError::Unauthorized)
    }
}

impl Service<RegisterWorkerRequest> for MediaStore {
    type Response = WorkerRecord;
    type Error = MediaError;

    async fn call(&self, req: RegisterWorkerRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let now = now_unix_ms();
            let session_id = req.session_id;
            let worker = {
                let worker = data
                    .workers
                    .get_mut(&req.worker_id)
                    .ok_or(MediaError::NotFound)?;
                if !worker.enabled || worker.namespace != req.namespace {
                    return Err(MediaError::Unauthorized);
                }
                worker.node_name = req.node_name;
                worker.session_id = Some(session_id.clone());
                worker.protocol_version = req.protocol_version;
                worker.agent_version = req.agent_version;
                worker.capabilities = req.capabilities;
                worker.status = WorkerStatus::Online;
                worker.active_jobs = 0;
                worker.queued_jobs = 0;
                worker.heartbeat_sequence = 0;
                worker.connected_at_unix_ms = Some(now);
                worker.last_heartbeat_unix_ms = Some(now);
                worker.updated_at_unix_ms = now;
                worker.clone()
            };
            // A reconnect supersedes the old socket. Incomplete journaled jobs
            // resume through the new session and must not be rejected as stale.
            for job in data
                .jobs
                .values_mut()
                .filter(|job| job.worker_id == req.worker_id && !job.state.is_terminal())
            {
                job.session_id = Some(session_id.clone());
                job.updated_at_unix_ms = now;
            }
            Ok(worker)
        })
        .await
    }
}

impl Service<WorkerHeartbeatRequest> for MediaStore {
    type Response = WorkerRecord;
    type Error = MediaError;

    async fn call(&self, req: WorkerHeartbeatRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let worker = data
                .workers
                .get_mut(&req.worker_id)
                .ok_or(MediaError::NotFound)?;
            ensure_session(worker, &req.session_id)?;
            if req.sequence <= worker.heartbeat_sequence {
                return Ok(worker.clone());
            }
            worker.heartbeat_sequence = req.sequence;
            worker.active_jobs = req.active_jobs;
            worker.queued_jobs = req.queued_jobs;
            worker.last_heartbeat_unix_ms = Some(now_unix_ms());
            worker.updated_at_unix_ms = now_unix_ms();
            Ok(worker.clone())
        })
        .await
    }
}

impl Service<DisconnectWorkerRequest> for MediaStore {
    type Response = WorkerRecord;
    type Error = MediaError;

    async fn call(&self, req: DisconnectWorkerRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let worker = data
                .workers
                .get_mut(&req.worker_id)
                .ok_or(MediaError::NotFound)?;
            if worker.session_id.as_deref() != Some(req.session_id.as_str()) {
                return Ok(worker.clone());
            }
            worker.status = WorkerStatus::Offline;
            worker.session_id = None;
            worker.active_jobs = 0;
            worker.queued_jobs = 0;
            worker.updated_at_unix_ms = now_unix_ms();
            Ok(worker.clone())
        })
        .await
    }
}

impl Service<GetWorkerRequest> for MediaStore {
    type Response = WorkerRecord;
    type Error = MediaError;

    async fn call(&self, req: GetWorkerRequest) -> Result<Self::Response, Self::Error> {
        self.data
            .read()
            .await
            .workers
            .get(&req.worker_id)
            .cloned()
            .ok_or(MediaError::NotFound)
    }
}

impl Service<SetWorkerEnabledRequest> for MediaStore {
    type Response = WorkerRecord;
    type Error = MediaError;

    async fn call(&self, req: SetWorkerEnabledRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let worker = data
                .workers
                .get_mut(&req.worker_id)
                .ok_or(MediaError::NotFound)?;
            worker.enabled = req.enabled;
            if !req.enabled {
                worker.status = WorkerStatus::Offline;
                worker.session_id = None;
                worker.active_jobs = 0;
                worker.queued_jobs = 0;
            }
            worker.updated_at_unix_ms = now_unix_ms();
            Ok(worker.clone())
        })
        .await
    }
}

impl Service<ListWorkersRequest> for MediaStore {
    type Response = Vec<WorkerRecord>;
    type Error = MediaError;

    async fn call(&self, _: ListWorkersRequest) -> Result<Self::Response, Self::Error> {
        let mut workers = self
            .data
            .read()
            .await
            .workers
            .values()
            .cloned()
            .collect::<Vec<_>>();
        workers.sort_by_key(|worker| Reverse(worker.created_at_unix_ms));
        Ok(workers)
    }
}

impl Service<CreateArtifactRequest> for MediaStore {
    type Response = ArtifactRecord;
    type Error = MediaError;

    async fn call(&self, req: CreateArtifactRequest) -> Result<Self::Response, Self::Error> {
        if req.object_key.trim().is_empty() || req.name.trim().is_empty() {
            return Err(MediaError::InvalidRequest(
                "object key and artifact name are required".into(),
            ));
        }
        self.mutate(|data| {
            if let Some(job_id) = req.job_id.as_deref() {
                let job = data.jobs.get(job_id).ok_or(MediaError::NotFound)?;
                if job.user_id != req.user_id || req.role != ArtifactRole::Output {
                    return Err(MediaError::Conflict(
                        "artifact job ownership mismatch".into(),
                    ));
                }
                if let Some(existing) = data.artifacts.values().find(|artifact| {
                    artifact.job_id.as_deref() == Some(job_id)
                        && artifact.role == ArtifactRole::Output
                        && artifact.name == sanitize_filename(&req.name)
                        && artifact.size_bytes == req.size_bytes
                        && artifact.sha256.eq_ignore_ascii_case(&req.sha256)
                        && artifact.state != ArtifactState::Deleted
                }) {
                    return Ok(existing.clone());
                }
            }
            let now = now_unix_ms();
            let artifact = ArtifactRecord {
                id:                 Uuid::new_v4().to_string(),
                job_id:             req.job_id,
                user_id:            req.user_id,
                role:               req.role,
                state:              ArtifactState::PendingUpload,
                object_key:         req.object_key,
                name:               sanitize_filename(&req.name),
                content_type:       req.content_type,
                size_bytes:         req.size_bytes,
                sha256:             req.sha256.to_ascii_lowercase(),
                expires_at_unix_ms: req.expires_at_unix_ms,
                created_at_unix_ms: now,
                updated_at_unix_ms: now,
            };
            data.artifacts.insert(artifact.id.clone(), artifact.clone());
            Ok(artifact)
        })
        .await
    }
}

impl Service<CompleteArtifactRequest> for MediaStore {
    type Response = ArtifactRecord;
    type Error = MediaError;

    async fn call(&self, req: CompleteArtifactRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let artifact = data
                .artifacts
                .get_mut(&req.artifact_id)
                .ok_or(MediaError::NotFound)?;
            if artifact.state == ArtifactState::Ready {
                if artifact.size_bytes == req.size_bytes
                    && artifact.sha256.eq_ignore_ascii_case(&req.sha256)
                {
                    return Ok(artifact.clone());
                }
                return Err(MediaError::Conflict(
                    "completed artifact metadata changed".into(),
                ));
            }
            if artifact.state != ArtifactState::PendingUpload {
                return Err(MediaError::Conflict("artifact is not uploadable".into()));
            }
            if artifact.size_bytes != req.size_bytes
                || !artifact.sha256.eq_ignore_ascii_case(&req.sha256)
            {
                return Err(MediaError::Conflict(
                    "uploaded object does not match declared metadata".into(),
                ));
            }
            artifact.state = ArtifactState::Ready;
            artifact.updated_at_unix_ms = now_unix_ms();
            if artifact.role == ArtifactRole::Output
                && let Some(job) = artifact
                    .job_id
                    .as_ref()
                    .and_then(|job_id| data.jobs.get_mut(job_id))
                && !job.output_artifact_ids.contains(&artifact.id)
            {
                job.output_artifact_ids.push(artifact.id.clone());
                job.updated_at_unix_ms = now_unix_ms();
            }
            Ok(artifact.clone())
        })
        .await
    }
}

impl Service<GetArtifactRequest> for MediaStore {
    type Response = ArtifactRecord;
    type Error = MediaError;

    async fn call(&self, req: GetArtifactRequest) -> Result<Self::Response, Self::Error> {
        self.data
            .read()
            .await
            .artifacts
            .get(&req.artifact_id)
            .cloned()
            .ok_or(MediaError::NotFound)
    }
}

impl Service<CreateJobRequest> for MediaStore {
    type Response = MediaJobRecord;
    type Error = MediaError;

    async fn call(&self, req: CreateJobRequest) -> Result<Self::Response, Self::Error> {
        if req.reserved_quota < 0 {
            return Err(MediaError::InvalidRequest(
                "reserved quota cannot be negative".into(),
            ));
        }
        if !req.params.is_object() {
            return Err(MediaError::InvalidRequest(
                "job parameters must be a JSON object".into(),
            ));
        }
        if req.input_artifact_ids.len() > 32 {
            return Err(MediaError::InvalidRequest(
                "a job may reference at most 32 input artifacts".into(),
            ));
        }
        if req
            .idempotency_key
            .as_deref()
            .is_some_and(|key| key.len() > 200)
        {
            return Err(MediaError::InvalidRequest(
                "idempotency key may contain at most 200 bytes".into(),
            ));
        }
        self.mutate(|data| {
            if let Some(key) = req.idempotency_key.as_deref()
                && let Some(existing_id) = data
                    .jobs
                    .values()
                    .find(|job| {
                        job.token_id == req.token_id && job.idempotency_key.as_deref() == Some(key)
                    })
                    .map(|job| job.id.clone())
            {
                let existing = data.jobs.get(&existing_id).expect("id came from map");
                if existing.model != req.model
                    || existing.workflow_id != req.workflow_id
                    || existing.workflow_version != req.workflow_version
                    || existing.params != req.params
                    || existing.input_artifact_ids != req.input_artifact_ids
                {
                    return Err(MediaError::Conflict(
                        "idempotency key was reused with a different job payload".into(),
                    ));
                }
                let (session_id, online) = data
                    .jobs
                    .get(&existing_id)
                    .and_then(|job| data.workers.get(&job.worker_id))
                    .map(|worker| {
                        (
                            worker.session_id.clone(),
                            worker.status == WorkerStatus::Online,
                        )
                    })
                    .unwrap_or((None, false));
                let existing = data.jobs.get_mut(&existing_id).expect("id came from map");
                if !existing.state.is_terminal() && online {
                    existing.session_id = session_id;
                    if existing.state == JobState::Orphaned {
                        existing.state = JobState::Queued;
                        existing.error = None;
                    }
                    existing.updated_at_unix_ms = now_unix_ms();
                }
                return Ok(existing.clone());
            }
            for artifact_id in &req.input_artifact_ids {
                let artifact = data
                    .artifacts
                    .get(artifact_id)
                    .ok_or(MediaError::NotFound)?;
                if artifact.user_id != req.user_id
                    || artifact.role != ArtifactRole::Input
                    || artifact.state != ArtifactState::Ready
                {
                    return Err(MediaError::Conflict(format!(
                        "input artifact {artifact_id} is not ready for this user"
                    )));
                }
            }
            let already_reserved = data
                .jobs
                .values()
                .filter(|job| job.token_id == req.token_id && !job.state.is_terminal())
                .map(|job| job.reserved_quota)
                .sum::<i64>();
            if !req.token_unlimited_quota
                && req.token_remaining_quota.saturating_sub(already_reserved) < req.reserved_quota
            {
                return Err(MediaError::Conflict(
                    "token quota is already reserved by active media jobs".into(),
                ));
            }

            let mut loads = BTreeMap::<&str, usize>::new();
            for job in data.jobs.values().filter(|job| !job.state.is_terminal()) {
                *loads.entry(job.worker_id.as_str()).or_default() += 1;
            }
            let worker = data
                .workers
                .values()
                .filter(|worker| worker.enabled && worker.status == WorkerStatus::Online)
                .filter(|worker| {
                    worker.group == normalized_group(&req.group) || worker.group == "*"
                })
                .filter(|worker| {
                    worker.capabilities.workflows.iter().any(|workflow| {
                        workflow.id == req.workflow_id
                            && workflow.version == req.workflow_version
                            && workflow.model == req.model
                    })
                })
                .filter(|worker| {
                    loads.get(worker.id.as_str()).copied().unwrap_or_default()
                        < usize::from(worker.capabilities.concurrency.max(1))
                })
                .min_by_key(|worker| {
                    (
                        loads.get(worker.id.as_str()).copied().unwrap_or_default(),
                        worker.id.as_str(),
                    )
                })
                .cloned()
                .ok_or_else(|| {
                    MediaError::Conflict("no capable Tokiame worker is online".into())
                })?;

            let now = now_unix_ms();
            let job = MediaJobRecord {
                id:                  Uuid::new_v4().to_string(),
                user_id:             req.user_id,
                token_id:            req.token_id,
                group:               normalized_group(&req.group),
                model:               req.model,
                workflow_id:         req.workflow_id,
                workflow_version:    req.workflow_version,
                params:              req.params,
                input_artifact_ids:  req.input_artifact_ids,
                output_artifact_ids: Vec::new(),
                worker_id:           worker.id,
                channel_id:          worker.channel_id,
                session_id:          worker.session_id,
                idempotency_key:     req.idempotency_key,
                state:               JobState::Queued,
                progress:            Some(0.0),
                prompt_id:           None,
                attempt:             1,
                last_event_sequence: 0,
                reserved_quota:      req.reserved_quota,
                charged_quota:       0,
                quota_settled:       false,
                error:               None,
                events:              Vec::new(),
                created_at_unix_ms:  now,
                updated_at_unix_ms:  now,
                finished_at_unix_ms: None,
            };
            data.jobs.insert(job.id.clone(), job.clone());
            Ok(job)
        })
        .await
    }
}

impl Service<GetJobRequest> for MediaStore {
    type Response = MediaJobRecord;
    type Error = MediaError;

    async fn call(&self, req: GetJobRequest) -> Result<Self::Response, Self::Error> {
        self.data
            .read()
            .await
            .jobs
            .get(&req.job_id)
            .cloned()
            .ok_or(MediaError::NotFound)
    }
}

impl Service<ListUserJobsRequest> for MediaStore {
    type Response = Vec<MediaJobRecord>;
    type Error = MediaError;

    async fn call(&self, req: ListUserJobsRequest) -> Result<Self::Response, Self::Error> {
        let mut jobs = self
            .data
            .read()
            .await
            .jobs
            .values()
            .filter(|job| job.user_id == req.user_id)
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by_key(|job| Reverse(job.created_at_unix_ms));
        jobs.truncate(req.limit.clamp(1, 200));
        Ok(jobs)
    }
}

impl Service<BindJobSessionRequest> for MediaStore {
    type Response = MediaJobRecord;
    type Error = MediaError;

    async fn call(&self, req: BindJobSessionRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let job = data.jobs.get_mut(&req.job_id).ok_or(MediaError::NotFound)?;
            if job.state.is_terminal() {
                return Ok(job.clone());
            }
            let worker = data
                .workers
                .get(&job.worker_id)
                .ok_or(MediaError::NotFound)?;
            ensure_session(worker, &req.session_id)?;
            job.session_id = Some(req.session_id);
            job.updated_at_unix_ms = now_unix_ms();
            Ok(job.clone())
        })
        .await
    }
}

impl Service<MarkDispatchFailedRequest> for MediaStore {
    type Response = MediaJobRecord;
    type Error = MediaError;

    async fn call(&self, req: MarkDispatchFailedRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let job = data.jobs.get_mut(&req.job_id).ok_or(MediaError::NotFound)?;
            if !job.state.is_terminal() {
                job.state = JobState::Orphaned;
                job.error = Some(req.message);
                job.updated_at_unix_ms = now_unix_ms();
            }
            Ok(job.clone())
        })
        .await
    }
}

impl Service<ApplyJobEventRequest> for MediaStore {
    type Response = ApplyJobEventResponse;
    type Error = MediaError;

    async fn call(&self, req: ApplyJobEventRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let job = data
                .jobs
                .get_mut(&req.event.job_id)
                .ok_or(MediaError::NotFound)?;
            if job.worker_id != req.worker_id
                || job.session_id.as_deref() != Some(req.session_id.as_str())
                || job.attempt != req.event.attempt
            {
                return Err(MediaError::Conflict(
                    "stale job event session or attempt".into(),
                ));
            }
            if req.event.sequence <= job.last_event_sequence {
                return Ok(ApplyJobEventResponse {
                    needs_quota_settlement: job.state == JobState::Completed && !job.quota_settled,
                    job:                    job.clone(),
                    changed:                false,
                });
            }
            if job.state.is_terminal() {
                return Err(MediaError::Conflict("job is already terminal".into()));
            }
            let next = state_for_event(job.state, req.event.kind)?;
            job.state = next;
            job.last_event_sequence = req.event.sequence;
            job.progress = req.event.progress.map(|value| value.clamp(0.0, 1.0));
            if let Some(prompt_id) = req.event.prompt_id {
                job.prompt_id = Some(prompt_id);
            }
            if matches!(next, JobState::Failed | JobState::Cancelled) {
                job.error = (!req.event.message.is_empty()).then(|| req.event.message.clone());
                job.quota_settled = true;
                job.charged_quota = 0;
            }
            if next.is_terminal() {
                job.finished_at_unix_ms = Some(req.event.unix_ms);
            }
            job.updated_at_unix_ms = req.event.unix_ms;
            job.events.push(JobEventRecord {
                sequence: req.event.sequence,
                kind:     req.event.kind,
                progress: req.event.progress,
                message:  req.event.message,
                unix_ms:  req.event.unix_ms,
            });
            if job.events.len() > MAX_JOB_EVENTS {
                job.events.drain(..job.events.len() - MAX_JOB_EVENTS);
            }
            Ok(ApplyJobEventResponse {
                needs_quota_settlement: next == JobState::Completed && !job.quota_settled,
                job:                    job.clone(),
                changed:                true,
            })
        })
        .await
    }
}

impl Service<MarkJobQuotaSettledRequest> for MediaStore {
    type Response = MediaJobRecord;
    type Error = MediaError;

    async fn call(&self, req: MarkJobQuotaSettledRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let job = data.jobs.get_mut(&req.job_id).ok_or(MediaError::NotFound)?;
            if job.state != JobState::Completed {
                return Err(MediaError::Conflict(
                    "only completed jobs can be billed".into(),
                ));
            }
            job.charged_quota = req.charged_quota.max(0);
            job.quota_settled = true;
            job.updated_at_unix_ms = now_unix_ms();
            Ok(job.clone())
        })
        .await
    }
}

impl Service<RequestJobCancellation> for MediaStore {
    type Response = MediaJobRecord;
    type Error = MediaError;

    async fn call(&self, req: RequestJobCancellation) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let job = data.jobs.get_mut(&req.job_id).ok_or(MediaError::NotFound)?;
            if job.user_id != req.user_id {
                return Err(MediaError::NotFound);
            }
            if job.state == JobState::Uploading {
                return Err(MediaError::Conflict(
                    "job cannot be cancelled after output upload begins".into(),
                ));
            }
            if !job.state.is_terminal() {
                let now = now_unix_ms();
                job.state = JobState::Cancelled;
                job.quota_settled = true;
                job.charged_quota = 0;
                job.error = Some("cancelled by user".into());
                job.finished_at_unix_ms = Some(now);
                job.updated_at_unix_ms = now;
            }
            Ok(job.clone())
        })
        .await
    }
}

fn ensure_session(worker: &WorkerRecord, session_id: &str) -> Result<(), MediaError> {
    if worker.status != WorkerStatus::Online || worker.session_id.as_deref() != Some(session_id) {
        return Err(MediaError::Conflict("stale worker session".into()));
    }
    Ok(())
}

fn state_for_event(current: JobState, kind: JobEventKind) -> Result<JobState, MediaError> {
    let next = match kind {
        JobEventKind::Accepted => JobState::Dispatched,
        JobEventKind::Running | JobEventKind::Progress => JobState::Running,
        JobEventKind::Uploading => JobState::Uploading,
        JobEventKind::Completed => JobState::Completed,
        JobEventKind::Failed => JobState::Failed,
        JobEventKind::Cancelled => JobState::Cancelled,
    };
    let allowed = match current {
        JobState::Queued | JobState::Orphaned => matches!(
            next,
            JobState::Dispatched | JobState::Failed | JobState::Cancelled
        ),
        JobState::Dispatched => matches!(
            next,
            JobState::Running | JobState::Failed | JobState::Cancelled
        ),
        JobState::Running => matches!(
            next,
            JobState::Running | JobState::Uploading | JobState::Failed | JobState::Cancelled
        ),
        JobState::Uploading => matches!(
            next,
            JobState::Uploading | JobState::Completed | JobState::Failed
        ),
        JobState::Cancelling => matches!(next, JobState::Cancelled | JobState::Failed),
        JobState::Completed | JobState::Failed | JobState::Cancelled => false,
    };
    allowed.then_some(next).ok_or_else(|| {
        MediaError::Conflict(format!("invalid job transition {current:?} -> {next:?}"))
    })
}

fn validate_name(field: &str, value: &str) -> Result<(), MediaError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(MediaError::InvalidRequest(format!(
            "{field} must contain only letters, numbers, '.', '-' or '_'"
        )));
    }
    Ok(())
}

fn normalized_group(group: &str) -> String {
    let group = group.trim();
    if group.is_empty() {
        "default".into()
    } else {
        group.to_string()
    }
}

fn sanitize_filename(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or("artifact.bin")
        .chars()
        .filter(|character| !character.is_control())
        .take(255)
        .collect()
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

fn storage_error(error: impl std::fmt::Display) -> MediaError {
    MediaError::Storage(error.to_string())
}

async fn load_data(backend: &Backend) -> Result<MediaData, MediaError> {
    let payload = match backend {
        Backend::Memory => return Ok(MediaData::default()),
        Backend::Sqlite(pool) => {
            sqlx::query("SELECT payload FROM tokiame_media_state WHERE id = ?")
                .bind(STATE_ROW_ID)
                .fetch_optional(pool)
                .await
                .map_err(storage_error)?
                .map(|row| row.get::<String, _>("payload"))
        }
        Backend::MySql(pool) => sqlx::query("SELECT payload FROM tokiame_media_state WHERE id = ?")
            .bind(STATE_ROW_ID)
            .fetch_optional(pool)
            .await
            .map_err(storage_error)?
            .map(|row| row.get::<String, _>("payload")),
        Backend::Postgres(pool) => {
            sqlx::query("SELECT payload FROM tokiame_media_state WHERE id = $1")
                .bind(STATE_ROW_ID)
                .fetch_optional(pool)
                .await
                .map_err(storage_error)?
                .map(|row| row.get::<String, _>("payload"))
        }
    };
    payload
        .map(|payload| serde_json::from_str(&payload).map_err(storage_error))
        .transpose()
        .map(Option::unwrap_or_default)
}

async fn save_data(backend: &Backend, data: &MediaData) -> Result<(), MediaError> {
    let payload = serde_json::to_string(data).map_err(storage_error)?;
    let updated_at = now_unix_ms();
    match backend {
        Backend::Memory => Ok(()),
        Backend::Sqlite(pool) => {
            sqlx::query(
                "INSERT INTO tokiame_media_state (id, payload, updated_at) VALUES (?, ?, ?) ON \
                 CONFLICT(id) DO UPDATE SET payload = excluded.payload, updated_at = \
                 excluded.updated_at",
            )
            .bind(STATE_ROW_ID)
            .bind(payload)
            .bind(updated_at)
            .execute(pool)
            .await
            .map_err(storage_error)?;
            Ok(())
        }
        Backend::MySql(pool) => {
            sqlx::query(
                "INSERT INTO tokiame_media_state (id, payload, updated_at) VALUES (?, ?, ?) ON \
                 DUPLICATE KEY UPDATE payload = VALUES(payload), updated_at = VALUES(updated_at)",
            )
            .bind(STATE_ROW_ID)
            .bind(payload)
            .bind(updated_at)
            .execute(pool)
            .await
            .map_err(storage_error)?;
            Ok(())
        }
        Backend::Postgres(pool) => {
            sqlx::query(
                "INSERT INTO tokiame_media_state (id, payload, updated_at) VALUES ($1, $2, $3) ON \
                 CONFLICT(id) DO UPDATE SET payload = excluded.payload, updated_at = \
                 excluded.updated_at",
            )
            .bind(STATE_ROW_ID)
            .bind(payload)
            .bind(updated_at)
            .execute(pool)
            .await
            .map_err(storage_error)?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halolake_tokiame_protocol::WorkflowCapability;

    fn capability() -> WorkerCapabilities {
        WorkerCapabilities {
            workflows:   vec![WorkflowCapability {
                id:           "portrait".into(),
                version:      "v1".into(),
                model:        "comfy-portrait".into(),
                output_types: vec!["image/png".into()],
            }],
            concurrency: 1,
            labels:      BTreeMap::new(),
        }
    }

    async fn online_worker(store: &MediaStore) -> WorkerRecord {
        let worker = store
            .call(CreateWorkerRequest {
                namespace:  "gpu-home".into(),
                node_name:  "node-1".into(),
                token_hash: "hash".into(),
                group:      "default".into(),
                channel_id: 7,
            })
            .await
            .unwrap();
        store
            .call(RegisterWorkerRequest {
                worker_id:        worker.id,
                namespace:        "gpu-home".into(),
                node_name:        "node-1".into(),
                session_id:       "session-1".into(),
                protocol_version: 2,
                agent_version:    "test".into(),
                capabilities:     capability(),
            })
            .await
            .unwrap()
    }

    fn create_job(idempotency_key: &str) -> CreateJobRequest {
        CreateJobRequest {
            user_id:               1,
            token_id:              2,
            group:                 "default".into(),
            model:                 "comfy-portrait".into(),
            workflow_id:           "portrait".into(),
            workflow_version:      "v1".into(),
            params:                serde_json::json!({"prompt":"hello"}),
            input_artifact_ids:    Vec::new(),
            idempotency_key:       Some(idempotency_key.into()),
            reserved_quota:        10,
            token_remaining_quota: 100,
            token_unlimited_quota: false,
        }
    }

    #[tokio::test]
    async fn idempotency_and_worker_capacity_are_atomic() {
        let store = MediaStore::memory();
        online_worker(&store).await;
        let first = store.call(create_job("same")).await.unwrap();
        let replay = store.call(create_job("same")).await.unwrap();
        assert_eq!(first.id, replay.id);
        let mut conflicting = create_job("same");
        conflicting.params = serde_json::json!({"prompt":"changed"});
        assert!(matches!(
            store.call(conflicting).await.unwrap_err(),
            MediaError::Conflict(_)
        ));
        let error = store.call(create_job("other")).await.unwrap_err();
        assert!(matches!(error, MediaError::Conflict(_)));
    }

    #[tokio::test]
    async fn stale_session_cannot_complete_job() {
        let store = MediaStore::memory();
        let worker = online_worker(&store).await;
        let job = store.call(create_job("job")).await.unwrap();
        let error = store
            .call(ApplyJobEventRequest {
                worker_id:  worker.id,
                session_id: "old-session".into(),
                event:      JobEvent {
                    job_id:    job.id,
                    attempt:   1,
                    sequence:  1,
                    kind:      JobEventKind::Completed,
                    progress:  Some(1.0),
                    prompt_id: None,
                    message:   String::new(),
                    unix_ms:   now_unix_ms(),
                },
            })
            .await
            .unwrap_err();
        assert!(matches!(error, MediaError::Conflict(_)));
    }

    #[tokio::test]
    async fn reconnect_rebinds_incomplete_job_and_terminal_event_is_idempotent() {
        let store = MediaStore::memory();
        let worker = online_worker(&store).await;
        let job = store.call(create_job("recoverable")).await.unwrap();
        let reconnected = store
            .call(RegisterWorkerRequest {
                worker_id:        worker.id.clone(),
                namespace:        worker.namespace,
                node_name:        "node-1".into(),
                session_id:       "session-2".into(),
                protocol_version: 2,
                agent_version:    "test-2".into(),
                capabilities:     capability(),
            })
            .await
            .unwrap();
        assert_eq!(reconnected.session_id.as_deref(), Some("session-2"));
        let rebound = store
            .call(GetJobRequest {
                job_id: job.id.clone(),
            })
            .await
            .unwrap();
        assert_eq!(rebound.session_id.as_deref(), Some("session-2"));

        for (sequence, kind) in [
            (1, JobEventKind::Accepted),
            (2, JobEventKind::Running),
            (3, JobEventKind::Uploading),
            (4, JobEventKind::Completed),
        ] {
            let result = store
                .call(ApplyJobEventRequest {
                    worker_id:  worker.id.clone(),
                    session_id: "session-2".into(),
                    event:      JobEvent {
                        job_id: job.id.clone(),
                        attempt: 1,
                        sequence,
                        kind,
                        progress: None,
                        prompt_id: None,
                        message: String::new(),
                        unix_ms: now_unix_ms(),
                    },
                })
                .await
                .unwrap();
            assert!(result.changed);
        }
        let replay = store
            .call(ApplyJobEventRequest {
                worker_id:  worker.id,
                session_id: "session-2".into(),
                event:      JobEvent {
                    job_id:    job.id,
                    attempt:   1,
                    sequence:  4,
                    kind:      JobEventKind::Completed,
                    progress:  Some(1.0),
                    prompt_id: None,
                    message:   String::new(),
                    unix_ms:   now_unix_ms(),
                },
            })
            .await
            .unwrap();
        assert!(!replay.changed);
        assert!(replay.needs_quota_settlement);
    }

    #[tokio::test]
    async fn uploading_job_cannot_be_cancelled_to_avoid_billing() {
        let store = MediaStore::memory();
        let worker = online_worker(&store).await;
        let job = store.call(create_job("uploading")).await.unwrap();
        for (sequence, kind) in [
            (1, JobEventKind::Accepted),
            (2, JobEventKind::Running),
            (3, JobEventKind::Uploading),
        ] {
            store
                .call(ApplyJobEventRequest {
                    worker_id:  worker.id.clone(),
                    session_id: "session-1".into(),
                    event:      JobEvent {
                        job_id: job.id.clone(),
                        attempt: 1,
                        sequence,
                        kind,
                        progress: None,
                        prompt_id: None,
                        message: String::new(),
                        unix_ms: now_unix_ms(),
                    },
                })
                .await
                .unwrap();
        }

        assert!(matches!(
            store
                .call(RequestJobCancellation {
                    job_id:  job.id.clone(),
                    user_id: 1,
                })
                .await
                .unwrap_err(),
            MediaError::Conflict(_)
        ));
        assert!(matches!(
            store
                .call(ApplyJobEventRequest {
                    worker_id:  worker.id,
                    session_id: "session-1".into(),
                    event:      JobEvent {
                        job_id:    job.id,
                        attempt:   1,
                        sequence:  4,
                        kind:      JobEventKind::Cancelled,
                        progress:  None,
                        prompt_id: None,
                        message:   String::new(),
                        unix_ms:   now_unix_ms(),
                    },
                })
                .await
                .unwrap_err(),
            MediaError::Conflict(_)
        ));
    }

    #[tokio::test]
    async fn sqlite_state_survives_reopen() {
        let path = std::env::temp_dir().join(format!("halolake-media-{}.db", Uuid::new_v4()));
        let url = format!("sqlite://{}", path.display());
        let store = MediaStore::sqlite(&url).await.unwrap();
        let worker = online_worker(&store).await;
        drop(store);
        let reopened = MediaStore::sqlite(&url).await.unwrap();
        assert_eq!(
            reopened
                .call(GetWorkerRequest {
                    worker_id: worker.id,
                })
                .await
                .unwrap()
                .status,
            WorkerStatus::Online
        );
        let _ = std::fs::remove_file(path);
    }
}
