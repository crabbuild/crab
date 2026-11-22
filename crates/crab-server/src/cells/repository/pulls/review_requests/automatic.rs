use std::collections::BTreeSet;

use uuid::Uuid;

use super::*;
use crate::cells::{Error, Result};

pub(crate) const MAX_RECIPIENTS: usize = 1_024;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    pub people: Vec<RepositoryAuthor>,
    pub teams: Vec<TeamRecipients>,
}

impl Plan {
    pub(crate) fn is_empty(&self) -> bool {
        self.people.is_empty() && self.teams.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TeamRecipients {
    pub team: Uuid,
    pub recipients: Vec<String>,
}

pub(in crate::cells::repository::pulls) fn apply(
    context: &Transaction<'_>,
    pull: &PullRecord,
    actor: &RepositoryAuthor,
    plan: Plan,
) -> Result<()> {
    if plan.is_empty() {
        return Ok(());
    }
    if plan.people.len() > MAX_RECIPIENTS || plan.teams.len() > crate::catalog::MAX_REPOSITORY_TEAMS
    {
        return Err(Error::Invalid(
            "automatic code owner requests exceed the request limit",
        ));
    }
    let mut notified = BTreeSet::new();
    let mut requested = false;
    let now = pull.updated_at_ms;
    super::super::super::search::index::ensure_ready(context)?;
    for reviewer in plan.people {
        validate_author(&reviewer)?;
        if reviewer.issuer != actor.issuer
            || same_author(&reviewer, &pull.author)
            || load(context, pull.number, &reviewer_id(&reviewer))?.is_some()
        {
            continue;
        }
        notified.insert(reviewer.subject.clone());
        project(
            context,
            pull.number,
            &Request {
                reviewer,
                requested_by: actor.clone(),
                code_owner: true,
                state: State::Requested,
                requested_at: now,
                updated_at: now,
            },
        )?;
        requested = true;
    }
    if !plan.teams.is_empty() {
        let mut current = teams::load_teams(context, pull.number)?;
        let before = current.len();
        for team in plan.teams {
            if team.team.is_nil() || team.recipients.len() > teams::MAX_REVIEW_RECIPIENTS {
                return Err(Error::Invalid("invalid automatic team review request"));
            }
            let key = teams::requested_key(pull.number, team.team);
            if current.iter().any(|item| item.team == team.team)
                || context.document::<bool>(&key)?.is_some()
            {
                continue;
            }
            if current.len() >= crate::catalog::MAX_REPOSITORY_TEAMS {
                return Err(Error::Invalid(
                    "pull requests support up to 100 team review requests",
                ));
            }
            context.put_document(&key, &true)?;
            current.push(teams::TeamRequest {
                team: team.team,
                state: State::Requested,
                code_owner: true,
            });
            notified.extend(team.recipients);
        }
        if current.len() != before {
            context.put_document(&teams::key(pull.number), &current)?;
            requested = true;
        }
    }
    if !requested {
        return Ok(());
    }
    notified.remove(&actor.subject);
    if actor.issuer == pull.author.issuer {
        notified.remove(&pull.author.subject);
    }
    if notified.len() > MAX_RECIPIENTS {
        return Err(Error::Invalid(
            "automatic code owner requests exceed the recipient limit",
        ));
    }
    let event = activity::append(
        context,
        Target::Pull {
            number: pull.number,
        },
        actor,
        Action::CodeOwnerReviewsRequested,
    )?;
    for subject in notified {
        super::super::super::collaboration::notifications::review_requested(
            context,
            &actor.issuer,
            &subject,
            &event,
        )?;
    }
    Ok(())
}
