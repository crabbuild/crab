use super::*;
use crate::cells::repository::issues;

#[derive(Deserialize, Serialize)]
struct Closure {
    request_id: [u8; 16],
    issues: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    keyword_issues: Vec<u64>,
}

pub(super) fn closing_prefix(pull: u64) -> String {
    format!("{ROOT}/closing/{pull:016x}/")
}

fn closure_key(pull: u64) -> String {
    format!("{ROOT}/publication/{pull:016x}")
}

fn published_keywords_key(pull: u64) -> String {
    format!("{ROOT}/published-keywords/{pull:016x}")
}

pub(in crate::cells::repository) fn projected_keywords(
    context: &Transaction<'_>,
    pull: u64,
    body: &str,
    merged: bool,
) -> crate::cells::Result<Vec<u64>> {
    if merged {
        return Ok(context
            .document(&published_keywords_key(pull))?
            .unwrap_or_default());
    }
    Ok(keywords::numbers(body))
}

pub(in crate::cells::repository) fn capture_closure(
    context: &Transaction<'_>,
    pull: u64,
    request_id: [u8; 16],
    on_default: bool,
    body: &str,
) -> crate::cells::Result<()> {
    if !on_default {
        return clear_closure(context, pull);
    }
    let mut issues = context
        .documents::<u64>(&closing_prefix(pull), None, MAX_CLOSING_ISSUES)?
        .into_iter()
        .map(|(_, issue)| issue)
        .collect::<Vec<_>>();
    let mut keyword_issues = Vec::new();
    for number in keywords::numbers(body) {
        let subject = Subject {
            kind: SubjectKind::Issue,
            number,
        };
        if subject.closed(context)?.is_some() {
            keyword_issues.push(number);
            if !issues.contains(&number) {
                issues.push(number);
            }
        }
    }
    if issues.is_empty() {
        return clear_closure(context, pull);
    }
    issues::lifecycle::ensure_ready(context)?;
    context.put_document(
        &closure_key(pull),
        &Closure {
            request_id,
            issues,
            keyword_issues,
        },
    )
}

pub(in crate::cells::repository) fn clear_closure(
    context: &Transaction<'_>,
    pull: u64,
) -> crate::cells::Result<()> {
    context.delete_document(&closure_key(pull)).map(drop)
}

pub(in crate::cells::repository) fn complete_closure(
    context: &mut Transaction<'_>,
    pull: u64,
    request_id: [u8; 16],
    actor: &RepositoryAuthor,
) -> crate::cells::Result<()> {
    let Some(captured) = context.document::<Closure>(&closure_key(pull))? else {
        return Ok(());
    };
    if captured.request_id != request_id {
        return Err(crate::cells::Error::Invalid(
            "linked issue closure belongs to another merge",
        ));
    }
    // The publication, not the retrying user's permissions or present HEAD,
    // authorized these issue state changes. Commit them with the PR transition.
    for issue in captured.issues {
        issues::close_by_pull(context, issue, pull, actor)?;
    }
    if !captured.keyword_issues.is_empty() {
        context.put_document(&published_keywords_key(pull), &captured.keyword_issues)?;
    }
    clear_closure(context, pull)
}
