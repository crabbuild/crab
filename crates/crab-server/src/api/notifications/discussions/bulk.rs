use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use axum::{
    Extension, Json,
    extract::{State, rejection::JsonRejection},
};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::may_read;
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        directory::IdentityKey,
        repository::collaboration::{
            Subject,
            watches::{SelectedThread, ThreadOutcome, ThreadSelection, UnsubscribeDiscussions},
        },
    },
    server::Server,
};

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct Item {
    repository_id: Uuid,
    subject: Subject,
    version: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api::notifications) struct Request {
    items: Vec<Item>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Unsubscribed,
    Changed,
    Unavailable,
    Unconfirmed,
}

#[derive(Serialize)]
struct Outcome {
    repository_id: Uuid,
    subject: Subject,
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
    let Json(Request { items }) = input.map_err(Error::Body)?;
    let actor = app::account(&principal)?;
    if !(1..=50).contains(&items.len()) {
        return Err(Error::Invalid("Select 1–50 discussions per request"));
    }
    let mut subjects = HashSet::with_capacity(items.len());
    let mut grouped = BTreeMap::<Uuid, Vec<(usize, Item)>>::new();
    for (index, item) in items.into_iter().enumerate() {
        app::number(item.subject.number)?;
        if item.repository_id.is_nil()
            || !(1..=app::MAX_NUMBER).contains(&item.version)
            || !subjects.insert((item.repository_id, item.subject.key()))
        {
            return Err(Error::Invalid(
                "Subscription selection contains invalid or repeated entries",
            ));
        }
        grouped
            .entry(item.repository_id)
            .or_default()
            .push((index, item));
    }

    let batches: Vec<_> = stream::iter(grouped)
        .map(|(id, items)| {
            let (server, principal, actor) = (&server, &principal, actor.clone());
            async move {
                let statuses = match unsubscribe(server, principal, actor, id, &items).await {
                    Ok(statuses) => statuses,
                    Err(error) => {
                        tracing::warn!(repository_id = %id, %error, "discussion unsubscription outcome unconfirmed");
                        vec![Status::Unconfirmed; items.len()]
                    }
                };
                items.into_iter().zip(statuses).map(|((index, item), status)| (index, Outcome {
                    repository_id: id,
                    subject: item.subject,
                    status,
                })).collect::<Vec<_>>()
            }
        })
        .buffer_unordered(8)
        .collect()
        .await;
    let mut ordered: Vec<_> = batches.into_iter().flatten().collect();
    ordered.sort_unstable_by_key(|(index, _)| *index);
    Ok(Json(Response {
        items: ordered.into_iter().map(|(_, outcome)| outcome).collect(),
    }))
}

async fn unsubscribe(
    server: &Server,
    principal: &Principal,
    actor: IdentityKey,
    id: Uuid,
    items: &[(usize, Item)],
) -> Result<Vec<Status>> {
    let unavailable = || vec![Status::Unavailable; items.len()];
    let Some(repo) = server.repositories.by_id(id) else {
        return Ok(unavailable());
    };
    if !may_read(server, principal, &repo).await? {
        return Ok(unavailable());
    }
    let cell = app::repository_cell(server, &repo)?;
    let statuses = app::command_output(
        cell.client
            .command::<UnsubscribeDiscussions>(
                &cell.target,
                app::mutation_identity()?,
                ThreadSelection {
                    actor,
                    items: items
                        .iter()
                        .map(|(_, item)| SelectedThread {
                            subject: item.subject,
                            version: item.version,
                        })
                        .collect(),
                },
            )
            .await,
    )?;
    if !may_read(server, principal, &repo).await? {
        return Ok(unavailable());
    }
    if statuses.len() != items.len() {
        return Err(Error::CellContract(
            "Discussion unsubscription count does not match",
        ));
    }
    Ok(statuses
        .into_iter()
        .map(|outcome| match outcome {
            ThreadOutcome::Saved(_) => Status::Unsubscribed,
            ThreadOutcome::Missing => Status::Unavailable,
            ThreadOutcome::Stale => Status::Changed,
        })
        .collect())
}
