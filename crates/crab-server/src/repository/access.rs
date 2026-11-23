use std::collections::{BTreeMap, HashMap};

use futures_util::{StreamExt, TryStreamExt};
use uuid::Uuid;

use crate::{
    RepositoryAccess, RepositoryMember,
    app::{self, Error, Result},
    auth::Principal,
    catalog::TeamGrant,
    cells::{
        Address,
        directory::{IdentityKey, LookupAccounts, LookupIdentities},
        organization::{
            Caller, GetOrganization, Role,
            teams::{
                repository_access::{
                    ActorTeams, RepositoryActor, RepositoryTeamNames, RepositoryTeamQuery,
                    RepositoryTeamRoster, RepositoryTeamsForActor, VisibleRepositoryTeams,
                },
                review_routing::{Algorithm, ReviewRoster},
            },
        },
        repository::{
            MergeReviewers, RepositoryAuthor,
            review_requests::{automatic::TeamRecipients, routing},
        },
    },
    server::Server,
};

pub(crate) async fn actor_id(server: &Server, key: IdentityKey) -> Result<Option<Uuid>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let result = app::query_output(
        cells
            .query::<LookupAccounts>(Address::Directory, None, vec![key])
            .await,
    )?;
    match result.as_slice() {
        [id] => Ok(*id),
        _ => Err(Error::CellContract(
            "Account lookup returned an invalid result count",
        )),
    }
}

pub(crate) async fn organization_role(
    server: &Server,
    organization: Uuid,
    actor: Uuid,
) -> Result<Option<Role>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    match cells
        .query::<GetOrganization>(Address::Organization(organization), None, Caller { actor })
        .await
    {
        Ok(result) => Ok(result.output.map(|value| value.role)),
        Err(crate::cells::Error::NotInitialized | crate::cells::Error::AccessDenied) => Ok(None),
        Err(error) => Err(Error::Cell(error)),
    }
}

pub(crate) fn organization_access(
    grant: &ActorTeams,
    teams: &[TeamGrant],
) -> Option<RepositoryAccess> {
    if grant.owner {
        return Some(RepositoryAccess::Admin);
    }
    teams
        .iter()
        .filter(|team| grant.teams.binary_search(&team.team).is_ok())
        .map(|team| team.access)
        .max()
}

pub(crate) async fn organization_grants(
    server: &Server,
    principal: &Principal,
    organizations: HashMap<Uuid, bool>,
) -> Result<HashMap<Uuid, ActorTeams>> {
    if organizations.is_empty() {
        return Ok(HashMap::new());
    }
    let identity = principal.identity().ok_or(Error::NotFound)?;
    let key = IdentityKey {
        issuer: identity.issuer,
        subject: identity.subject,
    };
    let Some(actor) = actor_id(server, key).await? else {
        return Ok(HashMap::new());
    };
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    futures_util::stream::iter(
        organizations
            .into_iter()
            .map(|(organization, include_teams)| {
                let cells = cells.clone();
                async move {
                    let result = cells
                        .query::<RepositoryTeamsForActor>(
                            Address::Organization(organization),
                            None,
                            RepositoryActor {
                                actor,
                                include_teams,
                            },
                        )
                        .await;
                    let grant = match result {
                        Ok(result) => result.output,
                        Err(
                            crate::cells::Error::NotInitialized | crate::cells::Error::AccessDenied,
                        ) => ActorTeams::default(),
                        Err(error) => return Err(Error::Cell(error)),
                    };
                    Ok((organization, grant))
                }
            }),
    )
    .buffer_unordered(8)
    .try_collect()
    .await
}

pub(crate) async fn visible_team_names(
    server: &Server,
    principal: &Principal,
    organization: Uuid,
    teams: Vec<Uuid>,
) -> Result<VisibleRepositoryTeams> {
    let key = app::account(principal)?;
    let Some(actor) = actor_id(server, key).await? else {
        return Ok(VisibleRepositoryTeams::default());
    };
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    app::query_output(
        cells
            .query::<RepositoryTeamNames>(
                Address::Organization(organization),
                None,
                RepositoryTeamQuery { actor, teams },
            )
            .await,
    )
}

pub(crate) async fn organization_members(
    server: &Server,
    organization: Uuid,
    teams: &[TeamGrant],
    principal: Option<&Principal>,
) -> Result<Vec<RepositoryMember>> {
    let issuer = server.auth.as_ref().ok_or(Error::NotFound)?.issuer();
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let roster = app::query_output(
        cells
            .query::<RepositoryTeamRoster>(
                Address::Organization(organization),
                None,
                teams.iter().map(|grant| grant.team).collect(),
            )
            .await,
    )?;
    let mut grants = BTreeMap::new();
    for (team, actor) in roster.members {
        let access = teams
            .iter()
            .find(|grant| grant.team == team)
            .ok_or(Error::CellContract(
                "Team roster returned an unrelated team",
            ))?
            .access;
        grants
            .entry(actor)
            .and_modify(|current: &mut RepositoryAccess| *current = (*current).max(access))
            .or_insert(access);
    }
    for owner in roster.owners {
        grants.insert(owner, RepositoryAccess::Admin);
    }

    let ids: Vec<_> = grants.keys().copied().collect();
    let current = principal.and_then(Principal::identity);
    let mut members = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(100) {
        let identities = app::query_output(
            cells
                .query::<LookupIdentities>(Address::Directory, None, chunk.to_vec())
                .await,
        )?;
        if identities.len() != chunk.len() || identities.iter().any(Option::is_none) {
            return Err(Error::CellContract(
                "Organization member is missing a Directory account",
            ));
        }
        members.extend(
            chunk
                .iter()
                .zip(identities.into_iter().flatten())
                .filter(|(_, key)| key.issuer == issuer)
                .map(|(id, key)| RepositoryMember {
                    name: current
                        .as_ref()
                        .filter(|actor| actor.issuer == issuer && actor.subject == key.subject)
                        .map_or_else(|| "Organization member".into(), |actor| actor.name.clone()),
                    subject: key.subject,
                    access: grants[id],
                }),
        );
    }
    Ok(members)
}

pub(crate) async fn team_review_plan(
    server: &Server,
    membership: &app::Membership,
    team: Uuid,
    eligibility: Option<&MergeReviewers>,
) -> Result<TeamRecipients> {
    use crate::cells::repository::review_requests::teams::MAX_REVIEW_RECIPIENTS;

    let issuer = server.auth.as_ref().ok_or(Error::NotFound)?.issuer();
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let roster = app::query_output(
        cells
            .query::<ReviewRoster>(
                Address::Organization(membership.organization.ok_or(Error::NotFound)?),
                None,
                team,
            )
            .await,
    )?;
    if roster.members.len() > MAX_REVIEW_RECIPIENTS {
        return Err(Error::Invalid(
            "Team review requests support up to 256 team members",
        ));
    }
    let mut recipients = Vec::with_capacity(roster.members.len());
    for chunk in roster.members.chunks(100) {
        let identities = app::query_output(
            cells
                .query::<LookupIdentities>(Address::Directory, None, chunk.to_vec())
                .await,
        )?;
        if identities.len() != chunk.len() || identities.iter().any(Option::is_none) {
            return Err(Error::CellContract(
                "Team member is missing a Directory account",
            ));
        }
        recipients.extend(
            identities
                .into_iter()
                .flatten()
                .filter_map(|key| (key.issuer == issuer).then_some(key.subject)),
        );
    }
    let routing = if roster.settings.algorithm == Algorithm::Disabled {
        None
    } else {
        let current;
        let eligibility = match eligibility {
            Some(eligibility) => eligibility,
            None => {
                current = membership.merge_reviewers(server).await?;
                &current
            }
        };
        let candidates = recipients
            .iter()
            .filter(|subject| eligibility.contains(issuer, subject))
            .map(|subject| RepositoryAuthor {
                issuer: issuer.to_owned(),
                subject: subject.clone(),
                name: membership
                    .members
                    .iter()
                    .find(|member| member.subject == *subject)
                    .map_or_else(
                        || "Organization member".into(),
                        |member| member.name.clone(),
                    ),
            })
            .collect();
        recipients.clear();
        Some(routing::Plan {
            algorithm: roster.settings.algorithm,
            reviewers: roster.settings.reviewers,
            candidates,
        })
    };
    Ok(TeamRecipients {
        team,
        recipients,
        routing,
    })
}
