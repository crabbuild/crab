//! Each cache belongs to one captured immutable layer directory.
use super::*;
use crab_storage::Store;
use futures_util::{StreamExt, TryStreamExt};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

const CACHE_BYTES: usize = 8 * 1024 * 1024;
const CACHE_PAGES: usize = 256;
pub(crate) const READ_AHEAD_BYTES: u64 = 64 * 1024;

#[derive(Debug, Default)]
pub(crate) struct PageCache {
    state: Mutex<State>,
    read_ahead: Option<std::ops::Range<u64>>,
    block_reads: [futures_util::lock::Mutex<()>; 4],
}

#[derive(Debug, Clone, Default)]
struct State {
    pages: BTreeMap<(usize, u64), (Kind, PageRef, Arc<Page>)>,
    order: VecDeque<Entry>,
    bytes: usize,
    blocks: BTreeMap<u64, bytes::Bytes>,
}

#[derive(Debug, Clone)]
enum Entry {
    Page((usize, u64)),
    Block(u64),
}

impl State {
    fn evict_for(&mut self, weight: usize) {
        while self.bytes.saturating_add(weight) > CACHE_BYTES
            || self.pages.len() + self.blocks.len() >= CACHE_PAGES
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            let removed = match oldest {
                Entry::Page(key) => self
                    .pages
                    .remove(&key)
                    .map_or(0, |(_, reference, page)| page_weight(&reference, &page)),
                Entry::Block(offset) => self
                    .blocks
                    .remove(&offset)
                    .map_or(0, |body| block_weight(&body)),
            };
            self.bytes = self.bytes.saturating_sub(removed);
        }
    }
}

#[derive(Clone)]
pub(crate) struct PageWork {
    maximum: u64,
    // Concurrent sources share page identities and counters. Admission must be
    // atomic with deduplication; no guard survives a storage read or await.
    state: Arc<Mutex<WorkState>>,
}

#[derive(Default)]
struct WorkState {
    bytes: u64,
    seen: BTreeSet<(usize, u64)>,
    objects: BTreeMap<[u8; 32], u64>,
    materialized: u64,
}

impl WorkState {
    fn admit_bytes(&mut self, bytes: u64, maximum: u64) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|total| *total <= maximum)
            .ok_or_else(|| corruption("metadata query exceeds its byte limit"))?;
        Ok(())
    }
}

impl PageWork {
    pub(crate) fn new(maximum: u64, bytes: u64) -> Self {
        Self {
            maximum,
            state: Arc::new(Mutex::new(WorkState {
                bytes,
                ..WorkState::default()
            })),
        }
    }

    fn state(&self) -> Result<MutexGuard<'_, WorkState>> {
        self.state
            .lock()
            .map_err(|_| MetadataError::Internal("metadata query admission is poisoned".to_owned()))
    }

    pub(crate) fn bytes(&self) -> Result<u64> {
        Ok(self.state()?.bytes)
    }

    pub(crate) fn maximum(&self) -> u64 {
        self.maximum
    }

    pub(crate) fn admit_bytes(&mut self, bytes: u64) -> Result<()> {
        self.state()?.admit_bytes(bytes, self.maximum)
    }

    pub(crate) fn admit_materialized(&mut self, bytes: u64) -> Result<()> {
        let mut state = self.state()?;
        let total = state
            .materialized
            .checked_add(bytes)
            // Encoded admission also pays for vector/string headers, hex keys
            // and simultaneous row/typed values. Bound that expansion separately
            // so aliases cannot grant unlimited copies of one physical page.
            .filter(|total| *total <= self.maximum.saturating_mul(4))
            .ok_or_else(|| corruption("metadata materialization exceeds its byte limit"))?;
        state.materialized = total;
        Ok(())
    }

    pub(crate) fn admit_object(&mut self, hash: [u8; 32], bytes: u64) -> Result<()> {
        let mut state = self.state()?;
        if let Some(prior) = state.objects.get(&hash) {
            return if *prior == bytes {
                Ok(())
            } else {
                Err(corruption("object identity has conflicting byte lengths"))
            };
        }
        state.admit_bytes(bytes, self.maximum)?;
        state.objects.insert(hash, bytes);
        Ok(())
    }

    pub(crate) fn object_dependencies(&self) -> Result<Vec<([u8; 32], u64)>> {
        Ok(self
            .state()?
            .objects
            .iter()
            .map(|(hash, bytes)| (*hash, *bytes))
            .collect())
    }

    fn admit(&mut self, layer: usize, offset: u64, bytes: u64) -> Result<()> {
        let mut state = self.state()?;
        if !state.seen.contains(&(layer, offset)) {
            state.admit_bytes(bytes, self.maximum)?;
            state.seen.insert((layer, offset));
        }
        Ok(())
    }
}

impl PageCache {
    /// Fork warm pages and read-ahead boundaries for the same captured source.
    pub(crate) fn fork(&self) -> Result<Self> {
        let state = self.state()?.clone();
        Ok(Self {
            state: Mutex::new(state),
            read_ahead: self.read_ahead.clone(),
            ..Self::default()
        })
    }

    pub(crate) fn for_region(region: std::ops::Range<u64>) -> Self {
        Self {
            read_ahead: Some(region),
            ..Self::default()
        }
    }

    pub(crate) fn for_prefetched_region(offset: u64, body: bytes::Bytes) -> Result<Self> {
        if body.len() as u64 > READ_AHEAD_BYTES {
            return Err(corruption("prefetched metadata exceeds one proof block"));
        }
        let end = offset
            .checked_add(body.len() as u64)
            .ok_or_else(|| corruption("prefetched metadata range overflows"))?;
        let cache = Self::for_region(offset..end);
        cache.cache_block(offset, body)?;
        Ok(cache)
    }

    fn blocks(&self, range: std::ops::Range<u64>) -> Vec<std::ops::Range<u64>> {
        let Some(region) = self
            .read_ahead
            .as_ref()
            .filter(|region| region.start <= range.start && range.end <= region.end)
        else {
            return vec![range];
        };
        let start =
            region.start + (range.start - region.start) / READ_AHEAD_BYTES * READ_AHEAD_BYTES;
        (start..range.end)
            .step_by(READ_AHEAD_BYTES as usize)
            .map(|start| start..start.saturating_add(READ_AHEAD_BYTES).min(region.end))
            .collect()
    }

    fn admit(
        &self,
        work: &mut PageWork,
        layer: usize,
        offset: u64,
        reference: &PageRef,
    ) -> Result<()> {
        for block in self.blocks(offset..offset + u64::from(reference.bytes)) {
            work.admit(layer, block.start, block.end - block.start)?;
        }
        Ok(())
    }

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
                self.admit(work, layer, source.start + reference.offset, reference)?;
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
                self.admit(work, layer, source.start + reference.offset, reference)?;
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

    #[cfg(feature = "remote-index")]
    #[expect(
        clippy::too_many_arguments,
        reason = "range search binds routing, origin and caller-owned aggregate admission"
    )]
    pub(crate) async fn find_row(
        &self,
        store: &Store,
        path: &object_store::path::Path,
        source: std::ops::Range<u64>,
        root: &PageRef,
        kind: Kind,
        keys: std::ops::RangeInclusive<Vec<u8>>,
        matches: impl Fn(&[u8]) -> bool,
        work: &mut PageWork,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let source_bytes = source
            .end
            .checked_sub(source.start)
            .ok_or_else(|| corruption("invalid page source range"))?;
        let mut pending = vec![root.clone()];
        while let Some(reference) = pending.pop() {
            reference.validate(kind, source_bytes)?;
            if reference.last < *keys.start() || reference.first > *keys.end() {
                continue;
            }
            self.admit(work, 0, source.start + reference.offset, &reference)?;
            let page = self
                .fetch(store, path, 0, source.clone(), &reference, kind)
                .await?;
            match page.as_ref() {
                Page::Leaf(rows) => {
                    if let Some(row) = rows
                        .iter()
                        .find(|(key, value)| keys.contains(key) && matches(value))
                    {
                        return Ok(Some(row.clone()));
                    }
                }
                Page::Branch(children) => pending.extend(children.iter().rev().cloned()),
            }
            // Warm scans must still yield so caller cancellation can be observed.
            let mut yielded = false;
            std::future::poll_fn(|context| {
                if std::mem::replace(&mut yielded, true) {
                    return std::task::Poll::Ready(());
                }
                context.waker().wake_by_ref();
                std::task::Poll::Pending
            })
            .await;
        }
        Ok(None)
    }

    // Guards protect local cache work only, never origin I/O.
    fn state(&self) -> Result<MutexGuard<'_, State>> {
        self.state
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
        // Page-call admission remains with the owning query.
        let bytes = self
            .read_range(
                store,
                path,
                offset..offset + u64::from(reference.bytes),
                u64::MAX,
            )
            .await?;
        let page = Arc::new(decode(
            &bytes,
            reference,
            kind,
            source_bytes,
            path.as_ref(),
        )?);
        self.remember_page(key, reference, kind, page)
    }

    pub(crate) fn prime(
        &self,
        path: &object_store::path::Path,
        layer: usize,
        source: std::ops::Range<u64>,
        reference: &PageRef,
        kind: Kind,
        bytes: &[u8],
    ) -> Result<()> {
        let source_bytes = source
            .end
            .checked_sub(source.start)
            .ok_or_else(|| corruption("invalid page source range"))?;
        let offset = source
            .start
            .checked_add(reference.offset)
            .ok_or_else(|| corruption("page source offset overflows"))?;
        // Inline routing pages use the same hash, bounds and coverage decoder
        // as origin reads. Query admission still charges every referenced page.
        let page = Arc::new(decode(bytes, reference, kind, source_bytes, path.as_ref())?);
        self.remember_page((layer, offset), reference, kind, page)?;
        Ok(())
    }

    fn remember_page(
        &self,
        key: (usize, u64),
        reference: &PageRef,
        kind: Kind,
        page: Arc<Page>,
    ) -> Result<Arc<Page>> {
        let weight = page_weight(reference, &page);
        if weight <= CACHE_BYTES {
            let mut cache = self.state()?;
            if let Some((previous_kind, previous, existing)) = cache.pages.get(&key) {
                if *previous_kind != kind || previous != reference {
                    return Err(corruption("cached page routing metadata changed"));
                }
                return Ok(existing.clone());
            }
            cache.evict_for(weight);
            cache.bytes += weight;
            cache.order.push_back(Entry::Page(key));
            cache
                .pages
                .insert(key, (kind, reference.clone(), page.clone()));
        }
        Ok(page)
    }

    pub(crate) async fn read_range(
        &self,
        store: &Store,
        path: &object_store::path::Path,
        range: std::ops::Range<u64>,
        maximum: u64,
    ) -> Result<bytes::Bytes> {
        let length = range
            .end
            .checked_sub(range.start)
            .filter(|length| *length <= maximum)
            .ok_or_else(|| corruption("metadata range exceeds its byte limit"))?;
        let blocks = self.blocks(range.clone());
        let expanded = blocks
            .first()
            .zip(blocks.last())
            .map_or(0, |(first, last)| last.end - first.start);
        // Tight bulk budgets retain exact reads; read-ahead never expands the
        // caller's admission. Warm blocks use the same planning rule as cold ones.
        if !self
            .read_ahead
            .as_ref()
            .is_some_and(|region| region.start <= range.start && range.end <= region.end)
            || expanded > maximum
        {
            return Ok(store.range_get(path, range).await?);
        }
        // Cold bulk scans use one transfer without evicting point proofs.
        // Pin complete warm spans so concurrent eviction cannot manufacture I/O.
        let warm = if length > READ_AHEAD_BYTES {
            let cache = self.state()?;
            blocks
                .iter()
                .map(|block| cache.blocks.get(&block.start).cloned())
                .collect::<Option<Vec<_>>>()
        } else {
            None
        };
        if length > READ_AHEAD_BYTES && warm.is_none() {
            return Ok(store.range_get(path, range).await?);
        }
        let capacity = usize::try_from(length)
            .map_err(|_| corruption("metadata range length cannot be represented"))?;
        let mut bytes = Vec::with_capacity(capacity);
        for (index, block) in blocks.into_iter().enumerate() {
            let body = match warm.as_ref().and_then(|bodies| bodies.get(index)) {
                Some(body) => body.clone(),
                None => self.read_block(store, path, block.clone()).await?,
            };
            let start = range.start.max(block.start) - block.start;
            let end = range.end.min(block.end) - block.start;
            bytes.extend_from_slice(
                body.get(start as usize..end as usize).ok_or_else(|| {
                    corruption("metadata read-ahead block does not cover its range")
                })?,
            );
        }
        Ok(bytes.into())
    }

    async fn read_block(
        &self,
        store: &Store,
        path: &object_store::path::Path,
        block: std::ops::Range<u64>,
    ) -> Result<bytes::Bytes> {
        if let Some(body) = self.state()?.blocks.get(&block.start).cloned() {
            return Ok(body);
        }
        // A page wave can request neighboring pages in the same proof block.
        // Coalesce those reads so admission does not undercount duplicate I/O;
        // warm hits stay independent of cold reads in the four fetch lanes.
        let lane = (block.start / READ_AHEAD_BYTES) as usize % self.block_reads.len();
        let _guard = self.block_reads[lane].lock().await;
        if let Some(body) = self.state()?.blocks.get(&block.start).cloned() {
            return Ok(body);
        }
        let body = store.range_get(path, block.clone()).await?;
        if body.len() as u64 != block.end - block.start {
            return Err(corruption("metadata read-ahead block is truncated"));
        }
        self.cache_block(block.start, body.clone())?;
        Ok(body)
    }

    fn cache_block(&self, offset: u64, body: bytes::Bytes) -> Result<()> {
        let mut cache = self.state()?;
        if cache.blocks.contains_key(&offset) {
            return Ok(());
        }
        if body.len() <= CACHE_BYTES {
            let weight = block_weight(&body);
            if weight > CACHE_BYTES {
                return Ok(());
            }
            cache.evict_for(weight);
            cache.bytes += weight;
            cache.order.push_back(Entry::Block(offset));
            cache.blocks.insert(offset, body);
        }
        Ok(())
    }
}

fn block_weight(body: &bytes::Bytes) -> usize {
    body.len()
        .saturating_add(std::mem::size_of::<bytes::Bytes>())
        .saturating_add(128)
}

fn page_weight(reference: &PageRef, page: &Page) -> usize {
    page.resident_bytes()
        .saturating_add(std::mem::size_of::<Page>())
        .saturating_add(std::mem::size_of::<PageRef>())
        .saturating_add(reference.first.capacity())
        .saturating_add(reference.last.capacity())
        .saturating_add(128)
}

#[cfg(test)]
mod tests;
