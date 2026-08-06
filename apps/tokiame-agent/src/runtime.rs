use crate::journal::AgentJournal;
use anyhow::{Context, Result};
use halolake_tokiame_protocol::{
    AgentMessage, ArtifactReady, ArtifactUpload, ArtifactUploaded, ArtifactUploadedAck, JobEvent,
    JobEventAck,
};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

type EventWaiters = Arc<Mutex<HashMap<(String, u64), oneshot::Sender<()>>>>;
type ArtifactTicketWaiters = Arc<Mutex<HashMap<String, (String, oneshot::Sender<ArtifactUpload>)>>>;
type ArtifactAckWaiters = Arc<Mutex<HashMap<String, (String, oneshot::Sender<()>)>>>;

#[derive(Debug, Clone)]
pub struct AgentRuntime {
    bus:              AgentBus,
    event_waiters:    EventWaiters,
    artifact_tickets: ArtifactTicketWaiters,
    artifact_acks:    ArtifactAckWaiters,
    cancellations:    Arc<Mutex<HashMap<String, watch::Sender<bool>>>>,
    active_jobs:      Arc<AtomicUsize>,
    queued_jobs:      Arc<AtomicUsize>,
}

#[derive(Debug, Clone)]
struct AgentBus {
    connection: watch::Sender<Option<mpsc::Sender<AgentMessage>>>,
}

impl AgentRuntime {
    pub fn new() -> Self {
        let (connection, _) = watch::channel(None);
        Self {
            bus:              AgentBus { connection },
            event_waiters:    Arc::new(Mutex::new(HashMap::new())),
            artifact_tickets: Arc::new(Mutex::new(HashMap::new())),
            artifact_acks:    Arc::new(Mutex::new(HashMap::new())),
            cancellations:    Arc::new(Mutex::new(HashMap::new())),
            active_jobs:      Arc::new(AtomicUsize::new(0)),
            queued_jobs:      Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn set_connection(&self, sender: mpsc::Sender<AgentMessage>) {
        self.bus.connection.send_replace(Some(sender));
    }

    pub fn clear_connection(&self) {
        self.bus.connection.send_replace(None);
    }

    pub async fn send(&self, message: AgentMessage) -> Result<()> {
        self.bus.send(message).await
    }

    pub async fn register_job(&self, job_id: &str) -> Option<watch::Receiver<bool>> {
        let mut jobs = self.cancellations.lock().await;
        if jobs.contains_key(job_id) {
            return None;
        }
        let (sender, receiver) = watch::channel(false);
        jobs.insert(job_id.to_string(), sender);
        self.queued_jobs.fetch_add(1, Ordering::Relaxed);
        Some(receiver)
    }

    pub async fn cancel_job(&self, job_id: &str) -> bool {
        let jobs = self.cancellations.lock().await;
        let known = jobs
            .get(job_id)
            .is_some_and(|sender| sender.send(true).is_ok());
        drop(jobs);
        if known {
            self.event_waiters
                .lock()
                .await
                .retain(|(waiting_job, _), _| waiting_job != job_id);
            self.artifact_tickets
                .lock()
                .await
                .retain(|_, (waiting_job, _)| waiting_job != job_id);
            self.artifact_acks
                .lock()
                .await
                .retain(|_, (waiting_job, _)| waiting_job != job_id);
        }
        known
    }

    async fn is_cancelled(&self, job_id: &str) -> bool {
        self.cancellations
            .lock()
            .await
            .get(job_id)
            .is_some_and(|sender| *sender.borrow())
    }

    pub async fn finish_job(&self, job_id: &str) {
        self.cancellations.lock().await.remove(job_id);
    }

    pub fn job_queue_guard(&self) -> JobQueueGuard {
        JobQueueGuard {
            queued_jobs: self.queued_jobs.clone(),
            active_jobs: self.active_jobs.clone(),
            queued:      true,
        }
    }

    pub fn metrics(&self) -> (u16, u16) {
        (
            self.active_jobs
                .load(Ordering::Relaxed)
                .min(u16::MAX as usize) as u16,
            self.queued_jobs
                .load(Ordering::Relaxed)
                .min(u16::MAX as usize) as u16,
        )
    }

    pub async fn send_job_event(
        &self,
        journal: &AgentJournal,
        event: JobEvent,
        status: Option<&str>,
    ) -> Result<()> {
        journal.set_pending_event(&event, status).await?;
        loop {
            if self.is_cancelled(&event.job_id).await {
                anyhow::bail!("job was cancelled");
            }
            let (sender, receiver) = oneshot::channel();
            self.event_waiters
                .lock()
                .await
                .insert((event.job_id.clone(), event.sequence), sender);
            self.send(AgentMessage::JobEvent(event.clone())).await?;
            if matches!(
                tokio::time::timeout(Duration::from_secs(30), receiver).await,
                Ok(Ok(()))
            ) {
                journal
                    .clear_pending_event(&event.job_id, event.sequence)
                    .await?;
                return Ok(());
            }
            self.event_waiters
                .lock()
                .await
                .remove(&(event.job_id.clone(), event.sequence));
        }
    }

    pub async fn resolve_job_event(&self, ack: JobEventAck) {
        if let Some(sender) = self
            .event_waiters
            .lock()
            .await
            .remove(&(ack.job_id, ack.sequence))
        {
            let _ = sender.send(());
        }
    }

    pub async fn request_artifact(&self, ready: ArtifactReady) -> Result<ArtifactUpload> {
        loop {
            if self.is_cancelled(&ready.job_id).await {
                anyhow::bail!("job was cancelled");
            }
            let (sender, receiver) = oneshot::channel();
            self.artifact_tickets
                .lock()
                .await
                .insert(ready.request_id.clone(), (ready.job_id.clone(), sender));
            self.send(AgentMessage::ArtifactReady(ready.clone()))
                .await?;
            if let Ok(Ok(upload)) = tokio::time::timeout(Duration::from_secs(30), receiver).await {
                return Ok(upload);
            }
            self.artifact_tickets.lock().await.remove(&ready.request_id);
        }
    }

    pub async fn resolve_artifact_ticket(&self, upload: ArtifactUpload) {
        if let Some((_, sender)) = self
            .artifact_tickets
            .lock()
            .await
            .remove(&upload.request_id)
        {
            let _ = sender.send(upload);
        }
    }

    pub async fn confirm_artifact(&self, uploaded: ArtifactUploaded) -> Result<()> {
        loop {
            if self.is_cancelled(&uploaded.job_id).await {
                anyhow::bail!("job was cancelled");
            }
            let (sender, receiver) = oneshot::channel();
            self.artifact_acks.lock().await.insert(
                uploaded.request_id.clone(),
                (uploaded.job_id.clone(), sender),
            );
            self.send(AgentMessage::ArtifactUploaded(uploaded.clone()))
                .await?;
            if matches!(
                tokio::time::timeout(Duration::from_secs(30), receiver).await,
                Ok(Ok(()))
            ) {
                return Ok(());
            }
            self.artifact_acks.lock().await.remove(&uploaded.request_id);
        }
    }

    pub async fn resolve_artifact_ack(&self, ack: ArtifactUploadedAck) {
        if let Some((_, sender)) = self.artifact_acks.lock().await.remove(&ack.request_id) {
            let _ = sender.send(());
        }
    }
}

impl AgentBus {
    async fn send(&self, message: AgentMessage) -> Result<()> {
        let mut connection = self.connection.subscribe();
        loop {
            let sender = connection.borrow().clone();
            if let Some(sender) = sender
                && sender.send(message.clone()).await.is_ok()
            {
                return Ok(());
            }
            connection
                .changed()
                .await
                .context("Tokiame connection bus closed")?;
        }
    }
}

pub struct JobActivityGuard {
    active_jobs: Arc<AtomicUsize>,
}

pub struct JobQueueGuard {
    queued_jobs: Arc<AtomicUsize>,
    active_jobs: Arc<AtomicUsize>,
    queued:      bool,
}

impl JobQueueGuard {
    pub fn activate(&mut self) -> JobActivityGuard {
        if self.queued {
            self.queued_jobs.fetch_sub(1, Ordering::Relaxed);
            self.queued = false;
        }
        self.active_jobs.fetch_add(1, Ordering::Relaxed);
        JobActivityGuard {
            active_jobs: self.active_jobs.clone(),
        }
    }
}

impl Drop for JobQueueGuard {
    fn drop(&mut self) {
        if self.queued {
            self.queued_jobs.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for JobActivityGuard {
    fn drop(&mut self) {
        self.active_jobs.fetch_sub(1, Ordering::Relaxed);
    }
}
