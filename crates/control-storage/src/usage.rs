//! Usage event store backends.
use super::management::string_col;
use halolake_control_plane::{
    MemoryUsageEventSink, UsageAck, UsageError, UsageEventBatch, UsageEventQuota,
};
use halolake_domain::{UsageEvent, UsageStatus};
use service_async::Service;
use sqlx::{
    MySqlPool, PgPool, Row, SqlitePool,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
    postgres::{PgConnectOptions, PgPoolOptions},
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::str::FromStr;

#[derive(Debug, Clone)]
pub enum UsageStore {
    Memory(MemoryUsageEventSink),
    Sqlite(SqliteUsageStore),
    MySql(MySqlUsageStore),
    Postgres(PostgresUsageStore),
}

#[derive(Debug, Clone, Copy)]
pub struct DeleteUsageBeforeRequest {
    pub target_timestamp: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct DeleteUsageAck {
    pub deleted: usize,
}

#[derive(Debug, Clone)]
pub struct RecordedUsageBatch {
    pub ack:             UsageAck,
    pub accepted_events: Vec<UsageEvent>,
}

impl UsageStore {
    pub fn memory() -> Self {
        Self::Memory(MemoryUsageEventSink::default())
    }

    pub async fn sqlite(url: &str) -> Result<Self, UsageError> {
        Ok(Self::Sqlite(SqliteUsageStore::connect(url).await?))
    }

    pub async fn mysql(url: &str) -> Result<Self, UsageError> {
        Ok(Self::MySql(MySqlUsageStore::connect(url).await?))
    }

    pub async fn postgres(url: &str) -> Result<Self, UsageError> {
        Ok(Self::Postgres(PostgresUsageStore::connect(url).await?))
    }

    pub fn events(&self) -> Result<Vec<UsageEvent>, UsageError> {
        match self {
            Self::Memory(store) => store.events(),
            Self::Sqlite(store) => store.events(),
            Self::MySql(store) => store.events(),
            Self::Postgres(store) => store.events(),
        }
    }

    pub fn count_before_unix_seconds(
        &self,
        target_timestamp: i64,
    ) -> Result<usize, UsageError> {
        let target_ms = target_timestamp.saturating_mul(1000);
        Ok(self
            .events()?
            .into_iter()
            .filter(|event| event.created_at_unix_ms < target_ms)
            .count())
    }

    pub async fn record_batch(
        &self,
        req: UsageEventBatch,
    ) -> Result<RecordedUsageBatch, UsageError> {
        match self {
            Self::Memory(store) => {
                let existing = store
                    .events()?
                    .into_iter()
                    .map(|event| event.request_id)
                    .collect::<std::collections::BTreeSet<_>>();
                let accepted_events = req
                    .events
                    .into_iter()
                    .filter(|event| !existing.contains(&event.request_id))
                    .collect::<Vec<_>>();
                let ack = UsageAck {
                    accepted: accepted_events.len(),
                };
                if !accepted_events.is_empty() {
                    store
                        .call(UsageEventBatch::new(accepted_events.clone()))
                        .await?;
                }
                Ok(RecordedUsageBatch {
                    ack,
                    accepted_events,
                })
            }
            Self::Sqlite(store) => store.record_batch(req).await,
            Self::MySql(store) => store.record_batch(req).await,
            Self::Postgres(store) => store.record_batch(req).await,
        }
    }

    pub async fn apply_quotas(&self, quotas: &[UsageEventQuota]) -> Result<(), UsageError> {
        match self {
            Self::Memory(store) => store.apply_quotas(quotas),
            Self::Sqlite(store) => store.apply_quotas(quotas).await,
            Self::MySql(store) => store.apply_quotas(quotas).await,
            Self::Postgres(store) => store.apply_quotas(quotas).await,
        }
    }
}

impl Service<DeleteUsageBeforeRequest> for UsageStore {
    type Response = DeleteUsageAck;
    type Error = UsageError;

    async fn call(&self, req: DeleteUsageBeforeRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => {
                let deleted = store.delete_before_unix_seconds(req.target_timestamp)?;
                Ok(DeleteUsageAck { deleted })
            }
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl Service<UsageEventBatch> for UsageStore {
    type Response = UsageAck;
    type Error = UsageError;

    async fn call(&self, req: UsageEventBatch) -> Result<Self::Response, Self::Error> {
        self.record_batch(req).await.map(|recorded| recorded.ack)
    }
}

#[derive(Debug, Clone)]
pub struct SqliteUsageStore {
    pool:   SqlitePool,
    memory: MemoryUsageEventSink,
}

impl SqliteUsageStore {
    async fn connect(url: &str) -> Result<Self, UsageError> {
        let options = SqliteConnectOptions::from_str(url)
            .map_err(usage_storage_err)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(usage_storage_err)?;
        migrate_usage(&pool).await?;
        let memory = MemoryUsageEventSink::default();
        let events = load_usage_events(&pool).await?;
        memory.call(UsageEventBatch::new(events)).await?;
        Ok(Self { pool, memory })
    }

    fn events(&self) -> Result<Vec<UsageEvent>, UsageError> {
        self.memory.events()
    }

    async fn record_batch(&self, req: UsageEventBatch) -> Result<RecordedUsageBatch, UsageError> {
        let mut accepted_events = Vec::new();
        for event in req.events {
            let result = sqlx::query(
                "INSERT OR IGNORE INTO usage_events (
                    request_id, user_id, token_id, channel_id, event_group, model, upstream_model,
                    prompt_tokens, completion_tokens, total_tokens, cache_read_tokens,
                    cache_creation_tokens, image_tokens, audio_tokens, quota, status, latency_ms,
                    is_stream, ip, upstream_request_id, created_at_unix_ms
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&event.request_id)
            .bind(&event.user_id)
            .bind(&event.token_id)
            .bind(&event.channel_id)
            .bind(&event.group)
            .bind(&event.model)
            .bind(&event.upstream_model)
            .bind(event.prompt_tokens.map(|value| value as i64))
            .bind(event.completion_tokens.map(|value| value as i64))
            .bind(event.total_tokens.map(|value| value as i64))
            .bind(event.cache_read_tokens.map(|value| value as i64))
            .bind(event.cache_creation_tokens.map(|value| value as i64))
            .bind(event.image_tokens.map(|value| value as i64))
            .bind(event.audio_tokens.map(|value| value as i64))
            .bind(event.quota)
            .bind(usage_status_str(event.status))
            .bind(event.latency_ms as i64)
            .bind(event.is_stream)
            .bind(&event.ip)
            .bind(&event.upstream_request_id)
            .bind(event.created_at_unix_ms)
            .execute(&self.pool)
            .await
            .map_err(usage_storage_err)?;
            if result.rows_affected() > 0 {
                accepted_events.push(event);
            }
        }

        let ack = UsageAck {
            accepted: accepted_events.len(),
        };
        if !accepted_events.is_empty() {
            self.memory
                .call(UsageEventBatch::new(accepted_events.clone()))
                .await?;
        }
        Ok(RecordedUsageBatch {
            ack,
            accepted_events,
        })
    }

    async fn apply_quotas(&self, quotas: &[UsageEventQuota]) -> Result<(), UsageError> {
        if quotas.is_empty() {
            return Ok(());
        }
        for quota in quotas {
            sqlx::query("UPDATE usage_events SET quota = ? WHERE request_id = ?")
                .bind(quota.quota)
                .bind(&quota.request_id)
                .execute(&self.pool)
                .await
                .map_err(usage_storage_err)?;
        }
        self.memory.apply_quotas(quotas)
    }
}

impl Service<UsageEventBatch> for SqliteUsageStore {
    type Response = UsageAck;
    type Error = UsageError;

    async fn call(&self, req: UsageEventBatch) -> Result<Self::Response, Self::Error> {
        self.record_batch(req).await.map(|recorded| recorded.ack)
    }
}

impl Service<DeleteUsageBeforeRequest> for SqliteUsageStore {
    type Response = DeleteUsageAck;
    type Error = UsageError;

    async fn call(&self, req: DeleteUsageBeforeRequest) -> Result<Self::Response, Self::Error> {
        let target_ms = req.target_timestamp.saturating_mul(1000);
        let result = sqlx::query("DELETE FROM usage_events WHERE created_at_unix_ms < ?")
            .bind(target_ms)
            .execute(&self.pool)
            .await
            .map_err(usage_storage_err)?;
        let deleted = result.rows_affected() as usize;
        if deleted > 0 {
            self.memory
                .delete_before_unix_seconds(req.target_timestamp)?;
        }
        Ok(DeleteUsageAck { deleted })
    }
}

async fn migrate_usage(pool: &SqlitePool) -> Result<(), UsageError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS usage_events (
            request_id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            token_id TEXT NOT NULL,
            channel_id TEXT NOT NULL,
            event_group TEXT NOT NULL DEFAULT '',
            model TEXT NOT NULL,
            upstream_model TEXT NOT NULL,
            prompt_tokens INTEGER,
            completion_tokens INTEGER,
            total_tokens INTEGER,
            cache_read_tokens INTEGER,
            cache_creation_tokens INTEGER,
            image_tokens INTEGER,
            audio_tokens INTEGER,
            quota INTEGER,
            status TEXT NOT NULL,
            latency_ms INTEGER NOT NULL,
            is_stream BOOLEAN NOT NULL DEFAULT 0,
            ip TEXT NOT NULL DEFAULT '',
            upstream_request_id TEXT NOT NULL DEFAULT '',
            created_at_unix_ms INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(usage_storage_err)?;
    ensure_usage_column(pool, "quota", "quota INTEGER").await?;
    ensure_usage_column(pool, "event_group", "event_group TEXT NOT NULL DEFAULT ''").await?;
    ensure_usage_column(pool, "cache_read_tokens", "cache_read_tokens INTEGER").await?;
    ensure_usage_column(
        pool,
        "cache_creation_tokens",
        "cache_creation_tokens INTEGER",
    )
    .await?;
    ensure_usage_column(pool, "image_tokens", "image_tokens INTEGER").await?;
    ensure_usage_column(pool, "audio_tokens", "audio_tokens INTEGER").await?;
    ensure_usage_column(pool, "is_stream", "is_stream BOOLEAN NOT NULL DEFAULT 0").await?;
    ensure_usage_column(pool, "ip", "ip TEXT NOT NULL DEFAULT ''").await?;
    ensure_usage_column(
        pool,
        "upstream_request_id",
        "upstream_request_id TEXT NOT NULL DEFAULT ''",
    )
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_usage_events_created_at ON \
         usage_events(created_at_unix_ms)",
    )
    .execute(pool)
    .await
    .map_err(usage_storage_err)?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_usage_events_user_id ON usage_events(user_id)")
        .execute(pool)
        .await
        .map_err(usage_storage_err)?;
    Ok(())
}

async fn migrate_usage_mysql(pool: &MySqlPool) -> Result<(), UsageError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS usage_events (
            request_id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            token_id TEXT NOT NULL,
            channel_id TEXT NOT NULL,
            event_group TEXT NOT NULL DEFAULT '',
            model TEXT NOT NULL,
            upstream_model TEXT NOT NULL,
            prompt_tokens BIGINT,
            completion_tokens BIGINT,
            total_tokens BIGINT,
            cache_read_tokens BIGINT,
            cache_creation_tokens BIGINT,
            image_tokens BIGINT,
            audio_tokens BIGINT,
            quota BIGINT,
            status TEXT NOT NULL,
            latency_ms BIGINT NOT NULL,
            is_stream BOOLEAN NOT NULL DEFAULT FALSE,
            ip TEXT NOT NULL DEFAULT '',
            upstream_request_id TEXT NOT NULL DEFAULT '',
            created_at_unix_ms BIGINT NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(usage_storage_err)?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_usage_events_created_at ON \
         usage_events(created_at_unix_ms)",
    )
    .execute(pool)
    .await
    .map_err(usage_storage_err)?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_usage_events_user_id ON usage_events(user_id)")
        .execute(pool)
        .await
        .map_err(usage_storage_err)?;
    Ok(())
}

async fn ensure_usage_column(pool: &SqlitePool, column: &str, ddl: &str) -> Result<(), UsageError> {
    let exists = sqlx::query("PRAGMA table_info(usage_events)")
        .fetch_all(pool)
        .await
        .map_err(usage_storage_err)?
        .into_iter()
        .any(|row| string_col(&row, "name").is_ok_and(|name| name == column));
    if !exists {
        sqlx::query(&format!("ALTER TABLE usage_events ADD COLUMN {ddl}"))
            .execute(pool)
            .await
            .map_err(usage_storage_err)?;
    }
    Ok(())
}

async fn load_usage_events(pool: &SqlitePool) -> Result<Vec<UsageEvent>, UsageError> {
    sqlx::query(
        "SELECT request_id, user_id, token_id, channel_id, event_group, model, upstream_model, \
         prompt_tokens,
            completion_tokens, total_tokens, cache_read_tokens, cache_creation_tokens,
            image_tokens, audio_tokens, quota, status, latency_ms, is_stream, ip,
            upstream_request_id, created_at_unix_ms
         FROM usage_events ORDER BY created_at_unix_ms DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(usage_storage_err)?
    .into_iter()
    .map(|row| {
        Ok(UsageEvent {
            request_id:            usage_string_col(&row, "request_id")?,
            user_id:               usage_string_col(&row, "user_id")?,
            token_id:              usage_string_col(&row, "token_id")?,
            channel_id:            usage_string_col(&row, "channel_id")?,
            group:                 usage_string_col(&row, "event_group")?,
            model:                 usage_string_col(&row, "model")?,
            upstream_model:        usage_string_col(&row, "upstream_model")?,
            prompt_tokens:         usage_opt_u64_col(&row, "prompt_tokens")?,
            completion_tokens:     usage_opt_u64_col(&row, "completion_tokens")?,
            total_tokens:          usage_opt_u64_col(&row, "total_tokens")?,
            cache_read_tokens:     usage_opt_u64_col(&row, "cache_read_tokens")?,
            cache_creation_tokens: usage_opt_u64_col(&row, "cache_creation_tokens")?,
            image_tokens:          usage_opt_u64_col(&row, "image_tokens")?,
            audio_tokens:          usage_opt_u64_col(&row, "audio_tokens")?,
            quota:                 usage_opt_i64_col(&row, "quota")?,
            status:                usage_status_from_str(&usage_string_col(&row, "status")?),
            latency_ms:            usage_u64_col(&row, "latency_ms")?,
            // Live gateway events carry FRT in memory; not yet a DB column.
            first_response_ms:     None,
            is_stream:             usage_bool_col(&row, "is_stream")?,
            ip:                    usage_string_col(&row, "ip")?,
            upstream_request_id:   usage_string_col(&row, "upstream_request_id")?,
            created_at_unix_ms:    usage_i64_col(&row, "created_at_unix_ms")?,
        })
    })
    .collect()
}

async fn load_usage_events_mysql(pool: &MySqlPool) -> Result<Vec<UsageEvent>, UsageError> {
    sqlx::query(
        "SELECT request_id, user_id, token_id, channel_id, event_group, model, upstream_model, \
         prompt_tokens,
            completion_tokens, total_tokens, cache_read_tokens, cache_creation_tokens,
            image_tokens, audio_tokens, quota, status, latency_ms, is_stream, ip,
            upstream_request_id, created_at_unix_ms
         FROM usage_events ORDER BY created_at_unix_ms DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(usage_storage_err)?
    .into_iter()
    .map(|row| {
        Ok(UsageEvent {
            request_id:            usage_string_col_mysql(&row, "request_id")?,
            user_id:               usage_string_col_mysql(&row, "user_id")?,
            token_id:              usage_string_col_mysql(&row, "token_id")?,
            channel_id:            usage_string_col_mysql(&row, "channel_id")?,
            group:                 usage_string_col_mysql(&row, "event_group")?,
            model:                 usage_string_col_mysql(&row, "model")?,
            upstream_model:        usage_string_col_mysql(&row, "upstream_model")?,
            prompt_tokens:         usage_opt_u64_col_mysql(&row, "prompt_tokens")?,
            completion_tokens:     usage_opt_u64_col_mysql(&row, "completion_tokens")?,
            total_tokens:          usage_opt_u64_col_mysql(&row, "total_tokens")?,
            cache_read_tokens:     usage_opt_u64_col_mysql(&row, "cache_read_tokens")?,
            cache_creation_tokens: usage_opt_u64_col_mysql(&row, "cache_creation_tokens")?,
            image_tokens:          usage_opt_u64_col_mysql(&row, "image_tokens")?,
            audio_tokens:          usage_opt_u64_col_mysql(&row, "audio_tokens")?,
            quota:                 usage_opt_i64_col_mysql(&row, "quota")?,
            status:                usage_status_from_str(&usage_string_col_mysql(&row, "status")?),
            latency_ms:            usage_u64_col_mysql(&row, "latency_ms")?,
            first_response_ms:     None,
            is_stream:             usage_bool_col_mysql(&row, "is_stream")?,
            ip:                    usage_string_col_mysql(&row, "ip")?,
            upstream_request_id:   usage_string_col_mysql(&row, "upstream_request_id")?,
            created_at_unix_ms:    usage_i64_col_mysql(&row, "created_at_unix_ms")?,
        })
    })
    .collect()
}

#[derive(Debug, Clone)]
pub struct MySqlUsageStore {
    pool:   MySqlPool,
    memory: MemoryUsageEventSink,
}

impl MySqlUsageStore {
    async fn connect(url: &str) -> Result<Self, UsageError> {
        let options = MySqlConnectOptions::from_str(url).map_err(usage_storage_err)?;
        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(usage_storage_err)?;
        migrate_usage_mysql(&pool).await?;
        let memory = MemoryUsageEventSink::default();
        let events = load_usage_events_mysql(&pool).await?;
        memory.call(UsageEventBatch::new(events)).await?;
        Ok(Self { pool, memory })
    }

    fn events(&self) -> Result<Vec<UsageEvent>, UsageError> {
        self.memory.events()
    }

    async fn record_batch(&self, req: UsageEventBatch) -> Result<RecordedUsageBatch, UsageError> {
        let mut accepted_events = Vec::new();
        for event in req.events {
            let result = sqlx::query(
                "INSERT IGNORE INTO usage_events (
                    request_id, user_id, token_id, channel_id, event_group, model, upstream_model,
                    prompt_tokens, completion_tokens, total_tokens, cache_read_tokens,
                    cache_creation_tokens, image_tokens, audio_tokens, quota, status, latency_ms,
                    is_stream, ip, upstream_request_id, created_at_unix_ms
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&event.request_id)
            .bind(&event.user_id)
            .bind(&event.token_id)
            .bind(&event.channel_id)
            .bind(&event.group)
            .bind(&event.model)
            .bind(&event.upstream_model)
            .bind(event.prompt_tokens.map(|value| value as i64))
            .bind(event.completion_tokens.map(|value| value as i64))
            .bind(event.total_tokens.map(|value| value as i64))
            .bind(event.cache_read_tokens.map(|value| value as i64))
            .bind(event.cache_creation_tokens.map(|value| value as i64))
            .bind(event.image_tokens.map(|value| value as i64))
            .bind(event.audio_tokens.map(|value| value as i64))
            .bind(event.quota)
            .bind(usage_status_str(event.status))
            .bind(event.latency_ms as i64)
            .bind(event.is_stream)
            .bind(&event.ip)
            .bind(&event.upstream_request_id)
            .bind(event.created_at_unix_ms)
            .execute(&self.pool)
            .await
            .map_err(usage_storage_err)?;
            if result.rows_affected() > 0 {
                accepted_events.push(event);
            }
        }

        let ack = UsageAck {
            accepted: accepted_events.len(),
        };
        if !accepted_events.is_empty() {
            self.memory
                .call(UsageEventBatch::new(accepted_events.clone()))
                .await?;
        }
        Ok(RecordedUsageBatch {
            ack,
            accepted_events,
        })
    }

    async fn apply_quotas(&self, quotas: &[UsageEventQuota]) -> Result<(), UsageError> {
        if quotas.is_empty() {
            return Ok(());
        }
        for quota in quotas {
            sqlx::query("UPDATE usage_events SET quota = ? WHERE request_id = ?")
                .bind(quota.quota)
                .bind(&quota.request_id)
                .execute(&self.pool)
                .await
                .map_err(usage_storage_err)?;
        }
        self.memory.apply_quotas(quotas)
    }
}

impl Service<UsageEventBatch> for MySqlUsageStore {
    type Response = UsageAck;
    type Error = UsageError;

    async fn call(&self, req: UsageEventBatch) -> Result<Self::Response, Self::Error> {
        self.record_batch(req).await.map(|recorded| recorded.ack)
    }
}

impl Service<DeleteUsageBeforeRequest> for MySqlUsageStore {
    type Response = DeleteUsageAck;
    type Error = UsageError;

    async fn call(&self, req: DeleteUsageBeforeRequest) -> Result<Self::Response, Self::Error> {
        let target_ms = req.target_timestamp.saturating_mul(1000);
        let result = sqlx::query("DELETE FROM usage_events WHERE created_at_unix_ms < ?")
            .bind(target_ms)
            .execute(&self.pool)
            .await
            .map_err(usage_storage_err)?;
        let deleted = result.rows_affected() as usize;
        if deleted > 0 {
            self.memory
                .delete_before_unix_seconds(req.target_timestamp)?;
        }
        Ok(DeleteUsageAck { deleted })
    }
}

#[derive(Debug, Clone)]
pub struct PostgresUsageStore {
    pool:   PgPool,
    memory: MemoryUsageEventSink,
}

impl PostgresUsageStore {
    async fn connect(url: &str) -> Result<Self, UsageError> {
        let options = PgConnectOptions::from_str(url).map_err(usage_storage_err)?;
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect_with(options)
            .await
            .map_err(usage_storage_err)?;
        migrate_usage_pg(&pool).await?;
        let memory = MemoryUsageEventSink::default();
        let events = load_usage_events_pg(&pool).await?;
        memory.call(UsageEventBatch::new(events)).await?;
        Ok(Self { pool, memory })
    }

    fn events(&self) -> Result<Vec<UsageEvent>, UsageError> {
        self.memory.events()
    }

    async fn record_batch(&self, req: UsageEventBatch) -> Result<RecordedUsageBatch, UsageError> {
        let mut accepted_events = Vec::new();
        for event in req.events {
            let result = sqlx::query(
                "INSERT INTO usage_events (
                    request_id, user_id, token_id, channel_id, event_group, model, upstream_model,
                    prompt_tokens, completion_tokens, total_tokens, cache_read_tokens,
                    cache_creation_tokens, image_tokens, audio_tokens, quota, status, latency_ms,
                    is_stream, ip, upstream_request_id, created_at_unix_ms
                 ) VALUES \
                 ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21)
                 ON CONFLICT (request_id) DO NOTHING",
            )
            .bind(&event.request_id)
            .bind(&event.user_id)
            .bind(&event.token_id)
            .bind(&event.channel_id)
            .bind(&event.group)
            .bind(&event.model)
            .bind(&event.upstream_model)
            .bind(event.prompt_tokens.map(|value| value as i64))
            .bind(event.completion_tokens.map(|value| value as i64))
            .bind(event.total_tokens.map(|value| value as i64))
            .bind(event.cache_read_tokens.map(|value| value as i64))
            .bind(event.cache_creation_tokens.map(|value| value as i64))
            .bind(event.image_tokens.map(|value| value as i64))
            .bind(event.audio_tokens.map(|value| value as i64))
            .bind(event.quota)
            .bind(usage_status_str(event.status))
            .bind(event.latency_ms as i64)
            .bind(event.is_stream)
            .bind(&event.ip)
            .bind(&event.upstream_request_id)
            .bind(event.created_at_unix_ms)
            .execute(&self.pool)
            .await
            .map_err(usage_storage_err)?;
            if result.rows_affected() > 0 {
                accepted_events.push(event);
            }
        }

        let ack = UsageAck {
            accepted: accepted_events.len(),
        };
        if !accepted_events.is_empty() {
            self.memory
                .call(UsageEventBatch::new(accepted_events.clone()))
                .await?;
        }
        Ok(RecordedUsageBatch {
            ack,
            accepted_events,
        })
    }

    async fn apply_quotas(&self, quotas: &[UsageEventQuota]) -> Result<(), UsageError> {
        if quotas.is_empty() {
            return Ok(());
        }
        for quota in quotas {
            sqlx::query("UPDATE usage_events SET quota = $1 WHERE request_id = $2")
                .bind(quota.quota)
                .bind(&quota.request_id)
                .execute(&self.pool)
                .await
                .map_err(usage_storage_err)?;
        }
        self.memory.apply_quotas(quotas)
    }
}

impl Service<UsageEventBatch> for PostgresUsageStore {
    type Response = UsageAck;
    type Error = UsageError;

    async fn call(&self, req: UsageEventBatch) -> Result<Self::Response, Self::Error> {
        self.record_batch(req).await.map(|recorded| recorded.ack)
    }
}

impl Service<DeleteUsageBeforeRequest> for PostgresUsageStore {
    type Response = DeleteUsageAck;
    type Error = UsageError;

    async fn call(&self, req: DeleteUsageBeforeRequest) -> Result<Self::Response, Self::Error> {
        let target_ms = req.target_timestamp.saturating_mul(1000);
        let result = sqlx::query("DELETE FROM usage_events WHERE created_at_unix_ms < $1")
            .bind(target_ms)
            .execute(&self.pool)
            .await
            .map_err(usage_storage_err)?;
        let deleted = result.rows_affected() as usize;
        if deleted > 0 {
            self.memory
                .delete_before_unix_seconds(req.target_timestamp)?;
        }
        Ok(DeleteUsageAck { deleted })
    }
}

async fn migrate_usage_pg(pool: &PgPool) -> Result<(), UsageError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS usage_events (
            request_id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            token_id TEXT NOT NULL,
            channel_id TEXT NOT NULL,
            event_group TEXT NOT NULL DEFAULT '',
            model TEXT NOT NULL,
            upstream_model TEXT NOT NULL,
            prompt_tokens BIGINT,
            completion_tokens BIGINT,
            total_tokens BIGINT,
            cache_read_tokens BIGINT,
            cache_creation_tokens BIGINT,
            image_tokens BIGINT,
            audio_tokens BIGINT,
            quota BIGINT,
            status TEXT NOT NULL,
            latency_ms BIGINT NOT NULL,
            is_stream BOOLEAN NOT NULL DEFAULT FALSE,
            ip TEXT NOT NULL DEFAULT '',
            upstream_request_id TEXT NOT NULL DEFAULT '',
            created_at_unix_ms BIGINT NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(usage_storage_err)?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_usage_events_created_at ON \
         usage_events(created_at_unix_ms)",
    )
    .execute(pool)
    .await
    .map_err(usage_storage_err)?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_usage_events_user_id ON usage_events(user_id)")
        .execute(pool)
        .await
        .map_err(usage_storage_err)?;
    Ok(())
}

async fn load_usage_events_pg(pool: &PgPool) -> Result<Vec<UsageEvent>, UsageError> {
    sqlx::query(
        "SELECT request_id, user_id, token_id, channel_id, event_group, model, upstream_model, \
         prompt_tokens,
            completion_tokens, total_tokens, cache_read_tokens, cache_creation_tokens,
            image_tokens, audio_tokens, quota, status, latency_ms, is_stream, ip,
            upstream_request_id, created_at_unix_ms
         FROM usage_events ORDER BY created_at_unix_ms DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(usage_storage_err)?
    .into_iter()
    .map(|row| {
        Ok(UsageEvent {
            request_id:            row.try_get("request_id").map_err(usage_storage_err)?,
            user_id:               row.try_get("user_id").map_err(usage_storage_err)?,
            token_id:              row.try_get("token_id").map_err(usage_storage_err)?,
            channel_id:            row.try_get("channel_id").map_err(usage_storage_err)?,
            group:                 row.try_get("event_group").map_err(usage_storage_err)?,
            model:                 row.try_get("model").map_err(usage_storage_err)?,
            upstream_model:        row.try_get("upstream_model").map_err(usage_storage_err)?,
            prompt_tokens:         row
                .try_get::<Option<i64>, _>("prompt_tokens")
                .map_err(usage_storage_err)?
                .map(|v| v.max(0) as u64),
            completion_tokens:     row
                .try_get::<Option<i64>, _>("completion_tokens")
                .map_err(usage_storage_err)?
                .map(|v| v.max(0) as u64),
            total_tokens:          row
                .try_get::<Option<i64>, _>("total_tokens")
                .map_err(usage_storage_err)?
                .map(|v| v.max(0) as u64),
            cache_read_tokens:     row
                .try_get::<Option<i64>, _>("cache_read_tokens")
                .map_err(usage_storage_err)?
                .map(|v| v.max(0) as u64),
            cache_creation_tokens: row
                .try_get::<Option<i64>, _>("cache_creation_tokens")
                .map_err(usage_storage_err)?
                .map(|v| v.max(0) as u64),
            image_tokens:          row
                .try_get::<Option<i64>, _>("image_tokens")
                .map_err(usage_storage_err)?
                .map(|v| v.max(0) as u64),
            audio_tokens:          row
                .try_get::<Option<i64>, _>("audio_tokens")
                .map_err(usage_storage_err)?
                .map(|v| v.max(0) as u64),
            quota:                 row.try_get("quota").map_err(usage_storage_err)?,
            status:                usage_status_from_str(
                &row.try_get::<String, _>("status")
                    .map_err(usage_storage_err)?,
            ),
            latency_ms:            row
                .try_get::<i64, _>("latency_ms")
                .map_err(usage_storage_err)?
                .max(0) as u64,
            first_response_ms:     None,
            is_stream:             row.try_get("is_stream").map_err(usage_storage_err)?,
            ip:                    row.try_get("ip").map_err(usage_storage_err)?,
            upstream_request_id:   row
                .try_get("upstream_request_id")
                .map_err(usage_storage_err)?,
            created_at_unix_ms:    row
                .try_get("created_at_unix_ms")
                .map_err(usage_storage_err)?,
        })
    })
    .collect()
}

fn usage_status_str(status: UsageStatus) -> &'static str {
    match status {
        UsageStatus::Success => "success",
        UsageStatus::ClientError => "client_error",
        UsageStatus::UpstreamError => "upstream_error",
        UsageStatus::GatewayError => "gateway_error",
    }
}

fn usage_status_from_str(value: &str) -> UsageStatus {
    match value {
        "success" => UsageStatus::Success,
        "client_error" => UsageStatus::ClientError,
        "upstream_error" => UsageStatus::UpstreamError,
        "gateway_error" => UsageStatus::GatewayError,
        _ => UsageStatus::GatewayError,
    }
}

fn usage_string_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<String, UsageError> {
    row.try_get::<String, _>(name).map_err(usage_storage_err)
}

fn usage_string_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<String, UsageError> {
    row.try_get::<String, _>(name).map_err(usage_storage_err)
}

fn usage_i64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<i64, UsageError> {
    row.try_get::<i64, _>(name).map_err(usage_storage_err)
}

fn usage_i64_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<i64, UsageError> {
    row.try_get::<i64, _>(name).map_err(usage_storage_err)
}

fn usage_u64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<u64, UsageError> {
    usage_i64_col(row, name).map(|value| value.max(0) as u64)
}

fn usage_u64_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<u64, UsageError> {
    usage_i64_col_mysql(row, name).map(|value| value.max(0) as u64)
}

fn usage_opt_u64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<Option<u64>, UsageError> {
    row.try_get::<Option<i64>, _>(name)
        .map(|value| value.map(|value| value.max(0) as u64))
        .map_err(usage_storage_err)
}

fn usage_opt_u64_col_mysql(
    row: &sqlx::mysql::MySqlRow,
    name: &str,
) -> Result<Option<u64>, UsageError> {
    row.try_get::<Option<i64>, _>(name)
        .map(|value| value.map(|value| value.max(0) as u64))
        .map_err(usage_storage_err)
}

fn usage_opt_i64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<Option<i64>, UsageError> {
    row.try_get::<Option<i64>, _>(name)
        .map_err(usage_storage_err)
}

fn usage_opt_i64_col_mysql(
    row: &sqlx::mysql::MySqlRow,
    name: &str,
) -> Result<Option<i64>, UsageError> {
    row.try_get::<Option<i64>, _>(name)
        .map_err(usage_storage_err)
}

fn usage_bool_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<bool, UsageError> {
    row.try_get::<bool, _>(name).map_err(usage_storage_err)
}

fn usage_bool_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<bool, UsageError> {
    row.try_get::<bool, _>(name).map_err(usage_storage_err)
}

fn usage_storage_err(err: impl std::fmt::Display) -> UsageError {
    UsageError::Storage(err.to_string())
}
