use super::*;
use crate::cells::{
    Error, Result,
    repository::collaboration::{
        Target,
        activity::{self, Action},
    },
};

#[derive(Deserialize, Serialize)]
pub(crate) struct Priority {
    pub pull: u64,
    pub id: Uuid,
    pub request_id: Uuid,
    pub actor: RepositoryAuthor,
    pub configured_queue_required: bool,
    pub ruleset_queue_required: bool,
}

fn revision_key(branch: &str) -> String {
    format!(
        "collaboration/v1/merge-queue-order/{}",
        blake3::hash(branch.as_bytes()).to_hex()
    )
}

pub(super) fn revision(context: &Transaction<'_>, branch: &str) -> Result<u64> {
    Ok(context.document(&revision_key(branch))?.unwrap_or(0))
}

pub(super) fn move_front(
    context: &Transaction<'_>,
    input: Priority,
) -> Result<MutationResult<Outcome>> {
    use MutationResult::{Rejected, Success};
    validate_number(input.pull)?;
    validate_author(&input.actor)?;
    if input.id.is_nil() || input.request_id.is_nil() {
        return Err(Error::Invalid("Invalid merge queue priority request"));
    }
    let receipt = format!("collaboration/v1/merge-queue-priority/{}", input.request_id);
    let mut digest = blake3::Hasher::new();
    digest.update(&input.pull.to_be_bytes());
    digest.update(input.id.as_bytes());
    hash_text(&mut digest, &input.actor.issuer);
    hash_text(&mut digest, &input.actor.subject);
    let digest = digest.finalize().to_hex().to_string();
    if let Some(previous) = context.document::<String>(&receipt)? {
        return Ok(if previous == digest {
            Success(Outcome::Saved)
        } else {
            Rejected(Outcome::Changed)
        });
    }
    if settings::archived(context)? {
        return Ok(Rejected(Outcome::Archived));
    }
    let active = key(ACTIVE, input.pull);
    let Some(mut job) = context
        .document::<Job>(&active)?
        .filter(|job| job.id == input.id)
    else {
        return Ok(Rejected(Outcome::Changed));
    };
    let Some(queue) = &job.queue else {
        return Ok(Rejected(Outcome::Changed));
    };
    let Some(pull) = load_pull(context, input.pull)? else {
        return Ok(Rejected(Outcome::Changed));
    };
    if pull.state != PullState::Open
        || pull.draft
        || pull.base_ref != job.base_ref
        || pull.head_ref != job.head_ref
        || pull.head_repository != job.head_repository
        || pull.retargeted_base.map(|base| base.revision) != job.comparison_revision
    {
        return Ok(Rejected(Outcome::Changed));
    }
    let rule = input.ruleset_queue_required
        || settings::requires_merge_queue(context, &job.base_ref, input.configured_queue_required)?;
    let configured = settings::merge_methods::load_for_ref(context, &job.base_ref)?;
    if !job.enabled(&configured, rule) {
        return Ok(Rejected(Outcome::Disabled));
    }
    let Some(front) = front(context, &job.base_ref)? else {
        return Err(Error::Invalid("Merge queue is missing its first entry"));
    };
    let front_pull = if front == pull.number {
        None
    } else {
        Some(
            load_pull(context, front)?
                .ok_or(Error::Invalid("Merge queue first pull request is missing"))?,
        )
    };
    if pull.merge_pending.is_some() || front_pull.is_some_and(|front| front.merge_pending.is_some())
    {
        // Begin-merge and priority both commit in this Cell. The winner fixes
        // publication order before the other command can act on its snapshot.
        return Ok(Rejected(Outcome::MergePending));
    }
    if front != pull.number {
        let sequence = allocate_sequence(context)?;
        context.delete_document(&queue_key(&job.base_ref, queue))?;
        let prioritized = QueueEntry {
            sequence,
            priority: true,
            testing: None,
        };
        context.put_document(&queue_key(&job.base_ref, &prioritized), &job.pull)?;
        job.queue = Some(prioritized);
        context.put_document(&active, &job)?;
        context.put_document(&revision_key(&job.base_ref), &sequence)?;
        activity::append(
            context,
            Target::Pull { number: input.pull },
            &input.actor,
            Action::MergeQueuePrioritized,
        )?;
    }
    context.put_document(&receipt, &digest)?;
    advance_revision(context)?;
    Ok(Success(Outcome::Saved))
}
