use super::*;
use crate::pages::{PageCache, PageWork};
use crab_storage::Store;

/// Range reads bound to one checkpoint's authenticated visibility directory.
#[derive(Debug)]
pub struct LayeredVisibilityReader {
    directory: Directory,
    path: object_store::path::Path,
    offset: u64,
    member_counts: Vec<usize>,
    maximum: u64,
    cache: PageCache,
}

impl LayeredVisibilityReader {
    pub(super) fn work(&self) -> PageWork {
        PageWork {
            maximum: self.maximum,
            bytes: 0,
            seen: BTreeSet::new(),
        }
    }

    pub(super) fn object_count(&self) -> u32 {
        self.directory.objects.as_ref().map_or(0, |root| root.count)
    }

    pub(super) async fn refs(&self, store: &Store, work: &mut PageWork) -> Result<Vec<RefRecord>> {
        let Some(root) = &self.directory.refs else {
            return Ok(Vec::new());
        };
        let rows = self
            .cache
            .scan(
                store,
                &self.path,
                0,
                self.offset,
                self.directory.body_bytes,
                root,
                Kind::VisibilityRefs,
                work,
            )
            .await?;
        rows.into_iter()
            .map(|(key, bytes)| {
                let entry: RefRecord = record(&bytes)?;
                validate_visibility_ref(&entry.name)?;
                if key.as_slice() != blake3::hash(entry.name.as_bytes()).as_bytes() {
                    return Err(corrupt("visibility ref key does not match its name"));
                }
                entry
                    .closure
                    .validate(self.object_count(), self.directory.body_bytes)?;
                for (root, present, maximum) in [
                    (&entry.transitions, entry.has_transitions, 64),
                    (
                        &entry.history,
                        entry.has_history,
                        MAX_LAYERED_VISIBILITY_TRANSITIONS,
                    ),
                ] {
                    if let Some(root) = root {
                        root.validate(Kind::VisibilityTransitions, self.directory.body_bytes)?;
                        if !present
                            || root.first != 0u32.to_be_bytes()
                            || root.count as usize > maximum
                        {
                            return Err(corrupt("visibility history root is invalid"));
                        }
                    }
                }
                Ok(entry)
            })
            .collect()
    }

    pub(super) async fn ordinals(
        &self,
        store: &Store,
        objects: &[[u8; 20]],
        work: &mut PageWork,
    ) -> Result<Vec<Option<u32>>> {
        let Some(root) = &self.directory.objects else {
            return Ok(vec![None; objects.len()]);
        };
        let keys = objects.iter().map(|oid| oid.to_vec()).collect::<Vec<_>>();
        let rows = self
            .cache
            .lookup_ranked(
                store,
                &self.path,
                0,
                self.offset,
                self.directory.body_bytes,
                root,
                Kind::VisibilityOids,
                &keys,
                work,
            )
            .await?;
        rows.into_iter()
            .map(|row| {
                let Some((rank, value)) = row else {
                    return Ok(None);
                };
                if rank >= self.object_count() {
                    return Err(corrupt("visibility dictionary rank is out of bounds"));
                }
                self.member(&value)?;
                Ok(Some(rank))
            })
            .collect()
    }

    pub(super) async fn contains(
        &self,
        store: &Store,
        closure: &Closure,
        ordinals: &[Option<u32>],
        work: &mut PageWork,
    ) -> Result<Vec<bool>> {
        closure.validate(self.object_count(), self.directory.body_bytes)?;
        let mut output = vec![false; ordinals.len()];
        let Some(root) = &closure.root else {
            return Ok(output);
        };
        let mut blocks = BTreeMap::<u32, Vec<(usize, u16)>>::new();
        for (index, ordinal) in ordinals.iter().enumerate() {
            let Some(ordinal) = ordinal else { continue };
            if *ordinal >= self.object_count() {
                return Err(corrupt("visibility query ordinal is out of bounds"));
            }
            blocks
                .entry(ordinal / BLOCK_BITS)
                .or_default()
                .push((index, (ordinal % BLOCK_BITS) as u16));
        }
        let keys = blocks
            .keys()
            .map(|key| key.to_be_bytes().to_vec())
            .collect::<Vec<_>>();
        let values = self
            .cache
            .lookup(
                store,
                &self.path,
                0,
                self.offset,
                self.directory.body_bytes,
                root,
                Kind::VisibilityClosures,
                &keys,
                work,
            )
            .await?;
        for ((block, requests), value) in blocks.into_iter().zip(values) {
            let Some(value) = value else { continue };
            let positions = block_positions(&value)?;
            let last = positions
                .last()
                .copied()
                .ok_or_else(|| corrupt("visibility block is empty"))?;
            if block
                .checked_mul(BLOCK_BITS)
                .and_then(|start| start.checked_add(u32::from(last)))
                .is_none_or(|last| last >= self.object_count())
                || positions.len() > closure.count as usize
            {
                return Err(corrupt("visibility block escapes declared coverage"));
            }
            for (index, position) in requests {
                output[index] = positions.binary_search(&position).is_ok();
            }
        }
        Ok(output)
    }

    fn member(&self, value: &[u8]) -> Result<Option<LayeredObjectMember>> {
        if !self.directory.member_admission {
            if !value.is_empty() {
                return Err(corrupt("unexpected visibility member admission"));
            }
            return Ok(None);
        }
        if value.len() != 4 {
            return Err(corrupt("visibility member admission width is invalid"));
        }
        let member = LayeredObjectMember::new(
            u16::from_be_bytes([value[0], value[1]]),
            u16::from_be_bytes([value[2], value[3]]),
        );
        if self
            .member_counts
            .get(usize::from(member.source_index))
            .is_none_or(|count| usize::from(member.member_index) >= *count)
        {
            return Err(corrupt(
                "visibility member admission does not name a checkpoint member",
            ));
        }
        Ok(Some(member))
    }
    pub(crate) fn new(
        directory: Directory,
        path: object_store::path::Path,
        offset: u64,
        sources: &[PackSourceDescriptor],
        maximum: u64,
    ) -> Self {
        Self {
            directory,
            path,
            offset,
            member_counts: sources
                .iter()
                .map(|source| source.members().len())
                .collect(),
            maximum: if maximum == 0 {
                MAX_LAYERED_VISIBILITY_BYTES as u64
            } else {
                maximum.min(MAX_LAYERED_VISIBILITY_BYTES as u64)
            },
            cache: PageCache::default(),
        }
    }

    /// Resolve physical admission for requested OIDs, preserving misses and duplicates.
    ///
    /// Admission selects a pack; it does not authorize an object against visible refs.
    /// The caller admits request/output counts and supplies an operation-budgeted store.
    pub async fn members(
        &self,
        store: &Store,
        objects: &[[u8; 20]],
    ) -> Result<Option<Vec<Option<LayeredObjectMember>>>> {
        if !self.directory.member_admission {
            return Ok(None);
        }
        let Some(root) = &self.directory.objects else {
            return Ok(Some(vec![None; objects.len()]));
        };
        let keys = objects.iter().map(|oid| oid.to_vec()).collect::<Vec<_>>();
        let mut work = self.work();
        let rows = self
            .cache
            .lookup(
                store,
                &self.path,
                0,
                self.offset,
                self.directory.body_bytes,
                root,
                Kind::VisibilityOids,
                &keys,
                &mut work,
            )
            .await?;
        rows.into_iter()
            .map(|value| {
                let Some(value) = value else { return Ok(None) };
                self.member(&value)
            })
            .collect::<Result<Vec<_>>>()
            .map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_storage::StoreLayout;
    use object_store::{ObjectStoreExt, memory::InMemory};
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    fn fixture(count: u32) -> LayeredCheckpoint {
        let mut snapshot = super::super::tests::fixture(count);
        let pack = CapsuleGitPack::new(
            Bytes::from_static(b"PACK test body"),
            Bytes::from_static(b"index"),
            Bytes::from_static(b"reverse"),
            Bytes::from_static(b"locator"),
            "a".repeat(40),
            u64::from(count),
        )
        .unwrap();
        let source = PackLayer::build(&pack)
            .unwrap()
            .source_descriptor()
            .unwrap();
        snapshot.catalog_digest = source_catalog_digest(std::slice::from_ref(&source)).unwrap();
        LayeredCheckpoint::build_with_ordinal_visibility(
            0,
            &"b".repeat(64),
            vec![source],
            PointerCatalog::new(),
            Some(snapshot),
        )
        .unwrap()
    }

    async fn stored(checkpoint: &LayeredCheckpoint) -> (Store, StoreLayout<Store>, Arc<AtomicU64>) {
        let bytes = Arc::new(AtomicU64::new(0));
        let observed = bytes.clone();
        let store =
            Store::new(Arc::new(InMemory::new())).with_read_byte_observer(Arc::new(move |read| {
                observed.fetch_add(read, Ordering::Relaxed);
            }));
        let layout = StoreLayout::new(store.clone(), "visibility-pages".into());
        let path = layout.capsule_checkpoint_path(checkpoint.hash());
        store.put(&path, checkpoint.bytes().clone()).await.unwrap();
        (store, layout, bytes)
    }

    #[tokio::test]
    async fn footer_and_point_reads_stay_bounded_as_the_dictionary_grows() {
        for count in [10_000, 100_000] {
            let checkpoint = fixture(count);
            let (store, layout, bytes) = stored(&checkpoint).await;
            let pointer = crate::capsule_protocol::CheckpointPointer::new_layered(
                checkpoint.hash(),
                checkpoint.object_size,
                checkpoint.control_offset(),
                checkpoint.control_size(),
                checkpoint.footer_hash(),
                checkpoint.covered_generation(),
                checkpoint.covered_root_digest(),
                checkpoint.pack_count().unwrap(),
                checkpoint.object_count().unwrap(),
            )
            .unwrap();
            let control =
                crate::capsule_protocol::load_layered_checkpoint_control(&layout, &pointer)
                    .await
                    .unwrap();
            let opening = bytes.swap(0, Ordering::Relaxed);
            assert!(opening < 16 * 1024 && opening < checkpoint.bytes().len() as u64);
            let reader = control
                .visibility_reader(
                    layout.capsule_checkpoint_path(checkpoint.hash()),
                    128 * 1024,
                )
                .unwrap()
                .unwrap();
            let dictionary = checkpoint.visibility_ordinal_snapshot().unwrap().unwrap();
            let requested = [
                dictionary.objects[0],
                dictionary.objects[count as usize - 1],
                dictionary.objects[0],
                [255; 20],
            ];
            let expected = Some(vec![
                Some(LayeredObjectMember::new(0, 0)),
                Some(LayeredObjectMember::new(0, 0)),
                Some(LayeredObjectMember::new(0, 0)),
                None,
            ]);
            assert_eq!(reader.members(&store, &requested).await.unwrap(), expected);
            let cold = bytes.swap(0, Ordering::Relaxed);
            assert!(cold <= 128 * 1024 && cold < checkpoint.bytes().len() as u64);
            assert_eq!(reader.members(&store, &requested).await.unwrap(), expected);
            assert_eq!(bytes.load(Ordering::Relaxed), 0);
        }
    }

    #[tokio::test]
    async fn dictionary_ranks_include_unread_sibling_counts() {
        let checkpoint = fixture(100_000);
        let (store, layout, _) = stored(&checkpoint).await;
        let reader = checkpoint
            .visibility_reader(
                layout.capsule_checkpoint_path(checkpoint.hash()),
                128 * 1024,
            )
            .unwrap()
            .unwrap();
        let snapshot = checkpoint.visibility_ordinal_snapshot().unwrap().unwrap();
        let objects = [
            snapshot.objects[99_999],
            snapshot.objects[0],
            snapshot.objects[50_000],
            snapshot.objects[99_999],
            [255; 20],
        ];
        let ranks = reader
            .ordinals(&store, &objects, &mut reader.work())
            .await
            .unwrap();
        assert_eq!(
            ranks,
            vec![Some(99_999), Some(0), Some(50_000), Some(99_999), None]
        );
    }

    #[tokio::test]
    async fn captured_catalog_ranges_preserve_full_catalog_and_reject_oversized_capture() {
        use crate::capsule_protocol::{CheckpointPointer, FileCatalogEntry, ShardCatalogEntry};
        let checkpoint = fixture(10_000);
        let mut catalog = PointerCatalog::new();
        catalog
            .insert_shard("b".repeat(64), ShardCatalogEntry::new(1, vec![]))
            .unwrap();
        catalog
            .insert_file("a".repeat(64), FileCatalogEntry::new(13, "b".repeat(64)))
            .unwrap();
        let checkpoint = LayeredCheckpoint::build_with_ordinal_visibility(
            0,
            &"b".repeat(64),
            checkpoint.sources().to_vec(),
            catalog.clone(),
            checkpoint.visibility_ordinal_snapshot().unwrap(),
        )
        .unwrap();
        let (store, layout, bytes) = stored(&checkpoint).await;
        let pointer = CheckpointPointer::new_layered(
            checkpoint.hash(),
            checkpoint.object_size,
            checkpoint.control_offset(),
            checkpoint.control_size(),
            checkpoint.footer_hash(),
            0,
            &"b".repeat(64),
            checkpoint.pack_count().unwrap(),
            checkpoint.object_count().unwrap(),
        )
        .unwrap();
        let captured = crate::capsule_protocol::load_layered_checkpoint_for_visibility(
            &layout,
            &pointer,
            128 * 1024,
        )
        .await
        .unwrap();
        assert_eq!(captured.pointer_catalog().unwrap(), catalog);
        assert_eq!(
            bytes.swap(0, Ordering::Relaxed),
            captured.captured_metadata_bytes()
        );
        assert!(captured.visibility_ordinal_snapshot().is_err());
        assert!(
            crate::capsule_protocol::load_layered_checkpoint_for_visibility(
                &layout,
                &pointer,
                checkpoint.control_size()
            )
            .await
            .is_err()
        );
        assert_eq!(bytes.load(Ordering::Relaxed), checkpoint.control_size());
        let detached = captured.into_pointer_catalog_bytes().unwrap().unwrap();
        assert_eq!(PointerCatalog::decode(&detached).unwrap(), catalog);
        let _ = store;
    }

    #[tokio::test]
    async fn dictionary_byte_limit_is_enforced_before_range_io() {
        let checkpoint = fixture(10_000);
        let (store, layout, bytes) = stored(&checkpoint).await;
        let reader = checkpoint
            .visibility_reader(layout.capsule_checkpoint_path(checkpoint.hash()), 1)
            .unwrap()
            .unwrap();
        assert!(reader.members(&store, &[[0; 20]]).await.is_err());
        assert_eq!(bytes.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn dictionary_corruption_and_missing_ranges_fail_explicitly() {
        let checkpoint = fixture(10_000);
        let (store, layout, _) = stored(&checkpoint).await;
        let path = layout.capsule_checkpoint_path(checkpoint.hash());
        let reader = checkpoint
            .visibility_reader(path.clone(), 128 * 1024)
            .unwrap()
            .unwrap();
        let root = reader.directory.objects.as_ref().unwrap();
        let offset = (reader.offset + root.offset) as usize;
        let mut corrupt = checkpoint.bytes().to_vec();
        corrupt[offset] ^= 1;
        store.put_overwrite(&path, corrupt.into()).await.unwrap();
        assert!(reader.members(&store, &[[0; 20]]).await.is_err());
        store.inner().delete(&path).await.unwrap();
        assert!(reader.members(&store, &[[0; 20]]).await.is_err());
    }

    #[test]
    fn footer_rejects_missing_or_conflicting_page_commitments() {
        let checkpoint = fixture(10);
        let mut footer = checkpoint.footer.clone();
        footer
            .visibility_pages
            .as_mut()
            .unwrap()
            .objects
            .as_mut()
            .unwrap()
            .count += 1;
        assert!(
            validate_checkpoint_footer(
                &footer,
                checkpoint.control_offset(),
                checkpoint.object_size
            )
            .is_err()
        );
        footer.visibility_pages = None;
        assert!(
            validate_checkpoint_footer(
                &footer,
                checkpoint.control_offset(),
                checkpoint.object_size
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn dictionary_admission_requires_an_actual_source_member() {
        let checkpoint = fixture(1);
        let mut snapshot = checkpoint.visibility_ordinal_snapshot().unwrap().unwrap();
        snapshot.member_admission = Some(vec![LayeredObjectMember::new(1, 0)]);
        let checkpoint = LayeredCheckpoint::build_with_ordinal_visibility(
            0,
            &"b".repeat(64),
            checkpoint.sources().to_vec(),
            PointerCatalog::new(),
            Some(snapshot),
        )
        .unwrap();
        let (store, layout, _) = stored(&checkpoint).await;
        let reader = checkpoint
            .visibility_reader(
                layout.capsule_checkpoint_path(checkpoint.hash()),
                128 * 1024,
            )
            .unwrap()
            .unwrap();
        assert!(reader.members(&store, &[[0; 20]]).await.is_err());
    }
}
