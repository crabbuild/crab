//! Requested visibility changes from one authenticated embedded section.
use super::*;
use crate::pages::{PageCache, PageWork};
use crab_storage::{Store, StoreLayout};

/// Change to an object's membership in one ref closure; never standalone authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityChange {
    Added,
    Removed,
}

/// Bounded lookups captured from a capsule run's authenticated visibility descriptor.
#[derive(Debug)]
pub struct CapsuleVisibilityReader {
    path: object_store::path::Path,
    offset: u64,
    descriptor: Descriptor,
    maximum: u64,
    length: u64,
    hash: String,
    complete: std::sync::OnceLock<std::sync::Arc<CapsuleVisibilityDelta>>,
    cache: std::sync::Arc<PageCache>,
}

impl CapsuleVisibilityReader {
    pub(crate) fn new(
        layout: &StoreLayout<Store>,
        run_hash: &str,
        range: &super::super::super::PackRange,
        descriptor: Descriptor,
        maximum: u64,
        cache: std::sync::Arc<PageCache>,
    ) -> Result<Self> {
        descriptor.validate_region(range.length())?;
        range
            .offset()
            .checked_add(range.length())
            .ok_or_else(|| corrupt("visibility source span overflows"))?;
        Ok(Self {
            path: layout.capsule_path(run_hash),
            offset: range.offset(),
            descriptor,
            maximum,
            length: range.length(),
            hash: range.blake3().to_owned(),
            complete: std::sync::OnceLock::new(),
            cache,
        })
    }

    /// Return the exact number of ref headers advertised by this captured descriptor.
    pub fn ref_count(&self) -> u32 {
        self.descriptor.refs.count
    }

    pub(crate) fn coalesce_small_edit(&mut self, headroom: &std::sync::atomic::AtomicU64) {
        use std::sync::atomic::Ordering::Relaxed;

        let bytes = self.descriptor.body_bytes;
        // Coalesce at most two 8 KiB point leaves. Larger edits keep selected
        // paging instead of charging unrelated changes to every captured tip.
        // Reserve headroom before I/O; page admission and hashes still apply.
        if self.ref_count() == 1
            && bytes <= (crate::pages::TARGET_BYTES / 2) as u64
            && bytes <= self.maximum
            && headroom
                .try_update(Relaxed, Relaxed, |remaining| remaining.checked_sub(bytes))
                .is_ok()
        {
            self.cache =
                std::sync::Arc::new(PageCache::for_region(self.offset..self.offset + bytes));
        }
    }

    /// Read only scalar edit metadata, preserving requested ref order and authenticated misses.
    pub async fn headers(
        &self,
        store: &Store,
        names: &[&str],
    ) -> Result<Vec<Option<VisibilityEditHeader>>> {
        let mut work = PageWork::new(self.maximum, 0);
        self.read_headers(store, names, &mut work).await
    }

    async fn read_headers(
        &self,
        store: &Store,
        names: &[&str],
        work: &mut PageWork,
    ) -> Result<Vec<Option<VisibilityEditHeader>>> {
        if names
            .iter()
            .any(|name| !name.starts_with("refs/") || !valid_ref_name(name))
        {
            return Err(contract_error("visibility lookup ref name is invalid"));
        }
        let keys = names
            .iter()
            .map(|name| blake3::hash(name.as_bytes()).as_bytes().to_vec())
            .collect::<Vec<_>>();
        self.rows(
            store,
            &self.descriptor.refs,
            Kind::VisibilityRefs,
            &keys,
            work,
        )
        .await?
        .into_iter()
        .zip(keys)
        .zip(names)
        .map(|((value, key), name)| {
            let row = value
                .map(|bytes| VisibilityEditHeader::decode(&key, &bytes, &self.descriptor))
                .transpose()?;
            if row.as_ref().is_some_and(|row| row.name != *name) {
                return Err(corrupt("visibility lookup ref hash collides"));
            }
            if let Some(header) = &row {
                self.prime_roots(header)?;
            }
            Ok(row)
        })
        .collect()
    }

    /// Inspect one actual edit set through bounded authenticated pages.
    pub async fn objects(
        &self,
        store: &Store,
        name: &str,
        direction: VisibilityChange,
    ) -> Result<Vec<[u8; 20]>> {
        let mut work = PageWork::new(self.maximum, 0);
        let row = self
            .read_headers(store, &[name], &mut work)
            .await?
            .pop()
            .flatten()
            .ok_or_else(|| corrupt("required visibility ref is absent"))?;
        if let (Some(descriptor), Some(sets)) = (&self.descriptor.shared, &row.shared) {
            let reader = shared::Reader::new(
                store,
                &self.path,
                &self.cache,
                self.offset,
                self.descriptor.body_bytes,
                descriptor,
            )?;
            return reader
                .objects(
                    match direction {
                        VisibilityChange::Added => sets.added.as_ref(),
                        VisibilityChange::Removed => sets.removed.as_ref(),
                    },
                    &mut work,
                )
                .await;
        }
        let root = match direction {
            VisibilityChange::Added => row.added,
            VisibilityChange::Removed => row.removed,
        };
        let Some(root) = root else {
            return Ok(Vec::new());
        };
        let end = self
            .offset
            .checked_add(self.descriptor.body_bytes)
            .ok_or_else(|| corrupt("visibility body span overflows"))?;
        // Reconciliation retains these readers for closure queries. Whole-set
        // inspection must not populate every long-lived point cache; reuse warm
        // pages and read-ahead boundaries only for this scan.
        self.cache
            .fork()?
            .scan(
                store,
                &self.path,
                0,
                self.offset..end,
                &root,
                Kind::VisibilityChanges,
                &mut work,
            )
            .await?
            .into_iter()
            .map(|(oid, value)| {
                if !value.is_empty() {
                    return Err(corrupt("visibility object row has unexpected payload"));
                }
                oid.try_into()
                    .map_err(|_| corrupt("visibility OID has invalid length"))
            })
            .collect()
    }

    /// Authenticate and materialize the complete captured delta for bulk inspection.
    pub async fn materialize(
        &self,
        store: &Store,
    ) -> Result<std::sync::Arc<CapsuleVisibilityDelta>> {
        if self.length > self.maximum {
            return Err(corrupt("complete visibility exceeds its byte limit"));
        }
        if let Some(delta) = self.complete.get() {
            return Ok(delta.clone());
        }
        let end = self
            .offset
            .checked_add(self.length)
            .ok_or_else(|| corrupt("visibility source span overflows"))?;
        // Bulk transitions share encoded pool blocks with point proofs. Reusing
        // transport bytes must still pass the full section hash and decoder below.
        let bytes = self
            .cache
            .read_range(store, &self.path, self.offset..end, self.maximum)
            .await?;
        if blake3::hash(&bytes).to_hex().as_str() != self.hash
            || Descriptor::decode(&bytes)? != self.descriptor
        {
            return Err(corrupt(
                "visibility section differs from its captured descriptor",
            ));
        }
        let delta = std::sync::Arc::new(CapsuleVisibilityDelta::decode(&bytes)?);
        Ok(self.complete.get_or_init(|| delta).clone())
    }

    /// Return requested changes in input order, or an authenticated absent-ref result.
    ///
    /// OID misses remain distinct from missing refs. Requested output, cached and
    /// fetched pages share one work budget.
    pub async fn lookup(
        &self,
        store: &Store,
        ref_name: &str,
        oids: &[[u8; 20]],
    ) -> Result<Option<Vec<Option<VisibilityChange>>>> {
        if !ref_name.starts_with("refs/") || !valid_ref_name(ref_name) {
            return Err(contract_error("visibility lookup ref name is invalid"));
        }
        let mut work = PageWork::new(self.maximum, 0);
        let key = blake3::hash(ref_name.as_bytes()).as_bytes().to_vec();
        let mut rows = self
            .rows(
                store,
                &self.descriptor.refs,
                Kind::VisibilityRefs,
                std::slice::from_ref(&key),
                &mut work,
            )
            .await?;
        let Some(value) = rows.remove(0) else {
            return Ok(None);
        };
        let row = VisibilityEditHeader::decode(&key, &value, &self.descriptor)?;
        if row.name != ref_name {
            return Err(corrupt("visibility lookup ref hash collides"));
        }
        self.prime_roots(&row)?;
        // Page-key deduplication cannot account for duplicate output rows.
        // Admit logical query work before either encoding allocates its results.
        let bytes = (oids.len() as u64)
            .checked_mul(64)
            .ok_or_else(|| corrupt("visibility query materialization overflows"))?;
        work.admit_materialized(bytes)?;
        if let (Some(descriptor), Some(sets)) = (&self.descriptor.shared, &row.shared) {
            let reader = shared::Reader::new(
                store,
                &self.path,
                &self.cache,
                self.offset,
                self.descriptor.body_bytes,
                descriptor,
            )?;
            let mut requested = oids.to_vec();
            if row.replaces {
                requested.push(decode_oid(&row.new_oid)?);
            }
            let added = reader
                .lookup(sets.added.as_ref(), &requested, &mut work)
                .await?;
            let removed = reader
                .lookup(sets.removed.as_ref(), &requested, &mut work)
                .await?;
            let changes = added
                .into_iter()
                .zip(removed)
                .map(|(added, removed)| match (added, removed) {
                    (true, true) => Err(corrupt("visibility object has conflicting sets")),
                    (true, false) => Ok(Some(VisibilityChange::Added)),
                    (false, true) => Ok(Some(VisibilityChange::Removed)),
                    (false, false) => Ok(None),
                })
                .collect::<Result<Vec<_>>>()?;
            if row.replaces && changes.last() != Some(&Some(VisibilityChange::Added)) {
                return Err(corrupt("replacement visibility does not admit its new tip"));
            }
            return Ok(Some(changes.into_iter().take(oids.len()).collect()));
        }
        let mut keys = oids.iter().map(|oid| oid.to_vec()).collect::<Vec<_>>();
        if row.replaces {
            keys.push(decode_oid(&row.new_oid)?.to_vec());
        }
        // Separate authenticated sets give each direction an exact count. A loaded
        // OID present in both is malformed evidence, never an implicit precedence.
        let added = match &row.added {
            Some(root) => {
                self.rows(store, root, Kind::VisibilityChanges, &keys, &mut work)
                    .await?
            }
            None => vec![None; keys.len()],
        };
        let removed = match &row.removed {
            Some(root) => {
                self.rows(store, root, Kind::VisibilityChanges, &keys, &mut work)
                    .await?
            }
            None => vec![None; keys.len()],
        };
        let changes = added
            .into_iter()
            .zip(removed)
            .map(|(added, removed)| match (added, removed) {
                (None, None) => Ok(None),
                (Some(value), None) if value.is_empty() => Ok(Some(VisibilityChange::Added)),
                (None, Some(value)) if value.is_empty() => Ok(Some(VisibilityChange::Removed)),
                _ => Err(corrupt(
                    "visibility object has conflicting sets or unexpected payload",
                )),
            })
            .collect::<Result<Vec<_>>>()?;
        if row.replaces && changes.last() != Some(&Some(VisibilityChange::Added)) {
            return Err(corrupt("replacement visibility does not admit its new tip"));
        }
        Ok(Some(changes.into_iter().take(oids.len()).collect()))
    }

    fn prime_roots(&self, row: &VisibilityEditHeader) -> Result<()> {
        // Authenticated header bytes seed the existing page cache. Lookups still
        // traverse and admit every root and child through the canonical reader.
        let end = self
            .offset
            .checked_add(self.descriptor.body_bytes)
            .ok_or_else(|| corrupt("visibility body span overflows"))?;
        for (root, bytes) in [
            (&row.added, &row.inline_added_root),
            (&row.removed, &row.inline_removed_root),
        ] {
            if let (Some(root), Some(bytes)) = (root, bytes) {
                self.cache.prime(
                    &self.path,
                    0,
                    self.offset..end,
                    root,
                    Kind::VisibilityChanges,
                    bytes,
                )?;
            }
        }
        Ok(())
    }

    async fn rows(
        &self,
        store: &Store,
        root: &PageRef,
        kind: Kind,
        keys: &[Vec<u8>],
        work: &mut PageWork,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let end = self
            .offset
            .checked_add(self.descriptor.body_bytes)
            .ok_or_else(|| corrupt("visibility body span overflows"))?;
        self.cache
            .lookup(
                store,
                &self.path,
                0,
                self.offset..end,
                root,
                kind,
                keys,
                work,
            )
            .await
    }
}

impl PartialEq for CapsuleVisibilityReader {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.offset == other.offset
            && self.length == other.length
            && self.hash == other.hash
            && self.descriptor == other.descriptor
    }
}
impl Eq for CapsuleVisibilityReader {}

#[cfg(test)]
mod point_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capsule_protocol::{
        Capsule, CapsuleRefEdit, CapsuleRun, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
    };
    use std::sync::Arc;

    #[tokio::test]
    async fn mixed_layout_pool_keeps_selected_membership_proofs_separate() {
        use crate::git_visibility::{GitVisibilityDictionary, GitVisibilityObjects};

        let mut leaves = Vec::new();
        let mut expected = Vec::new();
        for (index, shared) in [false, true].into_iter().enumerate() {
            let base = ((index + 1) as u128) << 64;
            let name = format!("refs/heads/layout-{index}");
            let tip = format!("{:040x}", base + 500);
            let objects = (1..=1000)
                .map(|index| format!("{:040x}", base + index))
                .collect::<Vec<_>>();
            let mut edit =
                GitVisibilityEdit::from_replacement_objects(None, tip.clone(), objects.clone());
            if shared {
                edit.added = GitVisibilityObjects::from_shared_ordinals(
                    GitVisibilityDictionary::new(objects).unwrap(),
                    (0..1000).collect(),
                )
                .unwrap();
            }
            let delta =
                CapsuleVisibilityDelta::new(BTreeMap::from([(name.clone(), edit)])).unwrap();
            let transaction = CapsuleTransaction::new(
                &"1".repeat(64),
                vec![CapsuleRefEdit::new(&name, None, Some(tip.clone()), None)],
            )
            .unwrap();
            let capsule = Capsule::build(
                &transaction,
                vec![],
                vec![CapsuleSection::new(
                    CapsuleSectionKind::VisibilityDelta,
                    encode(&delta).unwrap(),
                )],
            )
            .unwrap();
            leaves.push(CapsuleRun::leaf(capsule).unwrap());
            expected.push((name, decode_oid(&tip).unwrap()));
        }
        let run = CapsuleRun::compact(leaves).unwrap();
        let run = CapsuleRun::decode(run.bytes().clone()).unwrap();
        let layout = StoreLayout::new(
            Store::new(Arc::new(object_store::memory::InMemory::new())),
            "repositories/mixed-layout-proofs".into(),
        );
        layout
            .store()
            .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let (control, _) = run.verified_controls().unwrap();
        let controls = control
            .materialize_capsules_with_paged_visibility(&layout, None)
            .unwrap();
        for (index, control) in controls.iter().enumerate() {
            let (name, tip) = &expected[index];
            let other = expected[(index + 1) % expected.len()].1;
            assert_eq!(
                control
                    .paged_visibility()
                    .unwrap()
                    .lookup(layout.store(), name, &[*tip, other])
                    .await
                    .unwrap(),
                Some(vec![Some(VisibilityChange::Added), None])
            );
        }
    }

    #[tokio::test]
    async fn cold_tip_membership_keeps_large_closures_on_small_proof_pages() {
        use crate::git_visibility::{GitVisibilityDictionary, GitVisibilityObjects};
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

        for shared in [false, true] {
            let name = "refs/heads/main";
            let oid = format!("{:040x}", 5000);
            let objects = (1..=10_000)
                .map(|index| format!("{index:040x}"))
                .collect::<Vec<_>>();
            let mut edit =
                GitVisibilityEdit::from_replacement_objects(None, oid.clone(), objects.clone());
            if shared {
                let dictionary = GitVisibilityDictionary::new(objects).unwrap();
                edit.added =
                    GitVisibilityObjects::from_shared_ordinals(dictionary, (0..10_000).collect())
                        .unwrap();
            }
            let delta =
                CapsuleVisibilityDelta::new(BTreeMap::from([(name.to_owned(), edit)])).unwrap();
            let transaction = CapsuleTransaction::new(
                &"1".repeat(64),
                vec![CapsuleRefEdit::new(name, None, Some(oid), None)],
            )
            .unwrap();
            {
                let encoded = encode(&delta).unwrap();
                let capsule = Capsule::build(
                    &transaction,
                    vec![],
                    vec![CapsuleSection::new(
                        CapsuleSectionKind::VisibilityDelta,
                        encoded,
                    )],
                )
                .unwrap();
                let run = CapsuleRun::leaf(capsule.clone()).unwrap();
                let bytes = Arc::new(AtomicU64::new(0));
                let reads = Arc::new(AtomicU64::new(0));
                let observed_bytes = bytes.clone();
                let observed_reads = reads.clone();
                let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
                    .with_read_byte_observer(Arc::new(move |size| {
                        observed_bytes.fetch_add(size, Relaxed);
                    }))
                    .with_read_request_observer(Arc::new(move |_| {
                        observed_reads.fetch_add(1, Relaxed);
                    }));
                let layout = StoreLayout::new(store, "repositories/cold-tip-proof".into());
                layout
                    .store()
                    .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
                    .await
                    .unwrap();
                let (control, _) = run.verified_controls().unwrap();
                let reader = control
                    .visibility_reader(&layout, capsule.hash(), 1024 * 1024)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    reader
                        .lookup(
                            layout.store(),
                            name,
                            &[decode_oid(&format!("{:040x}", 5000)).unwrap()]
                        )
                        .await
                        .unwrap(),
                    Some(vec![Some(VisibilityChange::Added)])
                );
                assert!(
                    bytes.load(Relaxed) <= 24 * 1024,
                    "shared={shared}, proof bytes={}, requests={}",
                    bytes.load(Relaxed),
                    reads.load(Relaxed)
                );
            }
        }
    }

    #[tokio::test]
    async fn larger_leaf_edits_keep_selected_membership_reads() {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

        let name = "refs/heads/main";
        let oid = format!("{:040x}", 1_000);
        let transaction = CapsuleTransaction::new(
            &"1".repeat(64),
            vec![CapsuleRefEdit::new(name, None, Some(oid.clone()), None)],
        )
        .unwrap();
        let delta = CapsuleVisibilityDelta::new(BTreeMap::from([(
            name.to_owned(),
            GitVisibilityEdit::from_replacement_objects(
                None,
                oid.clone(),
                (1..=1_000).map(|value| format!("{value:040x}")).collect(),
            ),
        )]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            vec![],
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                delta.encode().unwrap(),
            )],
        )
        .unwrap();
        let run = CapsuleRun::leaf(capsule.clone()).unwrap();
        let received = Arc::new(AtomicU64::new(0));
        let observed = received.clone();
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
            .with_read_byte_observer(Arc::new(move |bytes| {
                observed.fetch_add(bytes, Relaxed);
            }));
        let layout = StoreLayout::new(store, "repositories/selected-leaf".into());
        layout
            .store()
            .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let (control, _) = run.verified_controls().unwrap();
        let mut reader = control
            .visibility_reader(&layout, capsule.hash(), crate::pages::READ_AHEAD_BYTES)
            .unwrap()
            .unwrap();
        let body_bytes = reader.descriptor.body_bytes;
        reader.coalesce_small_edit(&AtomicU64::new(crate::pages::READ_AHEAD_BYTES));
        assert_eq!(
            reader
                .lookup(layout.store(), name, &[decode_oid(&oid).unwrap()])
                .await
                .unwrap(),
            Some(vec![Some(VisibilityChange::Added)]),
        );
        let bytes = received.load(Relaxed);
        assert!(
            bytes < body_bytes,
            "{bytes} bytes fetched for a selected tip"
        );
    }

    #[tokio::test]
    async fn leaf_read_ahead_shares_headroom_without_overdrawing_it() {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

        let name = "refs/heads/main";
        let oid = "2".repeat(40);
        let transaction = CapsuleTransaction::new(
            &"1".repeat(64),
            vec![CapsuleRefEdit::new(name, None, Some(oid.clone()), None)],
        )
        .unwrap();
        let delta = CapsuleVisibilityDelta::new(BTreeMap::from([(
            name.to_owned(),
            GitVisibilityEdit::from_replacement_objects(None, oid.clone(), vec![oid]),
        )]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            vec![],
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                delta.encode().unwrap(),
            )],
        )
        .unwrap();
        let run = CapsuleRun::leaf(capsule.clone()).unwrap();
        let layout = StoreLayout::new(
            Store::new(Arc::new(object_store::memory::InMemory::new())),
            "repositories/bounded-leaf".into(),
        );
        layout
            .store()
            .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let (control, _) = run.verified_controls().unwrap();
        let reader = || {
            control
                .visibility_reader(&layout, capsule.hash(), crate::pages::READ_AHEAD_BYTES)
                .unwrap()
                .unwrap()
        };
        let body_bytes = reader().descriptor.body_bytes;
        for available in [0, body_bytes - 1, body_bytes, 2 * body_bytes - 1] {
            let headroom = AtomicU64::new(available);
            for _ in 0..2 {
                let mut reader = reader();
                reader.coalesce_small_edit(&headroom);
                assert_eq!(
                    reader
                        .objects(layout.store(), name, VisibilityChange::Added)
                        .await
                        .unwrap(),
                    vec![[0x22; 20]]
                );
            }
            assert_eq!(headroom.load(Relaxed), available % body_bytes);
        }
    }

    #[tokio::test]
    async fn visibility_lookup_distinguishes_removed_unchanged_and_absent_refs() {
        let delta = CapsuleVisibilityDelta::new(BTreeMap::from([
            (
                "refs/heads/main".into(),
                GitVisibilityEdit::from_delta_objects(
                    Some("1".repeat(40)),
                    "2".repeat(40),
                    vec!["2".repeat(40)],
                    vec!["3".repeat(40)],
                ),
            ),
            (
                "refs/heads/alias".into(),
                GitVisibilityEdit::from_delta_objects(
                    Some("2".repeat(40)),
                    "2".repeat(40),
                    vec![],
                    vec![],
                ),
            ),
        ]))
        .unwrap();
        let transaction = CapsuleTransaction::new(
            &"1".repeat(64),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                Some("1".repeat(40)),
                Some("2".repeat(40)),
                None,
            )],
        )
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            vec![],
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                delta.encode().unwrap(),
            )],
        )
        .unwrap();
        let run = CapsuleRun::leaf(capsule.clone()).unwrap();
        let layout = StoreLayout::new(
            Store::new(Arc::new(object_store::memory::InMemory::new())),
            "repositories/visibility".into(),
        );
        layout
            .store()
            .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let (control, _) = run.verified_controls().unwrap();
        let reader = control
            .visibility_reader(&layout, capsule.hash(), 128 * 1024)
            .unwrap()
            .unwrap();
        let oids = [[0x33; 20], [0x22; 20], [0x44; 20], [0x33; 20]];
        assert_eq!(
            (
                reader
                    .lookup(layout.store(), "refs/heads/main", &oids)
                    .await
                    .unwrap(),
                reader
                    .lookup(layout.store(), "refs/heads/alias", &oids)
                    .await
                    .unwrap(),
                reader
                    .lookup(layout.store(), "refs/heads/missing", &oids)
                    .await
                    .unwrap()
            ),
            (
                Some(vec![
                    Some(VisibilityChange::Removed),
                    Some(VisibilityChange::Added),
                    None,
                    Some(VisibilityChange::Removed)
                ]),
                Some(vec![None; 4]),
                None
            ),
        );
    }

    #[tokio::test]
    async fn shared_visibility_preserves_directions_absent_refs_empty_edits_and_header_counts() {
        use crate::git_visibility::{
            GitVisibilityDictionary, GitVisibilityObjects, VISIBILITY_CHUNK_BYTES,
        };
        let dictionary =
            GitVisibilityDictionary::new((0..2051).map(|index| format!("{index:040x}")).collect())
                .unwrap();
        let make = |positions: Vec<usize>| {
            let mut chunks =
                vec![[0; VISIBILITY_CHUNK_BYTES]; 2051usize.div_ceil(VISIBILITY_CHUNK_BYTES * 8)];
            for index in positions {
                chunks[index / (VISIBILITY_CHUNK_BYTES * 8)]
                    [index % (VISIBILITY_CHUNK_BYTES * 8) / 8] |= 1 << (index % 8);
            }
            GitVisibilityObjects::from_shared_chunks(
                dictionary.clone(),
                chunks.into_iter().map(Arc::new).collect(),
            )
            .unwrap()
        };
        let main = GitVisibilityEdit {
            version: GIT_VISIBILITY_EDIT_VERSION,
            old_oid: Some(format!("{:040x}", 3)),
            new_oid: format!("{:040x}", 2),
            replaces: false,
            added: make((0..2051).filter(|index| index % 2 == 0).collect()),
            removed: make(vec![1, 3]),
        };
        let replacement = GitVisibilityEdit {
            version: GIT_VISIBILITY_EDIT_VERSION,
            old_oid: None,
            new_oid: format!("{:040x}", 1),
            replaces: true,
            added: make((0..2051).filter(|index| index % 2 == 1).collect()),
            removed: GitVisibilityObjects::default(),
        };
        let empty = GitVisibilityEdit::from_delta_objects(
            Some(format!("{:040x}", 2)),
            format!("{:040x}", 2),
            Vec::new(),
            Vec::new(),
        );
        let delta = CapsuleVisibilityDelta::new(BTreeMap::from([
            ("refs/heads/main".into(), main),
            ("refs/heads/replace".into(), replacement),
            ("refs/heads/alias".into(), empty),
        ]))
        .unwrap();
        let transaction = CapsuleTransaction::new(
            &"1".repeat(64),
            vec![
                CapsuleRefEdit::new(
                    "refs/heads/main",
                    Some(format!("{:040x}", 3)),
                    Some(format!("{:040x}", 2)),
                    None,
                ),
                CapsuleRefEdit::new(
                    "refs/heads/replace",
                    None,
                    Some(format!("{:040x}", 1)),
                    None,
                ),
                CapsuleRefEdit::new("refs/heads/alias", None, Some(format!("{:040x}", 2)), None),
            ],
        )
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            vec![],
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                delta.encode().unwrap(),
            )],
        )
        .unwrap();
        let run = CapsuleRun::leaf(capsule.clone()).unwrap();
        let layout = StoreLayout::new(
            Store::new(Arc::new(object_store::memory::InMemory::new())),
            "repositories/shared-visibility".into(),
        );
        layout
            .store()
            .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let (control, _) = run.verified_controls().unwrap();
        let reader = control
            .visibility_reader(&layout, capsule.hash(), 2 * 1024 * 1024)
            .unwrap()
            .unwrap();
        let indexes = [3usize, 2, 2051, 3, 0];
        let oids = indexes.map(|index| decode_oid(&format!("{index:040x}")).unwrap());
        assert_eq!(
            reader
                .lookup(layout.store(), "refs/heads/main", &oids)
                .await
                .unwrap(),
            Some(vec![
                Some(VisibilityChange::Removed),
                Some(VisibilityChange::Added),
                None,
                Some(VisibilityChange::Removed),
                Some(VisibilityChange::Added)
            ])
        );
        assert_eq!(
            reader
                .lookup(layout.store(), "refs/heads/alias", &oids)
                .await
                .unwrap(),
            Some(vec![None; oids.len()])
        );
        assert_eq!(
            reader
                .lookup(layout.store(), "refs/heads/missing", &oids)
                .await
                .unwrap(),
            None
        );
        assert!(
            reader
                .lookup(layout.store(), "refs/heads/main", &vec![[0; 20]; 140_000])
                .await
                .is_err()
        );
        let headers = reader
            .headers(layout.store(), &["refs/heads/main", "refs/heads/replace"])
            .await
            .unwrap();
        assert_eq!(headers[0].as_ref().unwrap().removed_count(), 2);
        assert_eq!(
            reader
                .objects(layout.store(), "refs/heads/main", VisibilityChange::Removed)
                .await
                .unwrap(),
            vec![
                decode_oid(&format!("{:040x}", 1)).unwrap(),
                decode_oid(&format!("{:040x}", 3)).unwrap()
            ]
        );
        assert_eq!(*reader.materialize(layout.store()).await.unwrap(), delta);
    }
}
