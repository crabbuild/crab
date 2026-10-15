//! Captured run roots and exact selected logical-capsule membership.
use super::*;
use crate::pages::{PageCache, PageWork};
use crab_storage::{Store, StoreLayout};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(in crate::capsule_protocol) struct CatalogOrigin {
    pub path: object_store::path::Path,
    pub range: std::ops::Range<u64>,
    pub layer: usize,
}

pub(in crate::capsule_protocol) struct CatalogValue {
    pub origin: Arc<CatalogOrigin>,
    pub bytes: Vec<u8>,
    pub identity: Option<[u8; 32]>,
    pub rank: usize,
}

/// Authenticated catalog roots and source ranges captured from one immutable run.
///
/// Construction is restricted to verified run footers. The owning repository
/// view must retain this run against GC for the lifetime of its queries.
#[derive(Debug, Clone)]
pub struct RunCatalogSource {
    path: object_store::path::Path,
    range: std::ops::Range<u64>,
    pages: CatalogPages,
    capsules: Vec<(String, Option<std::ops::Range<u64>>)>,
}

impl RunCatalogSource {
    pub(in crate::capsule_protocol::run) fn new(
        layout: &StoreLayout<Store>,
        hash: &str,
        range: &RunSectionLocation,
        pages: CatalogPages,
        capsules: Vec<(String, Option<std::ops::Range<u64>>)>,
    ) -> Result<Self> {
        pages.validate_region(range.length, capsules.len())?;
        let end = range
            .offset
            .checked_add(pages.body_bytes)
            .ok_or_else(|| corrupt("run catalog body range overflows"))?;
        let capsules = capsules
            .into_iter()
            .zip(&pages.capsule_bodies)
            .map(|((hash, range), body)| {
                let range = match (range, body) {
                    (Some(range), Some(body)) => {
                        let end = range
                            .start
                            .checked_add(*body)
                            .filter(|end| *end <= range.end)
                            .ok_or_else(|| {
                                corrupt("run catalog source body escapes its section")
                            })?;
                        Some(range.start..end)
                    }
                    (None, None) => None,
                    _ => return Err(corrupt("run catalog source presence differs")),
                };
                Ok((hash, range))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            path: layout.capsule_path(hash),
            range: range.offset..end,
            pages,
            capsules,
        })
    }

    pub(in crate::capsule_protocol) async fn lookup(
        &self,
        store: &Store,
        cache: &PageCache,
        layer: usize,
        ranks: &BTreeMap<String, usize>,
        kind: Kind,
        keys: &[Vec<u8>],
        work: &mut PageWork,
    ) -> Result<Vec<Vec<CatalogValue>>> {
        let position = match kind {
            Kind::CatalogFiles => 0,
            Kind::CatalogShards => 1,
            Kind::CatalogXorbs => 2,
            Kind::CatalogChunks => 3,
            _ => return Err(corrupt("unsupported run catalog directory")),
        };
        let mut output = (0..keys.len()).map(|_| Vec::new()).collect::<Vec<_>>();
        if !self
            .capsules
            .iter()
            .any(|(hash, _)| ranks.contains_key(hash))
        {
            return Ok(output);
        }
        let Some(root) = &self.pages.roots[position] else {
            return Ok(output);
        };
        let rows = cache
            .lookup(
                store,
                &self.path,
                layer,
                self.range.clone(),
                root,
                KINDS[position],
                keys,
                work,
            )
            .await?;
        for (result, row) in output.iter_mut().zip(rows) {
            let Some(row) = row else {
                continue;
            };
            for variant in decode_variants(&row, self.capsules.len())? {
                let rank = variant
                    .members
                    .iter()
                    .filter_map(|member| ranks.get(&self.capsules[*member as usize].0))
                    .max()
                    .copied();
                let Some(rank) = rank else {
                    continue;
                };
                let range = self.capsules[variant.source as usize]
                    .1
                    .clone()
                    .ok_or_else(|| corrupt("run catalog variant has no physical source"))?;
                // Membership admits logical values, not all capsules in the
                // physical run. Equal variants may reuse an unselected source.
                result.push(CatalogValue {
                    origin: Arc::new(CatalogOrigin {
                        path: self.path.clone(),
                        range,
                        layer,
                    }),
                    bytes: variant.value,
                    identity: Some(variant.identity),
                    rank,
                });
            }
        }
        Ok(output)
    }
}
