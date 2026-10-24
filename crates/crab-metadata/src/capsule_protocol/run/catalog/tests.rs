use super::*;
use crate::capsule_protocol::{
    CapsuleRefEdit, CapsuleSection, FileCatalogEntry, ShardCatalogEntry, XorbCatalogEntry,
    XorbChunkEntry,
};

fn catalog(shard: &str, extra: bool) -> PointerCatalog {
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_xorb(
            "8".repeat(64),
            XorbCatalogEntry::new(
                99,
                "a".repeat(64),
                vec![XorbChunkEntry::new("9".repeat(64), 7)],
            ),
        )
        .unwrap();
    catalog
        .insert_shard(shard, ShardCatalogEntry::new(80, vec!["8".repeat(64)]))
        .unwrap();
    catalog
        .insert_file("b".repeat(64), FileCatalogEntry::new(7, shard))
        .unwrap();
    if extra {
        catalog
            .insert_xorb(
                "1".repeat(64),
                XorbCatalogEntry::new(
                    100,
                    "c".repeat(64),
                    vec![XorbChunkEntry::new("2".repeat(64), 8)],
                ),
            )
            .unwrap();
    }
    catalog
}

fn capsule(position: usize, catalog: Option<PointerCatalog>) -> Capsule {
    let transaction = CapsuleTransaction::new(
        &"e".repeat(64),
        vec![CapsuleRefEdit::new(
            format!("refs/tags/{position}"),
            None,
            Some("f".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let sections = catalog
        .into_iter()
        .map(|catalog| {
            CapsuleSection::new(
                CapsuleSectionKind::CatalogDelta,
                catalog.encode_delta().unwrap(),
            )
        })
        .collect();
    Capsule::build(&transaction, Vec::new(), sections).unwrap()
}

fn rows(bytes: &Bytes, descriptor: &CatalogPages, position: usize) -> Rows {
    crate::pages::scan(
        &bytes[..descriptor.body_bytes as usize],
        descriptor.roots[position].as_ref().unwrap(),
        KINDS[position],
        "run catalog",
        &mut BTreeMap::new(),
    )
    .unwrap()
}

#[test]
fn relocated_equal_descriptors_share_one_variant_and_retain_every_occurrence() {
    let capsules = vec![
        capsule(0, Some(catalog(&"d".repeat(64), false))),
        capsule(1, Some(catalog(&"d".repeat(64), true))),
    ];
    let (bytes, descriptor) = CatalogPages::build(&capsules).unwrap().unwrap();
    descriptor.verify(&bytes, &capsules).unwrap();
    let values = rows(&bytes, &descriptor, 2);
    let value = &values.iter().find(|(key, _)| key == &[0x88; 32]).unwrap().1;
    let variants = decode_variants(value, 2).unwrap();
    assert_eq!(
        variants
            .iter()
            .map(|variant| (variant.source, variant.members.as_slice()))
            .collect::<Vec<_>>(),
        vec![(0, &[0, 1][..])]
    );
}

#[test]
fn conflicting_file_bindings_keep_disjoint_capsule_membership() {
    let capsules = vec![
        capsule(0, Some(catalog(&"d".repeat(64), false))),
        capsule(1, Some(catalog(&"e".repeat(64), false))),
    ];
    let (bytes, descriptor) = CatalogPages::build(&capsules).unwrap().unwrap();
    descriptor.verify(&bytes, &capsules).unwrap();
    let values = rows(&bytes, &descriptor, 0);
    let variants = decode_variants(&values[0].1, 2).unwrap();
    let members = variants
        .into_iter()
        .map(|variant| variant.members)
        .collect::<BTreeSet<_>>();
    assert_eq!(members, BTreeSet::from([vec![0], vec![1]]));
}

#[test]
fn catalog_page_roots_and_full_run_agree_after_compaction() {
    let runs = (0..3)
        .map(|position| {
            CapsuleRun::leaf(capsule(
                position,
                Some(catalog(&"d".repeat(64), position > 0)),
            ))
            .unwrap()
        })
        .collect();
    let run = CapsuleRun::compact(runs).unwrap();
    assert_eq!(CapsuleRun::decode(run.bytes().clone()).unwrap(), run);
    let control = CapsuleRunControl::decode_suffix(
        run.bytes().slice(run.control_offset() as usize..),
        run.bytes().len() as u64,
        run.hash(),
        run.level(),
        &run.transaction_ids(),
        run.newest_base_root_digest(),
    )
    .unwrap();
    assert_eq!(control.capsule_locations().len(), 3);
}

#[test]
fn catalog_regions_preserve_index_pool_and_admission_boundaries() {
    let runs = (0..2)
        .map(|position| {
            let metadata = catalog(&"d".repeat(64), false);
            let transaction = capsule(position, None).transaction().unwrap();
            let pack = crate::capsule_protocol::CapsuleGitPack::new(
                Bytes::from_static(b"pack"),
                Bytes::from_static(b"index"),
                Bytes::from_static(b"reverse"),
                Bytes::from_static(b"locator"),
                "1".repeat(40),
                1,
            )
            .unwrap();
            let capsule = Capsule::build(
                &transaction,
                vec![pack],
                vec![CapsuleSection::new(
                    CapsuleSectionKind::CatalogDelta,
                    metadata.encode_delta().unwrap(),
                )],
            )
            .unwrap();
            CapsuleRun::leaf_with_member_oids(capsule, vec![vec![[position as u8; 20]]]).unwrap()
        })
        .collect();
    let run = CapsuleRun::compact(runs).unwrap();
    let (control, _) = run.verified_controls().unwrap();
    assert_eq!(control.admission().unwrap().entries().count(), 2);
}

#[test]
fn omitted_catalog_roots_cannot_hide_nonempty_capsule_data() {
    let run = CapsuleRun::leaf(capsule(0, Some(catalog(&"d".repeat(64), false)))).unwrap();
    assert!(CatalogPages::validate_presence(None, &run.footer.capsules).is_err());
}

#[test]
fn empty_catalog_sections_do_not_require_a_run_index() {
    let run = CapsuleRun::compact(vec![
        CapsuleRun::leaf(capsule(0, Some(PointerCatalog::new()))).unwrap(),
        CapsuleRun::leaf(capsule(1, None)).unwrap(),
    ])
    .unwrap();
    assert!(run.footer.catalog.is_none());
}

#[test]
fn authenticated_occurrence_changes_still_fail_full_capsule_binding() {
    let capsules = vec![
        capsule(0, Some(catalog(&"d".repeat(64), false))),
        capsule(1, None),
    ];
    let (bytes, mut descriptor) = CatalogPages::build(&capsules).unwrap().unwrap();
    let mut body = bytes[..descriptor.body_bytes as usize].to_vec();
    let root = descriptor.roots[0].as_mut().unwrap();
    // Keep the row structurally valid while falsely attributing it to capsule 1.
    let value = root.offset as usize + 14 + 32 + 4;
    body[value + 2 + 32] = 1;
    body[value + 2 + 32 + 2 + 2] = 1;
    root.hash =
        *blake3::hash(&body[root.offset as usize..root.offset as usize + root.bytes as usize])
            .as_bytes();
    descriptor.body_hash = *blake3::hash(&body).as_bytes();
    let header = json(&descriptor).unwrap();
    body.extend_from_slice(&header);
    crate::pages::put_u32(&mut body, header.len()).unwrap();
    body.extend_from_slice(MAGIC);
    let error = descriptor.verify(&body, &capsules).unwrap_err();
    assert!(
        matches!(error, MetadataError::CorruptObject { reason, .. } if reason.contains("nested capsules"))
    );
}

#[cfg(feature = "storage")]
#[tokio::test]
async fn selected_run_reads_reject_corrupt_index_pages_and_source_recipes() {
    use crate::capsule_protocol::PointerCatalogLookup;
    use crab_storage::{Store, StoreLayout};
    use std::sync::Arc;
    for index_page in [false, true] {
        let run = CapsuleRun::leaf(capsule(0, Some(catalog(&"d".repeat(64), false)))).unwrap();
        let layout = StoreLayout::new(
            Store::new(Arc::new(object_store::memory::InMemory::new())),
            "run-corruption".into(),
        );
        let source = run.catalog_source(&layout).unwrap().unwrap();
        let lookup = PointerCatalogLookup::empty(1024 * 1024)
            .with_run_sources(vec![source], &[run.capsules()[0].hash().to_owned()])
            .unwrap();
        let mut bytes = run.bytes().to_vec();
        let offset = if index_page {
            run.footer.catalog.as_ref().unwrap().offset
                + run.footer.catalog_pages.as_ref().unwrap().roots[0]
                    .as_ref()
                    .unwrap()
                    .offset
        } else {
            // Nested chunk recipes remain range-addressed in their original
            // catalog body; the aggregate cannot bypass their page hashes.
            let section = run.footer.capsules[0].catalog.as_ref().unwrap();
            section.offset
        };
        bytes[offset as usize] ^= 1;
        layout
            .store()
            .put_overwrite(&layout.capsule_path(run.hash()), bytes.into())
            .await
            .unwrap();
        assert!(
            lookup
                .files(layout.store(), &["b".repeat(64)])
                .await
                .is_err()
        );
    }
}

#[cfg(feature = "storage")]
#[tokio::test]
async fn catalog_control_identity_is_independent_of_materialization() {
    use crab_storage::{Store, StoreLayout};
    use std::sync::Arc;
    let expected = catalog(&"d".repeat(64), false);
    let run = CapsuleRun::leaf(capsule(0, Some(expected.clone()))).unwrap();
    let layout = StoreLayout::new(
        Store::new(Arc::new(object_store::memory::InMemory::new())),
        "catalog-identity".into(),
    );
    layout
        .store()
        .put_overwrite(&layout.capsule_path(run.hash()), run.bytes().clone())
        .await
        .unwrap();
    let (control, full) = run.verified_controls().unwrap();
    let paged = control
        .materialize_capsules_with_paged_visibility(&layout)
        .unwrap();
    assert_eq!(paged, full);
    assert_eq!(
        paged[0].materialize_catalog(layout.store()).await.unwrap(),
        Some(expected)
    );
    let mut conflicting = paged[0].clone();
    conflicting.catalog_identity.as_mut().unwrap().1 = "f".repeat(64);
    assert_ne!(conflicting, full[0]);
}
