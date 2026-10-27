#[path = "upload_pack_wire_capsule_tests.rs"]
mod capsule_negotiation;

use super::*;
use crab_remote_git::Error as RemoteGitError;
use std::io::Cursor;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn fatal_sideband_retains_corruption_after_client_disconnect() {
    let (mut writer, reader) = tokio::io::duplex(64);
    drop(reader);
    let error = CrabError::from(RemoteGitError::Corrupt {
        stage: crab_remote_git::CorruptionStage::PackEntry,
    });
    let result = reject_pack_response(&mut writer, error, &CancellationToken::new())
        .await
        .map_err(|error| (error.code(), std::error::Error::source(&error).is_some()));
    assert_eq!(result, Err(("CRAB-E0020", true)));
}

#[tokio::test]
async fn fatal_sideband_frames_integrity_before_response_end() {
    let error = CrabError::from(RemoteGitError::Corrupt {
        stage: crab_remote_git::CorruptionStage::PackEntry,
    });
    let mut payload = vec![3];
    payload.extend_from_slice(error.to_string().as_bytes());
    let mut expected = packet(&payload);
    expected.extend_from_slice(b"00000002");
    let mut writer = Vec::new();
    assert!(
        reject_pack_response(&mut writer, error, &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(writer, expected);
}

#[derive(Debug, PartialEq, Eq)]
enum Packet {
    Data(Vec<u8>),
    Flush,
    Delimiter,
    ResponseEnd,
}

async fn read_packet<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    _cancellation: &CancellationToken,
) -> std::io::Result<Packet> {
    let mut header = [0; 4];
    reader.read_exact(&mut header).await?;
    match &header {
        b"0000" => Ok(Packet::Flush),
        b"0001" => Ok(Packet::Delimiter),
        b"0002" => Ok(Packet::ResponseEnd),
        _ => {
            let length = usize::from_str_radix(
                std::str::from_utf8(&header).map_err(std::io::Error::other)?,
                16,
            )
            .map_err(std::io::Error::other)?;
            if length < 4 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "packet length is shorter than its header",
                ));
            }
            let mut payload = vec![0; length - 4];
            reader.read_exact(&mut payload).await?;
            Ok(Packet::Data(payload))
        }
    }
}

fn packet(data: &[u8]) -> Vec<u8> {
    let length = data.len() + 4;
    let mut packet = format!("{length:04x}").into_bytes();
    packet.extend_from_slice(data);
    packet
}

#[tokio::test]
async fn layered_cold_clone_streams_one_authenticated_pack_without_materializing_it() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let path = object_store::path::Path::from("layer/pack");
    let header = [b'P', b'A', b'C', b'K', 0, 0, 0, 2, 0, 0, 0, 0];
    let checksum = Sha1::digest(header);
    let mut body = header.to_vec();
    body.extend_from_slice(&checksum);
    store
        .put(&path, bytes::Bytes::from(body.clone()))
        .await
        .expect("store cold-clone source pack");
    let source = crab_read::capsule_protocol::LayeredColdClonePack {
        source_path: path,
        pack_range: 0..32,
        content_hash: blake3::hash(&body).to_hex().to_string(),
        git_checksum: checksum.iter().map(|byte| format!("{byte:02x}")).collect(),
        object_count: 0,
    };
    let mut output = Vec::new();
    write_layered_cold_clone_response(
        &mut output,
        &store,
        &source,
        false,
        &CancellationToken::new(),
    )
    .await
    .expect("stream authenticated cold-clone pack");

    let mut reader = BufReader::new(Cursor::new(output));
    assert_eq!(
        read_packet(&mut reader, &CancellationToken::new())
            .await
            .expect("packfile response"),
        Packet::Data(b"packfile\n".to_vec())
    );
    let Packet::Data(sideband) = read_packet(&mut reader, &CancellationToken::new())
        .await
        .expect("pack sideband")
    else {
        panic!("expected sideband pack data");
    };
    assert_eq!(sideband.first(), Some(&1));
    assert_eq!(&sideband[1..], body.as_slice());
    assert_eq!(
        read_packet(&mut reader, &CancellationToken::new())
            .await
            .expect("pack flush"),
        Packet::Flush
    );
    assert_eq!(
        read_packet(&mut reader, &CancellationToken::new())
            .await
            .expect("response end"),
        Packet::ResponseEnd
    );
}

#[test]
fn pack_body_hash_handles_arbitrary_stream_boundaries() {
    let body = (0_u8..=127).collect::<Vec<_>>();
    let checksum = Sha1::digest(&body);
    let mut pack = body.clone();
    pack.extend_from_slice(&checksum);
    let mut hasher = Sha1::new();
    let mut trailer = [0_u8; 20];
    let mut trailer_len = 0;
    let mut offset = 0;
    for width in [1, 7, 19, 31, 3, 64] {
        let end = (offset + width).min(pack.len());
        update_pack_body_hash(
            &mut hasher,
            &mut trailer,
            &mut trailer_len,
            &pack[offset..end],
        );
        offset = end;
        if offset == pack.len() {
            break;
        }
    }
    if offset < pack.len() {
        update_pack_body_hash(&mut hasher, &mut trailer, &mut trailer_len, &pack[offset..]);
    }
    assert_eq!(trailer_len, 20);
    assert_eq!(hasher.finalize().as_slice(), checksum.as_slice());
    assert_eq!(trailer.as_slice(), checksum.as_slice());
}

#[test]
fn preplanned_pack_request_digest_binds_shallow_incremental_negotiation() {
    let first = ObjectId::from_hex(b"1111111111111111111111111111111111111111").expect("object ID");
    let second =
        ObjectId::from_hex(b"2222222222222222222222222222222222222222").expect("object ID");
    let request = FetchRequest {
        wants: vec![second],
        haves: vec![first],
        shallow: vec![first],
        thin_pack: true,
        ..FetchRequest::default()
    };
    assert!(request_pack_preplanning_cache_eligible(&request));

    let digest = preplanned_pack_request_digest(&request);
    let mut changed = request.clone();
    changed.wants = vec![first];
    assert_ne!(digest, preplanned_pack_request_digest(&changed));
    changed = request.clone();
    changed.haves = vec![second];
    assert_ne!(digest, preplanned_pack_request_digest(&changed));
    changed = request.clone();
    changed.shallow = vec![second];
    assert_ne!(digest, preplanned_pack_request_digest(&changed));
    changed = request.clone();
    changed.deepen = Some(1);
    assert!(!request_pack_preplanning_cache_eligible(&changed));
    changed = request;
    changed.filter = UploadPackFilter::BlobNone;
    assert!(!request_pack_preplanning_cache_eligible(&changed));
    let changed = FetchRequest {
        wants: vec![second],
        haves: vec![first],
        shallow: vec![first],
        include_tags: true,
        ..FetchRequest::default()
    };
    assert!(request_pack_preplanning_cache_eligible(&changed));
}

#[test]
fn external_thin_pack_requires_a_complete_unfiltered_transition() {
    let first = ObjectId::from_hex(b"1111111111111111111111111111111111111111").expect("object ID");
    let second =
        ObjectId::from_hex(b"2222222222222222222222222222222222222222").expect("object ID");
    let plan = crab_read::PackPlan {
        wants: vec![second],
        common_haves: vec![first],
        filter: UploadPackFilter::None,
        include_tags: false,
        object_ids: vec![second],
        required_bases: Vec::new(),
        shallow: Vec::new(),
        unshallow: Vec::new(),
    };
    let request = FetchRequest {
        wants: vec![second],
        haves: vec![first],
        thin_pack: true,
        ..FetchRequest::default()
    };

    assert!(external_thin_pack_eligible(&request, &plan));

    let mut changed = request.clone();
    changed.filter = UploadPackFilter::BlobNone;
    assert!(!external_thin_pack_eligible(&changed, &plan));
    changed = request.clone();
    changed.shallow = vec![first];
    assert!(!external_thin_pack_eligible(&changed, &plan));
    changed = request;
    changed.thin_pack = false;
    assert!(!external_thin_pack_eligible(&changed, &plan));
}

#[test]
fn dense_selected_response_requires_a_catalog_filter_without_shallow_state() {
    let mut request = FetchRequest {
        filter: UploadPackFilter::BlobNone,
        ..FetchRequest::default()
    };
    assert!(dense_selected_response(&request));

    request.filter = UploadPackFilter::ObjectType(crab_read::UploadPackObjectType::Tree);
    assert!(dense_selected_response(&request));

    request.filter = UploadPackFilter::TreeDepth(1);
    assert!(!dense_selected_response(&request));

    request.filter = UploadPackFilter::BlobNone;
    request.deepen = Some(100);
    assert!(!dense_selected_response(&request));

    request.filter = UploadPackFilter::None;
    assert!(!dense_selected_response(&request));
}

#[test]
fn tip_bound_fetch_is_limited_to_ordinary_advertised_ref_updates() {
    let policy = FetchAdmissionPolicy::default();
    let request = FetchRequest {
        wants: vec![
            ObjectId::from_hex(b"1111111111111111111111111111111111111111").expect("object ID"),
        ],
        ..FetchRequest::default()
    };
    let visible_refs = vec!["refs/heads/main".to_owned()];
    assert!(tip_bound_fetch_eligible(&request, &policy, &visible_refs));

    let mut changed = request.clone();
    changed.include_tags = true;
    assert!(tip_bound_fetch_eligible(&changed, &policy, &visible_refs));
    let visible_refs_with_tag = vec!["refs/heads/main".to_owned(), "refs/tags/release".to_owned()];
    assert!(!tip_bound_fetch_eligible(
        &changed,
        &policy,
        &visible_refs_with_tag
    ));
    changed = request.clone();
    changed
        .shallow
        .push(ObjectId::from_hex(b"1111111111111111111111111111111111111111").expect("object ID"));
    assert!(!tip_bound_fetch_eligible(&changed, &policy, &visible_refs));
    changed = request.clone();
    changed.filter = UploadPackFilter::BlobNone;
    assert!(!tip_bound_fetch_eligible(&changed, &policy, &visible_refs));

    let denied_policy = FetchAdmissionPolicy {
        allow_tip_sha_in_want: false,
        ..policy
    };
    assert!(!tip_bound_fetch_eligible(
        &request,
        &denied_policy,
        &visible_refs
    ));
}

#[tokio::test]
async fn generated_pack_lease_provider_serializes_one_repository_resource() {
    let store: Arc<dyn object_store::ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    let provider = ObjectStoreGeneratedPackLeaseProvider {
        store,
        prefix: "org/repo".to_owned(),
    };
    let first = match crab_remote_git::GeneratedPackLeaseProvider::try_acquire(
        &provider,
        "generated-pack-request",
        Duration::from_secs(5),
    )
    .await
    .expect("first lease acquisition")
    {
        crab_remote_git::GeneratedPackLeaseAttempt::Acquired(lease) => lease,
        crab_remote_git::GeneratedPackLeaseAttempt::Held { .. } => {
            panic!("first lease must be available")
        }
    };
    let blocked = crab_remote_git::GeneratedPackLeaseProvider::try_acquire(
        &provider,
        "generated-pack-request",
        Duration::from_secs(5),
    )
    .await
    .expect("contending lease acquisition");
    let retry_after = match blocked {
        crab_remote_git::GeneratedPackLeaseAttempt::Held { retry_after } => retry_after,
        crab_remote_git::GeneratedPackLeaseAttempt::Acquired(_) => {
            panic!("contending lease must remain held")
        }
    };
    assert!(!retry_after.is_zero());
    assert!(retry_after <= Duration::from_secs(5));

    first.release().await.expect("release first lease");
    let replacement = match crab_remote_git::GeneratedPackLeaseProvider::try_acquire(
        &provider,
        "generated-pack-request",
        Duration::from_secs(5),
    )
    .await
    .expect("replacement lease acquisition")
    {
        crab_remote_git::GeneratedPackLeaseAttempt::Acquired(lease) => lease,
        crab_remote_git::GeneratedPackLeaseAttempt::Held { .. } => {
            panic!("released lease must be acquirable")
        }
    };
    replacement.release().await.expect("release replacement");
}

#[tokio::test]
async fn generated_pack_coordination_reserves_and_releases_read_admission() {
    let store: Arc<dyn object_store::ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    let provider = ObjectStoreGeneratedPackLeaseProvider {
        store: Arc::clone(&store),
        prefix: "org/repo".to_owned(),
    };
    let lease = match crab_remote_git::GeneratedPackLeaseProvider::try_acquire(
        &provider,
        "generated-pack-request",
        Duration::from_secs(5),
    )
    .await
    .expect("lease acquisition")
    {
        crab_remote_git::GeneratedPackLeaseAttempt::Acquired(lease) => lease,
        crab_remote_git::GeneratedPackLeaseAttempt::Held { .. } => {
            panic!("lease must be available")
        }
    };

    let mut admitted = Vec::new();
    for _ in 0..crab_coordination::DEFAULT_READ_ADMISSION_CAPACITY {
        let mut ticket = crab_coordination::ReadAdmissionTicket::new(
            &store,
            "org/repo",
            crab_coordination::DEFAULT_READ_ADMISSION_CAPACITY,
            Duration::from_secs(60),
        )
        .expect("read admission ticket");
        for _ in 0..crab_coordination::DEFAULT_READ_ADMISSION_CAPACITY {
            if ticket.try_admit().await.expect("read admission probe") {
                admitted.push(ticket);
                break;
            }
        }
    }
    assert_eq!(
        admitted.len(),
        crab_coordination::DEFAULT_READ_ADMISSION_CAPACITY - 1
    );

    lease.release().await.expect("release generated-pack lease");
    let permit = crab_remote_git::GeneratedPackLeaseProvider::acquire_read(
        &provider,
        &CancellationToken::new(),
        READ_ADMISSION_WAIT,
    )
    .await
    .expect("generated-pack cache read admission");
    permit
        .release()
        .await
        .expect("release cache read admission");
    let mut replacement = crab_coordination::ReadAdmissionTicket::new(
        &store,
        "org/repo",
        crab_coordination::DEFAULT_READ_ADMISSION_CAPACITY,
        Duration::from_secs(60),
    )
    .expect("replacement read admission ticket");
    let mut replacement_admitted = false;
    for _ in 0..crab_coordination::DEFAULT_READ_ADMISSION_CAPACITY {
        if replacement
            .try_admit()
            .await
            .expect("replacement admission")
        {
            replacement_admitted = true;
            break;
        }
    }
    assert!(replacement_admitted);

    replacement.release().await.expect("release replacement");
    for ticket in admitted {
        ticket.release().await.expect("release read admission");
    }
}

#[test]
fn upload_pack_profile_covers_the_supported_visibility_generation() {
    let options = upload_pack_repository_options().expect("valid upload-pack limits");

    assert_eq!(options.object_limits().max_object_bytes, 128 * MIB);
    assert_eq!(
        options.operation_limits().max_logical_objects,
        crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS
    );
    assert_eq!(options.operation_limits().max_duration, READ_ADMISSION_WAIT);
    assert!(
        options.operation_limits().max_response_bytes
            < options.operation_limits().max_inflated_bytes
    );
}

#[test]
fn read_admission_retry_delay_honors_provider_hint() {
    let hint = Duration::from_secs(2);

    let delay = read_admission_retry_delay(0, Some(hint));

    assert!(delay >= hint);
    assert!(delay <= hint.saturating_add(READ_ADMISSION_RETRY_BASE));
}

#[test]
fn read_admission_retry_delay_is_capped_without_provider_hint() {
    let delay = read_admission_retry_delay(usize::MAX, None);

    assert!(delay <= READ_ADMISSION_RETRY_CAP);
}

#[tokio::test]
async fn empty_repository_serves_unborn_refs_but_rejects_object_wants() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = crab_storage::StoreLayout::new(store.clone(), "org/empty".to_owned());
    let root =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    for (command, arguments, hidden_refs, expected, succeeds) in [
        ("ls-refs", vec!["symrefs"], vec![], "00000002", true),
        (
            "ls-refs",
            vec!["symrefs", "unborn"],
            vec![],
            "unborn HEAD symref-target:refs/heads/main\n",
            true,
        ),
        (
            "ls-refs",
            vec!["unborn"],
            vec!["refs/heads/main".to_owned()],
            "00000002",
            true,
        ),
        (
            "fetch",
            vec!["want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "done"],
            vec![],
            "fetch requested an object from an empty repository",
            false,
        ),
    ] {
        let mut request = packet(format!("command={command}\n").as_bytes());
        request.extend_from_slice(b"0001");
        for argument in arguments {
            request.extend(packet(format!("{argument}\n").as_bytes()));
        }
        request.extend_from_slice(b"0000");
        let mut reader = BufReader::new(Cursor::new(request));
        let mut output = Vec::new();
        let runtime = Arc::new(RemoteGitRuntime::default());
        let result = serve(
            &mut reader,
            &mut output,
            &store,
            "org/empty",
            &hidden_refs,
            &FetchAdmissionPolicy::default(),
            false,
            root.clone(),
            &runtime,
            &CancellationToken::new(),
        )
        .await;
        runtime.shutdown().await;
        assert_eq!(result.is_ok(), succeeds, "{command}: {result:?}");
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(expected), "{output}");
        if !succeeds {
            assert!(output.contains("ERR "));
        }
        assert!(!output.contains("packfile\n") && !output.contains("fallback\n"));
        if !hidden_refs.is_empty() {
            assert!(!output.contains("symref-target:refs/heads/main"));
        }
    }
}

async fn remove_pinned_runs(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    root: &crab_metadata::capsule_protocol::RootSnapshot,
) {
    let (heads, active) =
        crab_metadata::capsule_protocol::capture_ref_heads(layout, root.record().root())
            .await
            .unwrap();
    let visible = crab_metadata::capsule_protocol::materialize_visible_ref_heads(
        root.record().root(),
        &heads,
        &active,
    )
    .unwrap();
    assert!(!visible.pointers.is_empty());
    for pointer in visible.pointers {
        layout
            .store()
            .delete(&layout.capsule_path(pointer.hash()))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn ref_discovery_does_not_require_pack_payloads_or_locator_indexes() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = crab_storage::StoreLayout::new(store.clone(), "org/discovery".to_owned());
    let tip = "a".repeat(40);
    let tag = "b".repeat(40);
    let root =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::new(
        root.record().digest(),
        vec![
            crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some(tip.clone()),
                None,
            ),
            crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                "refs/heads/secret",
                None,
                Some(tip.clone()),
                None,
            ),
            crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                "refs/tags/v1",
                None,
                Some(tag.clone()),
                Some(tip.clone()),
            ),
        ],
    )
    .unwrap();
    let capsule =
        crab_metadata::capsule_protocol::Capsule::build(&transaction, Vec::new(), Vec::new())
            .unwrap();
    crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
        .await
        .unwrap();
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    remove_pinned_runs(&layout, &root).await;
    let mut request = packet(b"command=ls-refs\n");
    request.extend_from_slice(b"0001");
    request.extend(packet(b"symrefs\n"));
    request.extend(packet(b"peel\n"));
    request.extend_from_slice(b"0000");
    let mut reader = BufReader::new(Cursor::new(request));
    let mut output = Vec::new();
    let runtime = Arc::new(RemoteGitRuntime::default());
    serve(
        &mut reader,
        &mut output,
        &store,
        "org/discovery",
        &["refs/heads/secret".to_owned()],
        &FetchAdmissionPolicy::default(),
        false,
        root,
        &runtime,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    runtime.shutdown().await;
    let mut expected = packet(format!("{tip} HEAD symref-target:refs/heads/main\n").as_bytes());
    expected.extend(packet(format!("{tip} refs/heads/main\n").as_bytes()));
    expected.extend(packet(
        format!("{tag} refs/tags/v1 peeled:{tip}\n").as_bytes(),
    ));
    expected.extend_from_slice(b"00000002");
    let advertisement_end = output
        .windows(4)
        .position(|bytes| bytes == b"0000")
        .unwrap()
        + 4;
    assert_eq!(&output[advertisement_end..], expected);
}

#[test]
fn negotiated_haves_are_deduplicated_across_rounds() {
    let first = ObjectId::from([1; 20]);
    let second = ObjectId::from([2; 20]);
    let third = ObjectId::from([3; 20]);
    let mut accumulated = Vec::new();
    let mut seen = HashSet::new();

    merge_negotiated_haves(&mut accumulated, &mut seen, &[first, second])
        .expect("first negotiation round");
    merge_negotiated_haves(&mut accumulated, &mut seen, &[second, third])
        .expect("second negotiation round");

    assert_eq!(accumulated, [first, second, third]);
}

#[path = "upload_pack_wire_protocol_tests.rs"]
mod protocol;
