//! Exact adjacent-page transfers for one admitted lookup wave.
use super::*;
use futures_util::{StreamExt, TryStreamExt};

struct LookupGroup {
    range: std::ops::Range<u64>,
    pages: Vec<(PageRef, Vec<usize>)>,
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
                self.admit(work, layer, source.start + reference.offset, reference)?;
            }
            // Charge the entire wave before starting I/O, including warm pages.
            let mut pages = Vec::new();
            let mut cold = Vec::new();
            for (reference, positions) in std::mem::take(&mut pending) {
                let offset = source.start + reference.offset;
                match self.cached_page((layer, offset), &reference, kind)? {
                    Some(page) => pages.push((page, positions)),
                    None => cold.push((reference, positions)),
                }
            }
            cold.sort_unstable_by_key(|(reference, _)| reference.offset);
            let mut groups: Vec<LookupGroup> = Vec::new();
            for (reference, positions) in cold {
                let end = reference.offset + u64::from(reference.bytes);
                let extend = groups.last().is_some_and(|group| {
                    group.range.end == reference.offset
                        && end - group.range.start <= READ_AHEAD_BYTES
                });
                if let Some(group) = groups.last_mut().filter(|_| extend) {
                    group.range.end = end;
                    group.pages.push((reference, positions));
                } else {
                    groups.push(LookupGroup {
                        range: reference.offset..end,
                        pages: vec![(reference, positions)],
                    });
                }
            }
            if !groups.is_empty() {
                let fetched = futures_util::stream::iter(groups)
                    .map(|group| self.fetch_pages(store, path, layer, source.clone(), kind, group))
                    .buffer_unordered(4)
                    .try_collect::<Vec<_>>()
                    .await?;
                pages.extend(fetched.into_iter().flatten());
            }
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

    async fn fetch_pages(
        &self,
        store: &Store,
        path: &object_store::path::Path,
        layer: usize,
        source: std::ops::Range<u64>,
        kind: Kind,
        mut group: LookupGroup,
    ) -> Result<Vec<(Arc<Page>, Vec<usize>)>> {
        if group.pages.len() == 1
            && let Some((reference, positions)) = group.pages.pop()
        {
            let page = self
                .fetch(store, path, layer, source, &reference, kind)
                .await?;
            return Ok(vec![(page, positions)]);
        }
        // Only adjacent requested pages share a transfer. Every page retains
        // its own authenticated reference, admission and cache ownership.
        let start = source
            .start
            .checked_add(group.range.start)
            .ok_or_else(|| corruption("page group start overflows"))?;
        let end = source
            .start
            .checked_add(group.range.end)
            .ok_or_else(|| corruption("page group end overflows"))?;
        let bytes = self.read_range(store, path, start..end, u64::MAX).await?;
        if bytes.len() as u64 != end - start {
            return Err(corruption("metadata page group length mismatch"));
        }
        let mut pages = Vec::with_capacity(group.pages.len());
        for (reference, positions) in group.pages {
            let offset = reference.offset - group.range.start;
            let start = usize::try_from(offset)
                .map_err(|_| corruption("page group offset cannot be represented"))?;
            let end = start
                .checked_add(reference.bytes as usize)
                .ok_or_else(|| corruption("page group length overflows"))?;
            let body = bytes
                .get(start..end)
                .ok_or_else(|| corruption("page group does not cover its member"))?;
            let page = Arc::new(decode(
                body,
                &reference,
                kind,
                source.end - source.start,
                path.as_ref(),
            )?);
            let key = (layer, source.start + reference.offset);
            let page = self.remember_page(key, &reference, kind, page)?;
            pages.push((page, positions));
        }
        Ok(pages)
    }
}

#[cfg(test)]
#[path = "lookup_tests.rs"]
mod tests;
