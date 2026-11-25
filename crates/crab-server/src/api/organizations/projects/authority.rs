use std::collections::BTreeSet;

use super::*;
use crate::{
    catalog::RepositoryApplicationState::CellReady,
    cells::directory::{CompleteHandles, LookupIdentities, validate_handle_prefix},
};

pub(super) async fn caller(
    server: &Server,
    principal: &Principal,
    organization: Uuid,
) -> Result<projects::AccessActor> {
    let account = actor(server, principal).await?;
    viewer(server, principal, organization, account).await
}

pub(super) async fn viewer(
    server: &Server,
    principal: &Principal,
    organization: Uuid,
    account: Uuid,
) -> Result<projects::AccessActor> {
    if crate::repository::access::organization_role(server, organization, account)
        .await?
        .is_some()
    {
        return Ok(account.into());
    }
    let identity = app::account(principal)?;
    let Some(auth) = &server.auth else {
        return Ok(account.into());
    };
    if auth.issuer() != identity.issuer {
        return Ok(account.into());
    }
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (snapshot, _) = catalog
        .load_snapshot()
        .await
        .map_err(app::repository_access_error)?;
    let qualified = snapshot.repositories.iter().any(|repo| {
        repo.application == CellReady
            && repo.owns_direct_grants(organization)
            && crate::RepositoryMember::access_for(&repo.members, &identity.subject).is_some()
    });
    Ok(if qualified {
        projects::AccessActor::Guest { id: account }
    } else {
        account.into()
    })
}

pub(super) async fn eligible(
    server: &Server,
    organization: Uuid,
    ids: Vec<Uuid>,
) -> Result<BTreeSet<Uuid>> {
    if ids.is_empty() {
        return Ok(BTreeSet::new());
    }
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let identities = app::query_output(
        cells
            .query::<LookupIdentities>(Address::Directory, None, ids.clone())
            .await,
    )?;
    if identities.len() != ids.len() {
        return Err(Error::CellContract(
            "Project collaborator identity count is invalid",
        ));
    }
    let Some(auth) = &server.auth else {
        return Ok(BTreeSet::new());
    };
    let issuer = auth.issuer();
    let mut subjects: BTreeMap<_, _> = ids
        .into_iter()
        .zip(identities)
        .filter_map(|(id, identity)| {
            identity
                .filter(|identity| identity.issuer == issuer)
                .map(|identity| (identity.subject, id))
        })
        .collect();
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (snapshot, _) = catalog
        .load_snapshot()
        .await
        .map_err(app::repository_access_error)?;
    let mut eligible = BTreeSet::new();
    for repo in snapshot
        .repositories
        .iter()
        .filter(|repo| repo.application == CellReady && repo.owns_direct_grants(organization))
    {
        for grant in &repo.members {
            if let Some(id) = subjects.remove(&grant.subject) {
                eligible.insert(id);
            }
        }
        if subjects.is_empty() {
            break;
        }
    }
    Ok(eligible)
}

pub(super) async fn workspace(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
) -> Result<Json<projects::WorkspaceAccess>> {
    let actor = caller(&server, &principal, organization).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let workspace = cells
        .query::<projects::GetWorkspace>(
            target(organization)?,
            None,
            projects::ProjectCaller { actor },
        )
        .await
        .map_err(read_error)?
        .output;
    app::account(&principal)?;
    Ok(Json(workspace))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CollaboratorQuery {
    q: String,
    #[serde(default)]
    member: bool,
}

pub(super) async fn candidates(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((organization, selected)): Path<(Uuid, Uuid)>,
    Query(input): Query<CollaboratorQuery>,
) -> Result<Json<Value>> {
    let caller = caller(&server, &principal, organization).await?;
    let view = project(&server, &principal, organization, caller, selected).await?;
    if !view.can_admin {
        return Err(Error::NotFound);
    }
    let prefix = input.q.trim().trim_start_matches('@').to_ascii_lowercase();
    validate_handle_prefix(&prefix).map_err(Error::Invalid)?;
    if prefix.is_empty() {
        return Ok(Json(json!({"items":[],"truncated":false})));
    }
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let matches = app::query_output(
        cells
            .query::<CompleteHandles>(Address::Directory, None, prefix)
            .await,
    )?;
    let ids = matches
        .items
        .iter()
        .map(|profile| profile.id)
        .collect::<Vec<_>>();
    let eligible = if input.member {
        BTreeSet::new()
    } else {
        eligible(&server, organization, ids.clone()).await?
    };
    // The final native read rechecks Project administration and current
    // membership without exposing the organization's roster or repository IDs.
    let caller = viewer(&server, &principal, organization, caller.id()).await?;
    let members = cells
        .query::<projects::InspectCollaborators>(
            target(organization)?,
            None,
            projects::CollaboratorInspection {
                actor: caller,
                project: selected,
                candidates: ids,
            },
        )
        .await
        .map_err(read_error)?
        .output;
    let items: Vec<_> = matches
        .items
        .into_iter()
        .filter(|profile| {
            if input.member {
                members.contains(&profile.id)
            } else {
                !members.contains(&profile.id) && eligible.contains(&profile.id)
            }
        })
        .collect();
    Ok(Json(json!({"items":items,"truncated":matches.truncated})))
}
