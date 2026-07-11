use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::{Arc, RwLock},
};

use halolake_control_plane::{ManagementData, ManagementError};
use halolake_domain::{ChannelRecord, PageRequest, PageResult, STATUS_ENABLED, SearchRequest};
use serde::{Deserialize, Serialize};
use service_async::Service;
use sqlx::{
    Row, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

const NAME_RULE_EXACT: i32 = 0;
const NAME_RULE_PREFIX: i32 = 1;
const NAME_RULE_CONTAINS: i32 = 2;
const NAME_RULE_SUFFIX: i32 = 3;

#[derive(Debug, Clone, Default)]
pub(crate) struct CatalogData {
    pub(crate) vendors: Vec<VendorRecord>,
    pub(crate) models: Vec<ModelRecord>,
}

impl CatalogData {
    pub(crate) fn from_management(data: &ManagementData) -> Self {
        let models = enabled_model_names(data)
            .into_iter()
            .enumerate()
            .map(|(idx, model_name)| ModelRecord {
                id: (idx + 1) as u64,
                model_name,
                description: String::new(),
                icon: String::new(),
                tags: String::new(),
                vendor_id: 0,
                endpoints: String::new(),
                status: STATUS_ENABLED,
                sync_official: 0,
                created_time: 0,
                updated_time: 0,
                bound_channels: Vec::new(),
                enable_groups: Vec::new(),
                quota_types: Vec::new(),
                name_rule: NAME_RULE_EXACT,
                matched_models: Vec::new(),
                matched_count: 0,
            })
            .collect();

        Self {
            vendors: Vec::new(),
            models,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct VendorRecord {
    #[serde(default)]
    pub(crate) id: u64,
    pub(crate) name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) icon: String,
    #[serde(default = "default_status")]
    pub(crate) status: i32,
    #[serde(default)]
    pub(crate) created_time: i64,
    #[serde(default)]
    pub(crate) updated_time: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct BoundChannel {
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) channel_type: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ModelRecord {
    #[serde(default)]
    pub(crate) id: u64,
    #[serde(default)]
    pub(crate) model_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) icon: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) tags: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) vendor_id: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) endpoints: String,
    #[serde(default = "default_status")]
    pub(crate) status: i32,
    #[serde(default = "default_status")]
    pub(crate) sync_official: i32,
    #[serde(default)]
    pub(crate) created_time: i64,
    #[serde(default)]
    pub(crate) updated_time: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) bound_channels: Vec<BoundChannel>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) enable_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) quota_types: Vec<i32>,
    #[serde(default)]
    pub(crate) name_rule: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) matched_models: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub(crate) matched_count: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ListVendorsRequest {
    pub(crate) page: PageRequest,
}

#[derive(Debug, Clone)]
pub(crate) struct SearchVendorsRequest {
    pub(crate) search: SearchRequest,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GetVendorRequest {
    pub(crate) id: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct CreateVendorRequest {
    pub(crate) vendor: VendorRecord,
}

#[derive(Debug, Clone)]
pub(crate) struct UpdateVendorRequest {
    pub(crate) vendor: VendorRecord,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeleteVendorRequest {
    pub(crate) id: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ListModelsRequest {
    pub(crate) page: PageRequest,
}

#[derive(Debug, Clone)]
pub(crate) struct SearchModelsRequest {
    pub(crate) search: SearchRequest,
    pub(crate) vendor: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GetModelRequest {
    pub(crate) id: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct CreateModelRequest {
    pub(crate) model: ModelRecord,
}

#[derive(Debug, Clone)]
pub(crate) struct UpdateModelRequest {
    pub(crate) model: ModelRecord,
    pub(crate) status_only: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeleteModelRequest {
    pub(crate) id: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct VendorModelCountsRequest;

#[derive(Debug, Clone)]
pub(crate) enum CatalogStore {
    Memory(MemoryCatalogStore),
    Sqlite(SqliteCatalogStore),
}

impl CatalogStore {
    pub(crate) fn memory(data: CatalogData) -> Self {
        Self::Memory(MemoryCatalogStore::new(data))
    }

    pub(crate) async fn sqlite(url: &str, seed: CatalogData) -> Result<Self, ManagementError> {
        Ok(Self::Sqlite(SqliteCatalogStore::connect(url, seed).await?))
    }

    pub(crate) fn current_data(&self) -> Result<CatalogData, ManagementError> {
        match self {
            Self::Memory(store) => store.current_data(),
            Self::Sqlite(store) => store.current_data(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MemoryCatalogStore {
    inner: Arc<RwLock<CatalogData>>,
}

impl MemoryCatalogStore {
    fn new(data: CatalogData) -> Self {
        Self {
            inner: Arc::new(RwLock::new(data)),
        }
    }

    fn current_data(&self) -> Result<CatalogData, ManagementError> {
        self.inner
            .read()
            .map(|data| data.clone())
            .map_err(|_| ManagementError::Poisoned("catalog"))
    }

    fn mutate<F, T>(&self, f: F) -> Result<T, ManagementError>
    where
        F: FnOnce(&mut CatalogData) -> Result<T, ManagementError>,
    {
        let mut data = self
            .inner
            .write()
            .map_err(|_| ManagementError::Poisoned("catalog"))?;
        f(&mut data)
    }
}

macro_rules! impl_catalog_service {
    ($req:ty, $resp:ty) => {
        impl Service<$req> for CatalogStore {
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

impl_catalog_service!(ListVendorsRequest, PageResult<VendorRecord>);
impl_catalog_service!(SearchVendorsRequest, PageResult<VendorRecord>);
impl_catalog_service!(GetVendorRequest, VendorRecord);
impl_catalog_service!(CreateVendorRequest, VendorRecord);
impl_catalog_service!(UpdateVendorRequest, VendorRecord);
impl_catalog_service!(DeleteVendorRequest, ());
impl_catalog_service!(ListModelsRequest, PageResult<ModelRecord>);
impl_catalog_service!(SearchModelsRequest, PageResult<ModelRecord>);
impl_catalog_service!(GetModelRequest, ModelRecord);
impl_catalog_service!(CreateModelRequest, ModelRecord);
impl_catalog_service!(UpdateModelRequest, ModelRecord);
impl_catalog_service!(DeleteModelRequest, ());
impl_catalog_service!(VendorModelCountsRequest, BTreeMap<u64, usize>);

impl Service<ListVendorsRequest> for MemoryCatalogStore {
    type Response = PageResult<VendorRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListVendorsRequest) -> Result<Self::Response, Self::Error> {
        let mut vendors = self.current_data()?.vendors;
        vendors.sort_by_key(|vendor| std::cmp::Reverse(vendor.id));
        Ok(page(vendors, req.page))
    }
}

impl Service<SearchVendorsRequest> for MemoryCatalogStore {
    type Response = PageResult<VendorRecord>;
    type Error = ManagementError;

    async fn call(&self, req: SearchVendorsRequest) -> Result<Self::Response, Self::Error> {
        let keyword = req.search.keyword.trim().to_ascii_lowercase();
        let mut vendors = self
            .current_data()?
            .vendors
            .into_iter()
            .filter(|vendor| {
                keyword.is_empty()
                    || vendor.name.to_ascii_lowercase().contains(keyword.as_str())
                    || vendor
                        .description
                        .to_ascii_lowercase()
                        .contains(keyword.as_str())
            })
            .collect::<Vec<_>>();
        vendors.sort_by_key(|vendor| std::cmp::Reverse(vendor.id));
        Ok(page(vendors, req.search.page))
    }
}

impl Service<GetVendorRequest> for MemoryCatalogStore {
    type Response = VendorRecord;
    type Error = ManagementError;

    async fn call(&self, req: GetVendorRequest) -> Result<Self::Response, Self::Error> {
        self.current_data()?
            .vendors
            .into_iter()
            .find(|vendor| vendor.id == req.id)
            .ok_or(ManagementError::NotFound)
    }
}

impl Service<CreateVendorRequest> for MemoryCatalogStore {
    type Response = VendorRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateVendorRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let mut vendor = req.vendor;
            normalize_vendor_for_write(&mut vendor)?;
            if data
                .vendors
                .iter()
                .any(|item| item.name == vendor.name || item.id == vendor.id && vendor.id != 0)
            {
                return Err(ManagementError::Duplicate);
            }
            if vendor.id == 0 {
                vendor.id = next_id(data.vendors.iter().map(|vendor| vendor.id));
            }
            fill_record_times(
                vendor.created_time,
                &mut vendor.created_time,
                &mut vendor.updated_time,
            );
            data.vendors.push(vendor.clone());
            Ok(vendor)
        })
    }
}

impl Service<UpdateVendorRequest> for MemoryCatalogStore {
    type Response = VendorRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateVendorRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let mut vendor = req.vendor;
            if vendor.id == 0 {
                return Err(ManagementError::InvalidRequest("vendor id is required"));
            }
            normalize_vendor_for_write(&mut vendor)?;
            if data
                .vendors
                .iter()
                .any(|item| item.id != vendor.id && item.name == vendor.name)
            {
                return Err(ManagementError::Duplicate);
            }
            let current = data
                .vendors
                .iter_mut()
                .find(|item| item.id == vendor.id)
                .ok_or(ManagementError::NotFound)?;
            if vendor.created_time == 0 {
                vendor.created_time = current.created_time;
            }
            vendor.updated_time = now_unix();
            *current = vendor.clone();
            Ok(vendor)
        })
    }
}

impl Service<DeleteVendorRequest> for MemoryCatalogStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeleteVendorRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let before = data.vendors.len();
            data.vendors.retain(|vendor| vendor.id != req.id);
            if before == data.vendors.len() {
                return Err(ManagementError::NotFound);
            }
            for model in &mut data.models {
                if model.vendor_id == req.id {
                    model.vendor_id = 0;
                }
            }
            Ok(())
        })
    }
}

impl Service<ListModelsRequest> for MemoryCatalogStore {
    type Response = PageResult<ModelRecord>;
    type Error = ManagementError;

    async fn call(&self, req: ListModelsRequest) -> Result<Self::Response, Self::Error> {
        let mut models = self.current_data()?.models;
        models.sort_by_key(|model| std::cmp::Reverse(model.id));
        Ok(page(models, req.page))
    }
}

impl Service<SearchModelsRequest> for MemoryCatalogStore {
    type Response = PageResult<ModelRecord>;
    type Error = ManagementError;

    async fn call(&self, req: SearchModelsRequest) -> Result<Self::Response, Self::Error> {
        let data = self.current_data()?;
        let keyword = req.search.keyword.trim().to_ascii_lowercase();
        let vendor_filter = req.vendor.trim().to_ascii_lowercase();
        let vendor_ids = matching_vendor_ids(&data.vendors, &vendor_filter);
        let mut models = data
            .models
            .into_iter()
            .filter(|model| {
                keyword.is_empty()
                    || model
                        .model_name
                        .to_ascii_lowercase()
                        .contains(keyword.as_str())
                    || model
                        .description
                        .to_ascii_lowercase()
                        .contains(keyword.as_str())
                    || model.tags.to_ascii_lowercase().contains(keyword.as_str())
            })
            .filter(|model| vendor_filter.is_empty() || vendor_ids.contains(&model.vendor_id))
            .collect::<Vec<_>>();
        models.sort_by_key(|model| std::cmp::Reverse(model.id));
        Ok(page(models, req.search.page))
    }
}

impl Service<GetModelRequest> for MemoryCatalogStore {
    type Response = ModelRecord;
    type Error = ManagementError;

    async fn call(&self, req: GetModelRequest) -> Result<Self::Response, Self::Error> {
        self.current_data()?
            .models
            .into_iter()
            .find(|model| model.id == req.id)
            .ok_or(ManagementError::NotFound)
    }
}

impl Service<CreateModelRequest> for MemoryCatalogStore {
    type Response = ModelRecord;
    type Error = ManagementError;

    async fn call(&self, req: CreateModelRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let mut model = req.model;
            normalize_model_for_write(&mut model)?;
            if data.models.iter().any(|item| {
                item.model_name == model.model_name || item.id == model.id && model.id != 0
            }) {
                return Err(ManagementError::Duplicate);
            }
            if model.id == 0 {
                model.id = next_id(data.models.iter().map(|model| model.id));
            }
            fill_record_times(
                model.created_time,
                &mut model.created_time,
                &mut model.updated_time,
            );
            data.models.push(model.clone());
            Ok(model)
        })
    }
}

impl Service<UpdateModelRequest> for MemoryCatalogStore {
    type Response = ModelRecord;
    type Error = ManagementError;

    async fn call(&self, req: UpdateModelRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let model_idx = data
                .models
                .iter()
                .position(|model| model.id == req.model.id)
                .ok_or(ManagementError::NotFound)?;
            if req.status_only {
                data.models[model_idx].status = req.model.status;
                data.models[model_idx].updated_time = now_unix();
                return Ok(data.models[model_idx].clone());
            }
            let mut model = req.model;
            if model.id == 0 {
                return Err(ManagementError::InvalidRequest("model id is required"));
            }
            normalize_model_for_write(&mut model)?;
            if data
                .models
                .iter()
                .any(|item| item.id != model.id && item.model_name == model.model_name)
            {
                return Err(ManagementError::Duplicate);
            }
            if model.created_time == 0 {
                model.created_time = data.models[model_idx].created_time;
            }
            model.updated_time = now_unix();
            data.models[model_idx] = model.clone();
            Ok(model)
        })
    }
}

impl Service<DeleteModelRequest> for MemoryCatalogStore {
    type Response = ();
    type Error = ManagementError;

    async fn call(&self, req: DeleteModelRequest) -> Result<Self::Response, Self::Error> {
        self.mutate(|data| {
            let before = data.models.len();
            data.models.retain(|model| model.id != req.id);
            if before == data.models.len() {
                return Err(ManagementError::NotFound);
            }
            Ok(())
        })
    }
}

impl Service<VendorModelCountsRequest> for MemoryCatalogStore {
    type Response = BTreeMap<u64, usize>;
    type Error = ManagementError;

    async fn call(&self, _req: VendorModelCountsRequest) -> Result<Self::Response, Self::Error> {
        let mut counts = BTreeMap::new();
        for model in self.current_data()?.models {
            *counts.entry(model.vendor_id).or_insert(0usize) += 1;
        }
        Ok(counts)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SqliteCatalogStore {
    pool: SqlitePool,
    memory: MemoryCatalogStore,
}

impl SqliteCatalogStore {
    async fn connect(url: &str, seed: CatalogData) -> Result<Self, ManagementError> {
        let options = SqliteConnectOptions::from_str(url)
            .map_err(storage_err)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(storage_err)?;
        migrate_catalog(&pool).await?;

        let data = if catalog_is_empty(&pool).await? {
            save_catalog(&pool, &seed).await?;
            seed
        } else {
            load_catalog(&pool).await?
        };

        Ok(Self {
            pool,
            memory: MemoryCatalogStore::new(data),
        })
    }

    fn current_data(&self) -> Result<CatalogData, ManagementError> {
        self.memory.current_data()
    }

    async fn persist(&self) -> Result<(), ManagementError> {
        let data = self.memory.current_data()?;
        save_catalog(&self.pool, &data).await
    }
}

macro_rules! impl_sqlite_read_service {
    ($req:ty, $resp:ty) => {
        impl Service<$req> for SqliteCatalogStore {
            type Response = $resp;
            type Error = ManagementError;

            async fn call(&self, req: $req) -> Result<Self::Response, Self::Error> {
                self.memory.call(req).await
            }
        }
    };
}

macro_rules! impl_sqlite_write_service {
    ($req:ty, $resp:ty) => {
        impl Service<$req> for SqliteCatalogStore {
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

impl_sqlite_read_service!(ListVendorsRequest, PageResult<VendorRecord>);
impl_sqlite_read_service!(SearchVendorsRequest, PageResult<VendorRecord>);
impl_sqlite_read_service!(GetVendorRequest, VendorRecord);
impl_sqlite_read_service!(ListModelsRequest, PageResult<ModelRecord>);
impl_sqlite_read_service!(SearchModelsRequest, PageResult<ModelRecord>);
impl_sqlite_read_service!(GetModelRequest, ModelRecord);
impl_sqlite_read_service!(VendorModelCountsRequest, BTreeMap<u64, usize>);

impl_sqlite_write_service!(CreateVendorRequest, VendorRecord);
impl_sqlite_write_service!(UpdateVendorRequest, VendorRecord);
impl_sqlite_write_service!(DeleteVendorRequest, ());
impl_sqlite_write_service!(CreateModelRequest, ModelRecord);
impl_sqlite_write_service!(UpdateModelRequest, ModelRecord);
impl_sqlite_write_service!(DeleteModelRequest, ());

pub(crate) fn enrich_models(models: &mut [ModelRecord], management: &ManagementData) {
    let all_models = enabled_model_names(management);
    let exact_channels = all_models
        .iter()
        .map(|model| {
            (
                model.clone(),
                channels_for_model(model, &management.channels),
            )
        })
        .collect::<BTreeMap<_, _>>();

    for model in models {
        if model.name_rule == NAME_RULE_EXACT {
            let channels = exact_channels
                .get(&model.model_name)
                .cloned()
                .unwrap_or_default();
            fill_enriched_fields(model, &channels);
            continue;
        }

        let mut matched_models = Vec::new();
        let mut channels = Vec::new();
        for candidate in &all_models {
            if model_name_rule_matches(model.name_rule, &model.model_name, candidate) {
                matched_models.push(candidate.clone());
                channels.extend(channels_for_model(candidate, &management.channels));
            }
        }
        channels.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then(left.channel_type.cmp(&right.channel_type))
        });
        channels.dedup_by(|left, right| {
            left.name == right.name && left.channel_type == right.channel_type
        });
        model.matched_count = matched_models.len();
        model.matched_models = matched_models;
        fill_enriched_fields(model, &channels);
    }
}

pub(crate) fn missing_models(management: &ManagementData, catalog: &CatalogData) -> Vec<String> {
    let existing = catalog
        .models
        .iter()
        .filter(|model| model.name_rule == NAME_RULE_EXACT)
        .map(|model| model.model_name.clone())
        .collect::<BTreeSet<_>>();
    enabled_model_names(management)
        .into_iter()
        .filter(|model| !existing.contains(model))
        .collect()
}

async fn migrate_catalog(pool: &SqlitePool) -> Result<(), ManagementError> {
    for stmt in [
        "CREATE TABLE IF NOT EXISTS vendors (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            description TEXT NOT NULL DEFAULT '',
            icon TEXT NOT NULL DEFAULT '',
            status INTEGER NOT NULL DEFAULT 1,
            created_time INTEGER NOT NULL DEFAULT 0,
            updated_time INTEGER NOT NULL DEFAULT 0
        )",
        "CREATE TABLE IF NOT EXISTS models (
            id INTEGER PRIMARY KEY,
            model_name TEXT NOT NULL UNIQUE,
            description TEXT NOT NULL DEFAULT '',
            icon TEXT NOT NULL DEFAULT '',
            tags TEXT NOT NULL DEFAULT '',
            vendor_id INTEGER NOT NULL DEFAULT 0,
            endpoints TEXT NOT NULL DEFAULT '',
            status INTEGER NOT NULL DEFAULT 1,
            sync_official INTEGER NOT NULL DEFAULT 1,
            created_time INTEGER NOT NULL DEFAULT 0,
            updated_time INTEGER NOT NULL DEFAULT 0,
            name_rule INTEGER NOT NULL DEFAULT 0
        )",
    ] {
        sqlx::query(stmt).execute(pool).await.map_err(storage_err)?;
    }
    Ok(())
}

async fn catalog_is_empty(pool: &SqlitePool) -> Result<bool, ManagementError> {
    let row = sqlx::query("SELECT COUNT(*) AS count FROM models")
        .fetch_one(pool)
        .await
        .map_err(storage_err)?;
    Ok(row.try_get::<i64, _>("count").map_err(storage_err)? == 0)
}

async fn load_catalog(pool: &SqlitePool) -> Result<CatalogData, ManagementError> {
    let vendors = sqlx::query(
        "SELECT id, name, description, icon, status, created_time, updated_time
         FROM vendors ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(VendorRecord {
            id: u64_col(&row, "id")?,
            name: string_col(&row, "name")?,
            description: string_col(&row, "description")?,
            icon: string_col(&row, "icon")?,
            status: i32_col(&row, "status")?,
            created_time: i64_col(&row, "created_time")?,
            updated_time: i64_col(&row, "updated_time")?,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    let models = sqlx::query(
        "SELECT id, model_name, description, icon, tags, vendor_id, endpoints, status,
            sync_official, created_time, updated_time, name_rule
         FROM models ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(storage_err)?
    .into_iter()
    .map(|row| {
        Ok(ModelRecord {
            id: u64_col(&row, "id")?,
            model_name: string_col(&row, "model_name")?,
            description: string_col(&row, "description")?,
            icon: string_col(&row, "icon")?,
            tags: string_col(&row, "tags")?,
            vendor_id: u64_col(&row, "vendor_id")?,
            endpoints: string_col(&row, "endpoints")?,
            status: i32_col(&row, "status")?,
            sync_official: i32_col(&row, "sync_official")?,
            created_time: i64_col(&row, "created_time")?,
            updated_time: i64_col(&row, "updated_time")?,
            bound_channels: Vec::new(),
            enable_groups: Vec::new(),
            quota_types: Vec::new(),
            name_rule: i32_col(&row, "name_rule")?,
            matched_models: Vec::new(),
            matched_count: 0,
        })
    })
    .collect::<Result<Vec<_>, ManagementError>>()?;

    Ok(CatalogData { vendors, models })
}

async fn save_catalog(pool: &SqlitePool, data: &CatalogData) -> Result<(), ManagementError> {
    let mut tx = pool.begin().await.map_err(storage_err)?;
    save_catalog_tx(&mut tx, data).await?;
    tx.commit().await.map_err(storage_err)
}

async fn save_catalog_tx(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    data: &CatalogData,
) -> Result<(), ManagementError> {
    sqlx::query("DELETE FROM models")
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;
    sqlx::query("DELETE FROM vendors")
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;

    for vendor in &data.vendors {
        sqlx::query(
            "INSERT INTO vendors (id, name, description, icon, status, created_time, updated_time)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(vendor.id as i64)
        .bind(&vendor.name)
        .bind(&vendor.description)
        .bind(&vendor.icon)
        .bind(vendor.status as i64)
        .bind(vendor.created_time)
        .bind(vendor.updated_time)
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;
    }

    for model in &data.models {
        sqlx::query(
            "INSERT INTO models (
                id, model_name, description, icon, tags, vendor_id, endpoints, status,
                sync_official, created_time, updated_time, name_rule
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(model.id as i64)
        .bind(&model.model_name)
        .bind(&model.description)
        .bind(&model.icon)
        .bind(&model.tags)
        .bind(model.vendor_id as i64)
        .bind(&model.endpoints)
        .bind(model.status as i64)
        .bind(model.sync_official as i64)
        .bind(model.created_time)
        .bind(model.updated_time)
        .bind(model.name_rule as i64)
        .execute(&mut **tx)
        .await
        .map_err(storage_err)?;
    }
    Ok(())
}

fn normalize_vendor_for_write(vendor: &mut VendorRecord) -> Result<(), ManagementError> {
    vendor.name = vendor.name.trim().to_string();
    if vendor.name.is_empty() {
        return Err(ManagementError::InvalidRequest("vendor name is required"));
    }
    Ok(())
}

fn normalize_model_for_write(model: &mut ModelRecord) -> Result<(), ManagementError> {
    model.model_name = model.model_name.trim().to_string();
    if model.model_name.is_empty() {
        return Err(ManagementError::InvalidRequest("model name is required"));
    }
    if !matches!(
        model.name_rule,
        NAME_RULE_EXACT | NAME_RULE_PREFIX | NAME_RULE_CONTAINS | NAME_RULE_SUFFIX
    ) {
        return Err(ManagementError::InvalidRequest("invalid model name rule"));
    }
    Ok(())
}

fn fill_record_times(seed_created_time: i64, created_time: &mut i64, updated_time: &mut i64) {
    let now = now_unix();
    if *created_time == 0 {
        *created_time = if seed_created_time == 0 {
            now
        } else {
            seed_created_time
        };
    }
    if *updated_time == 0 {
        *updated_time = now;
    }
}

fn matching_vendor_ids(vendors: &[VendorRecord], vendor_filter: &str) -> BTreeSet<u64> {
    if vendor_filter.is_empty() {
        return BTreeSet::new();
    }
    if let Ok(id) = vendor_filter.parse::<u64>() {
        return [id].into_iter().collect();
    }
    vendors
        .iter()
        .filter(|vendor| vendor.name.to_ascii_lowercase().contains(vendor_filter))
        .map(|vendor| vendor.id)
        .collect()
}

fn fill_enriched_fields(model: &mut ModelRecord, channels: &[BoundChannel]) {
    model.bound_channels = channels.to_vec();
    let mut groups = BTreeSet::new();
    if !channels.is_empty() {
        groups.insert("default".to_string());
        model.quota_types = vec![0];
    }
    model.enable_groups = groups.into_iter().collect();
}

fn channels_for_model(model: &str, channels: &[ChannelRecord]) -> Vec<BoundChannel> {
    let mut out = channels
        .iter()
        .filter(|channel| channel.status == STATUS_ENABLED)
        .filter(|channel| channel.model_list().iter().any(|item| item == model))
        .map(|channel| BoundChannel {
            name: channel.name.clone(),
            channel_type: channel.channel_type,
        })
        .collect::<Vec<_>>();
    out.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.channel_type.cmp(&right.channel_type))
    });
    out.dedup_by(|left, right| left.name == right.name && left.channel_type == right.channel_type);
    out
}

fn model_name_rule_matches(rule: i32, pattern: &str, model: &str) -> bool {
    match rule {
        NAME_RULE_PREFIX => model.starts_with(pattern),
        NAME_RULE_CONTAINS => model.contains(pattern),
        NAME_RULE_SUFFIX => model.ends_with(pattern),
        _ => model == pattern,
    }
}

fn enabled_model_names(data: &ManagementData) -> Vec<String> {
    let mut names = data
        .channels
        .iter()
        .filter(|channel| channel.status == STATUS_ENABLED)
        .flat_map(ChannelRecord::model_list)
        .chain(
            data.model_mappings
                .iter()
                .map(|mapping| mapping.requested_model.clone()),
        )
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    names.sort();
    names
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

fn storage_err(err: impl std::fmt::Display) -> ManagementError {
    ManagementError::Storage(err.to_string())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn default_status() -> i32 {
    STATUS_ENABLED
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}
