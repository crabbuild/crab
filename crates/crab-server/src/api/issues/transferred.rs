use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use axum::Json;
use serde_json::{Value, json};

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
    Ready {
        repository: Arc<Repository>,
        number: u64,
    },
    Pending,
    Unavailable,
}

pub(super) async fn detail(
    server: &Server,
    principal: &Principal,
    source: &Repository,
    issue: u64,
) -> Result<Json<Value>> {
    let resolved = resolve(server, principal, source, issue).await?;
    let transfer = match resolved {
        Resolution::Ready { repository, number } => json!({
            "state": "ready",
            "number": number,
            "repository": {
                "id": repository.id,
                "owner": repository.config.owner,
                "name": repository.config.name,
            },
        }),
        Resolution::Pending => json!({"state": "pending"}),
        Resolution::Unavailable => json!({"state": "unavailable"}),
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
    source: &Repository,
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
    let mut access = BTreeMap::new();
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
        access.insert(repository, (Arc::clone(&repo), reader));
        match location(server, &repo, number).await? {
            Location::Present => {
                break Resolution::Ready {
                    repository: repo,
                    number,
                };
            }
            Location::Moved { destination: next } => destination = next,
            Location::Missing => break Resolution::Pending,
        }
    };
    // Permissions can change during Cell reads. A redirect discloses the
    // final path only if the original grant and every traversed grant hold.
    app::membership(server, principal, source).await?;
    for (_, (repo, reader)) in access {
        match app::membership(server, &reader, &repo).await {
            Ok(_) => {}
            Err(Error::NotFound) => return Ok(Resolution::Unavailable),
            Err(error) => return Err(error),
        }
    }
    Ok(resolved)
}
