use crate::{
    api::members::roster::{Roster, accounts, identities, members, profiles},
    app::{self, Error, Result},
    auth::Principal,
    cells::{directory::ActorSummary, repository::assignments::Assignee},
    repository::Repository,
    server::Server,
};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub(crate) struct Selection {
    profiles: BTreeMap<Uuid, ActorSummary>,
    names: BTreeMap<Uuid, String>,
    pub principal: Principal,
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
    pub fn validate(roster: &Roster, mut ids: Vec<Uuid>) -> Result<Vec<Assignee>> {
        ids.sort_unstable();
        if ids.len() > 10 || ids.windows(2).any(|p| p[0] == p[1]) {
            return Err(Error::Invalid(
                "An item supports at most 10 unique assignees",
            ));
        }
        ids.into_iter()
            .map(|id| {
                let member = roster
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
