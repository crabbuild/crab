//! Shallow boundary computation and `.git/shallow` file management.
//!
//! Provides boundary computation over a [`CommitGraphTraversal`],
//! pack filtering by depth, and helpers for writing/removing the
//! `.git/shallow` sentinel file.

use std::path::Path;

use tokio::fs;
use tracing::{Instrument, debug};

use crate::core::error::{CrabError, Result};
use crab_metadata::commit_graph::CommitGraphTraversal;
use crab_metadata::manifests::PackList;

/// Compute the shallow boundary for a given depth and ref set.
///
/// Walks the commit graph from each ref tip, stopping
/// at depth `depth`. Commits at exactly depth N form the boundary.
///
/// Special cases:
/// - `depth == 0`: the boundary equals the ref tips themselves (no commits
///   are fetched).
/// - `depth` exceeds the graph height: the boundary is empty (full clone).
/// - Empty graph or no matching tips: the boundary is empty.
pub fn compute_shallow_boundary(
    graph: &dyn CommitGraphTraversal,
    ref_tips: &[String],
    depth: u32,
) -> Vec<String> {
    let boundary = graph.shallow_boundary(ref_tips, depth).unwrap_or_default();
    debug!(
        boundary_len = boundary.len(),
        depth,
        tips = ref_tips.len(),
        "computed shallow boundary"
    );
    boundary
}

/// Filter packs to only those containing objects within the shallow boundary.
///
/// Uses BFS from `ref_tips` through the commit graph (bounded by `boundary`)
/// to determine which commits are reachable. Packs whose `ref_tips` metadata
/// intersects the reachable set are included. An empty tip set carries no
/// filtering proof, so that pack is included to preserve the superset guarantee.
pub fn filter_packs_by_depth(
    pack_list: &PackList,
    graph: &dyn CommitGraphTraversal,
    boundary: &[String],
    ref_tips: &[String],
) -> Vec<String> {
    let Some(reachable) = graph.reachable_to_boundary(ref_tips, boundary) else {
        return pack_list
            .entries
            .iter()
            .map(|entry| entry.pack_id.clone())
            .collect();
    };

    let mut result = Vec::new();
    for entry in &pack_list.entries {
        if entry.ref_tips.is_empty()
            || entry
                .ref_tips
                .iter()
                .any(|tip| reachable.contains(tip.as_str()))
        {
            result.push(entry.pack_id.clone());
        }
    }

    if !result.is_empty() {
        debug!(
            total = pack_list.entries.len(),
            filtered = result.len(),
            reachable_commits = reachable.len(),
            "filtered packs by depth"
        );
    }

    result
}

/// Write the `.git/shallow` file with boundary commit OIDs.
///
/// Each OID is written on its own line. If the boundary is empty the file
/// is not created (an empty shallow file is meaningless to git).
///
/// Accepts any slice whose elements implement `AsRef<str>`, so it works
/// with both `String` and `Cow<str>`.
///
/// ## `gix-shallow` adoption
///
/// `gix-shallow` provides `read()` and `write()` APIs. The read API
/// decodes each non-empty line as a `gix_hash::ObjectId`; the write
/// API takes a `gix_lock::File` plus a delta list of
/// `Update::Shallow(oid)` / `Update::Unshallow(oid)` — it's structured
/// around fetch-time shallow-boundary updates rather than whole-file
/// replacement, which is crab's current shape.
///
/// Scope: at this stage this site wraps I/O in `gix_boundary!` spans
/// so flamegraphs attribute shallow-file time to the gitoxide side of
/// the adoption boundary. Full `gix_shallow::write` adoption would
/// require restructuring shallow-file production around delta
/// [`gix_shallow::Update`] values plus a `gix_lock::File`, which is
/// more code than crab currently has for the plain-text path. The
/// LOC target "shrink git/shallow.rs" is documented in the task
/// summary as "minimal add — boundary tracing only — because
/// `gix-shallow` is a parser/delta writer, not a whole-file store
/// abstraction."
pub async fn write_shallow_file(git_dir: &Path, boundary: &[impl AsRef<str>]) -> Result<()> {
    if boundary.is_empty() {
        debug!("empty boundary — skipping .git/shallow write");
        return Ok(());
    }

    let content: String = boundary
        .iter()
        .map(std::convert::AsRef::as_ref)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let shallow_path = git_dir.join("shallow");
    let write_path = shallow_path.clone();
    let temp_dir = git_dir.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;

        let mut temp = tempfile::NamedTempFile::new_in(temp_dir)?;
        temp.write_all(content.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(write_path).map_err(|error| error.error)?;
        Ok::<(), std::io::Error>(())
    })
    .instrument(crate::gix_boundary!("shallow", "write"))
    .await
    .map_err(|error| CrabError::Internal(format!("shallow write join error: {error}")))??;

    debug!(
        path = %shallow_path.display(),
        entries = boundary.len(),
        "wrote .git/shallow"
    );

    Ok(())
}

/// Remove the `.git/shallow` file (for `--unshallow`).
///
/// Silently succeeds if the file does not exist.
pub async fn remove_shallow_file(git_dir: &Path) -> Result<()> {
    let shallow_path = git_dir.join("shallow");
    match fs::remove_file(&shallow_path)
        .instrument(crate::gix_boundary!("shallow", "remove"))
        .await
    {
        Ok(()) => {
            debug!(path = %shallow_path.display(), "removed .git/shallow");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %shallow_path.display(), "no .git/shallow to remove");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Read the `.git/shallow` file and return boundary OIDs as lowercase
/// hex strings.
///
/// Under `--features gix-revwalk` this delegates to
/// [`gix_shallow::read`], which parses every non-empty line as a
/// `gix_hash::ObjectId`. Outside the feature, falls back to a plain
/// tokio line-reader. Returns `Ok(vec![])` for a missing file (git's
/// semantics — no shallow file means not a shallow clone).
pub async fn read_shallow_file(git_dir: &Path) -> Result<Vec<String>> {
    let shallow_path = git_dir.join("shallow");

    #[cfg(feature = "gix-revwalk")]
    {
        // `gix_shallow::read` is a sync API; hop to blocking pool
        // to keep the async caller non-blocking. The file is small
        // (tens of OIDs in practice) so the hop is negligible.
        let path = shallow_path.clone();
        let boundary = tokio::task::spawn_blocking(move || gix_shallow::read(&path))
            .instrument(crate::gix_boundary!("shallow", "read"))
            .await
            .map_err(|e| {
                crate::core::error::CrabError::Internal(format!("shallow read join error: {e}"))
            })?
            .map_err(|e| {
                crate::core::error::CrabError::Internal(format!("gix_shallow::read failed: {e}"))
            })?;

        return Ok(match boundary {
            None => Vec::new(),
            Some(nonempty) => nonempty
                .into_iter()
                .map(|oid| oid.to_hex().to_string())
                .collect(),
        });
    }

    #[cfg(not(feature = "gix-revwalk"))]
    {
        match fs::read_to_string(&shallow_path)
            .instrument(crate::gix_boundary!("shallow", "read"))
            .await
        {
            Ok(content) => Ok(content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_ascii_lowercase())
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::borrow::Cow;

    use super::*;
    use crab_metadata::commit_graph::{CommitEntry, CommitGraphSummary};
    use crab_metadata::manifests::{PackEntry, PackList};

    fn make_summary(commits: Vec<CommitEntry>) -> CommitGraphSummary {
        CommitGraphSummary {
            generation: 1,
            commits,
        }
    }

    fn linear_chain(len: usize) -> (CommitGraphSummary, String) {
        // c0 (root) <- c1 <- c2 <- ... <- c{len-1} (tip)
        let mut commits = Vec::with_capacity(len);
        for i in 0..len {
            let parents = if i == 0 {
                vec![]
            } else {
                vec![format!("c{}", i - 1)]
            };
            commits.push(CommitEntry {
                oid: format!("c{i}"),
                gen_number: i as u64,
                parents,
            });
        }
        let tip = format!("c{}", len - 1);
        (make_summary(commits), tip)
    }

    // --- compute_shallow_boundary ---

    #[test]
    fn empty_graph_returns_empty_boundary() {
        let summary = make_summary(vec![]);
        let boundary = compute_shallow_boundary(&summary, &["tip".into()], 5);
        assert!(boundary.is_empty());
    }

    #[test]
    fn empty_tips_returns_empty_boundary() {
        let (summary, _tip) = linear_chain(5);
        let boundary = compute_shallow_boundary(&summary, &[], 3);
        assert!(boundary.is_empty());
    }

    #[test]
    fn depth_zero_returns_tips_as_boundary() {
        let (summary, tip) = linear_chain(5);
        let boundary = compute_shallow_boundary(&summary, &[tip.clone()], 0);
        assert_eq!(boundary, vec![tip]);
    }

    #[test]
    fn unknown_tip_disables_shallow_boundary() {
        let (summary, _tip) = linear_chain(3);
        let boundary = compute_shallow_boundary(&summary, &["c2".into(), "unknown".into()], 0);
        assert!(boundary.is_empty());
    }

    #[test]
    fn depth_one_boundary_is_tip_itself() {
        // depth=1 means only the tip commit; the boundary is the tip.
        let (summary, tip) = linear_chain(5);
        let boundary = compute_shallow_boundary(&summary, &[tip], 1);
        assert_eq!(boundary, vec!["c4".to_string()]);
    }

    #[test]
    fn depth_two_boundary_is_parent_of_tip() {
        let (summary, tip) = linear_chain(5);
        let boundary = compute_shallow_boundary(&summary, &[tip], 2);
        assert_eq!(boundary, vec!["c3".to_string()]);
    }

    #[test]
    fn depth_exceeds_graph_height_returns_empty() {
        let (summary, tip) = linear_chain(3); // c0 <- c1 <- c2
        // depth=10 exceeds the 3-commit chain — no boundary.
        let boundary = compute_shallow_boundary(&summary, &[tip], 10);
        assert!(boundary.is_empty());
    }

    #[test]
    fn compacted_graph_preserves_relative_deepen_and_its_shallow_edge() {
        let (mut summary, tip) = linear_chain(5);
        summary.compact_to_limit(4);

        let initial = compute_shallow_boundary(&summary, &[tip], 1);
        let deepened = compute_shallow_boundary(
            &summary,
            &initial.iter().map(ToString::to_string).collect::<Vec<_>>(),
            3,
        );
        let beyond_retained_history = compute_shallow_boundary(
            &summary,
            &deepened.iter().map(ToString::to_string).collect::<Vec<_>>(),
            10,
        );

        assert_eq!(initial, ["c4"]);
        assert_eq!(deepened, ["c2"]);
        assert_eq!(beyond_retained_history, ["c1"]);
    }

    #[test]
    fn branching_graph_boundary() {
        //   c0 <- c1 <- c2 (tip_a)
        //          \--- c3 (tip_b)
        let summary = make_summary(vec![
            CommitEntry {
                oid: "c0".into(),
                gen_number: 0,
                parents: vec![],
            },
            CommitEntry {
                oid: "c1".into(),
                gen_number: 1,
                parents: vec!["c0".into()],
            },
            CommitEntry {
                oid: "c2".into(),
                gen_number: 2,
                parents: vec!["c1".into()],
            },
            CommitEntry {
                oid: "c3".into(),
                gen_number: 2,
                parents: vec!["c1".into()],
            },
        ]);
        let boundary = compute_shallow_boundary(&summary, &["c2".into(), "c3".into()], 2);
        // At depth 2 from both tips, c1 is the boundary.
        assert_eq!(boundary, vec!["c1".to_string()]);
    }

    #[test]
    fn merge_commit_boundary() {
        //   c0 <- c1 \
        //              c3 (merge, tip)
        //   c0 <- c2 /
        let summary = make_summary(vec![
            CommitEntry {
                oid: "c0".into(),
                gen_number: 0,
                parents: vec![],
            },
            CommitEntry {
                oid: "c1".into(),
                gen_number: 1,
                parents: vec!["c0".into()],
            },
            CommitEntry {
                oid: "c2".into(),
                gen_number: 1,
                parents: vec!["c0".into()],
            },
            CommitEntry {
                oid: "c3".into(),
                gen_number: 2,
                parents: vec!["c1".into(), "c2".into()],
            },
        ]);
        let boundary = compute_shallow_boundary(&summary, &["c3".into()], 2);
        // depth 2 from c3: c3 is depth 1, c1 and c2 are depth 2 (boundary).
        assert_eq!(boundary, vec!["c1".to_string(), "c2".to_string()]);
    }

    #[test]
    fn octopus_merge_boundary() {
        // Octopus merge: c4 has three parents c1, c2, c3.
        //   c0 <- c1 \
        //   c0 <- c2  }- c4 (tip, octopus merge)
        //   c0 <- c3 /
        //
        // At depth=2 from c4, all three parents should be in the
        // boundary. This mirrors git's own --depth semantics, which
        // treats every parent as one hop. See finding CR6-F2.
        let summary = make_summary(vec![
            CommitEntry {
                oid: "c0".into(),
                gen_number: 0,
                parents: vec![],
            },
            CommitEntry {
                oid: "c1".into(),
                gen_number: 1,
                parents: vec!["c0".into()],
            },
            CommitEntry {
                oid: "c2".into(),
                gen_number: 1,
                parents: vec!["c0".into()],
            },
            CommitEntry {
                oid: "c3".into(),
                gen_number: 1,
                parents: vec!["c0".into()],
            },
            CommitEntry {
                oid: "c4".into(),
                gen_number: 2,
                parents: vec!["c1".into(), "c2".into(), "c3".into()],
            },
        ]);
        let boundary = compute_shallow_boundary(&summary, &["c4".into()], 2);
        // All three parents are at depth 2 and form the boundary.
        let mut boundary_sorted = boundary;
        boundary_sorted.sort();
        assert_eq!(
            boundary_sorted,
            vec!["c1".to_string(), "c2".to_string(), "c3".to_string()]
        );
    }

    #[test]
    fn octopus_merge_depth_three_reaches_common_root() {
        // Same octopus graph, but depth=3 reaches c0.
        let summary = make_summary(vec![
            CommitEntry {
                oid: "c0".into(),
                gen_number: 0,
                parents: vec![],
            },
            CommitEntry {
                oid: "c1".into(),
                gen_number: 1,
                parents: vec!["c0".into()],
            },
            CommitEntry {
                oid: "c2".into(),
                gen_number: 1,
                parents: vec!["c0".into()],
            },
            CommitEntry {
                oid: "c3".into(),
                gen_number: 1,
                parents: vec!["c0".into()],
            },
            CommitEntry {
                oid: "c4".into(),
                gen_number: 2,
                parents: vec!["c1".into(), "c2".into(), "c3".into()],
            },
        ]);
        let boundary = compute_shallow_boundary(&summary, &["c4".into()], 3);
        assert_eq!(boundary, vec!["c0".to_string()]);
    }

    // --- filter_packs_by_depth ---

    #[test]
    fn filter_packs_includes_unproven_packs_unconditionally() {
        // Empty tip proofs are always included regardless of the commit graph
        // or boundary, preserving the superset guarantee.
        let pack_list = PackList {
            generation: 1,
            entries: vec![
                PackEntry::new("pack_a", 100, Vec::new()),
                PackEntry::new("pack_b", 200, Vec::new()),
            ],
        };
        let summary = make_summary(vec![]);
        let ids = filter_packs_by_depth(&pack_list, &summary, &[] as &[String], &[]);
        assert_eq!(ids, vec!["pack_a".to_string(), "pack_b".to_string()]);
    }

    #[test]
    fn filter_packs_includes_empty_hints_unconditionally() {
        let pack_list = PackList {
            generation: 1,
            entries: vec![PackEntry {
                pack_id: "pack_a".to_owned(),
                size: 100,
                ref_tips: Some(Vec::new()),
            }],
        };
        let summary = make_summary(vec![]);

        let ids = filter_packs_by_depth(&pack_list, &summary, &[], &[] as &[String]);

        assert_eq!(ids, vec!["pack_a".to_owned()]);
    }

    #[test]
    fn filter_packs_empty_list_returns_empty() {
        let pack_list = PackList {
            generation: 0,
            entries: vec![],
        };
        let summary = make_summary(vec![]);
        let ids = filter_packs_by_depth(&pack_list, &summary, &[] as &[String], &[]);
        assert!(ids.is_empty());
    }

    #[test]
    fn filter_packs_excludes_unreachable_metadata_packs() {
        // c0 <- c1 <- c2 (tip). With boundary at c1, reachable = {c2, c1}.
        // A pack whose ref_tips are only "c0" should be excluded.
        let (summary, _) = linear_chain(3);
        let ref_tips = vec!["c2".to_string()];
        let boundary = vec!["c1".to_string()];

        let pack_list = PackList {
            generation: 1,
            entries: vec![
                PackEntry::new("reachable_pack", 100, vec!["c2".to_string()]),
                PackEntry::new("unreachable_pack", 200, vec!["c0".to_string()]),
                PackEntry::new("unproven_pack", 300, Vec::new()),
            ],
        };

        let ids = filter_packs_by_depth(&pack_list, &summary, &boundary, &ref_tips);
        assert_eq!(
            ids,
            vec!["reachable_pack".to_string(), "unproven_pack".to_string()]
        );
    }

    #[test]
    fn filter_packs_includes_pack_with_boundary_commit_tip() {
        // Boundary commits are reachable — packs referencing them should be included.
        let (summary, _) = linear_chain(3);
        let ref_tips = vec!["c2".to_string()];
        let boundary = vec!["c1".to_string()];

        let pack_list = PackList {
            generation: 1,
            entries: vec![PackEntry::new("boundary_pack", 100, vec!["c1".to_string()])],
        };

        let ids = filter_packs_by_depth(&pack_list, &summary, &boundary, &ref_tips);
        assert_eq!(ids, vec!["boundary_pack".to_string()]);
    }

    #[test]
    fn filter_packs_without_complete_tip_proof_returns_full_superset() {
        // Missing graph coverage must not exclude a pack needed by the unknown tip.
        let (summary, _) = linear_chain(3);
        let ref_tips = vec!["unknown_tip".to_string()];
        let boundary: Vec<String> = vec![];

        let pack_list = PackList {
            generation: 1,
            entries: vec![
                PackEntry::new("metadata_pack", 100, vec!["c0".to_string()]),
                PackEntry::new("unproven_pack", 200, Vec::new()),
            ],
        };

        let ids = filter_packs_by_depth(&pack_list, &summary, &boundary, &ref_tips);
        assert_eq!(
            ids,
            vec!["metadata_pack".to_string(), "unproven_pack".to_string()]
        );
    }

    // --- write_shallow_file / remove_shallow_file ---

    #[tokio::test]
    async fn write_and_read_shallow_file() {
        let dir = tempfile::tempdir().unwrap();
        let boundary = vec!["abc123".to_string(), "def456".to_string()];
        write_shallow_file(dir.path(), &boundary).await.unwrap();

        let content = tokio::fs::read_to_string(dir.path().join("shallow"))
            .await
            .unwrap();
        assert_eq!(content, "abc123\ndef456\n");
    }

    #[tokio::test]
    async fn write_shallow_file_accepts_cow_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let boundary: Vec<Cow<'_, str>> =
            vec![Cow::Borrowed("aaa111"), Cow::Owned("bbb222".to_string())];
        write_shallow_file(dir.path(), &boundary).await.unwrap();

        let content = tokio::fs::read_to_string(dir.path().join("shallow"))
            .await
            .unwrap();
        assert_eq!(content, "aaa111\nbbb222\n");
    }

    #[tokio::test]
    async fn write_shallow_file_atomically_replaces_existing_boundary() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("shallow"), b"old\n")
            .await
            .unwrap();

        write_shallow_file(dir.path(), &["new"]).await.unwrap();

        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("shallow"))
                .await
                .unwrap(),
            "new\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_shallow_write_preserves_existing_boundary() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let shallow_path = dir.path().join("shallow");
        tokio::fs::write(&shallow_path, b"old\n").await.unwrap();
        let original_permissions = std::fs::metadata(dir.path()).unwrap().permissions();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = write_shallow_file(dir.path(), &["new"]).await;

        std::fs::set_permissions(dir.path(), original_permissions).unwrap();
        assert!(result.is_err());
        assert_eq!(
            tokio::fs::read_to_string(shallow_path).await.unwrap(),
            "old\n"
        );
    }

    #[tokio::test]
    async fn write_empty_boundary_does_not_create_file() {
        let dir = tempfile::tempdir().unwrap();
        write_shallow_file(dir.path(), &[] as &[String])
            .await
            .unwrap();
        assert!(!dir.path().join("shallow").exists());
    }

    #[tokio::test]
    async fn remove_shallow_file_deletes_it() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("shallow"), b"abc\n")
            .await
            .unwrap();
        remove_shallow_file(dir.path()).await.unwrap();
        assert!(!dir.path().join("shallow").exists());
    }

    #[tokio::test]
    async fn remove_shallow_file_succeeds_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        // No shallow file exists — should succeed silently.
        remove_shallow_file(dir.path()).await.unwrap();
    }
}
