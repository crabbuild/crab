use super::*;
use flate2::{Compression, write::ZlibEncoder};
use std::{
    io::Cursor,
    process::{Command, Stdio},
};

fn limits() -> ReceiveLimits {
    ReceiveLimits {
        max_pack_bytes: 8 * 1024 * 1024,
        max_objects: 1000,
        max_object_bytes: 1024 * 1024,
        max_inflated_bytes: 16 * 1024 * 1024,
        max_delta_depth: 32,
    }
}
fn checksum(mut bytes: Vec<u8>) -> Vec<u8> {
    let hash = Sha1::digest(&bytes);
    bytes.extend_from_slice(&hash);
    bytes
}
fn pack(entries: &[(Header, Vec<u8>)]) -> Vec<u8> {
    let mut bytes = b"PACK\0\0\0\x02".to_vec();
    bytes.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (header, data) in entries {
        header.write_to(data.len() as u64, &mut bytes).unwrap();
        let mut encoder = ZlibEncoder::new(&mut bytes, Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap();
    }
    checksum(bytes)
}
fn no_base(
    _: &ObjectId,
) -> std::result::Result<Option<BaseObject>, Box<dyn std::error::Error + Send + Sync>> {
    Ok(None)
}

#[test]
fn generated_objects_spool_once_with_identity_and_resource_bounds() {
    let temp = tempfile::tempdir().unwrap();
    let generated = IncomingPack::from_generated_objects(
        [
            (Kind::Blob, b"first".to_vec()),
            (Kind::Blob, b"second".to_vec()),
            (Kind::Blob, b"first".to_vec()),
        ],
        temp.path(),
        limits(),
        || false,
    )
    .unwrap();

    assert_eq!(generated.received_objects(), 2);
    assert_eq!(
        generated
            .read_object(&object_id(Kind::Blob, b"second"))
            .unwrap()
            .unwrap()
            .data,
        b"second"
    );
    assert!(matches!(
        IncomingPack::from_generated_objects(
            [(Kind::Blob, vec![0; 1024 * 1024 + 1])],
            temp.path(),
            limits(),
            || false,
        ),
        Err(IncomingPackError::Limit("object size"))
    ));
    assert!(matches!(
        IncomingPack::from_generated_objects(
            [(Kind::Blob, Vec::new())],
            temp.path(),
            limits(),
            || true,
        ),
        Err(IncomingPackError::Cancelled)
    ));
}

#[test]
fn resolves_forward_ref_deltas_and_cleans_private_spools() {
    let temp = tempfile::tempdir().unwrap();
    let oid = object_id(Kind::Blob, b"abc");
    let bytes = pack(&[
        (
            Header::RefDelta { base_id: oid },
            vec![3, 4, 0x90, 3, 1, b'd'],
        ),
        (Header::Blob, b"abc".to_vec()),
    ]);
    let accepted = quarantine(
        Cursor::new(bytes),
        temp.path(),
        limits(),
        || false,
        |_| panic!("in-pack base should not be fetched"),
    )
    .unwrap();
    let result = accepted
        .read_object(&object_id(Kind::Blob, b"abcd"))
        .unwrap()
        .unwrap();
    assert_eq!(result.data, b"abcd");
    assert_eq!(accepted.objects().count(), 2);
    let directory = accepted.directory.path().to_owned();
    drop(accepted);
    assert!(!directory.exists());
}

#[test]
fn resolves_thin_delta_and_verifies_external_base_identity() {
    let temp = tempfile::tempdir().unwrap();
    let oid = object_id(Kind::Blob, b"abc");
    let bytes = pack(&[(
        Header::RefDelta { base_id: oid },
        vec![3, 4, 0x90, 3, 1, b'd'],
    )]);
    let accepted = quarantine(
        Cursor::new(&bytes),
        temp.path(),
        limits(),
        || false,
        |requested| {
            assert_eq!(*requested, oid);
            Ok(Some(BaseObject {
                kind: Kind::Blob,
                data: b"abc".to_vec(),
            }))
        },
    )
    .unwrap();
    assert_eq!(
        accepted
            .read_object(&object_id(Kind::Blob, b"abcd"))
            .unwrap()
            .unwrap()
            .data,
        b"abcd"
    );
    assert!(matches!(
        quarantine(
            Cursor::new(bytes),
            temp.path(),
            limits(),
            || false,
            |_| Ok(Some(BaseObject {
                kind: Kind::Blob,
                data: b"bad".to_vec()
            }))
        ),
        Err(IncomingPackError::Invalid(
            "external base identity mismatch"
        ))
    ));
}

#[test]
fn rejects_corrupt_truncated_extra_and_oversized_pack_data() {
    let temp = tempfile::tempdir().unwrap();
    let valid = pack(&[(Header::Blob, b"abcdef".to_vec())]);
    let mut corrupt = valid.clone();
    corrupt[13] ^= 1;
    let mut truncated = valid[..valid.len() - 22].to_vec();
    truncated = checksum(truncated);
    let mut extra = valid[..valid.len() - 20].to_vec();
    extra.extend_from_slice(b"extra");
    extra = checksum(extra);
    let mut wrong_count = valid[..valid.len() - 20].to_vec();
    wrong_count[11] = 0;
    wrong_count = checksum(wrong_count);
    let mut wrong_size = valid[..valid.len() - 20].to_vec();
    wrong_size[12] = 0x31;
    wrong_size = checksum(wrong_size);
    for (name, bytes) in [
        ("checksum", corrupt),
        ("zlib trailer", truncated),
        ("trailing bytes", extra),
        ("count", wrong_count),
        ("entry size", wrong_size),
    ] {
        assert!(
            quarantine(Cursor::new(bytes), temp.path(), limits(), || false, no_base).is_err(),
            "{name}"
        );
        assert_eq!(
            temp.path().read_dir().unwrap().count(),
            0,
            "cleanup: {name}"
        );
    }
    for constrained in [
        ReceiveLimits {
            max_pack_bytes: valid.len() as u64 - 1,
            ..limits()
        },
        ReceiveLimits {
            max_objects: 0,
            ..limits()
        },
        ReceiveLimits {
            max_object_bytes: 5,
            ..limits()
        },
        ReceiveLimits {
            max_inflated_bytes: 5,
            ..limits()
        },
    ] {
        assert!(matches!(
            quarantine(
                Cursor::new(&valid),
                temp.path(),
                constrained,
                || false,
                no_base
            ),
            Err(IncomingPackError::Limit(_))
        ));
    }
}

#[test]
fn rejects_missing_bases_invalid_offsets_depth_and_cancelled_intake() {
    let temp = tempfile::tempdir().unwrap();
    let oid = object_id(Kind::Blob, b"abc");
    let bytes = pack(&[(
        Header::RefDelta { base_id: oid },
        vec![3, 4, 0x90, 3, 1, b'd'],
    )]);
    assert!(matches!(
        quarantine(
            Cursor::new(&bytes),
            temp.path(),
            limits(),
            || false,
            no_base
        ),
        Err(IncomingPackError::MissingBase(_))
    ));
    assert!(matches!(
        quarantine(
            Cursor::new(&bytes),
            temp.path(),
            ReceiveLimits {
                max_delta_depth: 0,
                ..limits()
            },
            || false,
            |_| Ok(Some(BaseObject {
                kind: Kind::Blob,
                data: b"abc".to_vec()
            }))
        ),
        Err(IncomingPackError::Limit("delta depth"))
    ));
    let invalid = pack(&[(
        Header::OfsDelta { base_distance: 1 },
        vec![3, 4, 0x90, 3, 1, b'd'],
    )]);
    assert!(
        quarantine(
            Cursor::new(invalid),
            temp.path(),
            limits(),
            || false,
            no_base
        )
        .is_err()
    );
    assert!(matches!(
        quarantine(Cursor::new(bytes), temp.path(), limits(), || true, no_base),
        Err(IncomingPackError::Cancelled)
    ));
    assert_eq!(temp.path().read_dir().unwrap().count(), 0);
}

#[test]
fn rejects_overlong_headers_and_cancellation_after_base_lookup() {
    let temp = tempfile::tempdir().unwrap();
    let mut bytes = b"PACK\0\0\0\x02\0\0\0\x01\x60".to_vec();
    bytes.extend_from_slice(&[0x80; 40]);
    bytes.push(0);
    assert!(
        quarantine(
            Cursor::new(checksum(bytes)),
            temp.path(),
            limits(),
            || false,
            no_base
        )
        .is_err()
    );
    let cancel = std::cell::Cell::new(false);
    let bytes = pack(&[(
        Header::RefDelta {
            base_id: object_id(Kind::Blob, b"abc"),
        },
        vec![3, 4, 0x90, 3, 1, b'd'],
    )]);
    let result = quarantine(
        Cursor::new(bytes),
        temp.path(),
        limits(),
        || cancel.get(),
        |_| {
            cancel.set(true);
            Ok(Some(BaseObject {
                kind: Kind::Blob,
                data: b"abc".to_vec(),
            }))
        },
    );
    assert!(matches!(result, Err(IncomingPackError::Cancelled)));
    assert_eq!(temp.path().read_dir().unwrap().count(), 0);
}

#[test]
fn preserves_base_lookup_source_errors() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = pack(&[(
        Header::RefDelta {
            base_id: object_id(Kind::Blob, b"abc"),
        },
        vec![3, 4, 0x90, 3, 1, b'd'],
    )]);
    let result = quarantine(
        Cursor::new(bytes),
        temp.path(),
        limits(),
        || false,
        |_| {
            Err(Box::new(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "lookup denied",
            )))
        },
    );
    let Err(IncomingPackError::BaseLookup { source, .. }) = result else {
        panic!("expected source error")
    };
    assert_eq!(
        source.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
}

fn git(dir: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env("GIT_AUTHOR_NAME", "Crab")
        .env("GIT_AUTHOR_EMAIL", "crab@example.invalid")
        .env("GIT_COMMITTER_NAME", "Crab")
        .env("GIT_COMMITTER_EMAIL", "crab@example.invalid")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn native_git_full_and_thin_packs_reconstruct_byte_identical_objects() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-q"], b"");
    let mut contents = (0..3000)
        .map(|n| format!("Line {n}: deterministic content for delta compression\n"))
        .collect::<String>();
    std::fs::write(source.join("file"), &contents).unwrap();
    git(&source, &["add", "file"], b"");
    git(&source, &["commit", "-qm", "base"], b"");
    contents.push_str("A small incremental edit\n");
    std::fs::write(source.join("file"), &contents).unwrap();
    git(&source, &["add", "file"], b"");
    git(&source, &["commit", "-qm", "update"], b"");
    for (thin, revisions) in [(false, "HEAD\n"), (true, "HEAD\n^HEAD^\n")] {
        let mut args = vec!["pack-objects", "--stdout", "--revs", "--delta-base-offset"];
        if thin {
            args.push("--thin");
        }
        let bytes = git(&source, &args, revisions.as_bytes());
        let mut lookups = 0;
        let accepted = quarantine(
            Cursor::new(bytes),
            temp.path(),
            limits(),
            || false,
            |oid| {
                lookups += 1;
                let kind = git(&source, &["cat-file", "-t", &oid.to_string()], b"");
                let kind = Kind::from_bytes(kind.strip_suffix(b"\n").unwrap()).unwrap();
                let data = git(
                    &source,
                    &["cat-file", &kind.to_string(), &oid.to_string()],
                    b"",
                );
                Ok(Some(BaseObject { kind, data }))
            },
        )
        .unwrap();
        if thin {
            assert!(lookups > 0, "fixture must actually use a thin base");
        } else {
            assert_eq!(lookups, 0);
        }
        let listed = git(&source, &["rev-list", "--objects", "HEAD"], b"");
        for line in String::from_utf8(listed).unwrap().lines() {
            let oid = ObjectId::from_hex(line.split(' ').next().unwrap().as_bytes()).unwrap();
            if let Some(actual) = accepted.read_object(&oid).unwrap() {
                assert_eq!(
                    actual.data,
                    git(
                        &source,
                        &["cat-file", &actual.kind.to_string(), &oid.to_string()],
                        b""
                    )
                );
            } else {
                assert!(thin, "full pack omitted {oid}");
            }
        }
        verify_prepared_with_native_git(&accepted, temp.path());
        let expected = object_id(Kind::Blob, contents.as_bytes());
        assert_eq!(
            accepted.read_object(&expected).unwrap().unwrap().data,
            contents.as_bytes()
        );
    }
}

fn verify_prepared_with_native_git(accepted: &IncomingPack, temp: &Path) {
    use std::sync::atomic::AtomicBool;
    let prepared = accepted
        .prepare(temp, 16 * 1024 * 1024, &AtomicBool::new(false))
        .unwrap()
        .unwrap();
    let path = prepared.pack_path().parent().unwrap().to_owned();
    verify_prepared_artifacts_with_native_git(accepted, &prepared, temp);
    drop(prepared);
    assert!(!path.exists());
}

fn verify_prepared_artifacts_with_native_git(
    accepted: &IncomingPack,
    prepared: &PreparedPack,
    temp: &Path,
) -> Vec<u8> {
    let bytes = std::fs::read(prepared.pack_path()).unwrap();
    assert_eq!(prepared.content_hash(), blake3::hash(&bytes));
    assert_eq!(prepared.size(), bytes.len() as u64);
    assert_eq!(prepared.object_count() as usize, accepted.objects().count());
    assert_eq!(prepared.git_sha1().as_bytes(), &bytes[bytes.len() - 20..]);
    let locations = crate::PackLocationIter::open(
        prepared.index_path(),
        prepared.reverse_path(),
        prepared.size(),
    )
    .unwrap();
    let mut kinds =
        crate::decode_pack_kind_metadata(&std::fs::read(prepared.kinds_path()).unwrap(), locations)
            .unwrap();
    kinds.sort_unstable_by_key(|(oid, _)| *oid);
    let expected = accepted
        .objects()
        .map(|o| (o.oid, o.kind))
        .collect::<Vec<_>>();
    assert_eq!(kinds, expected);

    // Native Git sees only the new self-contained pack, with no alternates or
    // source repository. It independently reconstructs every quarantined object.
    let client = tempfile::tempdir_in(temp).unwrap();
    git(client.path(), &["init", "--bare", "-q"], b"");
    let base = client
        .path()
        .join("objects/pack")
        .join(format!("pack-{}", prepared.git_sha1()));
    for (source, extension) in [
        (prepared.pack_path(), "pack"),
        (prepared.index_path(), "idx"),
        (prepared.reverse_path(), "rev"),
    ] {
        std::fs::copy(source, base.with_extension(extension)).unwrap();
    }
    let verification = git(
        client.path(),
        &[
            "verify-pack",
            "-v",
            base.with_extension("idx").to_str().unwrap(),
        ],
        b"",
    );
    let oracle = client.path().join("oracle.idx");
    git(
        client.path(),
        &[
            "index-pack",
            "--index-version=2",
            "-o",
            oracle.to_str().unwrap(),
            base.with_extension("pack").to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(
        std::fs::read(oracle).unwrap(),
        std::fs::read(prepared.index_path()).unwrap()
    );
    for object in accepted.objects() {
        let bytes = git(
            client.path(),
            &[
                "cat-file",
                &object.kind.to_string(),
                &object.oid.to_string(),
            ],
            b"",
        );
        assert_eq!(
            bytes,
            accepted.read_object(&object.oid).unwrap().unwrap().data
        );
    }
    assert!(accepted.directory.path().exists());
    verification
}

#[test]
fn generated_delta_bases_produce_bounded_native_git_tree_deltas() {
    use std::sync::atomic::AtomicBool;

    let temp = tempfile::tempdir().unwrap();
    let mut base = Vec::new();
    let mut changed_offset = 0;
    for index in 0..4_000 {
        base.extend_from_slice(format!("100644 item-{index:05}\0").as_bytes());
        if index == 2_000 {
            changed_offset = base.len();
        }
        base.extend_from_slice(&[((index % 251) + 1) as u8; 20]);
    }
    let mut target = base.clone();
    target[changed_offset] ^= 1;
    let base_oid = object_id(Kind::Tree, &base);
    let target_oid = object_id(Kind::Tree, &target);
    let accepted = IncomingPack::from_generated_objects(
        [(Kind::Tree, target), (Kind::Tree, base)],
        temp.path(),
        ReceiveLimits {
            max_pack_bytes: 16 * 1024 * 1024,
            max_objects: 10,
            max_object_bytes: 1024 * 1024,
            max_inflated_bytes: 2 * 1024 * 1024,
            max_delta_depth: 8,
        },
        || false,
    )
    .unwrap();
    let flag = AtomicBool::new(false);
    let full = accepted
        .prepare(temp.path(), 16 * 1024 * 1024, &flag)
        .unwrap()
        .unwrap();
    let delta = accepted
        .prepare_with_delta_bases(
            temp.path(),
            16 * 1024 * 1024,
            &flag,
            &BTreeMap::from([(target_oid, base_oid)]),
            1,
            1024 * 1024,
        )
        .unwrap()
        .unwrap();

    assert!(delta.size() < full.size());
    let verification = verify_prepared_artifacts_with_native_git(&accepted, &delta, temp.path());
    let target_line = String::from_utf8(verification)
        .unwrap()
        .lines()
        .find(|line| line.starts_with(&target_oid.to_string()))
        .unwrap()
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let base_hex = base_oid.to_string();
    assert_eq!(target_line.get(5).map(String::as_str), Some("1"));
    assert_eq!(
        target_line.get(6).map(String::as_str),
        Some(base_hex.as_str())
    );
}

#[test]
fn generated_external_tree_base_produces_bounded_cross_pack_ref_delta() {
    use std::sync::atomic::AtomicBool;

    let temp = tempfile::tempdir().unwrap();
    let mut base = Vec::new();
    let mut changed_offset = 0;
    for index in 0..4_000 {
        base.extend_from_slice(format!("100644 item-{index:05}\0").as_bytes());
        if index == 2_000 {
            changed_offset = base.len();
        }
        base.extend_from_slice(&[((index % 251) + 1) as u8; 20]);
    }
    let mut target = base.clone();
    target[changed_offset] ^= 1;
    let base_oid = object_id(Kind::Tree, &base);
    let target_oid = object_id(Kind::Tree, &target);
    let accepted = IncomingPack::from_generated_objects(
        [(Kind::Tree, target.clone())],
        temp.path(),
        ReceiveLimits {
            max_pack_bytes: 16 * 1024 * 1024,
            max_objects: 10,
            max_object_bytes: 1024 * 1024,
            max_inflated_bytes: 2 * 1024 * 1024,
            max_delta_depth: 8,
        },
        || false,
    )
    .unwrap();
    let flag = AtomicBool::new(false);
    let full = accepted
        .prepare(temp.path(), 16 * 1024 * 1024, &flag)
        .unwrap()
        .unwrap();
    let delta = accepted
        .prepare_with_external_delta_bases(
            temp.path(),
            16 * 1024 * 1024,
            &flag,
            &BTreeMap::from([(target_oid, base_oid)]),
            &BTreeMap::from([(
                target_oid,
                ExternalDeltaBase::new(base_oid, Kind::Tree, base.clone(), 0),
            )]),
            8,
            1024 * 1024,
        )
        .unwrap()
        .unwrap();

    assert!(delta.size() < full.size());
    assert_eq!(delta.delta_depth(&target_oid), Some(1));
    assert_eq!(full.external_delta_count(), 0);
    assert_eq!(delta.external_delta_count(), 1);
    let client = tempfile::tempdir_in(temp.path()).unwrap();
    git(client.path(), &["init", "--bare", "-q"], b"");
    let written = git(
        client.path(),
        &["hash-object", "-w", "-t", "tree", "--stdin"],
        &base,
    );
    let base_hex = base_oid.to_string();
    assert_eq!(written.strip_suffix(b"\n"), Some(base_hex.as_bytes()));
    git(
        client.path(),
        &["index-pack", "--fix-thin", "--stdin"],
        &std::fs::read(delta.pack_path()).unwrap(),
    );
    assert_eq!(
        git(
            client.path(),
            &["cat-file", "tree", &target_oid.to_string()],
            b""
        ),
        target
    );
}

#[test]
fn prepared_artifacts_are_deterministic_and_empty_packs_need_no_artifacts() {
    use std::sync::atomic::AtomicBool;
    let temp = tempfile::tempdir().unwrap();
    let entries = [
        (Header::Blob, b"second".to_vec()),
        (Header::Blob, Vec::new()),
    ];
    let first = quarantine(
        Cursor::new(pack(&entries)),
        temp.path(),
        limits(),
        || false,
        no_base,
    )
    .unwrap();
    let mut reversed = entries.to_vec();
    reversed.reverse();
    reversed.push(entries[0].clone());
    let second = quarantine(
        Cursor::new(pack(&reversed)),
        temp.path(),
        limits(),
        || false,
        no_base,
    )
    .unwrap();
    let flag = AtomicBool::new(false);
    let a = first.prepare(temp.path(), 1024, &flag).unwrap().unwrap();
    let b = second.prepare(temp.path(), 1024, &flag).unwrap().unwrap();
    assert_eq!(a.content_hash(), b.content_hash());
    assert_eq!(
        std::fs::read(a.index_path()).unwrap(),
        std::fs::read(b.index_path()).unwrap()
    );
    verify_prepared_with_native_git(&first, temp.path());
    let empty = quarantine(
        Cursor::new(pack(&[])),
        temp.path(),
        limits(),
        || false,
        no_base,
    )
    .unwrap();
    assert!(empty.prepare(temp.path(), 0, &flag).unwrap().is_none());
}

#[test]
fn preparation_bounds_and_corruption_fail_without_retaining_artifacts() {
    use std::sync::atomic::AtomicBool;
    let temp = tempfile::tempdir().unwrap();
    let accepted = quarantine(
        Cursor::new(pack(&[(Header::Blob, b"test data".to_vec())])),
        temp.path(),
        limits(),
        || false,
        no_base,
    )
    .unwrap();
    let flag = AtomicBool::new(false);
    let prepared = accepted.prepare(temp.path(), 1024, &flag).unwrap().unwrap();
    let exact = prepared.size();
    drop(prepared);
    for limit in [0, 19, 31, exact - 1] {
        assert!(matches!(
            accepted.prepare(temp.path(), limit, &flag),
            Err(PreparePackError::Limit)
        ));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }
    assert!(
        accepted
            .prepare(temp.path(), exact, &flag)
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        accepted.prepare(temp.path(), 1024, &AtomicBool::new(true)),
        Err(PreparePackError::Cancelled)
    ));
    let spool = accepted.directory.path().join("objects");
    std::fs::write(&spool, b"bad bytes").unwrap();
    assert!(matches!(
        accepted.prepare(temp.path(), 1024, &flag),
        Err(PreparePackError::Mismatch("indexed object identities"))
    ));
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    std::fs::write(spool, b"short").unwrap();
    assert!(matches!(
        accepted.prepare(temp.path(), 1024, &flag),
        Err(PreparePackError::Mismatch("truncated object spool"))
    ));
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
}
