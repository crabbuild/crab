use std::collections::BTreeSet;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{RepositoryMember, cells::organization::Conversion};

use super::{
    CatalogError, CatalogStore, MembershipActor, MembershipAuditEvent,
    audit::{OutsideGrantChange, Scope},
};

pub(crate) struct Begin {
    pub(crate) organization: Uuid,
    pub(crate) revision: u64,
    pub(crate) decision: Conversion,
    pub(crate) retain: Vec<RetainedMember>,
    pub(crate) actor: MembershipActor,
}

pub(crate) struct RetainedMember {
    pub(crate) subject: String,
    pub(crate) name: String,
    pub(crate) teams: BTreeSet<Uuid>,
}

impl CatalogStore {
    pub(crate) async fn prepare_conversion(
        &self,
        input: Begin,
        cancel: &CancellationToken,
    ) -> Result<(), CatalogError> {
        let (mut document, etag) = self.load_for_mutation().await?;
        if !input.decision.valid() || input.retain.len() != input.decision.members.len() {
            return Err(CatalogError::Invalid(
                "Invalid collaborator conversion plan",
            ));
        }
        if document.version != input.revision
            || document
                .pending_conversions
                .contains_key(&input.organization)
        {
            return Err(CatalogError::Conflict);
        }
        let previous = document.organization_membership_digest(input.organization)?;
        let mut repositories = 0;
        let mut grants = 0;
        for repo in document
            .repositories
            .iter_mut()
            .filter(|repo| repo.holds_organization_grants(input.organization))
        {
            let mut changes = 0;
            for (requested, retain) in input.decision.members.iter().zip(&input.retain) {
                let Some(role) = repo
                    .organization
                    .iter()
                    .flat_map(|org| &org.teams)
                    .filter(|grant| retain.teams.contains(&grant.team))
                    .map(|grant| grant.access)
                    .max()
                else {
                    continue;
                };
                if let Some(direct) = repo
                    .members
                    .iter_mut()
                    .find(|member| member.subject == retain.subject)
                {
                    if direct.access < role {
                        direct.access = role;
                        changes += 1;
                    }
                    continue;
                }
                let names: BTreeSet<_> = repo
                    .members
                    .iter()
                    .map(|member| member.name.to_lowercase())
                    .collect();
                let name = if !names.contains(&retain.name.to_lowercase()) {
                    retain.name.clone()
                } else {
                    (1..=names.len() + 1)
                        .map(|index| format!("{} ({index})", requested.actor))
                        .find(|name| !names.contains(name))
                        .ok_or(CatalogError::Conflict)?
                };
                repo.members.push(RepositoryMember {
                    subject: retain.subject.clone(),
                    name,
                    access: role,
                });
                changes += 1;
            }
            repositories += usize::from(changes > 0);
            grants += changes;
        }

        // Publish the retained grants and recovery obligation in one CAS. The
        // organization lease keeps active team grants stable until this commit.
        let collaborators = input.decision.members.len();
        document
            .pending_conversions
            .insert(input.organization, input.decision);
        document.version = document
            .version
            .checked_add(1)
            .ok_or(CatalogError::Conflict)?;
        let current = document.organization_membership_digest(input.organization)?;
        let occurred_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| CatalogError::Invalid("system clock is before epoch"))?
            .as_secs();
        document.queue_membership_audit(MembershipAuditEvent {
            id: Uuid::now_v7(),
            repository_id: None,
            actor: input.actor,
            previous_members_digest: previous,
            new_members_digest: current,
            repository_version: document.version,
            occurred_at,
            previous: document.membership_audit_head.clone(),
            organizations: vec![Scope::conversion(input.organization)],
            outside_grants: Some(OutsideGrantChange {
                collaborators,
                repositories,
                grants,
            }),
        });
        document.validate(&self.root)?;
        if cancel.is_cancelled() || !self.write_document(&document, etag).await? {
            return Err(CatalogError::Conflict);
        }
        Ok(())
    }

    pub(crate) async fn finish_conversion(
        &self,
        organization: Uuid,
        expected: &Conversion,
    ) -> Result<(), CatalogError> {
        for _ in 0..super::MAX_CAS_ATTEMPTS {
            let (mut document, etag) = self.load().await?;
            match document.pending_conversions.get(&organization) {
                None => return Ok(()),
                Some(current) if current != expected => return Err(CatalogError::Conflict),
                Some(_) => {}
            }
            document.pending_conversions.remove(&organization);
            if self.write_document(&document, etag).await? {
                return Ok(());
            }
        }
        Err(CatalogError::Conflict)
    }
}
