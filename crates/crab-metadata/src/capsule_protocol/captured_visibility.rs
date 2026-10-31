//! Captured visibility closure layers; physical presence never grants membership.
use crate::capsule_protocol::{
    CapsuleControl, CapsuleTransaction, CapsuleVisibilityDelta, CapsuleVisibilityReader,
    LayeredVisibilityReader, VisibilityChange, VisibilityEditHeader,
};
use crate::error::{MetadataError, Result};
use crate::git_visibility::{
    GitVisibilityDictionary, GitVisibilityIndex, MAX_GIT_VISIBILITY_OBJECTS, decode_oid, encode_oid,
};
use crab_storage::Store;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Reconcile captured controls against complete or paged visibility evidence.
pub enum VisibilityReconciliation {
    Complete(GitVisibilityIndex),
    Paged(Box<CapturedVisibility>),
}

impl VisibilityReconciliation {
    pub(super) async fn prepare_controls<'a>(
        &mut self,
        controls: impl Iterator<Item = &'a CapsuleControl>,
    ) -> Result<()> {
        if let Self::Paged(index) = self {
            index.prepare_controls(controls).await?;
        }
        Ok(())
    }

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
    fn count(&self) -> u64 {
        match self {
            Self::Complete(index) => index.objects().len() as u64,
            Self::Paged(reader) => reader.object_count(),
        }
    }
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
    async fn dictionary(&self, store: &Store, oids: &[[u8; 20]]) -> Result<Vec<bool>> {
        match self {
            Self::Complete(index) => Ok(oids.iter().map(|oid| index.retains(oid)).collect()),
            Self::Paged(reader) => Ok(reader.dictionary_contains(store, oids).await?),
        }
    }
}

/// Retain immutable prior ref closures while applying authenticated visibility edits.
pub struct CapturedVisibility {
    store: Store,
    initial: Initial,
    layers: Vec<Layer>,
    refs: BTreeMap<String, usize>,
    dictionary: BTreeSet<[u8; 20]>,
    initial_absent: BTreeSet<[u8; 20]>,
    dictionary_roots: Vec<(Arc<CapsuleVisibilityReader>, String, VisibilityEditHeader)>,
    dictionary_count: u64,
    shared_dictionaries: BTreeMap<[u8; 32], Arc<GitVisibilityDictionary>>,
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
        let dictionary_count = initial.count();
        Self {
            store,
            initial,
            layers,
            refs,
            dictionary: BTreeSet::new(),
            initial_absent: BTreeSet::new(),
            dictionary_roots: Vec::new(),
            dictionary_count,
            shared_dictionaries: BTreeMap::new(),
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
                    .lookup_reconciled(&self.store, name, &keys)
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

    async fn prepare_controls<'a>(
        &mut self,
        controls: impl Iterator<Item = &'a CapsuleControl>,
    ) -> Result<()> {
        if !matches!(&self.initial, Initial::Paged(_)) || self.initial.count() == 0 {
            return Ok(());
        }
        let mut added = BTreeSet::new();
        for control in controls {
            let Some((reader, headers)) = self.headers(control).await? else {
                continue;
            };
            if let Some((_, dictionary)) = reader.prepare_reconciliation(&self.store).await? {
                for object in dictionary.objects() {
                    added.insert(decode_oid(object)?);
                    check_dictionary_size(added.len() as u64)?;
                }
                continue;
            }
            for (edit, header) in control.transaction().edits().iter().zip(headers) {
                if header.is_none() {
                    continue;
                }
                for object in reader
                    .objects_reconciled(&self.store, edit.ref_name(), VisibilityChange::Added)
                    .await?
                {
                    added.insert(object);
                    check_dictionary_size(added.len() as u64)?;
                }
            }
        }
        // Reconcile all additions against the pinned dictionary in one page walk.
        // Per-transaction walks thrash bounded caches on large checkpoints.
        let added = added.into_iter().collect::<Vec<_>>();
        let present = self.initial.dictionary(&self.store, &added).await?;
        // Cache only initial retention, never visibility from unapplied controls.
        for (oid, present) in added.into_iter().zip(present) {
            if present {
                self.dictionary.insert(oid);
            } else {
                self.initial_absent.insert(oid);
            }
        }
        Ok(())
    }

    async fn dictionary_contains(&self, oids: &[[u8; 20]]) -> Result<Vec<bool>> {
        let mut result = oids
            .iter()
            .map(|oid| {
                self.dictionary.contains(oid)
                    || (!self.shared_dictionaries.is_empty() && {
                        let oid = encode_oid(oid);
                        self.shared_dictionaries
                            .values()
                            .any(|dictionary| dictionary.objects().binary_search(&oid).is_ok())
                    })
            })
            .collect::<Vec<_>>();
        let (positions, missing): (Vec<_>, Vec<_>) = oids
            .iter()
            .enumerate()
            .filter(|(index, oid)| !result[*index] && !self.initial_absent.contains(*oid))
            .map(|(index, oid)| (index, *oid))
            .unzip();
        let present = self.initial.dictionary(&self.store, &missing).await?;
        for (index, present) in positions.into_iter().zip(present) {
            result[index] = present;
        }
        for (reader, name, _) in &self.dictionary_roots {
            let positions = result
                .iter()
                .enumerate()
                .filter_map(|(index, present)| (!present).then_some(index))
                .collect::<Vec<_>>();
            if positions.is_empty() {
                break;
            }
            let keys = positions
                .iter()
                .map(|index| oids[*index])
                .collect::<Vec<_>>();
            let changes = reader
                .lookup_reconciled(&self.store, name, &keys)
                .await?
                .ok_or_else(|| {
                    corrupt_path(
                        "capsule Git visibility",
                        "dictionary addition source is absent",
                    )
                })?;
            for (index, change) in positions.into_iter().zip(changes) {
                result[index] = change == Some(VisibilityChange::Added);
            }
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

    async fn headers(
        &self,
        control: &CapsuleControl,
    ) -> Result<
        Option<(
            Arc<CapsuleVisibilityReader>,
            Vec<Option<VisibilityEditHeader>>,
        )>,
    > {
        let Some(reader) = control.paged_visibility() else {
            if control.has_visibility() {
                return Err(corrupt_path(
                    "capsule Git visibility",
                    "Git control is not paged",
                ));
            }
            return Ok(None);
        };
        let edits = control.transaction().edits();
        let live = edits.iter().filter(|edit| edit.new_oid().is_some()).count();
        if reader.ref_count() as usize != live {
            return Err(corrupt_path(
                "capsule Git visibility",
                "visibility evidence has missing or uncommitted refs",
            ));
        }
        let names = edits.iter().map(|edit| edit.ref_name()).collect::<Vec<_>>();
        let headers = reader.headers(&self.store, &names).await?;
        for (edit, header) in edits.iter().zip(&headers) {
            match (edit.new_oid(), header) {
                (None, None) => {}
                (Some(new), Some(header))
                    if header.new_oid() == new
                        && edit
                            .expected_old()
                            .is_none_or(|old| header.old_oid() == Some(old)) => {}
                _ => {
                    return Err(corrupt_path(
                        "capsule Git visibility",
                        "visibility evidence differs from its ref edit",
                    ));
                }
            }
        }
        Ok(Some((reader.clone(), headers)))
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
        let Some((_, headers)) = self.headers(control).await? else {
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
        let Some((reader, headers)) = self.headers(control).await? else {
            // No payload skips OID changes, not deletion of an entire ref.
            // Keep historical dictionary entries separate from live closures.
            for edit in control.transaction().edits() {
                if edit.new_oid().is_none() {
                    self.refs.remove(edit.ref_name());
                }
            }
            return Ok(());
        };
        let shared_dictionary = reader.prepare_reconciliation(&self.store).await?;
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
                .lookup_reconciled(&self.store, edit.ref_name(), &[new])
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
                .objects_reconciled(&self.store, edit.ref_name(), VisibilityChange::Removed)
                .await?;
            if !removed.is_empty() {
                reader
                    .lookup_reconciled(&self.store, edit.ref_name(), &removed)
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
            if shared_dictionary.is_none() {
                // A first rooted set supplies its exact dictionary count. Later equal
                // roots add no objects; actual differing deltas admit their additions.
                let shared = self
                    .dictionary_roots
                    .iter()
                    .any(|(_, _, prior)| header.same_additions(prior));
                if header.added_count() > 0 && self.dictionary_count == 0 {
                    self.dictionary_count = u64::from(header.added_count());
                    self.dictionary_roots.push((
                        reader.clone(),
                        edit.ref_name().to_owned(),
                        header.clone(),
                    ));
                } else if !shared {
                    let added = reader
                        .objects_reconciled(&self.store, edit.ref_name(), VisibilityChange::Added)
                        .await?;
                    let present = self.dictionary_contains(&added).await?;
                    for (object, present) in added.into_iter().zip(present) {
                        if !present {
                            self.initial_absent.remove(&object);
                            self.dictionary.insert(object);
                            self.dictionary_count += 1;
                        }
                    }
                }
                if self.dictionary_count > MAX_GIT_VISIBILITY_OBJECTS {
                    return Err(corrupt_path(
                        "capsule Git visibility",
                        "visibility object dictionary is too large",
                    ));
                }
            }
            let index = self.layers.len();
            self.layers.push(Layer {
                parent,
                initial_ref: None,
                edit: Some((reader.clone(), edit.ref_name().to_owned(), header)),
            });
            self.refs.insert(edit.ref_name().to_owned(), index);
        }
        if let Some((identity, dictionary)) = shared_dictionary
            && !self.shared_dictionaries.contains_key(&identity)
        {
            let additional = if self.dictionary_count == 0 {
                dictionary.objects().len() as u64
            } else {
                let oids = dictionary
                    .objects()
                    .iter()
                    .map(|oid| decode_oid(oid))
                    .collect::<Result<Vec<_>>>()?;
                self.dictionary_contains(&oids)
                    .await?
                    .into_iter()
                    .filter(|present| !present)
                    .count() as u64
            };
            self.dictionary_count =
                self.dictionary_count
                    .checked_add(additional)
                    .ok_or_else(|| {
                        corrupt_path("capsule Git visibility", "dictionary count overflows")
                    })?;
            check_dictionary_size(self.dictionary_count)?;
            // Only applied, fully checked edit sets can contribute retention.
            // The dictionary itself never supplies a prior ref closure.
            self.shared_dictionaries.insert(identity, dictionary);
        }
        Ok(())
    }
}

fn check_dictionary_size(count: u64) -> Result<()> {
    if count > MAX_GIT_VISIBILITY_OBJECTS {
        return Err(corrupt_path(
            "capsule Git visibility",
            "visibility object dictionary is too large",
        ));
    }
    Ok(())
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
