use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    sync::Arc,
    time::Duration,
};

use crab_coordination::{GIT_MANIFEST_RESOURCE, LFS_LOCKS_RESOURCE};
use crab_git::receive_wire;
use crab_lfs::LfsLockManager;
use crab_metadata::{git_visibility, manifest_store, ref_journal::RefJournalEdit};
use crab_read::{dependency_proof::DependencyProofLimits, pointer_proof::PointerProofLimits};
use crab_remote_git::RepositoryOptions;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::{ReceiveError, Result, check_cancelled, validate};
use crate::{
    auth::Principal,
    server::{Repository, Server},
};

const TTL: Duration = Duration::from_secs(300);
const MAX_ACTIVE_LFS_LOCKS: usize = 10_000;
const MAX_RECEIVE_LOGICAL_OBJECTS: u64 = 5_000_000;
const MAX_RECEIVE_STORAGE_REQUESTS: u64 = 6_000_000;

fn repository_options(server: &Server) -> crate::Result<RepositoryOptions> {
    let mut operation = server.options.operation_limits();
    operation.max_duration = super::REQUEST_BUDGET;
    operation.max_logical_objects = MAX_RECEIVE_LOGICAL_OBJECTS;
    operation.max_storage_requests = MAX_RECEIVE_STORAGE_REQUESTS;
    Ok(RepositoryOptions::new(
        server.options.object_limits(),
        operation,
    )?)
}

#[derive(Serialize)]
struct NativePlanBinding<'a> {
    repository_owner: &'a str,
    repository_name: &'a str,
    actor_issuer: &'a str,
    actor_subject: &'a str,
    body_digest: [u8; 32],
}

fn native_plan_id(
    key: &(String, String),
    principal: &Principal,
    body_digest: [u8; 32],
) -> Result<String> {
    let identity = principal.identity().ok_or(ReceiveError::Forbidden)?;
    let binding = NativePlanBinding {
        repository_owner: &key.0,
        repository_name: &key.1,
        actor_issuer: &identity.issuer,
        actor_subject: &identity.subject,
        body_digest,
    };
    let request_digest = crab_metadata::receipts::publication_request_digest(&binding)?;
    // Native Git has no idempotency header. A fixed nonce makes an identical
    // wire request resolve to the same durable plan after a lost response.
    let plan = crab_metadata::receipts::publication_plan_id(&request_digest, &[0; 16]);
    Ok(blake3::Hash::from(plan).to_hex().to_string())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Publication {
    NativePush,
    PullRequest,
}

pub(super) struct PackPublication {
    pub visibility_base: Option<(String, gix_hash::ObjectId)>,
    pub kind: Publication,
}

struct ReceiveInput {
    pack: Option<BufReader<std::fs::File>>,
    plan_id: Option<String>,
    publication: Publication,
    visibility_bases: BTreeMap<String, (String, gix_hash::ObjectId)>,
}

struct PublishAttempt<'a> {
    directory: crate::local_disk::StagingDirectory,
    holders: &'a BTreeMap<String, String>,
    plan_id: Option<String>,
}

pub(super) async fn run(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
    directory: crate::local_disk::StagingDirectory,
    body_digest: [u8; 32],
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    let path = directory.path().join("receive");
    let (request, input) = tokio::task::spawn_blocking(move || -> Result<_> {
        let mut input = BufReader::new(std::fs::File::open(path)?);
        let request = receive_wire::read_request(&mut input)?;
        if request.updates.is_empty() && !input.fill_buf()?.is_empty() {
            return Err(ReceiveError::Request("Unexpected data after receive probe"));
        }
        Ok((request, input))
    })
    .await??;
    if request.updates.is_empty() {
        return Ok(vec![]);
    }
    let plan_id = native_plan_id(key, principal, body_digest)?;
    run_request(
        server,
        principal,
        key,
        directory,
        request,
        ReceiveInput {
            pack: Some(input),
            plan_id: Some(plan_id),
            publication: Publication::NativePush,
            visibility_bases: BTreeMap::new(),
        },
        cancel,
    )
    .await
}

pub(crate) async fn publish_existing_objects(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
    update: crab_git::receive_plan::RefUpdate,
    publication: Publication,
    cancel: &CancellationToken,
) -> Result<()> {
    let directory = server.local_staging.create(1, cancel).await?;
    let request = receive_wire::ReceiveRequest {
        updates: vec![update],
        report_status: false,
    };
    run_request(
        server,
        principal,
        key,
        directory,
        request,
        ReceiveInput {
            pack: None,
            plan_id: None,
            publication,
            visibility_bases: BTreeMap::new(),
        },
        cancel,
    )
    .await
    .map(drop)
}

pub(crate) async fn publish_default_branch(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
    expected_head: &str,
    branch: &str,
    expected_oid: gix_hash::ObjectId,
    cancel: &CancellationToken,
) -> Result<()> {
    let entry = server.repositories.get(key).ok_or(ReceiveError::NotFound)?;
    if !principal.can_admin(&entry.config) {
        return Err(ReceiveError::Forbidden);
    }
    entry
        .open_current(server, repository_options(server)?, cancel)
        .await?;
    let leased_entry = Arc::clone(&entry);
    crab_remote::publication::with_leases(
        &entry.store,
        &entry.layout,
        [branch.to_owned()],
        TTL,
        cancel,
        move |holders, cancel| {
            let entry = Arc::clone(&leased_entry);
            async move {
                let manifest_entry = Arc::clone(&entry);
                crab_remote::publication::with_internal_lease(
                    &entry.store,
                    &entry.layout,
                    GIT_MANIFEST_RESOURCE,
                    TTL,
                    &cancel,
                    move |cancel| async move {
                        check_cancelled(&cancel)?;
                        let snapshot = manifest_store::read_repository_snapshot(
                            &manifest_entry.store,
                            &manifest_entry.layout,
                        )
                        .await?;
                        if snapshot.journal.head != expected_head {
                            return Err(ReceiveError::DefaultBranchChanged);
                        }
                        let oid = expected_oid.to_string();
                        if snapshot.journal.refs.get(branch) != Some(&oid) {
                            return Err(ReceiveError::BranchChanged);
                        }
                        if snapshot.journal.head == branch {
                            return Ok(());
                        }
                        let evidence = git_visibility::GitVisibilityEdit::from_delta_objects(
                            Some(oid.clone()),
                            oid.clone(),
                            vec![],
                            vec![],
                        );
                        let evidence_hash = git_visibility::upload_edit(
                            &manifest_entry.store,
                            &manifest_entry.layout,
                            &evidence,
                        )
                        .await?;
                        check_cancelled(&cancel)?;
                        if !principal.can_admin(&manifest_entry.config) {
                            return Err(ReceiveError::Forbidden);
                        }
                        // Retargeting HEAD needs a journal parent and branch lease. This no-op
                        // ref edit preserves the branch's immutable visibility closure.
                        crab_write::journal::commit_edits(
                            &manifest_entry.store,
                            &manifest_entry.layout,
                            &snapshot,
                            vec![RefJournalEdit {
                                ref_name: branch.to_owned(),
                                old_oid: Some(oid.clone()),
                                new_oid: Some(oid),
                                peeled_oid: None,
                                lock_holder: holders.get(branch).cloned(),
                                visibility_evidence_hash: Some(evidence_hash),
                            }],
                            Some(branch.to_owned()),
                            vec![],
                            vec![],
                            crab_write::journal::CommitOptions::new(TTL, &cancel),
                        )
                        .await?;
                        Ok(())
                    },
                )
                .await?;
                // Release the manifest lease before maintenance reacquires it; keep
                // GC admission until this readiness attempt finishes. HEAD acceptance
                // is independent of read readiness.
                let _readiness = crab_remote::publication::finish_committed(async {
                    entry.invalidate().await;
                    let repository = entry
                        .open_current(server, repository_options(server)?, &cancel)
                        .await?;
                    Ok::<_, crate::Error>(repository.generation())
                })
                .await;
                Ok(())
            }
        },
    )
    .await
}

pub(super) async fn publish_pack(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
    directory: crate::local_disk::StagingDirectory,
    pack: BufReader<std::fs::File>,
    update: crab_git::receive_plan::RefUpdate,
    publication: PackPublication,
    cancel: &CancellationToken,
) -> Result<()> {
    let visibility_bases = publication
        .visibility_base
        .map(|base| BTreeMap::from([(update.name.clone(), base)]))
        .unwrap_or_default();
    let request = receive_wire::ReceiveRequest {
        updates: vec![update],
        report_status: false,
    };
    run_request(
        server,
        principal,
        key,
        directory,
        request,
        ReceiveInput {
            pack: Some(pack),
            plan_id: None,
            publication: publication.kind,
            visibility_bases,
        },
        cancel,
    )
    .await
    .map(drop)
}

async fn run_request(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
    directory: crate::local_disk::StagingDirectory,
    request: receive_wire::ReceiveRequest,
    input: ReceiveInput,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    let entry = server.repositories.get(key).ok_or(ReceiveError::NotFound)?;
    if !principal.can_write(&entry.config) {
        return Err(ReceiveError::Forbidden);
    }
    let names: Vec<_> = request
        .updates
        .iter()
        .map(|update| update.name.clone())
        .collect();
    let leased_entry = Arc::clone(&entry);
    crab_remote::publication::with_leases(
        &entry.store,
        &entry.layout,
        names,
        TTL,
        cancel,
        move |holders, cancel| async move {
            publish(
                server,
                principal,
                &leased_entry,
                &request,
                input,
                directory,
                &holders,
                &cancel,
            )
            .await
        },
    )
    .await
}

async fn publish(
    server: &Server,
    principal: &Principal,
    entry: &Repository,
    request: &receive_wire::ReceiveRequest,
    input: ReceiveInput,
    directory: crate::local_disk::StagingDirectory,
    holders: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    let Some(plan_id) = input.plan_id.clone() else {
        return publish_attempt(
            server,
            principal,
            entry,
            request,
            input,
            PublishAttempt {
                directory,
                holders,
                plan_id: None,
            },
            cancel,
        )
        .await;
    };
    let attempt = PublishAttempt {
        directory,
        holders,
        plan_id: Some(plan_id.clone()),
    };
    let result = crab_remote::publication::with_plan(
        &entry.store,
        &entry.layout,
        &plan_id,
        TTL,
        cancel,
        move |plan_cancel| async move {
            publish_attempt(
                server,
                principal,
                entry,
                request,
                input,
                attempt,
                &plan_cancel,
            )
            .await
        },
    )
    .await;
    match result {
        Ok(response) => Ok(response),
        Err(error @ ReceiveError::Write(_))
        | Err(
            error @ ReceiveError::Metadata(
                crab_metadata::error::MetadataError::PlanAlreadyAttempted { .. },
            ),
        ) => recover_native_plan(server, entry, request, &plan_id, cancel, error).await,
        Err(error) => Err(error),
    }
}

async fn publish_attempt<'a>(
    server: &Server,
    principal: &Principal,
    entry: &Repository,
    request: &receive_wire::ReceiveRequest,
    input: ReceiveInput,
    attempt: PublishAttempt<'a>,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    check_cancelled(cancel)?;
    if !principal.can_write(&entry.config) {
        return Err(ReceiveError::Forbidden);
    }
    let repository = entry
        .open_current(server, repository_options(server)?, cancel)
        .await?;
    let snapshot = manifest_store::read_repository_snapshot(&entry.store, &entry.layout).await?;
    let refs: BTreeMap<_, _> = repository
        .refs()
        .entries
        .iter()
        .map(|reference| (reference.name.clone(), reference.target.to_string()))
        .collect();
    if snapshot.manifest.generation != repository.generation()
        || snapshot.journal.refs != refs
        || !snapshot.journal.transactions.is_empty()
    {
        return Err(ReceiveError::Request(
            "Repository changed during receive admission; retry",
        ));
    }
    let has_branch = refs.keys().any(|name| name.starts_with("refs/heads/"));
    let actor = principal.identity().ok_or(ReceiveError::Forbidden)?;
    let protections = entry
        .branch_protections(server, &actor)
        .await
        .map_err(|error| ReceiveError::Settings(Box::new(error)))?;
    let protected = input.publication == Publication::NativePush
        && request.updates.iter().any(|update| {
            protections.protection(&update.name).is_some() && (update.old.is_some() || has_branch)
        });
    if protected {
        if request.report_status {
            let mut bytes = Vec::new();
            receive_wire::report(
                &mut bytes,
                &request.updates,
                None,
                Some("protected branch requires a pull request"),
            )?;
            return Ok(bytes);
        }
        return Err(ReceiveError::Protected);
    }
    let visibility_bases = input.visibility_bases;
    let prepared = match validate::prepare(
        repository.clone(),
        entry.layout.clone(),
        attempt.directory.path().to_owned(),
        input.pack,
        request.updates.clone(),
        visibility_bases,
        cancel,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(
            error @ (ReceiveError::Pack(_) | ReceiveError::Graph(_) | ReceiveError::Request(_)),
        ) if request.report_status => {
            tracing::warn!(error = ?error, "Git receive validation rejected");
            let mut bytes = Vec::new();
            let unpack = matches!(error, ReceiveError::Pack(_)).then_some("incoming pack rejected");
            let reason = match &error {
                ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Ref {
                    reason,
                    ..
                }) => *reason,
                _ => "incoming refs or graph rejected",
            };
            receive_wire::report(&mut bytes, &request.updates, unpack, Some(reason))?;
            return Ok(bytes);
        }
        Err(error) => return Err(error),
    };
    let changed_path_hashes = prepared.plan().changed_path_hashes().clone();
    let artifacts = prepared
        .upload(&snapshot, dependency_limits(), attempt.holders, cancel)
        .await
        .map_err(validate::map_error)?;
    let head = if prepared.plan().refs().is_empty()
        || prepared.plan().refs().contains_key(&snapshot.manifest.head)
    {
        None
    } else {
        // Tags can exist before the first branch. Keep HEAD unborn until a
        // branch is available instead of turning an arbitrary tag into HEAD.
        prepared
            .plan()
            .refs()
            .keys()
            .find(|name| name.starts_with("refs/heads/"))
            .cloned()
    };
    let outcome = if changed_path_hashes.is_empty() {
        commit_prepared(
            server,
            principal,
            entry,
            artifacts,
            head,
            attempt.plan_id.as_deref(),
            cancel,
        )
        .await
    } else {
        let subject = principal.identity().ok_or(ReceiveError::Forbidden)?.subject;
        // Lock endpoints take this lease for every mutation. Keep the final lock
        // read and journal commit together so replicas cannot create a lock between them.
        crab_remote::publication::with_internal_lease(
            &entry.store,
            &entry.layout,
            LFS_LOCKS_RESOURCE,
            TTL,
            cancel,
            move |lease_cancel| async move {
                check_cancelled(&lease_cancel)?;
                let manager = LfsLockManager::lfs(entry.store.clone(), &entry.config.prefix);
                let locks = tokio::select! {
                    () = lease_cancel.cancelled() => return Err(ReceiveError::Cancelled),
                    result = manager.list_page(None, None, None, MAX_ACTIVE_LFS_LOCKS + 1) => result?,
                };
                if locks.len() > MAX_ACTIVE_LFS_LOCKS {
                    return Err(ReceiveError::LfsLockLimit);
                }
                if locks.iter().any(|lock| {
                    lock.owner != subject
                        && changed_path_hashes.contains(blake3::hash(lock.path.as_bytes()).as_bytes())
                }) {
                    return Err(ReceiveError::Locked);
                }
                commit_prepared(
                    server,
                    principal,
                    entry,
                    artifacts,
                    head,
                    attempt.plan_id.as_deref(),
                    &lease_cancel,
                )
                .await
            },
        )
        .await
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(ReceiveError::Locked) if request.report_status => {
            let mut bytes = Vec::new();
            receive_wire::report(
                &mut bytes,
                &request.updates,
                None,
                Some("path is locked by another user"),
            )?;
            return Ok(bytes);
        }
        Err(error) => return Err(error),
    };
    if let crab_remote::publication::CommitOutcome::Indeterminate { source, .. } = outcome {
        // The deterministic native plan can recover a committed receipt after
        // transport loss, but an absent receipt is not proof of rejection.
        return Err(ReceiveError::Write(*source));
    }
    // Acknowledge known ref commitment even if read indexes remain pending.
    // A lost acknowledgement is indeterminate; matching refs cannot prove it.
    let _readiness = crab_remote::publication::finish_committed(async {
        entry.invalidate().await;
        let repository = entry
            .open_current(server, repository_options(server)?, cancel)
            .await?;
        Ok::<_, crate::Error>(repository.generation())
    })
    .await;
    let mut bytes = Vec::new();
    if request.report_status {
        receive_wire::report(&mut bytes, &request.updates, None, None)?;
    }
    Ok(bytes)
}

async fn recover_native_plan(
    server: &Server,
    entry: &Repository,
    request: &receive_wire::ReceiveRequest,
    plan_id: &str,
    cancel: &CancellationToken,
    original: ReceiveError,
) -> Result<Vec<u8>> {
    let receipt = match crab_metadata::plan_receipt::resolve_plan_receipt(
        &entry.store,
        &entry.layout,
        plan_id,
    )
    .await
    {
        Ok(receipt) => receipt,
        Err(error) => {
            tracing::warn!(%plan_id, %error, "native receive plan reconciliation was inconclusive");
            return Err(original);
        }
    };
    let Some(receipt) = receipt else {
        return Err(original);
    };
    if !matches!(
        receipt.commit,
        crab_metadata::plan_receipt::PlanCommit::RefJournal { .. }
    ) {
        tracing::error!(%plan_id, "native receive plan receipt used an unexpected commit authority");
        return Err(original);
    }
    // The receipt proves the ref visibility boundary. Index readiness remains
    // best-effort and cannot turn a recovered commit into a rejection.
    let _readiness = crab_remote::publication::finish_committed(async {
        entry.invalidate().await;
        let repository = entry
            .open_current(server, repository_options(server)?, cancel)
            .await?;
        Ok::<_, crate::Error>(repository.generation())
    })
    .await;
    let mut bytes = Vec::new();
    if request.report_status {
        receive_wire::report(&mut bytes, &request.updates, None, None)?;
    }
    Ok(bytes)
}

async fn commit_prepared(
    server: &Server,
    principal: &Principal,
    entry: &Repository,
    artifacts: crab_remote::prepare::Artifacts<'_>,
    head: Option<String>,
    plan_id: Option<&str>,
    cancel: &CancellationToken,
) -> Result<crab_remote::publication::CommitOutcome> {
    check_cancelled(cancel)?;
    if !principal.can_write(&entry.config) {
        return Err(ReceiveError::Forbidden);
    }
    let actor = principal.identity().ok_or(ReceiveError::Forbidden)?;
    if entry
        .lifecycle(server, &actor)
        .await
        .map_err(|error| ReceiveError::Settings(Box::new(error)))?
        .archived
    {
        return Err(ReceiveError::Archived);
    }
    let options = crab_write::journal::CommitOptions::new(TTL, cancel);
    let options = if let Some(plan_id) = plan_id {
        options.with_plan(plan_id)
    } else {
        options
    };
    artifacts
        .commit(head, options)
        .await
        .map_err(validate::map_error)
}

fn dependency_limits() -> DependencyProofLimits {
    DependencyProofLimits {
        max_dependencies: 1024,
        max_total_file_bytes: 2 * 1024 * 1024 * 1024,
        max_duration: Duration::from_secs(120),
        lookup: crab_metadata::file_index_lookup::FileIndexLookupLimits {
            max_files: 1024,
            max_shard_visits: 4096,
            max_shard_bytes: 128 * 1024 * 1024,
            max_recipe_entries: 1_000_000,
        },
        content: PointerProofLimits {
            max_file_bytes: crate::server::MAX_DEPENDENCY_FILE_BYTES,
            max_shard_bytes: 128 * 1024 * 1024,
            max_xorb_bytes: 128 * 1024 * 1024,
            max_read_bytes: 2 * 1024 * 1024 * 1024,
            max_chunks: 1_000_000,
            max_duration: Duration::from_secs(60),
        },
    }
}
