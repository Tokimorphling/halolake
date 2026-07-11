use halolake_control_plane::{
    AdjustUserQuotaRequest, BatchSetChannelTagRequest, BootstrapRootUserRequest,
    ChannelStatusUpdateRequest, CreateChannelRequest, CreateTokenRequest, CreateUserRequest,
    DeleteChannelRequest, DeleteDisabledChannelsRequest, DeleteTokenRequest, DeleteUserRequest,
    GetChannelRequest, GetTokenRequest, GetUserRequest, ListChannelsRequest, ListTokensRequest,
    ListUsersRequest, LoginUserRequest, ManageUserRequest, ManagementData, ManagementError,
    MemoryManagementStore, MemoryUsageEventSink, PublishManagementSnapshotRequest,
    RegisterUserRequest, RegisteredUser, RevealChannelKeyRequest, RevealTokenKeyRequest,
    RevealedChannelKey, RevealedTokenKey, SearchChannelsRequest, SearchTokensRequest,
    SearchUsersRequest, SettleUsageRequest, SnapshotPublished, SnapshotPublisher,
    UpdateChannelRequest, UpdateChannelsByTagRequest, UpdateTokenRequest,
    UpdateUserAccessTokenRequest, UpdateUserRequest, UsageAck, UsageError, UsageEventBatch,
    UsageEventQuota, UsageSettlement, ValidateUserAccessTokenRequest,
};
use halolake_domain::{
    ChannelRecord, PageResult, TokenRecord, UsageEvent, UsageStatus, UserRecord,
};
use halolake_router_core::ModelMapping;
use serde::Serialize;
use service_async::Service;
use sqlx::{
    MySqlPool, PgPool, Row, SqlitePool, Transaction,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
    postgres::{PgConnectOptions, PgPoolOptions},
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{Arc, RwLock},
};

#[derive(Debug, Clone)]
pub(crate) enum ManagementStore {
    Memory(MemoryManagementStore),
    Sqlite(SqliteManagementStore),
    MySql(MySqlManagementStore),
    Postgres(PostgresManagementStore),
}

impl ManagementStore {
    pub(crate) fn memory(data: ManagementData) -> Self {
        Self::Memory(MemoryManagementStore::new(data))
    }

    pub(crate) async fn sqlite(url: &str, seed: ManagementData) -> Result<Self, ManagementError> {
        Ok(Self::Sqlite(
            SqliteManagementStore::connect(url, seed).await?,
        ))
    }

    pub(crate) async fn mysql(url: &str, seed: ManagementData) -> Result<Self, ManagementError> {
        Ok(Self::MySql(MySqlManagementStore::connect(url, seed).await?))
    }

    pub(crate) async fn postgres(url: &str, seed: ManagementData) -> Result<Self, ManagementError> {
        Ok(Self::Postgres(
            PostgresManagementStore::connect(url, seed).await?,
        ))
    }

    pub(crate) fn current_data(&self) -> Result<ManagementData, ManagementError> {
        match self {
            Self::Memory(store) => store.current_data(),
            Self::Sqlite(store) => store.current_data(),
            Self::MySql(store) => store.current_data(),
            Self::Postgres(store) => store.current_data(),
        }
    }

    /// Bumps the management snapshot version without other data changes.
    /// Used when options-derived config (affinity/group routing) changes so the
    /// gateway poll does not treat the republish as NotModified.
    pub(crate) async fn bump_version(&self) -> Result<u64, ManagementError> {
        match self {
            Self::Memory(store) => store.bump_version(),
            Self::Sqlite(store) => {
                let version = store.memory.bump_version()?;
                // Persist so the bumped version survives a restart; otherwise the
                // DB would still hold the old version and the gateway could see a
                // lower version after control-api restarts.
                store.persist().await?;
                Ok(version)
            }
            Self::MySql(store) => {
                let version = store.memory.bump_version()?;
                store.persist().await?;
                Ok(version)
            }
            Self::Postgres(store) => {
                let version = store.memory.bump_version()?;
                store.persist().await?;
                Ok(version)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SqliteManagementStore {
    pool:   SqlitePool,
    memory: MemoryManagementStore,
}

impl SqliteManagementStore {
    async fn connect(url: &str, seed: ManagementData) -> Result<Self, ManagementError> {
        let options = SqliteConnectOptions::from_str(url)
            .map_err(storage_err)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate(&pool).await?;

        let data = if is_empty(&pool).await? {
            save_data(&pool, &seed).await?;
            seed
        } else {
            load_data(&pool).await?
        };

        Ok(Self {
            pool,
            memory: MemoryManagementStore::new(data),
        })
    }

    fn current_data(&self) -> Result<ManagementData, ManagementError> {
        self.memory.current_data()
    }

    async fn persist(&self) -> Result<(), ManagementError> {
        let data = self.memory.current_data()?;
        save_data(&self.pool, &data).await
    }
}

macro_rules! impl_read_service {
    ($req:ty, $resp:ty) => {
        impl Service<$req> for ManagementStore {
            type Response = $resp;
            type Error = ManagementError;

            async fn call(&self, req: $req) -> Result<Self::Response, Self::Error> {
                match self {
                    Self::Memory(store) => store.call(req).await,
                    Self::Sqlite(store) => store.call(req).await,
                    Self::MySql(store) => store.call(req).await,
                    Self::Postgres(store) => store.call(req).await,
                }
            }
        }

        impl Service<$req> for SqliteManagementStore {
            type Response = $resp;
            type Error = ManagementError;

            async fn call(&self, req: $req) -> Result<Self::Response, Self::Error> {
                self.memory.call(req).await
            }
        }

        impl Service<$req> for MySqlManagementStore {
            type Response = $resp;
            type Error = ManagementError;

            async fn call(&self, req: $req) -> Result<Self::Response, Self::Error> {
                self.memory.call(req).await
            }
        }

        impl Service<$req> for PostgresManagementStore {
            type Response = $resp;
            type Error = ManagementError;

            async fn call(&self, req: $req) -> Result<Self::Response, Self::Error> {
                self.memory.call(req).await
            }
        }
    };
}

macro_rules! impl_write_service {
    ($req:ty, $resp:ty) => {
        impl Service<$req> for ManagementStore {
            type Response = $resp;
            type Error = ManagementError;

            async fn call(&self, req: $req) -> Result<Self::Response, Self::Error> {
                match self {
                    Self::Memory(store) => store.call(req).await,
                    Self::Sqlite(store) => store.call(req).await,
                    Self::MySql(store) => store.call(req).await,
                    Self::Postgres(store) => store.call(req).await,
                }
            }
        }

        impl Service<$req> for SqliteManagementStore {
            type Response = $resp;
            type Error = ManagementError;

            async fn call(&self, req: $req) -> Result<Self::Response, Self::Error> {
                let resp = self.memory.call(req).await?;
                self.persist().await?;
                Ok(resp)
            }
        }

        impl Service<$req> for MySqlManagementStore {
            type Response = $resp;
            type Error = ManagementError;

            async fn call(&self, req: $req) -> Result<Self::Response, Self::Error> {
                let resp = self.memory.call(req).await?;
                self.persist().await?;
                Ok(resp)
            }
        }

        impl Service<$req> for PostgresManagementStore {
            type Response = $resp;
            type Error = ManagementError;

            async fn call(&self, req: $req) -> Result<Self::Response, Self::Error> {
                let resp = self.memory.call(req).await?;
                self.persist().await?;
                Ok(resp)
            }
        }
    };
}

impl_read_service!(LoginUserRequest, UserRecord);
impl_read_service!(ListUsersRequest, PageResult<UserRecord>);
impl_read_service!(SearchUsersRequest, PageResult<UserRecord>);
impl_read_service!(GetUserRequest, UserRecord);
impl_read_service!(ValidateUserAccessTokenRequest, UserRecord);
impl_read_service!(ListTokensRequest, PageResult<TokenRecord>);
impl_read_service!(SearchTokensRequest, PageResult<TokenRecord>);
impl_read_service!(GetTokenRequest, TokenRecord);
impl_read_service!(RevealTokenKeyRequest, RevealedTokenKey);
impl_read_service!(ListChannelsRequest, PageResult<ChannelRecord>);
impl_read_service!(SearchChannelsRequest, PageResult<ChannelRecord>);
impl_read_service!(GetChannelRequest, ChannelRecord);
impl_read_service!(RevealChannelKeyRequest, RevealedChannelKey);

impl_write_service!(UpdateUserAccessTokenRequest, String);
impl_write_service!(BootstrapRootUserRequest, UserRecord);
impl_write_service!(RegisterUserRequest, RegisteredUser);
impl_write_service!(CreateUserRequest, UserRecord);
impl_write_service!(UpdateUserRequest, UserRecord);
impl_write_service!(DeleteUserRequest, ());
impl_write_service!(ManageUserRequest, UserRecord);
impl_write_service!(AdjustUserQuotaRequest, UserRecord);
impl_write_service!(SettleUsageRequest, UsageSettlement);
impl_write_service!(CreateTokenRequest, TokenRecord);
impl_write_service!(UpdateTokenRequest, TokenRecord);
impl_write_service!(DeleteTokenRequest, ());
impl_write_service!(CreateChannelRequest, ChannelRecord);
impl_write_service!(UpdateChannelRequest, ChannelRecord);
impl_write_service!(DeleteChannelRequest, ());
impl_write_service!(ChannelStatusUpdateRequest, ChannelRecord);
impl_write_service!(DeleteDisabledChannelsRequest, usize);
impl_write_service!(BatchSetChannelTagRequest, usize);
impl_write_service!(UpdateChannelsByTagRequest, usize);

impl<P> Service<PublishManagementSnapshotRequest<P>> for ManagementStore
where
    P: SnapshotPublisher,
{
    type Response = SnapshotPublished;
    type Error = ManagementError;

    async fn call(
        &self,
        req: PublishManagementSnapshotRequest<P>,
    ) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl<P> Service<PublishManagementSnapshotRequest<P>> for SqliteManagementStore
where
    P: SnapshotPublisher,
{
    type Response = SnapshotPublished;
    type Error = ManagementError;

    async fn call(
        &self,
        req: PublishManagementSnapshotRequest<P>,
    ) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl<P> Service<PublishManagementSnapshotRequest<P>> for MySqlManagementStore
where
    P: SnapshotPublisher,
{
    type Response = SnapshotPublished;
    type Error = ManagementError;

    async fn call(
        &self,
        req: PublishManagementSnapshotRequest<P>,
    ) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl<P> Service<PublishManagementSnapshotRequest<P>> for PostgresManagementStore
where
    P: SnapshotPublisher,
{
    type Response = SnapshotPublished;
    type Error = ManagementError;

    async fn call(
        &self,
        req: PublishManagementSnapshotRequest<P>,
    ) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MySqlManagementStore {
    pool:   MySqlPool,
    memory: MemoryManagementStore,
}

impl MySqlManagementStore {
    async fn connect(url: &str, seed: ManagementData) -> Result<Self, ManagementError> {
        let options = MySqlConnectOptions::from_str(url).map_err(storage_err)?;
        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_mysql(&pool).await?;

        let data = if is_empty_mysql(&pool).await? {
            save_data_mysql(&pool, &seed).await?;
            seed
        } else {
            load_data_mysql(&pool).await?
        };

        Ok(Self {
            pool,
            memory: MemoryManagementStore::new(data),
        })
    }

    fn current_data(&self) -> Result<ManagementData, ManagementError> {
        self.memory.current_data()
    }

    async fn persist(&self) -> Result<(), ManagementError> {
        let data = self.memory.current_data()?;
        save_data_mysql(&self.pool, &data).await
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresManagementStore {
    pool:   PgPool,
    memory: MemoryManagementStore,
}

impl PostgresManagementStore {
    async fn connect(url: &str, seed: ManagementData) -> Result<Self, ManagementError> {
        let options = PgConnectOptions::from_str(url).map_err(storage_err)?;
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_pg(&pool).await?;

        let data = if is_empty_pg(&pool).await? {
            save_data_pg(&pool, &seed).await?;
            seed
        } else {
            load_data_pg(&pool).await?
        };

        Ok(Self {
            pool,
            memory: MemoryManagementStore::new(data),
        })
    }

    fn current_data(&self) -> Result<ManagementData, ManagementError> {
        self.memory.current_data()
    }

    async fn persist(&self) -> Result<(), ManagementError> {
        let data = self.memory.current_data()?;
        save_data_pg(&self.pool, &data).await
    }
}

async fn migrate(pool: &SqlitePool) -> Result<(), ManagementError> {
    for stmt in [
        "CREATE TABLE IF NOT EXISTS control_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY,
            username TEXT NOT NULL UNIQUE,
            password TEXT NOT NULL,
            access_token TEXT,
            display_name TEXT NOT NULL DEFAULT '',
            role INTEGER NOT NULL,
            status INTEGER NOT NULL,
            email TEXT NOT NULL DEFAULT '',
            quota INTEGER NOT NULL DEFAULT 0,
            used_quota INTEGER NOT NULL DEFAULT 0,
            user_group TEXT NOT NULL DEFAULT 'default',
            setting TEXT NOT NULL DEFAULT '',
            remark TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL DEFAULT 0,
            last_login_at INTEGER NOT NULL DEFAULT 0
        )",
        "CREATE TABLE IF NOT EXISTS tokens (
            id INTEGER PRIMARY KEY,
            snapshot_id TEXT,
            user_id INTEGER NOT NULL,
            snapshot_user_id TEXT,
            key TEXT NOT NULL UNIQUE,
            status INTEGER NOT NULL,
            name TEXT NOT NULL DEFAULT '',
            created_time INTEGER NOT NULL DEFAULT 0,
            accessed_time INTEGER NOT NULL DEFAULT 0,
            expired_time INTEGER NOT NULL DEFAULT -1,
            remain_quota INTEGER NOT NULL DEFAULT 0,
            unlimited_quota INTEGER NOT NULL DEFAULT 0,
            model_limits_enabled INTEGER NOT NULL DEFAULT 0,
            model_limits TEXT NOT NULL DEFAULT '',
            allow_ips TEXT,
            used_quota INTEGER NOT NULL DEFAULT 0,
            token_group TEXT NOT NULL DEFAULT '',
            cross_group_retry INTEGER NOT NULL DEFAULT 0
        )",
        "CREATE TABLE IF NOT EXISTS channels (
            id INTEGER PRIMARY KEY,
            snapshot_id TEXT,
            channel_type INTEGER NOT NULL,
            key TEXT NOT NULL,
            status INTEGER NOT NULL,
            name TEXT NOT NULL DEFAULT '',
            weight INTEGER,
            created_time INTEGER NOT NULL DEFAULT 0,
            test_time INTEGER NOT NULL DEFAULT 0,
            response_time INTEGER NOT NULL DEFAULT 0,
            base_url TEXT,
            balance REAL NOT NULL DEFAULT 0,
            balance_updated_time INTEGER NOT NULL DEFAULT 0,
            models TEXT NOT NULL DEFAULT '',
            channel_group TEXT NOT NULL DEFAULT 'default',
            used_quota INTEGER NOT NULL DEFAULT 0,
            model_mapping TEXT,
            priority INTEGER,
            auto_ban INTEGER,
            tag TEXT,
            setting TEXT,
            param_override TEXT,
            header_override TEXT,
            remark TEXT,
            proxy_id INTEGER
        )",
        "CREATE TABLE IF NOT EXISTS model_mappings (
            requested_model TEXT PRIMARY KEY,
            channel_id TEXT NOT NULL,
            upstream_model TEXT NOT NULL
        )",
    ] {
        sqlx::query(stmt).execute(pool).await.map_err(storage_err)?;
    }
    // Best-effort migration for existing DBs.
    let _ = sqlx::query("ALTER TABLE channels ADD COLUMN proxy_id INTEGER")
        .execute(pool)
        .await;
    Ok(())
}

async fn migrate_mysql(pool: &MySqlPool) -> Result<(), ManagementError> {
    for stmt in [
        "CREATE TABLE IF NOT EXISTS control_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS users (
            id BIGINT PRIMARY KEY,
            username TEXT NOT NULL UNIQUE,
            password TEXT NOT NULL,
            access_token TEXT,
            display_name TEXT NOT NULL DEFAULT '',
            role INTEGER NOT NULL,
            status INTEGER NOT NULL,
            email TEXT NOT NULL DEFAULT '',
            quota BIGINT NOT NULL DEFAULT 0,
            used_quota BIGINT NOT NULL DEFAULT 0,
            user_group TEXT NOT NULL DEFAULT 'default',
            setting TEXT NOT NULL DEFAULT '',
            remark TEXT NOT NULL DEFAULT '',
            created_at BIGINT NOT NULL DEFAULT 0,
            last_login_at BIGINT NOT NULL DEFAULT 0
        )",
        "CREATE TABLE IF NOT EXISTS tokens (
            id BIGINT PRIMARY KEY,
            snapshot_id TEXT,
            user_id BIGINT NOT NULL,
            snapshot_user_id TEXT,
            key TEXT NOT NULL UNIQUE,
            status INTEGER NOT NULL,
            name TEXT NOT NULL DEFAULT '',
            created_time BIGINT NOT NULL DEFAULT 0,
            accessed_time BIGINT NOT NULL DEFAULT 0,
            expired_time BIGINT NOT NULL DEFAULT -1,
            remain_quota BIGINT NOT NULL DEFAULT 0,
            unlimited_quota BIGINT NOT NULL DEFAULT 0,
            model_limits_enabled INTEGER NOT NULL DEFAULT 0,
            model_limits TEXT NOT NULL DEFAULT '',
            allow_ips TEXT,
            used_quota BIGINT NOT NULL DEFAULT 0,
            token_group TEXT NOT NULL DEFAULT '',
            cross_group_retry INTEGER NOT NULL DEFAULT 0
        )",
        "CREATE TABLE IF NOT EXISTS channels (
            id BIGINT PRIMARY KEY,
            snapshot_id TEXT,
            channel_type INTEGER NOT NULL,
            key TEXT NOT NULL,
            status INTEGER NOT NULL,
            name TEXT NOT NULL DEFAULT '',
            weight INTEGER,
            created_time BIGINT NOT NULL DEFAULT 0,
            test_time BIGINT NOT NULL DEFAULT 0,
            response_time INTEGER NOT NULL DEFAULT 0,
            base_url TEXT,
            balance DOUBLE NOT NULL DEFAULT 0,
            balance_updated_time BIGINT NOT NULL DEFAULT 0,
            models TEXT NOT NULL DEFAULT '',
            channel_group TEXT NOT NULL DEFAULT 'default',
            used_quota BIGINT NOT NULL DEFAULT 0,
            model_mapping TEXT,
            priority BIGINT,
            auto_ban INTEGER,
            tag TEXT,
            setting TEXT,
            param_override TEXT,
            header_override TEXT,
            remark TEXT,
            proxy_id BIGINT
        )",
        "CREATE TABLE IF NOT EXISTS model_mappings (
            requested_model TEXT PRIMARY KEY,
            channel_id TEXT NOT NULL,
            upstream_model TEXT NOT NULL
        )",
    ] {
        sqlx::query(stmt).execute(pool).await.map_err(storage_err)?;
    }
    // Best-effort migration for existing DBs.
    let _ = sqlx::query("ALTER TABLE channels ADD COLUMN proxy_id BIGINT")
        .execute(pool)
        .await;
    Ok(())
}

async fn is_empty(pool: &SqlitePool) -> Result<bool, ManagementError> {
    let row = sqlx::query("SELECT COUNT(*) AS count FROM control_meta")
        .fetch_one(pool)
        .await
        .map_err(storage_err)?;
    Ok(row.try_get::<i64, _>("count").map_err(storage_err)? == 0)
}

async fn is_empty_mysql(pool: &MySqlPool) -> Result<bool, ManagementError> {
    let row = sqlx::query("SELECT COUNT(*) AS count FROM control_meta")
        .fetch_one(pool)
        .await
        .map_err(storage_err)?;
    Ok(row.try_get::<i64, _>("count").map_err(storage_err)? == 0)
}

async fn load_data(pool: &SqlitePool) -> Result<ManagementData, ManagementError> {
    let version = sqlx::query("SELECT value FROM control_meta WHERE key = 'version'")
        .fetch_optional(pool)
        .await
        .map_err(storage_err)?
        .and_then(|row| row.try_get::<String, _>("value").ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1);

    let users = sqlx::query(
        "SELECT id, username, password, access_token, display_name, role, status, email, quota,
            used_quota, user_group, setting, remark, created_at, last_login_at
         FROM users ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(UserRecord {
            id:            u64_col(&row, "id")?,
            username:      string_col(&row, "username")?,
            password:      string_col(&row, "password")?,
            access_token:  opt_string_col(&row, "access_token")?,
            display_name:  string_col(&row, "display_name")?,
            role:          i32_col(&row, "role")?,
            status:        i32_col(&row, "status")?,
            email:         string_col(&row, "email")?,
            quota:         i64_col(&row, "quota")?,
            used_quota:    i64_col(&row, "used_quota")?,
            group:         string_col(&row, "user_group")?,
            setting:       string_col(&row, "setting")?,
            remark:        string_col(&row, "remark")?,
            created_at:    i64_col(&row, "created_at")?,
            last_login_at: i64_col(&row, "last_login_at")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    let tokens = sqlx::query(
        "SELECT id, snapshot_id, user_id, snapshot_user_id, key, status, name, created_time,
            accessed_time, expired_time, remain_quota, unlimited_quota, model_limits_enabled,
            model_limits, allow_ips, used_quota, token_group, cross_group_retry
         FROM tokens ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(TokenRecord {
            id:                   u64_col(&row, "id")?,
            snapshot_id:          opt_string_col(&row, "snapshot_id")?,
            user_id:              u64_col(&row, "user_id")?,
            snapshot_user_id:     opt_string_col(&row, "snapshot_user_id")?,
            key:                  string_col(&row, "key")?,
            status:               i32_col(&row, "status")?,
            name:                 string_col(&row, "name")?,
            created_time:         i64_col(&row, "created_time")?,
            accessed_time:        i64_col(&row, "accessed_time")?,
            expired_time:         i64_col(&row, "expired_time")?,
            remain_quota:         i64_col(&row, "remain_quota")?,
            unlimited_quota:      bool_col(&row, "unlimited_quota")?,
            model_limits_enabled: bool_col(&row, "model_limits_enabled")?,
            model_limits:         string_col(&row, "model_limits")?,
            allow_ips:            opt_string_col(&row, "allow_ips")?,
            used_quota:           i64_col(&row, "used_quota")?,
            group:                string_col(&row, "token_group")?,
            cross_group_retry:    bool_col(&row, "cross_group_retry")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    let channels = sqlx::query(
        "SELECT id, snapshot_id, channel_type, key, status, name, weight, created_time, test_time,
            response_time, base_url, balance, balance_updated_time, models, channel_group,
            used_quota, model_mapping, priority, auto_ban, tag, setting, param_override,
            header_override, remark, proxy_id
         FROM channels ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(ChannelRecord {
            id:                   u64_col(&row, "id")?,
            snapshot_id:          opt_string_col(&row, "snapshot_id")?,
            channel_type:         i32_col(&row, "channel_type")?,
            key:                  string_col(&row, "key")?,
            status:               i32_col(&row, "status")?,
            name:                 string_col(&row, "name")?,
            weight:               opt_u32_col(&row, "weight")?,
            created_time:         i64_col(&row, "created_time")?,
            test_time:            i64_col(&row, "test_time")?,
            response_time:        i32_col(&row, "response_time")?,
            base_url:             opt_string_col(&row, "base_url")?,
            balance:              f64_col(&row, "balance")?,
            balance_updated_time: i64_col(&row, "balance_updated_time")?,
            models:               string_col(&row, "models")?,
            group:                string_col(&row, "channel_group")?,
            used_quota:           i64_col(&row, "used_quota")?,
            model_mapping:        opt_string_col(&row, "model_mapping")?,
            priority:             opt_i64_col(&row, "priority")?,
            auto_ban:             opt_i32_col(&row, "auto_ban")?,
            tag:                  opt_string_col(&row, "tag")?,
            setting:              opt_string_col(&row, "setting")?,
            param_override:       opt_string_col(&row, "param_override")?,
            header_override:      opt_string_col(&row, "header_override")?,
            remark:               opt_string_col(&row, "remark")?,
            proxy_id:             opt_u64_col(&row, "proxy_id")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    let model_mappings = sqlx::query(
        "SELECT requested_model, channel_id, upstream_model FROM model_mappings ORDER BY \
         requested_model",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(ModelMapping {
            requested_model: string_col(&row, "requested_model")?,
            channel_id:      string_col(&row, "channel_id")?,
            upstream_model:  string_col(&row, "upstream_model")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    Ok(ManagementData::new(
        version,
        users,
        tokens,
        channels,
        model_mappings,
    ))
}

async fn load_data_mysql(pool: &MySqlPool) -> Result<ManagementData, ManagementError> {
    let version = sqlx::query("SELECT value FROM control_meta WHERE key = 'version'")
        .fetch_optional(pool)
        .await
        .map_err(storage_err)?
        .and_then(|row| row.try_get::<String, _>("value").ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1);

    let users = sqlx::query(
        "SELECT id, username, password, access_token, display_name, role, status, email, quota,
            used_quota, user_group, setting, remark, created_at, last_login_at
         FROM users ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(UserRecord {
            id:            u64_col_mysql(&row, "id")?,
            username:      string_col_mysql(&row, "username")?,
            password:      string_col_mysql(&row, "password")?,
            access_token:  opt_string_col_mysql(&row, "access_token")?,
            display_name:  string_col_mysql(&row, "display_name")?,
            role:          i32_col_mysql(&row, "role")?,
            status:        i32_col_mysql(&row, "status")?,
            email:         string_col_mysql(&row, "email")?,
            quota:         i64_col_mysql(&row, "quota")?,
            used_quota:    i64_col_mysql(&row, "used_quota")?,
            group:         string_col_mysql(&row, "user_group")?,
            setting:       string_col_mysql(&row, "setting")?,
            remark:        string_col_mysql(&row, "remark")?,
            created_at:    i64_col_mysql(&row, "created_at")?,
            last_login_at: i64_col_mysql(&row, "last_login_at")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    let tokens = sqlx::query(
        "SELECT id, snapshot_id, user_id, snapshot_user_id, key, status, name, created_time,
            accessed_time, expired_time, remain_quota, unlimited_quota, model_limits_enabled,
            model_limits, allow_ips, used_quota, token_group, cross_group_retry
         FROM tokens ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(TokenRecord {
            id:                   u64_col_mysql(&row, "id")?,
            snapshot_id:          opt_string_col_mysql(&row, "snapshot_id")?,
            user_id:              u64_col_mysql(&row, "user_id")?,
            snapshot_user_id:     opt_string_col_mysql(&row, "snapshot_user_id")?,
            key:                  string_col_mysql(&row, "key")?,
            status:               i32_col_mysql(&row, "status")?,
            name:                 string_col_mysql(&row, "name")?,
            created_time:         i64_col_mysql(&row, "created_time")?,
            accessed_time:        i64_col_mysql(&row, "accessed_time")?,
            expired_time:         i64_col_mysql(&row, "expired_time")?,
            remain_quota:         i64_col_mysql(&row, "remain_quota")?,
            unlimited_quota:      bool_col_mysql(&row, "unlimited_quota")?,
            model_limits_enabled: bool_col_mysql(&row, "model_limits_enabled")?,
            model_limits:         string_col_mysql(&row, "model_limits")?,
            allow_ips:            opt_string_col_mysql(&row, "allow_ips")?,
            used_quota:           i64_col_mysql(&row, "used_quota")?,
            group:                string_col_mysql(&row, "token_group")?,
            cross_group_retry:    bool_col_mysql(&row, "cross_group_retry")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    let channels = sqlx::query(
        "SELECT id, snapshot_id, channel_type, key, status, name, weight, created_time, test_time,
            response_time, base_url, balance, balance_updated_time, models, channel_group,
            used_quota, model_mapping, priority, auto_ban, tag, setting, param_override,
            header_override, remark, proxy_id
         FROM channels ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(ChannelRecord {
            id:                   u64_col_mysql(&row, "id")?,
            snapshot_id:          opt_string_col_mysql(&row, "snapshot_id")?,
            channel_type:         i32_col_mysql(&row, "channel_type")?,
            key:                  string_col_mysql(&row, "key")?,
            status:               i32_col_mysql(&row, "status")?,
            name:                 string_col_mysql(&row, "name")?,
            weight:               opt_u32_col_mysql(&row, "weight")?,
            created_time:         i64_col_mysql(&row, "created_time")?,
            test_time:            i64_col_mysql(&row, "test_time")?,
            response_time:        i32_col_mysql(&row, "response_time")?,
            base_url:             opt_string_col_mysql(&row, "base_url")?,
            balance:              f64_col_mysql(&row, "balance")?,
            balance_updated_time: i64_col_mysql(&row, "balance_updated_time")?,
            models:               string_col_mysql(&row, "models")?,
            group:                string_col_mysql(&row, "channel_group")?,
            used_quota:           i64_col_mysql(&row, "used_quota")?,
            model_mapping:        opt_string_col_mysql(&row, "model_mapping")?,
            priority:             opt_i64_col_mysql(&row, "priority")?,
            auto_ban:             opt_i32_col_mysql(&row, "auto_ban")?,
            tag:                  opt_string_col_mysql(&row, "tag")?,
            setting:              opt_string_col_mysql(&row, "setting")?,
            param_override:       opt_string_col_mysql(&row, "param_override")?,
            header_override:      opt_string_col_mysql(&row, "header_override")?,
            remark:               opt_string_col_mysql(&row, "remark")?,
            proxy_id:             opt_u64_col_mysql(&row, "proxy_id")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    let model_mappings = sqlx::query(
        "SELECT requested_model, channel_id, upstream_model FROM model_mappings ORDER BY \
         requested_model",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(ModelMapping {
            requested_model: string_col_mysql(&row, "requested_model")?,
            channel_id:      string_col_mysql(&row, "channel_id")?,
            upstream_model:  string_col_mysql(&row, "upstream_model")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    Ok(ManagementData::new(
        version,
        users,
        tokens,
        channels,
        model_mappings,
    ))
}

async fn save_data(pool: &SqlitePool, data: &ManagementData) -> Result<(), ManagementError> {
    let mut tx = pool.begin().await.map_err(storage_err)?;
    save_data_tx(&mut tx, data).await?;
    tx.commit().await.map_err(storage_err)
}

async fn save_data_mysql(pool: &MySqlPool, data: &ManagementData) -> Result<(), ManagementError> {
    let mut tx = pool.begin().await.map_err(storage_err)?;
    save_data_tx_mysql(&mut tx, data).await?;
    tx.commit().await.map_err(storage_err)
}

async fn save_data_tx(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    data: &ManagementData,
) -> Result<(), ManagementError> {
    sqlx::query(
        "INSERT INTO control_meta (key, value) VALUES ('version', ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(data.version.to_string())
    .execute(&mut **tx)
    .await
    .map_err(storage_err)?;

    let mut user_ids = Vec::with_capacity(data.users.len());
    for user in &data.users {
        user_ids.push(user.id as i64);
        sqlx::query(
            "INSERT INTO users (
                id, username, password, access_token, display_name, role, status, email, quota,
                used_quota, user_group, setting, remark, created_at, last_login_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                username = excluded.username,
                password = excluded.password,
                access_token = excluded.access_token,
                display_name = excluded.display_name,
                role = excluded.role,
                status = excluded.status,
                email = excluded.email,
                quota = excluded.quota,
                used_quota = excluded.used_quota,
                user_group = excluded.user_group,
                setting = excluded.setting,
                remark = excluded.remark,
                created_at = excluded.created_at,
                last_login_at = excluded.last_login_at",
        )
        .bind(user.id as i64)
        .bind(&user.username)
        .bind(&user.password)
        .bind(&user.access_token)
        .bind(&user.display_name)
        .bind(user.role as i64)
        .bind(user.status as i64)
        .bind(&user.email)
        .bind(user.quota)
        .bind(user.used_quota)
        .bind(&user.group)
        .bind(&user.setting)
        .bind(&user.remark)
        .bind(user.created_at)
        .bind(user.last_login_at)
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;
    }
    delete_orphans_sqlite(tx, "users", "id", &user_ids).await?;

    let mut token_ids = Vec::with_capacity(data.tokens.len());
    for token in &data.tokens {
        token_ids.push(token.id as i64);
        sqlx::query(
            "INSERT INTO tokens (
                id, snapshot_id, user_id, snapshot_user_id, key, status, name, created_time,
                accessed_time, expired_time, remain_quota, unlimited_quota, model_limits_enabled,
                model_limits, allow_ips, used_quota, token_group, cross_group_retry
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                snapshot_id = excluded.snapshot_id,
                user_id = excluded.user_id,
                snapshot_user_id = excluded.snapshot_user_id,
                key = excluded.key,
                status = excluded.status,
                name = excluded.name,
                created_time = excluded.created_time,
                accessed_time = excluded.accessed_time,
                expired_time = excluded.expired_time,
                remain_quota = excluded.remain_quota,
                unlimited_quota = excluded.unlimited_quota,
                model_limits_enabled = excluded.model_limits_enabled,
                model_limits = excluded.model_limits,
                allow_ips = excluded.allow_ips,
                used_quota = excluded.used_quota,
                token_group = excluded.token_group,
                cross_group_retry = excluded.cross_group_retry",
        )
        .bind(token.id as i64)
        .bind(&token.snapshot_id)
        .bind(token.user_id as i64)
        .bind(&token.snapshot_user_id)
        .bind(&token.key)
        .bind(token.status as i64)
        .bind(&token.name)
        .bind(token.created_time)
        .bind(token.accessed_time)
        .bind(token.expired_time)
        .bind(token.remain_quota)
        .bind(bool_to_i64(token.unlimited_quota))
        .bind(bool_to_i64(token.model_limits_enabled))
        .bind(&token.model_limits)
        .bind(&token.allow_ips)
        .bind(token.used_quota)
        .bind(&token.group)
        .bind(bool_to_i64(token.cross_group_retry))
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;
    }
    delete_orphans_sqlite(tx, "tokens", "id", &token_ids).await?;

    let mut channel_ids = Vec::with_capacity(data.channels.len());
    for channel in &data.channels {
        channel_ids.push(channel.id as i64);
        sqlx::query(
            "INSERT INTO channels (
                id, snapshot_id, channel_type, key, status, name, weight, created_time, test_time,
                response_time, base_url, balance, balance_updated_time, models, channel_group,
                used_quota, model_mapping, priority, auto_ban, tag, setting, param_override,
                header_override, remark, proxy_id
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                snapshot_id = excluded.snapshot_id,
                channel_type = excluded.channel_type,
                key = excluded.key,
                status = excluded.status,
                name = excluded.name,
                weight = excluded.weight,
                created_time = excluded.created_time,
                test_time = excluded.test_time,
                response_time = excluded.response_time,
                base_url = excluded.base_url,
                balance = excluded.balance,
                balance_updated_time = excluded.balance_updated_time,
                models = excluded.models,
                channel_group = excluded.channel_group,
                used_quota = excluded.used_quota,
                model_mapping = excluded.model_mapping,
                priority = excluded.priority,
                auto_ban = excluded.auto_ban,
                tag = excluded.tag,
                setting = excluded.setting,
                param_override = excluded.param_override,
                header_override = excluded.header_override,
                remark = excluded.remark,
                proxy_id = excluded.proxy_id",
        )
        .bind(channel.id as i64)
        .bind(&channel.snapshot_id)
        .bind(channel.channel_type as i64)
        .bind(&channel.key)
        .bind(channel.status as i64)
        .bind(&channel.name)
        .bind(channel.weight.map(|weight| weight as i64))
        .bind(channel.created_time)
        .bind(channel.test_time)
        .bind(channel.response_time as i64)
        .bind(&channel.base_url)
        .bind(channel.balance)
        .bind(channel.balance_updated_time)
        .bind(&channel.models)
        .bind(&channel.group)
        .bind(channel.used_quota)
        .bind(&channel.model_mapping)
        .bind(channel.priority)
        .bind(channel.auto_ban.map(i64::from))
        .bind(&channel.tag)
        .bind(&channel.setting)
        .bind(&channel.param_override)
        .bind(&channel.header_override)
        .bind(&channel.remark)
        .bind(channel.proxy_id.map(|v| v as i64))
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;
    }
    delete_orphans_sqlite(tx, "channels", "id", &channel_ids).await?;

    let mut mapping_keys = Vec::with_capacity(data.model_mappings.len());
    for mapping in &data.model_mappings {
        mapping_keys.push(mapping.requested_model.clone());
        sqlx::query(
            "INSERT INTO model_mappings (requested_model, channel_id, upstream_model)
             VALUES (?, ?, ?)
             ON CONFLICT(requested_model) DO UPDATE SET
                channel_id = excluded.channel_id,
                upstream_model = excluded.upstream_model",
        )
        .bind(&mapping.requested_model)
        .bind(&mapping.channel_id)
        .bind(&mapping.upstream_model)
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;
    }
    delete_orphan_strings_sqlite(tx, "model_mappings", "requested_model", &mapping_keys).await?;

    Ok(())
}

async fn delete_orphans_sqlite(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    table: &str,
    column: &str,
    keep_ids: &[i64],
) -> Result<(), ManagementError> {
    if keep_ids.is_empty() {
        sqlx::query(&format!("DELETE FROM {table}"))
            .execute(&mut **tx)
            .await
            .map_err(storage_err)?;
        return Ok(());
    }
    let placeholders = (0..keep_ids.len())
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("DELETE FROM {table} WHERE {column} NOT IN ({placeholders})");
    let mut q = sqlx::query(&sql);
    for id in keep_ids {
        q = q.bind(*id);
    }
    q.execute(&mut **tx).await.map_err(storage_err)?;
    Ok(())
}

async fn delete_orphan_strings_sqlite(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    table: &str,
    column: &str,
    keep: &[String],
) -> Result<(), ManagementError> {
    if keep.is_empty() {
        sqlx::query(&format!("DELETE FROM {table}"))
            .execute(&mut **tx)
            .await
            .map_err(storage_err)?;
        return Ok(());
    }
    let placeholders = (0..keep.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!("DELETE FROM {table} WHERE {column} NOT IN ({placeholders})");
    let mut q = sqlx::query(&sql);
    for key in keep {
        q = q.bind(key);
    }
    q.execute(&mut **tx).await.map_err(storage_err)?;
    Ok(())
}

async fn save_data_tx_mysql(
    tx: &mut Transaction<'_, sqlx::MySql>,
    data: &ManagementData,
) -> Result<(), ManagementError> {
    sqlx::query(
        "INSERT INTO control_meta (`key`, value) VALUES ('version', ?)
         ON DUPLICATE KEY UPDATE value = VALUES(value)",
    )
    .bind(data.version.to_string())
    .execute(&mut **tx)
    .await
    .map_err(storage_err)?;

    let mut user_ids = Vec::with_capacity(data.users.len());
    for user in &data.users {
        user_ids.push(user.id as i64);
        sqlx::query(
            "INSERT INTO users (
                id, username, password, access_token, display_name, role, status, email, quota,
                used_quota, user_group, setting, remark, created_at, last_login_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE
                username = VALUES(username),
                password = VALUES(password),
                access_token = VALUES(access_token),
                display_name = VALUES(display_name),
                role = VALUES(role),
                status = VALUES(status),
                email = VALUES(email),
                quota = VALUES(quota),
                used_quota = VALUES(used_quota),
                user_group = VALUES(user_group),
                setting = VALUES(setting),
                remark = VALUES(remark),
                created_at = VALUES(created_at),
                last_login_at = VALUES(last_login_at)",
        )
        .bind(user.id as i64)
        .bind(&user.username)
        .bind(&user.password)
        .bind(&user.access_token)
        .bind(&user.display_name)
        .bind(user.role as i64)
        .bind(user.status as i64)
        .bind(&user.email)
        .bind(user.quota)
        .bind(user.used_quota)
        .bind(&user.group)
        .bind(&user.setting)
        .bind(&user.remark)
        .bind(user.created_at)
        .bind(user.last_login_at)
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;
    }
    delete_orphans_mysql(tx, "users", "id", &user_ids).await?;

    let mut token_ids = Vec::with_capacity(data.tokens.len());
    for token in &data.tokens {
        token_ids.push(token.id as i64);
        sqlx::query(
            "INSERT INTO tokens (
                id, snapshot_id, user_id, snapshot_user_id, `key`, status, name, created_time,
                accessed_time, expired_time, remain_quota, unlimited_quota, model_limits_enabled,
                model_limits, allow_ips, used_quota, token_group, cross_group_retry
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE
                snapshot_id = VALUES(snapshot_id),
                user_id = VALUES(user_id),
                snapshot_user_id = VALUES(snapshot_user_id),
                `key` = VALUES(`key`),
                status = VALUES(status),
                name = VALUES(name),
                created_time = VALUES(created_time),
                accessed_time = VALUES(accessed_time),
                expired_time = VALUES(expired_time),
                remain_quota = VALUES(remain_quota),
                unlimited_quota = VALUES(unlimited_quota),
                model_limits_enabled = VALUES(model_limits_enabled),
                model_limits = VALUES(model_limits),
                allow_ips = VALUES(allow_ips),
                used_quota = VALUES(used_quota),
                token_group = VALUES(token_group),
                cross_group_retry = VALUES(cross_group_retry)",
        )
        .bind(token.id as i64)
        .bind(&token.snapshot_id)
        .bind(token.user_id as i64)
        .bind(&token.snapshot_user_id)
        .bind(&token.key)
        .bind(token.status as i64)
        .bind(&token.name)
        .bind(token.created_time)
        .bind(token.accessed_time)
        .bind(token.expired_time)
        .bind(token.remain_quota)
        .bind(bool_to_i64(token.unlimited_quota))
        .bind(bool_to_i64(token.model_limits_enabled))
        .bind(&token.model_limits)
        .bind(&token.allow_ips)
        .bind(token.used_quota)
        .bind(&token.group)
        .bind(bool_to_i64(token.cross_group_retry))
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;
    }
    delete_orphans_mysql(tx, "tokens", "id", &token_ids).await?;

    let mut channel_ids = Vec::with_capacity(data.channels.len());
    for channel in &data.channels {
        channel_ids.push(channel.id as i64);
        sqlx::query(
            "INSERT INTO channels (
                id, snapshot_id, channel_type, `key`, status, name, weight, created_time, \
             test_time,
                response_time, base_url, balance, balance_updated_time, models, channel_group,
                used_quota, model_mapping, priority, auto_ban, tag, setting, param_override,
                header_override, remark, proxy_id
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE
                snapshot_id = VALUES(snapshot_id),
                channel_type = VALUES(channel_type),
                `key` = VALUES(`key`),
                status = VALUES(status),
                name = VALUES(name),
                weight = VALUES(weight),
                created_time = VALUES(created_time),
                test_time = VALUES(test_time),
                response_time = VALUES(response_time),
                base_url = VALUES(base_url),
                balance = VALUES(balance),
                balance_updated_time = VALUES(balance_updated_time),
                models = VALUES(models),
                channel_group = VALUES(channel_group),
                used_quota = VALUES(used_quota),
                model_mapping = VALUES(model_mapping),
                priority = VALUES(priority),
                auto_ban = VALUES(auto_ban),
                tag = VALUES(tag),
                setting = VALUES(setting),
                param_override = VALUES(param_override),
                header_override = VALUES(header_override),
                remark = VALUES(remark),
                proxy_id = VALUES(proxy_id)",
        )
        .bind(channel.id as i64)
        .bind(&channel.snapshot_id)
        .bind(channel.channel_type as i64)
        .bind(&channel.key)
        .bind(channel.status as i64)
        .bind(&channel.name)
        .bind(channel.weight.map(|weight| weight as i64))
        .bind(channel.created_time)
        .bind(channel.test_time)
        .bind(channel.response_time as i64)
        .bind(&channel.base_url)
        .bind(channel.balance)
        .bind(channel.balance_updated_time)
        .bind(&channel.models)
        .bind(&channel.group)
        .bind(channel.used_quota)
        .bind(&channel.model_mapping)
        .bind(channel.priority)
        .bind(channel.auto_ban.map(i64::from))
        .bind(&channel.tag)
        .bind(&channel.setting)
        .bind(&channel.param_override)
        .bind(&channel.header_override)
        .bind(&channel.remark)
        .bind(channel.proxy_id.map(|v| v as i64))
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;
    }
    delete_orphans_mysql(tx, "channels", "id", &channel_ids).await?;

    let mut mapping_keys = Vec::with_capacity(data.model_mappings.len());
    for mapping in &data.model_mappings {
        mapping_keys.push(mapping.requested_model.clone());
        sqlx::query(
            "INSERT INTO model_mappings (requested_model, channel_id, upstream_model)
             VALUES (?, ?, ?)
             ON DUPLICATE KEY UPDATE
                channel_id = VALUES(channel_id),
                upstream_model = VALUES(upstream_model)",
        )
        .bind(&mapping.requested_model)
        .bind(&mapping.channel_id)
        .bind(&mapping.upstream_model)
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;
    }
    delete_orphan_strings_mysql(tx, "model_mappings", "requested_model", &mapping_keys).await?;

    Ok(())
}

async fn delete_orphans_mysql(
    tx: &mut Transaction<'_, sqlx::MySql>,
    table: &str,
    column: &str,
    keep_ids: &[i64],
) -> Result<(), ManagementError> {
    if keep_ids.is_empty() {
        sqlx::query(&format!("DELETE FROM {table}"))
            .execute(&mut **tx)
            .await
            .map_err(storage_err)?;
        return Ok(());
    }
    let placeholders = (0..keep_ids.len())
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("DELETE FROM {table} WHERE {column} NOT IN ({placeholders})");
    let mut q = sqlx::query(&sql);
    for id in keep_ids {
        q = q.bind(*id);
    }
    q.execute(&mut **tx).await.map_err(storage_err)?;
    Ok(())
}

async fn delete_orphan_strings_mysql(
    tx: &mut Transaction<'_, sqlx::MySql>,
    table: &str,
    column: &str,
    keep: &[String],
) -> Result<(), ManagementError> {
    if keep.is_empty() {
        sqlx::query(&format!("DELETE FROM {table}"))
            .execute(&mut **tx)
            .await
            .map_err(storage_err)?;
        return Ok(());
    }
    let placeholders = (0..keep.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!("DELETE FROM {table} WHERE {column} NOT IN ({placeholders})");
    let mut q = sqlx::query(&sql);
    for key in keep {
        q = q.bind(key);
    }
    q.execute(&mut **tx).await.map_err(storage_err)?;
    Ok(())
}

fn string_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<String, ManagementError> {
    row.try_get::<String, _>(name).map_err(storage_err)
}

fn string_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<String, ManagementError> {
    row.try_get::<String, _>(name).map_err(storage_err)
}

fn opt_string_col(
    row: &sqlx::sqlite::SqliteRow,
    name: &str,
) -> Result<Option<String>, ManagementError> {
    row.try_get::<Option<String>, _>(name).map_err(storage_err)
}

fn opt_string_col_mysql(
    row: &sqlx::mysql::MySqlRow,
    name: &str,
) -> Result<Option<String>, ManagementError> {
    row.try_get::<Option<String>, _>(name).map_err(storage_err)
}

fn i64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<i64, ManagementError> {
    row.try_get::<i64, _>(name).map_err(storage_err)
}

fn i64_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<i64, ManagementError> {
    row.try_get::<i64, _>(name).map_err(storage_err)
}

fn opt_i64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<Option<i64>, ManagementError> {
    row.try_get::<Option<i64>, _>(name).map_err(storage_err)
}

fn opt_i64_col_mysql(
    row: &sqlx::mysql::MySqlRow,
    name: &str,
) -> Result<Option<i64>, ManagementError> {
    row.try_get::<Option<i64>, _>(name).map_err(storage_err)
}

fn u64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<u64, ManagementError> {
    i64_col(row, name).map(|value| value.max(0) as u64)
}

fn u64_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<u64, ManagementError> {
    i64_col_mysql(row, name).map(|value| value.max(0) as u64)
}

fn i32_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<i32, ManagementError> {
    i64_col(row, name).map(|value| value as i32)
}

fn i32_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<i32, ManagementError> {
    i64_col_mysql(row, name).map(|value| value as i32)
}

fn opt_i32_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<Option<i32>, ManagementError> {
    opt_i64_col(row, name).map(|value| value.map(|value| value as i32))
}

fn opt_i32_col_mysql(
    row: &sqlx::mysql::MySqlRow,
    name: &str,
) -> Result<Option<i32>, ManagementError> {
    opt_i64_col_mysql(row, name).map(|value| value.map(|value| value as i32))
}

fn opt_u32_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<Option<u32>, ManagementError> {
    opt_i64_col(row, name).map(|value| value.map(|value| value.max(0) as u32))
}

fn opt_u64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<Option<u64>, ManagementError> {
    opt_i64_col(row, name).map(|value| value.map(|value| value.max(0) as u64))
}

fn opt_u32_col_mysql(
    row: &sqlx::mysql::MySqlRow,
    name: &str,
) -> Result<Option<u32>, ManagementError> {
    opt_i64_col_mysql(row, name).map(|value| value.map(|value| value.max(0) as u32))
}

fn opt_u64_col_mysql(
    row: &sqlx::mysql::MySqlRow,
    name: &str,
) -> Result<Option<u64>, ManagementError> {
    opt_i64_col_mysql(row, name).map(|value| value.map(|value| value.max(0) as u64))
}

fn f64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<f64, ManagementError> {
    row.try_get::<f64, _>(name).map_err(storage_err)
}

fn f64_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<f64, ManagementError> {
    row.try_get::<f64, _>(name).map_err(storage_err)
}

fn bool_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<bool, ManagementError> {
    i64_col(row, name).map(|value| value != 0)
}

fn bool_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<bool, ManagementError> {
    i64_col_mysql(row, name).map(|value| value != 0)
}

fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn storage_err(err: impl std::fmt::Display) -> ManagementError {
    ManagementError::Storage(err.to_string())
}

async fn migrate_pg(pool: &PgPool) -> Result<(), ManagementError> {
    for stmt in [
        "CREATE TABLE IF NOT EXISTS control_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS users (
            id BIGINT PRIMARY KEY,
            username TEXT NOT NULL UNIQUE,
            password TEXT NOT NULL,
            access_token TEXT,
            display_name TEXT NOT NULL DEFAULT '',
            role INTEGER NOT NULL,
            status INTEGER NOT NULL,
            email TEXT NOT NULL DEFAULT '',
            quota BIGINT NOT NULL DEFAULT 0,
            used_quota BIGINT NOT NULL DEFAULT 0,
            user_group TEXT NOT NULL DEFAULT 'default',
            setting TEXT NOT NULL DEFAULT '',
            remark TEXT NOT NULL DEFAULT '',
            created_at BIGINT NOT NULL DEFAULT 0,
            last_login_at BIGINT NOT NULL DEFAULT 0
        )",
        "CREATE TABLE IF NOT EXISTS tokens (
            id BIGINT PRIMARY KEY,
            snapshot_id TEXT,
            user_id BIGINT NOT NULL,
            snapshot_user_id TEXT,
            key TEXT NOT NULL UNIQUE,
            status INTEGER NOT NULL,
            name TEXT NOT NULL DEFAULT '',
            created_time BIGINT NOT NULL DEFAULT 0,
            accessed_time BIGINT NOT NULL DEFAULT 0,
            expired_time BIGINT NOT NULL DEFAULT -1,
            remain_quota BIGINT NOT NULL DEFAULT 0,
            unlimited_quota INTEGER NOT NULL DEFAULT 0,
            model_limits_enabled INTEGER NOT NULL DEFAULT 0,
            model_limits TEXT NOT NULL DEFAULT '',
            allow_ips TEXT,
            used_quota BIGINT NOT NULL DEFAULT 0,
            token_group TEXT NOT NULL DEFAULT '',
            cross_group_retry INTEGER NOT NULL DEFAULT 0
        )",
        "CREATE TABLE IF NOT EXISTS channels (
            id BIGINT PRIMARY KEY,
            snapshot_id TEXT,
            channel_type INTEGER NOT NULL,
            key TEXT NOT NULL,
            status INTEGER NOT NULL,
            name TEXT NOT NULL DEFAULT '',
            weight INTEGER,
            created_time BIGINT NOT NULL DEFAULT 0,
            test_time BIGINT NOT NULL DEFAULT 0,
            response_time INTEGER NOT NULL DEFAULT 0,
            base_url TEXT,
            balance DOUBLE PRECISION NOT NULL DEFAULT 0,
            balance_updated_time BIGINT NOT NULL DEFAULT 0,
            models TEXT NOT NULL DEFAULT '',
            channel_group TEXT NOT NULL DEFAULT 'default',
            used_quota BIGINT NOT NULL DEFAULT 0,
            model_mapping TEXT,
            priority BIGINT,
            auto_ban INTEGER,
            tag TEXT,
            setting TEXT,
            param_override TEXT,
            header_override TEXT,
            remark TEXT,
            proxy_id BIGINT
        )",
        "CREATE TABLE IF NOT EXISTS model_mappings (
            requested_model TEXT PRIMARY KEY,
            channel_id TEXT NOT NULL,
            upstream_model TEXT NOT NULL
        )",
    ] {
        sqlx::query(stmt).execute(pool).await.map_err(storage_err)?;
    }
    // Best-effort migration for existing DBs.
    let _ = sqlx::query("ALTER TABLE channels ADD COLUMN proxy_id BIGINT")
        .execute(pool)
        .await;
    Ok(())
}

async fn is_empty_pg(pool: &PgPool) -> Result<bool, ManagementError> {
    let row = sqlx::query("SELECT COUNT(*) AS count FROM control_meta")
        .fetch_one(pool)
        .await
        .map_err(storage_err)?;
    Ok(row.try_get::<i64, _>("count").map_err(storage_err)? == 0)
}

async fn load_data_pg(pool: &PgPool) -> Result<ManagementData, ManagementError> {
    let version = sqlx::query("SELECT value FROM control_meta WHERE key = 'version'")
        .fetch_optional(pool)
        .await
        .map_err(storage_err)?
        .and_then(|row| row.try_get::<String, _>("value").ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1);

    let users = sqlx::query(
        "SELECT id, username, password, access_token, display_name, role, status, email, quota,
            used_quota, user_group, setting, remark, created_at, last_login_at
         FROM users ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(UserRecord {
            id:            pg_u64_col(&row, "id")?,
            username:      pg_string_col(&row, "username")?,
            password:      pg_string_col(&row, "password")?,
            access_token:  pg_opt_string_col(&row, "access_token")?,
            display_name:  pg_string_col(&row, "display_name")?,
            role:          pg_i32_col(&row, "role")?,
            status:        pg_i32_col(&row, "status")?,
            email:         pg_string_col(&row, "email")?,
            quota:         pg_i64_col(&row, "quota")?,
            used_quota:    pg_i64_col(&row, "used_quota")?,
            group:         pg_string_col(&row, "user_group")?,
            setting:       pg_string_col(&row, "setting")?,
            remark:        pg_string_col(&row, "remark")?,
            created_at:    pg_i64_col(&row, "created_at")?,
            last_login_at: pg_i64_col(&row, "last_login_at")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    let tokens = sqlx::query(
        "SELECT id, snapshot_id, user_id, snapshot_user_id, key, status, name, created_time,
            accessed_time, expired_time, remain_quota, unlimited_quota, model_limits_enabled,
            model_limits, allow_ips, used_quota, token_group, cross_group_retry
         FROM tokens ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(TokenRecord {
            id:                   pg_u64_col(&row, "id")?,
            snapshot_id:          pg_opt_string_col(&row, "snapshot_id")?,
            user_id:              pg_u64_col(&row, "user_id")?,
            snapshot_user_id:     pg_opt_string_col(&row, "snapshot_user_id")?,
            key:                  pg_string_col(&row, "key")?,
            status:               pg_i32_col(&row, "status")?,
            name:                 pg_string_col(&row, "name")?,
            created_time:         pg_i64_col(&row, "created_time")?,
            accessed_time:        pg_i64_col(&row, "accessed_time")?,
            expired_time:         pg_i64_col(&row, "expired_time")?,
            remain_quota:         pg_i64_col(&row, "remain_quota")?,
            unlimited_quota:      pg_bool_col(&row, "unlimited_quota")?,
            model_limits_enabled: pg_bool_col(&row, "model_limits_enabled")?,
            model_limits:         pg_string_col(&row, "model_limits")?,
            allow_ips:            pg_opt_string_col(&row, "allow_ips")?,
            used_quota:           pg_i64_col(&row, "used_quota")?,
            group:                pg_string_col(&row, "token_group")?,
            cross_group_retry:    pg_bool_col(&row, "cross_group_retry")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    let channels = sqlx::query(
        "SELECT id, snapshot_id, channel_type, key, status, name, weight, created_time, test_time,
            response_time, base_url, balance, balance_updated_time, models, channel_group,
            used_quota, model_mapping, priority, auto_ban, tag, setting, param_override,
            header_override, remark, proxy_id
         FROM channels ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(ChannelRecord {
            id:                   pg_u64_col(&row, "id")?,
            snapshot_id:          pg_opt_string_col(&row, "snapshot_id")?,
            channel_type:         pg_i32_col(&row, "channel_type")?,
            key:                  pg_string_col(&row, "key")?,
            status:               pg_i32_col(&row, "status")?,
            name:                 pg_string_col(&row, "name")?,
            weight:               pg_opt_u32_col(&row, "weight")?,
            created_time:         pg_i64_col(&row, "created_time")?,
            test_time:            pg_i64_col(&row, "test_time")?,
            response_time:        pg_i32_col(&row, "response_time")?,
            base_url:             pg_opt_string_col(&row, "base_url")?,
            balance:              pg_f64_col(&row, "balance")?,
            balance_updated_time: pg_i64_col(&row, "balance_updated_time")?,
            models:               pg_string_col(&row, "models")?,
            group:                pg_string_col(&row, "channel_group")?,
            used_quota:           pg_i64_col(&row, "used_quota")?,
            model_mapping:        pg_opt_string_col(&row, "model_mapping")?,
            priority:             pg_opt_i64_col(&row, "priority")?,
            auto_ban:             pg_opt_i32_col(&row, "auto_ban")?,
            tag:                  pg_opt_string_col(&row, "tag")?,
            setting:              pg_opt_string_col(&row, "setting")?,
            param_override:       pg_opt_string_col(&row, "param_override")?,
            header_override:      pg_opt_string_col(&row, "header_override")?,
            remark:               pg_opt_string_col(&row, "remark")?,
            proxy_id:             pg_opt_u64_col(&row, "proxy_id")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    let model_mappings = sqlx::query(
        "SELECT requested_model, channel_id, upstream_model FROM model_mappings ORDER BY \
         requested_model",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(ModelMapping {
            requested_model: pg_string_col(&row, "requested_model")?,
            channel_id:      pg_string_col(&row, "channel_id")?,
            upstream_model:  pg_string_col(&row, "upstream_model")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    Ok(ManagementData::new(
        version,
        users,
        tokens,
        channels,
        model_mappings,
    ))
}

async fn save_data_pg(pool: &PgPool, data: &ManagementData) -> Result<(), ManagementError> {
    let mut tx = pool.begin().await.map_err(storage_err)?;

    sqlx::query(
        "INSERT INTO control_meta (key, value) VALUES ('version', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(data.version.to_string())
    .execute(&mut *tx)
    .await
    .map_err(storage_err)?;

    let mut user_ids = Vec::with_capacity(data.users.len());
    for user in &data.users {
        user_ids.push(user.id as i64);
        sqlx::query(
            "INSERT INTO users (
                id, username, password, access_token, display_name, role, status, email, quota,
                used_quota, user_group, setting, remark, created_at, last_login_at
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
             ON CONFLICT (id) DO UPDATE SET
                username = EXCLUDED.username,
                password = EXCLUDED.password,
                access_token = EXCLUDED.access_token,
                display_name = EXCLUDED.display_name,
                role = EXCLUDED.role,
                status = EXCLUDED.status,
                email = EXCLUDED.email,
                quota = EXCLUDED.quota,
                used_quota = EXCLUDED.used_quota,
                user_group = EXCLUDED.user_group,
                setting = EXCLUDED.setting,
                remark = EXCLUDED.remark,
                created_at = EXCLUDED.created_at,
                last_login_at = EXCLUDED.last_login_at",
        )
        .bind(user.id as i64)
        .bind(&user.username)
        .bind(&user.password)
        .bind(&user.access_token)
        .bind(&user.display_name)
        .bind(user.role as i32)
        .bind(user.status as i32)
        .bind(&user.email)
        .bind(user.quota)
        .bind(user.used_quota)
        .bind(&user.group)
        .bind(&user.setting)
        .bind(&user.remark)
        .bind(user.created_at)
        .bind(user.last_login_at)
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    }
    delete_orphans_pg(&mut tx, "users", "id", &user_ids).await?;

    let mut token_ids = Vec::with_capacity(data.tokens.len());
    for token in &data.tokens {
        token_ids.push(token.id as i64);
        sqlx::query(
            "INSERT INTO tokens (
                id, snapshot_id, user_id, snapshot_user_id, key, status, name, created_time,
                accessed_time, expired_time, remain_quota, unlimited_quota, model_limits_enabled,
                model_limits, allow_ips, used_quota, token_group, cross_group_retry
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18)
             ON CONFLICT (id) DO UPDATE SET
                snapshot_id = EXCLUDED.snapshot_id,
                user_id = EXCLUDED.user_id,
                snapshot_user_id = EXCLUDED.snapshot_user_id,
                key = EXCLUDED.key,
                status = EXCLUDED.status,
                name = EXCLUDED.name,
                created_time = EXCLUDED.created_time,
                accessed_time = EXCLUDED.accessed_time,
                expired_time = EXCLUDED.expired_time,
                remain_quota = EXCLUDED.remain_quota,
                unlimited_quota = EXCLUDED.unlimited_quota,
                model_limits_enabled = EXCLUDED.model_limits_enabled,
                model_limits = EXCLUDED.model_limits,
                allow_ips = EXCLUDED.allow_ips,
                used_quota = EXCLUDED.used_quota,
                token_group = EXCLUDED.token_group,
                cross_group_retry = EXCLUDED.cross_group_retry",
        )
        .bind(token.id as i64)
        .bind(&token.snapshot_id)
        .bind(token.user_id as i64)
        .bind(&token.snapshot_user_id)
        .bind(&token.key)
        .bind(token.status as i32)
        .bind(&token.name)
        .bind(token.created_time)
        .bind(token.accessed_time)
        .bind(token.expired_time)
        .bind(token.remain_quota)
        .bind(bool_to_i64(token.unlimited_quota) as i32)
        .bind(bool_to_i64(token.model_limits_enabled) as i32)
        .bind(&token.model_limits)
        .bind(&token.allow_ips)
        .bind(token.used_quota)
        .bind(&token.group)
        .bind(bool_to_i64(token.cross_group_retry) as i32)
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    }
    delete_orphans_pg(&mut tx, "tokens", "id", &token_ids).await?;

    let mut channel_ids = Vec::with_capacity(data.channels.len());
    for channel in &data.channels {
        channel_ids.push(channel.id as i64);
        sqlx::query(
            "INSERT INTO channels (
                id, snapshot_id, channel_type, key, status, name, weight, created_time, test_time,
                response_time, base_url, balance, balance_updated_time, models, channel_group,
                used_quota, model_mapping, priority, auto_ban, tag, setting, param_override,
                header_override, remark, proxy_id
             ) VALUES \
             ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,\
             $24,$25)
             ON CONFLICT (id) DO UPDATE SET
                snapshot_id = EXCLUDED.snapshot_id,
                channel_type = EXCLUDED.channel_type,
                key = EXCLUDED.key,
                status = EXCLUDED.status,
                name = EXCLUDED.name,
                weight = EXCLUDED.weight,
                created_time = EXCLUDED.created_time,
                test_time = EXCLUDED.test_time,
                response_time = EXCLUDED.response_time,
                base_url = EXCLUDED.base_url,
                balance = EXCLUDED.balance,
                balance_updated_time = EXCLUDED.balance_updated_time,
                models = EXCLUDED.models,
                channel_group = EXCLUDED.channel_group,
                used_quota = EXCLUDED.used_quota,
                model_mapping = EXCLUDED.model_mapping,
                priority = EXCLUDED.priority,
                auto_ban = EXCLUDED.auto_ban,
                tag = EXCLUDED.tag,
                setting = EXCLUDED.setting,
                param_override = EXCLUDED.param_override,
                header_override = EXCLUDED.header_override,
                remark = EXCLUDED.remark,
                proxy_id = EXCLUDED.proxy_id",
        )
        .bind(channel.id as i64)
        .bind(&channel.snapshot_id)
        .bind(channel.channel_type as i32)
        .bind(&channel.key)
        .bind(channel.status as i32)
        .bind(&channel.name)
        .bind(channel.weight.map(|w| w as i32))
        .bind(channel.created_time)
        .bind(channel.test_time)
        .bind(channel.response_time as i32)
        .bind(&channel.base_url)
        .bind(channel.balance)
        .bind(channel.balance_updated_time)
        .bind(&channel.models)
        .bind(&channel.group)
        .bind(channel.used_quota)
        .bind(&channel.model_mapping)
        .bind(channel.priority)
        .bind(channel.auto_ban)
        .bind(&channel.tag)
        .bind(&channel.setting)
        .bind(&channel.param_override)
        .bind(&channel.header_override)
        .bind(&channel.remark)
        .bind(channel.proxy_id.map(|v| v as i64))
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    }
    delete_orphans_pg(&mut tx, "channels", "id", &channel_ids).await?;

    let mut mapping_keys = Vec::with_capacity(data.model_mappings.len());
    for mapping in &data.model_mappings {
        mapping_keys.push(mapping.requested_model.clone());
        sqlx::query(
            "INSERT INTO model_mappings (requested_model, channel_id, upstream_model)
             VALUES ($1, $2, $3)
             ON CONFLICT (requested_model) DO UPDATE SET
                channel_id = EXCLUDED.channel_id,
                upstream_model = EXCLUDED.upstream_model",
        )
        .bind(&mapping.requested_model)
        .bind(&mapping.channel_id)
        .bind(&mapping.upstream_model)
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    }
    delete_orphan_strings_pg(&mut tx, "model_mappings", "requested_model", &mapping_keys).await?;

    tx.commit().await.map_err(storage_err)
}

async fn delete_orphans_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    column: &str,
    keep_ids: &[i64],
) -> Result<(), ManagementError> {
    if keep_ids.is_empty() {
        sqlx::query(&format!("DELETE FROM {table}"))
            .execute(&mut **tx)
            .await
            .map_err(storage_err)?;
        return Ok(());
    }
    let placeholders = (1..=keep_ids.len())
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("DELETE FROM {table} WHERE {column} NOT IN ({placeholders})");
    let mut q = sqlx::query(&sql);
    for id in keep_ids {
        q = q.bind(*id);
    }
    q.execute(&mut **tx).await.map_err(storage_err)?;
    Ok(())
}

async fn delete_orphan_strings_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    column: &str,
    keep: &[String],
) -> Result<(), ManagementError> {
    if keep.is_empty() {
        sqlx::query(&format!("DELETE FROM {table}"))
            .execute(&mut **tx)
            .await
            .map_err(storage_err)?;
        return Ok(());
    }
    let placeholders = (1..=keep.len())
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("DELETE FROM {table} WHERE {column} NOT IN ({placeholders})");
    let mut q = sqlx::query(&sql);
    for key in keep {
        q = q.bind(key);
    }
    q.execute(&mut **tx).await.map_err(storage_err)?;
    Ok(())
}

fn pg_string_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<String, ManagementError> {
    row.try_get::<String, _>(name).map_err(storage_err)
}

fn pg_opt_string_col(
    row: &sqlx::postgres::PgRow,
    name: &str,
) -> Result<Option<String>, ManagementError> {
    row.try_get::<Option<String>, _>(name).map_err(storage_err)
}

fn pg_i64_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<i64, ManagementError> {
    // PG INTEGER is INT4; BIGINT is INT8. Accept both (and bool-as-0/1).
    if let Ok(v) = row.try_get::<i64, _>(name) {
        return Ok(v);
    }
    if let Ok(v) = row.try_get::<i32, _>(name) {
        return Ok(i64::from(v));
    }
    if let Ok(v) = row.try_get::<bool, _>(name) {
        return Ok(i64::from(v));
    }
    row.try_get::<i64, _>(name).map_err(storage_err)
}

fn pg_opt_i64_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<Option<i64>, ManagementError> {
    if let Ok(v) = row.try_get::<Option<i64>, _>(name) {
        return Ok(v);
    }
    if let Ok(v) = row.try_get::<Option<i32>, _>(name) {
        return Ok(v.map(i64::from));
    }
    if let Ok(v) = row.try_get::<Option<bool>, _>(name) {
        return Ok(v.map(i64::from));
    }
    row.try_get::<Option<i64>, _>(name).map_err(storage_err)
}

fn pg_u64_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<u64, ManagementError> {
    pg_i64_col(row, name).map(|value| value.max(0) as u64)
}

fn pg_i32_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<i32, ManagementError> {
    if let Ok(v) = row.try_get::<i32, _>(name) {
        return Ok(v);
    }
    if let Ok(v) = row.try_get::<i64, _>(name) {
        return Ok(v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32);
    }
    if let Ok(v) = row.try_get::<bool, _>(name) {
        return Ok(i32::from(v));
    }
    row.try_get::<i32, _>(name).map_err(storage_err)
}

fn pg_opt_i32_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<Option<i32>, ManagementError> {
    if let Ok(v) = row.try_get::<Option<i32>, _>(name) {
        return Ok(v);
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(name) {
        return Ok(v.map(|value| value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32));
    }
    if let Ok(v) = row.try_get::<Option<bool>, _>(name) {
        return Ok(v.map(i32::from));
    }
    row.try_get::<Option<i32>, _>(name).map_err(storage_err)
}

fn pg_opt_u32_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<Option<u32>, ManagementError> {
    // weight is INTEGER (INT4); do not force INT8 decode.
    if let Ok(v) = row.try_get::<Option<i32>, _>(name) {
        return Ok(v.map(|value| value.max(0) as u32));
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(name) {
        return Ok(v.map(|value| value.max(0) as u32));
    }
    row.try_get::<Option<i32>, _>(name)
        .map(|v| v.map(|value| value.max(0) as u32))
        .map_err(storage_err)
}

fn pg_opt_u64_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<Option<u64>, ManagementError> {
    if let Ok(v) = row.try_get::<Option<i64>, _>(name) {
        return Ok(v.map(|value| value.max(0) as u64));
    }
    if let Ok(v) = row.try_get::<Option<i32>, _>(name) {
        return Ok(v.map(|value| value.max(0) as u64));
    }
    row.try_get::<Option<i64>, _>(name)
        .map(|v| v.map(|value| value.max(0) as u64))
        .map_err(storage_err)
}

fn pg_f64_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<f64, ManagementError> {
    row.try_get::<f64, _>(name).map_err(storage_err)
}

fn pg_bool_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<bool, ManagementError> {
    if let Ok(v) = row.try_get::<bool, _>(name) {
        return Ok(v);
    }
    pg_i32_col(row, name).map(|value| value != 0)
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct OptionRecord {
    pub(crate) key:   String,
    pub(crate) value: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ListOptionsRequest;

#[derive(Debug, Clone)]
pub(crate) struct UpdateOptionRequest {
    pub(crate) key:   String,
    pub(crate) value: String,
}

#[derive(Debug, Clone)]
pub(crate) enum OptionStore {
    Memory(MemoryOptionStore),
    Sqlite(SqliteOptionStore),
    MySql(MySqlOptionStore),
    Postgres(PostgresOptionStore),
}

impl OptionStore {
    pub(crate) fn memory(defaults: BTreeMap<String, String>) -> Self {
        Self::Memory(MemoryOptionStore::new(defaults))
    }

    pub(crate) async fn sqlite(
        url: &str,
        defaults: BTreeMap<String, String>,
    ) -> Result<Self, ManagementError> {
        Ok(Self::Sqlite(
            SqliteOptionStore::connect(url, defaults).await?,
        ))
    }

    pub(crate) async fn mysql(
        url: &str,
        defaults: BTreeMap<String, String>,
    ) -> Result<Self, ManagementError> {
        Ok(Self::MySql(MySqlOptionStore::connect(url, defaults).await?))
    }

    pub(crate) async fn postgres(
        url: &str,
        defaults: BTreeMap<String, String>,
    ) -> Result<Self, ManagementError> {
        Ok(Self::Postgres(
            PostgresOptionStore::connect(url, defaults).await?,
        ))
    }

    pub(crate) fn values(&self) -> Result<BTreeMap<String, String>, ManagementError> {
        match self {
            Self::Memory(store) => store.values(),
            Self::Sqlite(store) => store.values(),
            Self::MySql(store) => store.values(),
            Self::Postgres(store) => store.values(),
        }
    }
}

crate::impl_backend_service!(OptionStore, {
    ListOptionsRequest => Vec<OptionRecord>,
    UpdateOptionRequest => OptionRecord,
});

#[derive(Debug, Clone)]
pub(crate) struct MemoryOptionStore {
    inner: Arc<RwLock<BTreeMap<String, String>>>,
}

impl MemoryOptionStore {
    fn new(options: BTreeMap<String, String>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(options)),
        }
    }

    fn values(&self) -> Result<BTreeMap<String, String>, ManagementError> {
        self.inner
            .read()
            .map(|options| options.clone())
            .map_err(|_| ManagementError::Poisoned("options"))
    }
}

impl Service<ListOptionsRequest> for MemoryOptionStore {
    type Response = Vec<OptionRecord>;
    type Error = ManagementError;

    async fn call(&self, _req: ListOptionsRequest) -> Result<Self::Response, Self::Error> {
        Ok(self
            .values()?
            .into_iter()
            .map(|(key, value)| OptionRecord { key, value })
            .collect())
    }
}

impl Service<UpdateOptionRequest> for MemoryOptionStore {
    type Response = OptionRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateOptionRequest) -> Result<Self::Response, Self::Error> {
        let key = req.key.trim();
        if key.is_empty() {
            return Err(ManagementError::InvalidRequest("option key is required"));
        }
        let key = key.to_string();
        self.inner
            .write()
            .map_err(|_| ManagementError::Poisoned("options"))?
            .insert(key.clone(), req.value.clone());
        Ok(OptionRecord {
            key,
            value: req.value,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SqliteOptionStore {
    pool:   SqlitePool,
    memory: MemoryOptionStore,
}

impl SqliteOptionStore {
    async fn connect(
        url: &str,
        defaults: BTreeMap<String, String>,
    ) -> Result<Self, ManagementError> {
        let options = SqliteConnectOptions::from_str(url)
            .map_err(storage_err)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_options(&pool).await?;
        seed_options(&pool, &defaults).await?;
        let mut merged = defaults;
        merged.extend(load_options(&pool).await?);
        Ok(Self {
            pool,
            memory: MemoryOptionStore::new(merged),
        })
    }

    fn values(&self) -> Result<BTreeMap<String, String>, ManagementError> {
        self.memory.values()
    }
}

impl Service<ListOptionsRequest> for SqliteOptionStore {
    type Response = Vec<OptionRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListOptionsRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<UpdateOptionRequest> for SqliteOptionStore {
    type Response = OptionRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateOptionRequest) -> Result<Self::Response, Self::Error> {
        let key = req.key.trim();
        if key.is_empty() {
            return Err(ManagementError::InvalidRequest("option key is required"));
        }
        sqlx::query(
            "INSERT INTO options (option_key, option_value) VALUES (?, ?)
             ON CONFLICT(option_key) DO UPDATE SET option_value = excluded.option_value",
        )
        .bind(key)
        .bind(&req.value)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        self.memory
            .call(UpdateOptionRequest {
                key:   key.to_string(),
                value: req.value,
            })
            .await
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MySqlOptionStore {
    pool:   MySqlPool,
    memory: MemoryOptionStore,
}

impl MySqlOptionStore {
    async fn connect(
        url: &str,
        defaults: BTreeMap<String, String>,
    ) -> Result<Self, ManagementError> {
        let options = MySqlConnectOptions::from_str(url).map_err(storage_err)?;
        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_options_mysql(&pool).await?;
        seed_options_mysql(&pool, &defaults).await?;
        let mut merged = defaults;
        merged.extend(load_options_mysql(&pool).await?);
        Ok(Self {
            pool,
            memory: MemoryOptionStore::new(merged),
        })
    }

    fn values(&self) -> Result<BTreeMap<String, String>, ManagementError> {
        self.memory.values()
    }
}

impl Service<ListOptionsRequest> for MySqlOptionStore {
    type Response = Vec<OptionRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListOptionsRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<UpdateOptionRequest> for MySqlOptionStore {
    type Response = OptionRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateOptionRequest) -> Result<Self::Response, Self::Error> {
        let key = req.key.trim();
        if key.is_empty() {
            return Err(ManagementError::InvalidRequest("option key is required"));
        }
        sqlx::query(
            "INSERT INTO options (option_key, option_value) VALUES (?, ?)
             ON DUPLICATE KEY UPDATE option_value = VALUES(option_value)",
        )
        .bind(key)
        .bind(&req.value)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        self.memory
            .call(UpdateOptionRequest {
                key:   key.to_string(),
                value: req.value,
            })
            .await
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresOptionStore {
    pool:   PgPool,
    memory: MemoryOptionStore,
}

impl PostgresOptionStore {
    async fn connect(
        url: &str,
        defaults: BTreeMap<String, String>,
    ) -> Result<Self, ManagementError> {
        let options = PgConnectOptions::from_str(url).map_err(storage_err)?;
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_options_pg(&pool).await?;
        seed_options_pg(&pool, &defaults).await?;
        let mut merged = defaults;
        merged.extend(load_options_pg(&pool).await?);
        Ok(Self {
            pool,
            memory: MemoryOptionStore::new(merged),
        })
    }

    fn values(&self) -> Result<BTreeMap<String, String>, ManagementError> {
        self.memory.values()
    }
}

impl Service<ListOptionsRequest> for PostgresOptionStore {
    type Response = Vec<OptionRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListOptionsRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<UpdateOptionRequest> for PostgresOptionStore {
    type Response = OptionRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateOptionRequest) -> Result<Self::Response, Self::Error> {
        let key = req.key.trim();
        if key.is_empty() {
            return Err(ManagementError::InvalidRequest("option key is required"));
        }
        sqlx::query(
            "INSERT INTO options (option_key, option_value) VALUES ($1, $2)
             ON CONFLICT(option_key) DO UPDATE SET option_value = EXCLUDED.option_value",
        )
        .bind(key)
        .bind(&req.value)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        self.memory
            .call(UpdateOptionRequest {
                key:   key.to_string(),
                value: req.value,
            })
            .await
    }
}

async fn migrate_options_pg(pool: &PgPool) -> Result<(), ManagementError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS options (
            option_key TEXT PRIMARY KEY,
            option_value TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(storage_err)?;
    Ok(())
}

async fn seed_options_pg(
    pool: &PgPool,
    defaults: &BTreeMap<String, String>,
) -> Result<(), ManagementError> {
    for (key, value) in defaults {
        sqlx::query(
            "INSERT INTO options (option_key, option_value) VALUES ($1, $2)
             ON CONFLICT(option_key) DO NOTHING",
        )
        .bind(key)
        .bind(value)
        .execute(pool)
        .await
        .map_err(storage_err)?;
    }
    Ok(())
}

async fn load_options_pg(pool: &PgPool) -> Result<BTreeMap<String, String>, ManagementError> {
    sqlx::query("SELECT option_key, option_value FROM options ORDER BY option_key")
        .fetch_all(pool)
        .await
        .map_err(storage_err)?
        .into_iter()
        .map(|row| {
            Ok((
                pg_string_col(&row, "option_key")?,
                pg_string_col(&row, "option_value")?,
            ))
        })
        .collect()
}

async fn migrate_options(pool: &SqlitePool) -> Result<(), ManagementError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS options (
            option_key TEXT PRIMARY KEY,
            option_value TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(storage_err)?;
    Ok(())
}

async fn migrate_options_mysql(pool: &MySqlPool) -> Result<(), ManagementError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS options (
            option_key TEXT PRIMARY KEY,
            option_value TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(storage_err)?;
    Ok(())
}

async fn seed_options(
    pool: &SqlitePool,
    defaults: &BTreeMap<String, String>,
) -> Result<(), ManagementError> {
    for (key, value) in defaults {
        sqlx::query("INSERT OR IGNORE INTO options (option_key, option_value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(pool)
            .await
            .map_err(storage_err)?;
    }
    Ok(())
}

async fn seed_options_mysql(
    pool: &MySqlPool,
    defaults: &BTreeMap<String, String>,
) -> Result<(), ManagementError> {
    for (key, value) in defaults {
        sqlx::query("INSERT IGNORE INTO options (option_key, option_value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(pool)
            .await
            .map_err(storage_err)?;
    }
    Ok(())
}

async fn load_options(pool: &SqlitePool) -> Result<BTreeMap<String, String>, ManagementError> {
    sqlx::query("SELECT option_key, option_value FROM options ORDER BY option_key")
        .fetch_all(pool)
        .await
        .map_err(storage_err)?
        .into_iter()
        .map(|row| {
            Ok((
                string_col(&row, "option_key")?,
                string_col(&row, "option_value")?,
            ))
        })
        .collect()
}

async fn load_options_mysql(pool: &MySqlPool) -> Result<BTreeMap<String, String>, ManagementError> {
    sqlx::query("SELECT option_key, option_value FROM options ORDER BY option_key")
        .fetch_all(pool)
        .await
        .map_err(storage_err)?
        .into_iter()
        .map(|row| {
            Ok((
                string_col_mysql(&row, "option_key")?,
                string_col_mysql(&row, "option_value")?,
            ))
        })
        .collect()
}

#[derive(Debug, Clone)]
pub(crate) enum UsageStore {
    Memory(MemoryUsageEventSink),
    Sqlite(SqliteUsageStore),
    MySql(MySqlUsageStore),
    Postgres(PostgresUsageStore),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeleteUsageBeforeRequest {
    pub(crate) target_timestamp: i64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeleteUsageAck {
    pub(crate) deleted: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct RecordedUsageBatch {
    pub(crate) ack:             UsageAck,
    pub(crate) accepted_events: Vec<UsageEvent>,
}

impl UsageStore {
    pub(crate) fn memory() -> Self {
        Self::Memory(MemoryUsageEventSink::default())
    }

    pub(crate) async fn sqlite(url: &str) -> Result<Self, UsageError> {
        Ok(Self::Sqlite(SqliteUsageStore::connect(url).await?))
    }

    pub(crate) async fn mysql(url: &str) -> Result<Self, UsageError> {
        Ok(Self::MySql(MySqlUsageStore::connect(url).await?))
    }

    pub(crate) async fn postgres(url: &str) -> Result<Self, UsageError> {
        Ok(Self::Postgres(PostgresUsageStore::connect(url).await?))
    }

    pub(crate) fn events(&self) -> Result<Vec<UsageEvent>, UsageError> {
        match self {
            Self::Memory(store) => store.events(),
            Self::Sqlite(store) => store.events(),
            Self::MySql(store) => store.events(),
            Self::Postgres(store) => store.events(),
        }
    }

    pub(crate) fn count_before_unix_seconds(
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

    pub(crate) async fn record_batch(
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

    pub(crate) async fn apply_quotas(&self, quotas: &[UsageEventQuota]) -> Result<(), UsageError> {
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
pub(crate) struct SqliteUsageStore {
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
pub(crate) struct MySqlUsageStore {
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
pub(crate) struct PostgresUsageStore {
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
