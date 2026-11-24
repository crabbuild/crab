use super::*;
use crate::cells::{Error, Result, organization::teams::review_routing};
use std::collections::BTreeMap;

const RECENT_WINDOW_MS: u64 = 30 * 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Workload {
    open: u64,
    recent: u64,
}

impl Workload {
    pub(crate) fn add(&mut self, other: Self) {
        self.open = self.open.saturating_add(other.open);
        self.recent = self.recent.saturating_add(other.recent);
    }

    fn score(self) -> (u64, u64) {
        (self.recent.saturating_add(self.open), self.open)
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct HistoryQuery {
    pub issuer: String,
    pub workload: Vec<String>,
    pub teams: BTreeMap<uuid::Uuid, Vec<String>>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct History {
    pub workload: BTreeMap<String, Workload>,
    pub teams: BTreeMap<uuid::Uuid, BTreeMap<String, u64>>,
}

impl History {
    pub(crate) fn add(&mut self, other: Self) {
        for (subject, load) in other.workload {
            self.workload.entry(subject).or_default().add(load);
        }
        for (team, assignments) in other.teams {
            let current = self.teams.entry(team).or_default();
            for (subject, time) in assignments {
                let latest = current.entry(subject).or_default();
                *latest = (*latest).max(time);
            }
        }
    }
}

pub(crate) struct ReadHistory;
impl Read for ReadHistory {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 259;
    type Input = HistoryQuery;
    type Output = History;

    fn execute(context: &mut Transaction<'_>, query: HistoryQuery) -> Result<History> {
        if query.workload.len() > automatic::MAX_RECIPIENTS
            || query.teams.len() > crate::catalog::MAX_TEAM_GRANTS
            || query.teams.iter().any(|(team, subjects)| {
                team.is_nil() || subjects.len() > teams::MAX_REVIEW_RECIPIENTS
            })
        {
            return Err(Error::Invalid("invalid review assignment history query"));
        }
        let mut history = History::default();
        if settings::archived(context)? {
            return Ok(history);
        }
        if !query.workload.is_empty() {
            super::super::super::search::index::ensure_ready(context)?;
            let since = timestamp(context.now_ms())?.saturating_sub(RECENT_WINDOW_MS);
            for subject in query.workload {
                let load = workload(context, &query.issuer, &subject, since)?;
                if load != Workload::default() {
                    history.workload.insert(subject, load);
                }
            }
        }
        for (team, subjects) in query.teams {
            let rotation = rotation_key(team);
            let mut assignments = BTreeMap::new();
            for subject in subjects {
                let id = super::reviewer_identity_id(&query.issuer, &subject);
                if let Some(time) = context.document::<u64>(&format!("{rotation}/last/{id}"))? {
                    assignments.insert(subject, time);
                }
            }
            if !assignments.is_empty() {
                history.teams.insert(team, assignments);
            }
        }
        Ok(history)
    }
}

fn workload(
    context: &Transaction<'_>,
    issuer: &str,
    subject: &str,
    since: u64,
) -> Result<Workload> {
    let identity =
        SqlValue::Blob(super::super::super::search::people::identity(issuer, subject)?.to_vec());
    let rows = context.query(
        "SELECT COUNT(*) FROM discussion_index_reviewers r
         JOIN repository_pulls p ON p.number=r.pull
         WHERE r.identity=? AND r.requested=1 AND p.state=?",
        &[
            identity.clone(),
            SqlValue::Integer(i64::from(PullState::Open.code())),
        ],
    )?;
    let open = result_u64_from_row(&rows.rows[0], 0)?;
    let mut recent = 0;
    let mut after = 0;
    loop {
        // Every retained request updated its PR when it was created. The PR
        // timestamp bounds the candidates, but only the request proves its age.
        let rows = context.query(
            "SELECT r.pull, r.request_id FROM discussion_index_reviewers r
             JOIN repository_pulls p ON p.number=r.pull
             WHERE r.identity=? AND r.request_id IS NOT NULL
                AND p.updated_at_ms>=? AND r.pull>?
             ORDER BY r.pull LIMIT 128",
            &[identity.clone(), integer(since)?, integer(after)?],
        )?;
        for row in &rows.rows {
            after = result_u64_from_row(row, 0)?;
            let id = result_text(row, 1)?;
            let request = load(context, after, &id)?.ok_or(Error::Invalid(
                "review workload index is missing its request",
            ))?;
            if request.state != State::Cancelled && request.requested_at >= since {
                recent += 1;
            }
        }
        if rows.rows.len() < 128 {
            return Ok(Workload { open, recent });
        }
    }
}

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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub other_workload: BTreeMap<String, Workload>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub other_rotation: BTreeMap<String, u64>,
}

fn rotation_key(team: uuid::Uuid) -> String {
    format!("collaboration/v1/team-review-rotation/{team}")
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
    let since = now.saturating_sub(RECENT_WINDOW_MS);
    let rotation = rotation_key(team);
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
        let score = if plan.algorithm == FewestOpen {
            let mut load = workload(context, &reviewer.issuer, &reviewer.subject, since)?;
            if let Some(other) = plan.other_workload.get(&reviewer.subject) {
                load.add(*other);
            }
            load.score()
        } else {
            (0, 0)
        };
        let key = format!("{rotation}/{id}");
        let stamp = format!("{rotation}/last/{id}");
        let local_order = context.document::<u64>(&key)?.unwrap_or(0);
        let local_time = context.document::<u64>(&stamp)?.unwrap_or(0);
        let remote_time = plan
            .other_rotation
            .get(&reviewer.subject)
            .copied()
            .unwrap_or(0);
        candidates.push((
            (score, local_time.max(remote_time), local_order, key),
            stamp,
            reviewer,
        ));
    }
    let Some(latest) = candidates.iter().map(|(rank, _, _)| rank.1).max() else {
        return Ok(Vec::new());
    };
    candidates.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    // The time is comparable across repositories. Local ordinals retain
    // ordering for older entries and break ties within the same millisecond.
    let assigned_at = next_version(context.document::<u64>(&rotation)?.unwrap_or(0))?;
    let rotation_time = now.max(next_version(latest)?);
    context.put_document(&rotation, &assigned_at)?;
    let mut selected = Vec::new();
    for ((_, _, _, key), stamp, reviewer) in candidates.into_iter().take(slots) {
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
        context.put_document(&stamp, &rotation_time)?;
    }
    Ok(selected)
}
