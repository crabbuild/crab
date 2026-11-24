use super::*;
use crate::{
    catalog::{
        MembershipActor,
        conversion::{Begin, RetainedMember},
    },
    cells::{
        directory::{LookupIdentities, LookupProfiles},
        organization::{
            CheckConversion, Conversion, ConversionMember, FinalizeConversion, MemberKey,
            teams::repository_access::InspectMemberRepositoryRoles,
        },
    },
};
use futures_util::{StreamExt, TryStreamExt, stream};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/organizations/{id}/members/conversion",
        axum::routing::post(convert),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    members: Vec<ConversionMember>,
}

async fn convert(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
    Json(input): Json<Selection>,
) -> Result<Json<Value>> {
    let owner = actor(&server, &principal).await?;
    let decision = Conversion {
        owner,
        members: input.members,
    };
    if !decision.valid() {
        return Err(Error::Invalid(
            "Choose 1 to 50 distinct members with current versions",
        ));
    }
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let address = target(organization)?;
    require_owner(&cells, address, owner).await?;
    let worker = Arc::clone(&server);
    let count = decision.members.len();
    membership_change(&server, organization, move |cancel| async move {
        applied(
            cells
                .query::<CheckConversion>(address, None, decision.clone())
                .await
                .map_err(read_error)?
                .output,
        )?;
        let catalog = worker.catalog().ok_or(Error::NotFound)?;
        let (document, _) = catalog.load().await.map_err(app::repository_access_error)?;
        let caller = app::account(&principal)?;
        let retain = retained_members(&cells, address, &decision, &caller.issuer).await?;
        app::account(&principal)?;
        catalog
            .prepare_conversion(
                Begin {
                    organization,
                    revision: document.version,
                    decision: decision.clone(),
                    retain,
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
        finish(&worker, organization, &decision).await
    })
    .await?;
    Ok(Json(json!({"converted": count})))
}

async fn retained_members(
    cells: &CellClient,
    address: Address,
    decision: &Conversion,
    issuer: &str,
) -> Result<Vec<RetainedMember>> {
    let ids: Vec<_> = decision.members.iter().map(|member| member.actor).collect();
    let (identities, profiles) = tokio::try_join!(
        async {
            app::query_output(
                cells
                    .query::<LookupIdentities>(Address::Directory, None, ids.clone())
                    .await,
            )
        },
        async {
            app::query_output(
                cells
                    .query::<LookupProfiles>(Address::Directory, None, ids.clone())
                    .await,
            )
        },
    )?;
    if identities.len() != ids.len() || profiles.len() != ids.len() {
        return Err(Error::CellContract(
            "Conversion Directory lookup returned an invalid result count",
        ));
    }
    // The policy lease holds the selected member and team edges stable.
    // Keep each native role reply bounded and preserve the selection order.
    stream::iter(ids.into_iter().zip(identities).zip(profiles))
        .map(|((member, identity), profile)| async move {
            let identity = identity.ok_or(Error::CellContract(
                "Organization member has no Directory identity",
            ))?;
            if issuer != identity.issuer {
                return Err(Error::Invalid(
                    "A member’s identity provider is unavailable",
                ));
            }
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
            Ok(RetainedMember {
                subject: identity.subject,
                name,
                teams: roles.teams.into_keys().collect(),
            })
        })
        .buffered(8)
        .try_collect()
        .await
}

async fn finish(server: &Server, organization: Uuid, decision: &Conversion) -> Result<()> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let outcome = app::command_output(
        cells
            .command::<FinalizeConversion>(
                target(organization)?,
                app::mutation_identity()?,
                decision.clone(),
            )
            .await,
    )?;
    applied(outcome)?;
    catalog
        .finish_conversion(organization, decision)
        .await
        .map_err(app::repository_access_error)
}

pub(super) async fn recover_locked(server: &Server, organization: Uuid) -> Result<()> {
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (document, _) = catalog.load().await.map_err(app::repository_access_error)?;
    if let Some(decision) = document.pending_conversions.get(&organization) {
        finish(server, organization, decision).await?;
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
