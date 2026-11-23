use std::{collections::HashMap, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{actor, read_error, target};
use crate::{
    RepositoryAccess, RepositoryMember,
    api::read_admission,
    app::{self, Error, Result},
    auth::Principal,
    catalog::RepositoryApplicationState::CellReady,
    cells::{
        Address,
        directory::LookupIdentities,
        organization::{MemberKey, teams::repository_access::InspectMemberRepositoryRoles},
    },
    server::Server,
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/organizations/{id}/members/{member}/repository-access",
        get(list),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    after: Option<String>,
    limit: Option<u8>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) enum MemberAccessAccount {
    Active,
    Suspended,
    ProviderUnavailable,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct MemberAccessTeam {
    id: Uuid,
    name: String,
    access: RepositoryAccess,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct AccessRepository {
    id: Uuid,
    owner: String,
    name: String,
    access: Option<RepositoryAccess>,
    direct: Option<RepositoryAccess>,
    teams: Vec<MemberAccessTeam>,
    organization_owner: bool,
    sso_group_ceiling: Option<RepositoryAccess>,
    inherited_from: Option<String>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct MemberRepositoryAccess {
    account: MemberAccessAccount,
    managed: bool,
    items: Vec<AccessRepository>,
    next: Option<String>,
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, member)): Path<(Uuid, Uuid)>,
    Query(params): Query<Parameters>,
) -> Result<Response> {
    let limit = usize::from(params.limit.unwrap_or(30));
    if !(1..=50).contains(&limit) {
        return Err(Error::Invalid(
            "Choose a repository access page size from 1 to 50",
        ));
    }
    let address = target(id)?;
    let Ok(admission) = read_admission::acquire_bulk(&server) else {
        return Ok(read_admission::busy());
    };
    let caller = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let selection = || async {
        cells
            .query::<InspectMemberRepositoryRoles>(
                address,
                None,
                MemberKey {
                    actor: caller,
                    member,
                },
            )
            .await
            .map_err(read_error)?
            .output
            .ok_or(Error::NotFound)
    };
    let roles = selection().await?;
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (document, _) = catalog.load().await.map_err(app::repository_access_error)?;
    let identities = app::query_output(
        cells
            .query::<LookupIdentities>(Address::Directory, None, vec![member])
            .await,
    )?;
    let [Some(identity)] = identities.as_slice() else {
        return Err(Error::CellContract(
            "Organization member has no Directory identity",
        ));
    };
    let auth = server.auth.as_ref().ok_or(Error::NotFound)?;
    let known_provider = auth.issuer() == identity.issuer;
    let account_state = if known_provider {
        auth.account_state(&identity.subject)
            .await
            .map_err(|error| Error::IdentityAuthority(Box::new(error)))?
    } else {
        Default::default()
    };
    let account = if !known_provider {
        MemberAccessAccount::ProviderUnavailable
    } else if account_state.suspended {
        MemberAccessAccount::Suspended
    } else {
        MemberAccessAccount::Active
    };

    // A continuation applies to one member, owner, catalog, and team snapshot.
    // This prevents a later page from mixing a revoked role with newer grants.
    let mut scope = blake3::Hasher::new_keyed(&server.cursor_key);
    scope.update(b"crab-server.organization-member-repository-access.v1");
    for actor in [id, caller, member] {
        scope.update(actor.as_bytes());
    }
    scope.update(&document.version.to_le_bytes());
    scope.update(&account_state.revision.to_le_bytes());
    scope.update(&serde_json::to_vec(&roles)?);
    let signature = |repository: Uuid| {
        let mut digest = scope.clone();
        digest.update(repository.as_bytes());
        digest.finalize()
    };
    let after = params
        .after
        .as_deref()
        .map(|value| {
            let (id, tag) = value
                .split_once('.')
                .filter(|_| value.len() == 101)
                .ok_or(Error::Invalid("Invalid repository access cursor"))?;
            let id = Uuid::parse_str(id)
                .map_err(|_| Error::Invalid("Invalid repository access cursor"))?;
            let tag = blake3::Hash::from_hex(tag)
                .map_err(|_| Error::Invalid("Invalid repository access cursor"))?;
            if signature(id) != tag {
                return Err(Error::PageChanged);
            }
            Ok(id)
        })
        .transpose()?;

    let repositories: Vec<_> = document
        .repositories
        .iter()
        .filter(|record| record.application == CellReady && record.organization_id() == Some(id))
        .collect();
    let start = after.map_or(Ok(0), |after| {
        repositories
            .iter()
            .position(|record| record.id == after)
            .map(|index| index + 1)
            .ok_or(Error::PageChanged)
    })?;
    let source_records: HashMap<_, _> = repositories.iter().map(|r| (r.id, *r)).collect();
    let mut current_page = repositories.into_iter().skip(start);
    let mut items = Vec::with_capacity(limit);
    for repo in current_page.by_ref().take(limit) {
        let source = repo
            .access_record(
                repo.forked_from
                    .and_then(|id| source_records.get(&id).copied()),
            )
            .map_err(app::repository_access_error)?;
        let direct = RepositoryMember::access_for(&source.members, &identity.subject)
            .filter(|_| known_provider);
        let organization_owner = known_provider && roles.owner;
        let teams: Vec<_> = source
            .organization
            .iter()
            .flat_map(|org| &org.teams)
            .filter_map(|grant| {
                let name = roles.teams.get(&grant.team).filter(|_| known_provider)?;
                Some(MemberAccessTeam {
                    id: grant.team,
                    name: name.clone(),
                    access: grant.access,
                })
            })
            .collect();
        let access = direct
            .into_iter()
            .chain(teams.iter().map(|team| team.access))
            .chain(organization_owner.then_some(RepositoryAccess::Admin))
            .max();
        let sso_group_ceiling = source
            .groups
            .iter()
            .map(|grant| grant.access)
            .max()
            .filter(|_| known_provider && auth.is_oidc());
        items.push(AccessRepository {
            id: repo.id,
            owner: repo.owner.clone(),
            name: repo.name.clone(),
            access,
            direct,
            teams,
            organization_owner,
            sso_group_ceiling,
            inherited_from: repo.forked_from.map(|_| source.name.clone()),
        });
    }
    let next = items
        .last()
        .filter(|_| current_page.next().is_some())
        .map(|last| format!("{}.{}", last.id, signature(last.id).to_hex()));

    // The Organization Cell is the owner boundary for this view. Its read
    // rechecks the caller and member after crossing Catalog and Directory.
    if selection().await? != roles {
        return Err(Error::PageChanged);
    }
    app::account(&principal)?;
    Ok(read_admission::retain(
        Json(MemberRepositoryAccess {
            account,
            managed: account_state.managed,
            items,
            next,
        })
        .into_response(),
        admission,
    ))
}
