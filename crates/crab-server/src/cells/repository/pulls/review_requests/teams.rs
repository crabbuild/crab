use super::*;
use crate::{
    catalog::MAX_TEAM_GRANTS,
    cells::{Error, Result},
};
use uuid::Uuid;

pub(crate) const MAX_REVIEW_RECIPIENTS: usize = 256;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TeamRequest {
    pub team: Uuid,
    pub state: State,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub code_owner: bool,
}

pub(super) fn key(pull: u64) -> String {
    format!("collaboration/v1/pulls/{pull:016x}/team-review-requests")
}

pub(super) fn requested_key(pull: u64, team: Uuid) -> String {
    format!("collaboration/v1/pulls/{pull:016x}/requested-teams/{team}")
}

pub(in crate::cells::repository) fn load_teams(
    context: &Transaction<'_>,
    pull: u64,
) -> Result<Vec<TeamRequest>> {
    Ok(context.document(&key(pull))?.unwrap_or_default())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TeamSelection {
    pub pull: u64,
    pub version: u64,
    pub actor: RepositoryAuthor,
    pub team: Uuid,
    pub recipients: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<routing::Plan>,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum TeamOutcome {
    Applied,
    Archived,
    NotFound,
    Closed,
    MergePending,
    Conflict,
    Limit,
}

pub(crate) struct SetTeamReviewRequest;
impl Mutation for SetTeamReviewRequest {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 258;
    type Input = TeamSelection;
    type Output = TeamOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        use TeamOutcome::*;
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Archived));
        }
        validate_number(input.pull)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        if input.team.is_nil() {
            return Err(Error::Invalid("invalid reviewer team"));
        }
        let Some(mut pull) = load_pull(context, input.pull)? else {
            return Ok(MutationResult::Rejected(NotFound));
        };
        let receipt = format!(
            "collaboration/v1/team-review-submissions/{:016x}/{}/{:016x}/{}",
            input.pull,
            input.team,
            input.version,
            reviewer_id(&input.actor),
        );
        if input.recipients.is_some() && context.document::<bool>(&receipt)?.is_some() {
            return Ok(MutationResult::Success(Applied));
        }
        if pull.state != PullState::Open {
            return Ok(MutationResult::Rejected(Closed));
        }
        if pull.merge_pending.is_some() {
            return Ok(MutationResult::Rejected(MergePending));
        }
        let mut requests = load_teams(context, pull.number)?;
        let existing = requests.iter().find(|entry| entry.team == input.team);
        let requested = input.recipients.is_some();
        if (requested && existing.is_some_and(|entry| entry.state == State::Requested))
            || (!requested && existing.is_none())
        {
            return Ok(MutationResult::Success(Applied));
        }
        if pull.version != input.version {
            return Ok(MutationResult::Rejected(Conflict));
        }
        let mut recipients = input.recipients;
        if recipients
            .as_ref()
            .is_some_and(|items| items.len() > MAX_REVIEW_RECIPIENTS)
        {
            return Err(Error::Invalid("too many team review recipients"));
        }
        let (assigned, retain_team) = match (requested, input.routing) {
            (true, Some(plan)) => {
                let keep_team = plan.keep_team;
                let selected =
                    routing::assign(context, &pull, &input.actor, input.team, plan, false)?;
                let assigned = !selected.is_empty();
                // An enabled team with no available reviewers remains visible
                // as requested, without notifying members who cannot take the request.
                recipients = Some(selected);
                (assigned, keep_team || !assigned)
            }
            _ => (false, requested),
        };
        requests.retain(|entry| entry.team != input.team);
        if assigned {
            context.put_document(&receipt, &true)?;
        }
        if retain_team {
            if requests.len() >= MAX_TEAM_GRANTS {
                return Ok(MutationResult::Rejected(Limit));
            }
            requests.push(TeamRequest {
                team: input.team,
                state: State::Requested,
                code_owner: false,
            });
            context.put_document(&requested_key(pull.number, input.team), &true)?;
        }
        context.put_document(&key(pull.number), &requests)?;
        let old_version = pull.version;
        pull.version = next_version(pull.version)?;
        pull.updated_at_ms = timestamp(context.now_ms())?;
        update_pull_row(context, &pull, old_version)?;

        // Activity and inbox events omit team identity because their audiences
        // may differ from the team's current visibility.
        let action = if requested {
            Action::TeamReviewRequested
        } else {
            Action::TeamReviewRequestCancelled
        };
        let event = activity::append(
            context,
            Target::Pull {
                number: pull.number,
            },
            &input.actor,
            action,
        )?;
        if let Some(mut recipients) = recipients {
            recipients.sort_unstable();
            recipients.dedup();
            let issuer = &input.actor.issuer;
            crate::cells::repository::collaboration::mentions::review_requested(
                context,
                &event,
                issuer,
                recipients.into_iter().filter(|subject| {
                    pull.author.issuer != *issuer || pull.author.subject != *subject
                }),
            )?;
        }
        advance_revision(context)?;
        Ok(MutationResult::Success(Applied))
    }
}

pub(in crate::cells::repository::pulls) fn fulfill_teams(
    context: &Transaction<'_>,
    pull: &PullRecord,
    review: &PullReviewRecord,
    teams: &[Uuid],
) -> Result<bool> {
    if teams.is_empty() || same_author(&review.author, &pull.author) {
        return Ok(false);
    }
    if teams.len() > MAX_TEAM_GRANTS || teams.iter().any(Uuid::is_nil) {
        return Err(Error::Invalid("invalid review team membership"));
    }
    let mut requests = load_teams(context, pull.number)?;
    let mut changed = false;
    for request in &mut requests {
        if request.state == State::Requested && teams.contains(&request.team) {
            request.state = State::Fulfilled {
                review: review.number,
            };
            changed = true;
        }
    }
    if changed {
        context.put_document(&key(pull.number), &requests)?;
    }
    Ok(changed)
}
