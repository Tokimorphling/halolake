//! Versioned control protocol between `tokiame-agent` and `tokiame-hub`.
//!
//! The protocol deliberately carries metadata only. Inputs and outputs travel
//! directly between agents/users and private object storage through short-lived
//! presigned requests.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;

pub const PROTOCOL_VERSION: u16 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentMessage {
    Register(Register),
    Heartbeat(Heartbeat),
    CommandAck(CommandAck),
    JobEvent(JobEvent),
    ArtifactReady(ArtifactReady),
    ArtifactUploaded(ArtifactUploaded),
    Pong(Pong),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum HubMessage {
    Registered(Registered),
    DispatchJob(DispatchJob),
    CancelJob(CancelJob),
    ArtifactUpload(ArtifactUpload),
    JobEventAck(JobEventAck),
    ArtifactUploadedAck(ArtifactUploadedAck),
    Ping(Ping),
    Error(ProtocolError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Register {
    pub protocol_version: u16,
    pub namespace:        String,
    pub node_name:        String,
    pub agent_version:    String,
    pub capabilities:     WorkerCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkerCapabilities {
    #[serde(default)]
    pub workflows:   Vec<WorkflowCapability>,
    #[serde(default = "default_concurrency")]
    pub concurrency: u16,
    #[serde(default)]
    pub labels:      BTreeMap<String, String>,
}

fn default_concurrency() -> u16 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowCapability {
    pub id:           String,
    pub version:      String,
    pub model:        String,
    #[serde(default)]
    pub output_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registered {
    pub worker_id:                  String,
    pub session_id:                 String,
    pub heartbeat_interval_seconds: u64,
    pub server_unix_ms:             i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub session_id:  String,
    pub sequence:    u64,
    pub active_jobs: u16,
    pub queued_jobs: u16,
    pub unix_ms:     i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchJob {
    pub command_id:       String,
    pub job_id:           String,
    pub attempt:          u32,
    pub model:            String,
    pub workflow_id:      String,
    pub workflow_version: String,
    #[serde(default)]
    pub params:           JsonValue,
    #[serde(default)]
    pub inputs:           Vec<JobInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobInput {
    pub artifact_id:  String,
    pub name:         String,
    pub content_type: String,
    pub size_bytes:   u64,
    pub sha256:       String,
    pub download:     PresignedRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelJob {
    pub command_id: String,
    pub job_id:     String,
    pub reason:     String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandAck {
    pub command_id: String,
    pub accepted:   bool,
    #[serde(default)]
    pub message:    String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobEventKind {
    Accepted,
    Running,
    Progress,
    Uploading,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobEvent {
    pub job_id:    String,
    pub attempt:   u32,
    pub sequence:  u64,
    pub kind:      JobEventKind,
    #[serde(default)]
    pub progress:  Option<f32>,
    #[serde(default)]
    pub prompt_id: Option<String>,
    #[serde(default)]
    pub message:   String,
    pub unix_ms:   i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobEventAck {
    pub job_id:   String,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactReady {
    pub request_id:   String,
    pub job_id:       String,
    pub attempt:      u32,
    pub name:         String,
    pub content_type: String,
    pub size_bytes:   u64,
    pub sha256:       String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactUpload {
    pub request_id:  String,
    pub artifact_id: String,
    pub upload:      PresignedRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactUploaded {
    pub request_id:  String,
    pub artifact_id: String,
    pub job_id:      String,
    pub attempt:     u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactUploadedAck {
    pub request_id:  String,
    pub artifact_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresignedRequest {
    pub method:             String,
    pub url:                String,
    #[serde(default)]
    pub headers:            BTreeMap<String, String>,
    pub expires_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub nonce: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pong {
    pub nonce: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code:      String,
    pub message:   String,
    #[serde(default)]
    pub retryable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_round_trips_without_binary_body() {
        let message = HubMessage::DispatchJob(DispatchJob {
            command_id:       "command-1".into(),
            job_id:           "job-1".into(),
            attempt:          1,
            model:            "comfy-portrait".into(),
            workflow_id:      "portrait".into(),
            workflow_version: "v1".into(),
            params:           serde_json::json!({"prompt": "hello"}),
            inputs:           vec![JobInput {
                artifact_id:  "input-1".into(),
                name:         "source.png".into(),
                content_type: "image/png".into(),
                size_bytes:   42,
                sha256:       "abc".into(),
                download:     PresignedRequest {
                    method:             "GET".into(),
                    url:                "https://objects.example/input".into(),
                    headers:            BTreeMap::new(),
                    expires_at_unix_ms: 100,
                },
            }],
        });
        let json = serde_json::to_string(&message).expect("serialize");
        assert!(!json.contains("body"));
        assert_eq!(serde_json::from_str::<HubMessage>(&json).unwrap(), message);
    }
}
