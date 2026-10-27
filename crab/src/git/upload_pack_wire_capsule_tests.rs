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

pub(super) async fn fixture(rewrite: bool) -> (StoreLayout<Store>, ObjectId, ObjectId) {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store, "repositories/negotiation".to_owned());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    let seed = Box::pin(publish_commit(&layout, None, false, "seed")).await;
    let tip = Box::pin(publish_commit(&layout, Some(seed), rewrite, "incremental")).await;
    (layout, seed, tip)
}

async fn publish_commit(
    layout: &StoreLayout<Store>,
    previous: Option<ObjectId>,
    rewrite: bool,
    message: &str,
) -> ObjectId {
    let tree = crab_remote::objects::object_id(gix_object::Kind::Tree, b"").unwrap();
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
    let root = crab_write::capsule_protocol::open_root(layout)
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
    let visibility =
        CapsuleVisibilityDelta::new(BTreeMap::from([("refs/heads/main".to_owned(), visibility)]))
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
    crab_write::capsule_protocol::publish(layout, root, &transaction, &capsule)
        .await
        .unwrap();
    if previous.is_none() {
        crab_remote::checkpoint::publish_capsule_checkpoint(
            layout,
            0,
            LIMIT,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    }
    tip
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
        root,
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
        root,
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
        root,
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

#[tokio::test(flavor = "multi_thread")]
async fn complete_view_promotion_keeps_advertised_capsule_ref_state() {
    let (layout, _, tip) = Box::pin(fixture(false)).await;
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let root_digest = root.record().digest().to_owned();
    let (client, server) = tokio::io::duplex(8192);
    let server_layout = layout.clone();
    let server = Box::pin(async move {
        let (input, mut output) = tokio::io::split(server);
        let runtime = Arc::new(RemoteGitRuntime::default());
        let result = Box::pin(serve(
            &mut BufReader::new(input),
            &mut output,
            server_layout.store(),
            server_layout.repo_prefix(),
            &[],
            &FetchAdmissionPolicy::default(),
            false,
            root,
            &runtime,
            &CancellationToken::new(),
        ))
        .await;
        runtime.shutdown().await;
        result
    });
    let client = Box::pin(async move {
        let (input, mut output) = tokio::io::split(client);
        let mut input = BufReader::new(input);
        let cancellation = CancellationToken::new();
        assert_eq!(input.read_u8().await.unwrap(), b'\n');
        while read_packet(&mut input, &cancellation).await.unwrap() != Packet::Flush {}
        let mut discovery = packet(b"command=ls-refs\n");
        discovery.extend_from_slice(b"0001");
        discovery.extend(packet(b"symrefs\n"));
        discovery.extend_from_slice(b"0000");
        output.write_all(&discovery).await.unwrap();
        let mut advertised = Vec::new();
        loop {
            match read_packet(&mut input, &cancellation).await.unwrap() {
                Packet::ResponseEnd => break,
                Packet::Data(bytes) => advertised.extend(bytes),
                Packet::Flush => {}
                other => panic!("unexpected discovery packet: {other:?}"),
            }
        }
        assert!(
            String::from_utf8(advertised)
                .unwrap()
                .contains(&tip.to_string())
        );
        let advanced = Box::pin(publish_commit(&layout, Some(tip), false, "concurrent")).await;
        assert_eq!(
            crab_write::capsule_protocol::open_root(&layout)
                .await
                .unwrap()
                .record()
                .digest(),
            root_digest
        );
        let mut fetch = packet(b"command=fetch\n");
        fetch.extend_from_slice(b"0001");
        fetch.extend(packet(format!("want {tip}\n").as_bytes()));
        fetch.extend(packet(b"done\n"));
        fetch.extend_from_slice(b"0000");
        output.write_all(&fetch).await.unwrap();
        let mut response = Vec::new();
        input.read_to_end(&mut response).await.unwrap();
        (response, advanced)
    });
    let (result, (response, advanced)) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(server, client)
    })
    .await
    .unwrap();
    assert!(
        result.is_ok(),
        "advertised Capsule snapshot was replaced: {result:?}"
    );
    let cancellation = CancellationToken::new();
    let mut response = BufReader::new(Cursor::new(response));
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
    assert_eq!(&pack[..12], b"PACK\0\0\0\x02\0\0\0\x03");
    assert_eq!(
        &pack[pack.len() - 20..],
        Sha1::digest(&pack[..pack.len() - 20]).as_slice()
    );
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(repository)
            .status()
            .unwrap()
            .success()
    );
    let mut command = std::process::Command::new("git")
        .args(["index-pack", "--stdin", "--strict"])
        .current_dir(repository)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut command.stdin.take().unwrap(), &pack).unwrap();
    assert!(command.wait().unwrap().success());
    for (oid, expected) in [(tip, true), (advanced, false)] {
        let result = std::process::Command::new("git")
            .args(["cat-file", "-e", &oid.to_string()])
            .current_dir(repository)
            .output()
            .unwrap();
        assert_eq!(result.status.success(), expected);
    }
}
