//! Upstream proxy pool (`/api/proxy`) with memory + SQLite + MySQL + Postgres storage.

use halolake_control_plane::ManagementError;
use serde::{Deserialize, Serialize};
use service_async::Service;
use sqlx::{
    MySqlPool, PgPool, Row, SqlitePool,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
    postgres::{PgConnectOptions, PgPoolOptions},
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    str::FromStr,
    sync::{Arc, RwLock},
};

const PROXY_STATUS_ENABLED: i32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ProxyRecord {
    #[serde(default)]
    pub(crate) id:     u64,
    pub(crate) name:   String,
    pub(crate) url:    String,
    #[serde(default = "default_proxy_status")]
    pub(crate) status: i32,
    #[serde(default)]
    pub(crate) remark: String,
}

fn default_proxy_status() -> i32 {
    PROXY_STATUS_ENABLED
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ListProxiesRequest;

#[derive(Debug, Clone)]
pub(crate) struct CreateProxyRequest {
    pub(crate) proxy: ProxyRecord,
}

#[derive(Debug, Clone)]
pub(crate) struct UpdateProxyRequest {
    pub(crate) proxy: ProxyRecord,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeleteProxyRequest {
    pub(crate) id: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GetProxyRequest {
    pub(crate) id: u64,
}

#[derive(Debug, Clone)]
pub(crate) enum ProxyStore {
    Memory(MemoryProxyStore),
    Sqlite(SqliteProxyStore),
    MySql(MySqlProxyStore),
    Postgres(PostgresProxyStore),
}

impl ProxyStore {
    pub(crate) fn memory() -> Self {
        Self::Memory(MemoryProxyStore::default())
    }

    pub(crate) async fn sqlite(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Sqlite(SqliteProxyStore::connect(url).await?))
    }

    pub(crate) async fn mysql(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::MySql(MySqlProxyStore::connect(url).await?))
    }

    pub(crate) async fn postgres(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Postgres(PostgresProxyStore::connect(url).await?))
    }

    pub(crate) fn resolve_url(&self, proxy_id: Option<u64>) -> Option<String> {
        let id = proxy_id?;
        let proxies = match self {
            Self::Memory(store) => store.all().ok()?,
            Self::Sqlite(store) => store.memory.all().ok()?,
            Self::MySql(store) => store.memory.all().ok()?,
            Self::Postgres(store) => store.memory.all().ok()?,
        };
        proxies
            .into_iter()
            .find(|proxy| proxy.id == id && proxy.status == PROXY_STATUS_ENABLED)
            .map(|proxy| proxy.url)
    }
}

crate::impl_backend_service!(ProxyStore, {
    ListProxiesRequest => Vec<ProxyRecord>,
    CreateProxyRequest => ProxyRecord,
    UpdateProxyRequest => ProxyRecord,
    DeleteProxyRequest => (),
    GetProxyRequest => ProxyRecord,
});

#[derive(Debug, Clone, Default)]
pub(crate) struct MemoryProxyStore {
    inner: Arc<RwLock<Vec<ProxyRecord>>>,
}

impl MemoryProxyStore {
    fn list(&self) -> Result<Vec<ProxyRecord>, ManagementError> {
        self.inner
            .read()
            .map(|proxies| proxies.clone())
            .map_err(|_| ManagementError::Poisoned("proxy"))
    }

    fn get(&self, id: u64) -> Result<ProxyRecord, ManagementError> {
        self.inner
            .read()
            .map_err(|_| ManagementError::Poisoned("proxy"))?
            .iter()
            .find(|proxy| proxy.id == id)
            .cloned()
            .ok_or(ManagementError::NotFound)
    }

    fn create(&self, mut proxy: ProxyRecord) -> Result<ProxyRecord, ManagementError> {
        normalize_proxy(&mut proxy)?;
        let mut proxies = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("proxy"))?;
        if proxy.id == 0 {
            proxy.id = proxies
                .iter()
                .map(|p| p.id)
                .max()
                .unwrap_or(0)
                .saturating_add(1)
                .max(1);
        } else if proxies.iter().any(|p| p.id == proxy.id) {
            return Err(ManagementError::Duplicate);
        }
        proxies.push(proxy.clone());
        Ok(proxy)
    }

    fn update(&self, mut proxy: ProxyRecord) -> Result<ProxyRecord, ManagementError> {
        if proxy.id == 0 {
            return Err(ManagementError::InvalidRequest("id is required"));
        }
        normalize_proxy(&mut proxy)?;
        let mut proxies = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("proxy"))?;
        let current = proxies
            .iter_mut()
            .find(|item| item.id == proxy.id)
            .ok_or(ManagementError::NotFound)?;
        *current = proxy.clone();
        Ok(proxy)
    }

    fn delete(&self, id: u64) -> Result<(), ManagementError> {
        let mut proxies = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("proxy"))?;
        let before = proxies.len();
        proxies.retain(|proxy| proxy.id != id);
        if before == proxies.len() {
            return Err(ManagementError::NotFound);
        }
        Ok(())
    }

    fn replace_all(&self, proxies: Vec<ProxyRecord>) -> Result<(), ManagementError> {
        *self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("proxy"))? = proxies;
        Ok(())
    }

    fn all(&self) -> Result<Vec<ProxyRecord>, ManagementError> {
        self.list()
    }
}

impl Service<ListProxiesRequest> for MemoryProxyStore {
    type Response = Vec<ProxyRecord>;
    type Error = ManagementError;

    async fn call(&self, _req: ListProxiesRequest) -> Result<Self::Response, Self::Error> {
        self.list()
    }
}

impl Service<CreateProxyRequest> for MemoryProxyStore {
    type Response = ProxyRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateProxyRequest) -> Result<Self::Response, Self::Error> {
        self.create(req.proxy)
    }
}

impl Service<UpdateProxyRequest> for MemoryProxyStore {
    type Response = ProxyRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateProxyRequest) -> Result<Self::Response, Self::Error> {
        self.update(req.proxy)
    }
}

impl Service<DeleteProxyRequest> for MemoryProxyStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeleteProxyRequest) -> Result<Self::Response, Self::Error> {
        self.delete(req.id)
    }
}

impl Service<GetProxyRequest> for MemoryProxyStore {
    type Response = ProxyRecord;
    type Error = ManagementError;

    async fn call(&self, req: GetProxyRequest) -> Result<Self::Response, Self::Error> {
        self.get(req.id)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SqliteProxyStore {
    pool:   SqlitePool,
    memory: MemoryProxyStore,
}

impl SqliteProxyStore {
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
        let memory = MemoryProxyStore::default();
        memory.replace_all(load_sqlite(&pool).await?)?;
        Ok(Self { pool, memory })
    }

    async fn persist(&self) -> Result<(), ManagementError> {
        save_sqlite(&self.pool, &self.memory.all()?).await
    }
}

impl Service<ListProxiesRequest> for SqliteProxyStore {
    type Response = Vec<ProxyRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListProxiesRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<CreateProxyRequest> for SqliteProxyStore {
    type Response = ProxyRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateProxyRequest) -> Result<Self::Response, Self::Error> {
        let proxy = self.memory.call(req).await?;
        self.persist().await?;
        Ok(proxy)
    }
}

impl Service<UpdateProxyRequest> for SqliteProxyStore {
    type Response = ProxyRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateProxyRequest) -> Result<Self::Response, Self::Error> {
        let proxy = self.memory.call(req).await?;
        self.persist().await?;
        Ok(proxy)
    }
}

impl Service<DeleteProxyRequest> for SqliteProxyStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeleteProxyRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await?;
        self.persist().await
    }
}

impl Service<GetProxyRequest> for SqliteProxyStore {
    type Response = ProxyRecord;
    type Error = ManagementError;

    async fn call(&self, req: GetProxyRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MySqlProxyStore {
    pool:   MySqlPool,
    memory: MemoryProxyStore,
}

impl MySqlProxyStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = MySqlConnectOptions::from_str(url).map_err(storage_err)?;
        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_mysql(&pool).await?;
        let memory = MemoryProxyStore::default();
        memory.replace_all(load_mysql(&pool).await?)?;
        Ok(Self { pool, memory })
    }

    async fn persist(&self) -> Result<(), ManagementError> {
        save_mysql(&self.pool, &self.memory.all()?).await
    }
}

impl Service<ListProxiesRequest> for MySqlProxyStore {
    type Response = Vec<ProxyRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListProxiesRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<CreateProxyRequest> for MySqlProxyStore {
    type Response = ProxyRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateProxyRequest) -> Result<Self::Response, Self::Error> {
        let proxy = self.memory.call(req).await?;
        self.persist().await?;
        Ok(proxy)
    }
}

impl Service<UpdateProxyRequest> for MySqlProxyStore {
    type Response = ProxyRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateProxyRequest) -> Result<Self::Response, Self::Error> {
        let proxy = self.memory.call(req).await?;
        self.persist().await?;
        Ok(proxy)
    }
}

impl Service<DeleteProxyRequest> for MySqlProxyStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeleteProxyRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await?;
        self.persist().await
    }
}

impl Service<GetProxyRequest> for MySqlProxyStore {
    type Response = ProxyRecord;
    type Error = ManagementError;

    async fn call(&self, req: GetProxyRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresProxyStore {
    pool:   PgPool,
    memory: MemoryProxyStore,
}

impl PostgresProxyStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = PgConnectOptions::from_str(url).map_err(storage_err)?;
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_pg(&pool).await?;
        let memory = MemoryProxyStore::default();
        memory.replace_all(load_pg(&pool).await?)?;
        Ok(Self { pool, memory })
    }

    async fn persist(&self) -> Result<(), ManagementError> {
        save_pg(&self.pool, &self.memory.all()?).await
    }
}

impl Service<ListProxiesRequest> for PostgresProxyStore {
    type Response = Vec<ProxyRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListProxiesRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<CreateProxyRequest> for PostgresProxyStore {
    type Response = ProxyRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateProxyRequest) -> Result<Self::Response, Self::Error> {
        let proxy = self.memory.call(req).await?;
        self.persist().await?;
        Ok(proxy)
    }
}

impl Service<UpdateProxyRequest> for PostgresProxyStore {
    type Response = ProxyRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateProxyRequest) -> Result<Self::Response, Self::Error> {
        let proxy = self.memory.call(req).await?;
        self.persist().await?;
        Ok(proxy)
    }
}

impl Service<DeleteProxyRequest> for PostgresProxyStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeleteProxyRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await?;
        self.persist().await
    }
}

impl Service<GetProxyRequest> for PostgresProxyStore {
    type Response = ProxyRecord;
    type Error = ManagementError;

    async fn call(&self, req: GetProxyRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

fn normalize_proxy(proxy: &mut ProxyRecord) -> Result<(), ManagementError> {
    proxy.name = proxy.name.trim().to_string();
    proxy.url = proxy.url.trim().to_string();
    proxy.remark = proxy.remark.trim().to_string();
    if proxy.name.is_empty() || proxy.url.is_empty() {
        return Err(ManagementError::InvalidRequest("name and url are required"));
    }
    proxy.url = normalize_proxy_url(&proxy.url)?;
    Ok(())
}

/// Validate proxy URL and upgrade `socks5://` → `socks5h://` (remote DNS, no leak).
fn normalize_proxy_url(raw: &str) -> Result<String, ManagementError> {
    let uri: http::Uri = raw.parse().map_err(|_| {
        ManagementError::InvalidRequest("invalid proxy URL (allowed: http, https, socks5, socks5h)")
    })?;
    let mut scheme = uri.scheme_str().unwrap_or("").to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "socks5" | "socks5h") {
        return Err(ManagementError::InvalidRequest(
            "unsupported proxy scheme (allowed: http, https, socks5, socks5h)",
        ));
    }
    let host = uri
        .host()
        .filter(|h| !h.is_empty())
        .ok_or(ManagementError::InvalidRequest("proxy URL missing host"))?;
    let port = uri.port_u16().unwrap_or(match scheme.as_str() {
        "https" => 443,
        "socks5" | "socks5h" => 1080,
        _ => 80,
    });
    // sub2api: socks5 → socks5h so DNS is resolved by the proxy.
    if scheme == "socks5" {
        scheme = "socks5h".to_string();
    }
    let userinfo = uri
        .authority()
        .and_then(|a| {
            let s = a.as_str();
            let (userinfo, _) = s.split_once('@')?;
            Some(format!("{userinfo}@"))
        })
        .unwrap_or_default();
    let host = if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(format!("{scheme}://{userinfo}{host}:{port}"))
}

async fn migrate_sqlite(pool: &SqlitePool) -> Result<(), ManagementError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS proxies (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            url TEXT NOT NULL,
            status INTEGER NOT NULL DEFAULT 1,
            remark TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .map_err(storage_err)?;
    Ok(())
}

async fn migrate_mysql(pool: &MySqlPool) -> Result<(), ManagementError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS proxies (
            id BIGINT PRIMARY KEY,
            name TEXT NOT NULL,
            url TEXT NOT NULL,
            status INTEGER NOT NULL DEFAULT 1,
            remark TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .map_err(storage_err)?;
    Ok(())
}

async fn migrate_pg(pool: &PgPool) -> Result<(), ManagementError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS proxies (
            id BIGINT PRIMARY KEY,
            name TEXT NOT NULL,
            url TEXT NOT NULL,
            status INTEGER NOT NULL DEFAULT 1,
            remark TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .map_err(storage_err)?;
    Ok(())
}

async fn load_sqlite(pool: &SqlitePool) -> Result<Vec<ProxyRecord>, ManagementError> {
    sqlx::query("SELECT id, name, url, status, remark FROM proxies ORDER BY id")
        .fetch_all(pool)
        .await
        .map_err(storage_err)?
        .into_iter()
        .map(|row| {
            Ok(ProxyRecord {
                id:     row.try_get::<i64, _>("id").map_err(storage_err)?.max(0) as u64,
                name:   row.try_get("name").map_err(storage_err)?,
                url:    row.try_get("url").map_err(storage_err)?,
                status: row.try_get::<i64, _>("status").map_err(storage_err)? as i32,
                remark: row.try_get("remark").map_err(storage_err)?,
            })
        })
        .collect()
}

async fn load_mysql(pool: &MySqlPool) -> Result<Vec<ProxyRecord>, ManagementError> {
    sqlx::query("SELECT id, name, url, status, remark FROM proxies ORDER BY id")
        .fetch_all(pool)
        .await
        .map_err(storage_err)?
        .into_iter()
        .map(|row| {
            Ok(ProxyRecord {
                id:     row.try_get::<i64, _>("id").map_err(storage_err)?.max(0) as u64,
                name:   row.try_get("name").map_err(storage_err)?,
                url:    row.try_get("url").map_err(storage_err)?,
                status: row.try_get::<i64, _>("status").map_err(storage_err)? as i32,
                remark: row.try_get("remark").map_err(storage_err)?,
            })
        })
        .collect()
}

async fn load_pg(pool: &PgPool) -> Result<Vec<ProxyRecord>, ManagementError> {
    sqlx::query("SELECT id, name, url, status, remark FROM proxies ORDER BY id")
        .fetch_all(pool)
        .await
        .map_err(storage_err)?
        .into_iter()
        .map(|row| {
            let id = if let Ok(v) = row.try_get::<i64, _>("id") {
                v
            } else {
                i64::from(row.try_get::<i32, _>("id").map_err(storage_err)?)
            };
            let status = if let Ok(v) = row.try_get::<i32, _>("status") {
                v
            } else {
                row.try_get::<i64, _>("status").map_err(storage_err)? as i32
            };
            Ok(ProxyRecord {
                id: id.max(0) as u64,
                name: row.try_get("name").map_err(storage_err)?,
                url: row.try_get("url").map_err(storage_err)?,
                status,
                remark: row.try_get("remark").map_err(storage_err)?,
            })
        })
        .collect()
}

async fn save_sqlite(pool: &SqlitePool, proxies: &[ProxyRecord]) -> Result<(), ManagementError> {
    let mut tx = pool.begin().await.map_err(storage_err)?;
    sqlx::query("DELETE FROM proxies")
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    for proxy in proxies {
        sqlx::query(
            "INSERT INTO proxies (id, name, url, status, remark)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(proxy.id as i64)
        .bind(&proxy.name)
        .bind(&proxy.url)
        .bind(proxy.status as i64)
        .bind(&proxy.remark)
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    }
    tx.commit().await.map_err(storage_err)
}

async fn save_mysql(pool: &MySqlPool, proxies: &[ProxyRecord]) -> Result<(), ManagementError> {
    let mut tx = pool.begin().await.map_err(storage_err)?;
    sqlx::query("DELETE FROM proxies")
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    for proxy in proxies {
        sqlx::query(
            "INSERT INTO proxies (id, name, url, status, remark)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(proxy.id as i64)
        .bind(&proxy.name)
        .bind(&proxy.url)
        .bind(proxy.status as i64)
        .bind(&proxy.remark)
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    }
    tx.commit().await.map_err(storage_err)
}

async fn save_pg(pool: &PgPool, proxies: &[ProxyRecord]) -> Result<(), ManagementError> {
    let mut tx = pool.begin().await.map_err(storage_err)?;
    sqlx::query("DELETE FROM proxies")
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;
    for proxy in proxies {
        sqlx::query(
            "INSERT INTO proxies (id, name, url, status, remark)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(proxy.id as i64)
        .bind(&proxy.name)
        .bind(&proxy.url)
        .bind(proxy.status)
        .bind(&proxy.remark)
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
    fn memory_proxy_crud() {
        block_on(async {
            let store = ProxyStore::memory();
            let created = store
                .call(CreateProxyRequest {
                    proxy: ProxyRecord {
                        id:     0,
                        name:   "us-east".into(),
                        url:    "http://127.0.0.1:7890".into(),
                        status: 1,
                        remark: "dev".into(),
                    },
                })
                .await
                .expect("create");
            assert_eq!(created.id, 1);
            assert_eq!(
                store.resolve_url(Some(created.id)).as_deref(),
                Some("http://127.0.0.1:7890")
            );

            let listed = store.call(ListProxiesRequest).await.expect("list");
            assert_eq!(listed.len(), 1);

            let got = store
                .call(GetProxyRequest { id: created.id })
                .await
                .expect("get");
            assert_eq!(got.name, "us-east");

            let updated = store
                .call(UpdateProxyRequest {
                    proxy: ProxyRecord {
                        id:     created.id,
                        name:   "us-west".into(),
                        url:    "socks5://127.0.0.1:1080".into(),
                        status: 0,
                        remark: String::new(),
                    },
                })
                .await
                .expect("update");
            assert_eq!(updated.name, "us-west");
            assert!(
                updated.url.starts_with("socks5h://"),
                "socks5 should upgrade to socks5h: {}",
                updated.url
            );
            assert!(store.resolve_url(Some(created.id)).is_none());

            store
                .call(DeleteProxyRequest { id: created.id })
                .await
                .expect("delete");
            let listed = store.call(ListProxiesRequest).await.expect("list empty");
            assert!(listed.is_empty());
        });
    }

    #[test]
    fn rejects_invalid_proxy_url() {
        block_on(async {
            let store = ProxyStore::memory();
            let err = store
                .call(CreateProxyRequest {
                    proxy: ProxyRecord {
                        id:     0,
                        name:   "bad".into(),
                        url:    "ftp://example.com".into(),
                        status: 1,
                        remark: String::new(),
                    },
                })
                .await
                .expect_err("invalid url");
            assert!(matches!(err, ManagementError::InvalidRequest(_)));
        });
    }

    #[test]
    fn upgrades_socks5_to_socks5h_on_create() {
        block_on(async {
            let store = ProxyStore::memory();
            let created = store
                .call(CreateProxyRequest {
                    proxy: ProxyRecord {
                        id:     0,
                        name:   "s".into(),
                        url:    "socks5://127.0.0.1:1080".into(),
                        status: 1,
                        remark: String::new(),
                    },
                })
                .await
                .expect("create");
            assert_eq!(created.url, "socks5h://127.0.0.1:1080");
        });
    }

    #[test]
    fn preserves_ipv6_brackets_when_normalizing_proxy_url() {
        assert_eq!(
            normalize_proxy_url("http://user:pass@[2001:db8::1]:8080").expect("normalize"),
            "http://user:pass@[2001:db8::1]:8080"
        );
    }
}
