//! `crab lfs fetch` and `crab lfs pull` — download LFS objects.
//!
//! Wires the CLI fetch/pull subcommands to [`crate::lfs::batch::BatchResolver`]
//! via the shared [`super::store_setup`] module.

use std::collections::HashSet;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::error::{CrabError, Result};
use crate::core::output::emit_json;
use crate::lfs::batch::{BatchResolver, PatternFilter};
use crab_git::lfs_pointer::{LfsPointer, MAX_LFS_POINTER_SIZE, hex_encode};
use crab_git::pointer_detect::{PointerKind, classify};
use serde::Serialize;

use super::store_setup::resolve_lfs_remote_for_operation_with_remote_sync;

#[derive(Debug, Clone, Default)]
pub struct LfsFetchOptions {
    pub remote: Option<String>,
    pub refs: Vec<String>,
    pub include: Option<String>,
    pub exclude: Option<String>,
    pub recent: bool,
    pub all: bool,
    pub stdin: bool,
    pub prune: bool,
    pub refetch: bool,
    pub dry_run: bool,
    pub json: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LfsPullOptions {
    pub remote: Option<String>,
    pub include: Option<String>,
    pub exclude: Option<String>,
}

/// Run `crab lfs fetch`.
///
/// Downloads missing LFS objects from the remote store into the local
/// `.git/lfs/objects` cache. Supports include/exclude pattern filtering,
/// `--recent`, `--all`, and `--dry-run`.
pub fn run_lfs_fetch(options: LfsFetchOptions) -> Result<()> {
    validate_fetch_options(&options)?;
    let (remote, refs) = remote_and_refs_from_options(&options)?;
    let ctx = resolve_lfs_remote_for_operation_with_remote_sync("pull", remote.as_deref())?;
    let entries = collect_lfs_pointers(options.all, options.recent, &refs)?;

    if entries.is_empty() {
        if options.json {
            write_fetch_json(&[], &ctx.prefix, &ctx.local_lfs_dir)?;
        } else {
            eprintln!("fetch: no LFS objects to fetch");
        }
        if options.prune {
            run_fetch_prune(options.dry_run)?;
        }
        return Ok(());
    }

    let inc_filter = options
        .include
        .as_deref()
        .map(PatternFilter::new)
        .transpose()?;
    let exc_filter = options
        .exclude
        .as_deref()
        .map(PatternFilter::new)
        .transpose()?;

    let transfers = plan_fetch_transfers(
        &entries,
        inc_filter.as_ref(),
        exc_filter.as_ref(),
        &ctx.local_lfs_dir,
        options.refetch,
    )?;

    if transfers.is_empty() {
        if options.json {
            write_fetch_json(&[], &ctx.prefix, &ctx.local_lfs_dir)?;
        } else {
            eprintln!("fetch: all objects up to date");
        }
        if options.prune {
            run_fetch_prune(options.dry_run)?;
        }
        return Ok(());
    }

    if options.dry_run {
        if options.json {
            write_fetch_json(&transfers, &ctx.prefix, &ctx.local_lfs_dir)?;
        }
        if !options.json {
            eprintln!("fetch: would download {} object(s)", transfers.len());
            for transfer in &transfers {
                eprintln!("  {}", &hex_encode(&transfer.pointer.oid)[..10]);
            }
        }
        if options.prune {
            run_fetch_prune(true)?;
        }
        return Ok(());
    }

    let pointers: Vec<LfsPointer> = transfers
        .iter()
        .map(|transfer| transfer.pointer.clone())
        .collect();
    let downloaded_count = pointers.len();
    let remote_prefix_for_json = ctx.prefix.clone();
    let local_lfs_dir_for_json = ctx.local_lfs_dir.clone();

    super::block_on_runtime(async {
        let resolver = BatchResolver::new(ctx.store, ctx.local_lfs_dir, ctx.config);

        if !options.json {
            eprintln!("fetch: downloading {downloaded_count} object(s)");
        }
        let progress =
            super::progress::TransferProgress::new("Downloading", downloaded_count as u64);
        resolver
            .download_objects(&pointers, options.refetch)
            .await?;
        progress.finish();
        super::logs::log_transfer_event("fetch", downloaded_count as u64, progress.elapsed_secs());
        if !options.json {
            eprintln!("fetch: done");
        }
        Ok(())
    })?;

    if options.json {
        write_fetch_json(&transfers, &remote_prefix_for_json, &local_lfs_dir_for_json)?;
    }

    if options.prune {
        run_fetch_prune(false)?;
    }

    Ok(())
}

fn validate_fetch_options(options: &LfsFetchOptions) -> Result<()> {
    if options.stdin && !options.refs.is_empty() {
        return Err(CrabError::Configuration {
            key: "lfs fetch".to_owned(),
            origin: "--stdin reads refs instead of command-line refs".to_owned(),
        });
    }
    if options.all && options.recent {
        return Err(CrabError::Configuration {
            key: "lfs fetch".to_owned(),
            origin: "--all cannot be combined with --recent".to_owned(),
        });
    }
    if options.all && (options.include.is_some() || options.exclude.is_some()) {
        return Err(CrabError::Configuration {
            key: "lfs fetch".to_owned(),
            origin: "--all cannot be combined with --include or --exclude".to_owned(),
        });
    }
    if options.json && options.prune {
        return Err(CrabError::Configuration {
            key: "lfs fetch".to_owned(),
            origin: "Cannot combine --json with --prune".to_owned(),
        });
    }
    Ok(())
}

fn remote_and_refs_from_options(
    options: &LfsFetchOptions,
) -> Result<(Option<String>, Vec<String>)> {
    let refs = refs_from_options(options)?;
    if options.stdin {
        return Ok((options.remote.clone(), refs));
    }

    let Some(candidate) = options.remote.as_ref() else {
        return Ok((None, refs));
    };

    if git_remote_exists(candidate) || !refs.is_empty() {
        return Ok((Some(candidate.clone()), refs));
    }

    Ok((None, vec![candidate.clone()]))
}

fn git_remote_exists(name: &str) -> bool {
    Command::new("git")
        .args(["remote", "get-url", name])
        .output()
        .ok()
        .is_some_and(|output| output.status.success())
}

fn refs_from_options(options: &LfsFetchOptions) -> Result<Vec<String>> {
    if !options.stdin {
        return Ok(options.refs.clone());
    }

    let stdin = std::io::stdin();
    stdin
        .lock()
        .lines()
        .map(|line| {
            line.map(|line| line.trim().to_owned())
                .map_err(CrabError::Io)
        })
        .filter(|line| !matches!(line, Ok(value) if value.is_empty()))
        .collect()
}

#[derive(Debug, Clone)]
struct FetchTransfer {
    path: String,
    pointer: LfsPointer,
}

fn plan_fetch_transfers(
    entries: &[(String, LfsPointer)],
    include: Option<&PatternFilter>,
    exclude: Option<&PatternFilter>,
    local_lfs_dir: &Path,
    refetch: bool,
) -> Result<Vec<FetchTransfer>> {
    let mut transfers = Vec::new();
    let mut seen = HashSet::new();

    for (path, pointer) in entries {
        if let Some(inc) = include
            && !inc.matches(path)
        {
            continue;
        }
        if let Some(exc) = exclude
            && exc.matches(path)
        {
            continue;
        }
        let local_valid = match crate::lfs::cache::read_pointer(local_lfs_dir, pointer) {
            Ok(Some(_)) => true,
            Ok(None) | Err(CrabError::LfsObjectCorrupt { .. }) => false,
            Err(error) => return Err(error),
        };
        if !refetch && local_valid {
            continue;
        }
        if seen.insert(pointer.oid) {
            transfers.push(FetchTransfer {
                path: path.clone(),
                pointer: pointer.clone(),
            });
        }
    }

    Ok(transfers)
}

#[derive(Serialize)]
struct FetchJsonPayload {
    transfers: Vec<FetchJsonTransfer>,
}

#[derive(Serialize)]
struct FetchJsonTransfer {
    name: String,
    oid: String,
    size: u64,
    actions: FetchJsonActions,
    path: String,
}

#[derive(Serialize)]
struct FetchJsonActions {
    download: FetchJsonDownload,
}

#[derive(Serialize)]
struct FetchJsonDownload {
    href: String,
    expires_at: &'static str,
}

fn write_fetch_json(
    transfers: &[FetchTransfer],
    remote_prefix: &str,
    local_lfs_dir: &Path,
) -> Result<()> {
    let transfers = transfers
        .iter()
        .map(|transfer| {
            let oid = hex_encode(&transfer.pointer.oid);
            FetchJsonTransfer {
                name: transfer.path.clone(),
                oid: oid.clone(),
                size: transfer.pointer.size,
                actions: FetchJsonActions {
                    download: FetchJsonDownload {
                        href: fetch_json_href(remote_prefix, &oid),
                        expires_at: "0001-01-01T00:00:00Z",
                    },
                },
                path: local_object_path(local_lfs_dir, &transfer.pointer.oid)
                    .display()
                    .to_string(),
            }
        })
        .collect();
    let payload = FetchJsonPayload { transfers };
    emit_json("lfs.fetch", "1.1", &payload);
    Ok(())
}

fn fetch_json_href(remote_prefix: &str, oid_hex: &str) -> String {
    let object_path = format!(
        "lfs/objects/{}/{}/{}",
        &oid_hex[..2],
        &oid_hex[2..4],
        oid_hex
    );
    let prefix = remote_prefix.trim_matches('/');
    if prefix.is_empty() {
        return format!("crab-lfs://{object_path}");
    }
    format!("crab-lfs://{prefix}/{object_path}")
}

fn local_object_path(local_lfs_dir: &Path, oid: &[u8; 32]) -> PathBuf {
    let oid_hex = hex_encode(oid);
    local_lfs_dir
        .join("objects")
        .join(&oid_hex[..2])
        .join(&oid_hex[2..4])
        .join(oid_hex)
}

fn run_fetch_prune(dry_run: bool) -> Result<()> {
    super::prune::run_lfs_prune(super::prune::LfsPruneOptions {
        verify_remote: false,
        no_verify_remote: false,
        verify_unreachable: false,
        no_verify_unreachable: false,
        when_unverified: Some("continue".to_owned()),
        recent: false,
        dry_run,
        force: true,
        verbose: dry_run,
    })
}

/// Run `crab lfs pull`.
///
/// Fetches missing LFS objects then replaces LFS pointers in the working
/// tree with the actual file content.
pub fn run_lfs_pull(options: LfsPullOptions) -> Result<()> {
    let ctx = resolve_lfs_remote_for_operation_with_remote_sync("pull", options.remote.as_deref())?;
    let entries = collect_lfs_pointers(false, false, &[])?;

    if entries.is_empty() {
        eprintln!("pull: no LFS objects to pull");
        return Ok(());
    }

    let inc_filter = options
        .include
        .as_deref()
        .map(PatternFilter::new)
        .transpose()?;
    let exc_filter = options
        .exclude
        .as_deref()
        .map(PatternFilter::new)
        .transpose()?;

    super::block_on_runtime(async {
        let resolver = BatchResolver::new(ctx.store, ctx.local_lfs_dir, ctx.config);

        let missing =
            resolver.find_missing_for_fetch(&entries, inc_filter.as_ref(), exc_filter.as_ref())?;

        if !missing.is_empty() {
            eprintln!("pull: downloading {} object(s)", missing.len());
            let progress =
                super::progress::TransferProgress::new("Downloading", missing.len() as u64);
            resolver.download_missing(&missing).await?;
            progress.finish();
            super::logs::log_transfer_event("pull", missing.len() as u64, progress.elapsed_secs());
        }

        Ok::<(), CrabError>(())
    })?;

    let checkout_paths =
        checkout_paths_for_pull(&entries, inc_filter.as_ref(), exc_filter.as_ref());

    if checkout_paths.is_empty() {
        eprintln!("pull: updated 0 file(s) in working tree");
        return Ok(());
    }

    super::checkout::run_lfs_checkout(super::checkout::LfsCheckoutOptions {
        paths: checkout_paths,
        ..super::checkout::LfsCheckoutOptions::default()
    })?;

    Ok(())
}

fn checkout_paths_for_pull(
    entries: &[(String, LfsPointer)],
    include: Option<&PatternFilter>,
    exclude: Option<&PatternFilter>,
) -> Vec<String> {
    entries
        .iter()
        .filter_map(|(path, _)| {
            if let Some(inc) = include
                && !inc.matches(path)
            {
                return None;
            }
            if let Some(exc) = exclude
                && exc.matches(path)
            {
                return None;
            }
            Some(path.clone())
        })
        .collect()
}

/// Collect `(path, LfsPointer)` entries from HEAD (or all refs).
///
/// Uses `git ls-tree -r` + `git cat-file --batch` to enumerate blobs
/// and parse LFS pointers.
fn collect_lfs_pointers(
    all: bool,
    recent: bool,
    refs: &[String],
) -> Result<Vec<(String, LfsPointer)>> {
    let tree_lines = if all {
        if refs.is_empty() {
            ls_tree_all_refs()?
        } else {
            ls_tree_refs(refs)?
        }
    } else if recent {
        let mut entries = if refs.is_empty() {
            ls_tree_head()?
        } else {
            ls_tree_refs(refs)?
        };
        entries.extend(ls_tree_recent_refs_and_commits(refs)?);
        entries
    } else if !refs.is_empty() {
        ls_tree_refs(refs)?
    } else {
        ls_tree_head()?
    };

    if tree_lines.is_empty() {
        return Ok(Vec::new());
    }

    let oids_input: String = tree_lines
        .iter()
        .map(|(hash, _)| hash.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let mut child = Command::new("git")
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
        .map_err(|e| CrabError::Internal(format!("git cat-file failed: {e}")))?;

    let stdout = &output.stdout;
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut pos = 0;
    let mut line_idx = 0;

    while pos < stdout.len() && line_idx < tree_lines.len() {
        let header_end = match stdout[pos..].iter().position(|&b| b == b'\n') {
            Some(p) => pos + p,
            None => break,
        };

        let header = String::from_utf8_lossy(&stdout[pos..header_end]);
        let parts: Vec<&str> = header.split_whitespace().collect();

        if parts.len() < 3 || parts[1] == "missing" {
            pos = header_end + 1;
            line_idx += 1;
            continue;
        }

        let Ok(obj_size) = parts[2].parse::<usize>() else {
            pos = header_end + 1;
            line_idx += 1;
            continue;
        };

        let content_start = header_end + 1;
        let content_end = content_start + obj_size;

        if content_end > stdout.len() {
            break;
        }

        let content = &stdout[content_start..content_end];
        let (_, filename) = &tree_lines[line_idx];

        if parts[1] == "blob"
            && content.len() <= MAX_LFS_POINTER_SIZE
            && let PointerKind::Lfs(pointer) = classify(content)
            && pointer.size > 0
            && seen.insert((filename.clone(), pointer.oid))
        {
            entries.push((filename.clone(), pointer));
        }

        pos = content_end + 1;
        line_idx += 1;
    }

    Ok(entries)
}

/// Run `git ls-tree -r HEAD` and return `(blob_hash, filename)` pairs.
fn ls_tree_head() -> Result<Vec<(String, String)>> {
    ls_tree_ref("HEAD")
}

fn ls_tree_ref(ref_name: &str) -> Result<Vec<(String, String)>> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", ref_name])
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git ls-tree: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Not a valid object name") {
            return Ok(Vec::new());
        }
        return Err(CrabError::Internal(format!(
            "git ls-tree failed for {ref_name}: {stderr}"
        )));
    }

    Ok(parse_ls_tree_output(&output.stdout))
}

fn ls_tree_refs(refs: &[String]) -> Result<Vec<(String, String)>> {
    let mut all_entries = Vec::new();
    for ref_name in refs {
        all_entries.extend(ls_tree_ref(ref_name)?);
    }
    Ok(all_entries)
}

/// Run `git ls-tree -r` for every local ref.
fn ls_tree_all_refs() -> Result<Vec<(String, String)>> {
    let refs_output = Command::new("git")
        .args(["rev-parse", "--all"])
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git rev-parse: {e}")))?;

    if !refs_output.status.success() {
        return Ok(Vec::new());
    }

    let refs_text = String::from_utf8_lossy(&refs_output.stdout);
    let mut all_entries = Vec::new();

    for ref_sha in refs_text.lines() {
        let ref_sha = ref_sha.trim();
        if ref_sha.is_empty() {
            continue;
        }

        let output = Command::new("git")
            .args(["ls-tree", "-r", ref_sha])
            .output()
            .map_err(|e| {
                CrabError::Internal(format!("failed to run git ls-tree for {ref_sha}: {e}"))
            })?;

        if output.status.success() {
            let entries = parse_ls_tree_output(&output.stdout);
            all_entries.extend(entries);
        }
    }

    Ok(all_entries)
}

/// Run `git ls-tree -r` for recent refs and recent commits.
fn ls_tree_recent_refs_and_commits(base_refs: &[String]) -> Result<Vec<(String, String)>> {
    let recent_refs = crate::lfs::recent::recent_ref_oids(0)?;
    let mut recent_roots = if base_refs.is_empty() {
        vec!["HEAD".to_owned()]
    } else {
        base_refs.to_vec()
    };
    recent_roots.extend(recent_refs.iter().cloned());

    let mut refs = recent_refs;
    refs.extend(crate::lfs::recent::recent_commit_oids(&recent_roots)?);

    if refs.is_empty() {
        return Ok(Vec::new());
    }

    ls_tree_refs(&refs)
}

/// Parse `git ls-tree -r` output into `(blob_hash, filename)` pairs.
fn parse_ls_tree_output(output: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(output);
    let mut results = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((meta, filename)) = line.split_once('\t') else {
            continue;
        };
        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() < 3 || parts[1] != "blob" {
            continue;
        }
        results.push((parts[2].to_owned(), filename.to_owned()));
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;
    use std::fs;

    fn pointer(data: &[u8]) -> LfsPointer {
        let oid: [u8; 32] = sha2::Sha256::digest(data).into();
        LfsPointer {
            oid,
            size: data.len() as u64,
            extensions: Vec::new(),
        }
    }

    #[test]
    fn validate_fetch_rejects_json_with_prune() {
        let options = LfsFetchOptions {
            json: true,
            prune: true,
            ..LfsFetchOptions::default()
        };

        let err = validate_fetch_options(&options).unwrap_err();

        assert!(err.to_string().contains("--json"));
    }

    #[test]
    fn remote_operand_becomes_ref_when_not_configured_and_alone() {
        let options = LfsFetchOptions {
            remote: Some("feature".to_owned()),
            ..LfsFetchOptions::default()
        };

        let (remote, refs) = remote_and_refs_from_options(&options).unwrap();

        assert_eq!(remote, None);
        assert_eq!(refs, vec!["feature"]);
    }

    #[test]
    fn plan_fetch_transfers_skips_local_without_refetch() {
        let dir = tempfile::tempdir().unwrap();
        let ptr = pointer(b"local");
        let local_path = local_object_path(dir.path(), &ptr.oid);
        fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        fs::write(local_path, b"local").unwrap();
        let entries = vec![("asset.bin".to_owned(), ptr)];

        let transfers = plan_fetch_transfers(&entries, None, None, dir.path(), false).unwrap();

        assert!(transfers.is_empty());
    }

    #[test]
    fn plan_fetch_transfers_includes_local_with_refetch() {
        let dir = tempfile::tempdir().unwrap();
        let ptr = pointer(b"local");
        let local_path = local_object_path(dir.path(), &ptr.oid);
        fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        fs::write(local_path, b"local").unwrap();
        let entries = vec![("asset.bin".to_owned(), ptr)];

        let transfers = plan_fetch_transfers(&entries, None, None, dir.path(), true).unwrap();

        assert_eq!(transfers.len(), 1);
    }

    #[test]
    fn plan_fetch_transfers_deduplicates_by_oid() {
        let dir = tempfile::tempdir().unwrap();
        let ptr = pointer(b"same");
        let entries = vec![
            ("a.bin".to_owned(), ptr.clone()),
            ("b.bin".to_owned(), ptr.clone()),
        ];

        let transfers = plan_fetch_transfers(&entries, None, None, dir.path(), false).unwrap();

        assert_eq!(transfers.len(), 1);
        assert_eq!(transfers[0].path, "a.bin");
    }

    #[test]
    fn checkout_paths_for_pull_applies_include_and_exclude() {
        let entries = vec![
            ("models/a.bin".to_owned(), pointer(b"a")),
            ("models/tmp.bin".to_owned(), pointer(b"tmp")),
            ("docs/readme.bin".to_owned(), pointer(b"readme")),
        ];
        let include = PatternFilter::new("models/**").unwrap();
        let exclude = PatternFilter::new("models/tmp.bin").unwrap();

        let paths = checkout_paths_for_pull(&entries, Some(&include), Some(&exclude));

        assert_eq!(paths, vec!["models/a.bin"]);
    }

    #[test]
    fn fetch_json_href_uses_crab_lfs_scheme_and_fanout() {
        let oid = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        let href = fetch_json_href("repo/path", oid);

        assert_eq!(
            href,
            "crab-lfs://repo/path/lfs/objects/01/23/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }
}
