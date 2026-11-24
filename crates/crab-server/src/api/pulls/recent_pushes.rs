use std::{collections::BTreeSet, sync::Arc};

use axum::{
    Extension, Json,
    extract::{Path, State},
};
use serde::Serialize;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::{
        pull_branch,
        recent_pushes::{Branch, List},
    },
    repository::{Repository, settings},
    server::Server,
};

#[derive(Default, Serialize)]
pub(super) struct Suggestions {
    base_ref: String,
    items: Vec<Branch>,
}

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> Result<Json<Suggestions>> {
    let repo = app::repository(&server, &principal, &key)?;
    let actor = app::account(&principal)?;
    let cell = app::repository_cell(&server, &repo)?;
    let intents = app::query_output(cell.client.query::<List>(&cell.target, None, actor).await)?;
    if intents.is_empty() {
        return Ok(Json(Suggestions::default()));
    }

    let upstream = repo.fork_source(&server);
    if repo.forked_from.is_some() && upstream.is_none() {
        return Err(Error::NotFound);
    }
    let destination = upstream.as_deref().unwrap_or(&repo);
    if !available(&server, &principal, &repo, upstream.as_deref()).await?
        || settings::load_lifecycle(&server, destination)
            .await?
            .archived
    {
        return Ok(Json(Suggestions::default()));
    }

    let mut refs: BTreeSet<_> = intents
        .iter()
        .flat_map(|intent| intent.branches.iter().map(|branch| branch.name.clone()))
        .collect();
    let default = destination.default_ref().await?;
    if upstream.is_none() {
        refs.insert(default.clone());
    }
    let snapshot = repo.ref_view_for_refs(&refs).await?;
    let base_ref = if upstream.is_some() {
        default
    } else {
        snapshot.root_snapshot().record().root().head().to_owned()
    };
    let target = app::repository_cell(&server, destination)?;
    let mut items = Vec::new();
    for intent in intents {
        let branches: Vec<_> = intent
            .branches
            .into_iter()
            .filter(|branch| {
                (upstream.is_some()
                    || branch.name != base_ref
                        && snapshot.refs().get(&base_ref) != Some(&branch.oid))
                    && snapshot.refs().get(&branch.name) == Some(&branch.oid)
            })
            .collect();
        if branches.is_empty() {
            continue;
        }
        let Some(receipt) = crab_metadata::capsule_protocol::resolve_capsule_plan_receipt(
            &repo.store,
            &repo.layout,
            &intent.plan,
        )
        .await
        .map_err(metadata)?
        else {
            continue;
        };
        let transaction = receipt.transaction().id().map_err(metadata)?;
        for branch in branches {
            if snapshot.visible_ref_transactions().get(&branch.name) != Some(&transaction) {
                continue;
            }
            // An identical commit may have been restored or pushed again by
            // someone else. Only the transaction that owns the live ref wins.
            let handled = app::query_output(
                target
                    .client
                    .query::<pull_branch::HasPullForPush>(
                        &target.target,
                        None,
                        pull_branch::PushedHead {
                            name: branch.name.clone(),
                            fork: upstream.as_ref().map(|_| repo.id),
                            base: base_ref.clone(),
                            oid: branch.oid.clone(),
                        },
                    )
                    .await,
            )?;
            if !handled {
                items.push(branch);
            }
            if items.len() >= 3 {
                break;
            }
        }
        if items.len() >= 3 {
            break;
        }
    }
    if !available(&server, &principal, &repo, upstream.as_deref()).await? {
        return Ok(Json(Suggestions::default()));
    }
    Ok(Json(Suggestions { base_ref, items }))
}

async fn available(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
    upstream: Option<&Repository>,
) -> Result<bool> {
    let grant = app::membership(server, principal, repo).await?;
    let Some(upstream) = upstream else {
        return Ok(true);
    };
    let Some(reader) = grant.principal.related_read(repo.id, upstream.id) else {
        return Ok(false);
    };
    Ok(grant.principal.can_write(repo.id)
        && app::membership(server, &reader, upstream)
            .await?
            .principal
            .can_write(upstream.id))
}

fn metadata(source: crab_metadata::error::MetadataError) -> Error {
    crate::Error::Settings {
        source: Box::new(source),
    }
    .into()
}
