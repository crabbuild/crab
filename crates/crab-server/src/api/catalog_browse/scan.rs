use super::*;
use crate::cells::organization::properties::search::MATCH_BATCH;

const ARCHIVE_BATCH: usize = 16;
const ARCHIVE_LIMIT: usize = 128;
const PROPERTY_LIMIT: usize = MATCH_BATCH * 4;

pub(super) async fn window<'a>(
    server: &Server,
    query: &Query,
    property_organization: Option<Uuid>,
    candidates: &'a [Arc<Repository>],
    start: usize,
    limit: usize,
) -> Result<Window<'a>> {
    if query.archived.is_none() && property_organization.is_none() {
        return Ok(Window {
            entries: candidates[start..(start + limit + 1).min(candidates.len())]
                .iter()
                .collect(),
            ..Window::default()
        });
    }
    // Archive state lives in each Repository Cell. Cap those reads separately
    // from the cheaper batched property reads in the Organization Cell.
    let (batch_size, scan_limit) = if query.archived.is_some() {
        (ARCHIVE_BATCH, ARCHIVE_LIMIT)
    } else {
        (MATCH_BATCH, PROPERTY_LIMIT)
    };
    let end = (start + scan_limit).min(candidates.len());
    let mut offset = start;
    let mut result = Window::default();
    loop {
        let stop = (offset + batch_size).min(end);
        let batch = &candidates[offset..stop];
        let mut available: Vec<_> = batch.iter().collect();
        if let Some(organization) = property_organization {
            let matched = properties::matches(server, organization, &query.terms, batch).await?;
            if result
                .property_revision
                .is_some_and(|expected| expected != matched.revision)
            {
                return Err(Error::PageChanged);
            }
            result.property_revision = Some(matched.revision);
            let Some(matching) = matched.repositories else {
                result.invalid_property = true;
                return Ok(result);
            };
            let matching: HashSet<_> = matching.into_iter().collect();
            available.retain(|repo| matching.contains(&repo.id));
        }
        if let Some(archived) = query.archived {
            let repositories = available.iter().copied().cloned().collect();
            let states = lifecycles(server, repositories).await?;
            for (repo, lifecycle) in available.into_iter().zip(states) {
                result.archive_versions.insert(repo.id, lifecycle.version);
                if lifecycle.archived == archived {
                    result.entries.push(repo);
                }
                if result.entries.len() > limit {
                    break;
                }
            }
        } else {
            result
                .entries
                .extend(available.into_iter().take(limit + 1 - result.entries.len()));
        }
        if result.entries.len() > limit || stop == end {
            break;
        }
        offset = stop;
    }
    if result.entries.len() <= limit && end < candidates.len() {
        // Sparse matches can need multiple pages. Resume after the examined
        // range even if it produced no results.
        result.boundary = candidates.get(end - 1);
    }
    Ok(result)
}

pub(super) async fn lifecycles(
    server: &Server,
    repos: Vec<Arc<Repository>>,
) -> Result<Vec<settings::RepositoryLifecycle>> {
    stream::iter(repos)
        .map(|repo: Arc<Repository>| async move { settings::load_lifecycle(server, &repo).await })
        .buffered(8)
        .try_collect()
        .await
}

pub(super) fn scope(
    server: &Server,
    candidates: &[Arc<Repository>],
    property_revision: Option<u64>,
) -> blake3::Hasher {
    let mut proof = blake3::Hasher::new_keyed(&server.cursor_key);
    proof.update(b"crab-server.repository-property-search.v1");
    proof.update(&property_revision.unwrap_or_default().to_le_bytes());
    for repo in candidates {
        proof.update(repo.id.as_bytes());
        proof.update(&repo.ownership_epoch.to_le_bytes());
    }
    proof
}
