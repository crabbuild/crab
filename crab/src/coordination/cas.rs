//! CLI-facing Adapter for storage-domain JSON CAS updates.
//!
//! The implementation lives in `crab-storage`; this module preserves the
//! existing `CrabError` Interface for callers that still import
//! `crab::coordination::cas`.

use serde::{Deserialize, Serialize};

use crate::core::error::{CrabError, Result};
use crate::storage::store::Store;

pub use crab_storage::{DEFAULT_MAX_ATTEMPTS, MAX_CAS_OBJECT_BYTES};

/// Updates one JSON object with a load, mutate, conditional-write loop.
/// The default loop rejects objects larger than [`MAX_CAS_OBJECT_BYTES`].
///
/// # Errors
///
/// Returns [`CrabError::CasConflict`] if all attempts are exhausted.
pub async fn cas_update<T, F>(store: &Store, path: &str, max_attempts: u32, mutate: F) -> Result<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Default + Clone,
    F: Fn(&mut T),
{
    let storage_store = store.clone().into_storage();
    crab_storage::cas_update(&storage_store, path, max_attempts, mutate)
        .await
        .map_err(CrabError::from)
}

/// Updates one JSON object using the default CAS attempt budget.
///
/// # Errors
///
/// Returns [`CrabError::CasConflict`] if all attempts are exhausted.
pub async fn cas_update_default<T, F>(store: &Store, path: &str, mutate: F) -> Result<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Default + Clone,
    F: Fn(&mut T),
{
    let storage_store = store.clone().into_storage();
    crab_storage::cas_update_default(&storage_store, path, mutate)
        .await
        .map_err(CrabError::from)
}
