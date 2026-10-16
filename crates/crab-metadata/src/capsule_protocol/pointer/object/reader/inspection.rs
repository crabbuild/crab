//! One physical catalog scan followed by bounded local relation reduction.
use super::*;
use crate::pages::object::Node;
use std::sync::Arc;

mod join;
use join::{Join, Record};

type Seen = BTreeMap<[u8; 32], (Kind, Reference)>;

impl CatalogPages {
    pub(in crate::capsule_protocol) async fn inspected_objects(
        &self,
        store: &Store,
        maximum: u64,
    ) -> Result<Vec<(Path, u64)>> {
        let mut work = PageWork::new(maximum, 0);
        self.open(store, &mut work).await?;
        let space =
            crab_storage::scratch::ScratchSpace::temporary(maximum.saturating_mul(4)).await?;
        let mut join = Join::new(space);
        let checked = self.inspect(store, &mut work, &mut join).await;
        join.finish(checked).await?;
        Ok(work
            .object_dependencies()
            .map(|(hash, bytes)| (self.pages.path(hash), bytes))
            .collect())
    }

    async fn inspect(&self, store: &Store, work: &mut PageWork, join: &mut Join) -> Result<()> {
        let mut seen = Seen::new();
        for kind in [
            Kind::CatalogFiles,
            Kind::CatalogShards,
            Kind::CatalogXorbs,
            Kind::CatalogChunks,
        ] {
            let mut pending = self.root(kind)?.cloned().into_iter().collect();
            while let Some(node) = self
                .inspection_page(store, kind, &mut pending, work, &mut seen)
                .await?
            {
                let Node::Leaf(rows) = node.as_ref() else {
                    continue;
                };
                match kind {
                    Kind::CatalogFiles => {
                        for (_, value) in rows {
                            let file = file_from_bytes(value)?;
                            join.add(Record::reference(0, &hash(file.shard_hash())?, false)?)
                                .await?;
                        }
                    }
                    Kind::CatalogShards => {
                        for (identity, value) in rows {
                            let row: ShardRow = from_json(value)?;
                            if row.encoded_size == 0 {
                                return Err(corrupt("catalog shard is empty"));
                            }
                            join.add(Record::reference(0, identity, true)?).await?;
                            let mut pending = row.xorbs.into_iter().collect();
                            while let Some(node) = self
                                .inspection_page(
                                    store,
                                    Kind::CatalogShardXorbs,
                                    &mut pending,
                                    work,
                                    &mut seen,
                                )
                                .await?
                            {
                                if let Node::Leaf(rows) = node.as_ref() {
                                    for (xorb, value) in rows {
                                        if !value.is_empty() {
                                            return Err(corrupt(
                                                "catalog shard closure has unexpected row data",
                                            ));
                                        }
                                        join.add(Record::reference(1, xorb, false)?).await?;
                                    }
                                }
                            }
                        }
                    }
                    Kind::CatalogXorbs => {
                        for (identity, value) in rows {
                            let row: XorbRow = from_json(value)?;
                            if row.encoded_size == 0 || row.chunks.first != 0u32.to_be_bytes() {
                                return Err(corrupt(
                                    "catalog xorb is empty or its chunk sequence starts late",
                                ));
                            }
                            join.add(Record::reference(1, identity, true)?).await?;
                            let mut pending = vec![row.chunks];
                            while let Some(node) = self
                                .inspection_page(
                                    store,
                                    Kind::CatalogXorbChunks,
                                    &mut pending,
                                    work,
                                    &mut seen,
                                )
                                .await?
                            {
                                if let Node::Leaf(rows) = node.as_ref() {
                                    for (index, value) in rows {
                                        if value.len() != 36 || value[32..] == [0; 4] {
                                            return Err(corrupt(
                                                "catalog chunk length or size differs",
                                            ));
                                        }
                                        let mut placement = [0; 40];
                                        placement[..32].copy_from_slice(identity);
                                        placement[32..36].copy_from_slice(index);
                                        placement[36..].copy_from_slice(&value[32..]);
                                        join.add(Record::chunk(&value[..32], placement, false)?)
                                            .await?;
                                    }
                                }
                            }
                        }
                    }
                    Kind::CatalogChunks => {
                        for (hash, value) in rows {
                            let placement = value
                                .as_slice()
                                .try_into()
                                .map_err(|_| corrupt("catalog chunk placement length differs"))?;
                            join.add(Record::chunk(hash, placement, true)?).await?;
                        }
                    }
                    _ => return Err(corrupt("invalid inspection tree")),
                }
            }
        }
        Ok(())
    }

    async fn inspection_page(
        &self,
        store: &Store,
        kind: Kind,
        pending: &mut Vec<Reference>,
        work: &mut PageWork,
        seen: &mut Seen,
    ) -> Result<Option<Arc<Node>>> {
        while let Some(reference) = pending.last() {
            reference.validate(kind)?;
            if let Some((prior_kind, prior)) = seen.get(&reference.hash) {
                if *prior_kind != kind || prior != reference {
                    return Err(corrupt(
                        "shared inspection page has conflicting routing metadata",
                    ));
                }
                // Xorbs arrive in canonical order. A shared positional page was
                // already recorded for an earlier owner; later copies cannot win.
                // Shard membership is a set, so repeated closure pages add no work.
                pending.pop();
                continue;
            }
            let (reference, node) = self
                .pages
                .next_page(store, pending, kind, work)
                .await?
                .ok_or_else(|| corrupt("inspection lost its pending page"))?;
            seen.insert(reference.hash, (kind, reference));
            return Ok(Some(node));
        }
        Ok(None)
    }
}
