use super::*;
use crate::cells::repository::collaboration::{
    Target,
    activity::{self, Action},
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub revision: u64,
    pub base_oid: String,
    pub head_oid: String,
}

fn key(number: u64) -> String {
    format!("collaboration/v1/pulls/{number:016x}/base")
}

pub(super) fn snapshot(
    context: &Transaction<'_>,
    number: u64,
) -> crate::cells::Result<Option<Snapshot>> {
    context.document(&key(number))
}

pub(super) fn save_snapshot(
    context: &Transaction<'_>,
    number: u64,
    snapshot: &Snapshot,
) -> crate::cells::Result<()> {
    // A branch change on either side starts a new review comparison.
    context.put_document(&key(number), snapshot)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Change {
    pub request_id: uuid::Uuid,
    pub version: u64,
    pub base_ref: String,
    pub base_oid: String,
    pub head_oid: String,
}

pub(crate) mod stacked;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub pull: u64,
    pub actor: RepositoryAuthor,
    pub can_write: bool,
    pub comparison_current: bool,
    pub change: Change,
    pub code_owner_requests: review_requests::automatic::Plan,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Outcome {
    Saved,
    NotFound,
    Forbidden,
    Archived,
    InvalidBase,
    Conflict,
    ComparisonChanged,
    RequestConflict,
    MergePending,
}

pub(crate) struct Retarget;
impl Mutation for Retarget {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 412;
    type Input = Input;
    type Output = Outcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Input,
    ) -> crate::cells::Result<MutationResult<Outcome>> {
        use MutationResult::{Rejected, Success};
        let change = input.change;
        validate_number(input.pull)?;
        validate_number(change.version)?;
        validate_author(&input.actor)?;
        operations::validate_ref(&change.base_ref)?;
        validate_oid(&change.base_oid)?;
        validate_oid(&change.head_oid)?;
        if change.request_id.is_nil() {
            return Err(crate::cells::Error::Invalid(
                "Invalid base change request ID",
            ));
        }
        let Some(mut pull) = load_pull(context, input.pull)? else {
            return Ok(Rejected(Outcome::NotFound));
        };
        if !input.can_write && !same_author(&input.actor, &pull.author) {
            return Ok(Rejected(Outcome::Forbidden));
        }
        let receipt = format!("{}/requests/{}", key(input.pull), change.request_id);
        let digest = blake3::hash(&serde_json::to_vec(&(
            &input.actor.issuer,
            &input.actor.subject,
            &change,
        ))?)
        .to_hex()
        .to_string();
        if let Some(existing) = context.document::<String>(&receipt)? {
            return Ok(if existing == digest {
                Success(Outcome::Saved)
            } else {
                Rejected(Outcome::RequestConflict)
            });
        }
        if settings::archived(context)? {
            return Ok(Rejected(Outcome::Archived));
        }
        if pull.merge_pending.is_some() {
            return Ok(Rejected(Outcome::MergePending));
        }
        if pull.version != change.version || pull.state != PullState::Open {
            return Ok(Rejected(Outcome::Conflict));
        }
        if change.base_ref == pull.base_ref
            || (pull.head_repository.is_none() && change.base_ref == pull.head_ref)
        {
            return Ok(Rejected(Outcome::InvalidBase));
        }
        if !input.comparison_current {
            return Ok(Rejected(Outcome::ComparisonChanged));
        }
        let revision = next_version(pull.version)?;
        let now = timestamp(context.now_ms())?;
        reviews::dismissals::expire_approvals(
            context,
            &pull,
            &input.actor,
            now,
            "The pull request base branch changed.",
        )?;
        context.query("UPDATE repository_pulls SET base_ref=?,version=?,updated_at_ms=? WHERE number=? AND version=?", &[
            SqlValue::Text(change.base_ref.clone()), integer(revision)?, integer(now)?, integer(pull.number)?, integer(pull.version)?,
        ])?;
        let snapshot = Snapshot {
            revision,
            base_oid: change.base_oid,
            head_oid: change.head_oid,
        };
        save_snapshot(context, pull.number, &snapshot)?;
        pull.retargeted_base = Some(snapshot);
        let previous = std::mem::replace(&mut pull.base_ref, change.base_ref);
        pull.version = revision;
        pull.updated_at_ms = now;
        activity::append(
            context,
            Target::Pull {
                number: pull.number,
            },
            &input.actor,
            Action::BaseChanged {
                from: previous,
                to: pull.base_ref.clone(),
            },
        )?;
        webhooks::sync::route_changed(context, pull.number)?;
        webhooks::pull(context, &pull, webhooks::PullAction::Edited)?;
        if !pull.draft {
            review_requests::automatic::apply(
                context,
                &pull,
                &input.actor,
                input.code_owner_requests,
            )?;
        }
        context.put_document(&receipt, &digest)?;
        advance_revision(context)?;
        Ok(Success(Outcome::Saved))
    }
}
