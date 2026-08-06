use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use halolake_tokiame_protocol::{
    AgentMessage, ArtifactReady, ArtifactUpload, ArtifactUploaded, ArtifactUploadedAck, CancelJob,
    CommandAck, DispatchJob, Heartbeat, HubMessage, JobEvent, JobEventAck, ProtocolError, Register,
    Registered,
};
use mimalloc::MiMalloc;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use service_async::Service;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tracing::{info, warn};
use uuid::Uuid;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const INTERNAL_KEY_HEADER: &str = "x-halolake-internal-key";

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, env = "HALOLAKE_TOKIAME_HUB_CONFIG")]
    config: String,
}

#[derive(Debug, Clone, Deserialize)]
struct HubConfig {
    #[serde(default)]
    server:  ServerConfig,
    control: ControlConfig,
}

impl HubConfig {
    fn load(path: &str) -> Result<Self> {
        let data = std::fs::read_to_string(path).with_context(|| format!("read config {path}"))?;
        let mut config: Self =
            toml::from_str(&data).with_context(|| format!("parse config {path}"))?;
        if let Ok(secret) = std::env::var("HALOLAKE_INTERNAL_SECRET")
            && !secret.trim().is_empty()
        {
            config.control.internal_secret = secret;
        }
        if config.control.internal_secret.trim().is_empty() {
            anyhow::bail!("control.internal_secret or HALOLAKE_INTERNAL_SECRET is required");
        }
        Ok(config)
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct ServerConfig {
    #[serde(default = "default_listen")]
    listen: SocketAddr,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
        }
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9091))
}

#[derive(Debug, Clone, Deserialize)]
struct ControlConfig {
    #[serde(default = "default_control_base_url")]
    base_url:        String,
    #[serde(default)]
    internal_secret: String,
}

fn default_control_base_url() -> String {
    "http://127.0.0.1:9090".into()
}

#[derive(Debug, Clone)]
struct AppState {
    sessions:        SessionRegistry,
    control:         ControlClient,
    internal_secret: Arc<str>,
}

#[derive(Debug, Clone, Default)]
struct SessionRegistry {
    inner: Arc<RwLock<HashMap<String, SessionHandle>>>,
}

#[derive(Debug, Clone)]
struct SessionHandle {
    session_id:   String,
    outbound:     mpsc::Sender<HubMessage>,
    pending_acks: Arc<Mutex<HashMap<String, oneshot::Sender<CommandAck>>>>,
}

impl SessionRegistry {
    async fn insert(&self, worker_id: String, handle: SessionHandle) {
        if let Some(previous) = self.inner.write().await.insert(worker_id, handle) {
            let _ = previous.outbound.try_send(HubMessage::Error(ProtocolError {
                code:      "session_replaced".into(),
                message:   "a newer connection replaced this worker session".into(),
                retryable: true,
            }));
        }
    }

    async fn remove_if_current(&self, worker_id: &str, session_id: &str) {
        let mut sessions = self.inner.write().await;
        if sessions
            .get(worker_id)
            .is_some_and(|session| session.session_id == session_id)
        {
            sessions.remove(worker_id);
        }
    }

    async fn count(&self) -> usize {
        self.inner.read().await.len()
    }

    async fn send_command(
        &self,
        worker_id: &str,
        session_id: &str,
        command_id: &str,
        message: HubMessage,
    ) -> Result<CommandAck, HubError> {
        let handle = self
            .inner
            .read()
            .await
            .get(worker_id)
            .filter(|handle| handle.session_id == session_id)
            .cloned()
            .ok_or_else(|| HubError::Conflict("worker session is not connected".into()))?;
        let (sender, receiver) = oneshot::channel();
        handle
            .pending_acks
            .lock()
            .await
            .insert(command_id.to_string(), sender);
        if handle.outbound.send(message).await.is_err() {
            handle.pending_acks.lock().await.remove(command_id);
            return Err(HubError::Unavailable("worker socket is closed".into()));
        }
        match tokio::time::timeout(Duration::from_secs(10), receiver).await {
            Ok(Ok(ack)) if ack.accepted => Ok(ack),
            Ok(Ok(ack)) => Err(HubError::Conflict(if ack.message.is_empty() {
                "agent rejected command".into()
            } else {
                ack.message
            })),
            Ok(Err(_)) => Err(HubError::Unavailable(
                "worker session closed before ACK".into(),
            )),
            Err(_) => {
                handle.pending_acks.lock().await.remove(command_id);
                Err(HubError::Unavailable("worker command ACK timed out".into()))
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum HubError {
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Control(String),
}

#[derive(Debug, Clone)]
struct ControlClient {
    client:          reqwest::Client,
    base_url:        Arc<str>,
    internal_secret: Arc<str>,
}

impl ControlClient {
    fn new(config: &ControlConfig) -> Result<Self> {
        Ok(Self {
            client:          reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(15))
                .build()
                .context("build control-api client")?,
            base_url:        Arc::from(config.base_url.trim_end_matches('/')),
            internal_secret: Arc::from(config.internal_secret.as_str()),
        })
    }

    async fn post<T: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<R, HubError> {
        let response = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .header(INTERNAL_KEY_HEADER, self.internal_secret.as_ref())
            .json(body)
            .send()
            .await
            .map_err(|error| HubError::Control(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Err(HubError::Control(format!(
                "control-api returned {status}: {}",
                truncate(&detail, 512)
            )));
        }
        response
            .json()
            .await
            .map_err(|error| HubError::Control(error.to_string()))
    }

    async fn post_empty<T: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<(), HubError> {
        let response = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .header(INTERNAL_KEY_HEADER, self.internal_secret.as_ref())
            .json(body)
            .send()
            .await
            .map_err(|error| HubError::Control(error.to_string()))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(HubError::Control(format!(
                "control-api returned {}",
                response.status()
            )))
        }
    }
}

#[derive(Debug)]
struct RegisterControl {
    worker_token: String,
    session_id:   String,
    register:     Register,
}

#[derive(Debug, Serialize)]
struct RegisterControlBody<'a> {
    worker_token: &'a str,
    session_id:   &'a str,
    register:     &'a Register,
}

#[derive(Debug, Deserialize)]
struct RegisterControlResult {
    worker_id:                  String,
    channel_id:                 u64,
    heartbeat_interval_seconds: u64,
}

impl Service<RegisterControl> for ControlClient {
    type Response = RegisterControlResult;
    type Error = HubError;

    async fn call(&self, req: RegisterControl) -> Result<Self::Response, Self::Error> {
        self.post("/internal/tokiame/register", &RegisterControlBody {
            worker_token: &req.worker_token,
            session_id:   &req.session_id,
            register:     &req.register,
        })
        .await
    }
}

#[derive(Debug, Serialize)]
struct HeartbeatControl<'a> {
    worker_id:   &'a str,
    session_id:  &'a str,
    sequence:    u64,
    active_jobs: u16,
    queued_jobs: u16,
    unix_ms:     i64,
}

impl<'a> Service<HeartbeatControl<'a>> for ControlClient {
    type Response = ();
    type Error = HubError;

    async fn call(&self, req: HeartbeatControl<'a>) -> Result<Self::Response, Self::Error> {
        self.post_empty("/internal/tokiame/heartbeat", &req).await
    }
}

#[derive(Debug, Serialize)]
struct SessionControl<'a> {
    worker_id:  &'a str,
    session_id: &'a str,
}

#[derive(Debug, Serialize)]
struct JobEventControl<'a> {
    worker_id:  &'a str,
    session_id: &'a str,
    event:      &'a JobEvent,
}

#[derive(Debug, Serialize)]
struct ArtifactReadyControl<'a> {
    worker_id:  &'a str,
    session_id: &'a str,
    ready:      &'a ArtifactReady,
}

#[derive(Debug, Serialize)]
struct ArtifactUploadedControl<'a> {
    worker_id:  &'a str,
    session_id: &'a str,
    uploaded:   &'a ArtifactUploaded,
}

#[derive(Debug, Deserialize)]
struct DispatchEnvelope {
    worker_id:  String,
    session_id: String,
    dispatch:   DispatchJob,
}

#[derive(Debug, Deserialize)]
struct CancelEnvelope {
    worker_id:  String,
    session_id: String,
    cancel:     CancelJob,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _telemetry = halolake_telemetry::init("halolake-tokiame-hub")?;
    let args = Args::parse();
    let config = HubConfig::load(&args.config)?;
    let listen = config.server.listen;
    let state = AppState {
        sessions:        SessionRegistry::default(),
        control:         ControlClient::new(&config.control)?,
        internal_secret: Arc::from(config.control.internal_secret.as_str()),
    };
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/tokiame/connect", get(connect))
        .route("/internal/tokiame/dispatch", post(dispatch))
        .route("/internal/tokiame/cancel", post(cancel))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .context("bind tokiame-hub listener")?;
    info!(%listen, "starting tokiame hub");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve tokiame hub")
}

async fn healthz(State(state): State<AppState>) -> Response {
    Json(json!({
        "status": "ok",
        "connected_workers": state.sessions.count().await,
    }))
    .into_response()
}

async fn connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(token) = bearer_token(&headers) else {
        return hub_error(HubError::Unauthorized(
            "worker bearer token is required".into(),
        ));
    };
    upgrade
        .max_message_size(1024 * 1024)
        .max_frame_size(1024 * 1024)
        .on_upgrade(move |socket| serve_worker(socket, state, token))
        .into_response()
}

async fn serve_worker(mut socket: WebSocket, state: AppState, worker_token: String) {
    let session_id = Uuid::new_v4().to_string();
    let register = match tokio::time::timeout(Duration::from_secs(10), socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<AgentMessage>(&text) {
            Ok(AgentMessage::Register(register)) => register,
            _ => {
                let _ = send_protocol_error(
                    &mut socket,
                    "register_required",
                    "first message must be register",
                    false,
                )
                .await;
                return;
            }
        },
        _ => {
            let _ = send_protocol_error(
                &mut socket,
                "register_timeout",
                "register was not received in time",
                true,
            )
            .await;
            return;
        }
    };
    let registered = match state
        .control
        .call(RegisterControl {
            worker_token,
            session_id: session_id.clone(),
            register,
        })
        .await
    {
        Ok(registered) => registered,
        Err(error) => {
            let _ = send_protocol_error(
                &mut socket,
                "registration_failed",
                &error.to_string(),
                false,
            )
            .await;
            return;
        }
    };
    let worker_id = registered.worker_id;
    let (outbound, mut outbound_rx) = mpsc::channel(128);
    let pending_acks = Arc::new(Mutex::new(HashMap::new()));
    state
        .sessions
        .insert(worker_id.clone(), SessionHandle {
            session_id: session_id.clone(),
            outbound,
            pending_acks: pending_acks.clone(),
        })
        .await;
    if send_hub_message(
        &mut socket,
        &HubMessage::Registered(Registered {
            worker_id:                  worker_id.clone(),
            session_id:                 session_id.clone(),
            heartbeat_interval_seconds: registered.heartbeat_interval_seconds,
            server_unix_ms:             now_unix_ms(),
        }),
    )
    .await
    .is_err()
    {
        state
            .sessions
            .remove_if_current(&worker_id, &session_id)
            .await;
        return;
    }
    info!(%worker_id, %session_id, channel_id = registered.channel_id, "Tokiame worker connected");

    loop {
        tokio::select! {
            outbound = outbound_rx.recv() => {
                let Some(outbound) = outbound else { break };
                if send_hub_message(&mut socket, &outbound).await.is_err() {
                    break;
                }
            }
            inbound = socket.recv() => {
                let Some(inbound) = inbound else { break };
                match inbound {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<AgentMessage>(&text) {
                            Ok(message) => {
                                if let Err(error) = handle_agent_message(
                                    &state,
                                    &worker_id,
                                    &session_id,
                                    &pending_acks,
                                    message,
                                    &mut socket,
                                ).await {
                                    warn!(%worker_id, %session_id, ?error, "agent message failed");
                                    let _ = send_protocol_error(&mut socket, "message_failed", &error.to_string(), true).await;
                                }
                            }
                            Err(error) => {
                                let _ = send_protocol_error(&mut socket, "invalid_json", &error.to_string(), false).await;
                            }
                        }
                    }
                    Ok(Message::Ping(payload)) => {
                        if socket.send(Message::Pong(payload)).await.is_err() { break; }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(Message::Pong(_)) => {}
                    Ok(Message::Binary(_)) => {
                        let _ = send_protocol_error(&mut socket, "binary_not_supported", "binary frames are not part of protocol v2", false).await;
                    }
                }
            }
        }
    }

    state
        .sessions
        .remove_if_current(&worker_id, &session_id)
        .await;
    let _ = state
        .control
        .post_empty("/internal/tokiame/disconnect", &SessionControl {
            worker_id:  &worker_id,
            session_id: &session_id,
        })
        .await;
    info!(%worker_id, %session_id, "Tokiame worker disconnected");
}

async fn handle_agent_message(
    state: &AppState,
    worker_id: &str,
    session_id: &str,
    pending_acks: &Arc<Mutex<HashMap<String, oneshot::Sender<CommandAck>>>>,
    message: AgentMessage,
    socket: &mut WebSocket,
) -> Result<(), HubError> {
    match message {
        AgentMessage::Register(_) => Err(HubError::Conflict("worker is already registered".into())),
        AgentMessage::Heartbeat(Heartbeat {
            session_id: reported_session,
            sequence,
            active_jobs,
            queued_jobs,
            unix_ms,
        }) => {
            if reported_session != session_id {
                return Err(HubError::Conflict(
                    "heartbeat session does not match socket".into(),
                ));
            }
            state
                .control
                .call(HeartbeatControl {
                    worker_id,
                    session_id,
                    sequence,
                    active_jobs,
                    queued_jobs,
                    unix_ms,
                })
                .await
        }
        AgentMessage::CommandAck(ack) => {
            if let Some(sender) = pending_acks.lock().await.remove(&ack.command_id) {
                let _ = sender.send(ack);
            }
            Ok(())
        }
        AgentMessage::JobEvent(event) => {
            let ack = JobEventAck {
                job_id:   event.job_id.clone(),
                sequence: event.sequence,
            };
            state
                .control
                .post::<_, JsonValue>("/internal/tokiame/job-event", &JobEventControl {
                    worker_id,
                    session_id,
                    event: &event,
                })
                .await?;
            send_hub_message(socket, &HubMessage::JobEventAck(ack))
                .await
                .map_err(|error| HubError::Unavailable(error.to_string()))
        }
        AgentMessage::ArtifactReady(ready) => {
            let upload = state
                .control
                .post::<_, ArtifactUpload>(
                    "/internal/tokiame/artifact-ready",
                    &ArtifactReadyControl {
                        worker_id,
                        session_id,
                        ready: &ready,
                    },
                )
                .await?;
            send_hub_message(socket, &HubMessage::ArtifactUpload(upload))
                .await
                .map_err(|error| HubError::Unavailable(error.to_string()))
        }
        AgentMessage::ArtifactUploaded(uploaded) => {
            let ack = ArtifactUploadedAck {
                request_id:  uploaded.request_id.clone(),
                artifact_id: uploaded.artifact_id.clone(),
            };
            state
                .control
                .post::<_, JsonValue>(
                    "/internal/tokiame/artifact-uploaded",
                    &ArtifactUploadedControl {
                        worker_id,
                        session_id,
                        uploaded: &uploaded,
                    },
                )
                .await?;
            send_hub_message(socket, &HubMessage::ArtifactUploadedAck(ack))
                .await
                .map_err(|error| HubError::Unavailable(error.to_string()))
        }
        AgentMessage::Pong(_) => Ok(()),
    }
}

async fn dispatch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<DispatchEnvelope>,
) -> Response {
    if !authorized(&headers, &state.internal_secret) {
        return hub_error(HubError::Unauthorized("invalid internal key".into()));
    }
    let command_id = payload.dispatch.command_id.clone();
    match state
        .sessions
        .send_command(
            &payload.worker_id,
            &payload.session_id,
            &command_id,
            HubMessage::DispatchJob(payload.dispatch),
        )
        .await
    {
        Ok(ack) => Json(ack).into_response(),
        Err(error) => hub_error(error),
    }
}

async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CancelEnvelope>,
) -> Response {
    if !authorized(&headers, &state.internal_secret) {
        return hub_error(HubError::Unauthorized("invalid internal key".into()));
    }
    let command_id = payload.cancel.command_id.clone();
    match state
        .sessions
        .send_command(
            &payload.worker_id,
            &payload.session_id,
            &command_id,
            HubMessage::CancelJob(payload.cancel),
        )
        .await
    {
        Ok(ack) => Json(ack).into_response(),
        Err(error) => hub_error(error),
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .trim()
                .strip_prefix("Bearer ")
                .or_else(|| value.trim().strip_prefix("bearer "))
        })
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

fn authorized(headers: &HeaderMap, secret: &str) -> bool {
    headers
        .get(INTERNAL_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| constant_time_eq(value.as_bytes(), secret.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in a.iter().zip(b) {
        diff |= left ^ right;
    }
    diff == 0
}

async fn send_hub_message(socket: &mut WebSocket, message: &HubMessage) -> Result<(), axum::Error> {
    let json = serde_json::to_string(message).expect("protocol messages are serializable");
    socket.send(Message::Text(json.into())).await
}

async fn send_protocol_error(
    socket: &mut WebSocket,
    code: &str,
    message: &str,
    retryable: bool,
) -> Result<(), axum::Error> {
    send_hub_message(
        socket,
        &HubMessage::Error(ProtocolError {
            code: code.into(),
            message: truncate(message, 512),
            retryable,
        }),
    )
    .await
}

fn hub_error(error: HubError) -> Response {
    let status = match &error {
        HubError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
        HubError::Conflict(_) => StatusCode::CONFLICT,
        HubError::Unavailable(_) | HubError::Control(_) => StatusCode::BAD_GATEWAY,
    };
    (
        status,
        Json(json!({"error":{"message":error.to_string(),"type":"tokiame_hub_error"}})),
    )
        .into_response()
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        warn!(?error, "failed to install ctrl-c handler");
    }
}

use serde_json::{Value as JsonValue, json};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_stays_parseable() {
        let config: HubConfig = toml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/tokiame-hub.toml"
        )))
        .unwrap();

        assert_eq!(config.server.listen.port(), 9091);
        assert_eq!(config.control.base_url, "http://127.0.0.1:9090");
    }

    #[test]
    fn bearer_token_never_accepts_query_or_non_bearer_credentials() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer_token(&headers), None);
        headers.insert(http::header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);
        headers.insert(
            http::header::AUTHORIZATION,
            "Bearer tw_secret".parse().unwrap(),
        );
        assert_eq!(bearer_token(&headers).as_deref(), Some("tw_secret"));
    }

    #[tokio::test]
    async fn stale_session_cannot_receive_commands() {
        let registry = SessionRegistry::default();
        let (sender, _receiver) = mpsc::channel(1);
        registry
            .insert("worker".into(), SessionHandle {
                session_id:   "new".into(),
                outbound:     sender,
                pending_acks: Arc::new(Mutex::new(HashMap::new())),
            })
            .await;
        let result = registry
            .send_command(
                "worker",
                "old",
                "command",
                HubMessage::Ping(halolake_tokiame_protocol::Ping {
                    nonce: "test".into(),
                }),
            )
            .await;
        assert!(matches!(result, Err(HubError::Conflict(_))));
    }
}
