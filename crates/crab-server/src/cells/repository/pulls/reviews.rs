use super::*;
use crate::cells::repository::collaboration::{
    Target,
    activity::{self, Action},
};

pub(crate) struct CreatePullReview;

impl Mutation for CreatePullReview {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 20;
    type Input = CreatePullReviewInput;
    type Output = CreatePullReviewOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(CreatePullReviewOutcome::Archived));
        }
        validate_number(input.pull)?;
        validate_author(&input.author)?;
        validate_body(&input.body, input.state != ReviewState::Approved)?;
        validate_oid(&input.commit_oid)?;
        let digest = pull_child_digest(
            b"crab.repository.pull-review-submission.v1\0",
            input.pull,
            &input.author,
            &input.body,
            Some(input.state.code()),
            Some(&input.commit_oid),
        );
        let existing = context.sql(&SqlBatch { statements: vec![statement("SELECT payload_digest, review_number FROM repository_pull_review_submissions WHERE pull_number = ? AND request_id = ?", vec![integer(input.pull)?, SqlValue::Blob(input.submission_id.to_vec())])] })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(MutationResult::Rejected(
                    CreatePullReviewOutcome::RequestConflict,
                ));
            }
            let record = load_review(
                context,
                PullChildKey {
                    pull: input.pull,
                    number: result_u64_from_row(row, 1)?,
                },
            )?
            .ok_or(crate::cells::Error::Invalid(
                "repository pull review submission has no review",
            ))?;
            return Ok(MutationResult::Success(CreatePullReviewOutcome::Created(
                record,
            )));
        }
        super::super::search::index::ensure_ready(context)?;
        let Some(mut pull) = load_pull(context, input.pull)? else {
            return Ok(MutationResult::Rejected(
                CreatePullReviewOutcome::PullNotFound,
            ));
        };
        if pull.state != PullState::Open {
            return Ok(MutationResult::Rejected(
                CreatePullReviewOutcome::PullClosed,
            ));
        }
        if pull.merge_pending.is_some() {
            return Ok(MutationResult::Rejected(
                CreatePullReviewOutcome::MergePending,
            ));
        }
        if input.state != ReviewState::Commented && same_author(&pull.author, &input.author) {
            return Ok(MutationResult::Rejected(CreatePullReviewOutcome::OwnReview));
        }
        if input.state != ReviewState::Commented
            && !pull
                .review_decisions
                .iter()
                .any(|decision| same_author(&decision.author, &input.author))
            && pull.review_decisions.len() as u64 >= MAX_REVIEW_DECISIONS
        {
            return Ok(MutationResult::Rejected(
                CreatePullReviewOutcome::DecisionLimit,
            ));
        }
        let number = next_child_number(context, "repository_pull_review_sequences", input.pull)?;
        let now = timestamp(context.now_ms())?;
        let record = PullReviewRecord {
            pull: input.pull,
            number,
            author: input.author,
            body: input.body,
            state: input.state,
            commit_oid: input.commit_oid,
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let mut statements = vec![
            statement(
                "INSERT INTO repository_pull_review_submissions(pull_number, request_id, payload_digest, review_number) VALUES (?, ?, ?, ?)",
                vec![
                    integer(record.pull)?,
                    SqlValue::Blob(input.submission_id.to_vec()),
                    SqlValue::Blob(digest.as_bytes().to_vec()),
                    integer(record.number)?,
                ],
            ),
            statement(
                "INSERT INTO repository_pull_reviews(pull_number, number, author_issuer, author_subject, author_name, body, state, commit_oid, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    integer(record.pull)?,
                    integer(record.number)?,
                    SqlValue::Text(record.author.issuer.clone()),
                    SqlValue::Text(record.author.subject.clone()),
                    SqlValue::Text(record.author.name.clone()),
                    SqlValue::Text(record.body.clone()),
                    SqlValue::Integer(i64::from(record.state.code())),
                    SqlValue::Text(record.commit_oid.clone()),
                    integer(record.version)?,
                    integer(record.created_at_ms)?,
                    integer(record.updated_at_ms)?,
                ],
            ),
        ];
        let fulfilled_request = review_requests::fulfill(context, &record)?;
        if record.state != ReviewState::Commented {
            statements.push(statement("INSERT INTO repository_pull_review_decisions(pull_number, author_issuer, author_subject, review_number, author_name, state, commit_oid) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(pull_number, author_issuer, author_subject) DO UPDATE SET review_number = excluded.review_number, author_name = excluded.author_name, state = excluded.state, commit_oid = excluded.commit_oid", vec![integer(record.pull)?, SqlValue::Text(record.author.issuer.clone()), SqlValue::Text(record.author.subject.clone()), integer(record.number)?, SqlValue::Text(record.author.name.clone()), SqlValue::Integer(i64::from(record.state.code())), SqlValue::Text(record.commit_oid.clone())]));
        }
        if record.state != ReviewState::Commented || fulfilled_request {
            pull.version = next_version(pull.version)?;
            pull.updated_at_ms = now;
            statements.push(statement("UPDATE repository_pulls SET version = ?, updated_at_ms = ? WHERE number = ? AND version = ?", vec![integer(pull.version)?, integer(now)?, integer(pull.number)?, integer(pull.version - 1)?]));
        }
        context.sql(&SqlBatch { statements })?;
        // Comment-only reviews count as participation and fulfill requests too.
        // Record this after submission replay, in the review transaction.
        super::super::search::reviews::reviewed(
            context,
            record.pull,
            record.number,
            &record.author,
        )?;
        advance_revision(context)?;
        let action = match record.state {
            ReviewState::Commented => Action::Commented,
            ReviewState::Approved => Action::Approved,
            ReviewState::ChangesRequested => Action::ChangesRequested,
        };
        let target = Target::PullReview {
            pull: record.pull,
            number: record.number,
        };
        super::super::search::contributions::project(
            context,
            target,
            &record.author,
            &record.body,
        )?;
        activity::append(context, target, &record.author, action)?;
        Ok(MutationResult::Success(CreatePullReviewOutcome::Created(
            record,
        )))
    }
}

pub(crate) struct UpdatePullReview;

impl Mutation for UpdatePullReview {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 21;
    type Input = UpdatePullReviewInput;
    type Output = UpdatePullReviewOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(UpdatePullReviewOutcome::Archived));
        }
        validate_number(input.key.pull)?;
        validate_number(input.key.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        let Some(mut record) = load_review(context, input.key)? else {
            return Ok(MutationResult::Rejected(UpdatePullReviewOutcome::NotFound));
        };
        validate_body(&input.body, record.state != ReviewState::Approved)?;
        if !same_author(&record.author, &input.actor) {
            return Ok(MutationResult::Rejected(UpdatePullReviewOutcome::Forbidden));
        }
        if record.version != input.version {
            return Ok(MutationResult::Rejected(UpdatePullReviewOutcome::Conflict));
        }
        let edited = record.body != input.body;
        record.body = input.body;
        record.version = next_version(record.version)?;
        record.updated_at_ms = timestamp(context.now_ms())?;
        context.sql(&SqlBatch { statements: vec![statement("UPDATE repository_pull_reviews SET body = ?, version = ?, updated_at_ms = ? WHERE pull_number = ? AND number = ? AND version = ?", vec![SqlValue::Text(record.body.clone()), integer(record.version)?, integer(record.updated_at_ms)?, integer(record.pull)?, integer(record.number)?, integer(input.version)?])] })?;
        advance_revision(context)?;
        if edited {
            let target = Target::PullReview {
                pull: record.pull,
                number: record.number,
            };
            crate::cells::repository::search::contributions::edit(context, target, &record.body)?;
            activity::append(context, target, &input.actor, Action::Updated)?;
        }
        Ok(MutationResult::Success(UpdatePullReviewOutcome::Updated(
            record,
        )))
    }
}

pub(crate) struct GetPullReview;
impl Read for GetPullReview {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 20;
    type Input = PullChildKey;
    type Output = Option<PullReviewRecord>;
    fn execute(
        context: &mut Transaction<'_>,
        key: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        load_review_query(context, key)
    }
}
pub(crate) struct ListPullReviews;
impl Read for ListPullReviews {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 21;
    type Input = PullChildListInput;
    type Output = Option<PullReviewPage>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        if load_pull_query(context, input.pull)?.is_none() {
            return Ok(None);
        }
        let rows = list_child_rows(
            context,
            "repository_pull_reviews",
            &input,
            "author_issuer, author_subject, author_name, body, state, commit_oid, version, created_at_ms, updated_at_ms",
        )?;
        let items = rows
            .iter()
            .map(|row| review_from_row(row))
            .collect::<crate::cells::Result<Vec<_>>>()?;
        let (items, next) = bounded_child_page(items, input.limit, |item| item.number)?;
        Ok(Some(PullReviewPage { items, next }))
    }
}
pub(crate) struct GetPullReviewSubmission;
impl Read for GetPullReviewSubmission {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 22;
    type Input = PullSubmissionKey;
    type Output = Option<PullReviewRecord>;
    fn execute(
        context: &mut Transaction<'_>,
        key: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        let pull = key.pull.ok_or(crate::cells::Error::Invalid(
            "repository review submission scope is invalid",
        ))?;
        let result = context.sql(&SqlBatch { statements: vec![statement("SELECT review_number FROM repository_pull_review_submissions WHERE pull_number = ? AND request_id = ?", vec![integer(pull)?, SqlValue::Blob(key.submission_id.to_vec())])] })?;
        result[0]
            .rows
            .first()
            .map(|row| result_u64_from_row(row, 0))
            .transpose()?
            .map(|number| load_review_query(context, PullChildKey { pull, number }))
            .transpose()
            .map(Option::flatten)
    }
}
