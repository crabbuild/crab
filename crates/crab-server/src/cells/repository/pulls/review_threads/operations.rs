use super::*;
use crate::cells::repository::collaboration::{
    Target,
    activity::{self, Action},
};

pub(crate) struct CreatePullReviewThread;

impl Mutation for CreatePullReviewThread {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 32;
    type Input = CreatePullReviewThreadInput;
    type Output = CreatePullReviewThreadOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(
                CreatePullReviewThreadOutcome::Archived,
            ));
        }
        validate_create_thread(&input)?;
        let digest = thread_digest(&input);
        let existing = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT payload_digest, thread_number FROM repository_pull_review_thread_submissions WHERE pull_number = ? AND request_id = ?",
                vec![
                    integer(input.pull)?,
                    SqlValue::Blob(input.submission_id.to_vec()),
                ],
            )],
        })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(MutationResult::Rejected(
                    CreatePullReviewThreadOutcome::RequestConflict,
                ));
            }
            let record = load_thread(
                context,
                PullReviewThreadKey {
                    pull: input.pull,
                    number: result_u64_from_row(row, 1)?,
                },
            )?
            .ok_or(crate::cells::Error::Invalid(
                "repository pull review thread submission has no thread",
            ))?;
            return Ok(MutationResult::Success(
                CreatePullReviewThreadOutcome::Created(Box::new(record)),
            ));
        }
        let state = pull_state(context, input.pull)?;
        let Some(state) = state else {
            return Ok(MutationResult::Rejected(
                CreatePullReviewThreadOutcome::PullNotFound,
            ));
        };
        if state != 0 {
            return Ok(MutationResult::Rejected(
                CreatePullReviewThreadOutcome::PullClosed,
            ));
        }
        let number = next_number(
            context,
            "repository_pull_review_thread_sequences",
            input.pull,
        )?;
        let now = timestamp(context.now_ms())?;
        let record = PullReviewThreadRecord {
            pull: input.pull,
            number,
            author: input.author,
            body: input.body,
            suggested_text: input.suggested_text,
            base_oid: input.base_oid,
            head_oid: input.head_oid,
            path: input.path,
            old_blob_oid: input.old_blob_oid,
            new_blob_oid: input.new_blob_oid,
            side: input.side,
            start_line: input.start_line,
            end_line: input.end_line,
            resolved: false,
            resolved_by: None,
            resolved_at_ms: None,
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch {
            statements: vec![
                statement(
                    "INSERT INTO repository_pull_review_thread_submissions(pull_number, request_id, payload_digest, thread_number) VALUES (?, ?, ?, ?)",
                    vec![
                        integer(record.pull)?,
                        SqlValue::Blob(input.submission_id.to_vec()),
                        SqlValue::Blob(digest.as_bytes().to_vec()),
                        integer(record.number)?,
                    ],
                ),
                insert_thread_statement(&record)?,
            ],
        })?;
        advance_revision(context)?;
        let target = Target::PullThread {
            pull: record.pull,
            number: record.number,
        };
        crate::cells::repository::search::contributions::project(
            context,
            target,
            &record.author,
            &record.body,
        )?;
        activity::append(context, target, &record.author, Action::Created)?;
        Ok(MutationResult::Success(
            CreatePullReviewThreadOutcome::Created(Box::new(record)),
        ))
    }
}

pub(crate) struct UpdatePullReviewThread;

impl Mutation for UpdatePullReviewThread {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 33;
    type Input = UpdatePullReviewThreadInput;
    type Output = UpdatePullReviewThreadOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(
                UpdatePullReviewThreadOutcome::Archived,
            ));
        }
        validate_number(input.key.pull)?;
        validate_number(input.key.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        if input.body.is_none() && input.suggested_text.is_none() && input.resolved.is_none() {
            return Err(crate::cells::Error::Invalid(
                "repository pull review thread update is empty",
            ));
        }
        let Some(mut record) = load_thread(context, input.key.clone())? else {
            return Ok(MutationResult::Rejected(
                UpdatePullReviewThreadOutcome::NotFound,
            ));
        };
        if input.body.is_some() || input.suggested_text.is_some() {
            if !same_author(&record.author, &input.actor) {
                return Ok(MutationResult::Rejected(
                    UpdatePullReviewThreadOutcome::Forbidden,
                ));
            }
            if let Some(body) = &input.body {
                validate_body(body, true)?;
            }
            if let Some(suggested_text) = &input.suggested_text {
                validate_suggested_text(suggested_text.as_deref())?;
                if record.side == PullReviewThreadSide::Old && suggested_text.is_some() {
                    return Err(crate::cells::Error::Invalid(
                        "old pull review thread anchors cannot have suggestions",
                    ));
                }
            }
        }
        if input.resolved.is_some() && !input.can_resolve {
            return Ok(MutationResult::Rejected(
                UpdatePullReviewThreadOutcome::Forbidden,
            ));
        }
        if record.version != input.version {
            return Ok(MutationResult::Rejected(
                UpdatePullReviewThreadOutcome::Conflict,
            ));
        }
        let was_resolved = record.resolved;
        let edited = input
            .body
            .as_ref()
            .is_some_and(|value| value != &record.body)
            || input
                .suggested_text
                .as_ref()
                .is_some_and(|value| value != &record.suggested_text);
        let now = timestamp(context.now_ms())?;
        if let Some(body) = input.body {
            record.body = body;
        }
        if let Some(suggested_text) = input.suggested_text {
            record.suggested_text = suggested_text;
        }
        if let Some(resolved) = input.resolved {
            record.resolved = resolved;
            if resolved {
                record.resolved_by = Some(input.actor.clone());
                record.resolved_at_ms = Some(now);
            } else {
                record.resolved_by = None;
                record.resolved_at_ms = None;
            }
        }
        record.version = next_version(record.version)?;
        record.updated_at_ms = now;
        let old_version = input.version;
        context.sql(&SqlBatch {
            statements: vec![update_thread_statement(&record, old_version)?],
        })?;
        advance_revision(context)?;
        let target = Target::PullThread {
            pull: record.pull,
            number: record.number,
        };
        if edited {
            crate::cells::repository::search::contributions::edit(context, target, &record.body)?;
            activity::append(context, target, &input.actor, Action::Updated)?;
        }
        if was_resolved != record.resolved {
            let action = if record.resolved {
                Action::Resolved
            } else {
                Action::Unresolved
            };
            activity::append(context, target, &input.actor, action)?;
        }
        Ok(MutationResult::Success(
            UpdatePullReviewThreadOutcome::Updated(Box::new(record)),
        ))
    }
}

pub(crate) struct CreatePullReviewReply;

impl Mutation for CreatePullReviewReply {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 34;
    type Input = CreatePullReviewReplyInput;
    type Output = CreatePullReviewReplyOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(
                CreatePullReviewReplyOutcome::Archived,
            ));
        }
        validate_number(input.pull)?;
        validate_number(input.thread)?;
        validate_author(&input.author)?;
        validate_body(&input.body, true)?;
        if load_thread(
            context,
            PullReviewThreadKey {
                pull: input.pull,
                number: input.thread,
            },
        )?
        .is_none()
        {
            return Ok(MutationResult::Rejected(
                CreatePullReviewReplyOutcome::ThreadNotFound,
            ));
        }
        let digest = reply_digest(&input);
        let existing = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT payload_digest, reply_number FROM repository_pull_review_reply_submissions WHERE pull_number = ? AND thread_number = ? AND request_id = ?",
                vec![
                    integer(input.pull)?,
                    integer(input.thread)?,
                    SqlValue::Blob(input.submission_id.to_vec()),
                ],
            )],
        })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(MutationResult::Rejected(
                    CreatePullReviewReplyOutcome::RequestConflict,
                ));
            }
            let record = load_reply(
                context,
                PullReviewReplyKey {
                    pull: input.pull,
                    thread: input.thread,
                    number: result_u64_from_row(row, 1)?,
                },
            )?
            .ok_or(crate::cells::Error::Invalid(
                "repository pull review reply submission has no reply",
            ))?;
            return Ok(MutationResult::Success(
                CreatePullReviewReplyOutcome::Created(record),
            ));
        }
        let number = next_reply_number(context, input.pull, input.thread)?;
        let now = timestamp(context.now_ms())?;
        let record = PullReviewReplyRecord {
            pull: input.pull,
            thread: input.thread,
            number,
            author: input.author,
            body: input.body,
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch {
            statements: vec![
                statement(
                    "INSERT INTO repository_pull_review_reply_submissions(pull_number, thread_number, request_id, payload_digest, reply_number) VALUES (?, ?, ?, ?, ?)",
                    vec![
                        integer(record.pull)?,
                        integer(record.thread)?,
                        SqlValue::Blob(input.submission_id.to_vec()),
                        SqlValue::Blob(digest.as_bytes().to_vec()),
                        integer(record.number)?,
                    ],
                ),
                insert_reply_statement(&record)?,
            ],
        })?;
        advance_revision(context)?;
        let target = Target::PullReply {
            pull: record.pull,
            thread: record.thread,
            number: record.number,
        };
        crate::cells::repository::search::contributions::project(
            context,
            target,
            &record.author,
            &record.body,
        )?;
        activity::append(context, target, &record.author, Action::Created)?;
        Ok(MutationResult::Success(
            CreatePullReviewReplyOutcome::Created(record),
        ))
    }
}

pub(crate) struct UpdatePullReviewReply;

impl Mutation for UpdatePullReviewReply {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 35;
    type Input = UpdatePullReviewReplyInput;
    type Output = UpdatePullReviewReplyOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(
                UpdatePullReviewReplyOutcome::Archived,
            ));
        }
        validate_number(input.key.pull)?;
        validate_number(input.key.thread)?;
        validate_number(input.key.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        validate_body(&input.body, true)?;
        let Some(mut record) = load_reply(context, input.key.clone())? else {
            return Ok(MutationResult::Rejected(
                UpdatePullReviewReplyOutcome::NotFound,
            ));
        };
        if !same_author(&record.author, &input.actor) {
            return Ok(MutationResult::Rejected(
                UpdatePullReviewReplyOutcome::Forbidden,
            ));
        }
        if record.version != input.version {
            return Ok(MutationResult::Rejected(
                UpdatePullReviewReplyOutcome::Conflict,
            ));
        }
        let edited = record.body != input.body;
        record.body = input.body;
        record.version = next_version(record.version)?;
        record.updated_at_ms = timestamp(context.now_ms())?;
        context.sql(&SqlBatch {
            statements: vec![statement(
                "UPDATE repository_pull_review_replies SET body = ?, version = ?, updated_at_ms = ? WHERE pull_number = ? AND thread_number = ? AND number = ? AND version = ?",
                vec![
                    SqlValue::Text(record.body.clone()),
                    integer(record.version)?,
                    integer(record.updated_at_ms)?,
                    integer(record.pull)?,
                    integer(record.thread)?,
                    integer(record.number)?,
                    integer(input.version)?,
                ],
            )],
        })?;
        advance_revision(context)?;
        if edited {
            let target = Target::PullReply {
                pull: record.pull,
                thread: record.thread,
                number: record.number,
            };
            crate::cells::repository::search::contributions::edit(context, target, &record.body)?;
            activity::append(context, target, &input.actor, Action::Updated)?;
        }
        Ok(MutationResult::Success(
            UpdatePullReviewReplyOutcome::Updated(record),
        ))
    }
}

pub(crate) struct GetPullReviewThread;

impl Read for GetPullReviewThread {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 29;
    type Input = PullReviewThreadKey;
    type Output = Option<PullReviewThreadRecord>;

    fn execute(
        context: &mut Transaction<'_>,
        key: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_thread_key(&key)?;
        load_thread_query(context, key)
    }
}

pub(crate) struct ListPullReviewThreads;

impl Read for ListPullReviewThreads {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 30;
    type Input = PullReviewThreadListInput;
    type Output = Option<PullReviewThreadPage>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_thread_list(&input)?;
        if !pull_exists_query(context, input.pull)? {
            return Ok(None);
        }
        let rows = list_threads(context, &input)?;
        let items = rows
            .iter()
            .map(|row| thread_from_row(row))
            .collect::<crate::cells::Result<Vec<_>>>()?;
        let (items, next) = bounded_page(items, input.limit, |item| item.number)?;
        Ok(Some(PullReviewThreadPage { items, next }))
    }
}

pub(crate) struct GetPullReviewThreadSubmission;

impl Read for GetPullReviewThreadSubmission {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 31;
    type Input = PullReviewThreadSubmissionKey;
    type Output = Option<PullReviewThreadRecord>;

    fn execute(
        context: &mut Transaction<'_>,
        key: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        let pull = key.pull.ok_or(crate::cells::Error::Invalid(
            "repository pull review thread submission scope is invalid",
        ))?;
        validate_number(pull)?;
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT thread_number FROM repository_pull_review_thread_submissions WHERE pull_number = ? AND request_id = ?",
                vec![integer(pull)?, SqlValue::Blob(key.submission_id.to_vec())],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| result_u64_from_row(row, 0))
            .transpose()?
            .map(|number| load_thread_query(context, PullReviewThreadKey { pull, number }))
            .transpose()
            .map(Option::flatten)
    }
}

pub(crate) struct GetPullReviewReply;

impl Read for GetPullReviewReply {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 32;
    type Input = PullReviewReplyKey;
    type Output = Option<PullReviewReplyRecord>;

    fn execute(
        context: &mut Transaction<'_>,
        key: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_reply_key(&key)?;
        load_reply_query(context, key)
    }
}

pub(crate) struct ListPullReviewReplies;

impl Read for ListPullReviewReplies {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 33;
    type Input = PullReviewReplyListInput;
    type Output = Option<PullReviewReplyPage>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_reply_list(&input)?;
        if !thread_exists_query(context, input.pull, input.thread)? {
            return Ok(None);
        }
        let rows = list_replies(context, &input)?;
        let items = rows
            .iter()
            .map(|row| reply_from_row(row))
            .collect::<crate::cells::Result<Vec<_>>>()?;
        let (items, next) = bounded_page(items, input.limit, |item| item.number)?;
        Ok(Some(PullReviewReplyPage { items, next }))
    }
}

pub(crate) struct GetPullReviewReplySubmission;

impl Read for GetPullReviewReplySubmission {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 34;
    type Input = PullReviewReplySubmissionKey;
    type Output = Option<PullReviewReplyRecord>;

    fn execute(
        context: &mut Transaction<'_>,
        key: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        let pull = key.pull.ok_or(crate::cells::Error::Invalid(
            "repository pull review reply submission scope is invalid",
        ))?;
        let thread = key.thread.ok_or(crate::cells::Error::Invalid(
            "repository pull review reply submission scope is invalid",
        ))?;
        validate_number(pull)?;
        validate_number(thread)?;
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT reply_number FROM repository_pull_review_reply_submissions WHERE pull_number = ? AND thread_number = ? AND request_id = ?",
                vec![
                    integer(pull)?,
                    integer(thread)?,
                    SqlValue::Blob(key.submission_id.to_vec()),
                ],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| result_u64_from_row(row, 0))
            .transpose()?
            .map(|number| {
                load_reply_query(
                    context,
                    PullReviewReplyKey {
                        pull,
                        thread,
                        number,
                    },
                )
            })
            .transpose()
            .map(Option::flatten)
    }
}
