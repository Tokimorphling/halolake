use crate::{
    config::AgentConfig,
    journal::{AgentJournal, JournalJob},
    runtime::{AgentRuntime, JobQueueGuard},
    workflow::WorkflowCatalog,
};
use anyhow::{Context, Result, bail};
use data_encoding::HEXLOWER;
use futures_util::StreamExt;
use halolake_tokiame_protocol::{
    ArtifactReady, ArtifactUploaded, DispatchJob, JobEvent, JobEventKind, PresignedRequest,
};
use reqwest::{
    Body, Method,
    multipart::{Form, Part},
};
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    sync::{Semaphore, watch},
};
use tokio_util::io::ReaderStream;
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct JobRunner {
    config:    Arc<AgentConfig>,
    workflows: Arc<WorkflowCatalog>,
    journal:   AgentJournal,
    runtime:   AgentRuntime,
    client:    reqwest::Client,
    slots:     Arc<Semaphore>,
}

#[derive(Debug, Clone)]
struct OutputRef {
    filename:     String,
    subfolder:    String,
    storage_type: String,
    content_type: String,
}

struct EventUpdate {
    kind:      JobEventKind,
    progress:  Option<f32>,
    prompt_id: Option<String>,
    message:   String,
    status:    &'static str,
}

enum HistoryState {
    Pending,
    Complete(Vec<OutputRef>),
    Failed(String),
}

impl JobRunner {
    pub fn new(
        config: Arc<AgentConfig>,
        workflows: Arc<WorkflowCatalog>,
        journal: AgentJournal,
        runtime: AgentRuntime,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .build()
            .context("build ComfyUI/object client")?;
        Ok(Self {
            slots: Arc::new(Semaphore::new(usize::from(config.worker.concurrency))),
            config,
            workflows,
            journal,
            runtime,
            client,
        })
    }

    pub async fn execute(&self, record: JournalJob, mut cancelled: watch::Receiver<bool>) {
        let job_id = record.dispatch.job_id.clone();
        let attempt = record.dispatch.attempt;
        let mut sequence = record.event_sequence;
        let mut queue_guard = self.runtime.job_queue_guard();
        let result = self
            .execute_inner(record, &mut sequence, &mut cancelled, &mut queue_guard)
            .await;
        if let Err(error) = result {
            if *cancelled.borrow() {
                let _ = self.journal.set_status(&job_id, "cancelled").await;
            } else {
                warn!(%job_id, ?error, "ComfyUI job failed");
                let _ = self
                    .emit(&job_id, attempt, &mut sequence, EventUpdate {
                        kind:      JobEventKind::Failed,
                        progress:  None,
                        prompt_id: None,
                        message:   truncate(&error.to_string(), 1_000),
                        status:    "failed",
                    })
                    .await;
            }
        }
        self.runtime.finish_job(&job_id).await;
    }

    async fn execute_inner(
        &self,
        mut record: JournalJob,
        sequence: &mut u64,
        cancelled: &mut watch::Receiver<bool>,
        queue_guard: &mut JobQueueGuard,
    ) -> Result<()> {
        let dispatch = record.dispatch.clone();
        self.workflows.validate(&dispatch)?;
        if let Some(pending) = record.pending_event.take() {
            self.runtime
                .send_job_event(&self.journal, pending, None)
                .await?;
        }
        if matches!(record.status.as_str(), "completed" | "failed" | "cancelled") {
            return Ok(());
        }
        if record.status == "received" {
            self.emit(&dispatch.job_id, dispatch.attempt, sequence, EventUpdate {
                kind:      JobEventKind::Accepted,
                progress:  Some(0.0),
                prompt_id: None,
                message:   String::new(),
                status:    "accepted",
            })
            .await?;
            record.status = "accepted".into();
        }

        let permit = tokio::select! {
            permit = self.slots.clone().acquire_owned() => permit.context("job semaphore closed")?,
            _ = cancelled.changed() => {
                self.journal.set_status(&dispatch.job_id, "cancelled").await?;
                return Ok(());
            }
        };
        if *cancelled.borrow() {
            self.journal
                .set_status(&dispatch.job_id, "cancelled")
                .await?;
            return Ok(());
        }
        let _activity = queue_guard.activate();
        let _permit = permit;
        info!(job_id = %dispatch.job_id, workflow = %dispatch.workflow_id, "starting ComfyUI job");

        let input_names = self.prepare_inputs(&dispatch).await?;
        let prompt_id = if let Some(prompt_id) = record.prompt_id.clone() {
            prompt_id
        } else {
            let workflow = self.workflows.render(&dispatch, &input_names)?;
            let prompt_id = self.submit_prompt(&dispatch.job_id, workflow).await?;
            self.journal
                .set_prompt_id(&dispatch.job_id, &prompt_id)
                .await?;
            prompt_id
        };
        if !matches!(record.status.as_str(), "running" | "uploading") {
            self.emit(&dispatch.job_id, dispatch.attempt, sequence, EventUpdate {
                kind:      JobEventKind::Running,
                progress:  None,
                prompt_id: Some(prompt_id.clone()),
                message:   String::new(),
                status:    "running",
            })
            .await?;
        }

        let outputs = loop {
            if *cancelled.borrow() {
                self.cancel_prompt(&prompt_id).await;
                self.journal
                    .set_status(&dispatch.job_id, "cancelled")
                    .await?;
                return Ok(());
            }
            match self.history(&prompt_id).await? {
                HistoryState::Pending => {}
                HistoryState::Complete(outputs) => break outputs,
                HistoryState::Failed(message) => bail!("ComfyUI execution failed: {message}"),
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(self.config.comfyui.poll_interval_ms.max(200))) => {}
                _ = cancelled.changed() => {}
            }
        };

        self.emit(&dispatch.job_id, dispatch.attempt, sequence, EventUpdate {
            kind:      JobEventKind::Uploading,
            progress:  Some(0.95),
            prompt_id: Some(prompt_id.clone()),
            message:   String::new(),
            status:    "uploading",
        })
        .await?;
        for (index, output) in outputs.iter().enumerate() {
            if *cancelled.borrow() {
                self.journal
                    .set_status(&dispatch.job_id, "cancelled")
                    .await?;
                return Ok(());
            }
            self.upload_output(&dispatch, index, output).await?;
        }
        self.emit(&dispatch.job_id, dispatch.attempt, sequence, EventUpdate {
            kind:      JobEventKind::Completed,
            progress:  Some(1.0),
            prompt_id: Some(prompt_id),
            message:   String::new(),
            status:    "completed",
        })
        .await?;
        info!(job_id = %dispatch.job_id, "ComfyUI job completed");
        Ok(())
    }

    async fn emit(
        &self,
        job_id: &str,
        attempt: u32,
        sequence: &mut u64,
        update: EventUpdate,
    ) -> Result<()> {
        *sequence = sequence.saturating_add(1);
        self.runtime
            .send_job_event(
                &self.journal,
                JobEvent {
                    job_id: job_id.to_string(),
                    attempt,
                    sequence: *sequence,
                    kind: update.kind,
                    progress: update.progress,
                    prompt_id: update.prompt_id,
                    message: update.message,
                    unix_ms: now_unix_ms(),
                },
                Some(update.status),
            )
            .await
    }

    async fn prepare_inputs(&self, dispatch: &DispatchJob) -> Result<Vec<String>> {
        let input_dir = self.config.work_dir.join(&dispatch.job_id).join("inputs");
        tokio::fs::create_dir_all(&input_dir).await?;
        let mut comfy_names = Vec::with_capacity(dispatch.inputs.len());
        for (index, input) in dispatch.inputs.iter().enumerate() {
            let local_name = format!("{index:03}-{}", safe_filename(&input.name));
            let local_path = input_dir.join(&local_name);
            download_to_file(
                &self.client,
                &input.download,
                &local_path,
                input.size_bytes,
                &input.sha256,
            )
            .await
            .with_context(|| format!("download input artifact {}", input.artifact_id))?;
            let comfy_name = if let Some(comfy_input_dir) = &self.config.comfyui.input_dir {
                tokio::fs::create_dir_all(comfy_input_dir).await?;
                let name = format!("halolake-{}-{local_name}", dispatch.job_id.replace('-', ""));
                tokio::fs::copy(&local_path, comfy_input_dir.join(&name)).await?;
                name
            } else {
                self.upload_comfy_input(&local_path, &local_name).await?
            };
            comfy_names.push(comfy_name);
        }
        Ok(comfy_names)
    }

    async fn upload_comfy_input(&self, path: &Path, name: &str) -> Result<String> {
        let file = tokio::fs::File::open(path).await?;
        let size = file.metadata().await?.len();
        let part = Part::stream_with_length(Body::wrap_stream(ReaderStream::new(file)), size)
            .file_name(name.to_string());
        let response = self
            .client
            .post(format!(
                "{}/upload/image",
                self.config.comfyui.base_url.trim_end_matches('/')
            ))
            .multipart(
                Form::new()
                    .part("image", part)
                    .text("type", "input")
                    .text("overwrite", "true"),
            )
            .send()
            .await?
            .error_for_status()?;
        let payload: JsonValue = response.json().await?;
        payload
            .get("name")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("ComfyUI upload response did not contain name"))
    }

    async fn submit_prompt(&self, job_id: &str, workflow: JsonValue) -> Result<String> {
        let response = self
            .client
            .post(format!(
                "{}/prompt",
                self.config.comfyui.base_url.trim_end_matches('/')
            ))
            .json(&json!({
                "prompt": workflow,
                "client_id": format!("halolake-{}", self.config.worker.node_name),
                "extra_data": {"halolake_job_id": job_id},
            }))
            .send()
            .await?
            .error_for_status()?;
        let payload: JsonValue = response.json().await?;
        if let Some(errors) = payload.get("node_errors")
            && errors.as_object().is_some_and(|errors| !errors.is_empty())
        {
            bail!(
                "ComfyUI rejected workflow nodes: {}",
                truncate(&errors.to_string(), 1_000)
            );
        }
        payload
            .get("prompt_id")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("ComfyUI prompt response did not contain prompt_id"))
    }

    async fn history(&self, prompt_id: &str) -> Result<HistoryState> {
        let response = self
            .client
            .get(format!(
                "{}/history/{}",
                self.config.comfyui.base_url.trim_end_matches('/'),
                prompt_id
            ))
            .send()
            .await?
            .error_for_status()?;
        let payload: JsonValue = response.json().await?;
        let Some(entry) = payload.get(prompt_id) else {
            return Ok(HistoryState::Pending);
        };
        if entry
            .pointer("/status/status_str")
            .and_then(JsonValue::as_str)
            .is_some_and(|status| status.eq_ignore_ascii_case("error"))
        {
            return Ok(HistoryState::Failed(
                entry
                    .get("status")
                    .map(JsonValue::to_string)
                    .unwrap_or_else(|| "unknown execution error".into()),
            ));
        }
        let outputs = extract_outputs(entry.get("outputs"));
        if outputs.is_empty() {
            if entry
                .pointer("/status/completed")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false)
            {
                return Ok(HistoryState::Failed(
                    "workflow completed without image/video/audio artifacts".into(),
                ));
            }
            return Ok(HistoryState::Pending);
        }
        Ok(HistoryState::Complete(outputs))
    }

    async fn cancel_prompt(&self, prompt_id: &str) {
        let result = self
            .client
            .post(format!(
                "{}/queue",
                self.config.comfyui.base_url.trim_end_matches('/')
            ))
            .json(&json!({"delete":[prompt_id]}))
            .send()
            .await;
        if let Err(error) = result {
            warn!(%prompt_id, ?error, "failed to delete ComfyUI queue item");
        }
    }

    async fn upload_output(
        &self,
        dispatch: &DispatchJob,
        index: usize,
        output: &OutputRef,
    ) -> Result<()> {
        let output_dir = self.config.work_dir.join(&dispatch.job_id).join("outputs");
        tokio::fs::create_dir_all(&output_dir).await?;
        let path = output_dir.join(format!("{index:03}-{}", safe_filename(&output.filename)));
        let response = self
            .client
            .get(format!(
                "{}/view",
                self.config.comfyui.base_url.trim_end_matches('/')
            ))
            .query(&[
                ("filename", output.filename.as_str()),
                ("subfolder", output.subfolder.as_str()),
                ("type", output.storage_type.as_str()),
            ])
            .send()
            .await?
            .error_for_status()?;
        let (size_bytes, sha256) =
            stream_response_to_file(response, &path, self.config.comfyui.max_output_bytes).await?;
        let request_id = Uuid::new_v4().to_string();
        let ticket = self
            .runtime
            .request_artifact(ArtifactReady {
                request_id: request_id.clone(),
                job_id: dispatch.job_id.clone(),
                attempt: dispatch.attempt,
                name: safe_filename(&output.filename),
                content_type: output.content_type.clone(),
                size_bytes,
                sha256: sha256.clone(),
            })
            .await?;
        upload_file(&self.client, &ticket.upload, &path).await?;
        self.runtime
            .confirm_artifact(ArtifactUploaded {
                request_id,
                artifact_id: ticket.artifact_id,
                job_id: dispatch.job_id.clone(),
                attempt: dispatch.attempt,
            })
            .await?;
        let _ = tokio::fs::remove_file(path).await;
        Ok(())
    }
}

async fn download_to_file(
    client: &reqwest::Client,
    request: &PresignedRequest,
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<()> {
    if request.method != "GET" {
        bail!("input presigned request must use GET");
    }
    let mut builder = client.get(&request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    let response = builder.send().await?.error_for_status()?;
    let (size, sha256) = stream_response_to_file(response, path, expected_size).await?;
    if size != expected_size || !sha256.eq_ignore_ascii_case(expected_sha256) {
        bail!("downloaded input size or SHA-256 does not match control metadata");
    }
    Ok(())
}

async fn stream_response_to_file(
    response: reqwest::Response,
    path: &Path,
    max_bytes: u64,
) -> Result<(u64, String)> {
    if max_bytes == 0 {
        bail!("stream size limit must be greater than zero");
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        bail!("response content length exceeds the configured size limit");
    }
    let mut file = tokio::fs::File::create(path).await?;
    let mut stream = response.bytes_stream();
    let mut hash = Sha256::new();
    let mut size = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let next_size = size.saturating_add(chunk.len() as u64);
        if next_size > max_bytes {
            drop(file);
            let _ = tokio::fs::remove_file(path).await;
            bail!("stream exceeded the configured size limit");
        }
        size = next_size;
        hash.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok((size, HEXLOWER.encode(&hash.finalize())))
}

async fn upload_file(
    client: &reqwest::Client,
    request: &PresignedRequest,
    path: &Path,
) -> Result<()> {
    if request.method != "PUT" {
        bail!("output presigned request must use PUT");
    }
    let file = tokio::fs::File::open(path).await?;
    let mut builder = client.request(Method::PUT, &request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    builder
        .body(Body::wrap_stream(ReaderStream::new(file)))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

fn extract_outputs(outputs: Option<&JsonValue>) -> Vec<OutputRef> {
    let Some(outputs) = outputs.and_then(JsonValue::as_object) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut unique = BTreeSet::new();
    for node in outputs.values().filter_map(JsonValue::as_object) {
        for key in ["images", "gifs", "videos", "audio"] {
            let Some(items) = node.get(key).and_then(JsonValue::as_array) else {
                continue;
            };
            for item in items.iter().filter_map(JsonValue::as_object) {
                let Some(filename) = item.get("filename").and_then(JsonValue::as_str) else {
                    continue;
                };
                let subfolder = item
                    .get("subfolder")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                let storage_type = item
                    .get("type")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("output");
                let identity = format!("{storage_type}/{subfolder}/{filename}");
                if unique.insert(identity) {
                    found.push(OutputRef {
                        filename:     filename.to_string(),
                        subfolder:    subfolder.to_string(),
                        storage_type: storage_type.to_string(),
                        content_type: content_type_for(filename, key).into(),
                    });
                }
            }
        }
    }
    found
}

fn content_type_for(filename: &str, kind: &str) -> &'static str {
    match filename
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("wav") => "audio/wav",
        Some("mp3") => "audio/mpeg",
        _ if kind == "videos" => "video/mp4",
        _ if kind == "audio" => "audio/wav",
        _ => "application/octet-stream",
    }
}

fn safe_filename(name: &str) -> String {
    let value = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("artifact.bin")
        .chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect::<String>();
    if value.trim().is_empty() {
        "artifact.bin".into()
    } else {
        value
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_image_and_video_outputs_without_unknown_json_scanning() {
        let value = json!({
            "1": {"images":[{"filename":"a.png","subfolder":"","type":"output"}]},
            "2": {"videos":[{"filename":"b.mp4","subfolder":"clips","type":"output"}]}
        });
        let outputs = extract_outputs(Some(&value));
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].content_type, "image/png");
        assert_eq!(outputs[1].content_type, "video/mp4");
    }
}
