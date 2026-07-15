use halolake_control_plane::{
    AdjustUserQuotaRequest, AutoDisableChannelRequest, AutoDisableChannelResult,
    BatchSetChannelTagRequest, BatchUpdateChannelStatusRequest, BootstrapRootUserRequest,
    ChannelStatusUpdateRequest, CreateChannelRequest, CreateTokenRequest, CreateUserRequest,
    DeleteChannelRequest, DeleteChannelsBatchRequest, DeleteDisabledChannelsRequest,
    DeleteTokenRequest, DeleteUserRequest, GetChannelRequest, GetTokenRequest, GetUserRequest,
    ListChannelsRequest, ListTokensRequest, ListUsersRequest, LoginUserRequest, ManageUserRequest,
    ManagementData, ManagementError, MemoryManagementStore, PatchChannelBalanceRequest,
    PatchChannelModelStateRequest, PatchChannelProbeMetricsRequest,
    PublishManagementSnapshotRequest, RegisterUserRequest, RegisteredUser, RevealChannelKeyRequest,
    RevealTokenKeyRequest, RevealedChannelKey, RevealedTokenKey, RotateChannelCredentialRequest,
    SearchChannelsRequest, SearchTokensRequest, SearchUsersRequest, SettleUsageRequest,
    SnapshotPublished, SnapshotPublisher, UpdateChannelRequest, UpdateChannelsByTagRequest,
    UpdateTokenRequest, UpdateUserAccessTokenRequest, UpdateUserRequest, UsageSettlement,
    ValidateUserAccessTokenRequest,
};
use halolake_domain::{ChannelRecord, PageResult, TokenRecord, UserRecord};
use halolake_router_core::ModelMapping;
use service_async::Service;
use sqlx::{
    MySqlPool, PgPool, Row, SqlitePool, Transaction,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
    postgres::{PgConnectOptions, PgPoolOptions},
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::str::FromStr;

#[derive(Debug, Clone)]
pub enum ManagementStore {
    Memory(MemoryManagementStore),
    Sqlite(SqliteManagementStore),
    MySql(MySqlManagementStore),
    Postgres(PostgresManagementStore),
}

impl ManagementStore {
    pub fn memory(data: ManagementData) -> Self {
        Self::Memory(MemoryManagementStore::new(data))
    }

    pub async fn sqlite(url: &str, seed: ManagementData) -> Result<Self, ManagementError> {
        Ok(Self::Sqlite(
            SqliteManagementStore::connect(url, seed).await?,
        ))
    }

    pub async fn mysql(url: &str, seed: ManagementData) -> Result<Self, ManagementError> {
        Ok(Self::MySql(MySqlManagementStore::connect(url, seed).await?))
    }

    pub async fn postgres(url: &str, seed: ManagementData) -> Result<Self, ManagementError> {
        Ok(Self::Postgres(
            PostgresManagementStore::connect(url, seed).await?,
        ))
    }

    pub fn current_data(&self) -> Result<ManagementData, ManagementError> {
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
    pub async fn bump_version(&self) -> Result<u64, ManagementError> {
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
pub struct SqliteManagementStore {
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
impl_write_service!(DeleteChannelsBatchRequest, usize);
impl_write_service!(ChannelStatusUpdateRequest, bool);
impl_write_service!(BatchUpdateChannelStatusRequest, usize);
impl_write_service!(PatchChannelProbeMetricsRequest, ChannelRecord);
impl_write_service!(PatchChannelBalanceRequest, ChannelRecord);
impl_write_service!(PatchChannelModelStateRequest, ChannelRecord);
impl_write_service!(RotateChannelCredentialRequest, ChannelRecord);

impl Service<AutoDisableChannelRequest> for ManagementStore {
    type Response = AutoDisableChannelResult;
    type Error = ManagementError;

    async fn call(&self, req: AutoDisableChannelRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => {
                let resp = store.memory.call(req).await?;
                store.persist().await?;
                Ok(resp)
            }
            Self::MySql(store) => {
                let resp = store.memory.call(req).await?;
                store.persist().await?;
                Ok(resp)
            }
            Self::Postgres(store) => {
                let resp = store.memory.call(req).await?;
                store.persist().await?;
                Ok(resp)
            }
        }
    }
}

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
pub struct MySqlManagementStore {
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
pub struct PostgresManagementStore {
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

pub fn string_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<String, ManagementError> {
    row.try_get::<String, _>(name).map_err(storage_err)
}

pub fn string_col_mysql(
    row: &sqlx::mysql::MySqlRow,
    name: &str,
) -> Result<String, ManagementError> {
    row.try_get::<String, _>(name).map_err(storage_err)
}

pub fn opt_string_col(
    row: &sqlx::sqlite::SqliteRow,
    name: &str,
) -> Result<Option<String>, ManagementError> {
    row.try_get::<Option<String>, _>(name).map_err(storage_err)
}

pub fn opt_string_col_mysql(
    row: &sqlx::mysql::MySqlRow,
    name: &str,
) -> Result<Option<String>, ManagementError> {
    row.try_get::<Option<String>, _>(name).map_err(storage_err)
}

pub fn i64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<i64, ManagementError> {
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

pub fn u64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<u64, ManagementError> {
    i64_col(row, name).map(|value| value.max(0) as u64)
}

fn u64_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<u64, ManagementError> {
    i64_col_mysql(row, name).map(|value| value.max(0) as u64)
}

pub fn i32_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<i32, ManagementError> {
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

pub fn f64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<f64, ManagementError> {
    row.try_get::<f64, _>(name).map_err(storage_err)
}

fn f64_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<f64, ManagementError> {
    row.try_get::<f64, _>(name).map_err(storage_err)
}

pub fn bool_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<bool, ManagementError> {
    i64_col(row, name).map(|value| value != 0)
}

fn bool_col_mysql(row: &sqlx::mysql::MySqlRow, name: &str) -> Result<bool, ManagementError> {
    i64_col_mysql(row, name).map(|value| value != 0)
}

fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

pub fn storage_err(err: impl std::fmt::Display) -> ManagementError {
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

pub fn pg_string_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<String, ManagementError> {
    row.try_get::<String, _>(name).map_err(storage_err)
}

pub fn pg_opt_string_col(
    row: &sqlx::postgres::PgRow,
    name: &str,
) -> Result<Option<String>, ManagementError> {
    row.try_get::<Option<String>, _>(name).map_err(storage_err)
}

pub fn pg_i64_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<i64, ManagementError> {
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

pub fn pg_i32_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<i32, ManagementError> {
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

pub fn pg_opt_u32_col(
    row: &sqlx::postgres::PgRow,
    name: &str,
) -> Result<Option<u32>, ManagementError> {
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

pub fn pg_opt_u64_col(
    row: &sqlx::postgres::PgRow,
    name: &str,
) -> Result<Option<u64>, ManagementError> {
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

pub fn pg_f64_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<f64, ManagementError> {
    row.try_get::<f64, _>(name).map_err(storage_err)
}

pub fn pg_bool_col(row: &sqlx::postgres::PgRow, name: &str) -> Result<bool, ManagementError> {
    if let Ok(v) = row.try_get::<bool, _>(name) {
        return Ok(v);
    }
    pg_i32_col(row, name).map(|value| value != 0)
}
