use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    CatalogDocument, CatalogError, CatalogRecord, CatalogStore, MembershipActor,
    MembershipAuditEvent, RepositoryApplicationState, StorageRoot, audit, valid_actor,
};

pub(crate) const RESTORE_WINDOW_MS: u64 = 90 * 24 * 60 * 60 * 1_000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeletedRepository {
    pub repository: CatalogRecord,
    pub operation: Uuid,
    pub root: Uuid,
    pub deleted_at_ms: u64,
    actor: MembershipActor,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) retired_for_removal: bool,
}

impl DeletedRepository {
    pub(crate) fn restorable(&self, now: u64) -> bool {
        self.repository.id == self.root
            && self.repository.forked_from.is_none()
            && now < self.deleted_at_ms.saturating_add(RESTORE_WINDOW_MS)
    }

    pub(crate) fn same_deletion(&self, operation: Uuid, actor: &MembershipActor) -> bool {
        !self.retired_for_removal
            && self.operation == operation
            && self.root == self.repository.id
            && &self.actor == actor
    }
}

pub(crate) struct Delete {
    pub repository: Uuid,
    pub organization: Uuid,
    pub operation: Uuid,
    pub expected_version: u32,
    pub actor: MembershipActor,
    pub now: u64,
}

pub(crate) struct Restore {
    pub repository: Uuid,
    pub organization: Uuid,
    pub operation: Uuid,
    pub actor: MembershipActor,
    pub now: u64,
}

impl CatalogDocument {
    pub(crate) fn deleted_repository(&self, repository: Uuid) -> Option<&DeletedRepository> {
        self.deleted_repositories
            .iter()
            .find(|record| record.repository.id == repository)
    }

    pub(super) fn validate_deletions(
        &self,
        storage: &StorageRoot,
        active: &HashMap<Uuid, &CatalogRecord>,
        prefixes: &mut HashSet<String>,
    ) -> Result<(), CatalogError> {
        let deleted: HashMap<_, _> = self
            .deleted_repositories
            .iter()
            .map(|entry| (entry.repository.id, entry))
            .collect();
        if deleted.len() != self.deleted_repositories.len() {
            return Err(CatalogError::Invalid(
                "deleted repository IDs must be unique",
            ));
        }
        for entry in &self.deleted_repositories {
            let record = &entry.repository;
            if record.id.is_nil()
                || active.contains_key(&record.id)
                || !prefixes.insert(record.prefix.clone())
                || entry.operation.is_nil()
                || !valid_actor(&entry.actor)
                || entry.deleted_at_ms == 0
                || entry.deleted_at_ms > 9_007_199_254_740_991
                || record.placement_generation == 0
                || !record.timestamps_valid()
                || record.ownership_epoch != record.name_version()
                || !record.property_seed_valid()
                || !record.fork_baseline_valid()
                || !record.fork_creator_valid()
                || record.application != RepositoryApplicationState::CellReady
                || record.transfer.is_some()
                || (entry.retired_for_removal
                    && (record.fork_owner.is_none()
                        || record.forked_from.is_none()
                        || entry.root != record.id))
                || !record.forking.valid_saved()
                || !record.visibility.valid_saved()
                || record.organization.as_ref().is_none_or(|organization| {
                    !organization.teams.is_empty() || organization.validate().is_err()
                })
                || deleted.get(&entry.root).is_none_or(|root| {
                    entry.operation != root.operation
                        || root.root != entry.root
                        || (record.fork_owner.is_some()
                            && record.organization_id() != root.repository.organization_id())
                        || (record.id != entry.root && record.forked_from != Some(entry.root))
                })
            {
                return Err(CatalogError::Invalid("deleted repository is invalid"));
            }
            crate::config::validate_repository(&record.runtime_config(storage, "main")?).map_err(
                |_| CatalogError::Invalid("deleted repository configuration is invalid"),
            )?;
        }
        Ok(())
    }

    pub(super) fn retire_personal_forks(
        &mut self,
        organization: Uuid,
        owner: &MembershipActor,
        by: &MembershipActor,
        now_ms: u64,
    ) -> Result<BTreeMap<Uuid, u32>, CatalogError> {
        let mut retired = BTreeMap::new();
        self.repositories.retain_mut(|record| {
            if record.organization_id() == Some(organization)
                && record.fork_owner.as_ref() == Some(owner)
                && record.application == RepositoryApplicationState::CellReady
            {
                retired.insert(record.id, record.clone());
                false
            } else {
                true
            }
        });
        let mut archived = BTreeMap::new();
        for (id, mut repository) in retired {
            let source_epoch = repository
                .forked_from
                .and_then(|source| self.repository_by_id(source))
                .ok_or(CatalogError::Conflict)?
                .ownership_epoch;
            let operation = Uuid::now_v7();
            repository.retire_address(operation)?;
            self.deleted_repositories.push(DeletedRepository {
                repository,
                operation,
                root: id,
                deleted_at_ms: now_ms,
                actor: by.clone(),
                retired_for_removal: true,
            });
            archived.insert(id, source_epoch);
        }
        Ok(archived)
    }

    pub(super) fn restore_personal_forks(
        &mut self,
        organization: Uuid,
        owner: &MembershipActor,
        archived: &BTreeMap<Uuid, u32>,
    ) -> bool {
        let mut restored = Vec::new();
        let mut names: HashSet<_> = self
            .repositories
            .iter()
            .map(|record| (record.owner.to_lowercase(), record.name.to_lowercase()))
            .collect();
        let upstream: HashMap<_, _> = self
            .repositories
            .iter()
            .filter(|repo| {
                repo.organization_id() == Some(organization)
                    && repo.forked_from.is_none()
                    && !repo.visibility.value.is_public()
            })
            .map(|repo| (repo.id, repo.ownership_epoch))
            .collect();
        let available: HashSet<_> = self
            .deleted_repositories
            .iter()
            .map(|entry| &entry.repository)
            .filter(|repo| {
                self.namespace_bound_to(&repo.owner, repo.personal_namespace().as_deref())
            })
            .map(|repo| repo.id)
            .collect();
        self.deleted_repositories.retain(|entry| {
            let repo = &entry.repository;
            if entry.retired_for_removal
                && repo.organization_id() == Some(organization)
                && repo.fork_owner.as_ref() == Some(owner)
                && let Some(epoch) = archived.get(&repo.id)
                && repo.forked_from.and_then(|id| upstream.get(&id)) == Some(epoch)
                && available.contains(&repo.id)
                && names.insert((repo.owner.to_lowercase(), repo.name.to_lowercase()))
            {
                restored.push(repo.clone());
                false
            } else {
                true
            }
        });
        let changed = !restored.is_empty();
        for repo in restored {
            self.release_alias(&repo.owner, &repo.name);
            self.repositories.push(repo);
        }
        if changed {
            self.normalize();
        }
        changed
    }

    fn queue_deletion_audit(
        &mut self,
        record: &CatalogRecord,
        actor: MembershipActor,
        previous: String,
        action: audit::Action,
        now: u64,
    ) -> Result<(), CatalogError> {
        self.version = self.version.checked_add(1).ok_or(CatalogError::Conflict)?;
        self.queue_membership_audit(MembershipAuditEvent {
            id: Uuid::now_v7(),
            outside_grants: None,
            repository_id: Some(record.id),
            actor,
            previous_members_digest: previous,
            new_members_digest: record.membership_digest()?,
            repository_version: self.version,
            occurred_at: now / 1_000,
            previous: self.membership_audit_head.clone(),
            organizations: audit::Scope::for_record(record, action)
                .into_iter()
                .collect(),
        });
        Ok(())
    }
}

impl CatalogStore {
    pub(crate) async fn delete_repository(
        &self,
        revision: u64,
        input: Delete,
    ) -> Result<CatalogDocument, CatalogError> {
        let (mut document, etag) = self.load_for_mutation().await?;
        if let Some(deleted) = document.deleted_repository(input.repository) {
            return if deleted.same_deletion(input.operation, &input.actor)
                && deleted.repository.organization_id() == Some(input.organization)
            {
                Ok(document)
            } else {
                Err(CatalogError::NotFound)
            };
        }
        if document.version != revision {
            return Err(CatalogError::Conflict);
        }
        let root = document
            .repository_by_id(input.repository)
            .ok_or(CatalogError::NotFound)?;
        if root.organization_id() != Some(input.organization)
            || root.name_version() != input.expected_version
        {
            return Err(CatalogError::Conflict);
        }
        let previous = root.membership_digest()?;
        if root.visibility.value.is_public() {
            document.separate_public_forks(input.repository, input.now);
        }
        let (removed, retained): (Vec<_>, _) =
            document.repositories.into_iter().partition(|record| {
                record.id == input.repository || record.forked_from == Some(input.repository)
            });
        document.repositories = retained;
        for mut repository in removed {
            if repository.application != RepositoryApplicationState::CellReady {
                return Err(CatalogError::Conflict);
            }
            if let Some(access) = &mut repository.organization {
                access.teams.clear();
            }
            repository.retire_address(input.operation)?;
            document.deleted_repositories.push(DeletedRepository {
                repository,
                operation: input.operation,
                root: input.repository,
                deleted_at_ms: input.now,
                actor: input.actor.clone(),
                retired_for_removal: false,
            });
        }
        let removed = document
            .deleted_repository(input.repository)
            .ok_or(CatalogError::NotFound)?
            .repository
            .clone();
        document.queue_deletion_audit(
            &removed,
            input.actor,
            previous,
            audit::Action::Deleted,
            input.now,
        )?;
        document.validate(&self.root)?;
        if !self.write_document(&document, etag).await? {
            return Err(CatalogError::Conflict);
        }
        let _ = self.flush_membership_audit().await;
        Ok(document)
    }

    pub(crate) async fn restore_repository(
        &self,
        input: Restore,
    ) -> Result<CatalogDocument, CatalogError> {
        let (mut document, etag) = self.load_for_mutation().await?;
        if document
            .repository_by_id(input.repository)
            .is_some_and(|record| record.organization_id() == Some(input.organization))
        {
            return Ok(document);
        }
        let entry = document
            .deleted_repository(input.repository)
            .ok_or(CatalogError::NotFound)?;
        if entry.repository.organization_id() != Some(input.organization) {
            return Err(CatalogError::NotFound);
        }
        if entry.operation != input.operation || !entry.restorable(input.now) {
            return Err(CatalogError::Conflict);
        }
        let mut names: HashSet<_> = document
            .repositories
            .iter()
            .map(|record| {
                (
                    record.owner.to_ascii_lowercase(),
                    record.name.to_ascii_lowercase(),
                )
            })
            .collect();
        let (restored, retired): (Vec<_>, _) =
            document
                .deleted_repositories
                .into_iter()
                .partition(|entry| {
                    entry.root == input.repository && entry.operation == input.operation
                });
        document.deleted_repositories = retired;
        for entry in restored {
            let record = entry.repository;
            if !names.insert((
                record.owner.to_ascii_lowercase(),
                record.name.to_ascii_lowercase(),
            )) || !document
                .namespace_bound_to(&record.owner, record.personal_namespace().as_deref())
            {
                return Err(CatalogError::NameUnavailable);
            }
            document.release_alias(&record.owner, &record.name);
            document.repositories.push(record);
        }
        document.normalize();
        let record = document
            .repository_by_id(input.repository)
            .ok_or(CatalogError::NotFound)?
            .clone();
        let digest = record.membership_digest()?;
        document.queue_deletion_audit(
            &record,
            input.actor,
            digest,
            audit::Action::Restored,
            input.now,
        )?;
        document.validate(&self.root)?;
        if !self.write_document(&document, etag).await? {
            return Err(CatalogError::Conflict);
        }
        let _ = self.flush_membership_audit().await;
        Ok(document)
    }
}
