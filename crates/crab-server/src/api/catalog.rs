use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::{
    RepositoryAccess, RepositoryMember, app,
    auth::Principal,
    catalog::{
        BrowserNamespace, CatalogError, MembershipActor, RepositoryApplicationState,
        organization_namespace,
    },
    cells::{
        Address,
        organization::{
            Role,
            repositories::{AuthorizeCreation, Authorized, Creation, valid_creation},
        },
    },
    server::Server,
};

#[derive(Default, serde::Deserialize)]
pub(super) struct RepositorySelection {
    owner: Option<String>,
    name: Option<String>,
    organization: Option<Uuid>,
    q: Option<String>,
    after: Option<String>,
    limit: Option<u8>,
}

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(selection): Query<RepositorySelection>,
) -> app::Result<Json<serde_json::Value>> {
    if selection.owner.is_some() != selection.name.is_some() {
        return Err(app::Error::Invalid(
            "owner and name must be provided together",
        ));
    }
    let actor = app::actor(&principal)?;
    if selection.q.is_some() || selection.after.is_some() || selection.limit.is_some() {
        if selection.owner.is_some() {
            return Err(app::Error::Invalid(
                "Search and exact repository lookup cannot be combined",
            ));
        }
        return super::catalog_browse::list(
            &server,
            &principal,
            &actor,
            super::catalog_browse::Search {
                organization: selection.organization,
                query: selection.q.unwrap_or_default(),
                after: selection.after,
                limit: selection.limit,
            },
        )
        .await;
    }
    let address = selection.owner.as_deref().zip(selection.name.as_deref());
    let mut matches = visible(&server, &principal, address, selection.organization).await?;
    if matches.is_empty()
        && selection.organization.is_none()
        && let Some((owner, name)) = address
        && let Some(destination) =
            crate::server::repository_route::renamed(&server, &principal, owner, name, None).await?
    {
        if (destination.owner.as_str(), destination.name.as_str()) != (owner, name) {
            return Ok(Json(json!({"repositories": [], "redirect": destination})));
        }
        matches = visible(&server, &principal, address, selection.organization).await?;
    }
    let mut repositories = Vec::new();
    for VisibleRepository {
        repository,
        principal: current,
        organization_owner,
    } in matches
    {
        let protections = crate::repository::settings::load(&server, &repository).await?;
        let lifecycle = crate::repository::settings::load_lifecycle(&server, &repository).await?;
        let mut view = json!({
            "id": repository.id,
            "owner": repository.config.owner, "name": repository.config.name,
            "description": repository.config.description,
            "access": if current.can_write(repository.id) { "write" } else { "read" },
            "can_admin": current.can_admin(repository.id),
            "can_fork": organization_owner && repository.forked_from.is_none(),
            "protection_version": protections.version,
            "protected_branches": protections.rules,
            "archive_version": lifecycle.version,
            "archived": lifecycle.archived,
        });
        if let Some(organization) = repository.organization {
            view["organization_id"] = json!(organization);
        }
        if let Some(source) = repository
            .forked_from
            .and_then(|id| server.repositories.by_id(id))
        {
            view["forked_from"] = json!({
                "id": source.id, "owner": source.config.owner, "name": source.config.name,
            });
        }
        repositories.push(view);
    }
    if !principal.is_local()
        && repositories
            .iter()
            .any(|repo| repo.get("organization_id").is_some())
    {
        // Settings reads may yield after a change to the organization role.
        let current: std::collections::HashSet<_> =
            visible(&server, &principal, address, selection.organization)
                .await?
                .into_iter()
                .map(|visible| visible.repository.id.to_string())
                .collect();
        repositories.retain(|repo| {
            repo.get("organization_id").is_none()
                || repo["id"].as_str().is_some_and(|id| current.contains(id))
        });
    }
    Ok(Json(json!({"repositories":repositories})))
}

pub(super) struct VisibleRepository {
    pub(super) repository: Arc<crate::repository::Repository>,
    principal: Principal,
    organization_owner: bool,
}

pub(super) async fn visible(
    server: &Server,
    principal: &Principal,
    selection: Option<(&str, &str)>,
    organization: Option<Uuid>,
) -> app::Result<Vec<VisibleRepository>> {
    visible_matching(server, principal, |repository| {
        selection.is_none_or(|(owner, name)| {
            owner == repository.config.owner && name == repository.config.name
        }) && organization.is_none_or(|id| repository.organization == Some(id))
    })
    .await
}

pub(super) async fn visible_matching(
    server: &Server,
    principal: &Principal,
    mut include: impl FnMut(&crate::repository::Repository) -> bool,
) -> app::Result<Vec<VisibleRepository>> {
    let selected: Vec<_> = server
        .repositories
        .values()
        .into_iter()
        .filter(|repository| include(repository))
        .collect();
    if selected.is_empty() {
        return Ok(Vec::new());
    }
    let grants = if principal.is_local() {
        None
    } else {
        let catalog = server.catalog().ok_or(app::Error::NotFound)?;
        Some(
            catalog
                .load()
                .await
                .map_err(|error| app::Error::Membership(Box::new(error)))?
                .0
                .repositories
                .into_iter()
                .map(|record| (record.id, record))
                .collect::<std::collections::HashMap<_, _>>(),
        )
    };
    let issuer = server
        .auth
        .as_ref()
        .map_or("urn:crab:local", |auth| auth.issuer());
    let mut organizations = std::collections::HashMap::new();
    if let Some(grants) = &grants {
        for repo in &selected {
            if let Some(record) = grants.get(&repo.id)
                && let Some(organization) = record.organization_id()
            {
                let source = record
                    .access_record(record.forked_from.and_then(|id| grants.get(&id)))
                    .map_err(|error| app::Error::Membership(Box::new(error)))?;
                let include = organizations.entry(organization).or_insert(false);
                *include |= source
                    .organization
                    .as_ref()
                    .is_some_and(|access| !access.teams.is_empty());
            }
        }
    }
    let organization_grants =
        crate::repository::access::organization_grants(server, principal, organizations).await?;
    let mut visible = Vec::new();
    for repository in selected {
        let current = if let Some(grants) = &grants {
            let Some(record) = grants.get(&repository.id).filter(|record| {
                record.owner == repository.config.owner
                    && record.name == repository.config.name
                    && record.organization_id() == repository.organization
                    && record.ownership_epoch == repository.ownership_epoch
                    && record.forked_from == repository.forked_from
            }) else {
                continue;
            };
            let source = record
                .access_record(record.forked_from.and_then(|id| grants.get(&id)))
                .map_err(|error| app::Error::Membership(Box::new(error)))?;
            let mut config = repository.config.clone();
            config.members = source.members.clone();
            config.groups = source.groups.clone();
            principal.with_catalog_membership(
                repository.id,
                &config,
                issuer,
                record
                    .organization_id()
                    .and_then(|id| organization_grants.get(&id))
                    .and_then(|grant| {
                        source.organization.as_ref().and_then(|access| {
                            crate::repository::access::organization_access(grant, &access.teams)
                        })
                    }),
                record.name_version() > 0,
            )
        } else {
            principal.clone()
        };
        if !current.can_read(repository.id) {
            continue;
        }
        let organization_owner = repository
            .organization
            .and_then(|id| organization_grants.get(&id))
            .is_some_and(|grant| grant.owner);
        visible.push(VisibleRepository {
            repository,
            principal: current,
            organization_owner,
        });
    }
    Ok(visible)
}

pub(super) fn creation_routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/creation", get(creation).post(create))
        .route(
            "/api/organizations/{organization}/repositories/creation",
            get(organization_creation).post(create_organization),
        )
        .layer(axum::extract::DefaultBodyLimit::max(4096))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

fn namespace(principal: &Principal) -> app::Result<String> {
    let identity = app::account(principal)?;
    if principal.is_local() {
        return Ok("local".into());
    }
    // Profiles can be renamed or reassigned. Only the verified qualified
    // account determines the namespace for new personal repositories.
    Ok(crate::catalog::personal_namespace(
        &identity.issuer,
        &identity.subject,
    ))
}

async fn creation(
    Extension(principal): Extension<Principal>,
) -> app::Result<Json<serde_json::Value>> {
    Ok(Json(
        json!({"owner":namespace(&principal)?,"default_branch":"main"}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRepository {
    request_id: Uuid,
    name: String,
    description: String,
}

#[derive(Serialize)]
struct CreatedRepository {
    id: Uuid,
    owner: String,
    name: String,
}

async fn organization_creation(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
) -> app::Result<Json<serde_json::Value>> {
    let actor = super::organizations::actor(&server, &principal).await?;
    let role = crate::repository::access::organization_role(&server, organization, actor).await?;
    if role != Some(Role::Owner) {
        return Err(app::Error::NotFound);
    }
    app::account(&principal)?;
    Ok(Json(
        json!({"owner":organization_namespace(organization),"default_branch":"main"}),
    ))
}

async fn create_organization(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
    Json(input): Json<CreateRepository>,
) -> app::Result<(StatusCode, Json<CreatedRepository>)> {
    let actor = super::organizations::actor(&server, &principal).await?;
    input.validate()?;
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    app::account(&principal)?;
    let authorized = cells
        .command::<AuthorizeCreation>(
            Address::Organization(organization),
            app::mutation_identity()?,
            Creation {
                actor,
                request_id: input.request_id,
                name: input.name.clone(),
                description: input.description.clone(),
            },
        )
        .await
        .map_err(super::organizations::read_error)?
        .output;
    match authorized {
        Authorized::Accepted => {
            let response = publish(
                &server,
                &principal,
                BrowserNamespace::Organization(organization),
                input,
            )
            .await?;
            if crate::repository::access::organization_role(&server, organization, actor).await?
                != Some(Role::Owner)
            {
                return Err(app::Error::NotFound);
            }
            app::account(&principal)?;
            Ok(response)
        }
        Authorized::Conflict => Err(app::Error::RepositoryNameUnavailable),
    }
}

impl CreateRepository {
    fn validate(&self) -> app::Result<()> {
        if self.request_id.is_nil() {
            return Err(app::Error::Invalid("Repository submission ID is invalid"));
        }
        if !crate::config::valid_repository_component(&self.name) {
            return Err(app::Error::Invalid(
                "Use 1–100 letters, numbers, periods, hyphens or underscores for the repository name",
            ));
        }
        if !valid_creation(&self.name, &self.description) {
            return Err(app::Error::Invalid(
                "The repository description must be one line of at most 350 characters",
            ));
        }
        Ok(())
    }
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Json(input): Json<CreateRepository>,
) -> app::Result<(StatusCode, Json<CreatedRepository>)> {
    let owner = namespace(&principal)?;
    input.validate()?;
    publish(
        &server,
        &principal,
        BrowserNamespace::Personal(owner),
        input,
    )
    .await
}

async fn publish(
    server: &Arc<Server>,
    principal: &Principal,
    namespace: BrowserNamespace,
    input: CreateRepository,
) -> app::Result<(StatusCode, Json<CreatedRepository>)> {
    let actor = app::actor(principal)?;
    let catalog = server.catalog().ok_or(app::Error::CellUnavailable)?;
    let members = if principal.is_local() || matches!(&namespace, BrowserNamespace::Organization(_))
    {
        Vec::new()
    } else {
        vec![RepositoryMember {
            subject: actor.subject.clone(),
            name: actor.name.trim().into(),
            access: RepositoryAccess::Admin,
        }]
    };
    let record = catalog
        .reserve_browser_repository(
            namespace,
            input.name,
            input.description,
            input.request_id,
            MembershipActor {
                issuer: actor.issuer,
                subject: actor.subject,
            },
            members,
        )
        .await
        .map_err(|error| match error {
            CatalogError::Conflict => app::Error::RepositoryNameUnavailable,
            source => app::Error::RepositorySetup(Box::new(source.into())),
        })?;
    let worker = Arc::clone(server);
    let result = server
        .receives
        .spawn(async move {
            if record.application == RepositoryApplicationState::EmptyCellPending {
                catalog.initialize_git(&record, "main", true).await?;
                worker
                    .initialize_catalog_repository(
                        record.id,
                        RepositoryApplicationState::EmptyCellPending,
                    )
                    .await?;
            }
            let (document, _) = catalog.load().await?;
            worker.install_catalog(document).await?;
            Ok::<_, crate::Error>(record)
        })
        .await
        .map_err(|error| app::Error::Cell(crate::cells::Error::sdk(error)))?
        .map_err(|error| app::Error::RepositorySetup(Box::new(error)))?;
    let key = (result.owner.clone(), result.name.clone());
    let repository = server
        .repositories
        .get(&key)
        .filter(|repo| repo.id == result.id && repo.organization == result.organization_id())
        .ok_or(app::Error::NotFound)?;
    // A replay must still pass current grants after routing has been refreshed.
    app::membership(server, principal, &repository).await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedRepository {
            id: result.id,
            owner: result.owner,
            name: result.name,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{git, http::Harness};
    use reqwest::Method;
    use serde_json::Value;

    #[tokio::test]
    async fn browser_creation_recovers_a_reserved_name_and_accepts_the_first_git_push() {
        crate::test_support::server::init_test_tracing();
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let h =
            Harness::new_with_store_and_prefix(false, false, store, "repository-onboarding").await;
        h.initialize_directory().await;
        let catalog = h.server.catalog().unwrap();
        let existing = h.server.repositories.values()[0].id;
        catalog.mark_cell_ready(existing).await.unwrap();
        let alice = h.login().await;
        let route = "/api/repos/creation";
        let destination = h.json(route, &alice).await;
        let owner = destination["owner"].as_str().unwrap();
        let request_id = Uuid::now_v7();
        let request =
            json!({"request_id":request_id,"name":"first-repo","description":"Start here"});
        let without_csrf = h
            .http
            .post(format!("{}{route}", h.origin))
            .header(reqwest::header::COOKIE, &alice)
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(without_csrf.status(), StatusCode::FORBIDDEN);
        let pending = catalog
            .reserve_browser_repository(
                BrowserNamespace::Personal(owner.into()),
                "first-repo".into(),
                "Start here".into(),
                request_id,
                MembershipActor {
                    issuer: h.server.auth.as_ref().unwrap().issuer().into(),
                    subject: "alice-id".into(),
                },
                vec![RepositoryMember {
                    subject: "alice-id".into(),
                    name: "Alice".into(),
                    access: RepositoryAccess::Admin,
                }],
            )
            .await
            .unwrap();
        assert_eq!(
            pending.application,
            RepositoryApplicationState::EmptyCellPending
        );
        let created: Value = h
            .write(Method::POST, route, &alice, request.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            created,
            json!({"id":pending.id,"owner":owner,"name":"first-repo"})
        );
        let recovered: Value = h
            .write(Method::POST, route, &alice, request.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(recovered, created);
        for collision in [
            json!({"request_id":Uuid::now_v7(),"name":"FIRST-REPO","description":""}),
            json!({"request_id":request_id,"name":"different","description":"Start here"}),
        ] {
            assert_eq!(
                h.write(Method::POST, route, &alice, collision)
                    .await
                    .status(),
                StatusCode::CONFLICT
            );
        }
        *h.provider.mode.lock().await = "member".into();
        let bob = h.login().await;
        assert_ne!(h.json(route, &bob).await["owner"], owner);
        let lookup = format!("/api/repos?owner={owner}&name=first-repo");
        assert_eq!(h.json(&lookup, &bob).await["repositories"], json!([]));
        assert_eq!(
            h.json(&lookup, &alice).await["repositories"][0]["can_admin"],
            true
        );

        let token: Value = h
            .write(
                Method::POST,
                "/api/git-token",
                &alice,
                json!({"owner":owner,"repository":"first-repo","access":"write"}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut url = url::Url::parse(&format!("{}/git/{owner}/first-repo.git", h.origin)).unwrap();
        url.set_username("crab").unwrap();
        let secret = token["token"].as_str().unwrap();
        url.set_password(Some(secret)).unwrap();
        let source = tempfile::tempdir().unwrap();
        let path = source.path();
        git::success(
            path,
            &["init", "--initial-branch=main", "--object-format=sha1", "."],
        )
        .await;
        std::fs::write(path.join("README.md"), "# First repository\n").unwrap();
        git::success(path, &["add", "README.md"]).await;
        git::success(path, &["commit", "-m", "Initial commit"]).await;
        let pushed = git::git(path, &["push", url.as_str(), "main"]).await;
        assert!(
            pushed.status.success(),
            "{}",
            String::from_utf8_lossy(&pushed.stderr).replace(secret, "[redacted]")
        );
        let browse = format!("/api/repos/{owner}/first-repo/tree?rev=refs%2Fheads%2Fmain");
        let tree = h.json(&browse, &alice).await;
        assert!(
            tree["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["path"] == "README.md")
        );
        h.close().await;
    }
}
