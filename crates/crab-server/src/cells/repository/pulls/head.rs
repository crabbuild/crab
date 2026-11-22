use super::*;
use crate::cells::repository::collaboration::{
    Target,
    activity::{self, Action},
};

#[derive(Deserialize, Serialize)]
pub(crate) struct Input {
    pub number: u64,
    pub version: u64,
    pub actor: RepositoryAuthor,
    pub plan_id: String,
    pub head_repository: Option<uuid::Uuid>,
    pub previous_ref: String,
    pub new_ref: String,
    pub base_oid: String,
    pub new_oid: String,
    pub comparison_current: bool,
}

pub(crate) struct Switch;
impl Mutation for Switch {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 413;
    type Input = Input;
    type Output = bool;

    fn execute(
        context: &mut Transaction<'_>,
        input: Input,
    ) -> crate::cells::Result<MutationResult<bool>> {
        use MutationResult::{Rejected, Success};
        validate_number(input.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        operations::validate_ref(&input.previous_ref)?;
        operations::validate_ref(&input.new_ref)?;
        validate_oid(&input.base_oid)?;
        validate_oid(&input.new_oid)?;
        if input.plan_id.len() != 64 || !input.plan_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(crate::cells::Error::Invalid(
                "Invalid head publication receipt",
            ));
        }
        // Git's plan ID binds the actor, PR version, destination and contents.
        // A completed switch can be acknowledged even after the new tip moves.
        let receipt = format!(
            "collaboration/v1/pulls/{:016x}/head/{}",
            input.number, input.plan_id
        );
        if context.document::<bool>(&receipt)?.is_some() {
            return Ok(Success(true));
        }
        let Some(pull) = load_pull(context, input.number)? else {
            return Ok(Rejected(false));
        };
        if pull.version != input.version
            || pull.head_repository != input.head_repository
            || pull.head_ref != input.previous_ref
            || pull.head_ref == input.new_ref
            || (pull.head_repository.is_none() && pull.base_ref == input.new_ref)
            || pull.state != PullState::Open
            || pull.merge_pending.is_some()
            || !input.comparison_current
            || settings::archived(context)?
        {
            return Ok(Rejected(false));
        }
        let revision = next_version(pull.version)?;
        let now = timestamp(context.now_ms())?;
        reviews::dismissals::expire_approvals(
            context,
            &pull,
            &input.actor,
            now,
            "The pull request head branch changed.",
        )?;
        context.query(
            "UPDATE repository_pulls SET head_ref=?,version=?,updated_at_ms=? WHERE number=? AND version=?",
            &[SqlValue::Text(input.new_ref.clone()), integer(revision)?, integer(now)?, integer(pull.number)?, integer(pull.version)?],
        )?;
        base::save_snapshot(
            context,
            pull.number,
            &base::Snapshot {
                revision,
                base_oid: input.base_oid,
                head_oid: input.new_oid,
            },
        )?;
        activity::append(
            context,
            Target::Pull {
                number: pull.number,
            },
            &input.actor,
            Action::HeadChanged {
                from: pull.head_ref,
                to: input.new_ref,
            },
        )?;
        context.put_document(&receipt, &true)?;
        advance_revision(context)?;
        Ok(Success(true))
    }
}
