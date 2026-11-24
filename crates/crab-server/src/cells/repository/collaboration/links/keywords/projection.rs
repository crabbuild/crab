use super::*;
use crate::cells::{
    Result,
    repository::{GetPull, PullState, issues::transfer::publication::Destination},
};

const PREFIX: &str = "collaboration/v1/qualified-issue-references";
const QUEUE: &str = "collaboration/v1/qualified-issue-references/pending/";
const PROGRESS: &str = "collaboration/v1/qualified-issue-references/backfill";
const BACKFILL_BATCH: usize = 32;

fn queued(pull: u64) -> String {
    format!("{QUEUE}{pull:016x}")
}

fn published(pull: u64) -> String {
    format!("{PREFIX}/published/{pull:016x}")
}

pub(in crate::cells::repository) fn changed(
    context: &Transaction<'_>,
    pull: u64,
    previous: &str,
    body: &str,
) -> Result<()> {
    if qualified(previous) != qualified(body) {
        context.put_document(&queued(pull), &pull)?;
    }
    Ok(())
}

pub(in crate::cells::repository) fn record_publication(
    context: &Transaction<'_>,
    pull: u64,
    issues: &[Destination],
) -> Result<()> {
    context.put_document(&published(pull), &issues)?;
    context.put_document(&queued(pull), &pull)
}

#[derive(Deserialize, Serialize)]
pub(crate) enum References {
    Live {
        base_ref: String,
        targets: Vec<Qualified>,
    },
    Published(Vec<Destination>),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version {
    pub pull: u64,
    pub published: bool,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Snapshot {
    pub version: Version,
    pub references: References,
}

pub(crate) struct View;
impl Read for View {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 470;
    type Input = u64;
    type Output = Option<Snapshot>;

    fn execute(context: &mut Transaction<'_>, number: u64) -> Result<Self::Output> {
        let Some(pull) = GetPull::execute(context, number)? else {
            return Ok(None);
        };
        let published = context.document::<Vec<Destination>>(&published(number))?;
        let version = Version {
            pull: pull.version,
            published: published.is_some(),
        };
        let references = match published {
            Some(targets) => References::Published(targets),
            None if pull.state == PullState::Merged => References::Published(Vec::new()),
            None => References::Live {
                base_ref: pull.base_ref,
                targets: qualified(&pull.body),
            },
        };
        Ok(Some(Snapshot {
            version,
            references,
        }))
    }
}

pub(crate) struct Pending;
impl Read for Pending {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 471;
    type Input = ();
    type Output = (bool, Vec<u64>);

    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<Self::Output> {
        let ready = matches!(context.document(PROGRESS)?, Some(Scan::Ready));
        let pending = context
            .documents_after::<u64>(QUEUE, None, 8)?
            .into_iter()
            .map(|(_, pull)| pull)
            .collect();
        Ok((ready, pending))
    }
}

pub(crate) struct Acknowledge;
impl Mutation for Acknowledge {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 470;
    type Input = (u64, Version);
    type Output = ();

    fn execute(
        context: &mut Transaction<'_>,
        (pull, version): Self::Input,
    ) -> Result<MutationResult<()>> {
        if View::execute(context, pull)?.is_some_and(|view| view.version == version)
            && context.delete_document(&queued(pull))?
        {
            return Ok(MutationResult::Success(()));
        }
        Ok(MutationResult::Rejected(()))
    }
}

#[derive(Deserialize, Serialize)]
enum Scan {
    After(u64),
    Ready,
}

pub(crate) struct Backfill;
impl Mutation for Backfill {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 471;
    type Input = ();
    type Output = ();

    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<MutationResult<()>> {
        let after = match context.document(PROGRESS)? {
            Some(Scan::Ready) => return Ok(MutationResult::Rejected(())),
            Some(Scan::After(number)) => number,
            None => 0,
        };
        let rows = context.query(
            "SELECT number, body FROM repository_pulls WHERE number > ? ORDER BY number LIMIT 32",
            &[integer(after)?],
        )?.rows;
        let mut last = after;
        for row in &rows {
            let [_, SqlValue::Text(body)] = row.as_slice() else {
                return Err(crate::cells::Error::Invalid(
                    "invalid pull description in reference backfill",
                ));
            };
            last = result_u64_from_row(row, 0)?;
            changed(context, last, "", body)?;
        }
        let progress = if rows.len() == BACKFILL_BATCH {
            Scan::After(last)
        } else {
            Scan::Ready
        };
        context.put_document(PROGRESS, &progress)?;
        Ok(MutationResult::Success(()))
    }
}
