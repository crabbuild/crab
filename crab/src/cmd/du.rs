//! `crab du` — disk usage breakdown for crab-managed storage.
//!
//! Shows where space is consumed: local cache, staging area, hydrated
//! files in the working tree, pointer files, and (optionally) the
//! remote total. Answers the perennial "why is my disk full?" question
//! with actionable numbers.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::core::error::Result;
use crate::core::output::{OutputMode, emit_json};
use crate::engine::pointer;

/// Arguments for the `crab du` command.
pub struct DuArgs {
    /// Include remote storage size (requires network access).
    pub remote: bool,
    /// Output mode resolved from CLI flags.
    pub mode: OutputMode,
}

/// Accumulated disk usage statistics.
struct DuStats {
    cache_bytes: u64,
    cache_path: PathBuf,
    staging_bytes: u64,
    staging_path: PathBuf,
    hydrated_bytes: u64,
    hydrated_count: u64,
    pointer_bytes: u64,
    pointer_count: u64,
    git_dir_bytes: u64,
    remote_bytes: Option<u64>,
    remote_url: Option<String>,
}

/// Serializable payload for `--json` output. Field names and types are
/// byte-compatible with the pre-envelope bare JSON that `du` used to emit.
#[derive(Serialize, schemars::JsonSchema)]
pub struct DuPayload {
    cache_bytes: u64,
    cache_path: String,
    staging_bytes: u64,
    git_dir_bytes: u64,
    hydrated_bytes: u64,
    hydrated_count: u64,
    pointer_bytes: u64,
    pointer_count: u64,
    local_total_bytes: u64,
    remote_bytes: Option<u64>,
    remote_url: Option<String>,
}

impl From<&DuStats> for DuPayload {
    fn from(s: &DuStats) -> Self {
        Self {
            cache_bytes: s.cache_bytes,
            cache_path: s.cache_path.display().to_string(),
            staging_bytes: s.staging_bytes,
            git_dir_bytes: s.git_dir_bytes,
            hydrated_bytes: s.hydrated_bytes,
            hydrated_count: s.hydrated_count,
            pointer_bytes: s.pointer_bytes,
            pointer_count: s.pointer_count,
            local_total_bytes: s.cache_bytes
                + s.staging_bytes
                + s.git_dir_bytes
                + s.hydrated_bytes
                + s.pointer_bytes,
            remote_bytes: s.remote_bytes,
            remote_url: s.remote_url.clone(),
        }
    }
}

/// Run `crab du` in the current working directory.
pub async fn run_du(args: &DuArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_du_in(&cwd, args).await
}

/// Compute and display disk usage rooted at `root`.
pub async fn run_du_in(root: &Path, args: &DuArgs) -> Result<()> {
    let cache_path = resolve_cache_dir();
    let (staging_path, git_dir_path) = resolve_repository_usage_paths(root);

    let cache_bytes = dir_size(&cache_path).unwrap_or(0);
    let staging_bytes = dir_size(&staging_path).unwrap_or(0);
    let git_dir_bytes = dir_size(&git_dir_path).unwrap_or(0);

    let (hydrated_bytes, hydrated_count, pointer_bytes, pointer_count) = scan_working_tree(root)?;

    let (remote_bytes, remote_url) = if args.remote {
        fetch_remote_size(root).await
    } else {
        (None, read_remote_url(root))
    };

    let stats = DuStats {
        cache_bytes,
        cache_path,
        staging_bytes,
        staging_path,
        hydrated_bytes,
        hydrated_count,
        pointer_bytes,
        pointer_count,
        git_dir_bytes,
        remote_bytes,
        remote_url,
    };

    match args.mode {
        OutputMode::Json => emit_json("du", "1.1", DuPayload::from(&stats)),
        OutputMode::Text | OutputMode::Jsonl => print_table(&stats),
    }

    Ok(())
}

fn resolve_repository_usage_paths(root: &Path) -> (PathBuf, PathBuf) {
    crate::git::worktree::WorktreeContext::resolve_from_path(root).map_or_else(
        |_| (root.join(".crab/staging"), root.join(".git")),
        |ctx| (ctx.shared_staging_dir(), ctx.common_git_dir),
    )
}

/// Scan the working tree to separate hydrated files from pointer stubs.
fn scan_working_tree(root: &Path) -> Result<(u64, u64, u64, u64)> {
    let patterns = parse_crab_patterns(root)?;
    if patterns.is_empty() {
        return Ok((0, 0, 0, 0));
    }

    let mut hydrated_bytes: u64 = 0;
    let mut hydrated_count: u64 = 0;
    let mut pointer_bytes: u64 = 0;
    let mut pointer_count: u64 = 0;

    walk_tracked(
        root,
        root,
        &patterns,
        &mut hydrated_bytes,
        &mut hydrated_count,
        &mut pointer_bytes,
        &mut pointer_count,
    )?;

    Ok((hydrated_bytes, hydrated_count, pointer_bytes, pointer_count))
}

/// Recursively walk the tree, classifying tracked files.
fn walk_tracked(
    root: &Path,
    dir: &Path,
    patterns: &[String],
    hydrated_bytes: &mut u64,
    hydrated_count: &mut u64,
    pointer_bytes: &mut u64,
    pointer_count: &mut u64,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;

        if ft.is_dir() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            walk_tracked(
                root,
                &path,
                patterns,
                hydrated_bytes,
                hydrated_count,
                pointer_bytes,
                pointer_count,
            )?;
            continue;
        }

        if !ft.is_file() {
            continue;
        }

        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };

        if !matches_pattern(rel, patterns) {
            continue;
        }

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);

        if pointer::is_working_tree_pointer(&path).unwrap_or(false) {
            *pointer_bytes += size;
            *pointer_count += 1;
        } else {
            *hydrated_bytes += size;
            *hydrated_count += 1;
        }
    }

    Ok(())
}

/// Parse `.gitattributes` for `filter=crab` patterns.
fn parse_crab_patterns(root: &Path) -> Result<Vec<String>> {
    let ga_path = root.join(".gitattributes");
    let content = match std::fs::read_to_string(&ga_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let patterns = content
        .lines()
        .filter(|l| l.contains("filter=crab"))
        .filter_map(|l| l.split_whitespace().next())
        .map(String::from)
        .collect();

    Ok(patterns)
}

/// Simple glob matching (same logic as status.rs).
fn matches_pattern(rel_path: &Path, patterns: &[String]) -> bool {
    let path_str = rel_path.to_string_lossy();
    for pattern in patterns {
        if pattern == "*" || pattern == "**" || pattern == "**/*" {
            return true;
        }
        if let Some(suffix) = pattern.strip_prefix('*')
            && path_str.ends_with(suffix)
        {
            return true;
        }
        if *pattern == *path_str {
            return true;
        }
    }
    false
}

/// Recursively compute directory size.
fn dir_size(path: &Path) -> Option<u64> {
    let mut total: u64 = 0;
    let rd = std::fs::read_dir(path).ok()?;
    for entry in rd.flatten() {
        let ft = entry.file_type().ok()?;
        if ft.is_file() {
            total += entry.metadata().ok()?.len();
        } else if ft.is_dir() {
            total += dir_size(&entry.path()).unwrap_or(0);
        }
    }
    Some(total)
}

/// Resolve the local cache directory.
fn resolve_cache_dir() -> PathBuf {
    crate::cache::default_cache_root()
}

/// Read the remote URL from `crab.toml` (best-effort).
fn read_remote_url(root: &Path) -> Option<String> {
    crate::core::project_config::ProjectConfig::load_for_repo(root)
        .ok()
        .flatten()
        .map(|config| config.remote.url)
        .filter(|url| !url.trim().is_empty())
}

/// Query the remote for total storage size.
async fn fetch_remote_size(root: &Path) -> (Option<u64>, Option<String>) {
    use futures_util::TryStreamExt;
    use object_store::ObjectStore;

    let Some(url) = read_remote_url(root) else {
        return (None, None);
    };

    let Ok(parsed) = crate::git::url::CrabUrl::parse(&url) else {
        return (None, Some(url));
    };

    let config = crate::core::config::Config::resolve_for_repo(root).unwrap_or_default();
    let cancel = tokio_util::sync::CancellationToken::new();
    let Ok(store) = crate::auth::build_repository_url_store(&config, &parsed, "du", &cancel).await
    else {
        return (None, Some(url));
    };

    let mut total: u64 = 0;

    // List per-repo objects (refs, packs, manifests, locks).
    let repo_prefix = object_store::path::Path::from(parsed.repo_path.as_str());
    let mut stream = store.inner().list(Some(&repo_prefix));
    loop {
        match stream.try_next().await {
            Ok(Some(meta)) => total += meta.size,
            Ok(None) => break,
            Err(_) => return (None, Some(url)),
        }
    }

    // List global content-addressed objects (.crab/).
    let global_prefix = object_store::path::Path::from(".crab");
    let mut stream = store.inner().list(Some(&global_prefix));
    loop {
        match stream.try_next().await {
            Ok(Some(meta)) => total += meta.size,
            Ok(None) => break,
            Err(_) => return (None, Some(url)),
        }
    }

    (Some(total), Some(url))
}

/// Print the human-readable table.
fn print_table(stats: &DuStats) {
    println!("crab disk usage\n");

    print_row(
        "Local cache",
        stats.cache_bytes,
        &format!("({})", stats.cache_path.display()),
    );
    print_row(
        "Staging area",
        stats.staging_bytes,
        &format!("({})", stats.staging_path.display()),
    );
    print_row("Git objects", stats.git_dir_bytes, "(.git/)");
    print_row_with_count(
        "Hydrated files",
        stats.hydrated_bytes,
        stats.hydrated_count,
        "(working tree)",
    );
    print_row_with_count(
        "Pointer files",
        stats.pointer_bytes,
        stats.pointer_count,
        "(working tree)",
    );

    println!("  {:-<60}", "");

    let local_total = stats.cache_bytes
        + stats.staging_bytes
        + stats.git_dir_bytes
        + stats.hydrated_bytes
        + stats.pointer_bytes;
    print_row("Local total", local_total, "");

    if let Some(remote) = stats.remote_bytes {
        let url = stats.remote_url.as_deref().unwrap_or("remote");
        print_row("Remote total", remote, &format!("({url})"));
    } else if let Some(ref url) = stats.remote_url {
        println!("  {:<18} {:<12} ({url})", "Remote total", "--");
        println!("                   (use --remote to query)");
    }
}

fn print_row(label: &str, bytes: u64, suffix: &str) {
    println!("  {:<18} {:<12} {suffix}", label, format_bytes(bytes));
}

fn print_row_with_count(label: &str, bytes: u64, count: u64, suffix: &str) {
    println!(
        "  {:<18} {:<12} ({count} files) {suffix}",
        label,
        format_bytes(bytes),
    );
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
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

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
        assert_eq!(format_bytes(2_684_354_560), "2.5 GB");
        assert_eq!(format_bytes(1_099_511_627_776), "1.0 TB");
    }

    #[test]
    fn parse_crab_patterns_extracts_filter_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n\
             *.txt text\n\
             *.safetensors filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        let patterns = parse_crab_patterns(dir.path()).unwrap();
        assert_eq!(patterns, vec!["*.bin", "*.safetensors"]);
    }

    #[test]
    fn parse_crab_patterns_empty_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let patterns = parse_crab_patterns(dir.path()).unwrap();
        assert!(patterns.is_empty());
    }

    #[test]
    fn matches_pattern_star_ext() {
        assert!(matches_pattern(Path::new("model.bin"), &["*.bin".into()]));
        assert!(!matches_pattern(Path::new("model.txt"), &["*.bin".into()]));
    }

    #[test]
    fn matches_pattern_wildcard_all() {
        assert!(matches_pattern(Path::new("anything"), &["*".into()]));
        assert!(matches_pattern(Path::new("sub/dir/file"), &["**".into()]));
    }

    #[test]
    fn dir_size_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(dir_size(dir.path()), Some(0));
    }

    #[test]
    fn dir_size_with_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        std::fs::write(dir.path().join("b.txt"), "world!").unwrap();
        assert_eq!(dir_size(dir.path()), Some(11));
    }

    #[test]
    fn dir_size_nested() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/file"), "abc").unwrap();
        assert_eq!(dir_size(dir.path()), Some(3));
    }

    #[test]
    fn dir_size_nonexistent() {
        assert_eq!(dir_size(Path::new("/nonexistent/path")), None);
    }

    #[test]
    fn linked_worktree_du_uses_shared_staging_and_common_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let linked = dir.path().join("linked");

        let ok = std::process::Command::new("git")
            .args(["init", "--initial-branch=main", root.to_str().unwrap()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if ok.is_err() || !ok.unwrap().success() {
            return;
        }
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&root)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .status();

        std::fs::write(root.join("README.md"), b"initial").unwrap();
        std::fs::write(
            root.join("crab.toml"),
            b"[remote]\nurl = \"crab://bucket/worktree-du\"\n",
        )
        .unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "README.md", "crab.toml"])
            .current_dir(&root)
            .status();
        let committed = std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if committed.is_err() || !committed.unwrap().success() {
            return;
        }
        let added = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&root)
            .status();
        if added.is_err() || !added.unwrap().success() {
            return;
        }
        let (staging_path, git_dir_path) = resolve_repository_usage_paths(&linked);
        let root = root.canonicalize().unwrap();

        assert_eq!(staging_path, root.join(".crab").join("staging"));
        assert_eq!(git_dir_path, root.join(".git"));
        assert_eq!(
            read_remote_url(&linked).as_deref(),
            Some("crab://bucket/worktree-du")
        );
        assert!(linked.join(".git").is_file());
    }

    #[test]
    fn scan_working_tree_no_gitattributes() {
        let dir = tempfile::tempdir().unwrap();
        let (h, hc, p, pc) = scan_working_tree(dir.path()).unwrap();
        assert_eq!((h, hc, p, pc), (0, 0, 0, 0));
    }

    #[test]
    fn scan_working_tree_hydrated_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        // A large file (not a pointer).
        std::fs::write(dir.path().join("data.bin"), vec![0xAB; 4096]).unwrap();

        let (h_bytes, h_count, p_bytes, p_count) = scan_working_tree(dir.path()).unwrap();
        assert_eq!(h_bytes, 4096);
        assert_eq!(h_count, 1);
        assert_eq!(p_bytes, 0);
        assert_eq!(p_count, 0);
    }

    #[test]
    fn scan_working_tree_pointer_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        // Write a valid crab pointer.
        let pointer_content = format!(
            "version https://crab.dev/spec/v1\n\
             file-hash {}\n\
             size 1048576\n",
            "a".repeat(64),
        );
        std::fs::write(dir.path().join("model.bin"), &pointer_content).unwrap();

        let (h_bytes, h_count, p_bytes, p_count) = scan_working_tree(dir.path()).unwrap();
        assert_eq!(h_bytes, 0);
        assert_eq!(h_count, 0);
        assert!(p_bytes > 0);
        assert_eq!(p_count, 1);
    }
}
