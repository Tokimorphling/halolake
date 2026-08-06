use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub gateway:   GatewayConfig,
    pub worker:    WorkerConfig,
    #[serde(default)]
    pub comfyui:   ComfyUiConfig,
    #[serde(default)]
    pub state:     StateConfig,
    #[serde(default)]
    pub work_dir:  PathBuf,
    #[serde(default)]
    pub workflows: Vec<WorkflowConfig>,
}

impl AgentConfig {
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| format!("read config {path}"))?;
        let mut config: Self =
            toml::from_str(&raw).with_context(|| format!("parse config {path}"))?;
        if let Ok(token) = std::env::var("TOKIAME_WORKER_TOKEN")
            && !token.trim().is_empty()
        {
            config.worker.token = token;
        }
        if config.worker.token.trim().is_empty() {
            bail!("worker.token or TOKIAME_WORKER_TOKEN is required");
        }
        if config.worker.namespace.trim().is_empty() || config.worker.node_name.trim().is_empty() {
            bail!("worker.namespace and worker.node_name are required");
        }
        if config.worker.concurrency == 0 {
            bail!("worker.concurrency must be greater than zero");
        }
        if config.comfyui.max_output_bytes == 0
            || config.comfyui.max_output_bytes > 5 * 1024 * 1024 * 1024
        {
            bail!("comfyui.max_output_bytes must be between 1 byte and 5 GiB");
        }
        if config.workflows.is_empty() {
            bail!("at least one [[workflows]] entry is required");
        }
        let base = Path::new(path).parent().unwrap_or_else(|| Path::new("."));
        if config.work_dir.as_os_str().is_empty() {
            config.work_dir = base.join("tokiame-work");
        } else if config.work_dir.is_relative() {
            config.work_dir = base.join(&config.work_dir);
        }
        if let Some(input_dir) = config.comfyui.input_dir.as_mut()
            && input_dir.is_relative()
        {
            *input_dir = base.join(&*input_dir);
        }
        for workflow in &mut config.workflows {
            if workflow.file.is_relative() {
                workflow.file = base.join(&workflow.file);
            }
        }
        if config.state.sqlite_url.trim().is_empty() {
            config.state.sqlite_url =
                format!("sqlite://{}", base.join("tokiame-agent.db").display());
        }
        Ok(config)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    pub url:                   String,
    #[serde(default = "default_reconnect_max_seconds")]
    pub reconnect_max_seconds: u64,
}

fn default_reconnect_max_seconds() -> u64 {
    30
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkerConfig {
    pub token:       String,
    pub namespace:   String,
    pub node_name:   String,
    #[serde(default = "default_concurrency")]
    pub concurrency: u16,
    #[serde(default)]
    pub labels:      BTreeMap<String, String>,
}

fn default_concurrency() -> u16 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct ComfyUiConfig {
    #[serde(default = "default_comfyui_base_url")]
    pub base_url:         String,
    #[serde(default)]
    pub input_dir:        Option<PathBuf>,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: u64,
}

impl Default for ComfyUiConfig {
    fn default() -> Self {
        Self {
            base_url:         default_comfyui_base_url(),
            input_dir:        None,
            poll_interval_ms: default_poll_interval_ms(),
            max_output_bytes: default_max_output_bytes(),
        }
    }
}

fn default_comfyui_base_url() -> String {
    "http://127.0.0.1:8188".into()
}

fn default_poll_interval_ms() -> u64 {
    1_000
}

fn default_max_output_bytes() -> u64 {
    5 * 1024 * 1024 * 1024
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StateConfig {
    #[serde(default)]
    pub sqlite_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowConfig {
    pub id:           String,
    pub version:      String,
    pub model:        String,
    pub file:         PathBuf,
    #[serde(default)]
    pub output_types: Vec<String>,
    /// Maps user parameter names to RFC 6901 JSON pointers in the ComfyUI API workflow.
    #[serde(default)]
    pub parameters:   BTreeMap<String, String>,
    #[serde(default)]
    pub inputs:       Vec<InputBinding>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InputBinding {
    pub index:   usize,
    pub pointer: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_stays_parseable() {
        let config: AgentConfig = toml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/tokiame-agent.toml"
        )))
        .unwrap();

        assert_eq!(config.workflows.len(), 1);
        assert_eq!(config.workflows[0].parameters.len(), 6);
        assert_eq!(config.worker.concurrency, 1);
    }
}
