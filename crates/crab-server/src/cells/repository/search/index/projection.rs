use super::*;
use crate::cells::repository::collaboration::{Target, reactions};
use crate::cells::repository::{
    decode_assignees, decode_label_ids, result_blob, result_text, result_u64_from_row,
};

pub(super) fn remove(context: &Transaction<'_>, id: u64) -> crate::cells::Result<()> {
    context.sql(&SqlBatch {
        statements: vec![
            statement(
                "DELETE FROM discussion_fts WHERE rowid = ?",
                vec![integer(id)?],
            ),
            statement(
                "DELETE FROM discussion_index WHERE rowid = ?",
                vec![integer(id)?],
            ),
        ],
    })?;
    Ok(())
}

pub(super) fn refresh(context: &Transaction<'_>, subject: Subject) -> crate::cells::Result<()> {
    let (table, comments, foreign, target) = match subject.kind {
        SubjectKind::Issue => (
            "repository_issues",
            "repository_issue_comments",
            "issue_number",
            Target::Issue {
                number: subject.number,
            },
        ),
        SubjectKind::Pull => (
            "repository_pulls",
            "repository_pull_comments",
            "pull_number",
            Target::Pull {
                number: subject.number,
            },
        ),
    };
    let query = format!(
        "SELECT title, body, author_name, state, label_ids, assignee_subjects, created_at_ms, updated_at_ms FROM {table} WHERE number = ?"
    );
    let rows = context.sql(&SqlBatch {
        statements: vec![statement(&query, vec![integer(subject.number)?])],
    })?;
    let id = rowid(subject);
    let Some(row) = rows[0].rows.first() else {
        return remove(context, id);
    };
    let title = result_text(row, 0)?.to_lowercase();
    let body = result_text(row, 1)?.to_lowercase();
    let author = result_text(row, 2)?.to_lowercase();
    let state = result_u64_from_row(row, 3)?;
    let labels = decode_label_ids(result_blob(row, 4)?)?;
    let assignees = decode_assignees(result_blob(row, 5)?)?;
    let created = result_u64_from_row(row, 6)?;
    let updated = result_u64_from_row(row, 7)?;
    let draft = subject.kind == SubjectKind::Pull
        && crate::cells::repository::pulls::readiness::load(context, subject.number)?
            == crate::cells::repository::PullReadiness::Draft;
    let counts = reactions::counts(context, target)?;
    let total = counts.values().try_fold(0u64, |total, count| {
        total
            .checked_add(*count)
            .filter(|total| *total <= MAX_NUMBER)
            .ok_or(crate::cells::Error::Invalid(
                "discussion reaction total exceeds its count range",
            ))
    })?;
    let comment_rows = context.sql(&SqlBatch {
        statements: vec![statement(
            &format!("SELECT COUNT(*) FROM {comments} WHERE {foreign} = ?"),
            vec![integer(subject.number)?],
        )],
    })?;
    let comments = result_u64(&comment_rows, 0, 0)?;
    // Folded text and all metadata rows commit with the domain mutation. FTS5
    // contentless-delete avoids triggers that require trusted-schema access.
    remove(context, id)?;
    context.sql(&SqlBatch { statements: vec![statement(
        "INSERT INTO discussion_index(rowid, kind, number, state, draft, created_at_ms, updated_at_ms, title, body, author, comments, reactions) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![integer(id)?, SqlValue::Integer(kind(subject.kind)), integer(subject.number)?, integer(state)?, SqlValue::Integer(i64::from(draft)), integer(created)?, integer(updated)?, SqlValue::Text(title.clone()), SqlValue::Text(body.clone()), SqlValue::Text(author.clone()), integer(comments)?, integer(total)?],
    )] })?;
    let mut statements = Vec::with_capacity(labels.len() + assignees.len() + counts.len() + 1);
    for label in labels {
        statements.push(statement(
            "INSERT INTO discussion_index_labels(document, label) VALUES (?, ?)",
            vec![integer(id)?, integer(label)?],
        ));
    }
    for subject in assignees {
        statements.push(statement(
            "INSERT INTO discussion_index_assignees(document, subject) VALUES (?, ?)",
            vec![integer(id)?, SqlValue::Text(subject)],
        ));
    }
    if let Some(milestone) =
        crate::cells::repository::collaboration::milestones::selected(context, subject)?
    {
        statements.push(statement(
            "INSERT INTO discussion_index_milestones(document, milestone) VALUES (?, ?)",
            vec![integer(id)?, integer(milestone)?],
        ));
    }
    for (kind, count) in counts {
        statements.push(statement(
            "INSERT INTO discussion_index_reaction_counts(document, kind, count) VALUES (?, ?, ?)",
            vec![
                integer(id)?,
                SqlValue::Text(kind.key().into()),
                integer(count)?,
            ],
        ));
    }
    statements.push(statement(
        "INSERT INTO discussion_fts(rowid, title, body, author) VALUES (?, ?, ?, ?)",
        vec![
            integer(id)?,
            SqlValue::Text(title),
            SqlValue::Text(body),
            SqlValue::Text(author),
        ],
    ));
    context.sql(&SqlBatch { statements })?;
    Ok(())
}
