use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    CatalogDocument, CatalogError, CatalogRecord, CatalogStore, MembershipActor,
    MembershipAuditEvent, OrganizationAccess, RepositoryApplicationState, organization_namespace,
    personal_namespace, valid_account_namespace, valid_actor,
};
use crate::{RepositoryAccess, RepositoryMember, config::valid_repository_component};

const LIFETIME_MS: i64 = 24 * 60 * 60 * 1_000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Recipient {
    Account { identity: MembershipActor },
    Organization { id: Uuid },
}

impl Recipient {
    fn valid(&self) -> bool {
        match self {
            Self::Account { identity } => valid_actor(identity),
            Self::Organization { id } => !id.is_nil(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Pending,
    Accepted,
    Cancelled,
    Declined,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Offer {
    pub id: Uuid,
    pub requester: MembershipActor,
    pub recipient: Recipient,
    pub recipient_label: String,
    pub from_owner: String,
    pub from_name: String,
    pub name: String,
    pub version: u32,
    pub expires_at_ms: i64,
    pub state: State,
}

pub(crate) struct Proposal {
    pub id: Uuid,
    pub requester: MembershipActor,
    pub recipient: Recipient,
    pub recipient_label: String,
    pub name: String,
    pub version: u32,
}

pub(crate) enum Decision {
    Accept {
        actor: MembershipActor,
        display_name: String,
        property_seed: Option<Uuid>,
    },
    Cancel,
    Decline,
}

impl Offer {
    /// The owner address the repository moves to on acceptance.
    ///
    /// An account offer is labelled `@username`; that username is the
    /// destination both people confirmed, so it is never re-derived later.
    pub(crate) fn destination_owner(&self) -> String {
        match &self.recipient {
            Recipient::Account { .. } => self
                .recipient_label
                .strip_prefix('@')
                .unwrap_or_default()
                .to_owned(),
            Recipient::Organization { id } => organization_namespace(*id),
        }
    }

    pub(crate) fn created_at_ms(&self) -> i64 {
        self.expires_at_ms.saturating_sub(LIFETIME_MS)
    }

    pub(crate) fn available(&self, record: &CatalogRecord, now_ms: i64) -> bool {
        self.state == State::Pending
            && self.expires_at_ms > now_ms
            && self.version == record.name_version()
    }

    fn matches(&self, input: &Proposal) -> bool {
        self.id == input.id
            && self.requester == input.requester
            && self.recipient == input.recipient
            && self.name == input.name
            && self.version == input.version
    }

    fn valid(&self) -> bool {
        !self.id.is_nil()
            && valid_actor(&self.requester)
            && self.recipient.valid()
            && !self.recipient_label.is_empty()
            && self.recipient_label.chars().count() <= 100
            && !self.recipient_label.chars().any(char::is_control)
            && valid_repository_component(&self.from_owner)
            && valid_repository_component(&self.from_name)
            && valid_repository_component(&self.name)
            && (matches!(self.recipient, Recipient::Organization { .. })
                || valid_account_namespace(&self.destination_owner()))
            && !self
                .from_owner
                .eq_ignore_ascii_case(&self.destination_owner())
            && self.expires_at_ms > 0
    }
}

impl CatalogDocument {
    pub(crate) fn transfer(&self, id: Uuid) -> Option<(&CatalogRecord, &Offer)> {
        self.repositories.iter().find_map(|record| {
            record
                .transfer
                .as_ref()
                .filter(|offer| offer.id == id)
                .map(|offer| (record, offer))
        })
    }

    pub(super) fn validate_transfers(&self) -> Result<(), CatalogError> {
        let mut ids = HashSet::new();
        for record in &self.repositories {
            if let Some(offer) = &record.transfer
                && (!offer.valid()
                    || !ids.insert(offer.id)
                    || record.application != RepositoryApplicationState::CellReady
                    || offer.version > record.name_version())
            {
                return Err(CatalogError::Invalid(
                    "repository transfer record is invalid",
                ));
            }
        }
        Ok(())
    }

    fn allow_transfer(&self, record: &CatalogRecord) -> Result<(), CatalogError> {
        if record
            .organization_id()
            .is_some_and(|id| self.pending_membership_changes.contains_key(&id))
        {
            return Err(CatalogError::Conflict);
        }
        if record.forked_from.is_some()
            || self
                .repositories
                .iter()
                .any(|fork| fork.forked_from == Some(record.id))
        {
            return Err(CatalogError::Invalid(
                "Repositories in a private fork network cannot be transferred",
            ));
        }
        Ok(())
    }

    fn advance_transfer(&mut self) -> Result<(), CatalogError> {
        self.version = self
            .version
            .checked_add(1)
            .ok_or(CatalogError::Invalid("catalog version overflowed"))?;
        Ok(())
    }
}

impl CatalogStore {
    pub(crate) async fn propose_transfer(
        &self,
        revision: u64,
        repository: Uuid,
        proposal: Proposal,
        now_ms: i64,
    ) -> Result<CatalogDocument, CatalogError> {
        let (mut document, etag) = self.load_for_mutation().await?;
        if document.version != revision {
            return Err(CatalogError::Conflict);
        }
        let record = document
            .repository_by_id(repository)
            .ok_or(CatalogError::NotFound)?;
        if let Some((existing, offer)) = document.transfer(proposal.id) {
            return if existing.id == repository && offer.matches(&proposal) {
                Ok(document)
            } else {
                Err(CatalogError::Conflict)
            };
        }
        document.allow_transfer(record)?;
        if proposal.version != record.name_version()
            || record
                .transfer
                .as_ref()
                .is_some_and(|offer| offer.available(record, now_ms))
        {
            return Err(CatalogError::Conflict);
        }
        let offer = Offer {
            id: proposal.id,
            requester: proposal.requester,
            recipient: proposal.recipient,
            recipient_label: proposal.recipient_label,
            name: proposal.name,
            from_owner: record.owner.clone(),
            from_name: record.name.clone(),
            version: proposal.version,
            expires_at_ms: now_ms.saturating_add(LIFETIME_MS),
            state: State::Pending,
        };
        if !offer.valid() {
            return Err(CatalogError::Invalid(
                "Choose a different owner and a valid repository name",
            ));
        }
        let record = document
            .repositories
            .iter_mut()
            .find(|record| record.id == repository)
            .ok_or(CatalogError::NotFound)?;
        record.transfer = Some(offer);
        document.advance_transfer()?;
        document.validate(&self.root)?;
        if self.write_document(&document, etag).await? {
            Ok(document)
        } else {
            Err(CatalogError::Conflict)
        }
    }

    pub(crate) async fn decide_transfer(
        &self,
        revision: u64,
        id: Uuid,
        decision: Decision,
        now_ms: i64,
    ) -> Result<CatalogDocument, CatalogError> {
        let (mut document, etag) = self.load_for_mutation().await?;
        if document.version != revision {
            return Err(CatalogError::Conflict);
        }
        let (record, offer) = document.transfer(id).ok_or(CatalogError::NotFound)?;
        let state = match &decision {
            Decision::Accept { .. } => State::Accepted,
            Decision::Cancel => State::Cancelled,
            Decision::Decline => State::Declined,
        };
        if offer.state == state {
            if let Decision::Accept { property_seed, .. } = decision
                && record.property_seed != property_seed
            {
                return Err(CatalogError::Conflict);
            }
            return Ok(document);
        }
        if !offer.available(record, now_ms) {
            return Err(CatalogError::Conflict);
        }
        let repository = record.id;
        if let Decision::Accept {
            actor,
            display_name,
            property_seed,
        } = decision
        {
            document.allow_transfer(record)?;
            let outgoing =
                super::audit::Scope::for_record(record, super::audit::Action::TransferredOut);
            let previous = record.membership_digest()?;
            let offer = offer.clone();
            if let Recipient::Account { identity } = &offer.recipient
                && identity != &actor
            {
                return Err(CatalogError::Conflict);
            }
            let namespace = offer.destination_owner();
            if let Recipient::Organization { id } = offer.recipient
                && document.pending_membership_changes.contains_key(&id)
            {
                // A transfer brings direct grants into the destination. It
                // must not land between the departing member's two commits.
                return Err(CatalogError::Conflict);
            }
            let account = match &offer.recipient {
                Recipient::Account { identity } => {
                    Some(personal_namespace(&identity.issuer, &identity.subject))
                }
                Recipient::Organization { .. } => None,
            };
            if let Recipient::Organization { id } = offer.recipient
                && document.repositories.iter().any(|record| {
                    record.owner.eq_ignore_ascii_case(&namespace)
                        && record.organization_id() != Some(id)
                })
            {
                return Err(CatalogError::NameUnavailable);
            }
            if !document.namespace_bound_to(&namespace, account.as_deref()) {
                return Err(CatalogError::NameUnavailable);
            }
            let new = document.relocate(repository, namespace, offer.name.clone(), id)?;
            // Direct collaborators remain. Namespace-owned grants are replaced
            // at the same commit as the address, including former SSO groups.
            new.groups.clear();
            new.fork_baseline = super::RepositoryBaseline::Organization;
            // A later return to the same organization cannot revive an old
            // automation grant. Ordinary renames retain their ownership epoch.
            new.ownership_epoch = new.name_version();
            new.property_seed = property_seed;
            new.organization = match offer.recipient {
                Recipient::Organization { id } => {
                    new.personal_owner = None;
                    Some(OrganizationAccess {
                        id,
                        teams: Vec::new(),
                    })
                }
                Recipient::Account { identity } => {
                    new.personal_owner = Some(identity.clone());
                    if let Some(entry) = new
                        .members
                        .iter_mut()
                        .find(|entry| entry.subject == identity.subject)
                    {
                        entry.access = RepositoryAccess::Admin;
                    } else {
                        new.members.push(RepositoryMember {
                            subject: identity.subject,
                            name: display_name,
                            access: RepositoryAccess::Admin,
                        });
                    }
                    None
                }
            };
            let digest = new.membership_digest()?;
            let incoming =
                super::audit::Scope::for_record(new, super::audit::Action::TransferredIn);
            document.advance_transfer()?;
            document.queue_membership_audit(MembershipAuditEvent {
                id: Uuid::now_v7(),
                outside_grants: None,
                repository_id: Some(repository),
                actor,
                previous_members_digest: previous,
                new_members_digest: digest,
                repository_version: document.version,
                occurred_at: u64::try_from(now_ms / 1_000)
                    .map_err(|_| CatalogError::Invalid("system clock is before epoch"))?,
                previous: document.membership_audit_head.clone(),
                organizations: outgoing.into_iter().chain(incoming).collect(),
            });
            document.normalize();
        } else {
            document.advance_transfer()?;
        }
        let record = document
            .repositories
            .iter_mut()
            .find(|record| record.id == repository)
            .ok_or(CatalogError::NotFound)?;
        record
            .transfer
            .as_mut()
            .ok_or(CatalogError::NotFound)?
            .state = state;
        document.validate(&self.root)?;
        if !self.write_document(&document, etag).await? {
            return Err(CatalogError::Conflict);
        }
        if state == State::Accepted {
            let _ = self.flush_membership_audit().await;
        }
        Ok(document)
    }
}
