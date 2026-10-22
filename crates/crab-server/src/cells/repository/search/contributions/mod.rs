use crate::cells::{
    Error, Result,
    application::{SqlValue, Transaction},
    repository::{RepositoryAuthor, collaboration::Target, integer},
};
use serde::{Deserialize, Serialize};

mod rebuild;
mod text;
pub(in crate::cells::repository::search) use rebuild::{clear, rebuild, reset};
pub(in crate::cells::repository::search) use text::flush;

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
    context.install_schema(include_str!("schema.sql"))?;
    context.install_schema(include_str!("text.sql"))
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Key {
    source: Source,
    discussion: u64,
    thread: u64,
    number: u64,
}
impl TryFrom<Target> for Key {
    type Error = Error;
    fn try_from(target: Target) -> Result<Self> {
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
                    "Contribution source must be a comment or submitted review",
                ));
            }
        };
        Ok(Self {
            source,
            discussion,
            thread,
            number,
        })
    }
}

pub(in crate::cells::repository) fn project(
    context: &Transaction<'_>,
    target: Target,
    author: &RepositoryAuthor,
    body: &str,
) -> Result<()> {
    let key = Key::try_from(target)?;
    insert(context, &key, &author.issuer, &author.subject, body)
}

pub(in crate::cells::repository) fn edit(
    context: &Transaction<'_>,
    target: Target,
    body: &str,
) -> Result<()> {
    text::edit(context, &Key::try_from(target)?, body)
}

fn insert(
    context: &Transaction<'_>,
    key: &Key,
    issuer: &str,
    subject: &str,
    body: &str,
) -> Result<()> {
    let identity = super::people::identity(issuer, subject)?;
    context.query("INSERT INTO discussion_index_commenters(source, kind, discussion, thread, number, identity) VALUES (?, ?, ?, ?, ?, ?)", &[
        SqlValue::Integer(key.source.code()),
        SqlValue::Integer(i64::from(!matches!(key.source, Source::IssueComment))),
        integer(key.discussion)?, integer(key.thread)?, integer(key.number)?, SqlValue::Blob(identity.to_vec()),
    ])?;
    text::insert(context, key, body)
}

#[cfg(test)]
pub(in crate::cells::repository) mod tests;
