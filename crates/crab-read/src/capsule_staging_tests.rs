#![expect(clippy::unwrap_used, reason = "test fixture assertions")]

use super::*;
use crab_metadata::capsule_protocol::{CapsuleRefEdit, CapsuleTransaction};
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    router: StoreLayout<Store>,
    checkpoint: LayeredCheckpoint,
    blobs: Vec<Vec<u8>>,
    member_oids: BTreeMap<String, Vec<[u8; 20]>>,
}

async fn fixture(groups: usize, members: usize) -> Fixture {
    let router = StoreLayout::new(
        Store::new(Arc::new(object_store::memory::InMemory::new())),
        "staging/test".to_owned(),
    );
    let mut sources = Vec::new();
    let mut blobs = Vec::new();
    let mut member_oids = BTreeMap::new();
    for group in 0..groups {
        let mut packs = Vec::new();
        let mut edits = Vec::new();
        for ordinal in 0..members {
            let blob = format!("staged member {group} {ordinal}").into_bytes();
            let (pack, oid) = super::intake_tests::git_pack(&blob);
            packs.push(pack);
            blobs.push(blob);
            edits.push(CapsuleRefEdit::new(
                format!("refs/tags/member-{ordinal}"),
                None,
                Some(oid.to_string()),
                None,
            ));
        }
        let transaction = CapsuleTransaction::new(&"1".repeat(64), edits).unwrap();
        let capsule = Capsule::build(&transaction, packs, Vec::new()).unwrap();
        for descriptor in capsule.git_packs() {
            let index = capsule.section_bytes(descriptor.index_section()).unwrap();
            let pack = capsule.section_bytes(descriptor.pack_section()).unwrap();
            let (oids, _) =
                crab_git::pack_locator::sorted_object_ids_from_index_bytes(&index).unwrap();
            member_oids.insert(
                blake3::hash(&pack).to_hex().to_string(),
                oids.iter()
                    .map(|oid| oid.as_bytes().try_into().unwrap())
                    .collect(),
            );
        }
        let run = CapsuleRun::leaf(capsule).unwrap();
        router
            .store()
            .put(&router.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        sources.push(PackSourceDescriptor::from_capsule_run(&run).unwrap());
    }
    let checkpoint =
        LayeredCheckpoint::build(0, &"2".repeat(64), sources, PointerCatalog::new(), None).unwrap();
    Fixture {
        router,
        checkpoint,
        blobs,
        member_oids,
    }
}

fn members(fixture: &Fixture) -> Vec<LayeredPayloadMemberRead> {
    fixture
        .checkpoint
        .sources()
        .iter()
        .flat_map(|source| {
            source
                .members()
                .iter()
                .map(|member| LayeredPayloadMemberRead {
                    member: LayeredMemberRead {
                        source_path: fixture.router.capsule_path(source.object_hash()),
                        source_size: source.object_size(),
                        source_bytes: None,
                        pack_id: crab_xet::hash::MerkleHash::from_hex(member.pack().blake3())
                            .unwrap(),
                        member: member.clone(),
                    },
                    complete_local: false,
                })
        })
        .collect()
}

#[tokio::test]
async fn coalesced_staging_is_send_and_preserves_exact_transport_and_git_bytes() {
    let fixture = fixture(1, 16).await;
    let source = &fixture.checkpoint.sources()[0];
    let first = &source.members()[0];
    let last = source.members().last().unwrap();
    let full_bytes = last.locator().offset() + last.locator().length() - first.pack().offset();
    let split_bytes = last.locator().offset() + last.locator().length() - first.index().offset()
        + last.pack().offset()
        + last.pack().length()
        - first.pack().offset();
    let allowed = fixture
        .member_oids
        .values()
        .flatten()
        .map(|oid| ObjectId::from_bytes_or_panic(oid))
        .collect::<BTreeSet<_>>();
    let packs = fixture.member_oids.keys().cloned().collect::<BTreeSet<_>>();
    for mode in 0..4 {
        let requests = Arc::new(AtomicU64::new(0));
        let bytes = Arc::new(AtomicU64::new(0));
        let counter = requests.clone();
        let byte_counter = bytes.clone();
        let store = fixture
            .router
            .store()
            .clone()
            .with_read_request_observer(Arc::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            }))
            .with_read_byte_observer(Arc::new(move |length| {
                byte_counter.fetch_add(length, Ordering::SeqCst);
            }));
        let router = StoreLayout::new(store, fixture.router.repo_prefix().to_owned());
        let checkpoint = fixture.checkpoint.clone();
        let admitted = fixture.member_oids.clone();
        let allowed = allowed.clone();
        let packs = packs.clone();
        let destination = tempfile::tempdir().unwrap();
        crab_git::initialize_bare_git_dir(destination.path()).unwrap();
        let git_dir = destination.path().to_path_buf();
        let expected_bytes = if mode == 3 { split_bytes } else { full_bytes };
        let installed = tokio::spawn(async move {
            let no_admission = BTreeMap::new();
            let selection = match mode {
                0 => LayeredInstallSelection::Native,
                1 => LayeredInstallSelection::Maintenance(None),
                _ => LayeredInstallSelection::Fetch {
                    packs: &packs,
                    member_oids: if mode == 2 { &admitted } else { &no_admission },
                    allowed: &allowed,
                    required: &allowed,
                },
            };
            install_layered_git_packs_from_store_selected_with_sources(
                checkpoint.sources(),
                &[],
                &[],
                &router,
                &git_dir,
                expected_bytes,
                selection,
                &CancellationToken::new(),
            )
            .await
        })
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(installed.len(), 16);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            if mode == 3 { 2 } else { 1 }
        );
        assert_eq!(bytes.load(Ordering::SeqCst), expected_bytes);
        for blob in &fixture.blobs {
            let oid = gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, blob)
                .unwrap();
            let output = std::process::Command::new("git")
                .arg("--git-dir")
                .arg(destination.path())
                .args(["cat-file", "blob", &oid.to_string()])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(&output.stdout, blob);
        }
        assert!(
            std::fs::read_dir(destination.path().join("objects/pack"))
                .unwrap()
                .all(|entry| entry.unwrap().file_type().unwrap().is_file())
        );
    }
}

#[tokio::test]
async fn pack_file_owner_survives_transfer_to_the_blocking_installer() {
    let fixture = fixture(1, 1).await;
    let members = members(&fixture);
    let windows = plan_layered_payload_windows_for_members(
        &members,
        &BTreeSet::from([0]),
        LayeredPayloadWindowMode::Full,
    )
    .unwrap();
    let directory = Arc::new(tempfile::tempdir().unwrap());
    let private_path = directory.path().to_path_buf();
    let read = fetch_layered_payload_windows(
        fixture.router.store(),
        &members,
        &windows,
        LayeredPayloadWindowMode::Full,
        &directory,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let member = &members[0].member.member;
    let payload = GitPackPayload {
        pack: Some(read.pack(0).unwrap()),
        index: read.sidecar(0, SectionKind::Index).unwrap(),
        reverse_index: read.sidecar(0, SectionKind::ReverseIndex).unwrap(),
        locator: read.sidecar(0, SectionKind::Locator).unwrap(),
        verified_identity: Some(verified_layered_pack_identity(member).unwrap()),
        content_hash: member.pack().blake3().to_owned(),
        git_checksum: member.git_checksum().to_owned(),
        object_count: member.object_count(),
        external_delta_bases: Vec::new(),
    };
    drop(read);
    drop(directory);
    assert!(private_path.exists());
    let destination = tempfile::tempdir().unwrap();
    crab_git::initialize_bare_git_dir(destination.path()).unwrap();
    let installed = install_git_pack_payloads(destination.path(), vec![payload], 0, true)
        .await
        .unwrap();
    assert_eq!(
        blake3::hash(&std::fs::read(&installed[0]).unwrap())
            .to_hex()
            .as_str(),
        member.pack().blake3()
    );
    assert!(!private_path.exists());
}

#[tokio::test]
async fn corrupt_staged_component_never_publishes_a_pack_prefix() {
    let fixture = fixture(1, 2).await;
    let source = &fixture.checkpoint.sources()[0];
    let path = fixture.router.capsule_path(source.object_hash());
    let mut body = fixture
        .router
        .store()
        .get_with_etag(&path)
        .await
        .unwrap()
        .0
        .to_vec();
    body[source.members()[0].pack().offset() as usize] ^= 1;
    fixture
        .router
        .store()
        .put_overwrite(&path, Bytes::from(body))
        .await
        .unwrap();
    let destination = tempfile::tempdir().unwrap();
    crab_git::initialize_bare_git_dir(destination.path()).unwrap();

    let error =
        install_layered_checkpoint(&fixture.checkpoint, &fixture.router, destination.path(), 0)
            .await
            .unwrap_err();

    assert!(matches!(error, ReadError::CorruptObject { .. }));
    assert_eq!(
        std::fs::read_dir(destination.path().join("objects/pack"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn source_size_is_checked_even_when_selected_section_hashes_match() {
    let fixture = fixture(1, 1).await;
    let source = &fixture.checkpoint.sources()[0];
    let path = fixture.router.capsule_path(source.object_hash());
    let mut body = fixture
        .router
        .store()
        .get_with_etag(&path)
        .await
        .unwrap()
        .0
        .to_vec();
    body.push(0);
    fixture
        .router
        .store()
        .put_overwrite(&path, Bytes::from(body))
        .await
        .unwrap();
    let destination = tempfile::tempdir().unwrap();
    crab_git::initialize_bare_git_dir(destination.path()).unwrap();

    let error =
        install_layered_checkpoint(&fixture.checkpoint, &fixture.router, destination.path(), 0)
            .await
            .unwrap_err();

    assert!(matches!(error, ReadError::CorruptObject { .. }));
    assert_eq!(
        std::fs::read_dir(destination.path().join("objects/pack"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn cancellation_drains_staging_before_the_private_directory_is_removed() {
    let fixture = fixture(8, 1).await;
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let store = fixture
        .router
        .store()
        .clone()
        .with_read_byte_observer(Arc::new(move |_| signal.cancel()));
    let router = StoreLayout::new(store, fixture.router.repo_prefix().to_owned());
    let destination = tempfile::tempdir().unwrap();
    crab_git::initialize_bare_git_dir(destination.path()).unwrap();

    let error = install_layered_git_packs_from_store_selected_with_sources(
        fixture.checkpoint.sources(),
        &[],
        &[],
        &router,
        destination.path(),
        0,
        LayeredInstallSelection::Native,
        &cancel,
    )
    .await
    .unwrap_err();

    assert!(matches!(error, ReadError::Cancelled));
    assert_eq!(
        std::fs::read_dir(destination.path().join("objects/pack"))
            .unwrap()
            .count(),
        0
    );
}

#[cfg(feature = "hydration")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_staging_retains_its_directory_until_the_source_worker_drains() {
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    let fixture = fixture(1, 1).await;
    let members = members(&fixture);
    let windows = plan_layered_payload_windows_for_members(
        &members,
        &BTreeSet::from([0]),
        LayeredPayloadWindowMode::Full,
    )
    .unwrap();
    let (started, entered) = tokio::sync::oneshot::channel();
    let started = Mutex::new(Some(started));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let blocked = gate.clone();
    let store = fixture
        .router
        .store()
        .clone()
        .with_read_request_observer(Arc::new(move |_| {
            if let Some(started) = started.lock().unwrap().take() {
                let _ = started.send(());
                let (released, ready) = &*blocked;
                drop(
                    ready
                        .wait_while(released.lock().unwrap(), |released| !*released)
                        .unwrap(),
                );
            }
        }));
    let directory = Arc::new(tempfile::tempdir().unwrap());
    let private_path = directory.path().to_path_buf();
    let mut task = tokio::spawn(async move {
        fetch_layered_payload_windows(
            &store,
            &members,
            &windows,
            LayeredPayloadWindowMode::Full,
            &directory,
            &CancellationToken::new(),
        )
        .await
    });
    entered.await.unwrap();
    task.abort();
    let prompt = tokio::time::timeout(Duration::from_secs(1), &mut task).await;
    let retained = private_path.exists();
    let (released, ready) = &*gate;
    *released.lock().unwrap() = true;
    ready.notify_all();
    let cancelled = match prompt {
        Ok(result) => matches!(result, Err(error) if error.is_cancelled()),
        Err(_) => {
            let _ = task.await;
            false
        }
    };
    let drained = tokio::time::timeout(Duration::from_secs(5), async {
        while private_path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(cancelled);
    assert!(retained);
    assert!(drained.is_ok());
}
