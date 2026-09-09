use std::{
    collections::BTreeMap,
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    time::Duration,
};

use crab_remote::publication::{self, CommitOutcome};
use tokio_util::sync::CancellationToken;

use super::{Client, RemoteRepository};
use crate::{
    CommitReceipt, Error, ErrorKind, MutationOutcome, OperationOptions, Readiness, RecoveryToken,
    RefBatch, RefRejection, RepositoryLocator, Result, WritePolicy,
};

mod commit;
mod failure;
pub(crate) mod native_content;
use failure::Failure;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod initialize_tests;

const LEASE_TTL: Duration = Duration::from_secs(60);

/// Single-use validated mutation and its owned preparation files.
#[must_use]
pub struct PreparedMutation {
    client: Client,
    token: RecoveryToken,
    prepared: Option<crab_remote::prepare::Prepared>,
    directories: Vec<tempfile::TempDir>,
    commit: Option<crate::ObjectId>,
}

impl PreparedMutation {
    /// Return the complete recovery token to persist before attempting execution.
    #[must_use]
    pub fn recovery_token(&self) -> &RecoveryToken {
        &self.token
    }

    /// Return the created commit identity for a prepared commit mutation.
    #[must_use]
    pub const fn commit_id(&self) -> Option<crate::ObjectId> {
        self.commit
    }

    /// Execute once under operation/ref leases and both GC writer fences.
    ///
    /// Dropping the future cancels and drains the worker; retain the token to
    /// recover its outcome. Known commitment survives deadlines and cleanup.
    pub async fn execute(self, options: OperationOptions) -> Result<MutationOutcome> {
        let Self {
            client,
            token,
            prepared,
            directories,
            commit: _,
        } = self;
        #[cfg(feature = "managed")]
        if token.managed_finalize().is_some() {
            return execute_managed(client, token, options).await;
        }
        let prepared = prepared.ok_or_else(|| {
            Error::new(
                ErrorKind::Corruption,
                "direct prepared mutation lost its artifacts",
            )
        })?;
        execute_mutation(
            client,
            token,
            ExecutionInput::Prepared(Box::new((prepared, directories))),
            options,
        )
        .await
    }
}

enum ExecutionInput {
    Prepared(Box<(crab_remote::prepare::Prepared, Vec<tempfile::TempDir>)>),
    Resume(PathBuf),
}

async fn execute_mutation(
    client: Client,
    token: RecoveryToken,
    input: ExecutionInput,
    options: OperationOptions,
) -> Result<MutationOutcome> {
    let limits = options.read_limits();
    let readiness_options = options.clone();
    let caller = client.clone();
    caller
            .0
            .operations
            .run_mutation(options, move |cancel| async move {
                let locator = token.locator()?;
                let layout = crab_storage::StoreLayout::new(
                    client.0.store.clone(),
                    locator.direct_prefix()?.to_owned(),
                );
                let result = publication::with_plan(
                    &client.0.store,
                    &layout,
                    token.plan_id(),
                    LEASE_TTL,
                    &cancel,
                    |cancel| {
                        let client = &client;
                        let layout = &layout;
                        let token = &token;
                        let readiness_options = &readiness_options;
                        async move {
                            let (prepared, directories) = match input {
                                ExecutionInput::Prepared(files) => *files,
                                ExecutionInput::Resume(scratch) => {
                                    let (prepared, directory) = prepare_saved_batch(
                                        client,
                                        token,
                                        scratch,
                                        readiness_options,
                                        &cancel,
                                    )
                                    .await?;
                                    (prepared, vec![directory])
                                }
                            };
                            let names = token
                                .batch()
                                .edits()
                                .iter()
                                .map(|edit| edit.name().to_owned());
                            let result = publication::with_leases(
                                &client.0.store,
                                layout,
                                names,
                                LEASE_TTL,
                                &cancel,
                                |holders, cancel| async move {
                                    // Snapshot reads precede any publication attempt and own no
                                    // sessions. Cancel them inside the scopes that drain leases.
                                    let snapshot = tokio::select! {
                                        biased;
                                        () = cancel.cancelled() => return Err(Failure::Publication(publication::Error::Cancelled)),
                                        result = crab_metadata::manifest_store::read_repository_snapshot(&client.0.store, layout) => result?,
                                    };
                                    let artifacts = prepared
                                        .upload(
                                            &snapshot,
                                            dependency_limits(limits),
                                            &holders,
                                            &cancel,
                                        )
                                        .await?;
                                    // HEAD may remain unborn for a tag-only repository. Select only
                                    // a branch when its former symbolic target was removed.
                                    let refs = prepared.plan().refs();
                                    let head = if refs.is_empty()
                                        || refs.contains_key(&snapshot.manifest.head)
                                    {
                                        None
                                    } else {
                                        refs.keys()
                                            .find(|name| name.starts_with("refs/heads/"))
                                            .cloned()
                                    };
                                    let options =
                                        crab_write::journal::CommitOptions::new(LEASE_TTL, &cancel)
                                            .with_plan(token.plan_id());
                                    let outcome = artifacts.commit(head, options).await?;
                                    let outcome = match outcome {
                                        CommitOutcome::Indeterminate { .. } => {
                                            MutationOutcome::Indeterminate {
                                                recovery: token.clone(),
                                            }
                                        }
                                        CommitOutcome::Committed(committed) => {
                                            let readiness =
                                                readiness(client, layout, &locator, readiness_options, &cancel).await;
                                            MutationOutcome::Committed {
                                                receipt: CommitReceipt {
                                                    recovery: token.clone(),
                                                    transaction_id: committed.transaction_id,
                                                },
                                                readiness,
                                            }
                                        }
                                    };
                                    Ok::<_, Failure>(outcome)
                                },
                            )
                            .await;
                            // Preparation files outlive artifact borrows and ref/GC lease cleanup.
                            drop(directories);
                            result
                        }
                    },
                )
                .await;
                match result {
                    Ok(outcome) => Ok(outcome),
                    Err(Failure::Metadata(
                        crab_metadata::error::MetadataError::PlanAlreadyAttempted { .. },
                    )) => reconcile(&client, token, &readiness_options, &cancel).await,
                    Err(failure) if failure.rejected() => Ok(MutationOutcome::Rejected {
                        reasons: vec![RefRejection {
                            source: failure.into(),
                        }],
                    }),
                    Err(failure) => Err(failure.into()),
                }
            })
            .await
}

#[cfg(feature = "managed")]
async fn execute_managed(
    client: Client,
    token: RecoveryToken,
    options: OperationOptions,
) -> Result<MutationOutcome> {
    let locator = token.locator()?;
    let repository = locator.managed_owner().ok_or_else(|| {
        Error::new(
            ErrorKind::Corruption,
            "managed recovery token lost its repository identity",
        )
    })?;
    let (push_id, finalize) = token.managed_finalize().ok_or_else(|| {
        Error::new(
            ErrorKind::Corruption,
            "managed recovery token lost its finalize request",
        )
    })?;
    let finalize = finalize.clone();
    let managed = client.0.managed.clone().ok_or_else(|| {
        Error::new(
            ErrorKind::Authentication,
            "managed service options are required to finalize this mutation",
        )
    })?;
    let expected = finalize.ref_updates.clone();
    let recovery = token.clone();
    client
        .0
        .operations
        .run_mutation(options, move |cancel| async move {
            let resolved = managed
                .resolver
                .resolve(
                    &repository,
                    crab_auth::managed::TransferOperation::Fetch,
                    &cancel,
                )
                .await
                .map_err(crate::managed::managed_repository_error)?;
            if resolved.store.target_identity() != Some(recovery.placement()) {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "managed repository placement changed after preparation",
                ));
            }
            match managed
                .resolver
                .finalize_push(&repository, push_id, &finalize, &cancel)
                .await
            {
                Ok(response) => {
                    if response.status != "updated" || response.ref_updates != expected {
                        return Err(Error::new(
                            ErrorKind::Corruption,
                            "managed finalize returned a mismatched publication result",
                        ));
                    }
                    Ok(MutationOutcome::Committed {
                        receipt: CommitReceipt {
                            recovery,
                            transaction_id: push_id.simple().to_string(),
                        },
                        readiness: response
                            .manifest_generation
                            .map_or(Readiness::Pending, |generation| Readiness::Ready {
                                generation,
                            }),
                    })
                }
                Err(source) => {
                    let error = crate::managed::managed_repository_error(source);
                    match error.kind() {
                        ErrorKind::Transport | ErrorKind::Timeout | ErrorKind::Cancelled => {
                            Ok(MutationOutcome::Indeterminate { recovery })
                        }
                        ErrorKind::Authentication
                        | ErrorKind::Authorization
                        | ErrorKind::Conflict
                        | ErrorKind::NotFound => Ok(MutationOutcome::Rejected {
                            reasons: vec![RefRejection { source: error }],
                        }),
                        _ => Err(error),
                    }
                }
            }
        })
        .await
}

impl RemoteRepository {
    /// Prepare an atomic ref batch using remote objects and a caller-selected scratch root.
    ///
    /// Creates no checkout or Git object database and invokes no executable.
    /// Ref policy and expected values are checked again under leases at execution.
    pub async fn prepare_ref_update(
        &self,
        batch: RefBatch,
        scratch: PathBuf,
        options: OperationOptions,
    ) -> Result<PreparedMutation> {
        batch.validate()?;
        validate_ref_preparation(&self.client, &self.locator, &scratch)?;
        let client = self.client.clone();
        let locator = self.locator.clone();
        let caller = client.clone();
        caller
            .0
            .operations
            .run(options.clone(), move |cancel| async move {
                let (prepared, directory) = prepare_batch(
                    &client,
                    &locator,
                    &batch,
                    scratch,
                    &options,
                    &cancel,
                    PackInput::Empty,
                    None,
                )
                .await?;
                finish_preparation(
                    FinishPreparation {
                        client,
                        locator,
                        batch,
                        prepared,
                        directories: vec![directory],
                        commit: None,
                        persist_recovery: true,
                    },
                    &options,
                    &cancel,
                )
                .await
            })
            .await
    }

    /// Reconcile historical commitment without writing or inferring success from current refs.
    pub async fn reconcile(
        &self,
        token: RecoveryToken,
        options: OperationOptions,
    ) -> Result<MutationOutcome> {
        if token.locator()? != self.locator {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "recovery token belongs to another repository",
            ));
        }
        self.client.reconcile(token, options).await
    }
}

#[cfg(feature = "local")]
pub(crate) struct LocalPackPreparation {
    pub client: Client,
    pub locator: RepositoryLocator,
    pub batch: RefBatch,
    pub pack: Option<PathBuf>,
    pub content: Option<crab_remote::prepare::PreparedContent>,
    pub owned_directories: Vec<tempfile::TempDir>,
    pub scratch: PathBuf,
    pub persist_recovery: bool,
    pub options: OperationOptions,
}

#[cfg(feature = "local")]
pub(crate) async fn prepare_local_pack_update(
    input: LocalPackPreparation,
    cancel: &CancellationToken,
) -> Result<Option<PreparedMutation>> {
    let LocalPackPreparation {
        client,
        locator,
        batch,
        pack,
        content,
        mut owned_directories,
        scratch,
        persist_recovery,
        options,
    } = input;
    batch.validate()?;
    if persist_recovery {
        validate_ref_preparation(&client, &locator, &scratch)?;
    } else if !scratch.is_absolute() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "scratch root must be absolute",
        ));
    }
    let input = pack.map_or(PackInput::Empty, PackInput::Local);
    let (mut prepared, directory) = prepare_batch(
        &client, &locator, &batch, scratch, &options, cancel, input, None,
    )
    .await?;
    if let Some(content) = content {
        prepared = prepared.attach_content(content).map_err(Failure::from)?;
    }
    owned_directories.push(directory);
    #[cfg(feature = "managed")]
    if !persist_recovery && locator.managed_owner().is_some() {
        // Managed dry-run is local validation only. Preparing a protected
        // service session would reserve admission and create remote objects.
        drop(owned_directories);
        return Ok(None);
    }
    finish_preparation(
        FinishPreparation {
            client,
            locator,
            batch,
            prepared,
            directories: owned_directories,
            commit: None,
            persist_recovery,
        },
        &options,
        cancel,
    )
    .await
    .map(Some)
}

pub(super) struct FinishPreparation {
    client: Client,
    locator: RepositoryLocator,
    batch: RefBatch,
    prepared: crab_remote::prepare::Prepared,
    directories: Vec<tempfile::TempDir>,
    commit: Option<crate::ObjectId>,
    persist_recovery: bool,
}

pub(super) async fn finish_preparation(
    input: FinishPreparation,
    options: &OperationOptions,
    cancel: &CancellationToken,
) -> Result<PreparedMutation> {
    let FinishPreparation {
        client,
        locator,
        batch,
        prepared,
        directories,
        commit,
        persist_recovery,
    } = input;
    #[cfg(not(feature = "managed"))]
    let _ = options;
    #[cfg(feature = "managed")]
    if let Some(repository) = locator.managed_owner() {
        if !persist_recovery {
            return Err(Error::new(
                ErrorKind::UnsupportedCapability,
                "managed dry-run requires the protected service admission API",
            ));
        }
        let managed = client.0.managed.clone().ok_or_else(|| {
            Error::new(
                ErrorKind::Authentication,
                "managed service options are required for protected publication",
            )
        })?;
        let ref_updates = batch
            .edits()
            .iter()
            .map(|edit| {
                let target = edit.target()?.ok_or_else(|| {
                    Error::new(
                        ErrorKind::UnsupportedCapability,
                        "managed protected publication does not support ref deletion",
                    )
                })?;
                Ok(crab_auth::PushRefUpdate {
                    ref_name: edit.name().to_owned(),
                    old_oid: edit.expected()?.map(|oid| oid.to_string()),
                    new_oid: target.to_string(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let (estimated_bytes, estimated_objects) = prepared.admission_estimate();
        let key = crab_auth::managed::IdempotencyKey::new(format!(
            "sdk-push-{}",
            uuid::Uuid::now_v7().simple()
        ))
        .map_err(|source| {
            Error::with_source(
                ErrorKind::Corruption,
                "cannot construct managed push identity",
                source,
            )
        })?;
        let managed_push = managed
            .resolver
            .prepare_push_for_updates(
                &repository,
                ref_updates.clone(),
                crab_auth::managed::PushAdmissionPlan {
                    estimated_bytes,
                    estimated_objects,
                },
                env!("CARGO_PKG_VERSION").to_owned(),
                None,
                &key,
                cancel,
            )
            .await
            .map_err(crate::managed::managed_repository_error)?;
        let staging = managed_push
            .prepared
            .staging_grant
            .storage_scope
            .staging
            .as_ref()
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Corruption,
                    "managed push grant omitted its staging scope",
                )
            })?;
        let push_id = managed_push.prepared.push_id;
        let upload_prefix = staging.prefix.clone();
        let store = managed_push.store.store;
        let prefix = managed_push.store.repository_prefix;
        let layout = crab_storage::StoreLayout::new(store.clone(), prefix);
        let prepared = prepared
            .with_staging_layout(layout.clone())
            .map_err(Failure::from)?;
        let snapshot = crab_metadata::manifest_store::read_repository_snapshot(&store, &layout)
            .await
            .map_err(Failure::from)?;
        if snapshot.manifest.generation != managed_push.prepared.base_manifest_generation
            || snapshot.manifest_etag != managed_push.prepared.base_manifest_etag
        {
            return Err(Error::new(
                ErrorKind::Conflict,
                "managed repository changed during protected preparation",
            ));
        }
        let finalize = crab_auth::managed::PushFinalizeRequest {
            schema_version: managed_push.request.schema_version,
            repository_id: managed_push.request.repository_id,
            ref_updates: managed_push.request.ref_updates,
            plan: managed_push.request.plan,
            client_version: managed_push.request.client_version,
            replication: managed_push.request.replication,
        };
        let pack = prepared.pack_binding();
        let token = RecoveryToken::new_managed(
            *store.target_identity().ok_or_else(|| {
                Error::new(
                    ErrorKind::Corruption,
                    "managed push store has no placement identity",
                )
            })?,
            &locator,
            batch,
            pack.as_ref(),
            prepared.content(),
            commit,
            push_id,
            finalize,
        )?;
        let artifacts = prepared
            .upload(
                &snapshot,
                dependency_limits(options.read_limits()),
                &BTreeMap::new(),
                cancel,
            )
            .await
            .map_err(Failure::from)?;
        artifacts
            .stage_protected_plan(
                &push_id.simple().to_string(),
                &upload_prefix,
                ref_updates,
                cancel,
            )
            .await
            .map_err(Failure::from)?;
        drop(directories);
        return Ok(PreparedMutation {
            client,
            token,
            prepared: None,
            directories: Vec::new(),
            commit,
        });
    }

    let pack = prepared.pack_binding();
    let token = RecoveryToken::new(
        placement(&client)?,
        &locator,
        batch,
        pack.as_ref(),
        prepared.content(),
        commit,
    )?;
    if persist_recovery {
        prepared
            .stage_recovery_artifacts(token.plan_id(), cancel)
            .await
            .map_err(Failure::from)?;
    }
    Ok(PreparedMutation {
        client,
        token,
        prepared: Some(prepared),
        directories,
        commit,
    })
}

fn validate_ref_preparation(
    client: &Client,
    locator: &RepositoryLocator,
    scratch: &Path,
) -> Result<()> {
    if locator.prefix().is_some()
        && client.0.store.bucket_identity().cloud == crab_storage::StorageProviderKind::Local
    {
        // object_store's filesystem backend rejects conditional updates.
        // Ref execution must fail before creating any durable plan or lease.
        return Err(Error::new(
            ErrorKind::UnsupportedCapability,
            "filesystem stores do not support conditional publication",
        ));
    }
    if !scratch.is_absolute() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "scratch root must be absolute",
        ));
    }
    Ok(())
}

async fn prepare_saved_batch(
    client: &Client,
    token: &RecoveryToken,
    scratch: PathBuf,
    options: &OperationOptions,
    cancel: &CancellationToken,
) -> Result<(crab_remote::prepare::Prepared, tempfile::TempDir)> {
    let locator = token.locator()?;
    prepare_batch(
        client,
        &locator,
        token.batch(),
        scratch,
        options,
        cancel,
        token
            .pack()
            .map_or(PackInput::Empty, |(pack_id, size)| PackInput::Staged {
                pack_id: pack_id.to_owned(),
                size,
                plan_id: token.plan_id().to_owned(),
            }),
        token.content(),
    )
    .await
}

async fn prepare_batch(
    client: &Client,
    locator: &RepositoryLocator,
    batch: &RefBatch,
    scratch: PathBuf,
    options: &OperationOptions,
    cancel: &CancellationToken,
    input: PackInput,
    content: Option<&crate::mutation::ContentBinding>,
) -> Result<(crab_remote::prepare::Prepared, tempfile::TempDir)> {
    let bounds = options.read_limits();
    validate_recovery_budget(&input, content, bounds)?;
    let directory = tokio::task::spawn_blocking(move || tempfile::tempdir_in(scratch))
        .await
        .map_err(Failure::from)?
        .map_err(Failure::from)?;
    let resolved = client.0.resolve_repository(locator, cancel).await?;
    let layout = crab_storage::StoreLayout::new(resolved.store.clone(), resolved.prefix.clone());
    let identity = crab_remote_git::RepositoryIdentity::new(
        resolved.namespace,
        resolved.prefix,
        resolved.placement,
    )
    .map_err(crate::remote_error::remote_error)?;
    let repository_options =
        crab_remote_git::RepositoryOptions::new(Default::default(), options.owner_limits())
            .map_err(crate::remote_error::remote_error)?;
    let repository = crab_remote_git::RemoteGitRepository::open(
        resolved.store,
        layout.clone(),
        identity,
        client.0.git.clone(),
        repository_options,
        cancel,
    )
    .await
    .map_err(crate::remote_error::remote_error)?;
    let policy = batch.policy();
    let updates = batch
        .edits()
        .iter()
        .map(crate::RefUpdate::owner)
        .collect::<Result<Vec<_>>>()?;
    let expected_pack = match &input {
        PackInput::Staged { pack_id, size, .. } => Some((pack_id.clone(), *size)),
        PackInput::Empty | PackInput::Local(_) => None,
    };
    let recovery_plan_id = match &input {
        PackInput::Staged { plan_id, .. } => Some(plan_id.clone()),
        PackInput::Empty | PackInput::Local(_) => None,
    };
    let input = match input {
        PackInput::Staged {
            pack_id: _,
            size,
            plan_id,
        } => {
            let path = directory.path().join("recovery.pack");
            let remote_path = layout.ref_journal_recovery_pack_path(&plan_id);
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(Error::new(ErrorKind::Cancelled, "mutation recovery cancelled")),
                result = client.0.store.download_to_path_bounded(&remote_path, &path, size) => {
                    let downloaded = result.map_err(|source| Failure::Prepare(crab_remote::prepare::Error::Storage(source)))?;
                    if downloaded != size {
                        return Err(Error::new(ErrorKind::Corruption, "prepared pack size changed"));
                    }
                }
            }
            let file = tokio::task::spawn_blocking(move || File::open(path))
                .await
                .map_err(Failure::from)?
                .map_err(Failure::from)?;
            Some(BufReader::new(file))
        }
        PackInput::Local(path) => {
            let file = tokio::task::spawn_blocking(move || File::open(path))
                .await
                .map_err(Failure::from)?
                .map_err(Failure::from)?;
            Some(BufReader::new(file))
        }
        PackInput::Empty => None,
    };
    let max_object_bytes = usize::try_from(
        bounds
            .max_inflated_bytes
            .min(crate::mutation::MAX_GIT_OBJECT_BYTES),
    )
    .unwrap_or(usize::MAX);
    let max_objects = u32::try_from(bounds.max_logical_objects).unwrap_or(u32::MAX);
    let mut prepared = crab_remote::prepare::prepare(
        repository,
        directory.path().to_owned(),
        input,
        updates,
        BTreeMap::new(),
        cancel,
        crab_remote::prepare::Options {
            layout: layout.clone(),
            graph: crab_git::receive_plan::GraphLimits {
                max_ref_updates: crate::mutation::MAX_REF_UPDATES,
                max_graph_steps: usize::try_from(bounds.max_logical_objects).unwrap_or(usize::MAX),
                max_object_bytes,
                max_read_bytes: bounds.max_fetched_bytes,
            },
            pack: crab_git::incoming_pack::ReceiveLimits {
                max_pack_bytes: bounds.max_fetched_bytes,
                max_objects,
                max_object_bytes,
                max_inflated_bytes: bounds.max_inflated_bytes,
                max_delta_depth: 128,
            },
            policy: move |_: &str| crab_git::receive_plan::RefPolicy {
                allow_delete: true,
                allow_non_fast_forward: matches!(policy, WritePolicy::ForceWithLease),
            },
        },
    )
    .await
    .map_err(Failure::from)?;
    if let Some((pack_id, size)) = expected_pack {
        let Some(binding) = prepared.pack_binding() else {
            return Err(Error::new(
                ErrorKind::Corruption,
                "prepared pack became empty",
            ));
        };
        if binding.pack_id() != pack_id || binding.size() != size {
            return Err(Error::new(
                ErrorKind::Corruption,
                "prepared pack does not match recovery token",
            ));
        }
    }
    if let Some(content) = content {
        let plan_id = recovery_plan_id.as_deref().ok_or_else(|| {
            Error::new(
                ErrorKind::Corruption,
                "prepared content has no recovery plan",
            )
        })?;
        let mut xorb_artifacts = Vec::with_capacity(content.xorbs.len());
        for artifact in &content.xorbs {
            let path = directory.path().join(format!(
                "recovery-xorb-{}",
                crab_xet::hash::MerkleHash::from(artifact.protocol_hash).hex()
            ));
            let remote = layout.ref_journal_recovery_xorb_path(
                plan_id,
                &crab_xet::hash::MerkleHash::from(artifact.protocol_hash),
            );
            recover_artifact(client, &remote, &path, artifact.size, cancel).await?;
            xorb_artifacts.push(crab_remote::prepare::ContentArtifact::new(
                artifact.protocol_hash,
                artifact.body_hash,
                artifact.size,
                path,
            ));
        }
        let mut shard_artifacts = Vec::with_capacity(content.shards.len());
        for artifact in &content.shards {
            let path = directory.path().join(format!(
                "recovery-shard-{}",
                crab_xet::hash::MerkleHash::from(artifact.protocol_hash).hex()
            ));
            let remote = layout.ref_journal_recovery_shard_path(
                plan_id,
                &crab_xet::hash::MerkleHash::from(artifact.protocol_hash),
            );
            recover_artifact(client, &remote, &path, artifact.size, cancel).await?;
            shard_artifacts.push(crab_remote::prepare::ContentArtifact::new(
                artifact.protocol_hash,
                artifact.body_hash,
                artifact.size,
                path,
            ));
        }
        let prepared_content = crab_remote::prepare::PreparedContent::new(
            xorb_artifacts,
            shard_artifacts,
            content
                .files
                .iter()
                .map(|file| {
                    crab_remote::prepare::ContentFile::new(
                        file.file_hash,
                        file.size,
                        file.shard_hash,
                    )
                })
                .collect(),
            bounds.max_fetched_bytes,
            cancel,
        )
        .await
        .map_err(Failure::from)?;
        prepared = prepared
            .attach_content(prepared_content)
            .map_err(Failure::from)?;
    }
    Ok((prepared, directory))
}

fn validate_recovery_budget(
    input: &PackInput,
    content: Option<&crate::mutation::ContentBinding>,
    bounds: crate::ReadLimits,
) -> Result<()> {
    let pack_bytes = match input {
        PackInput::Staged { size, .. } => *size,
        PackInput::Empty | PackInput::Local(_) => 0,
    };
    let mut fetched_bytes = pack_bytes;
    let mut requests = u64::from(pack_bytes != 0);
    if let Some(content) = content {
        let artifact_count = content
            .xorbs
            .len()
            .checked_add(content.shards.len())
            .ok_or_else(|| {
                Error::new(ErrorKind::LimitExceeded, "recovery artifact count overflow")
            })?;
        requests = requests
            .checked_add(u64::try_from(artifact_count).unwrap_or(u64::MAX))
            .ok_or_else(|| {
                Error::new(ErrorKind::LimitExceeded, "recovery request count overflow")
            })?;
        if u64::try_from(artifact_count).unwrap_or(u64::MAX) > bounds.max_logical_objects
            || u64::try_from(content.files.len()).unwrap_or(u64::MAX) > bounds.max_entries
        {
            return Err(Error::new(
                ErrorKind::LimitExceeded,
                "recovery content count exceeds operation limits",
            ));
        }
        for artifact in content.xorbs.iter().chain(&content.shards) {
            fetched_bytes = fetched_bytes.checked_add(artifact.size).ok_or_else(|| {
                Error::new(ErrorKind::LimitExceeded, "recovery byte count overflow")
            })?;
        }
    }
    if requests > bounds.max_storage_requests || fetched_bytes > bounds.max_fetched_bytes {
        return Err(Error::new(
            ErrorKind::LimitExceeded,
            "recovery artifacts exceed operation limits",
        ));
    }
    Ok(())
}

async fn recover_artifact(
    client: &Client,
    remote: &object_store::path::Path,
    local: &Path,
    size: u64,
    cancel: &CancellationToken,
) -> Result<()> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Error::new(ErrorKind::Cancelled, "mutation recovery cancelled")),
        result = client.0.store.download_to_path_bounded(remote, local, size) => {
            let downloaded = result.map_err(|source| Failure::Prepare(crab_remote::prepare::Error::Storage(source)))?;
            if downloaded != size {
                return Err(Error::new(ErrorKind::Corruption, "prepared content size changed"));
            }
            Ok(())
        }
    }
}

enum PackInput {
    Empty,
    Local(PathBuf),
    Staged {
        pack_id: String,
        size: u64,
        plan_id: String,
    },
}

impl Client {
    /// Resume a saved mutation with current credentials and fresh scratch files.
    ///
    /// Prior attempts are reconciled without replay, including when reads are indexing.
    /// An unattempted plan is revalidated under its operation lease before execution.
    pub async fn resume_mutation(
        &self,
        token: RecoveryToken,
        scratch: PathBuf,
        options: OperationOptions,
    ) -> Result<MutationOutcome> {
        #[cfg(feature = "managed")]
        if token.managed_finalize().is_some() {
            return execute_managed(self.clone(), token, options).await;
        }
        if token.placement() != &placement(self)? {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "recovery token belongs to another storage placement",
            ));
        }
        let locator = token.locator()?;
        validate_ref_preparation(self, &locator, &scratch)?;
        execute_mutation(
            self.clone(),
            token,
            ExecutionInput::Resume(scratch),
            options,
        )
        .await
    }

    /// Create or adopt canonical metadata roots for one direct repository prefix.
    ///
    /// HEAD must be a fully qualified branch. Existing valid roots are adopted
    /// without changing HEAD or refs; incompatible nonempty prefixes are rejected.
    /// No bucket, checkout, or Git object database is created. Interruption can
    /// leave roots created remotely; repeating initialization safely adopts them.
    pub fn initialize_remote(
        &self,
        locator: RepositoryLocator,
        head: &str,
    ) -> crate::Request<'_, (), OperationOptions> {
        let head = head.to_owned();
        crate::Request::new(move |options: OperationOptions| {
            Box::pin(async move {
                if !self.0.direct_store_configured {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "direct storage is required for remote initialization",
                    ));
                }
                let state = self.0.clone();
                self.0.operations.run(options.with_owner_deadline()?, move |cancel| async move {
                    let layout = crab_storage::StoreLayout::new(
                        state.store.clone(),
                        locator.direct_prefix()?.to_owned(),
                    );
                    // Initialization owns only idempotent conditional root creates,
                    // never leases or database sessions. Dropping an interrupted
                    // request is safe; a later call adopts any completed creates.
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => Err(Error::new(ErrorKind::Cancelled, "initialization cancelled")),
                        result = crab_write::initialize::initialize_repository(&state.store, &layout, &head) => result.map_err(|error| Failure::Write(error).into()),
                    }
                }).await
            })
        })
    }

    /// Reconcile a saved token without requiring a readable repository generation.
    ///
    /// Read-only lookup requires current storage credentials and matching placement.
    /// Missing proof remains indeterminate, including before any recorded attempt.
    pub async fn reconcile(
        &self,
        token: RecoveryToken,
        options: OperationOptions,
    ) -> Result<MutationOutcome> {
        #[cfg(feature = "managed")]
        if token.managed_finalize().is_some() {
            return execute_managed(self.clone(), token, options).await;
        }
        if token.placement() != &placement(self)? {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "recovery token belongs to another storage placement",
            ));
        }
        let client = self.clone();
        self.0
            .operations
            .run_mutation(options.clone(), move |cancel| async move {
                reconcile(&client, token, &options, &cancel).await
            })
            .await
    }
}

fn placement(client: &Client) -> Result<[u8; 32]> {
    client.0.store.target_identity().copied().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            "store has no durable placement identity",
        )
    })
}

async fn reconcile(
    client: &Client,
    token: RecoveryToken,
    options: &OperationOptions,
    cancel: &CancellationToken,
) -> Result<MutationOutcome> {
    if cancel.is_cancelled() {
        return Err(Error::new(ErrorKind::Cancelled, "reconciliation cancelled"));
    }
    let locator = token.locator()?;
    let layout =
        crab_storage::StoreLayout::new(client.0.store.clone(), locator.direct_prefix()?.to_owned());
    // Read-only historical traversal owns no leases or catalog sessions. Cancel
    // its storage future so a long ancestry walk cannot stall client shutdown.
    let receipt = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(Error::new(ErrorKind::Cancelled, "reconciliation cancelled")),
        result = crab_metadata::plan_receipt::read_plan_receipt(&client.0.store, &layout, token.plan_id()) => match result {
            // Missing historical objects cannot distinguish an interrupted attempt
            // from committed evidence removed outside the retention contract.
            Err(crab_metadata::error::MetadataError::Storage {
                source: crab_storage::StorageError::NotFound { .. },
            }) => None,
            result => result.map_err(crate::mutation::metadata_error)?,
        },
    };
    let Some(receipt) = receipt else {
        return Ok(MutationOutcome::Indeterminate { recovery: token });
    };
    let crab_metadata::plan_receipt::PlanCommit::RefJournal { transaction_id, .. } = receipt.commit
    else {
        return Err(Error::new(
            ErrorKind::Corruption,
            "ref token has a different commit authority",
        ));
    };
    // Readiness is optional evidence after proven commitment. A failed read must
    // retain that commitment, and reconciliation never starts catalog repair.
    let readiness = match open(client, &layout, &locator, options, cancel).await {
        Ok(repository) => Readiness::Ready {
            generation: repository.generation(),
        },
        Err(_) => Readiness::Pending,
    };
    Ok(MutationOutcome::Committed {
        receipt: CommitReceipt {
            recovery: token,
            transaction_id,
        },
        readiness,
    })
}

async fn open(
    client: &Client,
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    locator: &RepositoryLocator,
    options: &OperationOptions,
    cancel: &CancellationToken,
) -> Result<crab_remote_git::RemoteGitRepository> {
    let identity = crab_remote_git::RepositoryIdentity::new(
        client.0.namespace.clone(),
        locator.direct_prefix()?.to_owned(),
        1,
    )
    .map_err(crate::remote_error::remote_error)?;
    let options =
        crab_remote_git::RepositoryOptions::new(Default::default(), options.owner_limits())
            .map_err(crate::remote_error::remote_error)?;
    crab_remote_git::RemoteGitRepository::open(
        client.0.store.clone(),
        layout.clone(),
        identity,
        client.0.git.clone(),
        options,
        cancel,
    )
    .await
    .map_err(crate::remote_error::remote_error)
}

async fn readiness(
    client: &Client,
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    locator: &RepositoryLocator,
    options: &OperationOptions,
    cancel: &CancellationToken,
) -> Readiness {
    let result = publication::finish_committed(publication::with_internal_lease(
        &client.0.store,
        layout,
        crab_coordination::GIT_GENERATION_OWNER_RESOURCE,
        LEASE_TTL,
        cancel,
        |cancel| async move {
            // Catalog maintenance has a large future. Keep it off each enclosing
            // lease/operation frame so default Tokio stacks can publish safely.
            let manifest = Box::pin(crab_write::generation::make_readable(
                &client.0.store,
                layout,
                LEASE_TTL,
                None,
                &cancel,
            ))
            .await?;
            if manifest.is_none() {
                return Ok::<_, Failure>(Readiness::Pending);
            }
            let repository = open(client, layout, locator, options, &cancel).await?;
            Ok(Readiness::Ready {
                generation: repository.generation(),
            })
        },
    ))
    .await;
    match result {
        publication::Readiness::Ready { generation } => generation,
        publication::Readiness::Pending => Readiness::Pending,
    }
}

fn dependency_limits(
    bounds: crate::ReadLimits,
) -> crab_read::dependency_proof::DependencyProofLimits {
    crab_read::dependency_proof::DependencyProofLimits {
        max_dependencies: crate::mutation::MAX_REF_UPDATES,
        max_total_file_bytes: bounds.max_fetched_bytes,
        max_duration: Duration::from_secs(60),
        lookup: crab_metadata::file_index_lookup::FileIndexLookupLimits {
            max_files: crate::mutation::MAX_REF_UPDATES,
            max_shard_visits: 4096,
            max_shard_bytes: bounds.max_fetched_bytes.min(128 * 1024 * 1024),
            max_recipe_entries: usize::try_from(bounds.max_entries).unwrap_or(usize::MAX),
        },
        content: crab_read::pointer_proof::PointerProofLimits {
            max_file_bytes: bounds.max_fetched_bytes,
            max_shard_bytes: bounds.max_fetched_bytes.min(128 * 1024 * 1024),
            max_xorb_bytes: bounds.max_fetched_bytes.min(128 * 1024 * 1024),
            max_read_bytes: bounds.max_fetched_bytes,
            max_chunks: usize::try_from(bounds.max_entries).unwrap_or(usize::MAX),
            max_duration: Duration::from_secs(60),
        },
    }
}
