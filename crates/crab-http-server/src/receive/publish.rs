use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    sync::Arc,
    time::Duration,
};

use crab_coordination::LFS_LOCKS_RESOURCE;
use crab_git::receive_wire;
use crab_lfs::LfsLockManager;
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

struct PublishAttempt {
    directory: crate::local_disk::StagingDirectory,
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
    let leased_entry = Arc::clone(&entry);
    crab_remote::publication::with_leases(
        &entry.store,
        &entry.layout,
        [branch.to_owned()],
        TTL,
        cancel,
        move |_holders, cancel| {
            let entry = Arc::clone(&leased_entry);
            async move {
                check_cancelled(&cancel)?;
                let view = entry.open_view().await?;
                if view.head() != expected_head {
                    return Err(ReceiveError::DefaultBranchChanged);
                }
                let expected_oid = expected_oid.to_string();
                if view.refs().get(branch) != Some(&expected_oid) {
                    return Err(ReceiveError::BranchChanged);
                }
                if view.head() == branch {
                    return Ok(());
                }
                if !principal.can_admin(&entry.config) {
                    return Err(ReceiveError::Forbidden);
                }
                match crab_write::capsule_protocol::retarget_head(
                    &entry.layout,
                    view.root_snapshot().clone(),
                    expected_head,
                    branch,
                )
                .await
                {
                    Ok(_) => {}
                    Err(crab_write::WriteError::CapsuleRootChanged { .. }) => {
                        return Err(ReceiveError::DefaultBranchChanged);
                    }
                    Err(error) => return Err(error.into()),
                }
                let _readiness = crab_remote::publication::finish_committed(async {
                    entry.invalidate().await;
                    let repository = entry
                        .open_current(server, RepositoryOptions::default(), &cancel)
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
        move |_holders, cancel| async move {
            publish(
                server,
                principal,
                &leased_entry,
                &request,
                input,
                directory,
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
                plan_id: None,
            },
            cancel,
        )
        .await;
    };
    let attempt = PublishAttempt {
        directory,
        plan_id: Some(plan_id.clone()),
    };
    let result = crab_remote::publication::with_capsule_plan(
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

async fn publish_attempt(
    server: &Server,
    principal: &Principal,
    entry: &Repository,
    request: &receive_wire::ReceiveRequest,
    input: ReceiveInput,
    attempt: PublishAttempt,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    check_cancelled(cancel)?;
    if !principal.can_write(&entry.config) {
        return Err(ReceiveError::Forbidden);
    }
    let mut view = entry.open_view().await?;
    for _ in 0..2 {
        if request.updates.iter().all(|update| {
            view.ref_capsule_count(&update.name) < crate::maintenance::FOREGROUND_CAPSULE_THRESHOLD
        }) {
            break;
        }
        entry.checkpoint_now(server, cancel).await?;
        view = entry.open_view().await?;
    }
    if request.updates.iter().any(|update| {
        view.ref_capsule_count(&update.name) >= crate::maintenance::FOREGROUND_CAPSULE_THRESHOLD
    }) {
        return Err(crab_write::WriteError::Internal(
            "repository checkpoint could not bound the selected ref frontier".to_owned(),
        )
        .into());
    }
    let refs = view.refs().clone();
    let visibility = view.git_visibility_index()?;
    let repository = view
        .git_repository(
            entry.identity.clone(),
            Arc::clone(&server.runtime),
            RepositoryOptions::default(),
            super::MAX_BODY,
            cancel,
        )
        .await?;
    let has_branch = refs.keys().any(|name| name.starts_with("refs/heads/"));
    let actor = principal.identity().ok_or(ReceiveError::Forbidden)?;
    let initial_head = (!has_branch)
        .then(|| {
            request
                .updates
                .iter()
                .find(|update| update.name.starts_with("refs/heads/") && update.new.is_some())
        })
        .flatten()
        .filter(|update| update.name != view.head())
        .map(|update| {
            (
                view.root_snapshot().clone(),
                view.head().to_owned(),
                update.name.clone(),
            )
        });
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
        visibility,
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
        .upload_capsule(&view, dependency_limits(), cancel)
        .await
        .map_err(validate::map_error)?;
    let outcome = if changed_path_hashes.is_empty() {
        commit_prepared(
            server,
            principal,
            entry,
            artifacts,
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
    if let crab_remote::prepare::CapsuleCommitOutcome::Indeterminate { source, .. } = outcome {
        // The deterministic native plan can recover a committed receipt after
        // transport loss, but an absent receipt is not proof of rejection.
        return Err(ReceiveError::Write(*source));
    }
    if let Some((root, expected_head, head)) = initial_head
        && let Err(error) =
            crab_write::capsule_protocol::retarget_head(&entry.layout, root, &expected_head, &head)
                .await
    {
        // The ref transaction is already committed and must be acknowledged.
        // A later admin update can repair an unavailable control-plane root.
        tracing::error!(%head, %error, "first branch committed but HEAD retargeting failed");
    }
    entry.schedule_maintenance(server).await;
    // Acknowledge known ref commitment even if read indexes remain pending.
    // A lost acknowledgement is indeterminate; matching refs cannot prove it.
    let _readiness = crab_remote::publication::finish_committed(async {
        entry.invalidate().await;
        let repository = entry
            .open_current(server, RepositoryOptions::default(), cancel)
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
    let receipt = match crab_metadata::capsule_protocol::resolve_capsule_plan_receipt(
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
    let receipt_updates = receipt
        .transaction()
        .edits()
        .iter()
        .map(|edit| (edit.ref_name(), (edit.expected_old(), edit.new_oid())))
        .collect::<BTreeMap<_, _>>();
    if request.updates.len() != receipt_updates.len()
        || request.updates.iter().any(|update| {
            let expected_old = update.old.map(|oid| oid.to_string());
            let new_oid = update.new.map(|oid| oid.to_string());
            receipt_updates
                .get(update.name.as_str())
                .is_none_or(|(receipt_old, receipt_new)| {
                    *receipt_old != expected_old.as_deref() || *receipt_new != new_oid.as_deref()
                })
        })
    {
        tracing::error!(%plan_id, "native receive plan receipt does not match the wire request");
        return Err(original);
    }
    entry.schedule_maintenance(server).await;
    // The receipt proves the ref visibility boundary. Index readiness remains
    // best-effort and cannot turn a recovered commit into a rejection.
    let _readiness = crab_remote::publication::finish_committed(async {
        entry.invalidate().await;
        let repository = entry
            .open_current(server, RepositoryOptions::default(), cancel)
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
    artifacts: crab_remote::prepare::CapsuleArtifacts,
    plan_id: Option<&str>,
    cancel: &CancellationToken,
) -> Result<crab_remote::prepare::CapsuleCommitOutcome> {
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
    artifacts
        .commit(plan_id, TTL, cancel)
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
