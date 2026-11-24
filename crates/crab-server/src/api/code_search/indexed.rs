use super::*;
use crate::repository::code_index::Row;

pub(super) const COMPLEX_PATHS_PER_PAGE: usize = 4_096;

pub(super) async fn inspect(
    rows: &[Row],
    snapshot: &RemoteGitSnapshot,
    pattern: &Pattern,
    scope: &Facts,
    window: &Window<'_>,
    operation: &OperationContext,
) -> crab_remote_git::Result<Scan> {
    let mut prefix = pattern.directory.as_bytes().to_vec();
    if !prefix.is_empty() {
        prefix.push(b'/');
    }
    let after = window.resume.map(|(_, path)| path.as_bytes());
    let start = rows.partition_point(|row| {
        let path = row.path.as_bytes();
        path < prefix.as_slice() || after.is_some_and(|after| path <= after)
    });
    let range = &rows[start..];
    let range = &range[..range.partition_point(|row| row.path.as_bytes().starts_with(&prefix))];
    let limit = if !pattern.uses_text {
        window.results
    } else {
        window.paths
    };
    // Selective expressions can find no candidates. Advance a bounded path
    // cursor even on an empty page to cap predicate work across the index.
    let examined = if pattern.query.bounded {
        range.len().min(COMPLEX_PATHS_PER_PAGE)
    } else {
        range.len()
    };
    let mut visited = examined;
    let mut candidates = Vec::new();
    let mut skipped = 0;
    for (i, row) in range.iter().take(examined).enumerate() {
        let mut facts = pattern.file_facts(row.path.as_bytes(), scope);
        match pattern.query.evaluate(&facts).0 {
            Truth::No => continue,
            Truth::Unknown if row.filter.is_none() => {
                skipped += 1;
                continue;
            }
            Truth::Unknown => {
                pattern.index_facts(row, &mut facts);
                if pattern.query.evaluate(&facts).0 == Truth::No {
                    continue;
                }
            }
            Truth::Yes => (),
        }
        if candidates.len() == limit {
            visited = i;
            break;
        }
        candidates.push((i, row, skipped));
    }
    let mut page = Page::for_commit(snapshot.commit_oid());
    let mut verified = 0;
    'verify: for batch in candidates.chunks(FILE_READ_BATCH) {
        let matches = futures_util::future::try_join_all(batch.iter().map(|(_, row, _)| async {
            // The persisted artifact only selects candidates. The immutable
            // Git tree still proves each returned path and supplies its text.
            let entry = snapshot
                .entry(&row.path, operation)
                .await?
                .ok_or(Error::PathNotFound)?;
            if entry.oid != row.oid || entry.kind != EntryKind::Blob {
                return Err(Error::InternalInvariant {
                    invariant: "code index path disagreed with the pinned Git tree",
                });
            }
            pattern.inspect_entry(&entry, scope, operation).await
        }))
        .await?;
        verified += matches.iter().filter(|(_, read)| *read).count();
        for ((position, _, skipped_before), (result, _)) in batch.iter().zip(matches) {
            page.skipped += usize::from(matches!(result, Inspection::Skipped));
            if let Inspection::Match(found) = result {
                page.items.push(found);
                if page.items.len() == window.results {
                    visited = position + 1;
                    skipped = *skipped_before;
                    break 'verify;
                }
            }
        }
    }
    page.scanned = visited;
    page.skipped += skipped;
    let after = visited
        .checked_sub(1)
        .filter(|_| visited < range.len())
        .map(|last| range[last].path.clone());
    Ok(Scan {
        commit: snapshot.commit_oid(),
        result: page,
        after,
        acceleration: Acceleration::Indexed,
        verified,
    })
}
