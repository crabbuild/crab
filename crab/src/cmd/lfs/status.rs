//! `crab lfs status` — show staged and modified LFS-tracked files.
//!
//! Displays LFS-tracked files that are staged for commit or modified in
//! the working tree, with OID changes shown for each file.

use crate::core::error::Result;
use crate::core::output::{OutputMode, emit_json};
use crate::lfs::status::{LfsFileStatus, lfs_status};

/// Run `crab lfs status` with the given output format flags.
pub fn run_lfs_status(mode: OutputMode, porcelain: bool) -> Result<()> {
    let repo_root = std::env::current_dir()?;
    let statuses = lfs_status(&repo_root)?;

    if mode == OutputMode::Json {
        emit_json("lfs.status", "1.1", &statuses);
        return Ok(());
    } else if porcelain {
        print_porcelain(&statuses);
    } else {
        print_human(&statuses);
    }

    Ok(())
}

/// Human-readable output grouped by staged/unstaged.
fn print_human(statuses: &[LfsFileStatus]) {
    let staged: Vec<_> = statuses.iter().filter(|s| s.staged).collect();
    let unstaged: Vec<_> = statuses.iter().filter(|s| !s.staged).collect();

    if staged.is_empty() && unstaged.is_empty() {
        println!("On branch (LFS objects)");
        println!("nothing to commit");
        return;
    }

    if !staged.is_empty() {
        println!("Objects to be committed:");
        println!();
        for s in &staged {
            print_entry_human(s);
        }
        println!();
    }

    if !unstaged.is_empty() {
        println!("Objects not staged for commit:");
        println!();
        for s in &unstaged {
            print_entry_human(s);
        }
        println!();
    }
}

/// Render one entry in human-readable form.
///
/// Renames are displayed with a leading `renamed:` marker plus the
/// source path, mirroring `git lfs status`'s output for `R old -> new`
/// entries.
fn print_entry_human(s: &LfsFileStatus) {
    let new = s
        .new_oid
        .as_deref()
        .map_or_else(|| "(deleted)".to_owned(), abbreviate);
    if let Some(src) = s.renamed_from.as_deref() {
        // For renames the old_oid equals the new_oid (exact-OID pairing),
        // so show just one abbreviated hash rather than `a -> a`.
        println!("\trenamed:    {src} -> {} ({new})", s.path);
    } else {
        let old = s
            .old_oid
            .as_deref()
            .map_or_else(|| "(new)".to_owned(), abbreviate);
        println!("\t{} ({old} -> {new})", s.path);
    }
}

/// Machine-parseable porcelain output.
///
/// Format for non-rename entries: `<status> <path> <old_oid> <new_oid>`
/// where status is `S` for staged or `M` for modified (unstaged).
///
/// Format for renames: `R<staged> <old_path> <new_path> <oid>`
/// where `<staged>` is `S` or `M` as above. The single `<oid>` reflects
/// the content identity shared by both paths — LFS pointer renames are
/// exact-OID by construction.
fn print_porcelain(statuses: &[LfsFileStatus]) {
    for s in statuses {
        let status_char = if s.staged { 'S' } else { 'M' };
        if let Some(src) = s.renamed_from.as_deref() {
            // Old and new OIDs are equal for exact-OID renames; emit
            // just one. Fallback to `-` for the degenerate case where
            // the OID isn't known (shouldn't happen for pair_renames
            // output, but keeps the porcelain format total).
            let oid = s.new_oid.as_deref().or(s.old_oid.as_deref()).unwrap_or("-");
            println!("R{status_char} {src} {} {oid}", s.path);
        } else {
            let old = s.old_oid.as_deref().unwrap_or("-");
            let new = s.new_oid.as_deref().unwrap_or("-");
            println!("{status_char} {} {old} {new}", s.path);
        }
    }
}

/// Abbreviate a hex OID to its first 10 characters.
fn abbreviate(oid: &str) -> String {
    if oid.len() > 10 {
        format!("{}...", &oid[..10])
    } else {
        oid.to_owned()
    }
}
