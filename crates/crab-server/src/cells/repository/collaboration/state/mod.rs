use super::{
    Subject, SubjectKind,
    submissions::{self, Kind},
};
use crate::cells::repository::{self, issues::lifecycle::Reason, *};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Open,
    Closed,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub submission_id: [u8; 16],
    pub subject: Subject,
    pub actor: RepositoryAuthor,
    pub can_triage: bool,
    pub version: u64,
    pub state: State,
    pub reason: Option<Reason>,
    pub body: Option<String>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Comment {
    Issue(CommentRecord),
    Pull(PullCommentRecord),
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Rejection {
    Archived,
    NotFound,
    Forbidden,
    Conflict,
    InvalidState,
    MergePending,
}
#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum Outcome {
    Applied(Option<Comment>),
    Rejected(Rejection),
    RequestConflict,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Resolution {
    Applied { comment: Option<u64> },
    Rejected { reason: Rejection },
}
pub(crate) struct ChangeState;
impl Mutation for ChangeState {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 300;
    type Input = Input;
    type Output = Outcome;
    fn execute(context: &mut Transaction<'_>, input: Input) -> Result<MutationResult<Outcome>> {
        let digest = blake3::hash(&serde_json::to_vec(&(
            input.subject,
            input.version,
            input.state,
            input.reason,
            &input.body,
            &input.actor.issuer,
            &input.actor.subject,
        ))?)
        .to_hex()
        .to_string();
        let kind = Kind::from(input.subject.kind);
        let subject = input.subject;
        let key = format!(
            "{}{:016x}/{}",
            kind.prefix(),
            subject.number,
            uuid::Uuid::from_bytes(input.submission_id)
        );
        let resolution = submissions::decide(context, kind, &key, digest, |context| {
            Ok(match apply(context, input)? {
                MutationResult::Success(Outcome::Applied(comment)) => {
                    let comment = comment.map(|comment| match comment {
                        Comment::Issue(value) => value.number,
                        Comment::Pull(value) => value.number,
                    });
                    MutationResult::Success(Resolution::Applied { comment })
                }
                MutationResult::Rejected(Outcome::Rejected(reason)) => {
                    MutationResult::Rejected(Resolution::Rejected { reason })
                }
                _ => {
                    return Err(Error::Invalid(
                        "state decision disagrees with its transaction",
                    ));
                }
            })
        })?;
        Ok(match resolution {
            Some(Resolution::Applied { comment }) => {
                MutationResult::Success(Outcome::Applied(load_comment(context, subject, comment)?))
            }
            Some(Resolution::Rejected { reason }) => {
                MutationResult::Success(Outcome::Rejected(reason))
            }
            None => MutationResult::Rejected(Outcome::RequestConflict),
        })
    }
}
fn apply(context: &mut Transaction<'_>, input: Input) -> Result<MutationResult<Outcome>> {
    validate_number(input.subject.number)?;
    validate_number(input.version)?;
    validate_author(&input.actor)?;
    if let Some(body) = &input.body {
        validate_body(body, true)?;
    }
    if input.reason.is_some()
        && (input.subject.kind == SubjectKind::Pull
            || input.state == State::Open
            || input.reason == Some(Reason::Reopened))
    {
        return Ok(MutationResult::Rejected(Outcome::Rejected(
            Rejection::InvalidState,
        )));
    }
    if settings::archived(context)? {
        return Ok(MutationResult::Rejected(Outcome::Rejected(
            Rejection::Archived,
        )));
    }
    if input.subject.closed(context)?.is_none() {
        return Ok(MutationResult::Rejected(Outcome::Rejected(
            Rejection::NotFound,
        )));
    }
    if input.subject.kind == SubjectKind::Issue {
        issues::lifecycle::ensure_ready(context)?;
    }
    // The contribution precedes the state event in the timeline. Rejection
    // rolls this scope back before the terminal decision is recorded.
    let comment =
        match (&input.body, input.subject.kind) {
            (Some(body), SubjectKind::Issue) => Some(Comment::Issue(issues::append_comment(
                context,
                input.subject.number,
                &input.actor,
                body,
            )?)),
            (Some(body), SubjectKind::Pull) => Some(Comment::Pull(
                pulls::comments::append_comment(context, input.subject.number, &input.actor, body)?,
            )),
            (None, _) => None,
        };
    let rejected = match input.subject.kind {
        SubjectKind::Issue => match UpdateIssue::execute(
            context,
            UpdateIssueInput {
                number: input.subject.number,
                actor: input.actor,
                can_triage: input.can_triage,
                version: input.version,
                state: Some(u8::from(input.state == State::Closed)),
                state_reason: input.reason,
                title: None,
                body: None,
                label_ids: None,
                assignees: None,
            },
        )? {
            MutationResult::Success(UpdateIssueOutcome::Updated(_)) => None,
            MutationResult::Rejected(outcome) => Some(match outcome {
                UpdateIssueOutcome::Archived => Outcome::Rejected(Rejection::Archived),
                UpdateIssueOutcome::NotFound => Outcome::Rejected(Rejection::NotFound),
                UpdateIssueOutcome::Forbidden => Outcome::Rejected(Rejection::Forbidden),
                UpdateIssueOutcome::Conflict => Outcome::Rejected(Rejection::Conflict),
                UpdateIssueOutcome::InvalidStateReason => {
                    Outcome::Rejected(Rejection::InvalidState)
                }
                _ => return Err(Error::Invalid("unexpected issue state outcome")),
            }),
            _ => {
                return Err(Error::Invalid(
                    "issue state outcome disagrees with its decision",
                ));
            }
        },
        SubjectKind::Pull => match UpdatePull::execute(
            context,
            UpdatePullInput {
                number: input.subject.number,
                actor: input.actor,
                can_triage: input.can_triage,
                version: input.version,
                state: Some(if input.state == State::Open {
                    PullState::Open
                } else {
                    PullState::Closed
                }),
                title: None,
                body: None,
                label_ids: None,
                assignees: None,
            },
        )? {
            MutationResult::Success(UpdatePullOutcome::Updated(_)) => None,
            MutationResult::Rejected(outcome) => Some(match outcome {
                UpdatePullOutcome::Archived => Outcome::Rejected(Rejection::Archived),
                UpdatePullOutcome::NotFound => Outcome::Rejected(Rejection::NotFound),
                UpdatePullOutcome::Forbidden => Outcome::Rejected(Rejection::Forbidden),
                UpdatePullOutcome::Conflict => Outcome::Rejected(Rejection::Conflict),
                UpdatePullOutcome::InvalidState => Outcome::Rejected(Rejection::InvalidState),
                UpdatePullOutcome::MergePending => Outcome::Rejected(Rejection::MergePending),
                _ => return Err(Error::Invalid("unexpected pull state outcome")),
            }),
            _ => {
                return Err(Error::Invalid(
                    "pull state outcome disagrees with its decision",
                ));
            }
        },
    };
    if let Some(rejected) = rejected {
        return Ok(MutationResult::Rejected(rejected));
    }
    Ok(MutationResult::Success(Outcome::Applied(comment)))
}
pub(super) fn load_comment(
    context: &mut Transaction<'_>,
    subject: Subject,
    number: Option<u64>,
) -> Result<Option<Comment>> {
    let Some(number) = number else {
        return Ok(None);
    };
    let comment = match subject.kind {
        SubjectKind::Issue => GetComment::execute(
            context,
            CommentKey {
                issue: subject.number,
                number,
            },
        )?
        .map(Comment::Issue),
        SubjectKind::Pull => GetPullComment::execute(
            context,
            PullChildKey {
                pull: subject.number,
                number,
            },
        )?
        .map(Comment::Pull),
    };
    comment
        .map(Some)
        .ok_or(Error::Invalid("state submission comment is missing"))
}

#[cfg(test)]
pub(in crate::cells::repository) mod tests;
