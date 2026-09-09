use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    time::Duration,
};

use crab_coordination::GIT_MANIFEST_RESOURCE;
use crab_git::receive_wire;
use crab_metadata::{git_visibility, manifest_store, ref_journal::RefJournalEdit};
use crab_read::{dependency_proof::DependencyProofLimits, pointer_proof::PointerProofLimits};
use crab_remote_git::RepositoryOptions;
use tokio_util::sync::CancellationToken;

use super::{ReceiveError, Result, check_cancelled, validate};
use crate::{
    auth::Principal,
    server::{Repository, Server},
};

const TTL: Duration = Duration::from_secs(300);

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
    publication: Publication,
    visibility_bases: BTreeMap<String, (String, gix_hash::ObjectId)>,
}

pub(super) async fn run(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
    directory: tempfile::TempDir,
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
    run_request(
        server,
        principal,
        key,
        directory,
        request,
        ReceiveInput {
            pack: Some(input),
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
    let directory = tokio::task::spawn_blocking(tempfile::tempdir).await??;
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
        .open_current(server, RepositoryOptions::default(), cancel)
        .await?;
    crab_remote::publication::with_leases(
        &entry.store,
        &entry.layout,
        [branch.to_owned()],
        TTL,
        cancel,
        |holders, cancel| async move {
            crab_remote::publication::with_internal_lease(
                &entry.store,
                &entry.layout,
                GIT_MANIFEST_RESOURCE,
                TTL,
                &cancel,
                |cancel| async move {
                    check_cancelled(&cancel)?;
                    let snapshot =
                        manifest_store::read_repository_snapshot(&entry.store, &entry.layout)
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
                    let evidence_hash =
                        git_visibility::upload_edit(&entry.store, &entry.layout, &evidence).await?;
                    check_cancelled(&cancel)?;
                    if !principal.can_admin(&entry.config) {
                        return Err(ReceiveError::Forbidden);
                    }
                    // Retargeting HEAD needs a journal parent and branch lease. This no-op
                    // ref edit preserves the branch's immutable visibility closure.
                    crab_write::journal::commit_edits(
                        &entry.store,
                        &entry.layout,
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
                    .open_current(server, RepositoryOptions::default(), &cancel)
                    .await?;
                Ok::<_, crate::Error>(repository.generation())
            })
            .await;
            Ok(())
        },
    )
    .await
}

pub(super) async fn publish_pack(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
    directory: tempfile::TempDir,
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
    directory: tempfile::TempDir,
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
    crab_remote::publication::with_leases(
        &entry.store,
        &entry.layout,
        names,
        TTL,
        cancel,
        |holders, cancel| async move {
            publish(
                server,
                principal,
                entry,
                &request,
                input,
                directory.path(),
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
    directory: &std::path::Path,
    holders: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    check_cancelled(cancel)?;
    if !principal.can_write(&entry.config) {
        return Err(ReceiveError::Forbidden);
    }
    let repository = entry
        .open_current(server, RepositoryOptions::default(), cancel)
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
    let protections = entry
        .branch_protections()
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
    let prepared = match validate::prepare(
        repository.clone(),
        entry.layout.clone(),
        directory.to_owned(),
        input.pack,
        request.updates.clone(),
        input.visibility_bases,
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
    let artifacts = prepared
        .upload(&snapshot, dependency_limits(), holders, cancel)
        .await
        .map_err(validate::map_error)?;
    check_cancelled(cancel)?;
    if !principal.can_write(&entry.config) {
        return Err(ReceiveError::Forbidden);
    }
    if entry
        .lifecycle()
        .await
        .map_err(|error| ReceiveError::Settings(Box::new(error)))?
        .archived
    {
        return Err(ReceiveError::Archived);
    }
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
    let outcome = artifacts
        .commit(head, crab_write::journal::CommitOptions::new(TTL, cancel))
        .await
        .map_err(validate::map_error)?;
    if let crab_remote::publication::CommitOutcome::Indeterminate { source, .. } = outcome {
        // Native Git has no durable client recovery token. Fail transport without
        // emitting a per-ref rejection for a potentially committed marker.
        return Err(ReceiveError::Write(*source));
    }
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
