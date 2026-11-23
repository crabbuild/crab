use super::*;
use crate::cells::{
    Address,
    organization::properties::{
        Repository as Binding,
        search::{MATCH_BATCH, MatchRepositories, Matches, Selection, Term},
    },
};

const SCAN_LIMIT: usize = MATCH_BATCH * 4;

pub(super) async fn scan<'a>(
    server: &Server,
    organization: Uuid,
    terms: &[Term],
    candidates: &'a [Arc<Repository>],
    start: usize,
    limit: usize,
    after: Option<blake3::Hash>,
) -> Result<Window<'a>> {
    let end = (start + SCAN_LIMIT).min(candidates.len());
    let mut offset = start;
    let mut window = Window::default();
    loop {
        let stop = (offset + MATCH_BATCH).min(end);
        let batch = &candidates[offset..stop];
        let result = matches(server, organization, terms, batch).await?;
        if let Some((_, revision)) = window.property_state {
            if result.revision != revision {
                return Err(Error::PageChanged);
            }
        } else {
            let mut proof = blake3::Hasher::new_keyed(&server.cursor_key);
            proof.update(b"crab-server.repository-property-search.v1");
            proof.update(&result.revision.to_le_bytes());
            for repo in candidates {
                proof.update(repo.id.as_bytes());
                proof.update(&repo.ownership_epoch.to_le_bytes());
            }
            let state = proof.finalize();
            if after.is_some_and(|expected| state != expected) {
                return Err(Error::PageChanged);
            }
            window.property_state = Some((state, result.revision));
        }
        let matched = result.repositories.ok_or(Error::Invalid(
            "Choose an existing repository property and a valid value",
        ))?;
        let matched: HashSet<_> = matched.into_iter().collect();
        window.entries.extend(
            batch
                .iter()
                .filter(|repo| matched.contains(&repo.id))
                .take(limit + 1 - window.entries.len()),
        );
        if window.entries.len() > limit || stop == end {
            break;
        }
        offset = stop;
    }
    if window.entries.len() <= limit && end < candidates.len() {
        // Sparse matches can need several pages. Resume after the examined
        // range even if it produced no visible results.
        window.boundary = candidates.get(end - 1);
    }
    Ok(window)
}

pub(super) async fn revision(server: &Server, organization: Uuid) -> Result<u64> {
    Ok(matches(server, organization, &[], &[]).await?.revision)
}

async fn matches(
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
