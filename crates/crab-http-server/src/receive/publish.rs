use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    time::Duration,
};

use crab_coordination::{CoordinationError, GcFenceHeartbeat, GcFenceLease, PushLock};
use crab_git::receive_wire;
use crab_metadata::{
    git_visibility, manifest_store, manifests::PackManifestEntry, ref_journal::RefJournalEdit,
};
use crab_read::{
    dependency_proof::{DependencyProofLimits, verify_dependencies},
    pointer_proof::PointerProofLimits,
};
use crab_remote_git::RepositoryOptions;
use tokio_util::sync::CancellationToken;

use super::{ReceiveError, Result, check_cancelled, validate};
use crate::{
    auth::Principal,
    server::{Repository, Server},
};

const TTL: Duration = Duration::from_secs(300);

struct RefLease {
    name: String,
    holder: String,
    stop: CancellationToken,
    worker: tokio::task::JoinHandle<std::result::Result<(), CoordinationError>>,
}

impl RefLease {
    async fn acquire(entry: &Repository, name: &str, cancel: &CancellationToken) -> Result<Self> {
        check_cancelled(cancel)?;
        let mut lock =
            PushLock::acquire_ref(entry.store.inner(), entry.layout.repo_prefix(), name, TTL)
                .await?;
        let holder = lock.holder().to_owned();
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let cancel = cancel.clone();
        let worker = tokio::spawn(async move {
            let result = crab_coordination::while_renewing(&mut lock, Some(&cancel), async {
                stopped.cancelled().await;
                Ok::<_, CoordinationError>(())
            })
            .await;
            result.and(lock.release().await)
        });
        Ok(Self {
            name: name.to_owned(),
            holder,
            stop,
            worker,
        })
    }
    async fn release(self) {
        self.stop.cancel();
        match self.worker.await {
            Ok(Ok(())) => {}
            result => tracing::warn!(?result, "receive ref lease cleanup failed"),
        }
    }
}

pub(super) async fn run(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
    directory: tempfile::TempDir,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    let entry = server.repositories.get(key).ok_or(ReceiveError::NotFound)?;
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
    let mut leases = Vec::new();
    let mut fences = Vec::new();
    let result = async {
        // Ordered ref admission matches native CLI writers. Fail promptly on
        // contention; partial acquisition is released by the outer cleanup.
        let names: std::collections::BTreeSet<_> = request
            .updates
            .iter()
            .map(|update| update.name.as_str())
            .collect();
        for name in names {
            leases.push(RefLease::acquire(entry, name, cancel).await?);
        }
        for domain in [entry.layout.global_prefix(), entry.layout.repo_prefix()] {
            check_cancelled(cancel)?;
            let lease = GcFenceLease::acquire_writer(entry.store.inner(), domain, TTL).await?;
            let heartbeat = GcFenceHeartbeat::spawn(&lease, cancel.clone(), TTL / 3);
            fences.push((lease, heartbeat));
        }
        publish(
            server,
            principal,
            entry,
            &request,
            input,
            directory.path(),
            &leases,
            cancel,
        )
        .await
    }
    .await;
    // Publication may already have committed. Cleanup errors are diagnostics,
    // never an invented per-ref rejection, and every acquired lease is drained.
    for (lease, heartbeat) in fences.into_iter().rev() {
        heartbeat.stop().await;
        if let Err(error) = lease.release().await {
            tracing::warn!(%error, "receive GC fence release failed");
        }
    }
    for lease in leases.into_iter().rev() {
        lease.release().await;
    }
    result
}

async fn publish(
    server: &Server,
    principal: &Principal,
    entry: &Repository,
    request: &receive_wire::ReceiveRequest,
    input: BufReader<std::fs::File>,
    directory: &std::path::Path,
    leases: &[RefLease],
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
    let prepared = match validate::prepare(
        repository.clone(),
        directory.to_owned(),
        input,
        request.updates.clone(),
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
    verify_dependencies(
        &entry.layout,
        &snapshot,
        prepared.plan.pointers(),
        dependency_limits(),
        cancel,
    )
    .await
    .map_err(|error| ReceiveError::Dependency(Box::new(error)))?;
    let mut packs = Vec::new();
    if let Some(pack) = &prepared.pack {
        let pack_id = pack.content_hash().to_hex().to_string();
        entry
            .store
            .put_multipart_file_retry(
                &entry.layout.pack_path(&pack_id),
                pack.pack_path(),
                pack.size(),
                *pack.content_hash().as_bytes(),
                8 * 1024 * 1024,
                cancel,
                None,
            )
            .await?;
        for (source, target) in [
            (pack.index_path(), entry.layout.pack_index_path(&pack_id)),
            (
                pack.reverse_path(),
                entry.layout.pack_reverse_index_path(&pack_id),
            ),
            (
                pack.kinds_path(),
                entry.layout.pack_kind_metadata_path(&pack_id),
            ),
        ] {
            check_cancelled(cancel)?;
            let bytes = tokio::fs::read(source).await?;
            entry.store.put_exact(&target, bytes.into()).await?;
        }
        packs.push(PackManifestEntry {
            pack_id: pack_id.clone(),
            content_hash: pack_id,
            size: pack.size(),
            object_count: pack.object_count().into(),
            ref_tips: request
                .updates
                .iter()
                .filter_map(|update| update.new.map(|oid| oid.to_string()))
                .collect(),
        });
    }
    let mut edits = Vec::new();
    for update in &request.updates {
        check_cancelled(cancel)?;
        let evidence = match prepared.visibility.get(&update.name) {
            Some(proof) => {
                Some(git_visibility::upload_edit(&entry.store, &entry.layout, proof).await?)
            }
            None => None,
        };
        edits.push(RefJournalEdit {
            ref_name: update.name.clone(),
            old_oid: update.old.map(|oid| oid.to_string()),
            new_oid: update.new.map(|oid| oid.to_string()),
            peeled_oid: prepared
                .plan
                .peeled()
                .get(&update.name)
                .map(ToString::to_string),
            lock_holder: leases
                .iter()
                .find(|lease| lease.name == update.name)
                .map(|lease| lease.holder.clone()),
            visibility_evidence_hash: evidence,
        });
    }
    check_cancelled(cancel)?;
    if !principal.can_write(&entry.config) {
        return Err(ReceiveError::Forbidden);
    }
    if !repository.is_current(cancel).await? {
        return Err(ReceiveError::Request(
            "Repository changed before publication; retry",
        ));
    }
    let head = if prepared.plan.refs().is_empty()
        || prepared.plan.refs().contains_key(&snapshot.manifest.head)
    {
        None
    } else {
        // Tags can exist before the first branch. Keep HEAD unborn until a
        // branch is available instead of turning an arbitrary tag into HEAD.
        prepared
            .plan
            .refs()
            .keys()
            .find(|name| name.starts_with("refs/heads/"))
            .cloned()
    };
    crab_write::journal::commit_edits(
        &entry.store,
        &entry.layout,
        &snapshot,
        edits,
        head,
        packs,
        vec![],
        cancel,
    )
    .await?;
    // From this point no failure means rejection. A disconnected/cancelled
    // caller can reconcile refs; on-demand maintenance repairs committed work.
    entry.invalidate().await;
    entry
        .open_current(server, RepositoryOptions::default(), cancel)
        .await?;
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
            max_file_bytes: 512 * 1024 * 1024,
            max_shard_bytes: 128 * 1024 * 1024,
            max_xorb_bytes: 128 * 1024 * 1024,
            max_read_bytes: 2 * 1024 * 1024 * 1024,
            max_chunks: 1_000_000,
            max_duration: Duration::from_secs(60),
        },
    }
}
