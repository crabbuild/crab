use super::may_notify;
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        directory::{
            IdentityKey,
            notifications::{
                CheckSelection, Position, Selected, Selection, thread_id, validate_selection,
            },
        },
        repository::collaboration::{Subject, watches},
    },
    server::Server,
};
use axum::{
    Extension, Json,
    extract::{State, rejection::JsonRejection},
};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct Item {
    id: Uuid,
    repository_id: Uuid,
    subject: Subject,
    expected: Position,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    items: Vec<Item>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Unsubscribed,
    Changed,
    Unavailable,
}

#[derive(Serialize)]
struct Outcome {
    id: Uuid,
    status: Status,
}

#[derive(Serialize)]
pub(super) struct Response {
    items: Vec<Outcome>,
}

pub(super) async fn apply(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    input: std::result::Result<Json<Request>, JsonRejection>,
) -> Result<Json<Response>> {
    let Json(input) = input.map_err(Error::Body)?;
    let identity = app::account(&principal)?;
    let items: Vec<_> = input
        .items
        .iter()
        .map(|item| Selected {
            id: item.id,
            repository_id: item.repository_id,
            expected: item.expected,
        })
        .collect();
    validate_selection(&items).map_err(Error::Invalid)?;
    for item in &input.items {
        app::number(item.subject.number)?;
        if thread_id(item.repository_id, item.subject) != item.id {
            return Err(Error::Invalid(
                "Notification address does not match the discussion",
            ));
        }
    }

    let directory = server.cells().ok_or(Error::CellUnavailable)?;
    let selected = app::query_output(
        directory
            .query::<CheckSelection>(
                Address::Directory,
                None,
                Selection {
                    identity: identity.clone(),
                    items,
                },
            )
            .await,
    )?;
    if selected.len() != input.items.len() {
        return Err(Error::CellContract(
            "Notification selection count does not match",
        ));
    }
    let mut groups = BTreeMap::<Uuid, Vec<(usize, Item, bool)>>::new();
    for (index, (item, selected)) in input.items.into_iter().zip(selected).enumerate() {
        groups
            .entry(item.repository_id)
            .or_default()
            .push((index, item, selected.current));
    }
    let batches: Vec<_> = stream::iter(groups)
        .map(|(id, items)| apply_repository(&server, &principal, &identity, id, items))
        .buffer_unordered(8)
        .try_collect()
        .await?;
    app::account(&principal)?;
    let mut results: Vec<_> = batches.into_iter().flatten().collect();
    results.sort_unstable_by_key(|(index, _)| *index);
    Ok(Json(Response {
        items: results.into_iter().map(|(_, outcome)| outcome).collect(),
    }))
}

async fn apply_repository(
    server: &Server,
    principal: &Principal,
    actor: &IdentityKey,
    id: Uuid,
    items: Vec<(usize, Item, bool)>,
) -> Result<Vec<(usize, Outcome)>> {
    let unavailable = || {
        items
            .iter()
            .map(|(index, item, _)| {
                (
                    *index,
                    Outcome {
                        id: item.id,
                        status: Status::Unavailable,
                    },
                )
            })
            .collect()
    };
    let Some(repo) = server.repositories.by_id(id) else {
        return Ok(unavailable());
    };
    if !may_notify(server, principal, &repo, actor).await? {
        return Ok(unavailable());
    }
    let target = app::repository_cell(server, &repo)?;
    let results = app::command_output(
        target
            .client
            .command::<watches::Dismiss>(
                &target.target,
                app::mutation_identity()?,
                watches::DismissSelection {
                    actor: actor.clone(),
                    items: items
                        .iter()
                        .map(|(_, item, selected)| watches::DismissItem {
                            subject: item.subject,
                            expected: item.expected,
                            selected: *selected,
                        })
                        .collect(),
                },
            )
            .await,
    )?;
    if !may_notify(server, principal, &repo, actor).await? {
        return Ok(unavailable());
    }
    if results.len() != items.len() {
        return Err(Error::CellContract(
            "Notification dismissal count does not match",
        ));
    }
    Ok(items
        .into_iter()
        .zip(results)
        .map(|((index, item, _), accepted)| {
            let status = if accepted {
                Status::Unsubscribed
            } else {
                Status::Changed
            };
            (
                index,
                Outcome {
                    id: item.id,
                    status,
                },
            )
        })
        .collect())
}
