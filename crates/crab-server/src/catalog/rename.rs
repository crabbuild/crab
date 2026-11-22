use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    CatalogDocument, CatalogError, CatalogRecord, CatalogStore, RepositoryApplicationState,
};
use crate::config::valid_repository_component;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Naming {
    version: u32,
    request: Uuid,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<String>,
}

pub(crate) struct Rename {
    pub repository: Uuid,
    pub expected_version: u32,
    pub request: Uuid,
    pub name: String,
}

impl CatalogRecord {
    pub(crate) fn name_version(&self) -> u32 {
        self.naming.as_ref().map_or(0, |state| state.version)
    }
}

impl CatalogDocument {
    pub(crate) fn repository_by_id(&self, id: Uuid) -> Option<&CatalogRecord> {
        self.repositories.iter().find(|record| {
            record.id == id && record.application == RepositoryApplicationState::CellReady
        })
    }

    pub(crate) fn renamed_route(&self, owner: &str, name: &str) -> Option<&CatalogRecord> {
        if let Some(active) = self.repositories.iter().find(|record| {
            record.owner.eq_ignore_ascii_case(owner) && record.name.eq_ignore_ascii_case(name)
        }) {
            return (active.naming.is_some()
                && active.application == RepositoryApplicationState::CellReady)
                .then_some(active);
        }
        self.repositories.iter().find(|record| {
            record.owner.eq_ignore_ascii_case(owner)
                && record.application == RepositoryApplicationState::CellReady
                && record.naming.as_ref().is_some_and(|state| {
                    state
                        .aliases
                        .iter()
                        .any(|alias| alias.eq_ignore_ascii_case(name))
                })
        })
    }

    pub(super) fn release_alias(&mut self, owner: &str, name: &str) {
        for record in &mut self.repositories {
            if record.owner.eq_ignore_ascii_case(owner)
                && let Some(state) = &mut record.naming
            {
                state
                    .aliases
                    .retain(|alias| !alias.eq_ignore_ascii_case(name));
            }
        }
    }

    pub(super) fn validate_renames(
        &self,
        names: &mut HashSet<(String, String)>,
    ) -> Result<(), CatalogError> {
        for record in &self.repositories {
            let Some(state) = &record.naming else {
                continue;
            };
            let owner = record.owner.to_ascii_lowercase();
            if state.version == 0
                || state.request.is_nil()
                || record.application != RepositoryApplicationState::CellReady
                || state.aliases.iter().any(|alias| {
                    !valid_repository_component(alias)
                        || !names.insert((owner.clone(), alias.to_ascii_lowercase()))
                })
            {
                return Err(CatalogError::Invalid(
                    "repository rename history is invalid",
                ));
            }
        }
        Ok(())
    }
}

impl CatalogStore {
    pub(crate) async fn rename_repository(
        &self,
        expected_revision: u64,
        input: Rename,
    ) -> Result<CatalogDocument, CatalogError> {
        if input.request.is_nil() || !valid_repository_component(&input.name) {
            return Err(CatalogError::Invalid("Enter a valid repository name"));
        }
        let (mut document, etag) = self.load_for_mutation().await?;
        // The caller authorized this exact catalog revision, including the
        // source ACL. A concurrent catalog change requires fresh authorization.
        if document.version != expected_revision {
            return Err(CatalogError::Conflict);
        }
        let current = document
            .repository_by_id(input.repository)
            .ok_or(CatalogError::NotFound)?;
        if current.naming.as_ref().is_some_and(|state| {
            state.request == input.request
                && input.expected_version.checked_add(1) == Some(state.version)
                && input.name == current.name
        }) {
            return Ok(document);
        }
        if current.name_version() != input.expected_version
            || current
                .naming
                .as_ref()
                .is_some_and(|state| state.request == input.request)
        {
            return Err(CatalogError::Conflict);
        }
        if current.name == input.name {
            return Ok(document);
        }
        let owner = current.owner.clone();
        if document.repositories.iter().any(|record| {
            record.id != input.repository
                && record.owner.eq_ignore_ascii_case(&owner)
                && record.name.eq_ignore_ascii_case(&input.name)
        }) {
            return Err(CatalogError::NameUnavailable);
        }
        let version = input
            .expected_version
            .checked_add(1)
            .ok_or(CatalogError::Conflict)?;
        // Reusing an address removes its old redirect in the same CAS. Older
        // paths always target repository UUIDs, so multiple renames cannot chain.
        document.release_alias(&owner, &input.name);
        let record = document
            .repositories
            .iter_mut()
            .find(|record| record.id == input.repository)
            .ok_or(CatalogError::NotFound)?;
        let previous = std::mem::replace(&mut record.name, input.name);
        let mut aliases = record
            .naming
            .take()
            .map_or_else(Vec::new, |state| state.aliases);
        if !previous.eq_ignore_ascii_case(&record.name) {
            aliases.push(previous);
        }
        record.naming = Some(Naming {
            version,
            request: input.request,
            aliases,
        });
        document.version = document
            .version
            .checked_add(1)
            .ok_or(CatalogError::Invalid("catalog version overflowed"))?;
        document.normalize();
        document.validate(&self.root)?;
        if self.write_document(&document, etag).await? {
            Ok(document)
        } else {
            Err(CatalogError::Conflict)
        }
    }
}
