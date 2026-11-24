use std::{collections::HashMap, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{OrganizationPage, actor, read_error, target};
use crate::{
    RepositoryAccess,
    api::read_admission,
    app::{self, Error, Result},
    auth::Principal,
    catalog::RepositoryApplicationState::CellReady,
    cells::{
        Address, CellClient,
        directory::{ActorSummary, IdentityKey, LookupAccounts, LookupProfiles},
        organization::{Caller, GetOrganization, InspectMemberships, MembershipInspection, Role},
    },
    server::Server,
};

const SCAN_LIMIT: usize = 100;
mod removal;
#[cfg(test)]
pub(in crate::api) use removal::{RemovalPlan, RemovalPreview};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/api/organizations/{id}/outside-collaborators", get(list))
        .route(
            "/api/organizations/{id}/outside-collaborators/removal-preview",
            post(removal::preview),
        )
        .route(
            "/api/organizations/{id}/outside-collaborators/removals",
            post(removal::remove),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    after: Option<String>,
    limit: Option<u8>,
    #[serde(default)]
    q: String,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct OutsideCollaboratorGrant {
    collaborator: String,
    name: String,
    profile: Option<ActorSummary>,
    owner: String,
    repository: String,
    repository_id: Uuid,
    access: RepositoryAccess,
}

async fn owner_revision(cells: &CellClient, address: Address, caller: Uuid) -> Result<u64> {
    let view = cells
        .query::<GetOrganization>(address, None, Caller { actor: caller })
        .await
        .map_err(read_error)?
        .output
        .filter(|view| view.role == Role::Owner)
        .ok_or(Error::NotFound)?;
    Ok(view.organization.version)
}

fn collaborator_key(
    server: &Server,
    organization: Uuid,
    issuer: &str,
    subject: &str,
) -> blake3::Hash {
    let mut key = blake3::Hasher::new_keyed(&server.cursor_key);
    key.update(b"crab-server.outside-collaborator.v1");
    key.update(organization.as_bytes());
    key.update(&(issuer.len() as u64).to_le_bytes());
    key.update(issuer.as_bytes());
    key.update(subject.as_bytes());
    key.finalize()
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(params): Query<Parameters>,
) -> Result<Response> {
    let limit = usize::from(params.limit.unwrap_or(30));
    let search = params.q.trim();
    if !(1..=50).contains(&limit)
        || search.chars().count() > 100
        || search.chars().any(char::is_control)
    {
        return Err(Error::Invalid(
            "Choose a search of up to 100 characters and a page size from 1 to 50",
        ));
    }
    let address = target(id)?;
    let Ok(admission) = read_admission::acquire_bulk(&server) else {
        return Ok(read_admission::busy());
    };
    let caller = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let revision = owner_revision(&cells, address, caller).await?;
    let issuer = server.auth.as_ref().ok_or(Error::NotFound)?.issuer();
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (snapshot, etag) = catalog
        .load_snapshot()
        .await
        .map_err(app::repository_access_error)?;
    let search = search.to_lowercase();
    let mut scope = blake3::Hasher::new_keyed(&server.cursor_key);
    scope.update(b"crab-server.organization-outside-collaborators.v1");
    scope.update(id.as_bytes());
    scope.update(caller.as_bytes());
    scope.update(&revision.to_le_bytes());
    scope.update(&snapshot.version.to_le_bytes());
    scope.update(search.as_bytes());
    let sign = |offset: usize| {
        let mut hash = scope.clone();
        hash.update(&(offset as u64).to_le_bytes());
        hash.finalize()
    };
    let start = params
        .after
        .as_deref()
        .map(|cursor| {
            let (offset, digest) = cursor
                .split_once('.')
                .filter(|_| cursor.len() <= 85)
                .ok_or(Error::Invalid("Invalid outside collaborator cursor"))?;
            let offset = offset
                .parse::<usize>()
                .map_err(|_| Error::Invalid("Invalid outside collaborator cursor"))?;
            let digest = blake3::Hash::from_hex(digest)
                .map_err(|_| Error::Invalid("Invalid outside collaborator cursor"))?;
            if sign(offset) != digest {
                return Err(Error::PageChanged);
            }
            Ok(offset)
        })
        .transpose()?
        .unwrap_or(0);

    // Forks inherit their source grants and have no independent access editor.
    // The catalog names and memberships, rather than mutable profile names,
    // determine page positions and search matches.
    let search = search.as_str();
    let mut batch: Vec<_> = snapshot
        .repositories
        .iter()
        .filter(|repo| repo.application == CellReady && repo.owns_direct_grants(id))
        .flat_map(|repo| {
            let repo_matches = search.is_empty() || repo.name.to_lowercase().contains(search);
            repo.members
                .iter()
                .filter(move |grant| repo_matches || grant.name.to_lowercase().contains(search))
                .map(move |grant| (repo, grant))
        })
        .skip(start)
        .take(SCAN_LIMIT + 1)
        .collect();
    let has_more = batch.len() > SCAN_LIMIT;
    batch.truncate(SCAN_LIMIT);
    let identities = batch
        .iter()
        .map(|(_, grant)| IdentityKey {
            issuer: issuer.to_owned(),
            subject: grant.subject.clone(),
        })
        .collect();
    let ids = app::query_output(
        cells
            .query::<LookupAccounts>(Address::Directory, None, identities)
            .await,
    )?;
    if ids.len() != batch.len() {
        return Err(Error::CellContract(
            "Outside collaborator lookup returned an invalid result count",
        ));
    }
    let mut accounts: Vec<_> = ids.iter().flatten().copied().collect();
    accounts.sort_unstable();
    accounts.dedup();
    let profiles: HashMap<_, _> = app::query_output(
        cells
            .query::<LookupProfiles>(Address::Directory, None, accounts.clone())
            .await,
    )?
    .into_iter()
    .flatten()
    .map(|profile| (profile.id, profile))
    .collect();

    let (current, current_etag) = catalog
        .load_snapshot()
        .await
        .map_err(app::repository_access_error)?;
    if current.version != snapshot.version || current_etag != etag {
        return Err(Error::PageChanged);
    }
    // Recheck owner authority and membership in one snapshot after enrichment.
    let mut active = cells
        .query::<InspectMemberships>(
            address,
            None,
            MembershipInspection {
                actor: caller,
                revision,
                candidates: accounts,
            },
        )
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::PageChanged)?;
    active.sort_unstable();
    app::account(&principal)?;

    let scanned = batch.len();
    let mut processed = 0;
    let mut items = Vec::with_capacity(limit);
    for ((repo, grant), account) in batch.into_iter().zip(ids) {
        processed += 1;
        if account.is_some_and(|id| active.binary_search(&id).is_ok()) {
            continue;
        }
        items.push(OutsideCollaboratorGrant {
            collaborator: collaborator_key(&server, id, issuer, &grant.subject)
                .to_hex()
                .to_string(),
            name: grant.name.clone(),
            profile: account.and_then(|id| profiles.get(&id).cloned()),
            owner: repo.owner.clone(),
            repository: repo.name.clone(),
            repository_id: repo.id,
            access: grant.access,
        });
        if items.len() == limit {
            break;
        }
    }
    let offset = start + processed;
    let next =
        (processed < scanned || has_more).then(|| format!("{offset}.{}", sign(offset).to_hex()));
    Ok(read_admission::retain(
        Json(OrganizationPage {
            items,
            next,
            revision,
        })
        .into_response(),
        admission,
    ))
}
