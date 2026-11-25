use crate::{
    app::{self, Error, Result},
    cells::repository::{
        PullState, PullSummary,
        rollups::{GetRollups, Head, Rollup},
        search::check_status::Context,
    },
    repository::Repository,
    server::Server,
};
use std::collections::{BTreeMap, BTreeSet};

pub(in crate::api) async fn load<'a>(
    server: &Server,
    repo: &Repository,
    source_token: Option<&str>,
    checked: Option<(&Context, u64)>,
    pulls: impl Iterator<Item = &'a PullSummary>,
) -> Result<BTreeMap<u64, Rollup>> {
    let pulls: Vec<_> = pulls.collect();
    if pulls.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut refs = BTreeMap::<_, BTreeSet<_>>::new();
    for pull in pulls.iter().filter(|pull| {
        pull.state != PullState::Merged && source_token.is_none() && checked.is_none()
    }) {
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
            oid: match checked {
                Some((captured, _)) => pull.head_repository.and_then(|fork| {
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
        source_token: checked.map_or_else(
            || source_token.map(str::to_owned),
            |(captured, _)| captured.source_token.clone(),
        ),
        check_revision: checked.map(|(_, revision)| revision),
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
