use super::*;
use crate::git_visibility::{GitVisibilityEdit, MAX_GIT_VISIBILITY_OBJECTS, decode_oid};
use crate::pages::PageWork;
use crab_storage::Store;
use std::sync::Arc;

#[derive(Clone, Default)]
struct RefClosure {
    base: Option<Closure>,
    changes: Arc<BTreeMap<[u8; 20], bool>>,
}

/// Apply captured visibility edits without materializing historical object sets.
///
/// One instance owns one immutable checkpoint reader and operation-scoped byte admission.
pub struct PagedGitVisibility {
    reader: Option<LayeredVisibilityReader>,
    store: Store,
    work: PageWork,
    refs: BTreeMap<String, RefClosure>,
    added: BTreeSet<[u8; 20]>,
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;

impl PagedGitVisibility {
    /// Load ref descriptors only; checkpoint closure and history pages remain cold.
    pub async fn new(reader: Option<LayeredVisibilityReader>, store: Store) -> Result<Self> {
        let mut work = reader.as_ref().map_or(
            PageWork {
                maximum: 0,
                bytes: 0,
                seen: BTreeSet::new(),
            },
            LayeredVisibilityReader::work,
        );
        let mut refs = BTreeMap::new();
        if let Some(reader) = &reader {
            for entry in reader.refs(&store, &mut work).await? {
                if refs
                    .insert(
                        entry.name,
                        RefClosure {
                            base: Some(entry.closure),
                            changes: Arc::default(),
                        },
                    )
                    .is_some()
                {
                    return Err(corrupt("visibility refs repeat a name"));
                }
            }
        }
        Ok(Self {
            reader,
            store,
            work,
            refs,
            added: BTreeSet::new(),
        })
    }

    /// Return whether this state retains a closure for the requested ref.
    pub fn contains_ref(&self, name: &str) -> bool {
        self.refs.contains_key(name)
    }

    /// Return captured ref names in lexical order, including empty closures.
    #[cfg(test)]
    fn ref_names(&self) -> impl Iterator<Item = &str> {
        self.refs.keys().map(String::as_str)
    }

    /// Remove a ref without changing dictionary admission for later edits.
    pub fn remove_ref(&mut self, name: &str) {
        self.refs.remove(name);
    }

    async fn ordinals(&mut self, objects: &[[u8; 20]]) -> Result<Vec<Option<u32>>> {
        match &self.reader {
            Some(reader) => reader.ordinals(&self.store, objects, &mut self.work).await,
            None => Ok(vec![None; objects.len()]),
        }
    }

    async fn membership(
        &mut self,
        closure: &RefClosure,
        objects: &[[u8; 20]],
        ordinals: &[Option<u32>],
    ) -> Result<Vec<bool>> {
        let mut membership = match (&self.reader, &closure.base) {
            (Some(reader), Some(base)) => {
                reader
                    .contains(&self.store, base, ordinals, &mut self.work)
                    .await?
            }
            _ => vec![false; objects.len()],
        };
        for (index, object) in objects.iter().enumerate() {
            if let Some(value) = closure.changes.get(object) {
                membership[index] = *value;
            }
        }
        Ok(membership)
    }

    /// Check requested OIDs against one current ref, preserving input positions.
    pub async fn contains_in_ref(&mut self, name: &str, objects: &[[u8; 20]]) -> Result<Vec<bool>> {
        let Some(closure) = self.refs.get(name).cloned() else {
            return Ok(vec![false; objects.len()]);
        };
        let ordinals = self.ordinals(objects).await?;
        self.membership(&closure, objects, &ordinals).await
    }

    /// Check whether the object belongs to any current ref closure.
    pub async fn contains_hex_in_any_ref(&mut self, object: &str) -> Result<bool> {
        let object = decode_oid(object)?;
        let ordinals = self.ordinals(&[object]).await?;
        let closures = self.refs.values().cloned().collect::<Vec<_>>();
        for closure in closures {
            if self
                .membership(&closure, &[object], &ordinals)
                .await?
                .first()
                == Some(&true)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Apply one edit using the same dictionary, inheritance and removal rules as full inspection.
    pub async fn apply_ref_edit(&mut self, name: String, edit: &GitVisibilityEdit) -> Result<()> {
        edit.validate()?;
        let old = edit.old_oid.as_deref().map(decode_oid).transpose()?;
        let new = decode_oid(&edit.new_oid)?;
        let added = edit
            .added
            .iter()
            .map(|oid| decode_oid(oid))
            .collect::<Result<Vec<_>>>()?;
        let removed = edit
            .removed
            .iter()
            .map(|oid| decode_oid(oid))
            .collect::<Result<Vec<_>>>()?;
        let mut requested = Vec::new();
        requested.extend(old);
        requested.push(new);
        requested.extend(&added);
        requested.extend(&removed);
        let ordinals = self.ordinals(&requested).await?;
        let known = requested
            .iter()
            .copied()
            .zip(ordinals.iter().copied())
            .collect::<BTreeMap<_, _>>();
        let mut closure = self.refs.get(&name).cloned().unwrap_or_default();
        if !self.refs.contains_key(&name)
            && !edit.replaces
            && let Some(old) = old
        {
            // Inheritance snapshots the first lexical matching ref. Later edits
            // to that ref must not change the new ref's borrowed base.
            let candidates = self.refs.values().cloned().collect::<Vec<_>>();
            for candidate in candidates {
                let ordinal = known.get(&old).copied().flatten();
                if self
                    .membership(&candidate, &[old], &[ordinal])
                    .await?
                    .first()
                    == Some(&true)
                {
                    closure = candidate;
                    break;
                }
            }
        }
        let prior = self.membership(&closure, &requested, &ordinals).await?;
        let membership = requested
            .iter()
            .copied()
            .zip(prior)
            .collect::<BTreeMap<_, _>>();
        let exists = |oid: &[u8; 20]| {
            known.get(oid).is_some_and(Option::is_some) || self.added.contains(oid)
        };
        if let Some(old) = old {
            if !exists(&old) {
                return Err(corrupt(
                    "visibility edit old tip is absent from its dictionary",
                ));
            }
            if membership.get(&old) != Some(&true) {
                return Err(corrupt(
                    "visibility delta prior closure does not contain its old ref tip",
                ));
            }
        }
        if removed.iter().any(|oid| !exists(oid)) {
            return Err(corrupt("visibility edit removes an unknown object"));
        }
        if removed.iter().any(|oid| membership.get(oid) != Some(&true)) {
            return Err(corrupt(
                "visibility edit removes an object outside the prior closure",
            ));
        }
        if !exists(&new) && !added.contains(&new) {
            return Err(corrupt(
                "visibility edit new tip is absent from its dictionary and additions",
            ));
        }
        let new_objects = added
            .iter()
            .filter(|oid| !exists(oid))
            .copied()
            .collect::<BTreeSet<_>>();
        let base_count = self
            .reader
            .as_ref()
            .map_or(0, LayeredVisibilityReader::object_count);
        if u64::from(base_count) + self.added.len() as u64 + new_objects.len() as u64
            > MAX_GIT_VISIBILITY_OBJECTS
        {
            return Err(corrupt("visibility object dictionary is too large"));
        }
        let result_contains_tip = added.contains(&new)
            || !edit.replaces && !removed.contains(&new) && membership.get(&new) == Some(&true);
        if !result_contains_tip {
            return Err(corrupt(
                "visibility edit result does not contain its new ref tip",
            ));
        }
        // Validation completes before consuming the stored closure. Moving its
        // changes avoids copying every prior delta unless another ref inherited it.
        if let Some(original) = self.refs.remove(&name) {
            closure = original;
        }
        if edit.replaces {
            closure = RefClosure::default();
        }
        let changes = Arc::make_mut(&mut closure.changes);
        for oid in removed {
            changes.insert(oid, false);
        }
        for oid in added {
            changes.insert(oid, true);
        }
        self.added.extend(new_objects);
        self.refs.insert(name, closure);
        Ok(())
    }
}
