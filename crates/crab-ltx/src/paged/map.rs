//! Copy-on-write page metadata; one small write must not clone every locator.

use super::Locator;
use std::{collections::BTreeMap, sync::Arc};

const BLOCK_PAGES: u32 = 256;

#[derive(Clone, Default)]
struct Block {
    pages: BTreeMap<u32, Locator>,
    checksum: u64,
}

#[derive(Clone, Default)]
pub(super) struct PageMap {
    blocks: BTreeMap<u32, Arc<Block>>,
    len: usize,
    checksum: u64,
}

impl PageMap {
    pub(super) fn get(&self, page: &u32) -> Option<&Locator> {
        self.blocks.get(&(page / BLOCK_PAGES))?.pages.get(page)
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&u32, &Locator)> {
        self.blocks.values().flat_map(|block| block.pages.iter())
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn checksum(&self) -> u64 {
        self.checksum | crate::CHECKSUM_FLAG
    }

    pub(super) fn insert(&mut self, page: u32, locator: Locator) {
        let block = Arc::make_mut(self.blocks.entry(page / BLOCK_PAGES).or_default());
        let mut delta = locator.checksum;
        match block.pages.insert(page, locator) {
            Some(previous) => delta ^= previous.checksum,
            None => self.len += 1,
        }
        block.checksum ^= delta;
        self.checksum ^= delta;
    }

    pub(super) fn truncate(&mut self, count: u32) {
        let last = count / BLOCK_PAGES;
        // Whole blocks carry aggregate checksums, so truncating a large tail
        // need not walk its individual locators or clone unchanged blocks.
        for block in self.blocks.split_off(&(last + 1)).into_values() {
            self.len -= block.pages.len();
            self.checksum ^= block.checksum;
        }
        if let Some(block) = self.blocks.get_mut(&last) {
            if block
                .pages
                .last_key_value()
                .is_none_or(|(page, _)| *page <= count)
            {
                return;
            }
            let block = Arc::make_mut(block);
            let removed = block.pages.split_off(&(count + 1));
            self.len -= removed.len();
            for locator in removed.into_values() {
                block.checksum ^= locator.checksum;
                self.checksum ^= locator.checksum;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SegmentInfo, replica::RemoteSegment};

    fn locator(checksum: u64) -> Locator {
        Locator {
            segment: Arc::new(RemoteSegment {
                epoch: "test".into(),
                level: 0,
                bundle: None,
                index_hash: [0; 32],
                index_size: 0,
                info: SegmentInfo {
                    min_txid: 1,
                    max_txid: 1,
                    page_size: 4096,
                    database_pages: 1024,
                    pre_checksum: 0,
                    post_checksum: crate::CHECKSUM_FLAG,
                    size_bytes: 128,
                    blake3: [0; 32],
                },
            }),
            offset: 100,
            size: 20,
            hash: [0; 32],
            checksum: checksum | crate::CHECKSUM_FLAG,
        }
    }

    #[test]
    fn small_updates_share_untouched_blocks_and_preserve_old_views() {
        let mut original = PageMap::default();
        for page in 1..=4096 {
            original.insert(page, locator(u64::from(page)));
        }
        let mut next = original.clone();
        next.truncate(4096);
        next.insert(300, locator(9999));
        let copied = original
            .blocks
            .iter()
            .filter(|(key, value)| !Arc::ptr_eq(value, &next.blocks[key]))
            .count();
        assert_eq!(copied, 1);
        assert_eq!(
            original.get(&300).unwrap().checksum,
            300 | crate::CHECKSUM_FLAG
        );
        assert_eq!(
            next.get(&300).unwrap().checksum,
            9999 | crate::CHECKSUM_FLAG
        );
    }

    #[test]
    fn rolling_checksum_matches_full_scan_after_updates_shrink_and_regrowth() {
        let mut map = PageMap::default();
        let mut oracle = BTreeMap::new();
        let mut seed = 5u64;
        for step in 0..4000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let page = (seed % 4096) as u32 + 1;
            if step % 11 == 0 {
                map.truncate(page);
                oracle.retain(|p, _| *p <= page);
            } else {
                let checksum = seed | crate::CHECKSUM_FLAG;
                map.insert(page, locator(checksum));
                oracle.insert(page, checksum);
            }
            assert_eq!(map.len(), oracle.len());
            assert_eq!(
                map.checksum(),
                oracle.values().fold(0, |sum, value| sum ^ value) | crate::CHECKSUM_FLAG
            );
        }
        map.truncate(0);
        assert_eq!(map.len(), 0);
    }
}
