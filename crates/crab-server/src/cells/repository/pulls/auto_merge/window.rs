use super::*;
use crate::cells::{Error, Result};

const PAGE_SIZE: usize = 50;

pub(super) fn pulls(context: &Transaction<'_>, branch: &str, limit: usize) -> Result<Vec<u64>> {
    let prefix = queue_prefix(branch);
    let mut remaining = limit;
    let mut before = None;
    let mut pulls = Vec::with_capacity(limit);
    while remaining > 0 {
        let size = remaining.min(PAGE_SIZE);
        let page = context.documents::<u64>(&prefix, before.as_deref(), size)?;
        before = page.last().map(|(key, _)| key.clone());
        let count = page.len();
        pulls.extend(page.into_iter().map(|(_, pull)| pull));
        if count < size {
            break;
        }
        remaining -= count;
    }
    Ok(pulls)
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Predecessor {
    pub pull: u64,
    pub candidate_id: Uuid,
    pub head_ref: String,
    pub head_repository: Option<Uuid>,
    pub head_oid: String,
    pub comparison_revision: Option<u64>,
    pub settings: QueueSettings,
    pub ready: bool,
    pub publishing: Option<String>,
}

pub(super) fn predecessor(
    context: &Transaction<'_>,
    branch: &str,
    pull: u64,
    order_revision: u64,
) -> Result<Predecessor> {
    let Some(job) = context
        .document::<Job>(&key(ACTIVE, pull))?
        .filter(|job| job.queue.is_some() && job.base_ref == branch)
    else {
        return Err(Error::Invalid("Merge queue entry is missing"));
    };
    // Prefix checks only need source identity and proof of a pending or
    // completed tested merge. Reviews and discussion bodies stay in their Cell.
    let record = context.query(
        "SELECT state,base_ref,head_ref,pending_merge,completed_merge FROM repository_pulls WHERE number=?",
        &[integer(pull)?],
    )?;
    let [row] = record.rows.as_slice() else {
        return Err(Error::Invalid(
            "Merge queue predecessor has no pull request",
        ));
    };
    let matches = result_text(row, 1)? == branch
        && result_text(row, 2)? == job.head_ref
        && storage::head_repository(context, pull)? == job.head_repository
        && base::snapshot(context, pull)?.map(|snapshot| snapshot.revision)
            == job.comparison_revision;
    let publishing = if matches {
        let chosen = if row[4] == SqlValue::Null {
            &row[3]
        } else {
            &row[4]
        };
        storage::decode_optional_record::<PullMerge>(chosen)?
            .filter(|record| record.tested_candidate)
            .map(|record| record.commit_oid)
    } else {
        None
    };
    let ready = matches
        && PullState::from_code(result_u64_from_row(row, 0)?)? == PullState::Open
        && readiness::load(context, pull)? == PullReadiness::Ready;
    Ok(Predecessor {
        pull,
        candidate_id: job.candidate_id(order_revision),
        settings: job.queue_settings(),
        head_ref: job.head_ref,
        head_repository: job.head_repository,
        head_oid: job.head_oid,
        comparison_revision: job.comparison_revision,
        ready,
        publishing,
    })
}
