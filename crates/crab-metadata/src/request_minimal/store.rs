use crab_storage::{ETag, Store, StoreLayout};

use crate::error::Result;
use crate::request_minimal::{MAX_ROOT_BYTES, RootRecord};

/// A verified stored root and the provider token protecting its next update.
#[derive(Debug, Clone)]
pub struct RootSnapshot {
    record: RootRecord,
    etag: ETag,
}

impl RootSnapshot {
    /// Return the immutable generation and ref state captured by this snapshot.
    #[must_use]
    pub fn record(&self) -> &RootRecord {
        &self.record
    }

    /// Return the opaque provider token required for a root CAS update.
    #[must_use]
    pub fn etag(&self) -> &ETag {
        &self.etag
    }

    /// Bind a successful root CAS result to its exact predecessor snapshot.
    pub fn committed_successor(&self, record: RootRecord, etag: ETag) -> Result<Self> {
        let generation = self
            .record
            .root()
            .generation()
            .checked_add(1)
            .ok_or_else(|| contract_error("root generation overflowed"))?;
        if record.root().generation() != generation
            || record.root().parent_root_digest() != Some(self.record.digest())
        {
            return Err(contract_error(
                "committed root does not directly extend its CAS snapshot",
            ));
        }
        Ok(Self { record, etag })
    }
}

/// Create the first root at an empty v2 publication key.
pub async fn create_root(router: &StoreLayout<Store>, record: RootRecord) -> Result<RootSnapshot> {
    if record.root().generation() != 0 {
        return Err(contract_error(
            "initial stored root must be generation zero",
        ));
    }
    let etag = router
        .store()
        .create_strict_with_etag(&router.request_minimal_root_path(), record.bytes().clone())
        .await?;
    Ok(RootSnapshot { record, etag })
}

/// Load and verify the single root used by readers and publication CAS.
pub async fn load_root(router: &StoreLayout<Store>) -> Result<RootSnapshot> {
    let (bytes, etag) = router
        .store()
        .get_with_etag_bounded(&router.request_minimal_root_path(), MAX_ROOT_BYTES)
        .await?;
    Ok(RootSnapshot {
        record: RootRecord::decode(bytes)?,
        etag,
    })
}

fn contract_error(reason: impl Into<String>) -> crate::error::MetadataError {
    crate::error::MetadataError::RequestMinimalContract {
        record: "stored root",
        reason: reason.into(),
    }
}
