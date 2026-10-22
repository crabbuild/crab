use crate::{
    app::{self, Error, Result},
    cells::repository::{
        PullState, PullSummary,
        rollups::{GetRollups, Head, Rollup},
    },
    repository::Repository,
    server::Server,
};
use std::collections::{BTreeMap, BTreeSet};

pub(in crate::api) async fn load<'a>(
    server: &Server,
    repo: &Repository,
    pulls: impl Iterator<Item = &'a PullSummary>,
) -> Result<BTreeMap<u64, Rollup>> {
    let pulls: Vec<_> = pulls.collect();
    if pulls.is_empty() {
        return Ok(BTreeMap::new());
    }
    let refs: BTreeSet<_> = pulls
        .iter()
        .filter(|pull| pull.state != PullState::Merged)
        .map(|pull| pull.head_ref.clone())
        .collect();
    let heads = if refs.is_empty() {
        BTreeMap::new()
    } else {
        repo.ref_oids(&refs).await?
    };
    let input = pulls
        .iter()
        .map(|pull| Head {
            number: pull.number,
            version: pull.version,
            oid: heads.get(&pull.head_ref).cloned(),
        })
        .collect();
    let cell = app::repository_cell(server, repo)?;
    let rows = app::query_output(
        cell.client
            .query::<GetRollups>(&cell.target, None, input)
            .await,
    )?
    .ok_or(Error::PageChanged)?;
    if rows.len() != pulls.len()
        || rows
            .iter()
            .zip(pulls)
            .any(|(row, pull)| row.number != pull.number)
    {
        return Err(Error::CellContract(
            "Pull rollup response does not match its selection",
        ));
    }
    Ok(rows.into_iter().map(|row| (row.number, row)).collect())
}

#[cfg(test)]
mod tests;
