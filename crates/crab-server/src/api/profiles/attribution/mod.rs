use std::collections::BTreeMap;

use serde::Serialize;

use crate::{
    app::{self, Result},
    auth::Principal,
    cells::{
        Address,
        directory::{ActorSummary, IdentityKey, LookupActors},
        repository::RepositoryAuthor,
    },
    server::Server,
};

// The recorded name preserves historical attribution for unenrolled accounts.
// Only a qualified Directory match supplies a public link; names are never keys.
#[derive(Serialize)]
pub(in crate::api) struct Author<'a> {
    pub name: &'a str,
    pub profile: Option<&'a ActorSummary>,
}

pub(in crate::api) struct Authors(BTreeMap<(String, String), ActorSummary>);

impl Authors {
    pub(in crate::api) async fn load<'a>(
        server: &Server,
        principal: &Principal,
        authors: impl Iterator<Item = &'a RepositoryAuthor>,
    ) -> Result<Self> {
        // The HTTP boundary admits browser sessions and the local operator.
        // Keep account lookup closed if another credential reaches this owner.
        if !(principal.is_user() || principal.is_local()) {
            return Err(app::Error::SessionRequired);
        }
        let keys: std::collections::BTreeSet<_> = authors
            .map(|author| (author.issuer.clone(), author.subject.clone()))
            .collect();
        if keys.is_empty() {
            return Ok(Self(BTreeMap::new()));
        }
        if keys.len() > 100 {
            return Err(app::Error::Invalid(
                "Author page exceeds its identity limit",
            ));
        }
        let identities = keys
            .iter()
            .map(|(issuer, subject)| IdentityKey {
                issuer: issuer.clone(),
                subject: subject.clone(),
            })
            .collect();
        let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
        let profiles = app::query_output(
            cells
                .query::<LookupActors>(Address::Directory, None, identities)
                .await,
        )?;
        // Positions bind results to qualified keys; never zip a partial reply
        // into a different author's public profile.
        if profiles.len() != keys.len() {
            return Err(app::Error::CellContract(
                "Author lookup returned an invalid result count",
            ));
        }
        Ok(Self(
            keys.into_iter()
                .zip(profiles)
                .filter_map(|(key, profile)| profile.map(|profile| (key, profile)))
                .collect(),
        ))
    }

    pub(in crate::api) fn view<'a>(&'a self, author: &'a RepositoryAuthor) -> Author<'a> {
        Author {
            name: &author.name,
            profile: self.0.get(&(author.issuer.clone(), author.subject.clone())),
        }
    }
}

#[cfg(test)]
mod tests;
