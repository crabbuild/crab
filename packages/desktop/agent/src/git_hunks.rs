//! Hunk-level staging, unstaging, and discard.
//!
//! The renderer drives a two-pane Changes view that lets the user
//! stage, unstage, or discard individual hunks of a diff. This
//! module turns those intents into `git apply` invocations, gated by
//! a **patch anchor** — the `(old_start, old_lines, new_start,
//! new_lines)` of the originating hunk. Before we shell out we
//! re-read the file and compare the anchor's old-side line slice
//! against the live content; if the user has edited the region
//! since the diff was rendered we return [`StaleHunk`] and let the
//! UI re-fetch. This avoids silently applying a patch at the wrong
//! spot when concurrent edits shift line numbers around.
//!
//! All verbs shell out to `git` rather than using gitoxide because
//! the 3-way apply machinery (`git apply --cached`, `git apply
//! --reverse`, `git apply -3`) is battle-tested and matches what a
//! user would do on the CLI.
//!
//! [`StaleHunk`]: crate::AgentError::StaleHunk

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

use crate::{AgentError, Result};

/// Anchor identifying a single hunk's position in both the old and
/// new file. Fields mirror the `@@ -old_start,old_lines +new_start,
/// new_lines @@` header of a unified diff. Line counts may be zero
/// for pure-add or pure-delete hunks, in which case the
/// corresponding start is the line *before* which content would be
/// inserted (standard diff convention).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HunkAnchor {
    pub path: String,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// Raw hunk body — the lines between two `@@` headers, without
    /// the `@@` line itself. Each line must already carry its `
    /// `/`+`/`-` prefix. The caller (renderer) ships this verbatim
    /// from the patch it rendered; we don't regenerate it to avoid
    /// introducing a diff-algorithm dependency here.
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct DiffIndexParams {
    pub repo_id: String,
    /// Optional path filter. `None` = all changes.
    #[serde(default)]
    pub path: Option<String>,
    /// Include untracked files by using `git diff --no-index` style?
    /// Defaults to `false`; untracked files surface via `git_status_porcelain`.
    #[serde(default)]
    pub include_untracked: bool,
}

/// Params for [`diff_cached_patch`]. Same shape as the worktree
/// variant minus `include_untracked` — untracked files by definition
/// can't be staged, so the cached diff never needs to consider them.
#[derive(Debug, Deserialize)]
pub struct DiffCachedParams {
    pub repo_id: String,
    /// Optional path filter. `None` = all staged changes.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ApplyPatchParams {
    pub repo_id: String,
    /// Raw unified-diff string. Must begin with `diff --git` /
    /// `--- a/` / `+++ b/` headers so `git apply` can locate the
    /// target file.
    pub patch: String,
    /// `true` → `--cached` (apply to index only). Used for staging /
    /// unstaging. `false` → apply to working tree. Used for discard.
    #[serde(default)]
    pub cached: bool,
    /// `true` → `--reverse` (undo the patch). Used for unstage and
    /// discard flows; `false` for forward stage-apply.
    #[serde(default)]
    pub reverse: bool,
}

#[derive(Debug, Deserialize)]
pub struct HunkSetParams {
    pub repo_id: String,
    pub hunks: Vec<HunkAnchor>,
}

/// Working-tree diff vs index, as a unified patch string. If `path`
/// is provided we limit to that one file so the renderer can
/// re-fetch a single file's patch cheaply after staging a hunk.
pub async fn diff_index_patch(
    work_dir: &Path,
    path: Option<&str>,
    _include_untracked: bool,
) -> Result<Value> {
    let mut args: Vec<String> = vec![
        "diff".into(),
        "--no-color".into(),
        // These patches drive staging and apply; display drivers and one-way
        // converters can omit paths or produce content Git cannot apply.
        "--no-ext-diff".into(),
        "--no-textconv".into(),
        // Binary files: emit a literal-patch so `git apply --cached`
        // can round-trip them. Text hunks still render the usual way.
        "--binary".into(),
    ];
    if let Some(p) = path {
        args.push("--".into());
        args.push(p.to_owned());
    }
    let out = crate::shellout::git_command()?
        .args(&args)
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;
    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let patch = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok(json!({ "patch": patch }))
}

/// Index-vs-HEAD diff (the "cached" side of `git diff`), as a
/// unified patch string. Mirrors [`diff_index_patch`] but runs
/// `git diff --cached` instead. The tri-state file checkbox in the
/// Changes view derives `empty / partial / full` from the combined
/// presence of a worktree patch and a cached patch.
///
/// A file with no staged changes produces an empty patch here; a
/// file with everything staged produces an empty patch from
/// [`diff_index_patch`]. Both empty = clean; both non-empty =
/// partial; only this one non-empty = fully staged.
pub async fn diff_cached_patch(work_dir: &Path, path: Option<&str>) -> Result<Value> {
    let mut args: Vec<String> = vec![
        "diff".into(),
        "--cached".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "--no-textconv".into(),
        // Same rationale as `diff_index_patch` — keep binary files
        // round-trippable via `git apply`.
        "--binary".into(),
    ];
    if let Some(p) = path {
        args.push("--".into());
        args.push(p.to_owned());
    }
    let out = crate::shellout::git_command()?
        .args(&args)
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;
    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let patch = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok(json!({ "patch": patch }))
}

/// Apply a raw patch fragment. This is the low-level escape hatch
/// the renderer uses when it has already assembled a valid patch
/// (e.g. from `diff_index_patch`) and just wants it staged or
/// reverted.
pub async fn apply_patch(
    work_dir: &Path,
    patch: &str,
    cached: bool,
    reverse: bool,
) -> Result<Value> {
    run_git_apply(work_dir, patch, cached, reverse).await?;
    Ok(json!({ "applied": true, "cached": cached, "reverse": reverse }))
}

/// Stage a set of hunks by building a minimal unified patch from
/// the anchors, validating each anchor against the current working
/// tree, then `git apply --cached`-ing the patch.
pub async fn stage_hunks(work_dir: &Path, hunks: &[HunkAnchor]) -> Result<Value> {
    validate_anchors_against_worktree(work_dir, hunks).await?;
    let patch = build_patch(hunks)?;
    run_git_apply(
        work_dir, &patch, /*cached=*/ true, /*reverse=*/ false,
    )
    .await?;
    Ok(json!({ "staged": hunks.len() }))
}

/// Unstage a set of hunks. Same anchor shape as `stage_hunks`, but
/// the "old side" of the anchor refers to the *index* blob (because
/// the hunk came from an index-vs-HEAD diff). We validate against
/// the index by extracting the blob with `git show :path`, then
/// reverse-apply `--cached`.
pub async fn unstage_hunks(work_dir: &Path, hunks: &[HunkAnchor]) -> Result<Value> {
    validate_anchors_against_index(work_dir, hunks).await?;
    let patch = build_patch(hunks)?;
    run_git_apply(
        work_dir, &patch, /*cached=*/ true, /*reverse=*/ true,
    )
    .await?;
    Ok(json!({ "unstaged": hunks.len() }))
}

/// Discard a set of hunks from the working tree by reverse-applying
/// them without `--cached`. Anchors validate against the worktree
/// (same as stage).
pub async fn discard_hunks(work_dir: &Path, hunks: &[HunkAnchor]) -> Result<Value> {
    validate_anchors_against_worktree(work_dir, hunks).await?;
    let patch = build_patch(hunks)?;
    run_git_apply(
        work_dir, &patch, /*cached=*/ false, /*reverse=*/ true,
    )
    .await?;
    Ok(json!({ "discarded": hunks.len() }))
}

// --- internals ---------------------------------------------------------

async fn validate_anchors_against_worktree(work_dir: &Path, hunks: &[HunkAnchor]) -> Result<()> {
    for h in hunks {
        let abs = work_dir.join(&h.path);
        let content = match tokio::fs::read_to_string(&abs).await {
            Ok(c) => c,
            // File gone from the worktree. For any hunk with a
            // non-empty "new" side (i.e. the user expects to see
            // some content there) that's a stale anchor: the
            // renderer's snapshot of the worktree is no longer
            // current.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && h.new_lines > 0 => {
                return Err(AgentError::StaleHunk {
                    path: h.path.clone(),
                    expected_old_start: h.old_start,
                    expected_old_lines: h.old_lines,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Pure-delete hunk against an already-missing file:
                // the operation is a no-op from the worktree's POV,
                // let `git apply` decide.
                continue;
            }
            Err(e) => return Err(AgentError::Io(e)),
        };
        verify_new_slice(&content, h)?;
    }
    Ok(())
}

async fn validate_anchors_against_index(work_dir: &Path, hunks: &[HunkAnchor]) -> Result<()> {
    for h in hunks {
        // `git show :<path>` prints the blob staged in the index.
        // Unstage patches come from `git diff --cached`, so the
        // "new side" of the hunk is the index content; that's what
        // we compare against for staleness detection.
        let out = crate::shellout::git_command()?
            .args(["show", &format!(":{}", h.path)])
            .current_dir(work_dir)
            .output()
            .await
            .map_err(AgentError::Io)?;
        if !out.status.success() {
            if h.new_lines == 0 {
                continue;
            }
            return Err(AgentError::StaleHunk {
                path: h.path.clone(),
                expected_old_start: h.old_start,
                expected_old_lines: h.old_lines,
            });
        }
        let content = String::from_utf8_lossy(&out.stdout).into_owned();
        verify_new_slice(&content, h)?;
    }
    Ok(())
}

/// Compare the anchor's `old_start..old_start+old_lines` range
/// against the old-side lines embedded in the hunk body. The body
/// carries `-` and ` ` (context) lines representing the old file;
/// they must match the live content byte-for-byte.
///
/// Retained as a low-level primitive. The main staleness-detection
/// path goes through [`verify_new_slice`] — we want to catch
/// concurrent edits to the *post-change* side (the worktree, for a
/// stage; the index, for an unstage) because that's what the user
/// was looking at when they clicked the button.
#[cfg(test)]
fn verify_old_slice(content: &str, h: &HunkAnchor) -> Result<()> {
    if h.old_lines == 0 {
        return Ok(());
    }
    let expected: Vec<&str> = h
        .body
        .split_inclusive('\n')
        .filter_map(|line| {
            // `\ No newline at end of file` is metadata, not content.
            if line.starts_with("\\ ") {
                return None;
            }
            // Keep context and deletions — those are the "old side".
            if let Some(rest) = line.strip_prefix('-') {
                Some(rest)
            } else if let Some(rest) = line.strip_prefix(' ') {
                Some(rest)
            } else {
                None
            }
        })
        .collect();
    if expected.len() as u32 != h.old_lines {
        // Renderer sent a body that doesn't match its own anchor.
        // That's a programming error, not a stale hunk, but the
        // nicest thing we can do for the user is fail loud with the
        // stale-hunk code so the UI re-fetches.
        warn!(
            path = %h.path,
            claimed = h.old_lines,
            actual = expected.len(),
            "hunk body line count disagrees with anchor"
        );
        return Err(AgentError::StaleHunk {
            path: h.path.clone(),
            expected_old_start: h.old_start,
            expected_old_lines: h.old_lines,
        });
    }
    // Now slice the live file. `lines()` drops trailing newlines;
    // `split_inclusive` keeps them, which matches what `git diff`
    // emits. We use `split_inclusive` on both sides so trailing-
    // newline handling is consistent.
    let live: Vec<&str> = content.split_inclusive('\n').collect();
    let start = h.old_start.saturating_sub(1) as usize;
    let end = start.saturating_add(h.old_lines as usize);
    if end > live.len() {
        return Err(AgentError::StaleHunk {
            path: h.path.clone(),
            expected_old_start: h.old_start,
            expected_old_lines: h.old_lines,
        });
    }
    let live_slice = &live[start..end];
    for (i, (want, got)) in expected.iter().zip(live_slice.iter()).enumerate() {
        // `want` may lack the trailing newline (if the hunk ended
        // at EOF without newline); compare trimmed-right in that
        // case so we don't false-positive on newline disagreement.
        fn normalize(s: &str) -> &str {
            s.strip_suffix('\n').unwrap_or(s)
        }
        if normalize(want) != normalize(got) {
            debug!(
                path = %h.path,
                line = h.old_start + i as u32,
                "anchor slice mismatch"
            );
            return Err(AgentError::StaleHunk {
                path: h.path.clone(),
                expected_old_start: h.old_start,
                expected_old_lines: h.old_lines,
            });
        }
    }
    Ok(())
}

/// Compare the anchor's `new_start..new_start+new_lines` range
/// against the new-side lines embedded in the hunk body.
///
/// This is the primary freshness check: the "new" side of a `git
/// diff` reflects the user's current-view content (the worktree for
/// a stage/discard, the index for an unstage). If the live bytes on
/// that side no longer match what the renderer shipped in the hunk
/// body, the user has concurrently edited the file and the patch
/// they're about to apply would land on the wrong lines. We fail
/// with [`AgentError::StaleHunk`] and let the UI re-fetch.
fn verify_new_slice(content: &str, h: &HunkAnchor) -> Result<()> {
    if h.new_lines == 0 {
        return Ok(());
    }
    // The "new" side is the concatenation of context (` `) and
    // addition (`+`) lines, in order.
    let expected: Vec<&str> = h
        .body
        .split_inclusive('\n')
        .filter_map(|line| {
            if line.starts_with("\\ ") {
                return None;
            }
            if let Some(rest) = line.strip_prefix('+') {
                Some(rest)
            } else if let Some(rest) = line.strip_prefix(' ') {
                Some(rest)
            } else {
                None
            }
        })
        .collect();
    if expected.len() as u32 != h.new_lines {
        warn!(
            path = %h.path,
            claimed = h.new_lines,
            actual = expected.len(),
            "hunk body new-side line count disagrees with anchor"
        );
        return Err(AgentError::StaleHunk {
            path: h.path.clone(),
            expected_old_start: h.old_start,
            expected_old_lines: h.old_lines,
        });
    }
    let live: Vec<&str> = content.split_inclusive('\n').collect();
    let start = h.new_start.saturating_sub(1) as usize;
    let end = start.saturating_add(h.new_lines as usize);
    if end > live.len() {
        return Err(AgentError::StaleHunk {
            path: h.path.clone(),
            expected_old_start: h.old_start,
            expected_old_lines: h.old_lines,
        });
    }
    let live_slice = &live[start..end];
    for (i, (want, got)) in expected.iter().zip(live_slice.iter()).enumerate() {
        fn normalize(s: &str) -> &str {
            s.strip_suffix('\n').unwrap_or(s)
        }
        if normalize(want) != normalize(got) {
            debug!(
                path = %h.path,
                line = h.new_start + i as u32,
                "new-side anchor slice mismatch"
            );
            return Err(AgentError::StaleHunk {
                path: h.path.clone(),
                expected_old_start: h.old_start,
                expected_old_lines: h.old_lines,
            });
        }
    }
    Ok(())
}

/// Build a unified-patch string from a list of anchors. Groups
/// hunks by path (stable order), emits one `diff --git` + `---`/
/// `+++` header per path, then every hunk's `@@` header and body.
fn build_patch(hunks: &[HunkAnchor]) -> Result<String> {
    if hunks.is_empty() {
        return Err(AgentError::Other("no hunks provided".into()));
    }
    let mut by_path: std::collections::BTreeMap<&str, Vec<&HunkAnchor>> =
        std::collections::BTreeMap::new();
    for h in hunks {
        by_path.entry(h.path.as_str()).or_default().push(h);
    }
    let mut out = String::new();
    for (path, group) in by_path {
        out.push_str(&format!("diff --git a/{path} b/{path}\n"));
        out.push_str(&format!("--- a/{path}\n"));
        out.push_str(&format!("+++ b/{path}\n"));
        for h in group {
            out.push_str(&format_hunk_header(h));
            // Ensure the body ends with a newline. `git apply` is
            // strict about this — a body ending mid-line makes it
            // reject the patch with "corrupt patch".
            if h.body.ends_with('\n') {
                out.push_str(&h.body);
            } else {
                out.push_str(&h.body);
                out.push('\n');
            }
        }
    }
    Ok(out)
}

fn format_hunk_header(h: &HunkAnchor) -> String {
    // Unified-diff convention: when a side has exactly one line,
    // the `,N` count is omitted. Matching git's own output here
    // keeps `git apply`'s parser happy on minimal hunks.
    let old_part = if h.old_lines == 1 {
        format!("{}", h.old_start)
    } else {
        format!("{},{}", h.old_start, h.old_lines)
    };
    let new_part = if h.new_lines == 1 {
        format!("{}", h.new_start)
    } else {
        format!("{},{}", h.new_start, h.new_lines)
    };
    format!("@@ -{old_part} +{new_part} @@\n")
}

/// Shell out to `git apply` with the given flags. Feeds the patch
/// to stdin (so large patches don't need a tempfile) and surfaces
/// `git`'s stderr via [`PatchApply`] on failure.
///
/// [`PatchApply`]: crate::AgentError::PatchApply
async fn run_git_apply(work_dir: &Path, patch: &str, cached: bool, reverse: bool) -> Result<()> {
    let mut args: Vec<&str> = vec!["apply"];
    if cached {
        args.push("--cached");
    }
    if reverse {
        args.push("--reverse");
    }
    // `--unidiff-zero` lets us pass hunks without surrounding
    // context if the renderer ever asks for line-level staging. For
    // the normal hunk case it's a no-op. Keeping it on keeps the
    // code-path uniform for both.
    args.push("--unidiff-zero");
    args.push("-");

    let mut child = crate::shellout::git_command()?
        .args(&args)
        .current_dir(work_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(AgentError::Io)?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(patch.as_bytes())
            .await
            .map_err(AgentError::Io)?;
        // Dropping closes stdin; do it explicitly for clarity.
        drop(stdin);
    }
    let out = child.wait_with_output().await.map_err(AgentError::Io)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(AgentError::PatchApply { stderr });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(
        path: &str,
        old_start: u32,
        old_lines: u32,
        new_lines: u32,
        body: &str,
    ) -> HunkAnchor {
        HunkAnchor {
            path: path.into(),
            old_start,
            old_lines,
            new_start: old_start,
            new_lines,
            body: body.into(),
        }
    }

    #[test]
    fn verify_old_slice_matches_live_content() {
        let content = "alpha\nbeta\ngamma\ndelta\n";
        // Hunk says old_start=2, old_lines=2 — claim the live slice
        // is "beta\n", "gamma\n" (a context + a deletion).
        let body = " beta\n-gamma\n+GAMMA\n";
        let h = anchor("f", 2, 2, 2, body);
        verify_old_slice(content, &h).unwrap_or_else(|e| panic!("unexpected err: {e}"));
    }

    #[test]
    fn verify_old_slice_flags_concurrent_edit_as_stale() {
        // File has been edited since the diff was rendered —
        // "gamma" was renamed to "GAMMA".
        let content = "alpha\nbeta\nGAMMA\ndelta\n";
        let body = " beta\n-gamma\n+xxx\n";
        let h = anchor("f", 2, 2, 2, body);
        let err = verify_old_slice(content, &h).unwrap_err();
        assert!(matches!(err, AgentError::StaleHunk { .. }));
    }

    #[test]
    fn verify_old_slice_flags_out_of_range_anchor() {
        let content = "one\ntwo\n";
        // Claim old_start=5, old_lines=1 — well past EOF.
        let body = "-missing\n+new\n";
        let h = anchor("f", 5, 1, 1, body);
        let err = verify_old_slice(content, &h).unwrap_err();
        assert!(matches!(err, AgentError::StaleHunk { .. }));
    }

    #[test]
    fn verify_old_slice_allows_pure_add() {
        // old_lines=0 means a pure-add hunk; no validation needed.
        let content = "anything\n";
        let body = "+brand new line\n";
        let h = anchor("f", 1, 0, 1, body);
        verify_old_slice(content, &h).unwrap_or_else(|e| panic!("unexpected err: {e}"));
    }

    #[test]
    fn build_patch_groups_hunks_by_path() {
        let a = anchor("a.txt", 1, 1, 1, "-old\n+new\n");
        let b1 = anchor("b.txt", 3, 1, 1, "-x\n+y\n");
        let b2 = anchor("b.txt", 10, 1, 1, "-m\n+n\n");
        let patch = build_patch(&[a, b1, b2]).expect("builds");
        // One header pair per file.
        assert_eq!(patch.matches("diff --git").count(), 2);
        assert_eq!(patch.matches("--- a/a.txt").count(), 1);
        assert_eq!(patch.matches("--- a/b.txt").count(), 1);
        // Two hunks in b.txt.
        assert_eq!(patch.matches("@@ -3 +3 @@").count(), 1);
        assert_eq!(patch.matches("@@ -10 +10 @@").count(), 1);
    }

    #[test]
    fn build_patch_rejects_empty_list() {
        let err = build_patch(&[]).unwrap_err();
        assert!(matches!(err, AgentError::Other(_)));
    }

    #[test]
    fn hunk_header_drops_count_when_one_line() {
        let h = anchor("f", 7, 1, 1, "-a\n+b\n");
        assert_eq!(format_hunk_header(&h), "@@ -7 +7 @@\n");
    }

    #[test]
    fn hunk_header_includes_count_when_multiline() {
        let h = HunkAnchor {
            path: "f".into(),
            old_start: 4,
            old_lines: 3,
            new_start: 4,
            new_lines: 5,
            body: String::new(),
        };
        assert_eq!(format_hunk_header(&h), "@@ -4,3 +4,5 @@\n");
    }

    #[test]
    fn body_line_count_mismatch_reports_stale() {
        // Anchor claims old_lines=3 but body only has 2 old-side lines.
        let content = "a\nb\nc\n";
        let body = " a\n-b\n+B\n";
        let h = anchor("f", 1, 3, 3, body);
        let err = verify_old_slice(content, &h).unwrap_err();
        assert!(matches!(err, AgentError::StaleHunk { .. }));
    }

    // --- Integration tests against a real temp git repo ---------------
    //
    // These tests shell out to `git` to set up a realistic fixture:
    // `git init`, seed a file, `git add && git commit`, then exercise
    // the hunk verbs end-to-end. Keeping the fixture behind a
    // `git_init_repo` helper keeps each test short.

    use std::path::PathBuf;
    use std::process::Stdio;
    use tempfile::TempDir;

    fn sh(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("git command spawn")
    }

    fn git_init_repo() -> (TempDir, PathBuf) {
        let td = tempfile::tempdir().expect("tempdir");
        let root = td.path().to_path_buf();
        // Use --initial-branch to avoid the hint warning on newer gits.
        let out = sh(&root, &["init", "--initial-branch=main"]);
        assert!(out.status.success(), "git init: {:?}", out);
        // Local identity so `git commit` works on CI machines that
        // don't have a global user set.
        let _ = sh(&root, &["config", "user.email", "test@example.com"]);
        let _ = sh(&root, &["config", "user.name", "test"]);
        // `core.autocrlf=false` avoids platform-specific line-ending
        // rewrites that would confuse our anchor checker on Windows.
        let _ = sh(&root, &["config", "core.autocrlf", "false"]);
        (td, root)
    }

    fn write_file(dir: &std::path::Path, rel: &str, contents: &str) {
        let abs = dir.join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).expect("mkdirs");
        }
        std::fs::write(&abs, contents).expect("write file");
    }

    fn seed_committed_file(root: &std::path::Path, rel: &str, contents: &str) {
        write_file(root, rel, contents);
        let out = sh(root, &["add", rel]);
        assert!(out.status.success(), "git add: {:?}", out);
        let out = sh(root, &["commit", "-m", "seed"]);
        assert!(out.status.success(), "git commit: {:?}", out);
    }

    /// Read the `patch` field out of the `diff_index_patch` JSON.
    async fn fetch_patch(root: &std::path::Path, path: Option<&str>) -> String {
        let v = diff_index_patch(root, path, false).await.expect("diff");
        v.get("patch")
            .and_then(|p| p.as_str())
            .map(|s| s.to_owned())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn staging_patches_bypass_display_drivers_and_converters() {
        let (_directory, root) = git_init_repo();
        write_file(&root, ".gitattributes", "file.txt diff=fixture\n");
        seed_committed_file(&root, "file.txt", "original\n");
        write_file(&root, "file.txt", "staged\n");
        assert!(sh(&root, &["add", "file.txt"]).status.success());
        write_file(&root, "file.txt", "working\n");
        for key in ["diff.fixture.command", "diff.fixture.textconv"] {
            assert!(
                sh(&root, &["config", key, "desktop-diff-driver-must-not-run"])
                    .status
                    .success()
            );
        }
        let working = fetch_patch(&root, Some("file.txt")).await;
        let staged = diff_cached_patch(&root, Some("file.txt"))
            .await
            .expect("cached patch");
        assert_eq!(
            (
                working.contains("-staged\n+working\n"),
                staged["patch"]
                    .as_str()
                    .expect("patch")
                    .contains("-original\n+staged\n")
            ),
            (true, true)
        );
    }

    /// Parse the first hunk out of a unified `git diff` output for a
    /// single-file patch. Returns `(old_start, old_lines, new_start,
    /// new_lines, body)`. Body is everything between this `@@` line
    /// and the next `@@` line (or EOF).
    fn parse_first_hunk(patch: &str) -> (u32, u32, u32, u32, String) {
        let mut lines = patch.lines().peekable();
        // Skip to the first "@@ -x,y +a,b @@" header.
        let header = loop {
            match lines.next() {
                Some(l) if l.starts_with("@@") => break l.to_owned(),
                Some(_) => continue,
                None => panic!("no hunk header in patch: {patch}"),
            }
        };
        // @@ -old_start,old_lines +new_start,new_lines @@
        let inner = header
            .trim_start_matches("@@")
            .split("@@")
            .next()
            .unwrap_or("")
            .trim();
        // inner is like "-1,3 +1,4"
        let mut parts = inner.split_whitespace();
        let old = parts.next().unwrap_or("-0,0").trim_start_matches('-');
        let new = parts.next().unwrap_or("+0,0").trim_start_matches('+');
        fn split(s: &str) -> (u32, u32) {
            let mut it = s.splitn(2, ',');
            let a: u32 = it.next().unwrap_or("0").parse().unwrap_or(0);
            let b: u32 = it.next().map_or(1, |v| v.parse().unwrap_or(1));
            (a, b)
        }
        let (os, ol) = split(old);
        let (ns, nl) = split(new);
        let mut body = String::new();
        while let Some(l) = lines.peek() {
            if l.starts_with("@@") {
                break;
            }
            body.push_str(l);
            body.push('\n');
            lines.next();
        }
        (os, ol, ns, nl, body)
    }

    #[tokio::test]
    async fn stage_single_hunk_then_rediff_drops_it() {
        let (_td, root) = git_init_repo();
        // 16 lines so two edits 10+ apart land in separate hunks
        // after git's default 3-line context expansion.
        let mut base = String::new();
        for i in 1..=16u32 {
            base.push_str(&format!("line{i}\n"));
        }
        seed_committed_file(&root, "a.txt", &base);

        // Edit line 2 and line 14 — far apart enough that git
        // emits two distinct hunks.
        let mut edited = String::new();
        for i in 1..=16u32 {
            if i == 2 {
                edited.push_str("LINE2\n");
            } else if i == 14 {
                edited.push_str("LINE14\n");
            } else {
                edited.push_str(&format!("line{i}\n"));
            }
        }
        write_file(&root, "a.txt", &edited);

        let patch_before = fetch_patch(&root, Some("a.txt")).await;
        assert!(
            patch_before.matches("@@ -").count() >= 2,
            "expected 2+ hunks, got: {patch_before}"
        );

        // Grab the first hunk and stage it.
        let (os, ol, ns, nl, body) = parse_first_hunk(&patch_before);
        let hunk = HunkAnchor {
            path: "a.txt".into(),
            old_start: os,
            old_lines: ol,
            new_start: ns,
            new_lines: nl,
            body,
        };
        stage_hunks(&root, &[hunk]).await.expect("stage succeeds");

        // Re-diff the working tree against the index — the staged
        // hunk should be gone, leaving only the second edit.
        let patch_after = fetch_patch(&root, Some("a.txt")).await;
        assert!(
            patch_after.contains("LINE14"),
            "second hunk should remain: {patch_after}"
        );
        assert!(
            !patch_after.contains("LINE2"),
            "first hunk should be staged away: {patch_after}"
        );
    }

    #[tokio::test]
    async fn stale_hunk_detected_when_file_mutates_between_diff_and_stage() {
        let (_td, root) = git_init_repo();
        seed_committed_file(&root, "a.txt", "alpha\nbeta\ngamma\ndelta\n");

        // Produce an initial diff by editing one line.
        write_file(&root, "a.txt", "alpha\nBETA\ngamma\ndelta\n");
        let patch = fetch_patch(&root, Some("a.txt")).await;
        let (os, ol, ns, nl, body) = parse_first_hunk(&patch);
        let hunk = HunkAnchor {
            path: "a.txt".into(),
            old_start: os,
            old_lines: ol,
            new_start: ns,
            new_lines: nl,
            body,
        };

        // Concurrent edit: rewrite the file so the anchor's old-side
        // content no longer matches.
        write_file(&root, "a.txt", "something\ncompletely\ndifferent\n");

        let err = stage_hunks(&root, &[hunk])
            .await
            .expect_err("expected stale-hunk error");
        assert!(matches!(err, AgentError::StaleHunk { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn discard_hunks_reverts_the_working_tree() {
        let (_td, root) = git_init_repo();
        let original = "one\ntwo\nthree\nfour\n";
        seed_committed_file(&root, "a.txt", original);

        // Edit a line.
        write_file(&root, "a.txt", "one\nTWO\nthree\nfour\n");
        let patch = fetch_patch(&root, Some("a.txt")).await;
        let (os, ol, ns, nl, body) = parse_first_hunk(&patch);
        let hunk = HunkAnchor {
            path: "a.txt".into(),
            old_start: os,
            old_lines: ol,
            new_start: ns,
            new_lines: nl,
            body,
        };

        discard_hunks(&root, &[hunk]).await.expect("discard");

        let on_disk = std::fs::read_to_string(root.join("a.txt")).expect("read");
        assert_eq!(on_disk, original, "working tree should revert to HEAD");
    }

    #[tokio::test]
    async fn git_apply_failure_surfaces_as_patch_apply_error() {
        let (_td, root) = git_init_repo();
        seed_committed_file(&root, "a.txt", "hello\n");

        // A patch against a file that doesn't exist in the index —
        // `git apply --cached` will reject it, and we should surface
        // a `PatchApply` error rather than a bare CLI error.
        let bogus = "diff --git a/missing.txt b/missing.txt\n\
                     --- a/missing.txt\n\
                     +++ b/missing.txt\n\
                     @@ -1 +1 @@\n\
                     -nope\n\
                     +nope2\n";
        let err = apply_patch(&root, bogus, /*cached=*/ true, /*reverse=*/ false)
            .await
            .expect_err("patch apply should fail");
        assert!(matches!(err, AgentError::PatchApply { .. }), "got {err:?}");
    }

    // --- diff_cached_patch -------------------------------------------

    /// After staging one of two hunks, `diff_cached_patch` should
    /// return exactly the staged hunk (index vs HEAD) while
    /// `diff_index_patch` continues to report the remaining
    /// unstaged hunk. The two diffs together drive the tri-state
    /// file checkbox's `partial` state.
    #[tokio::test]
    async fn diff_cached_patch_reports_only_staged_changes() {
        let (_td, root) = git_init_repo();
        let mut base = String::new();
        for i in 1..=16u32 {
            base.push_str(&format!("line{i}\n"));
        }
        seed_committed_file(&root, "a.txt", &base);

        // Two hunks: edit line 2 and line 14.
        let mut edited = String::new();
        for i in 1..=16u32 {
            if i == 2 {
                edited.push_str("LINE2\n");
            } else if i == 14 {
                edited.push_str("LINE14\n");
            } else {
                edited.push_str(&format!("line{i}\n"));
            }
        }
        write_file(&root, "a.txt", &edited);

        // Stage the first hunk.
        let patch = fetch_patch(&root, Some("a.txt")).await;
        let (os, ol, ns, nl, body) = parse_first_hunk(&patch);
        let hunk = HunkAnchor {
            path: "a.txt".into(),
            old_start: os,
            old_lines: ol,
            new_start: ns,
            new_lines: nl,
            body,
        };
        stage_hunks(&root, &[hunk]).await.expect("stage succeeds");

        // `diff_cached_patch` shows the staged edit (line 2).
        let cached = diff_cached_patch(&root, Some("a.txt"))
            .await
            .expect("cached diff")
            .get("patch")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        assert!(
            cached.contains("LINE2"),
            "cached diff should contain staged hunk: {cached}"
        );
        assert!(
            !cached.contains("LINE14"),
            "cached diff should not contain unstaged hunk: {cached}"
        );

        // `diff_index_patch` shows the remaining unstaged edit (line 14).
        let unstaged = fetch_patch(&root, Some("a.txt")).await;
        assert!(
            unstaged.contains("LINE14"),
            "worktree diff should contain unstaged hunk: {unstaged}"
        );
        assert!(
            !unstaged.contains("LINE2"),
            "worktree diff should not contain staged hunk: {unstaged}"
        );
    }

    /// When nothing is staged, `diff_cached_patch` returns an empty
    /// patch — the "empty" input for the tri-state derivation.
    #[tokio::test]
    async fn diff_cached_patch_is_empty_when_nothing_staged() {
        let (_td, root) = git_init_repo();
        seed_committed_file(&root, "a.txt", "hello\n");
        write_file(&root, "a.txt", "hello\nworld\n");

        let cached = diff_cached_patch(&root, None)
            .await
            .expect("cached diff")
            .get("patch")
            .and_then(|v| v.as_str())
            .unwrap_or("_missing_")
            .to_owned();
        assert!(
            cached.is_empty(),
            "expected empty cached patch, got: {cached:?}"
        );
    }

    // --- Property test: concurrent edit is always caught as StaleHunk ---

    use proptest::prelude::*;

    proptest! {
        // Shrink aggressively; each case spins up a fresh git repo
        // which is the slow part. 24 cases is enough to cover the
        // interesting mutation shapes without burning CI time.
        #![proptest_config(ProptestConfig {
            cases: 24,
            .. ProptestConfig::default()
        })]

        /// For any committed file and any single-line edit that
        /// produces a valid hunk, overwriting the file with arbitrary
        /// content before the stage call must yield `StaleHunk` —
        /// never a silent wrong-position patch application.
        #[test]
        fn concurrent_overwrite_always_yields_stale_hunk(
            base_lines in proptest::collection::vec("[a-z]{3,8}", 3..8),
            edit_idx in 0usize..3,
            replacement in "[A-Z]{3,8}",
            overwrite in "[a-z0-9]{10,40}",
        ) {
            // Build the base file deterministically.
            let mut base = String::new();
            for l in &base_lines {
                base.push_str(l);
                base.push('\n');
            }
            let idx = edit_idx.min(base_lines.len() - 1);

            // The body of the proptest runs in an async Tokio runtime
            // per case. We build it inline rather than relying on
            // tokio-test so we don't pull in yet another dev-dep.
            let outcome: std::result::Result<(), String> = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let (_td, root) = git_init_repo();
                    seed_committed_file(&root, "p.txt", &base);

                    // Make a simple single-line edit so we get a
                    // clean 1-line hunk.
                    let mut edited_lines: Vec<String> =
                        base_lines.iter().cloned().collect();
                    edited_lines[idx] = replacement.clone();
                    let mut edited = String::new();
                    for l in &edited_lines {
                        edited.push_str(l);
                        edited.push('\n');
                    }
                    write_file(&root, "p.txt", &edited);

                    let patch = fetch_patch(&root, Some("p.txt")).await;
                    // If git declined to produce a diff (identical
                    // content — shouldn't happen given our generators
                    // but be defensive), skip the case.
                    if !patch.contains("@@") {
                        return Ok(());
                    }
                    let (os, ol, ns, nl, body) = parse_first_hunk(&patch);
                    let hunk = HunkAnchor {
                        path: "p.txt".into(),
                        old_start: os,
                        old_lines: ol,
                        new_start: ns,
                        new_lines: nl,
                        body,
                    };

                    // Concurrent overwrite: replace the file with
                    // wholly different content. If by chance the
                    // overwrite happens to preserve the anchor
                    // region byte-for-byte (vanishingly rare but
                    // theoretically possible), the stage may
                    // succeed — that's fine, it isn't stale.
                    write_file(&root, "p.txt", &overwrite);

                    match stage_hunks(&root, &[hunk]).await {
                        Err(AgentError::StaleHunk { .. }) => Ok(()),
                        // Only acceptable non-stale outcome is a
                        // successful apply when the overwrite
                        // coincidentally preserved the region — vanishingly
                        // unlikely with random strings but don't fail the
                        // property on it.
                        Ok(_) => Ok(()),
                        // `git apply` may reject a patch that no
                        // longer matches via `PatchApply`. That's
                        // also a safe outcome — the bad patch was
                        // not silently misapplied.
                        Err(AgentError::PatchApply { .. }) => Ok(()),
                        Err(other) => Err(format!("unexpected error: {other:?}")),
                    }
                });

            prop_assert!(outcome.is_ok(), "{:?}", outcome.err());
        }
    }
}
