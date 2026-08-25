//! `crab lfs prune` — remove unreferenced LFS objects from local storage.
//!
//! Wires the CLI prune subcommand to [`crate::lfs::prune::run_prune`].

use crate::core::error::{CrabError, Result, check_cancelled};
use tokio_util::sync::CancellationToken;

/// Options for `crab lfs prune`.
#[derive(Debug, Clone)]
pub struct LfsPruneOptions {
    pub verify_remote: bool,
    pub no_verify_remote: bool,
    pub verify_unreachable: bool,
    pub no_verify_unreachable: bool,
    pub when_unverified: Option<String>,
    pub recent: bool,
    pub dry_run: bool,
    pub force: bool,
    pub verbose: bool,
}

/// Run `crab lfs prune`.
///
/// Identifies and optionally deletes unreferenced LFS objects from
/// `.git/lfs/objects/`.
pub fn run_lfs_prune(options: LfsPruneOptions) -> Result<()> {
    let prune_options = resolve_prune_options(options)?;
    let summary = crate::lfs::prune::run_prune(prune_options)?;

    tracing::info!(
        pruned_count = summary.pruned_count,
        pruned_bytes = summary.pruned_bytes,
        dry_run = summary.dry_run,
        "prune complete"
    );

    Ok(())
}

/// Run LFS pruning while honoring a caller's cancellation boundary.
pub fn run_lfs_prune_with_cancel(
    options: LfsPruneOptions,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;
    let prune_options = resolve_prune_options(options)?;
    let summary = crate::lfs::prune::run_prune_with_cancel(prune_options, cancel)?;
    tracing::info!(
        pruned_count = summary.pruned_count,
        pruned_bytes = summary.pruned_bytes,
        dry_run = summary.dry_run,
        "prune complete"
    );
    Ok(())
}

fn resolve_prune_options(options: LfsPruneOptions) -> Result<crate::lfs::prune::PruneOptions> {
    let verify_remote = options.verify_remote && !options.no_verify_remote;
    if verify_remote && options.no_verify_unreachable {
        return Err(CrabError::Configuration {
            key: "--no-verify-unreachable".to_owned(),
            origin: "--verify-remote verifies every prune candidate".to_owned(),
        });
    }
    let when_unverified =
        resolve_when_unverified(options.when_unverified.as_deref(), verify_remote)?;

    Ok(crate::lfs::prune::PruneOptions {
        verify_remote,
        verify_unreachable: verify_remote || options.verify_unreachable,
        when_unverified,
        dry_run: options.dry_run,
        force: options.force,
        verbose: options.verbose,
        recent: options.recent,
    })
}

fn resolve_when_unverified(
    value: Option<&str>,
    verify_remote: bool,
) -> Result<crate::lfs::prune::WhenUnverified> {
    match value {
        Some("halt") => Ok(crate::lfs::prune::WhenUnverified::Halt),
        Some("continue") if verify_remote => Err(CrabError::Configuration {
            key: "--when-unverified".to_owned(),
            origin: "--verify-remote is fail-closed; use halt".to_owned(),
        }),
        Some("continue") => Ok(crate::lfs::prune::WhenUnverified::Continue),
        None => Ok(if verify_remote {
            crate::lfs::prune::WhenUnverified::Halt
        } else {
            crate::lfs::prune::WhenUnverified::Continue
        }),
        Some(value) => Err(CrabError::Configuration {
            key: "--when-unverified".to_owned(),
            origin: format!("expected halt or continue, got {value}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> LfsPruneOptions {
        LfsPruneOptions {
            verify_remote: false,
            no_verify_remote: false,
            verify_unreachable: false,
            no_verify_unreachable: false,
            when_unverified: None,
            recent: false,
            dry_run: false,
            force: false,
            verbose: false,
        }
    }

    #[test]
    fn resolve_prune_options_maps_when_unverified() {
        let mut options = options();
        options.when_unverified = Some("halt".to_owned());

        let resolved = resolve_prune_options(options).unwrap();

        assert_eq!(
            resolved.when_unverified,
            crate::lfs::prune::WhenUnverified::Halt
        );
    }

    #[test]
    fn resolve_prune_options_rejects_invalid_when_unverified() {
        let mut options = options();
        options.when_unverified = Some("stop".to_owned());

        let err = resolve_prune_options(options).unwrap_err();

        assert!(err.to_string().contains("--when-unverified"));
    }

    #[test]
    fn resolve_prune_options_defaults_unverified_to_halt_when_verifying_remote() {
        let mut options = options();
        options.verify_remote = true;

        let resolved = resolve_prune_options(options).unwrap();

        assert!(resolved.verify_unreachable);
        assert_eq!(
            resolved.when_unverified,
            crate::lfs::prune::WhenUnverified::Halt
        );
    }

    #[test]
    fn resolve_prune_options_rejects_unreachable_verification_bypass() {
        let mut options = options();
        options.verify_remote = true;
        options.no_verify_unreachable = true;

        let err = resolve_prune_options(options).unwrap_err();

        assert!(err.to_string().contains("--no-verify-unreachable"));
    }

    #[test]
    fn resolve_prune_options_rejects_continue_with_remote_verification() {
        let mut options = options();
        options.verify_remote = true;
        options.when_unverified = Some("continue".to_owned());

        let err = resolve_prune_options(options).unwrap_err();

        assert!(err.to_string().contains("fail-closed"));
    }

    #[test]
    fn resolve_prune_options_defaults_unverified_to_continue_without_remote_verification() {
        let resolved = resolve_prune_options(options()).unwrap();

        assert_eq!(
            resolved.when_unverified,
            crate::lfs::prune::WhenUnverified::Continue
        );
    }

    #[test]
    fn resolve_prune_options_no_verify_remote_disables_verification() {
        let mut options = options();
        options.verify_remote = true;
        options.no_verify_remote = true;

        let resolved = resolve_prune_options(options).unwrap();

        assert!(!resolved.verify_remote);
    }
}
