use super::*;
use crate::git_object_locator::GitObjectMetadata;
use object_store::{ObjectStoreExt, memory::InMemory};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

fn oid(index: u32) -> [u8; 20] {
    let mut oid = [1; 20];
    oid[..4].copy_from_slice(&index.to_be_bytes());
    oid
}
type Inputs = (
    Manifest,
    Vec<GitPackInventoryEntry>,
    Vec<([u8; 20], GitObjectLocator)>,
);

fn inputs(count: u32) -> Inputs {
    let mut manifest = Manifest::default_for_repo("refs/heads/main");
    manifest.generation = 1;
    manifest.git_validation_digest = "b".repeat(64);
    let pack_id = MerkleHash::from([3; 32]);
    let inventory = vec![GitPackInventoryEntry {
        pack_id,
        object_count: u64::from(count),
        pack_size: 32 + u64::from(count) * 10,
    }];
    manifest.pack_index_hash = crate::manifests::compact_pack_index(
        1,
        &[crate::manifests::PackManifestEntry {
            pack_id: pack_id.to_string(),
            size: inventory[0].pack_size,
            content_hash: pack_id.to_string(),
            ref_tips: Vec::new(),
            object_count: u64::from(count),
        }],
    )
    .unwrap()
    .0;
    let entries = (0..count)
        .map(|index| {
            (
                oid(index),
                GitObjectLocator {
                    ordinal: 0,
                    pack_id,
                    location: GitObjectLocation {
                        pack_offset: 12 + u64::from(index) * 10,
                        entry_len: 10,
                        crc32: index,
                    },
                    metadata: GitObjectMetadata::default(),
                },
            )
        })
        .collect();
    (manifest, inventory, entries)
}

#[tokio::test]
async fn point_lookup_and_delta_offsets_are_bounded_and_preserve_duplicate_positions() {
    let (manifest, inventory, entries) = inputs(10_000);
    let write =
        PagedGitObjectIndexWrite::build(&"c".repeat(64), &manifest, &inventory, &entries).unwrap();
    let bytes = Arc::new(AtomicU64::new(0));
    let observed = bytes.clone();
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_byte_observer(Arc::new(move |read| {
            observed.fetch_add(read, Ordering::Relaxed);
        }));
    let layout = StoreLayout::new(store.clone(), "paged-object-index".into());
    let hash = write.upload(&store, &layout).await.unwrap();
    bytes.store(0, Ordering::Relaxed);
    let reader = PagedGitObjectIndex::load(
        &store,
        &layout,
        &hash,
        &"c".repeat(64),
        &manifest,
        256 * 1024,
    )
    .await
    .unwrap();
    assert_eq!(bytes.swap(0, Ordering::Relaxed), reader.descriptor_bytes);
    let inventory = inventory
        .into_iter()
        .map(|pack| (pack.pack_id, pack))
        .collect();
    let requested = [oid(9999), oid(0), oid(9999), oid(20_000)];
    let expected = vec![
        Some(entries[9999].1),
        Some(entries[0].1),
        Some(entries[9999].1),
        None,
    ];
    assert_eq!(
        reader.lookup(&store, &requested, &inventory).await.unwrap(),
        expected
    );
    let cold = bytes.swap(0, Ordering::Relaxed);
    assert!(cold <= 128 * 1024 && cold < write.body.len() as u64);
    assert_eq!(
        reader.lookup(&store, &requested, &inventory).await.unwrap(),
        expected
    );
    assert_eq!(bytes.load(Ordering::Relaxed), 0);
    let offsets = [entries[9999].1.location.pack_offset, 12, 12, 13]
        .map(|offset| (entries[0].1.pack_id, offset));
    assert_eq!(
        reader
            .oids_at_offsets(&store, &offsets, &inventory)
            .await
            .unwrap(),
        vec![Some(oid(9999)), Some(oid(0)), Some(oid(0)), None]
    );
    reader
        .verify_complete(&store, &manifest, write.body.len() as u64)
        .await
        .unwrap();
}

#[test]
fn construction_rejects_incomplete_or_overlapping_physical_coverage() {
    let (manifest, inventory, entries) = inputs(3);
    for case in [0, 1, 2] {
        let mut entries = entries.clone();
        match case {
            0 => {
                entries.remove(1);
            }
            1 => {
                entries[1].1.location.pack_offset = 12;
            }
            _ => {
                entries[2].1.location.entry_len = u64::MAX;
            }
        }
        assert!(
            PagedGitObjectIndexWrite::build(&"c".repeat(64), &manifest, &inventory, &entries)
                .is_err()
        );
    }
}

#[tokio::test]
async fn corrupt_required_pages_never_become_an_absent_oid() {
    let (manifest, inventory, entries) = inputs(1000);
    let write =
        PagedGitObjectIndexWrite::build(&"c".repeat(64), &manifest, &inventory, &entries).unwrap();
    let backend = Arc::new(InMemory::new());
    let store = Store::new(backend.clone());
    let layout = StoreLayout::new(store.clone(), "corrupt-object-index".into());
    let hash = write.upload(&store, &layout).await.unwrap();
    let reader = PagedGitObjectIndex::load(
        &store,
        &layout,
        &hash,
        &"c".repeat(64),
        &manifest,
        256 * 1024,
    )
    .await
    .unwrap();
    let mut body = write.body.to_vec();
    body[reader.descriptor.objects.offset as usize] ^= 1;
    backend
        .put(&reader.path, Bytes::from(body).into())
        .await
        .unwrap();
    let inventory = inventory
        .into_iter()
        .map(|pack| (pack.pack_id, pack))
        .collect();
    assert!(
        reader
            .lookup(&store, &[oid(20_000)], &inventory)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn warm_pages_cannot_bypass_inventory_binding_or_encoded_work_limits() {
    let (manifest, inventory, entries) = inputs(1000);
    let write =
        PagedGitObjectIndexWrite::build(&"c".repeat(64), &manifest, &inventory, &entries).unwrap();
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "bound-object-index".into());
    let hash = write.upload(&store, &layout).await.unwrap();
    let mut reader = PagedGitObjectIndex::load(
        &store,
        &layout,
        &hash,
        &"c".repeat(64),
        &manifest,
        256 * 1024,
    )
    .await
    .unwrap();
    let mut inventory: HashMap<_, _> = inventory
        .into_iter()
        .map(|pack| (pack.pack_id, pack))
        .collect();
    reader
        .lookup(&store, &[oid(999)], &inventory)
        .await
        .unwrap();
    inventory.get_mut(&entries[0].1.pack_id).unwrap().pack_size += 1;
    assert!(
        reader
            .lookup(&store, &[oid(999)], &inventory)
            .await
            .is_err()
    );
    inventory.get_mut(&entries[0].1.pack_id).unwrap().pack_size -= 1;
    reader.maximum = reader.descriptor_bytes;
    assert!(
        reader
            .lookup(&store, &[oid(999)], &inventory)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn descriptors_cannot_cross_captured_git_states() {
    let (manifest, inventory, entries) = inputs(10);
    let write =
        PagedGitObjectIndexWrite::build(&"c".repeat(64), &manifest, &inventory, &entries).unwrap();
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "state-object-index".into());
    let hash = write.upload(&store, &layout).await.unwrap();
    assert!(
        PagedGitObjectIndex::load(
            &store,
            &layout,
            &hash,
            &"d".repeat(64),
            &manifest,
            256 * 1024
        )
        .await
        .is_err()
    );
}

#[test]
fn object_aliases_prefer_full_encodings_and_keep_every_physical_offset() {
    let (mut manifest, mut inventory, mut entries) = inputs(1);
    entries[0].1.metadata.delta_base_oid = Some(oid(0));
    let mut full = entries[0].1;
    full.pack_id = MerkleHash::from([4; 32]);
    full.metadata.delta_base_oid = None;
    full.metadata.kind = Some(crate::git_object_locator::GitObjectKind::Blob);
    entries.push((oid(0), full));
    inventory.push(GitPackInventoryEntry {
        pack_id: full.pack_id,
        object_count: 1,
        pack_size: 42,
    });
    let mut packs = inventory
        .iter()
        .map(|pack| crate::manifests::PackManifestEntry {
            pack_id: pack.pack_id.to_string(),
            size: pack.pack_size,
            content_hash: pack.pack_id.to_string(),
            ref_tips: Vec::new(),
            object_count: pack.object_count,
        })
        .collect::<Vec<_>>();
    packs.sort_unstable_by(|left, right| left.pack_id.cmp(&right.pack_id));
    manifest.pack_index_hash = crate::manifests::compact_pack_index(1, &packs).unwrap().0;
    let write =
        PagedGitObjectIndexWrite::build(&"c".repeat(64), &manifest, &inventory, &entries).unwrap();
    let mut audit = BTreeMap::new();
    let rows = crate::pages::scan(
        &write.body,
        &write.descriptor.objects,
        Kind::ObjectLocators,
        "test index",
        &mut audit,
    )
    .unwrap();
    let selected = decode_locator(&rows[0].1).unwrap();
    assert_eq!((selected, write.descriptor.offsets.count), (full, 2));
}

proptest::proptest! {
    #[test]
    fn physical_input_order_does_not_change_authenticated_index_bytes(count in 1u32..1000, rotation in 0usize..1000) {
        let (manifest, inventory, mut entries) = inputs(count);
        let first = PagedGitObjectIndexWrite::build(&"c".repeat(64), &manifest, &inventory, &entries).unwrap();
        let rotation = rotation % entries.len();
        entries.rotate_left(rotation);
        entries.reverse();
        let second = PagedGitObjectIndexWrite::build(&"c".repeat(64), &manifest, &inventory, &entries).unwrap();
        proptest::prop_assert_eq!((first.descriptor, first.body), (second.descriptor, second.body));
    }
}

#[tokio::test]
async fn resigned_oid_locations_must_agree_with_the_physical_offset_directory() {
    let (manifest, inventory, entries) = inputs(10);
    let mut write =
        PagedGitObjectIndexWrite::build(&"c".repeat(64), &manifest, &inventory, &entries).unwrap();
    let mut audit = BTreeMap::new();
    let mut objects = crate::pages::scan(
        &write.body,
        &write.descriptor.objects,
        Kind::ObjectLocators,
        "test index",
        &mut audit,
    )
    .unwrap();
    let offsets = crate::pages::scan(
        &write.body,
        &write.descriptor.offsets,
        Kind::PackOffsets,
        "test index",
        &mut audit,
    )
    .unwrap();
    let packs = crate::pages::scan(
        &write.body,
        &write.descriptor.packs,
        Kind::PackInventory,
        "test index",
        &mut audit,
    )
    .unwrap();
    let mut forged = entries[0].1;
    forged.location.pack_offset = entries[1].1.location.pack_offset;
    objects[0].1 = encode_locator(forged);
    let (body, objects, offsets, packs) = encode_trees(objects, offsets, packs).unwrap();
    write.descriptor.body_hash = blake3::hash(&body).to_hex().to_string();
    write.descriptor.body_bytes = body.len() as u64;
    write.descriptor.objects = objects;
    write.descriptor.offsets = offsets;
    write.descriptor.packs = packs;
    write.body = body.into();
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "forged-object-index".into());
    let hash = write.upload(&store, &layout).await.unwrap();
    let reader = PagedGitObjectIndex::load(
        &store,
        &layout,
        &hash,
        &"c".repeat(64),
        &manifest,
        256 * 1024,
    )
    .await
    .unwrap();
    let inventory = inventory
        .into_iter()
        .map(|pack| (pack.pack_id, pack))
        .collect();
    assert_eq!(
        (
            reader.lookup(&store, &[oid(0)], &inventory).await.is_err(),
            reader
                .verify_complete(&store, &manifest, write.body.len() as u64)
                .await
                .is_err()
        ),
        (true, true)
    );
}

#[test]
fn generation_zero_ref_head_snapshots_can_build_physical_indexes() {
    let (mut manifest, inventory, entries) = inputs(1);
    manifest.generation = 0;
    let pack = inventory[0];
    manifest.pack_index_hash = crate::manifests::compact_pack_index(
        0,
        &[crate::manifests::PackManifestEntry {
            pack_id: pack.pack_id.to_string(),
            size: pack.pack_size,
            content_hash: pack.pack_id.to_string(),
            ref_tips: Vec::new(),
            object_count: pack.object_count,
        }],
    )
    .unwrap()
    .0;
    assert!(
        PagedGitObjectIndexWrite::build(&"c".repeat(64), &manifest, &inventory, &entries).is_ok()
    );
}

#[tokio::test]
async fn physical_index_attachment_binds_origin_prefixes_and_captured_state() {
    let (manifest, inventory, entries) = inputs(10);
    let state = "c".repeat(64);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "bound-origin".to_owned());
    let write = PagedGitObjectIndexWrite::build(&state, &manifest, &inventory, &entries).unwrap();
    let hash = write.upload(&store, &layout).await.unwrap();
    let reader = PagedGitObjectIndex::load(&store, &layout, &hash, &state, &manifest, 128 * 1024)
        .await
        .unwrap();
    let foreign = StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "bound-origin".to_owned(),
    );
    let prefix = StoreLayout::new(store.clone(), "other-repository".to_owned());
    let global = StoreLayout::with_global_prefix(
        store,
        "bound-origin".to_owned(),
        "other-global".to_owned(),
    );
    assert_eq!(
        [
            reader.matches_binding(&layout, &state, &manifest),
            reader.matches_binding(&foreign, &state, &manifest),
            reader.matches_binding(&prefix, &state, &manifest),
            reader.matches_binding(&global, &state, &manifest),
            reader.matches_binding(&layout, &"d".repeat(64), &manifest)
        ],
        [true, false, false, false, false]
    );
}
