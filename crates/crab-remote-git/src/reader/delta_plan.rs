use std::collections::VecDeque;

use super::*;

#[derive(Clone, Copy)]
pub(super) struct DeltaPlanStep {
    pub(super) oid: gix_hash::ObjectId,
    pub(super) locator: GitObjectLocator,
    pub(super) descriptor: EntryDescriptor,
}

#[derive(Clone, Copy)]
struct SearchNode {
    oid: gix_hash::ObjectId,
    locator: GitObjectLocator,
    parent: Option<PackedEntryKey>,
    depth: usize,
}

impl RemoteGitReader {
    pub(super) async fn delta_plan(
        &self,
        session: &GitObjectLocatorSession,
        requested: gix_hash::ObjectId,
        locator: GitObjectLocator,
        max_depth: usize,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Vec<DeltaPlanStep>> {
        let root = (requested, locator);
        let mut current = root;
        let mut preferred = Vec::new();
        let mut inspected = HashMap::new();
        loop {
            check_cancelled(cancellation)?;
            let (oid, locator) = current;
            let key = (locator.pack_id, locator.location.pack_offset);
            if inspected.contains_key(&key) {
                break;
            }
            let descriptor = self
                .inspect_packed_entry(oid, locator, budget, cancellation)
                .await?;
            let step = DeltaPlanStep {
                oid,
                locator,
                descriptor,
            };
            inspected.insert(key, step);
            preferred.push(step);
            let EntryDescriptor::Delta { header, .. } = descriptor else {
                return Ok(preferred);
            };
            if preferred.len() > max_depth {
                break;
            }
            current = self
                .preferred_delta_base(session, locator, header, budget, cancellation)
                .await?;
        }
        self.search_delta_aliases(session, root, max_depth, inspected, budget, cancellation)
            .await
    }

    async fn preferred_delta_base(
        &self,
        session: &GitObjectLocatorSession,
        locator: GitObjectLocator,
        header: Header,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<(gix_hash::ObjectId, GitObjectLocator)> {
        if let Some(base) = self
            .local_delta_base(
                locator.pack_id,
                locator.location.pack_offset,
                header,
                budget,
                cancellation,
            )
            .await?
        {
            return Ok(base);
        }
        let Header::RefDelta { base_id } = header else {
            return Err(Error::Corrupt {
                stage: CorruptionStage::Delta,
            });
        };
        Ok((
            base_id,
            self.locate(session, base_id, budget, cancellation).await?,
        ))
    }

    pub(super) async fn search_delta_aliases(
        &self,
        session: &GitObjectLocatorSession,
        root: (gix_hash::ObjectId, GitObjectLocator),
        max_depth: usize,
        mut inspected: HashMap<PackedEntryKey, DeltaPlanStep>,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Vec<DeltaPlanStep>> {
        let root_key = (root.1.pack_id, root.1.location.pack_offset);
        let mut nodes = HashMap::from([(
            root_key,
            SearchNode {
                oid: root.0,
                locator: root.1,
                parent: None,
                depth: 0,
            },
        )]);
        let mut pending = VecDeque::from([root_key]);
        let mut aliases = HashMap::new();
        let mut depth_exhausted = false;
        // REF edges can name several physical representations. A shortest
        // path to a full entry avoids cycles and keeps shared batch suffixes
        // within the same depth bound, regardless of the requested root.
        while let Some(key) = pending.pop_front() {
            check_cancelled(cancellation)?;
            let node = *nodes.get(&key).ok_or(Error::InternalInvariant {
                invariant: "delta search node disappeared",
            })?;
            let step = match inspected.get(&key).copied() {
                Some(step) => step,
                None => {
                    let descriptor = self
                        .inspect_packed_entry(node.oid, node.locator, budget, cancellation)
                        .await?;
                    let step = DeltaPlanStep {
                        oid: node.oid,
                        locator: node.locator,
                        descriptor,
                    };
                    inspected.insert(key, step);
                    step
                }
            };
            let EntryDescriptor::Delta { header, .. } = step.descriptor else {
                let mut path = Vec::new();
                let mut current = Some(key);
                while let Some(key) = current {
                    path.push(*inspected.get(&key).ok_or(Error::InternalInvariant {
                        invariant: "delta path entry disappeared",
                    })?);
                    current = nodes
                        .get(&key)
                        .ok_or(Error::InternalInvariant {
                            invariant: "delta path node disappeared",
                        })?
                        .parent;
                }
                path.reverse();
                return Ok(path);
            };
            if node.depth >= max_depth {
                depth_exhausted = true;
                continue;
            }
            let preferred = self
                .preferred_delta_base(session, node.locator, header, budget, cancellation)
                .await?;
            let mut candidates = vec![preferred];
            if let Header::RefDelta { base_id } = header {
                if let std::collections::hash_map::Entry::Vacant(slot) = aliases.entry(base_id) {
                    slot.insert(self.delta_aliases(base_id, budget, cancellation).await?);
                }
                candidates.extend(aliases.get(&base_id).into_iter().flatten().copied());
            }
            for (oid, locator) in candidates {
                let child = (locator.pack_id, locator.location.pack_offset);
                match nodes.entry(child) {
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(SearchNode {
                            oid,
                            locator,
                            parent: Some(key),
                            depth: node.depth + 1,
                        });
                        pending.push_back(child);
                    }
                    std::collections::hash_map::Entry::Occupied(slot) if slot.get().oid != oid => {
                        return Err(Error::Corrupt {
                            stage: CorruptionStage::Locator,
                        });
                    }
                    std::collections::hash_map::Entry::Occupied(_) => {}
                }
            }
        }
        Err(if depth_exhausted {
            Error::LimitExceeded {
                limit: "delta depth",
                actual: max_depth.saturating_add(1) as u64,
                maximum: max_depth as u64,
            }
        } else {
            Error::Corrupt {
                stage: CorruptionStage::Delta,
            }
        })
    }

    async fn delta_aliases(
        &self,
        oid: gix_hash::ObjectId,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Vec<(gix_hash::ObjectId, GitObjectLocator)>> {
        let mut packs = self.inventory.keys().copied().collect::<Vec<_>>();
        packs.sort_unstable();
        let mut output = Vec::new();
        for pack_id in packs {
            check_cancelled(cancellation)?;
            if let Some(index) = &self.object_index {
                let bytes: [u8; 20] = oid
                    .as_bytes()
                    .try_into()
                    .map_err(|_| Error::UnsupportedObjectFormat)?;
                let store = self
                    .store
                    .clone()
                    .with_read_admission(budget.read_admission(cancellation.clone()));
                let entries = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(Error::Cancelled),
                    result = index.entry_in_pack(&store, pack_id, &bytes, &self.inventory) => result?,
                };
                output.extend(entries.into_iter().map(|locator| (oid, locator)));
                continue;
            }
            let index = self.load_pack_index(pack_id, budget, cancellation).await?;
            if let Ok(position) = index.object_ids.binary_search(&oid) {
                output.push((
                    oid,
                    GitObjectLocator {
                        ordinal: 0,
                        pack_id,
                        location: index.location_at(position)?,
                        metadata: Default::default(),
                    },
                ));
            }
        }
        Ok(output)
    }
}
