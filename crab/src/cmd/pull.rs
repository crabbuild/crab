//! `crab pull` — git pull + conditional auto-hydration.
//!
//! Owns Git pull through completion or cancellation, then conditionally
//! hydrates changed pointer files using repository-root-relative policy.

use std::path::{Path, PathBuf};
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::core::config::Config;
use crate::core::error::{Result, check_cancelled};
use crate::core::output::OutputMode;
use crate::core::pattern::{PatternFilter, build_filter};
use crate::core::style::CliStyle;
use crate::engine::pointer::working_tree_pointer;
use crate::git::worktree::WorktreeContext;
use crab_types::pointer::Pointer;

mod git;

/// Arguments for the `crab pull` command.
pub struct PullArgs {
    /// Remote name (default: "origin").
    pub remote: String,
    /// Branch to pull (default: current branch).
    pub branch: Option<String>,
    /// Skip automatic hydration after pulling.
    pub no_hydrate: bool,
    /// Output mode.
    pub mode: OutputMode,
}

/// Run the `crab pull` command: git pull + conditional hydration.
pub async fn run_pull(args: &PullArgs, cancel: &CancellationToken) -> Result<()> {
    let style = CliStyle::resolve(args.mode);
    let start = Instant::now();
    check_cancelled(cancel)?;
    let cwd = std::env::current_dir()?;
    let repo_root = WorktreeContext::resolve_from_path(&cwd)?.current_worktree_root;

    // Phase 1: git pull
    if !args.mode.is_machine() {
        eprintln!(
            "Pulling from {}/{}\u{2026}",
            args.remote,
            args.branch.as_deref().unwrap_or("(current branch)")
        );
    }

    let changed_files = git::pull(
        &repo_root,
        &args.remote,
        args.branch.as_deref(),
        !args.mode.is_machine(),
        cancel,
    )
    .await?;

    let pull_elapsed = start.elapsed();

    if changed_files.is_empty() {
        if !args.mode.is_machine() {
            eprintln!(
                "{}",
                style.ok(&format!(
                    "Already up to date ({:.1}s)",
                    pull_elapsed.as_secs_f64()
                ))
            );
        }
        return Ok(());
    }

    // Show what was pulled.
    if !args.mode.is_machine() {
        eprintln!(
            "  Fetched {} file(s) in {:.1}s",
            changed_files.len(),
            pull_elapsed.as_secs_f64()
        );
    }

    if args.no_hydrate {
        if !args.mode.is_machine() {
            eprintln!(
                "{}",
                style.ok(&format!(
                    "Pull complete ({} file(s) updated, hydration skipped, {:.1}s)",
                    changed_files.len(),
                    pull_elapsed.as_secs_f64()
                ))
            );
        }
        return Ok(());
    }

    let config = Config::resolve_for_repo(&repo_root)?;
    let hydration_plan = pull_hydration_plan(&config)?;
    let filter = match hydration_plan {
        PullHydrationPlan::Skip(reason) => {
            if !args.mode.is_machine() {
                eprintln!(
                    "{}",
                    style.ok(&format!(
                        "Pull complete ({} file(s) updated, hydration skipped: {}, {:.1}s)",
                        changed_files.len(),
                        reason,
                        pull_elapsed.as_secs_f64()
                    ))
                );
            }
            return Ok(());
        }
        PullHydrationPlan::All => None,
        PullHydrationPlan::Filtered(ref filter) => Some(filter),
    };

    // Phase 2: hydrate newly-fetched pointers matching filter
    let pointers = find_hydration_candidates(&repo_root, &changed_files, filter, cancel)?;

    if pointers.is_empty() {
        if !args.mode.is_machine() {
            eprintln!(
                "{}",
                style.ok(&format!(
                    "Pull complete ({} file(s) updated, no pointers to hydrate, {:.1}s)",
                    changed_files.len(),
                    pull_elapsed.as_secs_f64()
                ))
            );
        }
        return Ok(());
    }

    if !args.mode.is_machine() {
        eprintln!("  Hydrating {} file(s)\u{2026}", pointers.len());
    }

    // Git selected concrete files, not patterns. Keep their pointer identities
    // with literal paths so hydration cannot expand the inventory to siblings.
    let hydrated =
        crate::cmd::hydrate::hydrate_selected(&repo_root, pointers, &config, args.mode, cancel)
            .await?;

    let total_elapsed = start.elapsed();
    if !args.mode.is_machine() {
        eprintln!(
            "{}",
            style.ok(&format!(
                "Pull complete ({} file(s) updated, {} hydrated, {:.1}s)",
                changed_files.len(),
                hydrated.hydrated,
                total_elapsed.as_secs_f64()
            ))
        );
    }

    Ok(())
}

enum PullHydrationPlan {
    Skip(&'static str),
    All,
    Filtered(PatternFilter),
}

fn pull_hydration_plan(config: &Config) -> Result<PullHydrationPlan> {
    if !config.checkout.lazy {
        return Ok(PullHydrationPlan::All);
    }

    if !config.hydrate.auto {
        return Ok(PullHydrationPlan::Skip("lazy checkout"));
    }

    if config.hydrate.include.is_empty() {
        return Ok(PullHydrationPlan::Skip("no auto-hydrate patterns"));
    }

    let filter = build_filter(&config.hydrate.include, &config.hydrate.exclude)?;
    Ok(PullHydrationPlan::Filtered(filter))
}

/// Find pointer files among the changed files that are candidates for hydration.
fn find_hydration_candidates(
    root: &Path,
    changed_files: &[String],
    filter: Option<&PatternFilter>,
    cancel: &CancellationToken,
) -> Result<Vec<(PathBuf, Pointer)>> {
    let mut pointers = Vec::new();

    for file in changed_files {
        check_cancelled(cancel)?;
        if let Some(filter) = filter
            && !filter.matches(file)
        {
            continue;
        }

        let path = root.join(file);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_file() => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        if let Some(pointer) = working_tree_pointer(&path)? {
            pointers.push((path, pointer));
        }
    }

    Ok(pointers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_git_paths_handles_nul_delimited_names() {
        let paths = git::parse_paths(b"normal.txt\0dir/with space.bin\0").unwrap();
        assert_eq!(paths, vec!["normal.txt", "dir/with space.bin"]);
    }

    #[test]
    fn candidate_selection_uses_its_explicit_root() {
        let root = tempfile::tempdir().unwrap();
        let pointer = crab_types::pointer::Pointer {
            file_hash: [1; 32],
            size: 100,
            shard_hint: None,
        };
        std::fs::write(root.path().join("model.bin"), pointer.serialize()).unwrap();
        let paths = find_hydration_candidates(
            root.path(),
            &["model.bin".to_owned(), "deleted.bin".to_owned()],
            None,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(paths, [(root.path().join("model.bin"), pointer)]);
    }

    #[cfg(unix)]
    #[test]
    fn candidate_selection_does_not_follow_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let pointer = crab_types::pointer::Pointer {
            file_hash: [1; 32],
            size: 100,
            shard_hint: None,
        };
        std::fs::write(root.path().join("model.bin"), pointer.serialize()).unwrap();
        std::os::unix::fs::symlink("model.bin", root.path().join("link.bin")).unwrap();
        let paths = find_hydration_candidates(
            root.path(),
            &["link.bin".to_owned()],
            None,
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(paths.is_empty());
    }

    #[test]
    fn lazy_pull_without_auto_hydrate_skips() {
        let mut config = Config::default();
        config.checkout.lazy = true;

        assert!(matches!(
            pull_hydration_plan(&config).unwrap(),
            PullHydrationPlan::Skip("lazy checkout")
        ));
    }

    #[test]
    fn eager_pull_keeps_fallback_hydration() {
        let config = Config::default();

        assert!(matches!(
            pull_hydration_plan(&config).unwrap(),
            PullHydrationPlan::All
        ));
    }

    #[test]
    fn lazy_pull_with_auto_patterns_filters() {
        let mut config = Config::default();
        config.checkout.lazy = true;
        config.hydrate.auto = true;
        config.hydrate.include = vec!["models/**".to_owned()];

        assert!(matches!(
            pull_hydration_plan(&config).unwrap(),
            PullHydrationPlan::Filtered(_)
        ));
    }
}
