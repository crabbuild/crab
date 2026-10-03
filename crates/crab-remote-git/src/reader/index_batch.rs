use super::*;
use crate::runtime::{PackIndexBatchFlightKey, PackIndexCacheKey, PackIndexes};

const MAX_INDEXES_PER_WINDOW: usize = 256;

// Inputs are sorted by object ID; requested positions retain caller order and
// duplicates. Index positions refer only to the already verified index.
pub(super) fn visit_index_matches<T: Ord>(
    requested: &[(T, usize)],
    indexed: &[T],
    cancellation: &CancellationToken,
    mut visit: impl FnMut(usize, usize) -> Result<()>,
) -> Result<()> {
    check_cancelled(cancellation)?;
    // Small frontier members must not each scan the full response OID set;
    // conversely, a point read must not scan a large stable pack's index.
    if requested.len() <= indexed.len() {
        for (oid, position) in requested {
            check_cancelled(cancellation)?;
            if let Ok(index_position) = indexed.binary_search(oid) {
                visit(*position, index_position)?;
            }
        }
    } else {
        for (index_position, oid) in indexed.iter().enumerate() {
            check_cancelled(cancellation)?;
            let start = requested.partition_point(|(requested, _)| requested < oid);
            for (_, position) in requested[start..]
                .iter()
                .take_while(|(requested, _)| requested == oid)
            {
                check_cancelled(cancellation)?;
                visit(*position, index_position)?;
            }
        }
    }
    Ok(())
}

pub(super) enum PackIndexRead {
    Individual(MerkleHash),
    Window(IndexWindow),
}

pub(super) struct IndexWindow {
    path: ObjectPath,
    range: std::ops::Range<u64>,
    extra_bytes: u64,
    members: Vec<(GitPackInventoryEntry, RemoteGitSidecarRange)>,
}

impl RemoteGitReader {
    pub(super) async fn plan_pack_index_reads(
        &self,
        inventory: &HashMap<MerkleHash, GitPackInventoryEntry>,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PackIndexRead>> {
        let mut reads = Vec::new();
        let mut by_source = std::collections::BTreeMap::<ObjectPath, Vec<_>>::new();
        let mut pack_ids = inventory.keys().copied().collect::<Vec<_>>();
        pack_ids.sort_unstable();
        for pack_id in pack_ids {
            check_cancelled(cancellation)?;
            let key = PackIndexCacheKey::new(&self.identity, pack_id);
            if self
                .runtime
                .cached_pack_index(&key, self.limits.max_pack_index_bytes)
                .await
                .is_some()
            {
                reads.push(PackIndexRead::Individual(pack_id));
                continue;
            }
            let Some(source) = self.pack_source(&pack_id) else {
                reads.push(PackIndexRead::Individual(pack_id));
                continue;
            };
            let (Some(path), Some(lazy)) = (&source.path, &source.lazy_index) else {
                reads.push(PackIndexRead::Individual(pack_id));
                continue;
            };
            check_limit(
                "pack index bytes",
                lazy.index.length,
                self.limits.max_pack_index_bytes,
            )?;
            by_source
                .entry(path.clone())
                .or_default()
                .push((inventory[&pack_id], lazy.index.clone()));
        }
        for (path, mut members) in by_source {
            members.sort_unstable_by_key(|(entry, index)| (index.offset, entry.pack_id));
            let mut current: Option<IndexWindow> = None;
            for member in members {
                let start = member.1.offset;
                let end = start.checked_add(member.1.length).ok_or(Error::Corrupt {
                    stage: CorruptionStage::PackIndex,
                })?;
                let can_extend = current.as_ref().is_some_and(|window| {
                    start
                        <= window
                            .range
                            .end
                            .saturating_add(MAX_COALESCED_SOURCE_GAP_BYTES)
                        && window.range.end.max(end).saturating_sub(window.range.start)
                            <= MAX_COALESCED_SOURCE_RANGE_BYTES
                        && window
                            .extra_bytes
                            .saturating_add(start.saturating_sub(window.range.end))
                            <= MAX_COALESCED_SOURCE_EXTRA_BYTES
                        && window.members.len() < MAX_INDEXES_PER_WINDOW
                });
                if let Some(window) = current.as_mut().filter(|_| can_extend) {
                    window.extra_bytes = window
                        .extra_bytes
                        .saturating_add(start.saturating_sub(window.range.end));
                    window.range.end = window.range.end.max(end);
                    window.members.push(member);
                } else {
                    if let Some(window) = current.take() {
                        reads.push(PackIndexRead::Window(window));
                    }
                    current = Some(IndexWindow {
                        path: path.clone(),
                        range: start..end,
                        extra_bytes: 0,
                        members: vec![member],
                    });
                }
            }
            if let Some(window) = current {
                reads.push(PackIndexRead::Window(window));
            }
        }
        Ok(reads)
    }

    pub(super) async fn load_pack_index_read(
        &self,
        read: PackIndexRead,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<PackIndexes> {
        let window = match read {
            PackIndexRead::Individual(pack_id) => {
                let index = self.load_pack_index(pack_id, budget, cancellation).await?;
                return Ok(vec![(pack_id, index)]);
            }
            PackIndexRead::Window(window) if window.members.len() == 1 => {
                let pack_id = window.members[0].0.pack_id;
                let index = self.load_pack_index(pack_id, budget, cancellation).await?;
                return Ok(vec![(pack_id, index)]);
            }
            PackIndexRead::Window(window) => window,
        };
        // The flight binds both the immutable location and all member proofs;
        // another snapshot cannot reuse a producer for different descriptors.
        let mut digest = blake3::Hasher::new();
        digest.update(&(window.path.as_ref().len() as u64).to_le_bytes());
        digest.update(window.path.as_ref().as_bytes());
        for (entry, range) in &window.members {
            digest.update(entry.pack_id.hex().as_bytes());
            digest.update(&entry.object_count.to_le_bytes());
            digest.update(&entry.pack_size.to_le_bytes());
            digest.update(&range.offset.to_le_bytes());
            digest.update(&range.length.to_le_bytes());
            digest.update(range.blake3.as_bytes());
        }
        let key = PackIndexBatchFlightKey::new(
            &self.identity,
            *digest.finalize().as_bytes(),
            self.limits.max_pack_index_bytes,
        );
        let runtime = self.runtime.clone();
        let store = self.store.clone();
        let identity = self.identity.clone();
        let maximum = self.limits.max_pack_index_bytes;
        self.runtime
            .load_pack_indexes_singleflight(
                key,
                cancellation,
                budget,
                move |cancellation, budget| async move {
                    check_cancelled(&cancellation)?;
                    let mut cached = Vec::with_capacity(window.members.len());
                    for (entry, _) in &window.members {
                        let key = PackIndexCacheKey::new(&identity, entry.pack_id);
                        if let Some(index) = runtime.cached_pack_index(&key, maximum).await {
                            cached.push((entry.pack_id, index));
                        }
                    }
                    if cached.len() == window.members.len() {
                        return Ok(cached);
                    }
                    drop(cached);
                    let bytes = read_index_window_from_store(
                        &store,
                        &runtime,
                        &window.path,
                        window.range.clone(),
                        budget,
                        &cancellation,
                    )
                    .await?;
                    let permit = runtime.decode_permit(&cancellation).await?;
                    let indexes = runtime
                        .spawn_blocking(move || {
                            window
                                .members
                                .into_iter()
                                .map(|(entry, range)| {
                                    check_cancelled(&cancellation)?;
                                    let index =
                                        copy_lazy_sidecar(&bytes, window.range.start, &range)?;
                                    let parsed = parse_pack_index(
                                        entry.pack_id,
                                        entry,
                                        index,
                                        None,
                                        &cancellation,
                                    )?;
                                    Ok((entry.pack_id, Arc::new(parsed)))
                                })
                                .collect::<Result<PackIndexes>>()
                        })
                        .await
                        .map_err(|source| Error::DecodeTask { source })?;
                    drop(permit);
                    indexes
                },
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use object_store::memory::InMemory;
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn index_matches_preserve_duplicates_misses_and_caller_positions(
            requested in prop::collection::vec(0_u16..256, 0..256),
            mut indexed in prop::collection::vec(0_u16..256, 0..256),
        ) {
            indexed.sort_unstable();
            indexed.dedup();
            let mut sorted = requested.iter().copied().zip(0..requested.len()).collect::<Vec<_>>();
            sorted.sort_unstable();
            let expected = requested.iter().enumerate().filter_map(|(position, oid)| {
                indexed.binary_search(oid).ok().map(|index| (position, index))
            }).collect::<Vec<_>>();
            let mut actual = Vec::new();
            visit_index_matches(&sorted, &indexed, &CancellationToken::new(), |position, index| {
                actual.push((position, index));
                Ok(())
            }).unwrap();
            actual.sort_unstable();
            prop_assert_eq!(actual, expected);
        }
    }

    #[test]
    fn index_matches_stop_on_cancellation_and_consumer_error() {
        let cancellation = CancellationToken::new();
        let mut visited = 0;
        let result = visit_index_matches(&[(1, 0), (2, 1)], &[1, 2], &cancellation, |_, _| {
            visited += 1;
            cancellation.cancel();
            Ok(())
        });
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(visited, 1);
        for (requested, indexed) in [(vec![(1, 0)], vec![1, 2]), (vec![(1, 0), (1, 1)], vec![1])] {
            let result =
                visit_index_matches(&requested, &indexed, &CancellationToken::new(), |_, _| {
                    Err(Error::Corrupt {
                        stage: CorruptionStage::PackIndex,
                    })
                });
            assert!(matches!(
                result,
                Err(Error::Corrupt {
                    stage: CorruptionStage::PackIndex
                })
            ));
        }
    }

    #[test]
    fn index_matching_work_scales_with_the_smaller_side() {
        struct CountedOid<'a>(usize, &'a Cell<usize>);
        impl PartialEq for CountedOid<'_> {
            fn eq(&self, other: &Self) -> bool {
                self.0 == other.0
            }
        }
        impl Eq for CountedOid<'_> {}
        impl PartialOrd for CountedOid<'_> {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }
        impl Ord for CountedOid<'_> {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                self.1.set(self.1.get() + 1);
                self.0.cmp(&other.0)
            }
        }
        let comparisons = Cell::new(0);
        let count = 8192;
        let requested = (0..count)
            .map(|oid| (CountedOid(oid, &comparisons), oid))
            .collect::<Vec<_>>();
        let mut matches = 0;
        for start in (0..count).step_by(32) {
            let indexed = (start..start + 32)
                .map(|oid| CountedOid(oid, &comparisons))
                .collect::<Vec<_>>();
            visit_index_matches(&requested, &indexed, &CancellationToken::new(), |_, _| {
                matches += 1;
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(matches, count);
        assert!(
            comparisons.get() < count * 32,
            "{} comparisons for {count} objects",
            comparisons.get()
        );

        comparisons.set(0);
        let indexed = (0..count)
            .map(|oid| CountedOid(oid, &comparisons))
            .collect::<Vec<_>>();
        for request in requested.chunks(1) {
            visit_index_matches(request, &indexed, &CancellationToken::new(), |_, _| Ok(()))
                .unwrap();
        }
        assert!(
            comparisons.get() < count * 32,
            "point reads must not scan the complete index"
        );
    }

    #[tokio::test]
    async fn index_windows_bound_gaps_span_overread_and_member_count() {
        for (name, count, length, gap, separate_sources, expected) in [
            ("gap limit", 2, 1, MAX_COALESCED_SOURCE_GAP_BYTES, false, 1),
            (
                "large gap",
                2,
                1,
                MAX_COALESCED_SOURCE_GAP_BYTES + 1,
                false,
                2,
            ),
            (
                "span limit",
                2,
                MAX_COALESCED_SOURCE_RANGE_BYTES / 2,
                0,
                false,
                1,
            ),
            (
                "large span",
                2,
                MAX_COALESCED_SOURCE_RANGE_BYTES / 2,
                1,
                false,
                2,
            ),
            (
                "large individual index",
                1,
                MAX_COALESCED_SOURCE_RANGE_BYTES + 1,
                0,
                false,
                1,
            ),
            (
                "overread limit",
                65,
                1,
                MAX_COALESCED_SOURCE_GAP_BYTES,
                false,
                1,
            ),
            (
                "excess overread",
                66,
                1,
                MAX_COALESCED_SOURCE_GAP_BYTES,
                false,
                2,
            ),
            ("member limit", MAX_INDEXES_PER_WINDOW, 1, 0, false, 1),
            ("excess members", MAX_INDEXES_PER_WINDOW + 1, 1, 0, false, 2),
            ("different sources", 2, 1, 0, true, 2),
        ] {
            let mut sources = HashMap::new();
            let mut inventory = Vec::new();
            for ordinal in 0..count {
                let pack_id = MerkleHash::from(*blake3::hash(&ordinal.to_le_bytes()).as_bytes());
                let offset = 32 + ordinal as u64 * (length + gap);
                let range = RemoteGitSidecarRange {
                    offset,
                    length,
                    blake3: blake3::hash(b"index").to_hex().to_string(),
                };
                let path = if separate_sources {
                    format!("source/{ordinal}")
                } else {
                    "source/one".to_owned()
                };
                sources.insert(
                    pack_id,
                    RemoteGitPackSource::embedded_lazy_index(
                        ObjectPath::from(path),
                        0,
                        32,
                        offset + length,
                        range.clone(),
                        range,
                    )
                    .unwrap(),
                );
                inventory.push(GitPackInventoryEntry {
                    pack_id,
                    object_count: 1,
                    pack_size: 32,
                });
            }
            let runtime = Arc::new(RemoteGitRuntime::default());
            let reader = RemoteGitReader::from_pinned_with_preferred_pack_indexes(
                Store::new(Arc::new(InMemory::new())),
                "repository",
                inventory,
                SnapshotLookupSources::default().with_pack_sources(sources),
                ReaderLimits::default(),
                runtime.clone(),
                RepositoryIdentity::new("provider", "repository", 1).unwrap(),
                1,
            )
            .unwrap();
            let reads = reader
                .plan_pack_index_reads(&reader.inventory, &CancellationToken::new())
                .await
                .unwrap();
            runtime.shutdown().await;
            assert_eq!(reads.len(), expected, "{name}");
            let retained = reads
                .iter()
                .map(|read| match read {
                    PackIndexRead::Individual(_) => 1,
                    PackIndexRead::Window(window) => window.members.len(),
                })
                .sum::<usize>();
            assert_eq!(retained, count, "{name}: no admitted index may be lost");
        }
    }
}
