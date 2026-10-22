use super::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Head {
    pub number: u64,
    pub version: u64,
    pub oid: Option<String>,
}
#[derive(Clone, Default, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Checks {
    pub success: u64,
    pub neutral: u64,
    pub skipped: u64,
    pub pending: u64,
    pub failed: u64,
}
#[derive(Default, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reviews {
    pub approved: u64,
    pub changes_requested: u64,
    pub requested: u64,
    pub outdated: u64,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rollup {
    pub number: u64,
    pub head_oid: Option<String>,
    pub checks: Option<Checks>,
    pub reviews: Option<Reviews>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub heads: Vec<Head>,
    pub source_token: Option<String>,
}
pub(crate) struct GetRollups;
impl Read for GetRollups {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 321;
    type Input = Input;
    type Output = Option<Vec<Rollup>>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        if input.heads.len() > 100
            || input
                .heads
                .iter()
                .map(|head| head.number)
                .collect::<BTreeSet<_>>()
                .len()
                != input.heads.len()
        {
            return Err(crate::cells::Error::Invalid(
                "Pull rollup selection is invalid",
            ));
        }
        search::index::ensure_ready(context)?;
        let mut commits = BTreeMap::<String, Checks>::new();
        let mut items = Vec::with_capacity(input.heads.len());
        let epoch = input
            .source_token
            .as_deref()
            .map(|token| crate::cells::repository::projection::ready_epoch(context, token))
            .transpose()?;
        for head in input.heads {
            validate_number(head.number)?;
            validate_number(head.version)?;
            if let Some(oid) = &head.oid {
                validate_oid(oid)?;
            }
            let rows = context.query(
                "SELECT version, completed_merge FROM repository_pulls WHERE number=?",
                &[integer(head.number)?],
            )?;
            let Some(row) = rows.rows.first() else {
                return Ok(None);
            };
            // A list row and its rollup must describe the same source version.
            // Concurrent review decisions/state edits require an explicit refresh.
            if result_u64_from_row(row, 0)? != head.version {
                return Ok(None);
            }
            let merged: Option<PullMerge> = storage::decode_optional_record(&row[1])?;
            // Filtered lists retain the search's immutable ref snapshot. A later
            // Git lookup could display rollups that no longer satisfy the filter.
            let selected = if let Some(epoch) = epoch {
                let rows=context.query("SELECT lower(hex(r.target_oid)) FROM discussion_index_pull_sources s JOIN git_projection_refs r ON r.name=s.head_ref AND r.epoch_id=? WHERE s.document=?", &[integer(epoch)?,integer(head.number*2+1)?])?;
                rows.rows
                    .first()
                    .map(|row| result_text(row, 0))
                    .transpose()?
            } else {
                head.oid
            };
            let oid = merged.map(|merge| merge.head_oid).or(selected);
            let (checks, reviews) = if let Some(oid) = &oid {
                let checks = if let Some(checks) = commits.get(oid) {
                    checks.clone()
                } else {
                    let checks = checks(context, oid)?;
                    commits.insert(oid.clone(), checks.clone());
                    checks
                };
                (Some(checks), Some(reviews(context, head.number, oid)?))
            } else {
                (None, None)
            };
            items.push(Rollup {
                number: head.number,
                head_oid: oid,
                checks,
                reviews,
            });
        }
        Ok(Some(items))
    }
}

fn checks(context: &mut Transaction<'_>, oid: &str) -> crate::cells::Result<Checks> {
    // Canonical catalogs bound this to 1,000 status submissions and 100 runs,
    // excluding check output/log bodies. Repeated heads share the result.
    let statuses = ListCommitStatuses::execute(context, oid.to_owned())?;
    let runs = ListCheckRuns::execute(
        context,
        ListCheckRunsInput {
            oid: oid.to_owned(),
            before: None,
            limit: 100,
        },
    )?;
    let mut counts = Checks::default();
    for status in statuses.statuses {
        match status.state {
            0 | 1 => counts.failed += 1,
            2 => counts.pending += 1,
            3 => counts.success += 1,
            _ => {
                return Err(crate::cells::Error::Invalid(
                    "Commit status state is invalid",
                ));
            }
        }
    }
    let mut names = BTreeSet::new();
    for run in runs.runs {
        // The latest run number wins per case-insensitive name, matching
        // api/checks::latest. An old run update cannot replace a newer rerun.
        if !names.insert(run.name.to_lowercase()) {
            continue;
        }
        match (run.status, run.conclusion) {
            (0 | 1, _) => counts.pending += 1,
            (2, Some(3)) => counts.neutral += 1,
            (2, Some(4)) => counts.skipped += 1,
            (2, Some(5)) => counts.success += 1,
            (2, _) => counts.failed += 1,
            _ => return Err(crate::cells::Error::Invalid("Check run state is invalid")),
        }
    }
    Ok(counts)
}

fn reviews(context: &Transaction<'_>, pull: u64, oid: &str) -> crate::cells::Result<Reviews> {
    let requested = context.query(
        "SELECT requested FROM discussion_pull_rollups WHERE pull=?",
        &[integer(pull)?],
    )?;
    if requested.rows.is_empty()
        && !context
            .query(
                "SELECT 1 FROM discussion_index_reviewers WHERE pull=? LIMIT 1",
                &[integer(pull)?],
            )?
            .rows
            .is_empty()
    {
        return Err(crate::cells::Error::Invalid(
            "Pull request reviewer counter is missing",
        ));
    }
    let mut counts = Reviews {
        requested: requested
            .rows
            .first()
            .map(|row| result_u64_from_row(row, 0))
            .transpose()?
            .unwrap_or(0),
        ..Reviews::default()
    };
    // The canonical decision set has at most 96 qualified reviewers. Outstanding
    // requests use the maintained counter, never a scan of all past participants.
    let rows = context.query(
        "SELECT state, commit_oid FROM repository_pull_review_decisions WHERE pull_number=?",
        &[integer(pull)?],
    )?;
    for row in &rows.rows {
        let current = result_text(row, 1)? == oid;
        if !current {
            counts.outdated += 1;
        }
        // Requested changes remain active after a push; only approvals depend
        // on the selected head. The anchor can still be reported as outdated.
        match ReviewState::from_code(result_u64_from_row(row, 0)?)? {
            ReviewState::Approved if current => counts.approved += 1,
            ReviewState::ChangesRequested => counts.changes_requested += 1,
            ReviewState::Approved | ReviewState::Commented => {}
        }
    }
    Ok(counts)
}

#[cfg(test)]
pub(in crate::cells::repository) mod tests;
