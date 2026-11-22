use super::*;
use crate::repository::code_index::Row;

pub(super) async fn inspect(
    rows: &[Row],
    snapshot: &RemoteGitSnapshot,
    pattern: &Pattern,
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
    let limit = if pattern.paths_only {
        window.results
    } else {
        window.paths
    };
    let mut visited = range.len();
    let mut candidates = Vec::new();
    for (i, row) in range.iter().enumerate() {
        if !eligible(row, pattern) {
            continue;
        }
        if candidates.len() == limit {
            visited = i;
            break;
        }
        candidates.push((i, row));
    }
    let mut page = Page {
        commit: snapshot.commit_oid().to_string(),
        items: Vec::new(),
        scanned: 0,
        skipped: 0,
        next: None,
    };
    let mut verified = 0;
    'verify: for batch in candidates.chunks(FILE_READ_BATCH) {
        let matches = futures_util::future::try_join_all(batch.iter().map(|(_, row)| async {
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
            if pattern.paths_only {
                Ok(Inspection::Match(FileMatches {
                    path: display_path(entry.path.as_bytes()),
                    path_hex: encode_hex(entry.path.as_bytes()),
                    lines: Vec::new(),
                    more_lines: false,
                }))
            } else {
                file_matches(snapshot, &entry, &pattern.matcher, operation).await
            }
        }))
        .await?;
        if !pattern.paths_only {
            verified += batch.len();
        }
        for ((position, _), result) in batch.iter().zip(matches) {
            if let Inspection::Match(found) = result {
                page.items.push(found);
                if page.items.len() == window.results {
                    visited = position + 1;
                    break 'verify;
                }
            }
        }
    }
    page.scanned = visited;
    if !pattern.paths_only {
        page.skipped = range[..visited]
            .iter()
            .filter(|row| row.filter.is_none())
            .count();
    }
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

fn eligible(row: &Row, pattern: &Pattern) -> bool {
    if pattern.paths_only {
        return pattern.matcher.is_match(&display_path(row.path.as_bytes()));
    }
    row.filter.as_ref().is_some_and(|mask| {
        pattern
            .mask
            .as_ref()
            .is_none_or(|query| mask.may_contain(query, pattern.case_sensitive))
    })
}
