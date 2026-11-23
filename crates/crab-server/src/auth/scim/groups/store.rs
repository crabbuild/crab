use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use crab_storage::{ETag, StorageError};
use object_store::path::Path as ObjectPath;
use serde::{Deserialize, Serialize};

use super::*;
use crate::{
    app,
    cells::{
        Address,
        directory::{IdentityKey, LookupAccounts},
        organization::teams::sync::{Binding, SyncAction, SyncGroup, SyncInput, SyncOutcome},
    },
};

const MAX_DIRECTORY: usize = 8 * 1024 * 1024;
const MAX_GROUPS: usize = 4096;
const MAX_LINKS: usize = 32;
const ATTEMPTS: usize = 8;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Group {
    pub(super) id: Uuid,
    pub(super) external_id: Option<String>,
    pub(super) name: String,
    pub(super) members: BTreeSet<Uuid>,
    pub(super) revision: u64,
    pub(super) created: String,
    pub(super) updated: String,
    pub(super) deleted: bool,
    pub(super) links: Vec<Link>,
}

impl Group {
    pub(super) fn tag(&self) -> String {
        format!("W/\"{}\"", self.revision)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Link {
    target: TeamTarget,
    generation: Uuid,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::auth::scim) struct TeamTarget {
    pub(in crate::auth::scim) organization: Uuid,
    pub(in crate::auth::scim) team: Uuid,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    group: Uuid,
    edit: Option<(Link, SyncAction)>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    groups: BTreeMap<Uuid, Group>,
    pending: Option<Pending>,
}

pub(super) struct Groups<'a> {
    server: &'a Server,
    pub(super) auth: &'a Authentication,
}

impl<'a> Groups<'a> {
    pub(super) fn new(server: &'a Server) -> Result<Self> {
        let auth = server.auth.as_ref().ok_or_else(Error::not_found)?;
        Ok(Self { server, auth })
    }

    fn path(&self) -> ObjectPath {
        let issuer = blake3::hash(self.auth.issuer().as_bytes());
        ObjectPath::from(format!("{}/scim/{issuer}/groups", self.auth.state.prefix))
    }

    async fn load(&self) -> Result<(Snapshot, Option<ETag>)> {
        match self
            .auth
            .state
            .store
            .get_with_etag_bounded(&self.path(), MAX_DIRECTORY as u64)
            .await
        {
            Ok((data, etag)) => Ok((
                serde_json::from_slice(&data).map_err(Error::unavailable)?,
                Some(etag),
            )),
            Err(StorageError::NotFound { .. }) => Ok((Snapshot::default(), None)),
            Err(error) => Err(Error::unavailable(error)),
        }
    }

    async fn save(&self, snapshot: &Snapshot, etag: Option<ETag>) -> Result<bool> {
        let data = serde_json::to_vec(snapshot).map_err(Error::unavailable)?;
        if data.len() > MAX_DIRECTORY || snapshot.groups.len() > MAX_GROUPS {
            return Err(Error::capacity());
        }
        let body = Bytes::from(data);
        let path = self.path();
        let result = match etag {
            None => self.auth.state.store.create_strict(&path, body).await,
            Some(etag) => self
                .auth
                .state
                .store
                .update(&path, body, etag)
                .await
                .map(drop),
        };
        match result {
            Ok(()) => Ok(true),
            Err(StorageError::StateConflict { .. }) => Ok(false),
            Err(error) => Err(Error::unavailable(error)),
        }
    }

    async fn settled(&self) -> Result<(Snapshot, Option<ETag>)> {
        for _ in 0..ATTEMPTS {
            let (mut state, etag) = self.load().await?;
            let Some(pending) = state.pending else {
                return Ok((state, etag));
            };
            let group = state
                .groups
                .get_mut(&pending.group)
                .ok_or_else(Error::busy)?;
            if let Some((link, action)) = pending.edit {
                self.reconcile_link(group, link, action).await?;
            } else if !group.links.is_empty() {
                let people = self.people(&group.members).await?;
                let mut live = Vec::with_capacity(group.links.len());
                for link in &group.links {
                    if self
                        .project(group, *link, SyncAction::Refresh, people.clone())
                        .await?
                        == SyncOutcome::Applied
                    {
                        live.push(*link);
                    }
                }
                group.links = live;
            }
            // Clear the intent with the observed CAS token. A delayed peer
            // cannot overwrite a newer mutation or its projection.
            state.pending = None;
            self.save(&state, etag).await?;
        }
        Err(Error::busy())
    }

    async fn reconcile_link(
        &self,
        group: &mut Group,
        link: Link,
        action: SyncAction,
    ) -> Result<()> {
        let people = match action {
            SyncAction::Link { .. } => self.people(&group.members).await?,
            SyncAction::Unlink { .. } => Vec::new(),
            SyncAction::Refresh => return Err(Error::busy()),
        };
        let outcome = self.project(group, link, action, people).await?;
        match (action, outcome) {
            (SyncAction::Link { .. }, SyncOutcome::Applied) => group.links.push(link),
            (SyncAction::Unlink { .. }, SyncOutcome::Applied | SyncOutcome::Missing) => {
                group.links.retain(|candidate| *candidate != link);
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) async fn users_exist(&self, members: &BTreeSet<Uuid>) -> Result<()> {
        if members.is_empty() {
            return Ok(());
        }
        let users = super::super::store::Directory::new(self.server)?
            .users()
            .await?;
        if members.iter().any(|id| !users.contains_key(id)) {
            return Err(Error::invalid(
                "invalidValue",
                "Group members must identify provisioned SCIM users",
            ));
        }
        Ok(())
    }

    async fn people(&self, members: &BTreeSet<Uuid>) -> Result<Vec<Uuid>> {
        if members.is_empty() {
            return Ok(Vec::new());
        }
        let users = super::super::store::Directory::new(self.server)?
            .users()
            .await?;
        let keys = members
            .iter()
            .map(|id| {
                let user = users.get(id).ok_or_else(Error::busy)?;
                Ok(IdentityKey {
                    issuer: self.auth.issuer().into(),
                    subject: user.external_id.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let cells = self.server.cells().ok_or_else(Error::busy)?;
        let mut ids = Vec::with_capacity(keys.len());
        for page in keys.chunks(100) {
            let resolved = cells
                .query::<LookupAccounts>(Address::Directory, None, page.to_vec())
                .await
                .map_err(Error::unavailable)?
                .output;
            if resolved.len() != page.len() {
                return Err(Error::busy());
            }
            for id in resolved {
                ids.push(id.ok_or_else(Error::busy)?);
            }
        }
        Ok(ids)
    }

    async fn project(
        &self,
        group: &Group,
        link: Link,
        action: SyncAction,
        members: Vec<Uuid>,
    ) -> Result<SyncOutcome> {
        let cells = self.server.cells().ok_or_else(Error::busy)?;
        let input = SyncInput {
            team: link.target.team,
            binding: Binding {
                group: group.id,
                generation: link.generation,
                revision: group.revision,
                name: group.name.clone(),
            },
            members,
            action,
        };
        let identity = app::mutation_identity().map_err(Error::unavailable)?;
        cells
            .command::<SyncGroup>(
                Address::Organization(link.target.organization),
                identity,
                input,
            )
            .await
            .map(|outcome| outcome.output)
            .map_err(Error::unavailable)
    }

    pub(super) async fn list(&self) -> Result<BTreeMap<Uuid, Group>> {
        Ok(self.settled().await?.0.groups)
    }

    pub(super) async fn update(
        &self,
        edit: impl Fn(&mut BTreeMap<Uuid, Group>) -> Result<Uuid>,
    ) -> Result<Group> {
        for _ in 0..ATTEMPTS {
            let (mut state, etag) = self.settled().await?;
            let id = edit(&mut state.groups)?;
            let group = state.groups.get(&id).ok_or_else(Error::busy)?;
            self.users_exist(&group.members).await?;
            state.pending = Some(Pending {
                group: id,
                edit: None,
            });
            if self.save(&state, etag).await? {
                self.settled().await?;
                return state.groups.remove(&id).ok_or_else(Error::busy);
            }
        }
        Err(Error::busy())
    }

    pub(in crate::auth::scim) async fn link(
        &self,
        target: TeamTarget,
        actor: Uuid,
        version: u64,
        group: Option<Uuid>,
    ) -> Result<()> {
        for _ in 0..ATTEMPTS {
            let (mut state, etag) = self.settled().await?;
            let existing = linked(&state, target);
            if existing.map(|(id, _)| id) == group {
                return Ok(());
            }
            let (id, link, action) = match (group, existing) {
                (Some(id), None) => {
                    let entry = state
                        .groups
                        .get(&id)
                        .filter(|group| !group.deleted)
                        .ok_or_else(Error::not_found)?;
                    if entry.links.len() == MAX_LINKS {
                        return Err(Error::capacity());
                    }
                    (
                        id,
                        Link {
                            target,
                            generation: Uuid::now_v7(),
                        },
                        SyncAction::Link { actor, version },
                    )
                }
                (None, Some((id, link))) => (id, link, SyncAction::Unlink { actor, version }),
                _ => {
                    return Err(Error::conflict(
                        "Unlink the current identity provider group first",
                    ));
                }
            };
            state.pending = Some(Pending {
                group: id,
                edit: Some((link, action)),
            });
            if self.save(&state, etag).await? {
                let (current, _) = self.settled().await?;
                return match linked(&current, target).map(|(id, _)| id) == group {
                    true => Ok(()),
                    false => Err(Error::conflict("The team changed. Reload and try again")),
                };
            }
        }
        Err(Error::busy())
    }
}

fn linked(state: &Snapshot, target: TeamTarget) -> Option<(Uuid, Link)> {
    state.groups.values().find_map(|group| {
        group
            .links
            .iter()
            .find(|link| link.target == target)
            .map(|link| (group.id, *link))
    })
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
