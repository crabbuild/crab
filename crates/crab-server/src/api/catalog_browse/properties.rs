use super::*;
use crate::cells::{
    Address,
    organization::properties::{
        Repository as Binding,
        search::{MatchRepositories, Matches, Selection, Term},
    },
};

pub(super) async fn revision(server: &Server, organization: Uuid) -> Result<u64> {
    Ok(matches(server, organization, &[], &[]).await?.revision)
}

pub(super) async fn matches(
    server: &Server,
    organization: Uuid,
    terms: &[Term],
    repositories: &[Arc<Repository>],
) -> Result<Matches> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let output = crate::app::query_output(
        cells
            .query::<MatchRepositories>(
                Address::Organization(organization),
                None,
                Selection {
                    repositories: repositories
                        .iter()
                        .map(|repo| Binding {
                            id: repo.id,
                            ownership_epoch: repo.ownership_epoch,
                        })
                        .collect(),
                    terms: terms.to_vec(),
                },
            )
            .await,
    )?;
    Ok(output)
}
