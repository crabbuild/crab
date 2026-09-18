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

pub(crate) async fn finalize_capsule_push(
    session: &ProtectedPushSession,
    store: &Store,
    router: &StoreLayout,
    transaction: &crab_metadata::capsule_protocol::CapsuleTransaction,
    capsule: &crab_metadata::capsule_protocol::Capsule,
    upload_concurrency: usize,
    cancel: &CancellationToken,
) -> Result<Option<crab_coordination::write_coordinator::CommitOutcome>> {
    crate::core::error::check_cancelled(cancel)?;
    let run = crab_metadata::capsule_protocol::CapsuleRun::leaf(capsule.clone())?;
    let run_path = router.capsule_path(run.hash());
    store.put(&run_path, run.bytes().clone()).await?;
    let staged_objects = store.flush_staged_writes(upload_concurrency).await?;
    let plan = crab_remote::protected::ProtectedCapsulePushPlan {
        schema_version: crab_remote::protected::PROTECTED_CAPSULE_PUSH_PLAN_SCHEMA_VERSION,
        repo_prefix: router.repo_prefix().to_owned(),
        push_id: session.push_id.clone(),
        upload_prefix: session.upload_prefix.clone(),
        base_root_digest: transaction.base_root_digest().to_owned(),
        transaction_id: transaction.id()?,
        run_hash: run.hash().to_owned(),
        run_size: run.bytes().len() as u64,
        ref_updates: session.ref_updates.clone(),
        staged_objects,
    };
    let plan_bytes = serde_json::to_vec_pretty(&plan).map_err(|error| {
        CrabError::Internal(format!("protected capsule plan serialize: {error}"))
    })?;
    let plan_digest = blake3::hash(&plan_bytes).to_hex().to_string();
    let plan_size = plan_bytes.len() as u64;
    let upload_prefix = session.upload_prefix.trim_matches('/');
    let plan_path = object_store::path::Path::from(format!("{upload_prefix}/push-plan.json"));
    store
        .put_exact(&plan_path, bytes::Bytes::from(plan_bytes))
        .await?;
    store.flush_staging_object(&plan_path, plan_size).await?;
    crate::core::error::check_cancelled(cancel)?;

    let response = match &session.backend {
        ProtectedPushBackend::CrabAuth {
            auth,
            bucket,
            prefix,
            active_active_replication,
        } => {
            auth.finalize_push(
                bucket,
                prefix,
                session.ref_updates.clone(),
                &session.push_id,
                active_active_replication.clone(),
                session.active_active_writer.clone(),
            )
            .await?
        }
        ProtectedPushBackend::Managed {
            token_cache_directory,
            repository,
            push_id,
            request,
        } => {
            crab_auth_store::ManagedRepositoryResolver::new(token_cache_directory.clone())
                .finalize_push(repository, *push_id, request, cancel)
                .await?
        }
    };
    if response.ref_updates != session.ref_updates {
        return Err(CrabError::AuthFailed {
            path: "protected capsule finalize returned mismatched ref updates".to_owned(),
        });
    }
    let outcome =
        protected_capsule_commit_outcome(&response, session.active_active_writer.as_deref())?;
    tracing::info!(
        push_id = %session.push_id,
        plan_digest,
        status = %response.status,
        "protected capsule push finalized"
    );
    Ok(outcome)
}

fn protected_capsule_commit_outcome(
    response: &crab_auth::PushFinalizeResponse,
    writer: Option<&str>,
) -> Result<Option<crab_coordination::write_coordinator::CommitOutcome>> {
    let fields = [
        response.operation_id.is_some(),
        response.coordinator_epoch.is_some(),
        response.writer_region.is_some(),
        response.manifest_generation.is_some(),
        response.commit_state.is_some(),
    ];
    if !fields.iter().any(|field| *field) {
        return Ok(None);
    }
    if !fields.iter().all(|field| *field) {
        return Err(CrabError::AuthFailed {
            path: "protected capsule finalize returned partial active-active metadata".to_owned(),
        });
    }
    Ok(Some(
        crab_coordination::write_coordinator::CommitOutcome {
            operation_id: response.operation_id.clone().ok_or_else(|| CrabError::AuthFailed {
                path: "protected capsule finalize omitted operation ID".to_owned(),
            })?,
            coordinator_epoch: response.coordinator_epoch.ok_or_else(|| CrabError::AuthFailed {
                path: "protected capsule finalize omitted coordinator epoch".to_owned(),
            })?,
            writer: writer
                .ok_or_else(|| CrabError::AuthFailed {
                    path: "protected capsule finalize returned coordinator metadata without a selected writer".to_owned(),
                })?
                .to_owned(),
            region: response.writer_region.clone().ok_or_else(|| CrabError::AuthFailed {
                path: "protected capsule finalize omitted writer region".to_owned(),
            })?,
            manifest_generation: response.manifest_generation.ok_or_else(|| CrabError::AuthFailed {
                path: "protected capsule finalize omitted manifest generation".to_owned(),
            })?,
            commit_sequence: 0,
            state: response.commit_state.ok_or_else(|| CrabError::AuthFailed {
                path: "protected capsule finalize omitted commit state".to_owned(),
            })?,
        },
    ))
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
    let read_store =
        crate::auth::build_repository_url_store(config, parsed_url, "fetch", cancel).await?;
    protected_push_ref_updates_from_store(&read_store, &parsed_url.repo_path, specs, cancel).await
}

async fn protected_push_ref_updates_from_store(
    read_store: &Store,
    repository_prefix: &str,
    specs: &[PushSpec],
    cancel: &CancellationToken,
) -> Result<Vec<PushRefUpdate>> {
    crate::core::error::check_cancelled(cancel)?;
    let requested_refs = specs
        .iter()
        .map(|spec| spec.dst.clone())
        .collect::<BTreeSet<_>>();
    let remote_refs = protected_remote_refs(read_store, repository_prefix, &requested_refs).await?;

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

async fn protected_remote_refs(
    read_store: &Store,
    repository_prefix: &str,
    ref_names: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>> {
    let router = StoreLayout::new(read_store.clone(), repository_prefix.to_owned());
    let layout = crab_storage::StoreLayout::new(
        read_store.as_storage().clone(),
        repository_prefix.to_owned(),
    );
    match crab_metadata::capsule_protocol::load_root(&layout).await {
        Ok(root) => {
            return crab_read::capsule_protocol::read_visible_refs_from_root_for_refs(
                &layout, &root, ref_names,
            )
            .await
            .map_err(CrabError::from);
        }
        Err(crab_metadata::error::MetadataError::Storage {
            source: crab_storage::StorageError::NotFound { .. },
        }) => {}
        Err(error) => return Err(error.into()),
    }
    match crate::metadata::manifest::read_repository_snapshot(read_store, &router).await {
        Ok(snapshot) => Ok(snapshot
            .journal
            .refs
            .into_iter()
            .filter(|(ref_name, _)| ref_names.contains(ref_name))
            .collect()),
        Err(CrabError::NotFound { path }) if path == router.manifest_path().as_ref() => {
            Ok(BTreeMap::default())
        }
        Err(error) => Err(error),
    }
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
    use bytes::Bytes;
    use object_store::memory::InMemory;

    async fn create_v1_manifest(store: &Store, prefix: &str, oid: &str) {
        let router = StoreLayout::new(store.clone(), prefix.to_owned());
        crate::core::remote_layout::initialize(store, &router)
            .await
            .unwrap();
        let mut manifest = crate::metadata::manifest::Manifest::default_for_repo("refs/heads/main");
        manifest
            .refs
            .insert("refs/heads/main".to_owned(), oid.to_owned());
        manifest.seal_git_validation();
        crate::metadata::manifest::create_manifest(store, &router, &manifest)
            .await
            .unwrap();
    }

    async fn publish_v2_ref(
        storage: &crab_storage::Store,
        prefix: &str,
        oid: &str,
    ) -> object_store::path::Path {
        let layout = crab_storage::StoreLayout::new(storage.clone(), prefix.to_owned());
        let root =
            crab_write::capsule_protocol::initialize(&layout, &"a".repeat(64), "refs/heads/main")
                .await
                .unwrap();
        let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::new(
            root.record().digest(),
            vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some(oid.to_owned()),
                None,
            )],
        )
        .unwrap();
        let capsule =
            crab_metadata::capsule_protocol::Capsule::build(&transaction, Vec::new(), Vec::new())
                .unwrap();
        let run = crab_metadata::capsule_protocol::CapsuleRun::leaf(capsule.clone()).unwrap();
        crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
            .await
            .unwrap();
        layout.capsule_path(run.hash())
    }

    #[test]
    fn admission_plan_conservatively_accounts_for_payload_and_object_overhead() {
        let plan = build_admission_plan(2_000_000, 32, 2, 4_000, 6, 1);

        assert_eq!(plan.estimated_objects, 58);
        assert_eq!(plan.estimated_bytes, 2_128_928);
    }

    #[test]
    fn protected_capsule_commit_outcome_preserves_coordinator_metadata() {
        let response = crab_auth::PushFinalizeResponse {
            status: "updated".to_owned(),
            ref_updates: Vec::new(),
            operation_id: Some("operation".to_owned()),
            coordinator_epoch: Some(7),
            writer_region: Some("us-west-2".to_owned()),
            manifest_generation: Some(0),
            commit_state: Some(
                crab_coordination::write_coordinator::PushTransactionState::Materialized,
            ),
        };

        let outcome = protected_capsule_commit_outcome(&response, Some("west"))
            .unwrap()
            .unwrap();

        assert_eq!(outcome.operation_id, "operation");
        assert_eq!(outcome.coordinator_epoch, 7);
        assert_eq!(outcome.writer, "west");
        assert_eq!(outcome.region, "us-west-2");
        assert_eq!(outcome.manifest_generation, 0);
        assert_eq!(
            outcome.state,
            crab_coordination::write_coordinator::PushTransactionState::Materialized
        );
    }

    #[test]
    fn protected_capsule_commit_outcome_rejects_partial_metadata() {
        let response = crab_auth::PushFinalizeResponse {
            status: "updated".to_owned(),
            ref_updates: Vec::new(),
            operation_id: Some("operation".to_owned()),
            coordinator_epoch: None,
            writer_region: None,
            manifest_generation: None,
            commit_state: None,
        };

        let error = protected_capsule_commit_outcome(&response, Some("west")).unwrap_err();

        assert!(matches!(error, CrabError::AuthFailed { .. }));
    }

    #[tokio::test]
    async fn protected_remote_refs_reads_capsule_protocol_heads() {
        let storage = crab_storage::Store::new(Arc::new(InMemory::new()));
        publish_v2_ref(&storage, "org/repo", &"1".repeat(40)).await;
        let store = Store::from_storage(storage);

        let refs = protected_remote_refs(
            &store,
            "org/repo",
            &BTreeSet::from(["refs/heads/main".to_owned()]),
        )
        .await
        .unwrap();

        assert_eq!(refs.get("refs/heads/main"), Some(&"1".repeat(40)));
    }

    #[tokio::test]
    async fn protected_remote_refs_falls_back_only_when_v2_root_is_absent() {
        let storage = crab_storage::Store::new(Arc::new(InMemory::new()));
        let store = Store::from_storage(storage);
        create_v1_manifest(&store, "org/repo", &"2".repeat(40)).await;

        let refs = protected_remote_refs(
            &store,
            "org/repo",
            &BTreeSet::from(["refs/heads/main".to_owned()]),
        )
        .await
        .unwrap();

        assert_eq!(refs.get("refs/heads/main"), Some(&"2".repeat(40)));
    }

    #[tokio::test]
    async fn protected_remote_refs_prefers_v2_and_skips_capsule_payloads() {
        let counted = Arc::new(crab_storage::test_support::CountingObjectStore::new(
            Arc::new(InMemory::new()),
        ));
        let storage =
            crab_storage::Store::new(Arc::clone(&counted) as Arc<dyn object_store::ObjectStore>);
        let store = Store::from_storage(storage.clone());
        let capsule_path = publish_v2_ref(&storage, "org/repo", &"1".repeat(40)).await;
        create_v1_manifest(&store, "org/repo", &"2".repeat(40)).await;
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        counted.reset();

        let refs = protected_remote_refs(
            &store,
            "org/repo",
            &BTreeSet::from(["refs/heads/main".to_owned()]),
        )
        .await
        .unwrap();

        assert_eq!(refs.get("refs/heads/main"), Some(&"1".repeat(40)));
        let requests = counted.requests();
        assert!(
            requests
                .iter()
                .all(|request| request.location != router.manifest_path().as_ref())
        );
        assert!(
            requests
                .iter()
                .all(|request| request.location != capsule_path.as_ref())
        );
    }

    #[tokio::test]
    async fn protected_remote_refs_does_not_fall_back_from_corrupt_v2() {
        let storage = crab_storage::Store::new(Arc::new(InMemory::new()));
        let store = Store::from_storage(storage.clone());
        create_v1_manifest(&store, "org/repo", &"2".repeat(40)).await;
        let layout = crab_storage::StoreLayout::new(storage.clone(), "org/repo".to_owned());
        storage
            .put(
                &layout.capsule_root_path(),
                Bytes::from_static(b"not a capsule root"),
            )
            .await
            .unwrap();

        assert!(matches!(
            protected_remote_refs(
                &store,
                "org/repo",
                &BTreeSet::from(["refs/heads/main".to_owned()]),
            )
            .await,
            Err(CrabError::CorruptObject { .. })
        ));
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
