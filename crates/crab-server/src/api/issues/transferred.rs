use axum::Json;
use serde_json::{Value, json};

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::{GetIssue, issues::transfer::publication::GetMove},
    repository::Repository,
    server::Server,
};

fn unavailable() -> Json<Value> {
    Json(json!({"transfer": {"state": "unavailable"}}))
}

pub(super) async fn detail(
    server: &Server,
    principal: &Principal,
    source: &Repository,
    issue: u64,
) -> Result<Json<Value>> {
    let cell = app::repository_cell(server, source)?;
    let decision = app::query_output(
        cell.client
            .query::<GetMove>(&cell.target, None, issue)
            .await,
    )?
    .ok_or(Error::NotFound)?;
    app::membership(server, principal, source).await?;
    let address = decision.destination();
    let Some(destination) = server.repositories.by_id(address.repository) else {
        return Ok(unavailable());
    };
    let Some(reader) = principal.related_read(source.id, address.repository) else {
        return Ok(unavailable());
    };
    match app::membership(server, &reader, &destination).await {
        Ok(_) => {}
        Err(Error::NotFound) => return Ok(unavailable()),
        Err(error) => return Err(error),
    }
    let cell = app::repository_cell(server, &destination)?;
    let issue_exists = app::query_output(
        cell.client
            .query::<GetIssue>(&cell.target, None, address.number)
            .await,
    )?
    .is_some();
    let ready = issue_exists
        || app::query_output(
            cell.client
                .query::<GetMove>(&cell.target, None, address.number)
                .await,
        )?
        .is_some();
    // Both grants can change during the native reads. A source link must not
    // disclose a destination location to a reader of only the original issue.
    app::membership(server, principal, source).await?;
    match app::membership(server, &reader, &destination).await {
        Ok(_) => {}
        Err(Error::NotFound) => return Ok(unavailable()),
        Err(error) => return Err(error),
    }
    if !ready {
        return Ok(Json(json!({"transfer": {"state": "pending"}})));
    }
    Ok(Json(json!({"transfer": {
        "state": "ready",
        "number": address.number,
        "repository": {
            "id": destination.id,
            "owner": destination.config.owner,
            "name": destination.config.name,
        },
    }})))
}
