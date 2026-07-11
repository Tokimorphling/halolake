use std::{
    str::FromStr,
    sync::{Arc, RwLock},
};

use halolake_control_plane::ManagementError;
use serde::Serialize;
use service_async::Service;
use sqlx::{
    MySqlPool, PgPool, Row, SqlitePool,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
    postgres::{PgConnectOptions, PgPoolOptions},
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CheckinRecord {
    pub(crate) id: u64,
    pub(crate) user_id: u64,
    pub(crate) checkin_date: String,
    pub(crate) quota_awarded: i64,
    pub(crate) created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CheckinPublicRecord {
    pub(crate) checkin_date: String,
    pub(crate) quota_awarded: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CheckinStats {
    pub(crate) total_quota: i64,
    pub(crate) total_checkins: usize,
    pub(crate) checkin_count: usize,
    pub(crate) checked_in_today: bool,
    pub(crate) records: Vec<CheckinPublicRecord>,
}

#[derive(Debug, Clone)]
pub(crate) struct GetCheckinStatsRequest {
    pub(crate) user_id: u64,
    pub(crate) month: String,
    pub(crate) today: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CreateCheckinRequest {
    pub(crate) user_id: u64,
    pub(crate) checkin_date: String,
    pub(crate) quota_awarded: i64,
    pub(crate) created_at: i64,
}

#[derive(Debug, Clone)]
pub(crate) enum CheckinStore {
    Memory(MemoryCheckinStore),
    Sqlite(SqliteCheckinStore),
    MySql(MySqlCheckinStore),
    Postgres(PostgresCheckinStore),
}

impl CheckinStore {
    pub(crate) fn memory() -> Self {
        Self::Memory(MemoryCheckinStore::default())
    }

    pub(crate) async fn sqlite(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Sqlite(SqliteCheckinStore::connect(url).await?))
    }

    pub(crate) async fn mysql(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::MySql(MySqlCheckinStore::connect(url).await?))
    }

    pub(crate) async fn postgres(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Postgres(PostgresCheckinStore::connect(url).await?))
    }
}

impl Service<GetCheckinStatsRequest> for CheckinStore {
    type Response = CheckinStats;
    type Error = ManagementError;

    async fn call(&self, req: GetCheckinStatsRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

impl Service<CreateCheckinRequest> for CheckinStore {
    type Response = CheckinRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateCheckinRequest) -> Result<Self::Response, Self::Error> {
        match self {
            Self::Memory(store) => store.call(req).await,
            Self::Sqlite(store) => store.call(req).await,
            Self::MySql(store) => store.call(req).await,
            Self::Postgres(store) => store.call(req).await,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MemoryCheckinStore {
    inner: Arc<RwLock<Vec<CheckinRecord>>>,
}

impl MemoryCheckinStore {
    fn current_records(&self) -> Result<Vec<CheckinRecord>, ManagementError> {
        self.inner
            .read()
            .map(|records| records.clone())
            .map_err(|_| ManagementError::Poisoned("checkins"))
    }

    fn insert(&self, mut record: CheckinRecord) -> Result<CheckinRecord, ManagementError> {
        let mut records = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("checkins"))?;
        if records
            .iter()
            .any(|item| item.user_id == record.user_id && item.checkin_date == record.checkin_date)
        {
            return Err(ManagementError::Duplicate);
        }
        if record.id == 0 {
            record.id = records
                .iter()
                .map(|record| record.id)
                .max()
                .unwrap_or(0)
                .saturating_add(1);
        }
        records.push(record.clone());
        Ok(record)
    }
}

impl Service<GetCheckinStatsRequest> for MemoryCheckinStore {
    type Response = CheckinStats;
    type Error = ManagementError;

    async fn call(&self, req: GetCheckinStatsRequest) -> Result<Self::Response, Self::Error> {
        let start_date = format!("{}-01", req.month);
        let end_date = format!("{}-31", req.month);
        let mut records = self
            .current_records()?
            .into_iter()
            .filter(|record| record.user_id == req.user_id)
            .collect::<Vec<_>>();
        let total_checkins = records.len();
        let total_quota = records
            .iter()
            .map(|record| record.quota_awarded)
            .sum::<i64>();
        let checked_in_today = records
            .iter()
            .any(|record| record.checkin_date == req.today);
        records
            .retain(|record| record.checkin_date >= start_date && record.checkin_date <= end_date);
        records.sort_by(|left, right| right.checkin_date.cmp(&left.checkin_date));
        let records = records
            .into_iter()
            .map(|record| CheckinPublicRecord {
                checkin_date: record.checkin_date,
                quota_awarded: record.quota_awarded,
            })
            .collect::<Vec<_>>();
        Ok(CheckinStats {
            total_quota,
            total_checkins,
            checkin_count: records.len(),
            checked_in_today,
            records,
        })
    }
}

impl Service<CreateCheckinRequest> for MemoryCheckinStore {
    type Response = CheckinRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateCheckinRequest) -> Result<Self::Response, Self::Error> {
        self.insert(CheckinRecord {
            id: 0,
            user_id: req.user_id,
            checkin_date: req.checkin_date,
            quota_awarded: req.quota_awarded,
            created_at: req.created_at,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SqliteCheckinStore {
    pool: SqlitePool,
    memory: MemoryCheckinStore,
}

impl SqliteCheckinStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = SqliteConnectOptions::from_str(url)
            .map_err(|err| ManagementError::Storage(err.to_string()))?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(|err| ManagementError::Storage(err.to_string()))?;
        migrate_checkins(&pool).await?;
        let memory = MemoryCheckinStore::default();
        for record in load_checkins(&pool).await? {
            memory.insert(record)?;
        }
        Ok(Self { pool, memory })
    }
}

impl Service<GetCheckinStatsRequest> for SqliteCheckinStore {
    type Response = CheckinStats;
    type Error = ManagementError;

    async fn call(&self, req: GetCheckinStatsRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<CreateCheckinRequest> for SqliteCheckinStore {
    type Response = CheckinRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateCheckinRequest) -> Result<Self::Response, Self::Error> {
        let result = sqlx::query(
            "INSERT OR IGNORE INTO checkins (user_id, checkin_date, quota_awarded, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(req.user_id as i64)
        .bind(&req.checkin_date)
        .bind(req.quota_awarded)
        .bind(req.created_at)
        .execute(&self.pool)
        .await
        .map_err(|err| ManagementError::Storage(err.to_string()))?;
        if result.rows_affected() == 0 {
            return Err(ManagementError::Duplicate);
        }
        let id = result.last_insert_rowid().max(0) as u64;
        self.memory.insert(CheckinRecord {
            id,
            user_id: req.user_id,
            checkin_date: req.checkin_date,
            quota_awarded: req.quota_awarded,
            created_at: req.created_at,
        })
    }
}

async fn migrate_checkins(pool: &SqlitePool) -> Result<(), ManagementError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS checkins (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            checkin_date TEXT NOT NULL,
            quota_awarded INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            UNIQUE(user_id, checkin_date)
        )",
    )
    .execute(pool)
    .await
    .map_err(|err| ManagementError::Storage(err.to_string()))?;
    Ok(())
}

async fn load_checkins(pool: &SqlitePool) -> Result<Vec<CheckinRecord>, ManagementError> {
    sqlx::query(
        "SELECT id, user_id, checkin_date, quota_awarded, created_at
         FROM checkins ORDER BY checkin_date DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(|err| ManagementError::Storage(err.to_string()))?
    .into_iter()
    .map(|row| {
        Ok(CheckinRecord {
            id: row
                .try_get::<i64, _>("id")
                .map_err(|err| ManagementError::Storage(err.to_string()))?
                .max(0) as u64,
            user_id: row
                .try_get::<i64, _>("user_id")
                .map_err(|err| ManagementError::Storage(err.to_string()))?
                .max(0) as u64,
            checkin_date: row
                .try_get("checkin_date")
                .map_err(|err| ManagementError::Storage(err.to_string()))?,
            quota_awarded: row
                .try_get("quota_awarded")
                .map_err(|err| ManagementError::Storage(err.to_string()))?,
            created_at: row
                .try_get("created_at")
                .map_err(|err| ManagementError::Storage(err.to_string()))?,
        })
    })
    .collect()
}

#[derive(Debug, Clone)]
pub(crate) struct MySqlCheckinStore {
    pool: MySqlPool,
    memory: MemoryCheckinStore,
}

impl MySqlCheckinStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = MySqlConnectOptions::from_str(url)
            .map_err(|err| ManagementError::Storage(err.to_string()))?;
        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(|err| ManagementError::Storage(err.to_string()))?;
        migrate_checkins_mysql(&pool).await?;
        let memory = MemoryCheckinStore::default();
        for record in load_checkins_mysql(&pool).await? {
            memory.insert(record)?;
        }
        Ok(Self { pool, memory })
    }
}

impl Service<GetCheckinStatsRequest> for MySqlCheckinStore {
    type Response = CheckinStats;
    type Error = ManagementError;

    async fn call(&self, req: GetCheckinStatsRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<CreateCheckinRequest> for MySqlCheckinStore {
    type Response = CheckinRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateCheckinRequest) -> Result<Self::Response, Self::Error> {
        let result = sqlx::query(
            "INSERT IGNORE INTO checkins (user_id, checkin_date, quota_awarded, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(req.user_id as i64)
        .bind(&req.checkin_date)
        .bind(req.quota_awarded)
        .bind(req.created_at)
        .execute(&self.pool)
        .await
        .map_err(|err| ManagementError::Storage(err.to_string()))?;
        if result.rows_affected() == 0 {
            return Err(ManagementError::Duplicate);
        }
        let id = result.last_insert_id().max(0) as u64;
        self.memory.insert(CheckinRecord {
            id,
            user_id: req.user_id,
            checkin_date: req.checkin_date,
            quota_awarded: req.quota_awarded,
            created_at: req.created_at,
        })
    }
}

async fn migrate_checkins_mysql(pool: &MySqlPool) -> Result<(), ManagementError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS checkins (
            id BIGINT PRIMARY KEY AUTO_INCREMENT,
            user_id BIGINT NOT NULL,
            checkin_date TEXT NOT NULL,
            quota_awarded BIGINT NOT NULL,
            created_at BIGINT NOT NULL,
            UNIQUE(user_id, checkin_date)
        )",
    )
    .execute(pool)
    .await
    .map_err(|err| ManagementError::Storage(err.to_string()))?;
    Ok(())
}

async fn load_checkins_mysql(pool: &MySqlPool) -> Result<Vec<CheckinRecord>, ManagementError> {
    sqlx::query(
        "SELECT id, user_id, checkin_date, quota_awarded, created_at
         FROM checkins ORDER BY checkin_date DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(|err| ManagementError::Storage(err.to_string()))?
    .into_iter()
    .map(|row| {
        Ok(CheckinRecord {
            id: row
                .try_get::<i64, _>("id")
                .map_err(|err| ManagementError::Storage(err.to_string()))?
                .max(0) as u64,
            user_id: row
                .try_get::<i64, _>("user_id")
                .map_err(|err| ManagementError::Storage(err.to_string()))?
                .max(0) as u64,
            checkin_date: row
                .try_get("checkin_date")
                .map_err(|err| ManagementError::Storage(err.to_string()))?,
            quota_awarded: row
                .try_get("quota_awarded")
                .map_err(|err| ManagementError::Storage(err.to_string()))?,
            created_at: row
                .try_get("created_at")
                .map_err(|err| ManagementError::Storage(err.to_string()))?,
        })
    })
    .collect()
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresCheckinStore {
    pool: PgPool,
    memory: MemoryCheckinStore,
}

impl PostgresCheckinStore {
    async fn connect(url: &str) -> Result<Self, ManagementError> {
        let options = PgConnectOptions::from_str(url)
            .map_err(|err| ManagementError::Storage(err.to_string()))?;
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(|err| ManagementError::Storage(err.to_string()))?;
        migrate_checkins_pg(&pool).await?;
        let memory = MemoryCheckinStore::default();
        for record in load_checkins_pg(&pool).await? {
            let _ = memory.insert(record);
        }
        Ok(Self { pool, memory })
    }
}

impl Service<GetCheckinStatsRequest> for PostgresCheckinStore {
    type Response = CheckinStats;
    type Error = ManagementError;

    async fn call(&self, req: GetCheckinStatsRequest) -> Result<Self::Response, Self::Error> {
        self.memory.call(req).await
    }
}

impl Service<CreateCheckinRequest> for PostgresCheckinStore {
    type Response = CheckinRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateCheckinRequest) -> Result<Self::Response, Self::Error> {
        let result = sqlx::query(
            "INSERT INTO checkins (user_id, checkin_date, quota_awarded, created_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (user_id, checkin_date) DO NOTHING
             RETURNING id",
        )
        .bind(req.user_id as i64)
        .bind(&req.checkin_date)
        .bind(req.quota_awarded)
        .bind(req.created_at)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| ManagementError::Storage(err.to_string()))?;
        let Some(row) = result else {
            return Err(ManagementError::Duplicate);
        };
        let id = row
            .try_get::<i64, _>("id")
            .map_err(|err| ManagementError::Storage(err.to_string()))?
            .max(0) as u64;
        self.memory.insert(CheckinRecord {
            id,
            user_id: req.user_id,
            checkin_date: req.checkin_date,
            quota_awarded: req.quota_awarded,
            created_at: req.created_at,
        })
    }
}

async fn migrate_checkins_pg(pool: &PgPool) -> Result<(), ManagementError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS checkins (
            id BIGSERIAL PRIMARY KEY,
            user_id BIGINT NOT NULL,
            checkin_date TEXT NOT NULL,
            quota_awarded BIGINT NOT NULL,
            created_at BIGINT NOT NULL,
            UNIQUE(user_id, checkin_date)
        )",
    )
    .execute(pool)
    .await
    .map_err(|err| ManagementError::Storage(err.to_string()))?;
    Ok(())
}

async fn load_checkins_pg(pool: &PgPool) -> Result<Vec<CheckinRecord>, ManagementError> {
    sqlx::query(
        "SELECT id, user_id, checkin_date, quota_awarded, created_at
         FROM checkins ORDER BY checkin_date DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(|err| ManagementError::Storage(err.to_string()))?
    .into_iter()
    .map(|row| {
        Ok(CheckinRecord {
            id: row
                .try_get::<i64, _>("id")
                .map_err(|err| ManagementError::Storage(err.to_string()))?
                .max(0) as u64,
            user_id: row
                .try_get::<i64, _>("user_id")
                .map_err(|err| ManagementError::Storage(err.to_string()))?
                .max(0) as u64,
            checkin_date: row
                .try_get("checkin_date")
                .map_err(|err| ManagementError::Storage(err.to_string()))?,
            quota_awarded: row
                .try_get("quota_awarded")
                .map_err(|err| ManagementError::Storage(err.to_string()))?,
            created_at: row
                .try_get("created_at")
                .map_err(|err| ManagementError::Storage(err.to_string()))?,
        })
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    use super::*;

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
    fn memory_checkin_rejects_duplicate_day_and_builds_stats() {
        block_on(async {
            let store = MemoryCheckinStore::default();
            let record = store
                .call(CreateCheckinRequest {
                    user_id: 7,
                    checkin_date: "2026-07-09".to_string(),
                    quota_awarded: 1234,
                    created_at: 1_783_548_000,
                })
                .await
                .expect("first checkin should be accepted");
            assert_eq!(record.id, 1);

            let duplicate = store
                .call(CreateCheckinRequest {
                    user_id: 7,
                    checkin_date: "2026-07-09".to_string(),
                    quota_awarded: 999,
                    created_at: 1_783_548_001,
                })
                .await;
            assert!(matches!(duplicate, Err(ManagementError::Duplicate)));

            let stats = store
                .call(GetCheckinStatsRequest {
                    user_id: 7,
                    month: "2026-07".to_string(),
                    today: "2026-07-09".to_string(),
                })
                .await
                .expect("stats should be returned");
            assert_eq!(stats.total_quota, 1234);
            assert_eq!(stats.total_checkins, 1);
            assert_eq!(stats.checkin_count, 1);
            assert!(stats.checked_in_today);
            assert_eq!(stats.records[0].checkin_date, "2026-07-09");
        });
    }
}
