//! Bounded admission lookups within one authenticated immutable run.
use super::*;
use crate::pages::{Kind, PageCache, PageWork};
use crab_storage::{Store, StoreLayout};
use std::collections::BTreeSet;

/// Paged exact placement hints; membership never grants reference visibility.
#[derive(Debug)]
pub struct CapsuleRunAdmissionReader {
    path: object_store::path::Path,
    offset: u64,
    pages: AdmissionPages,
    maximum: u64,
    cache: PageCache,
}

impl CapsuleRunAdmissionReader {
    pub(in crate::capsule_protocol::run) fn new(
        layout: &StoreLayout<Store>,
        hash: &str,
        offset: u64,
        length: u64,
        pages: AdmissionPages,
        member_count: usize,
        maximum: u64,
    ) -> Result<Self> {
        pages.validate_region(length, member_count)?;
        offset
            .checked_add(length)
            .ok_or_else(|| corrupt("capsule run admission range overflows"))?;
        Ok(Self {
            path: layout.capsule_path(hash),
            offset,
            pages,
            maximum,
            cache: PageCache::default(),
        })
    }

    /// Return exact member lists or authenticated misses, preserving duplicates and input order.
    ///
    /// The caller admits input/output cardinality; the reader bounds aggregate encoded page work.
    pub async fn lookup(&self, store: &Store, oids: &[[u8; 20]]) -> Result<Vec<Option<Vec<u32>>>> {
        let mut work = self.work();
        let keys = oids.iter().map(|oid| oid.to_vec()).collect::<Vec<_>>();
        let values = self
            .rows(
                store,
                &self.pages.objects,
                Kind::RunAdmission,
                &keys,
                &mut work,
            )
            .await?;
        let result = values
            .into_iter()
            .map(|value| {
                value
                    .map(|value| decode_members(&value, self.pages.member_count as usize))
                    .transpose()
            })
            .collect::<Result<Vec<_>>>()?;
        let members = result
            .iter()
            .flatten()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if self
            .member_counts(store, &members, &mut work)
            .await?
            .contains(&0)
        {
            return Err(corrupt(
                "capsule run admission names a member with zero objects",
            ));
        }
        Ok(result)
    }

    /// Return authenticated counts in member order without decoding the OID directory.
    pub async fn counts(&self, store: &Store) -> Result<Vec<u64>> {
        let members = (0..self.pages.member_count).collect::<Vec<_>>();
        self.member_counts(store, &members, &mut self.work()).await
    }

    async fn member_counts(
        &self,
        store: &Store,
        members: &[u32],
        work: &mut PageWork,
    ) -> Result<Vec<u64>> {
        let keys = members
            .iter()
            .map(|member| member.to_be_bytes().to_vec())
            .collect::<Vec<_>>();
        let values = self
            .rows(
                store,
                &self.pages.counts,
                Kind::RunAdmissionCounts,
                &keys,
                work,
            )
            .await?;
        values
            .into_iter()
            .map(|value| {
                let value =
                    value.ok_or_else(|| corrupt("required capsule member count is absent"))?;
                let count = u64::from_le_bytes(
                    value
                        .as_slice()
                        .try_into()
                        .map_err(|_| corrupt("capsule member count has an invalid size"))?,
                );
                if count > u64::from(self.pages.objects.count) {
                    return Err(corrupt("capsule member count exceeds total objects"));
                }
                Ok(count)
            })
            .collect()
    }

    async fn rows(
        &self,
        store: &Store,
        root: &crate::pages::PageRef,
        kind: Kind,
        keys: &[Vec<u8>],
        work: &mut PageWork,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let end = self
            .offset
            .checked_add(self.pages.body_bytes)
            .ok_or_else(|| corrupt("capsule admission body range overflows"))?;
        self.cache
            .lookup(
                store,
                &self.path,
                0,
                self.offset..end,
                root,
                kind,
                keys,
                work,
            )
            .await
    }

    fn work(&self) -> PageWork {
        PageWork::new(self.maximum, 0)
    }
}
