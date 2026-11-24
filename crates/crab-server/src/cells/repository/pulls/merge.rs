use super::*;
use crate::cells::repository::collaboration::{
    Target,
    activity::{self, Action},
    links,
};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CaptureMergeEffectsInput {
    pub pull: u64,
    pub request_id: [u8; 16],
    pub on_default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<branch::CleanupPlan>,
}

pub(crate) struct CaptureMergeEffects;
impl Mutation for CaptureMergeEffects {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 232;
    type Input = CaptureMergeEffectsInput;
    type Output = bool;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        validate_number(input.pull)?;
        let Some(pull) = load_pull(context, input.pull)? else {
            return Ok(MutationResult::Rejected(false));
        };
        if pull
            .merge_pending
            .is_none_or(|merge| merge.request_id != input.request_id)
        {
            return Ok(MutationResult::Rejected(false));
        }
        links::capture_closure(
            context,
            input.pull,
            input.request_id,
            input.on_default,
            &pull.body,
        )?;
        branch::capture(context, input.pull, input.request_id, input.cleanup)?;
        Ok(MutationResult::Success(true))
    }
}

pub(crate) struct ReservePullMerge;

impl Mutation for ReservePullMerge {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 22;
    type Input = ReservePullMergeInput;
    type Output = ReservePullMergeOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(ReservePullMergeOutcome::Archived));
        }
        lifecycle::ensure_ready(context)?;
        validate_number(input.pull)?;
        validate_merge(&input.merge)?;
        let Some(pull) = load_pull(context, input.pull)? else {
            return Ok(MutationResult::Rejected(
                ReservePullMergeOutcome::PullNotFound,
            ));
        };
        if pull.draft {
            return Ok(MutationResult::Rejected(ReservePullMergeOutcome::Draft));
        }
        let digest = merge_digest(input.pull, &input.merge);
        let existing = context.sql(&SqlBatch { statements: vec![statement("SELECT payload_digest, merge_record FROM repository_pull_merge_submissions WHERE pull_number = ? AND request_id = ?", vec![integer(input.pull)?, SqlValue::Blob(input.merge.request_id.to_vec())])] })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(MutationResult::Rejected(
                    ReservePullMergeOutcome::RequestConflict,
                ));
            }
            return Ok(MutationResult::Success(ReservePullMergeOutcome::Reserved(
                Box::new(decode_record(result_blob(row, 1)?)?),
            )));
        }
        context.sql(&SqlBatch { statements: vec![statement("INSERT INTO repository_pull_merge_submissions(pull_number, request_id, payload_digest, merge_record) VALUES (?, ?, ?, ?)", vec![integer(input.pull)?, SqlValue::Blob(input.merge.request_id.to_vec()), SqlValue::Blob(digest.as_bytes().to_vec()), SqlValue::Blob(encode_record(&input.merge)?)] )] })?;
        advance_revision(context)?;
        Ok(MutationResult::Success(ReservePullMergeOutcome::Reserved(
            Box::new(input.merge),
        )))
    }
}

pub(crate) struct TransitionPullMerge;

impl Mutation for TransitionPullMerge {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 23;
    type Input = TransitionPullMergeInput;
    type Output = TransitionPullMergeOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        lifecycle::ensure_ready(context)?;
        validate_number(input.pull)?;
        validate_merge(&input.merge)?;
        let Some(mut pull) = load_pull(context, input.pull)? else {
            return Ok(MutationResult::Rejected(
                TransitionPullMergeOutcome::NotFound,
            ));
        };
        let was_state = pull.state;
        let was_closed = pull.state != PullState::Open;
        let old_version = pull.version;
        match input.transition {
            PullMergeTransition::Begin => {
                if let Some(done) = &pull.merge {
                    return if done.request_id == input.merge.request_id {
                        Ok(MutationResult::Success(
                            TransitionPullMergeOutcome::Applied(Box::new(pull)),
                        ))
                    } else {
                        Ok(MutationResult::Rejected(
                            TransitionPullMergeOutcome::Conflict,
                        ))
                    };
                }
                if let Some(pending) = &pull.merge_pending {
                    return if pending.request_id == input.merge.request_id {
                        Ok(MutationResult::Success(
                            TransitionPullMergeOutcome::Applied(Box::new(pull)),
                        ))
                    } else {
                        Ok(MutationResult::Rejected(
                            TransitionPullMergeOutcome::Conflict,
                        ))
                    };
                }
                // Completed/pending retries above do not start new work. Only
                // a fresh admission must lose to an earlier archive command.
                if settings::archived(context)? {
                    return Ok(MutationResult::Rejected(
                        TransitionPullMergeOutcome::Archived,
                    ));
                }
                if pull.draft {
                    return Ok(MutationResult::Rejected(TransitionPullMergeOutcome::Draft));
                }
                if pull.state != PullState::Open
                    || pull.version != input.merge.pull_version
                    || !super::auto_merge::allows_merge(context, &pull, &input.merge)?
                {
                    return Ok(MutationResult::Rejected(
                        TransitionPullMergeOutcome::Conflict,
                    ));
                }
                pull.merge_pending = Some(input.merge);
            }
            PullMergeTransition::Abort => {
                if pull.merge.is_some()
                    || pull
                        .merge_pending
                        .as_ref()
                        .is_none_or(|pending| pending.request_id != input.merge.request_id)
                {
                    return Ok(MutationResult::Success(
                        TransitionPullMergeOutcome::Applied(Box::new(pull)),
                    ));
                }
                pull.merge_pending = None;
                links::clear_closure(context, pull.number)?;
                branch::finalize(context, pull.number, input.merge.request_id, false)?;
            }
            PullMergeTransition::Complete => {
                if let Some(done) = &pull.merge {
                    return if done.request_id == input.merge.request_id {
                        Ok(MutationResult::Success(
                            TransitionPullMergeOutcome::Applied(Box::new(pull)),
                        ))
                    } else {
                        Ok(MutationResult::Rejected(
                            TransitionPullMergeOutcome::Conflict,
                        ))
                    };
                }
                if pull
                    .merge_pending
                    .as_ref()
                    .is_none_or(|pending| pending.request_id != input.merge.request_id)
                {
                    return Ok(MutationResult::Rejected(
                        TransitionPullMergeOutcome::Conflict,
                    ));
                }
                pull.state = PullState::Merged;
                links::keywords::projection::changed(context, pull.number, &pull.body, "")?;
                pull.merge_pending = None;
                branch::finalize(context, pull.number, input.merge.request_id, true)?;
                pull.merge = Some(input.merge);
            }
        }
        pull.version = next_version(pull.version)?;
        pull.updated_at_ms = timestamp(context.now_ms())?;
        update_pull_row(context, &pull, old_version)?;
        // The immutable merge record dates reservation for reproducible Git publication.
        // Lifecycle metadata dates completion; later edits and retries must preserve it.
        lifecycle::change(
            context,
            pull.number,
            was_state,
            pull.state,
            pull.updated_at_ms,
        )?;
        // Progress changes commit with close/reopen and merge completion.
        let subject = collaboration::Subject {
            kind: collaboration::SubjectKind::Pull,
            number: pull.number,
        };
        collaboration::milestones::state_changed(
            context,
            subject,
            was_closed,
            pull.state != PullState::Open,
        )?;
        advance_revision(context)?;
        if let Some(merge) = &pull.merge {
            let target = Target::Pull {
                number: pull.number,
            };
            activity::append(context, target, &merge.author, Action::Merged)?;
            webhooks::pull(context, &pull, webhooks::PullAction::Closed)?;
            links::complete_closure(context, pull.number, merge.request_id, &merge.author)?;
        }
        Ok(MutationResult::Success(
            TransitionPullMergeOutcome::Applied(Box::new(pull)),
        ))
    }
}

pub(crate) struct GetPullMergeSubmission;
impl Read for GetPullMergeSubmission {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 23;
    type Input = PullSubmissionKey;
    type Output = Option<PullMerge>;
    fn execute(
        context: &mut Transaction<'_>,
        key: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        let pull = key.pull.ok_or(crate::cells::Error::Invalid(
            "repository merge submission scope is invalid",
        ))?;
        let result = context.sql(&SqlBatch { statements: vec![statement("SELECT merge_record FROM repository_pull_merge_submissions WHERE pull_number = ? AND request_id = ?", vec![integer(pull)?, SqlValue::Blob(key.submission_id.to_vec())])] })?;
        result[0]
            .rows
            .first()
            .map(|row| decode_record(result_blob(row, 0)?))
            .transpose()
    }
}

fn merge_digest(pull: u64, merge: &PullMerge) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.pull-merge-submission.v1\0");
    hasher.update(&pull.to_be_bytes());
    hash_text(&mut hasher, &merge.author.issuer);
    hash_text(&mut hasher, &merge.author.subject);
    hasher.update(&[match merge.method {
        MergeMethod::FastForward => 0,
        MergeMethod::MergeCommit => 1,
        MergeMethod::Squash => 2,
        MergeMethod::Rebase => 3,
    }]);
    hasher.update(&merge.pull_version.to_be_bytes());
    for value in [
        &merge.base_oid,
        &merge.head_oid,
        &merge.commit_oid,
        &merge.message,
    ] {
        hash_text(&mut hasher, value);
    }
    if let Some(plan) = &merge.publication_plan {
        hash_text(&mut hasher, plan);
    }
    let scoped = merge
        .bypass_rulesets
        .iter()
        .any(|id| matches!(id, rulesets::RulesetId::Organization { .. }));
    if !merge.bypass_rulesets.is_empty() {
        hasher.update(if scoped {
            b"ruleset-bypass/v2\0"
        } else {
            b"ruleset-bypass\0"
        });
        for id in &merge.bypass_rulesets {
            match id {
                rulesets::RulesetId::Repository(id) => {
                    if scoped {
                        hasher.update(&[0]);
                    }
                    hasher.update(id.as_bytes());
                }
                rulesets::RulesetId::Organization {
                    organization,
                    ruleset,
                } => {
                    hasher.update(&[1]);
                    hasher.update(organization.as_bytes());
                    hasher.update(ruleset.as_bytes());
                }
            }
        }
    }
    if merge.tested_candidate {
        hasher.update(b"tested-candidate\0");
    }
    hasher.finalize()
}
fn validate_merge(merge: &PullMerge) -> crate::cells::Result<()> {
    validate_author(&merge.author)?;
    validate_number(merge.pull_version)?;
    validate_oid(&merge.base_oid)?;
    validate_oid(&merge.head_oid)?;
    validate_oid(&merge.commit_oid)?;
    if merge.tested_candidate
        && (!merge.method.supports_queue()
            || merge.publication_plan.is_none()
            || !merge.message.is_empty()
            || !merge.bypass_rulesets.is_empty())
    {
        return Err(crate::cells::Error::Invalid(
            "invalid tested merge candidate",
        ));
    }
    if !rulesets::valid_bypass_ids(&merge.bypass_rulesets) {
        return Err(crate::cells::Error::Invalid("invalid merge ruleset bypass"));
    }
    if merge.publication_plan.as_ref().is_some_and(|plan| {
        plan.len() != 64
            || !plan
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }) {
        return Err(crate::cells::Error::Invalid(
            "repository pull publication plan is invalid",
        ));
    }
    if merge.message.chars().count() > 256
        || merge
            .message
            .chars()
            .any(|character| character.is_control() && character != '\n')
    {
        return Err(crate::cells::Error::Invalid(
            "repository pull merge message is invalid",
        ));
    }
    Ok(())
}
