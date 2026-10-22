use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        directory::{ActorSummary, IdentityKey, LookupAccounts, LookupProfiles, RegisterAccounts},
        repository::assignments::Assignee,
    },
    repository::Repository,
    server::Server,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Serialize)]
pub(crate) struct Candidate {
    pub id: Uuid,
    pub name: String,
    pub profile: Option<ActorSummary>,
}
pub(crate) struct Selection {
    profiles: BTreeMap<Uuid, ActorSummary>,
    names: BTreeMap<Uuid, String>,
    pub principal: Principal,
}

fn members(membership: &app::Membership) -> Vec<crate::RepositoryMember> {
    if membership.principal.is_local() {
        return vec![crate::RepositoryMember {
            subject: "operator".into(),
            name: "Local operator".into(),
            access: crate::RepositoryAccess::Admin,
        }];
    }
    membership.members.clone()
}
fn identities(
    principal: &Principal,
    members: &[crate::RepositoryMember],
) -> Result<Vec<IdentityKey>> {
    let actor = app::actor(principal)?;
    Ok(members
        .iter()
        .map(|member| IdentityKey {
            issuer: actor.issuer.clone(),
            subject: member.subject.clone(),
        })
        .collect())
}
async fn accounts(server: &Server, keys: &[IdentityKey]) -> Result<Vec<Option<Uuid>>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let mut result = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(100) {
        let ids = app::query_output(
            cells
                .query::<LookupAccounts>(Address::Directory, None, chunk.to_vec())
                .await,
        )?;
        if ids.len() != chunk.len() {
            return Err(Error::CellContract(
                "Account lookup returned an invalid result count",
            ));
        }
        result.extend(ids);
    }
    Ok(result)
}
async fn profiles(server: &Server, ids: &BTreeSet<Uuid>) -> Result<BTreeMap<Uuid, ActorSummary>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let ids: Vec<_> = ids.iter().copied().collect();
    let mut result = BTreeMap::new();
    for chunk in ids.chunks(100) {
        let profiles = app::query_output(
            cells
                .query::<LookupProfiles>(Address::Directory, None, chunk.to_vec())
                .await,
        )?;
        if profiles.len() != chunk.len() {
            return Err(Error::CellContract(
                "Profile lookup returned an invalid result count",
            ));
        }
        for (id, profile) in chunk.iter().zip(profiles) {
            if let Some(profile) = profile {
                if profile.id != *id {
                    return Err(Error::CellContract("Profile lookup returned another actor"));
                }
                result.insert(*id, profile);
            }
        }
    }
    Ok(result)
}
impl Selection {
    pub(crate) async fn load<'a>(
        server: &Server,
        principal: &Principal,
        repo: &Repository,
        selected: impl Iterator<Item = &'a Assignee>,
    ) -> Result<Self> {
        let ids: BTreeSet<_> = selected.map(|a| a.id).collect();
        let initial = app::membership(server, principal, repo).await?;
        if ids.is_empty() {
            return Ok(Self {
                profiles: BTreeMap::new(),
                names: BTreeMap::new(),
                principal: initial.principal,
            });
        }
        let initial_members = members(&initial);
        let keys = identities(principal, &initial_members)?;
        let (bound, profiles) = tokio::try_join!(accounts(server, &keys), profiles(server, &ids))?;
        let current = app::membership(server, principal, repo).await?;
        let current_members = members(&current);
        // Profiles and names are presentation only. Final current membership
        // decides visibility and capabilities after the separate Directory reads.
        let names = initial_members
            .iter()
            .zip(bound)
            .filter_map(|(member, id)| {
                let current = current_members
                    .iter()
                    .find(|current| current.subject == member.subject)?;
                Some((id?, current.name.clone()))
            })
            .collect();
        Ok(Self {
            profiles,
            names,
            principal: current.principal,
        })
    }
    pub(crate) fn view(&self, selected: &[Assignee]) -> Vec<serde_json::Value> {
        selected
            .iter()
            .map(|assignee| {
                let profile = self.profiles.get(&assignee.id);
                let name = profile
                    .map(|p| p.name.as_str())
                    .or_else(|| self.names.get(&assignee.id).map(String::as_str))
                    .unwrap_or(&assignee.name);
                serde_json::json!({"id":assignee.id,"name":name,"profile":profile})
            })
            .collect()
    }
}

pub(crate) struct Roster {
    pub items: Vec<Candidate>,
    pub principal: Principal,
}
impl Roster {
    pub async fn load(server: &Server, principal: &Principal, repo: &Repository) -> Result<Self> {
        let initial = app::membership(server, principal, repo).await?;
        let initial_members = members(&initial);
        let keys = identities(principal, &initial_members)?;
        let mut ids = accounts(server, &keys).await?;
        let missing: Vec<_> = ids
            .iter()
            .enumerate()
            .filter_map(|(i, id)| id.is_none().then_some(i))
            .collect();
        let cells = server.cells().ok_or(Error::CellUnavailable)?;
        for chunk in missing.chunks(100) {
            let input = chunk.iter().map(|i| keys[*i].clone()).collect();
            // Registration allocates private identity only, never a public handle
            // or grant. Idempotence permits harmless orphan IDs after revocation.
            let registered = app::command_output(
                cells
                    .command::<RegisterAccounts>(
                        Address::Directory,
                        app::mutation_identity()?,
                        input,
                    )
                    .await,
            )?;
            if registered.len() != chunk.len() {
                return Err(Error::CellContract(
                    "Account registration returned an invalid result count",
                ));
            }
            for (i, id) in chunk.iter().zip(registered) {
                ids[*i] = Some(id);
            }
        }
        let profiles = profiles(server, &ids.iter().flatten().copied().collect()).await?;
        let current = app::membership(server, principal, repo).await?;
        let mut items = Vec::new();
        for member in members(&current) {
            let position = initial_members
                .iter()
                .position(|m| m.subject == member.subject)
                .ok_or(Error::Conflict)?;
            let id = ids[position].ok_or(Error::CellContract("Registered account is missing"))?;
            let profile = profiles.get(&id).cloned();
            let name = profile.as_ref().map_or(member.name, |p| p.name.clone());
            items.push(Candidate { id, name, profile });
        }
        Ok(Self {
            items,
            principal: current.principal,
        })
    }
    pub fn validate(&self, mut ids: Vec<Uuid>) -> Result<Vec<Assignee>> {
        ids.sort_unstable();
        if ids.len() > 10 || ids.windows(2).any(|p| p[0] == p[1]) {
            return Err(Error::Invalid(
                "An item supports at most 10 unique assignees",
            ));
        }
        ids.into_iter()
            .map(|id| {
                let member = self
                    .items
                    .iter()
                    .find(|m| m.id == id)
                    .ok_or(Error::Invalid(
                        "Assignee selection contains an unavailable repository member",
                    ))?;
                Ok(Assignee {
                    id,
                    name: member.name.clone(),
                })
            })
            .collect()
    }
}
