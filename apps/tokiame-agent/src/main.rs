mod config;
mod journal;
mod runner;
mod runtime;
mod workflow;

use anyhow::{Context, Result, bail};
use clap::Parser;
use config::AgentConfig;
use futures_util::{SinkExt, StreamExt};
use halolake_tokiame_protocol::{
    AgentMessage, CommandAck, Heartbeat, HubMessage, PROTOCOL_VERSION, Pong, Register,
    WorkerCapabilities,
};
use journal::{AgentJournal, JournalJob};
use mimalloc::MiMalloc;
use runner::JobRunner;
use runtime::AgentRuntime;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message as WsMessage, client::IntoClientRequest},
};
use tracing::{info, warn};
use workflow::WorkflowCatalog;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, env = "TOKIAME_AGENT_CONFIG")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _telemetry = halolake_telemetry::init("halolake-tokiame-agent")?;
    let args = Args::parse();
    let config = Arc::new(AgentConfig::load(&args.config)?);
    tokio::fs::create_dir_all(&config.work_dir)
        .await
        .context("create agent work directory")?;
    let workflows = Arc::new(WorkflowCatalog::load(&config.workflows)?);
    let journal = AgentJournal::open(&config.state.sqlite_url).await?;
    let runtime = AgentRuntime::new();
    let runner = JobRunner::new(
        config.clone(),
        workflows.clone(),
        journal.clone(),
        runtime.clone(),
    )?;

    for record in journal.unfinished().await? {
        spawn_job(&runtime, &runner, record).await;
    }

    tokio::select! {
        result = reconnect_loop(config, workflows, journal, runtime, runner) => result,
        result = tokio::signal::ctrl_c() => {
            result.context("install ctrl-c handler")?;
            Ok(())
        }
    }
}

async fn reconnect_loop(
    config: Arc<AgentConfig>,
    workflows: Arc<WorkflowCatalog>,
    journal: AgentJournal,
    runtime: AgentRuntime,
    runner: JobRunner,
) -> Result<()> {
    let mut delay = 1u64;
    loop {
        match run_connection(&config, &workflows, &journal, &runtime, &runner).await {
            Ok(()) => warn!("Tokiame WebSocket closed"),
            Err(error) => warn!(?error, "Tokiame connection failed"),
        }
        runtime.clear_connection();
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = delay
            .saturating_mul(2)
            .min(config.gateway.reconnect_max_seconds.max(1));
    }
}

async fn run_connection(
    config: &AgentConfig,
    workflows: &WorkflowCatalog,
    journal: &AgentJournal,
    runtime: &AgentRuntime,
    runner: &JobRunner,
) -> Result<()> {
    let mut request = config
        .gateway
        .url
        .clone()
        .into_client_request()
        .context("parse gateway WebSocket URL")?;
    request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        format!("Bearer {}", config.worker.token)
            .parse()
            .context("encode worker authorization header")?,
    );
    request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::USER_AGENT,
        format!("halolake-tokiame-agent/{}", env!("CARGO_PKG_VERSION"))
            .parse()
            .expect("static user agent"),
    );
    let (socket, _) = connect_async(request)
        .await
        .context("connect Tokiame WebSocket")?;
    let (mut sink, mut source) = socket.split();
    let register = AgentMessage::Register(Register {
        protocol_version: PROTOCOL_VERSION,
        namespace:        config.worker.namespace.clone(),
        node_name:        config.worker.node_name.clone(),
        agent_version:    env!("CARGO_PKG_VERSION").into(),
        capabilities:     WorkerCapabilities {
            workflows:   workflows.capabilities(),
            concurrency: config.worker.concurrency,
            labels:      config.worker.labels.clone(),
        },
    });
    sink.send(protocol_message(&register)?).await?;
    let registered = match tokio::time::timeout(Duration::from_secs(15), source.next()).await {
        Ok(Some(Ok(WsMessage::Text(text)))) => match serde_json::from_str::<HubMessage>(&text)? {
            HubMessage::Registered(registered) => registered,
            HubMessage::Error(error) => bail!("hub registration failed: {}", error.message),
            other => bail!("unexpected registration response: {other:?}"),
        },
        Ok(Some(Ok(_))) => bail!("hub registration response was not text"),
        Ok(Some(Err(error))) => return Err(error.into()),
        Ok(None) => bail!("hub closed during registration"),
        Err(_) => bail!("hub registration timed out"),
    };
    let (outbound, mut outbound_rx) = mpsc::channel(256);
    runtime.set_connection(outbound);
    info!(
        worker_id = %registered.worker_id,
        session_id = %registered.session_id,
        "connected to Tokiame hub"
    );
    let mut heartbeat = tokio::time::interval(Duration::from_secs(
        registered.heartbeat_interval_seconds.max(5),
    ));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut heartbeat_sequence = 0u64;

    loop {
        tokio::select! {
            outbound = outbound_rx.recv() => {
                let Some(outbound) = outbound else { return Ok(()) };
                sink.send(protocol_message(&outbound)?).await?;
            }
            _ = heartbeat.tick() => {
                heartbeat_sequence = heartbeat_sequence.saturating_add(1);
                let (active_jobs, queued_jobs) = runtime.metrics();
                sink.send(protocol_message(&AgentMessage::Heartbeat(Heartbeat {
                    session_id: registered.session_id.clone(),
                    sequence: heartbeat_sequence,
                    active_jobs,
                    queued_jobs,
                    unix_ms: now_unix_ms(),
                }))?).await?;
            }
            incoming = source.next() => {
                match incoming {
                    Some(Ok(WsMessage::Text(text))) => {
                        let message = serde_json::from_str::<HubMessage>(&text)
                            .context("decode hub message")?;
                        handle_hub_message(message, journal, runtime, runner, workflows).await?;
                    }
                    Some(Ok(WsMessage::Ping(payload))) => sink.send(WsMessage::Pong(payload)).await?,
                    Some(Ok(WsMessage::Pong(_))) => {}
                    Some(Ok(WsMessage::Close(_))) | None => return Ok(()),
                    Some(Ok(WsMessage::Binary(_))) => bail!("hub sent unsupported binary frame"),
                    Some(Ok(WsMessage::Frame(_))) => {}
                    Some(Err(error)) => return Err(error.into()),
                }
            }
        }
    }
}

async fn handle_hub_message(
    message: HubMessage,
    journal: &AgentJournal,
    runtime: &AgentRuntime,
    runner: &JobRunner,
    workflows: &WorkflowCatalog,
) -> Result<()> {
    match message {
        HubMessage::DispatchJob(dispatch) => {
            let command_id = dispatch.command_id.clone();
            let result = async {
                workflows.validate(&dispatch)?;
                let record = journal.upsert_dispatch(&dispatch).await?;
                if !matches!(record.status.as_str(), "completed" | "failed" | "cancelled") {
                    spawn_job(runtime, runner, record).await;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            runtime
                .send(AgentMessage::CommandAck(CommandAck {
                    command_id,
                    accepted: result.is_ok(),
                    message: result
                        .err()
                        .map(|error| truncate(&error.to_string(), 512))
                        .unwrap_or_default(),
                }))
                .await?;
        }
        HubMessage::CancelJob(cancel) => {
            let known = runtime.cancel_job(&cancel.job_id).await
                || journal.get(&cancel.job_id).await?.is_some();
            if known {
                journal.set_status(&cancel.job_id, "cancelled").await?;
            }
            runtime
                .send(AgentMessage::CommandAck(CommandAck {
                    command_id: cancel.command_id,
                    accepted:   known,
                    message:    if known {
                        String::new()
                    } else {
                        "job is unknown".into()
                    },
                }))
                .await?;
        }
        HubMessage::ArtifactUpload(upload) => runtime.resolve_artifact_ticket(upload).await,
        HubMessage::JobEventAck(ack) => runtime.resolve_job_event(ack).await,
        HubMessage::ArtifactUploadedAck(ack) => runtime.resolve_artifact_ack(ack).await,
        HubMessage::Ping(ping) => {
            runtime
                .send(AgentMessage::Pong(Pong { nonce: ping.nonce }))
                .await?;
        }
        HubMessage::Error(error) => {
            warn!(code = %error.code, retryable = error.retryable, message = %error.message, "hub protocol error");
        }
        HubMessage::Registered(_) => warn!("hub sent duplicate registered message"),
    }
    Ok(())
}

async fn spawn_job(runtime: &AgentRuntime, runner: &JobRunner, record: JournalJob) {
    let job_id = record.dispatch.job_id.clone();
    let Some(cancelled) = runtime.register_job(&job_id).await else {
        return;
    };
    let runner = runner.clone();
    tokio::spawn(async move {
        runner.execute(record, cancelled).await;
    });
}

fn protocol_message(message: &AgentMessage) -> Result<WsMessage> {
    Ok(WsMessage::Text(serde_json::to_string(message)?.into()))
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}
