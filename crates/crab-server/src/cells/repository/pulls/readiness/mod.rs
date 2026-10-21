use super::*;
use crate::cells::repository::collaboration::{
    Target,
    activity::{self, Action},
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PullReadiness {
    Draft,
    #[default]
    Ready,
}

fn key(number: u64) -> String {
    format!("collaboration/v1/pulls/{number:016x}/readiness")
}

pub(super) fn load(
    context: &RepositoryTransaction<'_>,
    number: u64,
) -> crate::cells::Result<PullReadiness> {
    // Absence is the normal ready state. Reading an existing PR must not
    // manufacture new metadata or historical readiness transitions.
    Ok(context.document(&key(number))?.unwrap_or_default())
}

pub(super) fn store(
    context: &RepositoryTransaction<'_>,
    number: u64,
    state: PullReadiness,
) -> crate::cells::Result<()> {
    context.put_document(&key(number), &state)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetPullReadinessInput {
    pub number: u64,
    pub actor: RepositoryAuthor,
    pub can_manage: bool,
    pub version: u64,
    pub state: PullReadiness,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum SetPullReadinessOutcome {
    Applied(Box<PullRecord>),
    Archived,
    NotFound,
    Forbidden,
    Closed,
    MergePending,
    Conflict,
}

pub(crate) struct SetPullReadiness;
impl Mutation for SetPullReadiness {
    const ID: u32 = 240;
    type Input = SetPullReadinessInput;
    type Output = SetPullReadinessOutcome;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        use SetPullReadinessOutcome as Outcome;
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        validate_number(input.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        let Some(mut pull) = load_pull(context, input.number)? else {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        };
        if !(input.can_manage || same_author(&input.actor, &pull.author)) {
            return Ok(MutationResult::Rejected(Outcome::Forbidden));
        }
        if pull.state != PullState::Open {
            return Ok(MutationResult::Rejected(Outcome::Closed));
        }
        if pull.merge_pending.is_some() {
            return Ok(MutationResult::Rejected(Outcome::MergePending));
        }
        let draft = input.state == PullReadiness::Draft;
        if pull.draft == draft {
            return Ok(MutationResult::Success(Outcome::Applied(Box::new(pull))));
        }
        if input.version != pull.version {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        let old_version = pull.version;
        pull.draft = draft;
        pull.version = next_version(pull.version)?;
        pull.updated_at_ms = timestamp(context.now_ms())?;
        // The same PR version orders edits, readiness, and merge admission.
        // A conversion cannot race a stale merge into publishing a draft.
        store(context, pull.number, input.state)?;
        update_pull_row(context, &pull, old_version)?;
        let action = match input.state {
            PullReadiness::Draft => Action::Drafted,
            PullReadiness::Ready => Action::ReadyForReview,
        };
        activity::append(
            context,
            Target::Pull {
                number: pull.number,
            },
            &input.actor,
            action,
        )?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Outcome::Applied(Box::new(pull))))
    }
}

#[cfg(test)]
mod tests;
