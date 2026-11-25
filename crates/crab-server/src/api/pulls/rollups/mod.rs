use crate::{
    app::{self, Error, Result},
    cells::repository::{
        PullState, PullSummary,
        rollups::{GetRollups, Head, Rollup},
        search::heads::Context,
    },
    repository::Repository,
    server::Server,
};
use std::collections::{BTreeMap, BTreeSet};

pub(in crate::api) async fn load<'a>(
    server: &Server,
    repo: &Repository,
    captured: Option<&Context>,
    check_revision: Option<u64>,
    pulls: impl Iterator<Item = &'a PullSummary>,
) -> Result<BTreeMap<u64, Rollup>> {
    let pulls: Vec<_> = pulls.collect();
    if pulls.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut refs = BTreeMap::<_, BTreeSet<_>>::new();
    for pull in pulls
        .iter()
        .filter(|pull| pull.state != PullState::Merged && captured.is_none())
    {
        refs.entry(pull.head_repository)
            .or_default()
            .insert(pull.head_ref.clone());
    }
    let mut heads = BTreeMap::new();
    for (id, refs) in refs {
        let tips = match id {
            None => repo.ref_oids(&refs).await?,
            Some(fork) => match super::git::fork_repository(server, repo, fork) {
                Ok(fork) => fork.ref_oids(&refs).await?,
                Err(Error::NotFound) => continue,
                Err(error) => return Err(error),
            },
        };
        heads.insert(id, tips);
    }
    let heads = pulls
        .iter()
        .map(|pull| Head {
            number: pull.number,
            version: pull.version,
            oid: match captured {
                Some(captured) => pull.head_repository.and_then(|fork| {
                    captured
                        .fork_heads
                        .get(&format!("{fork}:{}", pull.head_ref))
                        .cloned()
                }),
                None => heads
                    .get(&pull.head_repository)
                    .and_then(|tips| tips.get(&pull.head_ref))
                    .cloned(),
            },
        })
        .collect();
    let input = crate::cells::repository::rollups::Input {
        heads,
        source_token: captured.and_then(|context| context.source_token.clone()),
        check_revision,
    };
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
