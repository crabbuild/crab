use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::*;

const INSTALLED: &str = "ci/v1/check-rollups-installed";
const AFTER: &str = "ci/v1/check-rollups-backfill";
const BATCH: usize = 8;

#[derive(Clone, Default, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Checks {
    pub success: u64,
    pub neutral: u64,
    pub skipped: u64,
    pub pending: u64,
    pub failed: u64,
}

pub(in crate::cells::repository) fn install(context: &Transaction<'_>) -> crate::cells::Result<()> {
    context.install_schema(include_str!("summary.sql"))?;
    context.put_document(INSTALLED, &true)
}

pub(in crate::cells::repository) fn begin(context: &Transaction<'_>) -> crate::cells::Result<()> {
    install(context)?;
    context.delete_document(AFTER)?;
    Ok(())
}

pub(in crate::cells::repository) fn read(
    context: &Transaction<'_>,
    oid: &str,
) -> crate::cells::Result<Checks> {
    let rows = context.query(
        "SELECT success, neutral, skipped, pending, failed FROM repository_check_rollups WHERE oid=?",
        &[SqlValue::Text(oid.into())],
    )?;
    let Some(row) = rows.rows.first() else {
        return Ok(Checks::default());
    };
    Ok(Checks {
        success: result_u64_from_row(row, 0)?,
        neutral: result_u64_from_row(row, 1)?,
        skipped: result_u64_from_row(row, 2)?,
        pending: result_u64_from_row(row, 3)?,
        failed: result_u64_from_row(row, 4)?,
    })
}

pub(in crate::cells::repository) fn refresh(
    context: &mut Transaction<'_>,
    oid: &str,
) -> crate::cells::Result<()> {
    // Canonical CI writes still work before an operator prepares older Cells.
    // Backfill reads those writes. Once installed, new commits behind its
    // cursor are covered by this same transaction as the report.
    if context.document::<bool>(INSTALLED)? != Some(true) {
        return Ok(());
    }
    write(context, oid)
}

pub(in crate::cells::repository) fn backfill(
    context: &mut Transaction<'_>,
    processed: &mut u64,
) -> crate::cells::Result<bool> {
    let after = context.document::<String>(AFTER)?.unwrap_or_default();
    let rows = context.query(
        "SELECT oid FROM (
           SELECT oid FROM (SELECT oid FROM repository_status_sequences WHERE oid>?1 ORDER BY oid LIMIT ?2)
           UNION
           SELECT oid FROM (SELECT DISTINCT oid FROM repository_check_run_versions WHERE oid>?1 ORDER BY oid LIMIT ?2)
         ) ORDER BY oid LIMIT ?2",
        &[SqlValue::Text(after), integer(BATCH as u64)?],
    )?;
    let mut last = String::new();
    for row in &rows.rows {
        let oid = result_text(row, 0)?;
        write(context, &oid)?;
        last = oid;
    }
    *processed = processed
        .checked_add(rows.rows.len() as u64)
        .filter(|count| *count <= MAX_NUMBER)
        .ok_or(crate::cells::Error::Invalid(
            "check summary build count is exhausted",
        ))?;
    if rows.rows.len() < BATCH {
        context.delete_document(AFTER)?;
        return Ok(true);
    }
    context.put_document(AFTER, &last)?;
    Ok(false)
}

fn write(context: &mut Transaction<'_>, oid: &str) -> crate::cells::Result<()> {
    let counts = reports(context, oid)?;
    context.query(
        "INSERT INTO repository_check_rollups(oid, success, neutral, skipped, pending, failed)
         VALUES (?,?,?,?,?,?) ON CONFLICT(oid) DO UPDATE SET success=excluded.success,
         neutral=excluded.neutral, skipped=excluded.skipped, pending=excluded.pending, failed=excluded.failed",
        &[
            SqlValue::Text(oid.into()),
            integer(counts.success)?,
            integer(counts.neutral)?,
            integer(counts.skipped)?,
            integer(counts.pending)?,
            integer(counts.failed)?,
        ],
    )?;
    Ok(())
}

fn reports(context: &mut Transaction<'_>, oid: &str) -> crate::cells::Result<Checks> {
    let statuses = ListCommitStatuses::execute(context, oid.to_owned())?;
    let runs = ListCheckRuns::execute(
        context,
        ListCheckRunsInput {
            oid: oid.into(),
            before: None,
            limit: MAX_CHECK_RUNS_PER_COMMIT as u8,
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
        // Runs are newest first. Updating an older run cannot replace a rerun.
        let provider = integrations::github_connection(&run.author.issuer, &run.author.subject);
        if !names.insert((provider, run.name.to_lowercase())) {
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
