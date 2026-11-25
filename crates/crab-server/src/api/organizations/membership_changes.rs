use super::*;
use crate::{
    catalog::{
        MembershipActor,
        membership_changes::{Begin, DepartureGrants, Pending, Rejoin},
    },
    cells::{
        directory::{LookupIdentities, LookupProfiles},
        organization::{
            CheckDeparture, DecideInvitation, Decision, Departure, DepartureMember, DepartureMode,
            FinalizeDeparture, GetInvitation, Invitation, InvitationState, Member, MemberKey,
            teams::repository_access::InspectMemberRepositoryRoles,
        },
    },
};
use futures_util::{StreamExt, TryStreamExt, stream};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/organizations/{id}/members/conversion",
            axum::routing::post(convert),
        )
        .route(
            "/api/organizations/{id}/members/removal",
            axum::routing::post(remove),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    members: Vec<DepartureMember>,
}

async fn convert(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
    Json(input): Json<Selection>,
) -> Result<Json<Value>> {
    let members = depart(
        &server,
        principal,
        organization,
        input.members,
        DepartureMode::Convert,
    )
    .await?;
    Ok(Json(json!({"converted": members.len()})))
}

async fn remove(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
    Json(input): Json<Selection>,
) -> Result<Json<Value>> {
    let members = depart(
        &server,
        principal,
        organization,
        input.members,
        DepartureMode::Remove,
    )
    .await?;
    Ok(Json(json!({"removed": members.len()})))
}

pub(super) async fn depart(
    server: &Arc<Server>,
    principal: Principal,
    organization: Uuid,
    members: Vec<DepartureMember>,
    mode: DepartureMode,
) -> Result<Vec<Member>> {
    let owner = actor(server, &principal).await?;
    let decision = Departure {
        owner,
        members,
        mode,
    };
    if !decision.valid() {
        return Err(Error::Invalid(
            "Choose 1 to 50 distinct members with current versions",
        ));
    }
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let address = target(organization)?;
    require_owner(&cells, address, owner).await?;
    let worker = Arc::clone(server);
    with_org_policy(server, organization, move |cancel| async move {
        applied(
            cells
                .query::<CheckDeparture>(address, None, decision.clone())
                .await
                .map_err(read_error)?
                .output,
        )?;
        let catalog = worker.catalog().ok_or(Error::NotFound)?;
        let (document, _) = catalog.load().await.map_err(app::repository_access_error)?;
        let caller = app::account(&principal)?;
        let issuer = worker
            .auth
            .as_ref()
            .map_or(caller.issuer.as_str(), |auth| auth.issuer());
        let members = departure_grants(&cells, address, &decision, issuer).await?;
        app::account(&principal)?;
        catalog
            .prepare_departure(
                Begin {
                    organization,
                    revision: document.version,
                    decision: decision.clone(),
                    members,
                    actor: MembershipActor {
                        issuer: caller.issuer,
                        subject: caller.subject,
                    },
                },
                &cancel,
            )
            .await
            .map_err(app::repository_access_error)?;
        if cancel.is_cancelled() {
            return Err(Error::MergeBusy);
        }
        finish_departure(&worker, organization, &decision).await
    })
    .await
}

async fn departure_grants(
    cells: &CellClient,
    address: Address,
    decision: &Departure,
    issuer: &str,
) -> Result<Vec<DepartureGrants>> {
    let ids: Vec<_> = decision.members.iter().map(|member| member.actor).collect();
    let identities = app::query_output(
        cells
            .query::<LookupIdentities>(Address::Directory, None, ids.clone())
            .await,
    )?;
    if identities.len() != ids.len() {
        return Err(Error::CellContract(
            "Departure identity lookup returned an invalid result count",
        ));
    }
    // Repository ACLs are scoped to the configured issuer. A departed
    // account from another issuer must not revoke another subject's grants.
    let identities = identities
        .into_iter()
        .map(|identity| {
            let identity = identity.ok_or(Error::CellContract(
                "Organization member has no Directory identity",
            ))?;
            Ok((identity.issuer == issuer).then_some(MembershipActor {
                issuer: identity.issuer,
                subject: identity.subject,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    if decision.mode == DepartureMode::Remove {
        return Ok(identities
            .into_iter()
            .map(|identity| DepartureGrants {
                identity,
                name: String::new(),
                teams: Default::default(),
            })
            .collect());
    }

    let profiles = app::query_output(
        cells
            .query::<LookupProfiles>(Address::Directory, None, ids.clone())
            .await,
    )?;
    if profiles.len() != ids.len() {
        return Err(Error::CellContract(
            "Departure profile lookup returned an invalid result count",
        ));
    }
    // The policy lease holds the selected members and team edges stable.
    stream::iter(ids.into_iter().zip(identities).zip(profiles))
        .map(|((member, identity), profile)| async move {
            let identity = identity.ok_or(Error::Invalid(
                "A member’s identity provider is unavailable",
            ))?;
            let name = match profile {
                Some(profile) if profile.id == member => profile.name,
                None => member.to_string(),
                Some(_) => {
                    return Err(Error::CellContract(
                        "Organization profile lookup returned another actor",
                    ));
                }
            };
            let roles = cells
                .query::<InspectMemberRepositoryRoles>(
                    address,
                    None,
                    MemberKey {
                        actor: decision.owner,
                        member,
                    },
                )
                .await
                .map_err(read_error)?
                .output
                .ok_or(Error::NotFound)?;
            Ok(DepartureGrants {
                identity: Some(identity),
                name,
                teams: roles.teams.into_keys().collect(),
            })
        })
        .buffered(8)
        .try_collect()
        .await
}

async fn finish_departure(
    server: &Server,
    organization: Uuid,
    decision: &Departure,
) -> Result<Vec<Member>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let outcome = app::command_output(
        cells
            .command::<FinalizeDeparture>(
                target(organization)?,
                app::mutation_identity()?,
                decision.clone(),
            )
            .await,
    )?;
    let members = applied(outcome)?;
    catalog
        .finish_membership_change(organization, &Pending::Departure(decision.clone()), false)
        .await
        .map_err(app::repository_access_error)?;
    Ok(members)
}

async fn invitation(
    cells: &CellClient,
    address: Address,
    actor: Uuid,
) -> Result<Option<Invitation>> {
    Ok(cells
        .query::<GetInvitation>(
            address,
            None,
            MemberKey {
                actor,
                member: actor,
            },
        )
        .await
        .map_err(read_error)?
        .output
        .and_then(|view| view.invitation))
}

async fn native_decision(
    cells: &CellClient,
    address: Address,
    decision: Decision,
) -> Result<ChangeOutcome<Invitation>> {
    app::command_output(
        cells
            .command::<DecideInvitation>(address, app::mutation_identity()?, decision)
            .await,
    )
}

pub(super) async fn decide_locked(
    server: &Server,
    organization: Uuid,
    principal: &Principal,
    decision: Decision,
    cancel: &CancellationToken,
) -> Result<Invitation> {
    let account = app::account(principal)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let address = target(organization)?;
    if decision.accept {
        let catalog = server.catalog().ok_or(Error::NotFound)?;
        let (document, _) = catalog.load().await.map_err(app::repository_access_error)?;
        if let Some(former) = document.former_grants(organization, decision.actor)
            && let Some(offer) = invitation(&cells, address, decision.actor).await?
            && offer.state == InvitationState::Pending
            && offer.version == decision.version
            && former.matches(offer.member_version)
        {
            let rejoin = Rejoin {
                recipient: decision.actor,
                invitation_version: decision.version,
                member_version: offer.member_version,
                reinstates: offer.reinstates == Some(true),
                identity: MembershipActor {
                    issuer: account.issuer,
                    subject: account.subject,
                },
            };
            app::account(principal)?;
            catalog
                .prepare_rejoin(organization, rejoin.clone(), cancel)
                .await
                .map_err(app::repository_access_error)?;
            if cancel.is_cancelled() {
                return Err(Error::MergeBusy);
            }
            return finish_rejoin(server, organization, &rejoin)
                .await?
                .ok_or(Error::Conflict);
        }
    }
    app::account(principal)?;
    applied(native_decision(&cells, address, decision).await?)
}

async fn finish_rejoin(
    server: &Server,
    organization: Uuid,
    rejoin: &Rejoin,
) -> Result<Option<Invitation>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let address = target(organization)?;
    let current = invitation(&cells, address, rejoin.recipient)
        .await?
        .filter(|offer| {
            offer.member_version == rejoin.member_version
                && offer.reinstates.unwrap_or(false) == rejoin.reinstates
        });
    // If the response or catalog write was lost, the exact accepted offer
    // proves consent. A changed, expired or cancelled offer grants nothing.
    let accepted = match current {
        Some(offer)
            if offer.state == InvitationState::Accepted
                && offer.version == rejoin.invitation_version + 1 =>
        {
            Some(offer)
        }
        Some(offer)
            if offer.state == InvitationState::Pending
                && offer.version == rejoin.invitation_version =>
        {
            match native_decision(
                &cells,
                address,
                Decision {
                    actor: rejoin.recipient,
                    version: rejoin.invitation_version,
                    accept: true,
                },
            )
            .await?
            {
                ChangeOutcome::Applied(offer) => Some(offer),
                ChangeOutcome::Conflict | ChangeOutcome::NotFound => None,
                ChangeOutcome::LastOwner => {
                    return Err(Error::CellContract(
                        "Invitation acceptance returned a last-owner error",
                    ));
                }
            }
        }
        _ => None,
    };
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let forks_restored = catalog
        .finish_membership_change(
            organization,
            &Pending::Rejoin(rejoin.clone()),
            accepted.is_some(),
        )
        .await
        .map_err(app::repository_access_error)?;
    if forks_restored {
        let (document, _) = catalog.load().await.map_err(app::repository_access_error)?;
        server
            .install_catalog(document)
            .await
            .map_err(|error| Error::RepositorySetup(Box::new(error)))?;
    }
    Ok(accepted)
}

pub(super) async fn recover_locked(server: &Server, organization: Uuid) -> Result<()> {
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (document, _) = catalog.load().await.map_err(app::repository_access_error)?;
    match document.pending_membership_changes.get(&organization) {
        Some(Pending::Departure(decision)) => {
            finish_departure(server, organization, decision).await?;
        }
        Some(Pending::Rejoin(rejoin)) => {
            finish_rejoin(server, organization, rejoin).await?;
        }
        None => {}
    }
    Ok(())
}

pub(crate) async fn recover(server: &Server, organization: Uuid) -> Result<()> {
    with_ruleset_lease(
        server,
        RulesetScope::Organization(organization),
        &server.cancellation,
        |cancel| async move {
            if cancel.is_cancelled() {
                return Err(Error::MergeBusy);
            }
            recover_locked(server, organization).await
        },
    )
    .await
}
