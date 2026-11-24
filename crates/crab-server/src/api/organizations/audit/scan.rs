use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    Parameters,
    event::{AuditAction, AuditEvent, RawEvent},
};
use crate::{
    app::{self, Error, Result},
    catalog::{CatalogDocument, CatalogStore, audit},
    cells::{
        Address, CellClient,
        directory::{LookupAccounts, LookupProfiles},
        organization::{self, ActivityQuery, ListActivity},
    },
};

const FETCH: usize = 16;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cursor {
    pub(super) organization_before: Option<u64>,
    pub(super) repository: Option<audit::Position>,
}

impl Cursor {
    pub(super) fn remaining(self) -> bool {
        self.organization_before.is_some() || self.repository.is_some()
    }
}

pub(super) struct Scan {
    pub(super) items: Vec<AuditEvent>,
    pub(super) next: Option<Cursor>,
}

struct Scanner<'a> {
    cells: &'a CellClient,
    organization: Uuid,
    owner: Uuid,
    catalog: CatalogStore,
    snapshot: Arc<CatalogDocument>,
    position: Cursor,
    pending_events: VecDeque<organization::Event>,
    more_events: bool,
    pending_repository: Option<audit::Entry>,
}

pub(super) async fn search(
    cells: &CellClient,
    catalog: CatalogStore,
    organization_id: Uuid,
    owner: Uuid,
    position: Option<Cursor>,
    filters: &Parameters,
    limit: usize,
    mut budget: usize,
) -> Result<Scan> {
    let (snapshot, _) = catalog
        .load_snapshot()
        .await
        .map_err(|error| Error::Membership(Box::new(error)))?;
    let position = position.unwrap_or_else(|| {
        let repository_action = filters.action.map(|action| {
            matches!(
                action,
                AuditAction::RepositoryRegistered
                    | AuditAction::RepositoryAccessChanged
                    | AuditAction::RepositoryTransferredIn
                    | AuditAction::RepositoryTransferredOut
                    | AuditAction::RepositoryDeleted
                    | AuditAction::RepositoryRestored
                    | AuditAction::RepositoryVisibilityChanged
                    | AuditAction::OutsideCollaboratorsRemoved
                    | AuditAction::CollaboratorGrantsRetained
            )
        });
        let organization = repository_action != Some(true) && filters.repository.is_none();
        let repository = repository_action != Some(false) && filters.account.is_none();
        Cursor {
            organization_before: organization.then_some(organization::MAX_VERSION + 1),
            repository: repository
                .then(|| snapshot.organization_audit_start(organization_id))
                .flatten(),
        }
    });
    let mut reader = Scanner {
        cells,
        organization: organization_id,
        owner,
        catalog,
        snapshot,
        position,
        pending_events: VecDeque::new(),
        more_events: false,
        pending_repository: None,
    };
    let mut items = Vec::new();
    let mut position = reader.position;
    while budget > 0 && position.remaining() && items.len() < limit {
        let mut batch = Vec::new();
        for _ in 0..budget.min(FETCH) {
            let Some(event) = reader.next().await? else {
                position = reader.position;
                break;
            };
            budget -= 1;
            let event_position = reader.position;
            batch.push((event, event_position));
        }
        let identities = batch
            .iter_mut()
            .filter_map(|(raw, _)| raw.identity.take().map(|key| (key, &mut raw.event.actor)))
            .collect::<Vec<_>>();
        if !identities.is_empty() {
            let keys = identities
                .iter()
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            let accounts = app::query_output(
                cells
                    .query::<LookupAccounts>(Address::Directory, None, keys)
                    .await,
            )?;
            if accounts.len() != identities.len() {
                return Err(Error::CellContract(
                    "Audit account lookup returned an invalid count",
                ));
            }
            for ((_, actor), account) in identities.into_iter().zip(accounts) {
                *actor = account;
            }
        }
        for (raw, after) in batch {
            position = after;
            if filters.matches(&raw.event) {
                items.push(raw.event);
                if items.len() == limit {
                    break;
                }
            }
        }
    }
    profiles(cells, &mut items).await?;
    Ok(Scan {
        items,
        next: position.remaining().then_some(position),
    })
}

impl Scanner<'_> {
    async fn next(&mut self) -> Result<Option<RawEvent>> {
        if self.pending_events.is_empty()
            && let Some(before) = self.position.organization_before
        {
            let page = self
                .cells
                .query::<ListActivity>(
                    Address::Organization(self.organization),
                    None,
                    ActivityQuery {
                        actor: self.owner,
                        before: Some(before),
                        limit: 50,
                    },
                )
                .await
                .map_err(super::super::read_error)?
                .output;
            // Pin the newest visible native revision before inspecting the
            // other source. Appends cannot move this page's starting point.
            self.position.organization_before = page.items.first().map(|event| event.revision + 1);
            self.pending_events = page.items.into();
            self.more_events = page.next.is_some();
        }
        if self.pending_repository.is_none()
            && let Some(position) = self.position.repository
        {
            self.pending_repository = Some(
                self.catalog
                    .organization_audit_entry(&self.snapshot, self.organization, position)
                    .await
                    .map_err(|error| Error::Membership(Box::new(error)))?,
            );
        }
        let take_organization = match (self.pending_events.front(), &self.pending_repository) {
            (Some(org), Some(repo)) => org.created_at_ms >= repo.occurred_at_ms,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => return Ok(None),
        };
        if take_organization && let Some(event) = self.pending_events.pop_front() {
            self.position.organization_before =
                (!self.pending_events.is_empty() || self.more_events).then_some(event.revision);
            Ok(Some(event.into()))
        } else {
            Ok(self.pending_repository.take().map(|event| {
                self.position.repository = event.previous;
                event.into()
            }))
        }
    }
}

async fn profiles(cells: &CellClient, events: &mut [AuditEvent]) -> Result<()> {
    let ids = events
        .iter()
        .filter_map(|event| event.actor)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut profiles = BTreeMap::new();
    for chunk in ids.chunks(100) {
        let result = app::query_output(
            cells
                .query::<LookupProfiles>(Address::Directory, None, chunk.to_vec())
                .await,
        )?;
        if result.len() != chunk.len() {
            return Err(Error::CellContract(
                "Audit profile lookup returned an invalid count",
            ));
        }
        profiles.extend(chunk.iter().copied().zip(result));
    }
    for event in events {
        event.actor_profile = event
            .actor
            .and_then(|id| profiles.get(&id).cloned().flatten());
    }
    Ok(())
}
