use std::collections::BTreeMap;

use axum::Json;
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    catalog::MAX_TEAM_GRANTS,
    cells::repository::review_requests::{
        State,
        teams::{SetTeamReviewRequest, TeamOutcome, TeamRequest, TeamSelection},
    },
    repository::{
        Repository,
        access::{team_review_recipients, visible_team_names},
    },
    server::Server,
};

#[derive(Serialize)]
pub(in crate::api::pulls) struct TeamView {
    id: Uuid,
    name: Option<String>,
    state: Option<State>,
    can_request: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    code_owner: bool,
}

pub(in crate::api::pulls) async fn views(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
    membership: &app::Membership,
    requests: Vec<TeamRequest>,
    can_manage: bool,
) -> Result<Vec<TeamView>> {
    let Some(organization) = membership.organization else {
        return Ok(Vec::new());
    };
    let mut teams = BTreeMap::new();
    if can_manage {
        for grant in &membership.teams {
            teams.insert(grant.team, (None, true));
        }
    }
    for request in requests {
        let team = request.team;
        teams.entry(team).or_default().0 = Some(request);
    }
    let ids: Vec<_> = teams.keys().copied().collect();
    let mut result = Vec::new();
    for chunk in ids.chunks(MAX_TEAM_GRANTS) {
        let visibility =
            visible_team_names(server, principal, organization, chunk.to_vec()).await?;
        let names: BTreeMap<_, _> = visibility.names.into_iter().collect();
        for id in chunk {
            let (request, can_request) = teams.remove(id).unwrap_or_default();
            let code_owner = request.as_ref().is_some_and(|request| request.code_owner);
            let state = request.map(|request| request.state);
            let name = names.get(id).cloned();
            if name.is_some() || (visibility.owner && state.is_some()) {
                result.push(TeamView {
                    id: *id,
                    state,
                    can_request: can_request && name.is_some(),
                    code_owner,
                    name,
                });
            }
        }
    }
    if !ids.is_empty() {
        app::membership(server, principal, repo).await?;
    }
    result.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    Ok(result)
}

pub(super) async fn set(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
    pull: u64,
    version: u64,
    team: Uuid,
    request: bool,
) -> Result<Json<Value>> {
    let organization = repo.organization.ok_or(Error::NotFound)?;
    let current = app::membership(server, principal, repo).await?;
    if !current.principal.can_triage(repo.id) {
        return Err(Error::Forbidden);
    }
    if team.is_nil() || (request && !current.teams.iter().any(|grant| grant.team == team)) {
        return Err(Error::NotFound);
    }
    let visible = visible_team_names(server, principal, organization, vec![team]).await?;
    if visible.names.is_empty() && (request || !visible.owner) {
        return Err(Error::NotFound);
    }
    let recipients = if request {
        Some(team_review_recipients(server, organization, team).await?)
    } else {
        None
    };
    let current = app::membership(server, principal, repo).await?;
    if !current.principal.can_triage(repo.id) {
        return Err(Error::Forbidden);
    }
    if request && !current.teams.iter().any(|grant| grant.team == team) {
        return Err(Error::NotFound);
    }
    let visible = visible_team_names(server, principal, organization, vec![team]).await?;
    if visible.names.is_empty() && (request || !visible.owner) {
        return Err(Error::NotFound);
    }
    let actor = app::actor(principal)?;
    let cell = app::repository_cell(server, repo)?;
    let result = app::command_output(
        cell.client
            .command::<SetTeamReviewRequest>(
                &cell.target,
                app::mutation_identity()?,
                TeamSelection {
                    pull,
                    version,
                    actor: (&actor).into(),
                    team,
                    recipients,
                },
            )
            .await,
    )?;
    match result {
        TeamOutcome::Applied => Ok(Json(json!({"team": team}))),
        TeamOutcome::Archived => Err(Error::Archived),
        TeamOutcome::NotFound => Err(Error::NotFound),
        TeamOutcome::Closed => Err(Error::Invalid(
            "Only open pull requests can request reviews",
        )),
        TeamOutcome::MergePending => Err(Error::MergePending),
        TeamOutcome::Conflict => Err(Error::Conflict),
        TeamOutcome::Limit => Err(Error::Invalid(
            "Pull requests support up to 100 team review requests",
        )),
    }
}
