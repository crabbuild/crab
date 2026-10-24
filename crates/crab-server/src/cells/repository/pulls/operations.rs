use super::*;
use crate::cells::repository::collaboration::{
    Target,
    activity::{self, Action},
};

pub(crate) struct CreatePull;

impl Mutation for CreatePull {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 16;
    type Input = CreatePullInput;
    type Output = CreatePullOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(CreatePullOutcome::Archived));
        }
        lifecycle::ensure_ready(context)?;
        validate_create_pull(&input)?;
        input.metadata.selection.validate()?;
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
        use crate::cells::repository::collaboration::{Subject, SubjectKind, creation};
        let comparison = Some(creation::Comparison {
            base_oid: input.base_oid.clone(),
            head_oid: input.head_oid.clone(),
        });
        match input.metadata.resolve(
            context,
            SubjectKind::Pull,
            input.submission_id,
            digest,
            comparison,
        )? {
            creation::Resolution::Ready => {}
            creation::Resolution::Rejected(reason) => {
                return Ok(MutationResult::Success(
                    CreatePullOutcome::MetadataRejected(reason),
                ));
            }
            creation::Resolution::Conflict => {
                return Ok(MutationResult::Rejected(CreatePullOutcome::RequestConflict));
            }
        }
        let milestone = input.metadata.selection.milestone;
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
            draft: input.draft,
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
            label_ids: input.metadata.selection.label_ids,
            assignees: input.metadata.eligible,
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
        lifecycle::create(context, record.number)?;
        let state = if input.draft {
            PullReadiness::Draft
        } else {
            PullReadiness::Ready
        };
        readiness::store(context, record.number, state)?;
        advance_revision(context)?;
        let target = Target::Pull {
            number: record.number,
        };
        activity::append(context, target, &record.author, Action::Created)?;
        creation::assign_milestone(
            context,
            Subject {
                kind: SubjectKind::Pull,
                number: record.number,
            },
            &record.author,
            milestone,
        )?;
        Ok(MutationResult::Success(CreatePullOutcome::Created(
            Box::new(record),
        )))
    }
}

pub(crate) struct UpdatePull;

impl Mutation for UpdatePull {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 17;
    type Input = UpdatePullInput;
    type Output = UpdatePullOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(UpdatePullOutcome::Archived));
        }
        lifecycle::ensure_ready(context)?;
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
        if let Some(assignees) = &input.assignees {
            assignments::validate(assignees)?;
        }
        let Some(mut pull) = load_pull(context, input.number)? else {
            return Ok(MutationResult::Rejected(UpdatePullOutcome::NotFound));
        };
        let was_state = pull.state;
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
                .assignees
                .as_ref()
                .is_some_and(|value| value != &pull.assignees);
        if pull.version != input.version {
            return Ok(MutationResult::Rejected(UpdatePullOutcome::Conflict));
        }
        if pull.merge_pending.is_some() {
            return Ok(MutationResult::Rejected(UpdatePullOutcome::MergePending));
        }
        let author = same_author(&pull.author, &input.actor);
        if (input.title.is_some() || input.body.is_some()) && !author
            || input.state.is_some() && !(author || input.can_triage)
            || (input.label_ids.is_some() || input.assignees.is_some()) && !input.can_triage
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
        if let Some(value) = input.assignees {
            pull.assignees = value;
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
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 15;
    type Input = u64;
    type Output = Option<PullRecord>;

    fn execute(
        context: &mut Transaction<'_>,
        number: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_number(number)?;
        load_pull_query(context, number)
    }
}

pub(crate) struct GetPullSubmission;

impl Read for GetPullSubmission {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 16;
    type Input = PullSubmissionKey;
    type Output = Option<crate::cells::repository::collaboration::creation::Comparison>;

    fn execute(
        context: &mut Transaction<'_>,
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
        let record = result[0]
            .rows
            .first()
            .map(|row| result_u64_from_row(row, 0))
            .transpose()?
            .map(|number| load_pull_query(context, number))
            .transpose()
            .map(Option::flatten)?;
        if let Some(record) = record {
            return Ok(Some(
                crate::cells::repository::collaboration::creation::Comparison {
                    base_oid: record.base_oid,
                    head_oid: record.head_oid,
                },
            ));
        }
        // A rejected creation also keeps its original tips so branch movement
        // cannot rebind an unknown response to a different comparison.
        crate::cells::repository::collaboration::creation::rejected_comparison(
            context,
            key.submission_id,
        )
    }
}

pub(crate) struct ListPulls;
impl Read for ListPulls {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 17;
    type Input = SearchInput;
    type Output = search::index::Page<PullSummary>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        search::index::page(
            context,
            input,
            Some(collaboration::SubjectKind::Pull),
            |context, subject, metrics| PullSummary::load(context, subject.number, metrics),
        )
    }
}

impl PullSummary {
    pub(in crate::cells::repository) fn load(
        context: &Transaction<'_>,
        number: u64,
        metrics: search::index::Metrics,
    ) -> crate::cells::Result<Self> {
        let result = context.sql(&SqlBatch { statements: vec![statement("SELECT number, author_issuer, author_subject, author_name, title, state, base_ref, head_ref, label_ids, assignees, version, created_at_ms, updated_at_ms FROM repository_pulls WHERE number = ?", vec![integer(number)?])] })?;
        let row = result[0].rows.first().ok_or(crate::cells::Error::Invalid(
            "indexed pull request is missing",
        ))?;
        let state = PullState::from_code(result_u64_from_row(row, 5)?)?;
        let lifecycle = lifecycle::get(context, number, state)?;
        Ok(PullSummary {
            number: result_u64_from_row(row, 0)?,
            author: author_from_row(row, 1)?,
            title: result_text(row, 4)?,
            state,
            closed_at_ms: lifecycle.closed_at_ms,
            merged_at_ms: lifecycle.merged_at_ms,
            base_ref: result_text(row, 6)?,
            head_ref: result_text(row, 7)?,
            label_ids: decode_label_ids(result_blob(row, 8)?)?,
            assignees: assignments::decode(result_blob(row, 9)?)?,
            version: result_u64_from_row(row, 10)?,
            created_at_ms: result_u64_from_row(row, 11)?,
            updated_at_ms: result_u64_from_row(row, 12)?,
            draft: readiness::load(context, number)? == PullReadiness::Draft,
            comment_count: metrics.comments,
            reaction_count: metrics.reactions,
        })
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
    // A default-ready request retains its durable submission identity.
    // Draft adds a tagged contribution so reusing an ID for another mode conflicts.
    if input.draft {
        hasher.update(b"draft\0");
    }
    input.metadata.selection.hash(&mut hasher);
    hasher.finalize()
}
