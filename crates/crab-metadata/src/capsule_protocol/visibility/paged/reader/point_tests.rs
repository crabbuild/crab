use super::*;
use crate::capsule_protocol::{
    Capsule, CapsuleRefEdit, CapsuleRun, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
    PackRange,
};
use std::sync::Arc;

fn singleton_delta() -> CapsuleVisibilityDelta {
    CapsuleVisibilityDelta::new(BTreeMap::from([(
        "refs/heads/main".to_owned(),
        GitVisibilityEdit::from_replacement_objects(
            None,
            format!("{:040x}", 5000),
            (0..10_000).map(|value| format!("{value:040x}")).collect(),
        ),
    )]))
    .unwrap()
}

#[test]
fn relabeling_singleton_geometry_does_not_reinterpret_immutable_bytes() {
    let delta = singleton_delta();
    for (from, to) in [
        (POINT_VERSION, SINGLE_REF_VERSION),
        (SINGLE_REF_VERSION, POINT_VERSION),
    ] {
        let bytes = encode_version(&delta, Some(from)).unwrap();
        assert_eq!(CapsuleVisibilityDelta::decode(&bytes).unwrap(), delta);
        let descriptor = Descriptor::decode(&bytes).unwrap();
        let mut writer = Writer::default();
        writer
            .bytes
            .extend_from_slice(&bytes[..descriptor.body_bytes as usize]);
        let relabeled = finish(writer, descriptor.refs, None, Some(to)).unwrap();
        assert!(CapsuleVisibilityDelta::decode(&relabeled).is_err());
    }
}

#[tokio::test]
async fn resigned_inline_roots_cannot_bypass_membership_or_routing_proofs() {
    let name = "refs/heads/main";
    let tip = format!("{:040x}", 5000);
    let delta = singleton_delta();
    let bytes = delta.encode().unwrap();
    let descriptor = Descriptor::decode(&bytes).unwrap();
    assert_eq!(descriptor.version, SINGLE_REF_VERSION);
    let body = &bytes[..descriptor.body_bytes as usize];
    let (key, value) = crate::pages::scan(
        body,
        &descriptor.refs,
        Kind::VisibilityRefs,
        "test inline header",
        &mut BTreeMap::new(),
    )
    .unwrap()
    .pop()
    .unwrap();
    let original: VisibilityEditHeader = serde_json::from_slice(&value).unwrap();
    assert!(original.inline_added_root.is_some());
    let layout = StoreLayout::new(
        Store::new(Arc::new(object_store::memory::InMemory::new())),
        "repositories/resigned-inline-roots".into(),
    );
    for fault in ["hash", "coverage", "bounds", "version"] {
        let mut row = original.clone();
        match fault {
            "hash" => row.inline_added_root.as_mut().unwrap()[0] ^= 1,
            "coverage" => row.added.as_mut().unwrap().count += 1,
            "bounds" => row.added.as_mut().unwrap().offset = descriptor.body_bytes,
            _ => {}
        }
        let mut writer = Writer::default();
        // Re-sign the selected ref page and descriptor so transport hashes cannot
        // satisfy a rejection that must come from the inline proof itself.
        writer.bytes.extend_from_slice(body);
        let refs = writer
            .tree(
                Kind::VisibilityRefs,
                vec![(key.clone(), serde_json::to_vec(&row).unwrap())],
            )
            .unwrap();
        let version = if fault == "version" {
            POINT_VERSION
        } else {
            SINGLE_REF_VERSION
        };
        let encoded = finish(writer, refs, None, Some(version)).unwrap();
        let descriptor = Descriptor::decode(&encoded).unwrap();
        let hash = blake3::hash(&encoded).to_hex().to_string();
        layout
            .store()
            .create_strict(&layout.capsule_path(&hash), encoded.clone())
            .await
            .unwrap();
        let range = PackRange::new(0, &encoded).unwrap();
        let reader = CapsuleVisibilityReader::new(
            &layout,
            &hash,
            &range,
            descriptor,
            1024 * 1024,
            Arc::new(PageCache::default()),
        )
        .unwrap();
        assert!(
            reader
                .lookup(layout.store(), name, &[decode_oid(&tip).unwrap()])
                .await
                .is_err(),
            "accepted {fault} inline proof"
        );
    }
}

#[tokio::test]
async fn scattered_large_ref_tips_bound_transfer_without_extra_requests() {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    let received = Arc::new(AtomicU64::new(0));
    let requests = Arc::new(AtomicU64::new(0));
    let observed_bytes = received.clone();
    let observed_requests = requests.clone();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_byte_observer(Arc::new(move |bytes| {
            observed_bytes.fetch_add(bytes, Relaxed);
        }))
        .with_read_request_observer(Arc::new(move |_| {
            observed_requests.fetch_add(1, Relaxed);
        }));
    let layout = StoreLayout::new(store, "repositories/scattered-ref-tips".into());
    let objects = (0_u64..40_000)
        .map(|index| blake3::hash(&index.to_le_bytes()).to_hex()[..40].to_owned())
        .collect::<Vec<_>>();
    let mut total_bytes = 0;
    let mut total_requests = 0;
    for index in 0_u64..64 {
        let name = format!("refs/heads/branch-{index}");
        let tip = blake3::hash(&(index + 40_000).to_le_bytes()).to_hex()[..40].to_owned();
        let mut closure = objects.clone();
        closure.push(tip.clone());
        let delta = CapsuleVisibilityDelta::new(BTreeMap::from([(
            name.clone(),
            GitVisibilityEdit::from_replacement_objects(None, tip.clone(), closure),
        )]))
        .unwrap();
        let transaction = CapsuleTransaction::new(
            &"1".repeat(64),
            vec![CapsuleRefEdit::new(&name, None, Some(tip.clone()), None)],
        )
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            vec![],
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                delta.encode().unwrap(),
            )],
        )
        .unwrap();
        let run = CapsuleRun::leaf(capsule.clone()).unwrap();
        layout
            .store()
            .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let (control, _) = run.verified_controls().unwrap();
        let reader = control
            .visibility_reader(&layout, capsule.hash(), 16 * 1024 * 1024)
            .unwrap()
            .unwrap();
        received.store(0, Relaxed);
        requests.store(0, Relaxed);
        assert_eq!(
            reader.headers(layout.store(), &[&name]).await.unwrap()[0]
                .as_ref()
                .unwrap()
                .new_oid(),
            tip
        );
        assert_eq!(
            reader
                .lookup(layout.store(), &name, &[decode_oid(&tip).unwrap()])
                .await
                .unwrap(),
            Some(vec![Some(VisibilityChange::Added)])
        );
        total_bytes += received.load(Relaxed);
        total_requests += requests.load(Relaxed);
        assert_eq!(
            reader
                .lookup(layout.store(), &name, &[[255; 20]])
                .await
                .unwrap(),
            Some(vec![None])
        );
    }
    assert!(
        total_bytes <= 768 * 1024 && total_requests <= 192,
        "scattered tip proofs used {total_bytes} bytes and {total_requests} requests"
    );
}
