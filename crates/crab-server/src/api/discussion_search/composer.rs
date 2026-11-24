use std::{collections::BTreeSet, sync::Arc};

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    response::Redirect,
};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    api::members::roster,
    app::{self, Error, Result},
    auth::{GroupEvidence, Principal},
    cells::{
        Address,
        directory::{
            ActorSummary, LookupActors, MentionCandidate, MentionCandidates, MentionSearch,
        },
        repository::{
            collaboration::{SubjectKind, SubjectSummary},
            search::index::references::{FindReferences, ReferenceInput},
        },
    },
    repository::Repository,
    server::Server,
};

const MENTION_LIMIT: usize = 12;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    repository_id: Uuid,
    #[serde(default)]
    search: String,
}

#[derive(Serialize)]
pub(super) struct Suggestions<T> {
    items: Vec<T>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReferenceParameters {
    repository_id: Option<Uuid>,
    #[serde(default)]
    search: String,
    #[serde(default)]
    exact: bool,
}

#[derive(Serialize)]
pub(super) struct ReferenceSuggestions {
    repository_id: Uuid,
    items: Vec<SubjectSummary>,
}

async fn find_references(
    server: &Server,
    repo: &Repository,
    input: ReferenceInput,
) -> Result<Vec<SubjectSummary>> {
    input.validate().map_err(Error::Invalid)?;
    let cell = app::repository_cell(server, repo)?;
    app::query_output(
        cell.client
            .query::<FindReferences>(&cell.target, None, input)
            .await,
    )
}

pub(super) async fn references(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(params): Query<ReferenceParameters>,
) -> Result<Json<ReferenceSuggestions>> {
    let repo = app::repository(&server, &principal, &key)?;
    if params.repository_id.is_some_and(|id| repo.id != id) {
        return Err(Error::NotFound);
    }
    let items = find_references(
        &server,
        &repo,
        ReferenceInput {
            search: params.search,
            exact: params.exact,
            author: None,
        },
    )
    .await?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(ReferenceSuggestions {
        repository_id: repo.id,
        items,
    }))
}

pub(super) async fn open_reference(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, kind, number)): Path<(Uuid, SubjectKind, u64)>,
) -> Result<Redirect> {
    let repo = server.repositories.by_id(id).ok_or(Error::NotFound)?;
    app::membership(&server, &principal, &repo).await?;
    let number = app::number(number)?;
    let items = find_references(
        &server,
        &repo,
        ReferenceInput {
            search: number.to_string(),
            exact: true,
            author: None,
        },
    )
    .await?;
    if !items.iter().any(|item| item.subject.kind == kind) {
        return Err(Error::NotFound);
    }
    app::membership(&server, &principal, &repo).await?;
    let path = match kind {
        SubjectKind::Issue => "issues",
        SubjectKind::Pull => "pull",
    };
    // Catalog owner and name contain only URL-safe characters. Keep the UUID
    // on the destination so the browser can reject a reused repository name.
    Ok(Redirect::to(&format!(
        "/{}/{}/{path}/{number}?repository_id={id}",
        repo.config.owner, repo.config.name,
    )))
}

pub(super) async fn mentions(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Suggestions<ActorSummary>>> {
    let repo = app::repository(&server, &principal, &key)?;
    if repo.id != params.repository_id {
        return Err(Error::NotFound);
    }
    let input = ReferenceInput {
        search: params.search,
        exact: false,
        author: None,
    };
    input.validate().map_err(Error::Invalid)?;
    let search = input.search.trim().to_lowercase();
    let initial = app::membership(&server, &principal, &repo).await?;
    let keys = roster::identities(&principal, &roster::members(&server, &initial).await?)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let mut matches = Vec::new();
    for batch in keys.chunks(100) {
        let profiles = app::query_output(
            cells
                .query::<LookupActors>(Address::Directory, None, batch.to_vec())
                .await,
        )?;
        if profiles.len() != batch.len() {
            return Err(Error::CellContract(
                "Mention lookup returned an invalid result count",
            ));
        }
        matches.extend(batch.iter().zip(profiles).filter_map(|(key, profile)| {
            let profile = profile?;
            (profile.handle.contains(&search) || profile.name.to_lowercase().contains(&search))
                .then_some((&key.subject, profile))
        }));
    }
    let group_matches = group_mentions(&server, &initial, &search).await?;
    let current = app::membership(&server, &principal, &repo).await?;
    let current_subjects: BTreeSet<_> = roster::members(&server, &current)
        .await?
        .into_iter()
        .map(|member| member.subject)
        .collect();
    let mut profiles: Vec<_> = matches
        .into_iter()
        .filter_map(|(subject, actor)| current_subjects.contains(subject).then_some(actor))
        .chain(
            group_matches
                .into_iter()
                .filter_map(|(candidate, evidence)| {
                    (current_subjects.contains(&candidate.subject)
                        || evidence.can_read(&current.groups))
                    .then_some(candidate.actor)
                }),
        )
        .collect();
    profiles.sort_unstable_by(|left, right| {
        let rank =
            |actor: &ActorSummary| (actor.handle != search, !actor.handle.starts_with(&search));
        rank(left)
            .cmp(&rank(right))
            .then_with(|| left.handle.cmp(&right.handle))
    });
    let mut seen = BTreeSet::new();
    let items = profiles
        .into_iter()
        .filter(|actor| seen.insert(actor.id))
        .take(MENTION_LIMIT)
        .collect();
    Ok(Json(Suggestions { items }))
}

async fn group_mentions(
    server: &Server,
    membership: &app::Membership,
    search: &str,
) -> Result<Vec<(MentionCandidate, GroupEvidence)>> {
    let Some(auth) = server.auth.as_ref().filter(|auth| auth.is_oidc()) else {
        return Ok(Vec::new());
    };
    if membership.groups.is_empty() {
        return Ok(Vec::new());
    }
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let candidates = app::query_output(
        cells
            .query::<MentionCandidates>(
                Address::Directory,
                None,
                MentionSearch {
                    issuer: auth.issuer().to_owned(),
                    search: search.to_owned(),
                },
            )
            .await,
    )?;
    let direct: BTreeSet<_> = membership
        .members
        .iter()
        .map(|member| member.subject.as_str())
        .collect();
    let candidates: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| !direct.contains(candidate.subject.as_str()))
        .collect();
    let groups = &membership.groups;
    let mut checks = stream::iter(candidates)
        .map(|candidate| async move {
            let evidence = auth.mention_group(&candidate.subject, groups).await?;
            Ok::<_, Error>(evidence.map(|proof| (candidate, proof)))
        })
        .buffered(6);
    let mut matches = Vec::new();
    while let Some(result) = checks.next().await {
        if let Some(candidate) = result? {
            matches.push(candidate);
            if matches.len() == MENTION_LIMIT {
                break;
            }
        }
    }
    Ok(matches)
}

#[cfg(test)]
mod tests;
