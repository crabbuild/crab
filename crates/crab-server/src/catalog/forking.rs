use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{CatalogDocument, CatalogError, CatalogStore};
use crate::app::MAX_NUMBER;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Forking {
    pub version: u64,
    pub allowed: bool,
}

impl Default for Forking {
    fn default() -> Self {
        Self {
            version: 0,
            allowed: true,
        }
    }
}

impl Forking {
    pub(super) fn is_initial(&self) -> bool {
        *self == Self::default()
    }

    pub(super) fn valid_saved(&self) -> bool {
        self.is_initial() || (self.version > 0 && self.version < MAX_NUMBER)
    }
}

impl CatalogStore {
    pub(crate) async fn change_forking(
        &self,
        revision: u64,
        repository: Uuid,
        input: Forking,
    ) -> Result<CatalogDocument, CatalogError> {
        self.edit_metadata(revision, repository, |record| {
            let current = record.forking;
            if input.allowed == current.allowed {
                return Ok(false);
            }
            if input.version != current.version || current.version >= MAX_NUMBER - 1 {
                return Err(CatalogError::Conflict);
            }
            record.forking = Forking {
                version: current.version + 1,
                allowed: input.allowed,
            };
            Ok(true)
        })
        .await
    }
}
