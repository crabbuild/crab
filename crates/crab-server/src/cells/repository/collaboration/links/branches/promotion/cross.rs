use super::*;
use crate::cells::repository::issues::transfer::publication::{self, Destination};

const CLASS: &str = crate::cells::repository::CLASS;
const PENDING: &str = "collaboration/v1/cross-branch-promotions/pull/";
const BACKFILL: &str = "collaboration/v1/cross-branch-promotions/backfill";
const BATCH: usize = 16;
pub(crate) const LIMIT: usize = MAX_CLOSING_ISSUES;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub issue: Destination,
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Batch {
    pub organization: Uuid,
    pub branch: Branch,
    pub issues: Vec<Candidate>,
}

fn key(pull: u64) -> String {
    format!("{PENDING}{pull:016x}")
}

pub(in crate::cells::repository) fn record(
    context: &Transaction<'_>,
    pull: &PullRecord,
    batch: Option<Batch>,
) -> Result<()> {
    let Some(batch) = batch else {
        return Ok(());
    };
    if batch.organization.is_nil()
        || batch.branch != head(context, pull)?
        || batch.issues.is_empty()
        || batch.issues.len() > LIMIT
    {
        return Err(Error::Invalid(
            "Invalid cross-repository development promotion",
        ));
    }
    for issue in &batch.issues {
        if issue.issue.repository.is_nil() {
            return Err(Error::Invalid("Invalid development issue repository"));
        }
        validate_number(issue.issue.number)?;
        validate_number(issue.version)?;
    }
    context.put_document(&key(pull.number), &batch)
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 472;
    type Input = u64;
    type Output = Option<Batch>;

    fn execute(context: &mut Transaction<'_>, pull: u64) -> Result<Self::Output> {
        context.document(&key(pull))
    }
}

pub(crate) struct Pending;
impl Read for Pending {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 474;
    type Input = ();
    type Output = Vec<u64>;

    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<Self::Output> {
        context
            .documents_after::<Batch>(PENDING, None, 8)?
            .into_iter()
            .map(|(key, _)| number(&key, PENDING))
            .collect()
    }
}

pub(crate) struct Complete;
impl Mutation for Complete {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 472;
    type Input = u64;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, pull: u64) -> Result<MutationResult<()>> {
        if context.delete_document(&key(pull))? {
            Ok(MutationResult::Success(()))
        } else {
            Ok(MutationResult::Rejected(()))
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) enum Position {
    #[default]
    Start,
    After(String),
    Done,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Page {
    pub from: Position,
    pub to: Position,
    pub branches: Vec<(Destination, Branch)>,
}

pub(crate) struct Backfill;
impl Read for Backfill {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 473;
    type Input = Uuid;
    type Output = Option<Page>;

    fn execute(context: &mut Transaction<'_>, organization: Uuid) -> Result<Self::Output> {
        let from = backfill_position(context, organization)?;
        let after = match &from {
            Position::Start => None,
            Position::After(key) => Some(key.as_str()),
            Position::Done => return Ok(None),
        };
        let rows = context.documents_after::<State>(STATES, after, BATCH)?;
        let repository = repository_id(context)?;
        let mut branches = Vec::new();
        for (key, state) in &rows {
            let number = number(key, STATES)?;
            let address =
                publication::origin(context, number)?.unwrap_or(Destination { repository, number });
            branches.extend(
                state
                    .branches
                    .iter()
                    .cloned()
                    .map(|branch| (address, branch)),
            );
        }
        let to = match rows.last().filter(|_| rows.len() == BATCH) {
            Some((last, _)) => Position::After(last.clone()),
            None => Position::Done,
        };
        Ok(Some(Page { from, to, branches }))
    }
}

fn backfill_position(context: &Transaction<'_>, organization: Uuid) -> Result<Position> {
    Ok(context
        .document::<(Uuid, Position)>(BACKFILL)?
        .filter(|(owner, _)| *owner == organization)
        .map(|(_, position)| position)
        .unwrap_or_default())
}

pub(crate) struct Advance;
impl Mutation for Advance {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 473;
    type Input = (Uuid, Position, Position);
    type Output = ();

    fn execute(
        context: &mut Transaction<'_>,
        (organization, from, to): Self::Input,
    ) -> Result<MutationResult<()>> {
        if backfill_position(context, organization)? != from {
            return Ok(MutationResult::Rejected(()));
        }
        context.put_document(BACKFILL, &(organization, to))?;
        Ok(MutationResult::Success(()))
    }
}
