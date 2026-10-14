//! Bounded reads from roots authenticated by the captured checkpoint footer.
use super::*;
use crate::pages::{PageCache, PageWork};
use crab_storage::{Store, StoreLayout};
use std::sync::{Arc, OnceLock};

/// Captured checkpoint dictionary and ref-closure pages; retention never grants visibility.
#[derive(Debug)]
pub struct LayeredVisibilityReader {
    path: object_store::path::Path,
    range: PackRange,
    descriptor: Descriptor,
    sources: Vec<PackSourceDescriptor>,
    maximum: u64,
    cache: PageCache,
    complete: OnceLock<Arc<LayeredVisibilitySnapshot>>,
}

impl LayeredVisibilityReader {
    pub(in crate::capsule_protocol::layered) fn new(
        layout: &StoreLayout<Store>,
        hash: &str,
        range: PackRange,
        descriptor: Descriptor,
        sources: Vec<PackSourceDescriptor>,
        maximum: u64,
    ) -> Result<Self> {
        descriptor.validate(range.length())?;
        range.end()?;
        if descriptor.catalog_digest != source_catalog_digest(&sources)? {
            return Err(corrupt(
                "checkpoint visibility pages name a different source catalog",
            ));
        }
        Ok(Self {
            path: layout.capsule_checkpoint_path(hash),
            range,
            descriptor,
            sources,
            maximum,
            cache: PageCache::default(),
            complete: OnceLock::new(),
        })
    }

    /// Return the retained dictionary count, independently of ref visibility.
    #[must_use]
    pub fn object_count(&self) -> u64 {
        u64::from(self.descriptor.object_count)
    }

    fn work(&self) -> PageWork {
        PageWork {
            maximum: self.maximum,
            bytes: 0,
            seen: BTreeSet::new(),
        }
    }

    async fn rows(
        &self,
        store: &Store,
        root: Option<&PageRef>,
        kind: Kind,
        keys: &[Vec<u8>],
        work: &mut PageWork,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let Some(root) = root else {
            return Ok(vec![None; keys.len()]);
        };
        let end = self
            .range
            .offset()
            .checked_add(self.descriptor.body_bytes)
            .ok_or_else(|| corrupt("checkpoint visibility region overflows"))?;
        self.cache
            .lookup(
                store,
                &self.path,
                0,
                self.range.offset()..end,
                root,
                kind,
                keys,
                work,
            )
            .await
    }

    async fn ref_rows(
        &self,
        store: &Store,
        names: &[&str],
        work: &mut PageWork,
    ) -> Result<Vec<Option<RefRow>>> {
        for name in names {
            validate_visibility_ref(name)?;
        }
        let keys = names
            .iter()
            .map(|name| blake3::hash(name.as_bytes()).as_bytes().to_vec())
            .collect::<Vec<_>>();
        self.rows(
            store,
            self.descriptor.refs.as_ref(),
            Kind::CheckpointRefs,
            &keys,
            work,
        )
        .await?
        .into_iter()
        .zip(keys)
        .zip(names)
        .map(|((value, key), name)| {
            let row = value
                .map(|value| RefRow::decode(&key, &value, &self.descriptor))
                .transpose()?;
            if row.as_ref().is_some_and(|row| row.name != *name) {
                return Err(corrupt("checkpoint ref name hash collides"));
            }
            Ok(row)
        })
        .collect()
    }

    /// Return authenticated ref presence without loading closures or transitions.
    pub async fn refs_present(&self, store: &Store, names: &[&str]) -> Result<Vec<bool>> {
        Ok(self
            .ref_rows(store, names, &mut self.work())
            .await?
            .into_iter()
            .map(|row| row.is_some())
            .collect())
    }

    async fn dictionary(
        &self,
        store: &Store,
        oids: &[[u8; 20]],
        work: &mut PageWork,
    ) -> Result<Vec<bool>> {
        let keys = oids.iter().map(|oid| oid.to_vec()).collect::<Vec<_>>();
        self.rows(
            store,
            self.descriptor.objects.as_ref(),
            Kind::CheckpointObjects,
            &keys,
            work,
        )
        .await?
        .into_iter()
        .map(|value| {
            let Some(value) = value else {
                return Ok(false);
            };
            if let Some(member) = member(&value, self.descriptor.member_admission)? {
                member.validate_sources(&self.sources)?;
            }
            Ok(true)
        })
        .collect()
    }

    /// Inspect retained object presence; these results do not authorize a ref read.
    pub async fn dictionary_contains(&self, store: &Store, oids: &[[u8; 20]]) -> Result<Vec<bool>> {
        self.dictionary(store, oids, &mut self.work()).await
    }

    /// Inspect one actual ref closure, preserving requested order and absent-ref distinction.
    pub async fn contains_in_ref(
        &self,
        store: &Store,
        name: &str,
        oids: &[[u8; 20]],
    ) -> Result<Option<Vec<bool>>> {
        let mut work = self.work();
        let Some(row) = self
            .ref_rows(store, &[name], &mut work)
            .await?
            .pop()
            .flatten()
        else {
            return Ok(None);
        };
        let keys = oids.iter().map(|oid| oid.to_vec()).collect::<Vec<_>>();
        let present = self
            .rows(
                store,
                row.objects.as_ref(),
                Kind::CheckpointSets,
                &keys,
                &mut work,
            )
            .await?
            .into_iter()
            .map(|value| {
                if value.as_ref().is_some_and(|value| !value.is_empty()) {
                    return Err(corrupt("checkpoint closure row has unexpected payload"));
                }
                Ok(value.is_some())
            })
            .collect::<Result<Vec<_>>>()?;
        // Loaded closure hits must also belong to the retained dictionary. The
        // dictionary alone cannot grant membership in this or another ref.
        let hits = oids
            .iter()
            .zip(&present)
            .filter_map(|(oid, present)| present.then_some(*oid))
            .collect::<Vec<_>>();
        if self
            .dictionary(store, &hits, &mut work)
            .await?
            .iter()
            .any(|present| !present)
        {
            return Err(corrupt(
                "checkpoint closure escapes its retained dictionary",
            ));
        }
        Ok(Some(present))
    }

    /// Authenticate the whole captured section for explicit bulk inspection.
    pub async fn materialize(&self, store: &Store) -> Result<Arc<LayeredVisibilitySnapshot>> {
        if let Some(snapshot) = self.complete.get() {
            return Ok(snapshot.clone());
        }
        if self.range.length() > self.maximum {
            return Err(corrupt(
                "complete checkpoint visibility exceeds its byte budget",
            ));
        }
        let bytes = store
            .range_get(&self.path, self.range.offset()..self.range.end()?)
            .await?;
        if blake3::hash(&bytes).to_hex().as_str() != self.range.blake3()
            || Descriptor::decode(&bytes)? != self.descriptor
        {
            return Err(corrupt(
                "checkpoint visibility section differs from captured roots",
            ));
        }
        let snapshot = Arc::new(LayeredVisibilitySnapshot::decode(&bytes)?);
        for member in snapshot.member_admission().unwrap_or_default() {
            member.validate_sources(&self.sources)?;
        }
        Ok(self.complete.get_or_init(|| snapshot).clone())
    }
}
