use crate::{
    api::{
        assignees::Selection,
        labels::{self, Label},
        profiles::attribution::Authors,
        reactions,
    },
    auth::Identity,
    cells::repository::{CommentRecord, IssueRecord, IssueSummary, RepositoryAuthor},
};
use serde_json::{Value, json};

pub(super) fn issue(
    authors: &Authors,
    issue: &IssueRecord,
    author: &Identity,
    labels: &[Label],
    assignees: &Selection,
    can_triage: bool,
    reactions: &reactions::View,
) -> Value {
    json!({
        "number": issue.number,
        "title": issue.title,
        "body": issue.body,
        "reactions": reactions,
        "state": state_name(issue.state),
        "author": authors.view(&issue.author),
        "version": issue.version,
        "created_at": issue.created_at_ms,
        "updated_at": issue.updated_at_ms,
        "labels": labels::selection_view(&issue.label_ids, labels),
        "assignees": assignees.view(&issue.assignees),
        "can_edit": same_author(&issue.author, author),
        "can_close": same_author(&issue.author, author) || can_triage,
        "can_label": can_triage,
        "can_assign": can_triage,
    })
}

pub(super) fn summary(
    authors: &Authors,
    issue: &IssueSummary,
    labels: &[crate::cells::repository::search::index::LabelSummary],
    assignees: &Selection,
) -> Value {
    json!({
        "kind": "issue",
        "number": issue.number,
        "title": issue.title,
        "state": state_name(issue.state),
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

pub(super) fn comment(
    authors: &Authors,
    comment: &CommentRecord,
    author: &Identity,
    reactions: &reactions::View,
) -> Value {
    json!({
        "number": comment.number,
        "body": comment.body,
        "reactions": reactions,
        "author": authors.view(&comment.author),
        "version": comment.version,
        "created_at": comment.created_at_ms,
        "updated_at": comment.updated_at_ms,
        "can_edit": same_author(&comment.author, author),
    })
}

fn state_name(state: u8) -> &'static str {
    match state {
        0 => "open",
        1 => "closed",
        _ => "invalid",
    }
}

fn same_author(left: &RepositoryAuthor, right: &Identity) -> bool {
    left.issuer == right.issuer && left.subject == right.subject
}
