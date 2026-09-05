//! Durable attribution of mirror plans to direct and managed ref commits.

use std::collections::BTreeSet;

use bytes::Bytes;
use crab_storage::{StorageError, Store, StoreLayout};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::error::{MetadataError, Result};
use crate::manifest_store::{read_manifest, read_manifest_history_exact, write_manifest_cas};
use crate::manifests::Manifest;
use crate::ref_journal::{
    RefJournalTransaction, read_ref_head, read_transaction, transaction_is_active,
};
use crate::validation::validate_content_hash;

const PLAN_RECEIPT_VERSION: u32 = 1;
const MAX_PLAN_ATTEMPTS: u32 = 3;
const MAX_PLAN_OBJECT_BYTES: u64 = 64 * 1024;
const MAX_PLAN_ANCESTORS: usize = 1_000_000;

/// Commit authority and immutable identity attributed to one mirror plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MirrorPlanCommit {
    RefJournal {
        transaction_id: String,
        dependency_digest: String,
    },
    Manifest {
        base_generation: u64,
        base_digest: String,
        generation: u64,
        digest: String,
    },
}

/// Immutable pre-commit binding between a mirror plan and one commit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MirrorPlanIntent {
    pub version: u32,
    pub repo_prefix: String,
    pub plan_id: String,
    pub attempt: u32,
    pub commit: MirrorPlanCommit,
}

/// Immutable proof that one mirror plan crossed the ref visibility boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MirrorPlanReceipt {
    pub version: u32,
    pub repo_prefix: String,
    pub plan_id: String,
    pub attempt: u32,
    pub commit: MirrorPlanCommit,
}

pub(crate) async fn prepare_ref_journal_plan_intent(
    store: &Store,
    router: &StoreLayout<Store>,
    plan_id: &str,
    transaction: &RefJournalTransaction,
) -> Result<MirrorPlanIntent> {
    prepare_plan_intent(
        store,
        router,
        plan_id,
        MirrorPlanCommit::RefJournal {
            transaction_id: transaction.id()?,
            dependency_digest: dependency_digest(transaction)?,
        },
    )
    .await
}

/// Commit a managed mirror plan through the canonical manifest CAS authority.
pub async fn commit_manifest_for_plan(
    store: &Store,
    router: &StoreLayout<Store>,
    candidate: &Manifest,
    expected_etag: &str,
    plan_id: &str,
) -> Result<String> {
    let (base, current_etag) = read_manifest(store, router).await?;
    if current_etag != expected_etag {
        return Err(MetadataError::ManifestCasConflict {
            path: router.manifest_path().to_string(),
            expected_etag: Some(expected_etag.to_owned()),
        });
    }
    let generation = base
        .generation
        .checked_add(1)
        .ok_or_else(|| MetadataError::Internal("manifest generation overflow".to_owned()))?;
    if candidate.generation != generation {
        return Err(corrupt(
            router.manifest_path().as_ref(),
            "managed mirror candidate generation does not follow its base",
        ));
    }
    let intent = prepare_plan_intent(
        store,
        router,
        plan_id,
        MirrorPlanCommit::Manifest {
            base_generation: base.generation,
            base_digest: manifest_digest(&base)?,
            generation,
            digest: manifest_digest(candidate)?,
        },
    )
    .await?;
    let etag = write_manifest_cas(store, router, candidate, expected_etag).await?;
    if let Err(error) = publish_plan_receipt(store, router, &intent).await {
        warn!(
            plan_id,
            %error,
            "committed managed mirror plan terminal receipt needs read-back repair"
        );
    }
    Ok(etag)
}

async fn prepare_plan_intent(
    store: &Store,
    router: &StoreLayout<Store>,
    plan_id: &str,
    commit: MirrorPlanCommit,
) -> Result<MirrorPlanIntent> {
    validate_content_hash(plan_id, "mirror plan id", "mirror plan intent")?;
    let existing = read_plan_intents(store, router, plan_id).await?;
    for intent in &existing {
        if intent_committed(store, router, intent).await? {
            return Err(StorageError::StateConflict {
                path: router.ref_journal_plan_receipt_path(plan_id).to_string(),
            }
            .into());
        }
    }
    // One immutable commit needs one intent even across retries. Duplicate
    // attempts would both appear committed after a lost terminal write.
    if let Some(intent) = existing.iter().find(|intent| intent.commit == commit) {
        return Ok(intent.clone());
    }
    let attempt = (1..=MAX_PLAN_ATTEMPTS)
        .find(|candidate| !existing.iter().any(|intent| intent.attempt == *candidate))
        .ok_or_else(|| StorageError::StateConflict {
            path: router.ref_journal_plan_attempts_prefix(plan_id).to_string(),
        })?;
    let intent = MirrorPlanIntent {
        version: PLAN_RECEIPT_VERSION,
        repo_prefix: router.repo_prefix().to_owned(),
        plan_id: plan_id.to_owned(),
        attempt,
        commit,
    };
    validate_intent(router, &intent)?;
    let path = router.ref_journal_plan_intent_path(plan_id, attempt);
    match store
        .create_strict(&path, Bytes::from(serialize(&intent)?))
        .await
    {
        Ok(()) => Ok(intent),
        Err(StorageError::StateConflict { .. }) => {
            let persisted: MirrorPlanIntent = read_bounded(store, &path).await?;
            validate_intent(router, &persisted)?;
            if persisted == intent {
                Ok(persisted)
            } else {
                Err(corrupt(
                    path.as_ref(),
                    "mirror plan attempt already binds a different commit",
                ))
            }
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) async fn publish_plan_receipt(
    store: &Store,
    router: &StoreLayout<Store>,
    intent: &MirrorPlanIntent,
) -> Result<MirrorPlanReceipt> {
    validate_intent(router, intent)?;
    if !intent_committed(store, router, intent).await? {
        return Err(corrupt(
            router
                .ref_journal_plan_intent_path(&intent.plan_id, intent.attempt)
                .as_ref(),
            "mirror plan commit is not visible",
        ));
    }
    write_receipt(store, router, intent).await
}

/// Resolve a plan's historical commit without using ref equality as proof.
pub async fn resolve_plan_receipt(
    store: &Store,
    router: &StoreLayout<Store>,
    plan_id: &str,
) -> Result<Option<MirrorPlanReceipt>> {
    validate_content_hash(plan_id, "mirror plan id", "mirror plan receipt")?;
    let receipt_path = router.ref_journal_plan_receipt_path(plan_id);
    if let Some(receipt) = read_optional_bounded(store, &receipt_path).await? {
        validate_receipt(store, router, &receipt).await?;
        return Ok(Some(receipt));
    }

    let intents = read_plan_intents(store, router, plan_id).await?;
    let mut committed = None;
    for intent in intents {
        if intent_committed(store, router, &intent).await? && committed.replace(intent).is_some() {
            return Err(corrupt(
                receipt_path.as_ref(),
                "mirror plan has more than one committed result",
            ));
        }
    }
    match committed {
        Some(intent) => write_receipt(store, router, &intent).await.map(Some),
        None => Ok(None),
    }
}

async fn read_plan_intents(
    store: &Store,
    router: &StoreLayout<Store>,
    plan_id: &str,
) -> Result<Vec<MirrorPlanIntent>> {
    let prefix = router.ref_journal_plan_attempts_prefix(plan_id);
    let objects = store
        .list_prefix_bounded(&prefix, MAX_PLAN_ATTEMPTS as usize + 1)
        .await?
        .ok_or_else(|| {
            corrupt(
                prefix.as_ref(),
                format!("mirror plan exceeds its {MAX_PLAN_ATTEMPTS} commit attempts"),
            )
        })?;
    let mut intents = Vec::with_capacity(objects.len());
    for object in objects {
        let intent: MirrorPlanIntent = read_bounded(store, &object.location).await?;
        validate_intent(router, &intent)?;
        if intent.plan_id != plan_id
            || object.location != router.ref_journal_plan_intent_path(plan_id, intent.attempt)
        {
            return Err(corrupt(
                object.location.as_ref(),
                "mirror plan intent key does not match its body",
            ));
        }
        intents.push(intent);
    }
    intents.sort_unstable_by_key(|intent| intent.attempt);
    Ok(intents)
}

async fn write_receipt(
    store: &Store,
    router: &StoreLayout<Store>,
    intent: &MirrorPlanIntent,
) -> Result<MirrorPlanReceipt> {
    let receipt = MirrorPlanReceipt {
        version: PLAN_RECEIPT_VERSION,
        repo_prefix: intent.repo_prefix.clone(),
        plan_id: intent.plan_id.clone(),
        attempt: intent.attempt,
        commit: intent.commit.clone(),
    };
    let path = router.ref_journal_plan_receipt_path(&receipt.plan_id);
    match store
        .create_strict(&path, Bytes::from(serialize(&receipt)?))
        .await
    {
        Ok(()) => Ok(receipt),
        Err(StorageError::StateConflict { .. }) => {
            let persisted: MirrorPlanReceipt = read_bounded(store, &path).await?;
            if persisted == receipt {
                Ok(persisted)
            } else {
                Err(corrupt(
                    path.as_ref(),
                    "mirror plan terminal receipt conflicts with the committed result",
                ))
            }
        }
        Err(error) => Err(error.into()),
    }
}

async fn validate_receipt(
    store: &Store,
    router: &StoreLayout<Store>,
    receipt: &MirrorPlanReceipt,
) -> Result<()> {
    validate_common(
        router,
        receipt.version,
        &receipt.repo_prefix,
        &receipt.plan_id,
        receipt.attempt,
        "mirror plan receipt",
    )?;
    let intent_path = router.ref_journal_plan_intent_path(&receipt.plan_id, receipt.attempt);
    let intent: MirrorPlanIntent = read_bounded(store, &intent_path).await?;
    validate_intent(router, &intent)?;
    if receipt.repo_prefix != intent.repo_prefix
        || receipt.plan_id != intent.plan_id
        || receipt.attempt != intent.attempt
        || receipt.commit != intent.commit
    {
        return Err(corrupt(
            router
                .ref_journal_plan_receipt_path(&receipt.plan_id)
                .as_ref(),
            "mirror plan receipt does not match its commit intent",
        ));
    }
    validate_receipt_commit(store, router, &intent).await
}

async fn validate_receipt_commit(
    store: &Store,
    router: &StoreLayout<Store>,
    intent: &MirrorPlanIntent,
) -> Result<()> {
    match &intent.commit {
        MirrorPlanCommit::RefJournal {
            transaction_id,
            dependency_digest,
        } => read_bound_transaction(store, router, intent, transaction_id, dependency_digest)
            .await
            .map(|_| ()),
        MirrorPlanCommit::Manifest {
            base_generation,
            base_digest,
            generation,
            digest,
        } => {
            if manifest_committed(
                store,
                router,
                *base_generation,
                base_digest,
                *generation,
                digest,
            )
            .await?
            {
                Ok(())
            } else {
                Err(corrupt(
                    router
                        .ref_journal_plan_receipt_path(&intent.plan_id)
                        .as_ref(),
                    "mirror plan receipt names an unavailable manifest commit",
                ))
            }
        }
    }
}

fn validate_intent(router: &StoreLayout<Store>, intent: &MirrorPlanIntent) -> Result<()> {
    validate_common(
        router,
        intent.version,
        &intent.repo_prefix,
        &intent.plan_id,
        intent.attempt,
        "mirror plan intent",
    )?;
    match &intent.commit {
        MirrorPlanCommit::RefJournal {
            transaction_id,
            dependency_digest,
        } => {
            validate_content_hash(
                transaction_id,
                "ref journal transaction id",
                "mirror plan intent",
            )?;
            validate_content_hash(dependency_digest, "dependency digest", "mirror plan intent")
        }
        MirrorPlanCommit::Manifest {
            base_generation,
            base_digest,
            generation,
            digest,
        } => {
            if base_generation.checked_add(1) != Some(*generation) {
                return Err(corrupt(
                    "mirror plan intent",
                    "mirror plan manifest generations are not consecutive",
                ));
            }
            validate_content_hash(base_digest, "base manifest digest", "mirror plan intent")?;
            validate_content_hash(digest, "manifest digest", "mirror plan intent")
        }
    }
}

fn validate_common(
    router: &StoreLayout<Store>,
    version: u32,
    repo_prefix: &str,
    plan_id: &str,
    attempt: u32,
    label: &str,
) -> Result<()> {
    if version != PLAN_RECEIPT_VERSION {
        return Err(corrupt(label, "unsupported mirror plan receipt version"));
    }
    if repo_prefix != router.repo_prefix() {
        return Err(corrupt(label, "mirror plan receipt repository mismatch"));
    }
    if !(1..=MAX_PLAN_ATTEMPTS).contains(&attempt) {
        return Err(corrupt(
            label,
            "mirror plan attempt is outside the supported bound",
        ));
    }
    validate_content_hash(plan_id, "mirror plan id", label)
}

async fn intent_committed(
    store: &Store,
    router: &StoreLayout<Store>,
    intent: &MirrorPlanIntent,
) -> Result<bool> {
    match &intent.commit {
        MirrorPlanCommit::RefJournal {
            transaction_id,
            dependency_digest,
        } => transaction_committed(store, router, intent, transaction_id, dependency_digest).await,
        MirrorPlanCommit::Manifest {
            base_generation,
            base_digest,
            generation,
            digest,
        } => {
            manifest_committed(
                store,
                router,
                *base_generation,
                base_digest,
                *generation,
                digest,
            )
            .await
        }
    }
}

async fn transaction_committed(
    store: &Store,
    router: &StoreLayout<Store>,
    intent: &MirrorPlanIntent,
    transaction_id: &str,
    dependency_digest: &str,
) -> Result<bool> {
    let transaction =
        read_bound_transaction(store, router, intent, transaction_id, dependency_digest).await?;
    if transaction_is_active(store, router, transaction_id).await? {
        return Ok(true);
    }
    let mut matched = 0usize;
    for edit in &transaction.edits {
        let head = read_ref_head(store, router, &edit.ref_name).await?;
        if transaction_is_ancestor(
            store,
            router,
            &edit.ref_name,
            head.visible_transaction,
            transaction_id,
        )
        .await?
        {
            matched += 1;
        }
    }
    if matched == 0 {
        return Ok(false);
    }
    if matched != transaction.edits.len() {
        return Err(corrupt(
            router
                .ref_journal_plan_intent_path(&intent.plan_id, intent.attempt)
                .as_ref(),
            "mirror plan transaction is visible for only part of its ref batch",
        ));
    }
    Ok(true)
}

async fn read_bound_transaction(
    store: &Store,
    router: &StoreLayout<Store>,
    intent: &MirrorPlanIntent,
    transaction_id: &str,
    expected_dependency_digest: &str,
) -> Result<RefJournalTransaction> {
    let transaction = read_transaction(store, router, transaction_id).await?;
    if dependency_digest(&transaction)? != expected_dependency_digest {
        return Err(corrupt(
            router
                .ref_journal_plan_intent_path(&intent.plan_id, intent.attempt)
                .as_ref(),
            "mirror plan dependency digest does not match its transaction",
        ));
    }
    Ok(transaction)
}

async fn transaction_is_ancestor(
    store: &Store,
    router: &StoreLayout<Store>,
    ref_name: &str,
    mut current: Option<String>,
    expected: &str,
) -> Result<bool> {
    let mut seen = BTreeSet::new();
    while let Some(transaction_id) = current {
        if transaction_id == expected {
            return Ok(true);
        }
        if seen.len() == MAX_PLAN_ANCESTORS || !seen.insert(transaction_id.clone()) {
            return Err(corrupt(
                router
                    .ref_journal_transaction_path(&transaction_id)
                    .as_ref(),
                "mirror plan transaction ancestry is cyclic or exceeds its bound",
            ));
        }
        let transaction = read_transaction(store, router, &transaction_id).await?;
        current = transaction.parents.get(ref_name).cloned().flatten();
    }
    Ok(false)
}

async fn manifest_committed(
    store: &Store,
    router: &StoreLayout<Store>,
    base_generation: u64,
    base_digest: &str,
    generation: u64,
    digest: &str,
) -> Result<bool> {
    if read_manifest_version(store, router, generation, digest)
        .await?
        .is_none()
    {
        return Ok(false);
    }
    if read_manifest_version(store, router, base_generation, base_digest)
        .await?
        .is_none()
    {
        return Err(corrupt(
            router.manifest_history_prefix().as_ref(),
            "managed mirror commit is missing its bound base manifest",
        ));
    }
    Ok(true)
}

/// Read the exact current or historical manifest named by a plan receipt.
pub async fn read_manifest_version(
    store: &Store,
    router: &StoreLayout<Store>,
    generation: u64,
    digest: &str,
) -> Result<Option<Manifest>> {
    validate_content_hash(digest, "manifest digest", "mirror plan receipt")?;
    let (current, _) = read_manifest(store, router).await?;
    let current_digest = manifest_digest(&current)?;
    if current.generation == generation {
        return Ok((current_digest == digest).then_some(current));
    }
    if current.generation < generation {
        return Ok(None);
    }
    let Some(entry) = read_manifest_history_exact(store, router, generation, digest).await? else {
        return Err(corrupt(
            router.manifest_history_path(generation, digest).as_ref(),
            "mirror plan commit history is missing",
        ));
    };
    Ok(Some(entry.manifest))
}

fn manifest_digest(manifest: &Manifest) -> Result<String> {
    let body = serde_json::to_vec_pretty(manifest).map_err(|error| {
        MetadataError::Internal(format!("serialize mirror plan manifest: {error}"))
    })?;
    Ok(blake3::hash(&body).to_hex().to_string())
}

fn dependency_digest(transaction: &RefJournalTransaction) -> Result<String> {
    let visibility = transaction
        .edits
        .iter()
        .map(|edit| (&edit.ref_name, &edit.visibility_evidence_hash))
        .collect::<Vec<_>>();
    let body = serde_json::to_vec(&(&transaction.packs, &transaction.shards, visibility)).map_err(
        |error| MetadataError::Internal(format!("serialize plan dependencies: {error}")),
    )?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab mirror plan dependencies v1\0");
    hasher.update(&body);
    Ok(hasher.finalize().to_hex().to_string())
}

async fn read_optional_bounded<T: for<'de> Deserialize<'de>>(
    store: &Store,
    path: &object_store::path::Path,
) -> Result<Option<T>> {
    match store
        .get_with_etag_bounded(path, MAX_PLAN_OBJECT_BYTES)
        .await
    {
        Ok((bytes, _)) => parse(path, &bytes).map(Some),
        Err(StorageError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn read_bounded<T: for<'de> Deserialize<'de>>(
    store: &Store,
    path: &object_store::path::Path,
) -> Result<T> {
    let (bytes, _) = store
        .get_with_etag_bounded(path, MAX_PLAN_OBJECT_BYTES)
        .await?;
    parse(path, &bytes)
}

fn parse<T: for<'de> Deserialize<'de>>(path: &object_store::path::Path, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|error| {
        corrupt(
            path.as_ref(),
            format!("invalid mirror plan receipt JSON: {error}"),
        )
    })
}

fn serialize(value: &impl Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|error| MetadataError::Internal(format!("serialize mirror plan receipt: {error}")))
}

fn corrupt(path: &str, reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use object_store::memory::InMemory;

    use super::*;
    use crate::ref_journal::{
        RefJournalEdit, commit_ref_transaction, commit_ref_transaction_for_plan,
    };

    fn fixture(prefix: &str) -> (Store, StoreLayout<Store>) {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), prefix.to_owned());
        (store, router)
    }

    fn edit(ref_name: &str, old_oid: Option<String>, value: char) -> RefJournalEdit {
        RefJournalEdit {
            ref_name: ref_name.to_owned(),
            old_oid,
            new_oid: Some(value.to_string().repeat(40)),
            peeled_oid: None,
            lock_holder: None,
            visibility_evidence_hash: Some(value.to_string().repeat(64)),
        }
    }

    fn receipt_transaction(receipt: &MirrorPlanReceipt) -> &str {
        match &receipt.commit {
            MirrorPlanCommit::RefJournal { transaction_id, .. } => transaction_id,
            MirrorPlanCommit::Manifest { .. } => panic!("expected ref-journal receipt"),
        }
    }

    async fn transaction(
        store: &Store,
        router: &StoreLayout<Store>,
        ref_name: &str,
        value: char,
    ) -> (
        RefJournalTransaction,
        Vec<crate::ref_journal::RefJournalHeadSnapshot>,
    ) {
        let head = read_ref_head(store, router, ref_name).await.unwrap();
        let old_oid = head.visible_transaction.as_ref().map(|_| {
            if value == 'b' {
                "a".repeat(40)
            } else {
                "b".repeat(40)
            }
        });
        let parents = BTreeMap::from([(ref_name.to_owned(), head.visible_transaction.clone())]);
        let transaction = RefJournalTransaction::new(
            parents,
            vec![edit(ref_name, old_oid, value)],
            None,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        (transaction, vec![head])
    }

    #[tokio::test]
    async fn committed_plan_publishes_a_resolvable_terminal_receipt() {
        let (store, router) = fixture("receipt/commit");
        let plan_id = "1".repeat(64);
        let (transaction, heads) = transaction(&store, &router, "refs/heads/main", 'a').await;
        let committed = commit_ref_transaction_for_plan(
            &store,
            &router,
            &transaction,
            &heads,
            &plan_id,
            || false,
        )
        .await
        .unwrap();

        let receipt = resolve_plan_receipt(&store, &router, &plan_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(receipt_transaction(&receipt), committed.transaction_id);
        assert_eq!(receipt.attempt, 1);
    }

    #[tokio::test]
    async fn missing_receipt_recovers_through_a_compacted_successor_chain() {
        let (store, router) = fixture("receipt/successor");
        let plan_id = "2".repeat(64);
        let ref_name = "refs/heads/main";
        let (first, heads) = transaction(&store, &router, ref_name, 'a').await;
        let committed =
            commit_ref_transaction_for_plan(&store, &router, &first, &heads, &plan_id, || false)
                .await
                .unwrap();
        store
            .delete(&router.ref_journal_plan_receipt_path(&plan_id))
            .await
            .unwrap();
        store
            .delete(&router.ref_journal_active_path(&committed.transaction_id))
            .await
            .unwrap();

        let (successor, heads) = transaction(&store, &router, ref_name, 'b').await;
        let successor = commit_ref_transaction(&store, &router, &successor, &heads)
            .await
            .unwrap();
        store
            .delete(&router.ref_journal_active_path(&successor.transaction_id))
            .await
            .unwrap();

        let receipt = resolve_plan_receipt(&store, &router, &plan_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt_transaction(&receipt), committed.transaction_id);
    }

    #[tokio::test]
    async fn missing_receipt_recovers_after_journal_compaction() {
        let (store, router) = fixture("receipt/compaction");
        crate::layout_descriptor::ensure_canonical_layout(&store, &router)
            .await
            .unwrap();
        let base = Manifest::default_for_repo("refs/heads/main");
        crate::manifest_store::create_manifest(&store, &router, &base)
            .await
            .unwrap();
        let plan_id = "d".repeat(64);
        let (mut transaction, heads) = transaction(&store, &router, "refs/heads/main", 'a').await;
        transaction.edits[0].visibility_evidence_hash = None;
        let committed = commit_ref_transaction_for_plan(
            &store,
            &router,
            &transaction,
            &heads,
            &plan_id,
            || false,
        )
        .await
        .unwrap();
        store
            .delete(&router.ref_journal_plan_receipt_path(&plan_id))
            .await
            .unwrap();

        crate::manifest_store::compact_ref_journal(
            &store,
            &router,
            "2026-09-03T00:00:00Z".to_owned(),
            Some("test".to_owned()),
            "receipt-compaction".to_owned(),
        )
        .await
        .unwrap()
        .unwrap();
        let receipt = resolve_plan_receipt(&store, &router, &plan_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(receipt_transaction(&receipt), committed.transaction_id);
    }

    #[tokio::test]
    async fn uncommitted_intent_allows_one_ordered_retry() {
        let (store, router) = fixture("receipt/retry");
        let plan_id = "3".repeat(64);
        let (abandoned, _) = transaction(&store, &router, "refs/heads/main", 'a').await;
        let abandoned_id = abandoned.id().unwrap();
        store
            .put_exact(
                &router.ref_journal_transaction_path(&abandoned_id),
                Bytes::from(serde_json::to_vec(&abandoned).unwrap()),
            )
            .await
            .unwrap();
        let intent = prepare_ref_journal_plan_intent(&store, &router, &plan_id, &abandoned)
            .await
            .unwrap();
        assert_eq!(intent.attempt, 1);
        assert!(
            resolve_plan_receipt(&store, &router, &plan_id)
                .await
                .unwrap()
                .is_none()
        );

        let (mut retry, heads) = transaction(&store, &router, "refs/heads/main", 'a').await;
        retry.edits[0].visibility_evidence_hash = Some("b".repeat(64));
        commit_ref_transaction_for_plan(&store, &router, &retry, &heads, &plan_id, || false)
            .await
            .unwrap();
        let receipt = resolve_plan_receipt(&store, &router, &plan_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.attempt, 2);
    }

    #[tokio::test]
    async fn identical_retry_reuses_the_uncommitted_intent() {
        let (store, router) = fixture("receipt/identical-retry");
        let plan_id = "e".repeat(64);
        let (transaction, heads) = transaction(&store, &router, "refs/heads/main", 'a').await;
        let transaction_id = transaction.id().unwrap();
        store
            .put_exact(
                &router.ref_journal_transaction_path(&transaction_id),
                Bytes::from(serde_json::to_vec(&transaction).unwrap()),
            )
            .await
            .unwrap();
        let first = prepare_ref_journal_plan_intent(&store, &router, &plan_id, &transaction)
            .await
            .unwrap();

        commit_ref_transaction_for_plan(&store, &router, &transaction, &heads, &plan_id, || false)
            .await
            .unwrap();
        store
            .delete(&router.ref_journal_plan_receipt_path(&plan_id))
            .await
            .unwrap();
        let receipt = resolve_plan_receipt(&store, &router, &plan_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(first.attempt, 1);
        assert_eq!(receipt.attempt, 1);
        assert_eq!(receipt_transaction(&receipt), transaction_id);
    }

    #[tokio::test]
    async fn another_plan_with_the_same_ref_value_cannot_inherit_the_receipt() {
        let (store, router) = fixture("receipt/isolation");
        let committed_plan = "4".repeat(64);
        let other_plan = "5".repeat(64);
        let (transaction, heads) = transaction(&store, &router, "refs/heads/main", 'a').await;
        commit_ref_transaction_for_plan(
            &store,
            &router,
            &transaction,
            &heads,
            &committed_plan,
            || false,
        )
        .await
        .unwrap();

        assert!(
            resolve_plan_receipt(&store, &router, &other_plan)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn copied_receipt_is_rejected_by_the_repository_binding() {
        let (store, source) = fixture("receipt/source");
        let target = StoreLayout::new(store.clone(), "receipt/target".to_owned());
        let plan_id = "6".repeat(64);
        let (transaction, heads) = transaction(&store, &source, "refs/heads/main", 'a').await;
        commit_ref_transaction_for_plan(&store, &source, &transaction, &heads, &plan_id, || false)
            .await
            .unwrap();

        for (source_path, target_path) in [
            (
                source.ref_journal_plan_intent_path(&plan_id, 1),
                target.ref_journal_plan_intent_path(&plan_id, 1),
            ),
            (
                source.ref_journal_plan_receipt_path(&plan_id),
                target.ref_journal_plan_receipt_path(&plan_id),
            ),
        ] {
            let (bytes, _) = store.get_with_etag(&source_path).await.unwrap();
            store.put_exact(&target_path, bytes).await.unwrap();
        }

        let error = resolve_plan_receipt(&store, &target, &plan_id)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("repository mismatch"));
    }

    #[tokio::test]
    async fn corrupt_terminal_receipt_fails_closed() {
        let (store, router) = fixture("receipt/corrupt");
        let plan_id = "c".repeat(64);
        store
            .put_exact(
                &router.ref_journal_plan_receipt_path(&plan_id),
                Bytes::from_static(b"{}"),
            )
            .await
            .unwrap();
        let (transaction, heads) = transaction(&store, &router, "refs/heads/main", 'a').await;
        commit_ref_transaction_for_plan(&store, &router, &transaction, &heads, &plan_id, || false)
            .await
            .unwrap();

        let error = resolve_plan_receipt(&store, &router, &plan_id)
            .await
            .unwrap_err();

        assert!(matches!(error, MetadataError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn committed_intent_without_a_receipt_blocks_a_second_transaction() {
        let (store, router) = fixture("receipt/committed-retry");
        let plan_id = "7".repeat(64);
        let (committed, heads) = transaction(&store, &router, "refs/heads/main", 'a').await;
        commit_ref_transaction_for_plan(&store, &router, &committed, &heads, &plan_id, || false)
            .await
            .unwrap();
        store
            .delete(&router.ref_journal_plan_receipt_path(&plan_id))
            .await
            .unwrap();
        let (duplicate, _) = transaction(&store, &router, "refs/heads/main", 'b').await;

        let error = prepare_ref_journal_plan_intent(&store, &router, &plan_id, &duplicate)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            MetadataError::Storage {
                source: StorageError::StateConflict { .. }
            }
        ));
    }

    #[tokio::test]
    async fn exhausted_uncommitted_attempts_fail_as_a_state_conflict() {
        let (store, router) = fixture("receipt/exhausted");
        let plan_id = "8".repeat(64);
        for value in ['a', 'b', 'c'] {
            let (candidate, _) = transaction(&store, &router, "refs/heads/main", value).await;
            let candidate_id = candidate.id().unwrap();
            store
                .put_exact(
                    &router.ref_journal_transaction_path(&candidate_id),
                    Bytes::from(serde_json::to_vec(&candidate).unwrap()),
                )
                .await
                .unwrap();
            prepare_ref_journal_plan_intent(&store, &router, &plan_id, &candidate)
                .await
                .unwrap();
        }
        let (candidate, _) = transaction(&store, &router, "refs/heads/main", 'd').await;
        let candidate_id = candidate.id().unwrap();
        store
            .put_exact(
                &router.ref_journal_transaction_path(&candidate_id),
                Bytes::from(serde_json::to_vec(&candidate).unwrap()),
            )
            .await
            .unwrap();

        let error = prepare_ref_journal_plan_intent(&store, &router, &plan_id, &candidate)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            MetadataError::Storage {
                source: StorageError::StateConflict { .. }
            }
        ));
    }

    #[tokio::test]
    async fn managed_manifest_retry_reuses_intent_and_recovers_receipt() {
        let (store, router) = fixture("receipt/managed");
        let plan_id = "9".repeat(64);
        let base = Manifest::default_for_repo("refs/heads/main");
        crate::manifest_store::create_manifest(&store, &router, &base)
            .await
            .unwrap();
        let (_, etag) = read_manifest(&store, &router).await.unwrap();
        let mut candidate = base.clone();
        candidate.generation += 1;
        candidate
            .refs
            .insert("refs/heads/main".to_owned(), "a".repeat(40));
        candidate.seal_git_validation();
        prepare_plan_intent(
            &store,
            &router,
            &plan_id,
            MirrorPlanCommit::Manifest {
                base_generation: base.generation,
                base_digest: manifest_digest(&base).unwrap(),
                generation: candidate.generation,
                digest: manifest_digest(&candidate).unwrap(),
            },
        )
        .await
        .unwrap();

        commit_manifest_for_plan(&store, &router, &candidate, &etag, &plan_id)
            .await
            .unwrap();
        store
            .delete(&router.ref_journal_plan_receipt_path(&plan_id))
            .await
            .unwrap();
        let receipt = resolve_plan_receipt(&store, &router, &plan_id)
            .await
            .unwrap()
            .unwrap();

        assert!(
            receipt.attempt == 1
                && matches!(
                    receipt.commit,
                    MirrorPlanCommit::Manifest { generation: 1, .. }
                )
        );
    }

    #[tokio::test]
    async fn missing_managed_receipt_recovers_after_a_successor_manifest() {
        let (store, router) = fixture("receipt/managed-history");
        let plan_id = "a".repeat(64);
        let base = Manifest::default_for_repo("refs/heads/main");
        crate::manifest_store::create_manifest(&store, &router, &base)
            .await
            .unwrap();
        let (_, etag) = read_manifest(&store, &router).await.unwrap();
        let mut candidate = base.clone();
        candidate.generation += 1;
        candidate
            .refs
            .insert("refs/heads/main".to_owned(), "a".repeat(40));
        candidate.seal_git_validation();
        commit_manifest_for_plan(&store, &router, &candidate, &etag, &plan_id)
            .await
            .unwrap();
        store
            .delete(&router.ref_journal_plan_receipt_path(&plan_id))
            .await
            .unwrap();
        let (_, etag) = read_manifest(&store, &router).await.unwrap();
        let mut successor = candidate;
        successor.generation += 1;
        successor.session_id = "successor".to_owned();
        successor.seal_git_validation();
        write_manifest_cas(&store, &router, &successor, &etag)
            .await
            .unwrap();

        let receipt = resolve_plan_receipt(&store, &router, &plan_id)
            .await
            .unwrap()
            .unwrap();

        assert!(matches!(
            receipt.commit,
            MirrorPlanCommit::Manifest { generation: 1, .. }
        ));
    }
}
