use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{CatalogDocument, CatalogError, CatalogRecord, CatalogStore};
use crate::{app::MAX_NUMBER, config::valid_repository_description};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Description {
    pub description: String,
    pub version: u64,
}

impl Description {
    pub(super) fn valid_saved(&self) -> bool {
        self.version > 0
            && self.version < MAX_NUMBER
            && valid_repository_description(&self.description)
    }
}

impl CatalogRecord {
    pub(crate) fn description(&self) -> Description {
        self.description_edit
            .clone()
            .unwrap_or_else(|| Description {
                description: self.description.clone(),
                version: 0,
            })
    }
}

impl CatalogStore {
    pub(crate) async fn change_description(
        &self,
        expected_revision: u64,
        repository: Uuid,
        input: Description,
    ) -> Result<CatalogDocument, CatalogError> {
        if !valid_repository_description(&input.description) {
            return Err(CatalogError::Invalid(
                "Enter a single-line description of up to 350 characters",
            ));
        }
        let (mut document, etag) = self.load_for_mutation().await?;
        if document.version != expected_revision {
            return Err(CatalogError::Conflict);
        }
        let record = document
            .repositories
            .iter_mut()
            .find(|record| {
                record.id == repository
                    && record.application == super::RepositoryApplicationState::CellReady
            })
            .ok_or(CatalogError::NotFound)?;
        let current = record.description();
        if current.description == input.description {
            return Ok(document);
        }
        if current.version != input.version || current.version >= MAX_NUMBER - 1 {
            return Err(CatalogError::Conflict);
        }
        record.description_edit = Some(Description {
            description: input.description,
            version: current.version + 1,
        });
        document.version = document
            .version
            .checked_add(1)
            .ok_or(CatalogError::Conflict)?;
        document.validate(&self.root)?;
        if self.write_document(&document, etag).await? {
            Ok(document)
        } else {
            Err(CatalogError::Conflict)
        }
    }
}
