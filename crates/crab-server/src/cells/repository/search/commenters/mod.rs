use crate::cells::{
    Error, Result,
    application::{SqlValue, Transaction},
    repository::{RepositoryAuthor, collaboration::Target, integer},
};
use serde::{Deserialize, Serialize};

mod rebuild;
pub(in crate::cells::repository::search) use rebuild::{clear, rebuild, reset};

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    IssueComment,
    PullComment,
    Review,
    Thread,
    Reply,
}
impl Source {
    const fn code(self) -> i64 {
        match self {
            Self::IssueComment => 0,
            Self::PullComment => 1,
            Self::Review => 2,
            Self::Thread => 3,
            Self::Reply => 4,
        }
    }
    const fn table(self) -> &'static str {
        match self {
            Self::IssueComment => "repository_issue_comments",
            Self::PullComment => "repository_pull_comments",
            Self::Review => "repository_pull_reviews",
            Self::Thread => "repository_pull_review_threads",
            Self::Reply => "repository_pull_review_replies",
        }
    }
    const fn next(self) -> Option<Self> {
        match self {
            Self::IssueComment => Some(Self::PullComment),
            Self::PullComment => Some(Self::Review),
            Self::Review => Some(Self::Thread),
            Self::Thread => Some(Self::Reply),
            Self::Reply => None,
        }
    }
}

pub(in crate::cells::repository::search) fn install(context: &Transaction<'_>) -> Result<()> {
    context.install_schema(include_str!("schema.sql"))
}

pub(in crate::cells::repository) fn project(
    context: &Transaction<'_>,
    target: Target,
    author: &RepositoryAuthor,
) -> Result<()> {
    let (source, discussion, thread, number) = match target {
        Target::IssueComment { issue, number } => (Source::IssueComment, issue, 0, number),
        Target::PullComment { pull, number } => (Source::PullComment, pull, 0, number),
        Target::PullReview { pull, number } => (Source::Review, pull, 0, number),
        Target::PullThread { pull, number } => (Source::Thread, pull, 0, number),
        Target::PullReply {
            pull,
            thread,
            number,
        } => (Source::Reply, pull, thread, number),
        _ => {
            return Err(Error::Invalid(
                "Commenter source must be a comment or submitted review",
            ));
        }
    };
    insert(
        context,
        source,
        discussion,
        thread,
        number,
        &author.issuer,
        &author.subject,
    )
}

fn insert(
    context: &Transaction<'_>,
    source: Source,
    discussion: u64,
    thread: u64,
    number: u64,
    issuer: &str,
    subject: &str,
) -> Result<()> {
    let identity = super::people::identity(issuer, subject)?;
    context.query("INSERT INTO discussion_index_commenters(source, kind, discussion, thread, number, identity) VALUES (?, ?, ?, ?, ?, ?)", &[
        SqlValue::Integer(source.code()),
        SqlValue::Integer(i64::from(!matches!(source, Source::IssueComment))),
        integer(discussion)?, integer(thread)?, integer(number)?, SqlValue::Blob(identity.to_vec()),
    ])?;
    Ok(())
}

#[cfg(test)]
pub(in crate::cells::repository) mod tests;
