use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    routing::get,
};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::catalog_browse::{self, Scope, Search};
use crate::{
    RepositoryAccess, RepositoryMember, app,
    auth::Principal,
    catalog::{
        BrowserCreation, BrowserNamespace, CatalogError, CatalogRecord, MembershipActor,
        RepositoryApplicationState, visibility::Visibility,
    },
    cells::{
        Address,
        organization::{
            Caller,
            discovery::Landing,
            repositories::{
                AuthorizeCreation, Authorized, Creation, CreationOptions, CreationOutcome,
                GetCreationOptions, InitializeCreationProperties, valid_creation,
            },
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
    #[serde(default)]
    public: bool,
}

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(selection): Query<RepositorySelection>,
) -> app::Result<Json<serde_json::Value>> {
    if principal.is_anonymous()
        && (!principal.permits_public_reads() || (!selection.public && selection.owner.is_none()))
    {
        return Err(app::Error::SessionRequired);
    }
    if selection.owner.is_some() != selection.name.is_some() {
        return Err(app::Error::Invalid(
            "owner and name must be provided together",
        ));
    }
    if selection.q.is_some()
        || selection.after.is_some()
        || selection.limit.is_some()
        || selection.public
    {
        if selection.owner.is_some() || (selection.public && selection.organization.is_some()) {
            return Err(app::Error::Invalid(
                "Search and exact repository lookup cannot be combined",
            ));
        }
        return catalog_browse::list(
            &server,
            &principal,
            if selection.public {
                Scope::Public
            } else {
                selection
                    .organization
                    .map_or(Scope::All, Scope::Organization)
            },
            Search {
                query: selection.q.unwrap_or_default(),
                after: selection.after,
                limit: selection.limit,
            },
        )
        .await;
    }
    if !principal.is_anonymous() {
        app::actor(&principal)?;
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
    let mut recheck = HashSet::new();
    for VisibleRepository {
        repository,
        principal: current,
        organization_owner,
        forking,
        visibility,
        topics,
        ..
    } in matches
    {
        let public_reader = current.is_public_reader(repository.id);
        let protections = if public_reader {
            crate::repository::settings::BranchProtections {
                version: 0,
                rules: Vec::new(),
            }
        } else {
            crate::repository::settings::load(&server, &repository).await?
        };
        let lifecycle = crate::repository::settings::load_lifecycle(&server, &repository).await?;
        let mut view = json!({
            "id": repository.id,
            "owner": repository.config.owner, "name": repository.config.name,
            "description": repository.config.description,
            "access": if current.can_write(repository.id) { "write" } else { "read" },
            "can_admin": current.can_admin(repository.id),
            "visibility": visibility,
            "can_configure_visibility": current.can_admin(repository.id) && repository.forked_from.is_none()
                && (repository.organization.is_some() || crate::catalog::is_personal_namespace(&repository.config.owner)),
            "public_read_only": public_reader,
            "can_maintain": current.can_maintain(repository.id),
            "fork": repository.forked_from.is_some(),
            "can_fork": (principal.is_user() || principal.is_local()) && forking.is_some_and(|(enabled, _, policy)| enabled && policy.allowed),
            "can_fork_to_organization": organization_owner && forking.is_some_and(|(enabled, _, policy)| enabled && policy.allowed),
            "can_fork_to_other_organizations": forking.is_some_and(|(enabled, external, policy)| enabled && external && policy.allowed),
            "protection_version": protections.version,
            "protected_branches": protections.rules,
            "archive_version": lifecycle.version,
            "archived": lifecycle.archived,
        });
        if let Some(organization) = repository.owning_organization() {
            view["organization_id"] = json!(organization);
        }
        if current.can_admin(repository.id)
            && let Some((organization_allows, _, policy)) = forking
        {
            view["forking"] = json!({
                "organization_allows": organization_allows,
                "version": policy.version,
                "allowed": policy.allowed,
            });
        }
        if !topics.is_empty() {
            view["topics"] = json!(topics);
        }
        if repository.forked_from.is_some()
            || (!principal.is_local()
                && (repository.owning_organization().is_some()
                    || !topics.is_empty()
                    || visibility.is_public()))
        {
            recheck.insert(repository.id);
            recheck.extend(repository.forked_from);
        }
        repositories.push((repository, view));
    }
    if !recheck.is_empty() {
        // Fork access and source access can differ. Read both grants from the
        // same fresh catalog snapshot before exposing any upstream address.
        let current: HashMap<_, _> =
            visible_matching(&server, &principal, None, |repo| recheck.contains(&repo.id))
                .await?
                .into_iter()
                .map(|visible| (visible.repository.id, visible))
                .collect();
        repositories.retain_mut(|(original, view)| {
            if !recheck.contains(&original.id) {
                return true;
            }
            let Some(visible) = current.get(&original.id) else {
                return false;
            };
            let fresh = &visible.repository;
            if view["public_read_only"] != visible.principal.is_public_reader(fresh.id)
                || fresh.config.owner != original.config.owner
                || fresh.config.name != original.config.name
                || fresh.ownership_epoch != original.ownership_epoch
                || fresh.forked_from != original.forked_from
            {
                return false;
            }
            if let Some(fields) = view.as_object_mut() {
                fields.insert("visibility".into(), json!(visible.visibility));
                if visible.topics.is_empty() {
                    fields.remove("topics");
                } else {
                    fields.insert("topics".into(), json!(visible.topics));
                }
                if let Some(source) = fresh.forked_from.and_then(|id| current.get(&id)) {
                    let source = &source.repository;
                    fields.insert("forked_from".into(), json!({
                        "id": source.id, "owner": source.config.owner, "name": source.config.name,
                    }));
                }
            }
            true
        });
    }
    let repositories: Vec<_> = repositories.into_iter().map(|(_, view)| view).collect();
    Ok(Json(json!({"repositories":repositories})))
}

pub(super) struct VisibleRepository {
    pub(super) repository: Arc<crate::repository::Repository>,
    pub(super) topics: Vec<String>,
    pub(super) metadata_updated_at_ms: Option<u64>,
    pub(super) team_access: Option<RepositoryAccess>,
    pub(super) team_access_inherited: bool,
    pub(super) principal: Principal,
    pub(super) organization_owner: bool,
    forking: Option<(bool, bool, crate::catalog::forking::Forking)>,
    pub(super) visibility: crate::catalog::visibility::Visibility,
}

pub(super) async fn visible(
    server: &Server,
    principal: &Principal,
    selection: Option<(&str, &str)>,
    organization: Option<Uuid>,
) -> app::Result<Vec<VisibleRepository>> {
    visible_matching(server, principal, None, |repository| {
        selection.is_none_or(|(owner, name)| {
            owner == repository.config.owner && name == repository.config.name
        }) && organization.is_none_or(|id| repository.owning_organization() == Some(id))
    })
    .await
}

pub(super) async fn visible_matching(
    server: &Server,
    principal: &Principal,
    team: Option<(Uuid, &[Uuid])>,
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
    let local_listing = principal.is_local() && team.is_none();
    let document = if local_listing && selected.iter().all(|repo| repo.organization.is_none()) {
        None
    } else {
        let catalog = server.catalog().ok_or(app::Error::NotFound)?;
        Some(
            catalog
                .load_snapshot()
                .await
                .map_err(|error| app::Error::Membership(Box::new(error)))?
                .0,
        )
    };
    let grants = document.as_ref().map(|document| {
        document
            .repositories
            .iter()
            .map(|record| (record.id, record))
            .collect::<std::collections::HashMap<_, _>>()
    });
    let issuer = server
        .auth
        .as_ref()
        .map_or("urn:crab:local", |auth| auth.issuer());
    let mut organizations = std::collections::HashMap::new();
    if let Some(grants) = &grants
        && !local_listing
        && !principal.is_anonymous()
    {
        for repo in &selected {
            if let Some(record) = grants.get(&repo.id) {
                let source = record
                    .access_record(record.forked_from.and_then(|id| grants.get(&id).copied()))
                    .map_err(|error| app::Error::Membership(Box::new(error)))?;
                if let Some(access) = &source.organization {
                    *organizations.entry(access.id).or_insert(false) |= !access.teams.is_empty();
                }
                if let Some(destination) = record.external_fork_destination(source) {
                    organizations.entry(destination).or_insert(false);
                }
            }
        }
    }
    let organization_grants =
        crate::repository::access::organization_grants(server, principal, organizations).await?;
    let mut visible = Vec::new();
    for repository in selected {
        let record = grants
            .as_ref()
            .and_then(|grants| grants.get(&repository.id))
            .copied();
        let mut team_access = None;
        let mut team_access_inherited = false;
        let current = if local_listing && repository.organization.is_none() {
            principal.clone()
        } else if let (Some(grants), Some(document)) = (&grants, &document) {
            let Some(record) = record.filter(|record| {
                record.owner == repository.config.owner
                    && record.name == repository.config.name
                    && record.organization_id() == repository.organization
                    && record.ownership_epoch == repository.ownership_epoch
                    && record.forked_from == repository.forked_from
            }) else {
                continue;
            };
            let source = record
                .access_record(record.forked_from.and_then(|id| grants.get(&id).copied()))
                .map_err(|error| app::Error::Membership(Box::new(error)))?;
            if let Some((team, ancestors)) = team
                && record.external_fork_destination(source).is_none()
                && let Some(access) = &source.organization
            {
                let direct = access.teams.iter().find(|grant| grant.team == team);
                let inherited = access
                    .teams
                    .iter()
                    .filter(|grant| ancestors.contains(&grant.team))
                    .map(|grant| grant.access)
                    .max();
                let direct = direct.map(|grant| grant.access);
                team_access = direct.max(inherited);
                team_access_inherited = inherited > direct;
            }
            let snapshot = record.access_snapshot(document.version, source);
            let inherited = crate::repository::access::snapshot_organization_access(
                &snapshot,
                &organization_grants,
            );
            let mut config = repository.config.clone();
            config.members = snapshot.members;
            config.groups = snapshot.groups;
            principal
                .with_catalog_membership(
                    repository.id,
                    &config,
                    issuer,
                    inherited,
                    record.name_version() > 0,
                    record.fork_owner.as_ref(),
                )
                .with_public_visibility(record.id, record.visibility.value.is_public())
        } else {
            principal.clone()
        };
        if !current.can_read(repository.id) || (team.is_some() && team_access.is_none()) {
            continue;
        }
        let organization = repository
            .organization
            .and_then(|id| organization_grants.get(&id));
        let organization_owner = organization.is_some_and(|grant| grant.owner);
        let forking = record
            .filter(|record| {
                record.organization_id().is_some()
                    && record.forked_from.is_none()
                    && !record.visibility.value.is_public()
            })
            .map(|record| {
                (
                    organization.is_some_and(|grant| grant.private_forks),
                    organization.is_some_and(|grant| grant.external_forks),
                    record.forking,
                )
            });
        visible.push(VisibleRepository {
            topics: record.map_or_else(
                || repository.topics.clone(),
                |record| record.topics.names.clone(),
            ),
            metadata_updated_at_ms: record.and_then(|record| record.metadata_updated_at_ms),
            repository,
            principal: current,
            organization_owner,
            forking,
            visibility: record.map_or(crate::catalog::visibility::Visibility::Private, |record| {
                record.visibility.value
            }),
            team_access,
            team_access_inherited,
        });
    }
    Ok(visible)
}

pub(super) fn namespace_routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/forks", get(forks))
        .route(
            "/api/repos/creation",
            get(creation)
                .post(create)
                .layer(axum::extract::DefaultBodyLimit::max(4096)),
        )
        .route("/api/repos/personal", get(personal))
        .route(
            "/api/organizations/{organization}/repositories/creation",
            get(organization_creation)
                .post(create_organization)
                .layer(axum::extract::DefaultBodyLimit::max(128 * 1024)),
        )
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn forks(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(search): Query<Search>,
) -> app::Result<Json<Value>> {
    let repository = app::repository(&server, &principal, &key)?;
    let current = app::membership(&server, &principal, &repository).await?;
    let viewer = current
        .principal
        .related_catalog(repository.id)
        .ok_or(app::Error::NotFound)?;
    let result =
        catalog_browse::list(&server, &viewer, Scope::Forks(repository.id), search).await?;
    // Listing a fork network needs both source access and an independent grant
    // for every result. A concurrent source revocation invalidates the page.
    app::membership(&server, &principal, &repository).await?;
    Ok(result)
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

async fn personal(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(search): Query<Search>,
) -> app::Result<Json<serde_json::Value>> {
    let owner = namespace(&principal)?;
    let page = catalog_browse::list(&server, &principal, Scope::Personal(owner), search).await?;
    app::account(&principal)?;
    Ok(page)
}

async fn creation(
    Extension(principal): Extension<Principal>,
) -> app::Result<Json<serde_json::Value>> {
    let owner = namespace(&principal)?;
    Ok(Json(
        json!({"owner":owner,"default_branch":crate::config::DEFAULT_REPOSITORY_BRANCH,
            "visibilities":personal_visibilities(&principal)}),
    ))
}

fn personal_visibilities(principal: &Principal) -> &'static [Visibility] {
    if principal.is_local() {
        &[Visibility::Private]
    } else {
        &[Visibility::Private, Visibility::Public]
    }
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
) -> app::Result<Json<CreationOptions>> {
    let actor = super::organizations::actor(&server, &principal).await?;
    let options = server
        .cells()
        .ok_or(app::Error::CellUnavailable)?
        .query::<GetCreationOptions>(Address::Organization(organization), None, Caller { actor })
        .await
        .map_err(super::organizations::read_error)?
        .output;
    app::account(&principal)?;
    Ok(Json(options))
}

async fn ensure_can_create(server: &Server, organization: Uuid, actor: Uuid) -> app::Result<()> {
    match crate::repository::access::organization_landing(server, organization, actor).await? {
        Some(Landing::Member {
            can_create: true, ..
        }) => Ok(()),
        _ => Err(app::Error::NotFound),
    }
}

async fn create_organization(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
    Json(input): Json<BrowserCreation>,
) -> app::Result<(StatusCode, Json<CreatedRepository>)> {
    let actor = super::organizations::actor(&server, &principal).await?;
    input.validate()?;
    app::account(&principal)?;
    let worker = Arc::clone(&server);
    let account = principal.clone();
    let prepare = move |cancel: tokio_util::sync::CancellationToken| async move {
        let cells = worker.cells().ok_or(app::Error::CellUnavailable)?;
        let address = Address::Organization(organization);
        let intent = Creation {
            actor,
            request_id: input.request_id,
            name: input.name.clone(),
            description: input.description.clone(),
            visibility: input.visibility,
            properties: input.properties.clone(),
        };
        let authorized = cells
            .command::<AuthorizeCreation>(address, app::mutation_identity()?, intent.clone())
            .await
            .map_err(super::organizations::read_error)?
            .output;
        let (grant_creator, default_branch) = match authorized {
            CreationOutcome::Accepted {
                grant_creator,
                default_branch,
            } => (grant_creator, default_branch),
            CreationOutcome::Conflict => return Err(app::Error::RepositoryNameUnavailable),
            CreationOutcome::VisibilityRestricted => {
                return Err(app::Error::Invalid(
                    "This organization does not allow you to create repositories with that visibility",
                ));
            }
            CreationOutcome::Properties(error) => return Err(app::Error::Invalid(error.message())),
        };
        // Policy and membership writes share this lease. Bind the
        // decision to the durable reservation before allowing a change.
        app::account(&account)?;
        if cancel.is_cancelled() {
            return Err(app::Error::MergeBusy);
        }
        let record = reserve(
            &worker,
            &account,
            BrowserNamespace::Organization(organization),
            grant_creator,
            input,
        )
        .await?;
        if record.application == RepositoryApplicationState::EmptyCellPending
            && !intent.properties.is_empty()
        {
            // Only the exact reserved creation can seed its values.
            // Publication and later edits always see the same ownership epoch.
            if cancel.is_cancelled() {
                return Err(app::Error::MergeBusy);
            }
            let initialized = cells
                .command::<InitializeCreationProperties>(address, app::mutation_identity()?, intent)
                .await
                .map_err(super::organizations::read_error)?
                .output;
            if matches!(initialized, Authorized::Conflict) {
                return Err(app::Error::RepositoryNameUnavailable);
            }
        }
        Ok((record, default_branch))
    };
    let (record, default_branch) =
        super::organizations::with_org_policy(&server, organization, prepare).await?;
    let response = publish(&server, &principal, record, default_branch).await?;
    ensure_can_create(&server, organization, actor).await?;
    app::account(&principal)?;
    Ok(response)
}

impl BrowserCreation {
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
    Json(input): Json<BrowserCreation>,
) -> app::Result<(StatusCode, Json<CreatedRepository>)> {
    let owner = namespace(&principal)?;
    input.validate()?;
    if !input.properties.is_empty() {
        return Err(app::Error::Invalid(
            "Repository properties are available for organization repositories",
        ));
    }
    if !personal_visibilities(&principal).contains(&input.visibility) {
        return Err(app::Error::Invalid(
            "Choose a visibility available for this account",
        ));
    }
    let record = reserve(
        &server,
        &principal,
        BrowserNamespace::Personal(owner),
        true,
        input,
    )
    .await?;
    publish(
        &server,
        &principal,
        record,
        crate::config::default_repository_branch(),
    )
    .await
}

async fn reserve(
    server: &Server,
    principal: &Principal,
    namespace: BrowserNamespace,
    grant_creator: bool,
    input: BrowserCreation,
) -> app::Result<CatalogRecord> {
    let actor = app::actor(principal)?;
    let catalog = server.catalog().ok_or(app::Error::CellUnavailable)?;
    let members = if principal.is_local() || !grant_creator {
        Vec::new()
    } else {
        vec![RepositoryMember {
            subject: actor.subject.clone(),
            name: actor.name.trim().into(),
            access: RepositoryAccess::Admin,
        }]
    };
    catalog
        .reserve_browser_repository(
            namespace,
            input,
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
        })
}

async fn publish(
    server: &Arc<Server>,
    principal: &Principal,
    record: CatalogRecord,
    default_branch: String,
) -> app::Result<(StatusCode, Json<CreatedRepository>)> {
    let catalog = server.catalog().ok_or(app::Error::CellUnavailable)?;
    let worker = Arc::clone(server);
    let result = server
        .receives
        .spawn(async move {
            if record.application == RepositoryApplicationState::EmptyCellPending {
                catalog
                    .initialize_git(&record, &default_branch, true)
                    .await?;
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
                BrowserCreation {
                    request_id,
                    name: "first-repo".into(),
                    description: "Start here".into(),
                    visibility: Visibility::Private,
                    properties: Default::default(),
                },
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
        h.write(
            Method::PUT,
            &format!("/api/repositories/{}/description", pending.id),
            &alice,
            json!({"version":0,"description":"An edited description"}),
        )
        .await
        .error_for_status()
        .unwrap();
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
