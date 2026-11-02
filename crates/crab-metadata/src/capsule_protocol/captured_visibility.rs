//! Captured visibility closure layers; physical presence never grants membership.
use crate::capsule_protocol::{
    CapsuleControl, CapsuleTransaction, CapsuleVisibilityDelta, CapsuleVisibilityReader,
    LayeredVisibilityReader, VisibilityChange, VisibilityEditHeader,
};
use crate::error::{MetadataError, Result};
use crate::git_visibility::{GitVisibilityIndex, decode_oid};
use crab_storage::Store;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Reconcile captured controls against complete or paged visibility evidence.
pub enum VisibilityReconciliation {
    Complete(GitVisibilityIndex),
    Paged(Box<CapturedVisibility>),
}

impl VisibilityReconciliation {
    /// Check expected ref values and captured prior-closure availability.
    pub async fn ready(
        &self,
        control: &CapsuleControl,
        refs: &BTreeMap<String, String>,
    ) -> Result<bool> {
        match self {
            Self::Complete(index) => visibility_is_ready(
                control.transaction(),
                control.visibility_delta()?,
                refs,
                index,
            ),
            Self::Paged(index) => index.ready(control, refs).await,
        }
    }
    /// Apply one authenticated control without widening to mutable ref state.
    pub async fn apply(&mut self, control: &CapsuleControl) -> Result<()> {
        match self {
            Self::Complete(index) if control.has_visibility() => {
                apply_transaction_visibility_index(
                    control.transaction(),
                    control.visibility_delta()?,
                    index,
                )
            }
            Self::Complete(index) => {
                // A deletion-only capsule has no visibility payload. Its ref
                // closure must still disappear before another control borrows it.
                for edit in control.transaction().edits() {
                    if edit.new_oid().is_none() {
                        index.remove_ref(edit.ref_name());
                    }
                }
                Ok(())
            }
            Self::Paged(index) => index.apply(control).await,
        }
    }
}

struct Layer {
    parent: Option<usize>,
    initial_ref: Option<String>,
    edit: Option<(Arc<CapsuleVisibilityReader>, String, VisibilityEditHeader)>,
}

enum Initial {
    Complete(GitVisibilityIndex),
    Paged(Arc<LayeredVisibilityReader>),
}

impl Initial {
    async fn contains(&self, store: &Store, name: &str, oids: &[[u8; 20]]) -> Result<Vec<bool>> {
        match self {
            Self::Complete(index) => Ok(oids
                .iter()
                .map(|oid| index.contains_in_ref(name, oid))
                .collect()),
            Self::Paged(reader) => {
                reader
                    .contains_in_ref(store, name, oids)
                    .await?
                    .ok_or_else(|| {
                        corrupt_path("checkpoint visibility", "captured initial ref is absent")
                    })
            }
        }
    }
}

/// Retain immutable prior ref closures while applying authenticated visibility edits.
pub struct CapturedVisibility {
    store: Store,
    initial: Initial,
    layers: Vec<Layer>,
    refs: BTreeMap<String, usize>,
}

impl CapturedVisibility {
    /// Seed captured closures from complete authenticated visibility evidence.
    #[must_use]
    pub fn new(
        store: Store,
        initial: GitVisibilityIndex,
        names: impl Iterator<Item = String>,
    ) -> Self {
        let names = names
            .filter(|name| initial.contains_ref(name))
            .collect::<Vec<_>>();
        Self::seed(store, Initial::Complete(initial), names)
    }

    /// Seed captured closures from the checkpoint's authenticated visibility pages.
    pub async fn from_checkpoint(
        store: Store,
        reader: Arc<LayeredVisibilityReader>,
        names: impl Iterator<Item = String>,
    ) -> Result<Self> {
        let names = names.collect::<Vec<_>>();
        let borrowed = names.iter().map(String::as_str).collect::<Vec<_>>();
        let present = reader.refs_present(&store, &borrowed).await?;
        let names = names
            .into_iter()
            .zip(present)
            .filter_map(|(name, present)| present.then_some(name))
            .collect();
        Ok(Self::seed(store, Initial::Paged(reader), names))
    }

    fn seed(store: Store, initial: Initial, names: Vec<String>) -> Self {
        let mut layers = Vec::new();
        let mut refs = BTreeMap::new();
        for name in names {
            refs.insert(name.clone(), layers.len());
            layers.push(Layer {
                parent: None,
                initial_ref: Some(name),
                edit: None,
            });
        }
        Self {
            store,
            initial,
            layers,
            refs,
        }
    }

    async fn contains(&self, layer: Option<usize>, oids: &[[u8; 20]]) -> Result<Vec<bool>> {
        let mut result = vec![false; oids.len()];
        let mut pending = (0..oids.len()).collect::<Vec<_>>();
        let mut next = layer;
        while let Some(index) = next {
            let layer = self.layers.get(index).ok_or_else(|| {
                corrupt_path("capsule Git visibility", "closure parent is invalid")
            })?;
            if let Some(name) = &layer.initial_ref {
                let keys = pending
                    .iter()
                    .map(|position| oids[*position])
                    .collect::<Vec<_>>();
                let present = self.initial.contains(&self.store, name, &keys).await?;
                for (position, present) in pending.into_iter().zip(present) {
                    result[position] = present;
                }
                break;
            }
            if let Some((reader, name, header)) = &layer.edit {
                let keys = pending
                    .iter()
                    .map(|position| oids[*position])
                    .collect::<Vec<_>>();
                let changes = reader
                    .lookup(&self.store, name, &keys)
                    .await?
                    .ok_or_else(|| {
                        corrupt_path("capsule Git visibility", "required edit is absent")
                    })?;
                let mut unresolved = Vec::new();
                for (position, change) in pending.into_iter().zip(changes) {
                    match change {
                        Some(VisibilityChange::Added) => result[position] = true,
                        Some(VisibilityChange::Removed) => {}
                        None if !header.replaces() => unresolved.push(position),
                        None => {}
                    }
                }
                pending = unresolved;
            }
            if pending.is_empty() {
                break;
            }
            next = layer.parent;
        }
        Ok(result)
    }

    async fn first_ref(&self, object: [u8; 20]) -> Result<Option<usize>> {
        for index in self.refs.values() {
            if self.contains(Some(*index), &[object]).await?[0] {
                return Ok(Some(*index));
            }
        }
        Ok(None)
    }

    /// Check expected ref values and captured prior-closure availability.
    pub async fn ready(
        &self,
        control: &CapsuleControl,
        refs: &BTreeMap<String, String>,
    ) -> Result<bool> {
        if control
            .transaction()
            .edits()
            .iter()
            .any(|edit| refs.get(edit.ref_name()).map(String::as_str) != edit.expected_old())
        {
            return Ok(false);
        }
        let Some((_, headers)) = control.paged_visibility_headers(&self.store).await? else {
            return Ok(true);
        };
        for (edit, header) in control.transaction().edits().iter().zip(headers) {
            let Some(header) = header else {
                continue;
            };
            if edit.expected_old().is_some() && !self.refs.contains_key(edit.ref_name()) {
                return Ok(false);
            }
            if edit.expected_old().is_none()
                && !header.replaces()
                && let Some(old) = header.old_oid()
                && self.first_ref(decode_oid(old)?).await?.is_none()
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Apply one authenticated control without widening to mutable ref state.
    pub async fn apply(&mut self, control: &CapsuleControl) -> Result<()> {
        let Some((reader, headers)) = control.paged_visibility_headers(&self.store).await? else {
            // No payload skips OID changes, not deletion of an entire ref.
            // Prior layers still anchor already captured descendant closures.
            for edit in control.transaction().edits() {
                if edit.new_oid().is_none() {
                    self.refs.remove(edit.ref_name());
                }
            }
            return Ok(());
        };
        for (edit, header) in control.transaction().edits().iter().zip(headers) {
            let Some(header) = header else {
                self.refs.remove(edit.ref_name());
                continue;
            };
            let mut parent = self.refs.get(edit.ref_name()).copied();
            if parent.is_none()
                && !header.replaces()
                && let Some(old) = header.old_oid()
            {
                parent = self.first_ref(decode_oid(old)?).await?;
            }
            if let Some(old) = header.old_oid()
                && !self.contains(parent, &[decode_oid(old)?]).await?[0]
            {
                return Err(corrupt_path(
                    "capsule Git visibility",
                    "visibility delta prior closure does not contain its old ref tip",
                ));
            }
            let new = decode_oid(header.new_oid())?;
            let change = reader
                .lookup(&self.store, edit.ref_name(), &[new])
                .await?
                .ok_or_else(|| corrupt_path("capsule Git visibility", "required edit is absent"))?;
            let visible = match change[0] {
                Some(VisibilityChange::Added) => true,
                Some(VisibilityChange::Removed) => false,
                None if !header.replaces() => self.contains(parent, &[new]).await?[0],
                None => false,
            };
            if !visible {
                return Err(corrupt_path(
                    "capsule Git visibility",
                    "visibility edit result does not contain its new ref tip",
                ));
            }
            let removed = reader
                .objects(&self.store, edit.ref_name(), VisibilityChange::Removed)
                .await?;
            if !removed.is_empty() {
                reader
                    .lookup(&self.store, edit.ref_name(), &removed)
                    .await?
                    .ok_or_else(|| {
                        corrupt_path("capsule Git visibility", "required edit is absent")
                    })?;
            }
            if self.contains(parent, &removed).await?.contains(&false) {
                return Err(corrupt_path(
                    "capsule Git visibility",
                    "visibility edit removes an object outside the prior closure",
                ));
            }
            // Captured layers own point membership, not a materialized global
            // dictionary. Complete GitVisibilityIndex construction enforces its
            // distinct-object limit; scanning additions here multiplies history I/O.
            let index = self.layers.len();
            self.layers.push(Layer {
                parent,
                initial_ref: None,
                edit: Some((reader.clone(), edit.ref_name().to_owned(), header)),
            });
            self.refs.insert(edit.ref_name().to_owned(), index);
        }
        Ok(())
    }
}

/// Validate publication evidence and check its prior ref dependencies.
pub fn visibility_is_ready(
    transaction: &CapsuleTransaction,
    delta: Option<&CapsuleVisibilityDelta>,
    refs: &BTreeMap<String, String>,
    visibility_index: &crate::git_visibility::GitVisibilityIndex,
) -> Result<bool> {
    if transaction
        .edits()
        .iter()
        .any(|edit| refs.get(edit.ref_name()).map(String::as_str) != edit.expected_old())
    {
        return Ok(false);
    }
    let Some(delta) = delta else {
        return Ok(true);
    };
    // Transactions have unique refs. Count matched rows without copying OID lists;
    // unmatched evidence still fails before it can authorize a publication.
    let evidence = delta.edits();
    let mut matched = 0;
    for edit in transaction.edits() {
        let Some(new_oid) = edit.new_oid() else {
            if evidence.get(edit.ref_name()).is_some() {
                return Err(corrupt_path(
                    "capsule Git visibility",
                    "deleted ref has visibility evidence",
                ));
            }
            continue;
        };
        let Some(visibility) = evidence.get(edit.ref_name()) else {
            return Err(corrupt_path(
                "capsule Git visibility",
                "live ref edit has no visibility evidence",
            ));
        };
        matched += 1;
        visibility.validate()?;
        if visibility.new_oid != new_oid {
            return Err(corrupt_path(
                "capsule Git visibility",
                "visibility evidence does not match its ref edit",
            ));
        }
        match edit.expected_old() {
            Some(expected_old) => {
                if visibility.old_oid.as_deref() != Some(expected_old) {
                    return Err(corrupt_path(
                        "capsule Git visibility",
                        "visibility evidence does not match the expected old ref",
                    ));
                }
                if !visibility_index.contains_ref(edit.ref_name()) {
                    return Ok(false);
                }
            }
            None if visibility.replaces => {}
            None => {
                let Some(old_oid) = visibility.old_oid.as_deref() else {
                    continue;
                };
                if !visibility_index.contains_hex_in_any_ref(old_oid) {
                    return Ok(false);
                }
            }
        }
    }
    if evidence.len() != matched {
        return Err(corrupt_path(
            "capsule Git visibility",
            "visibility evidence contains an uncommitted ref",
        ));
    }
    Ok(true)
}

/// Apply transaction-bound visibility evidence to a complete captured index.
pub fn apply_transaction_visibility_index(
    transaction: &CapsuleTransaction,
    delta: Option<&CapsuleVisibilityDelta>,
    index: &mut crate::git_visibility::GitVisibilityIndex,
) -> Result<()> {
    if let Some(delta) = delta
        && transaction.edits().len() == delta.edits().len()
        && transaction.edits().iter().all(|edit| {
            edit.expected_old().is_none()
                && edit.new_oid().is_some_and(|new| {
                    delta
                        .edits()
                        .get(edit.ref_name())
                        .is_some_and(|visibility| {
                            visibility.old_oid.is_none() && visibility.new_oid == new
                        })
                })
        })
        && index.apply_initial_shared_ref_edits(delta.edits())?
    {
        return Ok(());
    }
    let evidence = delta.map(|delta| delta.edits());
    let mut matched = 0;
    for edit in transaction.edits() {
        let Some(new_oid) = edit.new_oid() else {
            if evidence
                .and_then(|rows| rows.get(edit.ref_name()))
                .is_some()
            {
                return Err(corrupt_path(
                    "capsule Git visibility",
                    "deleted ref has visibility evidence",
                ));
            }
            index.remove_ref(edit.ref_name());
            continue;
        };
        let visibility = evidence
            .and_then(|rows| rows.get(edit.ref_name()))
            .ok_or_else(|| {
                corrupt_path(
                    "capsule Git visibility",
                    "live ref edit has no visibility evidence",
                )
            })?;
        matched += 1;
        if visibility.new_oid != new_oid {
            return Err(corrupt_path(
                "capsule Git visibility",
                "visibility evidence does not match its ref edit",
            ));
        }
        if let Some(expected_old) = edit.expected_old()
            && visibility.old_oid.as_deref() != Some(expected_old)
        {
            return Err(corrupt_path(
                "capsule Git visibility",
                "visibility evidence does not match the expected old ref",
            ));
        }
        index.apply_ref_edit(edit.ref_name().to_owned(), visibility)?;
    }
    if evidence.is_some_and(|rows| rows.len() != matched) {
        return Err(corrupt_path(
            "capsule Git visibility",
            "visibility evidence contains an uncommitted ref",
        ));
    }
    Ok(())
}

fn corrupt_path(path: impl Into<String>, reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: path.into(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests;
