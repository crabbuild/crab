//! Atomic multi-ref publication without a repository-wide mutable pointer.

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use crab_storage::{ETag, StorageError, Store, StoreLayout};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::error::{MetadataError, Result};
use crate::manifests::{Manifest, PackManifestEntry, validate_pack_manifest_entry};
use crate::validation::{corrupt_object, validate_content_hash, validate_sha1};

const REF_JOURNAL_VERSION: u32 = 1;
const MAX_REF_HEADS: usize = 1_000_000;
const MAX_ACTIVE_TRANSACTIONS: usize = 1_000_000;

/// One expected-old ref edit committed by a journal transaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RefJournalEdit {
    pub ref_name: String,
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peeled_oid: Option<String>,
}

/// Immutable data published atomically by one push.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RefJournalTransaction {
    pub version: u32,
    /// Visible parent transaction for every edited ref.
    pub parents: BTreeMap<String, Option<String>>,
    /// Ref edits sorted by canonical ref name.
    pub edits: Vec<RefJournalEdit>,
    /// HEAD replacement only when this transaction retargets HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// New immutable Git packs made reachable by these edits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packs: Vec<PackManifestEntry>,
    /// New immutable shard hashes made reachable by these edits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shards: Vec<String>,
}

impl RefJournalTransaction {
    /// Builds and validates one canonical transaction body.
    pub fn new(
        parents: BTreeMap<String, Option<String>>,
        mut edits: Vec<RefJournalEdit>,
        head: Option<String>,
        mut packs: Vec<PackManifestEntry>,
        mut shards: Vec<String>,
    ) -> Result<Self> {
        edits.sort_unstable_by(|left, right| left.ref_name.cmp(&right.ref_name));
        packs.sort_unstable_by(|left, right| left.pack_id.cmp(&right.pack_id));
        shards.sort_unstable();
        let transaction = Self {
            version: REF_JOURNAL_VERSION,
            parents,
            edits,
            head,
            packs,
            shards,
        };
        validate_transaction(&transaction)?;
        Ok(transaction)
    }

    /// Returns the Blake3 identity of the canonical JSON body.
    pub fn id(&self) -> Result<String> {
        Ok(blake3::hash(&serialize(self)?).to_hex().to_string())
    }
}

/// Mutable pointer for one ref. Prepared state is invisible until its marker exists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RefJournalHead {
    pub version: u32,
    pub ref_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_transaction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_transaction: Option<String>,
}

impl RefJournalHead {
    fn empty(ref_name: &str) -> Self {
        Self {
            version: REF_JOURNAL_VERSION,
            ref_name: ref_name.to_owned(),
            committed_transaction: None,
            prepared_transaction: None,
        }
    }
}

/// Head body, CAS token, and currently visible transaction observed together.
#[derive(Debug, Clone)]
pub struct RefJournalHeadSnapshot {
    pub head: RefJournalHead,
    pub etag: Option<ETag>,
    pub visible_transaction: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RefJournalActiveMarker {
    version: u32,
    transaction_id: String,
}

/// Result of an atomic journal commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefJournalCommitResult {
    pub transaction_id: String,
    pub edited_refs: usize,
}

/// Journal positions already folded into one immutable manifest state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RefJournalFrontier {
    pub version: u32,
    pub manifest_git_validation_digest: String,
    pub heads: BTreeMap<String, String>,
}

/// Fully materialized repository state after applying committed ref transactions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefJournalSnapshot {
    pub refs: BTreeMap<String, String>,
    pub peeled_refs: BTreeMap<String, String>,
    pub head: String,
    pub packs: Vec<PackManifestEntry>,
    pub shards: Vec<String>,
    /// Canonically ordered transaction identities included in this view.
    pub transactions: Vec<String>,
    /// Visible per-ref journal positions used to publish a compaction frontier.
    pub visible_heads: BTreeMap<String, String>,
    /// Digest binding the base manifest and ordered transaction set.
    pub state_digest: String,
}

/// Hash a canonical ref name for its mutable head key.
#[must_use]
pub fn ref_name_hash(ref_name: &str) -> String {
    blake3::hash(ref_name.as_bytes()).to_hex().to_string()
}

/// Read one ref head and resolve prepared state through its immutable marker.
pub async fn read_ref_head(
    store: &Store,
    router: &StoreLayout<Store>,
    ref_name: &str,
) -> Result<RefJournalHeadSnapshot> {
    validate_ref_name(ref_name, "ref journal head")?;
    let path = router.ref_journal_head_path(&ref_name_hash(ref_name));
    match store.get_with_etag(&path).await {
        Ok((body, etag)) => {
            let head: RefJournalHead =
                serde_json::from_slice(&body).map_err(|error| MetadataError::CorruptObject {
                    path: path.to_string(),
                    reason: format!("invalid ref journal head JSON: {error}"),
                })?;
            validate_head(&head, ref_name, path.as_ref())?;
            let visible_transaction = visible_transaction(store, router, &head).await?;
            Ok(RefJournalHeadSnapshot {
                head,
                etag: Some(etag),
                visible_transaction,
            })
        }
        Err(StorageError::NotFound { .. }) => Ok(RefJournalHeadSnapshot {
            head: RefJournalHead::empty(ref_name),
            etag: None,
            visible_transaction: None,
        }),
        Err(source) => Err(source.into()),
    }
}

/// Commit one transaction after callers hold every edited ref lock.
///
/// Each head first points at invisible prepared state. The immutable commit
/// marker makes every edit visible together; later head promotion is cleanup.
pub async fn commit_ref_transaction(
    store: &Store,
    router: &StoreLayout<Store>,
    transaction: &RefJournalTransaction,
    expected_heads: &[RefJournalHeadSnapshot],
) -> Result<RefJournalCommitResult> {
    validate_transaction(transaction)?;
    validate_expected_heads(transaction, expected_heads)?;
    let transaction_id = transaction.id()?;
    let transaction_path = router.ref_journal_transaction_path(&transaction_id);
    store
        .put_exact(&transaction_path, Bytes::from(serialize(transaction)?))
        .await?;

    let mut prepared = Vec::with_capacity(expected_heads.len());
    for expected in expected_heads {
        match prepare_head(store, router, expected, &transaction_id).await {
            Ok(snapshot) => prepared.push((expected, snapshot)),
            Err(error) => {
                rollback_prepared_heads(store, router, &prepared).await;
                return Err(error);
            }
        }
    }

    let marker = RefJournalActiveMarker {
        version: REF_JOURNAL_VERSION,
        transaction_id: transaction_id.clone(),
    };
    store
        .put_exact(
            &router.ref_journal_active_path(&transaction_id),
            Bytes::from(serialize(&marker)?),
        )
        .await?;

    for (_, prepared_head) in prepared {
        if let Err(error) = promote_head(store, router, prepared_head, &transaction_id).await {
            // The marker is the atomic visibility boundary. Promotion is only
            // bounded read cleanup and must not turn a committed push into an error.
            warn!(
                ref_name = %error.0,
                error = %error.1,
                "committed ref journal head promotion needs repair"
            );
        }
    }

    Ok(RefJournalCommitResult {
        transaction_id,
        edited_refs: expected_heads.len(),
    })
}

/// Read and verify an immutable transaction by identity.
pub async fn read_transaction(
    store: &Store,
    router: &StoreLayout<Store>,
    transaction_id: &str,
) -> Result<RefJournalTransaction> {
    validate_content_hash(
        transaction_id,
        "ref journal transaction id",
        "ref journal transaction",
    )?;
    let path = router.ref_journal_transaction_path(transaction_id);
    let (body, _) = store.get_with_etag(&path).await?;
    if blake3::hash(&body).to_hex().as_str() != transaction_id {
        return Err(corrupt_object(
            path.as_ref(),
            "ref journal transaction body does not match its identity",
        ));
    }
    let transaction: RefJournalTransaction =
        serde_json::from_slice(&body).map_err(|error| MetadataError::CorruptObject {
            path: path.to_string(),
            reason: format!("invalid ref journal transaction JSON: {error}"),
        })?;
    validate_transaction(&transaction)?;
    Ok(transaction)
}

/// List every ref head with a hard repository cardinality bound.
pub async fn list_ref_heads(
    store: &Store,
    router: &StoreLayout<Store>,
) -> Result<Vec<RefJournalHeadSnapshot>> {
    let prefix = router.ref_journal_heads_prefix();
    let objects = store
        .list_prefix_bounded(&prefix, MAX_REF_HEADS)
        .await?
        .ok_or_else(|| MetadataError::Internal("ref journal head limit exceeded".to_owned()))?;
    let mut heads = Vec::with_capacity(objects.len());
    for object in objects {
        let (body, etag) = store.get_with_etag(&object.location).await?;
        let head: RefJournalHead =
            serde_json::from_slice(&body).map_err(|error| MetadataError::CorruptObject {
                path: object.location.to_string(),
                reason: format!("invalid ref journal head JSON: {error}"),
            })?;
        validate_head(&head, &head.ref_name, object.location.as_ref())?;
        let expected_path = router.ref_journal_head_path(&ref_name_hash(&head.ref_name));
        if expected_path != object.location {
            return Err(corrupt_object(
                object.location.as_ref(),
                "ref journal head key does not match its ref name",
            ));
        }
        let visible_transaction = visible_transaction(store, router, &head).await?;
        heads.push(RefJournalHeadSnapshot {
            head,
            etag: Some(etag),
            visible_transaction,
        });
    }
    heads.sort_unstable_by(|left, right| left.head.ref_name.cmp(&right.head.ref_name));
    Ok(heads)
}

/// List transaction identities visible at one repository read's linearization point.
pub async fn list_active_transactions(
    store: &Store,
    router: &StoreLayout<Store>,
) -> Result<BTreeSet<String>> {
    let prefix = router.ref_journal_active_prefix();
    let objects = store
        .list_prefix_bounded(&prefix, MAX_ACTIVE_TRANSACTIONS)
        .await?
        .ok_or_else(|| {
            MetadataError::Internal("active ref transaction limit exceeded".to_owned())
        })?;
    let prefix = format!("{prefix}/");
    objects
        .into_iter()
        .map(|object| {
            let transaction_id = object
                .location
                .as_ref()
                .strip_prefix(&prefix)
                .and_then(|name| name.strip_suffix(".json"))
                .ok_or_else(|| {
                    corrupt_object(
                        object.location.as_ref(),
                        "active ref transaction key has an invalid shape",
                    )
                })?;
            validate_content_hash(
                transaction_id,
                "active ref transaction id",
                object.location.as_ref(),
            )?;
            Ok(transaction_id.to_owned())
        })
        .collect()
}

/// Remove compacted visibility markers once no failed head promotion needs them.
pub async fn cleanup_compacted_transactions(
    store: &Store,
    router: &StoreLayout<Store>,
    transaction_ids: &[String],
) {
    for transaction_id in transaction_ids {
        let transaction = match read_transaction(store, router, transaction_id).await {
            Ok(transaction) => transaction,
            Err(error) => {
                warn!(%transaction_id, %error, "retaining compacted ref transaction marker");
                continue;
            }
        };
        let mut promotion_pending = false;
        for edit in &transaction.edits {
            match read_ref_head(store, router, &edit.ref_name).await {
                Ok(head) => {
                    promotion_pending |=
                        head.head.prepared_transaction.as_deref() == Some(transaction_id);
                }
                Err(error) => {
                    warn!(%transaction_id, ref_name = %edit.ref_name, %error, "retaining compacted ref transaction marker");
                    promotion_pending = true;
                    break;
                }
            }
        }
        if promotion_pending {
            continue;
        }
        let path = router.ref_journal_active_path(transaction_id);
        if let Err(error) = store.delete(&path).await
            && !matches!(error, StorageError::NotFound { .. })
        {
            warn!(%transaction_id, %error, "failed to clean compacted ref transaction marker");
        }
    }
}

/// Publish the immutable frontier before its matching compacted manifest CAS.
pub async fn write_ref_journal_frontier(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
    heads: &BTreeMap<String, String>,
) -> Result<()> {
    let frontier = RefJournalFrontier {
        version: REF_JOURNAL_VERSION,
        manifest_git_validation_digest: manifest.git_validation_digest.clone(),
        heads: heads.clone(),
    };
    validate_frontier(&frontier, manifest)?;
    store
        .put_exact(
            &router.ref_journal_frontier_path(&manifest.git_validation_digest),
            Bytes::from(serialize(&frontier)?),
        )
        .await?;
    Ok(())
}

/// Materialize one coherent repository view from the compacted manifest plus
/// the active transactions captured before reading that manifest.
pub async fn materialize_ref_journal(
    store: &Store,
    router: &StoreLayout<Store>,
    base: &Manifest,
    base_packs: &[PackManifestEntry],
    base_shards: &[String],
    active_transactions: &BTreeSet<String>,
) -> Result<RefJournalSnapshot> {
    let frontier = read_ref_journal_frontier(store, router, base).await?;
    let compacted = frontier
        .as_ref()
        .map(|value| value.heads.values().cloned().collect::<BTreeSet<_>>())
        .unwrap_or_default();
    let mut visible_heads = frontier
        .as_ref()
        .map(|value| value.heads.clone())
        .unwrap_or_default();
    let mut pending = active_transactions
        .iter()
        .filter(|transaction| !compacted.contains(*transaction))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut transactions = BTreeMap::new();
    while let Some(transaction_id) = pending.pop_first() {
        if transactions.contains_key(&transaction_id) {
            continue;
        }
        if transactions.len() == MAX_REF_HEADS {
            return Err(MetadataError::Internal(
                "ref journal transaction limit exceeded".to_owned(),
            ));
        }
        let transaction = read_transaction(store, router, &transaction_id).await?;
        for parent in transaction.parents.values().flatten() {
            if !transactions.contains_key(parent) && !compacted.contains(parent) {
                pending.insert(parent.clone());
            }
        }
        transactions.insert(transaction_id, transaction);
    }

    let order = transaction_order(&transactions)?;
    let mut refs = base.refs.clone();
    let mut peeled_refs = base.peeled_refs.clone();
    let mut head = base.head.clone();
    let mut packs = base_packs
        .iter()
        .cloned()
        .map(|pack| (pack.pack_id.clone(), pack))
        .collect::<BTreeMap<_, _>>();
    let mut shards = base_shards.iter().cloned().collect::<BTreeSet<_>>();

    for transaction_id in &order {
        let transaction = transactions.get(transaction_id).ok_or_else(|| {
            MetadataError::Internal("ordered ref journal transaction disappeared".to_owned())
        })?;
        for edit in &transaction.edits {
            if refs.get(&edit.ref_name) != edit.old_oid.as_ref() {
                return Err(corrupt_object(
                    router.ref_journal_transaction_path(transaction_id).as_ref(),
                    "ref journal edit old OID does not match its parent state",
                ));
            }
            match &edit.new_oid {
                Some(oid) => {
                    refs.insert(edit.ref_name.clone(), oid.clone());
                    match &edit.peeled_oid {
                        Some(peeled) => {
                            peeled_refs.insert(edit.ref_name.clone(), peeled.clone());
                        }
                        None => {
                            peeled_refs.remove(&edit.ref_name);
                        }
                    }
                }
                None => {
                    refs.remove(&edit.ref_name);
                    peeled_refs.remove(&edit.ref_name);
                }
            }
            visible_heads.insert(edit.ref_name.clone(), transaction_id.clone());
        }
        if let Some(next_head) = &transaction.head {
            head.clone_from(next_head);
        }
        for pack in &transaction.packs {
            match packs.get(&pack.pack_id) {
                Some(existing) if existing != pack => {
                    return Err(corrupt_object(
                        router.ref_journal_transaction_path(transaction_id).as_ref(),
                        "ref journal pack identity has conflicting metadata",
                    ));
                }
                Some(_) => {}
                None => {
                    packs.insert(pack.pack_id.clone(), pack.clone());
                }
            }
        }
        shards.extend(transaction.shards.iter().cloned());
    }
    if !refs.is_empty() && !refs.contains_key(&head) {
        return Err(corrupt_object(
            router.ref_journal_heads_prefix().as_ref(),
            "materialized ref journal HEAD does not resolve",
        ));
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab ref journal state v1\0");
    hasher.update(base.git_validation_digest.as_bytes());
    for transaction_id in &order {
        hasher.update(transaction_id.as_bytes());
    }
    Ok(RefJournalSnapshot {
        refs,
        peeled_refs,
        head,
        packs: packs.into_values().collect(),
        shards: shards.into_iter().collect(),
        transactions: order,
        visible_heads,
        state_digest: hasher.finalize().to_hex().to_string(),
    })
}

pub(crate) async fn read_ref_journal_frontier(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
) -> Result<Option<RefJournalFrontier>> {
    let path = router.ref_journal_frontier_path(&manifest.git_validation_digest);
    let body = match store.get_with_etag(&path).await {
        Ok((body, _)) => body,
        Err(StorageError::NotFound { .. }) => return Ok(None),
        Err(source) => return Err(source.into()),
    };
    let frontier: RefJournalFrontier =
        serde_json::from_slice(&body).map_err(|error| MetadataError::CorruptObject {
            path: path.to_string(),
            reason: format!("invalid ref journal frontier JSON: {error}"),
        })?;
    validate_frontier(&frontier, manifest)?;
    Ok(Some(frontier))
}

fn transaction_order(
    transactions: &BTreeMap<String, RefJournalTransaction>,
) -> Result<Vec<String>> {
    let mut remaining = transactions.keys().cloned().collect::<BTreeSet<_>>();
    let mut order = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .filter(|transaction_id| {
                transactions
                    .get(*transaction_id)
                    .into_iter()
                    .flat_map(|transaction| transaction.parents.values().flatten())
                    .all(|parent| !remaining.contains(parent))
            })
            .cloned()
            .collect::<Vec<_>>();
        if ready.is_empty() {
            return Err(corrupt_object(
                "ref journal transactions",
                "ref journal transaction parent graph contains a cycle",
            ));
        }
        for transaction_id in ready {
            remaining.remove(&transaction_id);
            order.push(transaction_id);
        }
    }
    Ok(order)
}

async fn prepare_head(
    store: &Store,
    router: &StoreLayout<Store>,
    expected: &RefJournalHeadSnapshot,
    transaction_id: &str,
) -> Result<RefJournalHeadSnapshot> {
    let mut next = expected.head.clone();
    next.committed_transaction = expected.visible_transaction.clone();
    next.prepared_transaction = Some(transaction_id.to_owned());
    let path = router.ref_journal_head_path(&ref_name_hash(&next.ref_name));
    let body = Bytes::from(serialize(&next)?);
    let etag = match &expected.etag {
        Some(etag) => store.update(&path, body, etag.clone()).await?,
        None => store.create_strict_with_etag(&path, body).await?,
    };
    Ok(RefJournalHeadSnapshot {
        head: next,
        etag: Some(etag),
        visible_transaction: expected.visible_transaction.clone(),
    })
}

async fn promote_head(
    store: &Store,
    router: &StoreLayout<Store>,
    mut prepared: RefJournalHeadSnapshot,
    transaction_id: &str,
) -> std::result::Result<(), (String, StorageError)> {
    prepared.head.committed_transaction = Some(transaction_id.to_owned());
    prepared.head.prepared_transaction = None;
    let path = router.ref_journal_head_path(&ref_name_hash(&prepared.head.ref_name));
    let body = serialize(&prepared.head)
        .map(Bytes::from)
        .map_err(|error| {
            (
                prepared.head.ref_name.clone(),
                StorageError::Internal(error.to_string()),
            )
        })?;
    let etag = prepared.etag.ok_or_else(|| {
        (
            prepared.head.ref_name.clone(),
            StorageError::Internal("prepared ref head lost its CAS token".to_owned()),
        )
    })?;
    store
        .update(&path, body, etag)
        .await
        .map(|_| ())
        .map_err(|error| (prepared.head.ref_name, error))
}

async fn rollback_prepared_heads(
    store: &Store,
    router: &StoreLayout<Store>,
    prepared: &[(&RefJournalHeadSnapshot, RefJournalHeadSnapshot)],
) {
    for (original, written) in prepared.iter().rev() {
        let path = router.ref_journal_head_path(&ref_name_hash(&original.head.ref_name));
        let result = match (&original.etag, &written.etag) {
            (Some(_), Some(written_etag)) => match serialize(&original.head) {
                Ok(body) => store
                    .update(&path, Bytes::from(body), written_etag.clone())
                    .await
                    .map(|_| ())
                    .map_err(MetadataError::from),
                Err(error) => Err(error),
            },
            (None, Some(_)) => store.delete(&path).await.map_err(MetadataError::from),
            _ => Ok(()),
        };
        if let Err(error) = result {
            warn!(ref_name = %original.head.ref_name, %error, "failed to roll back uncommitted ref journal head");
        }
    }
}

async fn visible_transaction(
    store: &Store,
    router: &StoreLayout<Store>,
    head: &RefJournalHead,
) -> Result<Option<String>> {
    let Some(prepared) = head.prepared_transaction.as_deref() else {
        return Ok(head.committed_transaction.clone());
    };
    if active_marker_exists(store, router, prepared).await? {
        Ok(Some(prepared.to_owned()))
    } else {
        Ok(head.committed_transaction.clone())
    }
}

async fn active_marker_exists(
    store: &Store,
    router: &StoreLayout<Store>,
    transaction_id: &str,
) -> Result<bool> {
    match store
        .head(&router.ref_journal_active_path(transaction_id))
        .await
    {
        Ok(_) => Ok(true),
        Err(StorageError::NotFound { .. }) => Ok(false),
        Err(source) => Err(source.into()),
    }
}

fn validate_expected_heads(
    transaction: &RefJournalTransaction,
    expected_heads: &[RefJournalHeadSnapshot],
) -> Result<()> {
    if expected_heads.len() != transaction.edits.len() {
        return Err(MetadataError::Internal(
            "ref journal transaction and head counts differ".to_owned(),
        ));
    }
    for (edit, expected) in transaction.edits.iter().zip(expected_heads) {
        if edit.ref_name != expected.head.ref_name
            || transaction.parents.get(&edit.ref_name) != Some(&expected.visible_transaction)
        {
            return Err(MetadataError::Internal(format!(
                "ref journal head for {} changed before commit",
                edit.ref_name
            )));
        }
    }
    Ok(())
}

fn validate_transaction(transaction: &RefJournalTransaction) -> Result<()> {
    if transaction.version != REF_JOURNAL_VERSION || transaction.edits.is_empty() {
        return Err(corrupt_object(
            "ref journal transaction",
            "transaction version or edit set is invalid",
        ));
    }
    let mut names = BTreeSet::new();
    let mut previous = None;
    for edit in &transaction.edits {
        validate_ref_name(&edit.ref_name, "ref journal transaction")?;
        if !names.insert(edit.ref_name.as_str())
            || previous.is_some_and(|name| name >= edit.ref_name.as_str())
        {
            return Err(corrupt_object(
                "ref journal transaction",
                "transaction edits must be unique and sorted",
            ));
        }
        previous = Some(edit.ref_name.as_str());
        if edit.old_oid.is_none() && edit.new_oid.is_none() {
            return Err(corrupt_object(
                "ref journal transaction",
                "ref edit cannot keep both old and new OIDs absent",
            ));
        }
        if let Some(oid) = &edit.old_oid {
            validate_sha1(oid, "ref journal old oid", "ref journal transaction")?;
        }
        if let Some(oid) = &edit.new_oid {
            validate_sha1(oid, "ref journal new oid", "ref journal transaction")?;
        }
        if let Some(oid) = &edit.peeled_oid {
            validate_sha1(oid, "ref journal peeled oid", "ref journal transaction")?;
        }
        match transaction.parents.get(&edit.ref_name) {
            Some(Some(parent)) => validate_content_hash(
                parent,
                "ref journal parent transaction",
                "ref journal transaction",
            )?,
            Some(None) => {}
            None => {
                return Err(corrupt_object(
                    "ref journal transaction",
                    "transaction is missing an edited ref parent",
                ));
            }
        }
    }
    if transaction.parents.len() != transaction.edits.len() {
        return Err(corrupt_object(
            "ref journal transaction",
            "transaction contains an unedited ref parent",
        ));
    }
    if let Some(head) = &transaction.head {
        validate_ref_name(head, "ref journal HEAD")?;
    }
    for pack in &transaction.packs {
        validate_pack_manifest_entry(pack)?;
    }
    if transaction
        .packs
        .windows(2)
        .any(|pair| pair[0].pack_id >= pair[1].pack_id)
    {
        return Err(corrupt_object(
            "ref journal transaction",
            "transaction packs must be unique and sorted",
        ));
    }
    for shard in &transaction.shards {
        validate_content_hash(shard, "ref journal shard hash", "ref journal transaction")?;
    }
    if transaction.shards.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(corrupt_object(
            "ref journal transaction",
            "transaction shards must be unique and sorted",
        ));
    }
    Ok(())
}

fn validate_head(head: &RefJournalHead, ref_name: &str, path: &str) -> Result<()> {
    if head.version != REF_JOURNAL_VERSION || head.ref_name != ref_name {
        return Err(corrupt_object(path, "ref journal head identity is invalid"));
    }
    validate_ref_name(&head.ref_name, path)?;
    for transaction in [
        head.committed_transaction.as_deref(),
        head.prepared_transaction.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_content_hash(transaction, "ref journal head transaction", path)?;
    }
    Ok(())
}

fn validate_frontier(frontier: &RefJournalFrontier, manifest: &Manifest) -> Result<()> {
    if frontier.version != REF_JOURNAL_VERSION
        || frontier.manifest_git_validation_digest != manifest.git_validation_digest
    {
        return Err(corrupt_object(
            "ref journal frontier",
            "frontier does not match its compacted manifest",
        ));
    }
    for (ref_name, transaction) in &frontier.heads {
        validate_ref_name(ref_name, "ref journal frontier")?;
        validate_content_hash(
            transaction,
            "ref journal frontier transaction",
            "ref journal frontier",
        )?;
    }
    Ok(())
}

fn validate_ref_name(ref_name: &str, path: &str) -> Result<()> {
    if !ref_name.starts_with("refs/")
        || ref_name.len() > 1_024
        || ref_name.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(corrupt_object(path, "ref journal ref name is invalid"));
    }
    Ok(())
}

fn serialize<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|error| MetadataError::Internal(format!("ref journal serialize: {error}")))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;

    use super::*;

    fn fixture() -> (Store, StoreLayout<Store>) {
        let store = Store::new(Arc::new(InMemory::new()));
        let layout = StoreLayout::new(store.clone(), "org/repo".to_owned());
        (store, layout)
    }

    fn edit(ref_name: &str, byte: char) -> RefJournalEdit {
        RefJournalEdit {
            ref_name: ref_name.to_owned(),
            old_oid: None,
            new_oid: Some(byte.to_string().repeat(40)),
            peeled_oid: None,
        }
    }

    async fn transaction_for(
        store: &Store,
        layout: &StoreLayout<Store>,
        edits: Vec<RefJournalEdit>,
    ) -> (RefJournalTransaction, Vec<RefJournalHeadSnapshot>) {
        let mut heads = Vec::new();
        let mut parents = BTreeMap::new();
        for edit in &edits {
            let head = read_ref_head(store, layout, &edit.ref_name).await.unwrap();
            parents.insert(edit.ref_name.clone(), head.visible_transaction.clone());
            heads.push(head);
        }
        (
            RefJournalTransaction::new(parents, edits, None, Vec::new(), Vec::new()).unwrap(),
            heads,
        )
    }

    async fn materialize(
        store: &Store,
        layout: &StoreLayout<Store>,
        base: &Manifest,
    ) -> RefJournalSnapshot {
        let active = list_active_transactions(store, layout).await.unwrap();
        materialize_ref_journal(store, layout, base, &[], &[], &active)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn unrelated_refs_commit_through_distinct_mutable_heads() {
        let (store, layout) = fixture();
        let (left, left_heads) =
            transaction_for(&store, &layout, vec![edit("refs/heads/left", 'a')]).await;
        let (right, right_heads) =
            transaction_for(&store, &layout, vec![edit("refs/heads/right", 'b')]).await;

        let (left_result, right_result) = tokio::join!(
            commit_ref_transaction(&store, &layout, &left, &left_heads),
            commit_ref_transaction(&store, &layout, &right, &right_heads),
        );

        assert!(left_result.is_ok());
        assert!(right_result.is_ok());
        assert_ne!(
            layout.ref_journal_head_path(&ref_name_hash("refs/heads/left")),
            layout.ref_journal_head_path(&ref_name_hash("refs/heads/right"))
        );
        assert!(store.head(&layout.manifest_path()).await.is_err());
    }

    #[tokio::test]
    async fn same_ref_rejects_a_stale_head_snapshot() {
        let (store, layout) = fixture();
        let (first, stale_heads) =
            transaction_for(&store, &layout, vec![edit("refs/heads/main", 'a')]).await;
        let (second, _) =
            transaction_for(&store, &layout, vec![edit("refs/heads/main", 'b')]).await;

        commit_ref_transaction(&store, &layout, &first, &stale_heads)
            .await
            .unwrap();

        assert!(
            commit_ref_transaction(&store, &layout, &second, &stale_heads)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn multi_ref_transaction_becomes_visible_at_one_marker() {
        let (store, layout) = fixture();
        let edits = vec![edit("refs/heads/left", 'a'), edit("refs/heads/right", 'b')];
        let (transaction, heads) = transaction_for(&store, &layout, edits).await;
        let transaction_id = transaction.id().unwrap();
        store
            .put_exact(
                &layout.ref_journal_transaction_path(&transaction_id),
                Bytes::from(serialize(&transaction).unwrap()),
            )
            .await
            .unwrap();
        for head in &heads {
            prepare_head(&store, &layout, head, &transaction_id)
                .await
                .unwrap();
        }

        assert!(
            list_ref_heads(&store, &layout)
                .await
                .unwrap()
                .iter()
                .all(|head| head.visible_transaction.is_none())
        );

        let marker = RefJournalActiveMarker {
            version: REF_JOURNAL_VERSION,
            transaction_id: transaction_id.clone(),
        };
        store
            .put_exact(
                &layout.ref_journal_active_path(&transaction_id),
                Bytes::from(serialize(&marker).unwrap()),
            )
            .await
            .unwrap();

        assert!(
            list_ref_heads(&store, &layout)
                .await
                .unwrap()
                .iter()
                .all(|head| head.visible_transaction.as_deref() == Some(&transaction_id))
        );
    }

    #[tokio::test]
    async fn abandoned_prepared_state_is_reclaimed_by_the_next_commit() {
        let (store, layout) = fixture();
        let (abandoned, heads) =
            transaction_for(&store, &layout, vec![edit("refs/heads/main", 'a')]).await;
        let abandoned_id = abandoned.id().unwrap();
        prepare_head(&store, &layout, &heads[0], &abandoned_id)
            .await
            .unwrap();
        let observed = read_ref_head(&store, &layout, "refs/heads/main")
            .await
            .unwrap();
        assert!(observed.visible_transaction.is_none());

        let mut parents = BTreeMap::new();
        parents.insert("refs/heads/main".to_owned(), None);
        let next = RefJournalTransaction::new(
            parents,
            vec![edit("refs/heads/main", 'b')],
            None,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        commit_ref_transaction(&store, &layout, &next, &[observed])
            .await
            .unwrap();

        assert_eq!(
            read_ref_head(&store, &layout, "refs/heads/main")
                .await
                .unwrap()
                .visible_transaction,
            Some(next.id().unwrap())
        );
    }

    #[tokio::test]
    async fn materialization_merges_unrelated_transactions_without_a_manifest_write() {
        let (store, layout) = fixture();
        let mut base = Manifest::default_for_repo("refs/heads/main");
        base.refs
            .insert("refs/heads/main".to_owned(), "c".repeat(40));
        base.seal_git_validation();
        let (left, left_heads) =
            transaction_for(&store, &layout, vec![edit("refs/heads/left", 'a')]).await;
        let (right, right_heads) =
            transaction_for(&store, &layout, vec![edit("refs/heads/right", 'b')]).await;
        commit_ref_transaction(&store, &layout, &left, &left_heads)
            .await
            .unwrap();
        commit_ref_transaction(&store, &layout, &right, &right_heads)
            .await
            .unwrap();

        let snapshot = materialize(&store, &layout, &base).await;

        assert_eq!(snapshot.refs["refs/heads/left"], "a".repeat(40));
        assert_eq!(snapshot.refs["refs/heads/right"], "b".repeat(40));
        assert_eq!(snapshot.transactions.len(), 2);
        assert!(store.head(&layout.manifest_path()).await.is_err());
    }

    #[tokio::test]
    async fn materialization_does_not_read_dormant_ref_heads() {
        let (store, layout) = fixture();
        let mut base = Manifest::default_for_repo("refs/heads/main");
        base.refs
            .insert("refs/heads/main".to_owned(), "c".repeat(40));
        base.seal_git_validation();
        store
            .put_exact(
                &layout.ref_journal_head_path(&ref_name_hash("refs/heads/dormant")),
                Bytes::from_static(b"not-json"),
            )
            .await
            .unwrap();

        let snapshot = materialize(&store, &layout, &base).await;

        assert_eq!(snapshot.refs, base.refs);
    }

    #[tokio::test]
    async fn materialization_applies_same_ref_parent_chain_in_order() {
        let (store, layout) = fixture();
        let base = Manifest::default_for_repo("refs/heads/main");
        let (first, first_heads) =
            transaction_for(&store, &layout, vec![edit("refs/heads/main", 'a')]).await;
        commit_ref_transaction(&store, &layout, &first, &first_heads)
            .await
            .unwrap();
        let current = read_ref_head(&store, &layout, "refs/heads/main")
            .await
            .unwrap();
        let mut parents = BTreeMap::new();
        parents.insert(
            "refs/heads/main".to_owned(),
            current.visible_transaction.clone(),
        );
        let second = RefJournalTransaction::new(
            parents,
            vec![RefJournalEdit {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some("a".repeat(40)),
                new_oid: Some("b".repeat(40)),
                peeled_oid: None,
            }],
            None,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        commit_ref_transaction(&store, &layout, &second, &[current])
            .await
            .unwrap();

        let snapshot = materialize(&store, &layout, &base).await;

        assert_eq!(snapshot.refs["refs/heads/main"], "b".repeat(40));
        assert_eq!(
            snapshot.transactions,
            vec![first.id().unwrap(), second.id().unwrap()]
        );
    }

    #[tokio::test]
    async fn compaction_frontier_stops_replaying_folded_transactions() {
        let (store, layout) = fixture();
        let base = Manifest::default_for_repo("refs/heads/main");
        let (transaction, heads) =
            transaction_for(&store, &layout, vec![edit("refs/heads/main", 'a')]).await;
        let transaction_id = transaction.id().unwrap();
        commit_ref_transaction(&store, &layout, &transaction, &heads)
            .await
            .unwrap();
        let snapshot = materialize(&store, &layout, &base).await;
        let mut compacted = base;
        compacted.generation = 1;
        compacted.refs = snapshot.refs;
        compacted.peeled_refs = snapshot.peeled_refs;
        compacted.head = snapshot.head;
        compacted.seal_git_validation();
        write_ref_journal_frontier(&store, &layout, &compacted, &snapshot.visible_heads)
            .await
            .unwrap();
        store
            .delete(&layout.ref_journal_transaction_path(&transaction_id))
            .await
            .unwrap();

        let after = materialize(&store, &layout, &compacted).await;

        assert!(after.transactions.is_empty());
        assert_eq!(after.refs["refs/heads/main"], "a".repeat(40));
    }

    #[tokio::test]
    async fn old_manifest_reader_survives_active_marker_cleanup() {
        let (store, layout) = fixture();
        let base = Manifest::default_for_repo("refs/heads/main");
        let (transaction, heads) =
            transaction_for(&store, &layout, vec![edit("refs/heads/main", 'a')]).await;
        let transaction_id = transaction.id().unwrap();
        commit_ref_transaction(&store, &layout, &transaction, &heads)
            .await
            .unwrap();

        // Repository reads capture the active set before the manifest. A
        // compactor may remove its marker after publishing a newer manifest,
        // but this old-manifest reader must retain the captured transaction.
        let captured_active = list_active_transactions(&store, &layout).await.unwrap();
        store
            .delete(&layout.ref_journal_active_path(&transaction_id))
            .await
            .unwrap();

        let snapshot = materialize_ref_journal(&store, &layout, &base, &[], &[], &captured_active)
            .await
            .unwrap();

        assert_eq!(snapshot.refs["refs/heads/main"], "a".repeat(40));
    }
}
