use std::sync::Arc;

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
        transfer::{Decision, Offer, Proposal, Recipient, State as OfferState},
    },
    cells::{
        Address,
        directory::{IdentityKey, ResolveHandles},
        organization::Role,
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
        .route("/api/repository-transfers/{id}", get(read).put(decide))
        .layer(DefaultBodyLimit::max(2_048))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

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
                owner: offer.recipient.namespace(),
                name: offer.name.clone(),
            },
            recipient: offer.recipient_label.clone(),
            state: (offer.state != OfferState::Pending || available).then_some(offer.state),
            expires_at_ms: offer.expires_at_ms,
            can_accept: recipient && available,
            can_cancel: source && available,
        }
    }
}

#[derive(Serialize)]
struct Current {
    version: u32,
    offer: Option<View>,
}

fn account(principal: &Principal) -> Result<MembershipActor> {
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

async fn organization_owner(
    server: &Server,
    organization: Uuid,
    actor: &MembershipActor,
) -> Result<bool> {
    let Some(actor) = access::actor_id(server, key(actor)).await? else {
        return Ok(false);
    };
    Ok(access::organization_role(server, organization, actor).await? == Some(Role::Owner))
}

async fn can_manage_source(
    server: &Server,
    record: &CatalogRecord,
    actor: &MembershipActor,
) -> Result<bool> {
    let auth = server.auth.as_ref().ok_or(Error::SessionRequired)?;
    if actor.issuer != auth.issuer()
        || auth
            .account_state(&actor.subject)
            .await
            .map_err(|error| Error::IdentityAuthority(Box::new(error)))?
            .suspended
    {
        return Ok(false);
    }
    if record
        .members
        .iter()
        .any(|member| member.subject == actor.subject && member.access == RepositoryAccess::Admin)
    {
        return Ok(true);
    }
    if let Some(organization) = record.organization_id()
        && organization_owner(server, organization, actor).await?
    {
        return Ok(true);
    }
    let admin_groups: Vec<_> = record
        .groups
        .iter()
        .filter(|grant| grant.access == RepositoryAccess::Admin)
        .cloned()
        .collect();
    Ok(!admin_groups.is_empty()
        && auth.is_oidc()
        && auth
            .mention_group(&actor.subject, &admin_groups)
            .await?
            .is_some())
}

async fn can_receive(server: &Server, target: &Recipient, actor: &MembershipActor) -> Result<bool> {
    match target {
        Recipient::Account { identity } => Ok(identity == actor),
        Recipient::Organization { id } => organization_owner(server, *id, actor).await,
    }
}

async fn target(server: &Server, input: Target, issuer: &str) -> Result<(Recipient, String)> {
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
            if identity.issuer != issuer {
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
            ))
        }
        Target::Organization { id } if !id.is_nil() => {
            Ok((Recipient::Organization { id }, id.to_string()))
        }
        Target::Organization { .. } => Err(Error::Invalid("Choose a valid organization ID")),
    }
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
    if !can_manage_source(&server, record, &actor).await? {
        return Err(Error::NotFound);
    }
    let now = now()?;
    Ok(Json(Current {
        version: record.name_version(),
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
    if !can_manage_source(&server, record, &requester).await? {
        return Err(Error::NotFound);
    }
    let active = server
        .repositories
        .by_id(repository)
        .ok_or(Error::NotFound)?;
    if settings::load_lifecycle(&server, &active).await?.archived {
        return Err(Error::Archived);
    }
    let (recipient, recipient_label) = target(&server, input.recipient, &requester.issuer).await?;
    account(&principal)?;
    let now = now()?;
    let published = catalog
        .propose_transfer(
            document.version,
            repository,
            Proposal {
                id: input.request_id,
                requester,
                recipient,
                recipient_label,
                name: input.name,
                version: input.version,
            },
            now,
        )
        .await
        .map_err(catalog_error)?;
    let (record, offer) = published
        .transfer(input.request_id)
        .ok_or(Error::NotFound)?;
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
    let source =
        offer.state == OfferState::Pending && can_manage_source(&server, record, &actor).await?;
    if !recipient && !source && actor != offer.requester {
        return Err(Error::NotFound);
    }
    Ok(Json(View::new(record, offer, now()?, source, recipient)))
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
        can_manage_source(&server, record, &actor).await?
    } else {
        can_receive(&server, &offer.recipient, &actor).await?
    };
    if !permitted {
        return Err(Error::NotFound);
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
                &server, &principal, actor, &catalog, &document, record, offer,
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

#[derive(Debug, thiserror::Error)]
enum CommitError {
    #[error(transparent)]
    Application(#[from] Error),
    #[error(transparent)]
    Lease(#[from] crab_remote::publication::Error),
}

async fn accept(
    server: &Server,
    principal: &Principal,
    actor: MembershipActor,
    catalog: &CatalogStore,
    document: &CatalogDocument,
    record: &CatalogRecord,
    offer: &Offer,
) -> Result<CatalogDocument> {
    if offer.state == OfferState::Accepted {
        return Ok(document.clone());
    }
    if !offer.available(record, now()?) {
        return Err(Error::PageChanged);
    }
    let active = server
        .repositories
        .by_id(record.id)
        .ok_or(Error::NotFound)?;
    if active.config.owner != record.owner
        || active.config.name != record.name
        || active.organization != record.organization_id()
    {
        // A peer with stale routing could otherwise lock the former owner's policy.
        server
            .install_catalog(document.clone())
            .await
            .map_err(|error| Error::RepositorySetup(Box::new(error)))?;
    }
    let active = server
        .repositories
        .by_id(record.id)
        .ok_or(Error::NotFound)?;
    let source = active.as_ref();
    if source.config.owner != record.owner
        || source.config.name != record.name
        || source.organization != record.organization_id()
    {
        return Err(Error::PageChanged);
    }
    // Finish any publication holding the source policy lease before switching
    // namespaces. Later publications must read the new owner and its policies.
    settings::with_ruleset_lease(
        server,
        settings::RulesetScope::Repository(source),
        &server.cancellation,
        |cancel| async move {
            if settings::load_lifecycle(server, source).await?.archived {
                return Err(Error::Archived.into());
            }
            if !can_manage_source(server, record, &offer.requester).await?
                || !can_receive(server, &offer.recipient, &actor).await?
            {
                return Err(Error::PageChanged.into());
            }
            if cancel.is_cancelled() {
                return Err(crab_remote::publication::Error::Cancelled.into());
            }
            let signed = app::actor(principal)?;
            catalog
                .decide_transfer(
                    document.version,
                    offer.id,
                    Decision::Accept {
                        actor,
                        display_name: signed.name,
                    },
                    now()?,
                )
                .await
                .map_err(|error| CommitError::Application(catalog_error(error)))
        },
    )
    .await
    .map_err(|error| match error {
        CommitError::Application(error) => error,
        CommitError::Lease(error) => Error::TransferPublication(error),
    })
}

#[cfg(test)]
mod tests;
