use super::*;

#[cfg(feature = "storage")]
fn cache() -> RefHeadCache {
    let heads = ["main", "other"].map(|name| {
        crate::capsule_protocol::CapsuleRefHead::from_root(
            &format!("refs/heads/{name}"),
            "1".repeat(64),
            None,
            None,
        )
        .unwrap()
    });
    RefHeadCache::capture(
        "2".repeat(64),
        &"1".repeat(64),
        &"1".repeat(64),
        heads.iter().map(|head| (head, Some("opaque"), None)),
    )
    .unwrap()
    .unwrap()
}

#[test]
fn checkpoint_rejects_retired_and_relabelled_trailer_versions() {
    let checkpoint = LayeredCheckpoint::build(
        3,
        &"a".repeat(64),
        vec![
            PackLayer::build(&pack())
                .unwrap()
                .source_descriptor()
                .unwrap(),
        ],
        PointerCatalog::new(),
        None,
    )
    .unwrap();
    for (version, magic) in [
        (0, CHECKPOINT_MAGIC),
        (9, CHECKPOINT_MAGIC),
        (11, CHECKPOINT_MAGIC),
        (9, b"CRBCKP09"),
        (11, b"CRBCKP11"),
        (CHECKPOINT_VERSION, b"CRBCKP09"),
        (CHECKPOINT_VERSION, b"CRBCKP11"),
    ] {
        let mut footer = checkpoint.footer.clone();
        footer.version = version;
        let footer_bytes = serde_json::to_vec(&footer).unwrap();
        let mut bytes = footer_bytes.clone();
        bytes.extend_from_slice(&(footer_bytes.len() as u64).to_be_bytes());
        bytes.extend_from_slice(blake3::hash(&footer_bytes).as_bytes());
        bytes.extend_from_slice(magic);
        assert!(
            LayeredCheckpoint::decode(Bytes::from(bytes)).is_err(),
            "{version} / {magic:?}"
        );
    }
}

#[cfg(feature = "storage")]
#[test]
fn ref_head_cache_is_omitted_when_it_would_exceed_the_publication_budget() {
    let cache = cache();
    let source = PackLayer::build(&pack())
        .unwrap()
        .source_descriptor()
        .unwrap();
    let original = LayeredCheckpoint::build(
        3,
        &"a".repeat(64),
        vec![source.clone()],
        PointerCatalog::new(),
        None,
    )
    .unwrap();
    let limited = LayeredCheckpoint::build_with_catalog(
        3,
        &"a".repeat(64),
        vec![source],
        CatalogInput::New(PointerCatalog::new()),
        None,
        Some(&cache),
        original.publication_metadata_bytes().unwrap(),
    )
    .unwrap();
    assert_eq!(limited.bytes(), original.bytes());
}

#[cfg(feature = "storage")]
#[test]
fn checkpoint_cache_is_bound_in_both_pointer_and_footer() {
    let cache = cache();
    let checkpoint = LayeredCheckpoint::build_with_catalog(
        3,
        &"a".repeat(64),
        vec![
            PackLayer::build(&pack())
                .unwrap()
                .source_descriptor()
                .unwrap(),
        ],
        CatalogInput::New(PointerCatalog::new()),
        None,
        Some(&cache),
        u64::MAX,
    )
    .unwrap();
    let control = LayeredCheckpoint::decode_control(
        checkpoint
            .bytes()
            .slice(checkpoint.control_offset() as usize..),
        checkpoint.bytes().len() as u64,
        checkpoint.control_offset(),
        checkpoint.hash(),
        checkpoint.footer_hash(),
    )
    .unwrap();
    let pointer = checkpoint.pointer().unwrap();
    assert!(control.matches_pointer(&pointer).unwrap());
    for field in ["origin", "offset", "length", "blake3", "absent"] {
        let mut changed = serde_json::to_value(&pointer).unwrap();
        match field {
            "origin" => changed["ref_heads"][field] = "3".repeat(64).into(),
            "offset" | "length" => changed["ref_heads"]["range"][field] = 1.into(),
            "blake3" => changed["ref_heads"]["range"][field] = "3".repeat(64).into(),
            _ => {
                changed.as_object_mut().unwrap().remove("ref_heads");
            }
        }
        let changed = serde_json::from_value(changed).unwrap();
        assert!(!control.matches_pointer(&changed).unwrap(), "{field}");
    }
}
