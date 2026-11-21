use super::super::representation::file_preview;
use super::super::*;
use axum::http::header;

#[tokio::test]
async fn metadata_preview_omits_payload_without_guessing_encoding() {
    use crab_metadata::capsule_protocol::{Capsule, CapsuleRefEdit, CapsuleTransaction};
    use crab_remote_git::{OperationKind, OperationLimits, RepositoryOptions, Revision};
    use crab_storage::{StorageObservation, StorageObserver, StorageOperation, Store};
    use std::sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    };

    #[derive(Default)]
    struct Reads {
        bytes: AtomicU64,
        active: AtomicUsize,
    }
    impl StorageObserver for Reads {
        fn started(&self, _: StorageOperation) {
            self.active.fetch_add(1, Ordering::SeqCst);
        }
        fn finished(&self, observation: StorageObservation) {
            self.bytes
                .fetch_add(observation.bytes_read, Ordering::SeqCst);
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
    const MIB: usize = 1024 * 1024;
    let mut random = 17u32;
    let large_utf8 = (0..9 * MIB)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            b' ' + (random % 95) as u8
        })
        .collect::<Vec<_>>();
    let mut large_binary = large_utf8.clone();
    large_binary[0] = 0;
    let mut large_non_utf8 = large_utf8.clone();
    large_non_utf8[0] = 0xff;
    let boundary = vec![b'x'; MIB];
    let over_boundary = vec![b'x'; MIB + 1];
    let mut over_binary = over_boundary.clone();
    over_binary[0] = 0;
    let mut over_non_utf8 = over_boundary.clone();
    over_non_utf8[0] = 0xff;
    let crab_pointer = b"version https://crab.dev/spec/v1\nfile-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\nsize 4294967296\n";
    let lfs_pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\nsize 8589934592\n";
    let entries = [
        ("large-utf8", 0o100644, large_utf8.as_slice()),
        ("large-binary", 0o100644, large_binary.as_slice()),
        ("large-non-utf8", 0o100644, large_non_utf8.as_slice()),
        ("boundary", 0o100644, boundary.as_slice()),
        ("over-boundary", 0o100644, over_boundary.as_slice()),
        ("over-binary", 0o100644, over_binary.as_slice()),
        ("over-non-utf8", 0o100644, over_non_utf8.as_slice()),
        ("empty", 0o100644, b"".as_slice()),
        ("text", 0o100644, "UTF-8 ✓\n".as_bytes()),
        ("binary", 0o100644, b"a\0b".as_slice()),
        ("non-utf8", 0o100644, b"a\xffb".as_slice()),
        ("crab", 0o100644, crab_pointer.as_slice()),
        ("lfs", 0o100644, lfs_pointer.as_slice()),
        ("link", 0o120000, b"large-utf8".as_slice()),
    ];
    let history = crate::test_support::git::history_with_entries(&entries);
    let large_oids = [&large_utf8, &large_binary, &large_non_utf8].map(|body| {
        gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, body)
            .unwrap()
            .to_string()
    });
    // Qualify physical representations with native Git: packing heuristics must
    // not silently remove the delta metadata case from this fixture.
    let pack = tempfile::tempdir().unwrap();
    std::fs::write(pack.path().join("fixture.pack"), history.pack.pack_bytes()).unwrap();
    std::fs::write(pack.path().join("fixture.idx"), history.pack.index_bytes()).unwrap();
    let verified = std::process::Command::new("git")
        .args(["verify-pack", "-v"])
        .arg(pack.path().join("fixture.idx"))
        .output()
        .unwrap();
    assert!(verified.status.success());
    let mut representations = [0; 2];
    let mut encoded_sizes = std::collections::BTreeMap::new();
    let mut encoded_offsets = std::collections::BTreeMap::new();
    for line in std::str::from_utf8(&verified.stdout).unwrap().lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields
            .first()
            .is_some_and(|oid| large_oids.iter().any(|expected| expected.as_str() == *oid))
        {
            representations[usize::from(fields.len() > 5)] += 1;
            encoded_sizes.insert(fields[0].to_owned(), fields[3].parse::<u64>().unwrap());
            encoded_offsets.insert(fields[0].to_owned(), fields[4].parse::<u64>().unwrap());
        }
    }
    assert!(
        representations[0] > 0 && representations[1] > 0,
        "large base/delta coverage: {representations:?}"
    );

    let reads = Arc::new(Reads::default());
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_storage_observer(reads.clone());
    let mut server = crate::test_support::server::fixture_with_store(store).await;
    let repository = server.repositories.values().pop().unwrap();
    let root = crab_write::capsule_protocol::open_root(&repository.layout)
        .await
        .unwrap();
    let commit = history.oids[0].clone();
    let transaction = CapsuleTransaction::new(
        root.record().digest(),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some(commit.clone()),
            None,
        )],
    )
    .unwrap();
    let capsule = Capsule::build(&transaction, vec![history.pack], Vec::new()).unwrap();
    let run = crab_write::capsule_protocol::capsule_leaf_run(capsule.clone()).unwrap();
    let capsule_path = repository.layout.capsule_path(run.hash());
    let pack_offset = run.git_packs()[0].pack().offset();
    drop(run);
    crab_write::capsule_protocol::publish(&repository.layout, root, &transaction, capsule)
        .await
        .unwrap();
    Arc::get_mut(&mut server).unwrap().options = RepositoryOptions::new(
        Default::default(),
        OperationLimits {
            max_fetched_bytes: (32 * MIB) as u64,
            max_inflated_bytes: (4 * MIB) as u64,
            max_response_bytes: (8 * MIB) as u64,
            ..Default::default()
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = crate::server::http::router(Arc::clone(&server));
    let cancel = server.cancellation.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(cancel.cancelled_owned())
            .await
            .unwrap();
    });
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    for (name, mode, body) in entries {
        let before = reads.bytes.load(Ordering::SeqCst);
        let response = http
            .get(format!(
                "{origin}/api/repos/team/repo/file-preview?rev={commit}&path={name}"
            ))
            .send()
            .await
            .unwrap();
        let status = response.status();
        assert!(
            response.headers()["server-timing"]
                .to_str()
                .unwrap()
                .contains("encode;dur=")
        );
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-cache"
        );
        let etag = response.headers()[header::ETAG].clone();
        let value: Value = response.json().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{name}: {value}");
        let expected = if body.len() > MIB {
            "omitted_size"
        } else if body.contains(&0) {
            "binary"
        } else if std::str::from_utf8(body).is_err() {
            "unsupported_encoding"
        } else {
            "inline"
        };
        assert_eq!(value["preview"]["state"], expected, "{name}");
        assert_eq!(
            value["oid"],
            gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, body)
                .unwrap()
                .to_string()
        );
        assert_eq!(
            (
                value["size"].as_u64(),
                value["mode"].as_str(),
                value["commit"].as_str()
            ),
            (
                Some(body.len() as u64),
                Some(format!("{mode:06o}").as_str()),
                Some(commit.as_str())
            )
        );
        if expected == "inline" {
            assert_eq!(value["preview"]["text"], std::str::from_utf8(body).unwrap());
        } else {
            assert!(value["preview"].get("text").is_none());
        }
        let classification = match name {
            "crab" => "CrabPointer",
            "lfs" => "LfsPointer",
            _ => "OrdinaryGit",
        };
        assert_eq!(value["classification"], classification);
        if body.len() > 8 * MIB {
            // Alpha.1 metadata verifies the complete encoded entry's CRC. The
            // inflation ceiling stays below the logical body while transport
            // accounting must expose, rather than hide, that encoded I/O.
            let fetched = reads.bytes.load(Ordering::SeqCst) - before;
            let oid = value["oid"].as_str().unwrap();
            assert!(
                fetched >= encoded_sizes[oid] && fetched <= (32 * MIB) as u64,
                "{name}: fetched {fetched}, encoded entry {}",
                encoded_sizes[oid]
            );
            eprintln!(
                "{name}: fetched={fetched} encoded={} logical={} inflation_ceiling={}",
                encoded_sizes[oid],
                body.len(),
                4 * MIB
            );
        }
        let revalidated = http
            .get(format!(
                "{origin}/api/repos/team/repo/file-preview?rev={commit}&path={name}"
            ))
            .header(
                header::IF_NONE_MATCH,
                format!("\"old\", W/{}", etag.to_str().unwrap()),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED, "{name}");
        assert_eq!(revalidated.headers()[header::ETAG], etag);
        assert!(revalidated.bytes().await.unwrap().is_empty());
    }
    let raw_url = format!("{origin}/api/repos/team/repo/blob?rev={commit}&path=text");
    let raw = http.get(&raw_url).send().await.unwrap();
    let raw_etag = raw.headers()[header::ETAG].clone();
    assert_eq!(raw.bytes().await.unwrap(), "UTF-8 ✓\n".as_bytes());
    let preview = http
        .get(format!(
            "{origin}/api/repos/team/repo/file-preview?rev={commit}&path=text"
        ))
        .send()
        .await
        .unwrap();
    assert_ne!(preview.headers()[header::ETAG], raw_etag);
    for (if_range, status, body) in [
        (
            raw_etag.to_str().unwrap().to_owned(),
            StatusCode::PARTIAL_CONTENT,
            b"UTF".as_slice(),
        ),
        (
            format!("W/{}", raw_etag.to_str().unwrap()),
            StatusCode::OK,
            "UTF-8 ✓\n".as_bytes(),
        ),
        ("\"old\"".into(), StatusCode::OK, "UTF-8 ✓\n".as_bytes()),
    ] {
        let response = http
            .get(&raw_url)
            .header(header::RANGE, "bytes=0-2")
            .header(header::IF_RANGE, if_range)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.bytes().await.unwrap(), body);
    }
    let response = http
        .get(&raw_url)
        .header(header::IF_NONE_MATCH, "*")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    let response = http
        .get(format!(
            "{origin}/api/repos/team/repo/blob?rev={commit}&path=large-utf8"
        ))
        .header(header::RANGE, format!("bytes={}-", 10 * MIB))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        response.headers()[header::CONTENT_RANGE],
        format!("bytes */{}", 9 * MIB)
    );
    let opened = repository
        .open(&server, &server.cancellation)
        .await
        .unwrap();
    for (limits, expected_limit) in [
        (
            OperationLimits {
                max_fetched_bytes: (2 * MIB) as u64,
                ..server.options.operation_limits()
            },
            "fetched bytes",
        ),
        (
            OperationLimits {
                max_response_bytes: 128,
                ..server.options.operation_limits()
            },
            "response bytes",
        ),
    ] {
        let operation = opened
            .git
            .operation_with_limits(OperationKind::Content, &server.cancellation, limits)
            .await
            .unwrap();
        let result = async {
            let snapshot = opened
                .git
                .snapshot(&Revision::Reference("main".into()), &operation)
                .await?;
            file_preview(
                &snapshot,
                &GitPath::new(b"large-utf8".to_vec())?,
                &operation,
            )
            .await
            .map(|_| ())
        }
        .await;
        let error = operation.finish(result).await.unwrap_err();
        assert!(
            matches!(error, Error::LimitExceeded { limit, .. } if limit == expected_limit),
            "{expected_limit}: {error:?}"
        );
    }
    let cancelled = tokio_util::sync::CancellationToken::new();
    let operation = opened
        .git
        .operation(OperationKind::Content, &cancelled)
        .await
        .unwrap();
    let result = opened
        .git
        .snapshot(&Revision::Reference("main".into()), &operation)
        .await;
    let snapshot = operation.finish(result).await.unwrap();
    let operation = opened
        .git
        .operation(OperationKind::Content, &cancelled)
        .await
        .unwrap();
    cancelled.cancel();
    let result = file_preview(
        &snapshot,
        &GitPath::new(b"large-utf8".to_vec()).unwrap(),
        &operation,
    )
    .await
    .map(|_| ());
    assert!(matches!(
        operation.finish(result).await,
        Err(Error::Cancelled)
    ));

    // The tagged endpoint promises inspected UTF-8 truncation. The new
    // preview route owns omission without decoding; it cannot redefine this.
    for (name, truncated) in [
        ("over-boundary", true),
        ("over-binary", false),
        ("over-non-utf8", false),
    ] {
        let legacy: Value = http
            .get(format!(
                "{origin}/api/repos/team/repo/file?rev={commit}&path={name}"
            ))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            (legacy["text"].clone(), legacy["text_truncated"].clone()),
            (Value::Null, Value::Bool(truncated)),
            "{name}"
        );
    }
    let missing = http
        .get(format!(
            "{origin}/api/repos/team/repo/file-preview?rev={commit}&path=absent"
        ))
        .header(header::IF_NONE_MATCH, "*")
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    // Even a previously omitted result must recheck current captured metadata.
    // Keep control/footer bytes intact and corrupt the selected encoded entry.
    let mut corrupt = repository
        .layout
        .store()
        .get_with_etag(&capsule_path)
        .await
        .unwrap()
        .0
        .to_vec();
    let oid = &large_oids[1];
    let position = pack_offset + encoded_offsets[oid] + encoded_sizes[oid] - 1;
    corrupt[position as usize] ^= 1;
    repository
        .layout
        .store()
        .put_overwrite(&capsule_path, corrupt.into())
        .await
        .unwrap();
    let response = http
        .get(format!(
            "{origin}/api/repos/team/repo/file-preview?rev={commit}&path=large-binary"
        ))
        .header(header::IF_NONE_MATCH, "*")
        .send()
        .await
        .unwrap();
    let status = response.status();
    let error: Value = response.json().await.unwrap();
    assert_eq!(
        (status, error["error"]["code"].as_str()),
        (StatusCode::BAD_GATEWAY, Some("remote_read"))
    );
    crate::test_support::server::close(&server).await;
    task.await.unwrap();
    assert_eq!(reads.active.load(Ordering::SeqCst), 0);
}
