//! Each cache belongs to one captured immutable layer directory.
use super::*;
use crab_storage::Store;
use futures_util::{StreamExt, TryStreamExt};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

const CACHE_BYTES: usize = 8 * 1024 * 1024;
const CACHE_PAGES: usize = 256;

#[derive(Debug, Default)]
pub(crate) struct PageCache(Mutex<State>);

#[derive(Debug, Default)]
struct State {
    pages: BTreeMap<(usize, u64), (Kind, PageRef, Arc<Page>)>,
    order: VecDeque<(usize, u64)>,
    bytes: usize,
}

pub(crate) struct PageWork {
    pub(crate) maximum: u64,
    pub(crate) bytes: u64,
    pub(crate) seen: BTreeSet<(usize, u64)>,
}

impl PageWork {
    fn admit(&mut self, layer: usize, offset: u64, reference: &PageRef) -> Result<()> {
        if self.seen.insert((layer, offset)) {
            self.bytes = self
                .bytes
                .checked_add(u64::from(reference.bytes))
                .ok_or_else(|| corruption("query byte count overflows"))?;
            if self.bytes > self.maximum {
                return Err(corruption("metadata query exceeds its byte limit"));
            }
        }
        Ok(())
    }
}

impl PageCache {
    #[expect(
        clippy::too_many_arguments,
        reason = "page lookup binds routing, origin and caller-owned aggregate admission"
    )]
    pub(crate) async fn lookup(
        &self,
        store: &Store,
        path: &object_store::path::Path,
        layer: usize,
        source: std::ops::Range<u64>,
        root: &PageRef,
        kind: Kind,
        keys: &[Vec<u8>],
        work: &mut PageWork,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let source_bytes = source
            .end
            .checked_sub(source.start)
            .ok_or_else(|| corruption("invalid page source range"))?;
        if keys.iter().any(|key| key.len() != kind.key_bytes()) {
            return Err(corruption("invalid lookup key"));
        }
        let mut results = vec![None; keys.len()];
        if keys.is_empty() {
            return Ok(results);
        }
        let mut pending = vec![(root.clone(), (0..keys.len()).collect::<Vec<_>>())];
        while !pending.is_empty() {
            for (reference, _) in &pending {
                reference.validate(kind, source_bytes)?;
                work.admit(layer, source.start + reference.offset, reference)?;
            }
            // Charge the entire wave before starting I/O, including warm pages.
            let pages = futures_util::stream::iter(std::mem::take(&mut pending))
                .map(|(reference, positions)| {
                    let source = source.clone();
                    async move {
                        let page = self
                            .fetch(store, path, layer, source.clone(), &reference, kind)
                            .await?;
                        Ok::<_, MetadataError>((page, positions))
                    }
                })
                .buffer_unordered(4)
                .try_collect::<Vec<_>>()
                .await?;
            for (page, positions) in pages {
                match page.as_ref() {
                    Page::Leaf(rows) => {
                        for position in positions {
                            if let Ok(index) =
                                rows.binary_search_by(|row| row.0.cmp(&keys[position]))
                            {
                                results[position] = Some(rows[index].1.clone());
                            }
                        }
                    }
                    Page::Branch(children) => {
                        let mut batches = BTreeMap::<usize, Vec<usize>>::new();
                        for position in positions {
                            let index =
                                children.partition_point(|child| child.last < keys[position]);
                            if children
                                .get(index)
                                .is_some_and(|child| child.first <= keys[position])
                            {
                                batches.entry(index).or_default().push(position);
                            }
                        }
                        pending.extend(
                            batches
                                .into_iter()
                                .map(|(index, positions)| (children[index].clone(), positions)),
                        );
                    }
                }
            }
        }
        Ok(results)
    }

    pub(crate) async fn scan(
        &self,
        store: &Store,
        path: &object_store::path::Path,
        layer: usize,
        source: std::ops::Range<u64>,
        root: &PageRef,
        kind: Kind,
        work: &mut PageWork,
    ) -> Result<Rows> {
        let source_bytes = source
            .end
            .checked_sub(source.start)
            .ok_or_else(|| corruption("invalid page source range"))?;
        let mut pending = vec![root.clone()];
        let mut rows = Vec::new();
        while !pending.is_empty() {
            for reference in &pending {
                reference.validate(kind, source_bytes)?;
                work.admit(layer, source.start + reference.offset, reference)?;
            }
            let pages = futures_util::stream::iter(std::mem::take(&mut pending))
                .map(|reference| {
                    let source = source.clone();
                    async move {
                        self.fetch(store, path, layer, source, &reference, kind)
                            .await
                    }
                })
                .buffered(4)
                .try_collect::<Vec<_>>()
                .await?;
            for page in pages {
                match page.as_ref() {
                    Page::Leaf(values) => rows.extend(values.iter().cloned()),
                    Page::Branch(children) => pending.extend(children.iter().cloned()),
                }
            }
        }
        if rows.len() != root.count as usize || rows.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(corruption("scanned tree coverage is invalid"));
        }
        Ok(rows)
    }

    // Guards protect local cache work only, never origin I/O.
    fn state(&self) -> Result<MutexGuard<'_, State>> {
        self.0
            .lock()
            .map_err(|_| MetadataError::Internal("metadata page cache is poisoned".to_owned()))
    }
    pub(crate) async fn fetch(
        &self,
        store: &Store,
        path: &object_store::path::Path,
        layer: usize,
        source: std::ops::Range<u64>,
        reference: &PageRef,
        kind: Kind,
    ) -> Result<Arc<Page>> {
        let source_bytes = source
            .end
            .checked_sub(source.start)
            .ok_or_else(|| corruption("invalid page source range"))?;
        reference.validate(kind, source_bytes)?;
        let offset = source
            .start
            .checked_add(reference.offset)
            .ok_or_else(|| corruption("page source offset overflows"))?;
        let key = (layer, offset);
        if let Some((previous_kind, previous, page)) = self.state()?.pages.get(&key) {
            if *previous_kind != kind || previous != reference {
                return Err(corruption("cached page routing metadata changed"));
            }
            return Ok(page.clone());
        }
        let bytes = store
            .range_get(path, offset..offset + u64::from(reference.bytes))
            .await?;
        let page = Arc::new(decode(
            &bytes,
            reference,
            kind,
            source_bytes,
            path.as_ref(),
        )?);
        let weight = page_weight(reference, &page);
        if weight <= CACHE_BYTES {
            let mut cache = self.state()?;
            if let Some((previous_kind, previous, existing)) = cache.pages.get(&key) {
                if *previous_kind != kind || previous != reference {
                    return Err(corruption("concurrent page routing metadata changed"));
                }
                return Ok(existing.clone());
            }
            while cache.bytes.saturating_add(weight) > CACHE_BYTES
                || cache.pages.len() >= CACHE_PAGES
            {
                let Some(oldest) = cache.order.pop_front() else {
                    break;
                };
                if let Some((_, reference, old)) = cache.pages.remove(&oldest) {
                    cache.bytes = cache.bytes.saturating_sub(page_weight(&reference, &old));
                }
            }
            cache.bytes += weight;
            cache.order.push_back(key);
            cache
                .pages
                .insert(key, (kind, reference.clone(), page.clone()));
        }
        Ok(page)
    }
}

fn page_weight(reference: &PageRef, page: &Page) -> usize {
    page.resident_bytes()
        .saturating_add(std::mem::size_of::<Page>())
        .saturating_add(std::mem::size_of::<PageRef>())
        .saturating_add(reference.first.capacity())
        .saturating_add(reference.last.capacity())
        .saturating_add(128)
}
