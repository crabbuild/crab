//! Captured visibility closure layers; physical presence never grants membership.
use super::*;
use crab_metadata::capsule_protocol::{
    CapsuleVisibilityReader, VisibilityChange, VisibilityEditHeader,
};
use crab_metadata::git_visibility::{GitVisibilityIndex, MAX_GIT_VISIBILITY_OBJECTS};

pub(super) enum Reconciliation {
    Complete(GitVisibilityIndex),
    Paged(Box<Visibility>),
}

impl Reconciliation {
    pub(super) async fn ready(
        &self,
        control: &CapsuleControl,
        refs: &BTreeMap<String, String>,
    ) -> Result<bool> {
        match self {
            Self::Complete(index) => control_is_ready(control, refs, index),
            Self::Paged(index) => index.ready(control, refs).await,
        }
    }
    pub(super) async fn apply(&mut self, control: &CapsuleControl) -> Result<()> {
        match self {
            Self::Complete(index) if control.has_visibility() => {
                apply_capsule_control_visibility_index(control, index)
            }
            Self::Complete(_) => Ok(()),
            Self::Paged(index) => index.apply(control).await,
        }
    }
}

struct Layer {
    parent: Option<usize>,
    initial_ref: Option<String>,
    edit: Option<(Arc<CapsuleVisibilityReader>, String, VisibilityEditHeader)>,
}

pub(super) struct Visibility {
    store: Store,
    initial: GitVisibilityIndex,
    layers: Vec<Layer>,
    refs: BTreeMap<String, usize>,
    dictionary: BTreeSet<[u8; 20]>,
    dictionary_roots: Vec<(Arc<CapsuleVisibilityReader>, String, VisibilityEditHeader)>,
    dictionary_count: u64,
}

fn oid(value: &str) -> Result<[u8; 20]> {
    ObjectId::from_hex(value.as_bytes())
        .map_err(|_| corrupt_path("capsule Git visibility", "invalid object identity"))?
        .as_bytes()
        .try_into()
        .map_err(|_| corrupt_path("capsule Git visibility", "object format is unsupported"))
}

impl Visibility {
    pub(super) fn new(
        store: Store,
        initial: GitVisibilityIndex,
        names: impl Iterator<Item = String>,
    ) -> Self {
        let mut layers = Vec::new();
        let mut refs = BTreeMap::new();
        for name in names.filter(|name| initial.contains_ref(name)) {
            refs.insert(name.clone(), layers.len());
            layers.push(Layer {
                parent: None,
                initial_ref: Some(name),
                edit: None,
            });
        }
        let dictionary_count = initial.objects().len() as u64;
        Self {
            store,
            initial,
            layers,
            refs,
            dictionary: BTreeSet::new(),
            dictionary_roots: Vec::new(),
            dictionary_count,
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
                for position in pending {
                    result[position] = self.initial.contains_in_ref(name, &oids[position]);
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

    async fn dictionary_contains(&self, oids: &[[u8; 20]]) -> Result<Vec<bool>> {
        let mut result = oids
            .iter()
            .map(|oid| {
                self.initial.objects().binary_search(oid).is_ok() || self.dictionary.contains(oid)
            })
            .collect::<Vec<_>>();
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
                .lookup(&self.store, name, &keys)
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

    pub(super) async fn ready(
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
                && self.first_ref(oid(old)?).await?.is_none()
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) async fn apply(&mut self, control: &CapsuleControl) -> Result<()> {
        let Some((reader, headers)) = self.headers(control).await? else {
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
                parent = self.first_ref(oid(old)?).await?;
            }
            if let Some(old) = header.old_oid()
                && !self.contains(parent, &[oid(old)?]).await?[0]
            {
                return Err(corrupt_path(
                    "capsule Git visibility",
                    "visibility delta prior closure does not contain its old ref tip",
                ));
            }
            let new = oid(header.new_oid())?;
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
                    .objects(&self.store, edit.ref_name(), VisibilityChange::Added)
                    .await?;
                let present = self.dictionary_contains(&added).await?;
                for (object, present) in added.into_iter().zip(present) {
                    if !present {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crab_metadata::capsule_protocol::{
        CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
        CapsuleVisibilityDelta,
    };
    use crab_metadata::git_visibility::GitVisibilityEdit;
    use object_store::memory::InMemory;

    fn hex(value: u8) -> String {
        format!("{value:040x}")
    }
    fn object(value: u8) -> [u8; 20] {
        let mut oid = [0; 20];
        oid[19] = value;
        oid
    }
    async fn control(
        layout: &StoreLayout<Store>,
        name: &str,
        expected: Option<u8>,
        edit: GitVisibilityEdit,
    ) -> CapsuleControl {
        let transaction = CapsuleTransaction::new(
            &"1".repeat(64),
            vec![CapsuleRefEdit::new(
                name,
                expected.map(hex),
                Some(edit.new_oid.clone()),
                None,
            )],
        )
        .unwrap();
        let delta = CapsuleVisibilityDelta::new(BTreeMap::from([(name.to_owned(), edit)])).unwrap();
        let capsule = Capsule::build(
            &transaction,
            Vec::new(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                delta.encode().unwrap(),
            )],
        )
        .unwrap();
        let run = CapsuleRun::leaf(capsule).unwrap();
        layout
            .store()
            .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let pointer = CapsulePointer::new(
            run.hash(),
            run.bytes().len() as u64,
            run.control_offset(),
            run.control_size(),
            run.footer_hash(),
            run.level(),
            run.transaction_ids(),
            run.newest_base_root_digest(),
        )
        .unwrap();
        let (_, mut controls) =
            crab_metadata::capsule_protocol::load_capsule_run_git_control(layout, &pointer)
                .await
                .unwrap();
        controls.remove(0)
    }
    fn initial() -> GitVisibilityIndex {
        GitVisibilityIndex::new(0, "0".repeat(64), "0".repeat(64), BTreeMap::new()).unwrap()
    }

    #[tokio::test]
    async fn initial_replacement_opens_without_materializing_its_oid_set() {
        use crab_metadata::capsule_protocol::{RepositoryRoot, RootRecord};
        use std::sync::atomic::{AtomicU64, Ordering};
        let bytes = Arc::new(AtomicU64::new(0));
        let observed = bytes.clone();
        let store =
            Store::new(Arc::new(InMemory::new())).with_read_byte_observer(Arc::new(move |size| {
                observed.fetch_add(size, Ordering::Relaxed);
            }));
        let layout = StoreLayout::new(store, "repositories/large-root".into());
        let initial = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        crab_metadata::capsule_protocol::create_root(&layout, initial.clone())
            .await
            .unwrap();
        let objects = (0u32..10000)
            .map(|index| format!("{index:040x}"))
            .collect::<Vec<_>>();
        let tip = objects[9000].clone();
        let transaction = CapsuleTransaction::new(
            initial.digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some(tip.clone()),
                None,
            )],
        )
        .unwrap();
        let delta = CapsuleVisibilityDelta::new(BTreeMap::from([(
            "refs/heads/main".into(),
            GitVisibilityEdit::from_replacement_objects(None, tip.clone(), objects),
        )]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            Vec::new(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                delta.encode().unwrap(),
            )],
        )
        .unwrap();
        let run = CapsuleRun::leaf(capsule).unwrap();
        layout
            .store()
            .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let pointer = CapsulePointer::new(
            run.hash(),
            run.bytes().len() as u64,
            run.control_offset(),
            run.control_size(),
            run.footer_hash(),
            run.level(),
            run.transaction_ids(),
            run.newest_base_root_digest(),
        )
        .unwrap();
        let next = RootRecord::encode(
            initial
                .root()
                .advance(
                    initial.digest(),
                    BTreeMap::from([("refs/heads/main".into(), tip)]),
                    BTreeMap::new(),
                    vec![pointer],
                    transaction.id().unwrap().as_str(),
                )
                .unwrap(),
        )
        .unwrap();
        layout
            .store()
            .put_overwrite(&layout.capsule_root_path(), next.bytes().clone())
            .await
            .unwrap();
        let root = crab_metadata::capsule_protocol::load_root(&layout)
            .await
            .unwrap();
        bytes.store(0, Ordering::Relaxed);
        let view = open_git_view_from_root(
            &layout,
            root,
            CapsuleReadLimits {
                max_capsule_bytes: 1024 * 1024,
                max_frontier_bytes: 1024 * 1024,
            },
        )
        .await
        .unwrap();
        assert!(
            bytes.load(Ordering::Relaxed) < 64 * 1024
                && view.capsule_controls[0].visibility_delta().is_err()
        );
    }

    #[tokio::test]
    async fn layered_closures_match_complete_edits_and_preserve_captured_bases() {
        let layout = StoreLayout::new(
            Store::new(Arc::new(InMemory::new())),
            "repositories/layers".into(),
        );
        let mut full = initial();
        let mut lazy = Visibility::new(layout.store().clone(), full.clone(), std::iter::empty());
        let edits = [
            (
                "refs/heads/main",
                None,
                GitVisibilityEdit::from_replacement_objects(
                    None,
                    hex(1),
                    vec![hex(1), hex(2), hex(3)],
                ),
            ),
            (
                "refs/heads/feature",
                None,
                GitVisibilityEdit::from_delta_objects(
                    Some(hex(1)),
                    hex(2),
                    vec![hex(4)],
                    vec![hex(3)],
                ),
            ),
            (
                "refs/heads/main",
                Some(1),
                GitVisibilityEdit::from_delta_objects(
                    Some(hex(1)),
                    hex(5),
                    vec![hex(5)],
                    vec![hex(3)],
                ),
            ),
        ];
        for (name, expected, edit) in edits {
            let control = control(&layout, name, expected, edit.clone()).await;
            full.apply_ref_edit(name.to_owned(), &edit).unwrap();
            lazy.apply(&control).await.unwrap();
        }
        let requested = (1..7).map(object).collect::<Vec<_>>();
        let mut actual = BTreeMap::new();
        for (name, index) in &lazy.refs {
            actual.insert(
                name.clone(),
                lazy.contains(Some(*index), &requested).await.unwrap(),
            );
        }
        let expected = lazy
            .refs
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    requested
                        .iter()
                        .map(|oid| full.contains_in_ref(name, oid))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            (actual, lazy.dictionary_count),
            (expected, full.objects().len() as u64)
        );
    }

    #[tokio::test]
    async fn removed_invisible_objects_do_not_supply_new_ref_bases() {
        let layout = StoreLayout::new(
            Store::new(Arc::new(InMemory::new())),
            "repositories/invisible".into(),
        );
        let mut lazy = Visibility::new(layout.store().clone(), initial(), std::iter::empty());
        let create = control(
            &layout,
            "refs/heads/main",
            None,
            GitVisibilityEdit::from_replacement_objects(None, hex(1), vec![hex(1), hex(3)]),
        )
        .await;
        lazy.apply(&create).await.unwrap();
        let remove = control(
            &layout,
            "refs/heads/main",
            Some(1),
            GitVisibilityEdit::from_delta_objects(Some(hex(1)), hex(2), vec![hex(2)], vec![hex(3)]),
        )
        .await;
        lazy.apply(&remove).await.unwrap();
        let branch = control(
            &layout,
            "refs/heads/feature",
            None,
            GitVisibilityEdit::from_delta_objects(Some(hex(3)), hex(4), vec![hex(4)], vec![]),
        )
        .await;
        assert_eq!(
            (
                lazy.dictionary_contains(&[object(3)]).await.unwrap(),
                lazy.ready(
                    &branch,
                    &BTreeMap::from([("refs/heads/main".into(), hex(2))])
                )
                .await
                .unwrap()
            ),
            (vec![true], false)
        );
    }

    #[tokio::test]
    async fn removal_outside_captured_prior_closure_is_rejected() {
        let layout = StoreLayout::new(
            Store::new(Arc::new(InMemory::new())),
            "repositories/bad-removal".into(),
        );
        let mut lazy = Visibility::new(layout.store().clone(), initial(), std::iter::empty());
        let create = control(
            &layout,
            "refs/heads/main",
            None,
            GitVisibilityEdit::from_replacement_objects(None, hex(1), vec![hex(1)]),
        )
        .await;
        lazy.apply(&create).await.unwrap();
        let bad = control(
            &layout,
            "refs/heads/main",
            Some(1),
            GitVisibilityEdit::from_delta_objects(Some(hex(1)), hex(2), vec![hex(2)], vec![hex(9)]),
        )
        .await;
        assert!(lazy.apply(&bad).await.is_err());
    }
}
