use super::{issue_transfers::readable_origins, may_notify};
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address, MutationIdentity,
        directory::{
            IdentityKey,
            notifications::{
                Action, Changed, CheckSelection, Position, Selected, Selection, SetState,
                SetStateInput, State, validate_selection,
            },
        },
    },
    server::Server,
};
use axum::{
    Extension, Json,
    extract::{Path, State as ServerState, rejection::JsonRejection},
};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StateRequest {
    repository_id: Uuid,
    expected: Position,
    action: Action,
}

pub(super) async fn single(
    ServerState(server): ServerState<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    input: std::result::Result<Json<StateRequest>, JsonRejection>,
) -> Result<Json<State>> {
    let Json(input) = input.map_err(Error::Body)?;
    let identity = app::account(&principal)?;
    let mut states = apply(
        &server,
        &principal,
        identity,
        app::mutation_identity()?,
        vec![Selected {
            id,
            repository_id: input.repository_id,
            expected: input.expected,
        }],
        input.action,
    )
    .await?;
    states
        .pop()
        .map(|item| Json(item.state))
        .ok_or(Error::CellContract("Notification state is missing"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BulkRequest {
    request_id: Uuid,
    issued_at_ms: i64,
    items: Vec<Selected>,
    action: Action,
}

#[derive(Serialize)]
pub(super) struct BulkResult {
    items: Vec<Changed>,
}

pub(super) async fn bulk(
    ServerState(server): ServerState<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    input: std::result::Result<Json<BulkRequest>, JsonRejection>,
) -> Result<Json<BulkResult>> {
    let Json(input) = input.map_err(Error::Body)?;
    let identity = app::account(&principal)?;
    let mutation = batch_identity(&identity, input.request_id, input.issued_at_ms)?;
    let items = apply(
        &server,
        &principal,
        identity,
        mutation,
        input.items,
        input.action,
    )
    .await?;
    Ok(Json(BulkResult { items }))
}

fn batch_identity(
    identity: &IdentityKey,
    request_id: Uuid,
    issued_at_ms: i64,
) -> Result<MutationIdentity> {
    if request_id.is_nil() || issued_at_ms < 0 || issued_at_ms.unsigned_abs() > app::MAX_NUMBER {
        return Err(Error::Invalid("Notification submission is invalid"));
    }
    let expires_at_ms = issued_at_ms + 24 * 60 * 60 * 1_000;
    // Scope client-generated submission IDs to the signed-in account. The
    // Directory's existing receipt checks the exact payload on every replay.
    let mut digest = blake3::Hasher::new_derive_key("crab notification bulk submission v1");
    digest.update(request_id.as_bytes());
    digest.update(&serde_json::to_vec(identity)?);
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest.finalize().as_bytes()[..16]);
    Ok(MutationIdentity {
        request_id: Uuid::from_bytes(bytes),
        issued_at_ms,
        expires_at_ms,
    })
}

async fn apply(
    server: &Server,
    principal: &Principal,
    identity: IdentityKey,
    mutation: MutationIdentity,
    items: Vec<Selected>,
    action: Action,
) -> Result<Vec<Changed>> {
    validate_selection(&items).map_err(Error::Invalid)?;
    let repositories = items.iter().map(|item| item.repository_id).collect();
    authorize(server, principal, &identity, &repositories).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let selections = app::query_output(
        cells
            .query::<CheckSelection>(
                Address::Directory,
                None,
                Selection {
                    identity: identity.clone(),
                    items: items.clone(),
                },
            )
            .await,
    )?;
    if selections.len() != items.len() {
        return Err(Error::CellContract(
            "Notification selection count does not match",
        ));
    }
    let origins: BTreeSet<_> = selections
        .into_iter()
        .filter_map(|item| item.transfer_origin)
        .collect();
    authorize_origins(server, principal, &origins).await?;
    let result = app::command_output(
        cells
            .command::<SetState>(
                Address::Directory,
                mutation,
                SetStateInput {
                    identity: identity.clone(),
                    items,
                    action,
                },
            )
            .await,
    )?;
    authorize(server, principal, &identity, &repositories).await?;
    authorize_origins(server, principal, &origins).await?;
    app::account(principal)?;
    result.ok_or(Error::PageChanged)
}

async fn authorize_origins(
    server: &Server,
    principal: &Principal,
    origins: &BTreeSet<Uuid>,
) -> Result<()> {
    if readable_origins(server, principal, origins.clone())
        .await?
        .len()
        != origins.len()
    {
        return Err(Error::NotFound);
    }
    Ok(())
}

async fn authorize(
    server: &Server,
    principal: &Principal,
    identity: &IdentityKey,
    repositories: &BTreeSet<Uuid>,
) -> Result<()> {
    stream::iter(repositories.iter().copied())
        .map(|id| async move {
            let repo = server.repositories.by_id(id).ok_or(Error::NotFound)?;
            if may_notify(server, principal, &repo, identity).await? {
                Ok(())
            } else {
                Err(Error::NotFound)
            }
        })
        .buffer_unordered(8)
        .try_for_each(|()| async { Ok(()) })
        .await
}
