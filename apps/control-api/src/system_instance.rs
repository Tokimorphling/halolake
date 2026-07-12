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
use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{Arc, RwLock},
    time::Duration,
};
use tracing::warn;

const STALE_AFTER_SECONDS: i64 = 90;
const REPORT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SystemInstanceRecord {
    node_name:    String,
    info:         Option<JsonValue>,
    started_at:   i64,
    last_seen_at: i64,
    created_at:   i64,
    updated_at:   i64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SystemInstanceResponse {
    node_name:           String,
    status:              &'static str,
    stale_after_seconds: i64,
    started_at:          i64,
    last_seen_at:        i64,
    info:                Option<JsonValue>,
}

#[derive(Debug, Clone)]
pub(crate) struct UpsertSystemInstanceRequest {
    pub(crate) node_name:    String,
    pub(crate) info:         JsonValue,
    pub(crate) started_at:   i64,
    pub(crate) last_seen_at: i64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ListSystemInstancesRequest {
    pub(crate) now: i64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeleteStaleSystemInstancesRequest {
    pub(crate) now: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct DeleteStaleSystemInstanceRequest {
    pub(crate) node_name: String,
    pub(crate) now:       i64,
}

#[derive(Debug, Clone)]
pub(crate) enum SystemInstanceStore {
    Memory(MemorySystemInstanceStore),
    Sqlite(SqliteSystemInstanceStore),
    MySql(MySqlSystemInstanceStore),
    Postgres(PostgresSystemInstanceStore),
}

impl SystemInstanceStore {
    pub(crate) fn memory() -> Self {
        Self::Memory(MemorySystemInstanceStore::default())
    }

    pub(crate) async fn sqlite(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Sqlite(SqliteSystemInstanceStore::connect(url).await?))
    }

    pub(crate) async fn mysql(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::MySql(MySqlSystemInstanceStore::connect(url).await?))
    }

    pub(crate) async fn postgres(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Postgres(
            PostgresSystemInstanceStore::connect(url).await?,
        ))
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MemorySystemInstanceStore {
    inner: Arc<RwLock<BTreeMap<String, SystemInstanceRecord>>>,
}

impl MemorySystemInstanceStore {
    fn from_records(records: Vec<SystemInstanceRecord>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(
                records
                    .into_iter()
                    .map(|record| (record.node_name.clone(), record))
                    .collect(),
            )),
        }
    }
}

crate::impl_backend_service!(SystemInstanceStore, {
    UpsertSystemInstanceRequest => (),
    ListSystemInstancesRequest => Vec<SystemInstanceResponse>,
    DeleteStaleSystemInstancesRequest => usize,
    DeleteStaleSystemInstanceRequest => bool,
});

impl Service<UpsertSystemInstanceRequest> for MemorySystemInstanceStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: UpsertSystemInstanceRequest) -> Result<Self::Response, Self::Error> {
        if req.node_name.trim().is_empty() {
            return Err(ManagementError::InvalidRequest(
                "system instance node name is empty",
            ));
        }
        let mut instances = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("system_instances"))?;
        let now = req.last_seen_at;
        let created_at = instances
            .get(&req.node_name)
            .map(|record| record.created_at)
            .unwrap_or(now);
        instances.insert(req.node_name.clone(), SystemInstanceRecord {
            node_name: req.node_name,
            info: Some(req.info),
            started_at: req.started_at,
            last_seen_at: req.last_seen_at,
            created_at,
            updated_at: now,
        });
        Ok(())
    }
}

impl Service<ListSystemInstancesRequest> for MemorySystemInstanceStore {
    type Response = Vec<SystemInstanceResponse>;
    type Error = ManagementError;

    async fn call(&self, req: ListSystemInstancesRequest) -> Result<Self::Response, Self::Error> {
        let instances = self
            .inner
            .read()
            .map_err(|_| ManagementError::Poisoned("system_instances"))?;
        let mut records = instances.values().cloned().collect::<Vec<_>>();
        records.sort_by(|left, right| right.last_seen_at.cmp(&left.last_seen_at));
        Ok(records
            .into_iter()
            .map(|record| record.to_response(req.now))
            .collect())
    }
}

impl Service<DeleteStaleSystemInstancesRequest> for MemorySystemInstanceStore {
    type Response = usize;
    type Error = ManagementError;

    async fn call(
        &self,
        req: DeleteStaleSystemInstancesRequest,
    ) -> Result<Self::Response, Self::Error> {
        let threshold = stale_threshold(req.now);
        let mut deleted = 0usize;
        self.inner
            .write()
            .map_err(|_| ManagementError::Poisoned("system_instances"))?
            .retain(|_, record| {
                let keep = record.last_seen_at >= threshold;
                if !keep {
                    deleted = deleted.saturating_add(1);
                }
                keep
            });
        Ok(deleted)
    }
}

impl Service<DeleteStaleSystemInstanceRequest> for MemorySystemInstanceStore {
    type Response = bool;
    type Error = ManagementError;

    async fn call(
        &self,
        req: DeleteStaleSystemInstanceRequest,
    ) -> Result<Self::Response, Self::Error> {
        let threshold = stale_threshold(req.now);
        let mut instances = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("system_instances"))?;
        let stale = instances
            .get(&req.node_name)
            .is_some_and(|record| record.last_seen_at < threshold);
        if stale {
            instances.remove(&req.node_name);
        }
        Ok(stale)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SqliteSystemInstanceStore {
    pool:   SqlitePool,
    memory: MemorySystemInstanceStore,
}

impl SqliteSystemInstanceStore {
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
        migrate_system_instances(&pool).await?;
        let records = load_system_instances(&pool).await?;
        Ok(Self {
            pool,
            memory: MemorySystemInstanceStore::from_records(records),
        })
    }
}

impl Service<UpsertSystemInstanceRequest> for SqliteSystemInstanceStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: UpsertSystemInstanceRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req.clone()).await?;
        sqlx::query(
            "INSERT INTO system_instances (
                node_name, info, started_at, last_seen_at, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(node_name) DO UPDATE SET
                info = excluded.info,
                started_at = excluded.started_at,
                last_seen_at = excluded.last_seen_at,
                updated_at = excluded.updated_at",
        )
        .bind(&req.node_name)
        .bind(serde_json::to_string(&req.info).map_err(storage_err)?)
        .bind(req.started_at)
        .bind(req.last_seen_at)
        .bind(req.last_seen_at)
        .bind(req.last_seen_at)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }
}

impl Service<ListSystemInstancesRequest> for SqliteSystemInstanceStore {
    type Response = Vec<SystemInstanceResponse>;
    type Error = ManagementError;

    async fn call(&self, req: ListSystemInstancesRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<DeleteStaleSystemInstancesRequest> for SqliteSystemInstanceStore {
    type Response = usize;
    type Error = ManagementError;

    async fn call(
        &self,
        req: DeleteStaleSystemInstancesRequest,
    ) -> Result<Self::Response, Self::Error> {
        let deleted = self.memory.call(req).await?;
        if deleted > 0 {
            sqlx::query("DELETE FROM system_instances WHERE last_seen_at < ?")
                .bind(stale_threshold(req.now))
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
        }
        Ok(deleted)
    }
}

impl Service<DeleteStaleSystemInstanceRequest> for SqliteSystemInstanceStore {
    type Response = bool;
    type Error = ManagementError;

    async fn call(
        &self,
        req: DeleteStaleSystemInstanceRequest,
    ) -> Result<Self::Response, Self::Error> {
        let deleted = self.memory.call(req.clone()).await?;
        if deleted {
            sqlx::query("DELETE FROM system_instances WHERE node_name = ? AND last_seen_at < ?")
                .bind(&req.node_name)
                .bind(stale_threshold(req.now))
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
        }
        Ok(deleted)
    }
}

#[derive(Debug, Clone)]

pub(crate) struct MySqlSystemInstanceStore {
    pool:   MySqlPool,
    memory: MemorySystemInstanceStore,
}

impl MySqlSystemInstanceStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = MySqlConnectOptions::from_str(url).map_err(storage_err)?;
        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_system_instances_mysql(&pool).await?;
        let records = load_system_instances_mysql(&pool).await?;
        Ok(Self {
            pool,
            memory: MemorySystemInstanceStore::from_records(records),
        })
    }
}

impl Service<UpsertSystemInstanceRequest> for MySqlSystemInstanceStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: UpsertSystemInstanceRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req.clone()).await?;
        sqlx::query(
            "INSERT INTO system_instances (
                node_name, info, started_at, last_seen_at, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE
                info = VALUES(info),
                started_at = VALUES(started_at),
                last_seen_at = VALUES(last_seen_at),
                updated_at = VALUES(updated_at)",
        )
        .bind(&req.node_name)
        .bind(serde_json::to_string(&req.info).map_err(storage_err)?)
        .bind(req.started_at)
        .bind(req.last_seen_at)
        .bind(req.last_seen_at)
        .bind(req.last_seen_at)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }
}

impl Service<ListSystemInstancesRequest> for MySqlSystemInstanceStore {
    type Response = Vec<SystemInstanceResponse>;
    type Error = ManagementError;

    async fn call(&self, req: ListSystemInstancesRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<DeleteStaleSystemInstancesRequest> for MySqlSystemInstanceStore {
    type Response = usize;
    type Error = ManagementError;

    async fn call(
        &self,
        req: DeleteStaleSystemInstancesRequest,
    ) -> Result<Self::Response, Self::Error> {
        let deleted = self.memory.call(req).await?;
        if deleted > 0 {
            sqlx::query("DELETE FROM system_instances WHERE last_seen_at < ?")
                .bind(stale_threshold(req.now))
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
        }
        Ok(deleted)
    }
}

impl Service<DeleteStaleSystemInstanceRequest> for MySqlSystemInstanceStore {
    type Response = bool;
    type Error = ManagementError;

    async fn call(
        &self,
        req: DeleteStaleSystemInstanceRequest,
    ) -> Result<Self::Response, Self::Error> {
        let deleted = self.memory.call(req.clone()).await?;
        if deleted {
            sqlx::query("DELETE FROM system_instances WHERE node_name = ? AND last_seen_at < ?")
                .bind(&req.node_name)
                .bind(stale_threshold(req.now))
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
        }
        Ok(deleted)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresSystemInstanceStore {
    pool:   PgPool,
    memory: MemorySystemInstanceStore,
}

impl PostgresSystemInstanceStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = PgConnectOptions::from_str(url).map_err(storage_err)?;
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_system_instances_pg(&pool).await?;
        let records = load_system_instances_pg(&pool).await?;
        Ok(Self {
            pool,
            memory: MemorySystemInstanceStore::from_records(records),
        })
    }
}

impl Service<UpsertSystemInstanceRequest> for PostgresSystemInstanceStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: UpsertSystemInstanceRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req.clone()).await?;
        sqlx::query(
            "INSERT INTO system_instances (
                node_name, info, started_at, last_seen_at, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT(node_name) DO UPDATE SET
                info = excluded.info,
                started_at = excluded.started_at,
                last_seen_at = excluded.last_seen_at,
                updated_at = excluded.updated_at",
        )
        .bind(&req.node_name)
        .bind(serde_json::to_string(&req.info).map_err(storage_err)?)
        .bind(req.started_at)
        .bind(req.last_seen_at)
        .bind(req.last_seen_at)
        .bind(req.last_seen_at)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }
}

impl Service<ListSystemInstancesRequest> for PostgresSystemInstanceStore {
    type Response = Vec<SystemInstanceResponse>;
    type Error = ManagementError;

    async fn call(&self, req: ListSystemInstancesRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<DeleteStaleSystemInstancesRequest> for PostgresSystemInstanceStore {
    type Response = usize;
    type Error = ManagementError;

    async fn call(
        &self,
        req: DeleteStaleSystemInstancesRequest,
    ) -> Result<Self::Response, Self::Error> {
        let deleted = self.memory.call(req).await?;
        if deleted > 0 {
            sqlx::query("DELETE FROM system_instances WHERE last_seen_at < $1")
                .bind(stale_threshold(req.now))
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
        }
        Ok(deleted)
    }
}

impl Service<DeleteStaleSystemInstanceRequest> for PostgresSystemInstanceStore {
    type Response = bool;
    type Error = ManagementError;

    async fn call(
        &self,
        req: DeleteStaleSystemInstanceRequest,
    ) -> Result<Self::Response, Self::Error> {
        let deleted = self.memory.call(req.clone()).await?;
        if deleted {
            sqlx::query("DELETE FROM system_instances WHERE node_name = $1 AND last_seen_at < $2")
                .bind(&req.node_name)
                .bind(stale_threshold(req.now))
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
        }
        Ok(deleted)
    }
}

impl SystemInstanceRecord {
    fn to_response(self, now: i64) -> SystemInstanceResponse {
        let status = if now.saturating_sub(self.last_seen_at) > STALE_AFTER_SECONDS {
            "stale"
        } else {
            "online"
        };
        SystemInstanceResponse {
            node_name: self.node_name,
            status,
            stale_after_seconds: STALE_AFTER_SECONDS,
            started_at: self.started_at,
            last_seen_at: self.last_seen_at,
            info: self.info,
        }
    }
}

pub(crate) fn spawn_system_instance_reporter(store: SystemInstanceStore, started_at: i64) {
    tokio::spawn(async move {
        report_current_instance(&store, started_at).await;
        let mut ticker = tokio::time::interval(REPORT_INTERVAL);
        loop {
            ticker.tick().await;
            report_current_instance(&store, started_at).await;
        }
    });
}

async fn report_current_instance(store: &SystemInstanceStore, started_at: i64) {
    let node = current_node_identity();
    let now = now_unix();
    // Two-process deployment: control-api reports as `<host>/control-api`.
    let node_name = format!("{}/control-api", node.name);
    let metrics = crate::process_metrics::ProcessMetrics::collect();
    let req = UpsertSystemInstanceRequest {
        node_name: node_name.clone(),
        info: json!({
            "schema_version": 1,
            "node": {
                "name": node_name,
                "source": node.source,
                "manually_configured": node.manually_configured,
                "should_configure_manually": node.should_configure_manually,
                "process": "control-api",
                "host_key": node.name,
            },
            "role": {
                "is_master": true,
                "process": "control-api",
            },
            "runtime": {
                "version": env!("CARGO_PKG_VERSION"),
                "goos": std::env::consts::OS,
                "goarch": std::env::consts::ARCH,
                "started_at": started_at,
            },
            "host": {
                "hostname": host_name_or(&node.name),
            },
            "resources": metrics.to_resources_json("control-api"),
        }),
        started_at,
        last_seen_at: now,
    };
    if let Err(err) = store.call(req).await {
        warn!(?err, "system instance report failed");
    }
}

fn host_name_or(fallback: &str) -> String {
    let hostname = host_name();
    if hostname.trim().is_empty() {
        fallback.to_string()
    } else {
        hostname
    }
}

struct NodeIdentity {
    name: String,
    source: &'static str,
    manually_configured: bool,
    should_configure_manually: bool,
}

fn current_node_identity() -> NodeIdentity {
    if let Ok(name) = std::env::var("HALOLAKE_NODE_NAME")
        && !name.trim().is_empty()
    {
        return NodeIdentity {
            name,
            source: "halolake_node_name",
            manually_configured: true,
            should_configure_manually: false,
        };
    }
    if let Ok(name) = std::env::var("NODE_NAME")
        && !name.trim().is_empty()
    {
        return NodeIdentity {
            name,
            source: "node_name",
            manually_configured: true,
            should_configure_manually: false,
        };
    }

    let hostname = host_name();
    if !hostname.trim().is_empty() {
        return NodeIdentity {
            name: hostname,
            source: "hostname",
            manually_configured: false,
            should_configure_manually: true,
        };
    }

    NodeIdentity {
        name: "halolake-control".to_string(),
        source: "default",
        manually_configured: false,
        should_configure_manually: true,
    }
}

fn host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_default()
}

fn stale_threshold(now: i64) -> i64 {
    now.saturating_sub(STALE_AFTER_SECONDS)
}

async fn migrate_system_instances(pool: &SqlitePool) -> Result<(), ManagementError> {
    for stmt in [
        "CREATE TABLE IF NOT EXISTS system_instances (
            node_name TEXT PRIMARY KEY,
            info TEXT NOT NULL DEFAULT '',
            started_at INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )",
        "CREATE INDEX IF NOT EXISTS idx_system_instances_last_seen_at ON \
         system_instances(last_seen_at)",
    ] {
        sqlx::query(stmt).execute(pool).await.map_err(storage_err)?;
    }
    Ok(())
}

async fn migrate_system_instances_mysql(pool: &MySqlPool) -> Result<(), ManagementError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS system_instances (
            node_name VARCHAR(255) PRIMARY KEY,
            info TEXT NOT NULL,
            started_at BIGINT NOT NULL,
            last_seen_at BIGINT NOT NULL,
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(storage_err)?;
    let _ = sqlx::query(
        "CREATE INDEX idx_system_instances_last_seen_at ON system_instances(last_seen_at)",
    )
    .execute(pool)
    .await;
    Ok(())
}

async fn load_system_instances(
    pool: &SqlitePool,
) -> Result<Vec<SystemInstanceRecord>, ManagementError> {
    sqlx::query(
        "SELECT node_name, info, started_at, last_seen_at, created_at, updated_at
         FROM system_instances ORDER BY last_seen_at DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(SystemInstanceRecord {
            node_name:    string_col_sqlite(&row, "node_name")?,
            info:         json_col_sqlite(&row, "info")?,
            started_at:   i64_col_sqlite(&row, "started_at")?,
            last_seen_at: i64_col_sqlite(&row, "last_seen_at")?,
            created_at:   i64_col_sqlite(&row, "created_at")?,
            updated_at:   i64_col_sqlite(&row, "updated_at")?,
        })
    })
    .collect()
}

async fn load_system_instances_mysql(
    pool: &MySqlPool,
) -> Result<Vec<SystemInstanceRecord>, ManagementError> {
    sqlx::query(
        "SELECT node_name, info, started_at, last_seen_at, created_at, updated_at
         FROM system_instances ORDER BY last_seen_at DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(SystemInstanceRecord {
            node_name:    string_col_mysql(&row, "node_name")?,
            info:         json_col_mysql(&row, "info")?,
            started_at:   i64_col_mysql(&row, "started_at")?,
            last_seen_at: i64_col_mysql(&row, "last_seen_at")?,
            created_at:   i64_col_mysql(&row, "created_at")?,
            updated_at:   i64_col_mysql(&row, "updated_at")?,
        })
    })
    .collect()
}

async fn migrate_system_instances_pg(pool: &PgPool) -> Result<(), ManagementError> {
    for stmt in [
        "CREATE TABLE IF NOT EXISTS system_instances (
            node_name TEXT PRIMARY KEY,
            info TEXT NOT NULL DEFAULT '',
            started_at BIGINT NOT NULL,
            last_seen_at BIGINT NOT NULL,
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL
        )",
        "CREATE INDEX IF NOT EXISTS idx_system_instances_last_seen_at ON \
         system_instances(last_seen_at)",
    ] {
        sqlx::query(stmt).execute(pool).await.map_err(storage_err)?;
    }
    Ok(())
}

async fn load_system_instances_pg(
    pool: &PgPool,
) -> Result<Vec<SystemInstanceRecord>, ManagementError> {
    sqlx::query(
        "SELECT node_name, info, started_at, last_seen_at, created_at, updated_at
         FROM system_instances ORDER BY last_seen_at DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(SystemInstanceRecord {
            node_name:    string_col_pg(&row, "node_name")?,
            info:         json_col_pg(&row, "info")?,
            started_at:   i64_col_pg(&row, "started_at")?,
            last_seen_at: i64_col_pg(&row, "last_seen_at")?,
            created_at:   i64_col_pg(&row, "created_at")?,
            updated_at:   i64_col_pg(&row, "updated_at")?,
        })
    })
    .collect()
}

fn json_col_sqlite(
    row: &sqlx::sqlite::SqliteRow,
    name: &str,
) -> Result<Option<JsonValue>, ManagementError> {
    let raw = string_col_sqlite(row, name)?;
    if raw.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&raw).map(Some).map_err(|err| {
        ManagementError::Storage(format!("invalid system instance JSON {name}: {err}"))
    })
}

fn string_col_sqlite(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<String, ManagementError> {
    row.try_get::<String, _>(name).map_err(storage_err)
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
    serde_json::from_str(&raw).map(Some).map_err(|err| {
        ManagementError::Storage(format!("invalid system instance JSON {name}: {err}"))
    })
}

fn string_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<String, ManagementError> {
    row.try_get::<String, _>(name).map_err(storage_err)
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
    serde_json::from_str(&raw).map(Some).map_err(|err| {
        ManagementError::Storage(format!("invalid system instance JSON {name}: {err}"))
    })
}

fn string_col_pg(row: &sqlx::postgres::PgRow, name: &str) -> Result<String, ManagementError> {
    row.try_get::<String, _>(name).map_err(storage_err)
}

fn i64_col_pg(row: &sqlx::postgres::PgRow, name: &str) -> Result<i64, ManagementError> {
    row.try_get::<i64, _>(name).map_err(storage_err)
}

fn storage_err(err: impl std::fmt::Display) -> ManagementError {
    ManagementError::Storage(err.to_string())
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

    #[tokio::test]
    async fn memory_system_instances_list_and_delete_only_stale_records() {
        let store = SystemInstanceStore::memory();
        store
            .call(UpsertSystemInstanceRequest {
                node_name:    "online".to_string(),
                info:         json!({"node": {"name": "online"}}),
                started_at:   100,
                last_seen_at: 1_000,
            })
            .await
            .expect("online upsert");
        store
            .call(UpsertSystemInstanceRequest {
                node_name:    "stale".to_string(),
                info:         json!({"node": {"name": "stale"}}),
                started_at:   100,
                last_seen_at: 800,
            })
            .await
            .expect("stale upsert");

        let listed = store
            .call(ListSystemInstancesRequest { now: 1_000 })
            .await
            .expect("list");
        assert_eq!(listed[0].node_name, "online");
        assert_eq!(listed[1].status, "stale");

        let deleted = store
            .call(DeleteStaleSystemInstancesRequest { now: 1_000 })
            .await
            .expect("delete stale");
        assert_eq!(deleted, 1);
        let listed = store
            .call(ListSystemInstancesRequest { now: 1_000 })
            .await
            .expect("list after delete");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].node_name, "online");
    }
}
