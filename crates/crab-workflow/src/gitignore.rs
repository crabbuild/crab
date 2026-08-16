//! `.gitignore` management for the workflow layer.
//!
//! The workflow layer stores scratch journals, run logs, and
//! materialization sidecars under `.crab/workflow/` which must
//! stay out of git. We append a single line to the repo's
//! `.gitignore` on first invocation, emit `info!`, and no-op every
//! subsequent call.

use std::fs;
use std::io::Write;
use std::path::Path;

use tracing::info;

use crate::{Result, WorkflowError as CrabError};

/// Entry we append when the repo's `.gitignore` does not already
/// cover the workflow scratch tree.
const WORKFLOW_IGNORE_ENTRY: &str = ".crab/workflow/";

/// Ensure the repo's `.gitignore` excludes workflow working
/// directories. Returns `true` when this call appended an entry,
/// `false` when the path was already covered (or `.gitignore`
/// already listed it verbatim).
pub fn ensure_workflow_ignored(repo: &Path) -> Result<bool> {
    let path = repo.join(".gitignore");

    let contents = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(CrabError::Io(e)),
    };

    if is_covered(&contents) {
        return Ok(false);
    }

    append_entry(&path, &contents)?;
    info!(repo = %repo.display(), "appended workflow entry to .gitignore");
    Ok(true)
}

/// Check whether `.gitignore` text already matches our entry.
///
/// Accepts several equivalent forms a user may have written by hand:
/// `.crab/workflow/`, `.crab/workflow`, `.crab/workflow/*`,
/// `.crab/` (covers everything under it), and `.crab/**`.
fn is_covered(contents: &str) -> bool {
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Strip an optional leading `/` (gitignore allows either).
        let cleaned = trimmed.strip_prefix('/').unwrap_or(trimmed);
        if matches!(
            cleaned,
            ".crab/workflow/"
                | ".crab/workflow"
                | ".crab/workflow/*"
                | ".crab/workflow/**"
                | ".crab/"
                | ".crab"
                | ".crab/*"
                | ".crab/**"
        ) {
            return true;
        }
    }
    false
}

fn append_entry(path: &Path, existing: &str) -> Result<()> {
    // Start every write with a trailing newline so the new entry
    // lives on its own line, regardless of what the existing file
    // ended with.
    let needs_leading_newline = !existing.is_empty() && !existing.ends_with('\n');

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(CrabError::Io)?;

    if needs_leading_newline {
        file.write_all(b"\n").map_err(CrabError::Io)?;
    }
    file.write_all(WORKFLOW_IGNORE_ENTRY.as_bytes())
        .map_err(CrabError::Io)?;
    file.write_all(b"\n").map_err(CrabError::Io)?;
    file.sync_all().map_err(CrabError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn appends_when_gitignore_absent() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();

        let appended = ensure_workflow_ignored(repo).unwrap();
        assert!(appended);

        let contents = fs::read_to_string(repo.join(".gitignore")).unwrap();
        assert!(contents.contains(".crab/workflow/"));
    }

    #[test]
    fn appends_when_gitignore_exists_without_entry() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        fs::write(repo.join(".gitignore"), "target/\n*.log\n").unwrap();

        let appended = ensure_workflow_ignored(repo).unwrap();
        assert!(appended);

        let contents = fs::read_to_string(repo.join(".gitignore")).unwrap();
        assert!(contents.contains("target/"));
        assert!(contents.contains(".crab/workflow/"));
    }

    #[test]
    fn no_op_when_entry_already_present() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        fs::write(repo.join(".gitignore"), ".crab/workflow/\n").unwrap();

        let appended = ensure_workflow_ignored(repo).unwrap();
        assert!(!appended);

        // File should not grow on no-op.
        let contents = fs::read_to_string(repo.join(".gitignore")).unwrap();
        assert_eq!(contents, ".crab/workflow/\n");
    }

    #[test]
    fn no_op_when_entry_covered_by_parent_glob() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        fs::write(repo.join(".gitignore"), ".crab/\n").unwrap();

        let appended = ensure_workflow_ignored(repo).unwrap();
        assert!(!appended);
    }

    #[test]
    fn handles_file_without_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        fs::write(repo.join(".gitignore"), "target/").unwrap();

        let appended = ensure_workflow_ignored(repo).unwrap();
        assert!(appended);

        let contents = fs::read_to_string(repo.join(".gitignore")).unwrap();
        // Existing line must be preserved; new entry must be on its
        // own line.
        assert!(contents.starts_with("target/"));
        assert!(contents.contains("\n.crab/workflow/\n"));
    }

    #[test]
    fn ignores_comments_and_blank_lines_when_scanning() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        fs::write(
            repo.join(".gitignore"),
            "# pre-existing .crab/workflow/ in a comment\n\n\ntarget/\n",
        )
        .unwrap();

        let appended = ensure_workflow_ignored(repo).unwrap();
        assert!(appended, "commented-out entry should not count as coverage");
    }
}
