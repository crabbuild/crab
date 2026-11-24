use serde::{Deserialize, Serialize};

use super::{
    PREFIX, Target,
    activity::{self, Action},
    next_version,
};
use crate::cells::{
    Result,
    application::{Mutation, MutationResult, Transaction},
    repository::{CLASS, RepositoryAuthor, advance_revision, settings, validate_author},
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    Abuse,
    Duplicate,
    LowQuality,
    OffTopic,
    Outdated,
    Resolved,
    Spam,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    pub version: u64,
    pub reason: Option<Reason>,
}

pub(crate) fn supports(target: Target) -> bool {
    !matches!(target, Target::Issue { .. } | Target::Pull { .. })
}

fn key(target: Target) -> String {
    format!("{PREFIX}/minimization/{}", target.key())
}

pub(super) fn load(context: &Transaction<'_>, target: Target) -> Result<Option<State>> {
    if !supports(target) {
        return Ok(None);
    }
    context.document(&key(target))
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub target: Target,
    pub actor: RepositoryAuthor,
    pub version: u64,
    pub reason: Option<Reason>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum Outcome {
    Saved(State),
    Archived,
    NotFound,
    Conflict,
}

pub(crate) struct Set;
impl Mutation for Set {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 312;
    type Input = Input;
    type Output = Outcome;

    fn execute(context: &mut Transaction<'_>, input: Input) -> Result<MutationResult<Outcome>> {
        validate_author(&input.actor)?;
        if !supports(input.target) || !input.target.exists(context)? {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        }
        let current = load(context, input.target)?.unwrap_or_default();
        // A reply lost after publication can recover the same state and event.
        if current.reason == input.reason
            && (input.version == current.version
                || input.version.checked_add(1) == Some(current.version))
        {
            return Ok(MutationResult::Success(Outcome::Saved(current)));
        }
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        if input.version != current.version {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        let saved = State {
            version: next_version(current.version)?,
            reason: input.reason,
        };
        // Retain the restored version so a stale hide cannot restore a reason.
        context.put_document(&key(input.target), &saved)?;
        let action = match saved.reason {
            Some(reason) => Action::CommentHidden { reason },
            None => Action::CommentRestored,
        };
        activity::append(context, input.target, &input.actor, action)?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Outcome::Saved(saved)))
    }
}
