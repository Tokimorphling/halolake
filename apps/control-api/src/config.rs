//! Control-api configuration types and storage backend resolution.

use anyhow::{Context, Result, bail};
use halolake_domain::UserRecord;
use halolake_object_store::S3ObjectStoreConfig;
use halolake_router_core::{
    ChannelAffinityConfig, ChannelConfig, GatewaySnapshot, GroupRoutingConfig,
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::PathBuf,
};

#[derive(Debug, Clone, Deserialize)]
pub struct ControlApiConfig {
    #[serde(default)]
    pub server:         ServerConfig,
    #[serde(default)]
    pub internal:       InternalConfig,
    #[serde(default)]
    pub system:         SystemConfig,
    #[serde(default)]
    pub web:            WebConfig,
    #[serde(default)]
    pub storage:        StorageConfig,
    #[serde(default)]
    pub session:        SessionConfig,
    #[serde(default)]
    pub media:          MediaConfig,
    #[serde(default)]
    pub object_storage: ObjectStorageConfig,
    #[serde(default)]
    pub options:        BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub users:          Vec<UserRecord>,
    #[serde(default = "default_version")]
    pub version:        u64,
    #[serde(default)]
    pub tokens:         Vec<halolake_router_core::TokenConfig>,
    #[serde(default)]
    pub channels:       Vec<ChannelConfig>,
    #[serde(default)]
    pub model_mappings: Vec<halolake_router_core::ModelMapping>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ObjectStorageConfig {
    /// A private S3-compatible bucket. Presigned URLs are ephemeral and are
    /// never persisted in Halolake's database.
    #[serde(default)]
    pub s3: Option<S3ObjectStoreConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MediaConfig {
    #[serde(default)]
    pub enabled:                    bool,
    #[serde(default = "default_tokiame_hub_base_url")]
    pub hub_base_url:               String,
    #[serde(default = "default_heartbeat_interval_seconds")]
    pub heartbeat_interval_seconds: u64,
    #[serde(default = "default_artifact_prefix")]
    pub artifact_prefix:            String,
    #[serde(default = "default_max_input_bytes")]
    pub max_input_bytes:            u64,
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes:           u64,
    /// Trust the first `X-Forwarded-For` hop for token IP allowlists. Enable
    /// only when control-api is reachable exclusively through a trusted proxy.
    #[serde(default)]
    pub trust_x_forwarded_for:      bool,
    #[serde(default)]
    pub workflows:                  Vec<MediaWorkflowConfig>,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            enabled:                    false,
            hub_base_url:               default_tokiame_hub_base_url(),
            heartbeat_interval_seconds: default_heartbeat_interval_seconds(),
            artifact_prefix:            default_artifact_prefix(),
            max_input_bytes:            default_max_input_bytes(),
            max_output_bytes:           default_max_output_bytes(),
            trust_x_forwarded_for:      false,
            workflows:                  Vec::new(),
        }
    }
}

impl MediaConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let hub_url = reqwest::Url::parse(&self.hub_base_url)
            .context("media.hub_base_url must be an absolute URL")?;
        if !matches!(hub_url.scheme(), "http" | "https") {
            bail!("media.hub_base_url must use http or https");
        }
        if !(5..=300).contains(&self.heartbeat_interval_seconds) {
            bail!("media.heartbeat_interval_seconds must be between 5 and 300");
        }
        for (name, size) in [
            ("max_input_bytes", self.max_input_bytes),
            ("max_output_bytes", self.max_output_bytes),
        ] {
            if size == 0 || size > 5 * 1024 * 1024 * 1024 {
                bail!("media.{name} must be between 1 byte and 5 GiB");
            }
        }
        let prefix = self.artifact_prefix.trim_matches('/');
        if prefix.is_empty()
            || prefix.len() > 200
            || prefix
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
            || prefix.contains('\\')
            || prefix.chars().any(char::is_control)
        {
            bail!("media.artifact_prefix must be a safe relative object-key prefix");
        }
        if self.workflows.is_empty() {
            bail!("media.workflows must contain at least one allowlisted workflow");
        }
        let mut identities = BTreeSet::new();
        for workflow in &self.workflows {
            if workflow.id.trim().is_empty()
                || workflow.version.trim().is_empty()
                || workflow.model.trim().is_empty()
            {
                bail!("media workflow id, version and model are required");
            }
            if workflow.quota < 0 {
                bail!("media workflow quota cannot be negative");
            }
            if workflow.output_types.is_empty()
                || workflow.output_types.iter().any(|content_type| {
                    content_type.len() > 100
                        || !content_type.contains('/')
                        || content_type.chars().any(char::is_whitespace)
                })
            {
                bail!("media workflow output_types must contain valid MIME types");
            }
            if !identities.insert((
                workflow.id.as_str(),
                workflow.version.as_str(),
                workflow.model.as_str(),
            )) {
                bail!("duplicate media workflow id/version/model");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct MediaWorkflowConfig {
    pub id:           String,
    #[serde(default = "default_workflow_version")]
    pub version:      String,
    pub model:        String,
    #[serde(default)]
    pub quota:        i64,
    #[serde(default)]
    pub description:  String,
    #[serde(default)]
    pub output_types: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionConfig {
    /// HMAC secret for signing session cookies. Prefer env SESSION_SECRET.
    #[serde(default)]
    pub secret: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct InternalConfig {
    #[serde(default)]
    pub secret:           Option<String>,
    /// Base URL of the gateway data plane used by the playground proxy
    /// (`/pg/chat/completions`). Defaults to `http://127.0.0.1:8082`.
    #[serde(default)]
    pub gateway_base_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemConfig {
    #[serde(default = "default_system_name")]
    pub name: String,
    #[serde(default)]
    pub task_scheduler_enabled: bool,
    #[serde(default = "default_channel_test_interval_seconds")]
    pub channel_test_interval_seconds: u64,
    #[serde(default = "default_model_update_interval_seconds")]
    pub model_update_interval_seconds: u64,
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            name: default_system_name(),
            task_scheduler_enabled: false,
            channel_test_interval_seconds: default_channel_test_interval_seconds(),
            model_update_interval_seconds: default_model_update_interval_seconds(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebConfig {
    #[serde(default)]
    pub enabled:      bool,
    #[serde(default = "default_web_default_dist")]
    pub default_dist: PathBuf,
    #[serde(default = "default_web_classic_dist")]
    pub classic_dist: PathBuf,
    #[serde(default = "default_web_theme")]
    pub theme:        String,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled:      false,
            default_dist: default_web_default_dist(),
            classic_dist: default_web_classic_dist(),
            theme:        default_web_theme(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    #[serde(default)]
    pub backend:          StorageBackend,
    #[serde(default)]
    pub sqlite_url:       Option<String>,
    #[serde(default)]
    pub database_url:     Option<String>,
    #[serde(default)]
    pub log_backend:      Option<LogStorageBackend>,
    #[serde(default)]
    pub log_database_url: Option<String>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend:          StorageBackend::Memory,
            sqlite_url:       None,
            database_url:     None,
            log_backend:      None,
            log_database_url: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StorageBackend {
    #[default]
    Memory,
    Sqlite,
    #[serde(rename = "mysql", alias = "my_sql")]
    MySql,
    #[serde(rename = "postgres", alias = "postgresql")]
    Postgres,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogStorageBackend {
    Memory,
    Sqlite,
    #[serde(rename = "mysql", alias = "my_sql")]
    MySql,
    #[serde(rename = "postgres", alias = "postgresql")]
    Postgres,
    #[serde(rename = "clickhouse", alias = "click_house")]
    ClickHouse,
}

impl ControlApiConfig {
    pub fn load(path: &str) -> Result<Self> {
        let data = std::fs::read_to_string(path).with_context(|| format!("read config {path}"))?;
        let mut config: Self =
            toml::from_str(&data).with_context(|| format!("parse config {path}"))?;
        config.resolve_storage_env()?;
        config.resolve_channel_env_keys()?;
        Ok(config)
    }

    fn resolve_storage_env(&mut self) -> Result<()> {
        if self.storage.sqlite_url.is_none()
            && let Ok(path) = std::env::var("SQLITE_PATH")
            && !path.trim().is_empty()
        {
            self.storage.sqlite_url = Some(sqlite_url_from_path(path.trim()));
            if self.storage.backend == StorageBackend::Memory && self.storage.database_url.is_none()
            {
                self.storage.backend = StorageBackend::Sqlite;
            }
        }

        if self.storage.database_url.is_none()
            && let Ok(dsn) = std::env::var("SQL_DSN")
            && !dsn.trim().is_empty()
        {
            let dsn = dsn.trim().to_string();
            self.storage.backend = infer_main_storage_backend(&dsn)?;
            if self.storage.backend == StorageBackend::Sqlite && self.storage.sqlite_url.is_none() {
                self.storage.sqlite_url = Some(default_sqlite_url());
            }
            self.storage.database_url = Some(dsn);
        }

        if self.storage.log_database_url.is_none()
            && let Ok(dsn) = std::env::var("LOG_SQL_DSN")
            && !dsn.trim().is_empty()
        {
            let dsn = dsn.trim().to_string();
            self.storage.log_backend = Some(infer_log_storage_backend(&dsn));
            self.storage.log_database_url = Some(dsn);
        }

        Ok(())
    }

    fn resolve_channel_env_keys(&mut self) -> Result<()> {
        for channel in &mut self.channels {
            if channel.api_key.is_empty() {
                if let Some(env_name) = &channel.api_key_env {
                    channel.api_key = std::env::var(env_name).with_context(|| {
                        format!("read env var {env_name} for channel {}", channel.id)
                    })?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> GatewaySnapshot {
        GatewaySnapshot {
            version:          self.version,
            tokens:           self.tokens.clone(),
            channels:         self.channels.clone(),
            model_mappings:   self.model_mappings.clone(),
            channel_affinity: ChannelAffinityConfig::default(),
            group_routing:    GroupRoutingConfig::default(),
        }
    }
}

pub(crate) fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9090))
}

fn default_tokiame_hub_base_url() -> String {
    "http://127.0.0.1:9091".into()
}

fn default_heartbeat_interval_seconds() -> u64 {
    15
}

fn default_artifact_prefix() -> String {
    "halolake-media".into()
}

fn default_max_input_bytes() -> u64 {
    2 * 1024 * 1024 * 1024
}

fn default_max_output_bytes() -> u64 {
    // Current transport uses one presigned PUT. S3 multipart upload is the
    // next transport extension for artifacts larger than 5 GiB.
    5 * 1024 * 1024 * 1024
}

fn default_workflow_version() -> String {
    "v1".into()
}

pub(crate) fn default_version() -> u64 {
    1
}

pub(crate) fn default_system_name() -> String {
    "Halolake".to_string()
}

pub(crate) fn default_web_default_dist() -> PathBuf {
    PathBuf::from("web/new-api/default/dist")
}

pub(crate) fn default_web_classic_dist() -> PathBuf {
    PathBuf::from("web/new-api/classic/dist")
}

pub(crate) fn default_web_theme() -> String {
    "default".to_string()
}

pub(crate) fn default_sqlite_url() -> String {
    sqlite_url_from_path("one-api.db?_busy_timeout=30000")
}

pub(crate) fn sqlite_url_from_path(path: &str) -> String {
    if path.starts_with("sqlite:") {
        path.to_string()
    } else {
        format!("sqlite://{path}")
    }
}

pub(crate) fn normalize_mysql_url(url: &str) -> String {
    let url = url.trim();
    if url.starts_with("mysql://") || url.starts_with("mysql:////") {
        return url.to_string();
    }
    if let Some(rest) = url.split_once("@tcp(") {
        let userinfo = rest.0;
        let after = rest.1;
        if let Some((hostport, dbpart)) = after.split_once(")/") {
            let db = dbpart.split('?').next().unwrap_or(dbpart);
            return format!("mysql://{userinfo}@{hostport}/{db}");
        }
    }
    if !url.contains("://") {
        return format!("mysql://{url}");
    }
    url.to_string()
}

pub(crate) fn infer_main_storage_backend(dsn: &str) -> Result<StorageBackend> {
    let dsn = dsn.trim();
    if dsn.is_empty() || dsn.starts_with("local") {
        return Ok(StorageBackend::Sqlite);
    }
    if is_clickhouse_dsn(dsn) {
        bail!(
            "SQL_DSN does not support ClickHouse; use SQLite, MySQL, or PostgreSQL for the \
             primary database and LOG_SQL_DSN for ClickHouse logs"
        );
    }
    if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
        Ok(StorageBackend::Postgres)
    } else {
        Ok(StorageBackend::MySql)
    }
}

pub(crate) fn infer_log_storage_backend(dsn: &str) -> LogStorageBackend {
    let dsn = dsn.trim();
    if dsn.is_empty() || dsn.starts_with("local") {
        LogStorageBackend::Sqlite
    } else if is_clickhouse_dsn(dsn) {
        LogStorageBackend::ClickHouse
    } else if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
        LogStorageBackend::Postgres
    } else {
        LogStorageBackend::MySql
    }
}

pub(crate) fn is_clickhouse_dsn(dsn: &str) -> bool {
    dsn.starts_with("clickhouse://")
        || dsn.starts_with("tcp://")
        || dsn.starts_with("http://")
        || dsn.starts_with("https://")
}

pub(crate) fn ensure_supported_storage_backend(storage: &StorageConfig) -> Result<()> {
    match storage.backend {
        StorageBackend::Memory
        | StorageBackend::Sqlite
        | StorageBackend::MySql
        | StorageBackend::Postgres => {}
    }
    if let Some(log_backend) = storage.log_backend
        && log_backend != LogStorageBackend::Memory
        && log_backend != LogStorageBackend::Sqlite
    {
        bail!(
            "storage.log_backend = {} is recognized for new-api compatibility but separate log \
             storage is not implemented yet in halolake-control-api",
            log_storage_backend_name(log_backend)
        );
    }
    Ok(())
}

pub(crate) fn default_channel_test_interval_seconds() -> u64 {
    600
}

pub(crate) fn default_model_update_interval_seconds() -> u64 {
    1_800
}

pub(crate) fn storage_backend_name(backend: StorageBackend) -> &'static str {
    match backend {
        StorageBackend::Memory => "memory",
        StorageBackend::Sqlite => "sqlite",
        StorageBackend::MySql => "mysql",
        StorageBackend::Postgres => "postgres",
    }
}

pub(crate) fn log_storage_backend_name(backend: LogStorageBackend) -> &'static str {
    match backend {
        LogStorageBackend::Memory => "memory",
        LogStorageBackend::Sqlite => "sqlite",
        LogStorageBackend::MySql => "mysql",
        LogStorageBackend::Postgres => "postgres",
        LogStorageBackend::ClickHouse => "clickhouse",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokiame_example_config_stays_parseable() {
        let config: ControlApiConfig = toml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/tokiame-control-api.toml"
        )))
        .unwrap();

        assert!(config.media.enabled);
        config.media.validate().unwrap();
        assert_eq!(config.media.workflows.len(), 1);
        assert!(config.object_storage.s3.is_some());
    }
}
