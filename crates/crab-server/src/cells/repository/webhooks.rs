use super::{collaboration::next_version, pulls::PullRecord, *};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) mod sync;

const ACTIVE: &str = "hooks/v1/active";
const TASKS: &str = "hooks/v1/deliveries/";
const DUE: &str = "hooks/v1/due/";
const COMPLETED: &str = "hooks/v1/completed/";
const MAX_ATTEMPTS: u32 = 3;
const MAX_HISTORY: usize = 50;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Subscription {
    pub id: Uuid,
    pub destination: String,
    pub push: bool,
    pub pull_request: bool,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PullAction {
    Opened,
    Edited,
    Closed,
    Reopened,
    ReadyForReview,
    ConvertedToDraft,
    Synchronize,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Event {
    Push {
        plan: String,
    },
    PullRequest {
        action: PullAction,
        number: u64,
        title: String,
        draft: bool,
        version: u64,
        base_ref: String,
        base_sha: String,
        head_ref: String,
        head_sha: String,
        head_repository: Option<Uuid>,
        merge_commit: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head_change: Option<HeadChange>,
    },
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeadChange {
    pub before: Option<String>,
    pub after: String,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Queued,
    Sending,
    Delivered,
    Failed,
    Cancelled,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Attempt {
    pub at: u64,
    pub http_status: Option<u16>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Delivery {
    pub id: Uuid,
    pub subscription: Subscription,
    pub event: Event,
    pub queued_at: u64,
    pub status: Status,
    pub attempts: u32,
    pub http_status: Option<u16>,
    pub recent_attempts: Vec<Attempt>,
    pub version: u64,
    failures: u32,
    due: u64,
}

fn key(id: Uuid) -> String {
    format!("{TASKS}{id}")
}

fn due(at: u64, id: Uuid) -> String {
    format!("{DUE}{at:020}/{id}")
}

fn revoked(id: Uuid) -> String {
    format!("hooks/v1/revoked/{id}")
}

fn schedule(context: &Transaction<'_>, delivery: &Delivery) -> crate::cells::Result<()> {
    context.put_document(&due(delivery.due, delivery.id), &delivery.id)?;
    context.put_document(&key(delivery.id), delivery)
}

fn enqueue(
    context: &Transaction<'_>,
    subscription: Subscription,
    event: Event,
) -> crate::cells::Result<()> {
    enqueue_at(context, subscription, event, timestamp(context.now_ms())?)
}

fn enqueue_at(
    context: &Transaction<'_>,
    subscription: Subscription,
    event: Event,
    queued_at: u64,
) -> crate::cells::Result<()> {
    let now = timestamp(context.now_ms())?;
    schedule(
        context,
        &Delivery {
            id: Uuid::now_v7(),
            subscription,
            event,
            queued_at,
            status: Status::Queued,
            attempts: 0,
            http_status: None,
            recent_attempts: vec![],
            version: 0,
            failures: 0,
            due: now,
        },
    )
}

pub(super) fn pull(
    context: &Transaction<'_>,
    record: &PullRecord,
    action: PullAction,
) -> crate::cells::Result<()> {
    if matches!(action, PullAction::Reopened | PullAction::Synchronize) {
        sync::route_changed(context, record.number)?;
    }
    let Some(subscription) = context
        .document::<Subscription>(ACTIVE)?
        .filter(|hook| hook.pull_request)
    else {
        return Ok(());
    };
    enqueue(context, subscription, pull_event(record, action, None))
}

fn pull_event(record: &PullRecord, action: PullAction, head_change: Option<HeadChange>) -> Event {
    let merge = record.merge.as_ref();
    let (base_sha, head_sha) = match (merge, record.retargeted_base.as_ref()) {
        (Some(merge), _) => (&merge.base_oid, &merge.head_oid),
        (None, Some(snapshot)) => (&snapshot.base_oid, &snapshot.head_oid),
        _ => (&record.base_oid, &record.head_oid),
    };
    Event::PullRequest {
        action,
        number: record.number,
        title: record.title.clone(),
        draft: record.draft,
        version: record.version,
        base_ref: record.base_ref.clone(),
        base_sha: base_sha.clone(),
        head_ref: record.head_ref.clone(),
        head_sha: head_change
            .as_ref()
            .map_or_else(|| head_sha.clone(), |change| change.after.clone()),
        head_repository: record.head_repository,
        merge_commit: merge.map(|value| value.commit_oid.clone()),
        head_change,
    }
}

pub(crate) struct QueuePush;
impl Mutation for QueuePush {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 430;
    type Input = String;
    type Output = ();

    fn execute(
        context: &mut Transaction<'_>,
        plan: String,
    ) -> crate::cells::Result<MutationResult<()>> {
        let Some(subscription) = context
            .document::<Subscription>(ACTIVE)?
            .filter(|hook| hook.push)
        else {
            return Ok(MutationResult::Rejected(()));
        };
        if plan.len() != 64 || blake3::Hash::from_hex(&plan).is_err() {
            return Err(crate::cells::Error::Invalid(
                "Invalid webhook publication plan",
            ));
        }
        let recorded = format!("hooks/v1/push/{plan}");
        if context.document::<bool>(&recorded)?.is_some() {
            return Ok(MutationResult::Rejected(()));
        }
        enqueue(context, subscription, Event::Push { plan })?;
        context.put_document(&recorded, &true)?;
        Ok(MutationResult::Success(()))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigureInput {
    pub expected: Option<Uuid>,
    pub subscription: Option<Subscription>,
}

pub(crate) struct Configure;
impl Mutation for Configure {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 431;
    type Input = ConfigureInput;
    type Output = bool;

    fn execute(
        context: &mut Transaction<'_>,
        input: ConfigureInput,
    ) -> crate::cells::Result<MutationResult<bool>> {
        let current = context.document::<Subscription>(ACTIVE)?;
        if current == input.subscription {
            return Ok(MutationResult::Rejected(true));
        }
        if input.expected != current.as_ref().map(|hook| hook.id) {
            return Ok(MutationResult::Rejected(false));
        }
        if let Some(next) = &input.subscription
            && (settings::archived(context)?
                || next.id.is_nil()
                || input.expected == Some(next.id)
                || !(next.push || next.pull_request)
                || next.destination.len() != 64
                || blake3::Hash::from_hex(&next.destination).is_err()
                || context.document::<bool>(&revoked(next.id))?.is_some())
        {
            return Ok(MutationResult::Rejected(false));
        }
        if let Some(current) = current {
            context.put_document(&revoked(current.id), &true)?;
        }
        if let Some(next) = input.subscription {
            context.put_document(ACTIVE, &next)?;
        } else {
            context.delete_document(ACTIVE)?;
        }
        Ok(MutationResult::Success(true))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Snapshot {
    pub subscription: Option<Subscription>,
    pub deliveries: Vec<Delivery>,
}

pub(crate) struct View;
impl Read for View {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 430;
    type Input = ();
    type Output = Snapshot;
    fn execute(context: &mut Transaction<'_>, _: ()) -> crate::cells::Result<Snapshot> {
        Ok(Snapshot {
            subscription: context.document(ACTIVE)?,
            deliveries: context
                .documents::<Delivery>(TASKS, None, MAX_HISTORY)?
                .into_iter()
                .map(|(_, row)| row)
                .collect(),
        })
    }
}

fn next_due(context: &Transaction<'_>) -> crate::cells::Result<Option<(String, Uuid)>> {
    let now = timestamp(context.now_ms())?;
    let boundary = format!("{DUE}{now:020}/~");
    Ok(context
        .documents_after(DUE, None, 1)?
        .into_iter()
        .next()
        .filter(|(key, _)| key < &boundary))
}

pub(crate) struct HasWork;
impl Read for HasWork {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 431;
    type Input = ();
    type Output = bool;
    fn execute(context: &mut Transaction<'_>, _: ()) -> crate::cells::Result<bool> {
        Ok(next_due(context)?.is_some())
    }
}

pub(crate) struct Current;
impl Read for Current {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 432;
    type Input = ();
    type Output = Option<Subscription>;
    fn execute(context: &mut Transaction<'_>, _: ()) -> crate::cells::Result<Self::Output> {
        context.document(ACTIVE)
    }
}

pub(crate) struct Claim;
impl Mutation for Claim {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 432;
    type Input = ();
    type Output = Option<Delivery>;
    fn execute(
        context: &mut Transaction<'_>,
        _: (),
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        let Some((scheduled, id)) = next_due(context)? else {
            return Ok(MutationResult::Rejected(None));
        };
        let mut delivery = context
            .document::<Delivery>(&key(id))?
            .ok_or(crate::cells::Error::Invalid("Missing webhook delivery"))?;
        context.delete_document(&scheduled)?;
        if context.document::<Subscription>(ACTIVE)?.as_ref() != Some(&delivery.subscription) {
            complete(context, &mut delivery, Status::Cancelled)?;
            return Ok(MutationResult::Success(None));
        }
        delivery.status = Status::Sending;
        delivery.version = next_version(delivery.version)?;
        delivery.due = timestamp(context.now_ms())?.saturating_add(60_000);
        schedule(context, &delivery)?;
        Ok(MutationResult::Success(Some(delivery)))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Outcome {
    WaitingForPublication,
    Delivered(u16),
    Failed(Option<u16>),
    Cancelled,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct FinishInput {
    pub id: Uuid,
    pub version: u64,
    pub outcome: Outcome,
}

pub(crate) struct Finish;
impl Mutation for Finish {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 433;
    type Input = FinishInput;
    type Output = ();
    fn execute(
        context: &mut Transaction<'_>,
        input: FinishInput,
    ) -> crate::cells::Result<MutationResult<()>> {
        let Some(mut delivery) = context.document::<Delivery>(&key(input.id))? else {
            return Ok(MutationResult::Rejected(()));
        };
        if delivery.version != input.version || !matches!(delivery.status, Status::Sending) {
            return Ok(MutationResult::Rejected(()));
        }
        context.delete_document(&due(delivery.due, delivery.id))?;
        let now = timestamp(context.now_ms())?;
        delivery.version = next_version(delivery.version)?;
        match input.outcome {
            Outcome::Cancelled => complete(context, &mut delivery, Status::Cancelled)?,
            Outcome::WaitingForPublication
                if now.saturating_sub(delivery.queued_at) > 3_600_000 =>
            {
                complete(context, &mut delivery, Status::Cancelled)?;
            }
            Outcome::WaitingForPublication => {
                delivery.status = Status::Queued;
                delivery.due = now.saturating_add(10_000);
                schedule(context, &delivery)?;
            }
            Outcome::Delivered(code) => {
                delivery.recent_attempts.truncate(9);
                delivery.recent_attempts.insert(
                    0,
                    Attempt {
                        at: now,
                        http_status: Some(code),
                    },
                );
                delivery.attempts = delivery.attempts.saturating_add(1);
                delivery.http_status = Some(code);
                complete(context, &mut delivery, Status::Delivered)?;
            }
            Outcome::Failed(code) => {
                delivery.recent_attempts.truncate(9);
                delivery.recent_attempts.insert(
                    0,
                    Attempt {
                        at: now,
                        http_status: code,
                    },
                );
                delivery.attempts = delivery.attempts.saturating_add(1);
                delivery.failures += 1;
                delivery.http_status = code;
                if delivery.failures >= MAX_ATTEMPTS {
                    complete(context, &mut delivery, Status::Failed)?;
                } else {
                    delivery.status = Status::Queued;
                    delivery.due = now.saturating_add(10_000 * u64::from(delivery.failures));
                    schedule(context, &delivery)?;
                }
            }
        }
        Ok(MutationResult::Success(()))
    }
}

fn complete(
    context: &Transaction<'_>,
    delivery: &mut Delivery,
    status: Status,
) -> crate::cells::Result<()> {
    delivery.status = status;
    context.put_document(&key(delivery.id), delivery)?;
    context.put_document(&format!("{COMPLETED}{}", delivery.id), &delivery.id)?;
    // Retain the latest terminal deliveries and leave pending operations alone.
    // Push plan reservations survive history eviction to prevent duplicate events.
    if let Some((old, id)) = context
        .documents::<Uuid>(COMPLETED, None, MAX_HISTORY + 1)?
        .get(MAX_HISTORY)
    {
        context.delete_document(old)?;
        context.delete_document(&key(*id))?;
    }
    Ok(())
}

pub(crate) struct Retry;
impl Mutation for Retry {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 434;
    type Input = (Uuid, u64, String);
    type Output = bool;
    fn execute(
        context: &mut Transaction<'_>,
        (id, version, destination): Self::Input,
    ) -> crate::cells::Result<MutationResult<bool>> {
        let Some(mut delivery) = context.document::<Delivery>(&key(id))? else {
            return Ok(MutationResult::Rejected(false));
        };
        if delivery.subscription.destination != destination
            || context.document::<Subscription>(ACTIVE)?.as_ref() != Some(&delivery.subscription)
        {
            return Ok(MutationResult::Rejected(false));
        }
        let receipt = format!("hooks/v1/retry/{id}/{version}");
        if context.document::<bool>(&receipt)?.is_some() {
            return Ok(MutationResult::Rejected(true));
        }
        if delivery.version != version
            || !matches!(delivery.status, Status::Failed | Status::Delivered)
        {
            return Ok(MutationResult::Rejected(false));
        }
        context.delete_document(&format!("{COMPLETED}{id}"))?;
        delivery.version = next_version(delivery.version)?;
        delivery.status = Status::Queued;
        delivery.failures = 0;
        delivery.due = timestamp(context.now_ms())?;
        schedule(context, &delivery)?;
        context.put_document(&receipt, &true)?;
        Ok(MutationResult::Success(true))
    }
}
