use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    middleware,
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    RepositoryAccess,
    app::{self, Error, Result},
    auth::Principal,
    catalog::{
        CatalogDocument, CatalogError, CatalogRecord, CatalogStore, MembershipActor,
        RepositoryBaseline,
        transfer::{Decision, Offer, Proposal, Recipient, State as OfferState},
    },
    cells::{
        Address,
        directory::{IdentityKey, ResolveHandles},
        organization::{
            Caller, Role,
            discovery::Landing,
            properties::{
                Definition, GetSchema, PrepareTransferValues, PropertyValue,
                Repository as PropertyRepository, TransferValues, transfer_seed,
            },
        },
    },
    repository::{access, settings},
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repositories/{repository}/transfer",
            get(current).post(propose),
        )
        .route(
            "/api/repository-transfers/{id}",
            get(read)
                .put(decide)
                .layer(DefaultBodyLimit::max(128 * 1024)),
        )
        .route("/api/repository-transfers/incoming", get(incoming::list))
        .layer(DefaultBodyLimit::max(2_048))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

pub(super) mod incoming;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    Account { handle: String },
    Organization { id: Uuid },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Submit {
    request_id: Uuid,
    recipient: Target,
    name: String,
    version: u32,
}

#[derive(Serialize)]
struct Location {
    owner: String,
    name: String,
}

#[derive(Serialize)]
struct View {
    id: Uuid,
    repository: Uuid,
    from: Location,
    to: Location,
    recipient: String,
    state: Option<OfferState>,
    expires_at_ms: i64,
    can_accept: bool,
    can_cancel: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    properties: Option<Vec<Definition>>,
}

impl View {
    fn new(
        record: &CatalogRecord,
        offer: &Offer,
        now_ms: i64,
        source: bool,
        recipient: bool,
    ) -> Self {
        let available = offer.available(record, now_ms);
        Self {
            id: offer.id,
            repository: record.id,
            from: Location {
                owner: offer.from_owner.clone(),
                name: offer.from_name.clone(),
            },
            to: Location {
                owner: offer.destination_owner(),
                name: offer.name.clone(),
            },
            recipient: offer.recipient_label.clone(),
            state: (offer.state != OfferState::Pending || available).then_some(offer.state),
            expires_at_ms: offer.expires_at_ms,
            can_accept: recipient && available,
            can_cancel: source && available,
            properties: None,
        }
    }
}

#[derive(Serialize)]
struct Current {
    version: u32,
    can_transfer: bool,
    offer: Option<View>,
}

pub(super) fn account(principal: &Principal) -> Result<MembershipActor> {
    if !principal.is_user() {
        return Err(Error::SessionRequired);
    }
    let actor = app::account(principal)?;
    Ok(MembershipActor {
        issuer: actor.issuer,
        subject: actor.subject,
    })
}

fn key(actor: &MembershipActor) -> IdentityKey {
    IdentityKey {
        issuer: actor.issuer.clone(),
        subject: actor.subject.clone(),
    }
}

pub(super) async fn organization_owner(
    server: &Server,
    organization: Uuid,
    actor: &MembershipActor,
) -> Result<bool> {
    let Some(actor) = access::actor_id(server, key(actor)).await? else {
        return Ok(false);
    };
    Ok(access::organization_role(server, organization, actor).await? == Some(Role::Owner))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceAccess {
    None,
    Admin,
    Allowed,
}

impl SourceAccess {
    fn require_transfer(self) -> Result<()> {
        match self {
            Self::None => Err(Error::NotFound),
            Self::Admin => Err(Error::RepositoryTransferRestricted),
            Self::Allowed => Ok(()),
        }
    }
}

async fn source_access(
    server: &Server,
    record: &CatalogRecord,
    actor: &MembershipActor,
) -> Result<SourceAccess> {
    management_access(
        server,
        record,
        record.baseline(),
        actor,
        Management::Transfer,
    )
    .await
}

pub(super) enum Management {
    Transfer,
    Delete,
    Visibility,
}

pub(super) async fn management_access(
    server: &Server,
    record: &CatalogRecord,
    baseline: RepositoryBaseline,
    actor: &MembershipActor,
    operation: Management,
) -> Result<SourceAccess> {
    let auth = server.auth.as_ref().ok_or(Error::SessionRequired)?;
    if actor.issuer != auth.issuer()
        || auth
            .account_state(&actor.subject)
            .await
            .map_err(|error| Error::IdentityAuthority(Box::new(error)))?
            .suspended
    {
        return Ok(SourceAccess::None);
    }
    let (organization_admin, permitted) = if let Some(organization) = &record.organization {
        let grant = access::organization_grants_for_identity(
            server,
            key(actor),
            [(organization.id, !organization.teams.is_empty())].into(),
        )
        .await?
        .remove(&organization.id)
        .unwrap_or_default();
        let admin = access::organization_access(&grant, &organization.teams, baseline)
            == Some(RepositoryAccess::Admin);
        let members_allowed = match operation {
            Management::Transfer => grant.member_transfer,
            Management::Delete => grant.member_deletion,
            Management::Visibility => grant.member_visibility,
        };
        (admin, grant.owner || members_allowed)
    } else {
        (false, true)
    };
    let admin_access = if permitted {
        SourceAccess::Allowed
    } else {
        SourceAccess::Admin
    };
    if organization_admin
        || record.members.iter().any(|member| {
            member.subject == actor.subject && member.access == RepositoryAccess::Admin
        })
    {
        return Ok(admin_access);
    }
    let admin_groups: Vec<_> = record
        .groups
        .iter()
        .filter(|grant| grant.access == RepositoryAccess::Admin)
        .cloned()
        .collect();
    if !admin_groups.is_empty()
        && auth.is_oidc()
        && auth
            .mention_group(&actor.subject, &admin_groups)
            .await?
            .is_some()
    {
        return Ok(admin_access);
    }
    Ok(SourceAccess::None)
}

async fn can_receive(server: &Server, target: &Recipient, actor: &MembershipActor) -> Result<bool> {
    match target {
        Recipient::Account { identity } => Ok(identity == actor),
        Recipient::Organization { id } => organization_owner(server, *id, actor).await,
    }
}

async fn target(
    server: &Server,
    input: Target,
    requester: &MembershipActor,
    renaming: bool,
) -> Result<(Recipient, String, Option<String>)> {
    match input {
        Target::Account { handle } => {
            let handle = handle.trim().trim_start_matches('@').to_ascii_lowercase();
            crate::cells::directory::validate_handle(&handle).map_err(Error::Invalid)?;
            let cells = server.cells().ok_or(Error::CellUnavailable)?;
            let identities = app::query_output(
                cells
                    .query::<ResolveHandles>(Address::Directory, None, vec![handle.clone()])
                    .await,
            )?;
            let [Some(identity)] = identities.as_slice() else {
                return Err(Error::Invalid("No account has that handle"));
            };
            if identity.issuer != requester.issuer {
                return Err(Error::Invalid("No account has that handle"));
            }
            Ok((
                Recipient::Account {
                    identity: MembershipActor {
                        issuer: identity.issuer.clone(),
                        subject: identity.subject.clone(),
                    },
                },
                format!("@{handle}"),
                None,
            ))
        }
        Target::Organization { id } => {
            let (handle, name) = destination_organization(server, id, requester, renaming).await?;
            Ok((Recipient::Organization { id }, name, Some(handle)))
        }
    }
}

async fn destination_organization(
    server: &Server,
    id: Uuid,
    requester: &MembershipActor,
    renaming: bool,
) -> Result<(String, String)> {
    let denied = || Error::RepositoryTransferDestinationCreationRequired;
    let actor = access::actor_id(server, key(requester))
        .await?
        .ok_or_else(denied)?;
    let Some(Landing::Member {
        view,
        can_create: true,
        ..
    }) = access::organization_landing(server, id, actor).await?
    else {
        // Unknown organizations and destinations outside the actor's authority
        // return the same error, without disclosing private metadata.
        return Err(denied());
    };
    if renaming && view.role != Role::Owner {
        return Err(Error::RepositoryTransferDestinationOwnerRequired);
    }
    Ok((view.organization.handle, view.organization.name))
}

fn now() -> Result<i64> {
    crate::cells::unix_now_ms().map_err(Error::Cell)
}

fn catalog_error(error: CatalogError) -> Error {
    match error {
        CatalogError::NameUnavailable => Error::RepositoryNameUnavailable,
        CatalogError::Conflict => Error::PageChanged,
        error => app::repository_access_error(error),
    }
}

async fn load(server: &Server) -> Result<(CatalogStore, CatalogDocument)> {
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (document, _) = catalog.load().await.map_err(catalog_error)?;
    Ok((catalog, document))
}

async fn current(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
) -> Result<Json<Current>> {
    let actor = account(&principal)?;
    let (_, document) = load(&server).await?;
    let record = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    let access = source_access(&server, record, &actor).await?;
    if access == SourceAccess::None {
        return Err(Error::NotFound);
    }
    let now = now()?;
    Ok(Json(Current {
        version: record.name_version(),
        can_transfer: access == SourceAccess::Allowed,
        offer: record
            .transfer
            .as_ref()
            .map(|offer| View::new(record, offer, now, true, false)),
    }))
}

async fn propose(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
    Json(input): Json<Submit>,
) -> Result<Json<View>> {
    let requester = account(&principal)?;
    let (catalog, document) = load(&server).await?;
    let record = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    source_access(&server, record, &requester)
        .await?
        .require_transfer()?;
    let source = source_repository(&server, &document, record).await?;
    let (server, source) = (server.as_ref(), source.as_ref());
    let request_id = input.request_id;
    let destination = match &input.recipient {
        Target::Organization { id } if id.is_nil() => {
            return Err(Error::Invalid("Choose a valid organization ID"));
        }
        Target::Organization { id } => Some(*id),
        Target::Account { .. } => None,
    };
    let renaming = input.name != record.name;
    if let Some(id) = destination {
        destination_organization(server, id, &requester, renaming).await?;
    }
    let (published, now) = settings::with_transfer_ownership_leases(
        server,
        source,
        destination,
        |cancel| async move {
            source_access(server, record, &requester)
                .await?
                .require_transfer()?;
            if settings::load_lifecycle(server, source).await?.archived {
                return Err(Error::Archived);
            }
            let (recipient, recipient_label, destination_handle) =
                target(server, input.recipient, &requester, renaming).await?;
            account(&principal)?;
            if cancel.is_cancelled() {
                return Err(Error::TransferPublication(
                    crab_remote::publication::Error::Cancelled,
                ));
            }
            let now = now()?;
            let published = catalog
                .propose_transfer(
                    document.version,
                    repository,
                    Proposal {
                        id: request_id,
                        requester,
                        recipient,
                        recipient_label,
                        destination_handle,
                        name: input.name,
                        version: input.version,
                    },
                    now,
                )
                .await
                .map_err(catalog_error)?;
            Ok((published, now))
        },
    )
    .await?;
    let (record, offer) = published.transfer(request_id).ok_or(Error::NotFound)?;
    Ok(Json(View::new(record, offer, now, true, false)))
}

async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<View>> {
    let actor = account(&principal)?;
    let (_, document) = load(&server).await?;
    let (record, offer) = document.transfer(id).ok_or(Error::NotFound)?;
    let recipient = can_receive(&server, &offer.recipient, &actor).await?;
    let source = offer.state == OfferState::Pending
        && source_access(&server, record, &actor).await? != SourceAccess::None;
    if !recipient && !source && actor != offer.requester {
        return Err(Error::NotFound);
    }
    let mut view = View::new(record, offer, now()?, source, recipient);
    if view.can_accept
        && let Recipient::Organization { id } = offer.recipient
    {
        let actor = super::organizations::actor(&server, &principal).await?;
        let cells = server.cells().ok_or(Error::CellUnavailable)?;
        let schema = cells
            .query::<GetSchema>(Address::Organization(id), None, Caller { actor })
            .await
            .map_err(super::organizations::read_error)?
            .output;
        view.properties = Some(schema.definitions);
    }
    Ok(Json(view))
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Choice {
    Accept,
    Decline,
    Cancel,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resolve {
    decision: Choice,
    #[serde(default)]
    properties: BTreeMap<Uuid, PropertyValue>,
}

async fn decide(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Json(input): Json<Resolve>,
) -> Result<Json<View>> {
    let actor = account(&principal)?;
    let (catalog, document) = load(&server).await?;
    let (record, offer) = document.transfer(id).ok_or(Error::NotFound)?;
    let source = matches!(input.decision, Choice::Cancel);
    let permitted = if source {
        // Even under an owners-only transfer policy, administrators can stop an offer.
        source_access(&server, record, &actor).await? != SourceAccess::None
    } else {
        can_receive(&server, &offer.recipient, &actor).await?
    };
    if !permitted {
        return Err(Error::NotFound);
    }
    if !input.properties.is_empty() && !matches!(input.decision, Choice::Accept) {
        return Err(Error::Invalid(
            "Set repository properties when accepting a transfer",
        ));
    }
    let now = now()?;
    let published = match input.decision {
        Choice::Cancel => catalog
            .decide_transfer(document.version, id, Decision::Cancel, now)
            .await
            .map_err(catalog_error)?,
        Choice::Decline => catalog
            .decide_transfer(document.version, id, Decision::Decline, now)
            .await
            .map_err(catalog_error)?,
        Choice::Accept => {
            accept(
                &server,
                &principal,
                actor,
                &catalog,
                &document,
                id,
                input.properties,
            )
            .await?
        }
    };
    let (record, offer) = published.transfer(id).ok_or(Error::NotFound)?;
    let result = View::new(record, offer, now, source, !source);
    if offer.state == OfferState::Accepted {
        server
            .install_catalog(published)
            .await
            .map_err(|error| Error::RepositorySetup(Box::new(error)))?;
    }
    Ok(Json(result))
}

async fn accept(
    server: &Server,
    principal: &Principal,
    actor: MembershipActor,
    catalog: &CatalogStore,
    document: &CatalogDocument,
    id: Uuid,
    properties: BTreeMap<Uuid, PropertyValue>,
) -> Result<CatalogDocument> {
    let (record, offer) = document.transfer(id).ok_or(Error::NotFound)?;
    let destination = match &offer.recipient {
        Recipient::Organization { id } => Some(*id),
        Recipient::Account { .. } => None,
    };
    if destination.is_none() && !properties.is_empty() {
        return Err(Error::Invalid(
            "Repository properties require an organization destination",
        ));
    }
    let property_seed = transfer_seed(id, &properties)?;
    if offer.state == OfferState::Accepted {
        if record.property_seed != property_seed {
            return Err(Error::PageChanged);
        }
        return Ok(document.clone());
    }
    if !offer.available(record, now()?) {
        return Err(Error::PageChanged);
    }
    let active = source_repository(server, document, record).await?;
    let source = active.as_ref();
    // Both policies must stay current through publication. Existing source
    // publications also finish under the former ownership before this switch.
    settings::with_transfer_ownership_leases(server, source, destination, |cancel| async move {
        if settings::load_lifecycle(server, source).await?.archived {
            return Err(Error::Archived);
        }
        if !can_receive(server, &offer.recipient, &actor).await? {
            return Err(Error::PageChanged);
        }
        // The confirmed address names the recipient's username. After a
        // rename it could belong to someone else, so the offer must be redone.
        if destination.is_none()
            && access::username(server, key(&actor)).await.as_deref()
                != Some(offer.destination_owner().as_str())
        {
            return Err(Error::PageChanged);
        }
        match source_access(server, record, &offer.requester).await? {
            SourceAccess::None => return Err(Error::PageChanged),
            SourceAccess::Admin => return Err(Error::RepositoryTransferRestricted),
            SourceAccess::Allowed => {}
        }
        let renaming = offer.name != record.name;
        if let Some(destination) = destination {
            let (handle, _) =
                destination_organization(server, destination, &offer.requester, renaming).await?;
            if handle != offer.destination_owner() {
                return Err(Error::PageChanged);
            }
        }
        settings::require_no_issue_reservations(server, source).await?;
        if renaming {
            // The source administrator requested the name change. A
            // destination owner cannot lend them a ruleset bypass.
            settings::check_rename(server, record, key(&offer.requester)).await?;
        }
        if cancel.is_cancelled() {
            return Err(Error::TransferPublication(
                crab_remote::publication::Error::Cancelled,
            ));
        }
        let signed = app::actor(principal)?;
        if let Some(destination) = destination {
            if catalog
                .load_snapshot()
                .await
                .map_err(catalog_error)?
                .0
                .version
                != document.version
            {
                return Err(Error::PageChanged);
            }
            let owner = access::actor_id(server, key(&actor))
                .await?
                .ok_or(Error::NotFound)?;
            let cells = server.cells().ok_or(Error::CellUnavailable)?;
            cells
                .command::<PrepareTransferValues>(
                    Address::Organization(destination),
                    app::mutation_identity()?,
                    TransferValues {
                        actor: owner,
                        offer: id,
                        repository: PropertyRepository {
                            id: record.id,
                            ownership_epoch: record
                                .name_version()
                                .checked_add(1)
                                .ok_or(Error::PageChanged)?,
                            property_seed,
                        },
                        overrides: properties,
                    },
                )
                .await
                .map_err(super::organizations::read_error)?
                .output
                .map_err(|error| Error::Invalid(error.message()))?;
            if cancel.is_cancelled() {
                return Err(Error::TransferPublication(
                    crab_remote::publication::Error::Cancelled,
                ));
            }
        }
        catalog
            .decide_transfer(
                document.version,
                offer.id,
                Decision::Accept {
                    actor,
                    display_name: signed.name,
                    property_seed,
                },
                now()?,
            )
            .await
            .map_err(catalog_error)
    })
    .await
}

pub(super) async fn source_repository(
    server: &Server,
    document: &CatalogDocument,
    record: &CatalogRecord,
) -> Result<Arc<crate::repository::Repository>> {
    let matches = |repo: &crate::repository::Repository| {
        repo.config.owner == record.owner
            && repo.config.name == record.name
            && repo.organization == record.organization_id()
            && repo.ownership_epoch == record.ownership_epoch
    };
    let active = server
        .repositories
        .by_id(record.id)
        .ok_or(Error::NotFound)?;
    if matches(&active) {
        return Ok(active);
    }
    // A peer with stale routing could otherwise lock the former owner's policy.
    server
        .install_catalog(document.clone())
        .await
        .map_err(|error| Error::RepositorySetup(Box::new(error)))?;
    server
        .repositories
        .by_id(record.id)
        .filter(|repo| matches(repo))
        .ok_or(Error::PageChanged)
}

#[cfg(test)]
mod tests;
