use super::*;
use uuid::Uuid;

pub(crate) const LIMIT: usize = 16;

#[derive(Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Dependent {
    pub number: u64,
    pub version: u64,
    pub head: String,
    pub fork: Option<Uuid>,
    pub draft: bool,
    pub merging: bool,
}

pub(crate) fn token(base: &str, dependents: &[Dependent]) -> crate::cells::Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(&(base, dependents))?)
        .to_hex()
        .to_string())
}

pub(crate) fn eligible(base: &str, dependents: &[Dependent]) -> bool {
    dependents.len() <= LIMIT
        && dependents
            .iter()
            .all(|child| !child.merging && (child.fork.is_some() || child.head != base))
}

fn dependents(context: &Transaction<'_>, reference: &str) -> crate::cells::Result<Vec<Dependent>> {
    search::index::ensure_ready(context)?;
    context
        .query(
            "SELECT p.number, p.version, p.head_ref, d.draft, p.pending_merge IS NOT NULL
         FROM discussion_index_pull_sources s JOIN discussion_index d ON d.rowid = s.document
         JOIN repository_pulls p ON p.number = d.number
         WHERE d.kind = 1 AND d.state = ? AND s.base_ref = ? ORDER BY p.number LIMIT ?",
            &[
                SqlValue::Integer(i64::from(PullState::Open.code())),
                SqlValue::Text(reference.to_owned()),
                integer(LIMIT as u64 + 1)?,
            ],
        )?
        .rows
        .iter()
        .map(|row| {
            let number = result_u64_from_row(row, 0)?;
            Ok(Dependent {
                number,
                version: result_u64_from_row(row, 1)?,
                head: result_text(row, 2)?,
                fork: storage::head_repository(context, number)?,
                draft: result_u64_from_row(row, 3)? != 0,
                merging: result_u64_from_row(row, 4)? != 0,
            })
        })
        .collect()
}

fn receipt(plan: &str) -> crate::cells::Result<String> {
    if plan.len() != 64 || !plan.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(crate::cells::Error::Invalid(
            "Invalid stacked branch publication plan",
        ));
    }
    Ok(format!("pulls/stacked-base/v1/{plan}"))
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Query {
    pub reference: String,
    pub plan: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct View {
    pub dependents: Vec<Dependent>,
    pub applied: Option<String>,
}

pub(crate) struct Inspect;
impl Read for Inspect {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 450;
    type Input = Query;
    type Output = View;

    fn execute(context: &mut Transaction<'_>, input: Query) -> crate::cells::Result<View> {
        let applied = if let Some(plan) = input.plan {
            context.document(&receipt(&plan)?)?
        } else {
            None
        };
        Ok(View {
            dependents: dependents(context, &input.reference)?,
            applied,
        })
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Batch {
    pub parent: u64,
    pub plan: String,
    pub token: String,
    pub changes: Vec<Input>,
}

pub(crate) struct Apply;
impl Mutation for Apply {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 450;
    type Input = Batch;
    type Output = bool;

    fn execute(
        context: &mut Transaction<'_>,
        input: Batch,
    ) -> crate::cells::Result<MutationResult<bool>> {
        let Some(parent) = load_pull(context, input.parent)?.filter(|pull| {
            pull.state == PullState::Merged
                && pull.head_repository.is_none()
                && pull.merge.is_some()
        }) else {
            return Ok(MutationResult::Rejected(false));
        };
        let key = receipt(&input.plan)?;
        let current = dependents(context, &parent.head_ref)?;
        if let Some(previous) = context.document::<String>(&key)? {
            return if previous == input.token && current.is_empty() {
                Ok(MutationResult::Success(true))
            } else {
                Ok(MutationResult::Rejected(false))
            };
        }
        if current.is_empty()
            || !eligible(&parent.base_ref, &current)
            || token(&parent.base_ref, &current)? != input.token
            || current.len() != input.changes.len()
        {
            return Ok(MutationResult::Rejected(false));
        }
        for (child, change) in current.iter().zip(input.changes) {
            if change.pull != child.number
                || change.change.version != child.version
                || change.change.base_ref != parent.base_ref
                || !matches!(
                    Retarget::execute(context, change)?,
                    MutationResult::Success(Outcome::Saved)
                )
            {
                // Returning rejection rolls back all earlier retargets in this batch.
                return Ok(MutationResult::Rejected(false));
            }
        }
        context.put_document(&key, &input.token)?;
        Ok(MutationResult::Success(true))
    }
}
