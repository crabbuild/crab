//! Materialize and atomically install repository catalog snapshots.

use super::Repository;
use crate::{Result, catalog::CatalogStore};
use crab_remote_git::RepositoryIdentity;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, RwLock},
};
use uuid::Uuid;

pub(crate) struct RepositorySet {
    pub(super) current: RwLock<RepositoryIndex>,
}

pub(crate) struct RepositoryIndex {
    version: u64,
    pub(super) by_name: BTreeMap<(String, String), Arc<Repository>>,
    by_id: HashMap<Uuid, (String, String)>,
}

impl RepositorySet {
    pub(crate) fn version(&self) -> u64 {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .version
    }

    pub(crate) fn get(&self, key: &(String, String)) -> Option<Arc<Repository>> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_name
            .get(key)
            .cloned()
    }

    pub(crate) fn by_id(&self, id: Uuid) -> Option<Arc<Repository>> {
        let current = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current
            .by_id
            .get(&id)
            .and_then(|key| current.by_name.get(key))
            .cloned()
    }

    pub(crate) fn values(&self) -> Vec<Arc<Repository>> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_name
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_name
            .len()
    }

    pub(crate) fn replace(&self, next: RepositoryIndex) {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Import and refresh await metadata independently. Publish revision and
        // both indexes together so a late snapshot cannot restore revoked access.
        if next.version <= current.version {
            return;
        }
        let version = next.version;
        *current = next;
        drop(current);
        tracing::info!(catalog_version = version, "repository catalog refreshed");
    }

    #[cfg(test)]
    pub(crate) fn get_mut(&mut self, key: &(String, String)) -> Option<&mut Repository> {
        self.current
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_name
            .get_mut(key)
            .and_then(Arc::get_mut)
    }
}

#[cfg(test)]
impl From<BTreeMap<(String, String), Repository>> for RepositorySet {
    fn from(repositories: BTreeMap<(String, String), Repository>) -> Self {
        RepositoryIndex::new(
            0,
            repositories
                .into_iter()
                .map(|(key, repository)| (key, Arc::new(repository)))
                .collect(),
        )
        .into()
    }
}

#[cfg(test)]
impl From<BTreeMap<(String, String), Arc<Repository>>> for RepositorySet {
    fn from(repositories: BTreeMap<(String, String), Arc<Repository>>) -> Self {
        RepositoryIndex::new(0, repositories).into()
    }
}

impl From<RepositoryIndex> for RepositorySet {
    fn from(repositories: RepositoryIndex) -> Self {
        Self {
            current: RwLock::new(repositories),
        }
    }
}

impl RepositoryIndex {
    pub(crate) fn new(version: u64, by_name: BTreeMap<(String, String), Arc<Repository>>) -> Self {
        let by_id = by_name
            .iter()
            .map(|(key, repository)| (repository.id, key.clone()))
            .collect();
        Self {
            version,
            by_name,
            by_id,
        }
    }
}

pub(crate) async fn materialize(
    catalog: &CatalogStore,
    document: crate::catalog::CatalogDocument,
) -> Result<RepositoryIndex> {
    let version = document.version;
    let mut repositories = BTreeMap::new();
    for record in document.repositories.into_iter().filter(|record| {
        record.application == crate::catalog::RepositoryApplicationState::CellReady
    }) {
        let prefix = catalog.root().repository_prefix(&record.prefix)?;
        let layout = catalog.root().repository_layout(prefix.clone());
        let root = crab_metadata::capsule_protocol::load_root(&layout)
            .await
            .map_err(|source| crate::Error::Settings {
                source: Box::new(source),
            })?;
        let default_branch = root
            .record()
            .root()
            .head()
            .strip_prefix("refs/heads/")
            .ok_or(crate::Error::Config(
                "catalog repository HEAD must name a branch",
            ))?;
        let entry = record.runtime_config(catalog.root(), default_branch)?;
        let repository = Repository::new(
            record.id,
            entry.clone(),
            layout,
            RepositoryIdentity::new(
                catalog.root().provider_namespace.clone(),
                prefix,
                record.placement_generation,
            )?,
        );
        repositories.insert(
            (entry.owner.clone(), entry.name.clone()),
            Arc::new(repository),
        );
    }
    Ok(RepositoryIndex::new(version, repositories))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_storage::Store;
    #[tokio::test]
    async fn delayed_catalog_materialization_preserves_newer_membership() {
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let catalog = CatalogStore::new(crate::storage_root::StorageRoot::memory(
            store,
            "catalog-ordering",
        ));
        let member = crate::RepositoryMember {
            subject: "revoked-member".into(),
            name: "Member".into(),
            access: crate::RepositoryAccess::Write,
        };
        let record = catalog
            .create_repository(
                "team".into(),
                "project".into(),
                "team/project".into(),
                "main".into(),
                String::new(),
                vec![member],
            )
            .await
            .unwrap();
        catalog.mark_cell_ready(record.id).await.unwrap();
        // Import and periodic refresh both load a document before awaiting Git
        // metadata. An older import may finish after refresh installs revocation.
        let (delayed, _) = catalog.load().await.unwrap();
        catalog
            .set_members("team", "project", Vec::new(), false)
            .await
            .unwrap();
        let (current, _) = catalog.load().await.unwrap();
        let repositories = RepositorySet::from(materialize(&catalog, current).await.unwrap());
        repositories.replace(materialize(&catalog, delayed).await.unwrap());
        assert!(
            repositories
                .by_id(record.id)
                .unwrap()
                .config
                .members
                .is_empty()
        );
    }
}
