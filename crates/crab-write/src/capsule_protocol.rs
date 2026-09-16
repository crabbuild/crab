//! Capsule publication through independently mutable ref heads and transaction records.

use crab_metadata::capsule_protocol::{
    Capsule, CapsulePointer, CapsuleRun, CapsuleTransaction, Checkpoint, CheckpointPointer,
    RepositoryRoot, RootRecord, create_root, load_root,
};
use crab_storage::{ETag, StorageError, Store, StoreLayout};
use futures_util::future::try_join_all;

use crate::{Result, WriteError};

pub use crab_metadata::capsule_protocol::RootSnapshot;

/// Initialize a capsule-protocol repository with one unborn generation-zero root.
pub async fn initialize(
    router: &StoreLayout<Store>,
    repository_id: &str,
    head: &str,
) -> Result<RootSnapshot> {
    let record = RootRecord::encode(RepositoryRoot::initial(repository_id, head)?)?;
    match load_root(router).await {
        Ok(existing) => return Ok(existing),
        Err(crab_metadata::error::MetadataError::Storage {
            source: StorageError::NotFound { .. },
        }) => {}
        Err(error) => return Err(error.into()),
    }
    let prefix =
        object_store::path::Path::from(format!("{}/", router.repo_prefix().trim_end_matches('/')));
    let existing = router.store().list_prefix_bounded(&prefix, 1).await?;
    if !existing.is_some_and(|objects| objects.is_empty()) {
        return match load_root(router).await {
            Ok(root) => Ok(root),
            Err(crab_metadata::error::MetadataError::Storage {
                source: StorageError::NotFound { .. },
            }) => Err(WriteError::CorruptObject {
                path: prefix.to_string(),
                reason: "repository prefix contains data but has no capsule-protocol root; Crab left it unchanged"
                    .to_owned(),
            }),
            Err(error) => Err(error.into()),
        };
    }
    match create_root(router, record).await {
        Ok(created) => Ok(created),
        Err(crab_metadata::error::MetadataError::Storage {
            source: StorageError::StateConflict { .. },
        }) => Ok(load_root(router).await?),
        Err(error) => Err(error.into()),
    }
}

/// Open and verify the checkpoint root used as the base of per-ref state.
pub async fn open_root(router: &StoreLayout<Store>) -> Result<RootSnapshot> {
    Ok(load_root(router).await?)
}

/// Atomically retarget HEAD without serializing ordinary per-ref publication.
pub async fn retarget_head(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    expected_head: &str,
    head: &str,
) -> Result<RootSnapshot> {
    let next = base
        .record()
        .root()
        .retarget_head(base.record().digest(), expected_head, head)?;
    let candidate = RootRecord::encode(next)?;
    let root_path = router.capsule_root_path();
    match router
        .store()
        .update(&root_path, candidate.bytes().clone(), base.etag().clone())
        .await
    {
        Ok(etag) => Ok(base.committed_head(candidate, etag)?),
        Err(StorageError::StateConflict { .. }) => Err(WriteError::CapsuleRootChanged {
            path: root_path.to_string(),
        }),
        Err(source) => {
            let verification = open_root(router).await;
            match verification {
                Ok(snapshot) if snapshot.record().digest() == candidate.digest() => Ok(snapshot),
                Ok(snapshot) if snapshot.record().digest() == base.record().digest() => {
                    Err(source.into())
                }
                Ok(_) => Err(WriteError::CapsuleHeadCommitUncertain {
                    head: head.to_owned(),
                    source: Box::new(source),
                    verification: None,
                }),
                Err(verification) => Err(WriteError::CapsuleHeadCommitUncertain {
                    head: head.to_owned(),
                    source: Box::new(source),
                    verification: Some(Box::new(verification)),
                }),
            }
        }
    }
}

/// Publish one verified capsule through independently mutable per-ref heads.
///
/// A single-ref push commits at that ref's head CAS. Multi-ref pushes prepare
/// every head and become visible through one per-attempt transaction-record
/// CAS, so unrelated refs never contend on the repository root.
pub async fn publish(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    transaction: &CapsuleTransaction,
    capsule: &Capsule,
) -> Result<RootSnapshot> {
    validate_capsule_binding(&base, transaction, capsule)?;
    let transaction_id = transaction.id()?;
    let snapshots = try_join_all(
        transaction
            .edits()
            .iter()
            .map(|edit| read_ref_head(router, base.record().root(), edit.ref_name())),
    )
    .await?;
    for (edit, snapshot) in transaction.edits().iter().zip(&snapshots) {
        if snapshot.visible.oid() != edit.expected_old() {
            return Err(WriteError::RefChanged {
                ref_name: edit.ref_name().to_owned(),
                path: router
                    .capsule_ref_head_path(&crab_metadata::capsule_protocol::capsule_ref_name_key(
                        edit.ref_name(),
                    ))
                    .to_string(),
            });
        }
    }

    let prepared = try_join_all(transaction.edits().iter().zip(snapshots).map(
        |(edit, snapshot)| {
            prepare_ref_successor(router, snapshot, edit, &transaction_id, capsule.clone())
        },
    ))
    .await?;
    let mut runs = std::collections::BTreeMap::new();
    for (_, run) in &prepared {
        match runs.get(run.hash()) {
            Some(existing) if existing != run => {
                return Err(WriteError::Internal(
                    "capsule run hash names conflicting bodies".to_owned(),
                ));
            }
            Some(_) => {}
            None => {
                runs.insert(run.hash().to_owned(), run.clone());
            }
        }
    }
    try_join_all(runs.into_values().map(|run| async move {
        let path = router.capsule_path(run.hash());
        router
            .store()
            .put_if_absent_verified(&path, run.bytes().clone())
            .await
    }))
    .await?;
    let prepared = prepared
        .into_iter()
        .map(|(prepared, _)| prepared)
        .collect::<Vec<_>>();

    if prepared.len() == 1 && transaction.plan_id().is_none() {
        let prepared = prepared
            .into_iter()
            .next()
            .ok_or_else(|| WriteError::Internal("single-ref publication disappeared".to_owned()))?;
        commit_single_ref(router, prepared).await?;
        return Ok(base);
    }

    let activation_id = activation_id(&transaction_id);
    let plan_intent = match transaction.plan_id() {
        Some(_) => Some(
            crab_metadata::capsule_protocol::prepare_capsule_plan(
                router.store(),
                router,
                transaction,
                &activation_id,
            )
            .await?,
        ),
        None => None,
    };
    commit_multi_ref(router, &transaction_id, &activation_id, prepared).await?;
    if let Some(intent) = plan_intent {
        crab_metadata::capsule_protocol::publish_capsule_plan_receipt(
            router.store(),
            router,
            &intent,
        )
        .await?;
    }
    Ok(base)
}

/// Recheck the complete Git ref namespace inside the caller's conflict-domain lease.
pub async fn validate_ref_namespace(
    router: &StoreLayout<Store>,
    root: &RepositoryRoot,
    edits: &[crab_metadata::capsule_protocol::CapsuleRefEdit],
) -> Result<()> {
    let edited_names = edits
        .iter()
        .map(|edit| edit.ref_name())
        .collect::<std::collections::BTreeSet<_>>();
    let prefix = router.capsule_ref_heads_prefix();
    let objects = router
        .store()
        .list_prefix_bounded(
            &prefix,
            crab_metadata::capsule_protocol::MAX_CAPSULE_REF_HEADS,
        )
        .await?
        .ok_or_else(|| WriteError::Internal("capsule ref-head limit exceeded".to_owned()))?;
    let prefix = format!("{prefix}/");
    let mut names = Vec::new();
    for object in objects {
        let key = object
            .location
            .as_ref()
            .strip_prefix(&prefix)
            .and_then(|name| name.strip_suffix(".json"))
            .ok_or_else(|| WriteError::CorruptObject {
                path: object.location.to_string(),
                reason: "capsule ref-head key has an invalid shape".to_owned(),
            })?;
        let name = crab_metadata::capsule_protocol::capsule_ref_name_from_key(key)?;
        if edited_names.iter().any(|edited| {
            name.as_str() == *edited
                || name
                    .strip_prefix(*edited)
                    .is_some_and(|suffix| suffix.starts_with('/'))
                || edited
                    .strip_prefix(&name)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            names.push(name);
        }
    }
    let heads = try_join_all(names.iter().map(|name| read_ref_head(router, root, name))).await?;
    let mut refs = root.refs().clone();
    for head in heads {
        match head.visible.oid() {
            Some(oid) => {
                refs.insert(head.head.ref_name().to_owned(), oid.to_owned());
            }
            None => {
                refs.remove(head.head.ref_name());
            }
        }
    }
    for edit in edits {
        match edit.new_oid() {
            Some(oid) => {
                refs.insert(edit.ref_name().to_owned(), oid.to_owned());
            }
            None => {
                refs.remove(edit.ref_name());
            }
        }
    }
    crab_git::refname::validate_ref_namespace(refs.keys().map(String::as_str))?;
    Ok(())
}

#[derive(Debug, Clone)]
struct RefHeadSnapshot {
    head: crab_metadata::capsule_protocol::CapsuleRefHead,
    visible: crab_metadata::capsule_protocol::CapsuleRefState,
    active: std::collections::BTreeSet<String>,
    etag: Option<ETag>,
}

#[derive(Debug)]
struct PreparedRefHead {
    original: RefHeadSnapshot,
    candidate: crab_metadata::capsule_protocol::CapsuleRefHead,
}

async fn read_ref_head(
    router: &StoreLayout<Store>,
    root: &RepositoryRoot,
    ref_name: &str,
) -> Result<RefHeadSnapshot> {
    let path = router.capsule_ref_head_path(
        &crab_metadata::capsule_protocol::capsule_ref_name_key(ref_name),
    );
    let (head, etag) = match router.store().get_with_etag(&path).await {
        Ok((body, etag)) => {
            let head = crab_metadata::capsule_protocol::CapsuleRefHead::decode(&body)?;
            if head.ref_name() != ref_name {
                return Err(WriteError::CorruptObject {
                    path: path.to_string(),
                    reason: "capsule ref-head key does not match its ref name".to_owned(),
                });
            }
            (head, Some(etag))
        }
        Err(StorageError::NotFound { .. }) => (
            crab_metadata::capsule_protocol::CapsuleRefHead::from_root(
                ref_name,
                root.refs().get(ref_name).cloned(),
                root.peeled_refs().get(ref_name).cloned(),
            )?,
            None,
        ),
        Err(source) => return Err(source.into()),
    };
    let mut active = std::collections::BTreeSet::new();
    if let Some(activation_id) = head.prepared_activation_id()
        && resolve_prepared_activation(router, activation_id).await?
    {
        active.insert(activation_id.to_owned());
    }
    let visible = head.visible(&active).clone();
    Ok(RefHeadSnapshot {
        head,
        visible,
        active,
        etag,
    })
}

async fn resolve_prepared_activation(
    router: &StoreLayout<Store>,
    activation_id: &str,
) -> Result<bool> {
    let path = router.capsule_transaction_path(activation_id);
    loop {
        let (body, etag) = router
            .store()
            .get_with_etag_bounded(
                &path,
                crab_metadata::capsule_protocol::MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
            )
            .await?;
        let record = crab_metadata::capsule_protocol::CapsuleTransactionRecord::decode(&body)?;
        if record.activation_id() != activation_id {
            return Err(WriteError::CorruptObject {
                path: path.to_string(),
                reason: "transaction record key does not match its activation id".to_owned(),
            });
        }
        match record.status() {
            crab_metadata::capsule_protocol::CapsuleTransactionStatus::Committed => {
                let marker_path = router.capsule_committed_transaction_path(activation_id);
                let marker = router
                    .store()
                    .get_with_etag_bounded(
                        &marker_path,
                        crab_metadata::capsule_protocol::MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
                    )
                    .await;
                let (marker, _) = match marker {
                    Ok(marker) => marker,
                    Err(StorageError::NotFound { .. }) => {
                        let body = record.encode()?;
                        let created = router
                            .store()
                            .put_if_absent_verified(&marker_path, body.clone())
                            .await
                            .map_err(|source| WriteError::CapsuleCommitUncertain {
                                transaction_id: record.transaction_id().to_owned(),
                                source: Box::new(source),
                                verification: None,
                            })?;
                        if !created {
                            let (actual, _) = router
                                .store()
                                .get_with_etag_bounded(
                                    &marker_path,
                                    crab_metadata::capsule_protocol::MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
                                )
                                .await?;
                            if actual != body {
                                return Err(WriteError::CorruptObject {
                                    path: marker_path.to_string(),
                                    reason:
                                        "committed marker conflicts with its transaction record"
                                            .to_owned(),
                                });
                            }
                        }
                        return Ok(true);
                    }
                    Err(source) => {
                        return Err(WriteError::CapsuleCommitUncertain {
                            transaction_id: record.transaction_id().to_owned(),
                            source: Box::new(source),
                            verification: None,
                        });
                    }
                };
                let marker =
                    crab_metadata::capsule_protocol::CapsuleTransactionRecord::decode(&marker)?;
                if marker != record {
                    return Err(WriteError::CorruptObject {
                        path: marker_path.to_string(),
                        reason: "committed marker does not match its transaction record".to_owned(),
                    });
                }
                return Ok(true);
            }
            crab_metadata::capsule_protocol::CapsuleTransactionStatus::Aborted => {
                return Ok(false);
            }
            crab_metadata::capsule_protocol::CapsuleTransactionStatus::Preparing => {
                let aborted = record.abort()?;
                match router.store().update(&path, aborted.encode()?, etag).await {
                    Ok(_) => return Ok(false),
                    Err(StorageError::StateConflict { .. }) => continue,
                    Err(source) => {
                        let (actual, _) = match router
                            .store()
                            .get_with_etag_bounded(
                                &path,
                                crab_metadata::capsule_protocol::MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
                            )
                            .await
                        {
                            Ok(actual) => actual,
                            Err(_) => return Err(source.into()),
                        };
                        let actual =
                            crab_metadata::capsule_protocol::CapsuleTransactionRecord::decode(
                                &actual,
                            )?;
                        return match actual.status() {
                            crab_metadata::capsule_protocol::CapsuleTransactionStatus::Committed => Ok(true),
                            crab_metadata::capsule_protocol::CapsuleTransactionStatus::Aborted => Ok(false),
                            crab_metadata::capsule_protocol::CapsuleTransactionStatus::Preparing => Err(source.into()),
                        };
                    }
                }
            }
        }
    }
}

async fn prepare_ref_successor(
    router: &StoreLayout<Store>,
    snapshot: RefHeadSnapshot,
    edit: &crab_metadata::capsule_protocol::CapsuleRefEdit,
    transaction_id: &str,
    capsule: Capsule,
) -> Result<(PreparedRefHead, CapsuleRun)> {
    let mut run = CapsuleRun::leaf(capsule)?;
    let mut frontier = snapshot.visible.frontier().to_vec();
    while frontier.last().is_some_and(|pointer| {
        pointer.level() == run.level()
            && run.capsules().len() < crab_metadata::capsule_protocol::MAX_CAPSULES_PER_RUN
    }) {
        let pointer = frontier
            .pop()
            .ok_or_else(|| WriteError::Internal("capsule ref frontier became empty".to_owned()))?;
        let older = load_run(router, &pointer).await?;
        run = older.merge(&run)?;
    }
    frontier.push(CapsulePointer::new(
        run.hash(),
        run.bytes().len() as u64,
        run.level(),
        run.transaction_ids(),
        run.newest_base_root_digest(),
    )?);
    let state = snapshot.head.successor_state(
        &snapshot.active,
        edit.new_oid().map(str::to_owned),
        edit.peeled_oid().map(str::to_owned),
        transaction_id.to_owned(),
        frontier,
    )?;
    let candidate = snapshot.head.commit(state)?;
    Ok((
        PreparedRefHead {
            original: snapshot,
            candidate,
        },
        run,
    ))
}

async fn commit_single_ref(router: &StoreLayout<Store>, prepared: PreparedRefHead) -> Result<()> {
    write_ref_head(router, &prepared.original, &prepared.candidate)
        .await
        .map(|_| ())
}

async fn commit_multi_ref(
    router: &StoreLayout<Store>,
    transaction_id: &str,
    activation_id: &str,
    prepared: Vec<PreparedRefHead>,
) -> Result<()> {
    let conflict_ref = prepared
        .first()
        .map(|item| item.original.head.ref_name().to_owned())
        .ok_or_else(|| WriteError::Internal("multi-ref publication has no refs".to_owned()))?;
    let path = router.capsule_transaction_path(activation_id);
    let preparing = crab_metadata::capsule_protocol::CapsuleTransactionRecord::preparing(
        activation_id.to_owned(),
        transaction_id.to_owned(),
    )?;
    let preparing_body = preparing.encode()?;
    let record_etag = match router
        .store()
        .create_strict_with_etag(&path, preparing_body.clone())
        .await
    {
        Ok(etag) => etag,
        Err(source) => match router
            .store()
            .get_with_etag_bounded(&path, preparing_body.len() as u64)
            .await
        {
            Ok((actual, etag)) if actual == preparing_body => etag,
            _ => return Err(source.into()),
        },
    };
    let mut written = Vec::with_capacity(prepared.len());
    for item in prepared {
        let new_state = item
            .candidate
            .visible(&std::collections::BTreeSet::new())
            .clone();
        let candidate = item.original.head.prepare(
            item.original.visible.clone(),
            activation_id.to_owned(),
            new_state,
        )?;
        match write_ref_head(router, &item.original, &candidate).await {
            Ok(etag) => written.push((item.original, candidate, etag)),
            Err(error) => {
                abort_transaction(router, &path, &preparing, record_etag).await;
                rollback_ref_heads(router, &written).await;
                return Err(error);
            }
        }
    }

    let committed = preparing.commit()?;
    let committed_body = committed.encode()?;
    if let Err(source) = router
        .store()
        .update(&path, committed_body.clone(), record_etag)
        .await
    {
        let actual = router
            .store()
            .get_with_etag_bounded(
                &path,
                crab_metadata::capsule_protocol::MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
            )
            .await;
        match actual {
            Ok((actual, _)) if actual == committed_body => {}
            Ok((actual, _)) => {
                let actual =
                    crab_metadata::capsule_protocol::CapsuleTransactionRecord::decode(&actual)?;
                rollback_ref_heads(router, &written).await;
                if actual.activation_id() == activation_id
                    && actual.transaction_id() == transaction_id
                    && actual.status()
                        == crab_metadata::capsule_protocol::CapsuleTransactionStatus::Aborted
                {
                    return Err(WriteError::RefChanged {
                        ref_name: conflict_ref,
                        path: path.to_string(),
                    });
                }
                return Err(WriteError::CapsuleCommitUncertain {
                    transaction_id: transaction_id.to_owned(),
                    source: Box::new(source),
                    verification: None,
                });
            }
            Err(verification) => {
                rollback_ref_heads(router, &written).await;
                return Err(WriteError::CapsuleCommitUncertain {
                    transaction_id: transaction_id.to_owned(),
                    source: Box::new(source),
                    verification: Some(Box::new(verification.into())),
                });
            }
        }
    }

    let marker_path = router.capsule_committed_transaction_path(activation_id);
    let marker_created = match router
        .store()
        .put_if_absent_verified(&marker_path, committed_body.clone())
        .await
    {
        Ok(created) => created,
        Err(source) => {
            return Err(WriteError::CapsuleCommitUncertain {
                transaction_id: transaction_id.to_owned(),
                source: Box::new(source),
                verification: None,
            });
        }
    };
    if !marker_created {
        let (actual, _) = router
            .store()
            .get_with_etag_bounded(
                &marker_path,
                crab_metadata::capsule_protocol::MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
            )
            .await
            .map_err(|source| WriteError::CapsuleCommitUncertain {
                transaction_id: transaction_id.to_owned(),
                source: Box::new(source),
                verification: None,
            })?;
        if actual != committed_body {
            return Err(WriteError::CorruptObject {
                path: marker_path.to_string(),
                reason: "committed marker conflicts with its transaction record".to_owned(),
            });
        }
    }

    // Prepared heads remain in two-version form. Readers resolve this atomic
    // record once while double-collecting head versions, preserving all-old/all-new.
    Ok(())
}

fn activation_id(transaction_id: &str) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("crab capsule activation id v2");
    hasher.update(transaction_id.as_bytes());
    hasher.update(uuid::Uuid::now_v7().as_bytes());
    hasher.finalize().to_hex().to_string()
}

async fn abort_transaction(
    router: &StoreLayout<Store>,
    path: &object_store::path::Path,
    preparing: &crab_metadata::capsule_protocol::CapsuleTransactionRecord,
    etag: ETag,
) {
    let result = match preparing.abort().and_then(|record| record.encode()) {
        Ok(body) => router.store().update(path, body, etag).await,
        Err(error) => {
            tracing::warn!(%error, "could not encode capsule transaction abort");
            return;
        }
    };
    if let Err(error) = result {
        tracing::warn!(%error, "capsule transaction abort needs reconciliation");
    }
}

async fn write_ref_head(
    router: &StoreLayout<Store>,
    original: &RefHeadSnapshot,
    candidate: &crab_metadata::capsule_protocol::CapsuleRefHead,
) -> Result<ETag> {
    let path = router.capsule_ref_head_path(
        &crab_metadata::capsule_protocol::capsule_ref_name_key(original.head.ref_name()),
    );
    let body = candidate.encode()?;
    let result = match &original.etag {
        Some(etag) => {
            router
                .store()
                .update(&path, body.clone(), etag.clone())
                .await
        }
        None => {
            router
                .store()
                .create_strict_with_etag(&path, body.clone())
                .await
        }
    };
    match result {
        Ok(etag) => Ok(etag),
        Err(StorageError::StateConflict { .. }) => Err(WriteError::RefChanged {
            ref_name: original.head.ref_name().to_owned(),
            path: path.to_string(),
        }),
        Err(source) => match router
            .store()
            .get_with_etag_bounded(&path, body.len() as u64)
            .await
        {
            Ok((actual, etag)) if actual == body => Ok(etag),
            _ => Err(source.into()),
        },
    }
}

async fn rollback_ref_heads(
    router: &StoreLayout<Store>,
    written: &[(
        RefHeadSnapshot,
        crab_metadata::capsule_protocol::CapsuleRefHead,
        ETag,
    )],
) {
    for (original, candidate, etag) in written.iter().rev() {
        let path = router.capsule_ref_head_path(
            &crab_metadata::capsule_protocol::capsule_ref_name_key(original.head.ref_name()),
        );
        let result = match original.head.encode() {
            Ok(body) => router.store().update(&path, body, etag.clone()).await,
            Err(error) => {
                tracing::warn!(ref_name = %original.head.ref_name(), %error, "could not encode capsule ref-head rollback");
                continue;
            }
        };
        if let Err(error) = result {
            tracing::warn!(ref_name = %candidate.ref_name(), %error, "capsule ref-head rollback needs repair");
        }
    }
}

/// Publish a complete checkpoint and atomically replace the covered root's frontier.
pub async fn publish_checkpoint(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    checkpoint: &Checkpoint,
) -> Result<RootSnapshot> {
    publish_checkpoint_inner(router, base, checkpoint, None).await
}

/// Publish a checkpoint and fold the exact captured per-ref positions into the root.
pub async fn publish_ref_checkpoint(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    checkpoint: &Checkpoint,
    refs: std::collections::BTreeMap<String, String>,
    peeled_refs: std::collections::BTreeMap<String, String>,
    compacted_ref_transactions: std::collections::BTreeMap<String, String>,
) -> Result<RootSnapshot> {
    publish_checkpoint_inner(
        router,
        base,
        checkpoint,
        Some((refs, peeled_refs, compacted_ref_transactions)),
    )
    .await
}

type CheckpointRefState = (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
);

async fn publish_checkpoint_inner(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    checkpoint: &Checkpoint,
    ref_state: Option<CheckpointRefState>,
) -> Result<RootSnapshot> {
    if let Some(fence) = base.record().root().gc_fence() {
        return Err(WriteError::CapsuleGcFenced {
            fence_id: fence.id().to_owned(),
            expires_at_unix: fence.expires_at_unix(),
        });
    }
    if checkpoint.covered_generation() != base.record().root().generation()
        || checkpoint.covered_root_digest() != base.record().digest()
    {
        return Err(WriteError::CorruptObject {
            path: "capsule-protocol checkpoint".to_owned(),
            reason: "checkpoint does not cover the exact CAS base".to_owned(),
        });
    }
    let object_count = checkpoint
        .git_packs()
        .iter()
        .try_fold(0_u64, |total, pack| {
            total.checked_add(pack.object_count()).ok_or_else(|| {
                WriteError::Internal("checkpoint object count overflowed".to_owned())
            })
        })?;
    let pack_count = u32::try_from(checkpoint.git_packs().len())
        .map_err(|_| WriteError::Internal("checkpoint pack count overflowed".to_owned()))?;
    let path = router.capsule_checkpoint_path(checkpoint.hash());
    router
        .store()
        .put_if_absent_verified(&path, checkpoint.bytes().clone())
        .await?;
    let pointer = CheckpointPointer::new(
        checkpoint.hash(),
        checkpoint.bytes().len() as u64,
        checkpoint.covered_generation(),
        checkpoint.covered_root_digest(),
        pack_count,
        object_count,
    )?;
    let is_ref_checkpoint = ref_state.is_some();
    let next = match ref_state {
        Some((refs, peeled_refs, compacted_ref_transactions)) => {
            base.record().root().install_ref_checkpoint(
                base.record().digest(),
                pointer,
                refs,
                peeled_refs,
                compacted_ref_transactions,
            )?
        }
        None => base
            .record()
            .root()
            .install_checkpoint(base.record().digest(), pointer)?,
    };
    let candidate = RootRecord::encode(next)?;
    let root_path = router.capsule_root_path();
    match router
        .store()
        .update(&root_path, candidate.bytes().clone(), base.etag().clone())
        .await
    {
        Ok(etag) if is_ref_checkpoint => Ok(base.committed_ref_checkpoint(candidate, etag)?),
        Ok(etag) => Ok(base.committed_checkpoint(candidate, etag)?),
        Err(StorageError::StateConflict { .. }) => Err(WriteError::CapsuleRootChanged {
            path: root_path.to_string(),
        }),
        Err(source) => {
            reconcile_checkpoint_update(router, base.record(), candidate, checkpoint, source).await
        }
    }
}

/// Atomically fence a root for one exclusive GC sweep.
pub async fn begin_gc(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    fence: crab_metadata::capsule_protocol::GcFence,
) -> Result<RootSnapshot> {
    let fence_id = fence.id().to_owned();
    let next = base
        .record()
        .root()
        .begin_gc(base.record().digest(), fence)?;
    update_maintenance_root(router, base, RootRecord::encode(next)?, &fence_id).await
}

/// Atomically clear the exact GC fence after a sweep.
pub async fn end_gc(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    fence_id: &str,
) -> Result<RootSnapshot> {
    let next = base
        .record()
        .root()
        .end_gc(base.record().digest(), fence_id)?;
    update_maintenance_root(router, base, RootRecord::encode(next)?, fence_id).await
}

async fn update_maintenance_root(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    candidate: RootRecord,
    fence_id: &str,
) -> Result<RootSnapshot> {
    let root_path = router.capsule_root_path();
    match router
        .store()
        .update(&root_path, candidate.bytes().clone(), base.etag().clone())
        .await
    {
        Ok(etag) => Ok(base.committed_maintenance(candidate, etag)?),
        Err(StorageError::StateConflict { .. }) => Err(WriteError::CapsuleRootChanged {
            path: root_path.to_string(),
        }),
        Err(source) => {
            let verification = open_root(router).await;
            match verification {
                Ok(snapshot) if snapshot.record().digest() == candidate.digest() => Ok(snapshot),
                Ok(snapshot) if snapshot.record().digest() == base.record().digest() => {
                    Err(source.into())
                }
                Ok(_) => Err(WriteError::CapsuleMaintenanceCommitUncertain {
                    fence_id: fence_id.to_owned(),
                    source: Box::new(source),
                    verification: None,
                }),
                Err(verification) => Err(WriteError::CapsuleMaintenanceCommitUncertain {
                    fence_id: fence_id.to_owned(),
                    source: Box::new(source),
                    verification: Some(Box::new(verification)),
                }),
            }
        }
    }
}

async fn load_run(router: &StoreLayout<Store>, pointer: &CapsulePointer) -> Result<CapsuleRun> {
    let path = router.capsule_path(pointer.hash());
    let (bytes, _) = router
        .store()
        .get_with_etag_bounded(&path, pointer.size())
        .await?;
    let actual_size = u64::try_from(bytes.len())
        .map_err(|_| WriteError::Internal("capsule run size cannot be represented".to_owned()))?;
    let run = CapsuleRun::decode(bytes)?;
    if actual_size != pointer.size()
        || run.hash() != pointer.hash()
        || run.level() != pointer.level()
        || run.transaction_ids() != pointer.transaction_ids()
        || run.newest_base_root_digest() != pointer.newest_base_root_digest()
    {
        return Err(WriteError::CorruptObject {
            path: path.to_string(),
            reason: "capsule run does not match its authenticated root pointer".to_owned(),
        });
    }
    Ok(run)
}

fn validate_capsule_binding(
    base: &RootSnapshot,
    transaction: &CapsuleTransaction,
    capsule: &Capsule,
) -> Result<()> {
    if let Some(fence) = base.record().root().gc_fence() {
        return Err(WriteError::CapsuleGcFenced {
            fence_id: fence.id().to_owned(),
            expires_at_unix: fence.expires_at_unix(),
        });
    }
    if transaction.base_root_digest() != base.record().digest()
        || capsule.base_root_digest() != base.record().digest()
        || capsule.transaction_id() != transaction.id()?
    {
        return Err(WriteError::CorruptObject {
            path: "capsule-protocol capsule".to_owned(),
            reason: "capsule, transaction, and advertised root are not cryptographically bound"
                .to_owned(),
        });
    }
    Ok(())
}

async fn reconcile_checkpoint_update(
    router: &StoreLayout<Store>,
    base: &RootRecord,
    candidate: RootRecord,
    checkpoint: &Checkpoint,
    source: StorageError,
) -> Result<RootSnapshot> {
    let verification = open_root(router).await;
    match verification {
        Ok(snapshot) if snapshot.record().digest() == candidate.digest() => Ok(snapshot),
        Ok(snapshot) if snapshot.record().digest() == base.digest() => Err(source.into()),
        Ok(_) => Err(WriteError::CapsuleCheckpointCommitUncertain {
            checkpoint_hash: checkpoint.hash().to_owned(),
            source: Box::new(source),
            verification: None,
        }),
        Err(verification) => Err(WriteError::CapsuleCheckpointCommitUncertain {
            checkpoint_hash: checkpoint.hash().to_owned(),
            source: Box::new(source),
            verification: Some(Box::new(verification)),
        }),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use std::fmt;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use bytes::Bytes;
    use crab_metadata::capsule_protocol::{CapsuleGitPack, CapsuleRefEdit};
    use crab_storage::{
        ImmutableWriteVerification, StorageObservation, StorageObserver, StorageOperation,
        StorageOutcome,
    };
    use futures_util::stream::BoxStream;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult, memory::InMemory,
        path::Path,
    };

    use super::*;

    #[derive(Default)]
    struct RecordingObserver {
        observations: Mutex<Vec<StorageObservation>>,
    }

    impl StorageObserver for RecordingObserver {
        fn started(&self, _operation: StorageOperation) {}

        fn finished(&self, observation: StorageObservation) {
            self.observations.lock().unwrap().push(observation);
        }
    }

    #[derive(Debug)]
    struct LostHeadReplyStore {
        inner: Arc<InMemory>,
        head_path: String,
        lost: AtomicBool,
    }

    impl fmt::Display for LostHeadReplyStore {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("LostHeadReplyStore")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for LostHeadReplyStore {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            options: PutOptions,
        ) -> object_store::Result<PutResult> {
            let lose_reply = location.as_ref() == self.head_path
                && !matches!(options.mode, PutMode::Overwrite)
                && !self.lost.swap(true, Ordering::AcqRel);
            let result = self.inner.put_opts(location, payload, options).await?;
            if lose_reply {
                return Err(object_store::Error::Generic {
                    store: "capsule-protocol-root-test",
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "lost ref-head update reply",
                    )),
                });
            }
            Ok(result)
        }

        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            self.inner.get_opts(location, options).await
        }

        async fn put_multipart_opts(
            &self,
            location: &Path,
            options: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    fn transaction(base: &RootSnapshot, old: Option<&str>, new: &str) -> CapsuleTransaction {
        CapsuleTransaction::new(
            base.record().digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                old.map(str::to_owned),
                Some(new.to_owned()),
                None,
            )],
        )
        .unwrap()
    }

    fn multi_ref_transaction(base: &RootSnapshot) -> CapsuleTransaction {
        CapsuleTransaction::new(
            base.record().digest(),
            vec![
                CapsuleRefEdit::new("refs/heads/main", None, Some("2".repeat(40)), None),
                CapsuleRefEdit::new("refs/heads/feature", None, Some("3".repeat(40)), None),
            ],
        )
        .unwrap()
    }

    fn capsule(transaction: &CapsuleTransaction) -> Capsule {
        Capsule::build(
            transaction,
            vec![
                CapsuleGitPack::new(
                    Bytes::from_static(b"PACK capsule-protocol test"),
                    Bytes::from_static(b"index"),
                    Bytes::from_static(b"reverse"),
                    Bytes::from_static(b"locator"),
                    "4".repeat(40),
                    1,
                )
                .unwrap(),
            ],
            Vec::new(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn clean_publication_uses_four_requests_including_root_open() {
        let inner = Arc::new(InMemory::new());
        let seed_store = Store::new(inner.clone());
        let seed_router = StoreLayout::new(seed_store.clone(), "repositories/test".to_owned());
        initialize(&seed_router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();

        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner).with_storage_observer(observer.clone());
        let router = StoreLayout::new(store.clone(), "repositories/test".to_owned());
        let base = open_root(&router).await.unwrap();
        let transaction = transaction(&base, None, &"2".repeat(40));
        let capsule = capsule(&transaction);

        let published = publish(&router, base, &transaction, &capsule)
            .await
            .unwrap();

        assert_eq!(published.record().root().generation(), 0);
        let operations = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            vec![
                StorageOperation::Get,
                StorageOperation::Put,
                StorageOperation::Get,
                StorageOperation::Put,
            ]
        );
    }

    #[tokio::test]
    async fn checksum_qualified_publication_uses_three_requests_including_root_open() {
        let inner = Arc::new(InMemory::new());
        let seed_store = Store::new(inner.clone());
        let seed_router = StoreLayout::new(seed_store, "repositories/test".to_owned());
        initialize(&seed_router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();

        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner)
            .with_immutable_write_verification(ImmutableWriteVerification::Sha256Checksum)
            .with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        let base = open_root(&router).await.unwrap();
        let transaction = transaction(&base, None, &"2".repeat(40));
        let capsule = capsule(&transaction);

        let published = publish(&router, base, &transaction, &capsule)
            .await
            .unwrap();

        assert_eq!(published.record().root().generation(), 0);
        let operations = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            vec![
                StorageOperation::Get,
                StorageOperation::Put,
                StorageOperation::Put,
            ]
        );
    }

    #[tokio::test]
    async fn multi_ref_publication_commits_one_transaction_record() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let base = open_root(&router).await.unwrap();
        let transaction = multi_ref_transaction(&base);

        let published = publish(&router, base, &transaction, &capsule(&transaction))
            .await
            .unwrap();

        let records = router
            .store()
            .list_prefix_bounded(&router.capsule_transactions_prefix(), 2)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(records.len(), 1);
        let (record, _) = router
            .store()
            .get_with_etag_bounded(
                &records[0].location,
                crab_metadata::capsule_protocol::MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
            )
            .await
            .unwrap();
        let record =
            crab_metadata::capsule_protocol::CapsuleTransactionRecord::decode(&record).unwrap();
        assert_eq!(
            record.status(),
            crab_metadata::capsule_protocol::CapsuleTransactionStatus::Committed
        );
        for (ref_name, expected) in [
            ("refs/heads/main", "2".repeat(40)),
            ("refs/heads/feature", "3".repeat(40)),
        ] {
            let head = read_ref_head(&router, published.record().root(), ref_name)
                .await
                .unwrap();
            assert_eq!(head.visible.oid(), Some(expected.as_str()));
            assert_eq!(
                head.head.prepared_activation_id(),
                Some(record.activation_id())
            );
        }
    }

    #[tokio::test]
    async fn head_retarget_preserves_visible_per_ref_capsules() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        let base = initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let transaction = multi_ref_transaction(&base);
        publish(&router, base.clone(), &transaction, &capsule(&transaction))
            .await
            .unwrap();

        let retargeted = retarget_head(&router, base, "refs/heads/main", "refs/heads/feature")
            .await
            .unwrap();
        assert_eq!(retargeted.record().root().head(), "refs/heads/feature");
        for (ref_name, expected) in [
            ("refs/heads/main", "2".repeat(40)),
            ("refs/heads/feature", "3".repeat(40)),
        ] {
            let head = read_ref_head(&router, retargeted.record().root(), ref_name)
                .await
                .unwrap();
            assert_eq!(head.visible.oid(), Some(expected.as_str()));
        }
    }

    #[tokio::test]
    async fn planned_single_ref_uses_transaction_marker_and_repairs_its_receipt() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let base = open_root(&router).await.unwrap();
        let plan_id = "9".repeat(64);
        let transaction = CapsuleTransaction::for_plan(
            base.record().digest(),
            &plan_id,
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some("2".repeat(40)),
                None,
            )],
        )
        .unwrap();

        publish(&router, base, &transaction, &capsule(&transaction))
            .await
            .unwrap();
        let receipt = crab_metadata::capsule_protocol::resolve_capsule_plan_receipt(
            router.store(),
            &router,
            &plan_id,
        )
        .await
        .unwrap()
        .unwrap();
        router
            .store()
            .delete(&router.capsule_plan_receipt_path(&plan_id))
            .await
            .unwrap();
        router
            .store()
            .delete(&router.capsule_committed_transaction_path(receipt.activation_id()))
            .await
            .unwrap();

        let repaired = crab_metadata::capsule_protocol::resolve_capsule_plan_receipt(
            router.store(),
            &router,
            &plan_id,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(receipt, repaired);
        assert_eq!(
            repaired.transaction().id().unwrap(),
            transaction.id().unwrap()
        );
        router
            .store()
            .get_with_etag_bounded(
                &router.capsule_committed_transaction_path(repaired.activation_id()),
                crab_metadata::capsule_protocol::MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn aborting_a_prepared_attempt_prevents_late_multi_ref_commit() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let base = open_root(&router).await.unwrap();
        let transaction = transaction(&base, None, &"2".repeat(40));
        let transaction_id = transaction.id().unwrap();
        let snapshot = read_ref_head(&router, base.record().root(), "refs/heads/main")
            .await
            .unwrap();
        let (prepared, _) = prepare_ref_successor(
            &router,
            snapshot,
            &transaction.edits()[0],
            &transaction_id,
            capsule(&transaction),
        )
        .await
        .unwrap();
        let activation_id = "5".repeat(64);
        let record = crab_metadata::capsule_protocol::CapsuleTransactionRecord::preparing(
            activation_id.clone(),
            transaction_id.clone(),
        )
        .unwrap();
        let record_path = router.capsule_transaction_path(&activation_id);
        let record_etag = router
            .store()
            .create_strict_with_etag(&record_path, record.encode().unwrap())
            .await
            .unwrap();
        let state = prepared
            .candidate
            .visible(&std::collections::BTreeSet::new())
            .clone();
        let candidate = prepared
            .original
            .head
            .prepare(prepared.original.visible.clone(), activation_id, state)
            .unwrap();
        write_ref_head(&router, &prepared.original, &candidate)
            .await
            .unwrap();

        let visible = read_ref_head(&router, base.record().root(), "refs/heads/main")
            .await
            .unwrap();
        assert_eq!(visible.visible.oid(), None);
        let late_commit = router
            .store()
            .update(
                &record_path,
                record.commit().unwrap().encode().unwrap(),
                record_etag,
            )
            .await;
        assert!(matches!(
            late_commit,
            Err(StorageError::StateConflict { .. })
        ));
        let (stored, _) = router
            .store()
            .get_with_etag_bounded(
                &record_path,
                crab_metadata::capsule_protocol::MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
            )
            .await
            .unwrap();
        assert_eq!(
            crab_metadata::capsule_protocol::CapsuleTransactionRecord::decode(&stored)
                .unwrap()
                .status(),
            crab_metadata::capsule_protocol::CapsuleTransactionStatus::Aborted
        );

        publish(&router, base, &transaction, &capsule(&transaction))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn binary_carry_adds_one_get_without_an_intermediate_put() {
        let inner = Arc::new(InMemory::new());
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner)
            .with_immutable_write_verification(ImmutableWriteVerification::Sha256Checksum)
            .with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let base = open_root(&router).await.unwrap();
        let first = transaction(&base, None, &"2".repeat(40));
        publish(&router, base, &first, &capsule(&first))
            .await
            .unwrap();
        observer.observations.lock().unwrap().clear();

        let base = open_root(&router).await.unwrap();
        let second = transaction(&base, Some(&"2".repeat(40)), &"3".repeat(40));
        let published = publish(&router, base, &second, &capsule(&second))
            .await
            .unwrap();

        let head = read_ref_head(&router, published.record().root(), "refs/heads/main")
            .await
            .unwrap();
        assert_eq!(head.visible.frontier().len(), 1);
        assert_eq!(head.visible.frontier()[0].level(), 1);
        let operations = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            vec![
                StorageOperation::Get,
                StorageOperation::Get,
                StorageOperation::Get,
                StorageOperation::Put,
                StorageOperation::Put,
                StorageOperation::Get,
            ]
        );
    }

    #[tokio::test]
    async fn capped_runs_support_more_than_five_hundred_pushes_under_five_requests_average() {
        let inner = Arc::new(InMemory::new());
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner)
            .with_immutable_write_verification(ImmutableWriteVerification::Sha256Checksum)
            .with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        observer.observations.lock().unwrap().clear();

        let mut previous = None;
        let mut published = None;
        for sequence in 1..=1_025_u64 {
            let base = open_root(&router).await.unwrap();
            let next = format!("{sequence:040x}");
            let transaction = transaction(&base, previous.as_deref(), &next);
            published = Some(
                publish(&router, base, &transaction, &capsule(&transaction))
                    .await
                    .unwrap(),
            );
            previous = Some(next);
        }

        let published = published.unwrap();
        assert_eq!(published.record().root().generation(), 0);
        let head = read_ref_head(&router, published.record().root(), "refs/heads/main")
            .await
            .unwrap();
        assert_eq!(
            head.visible
                .frontier()
                .iter()
                .map(CapsulePointer::level)
                .collect::<Vec<_>>(),
            vec![9, 9, 0]
        );
        let request_count = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .count();
        assert!(request_count <= 5_125, "request count was {request_count}");
        assert!((request_count as f64 / 1_025.0) <= 5.0);
    }

    #[tokio::test]
    async fn stale_same_ref_cannot_publish_over_a_winner() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "repositories/test".to_owned());
        initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let first_base = open_root(&router).await.unwrap();
        let stale_base = open_root(&router).await.unwrap();
        let first_transaction = transaction(&first_base, None, &"2".repeat(40));
        publish(
            &router,
            first_base,
            &first_transaction,
            &capsule(&first_transaction),
        )
        .await
        .unwrap();
        let stale_transaction = transaction(&stale_base, None, &"3".repeat(40));

        let error = publish(
            &router,
            stale_base.clone(),
            &stale_transaction,
            &capsule(&stale_transaction),
        )
        .await
        .expect_err("stale ref-head CAS must fail");

        assert!(matches!(error, WriteError::RefChanged { .. }));
        let visible = read_ref_head(&router, stale_base.record().root(), "refs/heads/main")
            .await
            .unwrap();
        assert_eq!(
            visible.visible.oid(),
            Some("2222222222222222222222222222222222222222")
        );
    }

    #[tokio::test]
    async fn lost_ref_head_update_reply_reconciles_as_committed_success() {
        let inner = Arc::new(InMemory::new());
        let seed_store = Store::new(inner.clone());
        let seed_router = StoreLayout::new(seed_store.clone(), "repositories/test".to_owned());
        initialize(&seed_router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let fault_store = Store::with_retry(
            Arc::new(LostHeadReplyStore {
                inner,
                head_path: seed_router
                    .capsule_ref_head_path(&crab_metadata::capsule_protocol::capsule_ref_name_key(
                        "refs/heads/main",
                    ))
                    .to_string(),
                lost: AtomicBool::new(false),
            }),
            crab_storage::RetryPolicy {
                max_attempts: 1,
                base: std::time::Duration::ZERO,
                cap: std::time::Duration::ZERO,
            },
        );
        let router = StoreLayout::new(fault_store.clone(), "repositories/test".to_owned());
        let base = open_root(&router).await.unwrap();
        let transaction = transaction(&base, None, &"2".repeat(40));

        let published = publish(&router, base, &transaction, &capsule(&transaction))
            .await
            .unwrap();

        let head = read_ref_head(&router, published.record().root(), "refs/heads/main")
            .await
            .unwrap();
        let transaction_id = transaction.id().unwrap();
        assert_eq!(head.visible.transaction_id(), Some(transaction_id.as_str()));
    }

    #[tokio::test]
    async fn ref_mismatch_fails_before_capsule_upload() {
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(Arc::new(InMemory::new())).with_storage_observer(observer.clone());
        let router = StoreLayout::new(store.clone(), "repositories/test".to_owned());
        let initial = initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let transaction = transaction(&initial, Some(&"9".repeat(40)), &"2".repeat(40));
        let capsule = capsule(&transaction);
        observer.observations.lock().unwrap().clear();

        let error = publish(&router, initial, &transaction, &capsule)
            .await
            .expect_err("expected-old mismatch must fail");

        assert!(matches!(error, WriteError::RefChanged { .. }));
        assert!(
            observer
                .observations
                .lock()
                .unwrap()
                .iter()
                .all(|observation| observation.operation != StorageOperation::Put)
        );
    }
}
