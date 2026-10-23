use super::{
    Subject, SubjectKind, milestones,
    state::State,
    submissions::{self, Kind},
};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
    repository::{self, RepositoryAuthor, assignments::Assignee, issues::lifecycle::Reason, *},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

mod apply;

const MAX_ITEMS: usize = 100;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Versioned {
    pub subject: Subject,
    pub version: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selected {
    pub item: Versioned,
    pub milestone_version: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Action {
    State {
        state: State,
        reason: Option<Reason>,
    },
    Labels {
        add: Vec<u64>,
        remove: Vec<u64>,
    },
    Assignees {
        add: Vec<Uuid>,
        remove: Vec<Uuid>,
    },
    Milestone {
        number: Option<u64>,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub submission_id: [u8; 16],
    pub actor: RepositoryAuthor,
    pub can_triage: bool,
    pub items: Vec<Selected>,
    pub action: Action,
    // Current roster matches for additions only. They are eligibility/name
    // snapshots, excluded from intent identity so a committed retry survives removal.
    pub eligible: Vec<Assignee>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum Failure {
    Invalid,
    Archived,
    Forbidden,
    NotFound,
    Conflict,
    RequestConflict,
    InvalidState,
    MergePending,
    InvalidLabels,
    InvalidAssignees,
}
#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum Outcome<T> {
    Applied(T),
    Rejected(Failure),
}

fn valid_selection<'a>(items: impl Iterator<Item = &'a Versioned>) -> bool {
    let mut keys = BTreeSet::new();
    for item in items {
        if validate_number(item.subject.number).is_err()
            || validate_number(item.version).is_err()
            || !keys.insert(item.subject.key())
        {
            return false;
        }
    }
    !keys.is_empty() && keys.len() <= MAX_ITEMS
}
fn valid_delta<T: Ord>(add: &[T], remove: &[T], limit: usize) -> bool {
    (!add.is_empty() || !remove.is_empty())
        && add.len() <= limit
        && remove.len() <= limit
        && add.iter().chain(remove).collect::<BTreeSet<_>>().len() == add.len() + remove.len()
}
fn valid_action(action: &Action, items: &[Selected]) -> bool {
    match action {
        Action::State { state, reason } => {
            reason.is_none()
                || (*state == State::Closed
                    && matches!(reason, Some(Reason::Completed | Reason::NotPlanned))
                    && items
                        .iter()
                        .all(|item| item.item.subject.kind == SubjectKind::Issue))
        }
        Action::Labels { add, remove } => {
            valid_delta(add, remove, MAX_LABELS)
                && add
                    .iter()
                    .chain(remove)
                    .all(|id| validate_number(*id).is_ok())
        }
        Action::Assignees { add, remove } => {
            valid_delta(add, remove, 10) && add.iter().chain(remove).all(|id| !id.is_nil())
        }
        Action::Milestone { number } => number.is_none_or(|id| validate_number(id).is_ok()),
    }
}

pub(crate) struct Select;
impl Read for Select {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 310;
    type Input = Vec<Versioned>;
    type Output = Outcome<Vec<Selected>>;
    fn execute(context: &mut Transaction<'_>, items: Self::Input) -> Result<Self::Output> {
        if !valid_selection(items.iter()) {
            return Ok(Outcome::Rejected(Failure::Invalid));
        }
        let mut selected = Vec::with_capacity(items.len());
        for item in items {
            let Some(current) = apply::current(context, item.subject)? else {
                return Ok(Outcome::Rejected(Failure::NotFound));
            };
            if current.version != item.version {
                return Ok(Outcome::Rejected(Failure::Conflict));
            }
            let assignment = milestones::GetAssignment::execute(context, item.subject)?
                .ok_or(Error::Invalid("bulk selection assignment is missing"))?;
            selected.push(Selected {
                item,
                milestone_version: assignment.version,
            });
        }
        Ok(Outcome::Applied(selected))
    }
}

pub(crate) struct Edit;
impl Mutation for Edit {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 310;
    type Input = Input;
    type Output = Outcome<u16>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Input,
    ) -> Result<MutationResult<Self::Output>> {
        let digest = blake3::hash(&serde_json::to_vec(&(
            &input.items,
            &input.action,
            &input.actor.issuer,
            &input.actor.subject,
        ))?)
        .to_hex()
        .to_string();
        let key = format!(
            "{}{}",
            Kind::Bulk.prefix(),
            Uuid::from_bytes(input.submission_id)
        );
        Ok(
            match submissions::decide(context, Kind::Bulk, &key, digest, |context| {
                apply(context, input)
            })? {
                Some(outcome) => MutationResult::Success(outcome),
                None => MutationResult::Rejected(Outcome::Rejected(Failure::RequestConflict)),
            },
        )
    }
}
fn apply(context: &mut Transaction<'_>, input: Input) -> Result<MutationResult<Outcome<u16>>> {
    if !valid_selection(input.items.iter().map(|selected| &selected.item))
        || !valid_action(&input.action, &input.items)
    {
        return Ok(MutationResult::Rejected(Outcome::Rejected(
            Failure::Invalid,
        )));
    }
    validate_author(&input.actor)?;
    if settings::archived(context)? {
        return Ok(MutationResult::Rejected(Outcome::Rejected(
            Failure::Archived,
        )));
    }
    if !input.can_triage {
        return Ok(MutationResult::Rejected(Outcome::Rejected(
            Failure::Forbidden,
        )));
    }
    if let Some(failure) = apply::apply(context, &input)? {
        // The decision scope rolls every earlier item back before the
        // terminal rejection commits in the enclosing transaction.
        return Ok(MutationResult::Rejected(Outcome::Rejected(failure)));
    }
    let count = u16::try_from(input.items.len()).map_err(Error::sdk)?;
    Ok(MutationResult::Success(Outcome::Applied(count)))
}

#[cfg(test)]
mod tests;
