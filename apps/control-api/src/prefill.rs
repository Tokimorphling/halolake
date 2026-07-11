//! Prefill groups (`/api/prefill_group`) with memory + SQLite + Postgres storage.

use std::{
    str::FromStr,
    sync::{Arc, RwLock},
};

use halolake_control_plane::ManagementError;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use service_async::Service;
use sqlx::{
    MySqlPool, PgPool, Row, SqlitePool,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
    postgres::{PgConnectOptions, PgPoolOptions},
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct PrefillGroup {
    #[serde(default)]
    pub(crate) id: u64,
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) group_type: String,
    #[serde(default)]
    pub(crate) items: JsonValue,
    #[serde(default)]
    pub(crate) description: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ListPrefillGroupsRequest {
    pub(crate) group_type: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CreatePrefillGroupRequest {
    pub(crate) group: PrefillGroup,
}

#[derive(Debug, Clone)]
pub(crate) struct UpdatePrefillGroupRequest {
    pub(crate) group: PrefillGroup,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeletePrefillGroupRequest {
    pub(crate) id: u64,
}

#[derive(Debug, Clone)]
pub(crate) enum PrefillStore {
    Memory(MemoryPrefillStore),
    Sqlite(SqlitePrefillStore),
    MySql(MySqlPrefillStore),
    Postgres(PostgresPrefillStore),
}

impl PrefillStore {
    pub(crate) fn memory() -> Self {
        Self::Memory(MemoryPrefillStore::default())
    }

    pub(crate) async fn sqlite(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Sqlite(SqlitePrefillStore::connect(url).await?))
    }

    pub(crate) async fn mysql(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::MySql(MySqlPrefillStore::connect(url).await?))
    }

    pub(crate) async fn postgres(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Postgres(PostgresPrefillStore::connect(url).await?))
    }
}

impl Service<ListPrefillGroupsRequest> for PrefillStore {
    type Response = Vec<PrefillGroup>;
    type Error = ManagementError;

    async fn call(&self, req: ListPrefillGroupsRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl Service<CreatePrefillGroupRequest> for PrefillStore {
    type Response = PrefillGroup;
    type Error = ManagementError;

    async fn call(&self, req: CreatePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl Service<UpdatePrefillGroupRequest> for PrefillStore {
    type Response = PrefillGroup;
    type Error = ManagementError;

    async fn call(&self, req: UpdatePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl Service<DeletePrefillGroupRequest> for PrefillStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeletePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MemoryPrefillStore {
    inner: Arc<RwLock<Vec<PrefillGroup>>>,
}

impl MemoryPrefillStore {
    fn list(&self, group_type: &str) -> Result<Vec<PrefillGroup>, ManagementError> {
        let groups = self
            .inner
            .read()
            .map_err(|_| ManagementError::Poisoned("prefill"))?
            .clone();
        if group_type.is_empty() {
            return Ok(groups);
        }
        Ok(groups
            .into_iter()
            .filter(|group| group.group_type == group_type)
            .collect())
    }

    fn create(&self, mut group: PrefillGroup) -> Result<PrefillGroup, ManagementError> {
        normalize_group(&mut group)?;
        let mut groups = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("prefill"))?;
        if group.id == 0 {
            group.id = groups
                .iter()
                .map(|g| g.id)
                .max()
                .unwrap_or(0)
                .saturating_add(1)
                .max(1);
        } else if groups.iter().any(|g| g.id == group.id) {
            return Err(ManagementError::Duplicate);
        }
        groups.push(group.clone());
        Ok(group)
    }

    fn update(&self, group: PrefillGroup) -> Result<PrefillGroup, ManagementError> {
        if group.id == 0 {
            return Err(ManagementError::InvalidRequest("id is required"));
        }
        let mut groups = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("prefill"))?;
        let current = groups
            .iter_mut()
            .find(|item| item.id == group.id)
            .ok_or(ManagementError::NotFound)?;
        if !group.name.trim().is_empty() {
            current.name = group.name.trim().to_string();
        }
        if !group.group_type.trim().is_empty() {
            current.group_type = group.group_type.trim().to_string();
        }
        if !group.items.is_null() {
            current.items = group.items;
        }
        if !group.description.is_empty() {
            current.description = group.description;
        }
        Ok(current.clone())
    }

    fn delete(&self, id: u64) -> Result<(), ManagementError> {
        let mut groups = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("prefill"))?;
        let before = groups.len();
        groups.retain(|group| group.id != id);
        if before == groups.len() {
            return Err(ManagementError::NotFound);
        }
        Ok(())
    }

    fn replace_all(&self, groups: Vec<PrefillGroup>) -> Result<(), ManagementError> {
        *self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("prefill"))? = groups;
        Ok(())
    }

    fn all(&self) -> Result<Vec<PrefillGroup>, ManagementError> {
        self.inner
            .read()
            .map(|g| g.clone())
            .map_err(|_| ManagementError::Poisoned("prefill"))
    }
}

impl Service<ListPrefillGroupsRequest> for MemoryPrefillStore {
    type Response = Vec<PrefillGroup>;
    type Error = ManagementError;

    async fn call(&self, req: ListPrefillGroupsRequest) -> Result<Self::Response, Self::Error> {
        self.list(&req.group_type)
    }
}

impl Service<CreatePrefillGroupRequest> for MemoryPrefillStore {
    type Response = PrefillGroup;
    type Error = ManagementError;

    async fn call(&self, req: CreatePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        self.create(req.group)
    }
}

impl Service<UpdatePrefillGroupRequest> for MemoryPrefillStore {
    type Response = PrefillGroup;
    type Error = ManagementError;

    async fn call(&self, req: UpdatePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        self.update(req.group)
    }
}

impl Service<DeletePrefillGroupRequest> for MemoryPrefillStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeletePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        self.delete(req.id)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SqlitePrefillStore {
    pool: SqlitePool,
    memory: MemoryPrefillStore,
}

impl SqlitePrefillStore {
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
        migrate_sqlite(&pool).await?;
        let memory = MemoryPrefillStore::default();
        memory.replace_all(load_sqlite(&pool).await?)?;
        Ok(Self { pool, memory })
    }

    async fn persist(&self) -> Result<(), ManagementError> {
        save_sqlite(&self.pool, &self.memory.all()?).await
    }
}

impl Service<ListPrefillGroupsRequest> for SqlitePrefillStore {
    type Response = Vec<PrefillGroup>;
    type Error = ManagementError;

    async fn call(&self, req: ListPrefillGroupsRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<CreatePrefillGroupRequest> for SqlitePrefillStore {
    type Response = PrefillGroup;
    type Error = ManagementError;

    async fn call(&self, req: CreatePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        let group = self.memory.call(req).await?;
        self.persist().await?;
        Ok(group)
    }
}

impl Service<UpdatePrefillGroupRequest> for SqlitePrefillStore {
    type Response = PrefillGroup;
    type Error = ManagementError;

    async fn call(&self, req: UpdatePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        let group = self.memory.call(req).await?;
        self.persist().await?;
        Ok(group)
    }
}

impl Service<DeletePrefillGroupRequest> for SqlitePrefillStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeletePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await?;
        self.persist().await
    }
}

#[derive(Debug, Clone)]

pub(crate) struct MySqlPrefillStore {
    pool: MySqlPool,
    memory: MemoryPrefillStore,
}

impl MySqlPrefillStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = MySqlConnectOptions::from_str(url)
            .map_err(storage_err)?;
        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_mysql(&pool).await?;
        let memory = MemoryPrefillStore::default();
        memory.replace_all(load_mysql(&pool).await?)?;
        Ok(Self { pool, memory })
    }

    async fn persist(&self) -> Result<(), ManagementError> {
        save_mysql(&self.pool, &self.memory.all()?).await
    }
}

impl Service<ListPrefillGroupsRequest> for MySqlPrefillStore {
    type Response = Vec<PrefillGroup>;
    type Error = ManagementError;

    async fn call(&self, req: ListPrefillGroupsRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<CreatePrefillGroupRequest> for MySqlPrefillStore {
    type Response = PrefillGroup;
    type Error = ManagementError;

    async fn call(&self, req: CreatePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        let group = self.memory.call(req).await?;
        self.persist().await?;
        Ok(group)
    }
}

impl Service<UpdatePrefillGroupRequest> for MySqlPrefillStore {
    type Response = PrefillGroup;
    type Error = ManagementError;

    async fn call(&self, req: UpdatePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        let group = self.memory.call(req).await?;
        self.persist().await?;
        Ok(group)
    }
}

impl Service<DeletePrefillGroupRequest> for MySqlPrefillStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeletePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await?;
        self.persist().await
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresPrefillStore {
    pool: PgPool,
    memory: MemoryPrefillStore,
}

impl PostgresPrefillStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = PgConnectOptions::from_str(url).map_err(storage_err)?;
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_pg(&pool).await?;
        let memory = MemoryPrefillStore::default();
        memory.replace_all(load_pg(&pool).await?)?;
        Ok(Self { pool, memory })
    }

    async fn persist(&self) -> Result<(), ManagementError> {
        save_pg(&self.pool, &self.memory.all()?).await
    }
}

impl Service<ListPrefillGroupsRequest> for PostgresPrefillStore {
    type Response = Vec<PrefillGroup>;
    type Error = ManagementError;

    async fn call(&self, req: ListPrefillGroupsRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<CreatePrefillGroupRequest> for PostgresPrefillStore {
    type Response = PrefillGroup;
    type Error = ManagementError;

    async fn call(&self, req: CreatePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        let group = self.memory.call(req).await?;
        self.persist().await?;
        Ok(group)
    }
}

impl Service<UpdatePrefillGroupRequest> for PostgresPrefillStore {
    type Response = PrefillGroup;
    type Error = ManagementError;

    async fn call(&self, req: UpdatePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        let group = self.memory.call(req).await?;
        self.persist().await?;
        Ok(group)
    }
}

impl Service<DeletePrefillGroupRequest> for PostgresPrefillStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeletePrefillGroupRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await?;
        self.persist().await
    }
}

fn normalize_group(group: &mut PrefillGroup) -> Result<(), ManagementError> {
    group.name = group.name.trim().to_string();
    group.group_type = group.group_type.trim().to_string();
    if group.name.is_empty() || group.group_type.is_empty() {
        return Err(ManagementError::InvalidRequest(
            "name and type are required",
        ));
    }
    Ok(())
}

async fn migrate_sqlite(pool: &SqlitePool) -> Result<(), ManagementError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS prefill_groups (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            group_type TEXT NOT NULL,
            items TEXT NOT NULL DEFAULT 'null',
            description TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .map_err(storage_err)?;
    Ok(())
}

async fn migrate_mysql(pool: &MySqlPool) -> Result<(), ManagementError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS prefill_groups (
            id BIGINT PRIMARY KEY,
            name TEXT NOT NULL,
            group_type TEXT NOT NULL,
            items TEXT NOT NULL DEFAULT 'null',
            description TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .map_err(storage_err)?;
    Ok(())
}

async fn load_sqlite(pool: &SqlitePool) -> Result<Vec<PrefillGroup>, ManagementError> {
    sqlx::query(
        "SELECT id, name, group_type, items, description FROM prefill_groups ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        let items: String = row.try_get("items").map_err(storage_err)?;
        Ok(PrefillGroup {
            id: row.try_get::<i64, _>("id").map_err(storage_err)?.max(0) as u64,
            name: row.try_get("name").map_err(storage_err)?,
            group_type: row.try_get("group_type").map_err(storage_err)?,
            items: serde_json::from_str(&items).unwrap_or(JsonValue::Null),
            description: row.try_get("description").map_err(storage_err)?,
        })
    })
    .collect()
}

async fn load_mysql(pool: &MySqlPool) -> Result<Vec<PrefillGroup>, ManagementError> {
    sqlx::query(
        "SELECT id, name, group_type, items, description FROM prefill_groups ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        let items: String = row.try_get("items").map_err(storage_err)?;
        Ok(PrefillGroup {
            id: row.try_get::<i64, _>("id").map_err(storage_err)?.max(0) as u64,
            name: row.try_get("name").map_err(storage_err)?,
            group_type: row.try_get("group_type").map_err(storage_err)?,
            items: serde_json::from_str(&items).unwrap_or(JsonValue::Null),
            description: row.try_get("description").map_err(storage_err)?,
        })
    })
    .collect()
}

async fn save_sqlite(pool: &SqlitePool, groups: &[PrefillGroup]) -> Result<(), ManagementError> {
    let mut tx = pool.begin().await.map_err(storage_err)?;
    sqlx::query("DELETE FROM prefill_groups")
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    for group in groups {
        sqlx::query(
            "INSERT INTO prefill_groups (id, name, group_type, items, description)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(group.id as i64)
        .bind(&group.name)
        .bind(&group.group_type)
        .bind(group.items.to_string())
        .bind(&group.description)
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    }
    tx.commit().await.map_err(storage_err)
}

async fn save_mysql(pool: &MySqlPool, groups: &[PrefillGroup]) -> Result<(), ManagementError> {
    let mut tx = pool.begin().await.map_err(storage_err)?;
    sqlx::query("DELETE FROM prefill_groups")
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    for group in groups {
        sqlx::query(
            "INSERT INTO prefill_groups (id, name, group_type, items, description)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(group.id as i64)
        .bind(&group.name)
        .bind(&group.group_type)
        .bind(group.items.to_string())
        .bind(&group.description)
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    }
    tx.commit().await.map_err(storage_err)
}

async fn migrate_pg(pool: &PgPool) -> Result<(), ManagementError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS prefill_groups (
            id BIGINT PRIMARY KEY,
            name TEXT NOT NULL,
            group_type TEXT NOT NULL,
            items TEXT NOT NULL DEFAULT 'null',
            description TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .map_err(storage_err)?;
    Ok(())
}

async fn load_pg(pool: &PgPool) -> Result<Vec<PrefillGroup>, ManagementError> {
    sqlx::query(
        "SELECT id, name, group_type, items, description FROM prefill_groups ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        let items: String = row.try_get("items").map_err(storage_err)?;
        Ok(PrefillGroup {
            id: row.try_get::<i64, _>("id").map_err(storage_err)?.max(0) as u64,
            name: row.try_get("name").map_err(storage_err)?,
            group_type: row.try_get("group_type").map_err(storage_err)?,
            items: serde_json::from_str(&items).unwrap_or(JsonValue::Null),
            description: row.try_get("description").map_err(storage_err)?,
        })
    })
    .collect()
}

async fn save_pg(pool: &PgPool, groups: &[PrefillGroup]) -> Result<(), ManagementError> {
    let mut tx = pool.begin().await.map_err(storage_err)?;
    sqlx::query("DELETE FROM prefill_groups")
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    for group in groups {
        sqlx::query(
            "INSERT INTO prefill_groups (id, name, group_type, items, description)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(group.id as i64)
        .bind(&group.name)
        .bind(&group.group_type)
        .bind(group.items.to_string())
        .bind(&group.description)
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    }
    tx.commit().await.map_err(storage_err)
}

fn storage_err(err: impl std::fmt::Display) -> ManagementError {
    ManagementError::Storage(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn memory_prefill_crud() {
        block_on(async {
            let store = PrefillStore::memory();
            let created = store
                .call(CreatePrefillGroupRequest {
                    group: PrefillGroup {
                        id: 0,
                        name: "prompts".into(),
                        group_type: "prompt".into(),
                        items: serde_json::json!(["hi"]),
                        description: "d".into(),
                    },
                })
                .await
                .expect("create");
            assert_eq!(created.id, 1);
            let listed = store
                .call(ListPrefillGroupsRequest {
                    group_type: "prompt".into(),
                })
                .await
                .expect("list");
            assert_eq!(listed.len(), 1);
            store
                .call(DeletePrefillGroupRequest { id: created.id })
                .await
                .expect("delete");
            let listed = store
                .call(ListPrefillGroupsRequest {
                    group_type: String::new(),
                })
                .await
                .expect("list empty");
            assert!(listed.is_empty());
        });
    }
}
