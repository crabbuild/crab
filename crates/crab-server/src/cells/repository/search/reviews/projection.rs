use crate::cells::{
    Result,
    application::{SqlValue, Transaction},
    repository::{RepositoryAuthor, integer, search::people},
};

pub(in crate::cells::repository) fn reviewed(
    context: &Transaction<'_>,
    pull: u64,
    author: &RepositoryAuthor,
) -> Result<()> {
    let identity = people::identity(&author.issuer, &author.subject)?;
    context.query("INSERT INTO discussion_index_reviewers(pull, identity, reviewed, requested) VALUES (?, ?, 1, 0) ON CONFLICT(pull, identity) DO UPDATE SET reviewed = 1, requested = 0", &[integer(pull)?, SqlValue::Blob(identity.to_vec())])?;
    Ok(())
}
pub(in crate::cells::repository) fn requested(
    context: &Transaction<'_>,
    pull: u64,
    author: &RepositoryAuthor,
    requested: bool,
) -> Result<()> {
    let identity = people::identity(&author.issuer, &author.subject)?;
    let values = [integer(pull)?, SqlValue::Blob(identity.to_vec())];
    if requested {
        context.query("INSERT INTO discussion_index_reviewers(pull, identity, reviewed, requested) VALUES (?, ?, 0, 1) ON CONFLICT(pull, identity) DO UPDATE SET requested = 1", &values)?;
    } else {
        context.query(
            "UPDATE discussion_index_reviewers SET requested = 0 WHERE pull = ? AND identity = ?",
            &values,
        )?;
        context.query("DELETE FROM discussion_index_reviewers WHERE pull = ? AND identity = ? AND reviewed = 0 AND requested = 0", &values)?;
    }
    Ok(())
}
