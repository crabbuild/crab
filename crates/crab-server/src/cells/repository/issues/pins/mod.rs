use super::super::*;
use super::lifecycle;
use crate::cells::repository::collaboration::{Target, activity};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const KEY: &str = "issues/v1/pins";
const MAX_PINS: usize = 3;

#[derive(Clone, Default, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u64,
    numbers: Vec<u64>,
}
fn valid(numbers: &[u64]) -> bool {
    numbers.len() <= MAX_PINS
        && numbers.iter().all(|n| validate_number(*n).is_ok())
        && numbers.iter().collect::<BTreeSet<_>>().len() == numbers.len()
}
fn load(context: &Transaction<'_>) -> crate::cells::Result<State> {
    let state = context.document::<State>(KEY)?.unwrap_or_default();
    if !valid(&state.numbers)
        || state.version > MAX_NUMBER
        || state.version == 0 && !state.numbers.is_empty()
    {
        return Err(crate::cells::Error::Invalid(
            "issue pin collection is invalid",
        ));
    }
    Ok(state)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PinnedIssue {
    pub number: u64,
    pub title: String,
    pub state: u8,
    pub state_reason: Option<lifecycle::Reason>,
    pub author: RepositoryAuthor,
    pub created_at: u64,
    pub comments: u64,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Page {
    pub version: u64,
    pub items: Vec<PinnedIssue>,
    pub archived: bool,
}
pub(crate) struct GetPins;
impl Read for GetPins {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 320;
    type Input = ();
    type Output = Page;
    fn execute(context: &mut Transaction<'_>, (): ()) -> crate::cells::Result<Page> {
        lifecycle::ensure_ready(context)?;
        search::index::ensure_ready(context)?;
        let state = load(context)?;
        let mut items = Vec::with_capacity(state.numbers.len());
        for number in state.numbers {
            // At most three indexed point reads. Titles/state stay canonical;
            // transactional index counters avoid scanning a large comment history.
            let rows = context.query("SELECT i.title, i.state, i.author_issuer, i.author_subject, i.author_name, i.created_at_ms, d.comments FROM repository_issues i JOIN discussion_index d ON d.kind=0 AND d.number=i.number WHERE i.number=?", &[integer(number)?])?;
            let row = rows.rows.first().ok_or(crate::cells::Error::Invalid(
                "pinned issue or its projection is missing",
            ))?;
            let state = u8::try_from(result_u64_from_row(row, 1)?)
                .ok()
                .filter(|value| *value <= 1)
                .ok_or(crate::cells::Error::Invalid(
                    "pinned issue state is invalid",
                ))?;
            let reason = lifecycle::get(context, number, state != 0)?.state_reason;
            items.push(PinnedIssue {
                number,
                title: result_text(row, 0)?,
                state,
                state_reason: reason,
                author: RepositoryAuthor {
                    issuer: result_text(row, 2)?,
                    subject: result_text(row, 3)?,
                    name: result_text(row, 4)?,
                },
                created_at: result_u64_from_row(row, 5)?,
                comments: result_u64_from_row(row, 6)?,
            });
        }
        Ok(Page {
            version: state.version,
            items,
            archived: settings::archived(context)?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub submission_id: [u8; 16],
    pub actor: RepositoryAuthor,
    pub can_write: bool,
    pub version: u64,
    pub numbers: Vec<u64>,
}
#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum Outcome {
    Applied(u64),
    Invalid,
    Limit,
    Archived,
    Forbidden,
    NotFound,
    Conflict,
    RequestConflict,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Submission {
    digest: String,
    version: u64,
}
pub(crate) struct SetPins;
impl Mutation for SetPins {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 320;
    type Input = Input;
    type Output = Outcome;
    fn execute(
        context: &mut Transaction<'_>,
        input: Input,
    ) -> crate::cells::Result<MutationResult<Outcome>> {
        let digest = blake3::hash(&serde_json::to_vec(&(
            &input.numbers,
            input.version,
            &input.actor.issuer,
            &input.actor.subject,
        ))?)
        .to_hex()
        .to_string();
        let key = format!(
            "{KEY}/submissions/{}",
            uuid::Uuid::from_bytes(input.submission_id)
        );
        // Recover an admitted intent before later versions, roles or archive.
        // HTTP still requires current read access to its original repository.
        if let Some(saved) = context.document::<Submission>(&key)? {
            return Ok(if saved.digest == digest {
                MutationResult::Success(Outcome::Applied(saved.version))
            } else {
                MutationResult::Rejected(Outcome::RequestConflict)
            });
        }
        if input.numbers.len() > MAX_PINS {
            return Ok(MutationResult::Rejected(Outcome::Limit));
        }
        if !valid(&input.numbers) || input.version > MAX_NUMBER {
            return Ok(MutationResult::Rejected(Outcome::Invalid));
        }
        validate_author(&input.actor)?;
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        if !input.can_write {
            return Ok(MutationResult::Rejected(Outcome::Forbidden));
        }
        let current = load(context)?;
        if current.version != input.version {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        for number in &input.numbers {
            if super::GetIssue::execute(context, *number)?.is_none() {
                return Ok(MutationResult::Rejected(Outcome::NotFound));
            }
        }
        let changed = current.numbers != input.numbers;
        let version = if changed {
            current
                .version
                .checked_add(1)
                .filter(|version| *version <= MAX_NUMBER)
                .ok_or(crate::cells::Error::Invalid(
                    "issue pin version is exhausted",
                ))?
        } else {
            current.version
        };
        if changed {
            // Collection order has its own version. Pinning does not overwrite
            // an issue edit or reserve its source-record version.
            context.put_document(
                KEY,
                &State {
                    version,
                    numbers: input.numbers.clone(),
                },
            )?;
            for (numbers, previous, action) in [
                (&current.numbers, &input.numbers, activity::Action::Unpinned),
                (&input.numbers, &current.numbers, activity::Action::Pinned),
            ] {
                for number in numbers.iter().filter(|number| !previous.contains(number)) {
                    activity::append(
                        context,
                        Target::Issue { number: *number },
                        &input.actor,
                        action.clone(),
                    )?;
                }
            }
            advance_revision(context)?;
        }
        context.put_document(&key, &Submission { digest, version })?;
        Ok(MutationResult::Success(Outcome::Applied(version)))
    }
}

#[cfg(test)]
mod tests;
