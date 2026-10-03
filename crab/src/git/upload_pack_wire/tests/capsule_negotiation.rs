use super::*;
use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind,
    CapsuleTransaction, CapsuleVisibilityDelta,
};
use crab_metadata::git_visibility::GitVisibilityEdit;
use crab_storage::{Store, StoreLayout};
use std::collections::BTreeMap;

const LIMIT: u64 = 8 * 1024 * 1024;

async fn fixture(rewrite: bool) -> (StoreLayout<Store>, ObjectId, ObjectId) {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store, "repositories/negotiation".to_owned());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    let tree = crab_remote::objects::object_id(gix_object::Kind::Tree, b"").unwrap();
    let mut previous = None;
    let mut tips = Vec::new();
    for message in ["seed", "incremental"] {
        let parent = previous
            .filter(|_| !rewrite)
            .map_or(String::new(), |oid| format!("parent {oid}\n"));
        let commit = format!(
            "tree {tree}\n{parent}author Test <test@example.com> 1 +0000\ncommitter Test <test@example.com> 1 +0000\n\n{message}\n"
        )
        .into_bytes();
        let tip = crab_remote::objects::object_id(gix_object::Kind::Commit, &commit).unwrap();
        let mut objects = Vec::new();
        if previous.is_none() {
            objects.push((gix_object::Kind::Tree, Vec::new()));
        }
        objects.push((gix_object::Kind::Commit, commit));
        let mut pack_bytes = Vec::new();
        crab_git::pack_writer::write_pack(
            &mut pack_bytes,
            objects
                .iter()
                .map(|(kind, body)| Ok((*kind, body.len() as u64, body.as_slice()))),
            LIMIT,
            || false,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.pack");
        std::fs::write(&source, &pack_bytes).unwrap();
        let indexed = crab_git::pack::install_pack_file_from_path(
            &directory.path().join("indexed"),
            &source,
            blake3::hash(&pack_bytes).to_hex().as_ref(),
            LIMIT,
            true,
        )
        .unwrap();
        let kinds = objects.iter().map(|(kind, _)| *kind).collect::<Vec<_>>();
        let kind_bytes = crab_git::pack_locator::encode_pack_kind_metadata(
            ObjectId::from_hex(indexed.git_sha1.as_bytes()).unwrap(),
            &kinds,
        )
        .unwrap();
        let pack = CapsuleGitPack::new(
            Bytes::from(pack_bytes),
            Bytes::from(std::fs::read(indexed.idx_path).unwrap()),
            Bytes::from(std::fs::read(indexed.rev_path).unwrap()),
            Bytes::from(kind_bytes),
            indexed.git_sha1,
            objects.len() as u64,
        )
        .unwrap();
        let root = crab_write::capsule_protocol::open_root(&layout)
            .await
            .unwrap();
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                previous.map(|oid: ObjectId| oid.to_string()),
                Some(tip.to_string()),
                None,
            )],
        )
        .unwrap();
        let visibility = match previous {
            Some(old) => GitVisibilityEdit::from_delta_objects(
                Some(old.to_string()),
                tip.to_string(),
                vec![tip.to_string()],
                if rewrite {
                    vec![old.to_string()]
                } else {
                    Vec::new()
                },
            ),
            None => GitVisibilityEdit::from_replacement_objects(
                None,
                tip.to_string(),
                vec![tree.to_string(), tip.to_string()],
            ),
        };
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            "refs/heads/main".to_owned(),
            visibility,
        )]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            vec![pack],
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            )],
        )
        .unwrap();
        crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
            .await
            .unwrap();
        if previous.is_none() {
            crab_remote::checkpoint::publish_capsule_checkpoint(
                &layout,
                0,
                LIMIT,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        }
        previous = Some(tip);
        tips.push(tip);
    }
    (layout, tips[0], tips[1])
}

#[tokio::test]
async fn authenticated_transition_finishes_negotiation_without_done() {
    let (layout, seed, tip) = fixture(false).await;
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let mut request = packet(b"command=fetch\n");
    request.extend_from_slice(b"0001");
    request.extend(packet(format!("want {tip}\n").as_bytes()));
    request.extend(packet(format!("have {seed}\n").as_bytes()));
    request.extend_from_slice(b"0000");
    let runtime = Arc::new(RemoteGitRuntime::default());
    let mut input = BufReader::new(Cursor::new(request));
    let mut output = Vec::new();
    let cancellation = CancellationToken::new();
    let result = serve(
        &mut input,
        &mut output,
        layout.store(),
        layout.repo_prefix(),
        &[],
        &FetchAdmissionPolicy::default(),
        false,
        Some(root),
        &runtime,
        &cancellation,
    )
    .await;
    runtime.shutdown().await;
    result.unwrap();
    let mut response = BufReader::new(Cursor::new(&output[1..]));
    while read_packet(&mut response, &cancellation).await.unwrap() != Packet::Flush {}
    assert_eq!(
        read_packet(&mut response, &cancellation).await.unwrap(),
        Packet::Data(b"acknowledgments\n".to_vec())
    );
    assert_eq!(
        read_packet(&mut response, &cancellation).await.unwrap(),
        Packet::Data(b"ready\n".to_vec())
    );
    assert_eq!(
        read_packet(&mut response, &cancellation).await.unwrap(),
        Packet::Delimiter
    );
    assert_eq!(
        read_packet(&mut response, &cancellation).await.unwrap(),
        Packet::Data(b"packfile\n".to_vec())
    );
    let mut pack = Vec::new();
    loop {
        match read_packet(&mut response, &cancellation).await.unwrap() {
            Packet::Data(bytes) => {
                assert_eq!(bytes[0], 1);
                pack.extend_from_slice(&bytes[1..]);
            }
            Packet::Flush => break,
            other => panic!("unexpected pack packet: {other:?}"),
        }
    }
    assert_eq!(&pack[..12], b"PACK\0\0\0\x02\0\0\0\x01");
    assert_eq!(
        &pack[pack.len() - 20..],
        Sha1::digest(&pack[..pack.len() - 20]).as_slice()
    );
}

#[tokio::test]
async fn unknown_have_does_not_finish_negotiation_or_send_a_pack() {
    let (layout, _, tip) = fixture(false).await;
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let mut request = packet(b"command=fetch\n");
    request.extend_from_slice(b"0001");
    request.extend(packet(format!("want {tip}\n").as_bytes()));
    request.extend(packet(
        format!("have {}\n", ObjectId::from([9; 20])).as_bytes(),
    ));
    request.extend_from_slice(b"0000");
    let runtime = Arc::new(RemoteGitRuntime::default());
    let mut input = BufReader::new(Cursor::new(request));
    let mut output = Vec::new();
    let cancellation = CancellationToken::new();
    let result = serve(
        &mut input,
        &mut output,
        layout.store(),
        layout.repo_prefix(),
        &[],
        &FetchAdmissionPolicy::default(),
        false,
        Some(root),
        &runtime,
        &cancellation,
    )
    .await;
    runtime.shutdown().await;
    result.unwrap();
    let mut response = BufReader::new(Cursor::new(&output[1..]));
    while read_packet(&mut response, &cancellation).await.unwrap() != Packet::Flush {}
    assert_eq!(
        read_packet(&mut response, &cancellation).await.unwrap(),
        Packet::Data(b"acknowledgments\n".to_vec())
    );
    assert_eq!(
        read_packet(&mut response, &cancellation).await.unwrap(),
        Packet::Data(b"NAK\n".to_vec())
    );
    assert_eq!(
        read_packet(&mut response, &cancellation).await.unwrap(),
        Packet::Flush
    );
    assert_eq!(
        read_packet(&mut response, &cancellation).await.unwrap(),
        Packet::ResponseEnd
    );
}

#[tokio::test]
async fn rewritten_branch_uses_the_authenticated_base_without_acknowledging_it() {
    let (layout, seed, tip) = fixture(true).await;
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let mut request = packet(b"command=fetch\n");
    request.extend_from_slice(b"0001");
    request.extend(packet(format!("want {tip}\n").as_bytes()));
    request.extend(packet(format!("have {seed}\n").as_bytes()));
    request.extend_from_slice(b"0000");
    let runtime = Arc::new(RemoteGitRuntime::default());
    let mut input = BufReader::new(Cursor::new(request));
    let mut output = Vec::new();
    let cancellation = CancellationToken::new();
    let result = serve(
        &mut input,
        &mut output,
        layout.store(),
        layout.repo_prefix(),
        &[],
        &FetchAdmissionPolicy::default(),
        false,
        Some(root),
        &runtime,
        &cancellation,
    )
    .await;
    runtime.shutdown().await;
    result.unwrap();
    let mut response = BufReader::new(Cursor::new(&output[1..]));
    while read_packet(&mut response, &cancellation).await.unwrap() != Packet::Flush {}
    assert_eq!(
        read_packet(&mut response, &cancellation).await.unwrap(),
        Packet::Data(b"acknowledgments\n".to_vec())
    );
    assert_eq!(
        read_packet(&mut response, &cancellation).await.unwrap(),
        Packet::Data(b"ready\n".to_vec())
    );
    assert_eq!(
        read_packet(&mut response, &cancellation).await.unwrap(),
        Packet::Delimiter
    );
    assert_eq!(
        read_packet(&mut response, &cancellation).await.unwrap(),
        Packet::Data(b"packfile\n".to_vec())
    );
}
