use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    RepositoryMember,
    cells::organization::{Departure, DepartureMode, MAX_VERSION},
};

use super::{
    CatalogDocument, CatalogError, CatalogRecord, CatalogStore, MembershipActor,
    MembershipAuditEvent,
    audit::{Action, OutsideGrantChange, Scope},
    valid_actor,
};

// The native offer can be issued on the last eligible day and remain open
// for a week. Keep a margin across catalog and Cell clocks.
const ARCHIVE_SECONDS: u64 = 98 * 24 * 60 * 60;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "decision",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum Pending {
    Departure(Departure),
    Rejoin(Rejoin),
}

impl Pending {
    pub(super) fn valid(&self) -> bool {
        match self {
            Self::Departure(value) => value.valid(),
            Self::Rejoin(value) => {
                !value.recipient.is_nil()
                    && (1..MAX_VERSION).contains(&value.invitation_version)
                    && (1..MAX_VERSION).contains(&value.member_version)
                    && valid_actor(&value.identity)
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rejoin {
    pub(crate) recipient: Uuid,
    pub(crate) invitation_version: u64,
    pub(crate) member_version: u64,
    pub(crate) reinstates: bool,
    pub(crate) identity: MembershipActor,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct FormerGrants {
    removed_version: u64,
    prune_after: u64,
    grants: BTreeMap<Uuid, ArchivedGrant>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    forks: BTreeMap<Uuid, u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ArchivedGrant {
    ownership_epoch: u32,
    member: RepositoryMember,
}

impl FormerGrants {
    fn new(removed_version: u64, prune_after: u64) -> Self {
        Self {
            removed_version,
            prune_after,
            grants: BTreeMap::new(),
            forks: BTreeMap::new(),
        }
    }

    pub(crate) fn matches(&self, removed_version: u64) -> bool {
        self.removed_version == removed_version
    }

    pub(super) fn valid(&self) -> bool {
        (2..=MAX_VERSION).contains(&self.removed_version)
            && self.prune_after > 0
            && (!self.grants.is_empty() || !self.forks.is_empty())
            && self
                .grants
                .keys()
                .chain(self.forks.keys())
                .all(|id| !id.is_nil())
    }
}

pub(crate) struct Begin {
    pub(crate) organization: Uuid,
    pub(crate) revision: u64,
    pub(crate) decision: Departure,
    pub(crate) members: Vec<DepartureGrants>,
    pub(crate) actor: MembershipActor,
}

pub(crate) struct DepartureGrants {
    pub(crate) identity: Option<MembershipActor>,
    pub(crate) name: String,
    pub(crate) teams: BTreeSet<Uuid>,
}

fn now() -> Result<u64, CatalogError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CatalogError::Invalid("system clock is before epoch"))?
        .as_secs())
}

fn insert_member(
    repo: &mut CatalogRecord,
    mut member: RepositoryMember,
    actor: Uuid,
) -> Result<(), CatalogError> {
    let names: BTreeSet<_> = repo
        .members
        .iter()
        .map(|entry| entry.name.to_lowercase())
        .collect();
    if names.contains(&member.name.to_lowercase()) {
        member.name = (1..=names.len() + 1)
            .map(|index| format!("{actor} ({index})"))
            .find(|name| !names.contains(name))
            .ok_or(CatalogError::Conflict)?;
    }
    repo.members.push(member);
    Ok(())
}

impl CatalogDocument {
    pub(crate) fn former_grants(&self, organization: Uuid, actor: Uuid) -> Option<&FormerGrants> {
        self.former_repository_grants
            .get(&organization)?
            .get(&actor)
    }

    fn record_access_change(
        &mut self,
        organization: Uuid,
        action: Action,
        actor: MembershipActor,
        previous: String,
        change: OutsideGrantChange,
    ) -> Result<(), CatalogError> {
        self.version = self.version.checked_add(1).ok_or(CatalogError::Conflict)?;
        self.queue_membership_audit(MembershipAuditEvent {
            id: Uuid::now_v7(),
            repository_id: None,
            actor,
            previous_members_digest: previous,
            new_members_digest: self.organization_membership_digest(organization)?,
            repository_version: self.version,
            occurred_at: now()?,
            previous: self.membership_audit_head.clone(),
            organizations: vec![Scope::membership_change(organization, action)],
            outside_grants: Some(change),
        });
        Ok(())
    }
}

impl CatalogStore {
    pub(crate) async fn prepare_departure(
        &self,
        input: Begin,
        cancel: &CancellationToken,
    ) -> Result<(), CatalogError> {
        let (mut document, etag) = self.load_for_mutation().await?;
        if !input.decision.valid() || input.members.len() != input.decision.members.len() {
            return Err(CatalogError::Invalid("Invalid membership departure plan"));
        }
        if document.version != input.revision
            || document
                .pending_membership_changes
                .contains_key(&input.organization)
        {
            return Err(CatalogError::Conflict);
        }

        let previous = document.organization_membership_digest(input.organization)?;
        let mut change = OutsideGrantChange {
            collaborators: input.members.len(),
            repositories: 0,
            grants: 0,
        };
        let mut archives = BTreeMap::new();
        let conversion = input.decision.mode == DepartureMode::Convert;
        let prune_after = now()?.saturating_add(ARCHIVE_SECONDS);
        for repo in document
            .repositories
            .iter_mut()
            .chain(
                document
                    .deleted_repositories
                    .iter_mut()
                    .filter(|_| !conversion)
                    .map(|entry| &mut entry.repository),
            )
            .filter(|repo| {
                repo.holds_organization_grants(input.organization)
                    || (repo.organization_id() == Some(input.organization)
                        && repo.fork_creator.is_some())
            })
        {
            let mut changed = 0;
            for (requested, member) in input.decision.members.iter().zip(&input.members) {
                let Some(identity) = &member.identity else {
                    continue;
                };
                if repo.fork_creator.as_ref() == Some(identity) {
                    repo.fork_creator = None;
                    changed += 1;
                    continue;
                }
                let subject = &identity.subject;
                let direct = repo
                    .members
                    .iter()
                    .position(|entry| &entry.subject == subject);
                if !conversion {
                    if let Some(index) = direct {
                        let archived = archives.entry(requested.actor).or_insert_with(|| {
                            FormerGrants::new(requested.version + 1, prune_after)
                        });
                        archived.grants.insert(
                            repo.id,
                            ArchivedGrant {
                                ownership_epoch: repo.ownership_epoch,
                                member: repo.members.remove(index),
                            },
                        );
                        changed += 1;
                    }
                    continue;
                }
                let Some(access) = repo
                    .organization
                    .iter()
                    .flat_map(|org| &org.teams)
                    .filter(|grant| member.teams.contains(&grant.team))
                    .map(|grant| grant.access)
                    .max()
                else {
                    continue;
                };
                if let Some(index) = direct {
                    let old = &mut repo.members[index];
                    if old.access < access {
                        old.access = access;
                        changed += 1;
                    }
                } else {
                    insert_member(
                        repo,
                        RepositoryMember {
                            subject: subject.clone(),
                            name: member.name.clone(),
                            access,
                        },
                        requested.actor,
                    )?;
                    changed += 1;
                }
            }
            change.grants += changed;
            change.repositories += usize::from(changed > 0);
        }

        let current = now()?;
        if !conversion {
            for (requested, member) in input.decision.members.iter().zip(&input.members) {
                if let Some(identity) = &member.identity {
                    let forks = document.retire_personal_forks(
                        input.organization,
                        identity,
                        &input.actor,
                        current.saturating_mul(1_000),
                    )?;
                    if !forks.is_empty() {
                        archives
                            .entry(requested.actor)
                            .or_insert_with(|| {
                                FormerGrants::new(requested.version + 1, prune_after)
                            })
                            .forks = forks;
                    }
                }
            }
        }
        document
            .former_repository_grants
            .retain(|organization, people| {
                if document
                    .pending_membership_changes
                    .contains_key(organization)
                {
                    return true;
                }
                people.retain(|actor, archived| {
                    archived.prune_after > current
                        && (*organization != input.organization
                            || !input
                                .decision
                                .members
                                .iter()
                                .any(|member| member.actor == *actor))
                });
                !people.is_empty()
            });
        if !archives.is_empty() {
            document
                .former_repository_grants
                .entry(input.organization)
                .or_default()
                .extend(archives);
        }
        // The journal and access edits commit together. Recovery repeats
        // only the version-bound native member departure.
        document
            .pending_membership_changes
            .insert(input.organization, Pending::Departure(input.decision));
        if conversion || change.grants > 0 {
            document.record_access_change(
                input.organization,
                if conversion {
                    Action::CollaboratorGrantsRetained
                } else {
                    Action::MemberGrantsRevoked
                },
                input.actor,
                previous,
                change,
            )?;
        } else {
            document.version = document
                .version
                .checked_add(1)
                .ok_or(CatalogError::Conflict)?;
        }
        document.validate(&self.root)?;
        if cancel.is_cancelled() || !self.write_document(&document, etag).await? {
            return Err(CatalogError::Conflict);
        }
        Ok(())
    }

    pub(crate) async fn prepare_rejoin(
        &self,
        organization: Uuid,
        decision: Rejoin,
        cancel: &CancellationToken,
    ) -> Result<(), CatalogError> {
        let recipient = decision.recipient;
        let member_version = decision.member_version;
        let expected = Pending::Rejoin(decision);
        let (mut document, etag) = self.load_for_mutation().await?;
        if !expected.valid()
            || document
                .former_grants(organization, recipient)
                .is_none_or(|former| !former.matches(member_version))
            || document
                .pending_membership_changes
                .contains_key(&organization)
        {
            return Err(CatalogError::Conflict);
        }
        document
            .pending_membership_changes
            .insert(organization, expected);
        document.validate(&self.root)?;
        if cancel.is_cancelled() || !self.write_document(&document, etag).await? {
            return Err(CatalogError::Conflict);
        }
        Ok(())
    }

    pub(crate) async fn finish_membership_change(
        &self,
        organization: Uuid,
        expected: &Pending,
        accepted: bool,
    ) -> Result<bool, CatalogError> {
        for _ in 0..super::MAX_CAS_ATTEMPTS {
            let (mut document, etag) = self.load_for_mutation().await?;
            match document.pending_membership_changes.get(&organization) {
                None => return Ok(false),
                Some(current) if current != expected => return Err(CatalogError::Conflict),
                Some(_) => {}
            }
            let revision = document.version;
            let mut forks_restored = false;
            match expected {
                Pending::Departure(departure) => {
                    // A recovered departure may finish long after the first CAS.
                    // Give its archive the full native reinstatement window.
                    if let Some(people) = document.former_repository_grants.get_mut(&organization) {
                        let prune_after = now()?.saturating_add(ARCHIVE_SECONDS);
                        for selected in &departure.members {
                            if let Some(archive) = people.get_mut(&selected.actor)
                                && archive.matches(selected.version + 1)
                            {
                                archive.prune_after = prune_after;
                            }
                        }
                    }
                }
                Pending::Rejoin(decision) if accepted => {
                    forks_restored = apply_rejoin(&mut document, organization, decision)?;
                }
                Pending::Rejoin(_) => {}
            }
            document.pending_membership_changes.remove(&organization);
            if document.version == revision {
                // Fence repository writes authorized during the handoff, even
                // when finalizing only changed native membership.
                document.version = revision.checked_add(1).ok_or(CatalogError::Conflict)?;
            }
            document.validate(&self.root)?;
            if self.write_document(&document, etag).await? {
                return Ok(forks_restored);
            }
        }
        Err(CatalogError::Conflict)
    }
}

fn apply_rejoin(
    document: &mut CatalogDocument,
    organization: Uuid,
    decision: &Rejoin,
) -> Result<bool, CatalogError> {
    let Some(archived) = document
        .former_grants(organization, decision.recipient)
        .filter(|archived| archived.matches(decision.member_version))
    else {
        return Ok(false);
    };
    let archived = archived.clone();
    document.former_repository_grants.retain(|id, people| {
        if *id == organization {
            people.remove(&decision.recipient);
        }
        !people.is_empty()
    });
    if !decision.reinstates {
        return Ok(false);
    }
    let previous = document.organization_membership_digest(organization)?;
    let mut restored = 0;
    for repo in document
        .repositories
        .iter_mut()
        .chain(
            document
                .deleted_repositories
                .iter_mut()
                .map(|entry| &mut entry.repository),
        )
        .filter(|repo| repo.holds_organization_grants(organization))
    {
        let Some(grant) = archived.grants.get(&repo.id) else {
            continue;
        };
        // Transfers and newer direct grants supersede the recorded access.
        if grant.ownership_epoch != repo.ownership_epoch
            || grant.member.subject != decision.identity.subject
            || RepositoryMember::access_for(&repo.members, &grant.member.subject).is_some()
        {
            continue;
        }
        insert_member(repo, grant.member.clone(), decision.recipient)?;
        restored += 1;
    }
    if restored > 0 {
        document.record_access_change(
            organization,
            Action::MemberGrantsRestored,
            decision.identity.clone(),
            previous,
            OutsideGrantChange {
                collaborators: 1,
                repositories: restored,
                grants: restored,
            },
        )?;
    }
    Ok(document.restore_personal_forks(organization, &decision.identity, &archived.forks))
}
