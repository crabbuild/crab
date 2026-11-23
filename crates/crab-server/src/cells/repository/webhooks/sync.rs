use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::cells::repository::pulls::storage::{head_repository, load_pull};

const PENDING: &str = "hooks/v1/synchronize/due/";
const ROUTE_REVISION: &str = "hooks/v1/synchronize/route-revision";
const PAGE_SIZE: u64 = 32;

fn route_key(number: u64) -> String {
    format!("hooks/v1/synchronize/pulls/{number:016x}")
}

fn route_revision(context: &Transaction<'_>) -> crate::cells::Result<u64> {
    Ok(context.document(ROUTE_REVISION)?.unwrap_or(0))
}

pub(in crate::cells::repository) fn route_changed(
    context: &Transaction<'_>,
    number: u64,
) -> crate::cells::Result<()> {
    let revision = next_version(route_revision(context)?)?;
    context.put_document(ROUTE_REVISION, &revision)?;
    context.put_document(&route_key(number), &revision)
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Reservation {
    pub plan: String,
    pub source: Option<Uuid>,
    pub destinations: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct Job {
    id: Uuid,
    plan: String,
    source: Option<Uuid>,
    subscription: Subscription,
    queued_at: u64,
    due: u64,
    route_revision: u64,
    cursor: u64,
    last_pull: u64,
}

impl Job {
    fn key(&self) -> String {
        format!("{PENDING}{:020}/{}", self.due, self.id)
    }
}

pub(crate) struct Reserve;
impl Mutation for Reserve {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 437;
    type Input = Reservation;
    type Output = ();

    fn execute(
        context: &mut Transaction<'_>,
        input: Reservation,
    ) -> crate::cells::Result<MutationResult<()>> {
        let mut receivers = subscriptions(context)?
            .into_iter()
            .filter(|(_, hook)| hook.pull_request && input.destinations.contains(&hook.destination))
            .peekable();
        if receivers.peek().is_none() {
            return Ok(MutationResult::Rejected(()));
        }
        if blake3::Hash::from_hex(&input.plan).is_err()
            || input.source.is_some_and(|id| id.is_nil())
        {
            return Err(crate::cells::Error::Invalid(
                "Invalid PR synchronization plan",
            ));
        }
        let source = input
            .source
            .map_or_else(|| "local".into(), |id| id.to_string());
        let recorded = format!("hooks/v1/synchronize/plans/{source}/{}", input.plan);
        if context.document::<bool>(&recorded)?.is_some() {
            return Ok(MutationResult::Rejected(()));
        }
        let rows = context.query(
            "SELECT COALESCE(max(number), 0) FROM discussion_index WHERE kind=1 AND state=?",
            &[integer(u64::from(PullState::Open.code()))?],
        )?;
        let last_pull = result_u64(std::slice::from_ref(&rows), 0, 0)?;
        if last_pull == 0 {
            return Ok(MutationResult::Rejected(()));
        }
        let now = timestamp(context.now_ms())?;
        let route_revision = route_revision(context)?;
        for (_, subscription) in receivers {
            let job = Job {
                id: Uuid::now_v7(),
                plan: input.plan.clone(),
                source: input.source,
                subscription,
                queued_at: now,
                due: now,
                route_revision,
                cursor: 0,
                last_pull,
            };
            context.put_document(&job.key(), &job)?;
        }
        context.put_document(&recorded, &true)?;
        Ok(MutationResult::Success(()))
    }
}

struct Page {
    cursor: u64,
    heads: Vec<(u64, String)>,
}

fn page(context: &Transaction<'_>, job: &Job, through: u64) -> crate::cells::Result<Page> {
    // Bound rows examined even when a repository has few open pulls. The
    // reservation freezes the high-water mark and invalidates rerouted pulls.
    let rows = context.query(
        "SELECT p.number, p.head_ref
           FROM discussion_index d INDEXED BY discussion_index_states
           JOIN repository_pulls p ON p.number=d.number
          WHERE d.kind=1 AND d.state=? AND p.state=d.state AND d.number>? AND d.number<=?
          ORDER BY d.number LIMIT ?",
        &[
            integer(u64::from(PullState::Open.code()))?,
            integer(job.cursor)?,
            integer(through)?,
            integer(PAGE_SIZE)?,
        ],
    )?;
    let cursor = rows
        .rows
        .last()
        .map(|row| result_u64_from_row(row, 0))
        .transpose()?
        .unwrap_or(through);
    let mut heads = Vec::new();
    for row in &rows.rows {
        let number = result_u64_from_row(row, 0)?;
        if head_repository(context, number)? == job.source
            && context.document::<u64>(&route_key(number))?.unwrap_or(0) <= job.route_revision
        {
            heads.push((number, result_text(row, 1)?));
        }
    }
    Ok(Page { cursor, heads })
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Pending {
    pub id: Uuid,
    pub due: u64,
    pub cursor: u64,
    pub through: u64,
    pub source: Option<Uuid>,
    pub plan: String,
    pub destination: String,
    pub heads: BTreeSet<String>,
}

pub(crate) struct Next;
impl Read for Next {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 437;
    type Input = ();
    type Output = Option<Pending>;

    fn execute(context: &mut Transaction<'_>, _: ()) -> crate::cells::Result<Self::Output> {
        let now = timestamp(context.now_ms())?;
        let Some((_, job)) = context
            .documents_after::<Job>(PENDING, None, 1)?
            .into_iter()
            .next()
            .filter(|(_, job)| job.due <= now)
        else {
            return Ok(None);
        };
        let page = page(context, &job, job.last_pull)?;
        let heads = page.heads.into_iter().map(|(_, head)| head).collect();
        Ok(Some(Pending {
            id: job.id,
            due: job.due,
            cursor: job.cursor,
            through: page.cursor,
            source: job.source,
            plan: job.plan,
            destination: job.subscription.destination,
            heads,
        }))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Resolution {
    Wait,
    Discard,
    Committed(BTreeMap<String, HeadChange>),
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Step {
    pub id: Uuid,
    pub due: u64,
    pub cursor: u64,
    pub through: u64,
    pub resolution: Resolution,
}

pub(crate) struct Advance;
impl Mutation for Advance {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 438;
    type Input = Step;
    type Output = ();

    fn execute(
        context: &mut Transaction<'_>,
        input: Step,
    ) -> crate::cells::Result<MutationResult<()>> {
        let key = format!("{PENDING}{:020}/{}", input.due, input.id);
        let Some(mut job) = context.document::<Job>(&key)?.filter(|job| {
            job.cursor == input.cursor
                && input.through > job.cursor
                && input.through <= job.last_pull
        }) else {
            return Ok(MutationResult::Rejected(()));
        };
        context.delete_document(&key)?;
        if !is_active(context, &job.subscription)? {
            return Ok(MutationResult::Success(()));
        }
        let now = timestamp(context.now_ms())?;
        match input.resolution {
            Resolution::Wait if now.saturating_sub(job.queued_at) <= 3_600_000 => {
                job.due = now.saturating_add(10_000);
                context.put_document(&job.key(), &job)?;
            }
            Resolution::Committed(changes) => {
                // The sender resolves the immutable receipt. Queueing the page
                // and moving its cursor share one transaction, including after
                // a lost response or a second worker reading the same page.
                let page = page(context, &job, input.through)?;
                for (number, head) in page.heads {
                    if let Some(change) = changes.get(&head)
                        && let Some(pull) = load_pull(context, number)?
                    {
                        enqueue_at(
                            context,
                            job.subscription.clone(),
                            pull_event(&pull, PullAction::Synchronize, Some(change.clone())),
                            job.queued_at,
                        )?;
                    }
                }
                if input.through < job.last_pull {
                    job.cursor = input.through;
                    job.due = now;
                    context.put_document(&job.key(), &job)?;
                }
            }
            Resolution::Wait | Resolution::Discard => {}
        }
        Ok(MutationResult::Success(()))
    }
}
