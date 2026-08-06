//! Tokiame worker management and public media job APIs.

use crate::{
    AppState, api_error_status, api_success, management_error, publish_management_snapshot,
    require_role,
};
use async_stream::stream;
use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::LOCATION},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use data_encoding::HEXLOWER;
use halolake_control_plane::{
    ChannelStatusUpdateRequest, CreateChannelRequest, DeleteChannelRequest,
    PatchChannelModelStateRequest, SettleUsageIdempotentRequest, SettleUsageRequest,
    UsageEventBatch,
};
use halolake_domain::{
    CHANNEL_TYPE_TOKIAME, ChannelRecord, ROLE_ADMIN_USER, STATUS_AUTO_DISABLED, STATUS_ENABLED,
    STATUS_MANUALLY_DISABLED, TokenRecord, UsageEvent, UsageStatus, UserRecord,
};
use halolake_media::{
    ApplyJobEventRequest, ArtifactRecord, ArtifactRole, ArtifactState, AuthenticateWorkerRequest,
    BindJobSessionRequest, CompleteArtifactRequest, CreateArtifactRequest, CreateJobRequest,
    CreateWorkerRequest, DisconnectWorkerRequest, GetArtifactRequest, GetJobRequest, JobState,
    ListUserJobsRequest, ListWorkersRequest, MarkDispatchFailedRequest, MarkJobQuotaSettledRequest,
    MediaError, MediaJobRecord, RegisterWorkerRequest, RequestJobCancellation,
    RotateWorkerTokenRequest, SetWorkerEnabledRequest, WorkerHeartbeatRequest, WorkerRecord,
};
use halolake_tokiame_protocol::{
    ArtifactReady, ArtifactUpload, ArtifactUploaded, CancelJob, DispatchJob, JobEvent, JobInput,
    PROTOCOL_VERSION, Register,
};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use service_async::Service;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, convert::Infallible, net::SocketAddr, time::Duration};
use uuid::Uuid;

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/api/tokiame/workers",
            get(list_workers).post(create_worker),
        )
        .route(
            "/api/tokiame/workers/{worker_id}/rotate-token",
            post(rotate_worker_token),
        )
        .route(
            "/api/tokiame/workers/{worker_id}/enabled",
            axum::routing::put(set_worker_enabled),
        )
        .route("/v1/media/workflows", get(list_workflows))
        .route("/v1/media/uploads", post(create_upload))
        .route(
            "/v1/media/uploads/{artifact_id}/complete",
            post(complete_upload),
        )
        .route("/v1/media/jobs", get(list_jobs).post(create_job))
        .route("/v1/media/jobs/{job_id}", get(get_job).delete(cancel_job))
        .route("/v1/media/jobs/{job_id}/events", get(job_events))
        .route(
            "/v1/media/artifacts/{artifact_id}/content",
            get(artifact_content),
        )
        .route("/internal/tokiame/register", post(internal_register))
        .route("/internal/tokiame/heartbeat", post(internal_heartbeat))
        .route("/internal/tokiame/disconnect", post(internal_disconnect))
        .route("/internal/tokiame/job-event", post(internal_job_event))
        .route(
            "/internal/tokiame/artifact-ready",
            post(internal_artifact_ready),
        )
        .route(
            "/internal/tokiame/artifact-uploaded",
            post(internal_artifact_uploaded),
        )
}

#[derive(Debug, Deserialize)]
struct CreateWorkerPayload {
    namespace: String,
    #[serde(default)]
    node_name: String,
    #[serde(default)]
    group:     String,
}

#[derive(Debug, Deserialize)]
struct SetEnabledPayload {
    enabled: bool,
}

async fn create_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateWorkerPayload>,
) -> Response {
    if let Err(response) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return response;
    }
    let token = new_worker_token();
    let namespace = payload.namespace.trim().to_string();
    let node_name = if payload.node_name.trim().is_empty() {
        namespace.clone()
    } else {
        payload.node_name.trim().to_string()
    };
    let channel = match state
        .management
        .call(CreateChannelRequest {
            channel: tokiame_channel(&namespace, &payload.group),
        })
        .await
    {
        Ok(channel) => channel,
        Err(error) => return management_error(error),
    };
    let worker = match state
        .media
        .call(CreateWorkerRequest {
            namespace,
            node_name,
            token_hash: worker_token_hash(&token),
            group: payload.group,
            channel_id: channel.id,
        })
        .await
    {
        Ok(worker) => worker,
        Err(error) => {
            let _ = state
                .management
                .call(DeleteChannelRequest { id: channel.id })
                .await;
            return media_admin_error(error);
        }
    };
    let _ = state
        .management
        .call(PatchChannelModelStateRequest {
            id:              channel.id,
            expected_models: String::new(),
            models:          String::new(),
            setting_patch:   Some(json!({"tokiame_worker_id": worker.id}).to_string()),
        })
        .await;
    if let Err(error) = publish_management_snapshot(&state).await {
        return management_error(error);
    }
    api_success(json!({
        "worker": worker_payload(&worker),
        "token": token,
        "token_notice": "This token is shown once. Configure it on tokiame-agent.",
    }))
}

async fn list_workers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return response;
    }
    match state.media.call(ListWorkersRequest).await {
        Ok(workers) => api_success(
            workers
                .iter()
                .map(worker_payload)
                .collect::<Vec<JsonValue>>(),
        ),
        Err(error) => media_admin_error(error),
    }
}

async fn rotate_worker_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(worker_id): Path<String>,
) -> Response {
    if let Err(response) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return response;
    }
    let token = new_worker_token();
    match state
        .media
        .call(RotateWorkerTokenRequest {
            worker_id,
            token_hash: worker_token_hash(&token),
        })
        .await
    {
        Ok(worker) => {
            let _ = set_channel_status(&state, worker.channel_id, STATUS_AUTO_DISABLED).await;
            api_success(json!({"worker": worker_payload(&worker), "token": token}))
        }
        Err(error) => media_admin_error(error),
    }
}

async fn set_worker_enabled(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(worker_id): Path<String>,
    Json(payload): Json<SetEnabledPayload>,
) -> Response {
    if let Err(response) = require_role(&state, &headers, ROLE_ADMIN_USER).await {
        return response;
    }
    match state
        .media
        .call(SetWorkerEnabledRequest {
            worker_id,
            enabled: payload.enabled,
        })
        .await
    {
        Ok(worker) => {
            let status = if payload.enabled {
                STATUS_AUTO_DISABLED
            } else {
                STATUS_MANUALLY_DISABLED
            };
            if let Err(response) = set_channel_status(&state, worker.channel_id, status).await {
                return response;
            }
            api_success(worker_payload(&worker))
        }
        Err(error) => media_admin_error(error),
    }
}

#[derive(Debug, Deserialize)]
struct CreateUploadPayload {
    name:         String,
    content_type: String,
    size_bytes:   u64,
    sha256:       String,
}

#[derive(Debug, Deserialize)]
struct CompleteUploadPayload {
    size_bytes: u64,
    sha256:     String,
}

async fn create_upload(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<CreateUploadPayload>,
) -> Response {
    if let Err(response) = media_available(&state) {
        return response;
    }
    let auth = match authenticate_media_token(&state, &headers, peer, None) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    if payload.size_bytes == 0 || payload.size_bytes > state.media_config.max_input_bytes {
        return media_api_error(
            StatusCode::BAD_REQUEST,
            "input size is outside the configured limit",
        );
    }
    if !valid_sha256(&payload.sha256) || payload.content_type.trim().is_empty() {
        return media_api_error(
            StatusCode::BAD_REQUEST,
            "content_type and a lowercase/uppercase SHA-256 hex digest are required",
        );
    }
    let name = safe_filename(&payload.name);
    let object_key = format!(
        "{}/{}/inputs/{}/{}",
        clean_prefix(&state.media_config.artifact_prefix),
        auth.user.id,
        Uuid::new_v4().simple(),
        name
    );
    let upload = match state
        .object_store
        .presign_put(
            &object_key,
            payload.content_type.trim(),
            payload.size_bytes,
            &payload.sha256,
        )
        .await
    {
        Ok(upload) => upload,
        Err(error) => return object_store_error(error),
    };
    match state
        .media
        .call(CreateArtifactRequest {
            job_id: None,
            user_id: auth.user.id,
            role: ArtifactRole::Input,
            object_key,
            name,
            content_type: payload.content_type,
            size_bytes: payload.size_bytes,
            sha256: payload.sha256,
            expires_at_unix_ms: None,
        })
        .await
    {
        Ok(artifact) => Json(json!({
            "id": artifact.id,
            "state": artifact.state,
            "upload": upload,
        }))
        .into_response(),
        Err(error) => media_api_error_from(error),
    }
}

async fn complete_upload(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(artifact_id): Path<String>,
    Json(payload): Json<CompleteUploadPayload>,
) -> Response {
    let auth = match authenticate_media_token(&state, &headers, peer, None) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let artifact = match state
        .media
        .call(GetArtifactRequest {
            artifact_id: artifact_id.clone(),
        })
        .await
    {
        Ok(artifact)
            if artifact.user_id == auth.user.id && artifact.role == ArtifactRole::Input =>
        {
            artifact
        }
        Ok(_) | Err(MediaError::NotFound) => {
            return media_api_error(StatusCode::NOT_FOUND, "artifact not found");
        }
        Err(error) => return media_api_error_from(error),
    };
    if artifact.size_bytes != payload.size_bytes
        || !artifact.sha256.eq_ignore_ascii_case(&payload.sha256)
    {
        return media_api_error(
            StatusCode::CONFLICT,
            "completion metadata differs from the upload reservation",
        );
    }
    if let Err(response) = verify_object(&state, &artifact).await {
        return response;
    }
    match state
        .media
        .call(CompleteArtifactRequest {
            artifact_id,
            size_bytes: payload.size_bytes,
            sha256: payload.sha256,
        })
        .await
    {
        Ok(artifact) => Json(artifact_payload(&artifact)).into_response(),
        Err(error) => media_api_error_from(error),
    }
}

#[derive(Debug, Deserialize)]
struct CreateJobPayload {
    model:              String,
    workflow:           String,
    #[serde(default)]
    version:            Option<String>,
    #[serde(default)]
    parameters:         JsonValue,
    #[serde(default)]
    input_artifact_ids: Vec<String>,
}

async fn list_workflows(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let auth = match authenticate_media_token(&state, &headers, peer, None) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let workflows = state
        .media_config
        .workflows
        .iter()
        .filter(|workflow| token_allows_model(&auth.token, &workflow.model))
        .collect::<Vec<_>>();
    Json(json!({"object":"list", "data": workflows})).into_response()
}

async fn create_job(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<CreateJobPayload>,
) -> Response {
    if let Err(response) = media_available(&state) {
        return response;
    }
    let auth = match authenticate_media_token(&state, &headers, peer, Some(&payload.model)) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let workflow = match state.media_config.workflows.iter().find(|workflow| {
        workflow.id == payload.workflow
            && workflow.model == payload.model
            && payload
                .version
                .as_deref()
                .is_none_or(|version| version == workflow.version)
    }) {
        Some(workflow) => workflow.clone(),
        None => {
            return media_api_error(
                StatusCode::BAD_REQUEST,
                "workflow/model/version is not allowlisted",
            );
        }
    };
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let job = match state
        .media
        .call(CreateJobRequest {
            user_id: auth.user.id,
            token_id: auth.token.id,
            group: auth.group,
            model: workflow.model,
            workflow_id: workflow.id,
            workflow_version: workflow.version,
            params: if payload.parameters.is_null() {
                json!({})
            } else {
                payload.parameters
            },
            input_artifact_ids: payload.input_artifact_ids,
            idempotency_key,
            reserved_quota: workflow.quota,
            token_remaining_quota: auth.token.remain_quota,
            token_unlimited_quota: auth.token.unlimited_quota,
        })
        .await
    {
        Ok(job) => job,
        Err(error) => return media_api_error_from(error),
    };
    if job.state.is_terminal() {
        return Json(job_payload(&state, &job).await).into_response();
    }
    let dispatch = match build_dispatch(&state, &job).await {
        Ok(dispatch) => dispatch,
        Err(response) => return response,
    };
    match dispatch_to_hub(&state, &job, &dispatch).await {
        Ok(()) => {
            let _ = state.media_events.send(job.clone());
            (StatusCode::ACCEPTED, Json(job_payload(&state, &job).await)).into_response()
        }
        Err(message) => {
            let _ = state
                .media
                .call(MarkDispatchFailedRequest {
                    job_id:  job.id.clone(),
                    message: message.clone(),
                })
                .await;
            media_api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!(
                    "job {} was persisted but the worker dispatch failed: {message}; retry with \
                     the same Idempotency-Key",
                    job.id
                ),
            )
        }
    }
}

async fn get_job(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Response {
    let auth = match authenticate_media_token(&state, &headers, peer, None) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    match owned_job(&state, &job_id, auth.user.id).await {
        Ok(job) => Json(job_payload(&state, &job).await).into_response(),
        Err(response) => response,
    }
}

async fn list_jobs(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let auth = match authenticate_media_token(&state, &headers, peer, None) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    match state
        .media
        .call(ListUserJobsRequest {
            user_id: auth.user.id,
            limit:   100,
        })
        .await
    {
        Ok(jobs) => {
            let mut values = Vec::with_capacity(jobs.len());
            for job in jobs {
                values.push(job_payload(&state, &job).await);
            }
            Json(json!({"object":"list", "data": values})).into_response()
        }
        Err(error) => media_api_error_from(error),
    }
}

async fn cancel_job(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Response {
    let auth = match authenticate_media_token(&state, &headers, peer, None) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let job = match state
        .media
        .call(RequestJobCancellation {
            job_id,
            user_id: auth.user.id,
        })
        .await
    {
        Ok(job) => job,
        Err(error) => return media_api_error_from(error),
    };
    if job.state == JobState::Cancelled {
        let _ = cancel_on_hub(&state, &job).await;
    }
    let _ = state.media_events.send(job.clone());
    Json(job_payload(&state, &job).await).into_response()
}

async fn job_events(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> Response {
    let auth = match authenticate_media_token(&state, &headers, peer, None) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let initial = match owned_job(&state, &job_id, auth.user.id).await {
        Ok(job) => job,
        Err(response) => return response,
    };
    let mut receiver = state.media_events.subscribe();
    let stream = stream! {
        yield Ok::<Event, Infallible>(job_sse_event(&initial));
        loop {
            match receiver.recv().await {
                Ok(job) if job.id == job_id && job.user_id == auth.user.id => {
                    yield Ok(job_sse_event(&job));
                    if job.state.is_terminal() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    if let Ok(job) = state.media.call(GetJobRequest { job_id: job_id.clone() }).await {
                        yield Ok(job_sse_event(&job));
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

async fn artifact_content(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(artifact_id): Path<String>,
) -> Response {
    let auth = match authenticate_media_token(&state, &headers, peer, None) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    let artifact = match state.media.call(GetArtifactRequest { artifact_id }).await {
        Ok(artifact)
            if artifact.user_id == auth.user.id && artifact.state == ArtifactState::Ready =>
        {
            artifact
        }
        Ok(_) | Err(MediaError::NotFound) => {
            return media_api_error(StatusCode::NOT_FOUND, "artifact not found");
        }
        Err(error) => return media_api_error_from(error),
    };
    if artifact.role == ArtifactRole::Output {
        let Some(job_id) = artifact.job_id.as_deref() else {
            return media_api_error(StatusCode::NOT_FOUND, "artifact not found");
        };
        match state
            .media
            .call(GetJobRequest {
                job_id: job_id.to_string(),
            })
            .await
        {
            Ok(job) if job.state == JobState::Completed && job.quota_settled => {}
            Ok(_) | Err(MediaError::NotFound) => {
                return media_api_error(StatusCode::NOT_FOUND, "artifact not found");
            }
            Err(error) => return media_api_error_from(error),
        }
    }
    let download = match state.object_store.presign_get(&artifact.object_key).await {
        Ok(download) => download,
        Err(error) => return object_store_error(error),
    };
    match HeaderValue::from_str(&download.url) {
        Ok(location) => {
            let mut response = StatusCode::TEMPORARY_REDIRECT.into_response();
            response.headers_mut().insert(LOCATION, location);
            response
        }
        Err(_) => media_api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "object storage returned an invalid URL",
        ),
    }
}

#[derive(Debug, Deserialize)]
struct RegisterEnvelope {
    worker_token: String,
    session_id:   String,
    register:     Register,
}

#[derive(Debug, Serialize)]
struct RegisterResult {
    worker_id:                  String,
    channel_id:                 u64,
    heartbeat_interval_seconds: u64,
}

async fn internal_register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<RegisterEnvelope>,
) -> Response {
    if !state.authorized(&headers) {
        return media_api_error(StatusCode::UNAUTHORIZED, "invalid internal key");
    }
    if payload.register.protocol_version != PROTOCOL_VERSION {
        return media_api_error(
            StatusCode::BAD_REQUEST,
            "unsupported Tokiame protocol version",
        );
    }
    let authenticated = match state
        .media
        .call(AuthenticateWorkerRequest {
            token_hash: worker_token_hash(&payload.worker_token),
        })
        .await
    {
        Ok(worker) => worker,
        Err(error) => return media_api_error_from(error),
    };
    let worker = match state
        .media
        .call(RegisterWorkerRequest {
            worker_id:        authenticated.id,
            namespace:        payload.register.namespace,
            node_name:        payload.register.node_name,
            session_id:       payload.session_id,
            protocol_version: payload.register.protocol_version,
            agent_version:    payload.register.agent_version,
            capabilities:     payload.register.capabilities,
        })
        .await
    {
        Ok(worker) => worker,
        Err(error) => return media_api_error_from(error),
    };
    if let Err(response) = sync_worker_channel(&state, &worker, STATUS_ENABLED).await {
        return response;
    }
    Json(RegisterResult {
        worker_id:                  worker.id,
        channel_id:                 worker.channel_id,
        heartbeat_interval_seconds: state.media_config.heartbeat_interval_seconds.max(5),
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
struct HeartbeatEnvelope {
    worker_id:   String,
    session_id:  String,
    sequence:    u64,
    active_jobs: u16,
    queued_jobs: u16,
    unix_ms:     i64,
}

async fn internal_heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<HeartbeatEnvelope>,
) -> Response {
    if !state.authorized(&headers) {
        return media_api_error(StatusCode::UNAUTHORIZED, "invalid internal key");
    }
    match state
        .media
        .call(WorkerHeartbeatRequest {
            worker_id:   payload.worker_id,
            session_id:  payload.session_id,
            sequence:    payload.sequence,
            active_jobs: payload.active_jobs,
            queued_jobs: payload.queued_jobs,
            unix_ms:     payload.unix_ms,
        })
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => media_api_error_from(error),
    }
}

#[derive(Debug, Deserialize)]
struct SessionEnvelope {
    worker_id:  String,
    session_id: String,
}

async fn internal_disconnect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SessionEnvelope>,
) -> Response {
    if !state.authorized(&headers) {
        return media_api_error(StatusCode::UNAUTHORIZED, "invalid internal key");
    }
    match state
        .media
        .call(DisconnectWorkerRequest {
            worker_id:  payload.worker_id,
            session_id: payload.session_id,
        })
        .await
    {
        Ok(worker) => {
            if worker.session_id.is_none() {
                let _ = set_channel_status(&state, worker.channel_id, STATUS_AUTO_DISABLED).await;
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => media_api_error_from(error),
    }
}

#[derive(Debug, Deserialize)]
struct JobEventEnvelope {
    worker_id:  String,
    session_id: String,
    event:      JobEvent,
}

async fn internal_job_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<JobEventEnvelope>,
) -> Response {
    if !state.authorized(&headers) {
        return media_api_error(StatusCode::UNAUTHORIZED, "invalid internal key");
    }
    let result = match state
        .media
        .call(ApplyJobEventRequest {
            worker_id:  payload.worker_id,
            session_id: payload.session_id,
            event:      payload.event,
        })
        .await
    {
        Ok(result) => result,
        Err(error) => return media_api_error_from(error),
    };
    if result.needs_quota_settlement
        && let Err(response) = settle_media_job(&state, &result.job).await
    {
        return response;
    }
    let job = if result.needs_quota_settlement {
        state
            .media
            .call(GetJobRequest {
                job_id: result.job.id.clone(),
            })
            .await
            .unwrap_or(result.job)
    } else {
        result.job
    };
    if result.changed {
        let _ = state.media_events.send(job.clone());
    }
    Json(job_payload(&state, &job).await).into_response()
}

#[derive(Debug, Deserialize)]
struct ArtifactReadyEnvelope {
    worker_id:  String,
    session_id: String,
    ready:      ArtifactReady,
}

async fn internal_artifact_ready(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ArtifactReadyEnvelope>,
) -> Response {
    if !state.authorized(&headers) {
        return media_api_error(StatusCode::UNAUTHORIZED, "invalid internal key");
    }
    if payload.ready.size_bytes == 0
        || payload.ready.size_bytes > state.media_config.max_output_bytes
        || !valid_sha256(&payload.ready.sha256)
    {
        return media_api_error(StatusCode::BAD_REQUEST, "invalid output artifact metadata");
    }
    let job = match state
        .media
        .call(GetJobRequest {
            job_id: payload.ready.job_id.clone(),
        })
        .await
    {
        Ok(job) => job,
        Err(error) => return media_api_error_from(error),
    };
    if job.worker_id != payload.worker_id
        || job.session_id.as_deref() != Some(payload.session_id.as_str())
        || job.attempt != payload.ready.attempt
        || job.state != JobState::Uploading
        || job.state.is_terminal()
    {
        return media_api_error(StatusCode::CONFLICT, "stale output artifact session");
    }
    if let Some(workflow) =
        state.media_config.workflows.iter().find(|workflow| {
            workflow.id == job.workflow_id && workflow.version == job.workflow_version
        })
        && !workflow.output_types.is_empty()
        && !workflow
            .output_types
            .iter()
            .any(|content_type| content_type == &payload.ready.content_type)
    {
        return media_api_error(
            StatusCode::BAD_REQUEST,
            "output content type is not allowlisted",
        );
    }
    let name = safe_filename(&payload.ready.name);
    let object_key = format!(
        "{}/{}/outputs/{}/{}/{}",
        clean_prefix(&state.media_config.artifact_prefix),
        job.user_id,
        job.id,
        Uuid::new_v4().simple(),
        name
    );
    let artifact = match state
        .media
        .call(CreateArtifactRequest {
            job_id: Some(job.id),
            user_id: job.user_id,
            role: ArtifactRole::Output,
            object_key: object_key.clone(),
            name,
            content_type: payload.ready.content_type.clone(),
            size_bytes: payload.ready.size_bytes,
            sha256: payload.ready.sha256.clone(),
            expires_at_unix_ms: None,
        })
        .await
    {
        Ok(artifact) => artifact,
        Err(error) => return media_api_error_from(error),
    };
    match state
        .object_store
        .presign_put(
            &artifact.object_key,
            &artifact.content_type,
            artifact.size_bytes,
            &artifact.sha256,
        )
        .await
    {
        Ok(upload) => Json(ArtifactUpload {
            request_id: payload.ready.request_id,
            artifact_id: artifact.id,
            upload,
        })
        .into_response(),
        Err(error) => object_store_error(error),
    }
}

#[derive(Debug, Deserialize)]
struct ArtifactUploadedEnvelope {
    worker_id:  String,
    session_id: String,
    uploaded:   ArtifactUploaded,
}

async fn internal_artifact_uploaded(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ArtifactUploadedEnvelope>,
) -> Response {
    if !state.authorized(&headers) {
        return media_api_error(StatusCode::UNAUTHORIZED, "invalid internal key");
    }
    let job = match state
        .media
        .call(GetJobRequest {
            job_id: payload.uploaded.job_id,
        })
        .await
    {
        Ok(job) => job,
        Err(error) => return media_api_error_from(error),
    };
    if job.worker_id != payload.worker_id
        || job.session_id.as_deref() != Some(payload.session_id.as_str())
        || job.attempt != payload.uploaded.attempt
        || job.state != JobState::Uploading
    {
        return media_api_error(StatusCode::CONFLICT, "stale artifact completion session");
    }
    let artifact = match state
        .media
        .call(GetArtifactRequest {
            artifact_id: payload.uploaded.artifact_id.clone(),
        })
        .await
    {
        Ok(artifact)
            if artifact.job_id.as_deref() == Some(job.id.as_str())
                && artifact.role == ArtifactRole::Output =>
        {
            artifact
        }
        Ok(_) => return media_api_error(StatusCode::CONFLICT, "artifact does not belong to job"),
        Err(error) => return media_api_error_from(error),
    };
    if let Err(response) = verify_object(&state, &artifact).await {
        return response;
    }
    match state
        .media
        .call(CompleteArtifactRequest {
            artifact_id: artifact.id,
            size_bytes:  artifact.size_bytes,
            sha256:      artifact.sha256,
        })
        .await
    {
        Ok(artifact) => Json(artifact_payload(&artifact)).into_response(),
        Err(error) => media_api_error_from(error),
    }
}

#[derive(Debug)]
struct MediaAuth {
    token: TokenRecord,
    user:  UserRecord,
    group: String,
}

fn authenticate_media_token(
    state: &AppState,
    headers: &HeaderMap,
    peer: SocketAddr,
    model: Option<&str>,
) -> Result<MediaAuth, Response> {
    let keys = crate::http_auth::authorization_token_keys(headers)?;
    let data = state.management.current_data().map_err(management_error)?;
    let token = data
        .tokens
        .iter()
        .find(|token| keys.matches(&token.key))
        .cloned()
        .ok_or_else(|| media_api_error(StatusCode::UNAUTHORIZED, "token is invalid"))?;
    let now = crate::now_unix();
    if token.status != STATUS_ENABLED
        || (token.expired_time != -1 && token.expired_time < now)
        || (!token.unlimited_quota && token.remain_quota <= 0)
    {
        return Err(media_api_error(
            StatusCode::FORBIDDEN,
            "token is unavailable",
        ));
    }
    if let Some(model) = model
        && !token_allows_model(&token, model)
    {
        return Err(media_api_error(
            StatusCode::FORBIDDEN,
            "token may not use this model",
        ));
    }
    let user = data
        .users
        .iter()
        .find(|user| user.id == token.user_id && user.status == STATUS_ENABLED)
        .cloned()
        .ok_or_else(|| media_api_error(StatusCode::FORBIDDEN, "user is disabled"))?;
    if !token_ip_allowed(state, &token, headers, peer) {
        return Err(media_api_error(
            StatusCode::FORBIDDEN,
            "source IP is not allowed by token",
        ));
    }
    let group = if token.group.trim().is_empty() {
        if user.group.trim().is_empty() {
            "default".into()
        } else {
            user.group.clone()
        }
    } else {
        token.group.clone()
    };
    Ok(MediaAuth { token, user, group })
}

fn token_allows_model(token: &TokenRecord, model: &str) -> bool {
    !token.model_limits_enabled
        || token
            .allowed_models()
            .iter()
            .any(|allowed| allowed == model)
}

fn token_ip_allowed(
    state: &AppState,
    token: &TokenRecord,
    headers: &HeaderMap,
    peer: SocketAddr,
) -> bool {
    let Some(raw) = token
        .allow_ips
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return true;
    };
    let source = if state.media_config.trust_x_forwarded_for {
        headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or_else(|| peer.ip())
    } else {
        peer.ip()
    };
    raw.split([',', '\n', '\r'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter_map(|value| value.parse::<IpNet>().ok())
        .any(|network| network.contains(&source))
}

async fn build_dispatch(state: &AppState, job: &MediaJobRecord) -> Result<DispatchJob, Response> {
    let mut inputs = Vec::with_capacity(job.input_artifact_ids.len());
    for artifact_id in &job.input_artifact_ids {
        let artifact = state
            .media
            .call(GetArtifactRequest {
                artifact_id: artifact_id.clone(),
            })
            .await
            .map_err(media_api_error_from)?;
        let download = state
            .object_store
            .presign_get(&artifact.object_key)
            .await
            .map_err(object_store_error)?;
        inputs.push(JobInput {
            artifact_id: artifact.id,
            name: artifact.name,
            content_type: artifact.content_type,
            size_bytes: artifact.size_bytes,
            sha256: artifact.sha256,
            download,
        });
    }
    Ok(DispatchJob {
        command_id: Uuid::new_v4().to_string(),
        job_id: job.id.clone(),
        attempt: job.attempt,
        model: job.model.clone(),
        workflow_id: job.workflow_id.clone(),
        workflow_version: job.workflow_version.clone(),
        params: job.params.clone(),
        inputs,
    })
}

#[derive(Debug, Serialize)]
struct HubDispatchEnvelope<'a> {
    worker_id:  &'a str,
    session_id: &'a str,
    dispatch:   &'a DispatchJob,
}

async fn dispatch_to_hub(
    state: &AppState,
    job: &MediaJobRecord,
    dispatch: &DispatchJob,
) -> Result<(), String> {
    let session_id = job
        .session_id
        .as_deref()
        .ok_or_else(|| "worker has no active session".to_string())?;
    let secret = state
        .internal_secret
        .as_deref()
        .ok_or_else(|| "internal secret is not configured".to_string())?;
    let response = state
        .media_http
        .post(format!(
            "{}/internal/tokiame/dispatch",
            state.media_config.hub_base_url.trim_end_matches('/')
        ))
        .header(crate::INTERNAL_KEY_HEADER, secret)
        .json(&HubDispatchEnvelope {
            worker_id: &job.worker_id,
            session_id,
            dispatch,
        })
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if response.status().is_success() {
        state
            .media
            .call(BindJobSessionRequest {
                job_id:     job.id.clone(),
                session_id: session_id.to_string(),
            })
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    } else {
        Err(format!("hub returned {}", response.status()))
    }
}

#[derive(Debug, Serialize)]
struct HubCancelEnvelope<'a> {
    worker_id:  &'a str,
    session_id: &'a str,
    cancel:     CancelJob,
}

async fn cancel_on_hub(state: &AppState, job: &MediaJobRecord) -> Result<(), String> {
    let session_id = job
        .session_id
        .as_deref()
        .ok_or_else(|| "worker has no active session".to_string())?;
    let secret = state
        .internal_secret
        .as_deref()
        .ok_or_else(|| "internal secret is not configured".to_string())?;
    let cancel = CancelJob {
        command_id: Uuid::new_v4().to_string(),
        job_id:     job.id.clone(),
        reason:     "cancelled by user".into(),
    };
    let response = state
        .media_http
        .post(format!(
            "{}/internal/tokiame/cancel",
            state.media_config.hub_base_url.trim_end_matches('/')
        ))
        .header(crate::INTERNAL_KEY_HEADER, secret)
        .json(&HubCancelEnvelope {
            worker_id: &job.worker_id,
            session_id,
            cancel,
        })
        .send()
        .await
        .map_err(|error| error.to_string())?;
    response
        .status()
        .is_success()
        .then_some(())
        .ok_or_else(|| format!("hub returned {}", response.status()))
}

async fn settle_media_job(state: &AppState, job: &MediaJobRecord) -> Result<(), Response> {
    if job.quota_settled {
        return Ok(());
    }
    let quota = job.reserved_quota.max(0);
    if quota > 0 {
        let event = UsageEvent {
            request_id:            format!("media:{}", job.id),
            user_id:               job.user_id.to_string(),
            token_id:              job.token_id.to_string(),
            channel_id:            job.channel_id.to_string(),
            group:                 job.group.clone(),
            model:                 job.model.clone(),
            upstream_model:        format!("{}@{}", job.workflow_id, job.workflow_version),
            prompt_tokens:         None,
            completion_tokens:     None,
            total_tokens:          None,
            cache_read_tokens:     None,
            cache_creation_tokens: None,
            image_tokens:          None,
            audio_tokens:          None,
            quota:                 Some(quota),
            status:                UsageStatus::Success,
            latency_ms:            job
                .finished_at_unix_ms
                .unwrap_or(job.updated_at_unix_ms)
                .saturating_sub(job.created_at_unix_ms)
                .max(0) as u64,
            first_response_ms:     None,
            is_stream:             false,
            ip:                    String::new(),
            upstream_request_id:   job.prompt_id.clone().unwrap_or_default(),
            created_at_unix_ms:    job.finished_at_unix_ms.unwrap_or(job.updated_at_unix_ms),
        };
        state
            .usage_events
            .record_batch(UsageEventBatch::new(vec![event.clone()]))
            .await
            .map_err(crate::http_response::usage_error)?;
        let pricing = state
            .options
            .values()
            .map(|options| crate::usage_pricing_from_options(&options))
            .unwrap_or_default();
        let settlement = state
            .management
            .call(SettleUsageIdempotentRequest {
                settlement_key: event.request_id.clone(),
                request:        SettleUsageRequest {
                    events: vec![event],
                    pricing,
                },
            })
            .await
            .map_err(management_error)?;
        state
            .usage_events
            .apply_quotas(&settlement.event_quotas)
            .await
            .map_err(crate::http_response::usage_error)?;
        if settlement.tokens_exhausted > 0 {
            publish_management_snapshot(state)
                .await
                .map_err(management_error)?;
        }
    }
    state
        .media
        .call(MarkJobQuotaSettledRequest {
            job_id:        job.id.clone(),
            charged_quota: quota,
        })
        .await
        .map_err(media_api_error_from)?;
    Ok(())
}

async fn sync_worker_channel(
    state: &AppState,
    worker: &WorkerRecord,
    status: i32,
) -> Result<(), Response> {
    let mut models = worker
        .capabilities
        .workflows
        .iter()
        .map(|workflow| workflow.model.trim())
        .filter(|model| !model.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    models.sort_unstable();
    let current = state
        .management
        .current_data()
        .map_err(management_error)?
        .channels
        .into_iter()
        .find(|channel| channel.id == worker.channel_id)
        .ok_or_else(|| media_api_error(StatusCode::NOT_FOUND, "worker channel not found"))?;
    state
        .management
        .call(PatchChannelModelStateRequest {
            id:              worker.channel_id,
            expected_models: current.models,
            models:          models.join(","),
            setting_patch:   Some(
                json!({
                    "tokiame_worker_id": worker.id,
                    "tokiame_namespace": worker.namespace,
                    "tokiame_protocol": worker.protocol_version,
                    "tokiame_agent_version": worker.agent_version,
                })
                .to_string(),
            ),
        })
        .await
        .map_err(management_error)?;
    set_channel_status(state, worker.channel_id, status).await
}

async fn set_channel_status(
    state: &AppState,
    channel_id: u64,
    status: i32,
) -> Result<(), Response> {
    state
        .management
        .call(ChannelStatusUpdateRequest {
            id: channel_id,
            status,
        })
        .await
        .map_err(management_error)?;
    publish_management_snapshot(state)
        .await
        .map_err(management_error)
}

async fn verify_object(state: &AppState, artifact: &ArtifactRecord) -> Result<(), Response> {
    let metadata = state
        .object_store
        .head(&artifact.object_key)
        .await
        .map_err(object_store_error)?;
    if metadata.size_bytes != artifact.size_bytes
        || metadata
            .sha256
            .as_deref()
            .is_none_or(|sha256| !sha256.eq_ignore_ascii_case(&artifact.sha256))
    {
        return Err(media_api_error(
            StatusCode::CONFLICT,
            "object size or SHA-256 metadata does not match the reservation",
        ));
    }
    Ok(())
}

async fn owned_job(
    state: &AppState,
    job_id: &str,
    user_id: u64,
) -> Result<MediaJobRecord, Response> {
    match state
        .media
        .call(GetJobRequest {
            job_id: job_id.to_string(),
        })
        .await
    {
        Ok(job) if job.user_id == user_id => Ok(job),
        Ok(_) | Err(MediaError::NotFound) => {
            Err(media_api_error(StatusCode::NOT_FOUND, "job not found"))
        }
        Err(error) => Err(media_api_error_from(error)),
    }
}

async fn job_payload(state: &AppState, job: &MediaJobRecord) -> JsonValue {
    let data = state.media.current_data().await;
    let outputs = if job.state == JobState::Completed && job.quota_settled {
        job.output_artifact_ids
            .iter()
            .filter_map(|id| data.artifacts.get(id))
            .map(artifact_payload)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    json!({
        "id": job.id,
        "object": "media.job",
        "state": job.state,
        "model": job.model,
        "workflow": job.workflow_id,
        "workflow_version": job.workflow_version,
        "progress": job.progress,
        "input_artifact_ids": job.input_artifact_ids,
        "outputs": outputs,
        "reserved_quota": job.reserved_quota,
        "charged_quota": job.charged_quota,
        "quota_settled": job.quota_settled,
        "error": job.error,
        "created_at": job.created_at_unix_ms,
        "updated_at": job.updated_at_unix_ms,
        "finished_at": job.finished_at_unix_ms,
    })
}

fn artifact_payload(artifact: &ArtifactRecord) -> JsonValue {
    json!({
        "id": artifact.id,
        "object": "media.artifact",
        "role": artifact.role,
        "state": artifact.state,
        "name": artifact.name,
        "content_type": artifact.content_type,
        "size_bytes": artifact.size_bytes,
        "sha256": artifact.sha256,
        "content_url": format!("/v1/media/artifacts/{}/content", artifact.id),
        "created_at": artifact.created_at_unix_ms,
    })
}

fn worker_payload(worker: &WorkerRecord) -> JsonValue {
    json!({
        "id": worker.id,
        "namespace": worker.namespace,
        "node_name": worker.node_name,
        "group": worker.group,
        "channel_id": worker.channel_id,
        "enabled": worker.enabled,
        "status": worker.status,
        "protocol_version": worker.protocol_version,
        "agent_version": worker.agent_version,
        "capabilities": worker.capabilities,
        "active_jobs": worker.active_jobs,
        "queued_jobs": worker.queued_jobs,
        "connected_at": worker.connected_at_unix_ms,
        "last_heartbeat": worker.last_heartbeat_unix_ms,
        "created_at": worker.created_at_unix_ms,
    })
}

fn job_sse_event(job: &MediaJobRecord) -> Event {
    Event::default()
        .event("job")
        .id(job.last_event_sequence.to_string())
        .json_data(json!({
            "id": job.id,
            "state": job.state,
            "progress": job.progress,
            "error": job.error,
            "updated_at": job.updated_at_unix_ms,
        }))
        .unwrap_or_else(|_| Event::default().event("error").data("serialization error"))
}

fn tokiame_channel(namespace: &str, group: &str) -> ChannelRecord {
    ChannelRecord {
        id:                   0,
        snapshot_id:          None,
        channel_type:         CHANNEL_TYPE_TOKIAME,
        key:                  String::new(),
        status:               STATUS_AUTO_DISABLED,
        name:                 format!("Tokiame / {namespace}"),
        weight:               Some(1),
        created_time:         crate::now_unix(),
        test_time:            0,
        response_time:        0,
        base_url:             Some(format!("tokiame://{namespace}")),
        balance:              0.0,
        balance_updated_time: 0,
        models:               String::new(),
        group:                if group.trim().is_empty() {
            "default".into()
        } else {
            group.trim().into()
        },
        used_quota:           0,
        model_mapping:        None,
        priority:             None,
        auto_ban:             Some(0),
        tag:                  Some("tokiame".into()),
        setting:              Some(json!({"managed_by":"tokiame"}).to_string()),
        param_override:       None,
        header_override:      None,
        remark:               Some(
            "Managed automatically by the Tokiame media control plane".into(),
        ),
        proxy_id:             None,
    }
}

fn new_worker_token() -> String {
    format!("tw_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn worker_token_hash(token: &str) -> String {
    HEXLOWER.encode(&Sha256::digest(token.trim().as_bytes()))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn safe_filename(name: &str) -> String {
    let name = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("artifact.bin")
        .chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect::<String>();
    if name.trim().is_empty() {
        "artifact.bin".into()
    } else {
        name
    }
}

fn clean_prefix(prefix: &str) -> String {
    prefix
        .split('/')
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .collect::<Vec<_>>()
        .join("/")
}

fn media_available(state: &AppState) -> Result<(), Response> {
    if state.media_config.enabled && state.object_store.is_enabled() {
        Ok(())
    } else {
        Err(media_api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "media service is disabled",
        ))
    }
}

fn media_admin_error(error: MediaError) -> Response {
    let status = match error {
        MediaError::NotFound => StatusCode::NOT_FOUND,
        MediaError::Duplicate | MediaError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
        MediaError::Unauthorized => StatusCode::UNAUTHORIZED,
        MediaError::Conflict(_) => StatusCode::CONFLICT,
        MediaError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    api_error_status(status, &error.to_string())
}

fn media_api_error_from(error: MediaError) -> Response {
    let status = match error {
        MediaError::NotFound => StatusCode::NOT_FOUND,
        MediaError::Duplicate | MediaError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
        MediaError::Unauthorized => StatusCode::UNAUTHORIZED,
        MediaError::Conflict(_) => StatusCode::CONFLICT,
        MediaError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    media_api_error(status, &error.to_string())
}

fn object_store_error(error: halolake_object_store::ObjectStoreError) -> Response {
    tracing::warn!(?error, "media object storage operation failed");
    media_api_error(StatusCode::BAD_GATEWAY, &error.to_string())
}

fn media_api_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "media_error"
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_tokens_are_high_entropy_and_only_hash_is_persisted() {
        let token = new_worker_token();
        assert!(token.starts_with("tw_"));
        assert_eq!(token.len(), 67);
        assert_eq!(worker_token_hash(&token).len(), 64);
        assert_ne!(worker_token_hash(&token), token);
    }

    #[test]
    fn filenames_and_prefixes_cannot_escape_object_namespace() {
        assert_eq!(safe_filename("../../source.png"), "source.png");
        assert_eq!(clean_prefix("/media/../tenant//"), "media/tenant");
    }
}
