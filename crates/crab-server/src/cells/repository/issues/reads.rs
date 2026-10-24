use super::*;

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct IssueDetail {
    pub(crate) lifecycle: Option<lifecycle::Lifecycle>,
    pub(crate) issue: Option<IssueRecord>,
    pub(crate) labels: LabelCatalog,
}

pub(crate) struct GetIssueDetail;

impl Read for GetIssueDetail {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 36;
    type Input = u64;
    type Output = IssueDetail;

    fn execute(context: &mut Transaction<'_>, number: u64) -> crate::cells::Result<IssueDetail> {
        // Detail metadata shares the issue's observation point and routing
        // decision, so one HTTP request consumes exactly one reader turn.
        lifecycle::ensure_ready(context)?;
        let issue = GetIssue::execute(context, number)?;
        let lifecycle = issue
            .as_ref()
            .map(|issue| lifecycle::get(context, issue.number, issue.state != 0))
            .transpose()?;
        Ok(IssueDetail {
            issue,
            lifecycle,
            labels: ListLabels::execute(context, ())?,
        })
    }
}

pub(crate) struct GetIssue;

impl Read for GetIssue {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 1;
    type Input = u64;
    type Output = Option<IssueRecord>;

    fn execute(
        context: &mut Transaction<'_>,
        number: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_number(number)?;
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignees, version, created_at_ms, updated_at_ms FROM repository_issues WHERE number = ?",
                vec![integer(number)?],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| issue_from_row(row.as_slice()))
            .transpose()
    }
}

pub(crate) struct GetComment;

impl Read for GetComment {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 2;
    type Input = CommentKey;
    type Output = Option<CommentRecord>;

    fn execute(
        context: &mut Transaction<'_>,
        key: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_number(key.issue)?;
        validate_number(key.number)?;
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT issue_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_issue_comments WHERE issue_number = ? AND number = ?",
                vec![integer(key.issue)?, integer(key.number)?],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| comment_from_row(row.as_slice()))
            .transpose()
    }
}

impl IssueSummary {
    pub(in crate::cells::repository) fn load(
        context: &Transaction<'_>,
        number: u64,
        metrics: search::index::Metrics,
    ) -> crate::cells::Result<Self> {
        let result = context.sql(&SqlBatch { statements: vec![statement("SELECT number, author_issuer, author_subject, author_name, title, state, label_ids, assignees, version, created_at_ms, updated_at_ms FROM repository_issues WHERE number = ?", vec![integer(number)?])] })?;
        let row = result[0]
            .rows
            .first()
            .ok_or(crate::cells::Error::Invalid("indexed issue is missing"))?;
        let state = u8::try_from(result_u64_from_row(row, 5)?)
            .map_err(|_| crate::cells::Error::Invalid("indexed issue state is invalid"))?;
        let lifecycle = lifecycle::get(context, number, state != 0)?;
        Ok(IssueSummary {
            lifecycle,
            number: result_u64_from_row(row, 0)?,
            author: RepositoryAuthor {
                issuer: result_text(row, 1)?,
                subject: result_text(row, 2)?,
                name: result_text(row, 3)?,
            },
            title: result_text(row, 4)?,
            state: u8::try_from(result_u64_from_row(row, 5)?)
                .map_err(|_| crate::cells::Error::Invalid("indexed issue state is invalid"))?,
            label_ids: decode_label_ids(result_blob(row, 6)?)?,
            assignees: assignments::decode(result_blob(row, 7)?)?,
            version: result_u64_from_row(row, 8)?,
            created_at_ms: result_u64_from_row(row, 9)?,
            updated_at_ms: result_u64_from_row(row, 10)?,
            comment_count: metrics.comments,
            reaction_count: metrics.reactions,
        })
    }
}

pub(crate) struct ListComments;

impl Read for ListComments {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 4;
    type Input = ListCommentsInput;
    type Output = CommentPage;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_number(input.issue)?;
        validate_list(input.before, input.limit)?;
        let exists = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT 1 FROM repository_issues WHERE number = ?",
                vec![integer(input.issue)?],
            )],
        })?;
        if exists[0].rows.is_empty() {
            return Ok(CommentPage::IssueNotFound);
        }
        let sequence = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT last FROM repository_comment_sequences WHERE issue_number = ?",
                vec![integer(input.issue)?],
            )],
        })?;
        let last = sequence[0]
            .rows
            .first()
            .map(|row| result_u64_from_row(row, 0))
            .transpose()?
            .unwrap_or(0);
        let maximum = input.before.map_or(last, |before| (before - 1).min(last));
        let mut cursor = maximum;
        let mut scanned = 0_u64;
        // Variant tag, collection count and trailing optional cursor are part of
        // the published result bound, not just the encoded comment bodies.
        let mut encoded_bytes = 5_usize;
        let mut items = Vec::new();
        while cursor > 0 && scanned < MAX_LIST_SCAN && items.len() < usize::from(input.limit) {
            let count = 8_u64.min(MAX_LIST_SCAN - scanned).min(cursor);
            let bottom = cursor - count + 1;
            let result = context.sql(&SqlBatch {
                statements: vec![statement(
                    "SELECT issue_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_issue_comments WHERE issue_number = ? AND number BETWEEN ? AND ? ORDER BY number DESC",
                    vec![integer(input.issue)?, integer(bottom)?, integer(cursor)?],
                )],
            })?;
            let mut rows = result[0].rows.iter().peekable();
            let mut output_full = false;
            for number in (bottom..=cursor).rev() {
                let matches = rows
                    .peek()
                    .is_some_and(|row| result_u64_from_row(row, 1).ok() == Some(number));
                let row = matches.then(|| rows.next()).flatten();
                scanned += 1;
                cursor -= 1;
                let Some(row) = row else {
                    continue;
                };
                let comment = comment_from_row(row)?;
                let comment_bytes = encoded_size(&comment)?;
                if encoded_bytes
                    .checked_add(comment_bytes)
                    .and_then(|bytes| bytes.checked_add(9))
                    .is_none_or(|bytes| bytes > MAX_LIST_OUTPUT_BYTES)
                {
                    cursor += 1;
                    output_full = true;
                    break;
                }
                encoded_bytes += comment_bytes;
                items.push(comment);
                if items.len() == usize::from(input.limit) {
                    break;
                }
            }
            if output_full {
                break;
            }
        }
        Ok(CommentPage::Found {
            items,
            next: (cursor > 0).then_some(cursor + 1),
        })
    }
}
