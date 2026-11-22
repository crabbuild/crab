use std::{collections::HashSet, sync::Arc};

use axum::{
    Extension, Json,
    extract::{State, rejection::JsonRejection},
};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{super::may_read, subscriptions, valid_owner};
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{directory::IdentityKey, repository::collaboration::watches::Preference},
    server::Server,
};

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    id: Uuid,
    version: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api::notifications) struct Request {
    owner: String,
    items: Vec<Selection>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Unwatched,
    Changed,
    Unavailable,
    Unconfirmed,
}

#[derive(Serialize)]
struct Outcome {
    id: Uuid,
    status: Status,
}

#[derive(Serialize)]
pub(in crate::api::notifications) struct Response {
    items: Vec<Outcome>,
}

pub(in crate::api::notifications) async fn apply(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    input: std::result::Result<Json<Request>, JsonRejection>,
) -> Result<Json<Response>> {
    let Json(Request { owner, items }) = input.map_err(Error::Body)?;
    let actor = app::account(&principal)?;
    valid_owner(&owner)?;
    if !(1..=50).contains(&items.len()) {
        return Err(Error::Invalid("Select 1–50 repository watches per request"));
    }
    let mut ids = HashSet::with_capacity(items.len());
    if items.iter().any(|item| {
        item.id.is_nil() || !(1..=app::MAX_NUMBER).contains(&item.version) || !ids.insert(item.id)
    }) {
        return Err(Error::Invalid(
            "Watch selection contains invalid or repeated entries",
        ));
    }

    let items = stream::iter(items)
        .map(|item| {
            let (server, principal, owner, actor) = (&server, &principal, &owner, actor.clone());
            async move {
                let status = match unwatch(server, principal, owner, actor, item).await {
                    Ok(status) => status,
                    Err(error) => {
                        tracing::warn!(repository_id = %item.id, %error, "bulk unwatch outcome unconfirmed");
                        Status::Unconfirmed
                    }
                };
                Outcome { id: item.id, status }
            }
        })
        .buffered(8)
        .collect()
        .await;
    Ok(Json(Response { items }))
}

async fn unwatch(
    server: &Server,
    principal: &Principal,
    owner: &str,
    actor: IdentityKey,
    item: Selection,
) -> Result<Status> {
    let Some(repo) = server.repositories.by_id(item.id) else {
        return Ok(Status::Unavailable);
    };
    if repo.config.owner != owner || !may_read(server, principal, &repo).await? {
        return Ok(Status::Unavailable);
    }
    // The canonical version check also makes a retry safe after a lost reply.
    // A newer watch or mute setting cannot be overwritten by this selection.
    let desired = Preference {
        version: item.version,
        ..Preference::default()
    };
    let saved = subscriptions::write(server, &repo, actor, desired).await?;
    if !may_read(server, principal, &repo).await? {
        return Ok(Status::Unavailable);
    }
    Ok(if saved.is_some() {
        Status::Unwatched
    } else {
        Status::Changed
    })
}
