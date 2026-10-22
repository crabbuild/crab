use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        directory::{ActorSummary, IdentityKey, LookupAccounts, LookupProfiles, RegisterAccounts},
        repository::RepositoryAuthor,
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
pub(in crate::api) fn members(membership: &app::Membership) -> Vec<crate::RepositoryMember> {
    if membership.principal.is_local() {
        return vec![crate::RepositoryMember {
            subject: "operator".into(),
            name: "Local operator".into(),
            access: crate::RepositoryAccess::Admin,
        }];
    }
    membership.members.clone()
}
pub(in crate::api) fn identities(
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
pub(in crate::api) async fn accounts(
    server: &Server,
    keys: &[IdentityKey],
) -> Result<Vec<Option<Uuid>>> {
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
pub(in crate::api) async fn profiles(
    server: &Server,
    ids: &BTreeSet<Uuid>,
) -> Result<BTreeMap<Uuid, ActorSummary>> {
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
pub(crate) struct Roster {
    identities: BTreeMap<Uuid, RepositoryAuthor>,
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
        let mut identities = BTreeMap::new();
        for member in members(&current) {
            let position = initial_members
                .iter()
                .position(|m| m.subject == member.subject)
                .ok_or(Error::Conflict)?;
            let id = ids[position].ok_or(Error::CellContract("Registered account is missing"))?;
            let profile = profiles.get(&id).cloned();
            // Bind only final current members. A public profile never supplies authority
            // or changes the qualified account selected by this UUID.
            identities.insert(
                id,
                RepositoryAuthor {
                    issuer: keys[position].issuer.clone(),
                    subject: member.subject,
                    name: member.name.clone(),
                },
            );
            let name = profile.as_ref().map_or(member.name, |p| p.name.clone());
            items.push(Candidate { id, name, profile });
        }
        Ok(Self {
            identities,
            items,
            principal: current.principal,
        })
    }
    pub fn author(&self, id: Uuid) -> Option<&RepositoryAuthor> {
        self.identities.get(&id)
    }
    pub fn candidate(&self, author: &RepositoryAuthor) -> Option<&Candidate> {
        let id = self.identities.iter().find_map(|(id, bound)| {
            (bound.issuer == author.issuer && bound.subject == author.subject).then_some(id)
        })?;
        self.items.iter().find(|candidate| candidate.id == *id)
    }
}
