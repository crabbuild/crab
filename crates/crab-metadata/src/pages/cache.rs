//! Each cache belongs to one captured immutable layer directory.
use super::*;
use crab_storage::Store;
use std::collections::{BTreeMap, VecDeque};
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

impl PageCache {
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
        source_bytes: u64,
        reference: &PageRef,
        kind: Kind,
    ) -> Result<Arc<Page>> {
        reference.validate(kind, source_bytes)?;
        let key = (layer, reference.offset);
        if let Some((previous_kind, previous, page)) = self.state()?.pages.get(&key) {
            if *previous_kind != kind || previous != reference {
                return Err(corruption("cached page routing metadata changed"));
            }
            return Ok(page.clone());
        }
        let bytes = store
            .range_get(
                path,
                reference.offset..reference.offset + u64::from(reference.bytes),
            )
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
