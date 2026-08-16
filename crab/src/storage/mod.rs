//! Object-store transport helpers and upload pool.

pub mod head_cache;
pub mod head_class;
pub mod resolver;
pub mod retry;
pub mod store;

#[cfg(feature = "testing")]
pub mod testing;

pub use crab_storage::ObjectType;
pub use resolver::{ResolvedObjectStore, resolve_object_url_store};
pub use retry::{RetryClass, RetryPolicy, retry, retry_class};
pub use store::{ETag, Store};
pub type StoreLayout = crab_storage::StoreLayout<store::Store>;
