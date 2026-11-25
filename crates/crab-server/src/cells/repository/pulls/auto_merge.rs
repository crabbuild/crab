use super::*;
use uuid::Uuid;

pub(crate) mod list;
mod priority;
mod window;
pub(crate) use priority::Priority;

const ACTIVE: &str = "collaboration/v1/auto-merge/active/";
const QUEUE: &str = "collaboration/v1/merge-queue/";
const QUEUE_SEQUENCE: &str = "collaboration/v1/merge-queue-sequence";
const STOPPED: &str = "collaboration/v1/auto-merge/stopped/";
pub(crate) const PAGE: usize = 32;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Job {
    pub pull: u64,
    pub id: Uuid,
    pub actor: RepositoryAuthor,
    pub base_ref: String,
    pub head_ref: String,
    pub head_repository: Option<Uuid>,
    pub head_oid: String,
    pub initial_base_oid: String,
    pub initial_version: u64,
    pub generation: u64,
    #[serde(default)]
    pub queue_method_revision: u64,
    pub comparison_revision: Option<u64>,
    pub method: MergeMethod,
    pub message: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub when_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<QueueEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct QueueEntry {
    sequence: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    priority: bool,
    pub testing: Option<QueueTest>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct QueueTest {
    pub request_id: Uuid,
    pub version: u64,
    pub base_oid: String,
    pub previous_oid: Option<String>,
    #[serde(default)]
    pub order_revision: u64,
}

impl QueueTest {
    pub(crate) fn timed_out(
        &self,
        now_ms: i64,
        timeout_minutes: u16,
    ) -> crate::cells::Result<bool> {
        let started = self
            .request_id
            .get_timestamp()
            .ok_or(crate::cells::Error::Invalid(
                "merge queue test timestamp is missing",
            ))?;
        let seconds = i64::try_from(started.to_unix().0).map_err(|_| {
            crate::cells::Error::Invalid("merge queue test timestamp is out of range")
        })?;
        Ok(now_ms.div_euclid(1_000).saturating_sub(seconds) >= i64::from(timeout_minutes) * 60)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stop {
    Canceled,
    HeadChanged,
    PullChanged,
    PermissionChanged,
    Disabled,
    MethodDisabled,
    QueueRequired,
    Conflict,
    ChecksFailed,
    ChecksTimedOut,
    RequirementsChanged,
    PathLocked,
    Merged,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct State {
    pub job: Job,
    pub stopped: Option<Stop>,
    #[serde(default)]
    pub first: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead: Option<Vec<window::Predecessor>>,
    #[serde(default)]
    pub order_revision: u64,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Change {
    Arm {
        version: u64,
        job: Job,
        #[serde(default)]
        queue: bool,
        #[serde(default)]
        configured_queue_required: bool,
        #[serde(default)]
        ruleset_queue_required: bool,
    },
    Enqueue {
        pull: u64,
        id: Uuid,
        version: u64,
        configured_queue_required: bool,
        ruleset_queue_required: bool,
    },
    Test {
        pull: u64,
        id: Uuid,
        attempt: QueueTest,
        #[serde(default)]
        configured_queue_required: bool,
        #[serde(default)]
        ruleset_queue_required: bool,
    },
    Prioritize(Priority),
    Disarm {
        pull: u64,
        id: Uuid,
        reason: Stop,
        #[serde(default)]
        front_revision: Option<u64>,
    },
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Outcome {
    Saved,
    Changed,
    Disabled,
    Archived,
    MergePending,
    QueueRequired,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct QueueSettings {
    pub method: MergeMethod,
    method_revision: u64,
    generation: u64,
}

impl QueueSettings {
    fn matches(&self, settings: &settings::merge_methods::MergeMethods) -> bool {
        self.method == settings.merge_queue_method
            && self.method_revision == settings.merge_queue_method_revision
    }

    pub(crate) fn enabled(
        &self,
        settings: &settings::merge_methods::MergeMethods,
        required: bool,
    ) -> bool {
        self.matches(settings)
            && (required
                || (settings.merge_queue && settings.merge_queue_generation == self.generation))
    }
}

impl Job {
    pub(crate) fn uses_queue(&self) -> bool {
        self.when_ready || self.queue.is_some()
    }

    pub(crate) fn waiting_for_queue(&self) -> bool {
        self.when_ready && self.queue.is_none()
    }

    fn matches_pull(&self, pull: &PullRecord, version: u64) -> bool {
        pull.version == version
            && pull.state == PullState::Open
            && !pull.draft
            && pull.base_ref == self.base_ref
            && pull.head_ref == self.head_ref
            && pull.head_repository == self.head_repository
            && pull.retargeted_base.as_ref().map(|base| base.revision) == self.comparison_revision
    }

    pub(crate) fn candidate_id(&self, order_revision: u64) -> Uuid {
        if order_revision == 0 {
            return self.id;
        }
        let mut digest = blake3::Hasher::new();
        digest.update(b"crab-merge-queue-order-v1\0");
        digest.update(self.id.as_bytes());
        digest.update(&order_revision.to_be_bytes());
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest.finalize().as_bytes()[..16]);
        Uuid::from_bytes(bytes)
    }

    pub(crate) fn publication_id(
        &self,
        version: u64,
        base: &str,
        tested: Option<&str>,
        order_revision: u64,
    ) -> Uuid {
        let mut digest = blake3::Hasher::new();
        digest.update(b"crab-auto-merge-attempt-v1\0");
        digest.update(self.candidate_id(order_revision).as_bytes());
        digest.update(&version.to_be_bytes());
        digest.update(base.as_bytes());
        if let Some(tested) = tested {
            digest.update(b"\0tested\0");
            digest.update(tested.as_bytes());
        }
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest.finalize().as_bytes()[..16]);
        Uuid::from_bytes(bytes)
    }

    pub(crate) fn enabled(
        &self,
        settings: &settings::merge_methods::MergeMethods,
        queue_required: bool,
    ) -> bool {
        if self.uses_queue() {
            self.queue_settings().enabled(settings, queue_required)
        } else {
            !queue_required
                && settings.auto_merge
                && settings.auto_merge_generation == self.generation
        }
    }

    pub(crate) fn queue_method_matches(
        &self,
        settings: &settings::merge_methods::MergeMethods,
    ) -> bool {
        self.queue_settings().matches(settings)
    }

    fn queue_settings(&self) -> QueueSettings {
        QueueSettings {
            method: self.method,
            method_revision: self.queue_method_revision,
            generation: self.generation,
        }
    }

    pub(crate) fn sweep_order(&self) -> (bool, u64) {
        self.queue.as_ref().map_or((true, self.pull), |entry| {
            // Favor the queue front within each page to start building
            // prefixes before successors are scheduled in this pass.
            if entry.priority {
                (false, u64::MAX - entry.sequence)
            } else {
                (true, entry.sequence)
            }
        })
    }
}

fn queue_prefix(base_ref: &str) -> String {
    format!("{QUEUE}{}/", blake3::hash(base_ref.as_bytes()).to_hex())
}

fn queue_key(base_ref: &str, entry: &QueueEntry) -> String {
    let prefix = queue_prefix(base_ref);
    // Descending scans read explicit moves first, latest move first. Inverted
    // sequences preserve FIFO ordering and the keys of existing admissions.
    if entry.priority {
        format!("{prefix}priority/{:016x}", entry.sequence)
    } else {
        format!("{prefix}{:016x}", u64::MAX - entry.sequence)
    }
}

fn allocate_sequence(context: &Transaction<'_>) -> crate::cells::Result<u64> {
    let sequence = context
        .document::<u64>(QUEUE_SEQUENCE)?
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(crate::cells::Error::Invalid(
            "Merge queue sequence is exhausted",
        ))?;
    context.put_document(QUEUE_SEQUENCE, &sequence)?;
    Ok(sequence)
}

fn enter_queue(context: &Transaction<'_>, job: &mut Job) -> crate::cells::Result<()> {
    let entry = QueueEntry {
        sequence: allocate_sequence(context)?,
        priority: false,
        testing: None,
    };
    context.put_document(&queue_key(&job.base_ref, &entry), &job.pull)?;
    job.queue = Some(entry);
    Ok(())
}

fn front(context: &Transaction<'_>, branch: &str) -> crate::cells::Result<Option<u64>> {
    let first = context.documents::<u64>(&queue_prefix(branch), None, 1)?;
    Ok(first.first().map(|(_, pull)| *pull))
}

fn first(context: &Transaction<'_>, job: &Job) -> crate::cells::Result<bool> {
    Ok(front(context, &job.base_ref)? == Some(job.pull))
}

pub(super) fn allows_merge(
    context: &Transaction<'_>,
    pull: &PullRecord,
    merge: &PullMerge,
) -> crate::cells::Result<bool> {
    let Some(job) = context
        .document::<Job>(&key(ACTIVE, pull.number))?
        .filter(|job| job.queue.is_some())
    else {
        return Ok(!settings::requires_merge_queue(
            context,
            &pull.base_ref,
            false,
        )?);
    };
    Ok(merge.tested_candidate
        && merge.method == job.method
        && merge.request_id
            == job
                .publication_id(
                    merge.pull_version,
                    &merge.base_oid,
                    Some(&merge.commit_oid),
                    priority::revision(context, &job.base_ref)?,
                )
                .into_bytes()
        && first(context, &job)?)
}

fn key(prefix: &str, number: u64) -> String {
    format!("{prefix}{number:016x}")
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 414;
    type Input = u64;
    type Output = Option<State>;

    fn execute(context: &mut Transaction<'_>, pull: u64) -> crate::cells::Result<Self::Output> {
        if let Some(job) = context.document::<Job>(&key(ACTIVE, pull))? {
            let (ahead, order_revision) = if job.queue.is_some() {
                let limit = settings::merge_methods::load_for_ref(context, &job.base_ref)?
                    .merge_queue_builds;
                let revision = priority::revision(context, &job.base_ref)?;
                let pulls = window::pulls(context, &job.base_ref, usize::from(limit))?;
                let ahead = pulls
                    .iter()
                    .position(|pull| *pull == job.pull)
                    .map(|position| {
                        pulls[..position]
                            .iter()
                            .map(|pull| {
                                window::predecessor(context, &job.base_ref, *pull, revision)
                            })
                            .collect::<crate::cells::Result<Vec<_>>>()
                    })
                    .transpose()?;
                (ahead, revision)
            } else {
                (None, 0)
            };
            let first = ahead.as_ref().is_some_and(Vec::is_empty);
            return Ok(Some(State {
                job,
                stopped: None,
                first,
                ahead,
                order_revision,
            }));
        }
        context.document(&key(STOPPED, pull))
    }
}

pub(crate) struct Pending;
impl Read for Pending {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 415;
    type Input = Option<u64>;
    type Output = Vec<Job>;

    fn execute(
        context: &mut Transaction<'_>,
        before: Option<u64>,
    ) -> crate::cells::Result<Self::Output> {
        let before = before.map(|number| key(ACTIVE, number));
        Ok(context
            .documents::<Job>(ACTIVE, before.as_deref(), PAGE)?
            .into_iter()
            .map(|(_, job)| job)
            .collect())
    }
}

pub(crate) struct Edit;
impl Mutation for Edit {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 414;
    type Input = Change;
    type Output = Outcome;

    fn execute(
        context: &mut Transaction<'_>,
        action: Change,
    ) -> crate::cells::Result<MutationResult<Outcome>> {
        use MutationResult::{Rejected, Success};
        match action {
            Change::Arm {
                version,
                mut job,
                queue,
                configured_queue_required,
                ruleset_queue_required,
            } => {
                validate_number(job.pull)?;
                validate_number(version)?;
                validate_author(&job.actor)?;
                validate_oid(&job.head_oid)?;
                validate_oid(&job.initial_base_oid)?;
                if job.id.is_nil()
                    || job.queue.is_some()
                    || (job.when_ready && !queue)
                    || job.message.chars().count() > 256
                    || (queue && (!job.method.supports_queue() || !job.message.is_empty()))
                {
                    return Err(crate::cells::Error::Invalid("Invalid auto-merge request"));
                }
                let active = key(ACTIVE, job.pull);
                if let Some(existing) = context.document::<Job>(&active)? {
                    let same_mode = existing.uses_queue() == queue;
                    job.queue = existing.queue.clone();
                    return Ok(if same_mode && existing == job {
                        Success(Outcome::Saved)
                    } else {
                        Rejected(Outcome::Changed)
                    });
                }
                if context
                    .document::<State>(&key(STOPPED, job.pull))?
                    .is_some_and(|state| state.job.id == job.id)
                {
                    return Ok(Rejected(Outcome::Changed));
                }
                let receipt = format!(
                    "collaboration/v1/auto-merge/requests/{:016x}/{}",
                    job.pull, job.id
                );
                if context.document::<bool>(&receipt)?.is_some() {
                    return Ok(Rejected(Outcome::Changed));
                }
                if settings::archived(context)? {
                    return Ok(Rejected(Outcome::Archived));
                }
                let queue_required = ruleset_queue_required
                    || settings::requires_merge_queue(
                        context,
                        &job.base_ref,
                        configured_queue_required,
                    )?;
                if queue_required && !queue {
                    return Ok(Rejected(Outcome::QueueRequired));
                }
                if job.when_ready && !queue_required {
                    return Ok(Rejected(Outcome::Disabled));
                }
                let settings = settings::merge_methods::load_for_ref(context, &job.base_ref)?;
                if queue && !job.queue_method_matches(&settings) {
                    return Ok(Rejected(Outcome::Changed));
                }
                let enabled = if queue {
                    queue_required
                        || (settings.merge_queue
                            && settings.merge_queue_generation == job.generation)
                } else {
                    settings.auto_merge && settings.auto_merge_generation == job.generation
                };
                if !enabled || (!queue && !settings.enabled.contains(&job.method)) {
                    return Ok(Rejected(Outcome::Disabled));
                }
                let Some(pull) = load_pull(context, job.pull)? else {
                    return Ok(Rejected(Outcome::Changed));
                };
                if pull.merge_pending.is_some() {
                    return Ok(Rejected(Outcome::MergePending));
                }
                if !job.matches_pull(&pull, version) {
                    return Ok(Rejected(Outcome::Changed));
                }
                if queue && !job.when_ready {
                    enter_queue(context, &mut job)?;
                }
                context.put_document(&active, &job)?;
                context.put_document(&receipt, &true)?;
                context.delete_document(&key(STOPPED, job.pull))?;
            }
            Change::Enqueue {
                pull,
                id,
                version,
                configured_queue_required,
                ruleset_queue_required,
            } => {
                let active = key(ACTIVE, pull);
                let Some(mut job) = context
                    .document::<Job>(&active)?
                    .filter(|job| job.id == id && job.when_ready)
                else {
                    return Ok(Rejected(Outcome::Changed));
                };
                if job.queue.is_some() {
                    return Ok(Success(Outcome::Saved));
                }
                if settings::archived(context)? {
                    return Ok(Rejected(Outcome::Archived));
                }
                let configured = settings::merge_methods::load_for_ref(context, &job.base_ref)?;
                let required = ruleset_queue_required
                    || settings::requires_merge_queue(
                        context,
                        &job.base_ref,
                        configured_queue_required,
                    )?;
                if !job.enabled(&configured, required) {
                    return Ok(Rejected(Outcome::Disabled));
                }
                let Some(pull) = load_pull(context, job.pull)? else {
                    return Ok(Rejected(Outcome::Changed));
                };
                if pull.merge_pending.is_some() {
                    return Ok(Rejected(Outcome::MergePending));
                }
                if !job.matches_pull(&pull, version) {
                    return Ok(Rejected(Outcome::Changed));
                }
                // Only eligible requests occupy a queue position. The index
                // and the active job must commit together for retry safety.
                enter_queue(context, &mut job)?;
                context.put_document(&active, &job)?;
            }
            Change::Test {
                pull,
                id,
                attempt,
                configured_queue_required,
                ruleset_queue_required,
            } => {
                validate_number(attempt.version)?;
                validate_oid(&attempt.base_oid)?;
                if let Some(previous) = &attempt.previous_oid {
                    validate_oid(previous)?;
                }
                if attempt.request_id.get_version_num() != 7 {
                    return Err(crate::cells::Error::Invalid("Invalid merge queue test ID"));
                }
                let active = key(ACTIVE, pull);
                let Some(mut job) = context.document::<Job>(&active)?.filter(|job| job.id == id)
                else {
                    return Ok(Rejected(Outcome::Changed));
                };
                if attempt.order_revision != priority::revision(context, &job.base_ref)? {
                    return Ok(Rejected(Outcome::Changed));
                }
                let queue_required = ruleset_queue_required
                    || settings::requires_merge_queue(
                        context,
                        &job.base_ref,
                        configured_queue_required,
                    )?;
                let configured = settings::merge_methods::load_for_ref(context, &job.base_ref)?;
                let window = window::pulls(
                    context,
                    &job.base_ref,
                    usize::from(configured.merge_queue_builds),
                )?;
                if !window.contains(&pull) || !job.enabled(&configured, queue_required) {
                    return Ok(Rejected(Outcome::Disabled));
                }
                let Some(queue) = job.queue.as_mut() else {
                    return Ok(Rejected(Outcome::Changed));
                };
                queue.testing = Some(attempt);
                context.put_document(&active, &job)?;
            }
            Change::Prioritize(input) => return priority::move_front(context, input),
            Change::Disarm {
                pull,
                id,
                reason,
                front_revision,
            } => {
                let active = key(ACTIVE, pull);
                let stopped = key(STOPPED, pull);
                let Some(job) = context.document::<Job>(&active)? else {
                    return Ok(
                        if context
                            .document::<State>(&stopped)?
                            .is_some_and(|state| state.job.id == id)
                        {
                            Success(Outcome::Saved)
                        } else {
                            Rejected(Outcome::Changed)
                        },
                    );
                };
                if job.id != id {
                    return Ok(Rejected(Outcome::Changed));
                }
                if load_pull(context, pull)?.is_some_and(|pull| pull.merge_pending.is_some()) {
                    return Ok(Rejected(Outcome::MergePending));
                }
                if let Some(revision) = front_revision
                    && (revision != priority::revision(context, &job.base_ref)?
                        || !first(context, &job)?)
                {
                    return Ok(Rejected(Outcome::Changed));
                }
                if reason == Stop::ChecksTimedOut {
                    let Some(test) = job.queue.as_ref().and_then(|queue| queue.testing.as_ref())
                    else {
                        return Ok(Rejected(Outcome::Changed));
                    };
                    // A settings edit can extend a deadline after the worker's
                    // read. Exit only under the branch's current timeout.
                    let settings = settings::merge_methods::load_for_ref(context, &job.base_ref)?;
                    if !first(context, &job)?
                        || !test
                            .timed_out(context.now_ms(), settings.merge_queue_timeout_minutes)?
                    {
                        return Ok(Rejected(Outcome::Changed));
                    }
                }
                if let Some(queue) = &job.queue {
                    context.delete_document(&queue_key(&job.base_ref, queue))?;
                }
                context.delete_document(&active)?;
                context.put_document(
                    &stopped,
                    &State {
                        job,
                        stopped: Some(reason),
                        first: false,
                        ahead: None,
                        order_revision: 0,
                    },
                )?;
            }
        }
        advance_revision(context)?;
        Ok(Success(Outcome::Saved))
    }
}
