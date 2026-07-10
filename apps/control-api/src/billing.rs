use std::{
    str::FromStr,
    sync::{Arc, RwLock},
};

use halolake_control_plane::ManagementError;
use halolake_domain::{PageRequest, PageResult};
use serde::{Deserialize, Serialize};
use service_async::Service;
use sqlx::{
    Row, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use uuid::Uuid;

pub(crate) const REDEMPTION_STATUS_ENABLED: i32 = 1;
pub(crate) const REDEMPTION_STATUS_DISABLED: i32 = 2;
pub(crate) const REDEMPTION_STATUS_USED: i32 = 3;
pub(crate) const TOPUP_STATUS_PENDING: &str = "pending";
pub(crate) const TOPUP_STATUS_SUCCESS: &str = "success";
const TOPUP_QUERY_WINDOW_SECONDS: i64 = 30 * 24 * 60 * 60;
const PAYMENT_PROVIDER_STRIPE: &str = "stripe";

#[derive(Debug, Clone, Default)]
pub(crate) struct BillingData {
    pub(crate) redemptions: Vec<RedemptionRecord>,
    pub(crate) topups: Vec<TopUpRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct RedemptionRecord {
    #[serde(default)]
    pub(crate) id: u64,
    #[serde(default)]
    pub(crate) user_id: u64,
    #[serde(default)]
    pub(crate) key: String,
    #[serde(default = "default_redemption_status")]
    pub(crate) status: i32,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default = "default_redemption_quota")]
    pub(crate) quota: i64,
    #[serde(default)]
    pub(crate) created_time: i64,
    #[serde(default)]
    pub(crate) redeemed_time: i64,
    #[serde(default)]
    pub(crate) count: usize,
    #[serde(default)]
    pub(crate) used_user_id: u64,
    #[serde(default)]
    pub(crate) expired_time: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub(crate) struct TopUpRecord {
    #[serde(default)]
    pub(crate) id: u64,
    #[serde(default)]
    pub(crate) user_id: u64,
    #[serde(default)]
    pub(crate) amount: i64,
    #[serde(default)]
    pub(crate) money: f64,
    #[serde(default)]
    pub(crate) trade_no: String,
    #[serde(default)]
    pub(crate) payment_method: String,
    #[serde(default)]
    pub(crate) payment_provider: String,
    #[serde(default)]
    pub(crate) create_time: i64,
    #[serde(default)]
    pub(crate) complete_time: i64,
    #[serde(default = "default_topup_status")]
    pub(crate) status: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ListRedemptionsRequest {
    pub(crate) page: PageRequest,
}

#[derive(Debug, Clone)]
pub(crate) struct SearchRedemptionsRequest {
    pub(crate) page: PageRequest,
    pub(crate) keyword: String,
    pub(crate) status: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GetRedemptionRequest {
    pub(crate) id: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct CreateRedemptionsRequest {
    pub(crate) redemption: RedemptionRecord,
    pub(crate) user_id: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct UpdateRedemptionRequest {
    pub(crate) redemption: RedemptionRecord,
    pub(crate) status_only: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeleteRedemptionRequest {
    pub(crate) id: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeleteInvalidRedemptionsRequest;

#[derive(Debug, Clone)]
pub(crate) struct RedeemRedemptionRequest {
    pub(crate) key: String,
    pub(crate) user_id: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RollbackRedeemRedemptionRequest {
    pub(crate) id: u64,
    pub(crate) user_id: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct RedeemedRedemption {
    pub(crate) id: u64,
    pub(crate) quota: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct ListTopUpsRequest {
    pub(crate) user_id: Option<u64>,
    pub(crate) page: PageRequest,
    pub(crate) keyword: String,
    pub(crate) recent_only: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct CreateTopUpRequest {
    pub(crate) topup: TopUpRecord,
}

#[derive(Debug, Clone)]
pub(crate) struct CompleteTopUpRequest {
    pub(crate) trade_no: String,
    pub(crate) quota_per_unit: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct CompletedTopUp {
    pub(crate) topup: TopUpRecord,
    pub(crate) quota: i64,
}

#[derive(Debug, Clone)]
pub(crate) enum BillingStore {
    Memory(MemoryBillingStore),
    Sqlite(SqliteBillingStore),
}

impl BillingStore {
    pub(crate) fn memory() -> Self {
        Self::Memory(MemoryBillingStore::new(BillingData::default()))
    }

    pub(crate) async fn sqlite(url: &str) -> Result<Self, ManagementError> {
        Ok(Self::Sqlite(SqliteBillingStore::connect(url).await?))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MemoryBillingStore {
    inner: Arc<RwLock<BillingData>>,
}

impl MemoryBillingStore {
    fn new(data: BillingData) -> Self {
        Self {
            inner: Arc::new(RwLock::new(data)),
        }
    }

    fn current_data(&self) -> Result<BillingData, ManagementError> {
        self.inner
            .read()
            .map(|data| data.clone())
            .map_err(|_| ManagementError::Poisoned("billing"))
    }

    fn mutate<F, T>(&self, f: F) -> Result<T, ManagementError>
    where
        F: FnOnce(&mut BillingData) -> Result<T, ManagementError>,
    {
        let mut data = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("billing"))?;
        f(&mut data)
    }
}

macro_rules! impl_billing_service {
    ($req:ty, $resp:ty) => {
        impl Service<$req> for BillingStore {
            type Response = $resp;
            type Error = ManagementError;

            async fn call(&self, req: $req) -> Result<Self::Response, Self::Error> {
                match self {
                    Self::Memory(store) => store.call(req).await,
                    Self::Sqlite(store) => store.call(req).await,
                }
            }
        }
    };
}

impl_billing_service!(ListRedemptionsRequest, PageResult<RedemptionRecord>);
impl_billing_service!(SearchRedemptionsRequest, PageResult<RedemptionRecord>);
impl_billing_service!(GetRedemptionRequest, RedemptionRecord);
impl_billing_service!(CreateRedemptionsRequest, Vec<String>);
impl_billing_service!(UpdateRedemptionRequest, RedemptionRecord);
impl_billing_service!(DeleteRedemptionRequest, ());
impl_billing_service!(DeleteInvalidRedemptionsRequest, usize);
impl_billing_service!(RedeemRedemptionRequest, RedeemedRedemption);
impl_billing_service!(RollbackRedeemRedemptionRequest, ());
impl_billing_service!(ListTopUpsRequest, PageResult<TopUpRecord>);
impl_billing_service!(CreateTopUpRequest, TopUpRecord);
impl_billing_service!(CompleteTopUpRequest, CompletedTopUp);

impl Service<ListRedemptionsRequest> for MemoryBillingStore {
    type Response = PageResult<RedemptionRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListRedemptionsRequest) -> Result<Self::Response, Self::Error> {
        let mut redemptions = self.current_data()?.redemptions;
        sort_desc_by_id(&mut redemptions);
        Ok(page(redemptions, req.page))
    }
}

impl Service<SearchRedemptionsRequest> for MemoryBillingStore {
    type Response = PageResult<RedemptionRecord>;
    type Error = ManagementError;

    async fn call(&self, req: SearchRedemptionsRequest) -> Result<Self::Response, Self::Error> {
        let keyword = req.keyword.trim();
        let id_keyword = keyword.parse::<u64>().ok();
        let status = req.status.trim();
        let now = now_unix();
        let mut redemptions = self
            .current_data()?
            .redemptions
            .into_iter()
            .filter(|redemption| {
                keyword.is_empty()
                    || id_keyword.is_some_and(|id| redemption.id == id)
                    || redemption.name.starts_with(keyword)
            })
            .filter(|redemption| match status {
                "expired" => {
                    redemption.status == REDEMPTION_STATUS_ENABLED
                        && redemption.expired_time != 0
                        && redemption.expired_time < now
                }
                "1" => {
                    redemption.status == REDEMPTION_STATUS_ENABLED
                        && (redemption.expired_time == 0 || redemption.expired_time >= now)
                }
                "2" => redemption.status == REDEMPTION_STATUS_DISABLED,
                "3" => redemption.status == REDEMPTION_STATUS_USED,
                _ => true,
            })
            .collect::<Vec<_>>();
        sort_desc_by_id(&mut redemptions);
        Ok(page(redemptions, req.page))
    }
}

impl Service<GetRedemptionRequest> for MemoryBillingStore {
    type Response = RedemptionRecord;
    type Error = ManagementError;

    async fn call(&self, req: GetRedemptionRequest) -> Result<Self::Response, Self::Error> {
        self.current_data()?
            .redemptions
            .into_iter()
            .find(|redemption| redemption.id == req.id)
            .ok_or(ManagementError::NotFound)
    }
}

impl Service<CreateRedemptionsRequest> for MemoryBillingStore {
    type Response = Vec<String>;
    type Error = ManagementError;

    async fn call(&self, req: CreateRedemptionsRequest) -> Result<Self::Response, Self::Error> {
        let mut template = req.redemption;
        validate_redemption_create(&mut template)?;
        self.mutate(|data| {
            let now = now_unix();
            let mut next = next_id(data.redemptions.iter().map(|redemption| redemption.id));
            let mut keys = Vec::with_capacity(template.count);
            for _ in 0..template.count {
                let key = unique_redemption_key(&data.redemptions);
                let record = RedemptionRecord {
                    id: next,
                    user_id: req.user_id,
                    key: key.clone(),
                    status: REDEMPTION_STATUS_ENABLED,
                    name: template.name.clone(),
                    quota: template.quota,
                    created_time: now,
                    redeemed_time: 0,
                    count: 0,
                    used_user_id: 0,
                    expired_time: template.expired_time,
                };
                data.redemptions.push(record);
                keys.push(key);
                next = next.saturating_add(1);
            }
            Ok(keys)
        })
    }
}

impl Service<UpdateRedemptionRequest> for MemoryBillingStore {
    type Response = RedemptionRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateRedemptionRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let current = data
                .redemptions
                .iter_mut()
                .find(|redemption| redemption.id == req.redemption.id)
                .ok_or(ManagementError::NotFound)?;
            if req.status_only {
                current.status = req.redemption.status;
                return Ok(current.clone());
            }
            if req.redemption.expired_time != 0 && req.redemption.expired_time < now_unix() {
                return Err(ManagementError::InvalidRequest(
                    "redemption expired_time is invalid",
                ));
            }
            current.name = req.redemption.name;
            current.quota = req.redemption.quota;
            current.expired_time = req.redemption.expired_time;
            Ok(current.clone())
        })
    }
}

impl Service<DeleteRedemptionRequest> for MemoryBillingStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeleteRedemptionRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let before = data.redemptions.len();
            data.redemptions
                .retain(|redemption| redemption.id != req.id);
            if before == data.redemptions.len() {
                return Err(ManagementError::NotFound);
            }
            Ok(())
        })
    }
}

impl Service<DeleteInvalidRedemptionsRequest> for MemoryBillingStore {
    type Response = usize;
    type Error = ManagementError;

    async fn call(
        &self,
        _req: DeleteInvalidRedemptionsRequest,
    ) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let now = now_unix();
            let before = data.redemptions.len();
            data.redemptions.retain(|redemption| {
                let invalid_status = matches!(
                    redemption.status,
                    REDEMPTION_STATUS_DISABLED | REDEMPTION_STATUS_USED
                );
                let expired = redemption.status == REDEMPTION_STATUS_ENABLED
                    && redemption.expired_time != 0
                    && redemption.expired_time < now;
                !(invalid_status || expired)
            });
            Ok(before.saturating_sub(data.redemptions.len()))
        })
    }
}

impl Service<RedeemRedemptionRequest> for MemoryBillingStore {
    type Response = RedeemedRedemption;
    type Error = ManagementError;

    async fn call(&self, req: RedeemRedemptionRequest) -> Result<Self::Response, Self::Error> {
        let key = req.key.trim();
        if key.is_empty() {
            return Err(ManagementError::InvalidRequest(
                "redemption key is required",
            ));
        }
        if req.user_id == 0 {
            return Err(ManagementError::InvalidRequest("user id is required"));
        }
        self.mutate(|data| {
            let redemption = data
                .redemptions
                .iter_mut()
                .find(|redemption| redemption.key == key)
                .ok_or(ManagementError::InvalidRequest("redeem failed"))?;
            if redemption.status != REDEMPTION_STATUS_ENABLED {
                return Err(ManagementError::InvalidRequest("redeem failed"));
            }
            if redemption.expired_time != 0 && redemption.expired_time < now_unix() {
                return Err(ManagementError::InvalidRequest("redeem failed"));
            }
            redemption.status = REDEMPTION_STATUS_USED;
            redemption.redeemed_time = now_unix();
            redemption.used_user_id = req.user_id;
            Ok(RedeemedRedemption {
                id: redemption.id,
                quota: redemption.quota,
            })
        })
    }
}

impl Service<RollbackRedeemRedemptionRequest> for MemoryBillingStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(
        &self,
        req: RollbackRedeemRedemptionRequest,
    ) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let redemption = data
                .redemptions
                .iter_mut()
                .find(|redemption| redemption.id == req.id)
                .ok_or(ManagementError::NotFound)?;
            if redemption.status == REDEMPTION_STATUS_USED && redemption.used_user_id == req.user_id
            {
                redemption.status = REDEMPTION_STATUS_ENABLED;
                redemption.redeemed_time = 0;
                redemption.used_user_id = 0;
            }
            Ok(())
        })
    }
}

impl Service<ListTopUpsRequest> for MemoryBillingStore {
    type Response = PageResult<TopUpRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListTopUpsRequest) -> Result<Self::Response, Self::Error> {
        let keyword = req.keyword.trim();
        let cutoff = now_unix().saturating_sub(TOPUP_QUERY_WINDOW_SECONDS);
        let mut topups = self
            .current_data()?
            .topups
            .into_iter()
            .filter(|topup| req.user_id.is_none_or(|user_id| topup.user_id == user_id))
            .filter(|topup| !req.recent_only || topup.create_time >= cutoff)
            .filter(|topup| keyword.is_empty() || topup.trade_no.contains(keyword))
            .collect::<Vec<_>>();
        sort_desc_by_id(&mut topups);
        Ok(page(topups, req.page))
    }
}

impl Service<CreateTopUpRequest> for MemoryBillingStore {
    type Response = TopUpRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateTopUpRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let mut topup = req.topup;
            normalize_topup_for_write(&mut topup)?;
            if data
                .topups
                .iter()
                .any(|item| item.trade_no == topup.trade_no || item.id == topup.id && topup.id != 0)
            {
                return Err(ManagementError::Duplicate);
            }
            if topup.id == 0 {
                topup.id = next_id(data.topups.iter().map(|topup| topup.id));
            }
            if topup.create_time == 0 {
                topup.create_time = now_unix();
            }
            data.topups.push(topup.clone());
            Ok(topup)
        })
    }
}

impl Service<CompleteTopUpRequest> for MemoryBillingStore {
    type Response = CompletedTopUp;
    type Error = ManagementError;

    async fn call(&self, req: CompleteTopUpRequest) -> Result<Self::Response, Self::Error> {
        let trade_no = req.trade_no.trim();
        if trade_no.is_empty() {
            return Err(ManagementError::InvalidRequest("trade_no is required"));
        }
        self.mutate(|data| {
            let topup = data
                .topups
                .iter_mut()
                .find(|topup| topup.trade_no == trade_no)
                .ok_or(ManagementError::NotFound)?;
            if topup.status == TOPUP_STATUS_SUCCESS {
                return Ok(CompletedTopUp {
                    topup: topup.clone(),
                    quota: 0,
                });
            }
            if topup.status != TOPUP_STATUS_PENDING {
                return Err(ManagementError::InvalidRequest(
                    "topup status is not pending",
                ));
            }
            let quota = topup_quota(topup, req.quota_per_unit)?;
            topup.complete_time = now_unix();
            topup.status = TOPUP_STATUS_SUCCESS.to_string();
            Ok(CompletedTopUp {
                topup: topup.clone(),
                quota,
            })
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SqliteBillingStore {
    pool: SqlitePool,
    memory: MemoryBillingStore,
}

impl SqliteBillingStore {
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
        migrate_billing(&pool).await?;
        let data = load_billing(&pool).await?;
        Ok(Self {
            pool,
            memory: MemoryBillingStore::new(data),
        })
    }

    async fn persist(&self) -> Result<(), ManagementError> {
        let data = self.memory.current_data()?;
        save_billing(&self.pool, &data).await
    }
}

macro_rules! impl_sqlite_billing_read_service {
    ($req:ty, $resp:ty) => {
        impl Service<$req> for SqliteBillingStore {
            type Response = $resp;
            type Error = ManagementError;

            async fn call(&self, req: $req) -> Result<Self::Response, Self::Error> {
                self.memory.call(req).await
            }
        }
    };
}

macro_rules! impl_sqlite_billing_write_service {
    ($req:ty, $resp:ty) => {
        impl Service<$req> for SqliteBillingStore {
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

impl_sqlite_billing_read_service!(ListRedemptionsRequest, PageResult<RedemptionRecord>);
impl_sqlite_billing_read_service!(SearchRedemptionsRequest, PageResult<RedemptionRecord>);
impl_sqlite_billing_read_service!(GetRedemptionRequest, RedemptionRecord);
impl_sqlite_billing_read_service!(ListTopUpsRequest, PageResult<TopUpRecord>);

impl_sqlite_billing_write_service!(CreateRedemptionsRequest, Vec<String>);
impl_sqlite_billing_write_service!(UpdateRedemptionRequest, RedemptionRecord);
impl_sqlite_billing_write_service!(DeleteRedemptionRequest, ());
impl_sqlite_billing_write_service!(DeleteInvalidRedemptionsRequest, usize);
impl_sqlite_billing_write_service!(RedeemRedemptionRequest, RedeemedRedemption);
impl_sqlite_billing_write_service!(RollbackRedeemRedemptionRequest, ());
impl_sqlite_billing_write_service!(CreateTopUpRequest, TopUpRecord);
impl_sqlite_billing_write_service!(CompleteTopUpRequest, CompletedTopUp);

async fn migrate_billing(pool: &SqlitePool) -> Result<(), ManagementError> {
    for stmt in [
        "CREATE TABLE IF NOT EXISTS redemptions (
            id INTEGER PRIMARY KEY,
            user_id INTEGER NOT NULL,
            key TEXT NOT NULL UNIQUE,
            status INTEGER NOT NULL DEFAULT 1,
            name TEXT NOT NULL DEFAULT '',
            quota INTEGER NOT NULL DEFAULT 100,
            created_time INTEGER NOT NULL DEFAULT 0,
            redeemed_time INTEGER NOT NULL DEFAULT 0,
            used_user_id INTEGER NOT NULL DEFAULT 0,
            expired_time INTEGER NOT NULL DEFAULT 0
        )",
        "CREATE TABLE IF NOT EXISTS topups (
            id INTEGER PRIMARY KEY,
            user_id INTEGER NOT NULL,
            amount INTEGER NOT NULL DEFAULT 0,
            money REAL NOT NULL DEFAULT 0,
            trade_no TEXT NOT NULL UNIQUE,
            payment_method TEXT NOT NULL DEFAULT '',
            payment_provider TEXT NOT NULL DEFAULT '',
            create_time INTEGER NOT NULL DEFAULT 0,
            complete_time INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'pending'
        )",
        "CREATE INDEX IF NOT EXISTS idx_redemptions_status ON redemptions(status)",
        "CREATE INDEX IF NOT EXISTS idx_topups_user_id ON topups(user_id)",
        "CREATE INDEX IF NOT EXISTS idx_topups_create_time ON topups(create_time)",
    ] {
        sqlx::query(stmt).execute(pool).await.map_err(storage_err)?;
    }
    Ok(())
}

async fn load_billing(pool: &SqlitePool) -> Result<BillingData, ManagementError> {
    let redemptions = sqlx::query(
        "SELECT id, user_id, key, status, name, quota, created_time, redeemed_time,
            used_user_id, expired_time
         FROM redemptions ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(RedemptionRecord {
            id: u64_col(&row, "id")?,
            user_id: u64_col(&row, "user_id")?,
            key: string_col(&row, "key")?,
            status: i32_col(&row, "status")?,
            name: string_col(&row, "name")?,
            quota: i64_col(&row, "quota")?,
            created_time: i64_col(&row, "created_time")?,
            redeemed_time: i64_col(&row, "redeemed_time")?,
            count: 0,
            used_user_id: u64_col(&row, "used_user_id")?,
            expired_time: i64_col(&row, "expired_time")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    let topups = sqlx::query(
        "SELECT id, user_id, amount, money, trade_no, payment_method, payment_provider,
            create_time, complete_time, status
         FROM topups ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(TopUpRecord {
            id: u64_col(&row, "id")?,
            user_id: u64_col(&row, "user_id")?,
            amount: i64_col(&row, "amount")?,
            money: f64_col(&row, "money")?,
            trade_no: string_col(&row, "trade_no")?,
            payment_method: string_col(&row, "payment_method")?,
            payment_provider: string_col(&row, "payment_provider")?,
            create_time: i64_col(&row, "create_time")?,
            complete_time: i64_col(&row, "complete_time")?,
            status: string_col(&row, "status")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    Ok(BillingData {
        redemptions,
        topups,
    })
}

async fn save_billing(pool: &SqlitePool, data: &BillingData) -> Result<(), ManagementError> {
    let mut tx = pool.begin().await.map_err(storage_err)?;
    save_billing_tx(&mut tx, data).await?;
    tx.commit().await.map_err(storage_err)
}

async fn save_billing_tx(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    data: &BillingData,
) -> Result<(), ManagementError> {
    sqlx::query("DELETE FROM topups")
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;
    sqlx::query("DELETE FROM redemptions")
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;

    for redemption in &data.redemptions {
        sqlx::query(
            "INSERT INTO redemptions (
                id, user_id, key, status, name, quota, created_time, redeemed_time,
                used_user_id, expired_time
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(redemption.id as i64)
        .bind(redemption.user_id as i64)
        .bind(&redemption.key)
        .bind(redemption.status as i64)
        .bind(&redemption.name)
        .bind(redemption.quota)
        .bind(redemption.created_time)
        .bind(redemption.redeemed_time)
        .bind(redemption.used_user_id as i64)
        .bind(redemption.expired_time)
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;
    }

    for topup in &data.topups {
        sqlx::query(
            "INSERT INTO topups (
                id, user_id, amount, money, trade_no, payment_method, payment_provider,
                create_time, complete_time, status
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(topup.id as i64)
        .bind(topup.user_id as i64)
        .bind(topup.amount)
        .bind(topup.money)
        .bind(&topup.trade_no)
        .bind(&topup.payment_method)
        .bind(&topup.payment_provider)
        .bind(topup.create_time)
        .bind(topup.complete_time)
        .bind(&topup.status)
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;
    }
    Ok(())
}

fn validate_redemption_create(redemption: &mut RedemptionRecord) -> Result<(), ManagementError> {
    redemption.name = redemption.name.trim().to_string();
    let name_len = redemption.name.chars().count();
    if name_len == 0 || name_len > 20 {
        return Err(ManagementError::InvalidRequest(
            "redemption name length must be 1..20",
        ));
    }
    if redemption.count == 0 {
        return Err(ManagementError::InvalidRequest(
            "redemption count must be positive",
        ));
    }
    if redemption.count > 100 {
        return Err(ManagementError::InvalidRequest(
            "redemption count must be <= 100",
        ));
    }
    if redemption.expired_time != 0 && redemption.expired_time < now_unix() {
        return Err(ManagementError::InvalidRequest(
            "redemption expired_time is invalid",
        ));
    }
    if redemption.quota <= 0 {
        return Err(ManagementError::InvalidRequest(
            "redemption quota must be positive",
        ));
    }
    Ok(())
}

fn normalize_topup_for_write(topup: &mut TopUpRecord) -> Result<(), ManagementError> {
    topup.trade_no = topup.trade_no.trim().to_string();
    if topup.trade_no.is_empty() {
        return Err(ManagementError::InvalidRequest("trade_no is required"));
    }
    if topup.user_id == 0 {
        return Err(ManagementError::InvalidRequest("user_id is required"));
    }
    if topup.status.is_empty() {
        topup.status = TOPUP_STATUS_PENDING.to_string();
    }
    Ok(())
}

fn topup_quota(topup: &TopUpRecord, quota_per_unit: f64) -> Result<i64, ManagementError> {
    let value = if topup.payment_provider == PAYMENT_PROVIDER_STRIPE {
        topup.money * quota_per_unit
    } else {
        topup.amount as f64 * quota_per_unit
    };
    if !value.is_finite() || value <= 0.0 {
        return Err(ManagementError::InvalidRequest("invalid topup quota"));
    }
    Ok(value as i64)
}

fn unique_redemption_key(redemptions: &[RedemptionRecord]) -> String {
    loop {
        let key = Uuid::new_v4().simple().to_string();
        if !redemptions
            .iter()
            .any(|redemption| redemption.key.as_str() == key.as_str())
        {
            return key;
        }
    }
}

fn sort_desc_by_id<T>(items: &mut [T])
where
    T: HasBillingId,
{
    items.sort_by_key(|item| std::cmp::Reverse(item.id()));
}

trait HasBillingId {
    fn id(&self) -> u64;
}

impl HasBillingId for RedemptionRecord {
    fn id(&self) -> u64 {
        self.id
    }
}

impl HasBillingId for TopUpRecord {
    fn id(&self) -> u64 {
        self.id
    }
}

fn page<T>(items: Vec<T>, page: PageRequest) -> PageResult<T> {
    let total = items.len();
    let start = page.offset();
    let limit = page.limit();
    let items = items.into_iter().skip(start).take(limit).collect();
    PageResult::new(items, total, page)
}

fn next_id(ids: impl Iterator<Item = u64>) -> u64 {
    ids.max().unwrap_or(0).saturating_add(1).max(1)
}

fn string_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<String, ManagementError> {
    row.try_get::<String, _>(name).map_err(storage_err)
}

fn i64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<i64, ManagementError> {
    row.try_get::<i64, _>(name).map_err(storage_err)
}

fn u64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<u64, ManagementError> {
    i64_col(row, name).map(|value| value.max(0) as u64)
}

fn i32_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<i32, ManagementError> {
    i64_col(row, name).map(|value| value as i32)
}

fn f64_col(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<f64, ManagementError> {
    row.try_get::<f64, _>(name).map_err(storage_err)
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

fn default_redemption_status() -> i32 {
    REDEMPTION_STATUS_ENABLED
}

fn default_redemption_quota() -> i64 {
    100
}

fn default_topup_status() -> String {
    TOPUP_STATUS_PENDING.to_string()
}
