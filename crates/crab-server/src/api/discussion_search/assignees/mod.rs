use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Extension, Json,
    extract::{Path, Query as QueryParameters, State},
};
use serde::Serialize;

use crate::{
    api::assignees::{Candidate, Roster},
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        directory::{
            CompleteHandles, IdentityKey, LookupAccounts, LookupProfiles, ResolveHandles,
            validate_handle_prefix,
        },
        repository::{
            SearchInput,
            search::{
                Choice, Query,
                assignees::{AssigneeOptions, Binding, Filter, OptionInput},
            },
        },
    },
    server::Server,
};

pub(in crate::api) async fn resolve(
    server: &Server,
    principal: &Principal,
    input: &mut SearchInput,
) -> Result<()> {
    let names = Query::parse(input.query.as_deref())
        .map_err(Error::Invalid)?
        .assignees();
    if names.is_empty() {
        input.assignees.clear();
        return Ok(());
    }
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let handles: Vec<_> = names
        .iter()
        .filter(|name| name.as_str() != "@me")
        .cloned()
        .collect();
    let keys = if handles.is_empty() {
        vec![]
    } else {
        app::query_output(
            cells
                .query::<ResolveHandles>(Address::Directory, None, handles.clone())
                .await,
        )?
    };
    if keys.len() != handles.len() {
        return Err(Error::CellContract(
            "Assignee resolution returned an invalid result count",
        ));
    }
    let mut keys: BTreeMap<_, _> = handles.into_iter().zip(keys).collect();
    if names.contains("@me") {
        let actor = app::actor(principal)?;
        keys.insert(
            "@me".into(),
            Some(IdentityKey {
                issuer: actor.issuer,
                subject: actor.subject,
            }),
        );
    }
    let qualified: Vec<_> = keys.values().flatten().cloned().collect();
    let ids = if qualified.is_empty() {
        vec![]
    } else {
        app::query_output(
            cells
                .query::<LookupAccounts>(Address::Directory, None, qualified)
                .await,
        )?
    };
    if ids.len() != keys.values().flatten().count() {
        return Err(Error::CellContract(
            "Assignee account lookup returned an invalid result count",
        ));
    }
    let mut ids = ids.into_iter();
    input.assignees = keys
        .into_iter()
        .map(|(name, key)| Binding {
            name,
            id: if key.is_some() {
                ids.next().flatten()
            } else {
                None
            },
        })
        .collect();
    Ok(())
}

#[derive(Serialize)]
struct PersonOption {
    person: Candidate,
    #[serde(flatten)]
    choice: Choice,
}
#[derive(Serialize)]
pub(super) struct Options {
    items: Vec<PersonOption>,
    clear: Choice,
    missing: Choice,
    #[serde(rename = "self")]
    current: Choice,
    any: Choice,
    truncated: bool,
}

pub(super) async fn options(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    QueryParameters(input): QueryParameters<OptionInput>,
) -> Result<Json<Options>> {
    let repo = app::repository(&server, &principal, &key)?;
    input.validate().map_err(Error::Invalid)?;
    let query = Query::parse(input.query.as_deref()).map_err(Error::Invalid)?;
    let search = input.search.trim().to_lowercase();
    let prefix = search.strip_prefix('@').unwrap_or(&search);
    let cell = app::repository_cell(&server, &repo)?;
    let snapshots = app::query_output(
        cell.client
            .query::<AssigneeOptions>(&cell.target, None, input)
            .await,
    )?;
    let ids: Vec<_> = snapshots.items.iter().map(|person| person.id).collect();
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let profiles = if ids.is_empty() {
        vec![]
    } else {
        app::query_output(
            cells
                .query::<LookupProfiles>(Address::Directory, None, ids.clone())
                .await,
        )?
    };
    if profiles.len() != ids.len()
        || profiles
            .iter()
            .zip(&ids)
            .any(|(profile, id)| profile.as_ref().is_some_and(|profile| profile.id != *id))
    {
        return Err(Error::CellContract(
            "Assignee profile lookup returned an invalid result",
        ));
    }
    let mut people: BTreeMap<_, _> = snapshots
        .items
        .into_iter()
        .zip(profiles)
        .map(|(snapshot, profile)| {
            let name = profile
                .as_ref()
                .map_or(snapshot.name, |profile| profile.name.clone());
            (
                snapshot.id,
                Candidate {
                    id: snapshot.id,
                    name,
                    profile,
                },
            )
        })
        .collect();
    let mut truncated = snapshots.truncated;
    if validate_handle_prefix(prefix).is_ok() {
        let matches = app::query_output(
            cells
                .query::<CompleteHandles>(Address::Directory, None, prefix.to_owned())
                .await,
        )?;
        truncated |= matches.truncated;
        for profile in matches.items {
            people.insert(
                profile.id,
                Candidate {
                    id: profile.id,
                    name: profile.name.clone(),
                    profile: Some(profile),
                },
            );
        }
    }
    let roster = Roster::load(&server, &principal, &repo).await?;
    for person in roster.items {
        if person.name.to_lowercase().contains(&search)
            || person
                .profile
                .as_ref()
                .is_some_and(|profile| profile.handle.contains(prefix))
            || person.id.to_string() == search
        {
            people.insert(person.id, person);
        }
    }
    let mut people: Vec<_> = people.into_values().collect();
    people.sort_unstable_by_key(|person| {
        (
            person
                .profile
                .as_ref()
                .map_or_else(|| person.name.to_lowercase(), |p| p.handle.clone()),
            person.id,
        )
    });
    truncated |= people.len() > 20;
    let items = people
        .into_iter()
        .take(20)
        .map(|person| {
            let actor = format!("actor/{}", person.id);
            let value = person
                .profile
                .as_ref()
                .map_or(actor.as_str(), |profile| profile.handle.as_str());
            let mut choice = query
                .assignee_choice(Filter::Person(value))
                .map_err(Error::Invalid)?;
            choice.selected |= query
                .assignee_choice(Filter::Person(&actor))
                .map_err(Error::Invalid)?
                .selected;
            Ok(PersonOption { person, choice })
        })
        .collect::<Result<_>>()?;
    // Public choices and historical assignments grant no access. Recheck after
    // all separate Directory/catalog reads before returning repository context.
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(Options {
        items,
        truncated,
        clear: query
            .assignee_choice(Filter::Clear)
            .map_err(Error::Invalid)?,
        missing: query
            .assignee_choice(Filter::Missing)
            .map_err(Error::Invalid)?,
        current: query
            .assignee_choice(Filter::Person("@me"))
            .map_err(Error::Invalid)?,
        any: query
            .assignee_choice(Filter::Person("*"))
            .map_err(Error::Invalid)?,
    }))
}

#[cfg(test)]
mod tests;
