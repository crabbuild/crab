//! Capsule publication through independently mutable ref heads and transaction records.

use crab_metadata::capsule_protocol::{
    Capsule, CapsulePointer, CapsuleRun, CapsuleTransaction, Checkpoint, CheckpointPointer,
    HistorySegment, HistorySegmentState, RepositoryRoot, RootRecord, create_root, load_root,
};
use crab_storage::{ETag, StorageError, Store, StoreLayout};
use futures_util::future::try_join_all;
use futures_util::{StreamExt, TryStreamExt};

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
    let prepared = prepare_publication(router, base, transaction, capsule).await?;
    publish_prepared(router, prepared).await
}

/// Immutable capsule bytes and exact per-ref successors ready for publication.
#[derive(Debug)]
pub struct PreparedCapsulePublication {
    base: RootSnapshot,
    transaction: CapsuleTransaction,
    run: CapsuleRun,
    refs: Vec<PreparedRefHead>,
}

impl PreparedCapsulePublication {
    /// Return the exact coordinator payload that authorizes this publication.
    pub fn coordinated_publication(
        &self,
    ) -> Result<crab_coordination::write_coordinator::CoordinatedCapsulePublication> {
        let transaction_id = self.transaction.id()?;
        Ok(coordinated_publication_descriptor(
            self.base.record().digest(),
            &transaction_id,
            self.run.hash(),
            self.run.bytes().len() as u64,
        ))
    }
}

/// Build the deterministic coordinator descriptor for one verified leaf run.
#[must_use]
pub fn coordinated_publication_descriptor(
    base_root_digest: &str,
    transaction_id: &str,
    run_hash: &str,
    run_size: u64,
) -> crab_coordination::write_coordinator::CoordinatedCapsulePublication {
    crab_coordination::write_coordinator::CoordinatedCapsulePublication {
        base_root_digest: base_root_digest.to_owned(),
        transaction_id: transaction_id.to_owned(),
        activation_id: coordinated_activation_id(transaction_id, run_hash),
        run_hash: run_hash.to_owned(),
        run_size,
    }
}

/// Upload one immutable capsule run and capture exact CAS bases for its refs.
pub async fn prepare_publication(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    transaction: &CapsuleTransaction,
    capsule: &Capsule,
) -> Result<PreparedCapsulePublication> {
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

    let run = CapsuleRun::leaf(capsule.clone())?;
    let prepared = try_join_all(transaction.edits().iter().zip(snapshots).map(
        |(edit, snapshot)| {
            prepare_ref_successor(router, snapshot, edit, &transaction_id, run.clone())
        },
    ))
    .await?;
    let mut immutable_runs = vec![run.clone()];
    immutable_runs.extend(
        prepared
            .iter()
            .filter_map(|(_, compacted)| compacted.clone()),
    );
    upload_immutable_runs(router, immutable_runs).await?;
    let refs = prepared
        .into_iter()
        .map(|(prepared, _)| prepared)
        .collect::<Vec<_>>();

    Ok(PreparedCapsulePublication {
        base,
        transaction: transaction.clone(),
        run,
        refs,
    })
}

async fn publish_prepared(
    router: &StoreLayout<Store>,
    prepared: PreparedCapsulePublication,
) -> Result<RootSnapshot> {
    let PreparedCapsulePublication {
        base,
        transaction,
        refs,
        ..
    } = prepared;
    let transaction_id = transaction.id()?;

    if refs.len() == 1 && transaction.plan_id().is_none() {
        let prepared = refs
            .into_iter()
            .next()
            .ok_or_else(|| WriteError::Internal("single-ref publication disappeared".to_owned()))?;
        commit_single_ref(router, prepared).await?;
        return verify_ref_epoch(router, &base).await;
    }

    let activation_id = activation_id(&transaction_id);
    let plan_intent = match transaction.plan_id() {
        Some(_) => Some(
            crab_metadata::capsule_protocol::prepare_capsule_plan(
                router.store(),
                router,
                &transaction,
                &activation_id,
            )
            .await?,
        ),
        None => None,
    };
    commit_multi_ref(router, &transaction_id, &activation_id, refs).await?;
    if let Some(intent) = plan_intent {
        crab_metadata::capsule_protocol::publish_capsule_plan_receipt(
            router.store(),
            router,
            &intent,
        )
        .await?;
    }
    verify_ref_epoch(router, &base).await
}

/// Prepared v2 publication whose immutable bytes and optional plan intent are durable.
#[derive(Debug)]
pub struct CoordinatedPreparedCapsulePublication {
    prepared: PreparedCapsulePublication,
    descriptor: crab_coordination::write_coordinator::CoordinatedCapsulePublication,
    plan_intent: Option<crab_metadata::capsule_protocol::CapsulePlanReceipt>,
}

/// Regional visibility proof produced while replaying a coordinator decision.
#[derive(Debug)]
pub struct CoordinatedRepairOutcome {
    root: RootSnapshot,
    activation_id: String,
}

impl CoordinatedRepairOutcome {
    /// Return the root snapshot against which the repaired transaction was applied.
    #[must_use]
    pub fn root(&self) -> &RootSnapshot {
        &self.root
    }

    /// Return the committed regional activation that made the transaction visible.
    #[must_use]
    pub fn activation_id(&self) -> &str {
        &self.activation_id
    }
}

impl CoordinatedPreparedCapsulePublication {
    /// Return the exact immutable publication that the coordinator must commit.
    #[must_use]
    pub fn descriptor(
        &self,
    ) -> &crab_coordination::write_coordinator::CoordinatedCapsulePublication {
        &self.descriptor
    }
}

/// Prepare immutable bytes before an external coordinator commits the ref edits.
pub async fn prepare_coordinated_publication(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    transaction: &CapsuleTransaction,
    capsule: &Capsule,
) -> Result<CoordinatedPreparedCapsulePublication> {
    let prepared = prepare_publication(router, base, transaction, capsule).await?;
    let descriptor = prepared.coordinated_publication()?;
    let plan_intent = match transaction.plan_id() {
        Some(_) => Some(
            crab_metadata::capsule_protocol::prepare_capsule_plan(
                router.store(),
                router,
                transaction,
                &descriptor.activation_id,
            )
            .await?,
        ),
        None => None,
    };
    Ok(CoordinatedPreparedCapsulePublication {
        prepared,
        descriptor,
        plan_intent,
    })
}

/// Materialize one already-committed coordinator decision in a writer region.
pub async fn materialize_coordinated_publication(
    router: &StoreLayout<Store>,
    publication: CoordinatedPreparedCapsulePublication,
) -> Result<RootSnapshot> {
    let CoordinatedPreparedCapsulePublication {
        prepared,
        descriptor,
        plan_intent,
    } = publication;
    let PreparedCapsulePublication {
        base,
        transaction,
        refs,
        ..
    } = prepared;
    let transaction_id = transaction.id()?;
    if transaction_id != descriptor.transaction_id
        || base.record().digest() != descriptor.base_root_digest
    {
        return Err(WriteError::CorruptObject {
            path: "capsule-protocol coordinated publication".to_owned(),
            reason: "prepared publication no longer matches its coordinator descriptor".to_owned(),
        });
    }
    commit_multi_ref(
        router,
        &descriptor.transaction_id,
        &descriptor.activation_id,
        refs,
    )
    .await?;
    if let Some(intent) = plan_intent {
        crab_metadata::capsule_protocol::publish_capsule_plan_receipt(
            router.store(),
            router,
            &intent,
        )
        .await?;
    }
    verify_ref_epoch(router, &base).await
}

/// Replay one coordinator-authorized v2 publication from immutable regional bytes.
pub async fn materialize_coordinated_repair(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    descriptor: &crab_coordination::write_coordinator::CoordinatedCapsulePublication,
) -> Result<CoordinatedRepairOutcome> {
    let (run, _, transaction) = load_coordinated_publication(router, descriptor).await?;
    let snapshots = try_join_all(
        transaction
            .edits()
            .iter()
            .map(|edit| read_ref_head(router, base.record().root(), edit.ref_name())),
    )
    .await?;
    if snapshots.iter().all(|snapshot| {
        ref_state_contains_transaction(&snapshot.visible, &descriptor.transaction_id)
    }) {
        let activation_id = visible_transaction_activation(router, &snapshots, descriptor).await?;
        let root = verify_ref_epoch(router, &base).await?;
        return Ok(CoordinatedRepairOutcome {
            root,
            activation_id,
        });
    }
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
            prepare_ref_successor(
                router,
                snapshot,
                edit,
                &descriptor.transaction_id,
                run.clone(),
            )
        },
    ))
    .await?;
    let compacted = prepared
        .iter()
        .filter_map(|(_, run)| run.clone())
        .collect::<Vec<_>>();
    upload_immutable_runs(router, compacted).await?;
    let refs = prepared.into_iter().map(|(prepared, _)| prepared).collect();
    let activation_id = activation_id(&descriptor.transaction_id);
    commit_multi_ref(router, &descriptor.transaction_id, &activation_id, refs).await?;
    let root = verify_ref_epoch(router, &base).await?;
    Ok(CoordinatedRepairOutcome {
        root,
        activation_id,
    })
}

async fn verify_ref_epoch(
    router: &StoreLayout<Store>,
    base: &RootSnapshot,
) -> Result<RootSnapshot> {
    let observed = open_root(router).await?;
    if observed.record().root().repository_id() != base.record().root().repository_id() {
        return Err(WriteError::CorruptObject {
            path: router.capsule_root_path().to_string(),
            reason: "repository identity changed during ref publication".to_owned(),
        });
    }
    if observed.record().root().ref_epoch() != base.record().root().ref_epoch() {
        return Err(WriteError::CapsuleRefEpochChanged {
            path: router.capsule_root_path().to_string(),
            expected_epoch: base.record().root().ref_epoch().to_owned(),
            actual_epoch: observed.record().root().ref_epoch().to_owned(),
        });
    }
    Ok(observed)
}

async fn visible_transaction_activation(
    router: &StoreLayout<Store>,
    snapshots: &[RefHeadSnapshot],
    descriptor: &crab_coordination::write_coordinator::CoordinatedCapsulePublication,
) -> Result<String> {
    let mut activations = snapshots
        .iter()
        .filter_map(|snapshot| snapshot.head.prepared_activation_id())
        .collect::<std::collections::BTreeSet<_>>();
    if activations.len() == 1 {
        let activation_id = activations
            .pop_first()
            .map(str::to_owned)
            .ok_or_else(|| WriteError::Internal("regional activation disappeared".to_owned()))?;
        if committed_activation_matches(router, &activation_id, &descriptor.transaction_id).await? {
            return Ok(activation_id);
        }
    }
    for metadata in router
        .store()
        .list_prefix(&router.capsule_committed_transactions_prefix())
        .await?
    {
        let (body, _) = router
            .store()
            .get_with_etag_bounded(
                &metadata.location,
                crab_metadata::capsule_protocol::MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
            )
            .await?;
        let record = crab_metadata::capsule_protocol::CapsuleTransactionRecord::decode(&body)?;
        if router.capsule_committed_transaction_path(record.activation_id()) != metadata.location {
            return Err(WriteError::CorruptObject {
                path: metadata.location.to_string(),
                reason: "committed transaction marker key does not match its activation".to_owned(),
            });
        }
        if record.status() == crab_metadata::capsule_protocol::CapsuleTransactionStatus::Committed
            && record.transaction_id() == descriptor.transaction_id
        {
            return Ok(record.activation_id().to_owned());
        }
    }
    Err(WriteError::CorruptObject {
        path: "capsule-protocol coordinated repair".to_owned(),
        reason: format!(
            "visible transaction {} has no unique regional activation",
            descriptor.transaction_id
        ),
    })
}

async fn committed_activation_matches(
    router: &StoreLayout<Store>,
    activation_id: &str,
    transaction_id: &str,
) -> Result<bool> {
    let path = router.capsule_committed_transaction_path(activation_id);
    let (body, _) = match router
        .store()
        .get_with_etag_bounded(
            &path,
            crab_metadata::capsule_protocol::MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
        )
        .await
    {
        Ok(record) => record,
        Err(StorageError::NotFound { .. }) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let record = crab_metadata::capsule_protocol::CapsuleTransactionRecord::decode(&body)?;
    Ok(record.activation_id() == activation_id
        && record.transaction_id() == transaction_id
        && record.status() == crab_metadata::capsule_protocol::CapsuleTransactionStatus::Committed)
}

/// Load the authenticated pointer delta from one coordinator-bound capsule run.
pub async fn coordinated_pointer_catalog_delta(
    router: &StoreLayout<Store>,
    descriptor: &crab_coordination::write_coordinator::CoordinatedCapsulePublication,
) -> Result<Option<crab_metadata::capsule_protocol::PointerCatalog>> {
    let (_, capsule, _) = load_coordinated_publication(router, descriptor).await?;
    Ok(capsule.pointer_catalog_delta()?)
}

/// Load the exact ref transaction authenticated by a coordinator-bound run.
pub async fn coordinated_transaction(
    router: &StoreLayout<Store>,
    descriptor: &crab_coordination::write_coordinator::CoordinatedCapsulePublication,
) -> Result<CapsuleTransaction> {
    let (_, _, transaction) = load_coordinated_publication(router, descriptor).await?;
    Ok(transaction)
}

async fn load_coordinated_publication(
    router: &StoreLayout<Store>,
    descriptor: &crab_coordination::write_coordinator::CoordinatedCapsulePublication,
) -> Result<(CapsuleRun, Capsule, CapsuleTransaction)> {
    if descriptor.activation_id
        != coordinated_activation_id(&descriptor.transaction_id, &descriptor.run_hash)
    {
        return Err(WriteError::CorruptObject {
            path: "capsule-protocol coordinated publication".to_owned(),
            reason: "coordinator activation identity does not match its transaction and run"
                .to_owned(),
        });
    }
    let path = router.capsule_path(&descriptor.run_hash);
    let (bytes, _) = router
        .store()
        .get_with_etag_bounded(&path, descriptor.run_size)
        .await?;
    if bytes.len() as u64 != descriptor.run_size
        || blake3::hash(&bytes).to_hex().as_str() != descriptor.run_hash
    {
        return Err(WriteError::CorruptObject {
            path: path.to_string(),
            reason: "coordinator capsule run identity does not match regional bytes".to_owned(),
        });
    }
    let run = CapsuleRun::decode(bytes)?;
    let capsule = run
        .capsules()
        .first()
        .cloned()
        .ok_or_else(|| WriteError::CorruptObject {
            path: path.to_string(),
            reason: "coordinator capsule run contains no publication".to_owned(),
        })?;
    if run.capsules().len() != 1
        || run.level() != 0
        || capsule.transaction_id() != descriptor.transaction_id
        || capsule.base_root_digest() != descriptor.base_root_digest
    {
        return Err(WriteError::CorruptObject {
            path: path.to_string(),
            reason: "coordinator descriptor does not bind this leaf capsule run".to_owned(),
        });
    }
    let transaction = capsule.transaction()?;
    if transaction.id()? != descriptor.transaction_id
        || transaction.base_root_digest() != descriptor.base_root_digest
    {
        return Err(WriteError::CorruptObject {
            path: path.to_string(),
            reason: "coordinator descriptor does not bind the capsule transaction".to_owned(),
        });
    }
    Ok((run, capsule, transaction))
}

fn ref_state_contains_transaction(
    state: &crab_metadata::capsule_protocol::CapsuleRefState,
    transaction_id: &str,
) -> bool {
    state.transaction_id() == Some(transaction_id)
        || state.checkpoint_transaction_id() == Some(transaction_id)
        || state.frontier().iter().any(|pointer| {
            pointer
                .transaction_ids()
                .iter()
                .any(|candidate| candidate == transaction_id)
        })
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
            if head.ref_epoch() == root.ref_epoch() {
                (head, Some(etag))
            } else {
                (
                    crab_metadata::capsule_protocol::CapsuleRefHead::from_root(
                        ref_name,
                        root.ref_epoch().to_owned(),
                        root.refs().get(ref_name).cloned(),
                        root.peeled_refs().get(ref_name).cloned(),
                    )?,
                    Some(etag),
                )
            }
        }
        Err(StorageError::NotFound { .. }) => (
            crab_metadata::capsule_protocol::CapsuleRefHead::from_root(
                ref_name,
                root.ref_epoch().to_owned(),
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
    let visible = match root.compacted_ref_transactions().get(ref_name) {
        Some(compacted) => {
            let position = visible
                .frontier()
                .iter()
                .position(|pointer| pointer.transaction_ids().iter().any(|id| id == compacted));
            let frontier = match position {
                Some(position) => {
                    let compacted_pointer = &visible.frontier()[position];
                    let suffix_start = position
                        + usize::from(
                            compacted_pointer.transaction_ids().last() == Some(compacted),
                        );
                    visible.frontier()[suffix_start..].to_vec()
                }
                None if visible.checkpoint_transaction_id() == Some(compacted.as_str()) => {
                    visible.frontier().to_vec()
                }
                None => {
                    return Err(WriteError::CorruptObject {
                        path: path.to_string(),
                        reason: "ref head does not extend its checkpoint transaction".to_owned(),
                    });
                }
            };
            let transaction_id = frontier
                .last()
                .and_then(|pointer| pointer.transaction_ids().last())
                .cloned();
            if transaction_id.as_deref().or(Some(compacted.as_str())) != visible.transaction_id() {
                return Err(WriteError::CorruptObject {
                    path: path.to_string(),
                    reason: "ref head suffix does not reach its visible transaction".to_owned(),
                });
            }
            crab_metadata::capsule_protocol::CapsuleRefState::from_checkpoint(
                compacted.to_owned(),
                visible.oid().map(str::to_owned),
                visible.peeled_oid().map(str::to_owned),
                transaction_id,
                frontier,
            )?
        }
        None if visible.checkpoint_transaction_id().is_none() => visible,
        None => {
            return Err(WriteError::CorruptObject {
                path: path.to_string(),
                reason: "ref head names a checkpoint absent from the repository root".to_owned(),
            });
        }
    };
    Ok(RefHeadSnapshot {
        head,
        visible,
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
    leaf: CapsuleRun,
) -> Result<(PreparedRefHead, Option<CapsuleRun>)> {
    let mut frontier = snapshot.visible.frontier().to_vec();
    frontier.push(CapsulePointer::new(
        leaf.hash(),
        leaf.bytes().len() as u64,
        leaf.level(),
        leaf.transaction_ids(),
        leaf.newest_base_root_digest(),
    )?);
    let compacted = compact_ref_frontier(router, &mut frontier, &leaf).await?;
    let state = snapshot.visible.successor(
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
        compacted,
    ))
}

async fn compact_ref_frontier(
    router: &StoreLayout<Store>,
    frontier: &mut Vec<CapsulePointer>,
    known_leaf: &CapsuleRun,
) -> Result<Option<CapsuleRun>> {
    let fan_in = crab_metadata::capsule_protocol::CAPSULE_REF_COMPACTION_FAN_IN;
    if !fan_in.is_power_of_two() {
        return Err(WriteError::Internal(
            "capsule compaction fan-in is not a power of two".to_owned(),
        ));
    }
    let Some(level) = frontier.last().map(CapsulePointer::level) else {
        return Ok(None);
    };
    let suffix_len = frontier
        .iter()
        .rev()
        .take_while(|pointer| pointer.level() == level)
        .count();
    if suffix_len < fan_in {
        return Ok(None);
    }

    let capsules_per_run = 1_usize
        .checked_shl(u32::from(level))
        .ok_or_else(|| WriteError::Internal("capsule compaction level overflowed".to_owned()))?;
    if capsules_per_run
        .checked_mul(fan_in)
        .is_none_or(|count| count > crab_metadata::capsule_protocol::MAX_CAPSULES_PER_RUN)
    {
        return Ok(None);
    }

    let suffix_start = frontier.len() - fan_in;
    let level_delta = u8::try_from(fan_in.ilog2())
        .map_err(|_| WriteError::Internal("capsule compaction level overflowed".to_owned()))?;
    let mut next_level = level
        .checked_add(level_delta)
        .ok_or_else(|| WriteError::Internal("capsule compaction level overflowed".to_owned()))?;
    let mut carry_start = suffix_start;
    while carry_start > 0
        && frontier[carry_start - 1].level() == next_level
        && (1_usize << usize::from(next_level))
            < crab_metadata::capsule_protocol::MAX_CAPSULES_PER_RUN
    {
        carry_start -= 1;
        next_level = next_level.checked_add(1).ok_or_else(|| {
            WriteError::Internal("capsule compaction level overflowed".to_owned())
        })?;
    }

    let pointers = frontier[carry_start..].to_vec();
    let pointer_count = pointers.len();
    let runs = try_join_all(
        pointers
            .iter()
            .enumerate()
            .map(|(index, pointer)| async move {
                if index + 1 == pointer_count {
                    return Ok::<_, WriteError>(known_leaf.clone());
                }
                Ok::<_, WriteError>(
                    crab_metadata::capsule_protocol::load_capsule_run(router, pointer).await?,
                )
            }),
    )
    .await?;
    let carry_count = suffix_start - carry_start;
    let mut level_runs = runs[carry_count..].to_vec();
    while level_runs.len() > 1 {
        let mut merged = Vec::with_capacity(level_runs.len() / 2);
        for pair in level_runs.chunks_exact(2) {
            merged.push(pair[0].merge(&pair[1])?);
        }
        level_runs = merged;
    }
    let mut compacted = level_runs
        .pop()
        .ok_or_else(|| WriteError::Internal("capsule compaction suffix disappeared".to_owned()))?;
    for older in runs[..carry_count].iter().rev() {
        compacted = older.merge(&compacted)?;
    }
    frontier.truncate(carry_start);
    frontier.push(CapsulePointer::new(
        compacted.hash(),
        compacted.bytes().len() as u64,
        compacted.level(),
        compacted.transaction_ids(),
        compacted.newest_base_root_digest(),
    )?);
    Ok(Some(compacted))
}

async fn upload_immutable_runs(router: &StoreLayout<Store>, runs: Vec<CapsuleRun>) -> Result<()> {
    let mut unique = std::collections::BTreeMap::new();
    for run in runs {
        match unique.get(run.hash()) {
            Some(existing) if existing != &run => {
                return Err(WriteError::Internal(
                    "capsule run hash names conflicting bodies".to_owned(),
                ));
            }
            Some(_) => {}
            None => {
                unique.insert(run.hash().to_owned(), run);
            }
        }
    }
    try_join_all(unique.into_values().map(|run| async move {
        router
            .store()
            .put_if_absent_verified(&router.capsule_path(run.hash()), run.bytes().clone())
            .await
    }))
    .await?;
    Ok(())
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

fn coordinated_activation_id(transaction_id: &str, run_hash: &str) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("crab coordinated capsule activation v2");
    hasher.update(transaction_id.as_bytes());
    hasher.update(run_hash.as_bytes());
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
    capsule_runs: Vec<CapsulePointer>,
) -> Result<RootSnapshot> {
    publish_checkpoint_inner(
        router,
        base,
        checkpoint,
        Some(CheckpointRefState {
            refs,
            peeled_refs,
            compacted_ref_transactions,
            capsule_runs,
        }),
    )
    .await
}

struct CheckpointRefState {
    refs: std::collections::BTreeMap<String, String>,
    peeled_refs: std::collections::BTreeMap<String, String>,
    compacted_ref_transactions: std::collections::BTreeMap<String, String>,
    capsule_runs: Vec<CapsulePointer>,
}

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
    let pointer = CheckpointPointer::new(
        checkpoint.hash(),
        checkpoint.bytes().len() as u64,
        checkpoint.covered_generation(),
        checkpoint.covered_root_digest(),
        pack_count,
        object_count,
    )?;
    if let Some(state) = &ref_state {
        let retained_transactions = state
            .capsule_runs
            .iter()
            .flat_map(|run| run.transaction_ids())
            .collect::<std::collections::BTreeSet<_>>();
        let missing = state
            .compacted_ref_transactions
            .iter()
            .find(|(ref_name, transaction_id)| {
                base.record()
                    .root()
                    .compacted_ref_transactions()
                    .get(*ref_name)
                    != Some(*transaction_id)
                    && !retained_transactions.contains(transaction_id)
            });
        if let Some((ref_name, _)) = missing {
            return Err(WriteError::CorruptObject {
                path: "capsule-protocol checkpoint".to_owned(),
                reason: format!(
                    "checkpoint advances {ref_name} without retaining its transaction capsule"
                ),
            });
        }
    }
    let history_state = match &ref_state {
        Some(state) => Some(HistorySegmentState::new(
            state.refs.clone(),
            state.peeled_refs.clone(),
            base.record().root().head().to_owned(),
            state.compacted_ref_transactions.clone(),
            state.capsule_runs.clone(),
        )),
        None if !base.record().root().capsule_frontier().is_empty() => {
            Some(HistorySegmentState::new(
                base.record().root().refs().clone(),
                base.record().root().peeled_refs().clone(),
                base.record().root().head().to_owned(),
                base.record().root().compacted_ref_transactions().clone(),
                base.record().root().capsule_frontier().to_vec(),
            ))
        }
        None => None,
    };
    let history = history_state
        .map(|state| {
            HistorySegment::build(
                pointer.clone(),
                base.record().root().history().cloned(),
                state,
            )
        })
        .transpose()?;
    let path = router.capsule_checkpoint_path(checkpoint.hash());
    if let Some(history) = &history {
        let history_path = router.capsule_history_segment_path(history.hash());
        tokio::try_join!(
            router
                .store()
                .put_if_absent_verified(&path, checkpoint.bytes().clone()),
            router
                .store()
                .put_if_absent_verified(&history_path, history.bytes().clone()),
        )?;
    } else {
        router
            .store()
            .put_if_absent_verified(&path, checkpoint.bytes().clone())
            .await?;
    }
    let history_pointer = history.as_ref().map(HistorySegment::pointer).transpose()?;
    let is_ref_checkpoint = ref_state.is_some();
    let next = match ref_state {
        Some(state) => {
            let history_pointer = history_pointer.ok_or_else(|| {
                WriteError::Internal(
                    "ref checkpoint did not retain its compacted history".to_owned(),
                )
            })?;
            base.record().root().install_ref_checkpoint(
                base.record().digest(),
                pointer,
                history_pointer,
                state.refs,
                state.peeled_refs,
                state.compacted_ref_transactions,
            )?
        }
        None => base.record().root().install_checkpoint(
            base.record().digest(),
            pointer,
            history_pointer.or_else(|| base.record().root().history().cloned()),
        )?,
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

/// Fence a restore and atomically invalidate every prior ref-head authority.
pub async fn begin_restore(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    fence: crab_metadata::capsule_protocol::GcFence,
    ref_epoch: String,
) -> Result<RootSnapshot> {
    let fence_id = fence.id().to_owned();
    let next = base
        .record()
        .root()
        .begin_restore(base.record().digest(), fence, ref_epoch)?;
    let candidate = RootRecord::encode(next)?;
    let root_path = router.capsule_root_path();
    match router
        .store()
        .update(&root_path, candidate.bytes().clone(), base.etag().clone())
        .await
    {
        Ok(etag) => Ok(base.committed_restore_fence(candidate, etag)?),
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
                    fence_id,
                    source: Box::new(source),
                    verification: None,
                }),
                Err(verification) => Err(WriteError::CapsuleMaintenanceCommitUncertain {
                    fence_id,
                    source: Box::new(source),
                    verification: Some(Box::new(verification)),
                }),
            }
        }
    }
}

/// Publish one verified historical checkpoint as the new fenced repository state.
pub async fn restore_checkpoint(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    checkpoint: &Checkpoint,
    refs: std::collections::BTreeMap<String, String>,
    peeled_refs: std::collections::BTreeMap<String, String>,
    head: String,
) -> Result<RootSnapshot> {
    if base.record().root().gc_fence().is_none() {
        return Err(WriteError::Internal(
            "checkpoint restore requires a GC fence".to_owned(),
        ));
    }
    if checkpoint.covered_generation() != base.record().root().generation()
        || checkpoint.covered_root_digest() != base.record().digest()
    {
        return Err(WriteError::CorruptObject {
            path: "capsule-protocol restore checkpoint".to_owned(),
            reason: "restore checkpoint does not cover the exact fenced root".to_owned(),
        });
    }
    let visibility =
        checkpoint
            .visibility_snapshot()?
            .ok_or_else(|| WriteError::CorruptObject {
                path: checkpoint.hash().to_owned(),
                reason: "restore checkpoint has no complete Git visibility snapshot".to_owned(),
            })?;
    if visibility
        .refs()
        .keys()
        .collect::<std::collections::BTreeSet<_>>()
        != refs.keys().collect::<std::collections::BTreeSet<_>>()
        || refs.iter().any(|(name, oid)| {
            visibility
                .refs()
                .get(name)
                .is_none_or(|objects| objects.binary_search(oid).is_err())
        })
    {
        return Err(WriteError::CorruptObject {
            path: checkpoint.hash().to_owned(),
            reason: "restore checkpoint visibility does not authenticate its ref tips".to_owned(),
        });
    }
    let object_count = checkpoint
        .git_packs()
        .iter()
        .try_fold(0_u64, |total, pack| {
            total.checked_add(pack.object_count()).ok_or_else(|| {
                WriteError::Internal("restore checkpoint object count overflowed".to_owned())
            })
        })?;
    let pack_count = u32::try_from(checkpoint.git_packs().len())
        .map_err(|_| WriteError::Internal("restore checkpoint pack count overflowed".to_owned()))?;
    let pointer = CheckpointPointer::new(
        checkpoint.hash(),
        checkpoint.bytes().len() as u64,
        checkpoint.covered_generation(),
        checkpoint.covered_root_digest(),
        pack_count,
        object_count,
    )?;
    router
        .store()
        .put_if_absent_verified(
            &router.capsule_checkpoint_path(checkpoint.hash()),
            checkpoint.bytes().clone(),
        )
        .await?;
    let next = base.record().root().restore_checkpoint(
        base.record().digest(),
        pointer,
        refs,
        peeled_refs,
        head,
    )?;
    let candidate = RootRecord::encode(next)?;
    let root_path = router.capsule_root_path();
    match router
        .store()
        .update(&root_path, candidate.bytes().clone(), base.etag().clone())
        .await
    {
        Ok(etag) => Ok(base.committed_restore(candidate, etag)?),
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
    }
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

/// Publish a rebuilt retained-history chain and atomically replace its root pointer.
pub async fn replace_history(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    segments: &[HistorySegment],
) -> Result<RootSnapshot> {
    let fence = base.record().root().gc_fence().ok_or_else(|| {
        WriteError::Internal("history replacement requires a GC fence".to_owned())
    })?;
    let newest = segments
        .first()
        .ok_or_else(|| WriteError::Internal("history replacement cannot be empty".to_owned()))?;
    if base.record().root().checkpoint() != Some(newest.checkpoint()) {
        return Err(WriteError::CorruptObject {
            path: "capsule-protocol history".to_owned(),
            reason: "rebuilt history does not retain the current checkpoint frontier".to_owned(),
        });
    }
    for pair in segments.windows(2) {
        if pair[0].previous() != Some(&pair[1].pointer()?) {
            return Err(WriteError::CorruptObject {
                path: "capsule-protocol history".to_owned(),
                reason: "rebuilt history chain is not contiguous".to_owned(),
            });
        }
    }
    if segments
        .last()
        .is_some_and(|segment| segment.previous().is_some())
    {
        return Err(WriteError::CorruptObject {
            path: "capsule-protocol history".to_owned(),
            reason: "rebuilt history chain does not terminate".to_owned(),
        });
    }
    futures_util::stream::iter(segments.iter().map(|segment| async move {
        let path = router.capsule_history_segment_path(segment.hash());
        router
            .store()
            .put_if_absent_verified(&path, segment.bytes().clone())
            .await
    }))
    .buffer_unordered(16)
    .try_collect::<Vec<_>>()
    .await?;
    let next = base
        .record()
        .root()
        .replace_history(base.record().digest(), newest.pointer()?)?;
    let candidate = RootRecord::encode(next)?;
    let root_path = router.capsule_root_path();
    match router
        .store()
        .update(&root_path, candidate.bytes().clone(), base.etag().clone())
        .await
    {
        Ok(etag) => Ok(base.committed_history(candidate, etag)?),
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
                    fence_id: fence.id().to_owned(),
                    source: Box::new(source),
                    verification: None,
                }),
                Err(verification) => Err(WriteError::CapsuleMaintenanceCommitUncertain {
                    fence_id: fence.id().to_owned(),
                    source: Box::new(source),
                    verification: Some(Box::new(verification)),
                }),
            }
        }
    }
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
    async fn clean_publication_uses_five_requests_including_epoch_confirmation() {
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
                StorageOperation::Get,
            ]
        );
    }

    #[tokio::test]
    async fn checksum_qualified_publication_uses_four_requests_including_epoch_confirmation() {
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
                StorageOperation::Get,
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
    async fn coordinated_publication_separates_immutable_prepare_from_visibility() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let base = open_root(&router).await.unwrap();
        let transaction = transaction(&base, None, &"2".repeat(40));
        let prepared = prepare_coordinated_publication(
            &router,
            base.clone(),
            &transaction,
            &capsule(&transaction),
        )
        .await
        .unwrap();
        let descriptor = prepared.descriptor().clone();
        let rebuilt = coordinated_publication_descriptor(
            &descriptor.base_root_digest,
            &descriptor.transaction_id,
            &descriptor.run_hash,
            descriptor.run_size,
        );

        let before = read_ref_head(&router, base.record().root(), "refs/heads/main")
            .await
            .unwrap();
        assert_eq!(before.visible.oid(), None);
        assert_eq!(descriptor.transaction_id, transaction.id().unwrap());
        assert_eq!(descriptor.base_root_digest, base.record().digest());
        assert_eq!(rebuilt, descriptor);

        materialize_coordinated_publication(&router, prepared)
            .await
            .unwrap();

        let after = read_ref_head(&router, base.record().root(), "refs/heads/main")
            .await
            .unwrap();
        assert_eq!(
            after.visible.oid(),
            Some("2222222222222222222222222222222222222222")
        );
        let (record, _) = router
            .store()
            .get_with_etag_bounded(
                &router.capsule_transaction_path(&descriptor.activation_id),
                crab_metadata::capsule_protocol::MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
            )
            .await
            .unwrap();
        assert_eq!(
            crab_metadata::capsule_protocol::CapsuleTransactionRecord::decode(&record)
                .unwrap()
                .status(),
            crab_metadata::capsule_protocol::CapsuleTransactionStatus::Committed
        );
    }

    #[tokio::test]
    async fn coordinated_repair_replays_verified_run_and_is_idempotent() {
        let source = StoreLayout::new(
            Store::new(Arc::new(InMemory::new())),
            "repositories/test".to_owned(),
        );
        let target = StoreLayout::new(
            Store::new(Arc::new(InMemory::new())),
            "repositories/test".to_owned(),
        );
        let source_base = initialize(&source, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let target_base = initialize(&target, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let transaction = transaction(&source_base, None, &"2".repeat(40));
        let prepared = prepare_coordinated_publication(
            &source,
            source_base,
            &transaction,
            &capsule(&transaction),
        )
        .await
        .unwrap();
        let descriptor = prepared.descriptor().clone();
        let source_path = source.capsule_path(&descriptor.run_hash);
        let (run, _) = source
            .store()
            .get_with_etag_bounded(&source_path, descriptor.run_size)
            .await
            .unwrap();
        target
            .store()
            .put_if_absent_verified(&target.capsule_path(&descriptor.run_hash), run)
            .await
            .unwrap();

        materialize_coordinated_repair(&target, target_base.clone(), &descriptor)
            .await
            .unwrap();
        materialize_coordinated_repair(&target, target_base.clone(), &descriptor)
            .await
            .unwrap();

        let head = read_ref_head(&target, target_base.record().root(), "refs/heads/main")
            .await
            .unwrap();
        assert_eq!(
            head.visible.oid(),
            Some("2222222222222222222222222222222222222222")
        );
        assert_eq!(
            head.visible.transaction_id(),
            Some(descriptor.transaction_id.as_str())
        );
    }

    #[tokio::test]
    async fn coordinated_repair_rebuilds_batched_ref_compaction() {
        let source = StoreLayout::new(
            Store::new(Arc::new(InMemory::new())),
            "repositories/test".to_owned(),
        );
        let target = StoreLayout::new(
            Store::new(Arc::new(InMemory::new())),
            "repositories/test".to_owned(),
        );
        let source_base = initialize(&source, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let target_base = initialize(&target, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let mut previous = None;

        for sequence in 1..=32_u64 {
            let next = format!("{sequence:040x}");
            let transaction = transaction(&source_base, previous.as_deref(), &next);
            let prepared = prepare_coordinated_publication(
                &source,
                source_base.clone(),
                &transaction,
                &capsule(&transaction),
            )
            .await
            .unwrap();
            let descriptor = prepared.descriptor().clone();
            let (leaf, _) = source
                .store()
                .get_with_etag_bounded(
                    &source.capsule_path(&descriptor.run_hash),
                    descriptor.run_size,
                )
                .await
                .unwrap();
            target
                .store()
                .put_if_absent_verified(&target.capsule_path(&descriptor.run_hash), leaf)
                .await
                .unwrap();
            materialize_coordinated_publication(&source, prepared)
                .await
                .unwrap();
            materialize_coordinated_repair(&target, target_base.clone(), &descriptor)
                .await
                .unwrap();
            previous = Some(next);
        }

        let source_head = read_ref_head(&source, source_base.record().root(), "refs/heads/main")
            .await
            .unwrap();
        let target_head = read_ref_head(&target, target_base.record().root(), "refs/heads/main")
            .await
            .unwrap();
        assert_eq!(source_head.visible, target_head.visible);
        assert_eq!(target_head.visible.frontier().len(), 1);
        assert_eq!(target_head.visible.frontier()[0].level(), 5);
    }

    #[tokio::test]
    async fn coordinated_repair_publishes_plan_receipt_for_fresh_activation() {
        let source = StoreLayout::new(
            Store::new(Arc::new(InMemory::new())),
            "repositories/test".to_owned(),
        );
        let target = StoreLayout::new(
            Store::new(Arc::new(InMemory::new())),
            "repositories/test".to_owned(),
        );
        let source_base = initialize(&source, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let target_base = initialize(&target, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let plan_id = "9".repeat(64);
        let planned_transaction = CapsuleTransaction::for_plan(
            source_base.record().digest(),
            &plan_id,
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some("2".repeat(40)),
                None,
            )],
        )
        .unwrap();
        let prepared = prepare_coordinated_publication(
            &source,
            source_base,
            &planned_transaction,
            &capsule(&planned_transaction),
        )
        .await
        .unwrap();
        let descriptor = prepared.descriptor().clone();
        for path in [
            source.capsule_path(&descriptor.run_hash),
            source.capsule_plan_intent_path(&plan_id),
        ] {
            let (body, _) = source.store().get_with_etag(&path).await.unwrap();
            target
                .store()
                .put_if_absent_verified(&path, body)
                .await
                .unwrap();
        }
        let aborted = crab_metadata::capsule_protocol::CapsuleTransactionRecord::preparing(
            descriptor.activation_id.clone(),
            descriptor.transaction_id.clone(),
        )
        .unwrap()
        .abort()
        .unwrap();
        target
            .store()
            .create_strict(
                &target.capsule_transaction_path(&descriptor.activation_id),
                aborted.encode().unwrap(),
            )
            .await
            .unwrap();

        let repaired = materialize_coordinated_repair(&target, target_base.clone(), &descriptor)
            .await
            .unwrap();
        assert_ne!(repaired.activation_id(), descriptor.activation_id);
        let later = transaction(&target_base, Some(&"2".repeat(40)), &"3".repeat(40));
        publish(&target, target_base.clone(), &later, &capsule(&later))
            .await
            .unwrap();
        let recovered = materialize_coordinated_repair(&target, target_base, &descriptor)
            .await
            .unwrap();
        assert_eq!(recovered.activation_id(), repaired.activation_id());
        let intent = crab_metadata::capsule_protocol::read_capsule_plan_intent(
            target.store(),
            &target,
            &plan_id,
        )
        .await
        .unwrap()
        .unwrap();
        let receipt = crab_metadata::capsule_protocol::publish_capsule_plan_repair_receipt(
            target.store(),
            &target,
            &intent,
            recovered.activation_id(),
        )
        .await
        .unwrap();

        assert_eq!(receipt.activation_id(), descriptor.activation_id);
        assert_eq!(receipt.committed_activation_id(), recovered.activation_id());
        assert_eq!(
            crab_metadata::capsule_protocol::resolve_capsule_plan_receipt(
                target.store(),
                &target,
                &plan_id,
            )
            .await
            .unwrap(),
            Some(receipt)
        );
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
        let leaf = CapsuleRun::leaf(capsule(&transaction)).unwrap();
        let (prepared, _) = prepare_ref_successor(
            &router,
            snapshot,
            &transaction.edits()[0],
            &transaction_id,
            leaf,
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
    async fn incremental_append_does_not_read_or_rewrite_history() {
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
        assert_eq!(head.visible.frontier().len(), 2);
        assert!(
            head.visible
                .frontier()
                .iter()
                .all(|pointer| pointer.level() == 0)
        );
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
                StorageOperation::Put,
                StorageOperation::Put,
                StorageOperation::Get,
                StorageOperation::Get,
            ]
        );
    }

    #[tokio::test]
    async fn batched_ref_compaction_stays_below_ten_requests_per_push() {
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
        for sequence in 1..=65_u64 {
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
            vec![6, 0]
        );
        let request_count = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .count();
        assert!(request_count <= 393, "request count was {request_count}");
        assert!((request_count as f64 / 65.0) < 6.1);
    }

    #[tokio::test]
    async fn checkpoint_may_split_a_compacted_ref_run() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        let mut base = initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let mut previous = None;
        let mut checkpoint_transaction = None;
        let mut checkpoint_runs = None;

        for sequence in 1..=64_u64 {
            let next = format!("{sequence:040x}");
            let transaction = transaction(&base, previous.as_deref(), &next);
            base = publish(&router, base, &transaction, &capsule(&transaction))
                .await
                .unwrap();
            previous = Some(next);
            if sequence == 40 {
                checkpoint_transaction = Some(transaction.id().unwrap());
                checkpoint_runs = Some(
                    read_ref_head(&router, base.record().root(), "refs/heads/main")
                        .await
                        .unwrap()
                        .visible
                        .frontier()
                        .to_vec(),
                );
            }
        }

        let checkpoint_transaction = checkpoint_transaction.unwrap();
        let checkpoint = Checkpoint::build(
            base.record().root().generation(),
            base.record().digest(),
            vec![
                CapsuleGitPack::new(
                    Bytes::from_static(b"PACK"),
                    Bytes::from_static(b"index"),
                    Bytes::from_static(b"reverse"),
                    Bytes::from_static(b"locator"),
                    "f".repeat(40),
                    1,
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let checkpointed = publish_ref_checkpoint(
            &router,
            base,
            &checkpoint,
            std::collections::BTreeMap::from([(
                "refs/heads/main".to_owned(),
                format!("{:040x}", 40),
            )]),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::from([(
                "refs/heads/main".to_owned(),
                checkpoint_transaction.clone(),
            )]),
            checkpoint_runs.unwrap(),
        )
        .await
        .unwrap();

        let visible = read_ref_head(&router, checkpointed.record().root(), "refs/heads/main")
            .await
            .unwrap();
        assert_eq!(visible.visible.oid(), Some(format!("{:040x}", 64).as_str()));
        assert_eq!(
            visible.visible.checkpoint_transaction_id(),
            Some(checkpoint_transaction.as_str())
        );
        assert_eq!(visible.visible.frontier().len(), 1);
        assert_eq!(visible.visible.frontier()[0].level(), 6);
        let catalog =
            crab_metadata::capsule_protocol::load_pointer_catalog_from_root(&router, &checkpointed)
                .await
                .unwrap();
        assert!(catalog.files().is_empty());
    }

    #[tokio::test]
    async fn restore_epoch_makes_a_late_old_epoch_publication_invisible() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        let base = initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let first = transaction(&base, None, &"2".repeat(40));
        let base = publish(&router, base, &first, &capsule(&first))
            .await
            .unwrap();
        let late = transaction(&base, Some(&"2".repeat(40)), &"3".repeat(40));
        let prepared = prepare_publication(&router, base.clone(), &late, &capsule(&late))
            .await
            .unwrap();
        let visible = read_ref_head(&router, base.record().root(), "refs/heads/main")
            .await
            .unwrap();
        let checkpoint = Checkpoint::build(
            base.record().root().generation(),
            base.record().digest(),
            vec![
                CapsuleGitPack::new(
                    Bytes::from_static(b"PACK"),
                    Bytes::from_static(b"index"),
                    Bytes::from_static(b"reverse"),
                    Bytes::from_static(b"locator"),
                    "3".repeat(40),
                    1,
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let checkpointed = publish_ref_checkpoint(
            &router,
            base,
            &checkpoint,
            std::collections::BTreeMap::from([("refs/heads/main".to_owned(), "2".repeat(40))]),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::from([("refs/heads/main".to_owned(), first.id().unwrap())]),
            visible.visible.frontier().to_vec(),
        )
        .await
        .unwrap();
        let fenced = begin_restore(
            &router,
            checkpointed,
            crab_metadata::capsule_protocol::GcFence::new("f".repeat(64), 1).unwrap(),
            "e".repeat(64),
        )
        .await
        .unwrap();

        let error = publish_prepared(&router, prepared).await.unwrap_err();
        assert!(matches!(error, WriteError::CapsuleRefEpochChanged { .. }));
        let visible = read_ref_head(&router, fenced.record().root(), "refs/heads/main")
            .await
            .unwrap();
        let expected = "2".repeat(40);
        assert_eq!(visible.visible.oid(), Some(expected.as_str()));
        assert_eq!(visible.head.ref_epoch(), fenced.record().root().ref_epoch());

        let restored_oid = "4".repeat(40);
        let visibility_index = crab_metadata::git_visibility::GitVisibilityIndex::new(
            fenced.record().root().generation(),
            "6".repeat(64),
            "7".repeat(64),
            std::collections::BTreeMap::from([(
                "refs/heads/recovered".to_owned(),
                vec![restored_oid.clone()],
            )]),
        )
        .unwrap();
        let visibility = crab_metadata::capsule_protocol::CapsuleVisibilitySnapshot::from_index(
            &visibility_index,
        )
        .unwrap();
        let restore_checkpoint_body = Checkpoint::build_with_catalogs(
            fenced.record().root().generation(),
            fenced.record().digest(),
            vec![
                CapsuleGitPack::new(
                    Bytes::from_static(b"RESTORED PACK"),
                    Bytes::from_static(b"restored index"),
                    Bytes::from_static(b"restored reverse"),
                    Bytes::from_static(b"restored locator"),
                    "5".repeat(40),
                    1,
                )
                .unwrap(),
            ],
            crab_metadata::capsule_protocol::PointerCatalog::new(),
            Some(visibility),
        )
        .unwrap();
        let restored = restore_checkpoint(
            &router,
            fenced.clone(),
            &restore_checkpoint_body,
            std::collections::BTreeMap::from([("refs/heads/recovered".to_owned(), restored_oid)]),
            std::collections::BTreeMap::new(),
            "refs/heads/recovered".to_owned(),
        )
        .await
        .unwrap();
        assert_eq!(
            restored.record().root().generation(),
            fenced.record().root().generation() + 1
        );
        assert_eq!(
            restored.record().root().ref_epoch(),
            fenced.record().root().ref_epoch()
        );
        assert_eq!(restored.record().root().head(), "refs/heads/recovered");
        assert_eq!(
            restored.record().root().history(),
            fenced.record().root().history()
        );
        let released = end_gc(&router, restored, &"f".repeat(64)).await.unwrap();
        assert!(released.record().root().gc_fence().is_none());
    }

    #[tokio::test]
    async fn checkpoint_retains_compacted_capsules_in_one_history_put() {
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(Arc::new(InMemory::new()))
            .with_immutable_write_verification(ImmutableWriteVerification::Sha256Checksum)
            .with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        let base = initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let first_transaction = transaction(&base, None, &"2".repeat(40));
        let transaction_id = first_transaction.id().unwrap();
        let base = publish(
            &router,
            base,
            &first_transaction,
            &capsule(&first_transaction),
        )
        .await
        .unwrap();
        let head = read_ref_head(&router, base.record().root(), "refs/heads/main")
            .await
            .unwrap();
        let runs = head.visible.frontier().to_vec();
        let refs =
            std::collections::BTreeMap::from([("refs/heads/main".to_owned(), "2".repeat(40))]);
        let positions =
            std::collections::BTreeMap::from([("refs/heads/main".to_owned(), transaction_id)]);
        let checkpoint = Checkpoint::build(
            base.record().root().generation(),
            base.record().digest(),
            vec![
                CapsuleGitPack::new(
                    Bytes::from_static(b"PACK"),
                    Bytes::from_static(b"index"),
                    Bytes::from_static(b"reverse"),
                    Bytes::from_static(b"locator"),
                    "3".repeat(40),
                    1,
                )
                .unwrap(),
            ],
        )
        .unwrap();
        observer.observations.lock().unwrap().clear();

        let published = publish_ref_checkpoint(
            &router,
            base,
            &checkpoint,
            refs.clone(),
            std::collections::BTreeMap::new(),
            positions.clone(),
            runs.clone(),
        )
        .await
        .unwrap();

        let history = published.record().root().history().unwrap();
        let segments =
            crab_metadata::capsule_protocol::load_history_chain(&router, history, 8, 1024 * 1024)
                .await
                .unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].refs(), &refs);
        assert_eq!(segments[0].compacted_ref_transactions(), &positions);
        assert_eq!(segments[0].capsule_runs(), runs);
        let puts = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| {
                observation.outcome == StorageOutcome::Success
                    && observation.operation == StorageOperation::Put
            })
            .count();
        assert_eq!(puts, 3, "checkpoint, history, and root are the only writes");

        let second = transaction(&published, Some(&"2".repeat(40)), &"4".repeat(40));
        let second_id = second.id().unwrap();
        let base = publish(&router, published, &second, &capsule(&second))
            .await
            .unwrap();
        let head = read_ref_head(&router, base.record().root(), "refs/heads/main")
            .await
            .unwrap();
        let checkpoint = Checkpoint::build(
            base.record().root().generation(),
            base.record().digest(),
            vec![
                CapsuleGitPack::new(
                    Bytes::from_static(b"PACK2"),
                    Bytes::from_static(b"index2"),
                    Bytes::from_static(b"reverse2"),
                    Bytes::from_static(b"locator2"),
                    "5".repeat(40),
                    1,
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let published = publish_ref_checkpoint(
            &router,
            base,
            &checkpoint,
            std::collections::BTreeMap::from([("refs/heads/main".to_owned(), "4".repeat(40))]),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::from([("refs/heads/main".to_owned(), second_id)]),
            head.visible.frontier().to_vec(),
        )
        .await
        .unwrap();
        let history = published.record().root().history().unwrap();
        let segments =
            crab_metadata::capsule_protocol::load_history_chain(&router, history, 8, 1024 * 1024)
                .await
                .unwrap();

        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].checkpoint().covered_generation(), 1);
        assert_eq!(segments[1].checkpoint().covered_generation(), 0);
        assert_eq!(segments[0].previous().unwrap().hash(), segments[1].hash());

        let newest = &segments[0];
        let replacement = HistorySegment::build(
            newest.checkpoint().clone(),
            None,
            HistorySegmentState::new(
                newest.refs().clone(),
                newest.peeled_refs().clone(),
                newest.head().to_owned(),
                newest.compacted_ref_transactions().clone(),
                newest.capsule_runs().to_vec(),
            ),
        )
        .unwrap();
        let fence_id = "f".repeat(64);
        let fenced = begin_gc(
            &router,
            published,
            crab_metadata::capsule_protocol::GcFence::new(&fence_id, 1).unwrap(),
        )
        .await
        .unwrap();
        let replaced = replace_history(&router, fenced, &[replacement])
            .await
            .unwrap();
        let released = end_gc(&router, replaced, &fence_id).await.unwrap();
        let retained = crab_metadata::capsule_protocol::load_history_chain(
            &router,
            released.record().root().history().unwrap(),
            8,
            1024 * 1024,
        )
        .await
        .unwrap();

        assert_eq!(retained.len(), 1);
        assert_eq!(
            retained[0].checkpoint(),
            released.record().root().checkpoint().unwrap()
        );
        assert!(released.record().root().gc_fence().is_none());
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
