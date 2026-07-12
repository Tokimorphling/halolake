//! Options key/value store backends.
use super::management::{pg_string_col, storage_err, string_col, string_col_mysql};
use halolake_control_plane::ManagementError;
use serde::Serialize;
use service_async::Service;
use sqlx::{
    MySqlPool, PgPool, SqlitePool,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
    postgres::{PgConnectOptions, PgPoolOptions},
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{Arc, RwLock},
};

#[derive(Debug, Clone, Serialize)]
pub struct OptionRecord {
    pub key:   String,
    pub value: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ListOptionsRequest;

#[derive(Debug, Clone)]
pub struct UpdateOptionRequest {
    pub key:   String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub enum OptionStore {
    Memory(MemoryOptionStore),
    Sqlite(SqliteOptionStore),
    MySql(MySqlOptionStore),
    Postgres(PostgresOptionStore),
}

impl OptionStore {
    pub fn memory(defaults: BTreeMap<String, String>) -> Self {
        Self::Memory(MemoryOptionStore::new(defaults))
    }

    pub async fn sqlite(
        url: &str,
        defaults: BTreeMap<String, String>,
    ) -> Result<Self, ManagementError> {
        Ok(Self::Sqlite(
            SqliteOptionStore::connect(url, defaults).await?,
        ))
    }

    pub async fn mysql(
        url: &str,
        defaults: BTreeMap<String, String>,
    ) -> Result<Self, ManagementError> {
        Ok(Self::MySql(MySqlOptionStore::connect(url, defaults).await?))
    }

    pub async fn postgres(
        url: &str,
        defaults: BTreeMap<String, String>,
    ) -> Result<Self, ManagementError> {
        Ok(Self::Postgres(
            PostgresOptionStore::connect(url, defaults).await?,
        ))
    }

    pub fn values(&self) -> Result<BTreeMap<String, String>, ManagementError> {
        match self {
            Self::Memory(store) => store.values(),
            Self::Sqlite(store) => store.values(),
            Self::MySql(store) => store.values(),
            Self::Postgres(store) => store.values(),
        }
    }
}

crate::impl_storage_backend_service!(OptionStore, {
    ListOptionsRequest => Vec<OptionRecord>,
    UpdateOptionRequest => OptionRecord,
});

#[derive(Debug, Clone)]
pub struct MemoryOptionStore {
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
pub struct SqliteOptionStore {
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
pub struct MySqlOptionStore {
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
pub struct PostgresOptionStore {
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
