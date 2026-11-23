use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::rulesets::{
        BypassActor, Enforcement, GetRulesets, ReplaceRulesets, ReplaceRulesetsInput,
        ReplaceRulesetsOutcome, Rulesets,
    },
    repository::settings::{RulesetScope, with_ruleset_lease},
    server::Server,
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route("/api/repos/{owner}/{name}/rulesets", get(read).put(replace))
}

#[derive(Serialize)]
struct Inherited {
    organization: Uuid,
    rulesets: Vec<crate::cells::repository::rulesets::Ruleset>,
}

async fn inherited(
    server: &Server,
    repo: &crate::repository::Repository,
) -> Result<Option<Inherited>> {
    let Some(organization) = repo.organization else {
        return Ok(None);
    };
    let mut inherited = crate::repository::settings::inherited_rulesets(server, repo).await?;
    for rule in &mut inherited.rulesets {
        rule.bypass_actors.clear();
    }
    Ok(Some(Inherited {
        organization,
        rulesets: inherited.rulesets,
    }))
}

#[derive(Serialize)]
struct View {
    #[serde(flatten)]
    settings: Rulesets,
    can_manage: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    inherited: Option<Inherited>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bypass_options: Option<BypassOptions>,
}

#[derive(Default, Serialize)]
struct BypassOptions {
    organization_owner: bool,
    teams: Vec<TeamOption>,
}

#[derive(Serialize)]
struct TeamOption {
    id: Uuid,
    name: String,
}

async fn bypass_options(server: &Server, grant: &app::Membership) -> Result<BypassOptions> {
    use crate::cells::{
        Address,
        organization::teams::repository_access::{CodeOwnerTeamQuery, CodeOwnerTeams},
    };
    let Some(organization) = grant.organization else {
        return Ok(BypassOptions::default());
    };
    let teams = if grant.teams.is_empty() {
        Vec::new()
    } else {
        let cells = server.cells().ok_or(Error::CellUnavailable)?;
        app::query_output(
            cells
                .query::<CodeOwnerTeams>(
                    Address::Organization(organization),
                    None,
                    CodeOwnerTeamQuery {
                        teams: grant.teams.iter().map(|grant| grant.team).collect(),
                        approvers: Vec::new(),
                    },
                )
                .await,
        )?
        .into_iter()
        .map(|team| TeamOption {
            id: team.id,
            name: team.name,
        })
        .collect()
    };
    Ok(BypassOptions {
        organization_owner: true,
        teams,
    })
}

async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> Result<Json<View>> {
    let repo = app::repository(&server, &principal, &key)?;
    let grant = app::membership(&server, &principal, &repo).await?;
    let can_manage = grant.principal.can_admin(repo.id);
    let routed = app::repository_cell(&server, &repo)?;
    let mut settings = app::query_output(
        routed
            .client
            .query::<GetRulesets>(&routed.target, None, ())
            .await,
    )?;
    let bypass_options = if can_manage {
        Some(bypass_options(&server, &grant).await?)
    } else {
        settings
            .rulesets
            .retain(|rule| rule.enforcement == Enforcement::Active);
        for rule in &mut settings.rulesets {
            rule.bypass_actors.clear();
        }
        None
    };
    Ok(Json(View {
        settings,
        can_manage,
        inherited: inherited(&server, &repo).await?,
        bypass_options,
    }))
}

async fn replace(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Json(input): Json<ReplaceRulesetsInput>,
) -> Result<Json<View>> {
    let repo = app::repository(&server, &principal, &key)?;
    let grant = app::membership(&server, &principal, &repo).await?;
    if !grant.principal.can_admin(repo.id) {
        return Err(Error::Forbidden);
    }
    if !input.valid() {
        return Err(Error::Invalid(
            "Choose unique ruleset names, valid ref patterns and at least one rule per ruleset",
        ));
    }
    let options = bypass_options(&server, &grant).await?;
    let invalid = input
        .rulesets
        .iter()
        .flat_map(|rule| &rule.bypass_actors)
        .any(|grant| match grant.actor {
            BypassActor::OrganizationOwner => !options.organization_owner,
            BypassActor::Team { id } => !options.teams.iter().any(|team| team.id == id),
            _ => false,
        });
    if invalid {
        return Err(Error::Invalid(
            "Choose a visible team with repository access for ruleset bypass",
        ));
    }
    let inherited = inherited(&server, &repo).await?;
    let (send, reply) = tokio::sync::oneshot::channel();
    let tracker = server.receives.clone();
    tracker.spawn(async move {
        let server = server.as_ref();
        let repo = repo.as_ref();
        let outcome = with_ruleset_lease(
            server,
            RulesetScope::Repository(repo),
            &server.cancellation,
            |cancel| async move {
                let grant = app::membership(server, &principal, repo).await?;
                if !grant.principal.can_admin(repo.id) {
                    return Err(Error::Forbidden);
                }
                if cancel.is_cancelled() {
                    return Err(Error::MergeBusy);
                }
                let routed = app::repository_cell(server, repo)?;
                app::command_output(
                    routed
                        .client
                        .command::<ReplaceRulesets>(
                            &routed.target,
                            app::mutation_identity()?,
                            input,
                        )
                        .await,
                )
            },
        )
        .await;
        let _ = send.send(outcome);
    });
    match reply.await.map_err(|_| Error::MergeBusy)?? {
        ReplaceRulesetsOutcome::Saved(settings) => Ok(Json(View {
            settings,
            can_manage: true,
            inherited,
            bypass_options: Some(options),
        })),
        ReplaceRulesetsOutcome::Conflict => Err(Error::Conflict),
        ReplaceRulesetsOutcome::Archived => Err(Error::Archived),
    }
}

#[cfg(test)]
mod tests;
