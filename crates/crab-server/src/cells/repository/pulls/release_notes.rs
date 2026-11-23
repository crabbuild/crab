use super::*;

const PAGE_SIZE: usize = 16;

#[derive(Deserialize, Serialize)]
pub(crate) struct MergedPullForRelease {
    pub number: u64,
    pub title: String,
    pub author: RepositoryAuthor,
    pub commit_oid: String,
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
            "SELECT number, title, author_issuer, author_subject, author_name, completed_merge \
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
