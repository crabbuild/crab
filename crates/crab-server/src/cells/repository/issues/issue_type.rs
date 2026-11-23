use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::*;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Selection {
    pub id: Uuid,
    pub organization: Uuid,
    pub ownership_epoch: u32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Assignment {
    pub version: u64,
    pub selection: Option<Selection>,
}

fn key(number: u64) -> String {
    format!("issues/v1/types/{number:016x}")
}

pub(super) fn load(context: &Transaction<'_>, number: u64) -> crate::cells::Result<Assignment> {
    Ok(context.document(&key(number))?.unwrap_or_default())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetInput {
    pub number: u64,
    pub expected_version: u64,
    pub selection: Option<Selection>,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Outcome {
    Saved(Assignment),
    Conflict,
    Archived,
    NotFound,
}

pub(crate) struct Set;
impl Mutation for Set {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 291;
    type Input = SetInput;
    type Output = Outcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: SetInput,
    ) -> crate::cells::Result<MutationResult<Outcome>> {
        if GetIssue::execute(context, input.number)?.is_none() {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        }
        let current = load(context, input.number)?;
        if current.selection == input.selection
            && (current.version == input.expected_version
                || current.version.checked_sub(1) == Some(input.expected_version))
        {
            return Ok(MutationResult::Success(Outcome::Saved(current)));
        }
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        if current.version != input.expected_version || current.version >= MAX_NUMBER {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        let next = Assignment {
            version: current.version + 1,
            selection: input.selection,
        };
        context.put_document(&key(input.number), &next)?;
        Ok(MutationResult::Success(Outcome::Saved(next)))
    }
}
