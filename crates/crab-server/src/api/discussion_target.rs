use crate::{
    app::{self, Error, Result},
    cells::repository::collaboration::Target,
};
use crate::{auth::Principal, cells::repository::collaboration::SubjectKind};
use uuid::Uuid;

pub(super) fn target(path: &str) -> Result<Target> {
    fn number(value: &str) -> Result<u64> {
        app::number(value.parse().map_err(|_| Error::NotFound)?)
    }
    match path.split('/').collect::<Vec<_>>().as_slice() {
        ["issues", id] => Ok(Target::Issue {
            number: number(id)?,
        }),
        ["pulls", id] => Ok(Target::Pull {
            number: number(id)?,
        }),
        ["issues", issue, "comments", id] => Ok(Target::IssueComment {
            issue: number(issue)?,
            number: number(id)?,
        }),
        ["pulls", pull, "comments", id] => Ok(Target::PullComment {
            pull: number(pull)?,
            number: number(id)?,
        }),
        ["pulls", pull, "reviews", id] => Ok(Target::PullReview {
            pull: number(pull)?,
            number: number(id)?,
        }),
        ["pulls", pull, "threads", id] => Ok(Target::PullThread {
            pull: number(pull)?,
            number: number(id)?,
        }),
        ["pulls", pull, "threads", thread, "replies", id] => Ok(Target::PullReply {
            pull: number(pull)?,
            thread: number(thread)?,
            number: number(id)?,
        }),
        _ => Err(Error::NotFound),
    }
}

pub(super) fn can_manage(principal: &Principal, repo: Uuid, kind: SubjectKind) -> bool {
    match kind {
        SubjectKind::Issue => principal.can_moderate(repo),
        SubjectKind::Pull => principal.can_write(repo),
    }
}
