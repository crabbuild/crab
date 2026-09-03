//! `crab lfs fetch` and `crab lfs pull` — download LFS objects.
//!
//! Wires the CLI fetch/pull subcommands to [`crate::lfs::batch::BatchResolver`]
//! via the shared [`super::store_setup`] module.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::emit_json;
use crate::lfs::batch::{BatchResolver, PatternFilter};
use crab_git::lfs_pointer::{LfsPointer, hex_encode};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

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
pub fn run_lfs_fetch(options: LfsFetchOptions, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;
    validate_fetch_options(&options)?;
    let (remote, refs) = remote_and_refs_from_options(&options)?;
    let ctx = resolve_lfs_remote_for_operation_with_remote_sync("pull", remote.as_deref())?;
    let entries = collect_lfs_pointers(Path::new("."), options.all, options.recent, &refs, cancel)?;
    check_cancelled(cancel)?;

    if entries.is_empty() {
        if options.json {
            write_fetch_json(&[], &ctx.prefix, &ctx.local_lfs_dir)?;
        } else {
            eprintln!("fetch: no LFS objects to fetch");
        }
        if options.prune {
            run_fetch_prune(options.dry_run, cancel)?;
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
    check_cancelled(cancel)?;

    if transfers.is_empty() {
        if options.json {
            write_fetch_json(&[], &ctx.prefix, &ctx.local_lfs_dir)?;
        } else {
            eprintln!("fetch: all objects up to date");
        }
        if options.prune {
            run_fetch_prune(options.dry_run, cancel)?;
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
            run_fetch_prune(true, cancel)?;
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
        let resolver = BatchResolver::new(ctx.store, ctx.local_lfs_dir, ctx.config, cancel);

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
        run_fetch_prune(false, cancel)?;
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
    let mut seen_sizes = HashMap::new();

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
        if let Some(existing_size) = seen_sizes.get(&pointer.oid) {
            if *existing_size != pointer.size {
                return Err(CrabError::LfsObjectCorrupt {
                    oid: hex_encode(&pointer.oid),
                });
            }
            continue;
        }
        seen_sizes.insert(pointer.oid, pointer.size);
        let local_valid = crate::lfs::cache::is_valid(local_lfs_dir, &pointer.oid, pointer.size)?;
        if !refetch && local_valid {
            continue;
        }
        transfers.push(FetchTransfer {
            path: path.clone(),
            pointer: pointer.clone(),
        });
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

fn run_fetch_prune(dry_run: bool, cancel: &CancellationToken) -> Result<()> {
    super::prune::run_lfs_prune_with_cancel(
        super::prune::LfsPruneOptions {
            verify_remote: false,
            no_verify_remote: false,
            verify_unreachable: false,
            no_verify_unreachable: false,
            when_unverified: Some("continue".to_owned()),
            recent: false,
            dry_run,
            force: true,
            verbose: dry_run,
        },
        cancel,
    )
}

/// Run `crab lfs pull`.
///
/// Fetches missing LFS objects then replaces LFS pointers in the working
/// tree with the actual file content.
pub fn run_lfs_pull(options: LfsPullOptions, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;
    let ctx = resolve_lfs_remote_for_operation_with_remote_sync("pull", options.remote.as_deref())?;
    let entries = collect_lfs_pointers(Path::new("."), false, false, &[], cancel)?;
    check_cancelled(cancel)?;

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
        let resolver = BatchResolver::new(ctx.store, ctx.local_lfs_dir, ctx.config, cancel);

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
    check_cancelled(cancel)?;

    if checkout_paths.is_empty() {
        eprintln!("pull: updated 0 file(s) in working tree");
        return Ok(());
    }

    super::checkout::run_lfs_checkout(super::checkout::LfsCheckoutOptions {
        paths: checkout_paths,
        ..super::checkout::LfsCheckoutOptions::default()
    })?;
    check_cancelled(cancel)?;
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

fn collect_lfs_pointers(
    repo_dir: &Path,
    all: bool,
    recent: bool,
    refs: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<(String, LfsPointer)>> {
    check_cancelled(cancel)?;
    let mut selected = if all && refs.is_empty() {
        crate::lfs::discovery::all_ref_names(repo_dir, cancel)?
    } else if refs.is_empty() {
        let repository = gix::discover(repo_dir).map_err(io::Error::other)?;
        let head = repository.head().map_err(io::Error::other)?;
        if head.is_unborn() {
            Vec::new()
        } else {
            vec!["HEAD".to_owned()]
        }
    } else {
        refs.to_vec()
    };
    if recent {
        let recent_refs = crate::lfs::recent::recent_ref_oids_in(repo_dir, 0)?;
        check_cancelled(cancel)?;
        let mut roots = selected.clone();
        roots.extend(recent_refs.iter().cloned());
        selected.extend(recent_refs);
        selected.extend(crate::lfs::recent::recent_commit_oids_in(repo_dir, &roots)?);
    }
    check_cancelled(cancel)?;
    let mut seen = HashSet::new();
    selected.retain(|revision| seen.insert(revision.clone()));
    crate::lfs::discovery::collect_pointers_from_trees_in(repo_dir, &selected, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_fetch_and_pull_stop_before_repository_resolution() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            run_lfs_fetch(LfsFetchOptions::default(), &cancel),
            Err(CrabError::Cancelled)
        ));
        assert!(matches!(
            run_lfs_pull(LfsPullOptions::default(), &cancel),
            Err(CrabError::Cancelled)
        ));
    }
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

    fn fixture_git(root: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        for key in crate::git::process::GIT_ENV_REMOVALS {
            command.env_remove(key);
        }
        let output = command.current_dir(root).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn commit_fixture(root: &Path) {
        fixture_git(root, &["add", "."]);
        fixture_git(
            root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "pointer fixture",
            ],
        );
    }

    #[test]
    fn unborn_default_fetch_is_empty_but_an_invalid_explicit_ref_fails() {
        let temporary = tempfile::tempdir().unwrap();
        fixture_git(temporary.path(), &["init", "-q"]);
        let cancel = CancellationToken::new();
        assert!(
            collect_lfs_pointers(temporary.path(), false, false, &[], &cancel)
                .unwrap()
                .is_empty()
        );
        assert!(
            collect_lfs_pointers(
                temporary.path(),
                false,
                false,
                &["missing-ref".to_owned()],
                &cancel
            )
            .is_err()
        );
    }

    #[test]
    fn fetch_from_subdirectory_preserves_aliases_until_path_policy() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fixture_git(root, &["init", "-q"]);
        fs::create_dir(root.join("nested")).unwrap();
        let ptr = pointer(b"shared content");
        fs::write(root.join("a.bin"), ptr.serialize()).unwrap();
        fs::write(root.join("nested/z.bin"), ptr.serialize()).unwrap();
        commit_fixture(root);
        let entries = collect_lfs_pointers(
            &root.join("nested"),
            false,
            false,
            &[],
            &CancellationToken::new(),
        )
        .unwrap();
        let include = PatternFilter::new("nested/**").unwrap();
        let transfers = plan_fetch_transfers(
            &entries,
            Some(&include),
            None,
            &root.join("empty-cache"),
            false,
        )
        .unwrap();
        assert_eq!(
            transfers
                .iter()
                .map(|transfer| transfer.path.as_str())
                .collect::<Vec<_>>(),
            ["nested/z.bin"]
        );
        assert_eq!(
            checkout_paths_for_pull(&entries, None, None),
            ["a.bin", "nested/z.bin"]
        );
    }

    #[test]
    fn all_ref_fetch_retains_distinct_pointer_versions_and_rejects_partial_inventory() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fixture_git(root, &["init", "-q"]);
        let old = pointer(b"old content");
        fs::write(root.join("asset.bin"), old.serialize()).unwrap();
        commit_fixture(root);
        fixture_git(root, &["branch", "old"]);
        let new = pointer(b"new content");
        fs::write(root.join("asset.bin"), new.serialize()).unwrap();
        commit_fixture(root);
        let cancel = CancellationToken::new();
        let entries = collect_lfs_pointers(root, true, false, &[], &cancel).unwrap();
        let oids = entries
            .into_iter()
            .map(|(_, pointer)| pointer.oid)
            .collect::<HashSet<_>>();
        assert_eq!(oids, HashSet::from([old.oid, new.oid]));
        assert!(
            collect_lfs_pointers(
                root,
                false,
                false,
                &["HEAD".to_owned(), "missing-ref".to_owned()],
                &cancel
            )
            .is_err()
        );
    }

    #[test]
    fn cancelled_fetch_discovery_never_opens_a_repository() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            collect_lfs_pointers(Path::new("absent"), false, false, &[], &cancel),
            Err(CrabError::Cancelled)
        ));
    }

    #[test]
    fn fetch_discovery_resolves_promised_git_pointer_blobs() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir(&source).unwrap();
        fixture_git(&source, &["init", "-q"]);
        fixture_git(&source, &["config", "uploadpack.allowFilter", "true"]);
        let ptr = pointer(b"promised LFS content");
        fs::write(source.join("asset.bin"), ptr.serialize()).unwrap();
        commit_fixture(&source);
        let remote = url::Url::from_file_path(&source).unwrap().to_string();
        fixture_git(
            temporary.path(),
            &[
                "clone",
                "--filter=blob:none",
                "--no-checkout",
                &remote,
                "reader",
            ],
        );
        let reader = temporary.path().join("reader");
        let missing = fixture_git(
            &reader,
            &["rev-list", "--objects", "--all", "--missing=print"],
        );
        assert!(missing.lines().any(|line| line.starts_with('?')));

        let head = fixture_git(&reader, &["rev-parse", "HEAD"]);
        let cancel = CancellationToken::new();
        assert!(
            crate::lfs::discovery::collect_pointers_from_range_in(&reader, &[head], &[], &cancel)
                .is_err()
        );
        assert_eq!(
            fixture_git(
                &reader,
                &["rev-list", "--objects", "--all", "--missing=print"]
            ),
            missing
        );

        let entries = collect_lfs_pointers(&reader, false, false, &[], &cancel)
            .expect("fetch must resolve promised Git blobs before LFS transfer");

        assert_eq!(entries, [("asset.bin".to_owned(), ptr)]);
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
