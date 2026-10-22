use super::*;
const STATE: &str = "discussion-conversation/v1/rebuild";
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum Progress {
    Clear,
    Comments {
        kind: SubjectKind,
        discussion: u64,
        number: u64,
    },
    Reviews {
        discussion: u64,
        number: u64,
    },
    Activity {
        before: Option<u64>,
    },
}

pub(in crate::cells::repository) fn reset(context: &Transaction<'_>) -> Result<()> {
    install(context)?;
    context.put_document(STATE, &Progress::Clear)
}

pub(in crate::cells::repository) fn step(context: &mut Transaction<'_>) -> Result<bool> {
    let progress = context
        .document::<Progress>(STATE)?
        .ok_or(Error::Invalid("conversation rebuild cursor is missing"))?;
    let next = match progress {
        Progress::Clear => {
            let rows=context.query("SELECT kind, discussion, source, number FROM discussion_conversation ORDER BY kind, discussion, source, number LIMIT 8",&[])?;
            for row in &rows.rows {
                context.query("DELETE FROM discussion_conversation WHERE kind = ? AND discussion = ? AND source = ? AND number = ?",row)?;
            }
            if rows.rows.len() == 8 {
                Progress::Clear
            } else {
                Progress::Comments {
                    kind: SubjectKind::Issue,
                    discussion: 0,
                    number: 0,
                }
            }
        }
        Progress::Activity { before } => {
            let page = activity::ListActivity::execute(
                context,
                activity::Query {
                    subject: None,
                    before,
                    limit: 8,
                },
            )?
            .ok_or(Error::Invalid("repository activity source is missing"))?;
            for event in &page.items {
                project_event(context, event)?;
            }
            let Some(before) = page.next else {
                context.delete_document(STATE)?;
                return Ok(true);
            };
            Progress::Activity {
                before: Some(before),
            }
        }
        _ => {
            let (subject_kind, source, table, parent, discussion, number) = match progress {
                Progress::Comments {
                    kind: SubjectKind::Issue,
                    discussion,
                    number,
                } => (
                    SubjectKind::Issue,
                    Source::Comment,
                    "repository_issue_comments",
                    "issue_number",
                    discussion,
                    number,
                ),
                Progress::Comments {
                    kind: SubjectKind::Pull,
                    discussion,
                    number,
                } => (
                    SubjectKind::Pull,
                    Source::Comment,
                    "repository_pull_comments",
                    "pull_number",
                    discussion,
                    number,
                ),
                Progress::Reviews { discussion, number } => (
                    SubjectKind::Pull,
                    Source::Review,
                    "repository_pull_reviews",
                    "pull_number",
                    discussion,
                    number,
                ),
                _ => return Err(Error::Invalid("conversation rebuild source is invalid")),
            };
            let sql = format!(
                "SELECT {parent}, number, created_at_ms FROM {table} WHERE ({parent}, number) > (?, ?) ORDER BY {parent}, number LIMIT 8"
            );
            let rows = context.query(&sql, &[integer(discussion)?, integer(number)?])?;
            let mut last = (discussion, number);
            for row in &rows.rows {
                last = (result_u64_from_row(row, 0)?, result_u64_from_row(row, 1)?);
                insert(
                    context,
                    Subject {
                        kind: subject_kind,
                        number: last.0,
                    },
                    Key {
                        source,
                        number: last.1,
                    },
                    result_u64_from_row(row, 2)?,
                )?;
            }
            if rows.rows.len() < 8 {
                match progress {
                    Progress::Comments {
                        kind: SubjectKind::Issue,
                        ..
                    } => Progress::Comments {
                        kind: SubjectKind::Pull,
                        discussion: 0,
                        number: 0,
                    },
                    Progress::Comments { .. } => Progress::Reviews {
                        discussion: 0,
                        number: 0,
                    },
                    _ => Progress::Activity { before: None },
                }
            } else {
                match progress {
                    Progress::Comments { kind, .. } => Progress::Comments {
                        kind,
                        discussion: last.0,
                        number: last.1,
                    },
                    _ => Progress::Reviews {
                        discussion: last.0,
                        number: last.1,
                    },
                }
            }
        }
    };
    context.put_document(STATE, &next)?;
    Ok(false)
}
