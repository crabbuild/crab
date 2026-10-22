use super::*;
use crate::cells::repository::collaboration::{
    Target,
    activity::{self, Action},
};

pub(crate) struct CreatePullComment;

impl Mutation for CreatePullComment {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 18;
    type Input = CreatePullCommentInput;
    type Output = CreatePullCommentOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(CreatePullCommentOutcome::Archived));
        }
        validate_number(input.pull)?;
        validate_author(&input.author)?;
        validate_body(&input.body, true)?;
        if load_pull(context, input.pull)?.is_none() {
            return Ok(MutationResult::Rejected(
                CreatePullCommentOutcome::PullNotFound,
            ));
        }
        let digest = pull_child_digest(
            b"crab.repository.pull-comment-submission.v1\0",
            input.pull,
            &input.author,
            &input.body,
            None,
            None,
        );
        let existing = context.sql(&SqlBatch { statements: vec![statement("SELECT payload_digest, comment_number FROM repository_pull_comment_submissions WHERE pull_number = ? AND request_id = ?", vec![integer(input.pull)?, SqlValue::Blob(input.submission_id.to_vec())])] })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(MutationResult::Rejected(
                    CreatePullCommentOutcome::RequestConflict,
                ));
            }
            let record = load_comment(
                context,
                PullChildKey {
                    pull: input.pull,
                    number: result_u64_from_row(row, 1)?,
                },
            )?
            .ok_or(crate::cells::Error::Invalid(
                "repository pull comment submission has no comment",
            ))?;
            return Ok(MutationResult::Success(CreatePullCommentOutcome::Created(
                record,
            )));
        }
        let number = next_child_number(context, "repository_pull_comment_sequences", input.pull)?;
        let now = timestamp(context.now_ms())?;
        let record = PullCommentRecord {
            pull: input.pull,
            number,
            author: input.author,
            body: input.body,
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch { statements: vec![
            statement("INSERT INTO repository_pull_comment_submissions(pull_number, request_id, payload_digest, comment_number) VALUES (?, ?, ?, ?)", vec![integer(record.pull)?, SqlValue::Blob(input.submission_id.to_vec()), SqlValue::Blob(digest.as_bytes().to_vec()), integer(record.number)?]),
            statement("INSERT INTO repository_pull_comments(pull_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)", vec![integer(record.pull)?, integer(record.number)?, SqlValue::Text(record.author.issuer.clone()), SqlValue::Text(record.author.subject.clone()), SqlValue::Text(record.author.name.clone()), SqlValue::Text(record.body.clone()), integer(record.version)?, integer(record.created_at_ms)?, integer(record.updated_at_ms)?]),
        ] })?;
        advance_revision(context)?;
        let target = Target::PullComment {
            pull: record.pull,
            number: record.number,
        };
        super::super::search::contributions::project(
            context,
            target,
            &record.author,
            &record.body,
        )?;
        activity::append(context, target, &record.author, Action::Created)?;
        Ok(MutationResult::Success(CreatePullCommentOutcome::Created(
            record,
        )))
    }
}

pub(crate) struct UpdatePullComment;

impl Mutation for UpdatePullComment {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 19;
    type Input = UpdatePullCommentInput;
    type Output = UpdatePullCommentOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(UpdatePullCommentOutcome::Archived));
        }
        validate_number(input.key.pull)?;
        validate_number(input.key.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        validate_body(&input.body, true)?;
        let Some(mut record) = load_comment(context, input.key)? else {
            return Ok(MutationResult::Rejected(UpdatePullCommentOutcome::NotFound));
        };
        if !same_author(&record.author, &input.actor) {
            return Ok(MutationResult::Rejected(
                UpdatePullCommentOutcome::Forbidden,
            ));
        }
        if record.version != input.version {
            return Ok(MutationResult::Rejected(UpdatePullCommentOutcome::Conflict));
        }
        let edited = record.body != input.body;
        record.body = input.body;
        record.version = next_version(record.version)?;
        record.updated_at_ms = timestamp(context.now_ms())?;
        context.sql(&SqlBatch { statements: vec![statement("UPDATE repository_pull_comments SET body = ?, version = ?, updated_at_ms = ? WHERE pull_number = ? AND number = ? AND version = ?", vec![SqlValue::Text(record.body.clone()), integer(record.version)?, integer(record.updated_at_ms)?, integer(record.pull)?, integer(record.number)?, integer(input.version)?])] })?;
        advance_revision(context)?;
        if edited {
            let target = Target::PullComment {
                pull: record.pull,
                number: record.number,
            };
            crate::cells::repository::search::contributions::edit(context, target, &record.body)?;
            activity::append(context, target, &input.actor, Action::Updated)?;
        }
        Ok(MutationResult::Success(UpdatePullCommentOutcome::Updated(
            record,
        )))
    }
}

pub(crate) struct GetPullComment;
impl Read for GetPullComment {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 18;
    type Input = PullChildKey;
    type Output = Option<PullCommentRecord>;
    fn execute(
        context: &mut Transaction<'_>,
        key: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        load_comment_query(context, key)
    }
}
pub(crate) struct ListPullComments;
impl Read for ListPullComments {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 19;
    type Input = PullChildListInput;
    type Output = Option<PullCommentPage>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        if load_pull_query(context, input.pull)?.is_none() {
            return Ok(None);
        }
        let rows = list_child_rows(
            context,
            "repository_pull_comments",
            &input,
            "author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms",
        )?;
        let items = rows
            .iter()
            .map(|row| comment_from_row(row))
            .collect::<crate::cells::Result<Vec<_>>>()?;
        let (items, next) = bounded_child_page(items, input.limit, |item| item.number)?;
        Ok(Some(PullCommentPage { items, next }))
    }
}
