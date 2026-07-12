//! SQL and memory backends for control-plane management data.
//!
//! Extracted from `halolake-control-api` for compile isolation and reuse.

/// Fan-out `Service` impl across Memory/Sqlite/MySql/Postgres store variants.
#[macro_export]
macro_rules! impl_storage_backend_service {
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
    ($store:ty, { $($req:ty => $resp:ty),+ $(,)? }) => {
        $(
            $crate::impl_storage_backend_service!($store, $req, $resp);
        )+
    };
}

mod management;
mod options;
mod usage;

pub use management::ManagementStore;
pub use options::{ListOptionsRequest, OptionRecord, OptionStore, UpdateOptionRequest};
pub use usage::{DeleteUsageAck, DeleteUsageBeforeRequest, RecordedUsageBatch, UsageStore};
