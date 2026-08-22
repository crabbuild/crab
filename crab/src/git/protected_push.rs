//! Shared Crab Auth protected-push preparation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use crab_auth::PushRefUpdate;
use crab_auth::managed::{
    IdempotencyKey, PushAdmissionPlan, PushFinalizeRequest, PushReplicationRequest,
};
use crab_coordination::active_active::ActiveActiveReplicationConfig;
use crab_git::ManagedRepository;
use tokio_util::sync::CancellationToken;

use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::git::push::{ProtectedPushBackend, ProtectedPushSession};
use crate::git::remote_helper::PushSpec;
use crate::git::url::CrabUrl;
use crate::storage::StoreLayout;
use crate::storage::store::Store;

pub(crate) struct PreparedProtectedPush {
    pub store: Store,
    pub session: ProtectedPushSession,
}

pub(crate) async fn prepare_crab_auth_push(
    config: &Config,
    parsed_url: &CrabUrl,
    specs: &[PushSpec],
    cancel: &CancellationToken,
) -> Result<PreparedProtectedPush> {
    let ref_updates = protected_push_ref_updates(config, parsed_url, specs, cancel).await?;
    let auth = Arc::new(crab_auth::create_crab_auth_provider(
        crate::auth::crab_auth_client_config(&config.auth)?,
    )?);
    let (active_active_replication, active_active_writer) =
        protected_push_active_active_context(config, parsed_url)?;
    let prepared = auth
        .prepare_push(
            &parsed_url.bucket,
            &parsed_url.repo_path,
            ref_updates.clone(),
        )
        .await?;
    let store = crate::auth::build_protected_push_store(
        &parsed_url.bucket,
        prepared.credentials,
        &prepared.upload_prefix,
    )?;
    let session = ProtectedPushSession {
        ref_updates,
        push_id: prepared.push_id,
        upload_prefix: prepared.upload_prefix,
        active_active_writer,
        backend: ProtectedPushBackend::CrabAuth {
            auth,
            bucket: parsed_url.bucket.clone(),
            prefix: parsed_url.repo_path.clone(),
            active_active_replication,
        },
    };
    Ok(PreparedProtectedPush { store, session })
}

pub(crate) async fn prepare_managed_push(
    config: &Config,
    repository: &ManagedRepository,
    read_store: &Store,
    repository_prefix: &str,
    specs: &[PushSpec],
    staging: Option<&Arc<crab_staging::StagingAreaReadOnly>>,
    cancel: &CancellationToken,
) -> Result<PreparedProtectedPush> {
    tracing::info!(repository = %repository.canonical_url(), "preparing managed push refs");
    let ref_updates =
        protected_push_ref_updates_from_store(read_store, repository_prefix, specs, cancel).await?;
    let (active_active_replication, active_active_writer) = protected_push_active_active_context(
        config,
        &CrabUrl {
            bucket: repository.authority.clone(),
            repo_path: format!("{}/{}", repository.organization, repository.repository),
        },
    )?;
    let replication = match (&active_active_replication, &active_active_writer) {
        (Some(configuration), Some(writer)) => Some(PushReplicationRequest::ActiveActive {
            writer: writer.clone(),
            configuration: serde_json::to_value(configuration).map_err(|error| {
                CrabError::Internal(format!("serialize managed push replication: {error}"))
            })?,
        }),
        (None, None) => None,
        _ => {
            return Err(CrabError::Internal(
                "managed active-active push context is incomplete".to_owned(),
            ));
        }
    };
    let plan = estimate_managed_push_plan(staging, &ref_updates)?;
    let client_version = env!("CARGO_PKG_VERSION").to_owned();
    let request_digest = blake3::hash(
        &serde_json::to_vec(&(&ref_updates, &plan, &client_version, &replication)).map_err(
            |error| CrabError::Internal(format!("serialize managed push admission: {error}")),
        )?,
    )
    .to_hex()
    .to_string();
    let idempotency_key = IdempotencyKey::new(format!("crab-push-{}", &request_digest[..32]))
        .map_err(|error| CrabError::Internal(format!("construct managed push key: {error}")))?;
    let token_cache_directory =
        crab_auth::token_cache::expand_token_cache_path(&config.auth.token_cache_path);
    tracing::info!(repository = %repository.canonical_url(), "requesting managed push session");
    let managed = crab_auth_store::ManagedRepositoryResolver::new(token_cache_directory.clone())
        .prepare_push_for_updates(
            repository,
            ref_updates.clone(),
            plan,
            client_version,
            replication,
            &idempotency_key,
            cancel,
        )
        .await?;
    tracing::info!(
        repository = %repository.canonical_url(),
        push_id = %managed.prepared.push_id,
        "managed push session prepared"
    );
    let staging = managed
        .prepared
        .staging_grant
        .storage_scope
        .staging
        .as_ref()
        .ok_or_else(|| {
            CrabError::Internal("managed push grant omitted its staging scope".to_owned())
        })?;
    let push_id = managed.prepared.push_id;
    let upload_prefix = staging.prefix.clone();
    let request = PushFinalizeRequest {
        schema_version: managed.request.schema_version,
        repository_id: managed.request.repository_id,
        ref_updates: managed.request.ref_updates,
        plan: managed.request.plan,
        client_version: managed.request.client_version,
        replication: managed.request.replication,
    };
    Ok(PreparedProtectedPush {
        store: Store::from_storage(managed.store.store),
        session: ProtectedPushSession {
            ref_updates,
            push_id: push_id.simple().to_string(),
            upload_prefix,
            active_active_writer,
            backend: ProtectedPushBackend::Managed {
                token_cache_directory,
                repository: repository.clone(),
                push_id,
                request,
            },
        },
    })
}

fn estimate_managed_push_plan(
    staging: Option<&Arc<crab_staging::StagingAreaReadOnly>>,
    ref_updates: &[PushRefUpdate],
) -> Result<PushAdmissionPlan> {
    let (staging_bytes, staging_chunks, staging_files) =
        match staging {
            Some(staging) => staging.list_files()?.iter().fold(
                (0u64, 0u64, 0u64),
                |(bytes, chunks, files), file| {
                    (
                        bytes.saturating_add(file.total_bytes),
                        chunks
                            .saturating_add(file.committed_chunks)
                            .saturating_add(file.pending_chunks),
                        files.saturating_add(1),
                    )
                },
            ),
            None => (0, 0, 0),
        };
    let repository = std::env::current_dir().map_err(CrabError::Io)?;
    let (git_bytes, git_objects) = estimate_git_object_delta(&repository, ref_updates)?;
    Ok(build_admission_plan(
        staging_bytes,
        staging_chunks,
        staging_files,
        git_bytes,
        git_objects,
        ref_updates.len() as u64,
    ))
}

fn build_admission_plan(
    staging_bytes: u64,
    staging_chunks: u64,
    staging_files: u64,
    git_bytes: u64,
    git_objects: u64,
    ref_updates: u64,
) -> PushAdmissionPlan {
    const FIXED_CONTROL_BYTES: u64 = 64 * 1024;
    const PER_OBJECT_OVERHEAD_BYTES: u64 = 1024;
    const FIXED_CONTROL_OBJECTS: u64 = 8;

    let estimated_objects = staging_chunks
        .saturating_add(staging_files.saturating_mul(4))
        .saturating_add(git_objects)
        .saturating_add(ref_updates.saturating_mul(4))
        .saturating_add(FIXED_CONTROL_OBJECTS)
        .max(1);
    let estimated_bytes = staging_bytes
        .saturating_add(git_bytes)
        .saturating_add(FIXED_CONTROL_BYTES)
        .saturating_add(estimated_objects.saturating_mul(PER_OBJECT_OVERHEAD_BYTES))
        .max(1);
    PushAdmissionPlan {
        estimated_bytes,
        estimated_objects,
    }
}

fn estimate_git_object_delta(
    repository: &Path,
    ref_updates: &[PushRefUpdate],
) -> Result<(u64, u64)> {
    let tips = ref_updates
        .iter()
        .map(|update| update.new_oid.clone())
        .collect::<Vec<_>>();
    let excluded_tips = ref_updates
        .iter()
        .filter_map(|update| update.old_oid.as_deref())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    super::pack::estimate_reachable_object_bytes(Some(repository), None, &tips, &excluded_tips)
}

fn protected_push_active_active_context(
    config: &Config,
    parsed_url: &CrabUrl,
) -> Result<(Option<ActiveActiveReplicationConfig>, Option<String>)> {
    let Some(replication) = config
        .replication
        .as_ref()
        .filter(|replication| replication.is_active_active())
    else {
        return Ok((None, None));
    };

    crate::replication::validate_active_active_config(replication)?;
    let remote_url = format!("crab://{}/{}", parsed_url.bucket, parsed_url.repo_path);
    let writer =
        crate::replication::active_active_writer_name_for_remote(replication, Some(&remote_url))?;
    Ok((
        Some(crate::replication::active_active_coordination_config(
            replication,
        )),
        Some(writer),
    ))
}

async fn protected_push_ref_updates(
    config: &Config,
    parsed_url: &CrabUrl,
    specs: &[PushSpec],
    cancel: &CancellationToken,
) -> Result<Vec<PushRefUpdate>> {
    let read_store = crate::auth::build_store(config, parsed_url, "fetch", cancel).await?;
    protected_push_ref_updates_from_store(&read_store, &parsed_url.repo_path, specs, cancel).await
}

async fn protected_push_ref_updates_from_store(
    read_store: &Store,
    repository_prefix: &str,
    specs: &[PushSpec],
    cancel: &CancellationToken,
) -> Result<Vec<PushRefUpdate>> {
    crate::core::error::check_cancelled(cancel)?;
    let router = StoreLayout::new(read_store.clone(), repository_prefix.to_owned());
    let remote_refs =
        match crate::metadata::manifest::read_repository_snapshot(&read_store, &router).await {
            Ok(snapshot) => snapshot.journal.refs,
            Err(CrabError::NotFound { .. }) => BTreeMap::default(),
            Err(e) => return Err(e),
        };

    let mut seen = BTreeSet::new();
    let mut updates = Vec::with_capacity(specs.len());
    for spec in specs {
        if spec.src.is_empty() {
            return Err(CrabError::AuthFailed {
                path: "crab-auth protected push denies ref deletion until delete-ref policy exists"
                    .into(),
            });
        }
        if !seen.insert(spec.dst.as_str()) {
            return Err(CrabError::AuthFailed {
                path: format!(
                    "crab-auth protected push has duplicate destination {}",
                    spec.dst
                ),
            });
        }
        let new_oid = resolve_rev(&spec.src).ok_or_else(|| CrabError::Configuration {
            key: format!("could not resolve push source ref {}", spec.src),
            origin: "git rev-parse".into(),
        })?;
        updates.push(PushRefUpdate {
            ref_name: spec.dst.clone(),
            old_oid: remote_refs.get(&spec.dst).cloned(),
            new_oid,
        });
    }
    Ok(updates)
}

fn resolve_rev(refspec: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", refspec])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if sha.is_empty() { None } else { Some(sha) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_plan_conservatively_accounts_for_payload_and_object_overhead() {
        let plan = build_admission_plan(2_000_000, 32, 2, 4_000, 6, 1);

        assert_eq!(plan.estimated_objects, 58);
        assert_eq!(plan.estimated_bytes, 2_128_928);
    }

    #[test]
    fn git_object_delta_excludes_the_old_reachable_history() {
        let _guard = crate::test::git_repo::CleanGitEnvGuard::new();
        let repository = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-b", "main"][..],
            &["config", "user.name", "Crab test"],
            &["config", "user.email", "crab@example.invalid"],
        ] {
            assert!(
                Command::new("git")
                    .current_dir(repository.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(repository.path().join("model.txt"), "first").unwrap();
        assert!(
            Command::new("git")
                .current_dir(repository.path())
                .args(["add", "model.txt"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .current_dir(repository.path())
                .args(["commit", "-m", "first"])
                .status()
                .unwrap()
                .success()
        );
        let old_oid = resolve_in(repository.path(), "HEAD");
        std::fs::write(repository.path().join("model.txt"), "second version").unwrap();
        assert!(
            Command::new("git")
                .current_dir(repository.path())
                .args(["commit", "-am", "second"])
                .status()
                .unwrap()
                .success()
        );
        let new_oid = resolve_in(repository.path(), "HEAD");
        let (bytes, objects) = estimate_git_object_delta(
            repository.path(),
            &[PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some(old_oid),
                new_oid,
            }],
        )
        .unwrap();

        assert!(bytes > 0);
        assert_eq!(objects, 3);
    }

    fn resolve_in(repository: &Path, revision: &str) -> String {
        let output = Command::new("git")
            .current_dir(repository)
            .args(["rev-parse", revision])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}
