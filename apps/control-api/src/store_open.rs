//! Zero-cost multi-backend store helpers.
//!
//! - Enum dispatch stays monomorphized (`match`, no `dyn`).
//! - Opening stores from `StorageConfig` goes through a single generic path.
//! - Service fan-out across Memory/Sqlite/MySql/Postgres is one macro.

use anyhow::{Context, Result};

use crate::config::{StorageBackend, StorageConfig, normalize_mysql_url};

/// Construct a store backend from resolved URLs (no-arg constructors).
pub(crate) trait OpenStore: Sized {
    type Error: Into<anyhow::Error> + Send + Sync + 'static;

    fn open_memory() -> Self;

    fn open_sqlite(
        url: &str,
    ) -> impl std::future::Future<Output = Result<Self, Self::Error>> + Send;

    fn open_mysql(
        url: &str,
    ) -> impl std::future::Future<Output = Result<Self, Self::Error>> + Send;

    fn open_postgres(
        url: &str,
    ) -> impl std::future::Future<Output = Result<Self, Self::Error>> + Send;
}

/// Construct a store that needs a seed value (management/catalog/options).
pub(crate) trait OpenStoreSeeded<S>: Sized {
    type Error: Into<anyhow::Error> + Send + Sync + 'static;

    fn open_memory(seed: S) -> Self;

    fn open_sqlite(
        url: &str,
        seed: S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Error>> + Send;

    fn open_mysql(
        url: &str,
        seed: S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Error>> + Send;

    fn open_postgres(
        url: &str,
        seed: S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Error>> + Send;
}

/// Open any [`OpenStore`] from storage config (static dispatch).
pub(crate) async fn open_from_config<T>(storage: &StorageConfig) -> Result<T>
where
    T: OpenStore,
{
    match storage.backend {
        StorageBackend::Memory => Ok(T::open_memory()),
        StorageBackend::Sqlite => {
            let url = storage
                .sqlite_url
                .as_deref()
                .context("storage.sqlite_url is required when storage.backend = sqlite")?;
            T::open_sqlite(url).await.map_err(Into::into)
        }
        StorageBackend::Postgres => {
            let url = storage
                .database_url
                .as_deref()
                .context("storage.database_url is required when storage.backend = postgres")?;
            T::open_postgres(url).await.map_err(Into::into)
        }
        StorageBackend::MySql => {
            let url = storage
                .database_url
                .as_deref()
                .context("storage.database_url is required when storage.backend = mysql")?;
            let url = normalize_mysql_url(url);
            T::open_mysql(&url).await.map_err(Into::into)
        }
    }
}

/// Open any [`OpenStoreSeeded`] from storage config.
pub(crate) async fn open_seeded_from_config<T, S>(storage: &StorageConfig, seed: S) -> Result<T>
where
    T: OpenStoreSeeded<S>,
{
    match storage.backend {
        StorageBackend::Memory => Ok(T::open_memory(seed)),
        StorageBackend::Sqlite => {
            let url = storage
                .sqlite_url
                .as_deref()
                .context("storage.sqlite_url is required when storage.backend = sqlite")?;
            T::open_sqlite(url, seed).await.map_err(Into::into)
        }
        StorageBackend::Postgres => {
            let url = storage
                .database_url
                .as_deref()
                .context("storage.database_url is required when storage.backend = postgres")?;
            T::open_postgres(url, seed).await.map_err(Into::into)
        }
        StorageBackend::MySql => {
            let url = storage
                .database_url
                .as_deref()
                .context("storage.database_url is required when storage.backend = mysql")?;
            let url = normalize_mysql_url(url);
            T::open_mysql(&url, seed).await.map_err(Into::into)
        }
    }
}

/// `Service` impl that fans out to Memory/Sqlite/MySql/Postgres arms.
#[macro_export]
macro_rules! impl_backend_service {
    ($store:ty, $req:ty, $resp:ty) => {
        impl ::service_async::Service<$req> for $store {
            type Response = $resp;
            type Error = ::halolake_control_plane::ManagementError;

            async fn call(&self, req: $req) -> Result<Self::Response, Self::Error> {
                match self {
                    Self::Memory(s) => ::service_async::Service::<$req>::call(s, req).await,
                    Self::Sqlite(s) => ::service_async::Service::<$req>::call(s, req).await,
                    Self::MySql(s) => ::service_async::Service::<$req>::call(s, req).await,
                    Self::Postgres(s) => ::service_async::Service::<$req>::call(s, req).await,
                }
            }
        }
    };
    // Shorthand: multiple request types share the same response type pattern via TT muncher
    ($store:ty, { $($req:ty => $resp:ty),+ $(,)? }) => {
        $(
            $crate::impl_backend_service!($store, $req, $resp);
        )+
    };
}


// --- Backend open adapters (static dispatch) ---

use crate::{
    billing::BillingStore,
    catalog::{CatalogData, CatalogStore},
    checkin::CheckinStore,
    prefill::PrefillStore,
    proxy::ProxyStore,
    security::SecurityStore,
    session::SessionStore,
    storage::{ManagementStore, OptionStore, UsageStore},
    system_instance::SystemInstanceStore,
    system_task::SystemTaskStore,
};
use halolake_control_plane::{ManagementData, ManagementError};
use std::collections::BTreeMap;

macro_rules! impl_open_store {
    ($ty:ty) => {
        impl OpenStore for $ty {
            type Error = ManagementError;

            fn open_memory() -> Self {
                Self::memory()
            }

            async fn open_sqlite(url: &str) -> Result<Self, Self::Error> {
                Self::sqlite(url).await
            }

            async fn open_mysql(url: &str) -> Result<Self, Self::Error> {
                Self::mysql(url).await
            }

            async fn open_postgres(url: &str) -> Result<Self, Self::Error> {
                Self::postgres(url).await
            }
        }
    };
}

impl_open_store!(PrefillStore);
impl_open_store!(ProxyStore);
impl_open_store!(SessionStore);
impl_open_store!(BillingStore);
impl_open_store!(CheckinStore);
impl_open_store!(SecurityStore);
impl_open_store!(SystemTaskStore);
impl_open_store!(SystemInstanceStore);

impl OpenStore for UsageStore {
    type Error = halolake_control_plane::UsageError;

    fn open_memory() -> Self {
        Self::memory()
    }

    async fn open_sqlite(url: &str) -> Result<Self, Self::Error> {
        Self::sqlite(url).await
    }

    async fn open_mysql(url: &str) -> Result<Self, Self::Error> {
        Self::mysql(url).await
    }

    async fn open_postgres(url: &str) -> Result<Self, Self::Error> {
        Self::postgres(url).await
    }
}

impl OpenStoreSeeded<ManagementData> for ManagementStore {
    type Error = ManagementError;

    fn open_memory(seed: ManagementData) -> Self {
        Self::memory(seed)
    }

    async fn open_sqlite(url: &str, seed: ManagementData) -> Result<Self, Self::Error> {
        Self::sqlite(url, seed).await
    }

    async fn open_mysql(url: &str, seed: ManagementData) -> Result<Self, Self::Error> {
        Self::mysql(url, seed).await
    }

    async fn open_postgres(url: &str, seed: ManagementData) -> Result<Self, Self::Error> {
        Self::postgres(url, seed).await
    }
}

impl OpenStoreSeeded<CatalogData> for CatalogStore {
    type Error = ManagementError;

    fn open_memory(seed: CatalogData) -> Self {
        Self::memory(seed)
    }

    async fn open_sqlite(url: &str, seed: CatalogData) -> Result<Self, Self::Error> {
        Self::sqlite(url, seed).await
    }

    async fn open_mysql(url: &str, seed: CatalogData) -> Result<Self, Self::Error> {
        Self::mysql(url, seed).await
    }

    async fn open_postgres(url: &str, seed: CatalogData) -> Result<Self, Self::Error> {
        Self::postgres(url, seed).await
    }
}

impl OpenStoreSeeded<BTreeMap<String, String>> for OptionStore {
    type Error = ManagementError;

    fn open_memory(seed: BTreeMap<String, String>) -> Self {
        Self::memory(seed)
    }

    async fn open_sqlite(
        url: &str,
        seed: BTreeMap<String, String>,
    ) -> Result<Self, Self::Error> {
        Self::sqlite(url, seed).await
    }

    async fn open_mysql(
        url: &str,
        seed: BTreeMap<String, String>,
    ) -> Result<Self, Self::Error> {
        Self::mysql(url, seed).await
    }

    async fn open_postgres(
        url: &str,
        seed: BTreeMap<String, String>,
    ) -> Result<Self, Self::Error> {
        Self::postgres(url, seed).await
    }
}
