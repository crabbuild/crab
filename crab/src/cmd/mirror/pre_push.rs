//! Exact-ref publication for the local collaboration pre-push guard.

use std::collections::BTreeMap;
use std::path::Path;

use clap::Parser;
use crab_git::pre_push::{PrePushUpdate, read_pre_push};
use tokio_util::sync::CancellationToken;

use super::{CommandRunner, ProcessCommand, SystemCommandRunner, run_required};
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::OutputMode;
use crate::core::project_config::ProjectConfig;
use crate::git::url::CrabUrl;
use crate::storage::{Store, StoreLayout};

/// Arguments supplied by the installed pre-push hook.
#[derive(Debug, Clone, Parser)]
pub struct MirrorPrePushArgs {
    /// Git's destination remote name.
    pub remote: String,
    /// Git's resolved destination URL.
    pub url: String,
    /// Run the installed LFS guard on the same decoded batch.
    #[arg(long)]
    pub lfs: bool,
}

/// Validate and publish Git's complete hook batch before collaboration push.
pub async fn run_mirror_pre_push(
    args: &MirrorPrePushArgs,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;
    let updates = read_pre_push(std::io::stdin().lock(), 16 * 1024 * 1024)?;
    if updates.is_empty() {
        return Ok(());
    }
    // Native Crab publication already owns dependency validation. The
    // optional LFS guard still runs, but cannot recursively mirror this push.
    if args.url.starts_with("crab://") {
        if args.lfs {
            crate::cmd::lfs::push::run_lfs_pre_push_batch(Some(&args.url), &updates, cancel)?;
        }
        return Ok(());
    }

    let worktree = crate::git::worktree::WorktreeContext::resolve()?;
    let root = &worktree.current_worktree_root;
    let project = ProjectConfig::load(&root.join("crab.toml"))?;
    let mirror = project.mirror.ok_or_else(|| CrabError::Configuration {
        key: "mirror".to_owned(),
        origin: "pre-push hook requires [mirror] in crab.toml; rerun crab init".to_owned(),
    })?;
    let mut runner = SystemCommandRunner::new(cancel.clone());
    let source_urls = push_urls(root, &mirror.origin_remote, &mut runner)?;
    if !source_urls.iter().any(|url| url == &args.url) {
        return Err(CrabError::Configuration {
            key: "mirror collaboration destination".to_owned(),
            origin: "hook destination does not match the configured mirror source push URL"
                .to_owned(),
        });
    }
    let destinations = push_urls(root, &mirror.crab_remote, &mut runner)?;
    let [destination] = destinations.as_slice() else {
        return Err(CrabError::Configuration {
            key: "mirror Crab destination".to_owned(),
            origin: "mirror publication requires exactly one Crab push URL".to_owned(),
        });
    };
    let parsed = CrabUrl::parse(destination)?;
    let config = crate::core::config::Config::resolve_for_repo(root)?;
    let store =
        crate::auth::build_repository_url_store(&config, parsed.clone(), "mirror-pre-push", cancel)
            .await?;
    let router = StoreLayout::new(store.clone(), parsed.repo_path);
    let before = destination_snapshot(&store, &router, cancel).await?;
    let expected = admit_updates(&updates, &before.journal.refs)?;
    let refspecs = updates
        .iter()
        .map(|update| {
            format!(
                "{}:{}",
                update.local_oid.as_deref().unwrap_or_default(),
                update.remote_ref
            )
        })
        .collect::<Vec<_>>();
    let source_refs = updates
        .iter()
        .filter_map(|update| {
            update
                .local_oid
                .as_ref()
                .map(|oid| (update.remote_ref.clone(), oid.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let pointers = super::pointers::collect(
        super::pointers::Source::Repository(worktree.common_git_dir.clone()),
        &source_refs,
        cancel,
        &mut runner,
    )
    .await?;

    if args.lfs {
        // Collaboration URLs point at Git hosts, not the Crab LFS store.
        // Both guards use the captured Crab destination and the same OIDs.
        crate::cmd::lfs::push::run_lfs_pre_push_batch(Some(destination), &updates, cancel)?;
    }
    check_cancelled(cancel)?;
    crate::cmd::push::run_push_prepared_refspecs(
        Some(destination),
        &refspecs,
        Some(expected),
        cancel,
    )
    .await?;
    let checker =
        crate::cmd::fsck_store::StoreChecker::new(store.clone(), router.repo_prefix().to_owned());
    let after = destination_snapshot(&store, &router, cancel).await?;
    let proof = checker
        .verify_pointer_data(&after, &pointers, cancel)
        .await?;
    if !proof.issues.is_empty() || proof.verified != pointers.len() as u64 {
        let details = proof
            .issues
            .iter()
            .map(|issue| issue.detail.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(CrabError::Protocol(format!(
            "mirror pointer data is not fully verified in Crab: {details}"
        )));
    }
    let confirmed = destination_snapshot(&store, &router, cancel).await?;
    if updates
        .iter()
        .any(|update| after.journal.refs.get(&update.remote_ref) != update.local_oid.as_ref())
        || after != confirmed
    {
        return Err(CrabError::Protocol("Crab refs changed before mirror publication could be confirmed; run crab mirror --check".to_owned()));
    }
    Ok(())
}

fn push_urls(root: &Path, remote: &str, runner: &mut dyn CommandRunner) -> Result<Vec<String>> {
    let command = ProcessCommand::new(
        "git",
        vec![
            "remote".into(),
            "get-url".into(),
            "--push".into(),
            "--all".into(),
            remote.to_owned(),
        ],
    )
    .current_dir(Some(root))
    .env_remove(super::GIT_ENV_REMOVALS);
    let output = run_required(runner, command, OutputMode::Text)?;
    Ok(output.stdout.lines().map(ToOwned::to_owned).collect())
}

async fn destination_snapshot(
    store: &Store,
    router: &StoreLayout,
    cancel: &CancellationToken,
) -> Result<crate::metadata::manifest::RepositorySnapshot> {
    check_cancelled(cancel)?;
    let snapshot = crate::metadata::manifest::read_repository_snapshot(store, router).await?;
    check_cancelled(cancel)?;
    Ok(snapshot)
}

fn admit_updates(
    updates: &[PrePushUpdate],
    crab_refs: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Option<String>>> {
    let mut expected = BTreeMap::new();
    for update in updates {
        if update.local_oid.as_ref().is_some_and(|oid| oid.len() != 40)
            || update
                .remote_oid
                .as_ref()
                .is_some_and(|oid| oid.len() != 40)
        {
            return Err(CrabError::Protocol(
                "mirror publication requires SHA-1 Git objects".to_owned(),
            ));
        }
        let crab_oid = crab_refs.get(&update.remote_ref);
        if crab_oid.is_some()
            && crab_oid != update.remote_oid.as_ref()
            && crab_oid != update.local_oid.as_ref()
        {
            return Err(CrabError::Protocol(format!(
                "{} differs in Crab; reconcile it before retrying the collaboration push",
                update.remote_ref
            )));
        }
        expected.insert(update.remote_ref.clone(), crab_oid.cloned());
    }
    Ok(expected)
}

#[cfg(test)]
mod tests;
