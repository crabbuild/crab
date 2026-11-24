use super::*;
use uuid::Uuid;

const PREPARED: &str = "pulls/branch-cleanup/v1/prepared/";
const PENDING: &str = "pulls/branch-cleanup/v1/pending/";
pub(crate) const PAGE: usize = 16;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupPlan {
    pub source: Uuid,
    pub ownership_epoch: u32,
    pub setting_generation: u64,
    pub ref_transaction: String,
    pub actor: RepositoryAuthor,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct CleanupJob {
    pub pull: u64,
    pub merge: Uuid,
    pub plan: CleanupPlan,
}

fn key(prefix: &str, pull: u64) -> String {
    format!("{prefix}{pull:016x}")
}

pub(super) fn capture(
    context: &Transaction<'_>,
    pull: u64,
    merge: [u8; 16],
    plan: Option<CleanupPlan>,
) -> crate::cells::Result<()> {
    let key = key(PREPARED, pull);
    if let Some(plan) = plan {
        validate_author(&plan.actor)?;
        if plan.source.is_nil() || plan.setting_generation == 0 || plan.ref_transaction.is_empty() {
            return Err(crate::cells::Error::Invalid("invalid branch cleanup plan"));
        }
        context.put_document(
            &key,
            &CleanupJob {
                pull,
                merge: Uuid::from_bytes(merge),
                plan,
            },
        )?;
    } else {
        context.delete_document(&key)?;
    }
    Ok(())
}

pub(super) fn finalize(
    context: &Transaction<'_>,
    pull: u64,
    merge: [u8; 16],
    merged: bool,
) -> crate::cells::Result<()> {
    let prepared = key(PREPARED, pull);
    if let Some(job) = context.document::<CleanupJob>(&prepared)? {
        if job.merge != Uuid::from_bytes(merge) {
            return Err(crate::cells::Error::Invalid(
                "branch cleanup belongs to a different merge",
            ));
        }
        if merged {
            context.put_document(&key(PENDING, pull), &job)?;
        }
        context.delete_document(&prepared)?;
    }
    Ok(())
}

pub(crate) struct PendingCleanup;
impl Read for PendingCleanup {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 448;
    type Input = Option<u64>;
    type Output = Vec<CleanupJob>;

    fn execute(
        context: &mut Transaction<'_>,
        before: Option<u64>,
    ) -> crate::cells::Result<Self::Output> {
        let before = before.map(|pull| key(PENDING, pull));
        Ok(context
            .documents::<CleanupJob>(PENDING, before.as_deref(), PAGE)?
            .into_iter()
            .map(|(_, job)| job)
            .collect())
    }
}

pub(crate) struct FinishCleanup;
impl Mutation for FinishCleanup {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 448;
    type Input = (u64, Uuid);
    type Output = ();

    fn execute(
        context: &mut Transaction<'_>,
        (pull, merge): Self::Input,
    ) -> crate::cells::Result<MutationResult<()>> {
        let key = key(PENDING, pull);
        if context
            .document::<CleanupJob>(&key)?
            .is_some_and(|job| job.merge == merge)
        {
            context.delete_document(&key)?;
        }
        Ok(MutationResult::Success(()))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Reference {
    pub name: String,
    pub fork: Option<uuid::Uuid>,
    #[serde(default)]
    pub head_only: bool,
}

pub(crate) struct HasCleanup;
impl Read for HasCleanup {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 449;
    type Input = u64;
    type Output = bool;

    fn execute(context: &mut Transaction<'_>, pull: u64) -> crate::cells::Result<Self::Output> {
        Ok(context
            .document::<CleanupJob>(&key(PENDING, pull))?
            .is_some())
    }
}

pub(crate) struct InUse;

impl Read for InUse {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 447;
    type Input = Reference;
    type Output = bool;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        // Ref bindings and lifecycle state are projected in the same Cell
        // transaction as creation, retargeting, and reopening.
        search::index::ensure_ready(context)?;
        let head = qualified_head(&input.name, input.fork);
        let base = (!input.head_only && input.fork.is_none()).then_some(input.name);
        let rows = context.query(
            "SELECT 1 FROM discussion_index d JOIN discussion_index_pull_sources s ON s.document=d.rowid
                WHERE d.kind=1 AND d.state=? AND (s.head_ref=? OR s.base_ref=?) LIMIT 1",
            &[
                SqlValue::Integer(i64::from(PullState::Open.code())),
                SqlValue::Blob(head.into_bytes()),
                base.map_or(SqlValue::Null, SqlValue::Text),
            ],
        )?;
        Ok(!rows.rows.is_empty())
    }
}

pub(super) fn qualified_head(reference: &str, fork: Option<uuid::Uuid>) -> String {
    // A fork ref must never match a same-named upstream ref.
    fork.map_or_else(
        || reference.to_owned(),
        |fork| format!("{fork}:{reference}"),
    )
}

#[derive(Deserialize, Serialize)]
pub(crate) struct PushedHead {
    pub name: String,
    pub fork: Option<Uuid>,
    pub base: String,
    pub oid: String,
}

pub(crate) struct HasPullForPush;
impl Read for HasPullForPush {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 453;
    type Input = PushedHead;
    type Output = bool;

    fn execute(context: &mut Transaction<'_>, input: PushedHead) -> crate::cells::Result<bool> {
        search::index::ensure_ready(context)?;
        let rows = context.query(
            "SELECT 1 FROM discussion_index d JOIN discussion_index_pull_sources s ON s.document=d.rowid
                WHERE d.kind=1 AND s.head_ref=? AND (d.state=? OR
                    (d.state=? AND s.base_ref=? AND s.merged_head_oid=?)) LIMIT 1",
            &[
                SqlValue::Blob(qualified_head(&input.name, input.fork).into_bytes()),
                SqlValue::Integer(i64::from(PullState::Open.code())),
                SqlValue::Integer(i64::from(PullState::Merged.code())),
                SqlValue::Text(input.base),
                SqlValue::Text(input.oid),
            ],
        )?;
        Ok(!rows.rows.is_empty())
    }
}
