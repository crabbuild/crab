//! Newest-wins merge of one replica view's page locators.

use std::{cmp::Reverse, collections::BinaryHeap};

use crate::Result;

use super::DirectoryEntry;

/// Merges the page locators of one view's sources, oldest source first.
///
/// A later truncation invalidates every older locator above its boundary, so
/// suffix minima of each source's `database_pages` drop those locators without a
/// page map, and at one page the newest surviving cut is authoritative. Both the
/// compaction rewrite and the initial directory build must resolve a page the
/// same way, so the decision lives here once.
pub(in crate::replica) struct LocatorMerge {
    heap: BinaryHeap<Reverse<(u32, usize)>>,
    valid_through: Vec<u32>,
    failed: bool,
}

impl LocatorMerge {
    /// Prepares the merge for one `database_pages` entry per source.
    pub(in crate::replica) fn new(database_pages: impl DoubleEndedIterator<Item = u32>) -> Self {
        let mut valid_through = Vec::new();
        let mut suffix_min = u32::MAX;
        for pages in database_pages.rev() {
            suffix_min = suffix_min.min(pages);
            valid_through.push(suffix_min);
        }
        valid_through.reverse();
        Self {
            heap: BinaryHeap::new(),
            valid_through,
            failed: false,
        }
    }

    /// Records that `index` is positioned on `page`.
    pub(in crate::replica) fn push(&mut self, index: usize, page: u32) {
        self.heap.push(Reverse((page, index)));
    }

    /// Resolves one page group, returning no locator when truncation discards it.
    ///
    /// `take` removes one source's current locator and reports the page that
    /// source moved to, if any. After an error the merge yields nothing further,
    /// so a caller cannot build a view from a partially read set of sources.
    pub(in crate::replica) fn next_group(
        &mut self,
        mut take: impl FnMut(usize) -> Result<(DirectoryEntry, Option<u32>)>,
    ) -> Option<Result<Option<DirectoryEntry>>> {
        if self.failed {
            return None;
        }
        let Reverse((page, first_index)) = self.heap.pop()?;
        let mut selected = None;
        let mut index = first_index;
        loop {
            let (entry, next_page) = match take(index) {
                Ok(taken) => taken,
                Err(error) => {
                    self.failed = true;
                    self.heap.clear();
                    return Some(Err(error));
                }
            };
            if self
                .valid_through
                .get(index)
                .is_some_and(|pages| page <= *pages)
                && selected
                    .as_ref()
                    .is_none_or(|(selected_index, _)| index > *selected_index)
            {
                selected = Some((index, entry));
            }
            if let Some(next_page) = next_page {
                self.heap.push(Reverse((next_page, index)));
            }
            let Some(Reverse((next_page, next_index))) = self.heap.peek().copied() else {
                break;
            };
            if next_page != page {
                break;
            }
            self.heap.pop();
            index = next_index;
        }
        Some(Ok(selected.map(|(_, entry)| entry)))
    }
}
