use super::*;
use crate::cells::repository::{result_blob, search};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Review {
    pub number: u64,
    pub state: ReviewState,
    pub commit_oid: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reviewer {
    pub id: String,
    pub author: RepositoryAuthor,
    pub request: Option<review_requests::Request>,
    pub review: Option<Review>,
    pub decision: Option<Review>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub pull: u64,
    pub after: Option<String>,
    pub limit: u8,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Page {
    pub items: Vec<Reviewer>,
    pub next: Option<String>,
    pub author: RepositoryAuthor,
    pub version: u64,
    pub open: bool,
    pub merge_pending: bool,
    pub head_ref: String,
    pub merged_head: Option<String>,
}

fn cursor(value: &str) -> crate::cells::Result<Vec<u8>> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(crate::cells::Error::Invalid("Reviewer cursor is invalid"));
    }
    Ok(blake3::Hash::from_hex(value)
        .map_err(crate::cells::Error::sdk)?
        .as_bytes()
        .to_vec())
}
fn review(row: &[SqlValue], start: usize) -> crate::cells::Result<Option<Review>> {
    if row.get(start) == Some(&SqlValue::Null) {
        return Ok(None);
    }
    Ok(Some(Review {
        number: result_u64_from_row(row, start)?,
        state: ReviewState::from_code(result_u64_from_row(row, start + 1)?)?,
        commit_oid: result_text(row, start + 2)?,
    }))
}

pub(crate) struct ListReviewers;
impl Read for ListReviewers {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 252;
    type Input = Query;
    type Output = Option<Page>;
    fn execute(context: &mut Transaction<'_>, input: Query) -> crate::cells::Result<Self::Output> {
        validate_number(input.pull)?;
        if !(1..=50).contains(&input.limit) {
            return Err(crate::cells::Error::Invalid(
                "Reviewer page size is invalid",
            ));
        }
        let after = input
            .after
            .as_deref()
            .map(cursor)
            .transpose()?
            .unwrap_or_default();
        search::index::ensure_ready(context)?;
        let metadata = context.query("SELECT author_issuer, author_subject, author_name, version, state, pending_merge IS NOT NULL, head_ref, completed_merge FROM repository_pulls WHERE number = ?", &[integer(input.pull)?])?;
        let Some(row) = metadata.rows.first() else {
            return Ok(None);
        };
        let merged: Option<PullMerge> = super::storage::decode_optional_record(&row[7])?;
        let mut page = Page {
            items: Vec::new(),
            next: None,
            author: author_from_row(row, 0)?,
            version: result_u64_from_row(row, 3)?,
            open: PullState::from_code(result_u64_from_row(row, 4)?)? == PullState::Open,
            merge_pending: result_u64_from_row(row, 5)? != 0,
            head_ref: result_text(row, 6)?,
            merged_head: merged.map(|merge| merge.head_oid),
        };
        // One ordered participant row per qualified account. Primary-key seeks and
        // point joins bound work independently of the number of historical reviews.
        let rows = context.query("SELECT r.identity, r.request_id, v.author_issuer, v.author_subject, v.author_name, v.number, v.state, v.commit_oid, d.review_number, d.state, d.commit_oid, r.review_number FROM discussion_index_reviewers r LEFT JOIN repository_pull_reviews v ON v.pull_number = r.pull AND v.number = r.review_number LEFT JOIN repository_pull_review_decisions d ON d.pull_number = r.pull AND d.author_issuer = v.author_issuer AND d.author_subject = v.author_subject WHERE r.pull = ? AND r.identity > ? ORDER BY r.identity LIMIT ?", &[integer(input.pull)?, SqlValue::Blob(after), SqlValue::Integer(i64::from(input.limit)+1)])?;
        let more = rows.rows.len() > usize::from(input.limit);
        for row in rows.rows.iter().take(usize::from(input.limit)) {
            let request = match &row[1] {
                SqlValue::Null => None,
                SqlValue::Text(id) => Some(review_requests::load(context, input.pull, id)?.ok_or(
                    crate::cells::Error::Invalid("Reviewer request source is missing"),
                )?),
                _ => {
                    return Err(crate::cells::Error::Invalid(
                        "Reviewer request reference is invalid",
                    ));
                }
            };
            let latest = review(row, 5)?;
            if latest.is_some() != (row[11] != SqlValue::Null) {
                return Err(crate::cells::Error::Invalid(
                    "Reviewer review source is missing",
                ));
            }
            let author = if latest.is_some() {
                author_from_row(row, 2)?
            } else {
                request
                    .as_ref()
                    .ok_or(crate::cells::Error::Invalid("Reviewer has no source"))?
                    .reviewer
                    .clone()
            };
            let identity = search::people::identity(&author.issuer, &author.subject)?;
            if result_blob(row, 0)? != identity
                || request
                    .as_ref()
                    .is_some_and(|request| !same_author(&request.reviewer, &author))
            {
                return Err(crate::cells::Error::Invalid(
                    "Reviewer source identity is invalid",
                ));
            }
            page.items.push(Reviewer {
                id: blake3::Hash::from(identity).to_hex().to_string(),
                author,
                request,
                review: latest,
                decision: review(row, 8)?,
            });
        }
        if more {
            page.next = page.items.last().map(|reviewer| reviewer.id.clone());
        }
        Ok(Some(page))
    }
}

#[cfg(test)]
pub(in crate::cells::repository) mod tests;
