use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    middleware,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    api::profiles::attribution::Authors,
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::collaboration::{
        Subject, SubjectKind,
        activity::{Action, Event, ListActivity, Query as ActivityQuery},
    },
    repository::Repository,
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/activity", get(repository))
        .route(
            "/api/repos/{owner}/{name}/issues/{number}/activity",
            get(issue),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/activity",
            get(pull),
        )
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Parameters {
    before: Option<u64>,
    limit: Option<u8>,
}

async fn page(
    server: &Server,
    principal: &Principal,
    key: (String, String),
    subject: Option<Subject>,
    params: Parameters,
) -> Result<Json<Value>> {
    let repo = app::repository(server, principal, &key)?;
    let limit = params.limit.unwrap_or(30);
    if !(1..=50).contains(&limit) {
        return Err(Error::Invalid("Activity pages require 1–50 events"));
    }
    if let Some(before) = params.before {
        app::number(before)?;
    }
    if let Some(subject) = subject {
        app::number(subject.number)?;
    }
    let cell = app::repository_cell(server, &repo)?;
    let page = app::query_output(
        cell.client
            .query::<ListActivity>(
                &cell.target,
                None,
                ActivityQuery {
                    subject,
                    before: params.before,
                    limit,
                },
            )
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let authors = Authors::load(
        server,
        principal,
        page.items.iter().map(|event| &event.actor),
    )
    .await?;
    let sources = CommitSources::load(server, principal, &repo, page.items.iter()).await?;
    app::membership(server, principal, &repo).await?;
    let items = page
        .items
        .iter()
        .map(|event| view(&authors, &sources, event))
        .collect::<Vec<_>>();
    Ok(Json(json!({"items":items,"next":page.next})))
}

pub(in crate::api) struct CommitSources(BTreeMap<Uuid, Option<Arc<Repository>>>);

impl CommitSources {
    pub(in crate::api) async fn load<'a>(
        server: &Server,
        principal: &Principal,
        repo: &Repository,
        events: impl Iterator<Item = &'a Event>,
    ) -> Result<Self> {
        let mut sources = BTreeMap::new();
        for event in events {
            let Action::IssueClosed {
                external_commit: Some(commit),
                ..
            } = &event.action
            else {
                continue;
            };
            let std::collections::btree_map::Entry::Vacant(entry) =
                sources.entry(commit.repository)
            else {
                continue;
            };
            let source = match (
                server.repositories.by_id(commit.repository),
                principal.related_read(repo.id, commit.repository),
            ) {
                (Some(source), Some(reader)) => {
                    match app::membership(server, &reader, &source).await {
                        Ok(grant) if grant.principal.can_read(source.id) => Some(source),
                        Ok(_) | Err(Error::NotFound) => None,
                        Err(error) => return Err(error),
                    }
                }
                _ => None,
            };
            entry.insert(source);
        }
        Ok(Self(sources))
    }
}

pub(in crate::api) fn view(authors: &Authors, sources: &CommitSources, event: &Event) -> Value {
    let mut action = json!(event.action);
    // The recorded commit belongs to an independently protected repository.
    // Always remove the native field before presenting either timeline.
    if let Some(fields) = action.as_object_mut()
        && fields.remove("external_commit").is_some()
        && let Action::IssueClosed {
            external_commit: Some(commit),
            ..
        } = &event.action
        && let Some(Some(repo)) = sources.0.get(&commit.repository)
    {
        fields.insert("commit".into(), json!(commit.oid));
        fields.insert(
            "commit_repository".into(),
            json!({
                "id":repo.id,"owner":repo.config.owner,"name":repo.config.name
            }),
        );
    }
    json!({"number":event.number,"target":event.target,"title":event.title,
    "actor":authors.view(&event.actor),"action":action,"created_at":event.created_at})
}

async fn repository(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Value>> {
    page(&server, &principal, key, None, params).await
}

async fn issue(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Value>> {
    let subject = Subject {
        kind: SubjectKind::Issue,
        number,
    };
    page(&server, &principal, (owner, name), Some(subject), params).await
}

async fn pull(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Value>> {
    let subject = Subject {
        kind: SubjectKind::Pull,
        number,
    };
    page(&server, &principal, (owner, name), Some(subject), params).await
}

#[cfg(test)]
mod tests;
