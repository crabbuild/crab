use super::*;
use crate::cells::{Error, Result, organization::teams::review_routing};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    pub algorithm: review_routing::Algorithm,
    pub reviewers: u8,
    pub candidates: Vec<RepositoryAuthor>,
    // Count pending requests for eligible members who may be unavailable now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count_existing: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_team: bool,
}

fn remaining_slots(
    context: &Transaction<'_>,
    pull: u64,
    issuer: &str,
    plan: &Plan,
) -> Result<usize> {
    let target = usize::from(plan.reviewers);
    let Some(subjects) = plan
        .count_existing
        .as_ref()
        .filter(|items| !items.is_empty())
    else {
        return Ok(target);
    };
    if subjects.len() > teams::MAX_REVIEW_RECIPIENTS {
        return Err(Error::Invalid("invalid team review request count"));
    }
    let params = std::iter::once(Ok(integer(pull)?))
        .chain(subjects.iter().map(|subject| {
            Ok(SqlValue::Blob(
                super::super::super::search::people::identity(issuer, subject)?.to_vec(),
            ))
        }))
        .collect::<Result<Vec<_>>>()?;
    let slots = vec!["?"; subjects.len()].join(",");
    let rows = context.query(
        &format!("SELECT COUNT(*) FROM discussion_index_reviewers WHERE pull=? AND requested=1 AND identity IN ({slots})"),
        &params,
    )?;
    let count = result_u64_from_row(&rows.rows[0], 0)?;
    Ok(target.saturating_sub(count as usize))
}

pub(super) fn assign(
    context: &Transaction<'_>,
    pull: &PullRecord,
    actor: &RepositoryAuthor,
    team: uuid::Uuid,
    plan: Plan,
    code_owner: bool,
) -> Result<Vec<String>> {
    use review_routing::Algorithm::{Disabled, FewestOpen};
    if plan.algorithm == Disabled
        || !(1..=review_routing::MAX_REVIEWERS).contains(&plan.reviewers)
        || plan.candidates.len() > teams::MAX_REVIEW_RECIPIENTS
    {
        return Err(Error::Invalid("invalid team reviewer routing plan"));
    }
    super::super::super::search::index::ensure_ready(context)?;
    let slots = remaining_slots(context, pull.number, &actor.issuer, &plan)?;
    if slots == 0 {
        return Ok(Vec::new());
    }
    let now = timestamp(context.now_ms())?;
    let rotation = format!("collaboration/v1/team-review-rotation/{team}");
    let mut candidates = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for reviewer in plan.candidates {
        validate_author(&reviewer)?;
        let id = reviewer_id(&reviewer);
        if reviewer.issuer != actor.issuer
            || same_author(&reviewer, &pull.author)
            || same_author(&reviewer, actor)
            || !seen.insert(id.clone())
            || load(context, pull.number, &id)?.is_some()
        {
            continue;
        }
        let identity = SqlValue::Blob(
            super::super::super::search::people::identity(&reviewer.issuer, &reviewer.subject)?
                .to_vec(),
        );
        let reviewed = context.query(
            "SELECT 1 FROM discussion_index_reviewers WHERE pull=? AND identity=? AND reviewed=1",
            &[integer(pull.number)?, identity.clone()],
        )?;
        if !reviewed.rows.is_empty() {
            continue;
        }
        let open = if plan.algorithm == FewestOpen {
            let rows = context.query(
                "SELECT COUNT(*) FROM discussion_index_reviewers r
                 JOIN repository_pulls p ON p.number=r.pull
                 WHERE r.identity=? AND r.requested=1 AND p.state=?",
                &[
                    identity,
                    SqlValue::Integer(i64::from(PullState::Open.code())),
                ],
            )?;
            result_u64_from_row(&rows.rows[0], 0)?
        } else {
            0
        };
        let key = format!("{rotation}/{id}");
        let last = context.document::<u64>(&key)?.unwrap_or(0);
        candidates.push((open, last, key, reviewer));
    }
    candidates.sort_unstable_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    // Write the team's rotation ordinal with the requests so concurrent
    // assignments share a single history and receipt replay cannot rotate it.
    let assigned_at = next_version(context.document::<u64>(&rotation)?.unwrap_or(0))?;
    context.put_document(&rotation, &assigned_at)?;
    let mut selected = Vec::new();
    for (_, _, key, reviewer) in candidates.into_iter().take(slots) {
        selected.push(reviewer.subject.clone());
        project(
            context,
            pull.number,
            &Request {
                reviewer,
                requested_by: actor.clone(),
                code_owner,
                state: State::Requested,
                requested_at: now,
                updated_at: now,
            },
        )?;
        context.put_document(&key, &assigned_at)?;
    }
    Ok(selected)
}
