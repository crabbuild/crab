use super::*;

const PAGE_SIZE: usize = 16;

#[derive(Deserialize, Serialize)]
pub(crate) struct MergedPullForRelease {
    pub number: u64,
    pub title: String,
    pub author: RepositoryAuthor,
    pub commit_oid: String,
    pub label_ids: Vec<u64>,
}

pub(crate) struct ListMergedPullsForRelease;

impl Read for ListMergedPullsForRelease {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 439;
    type Input = Option<u64>;
    type Output = (Vec<MergedPullForRelease>, Option<u64>);

    fn execute(
        context: &mut Transaction<'_>,
        before: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        let upper = before.map_or(MAX_NUMBER, |number| number.saturating_sub(1));
        let rows = context.query(
            "SELECT number, title, author_issuer, author_subject, author_name, completed_merge, label_ids \
             FROM repository_pulls WHERE state = ? AND number <= ? ORDER BY number DESC LIMIT ?",
            &[
                SqlValue::Integer(i64::from(PullState::Merged.code())),
                integer(upper)?,
                SqlValue::Integer(PAGE_SIZE as i64),
            ],
        )?;
        let items = rows
            .rows
            .iter()
            .map(|row| {
                let merged: PullMerge = storage::decode_optional_record(&row[5])?.ok_or(
                    crate::cells::Error::Invalid("Merged pull request has no completed merge"),
                )?;
                Ok(MergedPullForRelease {
                    number: result_u64_from_row(row, 0)?,
                    title: result_text(row, 1)?,
                    author: author_from_row(row, 2)?,
                    commit_oid: merged.commit_oid,
                    label_ids: decode_label_ids(result_blob(row, 6)?)?,
                })
            })
            .collect::<crate::cells::Result<Vec<_>>>()?;
        let next = items
            .last()
            .filter(|_| items.len() == PAGE_SIZE)
            .map(|item| item.number);
        Ok((items, next))
    }
}

#[cfg(test)]
pub(crate) struct SeedHistoricalMerges;

#[cfg(test)]
impl SeedHistoricalMerges {
    pub(crate) const BATCH: usize = 128;
}

#[cfg(test)]
impl Mutation for SeedHistoricalMerges {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 59001;
    type Input = u64;
    type Output = ();

    fn execute(
        context: &mut Transaction<'_>,
        first: u64,
    ) -> crate::cells::Result<MutationResult<()>> {
        // Use the fixture's original completed merge so each historical row
        // carries a valid record for a real commit. Batches leave the normal
        // discussion-index work limit in force.
        let bounds = vec![integer(first)?, integer(first + Self::BATCH as u64 - 1)?];
        let rows = context.sql(&SqlBatch {
            statements: vec![
                statement(
                    "WITH RECURSIVE history(n) AS (VALUES(?) UNION ALL SELECT n+1 FROM history WHERE n<?)
                     INSERT INTO repository_pull_submissions (request_id, payload_digest, pull_number)
                     SELECT CAST(printf('fixture-%08d',n) AS BLOB), zeroblob(32), n FROM history",
                    bounds.clone(),
                ),
                statement(
                    "INSERT INTO repository_pulls (number, create_request_id, author_issuer, author_subject,
                     author_name, title, body, state, base_ref, base_oid, head_ref, head_oid, label_ids,
                     assignees, completed_merge, version, created_at_ms, updated_at_ms)
                     SELECT s.pull_number, s.request_id, p.author_issuer, p.author_subject, p.author_name,
                     'Historical release entry', '', p.state, p.base_ref, p.base_oid, p.head_ref, p.head_oid,
                     p.label_ids, p.assignees, p.completed_merge, p.version, p.created_at_ms, p.updated_at_ms
                     FROM repository_pull_submissions s JOIN repository_pulls p ON p.number=1
                     WHERE s.pull_number BETWEEN ? AND ? AND p.completed_merge IS NOT NULL",
                    bounds,
                ),
            ],
        })?;
        assert_eq!(rows[1].rows_affected, Self::BATCH as u64);
        advance_revision(context)?;
        Ok(MutationResult::Success(()))
    }
}
