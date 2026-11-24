use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use axum::Json;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::issues::transfer::publication::{Destination, GetLocation, Location},
    repository::Repository,
    server::Server,
};

#[cfg(test)]
mod tests;

enum Resolution {
    Ready(PublishedIssue),
    Pending,
    Unavailable,
}

pub(in crate::api) struct PublishedIssue {
    pub repository: Arc<Repository>,
    pub number: u64,
    grants: BTreeMap<Uuid, (Arc<Repository>, Principal)>,
}

impl PublishedIssue {
    pub(in crate::api) async fn visible(&self, server: &Server) -> Result<bool> {
        verify_grants(server, &self.grants).await
    }
}

async fn verify_grants(
    server: &Server,
    grants: &BTreeMap<Uuid, (Arc<Repository>, Principal)>,
) -> Result<bool> {
    for (repo, reader) in grants.values() {
        match app::membership(server, reader, repo).await {
            Ok(_) => {}
            Err(Error::NotFound) => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

pub(in crate::api) async fn published(
    server: &Server,
    principal: &Principal,
    source: &Arc<Repository>,
    issue: u64,
) -> Result<Option<PublishedIssue>> {
    match resolve(server, principal, source, issue).await {
        Ok(Resolution::Ready(issue)) => Ok(Some(issue)),
        Ok(Resolution::Pending | Resolution::Unavailable) | Err(Error::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) async fn detail(
    server: &Server,
    principal: &Principal,
    source: &Arc<Repository>,
    issue: u64,
) -> Result<Json<Value>> {
    let resolved = resolve(server, principal, source, issue).await?;
    let transfer = match resolved {
        Resolution::Ready(issue) if issue.visible(server).await? => json!({
            "state": "ready",
            "number": issue.number,
            "repository": {
                "id": issue.repository.id,
                "owner": issue.repository.config.owner,
                "name": issue.repository.config.name,
            },
        }),
        Resolution::Pending => json!({"state": "pending"}),
        Resolution::Ready(_) | Resolution::Unavailable => json!({"state": "unavailable"}),
    };
    Ok(Json(json!({"transfer": transfer})))
}

async fn location(server: &Server, repo: &Repository, issue: u64) -> Result<Location> {
    let cell = app::repository_cell(server, repo)?;
    app::query_output(
        cell.client
            .query::<GetLocation>(&cell.target, None, issue)
            .await,
    )
}

async fn resolve(
    server: &Server,
    principal: &Principal,
    source: &Arc<Repository>,
    issue: u64,
) -> Result<Resolution> {
    let origin = app::membership(server, principal, source).await?;
    let Location::Moved { mut destination } = location(server, source, issue).await? else {
        return Err(Error::NotFound);
    };
    let Some(browser) = origin.principal.related_catalog(source.id) else {
        return Ok(Resolution::Unavailable);
    };
    let mut seen = BTreeSet::from([(source.id, issue)]);
    let mut grants = BTreeMap::from([(source.id, (Arc::clone(source), principal.clone()))]);
    let resolved = loop {
        let Destination { repository, number } = destination;
        if !seen.insert((repository, number)) {
            return Err(Error::CellContract("Issue transfer redirects form a cycle"));
        }
        let Some(repo) = server.repositories.by_id(repository) else {
            break Resolution::Unavailable;
        };
        let reader = browser.clone().scoped_to_repository(Some(repository));
        match app::membership(server, &reader, &repo).await {
            Ok(_) => {}
            Err(Error::NotFound) => break Resolution::Unavailable,
            Err(error) => return Err(error),
        }
        grants.insert(repository, (Arc::clone(&repo), reader));
        match location(server, &repo, number).await? {
            Location::Present => {
                break Resolution::Ready(PublishedIssue {
                    repository: repo,
                    number,
                    grants,
                });
            }
            Location::Moved { destination: next } => destination = next,
            Location::Missing => {
                break if verify_grants(server, &grants).await? {
                    Resolution::Pending
                } else {
                    Resolution::Unavailable
                };
            }
        }
    };
    // Direct lookups still require the original grant even when a destination
    // is pending or unavailable. Ready callers recheck the entire path.
    app::membership(server, principal, source).await?;
    Ok(resolved)
}
