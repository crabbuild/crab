use super::*;
use crate::cells::repository::collaboration::{
    Target,
    activity::{self, Action},
};

pub(crate) struct CreatePull;

impl Mutation for CreatePull {
    const ID: u32 = 16;
    type Input = CreatePullInput;
    type Output = CreatePullOutcome;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        validate_create_pull(&input)?;
        let digest = create_pull_digest(&input);
        let existing = context.sql(&SqlBatch { statements: vec![statement(
            "SELECT payload_digest, pull_number FROM repository_pull_submissions WHERE request_id = ?",
            vec![SqlValue::Blob(input.submission_id.to_vec())],
        )] })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(MutationResult::Rejected(CreatePullOutcome::RequestConflict));
            }
            return Ok(MutationResult::Success(CreatePullOutcome::Created(
                Box::new(load_pull(context, result_u64_from_row(row, 1)?)?.ok_or(
                    crate::cells::Error::Invalid("repository pull submission has no pull"),
                )?),
            )));
        }
        let sequence = context.sql(&SqlBatch { statements: vec![
            statement("UPDATE repository_sequences SET last = last + 1 WHERE kind = 'pull' AND last < 9007199254740991", vec![]),
            statement("SELECT last FROM repository_sequences WHERE kind = 'pull'", vec![]),
        ] })?;
        if sequence[0].rows_affected != 1 {
            return Err(crate::cells::Error::Invalid(
                "repository pull numbering is exhausted",
            ));
        }
        let now = timestamp(context.now_ms())?;
        let record = PullRecord {
            number: result_u64(&sequence, 1, 0)?,
            create_submission_id: input.submission_id,
            author: input.author,
            title: input.title,
            body: input.body,
            state: PullState::Open,
            base_ref: input.base_ref,
            base_oid: input.base_oid,
            head_ref: input.head_ref,
            head_oid: input.head_oid,
            label_ids: vec![],
            assignee_subjects: vec![],
            merge_pending: None,
            merge: None,
            review_decisions: vec![],
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch { statements: vec![
            statement("INSERT INTO repository_pull_submissions(request_id, payload_digest, pull_number) VALUES (?, ?, ?)", vec![SqlValue::Blob(record.create_submission_id.to_vec()), SqlValue::Blob(digest.as_bytes().to_vec()), integer(record.number)?]),
            insert_pull_statement(&record)?,
        ] })?;
        advance_revision(context)?;
        let target = Target::Pull {
            number: record.number,
        };
        activity::append(context, target, &record.author, Action::Created)?;
        Ok(MutationResult::Success(CreatePullOutcome::Created(
            Box::new(record),
        )))
    }
}

pub(crate) struct UpdatePull;

impl Mutation for UpdatePull {
    const ID: u32 = 17;
    type Input = UpdatePullInput;
    type Output = UpdatePullOutcome;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        validate_number(input.number)?;
        validate_author(&input.actor)?;
        validate_number(input.version)?;
        if let Some(title) = &input.title {
            validate_title(title)?;
        }
        if let Some(body) = &input.body {
            validate_body(body, false)?;
        }
        if let Some(labels) = &input.label_ids {
            validate_label_ids(labels)?;
        }
        if let Some(assignees) = &input.assignee_subjects {
            validate_assignees(assignees)?;
        }
        let Some(mut pull) = load_pull(context, input.number)? else {
            return Ok(MutationResult::Rejected(UpdatePullOutcome::NotFound));
        };
        let was_closed = pull.state != PullState::Open;
        let edited = input
            .title
            .as_ref()
            .is_some_and(|value| value != &pull.title)
            || input.body.as_ref().is_some_and(|value| value != &pull.body)
            || input
                .label_ids
                .as_ref()
                .is_some_and(|value| value != &pull.label_ids)
            || input
                .assignee_subjects
                .as_ref()
                .is_some_and(|value| value != &pull.assignee_subjects);
        if pull.version != input.version {
            return Ok(MutationResult::Rejected(UpdatePullOutcome::Conflict));
        }
        if pull.merge_pending.is_some() {
            return Ok(MutationResult::Rejected(UpdatePullOutcome::MergePending));
        }
        let author = same_author(&pull.author, &input.actor);
        if (input.title.is_some() || input.body.is_some()) && !author
            || input.state.is_some() && !(author || input.can_manage)
            || (input.label_ids.is_some() || input.assignee_subjects.is_some()) && !input.can_manage
        {
            return Ok(MutationResult::Rejected(UpdatePullOutcome::Forbidden));
        }
        if input.state == Some(PullState::Merged) || pull.merge.is_some() && input.state.is_some() {
            return Ok(MutationResult::Rejected(UpdatePullOutcome::InvalidState));
        }
        if let Some(labels) = &input.label_ids {
            for label in labels {
                let found = context.sql(&SqlBatch { statements: vec![statement("SELECT 1 FROM repository_labels WHERE number = ? AND deleted_version IS NULL", vec![integer(*label)?])] })?;
                if found[0].rows.is_empty() {
                    return Ok(MutationResult::Rejected(UpdatePullOutcome::InvalidLabel));
                }
            }
        }
        if let Some(value) = input.title {
            pull.title = value;
        }
        if let Some(value) = input.body {
            pull.body = value;
        }
        if let Some(value) = input.state {
            pull.state = value;
        }
        if let Some(value) = input.label_ids {
            pull.label_ids = value;
        }
        if let Some(value) = input.assignee_subjects {
            pull.assignee_subjects = value;
        }
        pull.version = pull
            .version
            .checked_add(1)
            .filter(|value| *value < MAX_NUMBER)
            .ok_or(crate::cells::Error::Invalid(
                "repository pull version is exhausted",
            ))?;
        pull.updated_at_ms = timestamp(context.now_ms())?;
        update_pull_row(context, &pull, input.version)?;
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
        let target = Target::Pull {
            number: pull.number,
        };
        if edited {
            activity::append(context, target, &input.actor, Action::Updated)?;
        }
        if was_closed != (pull.state != PullState::Open) {
            let action = if pull.state == PullState::Open {
                Action::Reopened
            } else {
                Action::Closed
            };
            activity::append(context, target, &input.actor, action)?;
        }
        Ok(MutationResult::Success(UpdatePullOutcome::Updated(
            Box::new(pull),
        )))
    }
}

pub(crate) struct GetPull;

impl Read for GetPull {
    const ID: u32 = 15;
    type Input = u64;
    type Output = Option<PullRecord>;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        number: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_number(number)?;
        load_pull_query(context, number)
    }
}

pub(crate) struct GetPullSubmission;

impl Read for GetPullSubmission {
    const ID: u32 = 16;
    type Input = PullSubmissionKey;
    type Output = Option<PullRecord>;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        key: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        if key.pull.is_some() {
            return Err(crate::cells::Error::Invalid(
                "repository pull submission scope is invalid",
            ));
        }
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT pull_number FROM repository_pull_submissions WHERE request_id = ?",
                vec![SqlValue::Blob(key.submission_id.to_vec())],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| result_u64_from_row(row, 0))
            .transpose()?
            .map(|number| load_pull_query(context, number))
            .transpose()
            .map(Option::flatten)
    }
}

pub(crate) struct ListPulls;

impl Read for ListPulls {
    const ID: u32 = 17;
    type Input = PullListInput;
    type Output = PullPage;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_list(input.before, input.limit)?;
        validate_query(input.query.as_deref())?;
        if input.state > 2 {
            return Err(crate::cells::Error::Invalid(
                "repository pull list state is invalid",
            ));
        }
        let upper = input.before.map_or(MAX_NUMBER, |value| value - 1);
        let result = context.sql(&SqlBatch { statements: vec![statement(
            "SELECT number, author_issuer, author_subject, author_name, title, state, base_ref, head_ref, label_ids, assignee_subjects, created_at_ms, updated_at_ms, body FROM repository_pulls WHERE number <= ? ORDER BY number DESC LIMIT 200",
            vec![integer(upper)?],
        )] })?;
        let mut items = Vec::new();
        let mut examined = 0_u64;
        let mut last_examined = None;
        for row in &result[0].rows {
            examined += 1;
            last_examined = Some(result_u64_from_row(row, 0)?);
            let state = PullState::from_code(result_u64_from_row(row, 5)?)?;
            let matches_state = input.state == 2
                || input.state == 0 && state == PullState::Open
                || input.state == 1 && state != PullState::Open;
            let title = result_text(row, 4)?;
            let body = result_text(row, 12)?;
            let author_name = result_text(row, 3)?;
            let matches_text = input.query.as_ref().is_none_or(|query| {
                [&title, &body, &author_name]
                    .iter()
                    .any(|value| value.to_lowercase().contains(query))
            });
            if matches_state && matches_text {
                items.push(PullSummary {
                    number: result_u64_from_row(row, 0)?,
                    author: author_from_row(row, 1)?,
                    title: result_text(row, 4)?,
                    state,
                    base_ref: result_text(row, 6)?,
                    head_ref: result_text(row, 7)?,
                    label_ids: decode_label_ids(result_blob(row, 8)?)?,
                    assignee_subjects: decode_assignees(result_blob(row, 9)?)?,
                    created_at_ms: result_u64_from_row(row, 10)?,
                    updated_at_ms: result_u64_from_row(row, 11)?,
                });
            }
            if items.len() == usize::from(input.limit) {
                break;
            }
        }
        let next = (examined == MAX_LIST_SCAN || items.len() == usize::from(input.limit))
            .then_some(last_examined)
            .flatten();
        Ok(PullPage { items, next })
    }
}

fn validate_create_pull(input: &CreatePullInput) -> crate::cells::Result<()> {
    validate_author(&input.author)?;
    validate_title(&input.title)?;
    validate_body(&input.body, false)?;
    validate_ref(&input.base_ref)?;
    validate_ref(&input.head_ref)?;
    validate_oid(&input.base_oid)?;
    validate_oid(&input.head_oid)?;
    if input.base_ref == input.head_ref || input.base_oid == input.head_oid {
        return Err(crate::cells::Error::Invalid(
            "repository pull branches are invalid",
        ));
    }
    Ok(())
}

fn validate_ref(value: &str) -> crate::cells::Result<()> {
    if !value.starts_with("refs/heads/")
        || value == "refs/heads/"
        || value.len() > 1_024
        || value.chars().any(char::is_control)
    {
        return Err(crate::cells::Error::Invalid(
            "repository pull ref is invalid",
        ));
    }
    Ok(())
}

pub(super) fn validate_oid(value: &str) -> crate::cells::Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || value.bytes().all(|byte| byte == b'0')
    {
        return Err(crate::cells::Error::Invalid(
            "repository pull object ID is invalid",
        ));
    }
    Ok(())
}

fn create_pull_digest(input: &CreatePullInput) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.pull-submission.v1\0");
    hash_text(&mut hasher, &input.author.issuer);
    hash_text(&mut hasher, &input.author.subject);
    for value in [
        &input.title,
        &input.body,
        &input.base_ref,
        &input.base_oid,
        &input.head_ref,
        &input.head_oid,
    ] {
        hash_text(&mut hasher, value);
    }
    hasher.finalize()
}
