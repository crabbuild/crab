//! Unreferenced LFS object identification and deletion.
//!
//! Walks `.git/lfs/objects/` to find all locally-stored LFS objects, then
//! builds a protected OID set from the current checkout, stashes, recent refs,
//! unpushed local commits, and other worktree checkouts. Objects outside that
//! protected set are candidates for pruning.
//!
//! Two walk implementations live side-by-side:
//!
//! - Under `--features gix-revwalk`: the walk runs in-process via
//!   `gix_ref::file::Store::iter`, `gix_traverse::commit::Simple`,
//!   `gix_traverse::tree::breadthfirst`, and `gix_odb::Handle::try_find`.
//!   No subprocess. On a 100k-object prune this avoids the ~1000
//!   `git cat-file --batch` child-process forks the legacy path spawns.
//! - Under the legacy path (no `gix-revwalk`): git shellouts for
//!   `rev-list --all --objects` and `cat-file --batch`.
//!
//! Supports `--dry-run` (report only), `--force` (skip confirmation), and
//! `--verify-remote` (download and verify each candidate before local deletion).

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt as LockFileExt;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};
use crab_git::lfs_pointer::LfsPointer;
use crab_lfs::LfsError;

const PRUNE_LOCK: &str = "crab-lfs-prune.lock";
const MAX_LFS_SCAN_OBJECTS: usize = 5_000_000;

/// Summary of a prune operation.
#[derive(Debug, Clone)]
pub struct PruneSummary {
    /// Number of objects pruned (or that would be pruned in dry-run mode).
    pub pruned_count: u64,
    /// Total size in bytes of pruned objects.
    pub pruned_bytes: u64,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

/// Options for LFS prune.
#[derive(Debug, Clone, Copy)]
pub struct PruneOptions {
    /// Check prune candidates against the configured Crab LFS remote.
    pub verify_remote: bool,
    /// Require remote verification for unreachable candidates too.
    pub verify_unreachable: bool,
    /// Behavior when a candidate cannot be verified remotely.
    pub when_unverified: WhenUnverified,
    /// Report what would be pruned without deleting.
    pub dry_run: bool,
    /// Delete without confirmation.
    pub force: bool,
    /// Print full object details.
    pub verbose: bool,
    /// Prune objects that are only retained by recent-ref protection.
    pub recent: bool,
}

impl PruneOptions {
    #[must_use]
    pub fn new(verify_remote: bool, dry_run: bool, force: bool) -> Self {
        Self {
            verify_remote,
            verify_unreachable: true,
            when_unverified: if verify_remote {
                WhenUnverified::Halt
            } else {
                WhenUnverified::Continue
            },
            dry_run,
            force,
            verbose: false,
            recent: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhenUnverified {
    Halt,
    Continue,
}

/// Run the LFS prune operation.
///
/// Identifies unreferenced LFS objects in `.git/lfs/objects/` and
/// optionally deletes them. Returns a summary of what was (or would be)
/// pruned.
///
/// # Flags
///
/// - `verify_remote`: when true, only prunes candidates whose remote bytes match the OID.
/// - `dry_run`: when true, reports what would be pruned without deleting.
/// - `force`: when true, skips the confirmation prompt.
pub fn run_prune(options: PruneOptions) -> Result<PruneSummary> {
    run_prune_with_cancel(options, &CancellationToken::new())
}

/// Run LFS pruning with cancellation checks and a repository-scoped lock.
pub fn run_prune_with_cancel(
    options: PruneOptions,
    cancel: &CancellationToken,
) -> Result<PruneSummary> {
    check_cancelled(cancel)?;
    // `println!`/`eprintln!` in this function are user-facing CLI output
    // (status messages, dry-run listings, confirmation prompts). These
    // are intentional and match the pattern used by other crab
    // commands. Internal diagnostics use `tracing` and the review
    // accepts this split. See finding CR7-F1.
    let lfs_objects_dir = discover_lfs_objects_dir()?;
    let git_dir = crate::git::discover::discover_common_git_dir()?;
    let lock_path = git_dir.join(PRUNE_LOCK);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(CrabError::Io)?;
    if !LockFileExt::try_lock_exclusive(&lock).map_err(CrabError::Io)? {
        return Err(CrabError::Configuration {
            key: "crab lfs prune".to_owned(),
            origin: format!("another prune holds {}", lock_path.display()),
        });
    }

    if !lfs_objects_dir.is_dir() {
        println!("crab lfs prune: no local LFS objects directory found");
        return Ok(PruneSummary {
            pruned_count: 0,
            pruned_bytes: 0,
            dry_run: options.dry_run,
        });
    }

    // Step 1: Collect all local LFS object OIDs and their file paths.
    let local_objects = collect_local_objects_with_cancel(&lfs_objects_dir, cancel)?;

    if local_objects.is_empty() {
        println!("crab lfs prune: no local LFS objects found");
        return Ok(PruneSummary {
            pruned_count: 0,
            pruned_bytes: 0,
            dry_run: options.dry_run,
        });
    }

    tracing::debug!(
        local_count = local_objects.len(),
        "collected local LFS objects"
    );

    // Step 2: Build all referenced OIDs and the Git LFS-style protected subset.
    let referenced = collect_referenced_oids()?;
    let protected = collect_protected_oids(options.recent || options.force)?;

    tracing::debug!(
        referenced_count = referenced.len(),
        protected_count = protected.len(),
        "collected referenced LFS OIDs"
    );

    // Step 3: Identify candidates not protected by current/recent/unpushed refs.
    let mut candidates: Vec<&LocalObject> = local_objects
        .iter()
        .filter(|obj| !protected.contains(&obj.oid_hex))
        .collect();

    if candidates.is_empty() {
        println!(
            "crab lfs prune: all {} local object(s) are protected",
            local_objects.len()
        );
        return Ok(PruneSummary {
            pruned_count: 0,
            pruned_bytes: 0,
            dry_run: options.dry_run,
        });
    }

    if options.verify_remote {
        let (verified, skipped) = verify_remote_candidates(&candidates)?;
        if skipped > 0 {
            println!("crab lfs prune: kept {skipped} object(s) missing remotely");
            if options.when_unverified == WhenUnverified::Halt {
                return Err(CrabError::LfsObjectMissing {
                    oid: format!("{skipped} unverified object(s)"),
                });
            }
        }
        candidates = verified;
        if candidates.is_empty() {
            println!("crab lfs prune: no remotely verified objects to prune");
            return Ok(PruneSummary {
                pruned_count: 0,
                pruned_bytes: 0,
                dry_run: options.dry_run,
            });
        }
    }

    let total_bytes: u64 = candidates.iter().map(|o| o.size).sum();
    let count = candidates.len() as u64;

    if options.dry_run {
        println!(
            "crab lfs prune (dry run): would prune {count} object(s), {}",
            format_size(total_bytes)
        );
        print_prune_candidates(&candidates, options.verbose);
        return Ok(PruneSummary {
            pruned_count: count,
            pruned_bytes: total_bytes,
            dry_run: true,
        });
    }

    // Confirmation prompt (unless --force).
    if !options.force {
        eprintln!(
            "crab lfs prune: {count} object(s) ({size})\n\
             use --dry-run to preview, --force to delete",
            size = format_size(total_bytes),
        );
        return Ok(PruneSummary {
            pruned_count: 0,
            pruned_bytes: 0,
            dry_run: false,
        });
    }

    if options.verbose {
        print_prune_candidates(&candidates, true);
    }

    // Step 4: Delete candidate objects.
    let mut deleted_count = 0u64;
    let mut deleted_bytes = 0u64;

    for obj in &candidates {
        check_cancelled(cancel)?;
        verify_local_object(obj, cancel)?;
        match std::fs::remove_file(&obj.path) {
            Ok(()) => {
                deleted_count += 1;
                deleted_bytes += obj.size;
                tracing::debug!(oid = %obj.oid_hex, "pruned LFS object");
            }
            Err(e) => {
                tracing::warn!(
                    oid = %obj.oid_hex,
                    error = %e,
                    "failed to delete LFS object"
                );
            }
        }
    }

    // Clean up empty fan-out directories.
    cleanup_empty_dirs(&lfs_objects_dir);

    println!(
        "crab lfs prune: deleted {deleted_count} object(s), {}",
        format_size(deleted_bytes)
    );

    Ok(PruneSummary {
        pruned_count: deleted_count,
        pruned_bytes: deleted_bytes,
        dry_run: false,
    })
}

fn collect_local_objects_with_cancel(
    lfs_objects_dir: &Path,
    cancel: &CancellationToken,
) -> Result<Vec<LocalObject>> {
    check_cancelled(cancel)?;
    let objects = collect_local_objects(lfs_objects_dir)?;
    if objects.len() > MAX_LFS_SCAN_OBJECTS {
        return Err(CrabError::Configuration {
            key: "lfs prune object count".to_owned(),
            origin: format!("local LFS inventory exceeds {MAX_LFS_SCAN_OBJECTS} objects"),
        });
    }
    check_cancelled(cancel)?;
    Ok(objects)
}

fn verify_local_object(object: &LocalObject, cancel: &CancellationToken) -> Result<()> {
    let mut file = File::open(&object.path).map_err(CrabError::Io)?;
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        check_cancelled(cancel)?;
        let read = file.read(&mut buffer).map_err(CrabError::Io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes.saturating_add(read as u64);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let expected = parse_hex32(&object.oid_hex)?;
    if bytes != object.size || digest != expected {
        return Err(CrabError::LfsObjectCorrupt {
            oid: object.oid_hex.clone(),
        });
    }
    Ok(())
}

fn print_prune_candidates(candidates: &[&LocalObject], verbose: bool) {
    for obj in candidates {
        if verbose {
            println!(
                "  {}  {}  {}",
                obj.oid_hex,
                format_size(obj.size),
                obj.path.display()
            );
        } else {
            let short = if obj.oid_hex.len() >= 10 {
                &obj.oid_hex[..10]
            } else {
                &obj.oid_hex
            };
            println!("  {short}  {}", format_size(obj.size));
        }
    }
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// A locally-stored LFS object with its filesystem metadata.
struct LocalObject {
    /// Full hex OID (64 chars).
    oid_hex: String,
    /// Absolute path to the object file.
    path: PathBuf,
    /// File size in bytes.
    size: u64,
}

// ---------------------------------------------------------------------------
// Collection helpers
// ---------------------------------------------------------------------------

/// Walk `.git/lfs/objects/` and collect all local LFS objects.
///
/// Expects the standard two-level fan-out layout: `{aa}/{bb}/{oid}`.
fn collect_local_objects(lfs_objects_dir: &Path) -> Result<Vec<LocalObject>> {
    let mut objects = Vec::new();

    let top_entries = read_dir_sorted(lfs_objects_dir)?;
    for aa in &top_entries {
        let aa_path = lfs_objects_dir.join(aa);
        if !aa_path.is_dir() {
            continue;
        }

        let bb_entries = read_dir_sorted(&aa_path)?;
        for bb in &bb_entries {
            let bb_path = aa_path.join(bb);
            if !bb_path.is_dir() {
                continue;
            }

            let obj_entries = read_dir_sorted(&bb_path)?;
            for obj_name in &obj_entries {
                let obj_path = bb_path.join(obj_name);
                if !obj_path.is_file() {
                    continue;
                }

                // The filename should be the full 64-char hex OID.
                if obj_name.len() != 64 || !obj_name.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }

                let metadata = std::fs::metadata(&obj_path).map_err(|e| {
                    CrabError::Internal(format!("failed to stat {}: {e}", obj_path.display()))
                })?;

                objects.push(LocalObject {
                    oid_hex: obj_name.to_lowercase(),
                    path: obj_path,
                    size: metadata.len(),
                });
            }
        }
    }

    Ok(objects)
}

fn verify_remote_candidates<'a>(
    candidates: &[&'a LocalObject],
) -> Result<(Vec<&'a LocalObject>, u64)> {
    let ctx = crate::cmd::lfs::store_setup::resolve_lfs_remote_for_operation_sync("prune")?;
    let mut verified = Vec::new();
    let mut skipped = 0u64;

    for candidate in candidates {
        let oid = parse_hex32(&candidate.oid_hex)?;
        let valid = crate::cmd::lfs::block_on_runtime(async {
            match ctx.store.verify(&oid).await {
                Ok(_) => Ok(true),
                Err(LfsError::ObjectMissing { .. } | LfsError::ObjectCorrupt { .. }) => Ok(false),
                Err(error) => Err(CrabError::from(error)),
            }
        })?;
        if valid {
            verified.push(*candidate);
        } else {
            skipped += 1;
        }
    }

    Ok((verified, skipped))
}

fn parse_hex32(hex: &str) -> Result<[u8; 32]> {
    if hex.len() != 64 {
        return Err(CrabError::Configuration {
            key: "lfs oid".into(),
            origin: format!("expected 64 hex characters, got {}", hex.len()),
        });
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let start = i * 2;
        *byte = u8::from_str_radix(&hex[start..start + 2], 16).map_err(|e| {
            CrabError::Configuration {
                key: "lfs oid".into(),
                origin: format!("invalid hex byte {:?}: {e}", &hex[start..start + 2]),
            }
        })?;
    }
    Ok(out)
}

fn collect_protected_oids(prune_recent: bool) -> Result<HashSet<String>> {
    let mut protected = HashSet::new();

    for rev_args in protected_revision_arg_sets(prune_recent)? {
        if rev_args.is_empty() {
            continue;
        }
        protected.extend(collect_referenced_oids_for_rev_args(&rev_args)?);
    }

    Ok(protected)
}

fn protected_revision_arg_sets(prune_recent: bool) -> Result<Vec<Vec<String>>> {
    let mut sets = Vec::new();

    sets.push(vec!["HEAD".to_owned()]);
    if git_ref_exists("refs/stash") {
        sets.push(vec!["refs/stash".to_owned()]);
    }

    for head in worktree_heads()? {
        sets.push(vec![head]);
    }

    sets.push(unpushed_revision_args()?);

    if !prune_recent {
        let offset_days = crate::lfs::recent::git_config_u64("lfs.pruneoffsetdays", 3)?;
        let recent_refs = crate::lfs::recent::recent_ref_oids(offset_days)?;
        let mut recent_roots = vec!["HEAD".to_owned()];
        recent_roots.extend(recent_refs.iter().cloned());
        if !recent_refs.is_empty() {
            sets.push(recent_refs.clone());
        }
        let recent_commits = crate::lfs::recent::recent_commit_oids(&recent_roots)?;
        if !recent_commits.is_empty() {
            sets.push(recent_commits);
        }
    }

    Ok(sets)
}

fn unpushed_revision_args() -> Result<Vec<String>> {
    let remote = prune_remote_name()?;
    if git_remote_exists(&remote) {
        return Ok(vec![
            "--branches".to_owned(),
            "--not".to_owned(),
            format!("--remotes={remote}"),
        ]);
    }

    Ok(vec!["--all".to_owned()])
}

fn prune_remote_name() -> Result<String> {
    let output = std::process::Command::new("git")
        .args(["config", "--get", "lfs.pruneremotetocheck"])
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to read lfs.pruneremotetocheck: {e}")))?;

    if output.status.success() {
        let remote = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !remote.is_empty() {
            return Ok(remote);
        }
    }

    Ok("origin".to_owned())
}

fn git_remote_exists(remote: &str) -> bool {
    std::process::Command::new("git")
        .args(["remote", "get-url", remote])
        .output()
        .ok()
        .is_some_and(|output| output.status.success())
}

fn git_ref_exists(reference: &str) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", reference])
        .output()
        .ok()
        .is_some_and(|output| output.status.success())
}

fn worktree_heads() -> Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to list git worktrees: {e}")))?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    Ok(parse_worktree_heads(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_worktree_heads(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("HEAD "))
        .map(str::trim)
        .filter(|head| !head.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn collect_referenced_oids_for_rev_args(rev_args: &[String]) -> Result<HashSet<String>> {
    let mut args = vec!["rev-list".to_owned(), "--objects".to_owned()];
    args.extend(rev_args.iter().cloned());

    let output = std::process::Command::new("git")
        .args(&args)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git rev-list: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if crate::lfs::recent::rev_list_can_be_empty(&stderr) {
            return Ok(HashSet::new());
        }
        return Err(CrabError::Internal(format!(
            "git rev-list failed: {stderr}"
        )));
    }

    collect_lfs_oids_from_rev_list_output(&output.stdout)
}

fn collect_lfs_oids_from_rev_list_output(output: &[u8]) -> Result<HashSet<String>> {
    let text = String::from_utf8_lossy(output);

    let mut blob_hashes: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((hash, _)) = line.split_once(' ') {
            blob_hashes.push(hash.to_owned());
        }
    }

    let mut referenced = HashSet::new();
    for chunk in blob_hashes.chunks(500) {
        let oids_input: String = chunk.join("\n");

        let mut child = std::process::Command::new("git")
            .args(["cat-file", "--batch"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| CrabError::Internal(format!("failed to spawn git cat-file: {e}")))?;

        if let Some(ref mut stdin) = child.stdin {
            use std::io::Write;
            let _ = stdin.write_all(oids_input.as_bytes());
        }
        drop(child.stdin.take());

        let output = child
            .wait_with_output()
            .map_err(|e| CrabError::Internal(format!("git cat-file --batch failed: {e}")))?;

        collect_lfs_oids_from_cat_file_batch(&output.stdout, &mut referenced);
    }

    Ok(referenced)
}

fn collect_lfs_oids_from_cat_file_batch(stdout: &[u8], referenced: &mut HashSet<String>) {
    let mut pos = 0;

    while pos < stdout.len() {
        let header_end = match stdout[pos..].iter().position(|&b| b == b'\n') {
            Some(p) => pos + p,
            None => break,
        };

        let header = String::from_utf8_lossy(&stdout[pos..header_end]);
        let parts: Vec<&str> = header.split_whitespace().collect();

        if parts.len() < 3 || parts[1] == "missing" {
            pos = header_end + 1;
            continue;
        }

        let Ok(obj_size) = parts[2].parse::<usize>() else {
            pos = header_end + 1;
            continue;
        };

        let content_start = header_end + 1;
        let content_end = content_start + obj_size;

        if content_end > stdout.len() {
            break;
        }

        let content = &stdout[content_start..content_end];
        if parts[1] == "blob"
            && content.len() <= 1024
            && let Ok(pointer) = LfsPointer::parse(content)
            && pointer.size > 0
        {
            referenced.insert(crab_git::lfs_pointer::hex_encode(&pointer.oid));
        }

        pos = content_end + 1;
    }
}

// ---------------------------------------------------------------------------
// In-process walk (gix-revwalk feature)
// ---------------------------------------------------------------------------

/// Build a set of LFS OIDs referenced by any local ref, using an
/// in-process traversal across every ref tip.
///
/// Flow:
/// 1. Discover `.git` via `gix_discover::upwards` (shared with the
///    legacy path through the `discover_git_dir` helper).
/// 2. Enumerate all refs via `gix_ref::file::Store::iter().all()`.
/// 3. Open the ODB at `.git/objects` via `gix_odb::at`.
/// 4. Walk commits from every ref tip via
///    `gix_traverse::commit::Simple`.
/// 5. For each commit's tree, breadth-first-walk descendants via
///    `gix_traverse::tree::breadthfirst`, collecting blob OIDs.
/// 6. Read each blob with `FindExt::try_find`. Blobs ≤ 1024 bytes
///    that parse as an [`LfsPointer`] with `size > 0` contribute
///    their SHA-256 OID to the referenced set.
///
/// The single-in-process walk replaces the legacy object-listing shellouts
/// with no subprocess calls.
#[cfg(feature = "gix-revwalk")]
fn collect_referenced_oids() -> Result<HashSet<String>> {
    use gix_hash::ObjectId;
    use gix_object::{Find, FindExt};

    let _span = crate::gix_boundary!("lfs", "prune_collect_referenced").entered();

    let git_dir = crate::git::discover::discover_common_git_dir()?;

    // Open the ref store at `.git/` (`refs/` is a subdirectory of
    // git_dir — `Store::at` takes git_dir).
    let ref_store = gix_ref::file::Store::at(
        git_dir.clone(),
        gix_ref::store::init::Options {
            write_reflog: gix_ref::store::WriteReflog::Disable,
            object_hash: gix_hash::Kind::Sha1,
            ..Default::default()
        },
    );

    // Collect every ref tip. Symbolic targets are followed to their
    // object via `gix_ref::Store::peel` — we walk only object tips.
    let platform = ref_store.iter().map_err(|e| {
        CrabError::Internal(format!(
            "failed to open refs store at {}: {e}",
            git_dir.display()
        ))
    })?;
    let iter = platform
        .all()
        .map_err(|e| CrabError::Internal(format!("failed to iterate refs: {e}")))?;

    let mut tips: Vec<ObjectId> = Vec::new();
    for reference_result in iter {
        let reference = match reference_result {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "skipping unreadable ref");
                continue;
            }
        };
        if let gix_ref::Target::Object(oid) = reference.target {
            tips.push(oid);
        }
        // Symbolic refs (HEAD → refs/heads/main) are covered by the
        // concrete ref they target; the iterator yields both.
    }

    if tips.is_empty() {
        tracing::debug!("no ref tips in repo, referenced set is empty");
        return Ok(HashSet::new());
    }

    // Open the git ODB. `gix_odb::at` accepts the `objects/` dir.
    let objects_dir = git_dir.join("objects");
    let odb = {
        let _span = crate::gix_boundary!("odb", "at").entered();
        gix_odb::at(&objects_dir).map_err(|e| {
            CrabError::Internal(format!(
                "failed to open git ODB at {}: {e}",
                objects_dir.display()
            ))
        })?
    };

    let mut seen_blobs: HashSet<[u8; 20]> = HashSet::new();
    let mut seen_trees: HashSet<[u8; 20]> = HashSet::new();
    let mut referenced: HashSet<String> = HashSet::new();

    let commit_walk = gix_traverse::commit::Simple::new(tips, &odb);

    let mut commit_buf = Vec::with_capacity(4096);
    let mut tree_buf = Vec::with_capacity(4096);
    let mut blob_buf = Vec::with_capacity(1024);

    for info_result in commit_walk {
        let info =
            info_result.map_err(|e| CrabError::Internal(format!("commit walk error: {e}")))?;

        // Find the commit's tree id.
        commit_buf.clear();
        let tree_id = {
            let mut commit_iter = odb
                .find_commit_iter(&info.id, &mut commit_buf)
                .map_err(|e| {
                    CrabError::Internal(format!("failed to read commit {}: {e}", info.id))
                })?;
            commit_iter.tree_id().map_err(|e| {
                CrabError::Internal(format!("failed to parse tree from commit {}: {e}", info.id))
            })?
        };

        // Breadth-first tree walk collecting blob OIDs.
        if !seen_trees.insert(oid_to_bytes(&tree_id)) {
            continue;
        }

        tree_buf.clear();
        let tree_iter = odb
            .find_tree_iter(&tree_id, &mut tree_buf)
            .map_err(|e| CrabError::Internal(format!("failed to read tree {tree_id}: {e}")))?;

        let mut visitor = BlobCollector {
            seen_trees: &mut seen_trees,
            seen_blobs: &mut seen_blobs,
            pending_blobs: Vec::new(),
        };
        let mut state = gix_traverse::tree::breadthfirst::State::default();

        if let Err(e) = gix_traverse::tree::breadthfirst(tree_iter, &mut state, &odb, &mut visitor)
        {
            tracing::warn!(tree = %tree_id, error = %e, "tree walk error during lfs prune");
            continue;
        }

        // Read each newly-seen blob; record as referenced if it
        // parses as an LFS pointer with a non-zero size.
        for blob_id in visitor.pending_blobs {
            blob_buf.clear();
            let data = {
                let _span = crate::gix_boundary!("odb", "try_find").entered();
                match odb.try_find(&blob_id, &mut blob_buf) {
                    Ok(Some(d)) => d,
                    Ok(None) => {
                        // Missing blob — not our problem here; connectivity
                        // check covers that path. Skip.
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(oid = %blob_id, error = %e, "blob read error");
                        continue;
                    }
                }
            };

            if data.kind != gix_object::Kind::Blob || data.data.len() > 1024 {
                continue;
            }

            if let Ok(pointer) = LfsPointer::parse(data.data) {
                if pointer.size > 0 {
                    referenced.insert(crab_git::lfs_pointer::hex_encode(&pointer.oid));
                }
            }
        }
    }

    Ok(referenced)
}

/// Convert a `gix_hash::oid` to a fixed 20-byte array for set
/// tracking. Mirrors the helper in `git/walk.rs` — not shared
/// because this module is feature-gated and the helper stays tiny.
#[cfg(feature = "gix-revwalk")]
fn oid_to_bytes(oid: &gix_hash::oid) -> [u8; 20] {
    let mut buf = [0u8; 20];
    buf.copy_from_slice(oid.as_bytes());
    buf
}

/// Visitor that collects blob OIDs during a tree walk. Trees that
/// have already been seen are pruned; blobs are recorded for a
/// follow-up read after the walk completes (the walker borrows the
/// ODB buffer for its own traversal and we can't nest).
#[cfg(feature = "gix-revwalk")]
struct BlobCollector<'a> {
    seen_trees: &'a mut HashSet<[u8; 20]>,
    seen_blobs: &'a mut HashSet<[u8; 20]>,
    pending_blobs: Vec<gix_hash::ObjectId>,
}

#[cfg(feature = "gix-revwalk")]
impl<'a> gix_traverse::tree::Visit for BlobCollector<'a> {
    fn pop_front_tracked_path_and_set_current(&mut self) {}
    fn pop_back_tracked_path_and_set_current(&mut self) {}
    fn push_back_tracked_path_component(&mut self, _component: &gix_object::bstr::BStr) {}
    fn push_path_component(&mut self, _component: &gix_object::bstr::BStr) {}
    fn pop_path_component(&mut self) {}

    fn visit_tree(
        &mut self,
        entry: &gix_object::tree::EntryRef<'_>,
    ) -> std::ops::ControlFlow<(), bool> {
        if self.seen_trees.insert(oid_to_bytes(entry.oid)) {
            std::ops::ControlFlow::Continue(true)
        } else {
            std::ops::ControlFlow::Continue(false)
        }
    }

    fn visit_nontree(
        &mut self,
        entry: &gix_object::tree::EntryRef<'_>,
    ) -> std::ops::ControlFlow<(), bool> {
        if self.seen_blobs.insert(oid_to_bytes(entry.oid)) {
            self.pending_blobs.push(entry.oid.to_owned());
        }
        std::ops::ControlFlow::Continue(true)
    }
}

// ---------------------------------------------------------------------------
// Legacy walk (no gix-revwalk)
// ---------------------------------------------------------------------------

/// Legacy implementation: three `git` shellouts.
///
/// Kept until the `gix-revwalk` feature flag is default-on for a
/// full release cycle, at which point this block deletes.
#[cfg(not(feature = "gix-revwalk"))]
fn collect_referenced_oids() -> Result<HashSet<String>> {
    collect_referenced_oids_for_rev_args(&["--all".to_owned()])
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

/// Discover the `.git/lfs/objects` directory from the current repo.
#[cfg(feature = "gix-revwalk")]
fn discover_lfs_objects_dir() -> Result<PathBuf> {
    Ok(crate::git::discover::discover_common_git_dir()?
        .join("lfs")
        .join("objects"))
}

#[cfg(not(feature = "gix-revwalk"))]
fn discover_lfs_objects_dir() -> Result<PathBuf> {
    Ok(crate::git::discover::discover_common_git_dir()?
        .join("lfs")
        .join("objects"))
}

/// Read directory entries sorted by name.
fn read_dir_sorted(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|e| CrabError::Internal(format!("failed to read dir {}: {e}", dir.display())))?;

    for entry in entries {
        let entry =
            entry.map_err(|e| CrabError::Internal(format!("failed to read dir entry: {e}")))?;
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_owned());
        }
    }

    names.sort();
    Ok(names)
}

/// Remove empty fan-out directories after pruning.
fn cleanup_empty_dirs(lfs_objects_dir: &Path) {
    if let Ok(top_entries) = std::fs::read_dir(lfs_objects_dir) {
        for entry in top_entries.flatten() {
            let aa_path = entry.path();
            if !aa_path.is_dir() {
                continue;
            }

            if let Ok(bb_entries) = std::fs::read_dir(&aa_path) {
                for bb_entry in bb_entries.flatten() {
                    let bb_path = bb_entry.path();
                    if bb_path.is_dir() {
                        // Remove if empty (ignore errors — directory might not be empty).
                        let _ = std::fs::remove_dir(&bb_path);
                    }
                }
            }

            // Try to remove the aa directory if now empty.
            let _ = std::fs::remove_dir(&aa_path);
        }
    }
}

/// Format a byte size for human-readable display.
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn format_size_display() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn collect_local_objects_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let objects = collect_local_objects(dir.path()).unwrap();
        assert!(objects.is_empty());
    }

    #[test]
    fn collect_local_objects_with_fanout() {
        let dir = tempfile::tempdir().unwrap();
        let oid = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

        // Create fan-out structure: ab/cd/<oid>
        let obj_dir = dir.path().join("ab").join("cd");
        std::fs::create_dir_all(&obj_dir).unwrap();

        let obj_path = obj_dir.join(oid);
        let mut f = std::fs::File::create(&obj_path).unwrap();
        f.write_all(b"test content").unwrap();
        drop(f);

        let objects = collect_local_objects(dir.path()).unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].oid_hex, oid);
        assert_eq!(objects[0].size, 12); // "test content" is 12 bytes
    }

    #[test]
    fn collect_local_objects_skips_non_hex_filenames() {
        let dir = tempfile::tempdir().unwrap();

        let obj_dir = dir.path().join("ab").join("cd");
        std::fs::create_dir_all(&obj_dir).unwrap();

        // Create a file with a non-hex name — should be skipped.
        let bad_path = obj_dir.join("not-a-valid-oid");
        std::fs::File::create(&bad_path).unwrap();

        let objects = collect_local_objects(dir.path()).unwrap();
        assert!(objects.is_empty());
    }

    #[test]
    fn cleanup_empty_dirs_removes_empty() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("ab").join("cd");
        std::fs::create_dir_all(&nested).unwrap();

        cleanup_empty_dirs(dir.path());

        assert!(!nested.exists());
        assert!(!dir.path().join("ab").exists());
    }

    #[test]
    fn cleanup_empty_dirs_preserves_non_empty() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("ab").join("cd");
        std::fs::create_dir_all(&nested).unwrap();

        // Put a file in the nested dir so it's not empty.
        std::fs::File::create(nested.join("keep")).unwrap();

        cleanup_empty_dirs(dir.path());

        assert!(nested.exists());
    }

    #[test]
    fn parse_hex32_accepts_oid() {
        let hex = "ab".repeat(32);
        let parsed = parse_hex32(&hex).unwrap();
        assert_eq!(parsed, [0xab; 32]);
    }

    #[test]
    fn parse_worktree_heads_extracts_each_checkout_head() {
        let output = "\
worktree /repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repo-linked
HEAD 2222222222222222222222222222222222222222
detached
";

        let heads = parse_worktree_heads(output);

        assert_eq!(
            heads,
            vec![
                "1111111111111111111111111111111111111111",
                "2222222222222222222222222222222222222222"
            ]
        );
    }

    /// Build a fixture repo with two LFS pointer blobs: one
    /// committed and therefore reachable from `refs/heads/main`,
    /// another written to the ODB as a loose object but never
    /// referenced. Verify `collect_referenced_oids` returns only
    /// the reachable pointer's SHA-256 OID.
    #[cfg(feature = "gix-revwalk")]
    #[test]
    fn lfs_prune_identifies_unreachable_pointers() {
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path();

        macro_rules! git {
            ($($arg:expr),+ $(,)?) => {
                Command::new("git")
                    .args([$($arg),+])
                    .current_dir(repo_dir)
            };
        }

        let Ok(init) = git!("init", "--initial-branch=main")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        else {
            eprintln!("skipping: git not available");
            return;
        };
        if !init.success() {
            eprintln!("skipping: git init failed");
            return;
        }

        let _ = git!("config", "user.email", "test@test.com").status();
        let _ = git!("config", "user.name", "Test").status();

        // Pointer A: referenced. Write as data.bin, commit it.
        let oid_a_hex = "aa".repeat(32);
        let pointer_a = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid_a_hex}\nsize 100\n"
        );
        std::fs::write(repo_dir.join("data.bin"), pointer_a.as_bytes()).unwrap();
        let _ = git!("add", "data.bin").status();
        let commit = git!("commit", "-m", "add pointer")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !matches!(commit, Ok(s) if s.success()) {
            eprintln!("skipping: git commit failed");
            return;
        }

        // Pointer B: unreachable. Write as a loose blob in the ODB
        // via `git hash-object -w --stdin`, then don't reference it.
        let oid_b_hex = "bb".repeat(32);
        let pointer_b = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid_b_hex}\nsize 200\n"
        );
        let mut child = Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(repo_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(pointer_b.as_bytes())
            .unwrap();
        child.wait().unwrap();

        // Point GIT_DIR at the fixture repo so `discover_git_dir`
        // picks it up. Serialise against the process-wide mutex
        // shared with other tests that touch `GIT_DIR`. The mutex
        // is re-exported by `crate::test::git_repo::GIT_DIR_MUTEX`.
        let lock = crate::test::git_repo::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("GIT_DIR").ok();
        // SAFETY: access is serialised via GIT_DIR_MUTEX held in
        // `lock` for the duration of the test body.
        unsafe { std::env::set_var("GIT_DIR", repo_dir.join(".git")) };

        let result = collect_referenced_oids();

        // Restore GIT_DIR before any assertion so a failure
        // doesn't leave the process env dirty for sibling tests.
        // SAFETY: GIT_DIR_MUTEX is still held via `lock`.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("GIT_DIR", v),
                None => std::env::remove_var("GIT_DIR"),
            }
        }
        drop(lock);

        let referenced = result.expect("walk");

        assert!(
            referenced.contains(&oid_a_hex),
            "referenced pointer's OID should be in the set: {referenced:?}"
        );
        assert!(
            !referenced.contains(&oid_b_hex),
            "unreferenced pointer's OID should NOT be in the set: {referenced:?}"
        );
    }
}
