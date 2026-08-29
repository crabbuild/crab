//! Shared live mount operations used by backend control planes.

use std::fs;
use std::path::Path;

use tracing::{debug, error, info};

use crate::core::error::{CrabError, Result};
use crate::pipeline::{PipelineConfig, PipelineOutput};
use crate::refresh::{
    GitRemoteRefFetcher, RemoteRefFetcher, normalized_fetch_ref, reconcile_fetched_ref,
    run_read_tree_head,
};
use crate::source::MountSource;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountRuntimeUpdate {
    pub head_oid: String,
    pub head_ref: String,
    pub generation: i64,
}

pub fn refresh_mount_runtime(
    output: &mut PipelineOutput,
    config: &PipelineConfig,
    mountpoint: &Path,
) -> Result<MountRuntimeUpdate> {
    let git_dir = &config.git_dir;
    let (head_oid, head_ref) =
        resolve_refresh_head(git_dir, &config.source, config.ref_name.as_deref())?;
    info!(head_oid = %head_oid, head_ref = %head_ref, "resolved new HEAD after fetch");

    if output.head_oid == head_oid && output.head_ref == head_ref {
        return Ok(MountRuntimeUpdate {
            head_oid,
            head_ref,
            generation: output.resolver.generation(),
        });
    }

    let update = publish_runtime_snapshot(output, config, &head_oid, &head_ref)?;
    info!(
        mountpoint = %mountpoint.display(),
        generation = update.generation,
        head_oid = %update.head_oid,
        "mount refreshed"
    );
    Ok(update)
}

pub fn switch_mount_runtime(
    output: &mut PipelineOutput,
    config: &mut PipelineConfig,
    mountpoint: &Path,
    new_ref: &str,
) -> Result<MountRuntimeUpdate> {
    let git_dir = &config.git_dir;

    info!(
        git_dir = %git_dir.display(),
        new_ref,
        "switching mount ref: fetching"
    );
    git_fetch_in_dir(git_dir)?;

    let full_ref = normalized_fetch_ref(new_ref)?;
    let head_oid = resolve_ref(git_dir, &full_ref)?;
    info!(head_oid = %head_oid, full_ref = %full_ref, "resolved new ref");

    let update = publish_runtime_snapshot(output, config, &head_oid, &full_ref)?;
    config.ref_name = Some(full_ref.clone());
    info!(
        mountpoint = %mountpoint.display(),
        generation = update.generation,
        head_oid = %update.head_oid,
        new_ref = %full_ref,
        "mount switched to new ref"
    );
    Ok(update)
}

fn publish_runtime_snapshot(
    output: &mut PipelineOutput,
    config: &PipelineConfig,
    head_oid: &str,
    head_ref: &str,
) -> Result<MountRuntimeUpdate> {
    output
        .snapshot
        .publish_generation_from_git(&config.git_dir, head_oid, head_ref)?;

    let generation = output
        .snapshot
        .current_generation()?
        .ok_or_else(|| CrabError::Internal("no generation after publish".into()))?;

    if let Some(ref overlay) = output.overlay {
        let snap_ref = std::sync::Arc::clone(&output.snapshot);
        overlay.reconcile(|path| {
            let node = snap_ref.get_node(generation, path).ok().flatten()?;
            Some(crate::overlay::ReconcileBaseInfo {
                is_dir: node.node_type == crate::snapshot::NodeType::Dir,
                object_oid: node.object_oid.clone(),
            })
        })?;
    }

    let commit_time = crate::refresh::commit_time_from_oid(&config.git_dir, head_oid).unwrap_or(0);
    output.resolver.set_commit_time(commit_time);
    output.resolver.set_generation(generation);
    output.engine.invalidate_read_source_cache();
    output.head_oid = head_oid.to_owned();
    output.head_ref = head_ref.to_owned();
    output.generation = generation;
    run_read_tree_head(&config.git_dir);

    Ok(MountRuntimeUpdate {
        head_oid: head_oid.to_owned(),
        head_ref: head_ref.to_owned(),
        generation,
    })
}

/// Adopt a snapshot generation already published by a mounted commit.
///
/// The commit path builds and persists the new generation before clearing the
/// overlay. The live mount must swap to that exact generation before serving
/// more requests, otherwise later publication can prune the generation still
/// held by the resolver.
pub fn adopt_published_snapshot(
    output: &mut PipelineOutput,
    git_dir: &Path,
    head_oid: &str,
    head_ref: &str,
) -> Result<MountRuntimeUpdate> {
    let persisted_oid = output
        .snapshot
        .head_oid()?
        .ok_or_else(|| CrabError::Internal("published snapshot is missing its HEAD OID".into()))?;
    if persisted_oid != head_oid {
        return Err(CrabError::Internal(format!(
            "published snapshot HEAD {persisted_oid} does not match committed HEAD {head_oid}"
        )));
    }
    let persisted_ref = output
        .snapshot
        .ref_name()?
        .ok_or_else(|| CrabError::Internal("published snapshot is missing its ref".into()))?;
    if persisted_ref != head_ref {
        return Err(CrabError::Internal(format!(
            "published snapshot ref {persisted_ref} does not match committed ref {head_ref}"
        )));
    }

    let generation = output
        .snapshot
        .current_generation()?
        .ok_or_else(|| CrabError::Internal("published snapshot has no generation".into()))?;
    let old_generation = output.resolver.generation();
    let commit_time = crate::refresh::commit_time_from_oid(git_dir, head_oid).unwrap_or(0);
    output.resolver.set_commit_time(commit_time);
    output.resolver.set_generation(generation);
    output.engine.invalidate_read_source_cache();
    output.head_oid = head_oid.to_owned();
    output.head_ref = head_ref.to_owned();
    output.generation = generation;
    run_read_tree_head(git_dir);

    info!(
        old_generation,
        generation, head_oid, head_ref, "adopted committed mount snapshot"
    );
    Ok(MountRuntimeUpdate {
        head_oid: head_oid.to_owned(),
        head_ref: head_ref.to_owned(),
        generation,
    })
}

fn resolve_refresh_head(
    git_dir: &Path,
    source: &str,
    tracked_ref: Option<&str>,
) -> Result<(String, String)> {
    match MountSource::parse(source)? {
        MountSource::Remote { .. } => {
            let head_ref = normalized_fetch_ref(tracked_ref.unwrap_or("HEAD"))?;
            let fetcher = GitRemoteRefFetcher::new(git_dir.to_path_buf());
            let remote_oid =
                fetcher
                    .fetch_ref_oid(&head_ref)?
                    .ok_or_else(|| CrabError::NotFound {
                        path: format!("remote ref {head_ref}"),
                    })?;
            let head_oid = reconcile_fetched_ref(git_dir, &head_ref, &remote_oid)?;
            Ok((head_oid, head_ref))
        }
        MountSource::Local { .. } => {
            git_fetch_in_dir(git_dir)?;
            crate::pipeline::resolve_head(git_dir)
        }
    }
}

fn git_fetch_in_dir(git_dir: &Path) -> Result<()> {
    debug!(git_dir = %git_dir.display(), "running git fetch origin");

    let output = std::process::Command::new("git")
        .args(["fetch", "origin"])
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| {
            error!(error = %error, "failed to spawn git fetch");
            CrabError::Io(error)
        })?;

    if output.status.success() {
        debug!("git fetch complete");
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    error!(error = %stderr.trim(), "git fetch failed");
    Err(CrabError::Internal(format!(
        "git fetch failed: {}",
        stderr.trim()
    )))
}

fn resolve_ref(git_dir: &Path, ref_name: &str) -> Result<String> {
    let ref_path = git_dir.join(ref_name);
    if ref_path.exists() {
        let oid = fs::read_to_string(&ref_path)
            .map_err(|error| {
                CrabError::Internal(format!(
                    "failed to read ref {}: {error}",
                    ref_path.display()
                ))
            })?
            .trim()
            .to_owned();
        return Ok(oid);
    }

    let output = std::process::Command::new("git")
        .args(["rev-parse", ref_name])
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| {
            error!(error = %error, "failed to spawn git rev-parse");
            CrabError::Io(error)
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Internal(format!(
            "failed to resolve ref '{}': {}",
            ref_name,
            stderr.trim()
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::OverlayWriter;
    use crate::pipeline::MountPipelineBuilder;
    use tokio_util::sync::CancellationToken;

    fn git<const N: usize>(repo: &Path, args: [&str; N]) {
        let _git_env = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn git_in<const N: usize>(dir: &Path, args: [&str; N]) {
        let _git_env = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn git_stdout<const N: usize>(repo: &Path, args: [&str; N]) -> String {
        let _git_env = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn git_dir_stdout<const N: usize>(git_dir: &Path, args: [&str; N]) -> String {
        let _git_env = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let output = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn switch_normalizes_branch_names() {
        assert_eq!(normalized_fetch_ref("main").unwrap(), "refs/heads/main");
        assert_eq!(
            normalized_fetch_ref("refs/tags/v1").unwrap(),
            "refs/tags/v1"
        );
    }

    #[test]
    fn resolve_refresh_head_fetches_remote_tracked_ref_into_mount_cache() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let remote = dir.path().join("remote.git");
        let mount_git = dir.path().join("mount.git");

        std::fs::create_dir_all(&source).unwrap();
        git(&source, ["init", "-b", "main"]);
        git(
            &source,
            ["config", "user.email", "mount-runtime-test@crab.local"],
        );
        git(&source, ["config", "user.name", "mount runtime test"]);
        std::fs::write(source.join("file.txt"), "base").unwrap();
        git(&source, ["add", "file.txt"]);
        git(&source, ["commit", "-m", "base"]);
        git_in(
            dir.path(),
            [
                "clone",
                "--bare",
                source.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        git_in(
            dir.path(),
            [
                "clone",
                "--bare",
                "--filter=blob:none",
                remote.to_str().unwrap(),
                mount_git.to_str().unwrap(),
            ],
        );

        let old_head = git_dir_stdout(&mount_git, ["rev-parse", "refs/heads/main"]);
        std::fs::write(source.join("file.txt"), "updated").unwrap();
        git(&source, ["add", "file.txt"]);
        git(&source, ["commit", "-m", "update"]);
        git(&source, ["push", remote.to_str().unwrap(), "main"]);

        let (head_oid, head_ref) =
            resolve_refresh_head(&mount_git, "crab://bucket/repo", Some("refs/heads/main"))
                .unwrap();
        let new_head = git_stdout(&source, ["rev-parse", "HEAD"]);

        assert_ne!(old_head, new_head);
        assert_eq!(head_ref, "refs/heads/main");
        assert_eq!(head_oid, new_head);
        assert_eq!(
            git_dir_stdout(&mount_git, ["rev-parse", "refs/heads/main"]),
            new_head
        );
    }

    #[tokio::test]
    async fn committed_generations_are_adopted_before_pruning_live_resolver() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&source).unwrap();
        git(&source, ["init", "-b", "main"]);
        git(
            &source,
            ["config", "user.email", "mount-runtime-test@crab.local"],
        );
        git(&source, ["config", "user.name", "mount runtime test"]);
        std::fs::write(source.join("base.txt"), "base").unwrap();
        git(&source, ["add", "base.txt"]);
        git(&source, ["commit", "-m", "base"]);

        let config = PipelineConfig {
            source: source.display().to_string(),
            git_dir: source.join(".git"),
            ref_name: Some("refs/heads/main".to_owned()),
            read_only: false,
            cache_dir: cache.clone(),
            cancel_token: CancellationToken::new(),
        };
        let mut output = MountPipelineBuilder::new(config.clone()).execute().unwrap();
        let overlay = std::sync::Arc::clone(output.overlay.as_ref().unwrap());

        for (path, content, message) in [
            ("first.txt", b"first".as_slice(), "first mounted commit"),
            ("second.txt", b"second".as_slice(), "second mounted commit"),
        ] {
            overlay.create_file(path, 0o100644).unwrap();
            overlay.write_file(path, 0, content).unwrap();
            let engine = std::sync::Arc::clone(&output.engine);
            let _reset = engine.begin_overlay_reset().await;
            let result = crate::publish::commit_overlay_with_snapshot(
                &crate::publish::OverlayCommitOptions {
                    cache_dir: cache.clone(),
                    git_dir: config.git_dir.clone(),
                    ref_name: "refs/heads/main".to_owned(),
                    message: message.to_owned(),
                    push: false,
                },
                Some(output.snapshot.as_ref()),
            )
            .unwrap();
            let commit_oid = result.commit_oid.as_deref().unwrap();
            adopt_published_snapshot(&mut output, &config.git_dir, commit_oid, "refs/heads/main")
                .unwrap();
        }

        assert_eq!(output.resolver.generation(), 3);
        assert!(output.snapshot.get_node(1, "base.txt").unwrap().is_none());
        let names = output
            .resolver
            .readdir("")
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["base.txt", "first.txt", "second.txt"]);
    }
}
