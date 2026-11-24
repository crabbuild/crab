use crate::{
    api::{assignees::Selection, labels, profiles::attribution::Authors, reactions},
    auth::Identity,
    cells::{
        organization::issue_types::Definition,
        repository::{
            CommentRecord, IssueRecord, IssueSummary, RepositoryAuthor, issues::IssueDetail,
        },
    },
};
use serde_json::{Value, json};

pub(super) fn issue(
    authors: &Authors,
    issue: &IssueRecord,
    detail: &IssueDetail,
    author: Option<&Identity>,
    assignees: &Selection,
    repository: uuid::Uuid,
    reactions: &reactions::View,
) -> crate::app::Result<Value> {
    let can_triage = assignees.principal.can_triage(repository);
    let lifecycle = detail
        .lifecycle
        .as_ref()
        .ok_or(crate::app::Error::CellContract(
            "Issue lifecycle is missing",
        ))?;
    Ok(json!({
        "number": issue.number,
        "title": issue.title,
        "body": issue.body,
        "reactions": reactions,
        "has_edit_history": reactions.has_edit_history,
        "state": state_name(issue.state),
        "blocked": detail.blocked,
        "state_reason": lifecycle.state_reason,
        "closed_at": lifecycle.closed_at_ms,
        "author": authors.view(&issue.author),
        "version": issue.version,
        "created_at": issue.created_at_ms,
        "updated_at": issue.updated_at_ms,
        "labels": labels::selection_view(&issue.label_ids, &detail.labels.labels),
        "assignees": assignees.view(&issue.assignees),
        "can_edit": same_author(&issue.author, author) || assignees.principal.can_moderate(repository),
        "can_close": same_author(&issue.author, author) || can_triage,
        "can_transfer": issue.state == 0 && assignees.principal.can_moderate(repository),
        "can_label": can_triage,
        "can_assign": can_triage,
    }))
}

pub(super) fn summary(
    authors: &Authors,
    issue: &IssueSummary,
    labels: &[crate::cells::repository::search::index::LabelSummary],
    assignees: &Selection,
    issue_type: Option<&Definition>,
) -> Value {
    json!({
        "kind": "issue",
        "number": issue.number,
        "title": issue.title,
        "state": state_name(issue.state),
        "blocked": issue.blocked,
        "issue_type": issue_type,
        "state_reason": issue.lifecycle.state_reason,
        "closed_at": issue.lifecycle.closed_at_ms,
        "author": authors.view(&issue.author),
        "version": issue.version,
        "created_at": issue.created_at_ms,
        "updated_at": issue.updated_at_ms,
        "comment_count": issue.comment_count,
        "reaction_count": issue.reaction_count,
        "labels": issue.label_ids.iter().filter_map(|id| labels.iter().find(|label|label.id==*id)).collect::<Vec<_>>(),
        "assignees": assignees.view(&issue.assignees),
    })
}

pub(in crate::api) fn comment<'a>(
    authors: &Authors,
    comment: &CommentRecord,
    author: impl Into<Option<&'a Identity>>,
    can_moderate: bool,
    reactions: &reactions::View,
) -> Value {
    json!({
        "number": comment.number,
        "body": comment.body,
        "reactions": reactions,
        "has_edit_history": reactions.has_edit_history,
        "minimization": reactions.minimization,
        "author": authors.view(&comment.author),
        "version": comment.version,
        "created_at": comment.created_at_ms,
        "updated_at": comment.updated_at_ms,
        "can_edit": same_author(&comment.author, author.into()) || can_moderate,
    })
}

fn state_name(state: u8) -> &'static str {
    match state {
        0 => "open",
        1 => "closed",
        _ => "invalid",
    }
}

fn same_author(left: &RepositoryAuthor, right: Option<&Identity>) -> bool {
    right.is_some_and(|right| left.issuer == right.issuer && left.subject == right.subject)
}
