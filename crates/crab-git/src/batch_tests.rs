use super::*;
use std::cell::Cell;
use std::process::{Command, Stdio};

fn write_git_blob(git_dir: &Path, body: &[u8]) -> BlobHeader {
    let mut child = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(body).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oid =
        gix_hash::ObjectId::from_hex(String::from_utf8(output.stdout).unwrap().trim().as_bytes())
            .unwrap();
    BlobHeader {
        oid: oid.as_slice().try_into().unwrap(),
        size: body.len() as u64,
    }
}

fn frame(body: &[u8]) -> (BlobHeader, Vec<u8>) {
    let mut hash = gix_hash::hasher(gix_hash::Kind::Sha1);
    hash.update(format!("blob {}\0", body.len()).as_bytes());
    hash.update(body);
    let oid = hash.try_finalize().unwrap();
    let mut bytes = format!("{oid} blob {}\n", body.len()).into_bytes();
    bytes.extend_from_slice(body);
    bytes.push(b'\n');
    (
        BlobHeader {
            oid: oid.as_slice().try_into().unwrap(),
            size: body.len() as u64,
        },
        bytes,
    )
}

fn compressed(bytes: &[u8]) -> Vec<u8> {
    let mut writer = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::none());
    writer.write_all(bytes).unwrap();
    writer.finish().unwrap()
}

fn loose_frame(body: &[u8]) -> (BlobHeader, Vec<u8>) {
    let (blob, _) = frame(body);
    let bytes = [format!("blob {}\0", body.len()).into_bytes(), body.to_vec()].concat();
    (blob, compressed(&bytes))
}

fn initialize_bare(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    let result = Command::new("git")
        .args([
            "init",
            "--bare",
            "--quiet",
            "--object-format=sha1",
            "--template=",
        ])
        .current_dir(path)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(result.status.success(), "{:?}", result.stderr);
}

fn native_blob(git_dir: &Path, oid: &str) -> Vec<u8> {
    let result = Command::new("git")
        .args(["--no-replace-objects", "cat-file", "blob", oid])
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .output()
        .unwrap();
    assert!(result.status.success(), "{:?}", result.stderr);
    result.stdout
}

#[test]
fn loose_lookup_matches_native_nested_relative_alternates_and_cycles() {
    let root = tempfile::tempdir().unwrap();
    let primary = root.path().join("primary");
    let first = root.path().join("nested/first");
    let second = root.path().join("nested/deeper/second");
    for path in [&primary, &first, &second] {
        initialize_bare(path);
    }
    for (path, alternate) in [
        (&primary, "../../nested/first/objects\n"),
        (&first, "../../deeper/second/objects\n"),
        (&second, "../../../../primary/objects\n"),
    ] {
        std::fs::write(path.join("objects/info/alternates"), alternate).unwrap();
    }
    let blob = write_git_blob(&second, b"nested alternate bytes");
    let oid = gix_hash::ObjectId::Sha1(blob.oid).to_string();
    let file = open_loose_blob(&primary.join("objects"), &oid, &|| false)
        .unwrap()
        .unwrap();
    let mut bytes = Vec::new();
    copy_loose_blob(file, &oid, blob.size, &mut bytes, &|| false).unwrap();
    assert_eq!(bytes, native_blob(&primary, &oid));
    assert!(
        open_loose_blob(&primary.join("objects"), &"0".repeat(40), &|| false)
            .unwrap()
            .is_none()
    );
}

#[test]
fn loose_lookup_preserves_alternate_path_bytes_and_git_quoting() {
    let root = tempfile::tempdir().unwrap();
    let primary = root.path().join("primary");
    initialize_bare(&primary);
    let mut paths = Vec::with_capacity(4);
    paths.push((
        root.path().join("with space"),
        b"../../with space/objects\n".to_vec(),
    ));
    #[cfg(unix)]
    {
        paths.extend([
            (
                root.path().join("with\ttab"),
                b"\"../../with\\ttab/objects\"\n".to_vec(),
            ),
            (
                primary.join("objects/\"literal"),
                b"\"literal/objects\n".to_vec(),
            ),
        ]);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStringExt;
        paths.push((
            root.path()
                .join(std::ffi::OsString::from_vec(b"non-utf8-\xff".to_vec())),
            b"\"../../non-utf8-\\377/objects\"\n".to_vec(),
        ));
    }
    for (path, alternate) in paths {
        initialize_bare(&path);
        let blob = write_git_blob(&path, path.as_os_str().as_encoded_bytes());
        let oid = gix_hash::ObjectId::Sha1(blob.oid).to_string();
        std::fs::write(primary.join("objects/info/alternates"), alternate).unwrap();
        let expected = native_blob(&primary, &oid);
        let file = open_loose_blob(&primary.join("objects"), &oid, &|| false)
            .unwrap()
            .unwrap();
        let mut bytes = Vec::new();
        copy_loose_blob(file, &oid, blob.size, &mut bytes, &|| false).unwrap();
        assert_eq!(bytes, expected);
    }
}

#[cfg(unix)]
#[test]
fn quoted_alternate_paths_preserve_non_unicode_bytes() {
    let paths = alternate_paths(b"\"../../non-utf8-\\377/objects\"\n").unwrap();
    assert_eq!(
        paths[0].as_os_str().as_encoded_bytes(),
        b"../../non-utf8-\xff/objects"
    );
}

#[test]
fn loose_copies_are_bounded_and_preserve_exact_bytes() {
    struct Chopped(io::Cursor<Vec<u8>>);
    impl Read for Chopped {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            assert!(output.len() <= 64 * 1024);
            let count = output.len().min(7);
            self.0.read(&mut output[..count])
        }
    }
    for body in [vec![], vec![0, 255, b'\n', 13], vec![0x80; 256 * 1024]] {
        let (blob, bytes) = loose_frame(&body);
        let oid = gix_hash::ObjectId::Sha1(blob.oid).to_string();
        let mut output = Vec::new();
        copy_loose_blob(
            Chopped(io::Cursor::new(bytes)),
            &oid,
            blob.size,
            &mut output,
            &|| false,
        )
        .unwrap();
        assert_eq!(output, body);
    }
}

#[test]
fn loose_copies_reject_corrupt_headers_bodies_footers_and_extra_bytes() {
    let (blob, valid) = loose_frame(b"data");
    let oid = gix_hash::ObjectId::Sha1(blob.oid).to_string();
    let mut bad_footer = valid.clone();
    *bad_footer.last_mut().unwrap() ^= 1;
    for bytes in [
        compressed(b"tree 4\0data"),
        compressed(b"blob 5\0data"),
        compressed(b"blob 4\0evil"),
        compressed(b"blob 4\0dataextra"),
        [valid.clone(), vec![0]].concat(),
        bad_footer,
    ] {
        assert!(
            copy_loose_blob(bytes.as_slice(), &oid, blob.size, &mut io::sink(), &|| {
                false
            })
            .is_err()
        );
    }
    for end in 0..valid.len() {
        assert!(
            copy_loose_blob(&valid[..end], &oid, blob.size, &mut io::sink(), &|| false).is_err(),
            "truncated at {end} of {}",
            valid.len()
        );
    }
}

#[test]
fn verifies_binary_empty_and_large_blobs_without_large_reads() {
    struct BoundedReads(io::Cursor<Vec<u8>>);
    impl Read for BoundedReads {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            assert!(buffer.len() <= 64 * 1024);
            self.0.read(buffer)
        }
    }
    let mut expected = Vec::new();
    let mut bytes = Vec::new();
    for body in [vec![], vec![0, 255, b'\n', 13], vec![0x80; 256 * 1024]] {
        let (blob, wire) = frame(&body);
        expected.push(blob);
        bytes.extend(wire);
    }
    verify_blob_batch(BoundedReads(io::Cursor::new(bytes)), &expected, &|| false).unwrap();
}

#[test]
fn streaming_blob_copies_are_bounded_and_byte_exact() {
    struct Output(Vec<u8>);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            assert!(bytes.len() <= 64 * 1024);
            let count = bytes.len().min(7);
            self.0.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    for body in [vec![], vec![0, 255, b'\n', 13], vec![0x80; 256 * 1024]] {
        let (blob, wire) = frame(&body);
        let oid = gix_hash::ObjectId::Sha1(blob.oid).to_string();
        let mut output = Output(Vec::new());
        copy_blob(wire.as_slice(), &oid, blob.size, &mut output, &|| false).unwrap();
        assert_eq!(output.0, body);
    }
}

#[test]
fn streaming_blob_copies_reject_corruption_and_invalid_framing() {
    let (blob, valid) = frame(b"data");
    let oid = gix_hash::ObjectId::Sha1(blob.oid).to_string();
    for wire in [
        format!("{oid} blob 4\nevil\n").into_bytes(),
        format!("{oid} tree 4\ndata\n").into_bytes(),
        format!("{oid} blob 5\ndata\n").into_bytes(),
        format!("{oid} blob 4\ndata!").into_bytes(),
        [valid, b"extra".to_vec()].concat(),
    ] {
        assert!(copy_blob(wire.as_slice(), &oid, blob.size, &mut io::sink(), &|| false).is_err());
    }
}

#[test]
fn streaming_blob_copy_preserves_writer_errors() {
    struct Denied;
    impl Write for Denied {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let (blob, wire) = frame(b"data");
    let oid = gix_hash::ObjectId::Sha1(blob.oid).to_string();
    assert_eq!(
        copy_blob(wire.as_slice(), &oid, blob.size, &mut Denied, &|| false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let (_, loose) = loose_frame(b"data");
    assert_eq!(
        copy_loose_blob(loose.as_slice(), &oid, blob.size, &mut Denied, &|| false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn git_dir_verifier_handles_large_output_before_a_large_request_list() {
    let repo = tempfile::tempdir().unwrap();
    let output = Command::new("git")
        .args(["init", "--bare", "--quiet"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let large = write_git_blob(repo.path(), &vec![0x80; 2 * 1024 * 1024]);
    let small = write_git_blob(repo.path(), b"small");
    let mut expected = Vec::with_capacity(5_001);
    expected.push(large);
    expected.extend(std::iter::repeat_n(small, 5_000));

    verify_git_dir_blobs(repo.path(), &expected, &|| false).unwrap();
}

#[test]
fn malformed_reordered_truncated_and_extra_responses_never_verify() {
    let (blob, valid) = frame(b"data");
    let oid = gix_hash::ObjectId::Sha1(blob.oid);
    for wire in [
        format!("{oid} missing\n").into_bytes(),
        format!("{oid} tree 4\ndata\n").into_bytes(),
        format!("{oid} blob 5\ndata\n").into_bytes(),
        format!("{oid} blob 4\nevil\n").into_bytes(),
        format!("{oid} blob 4\ndata!").into_bytes(),
        vec![b'x'; 81],
        [valid.clone(), b"extra".to_vec()].concat(),
        frame(b"other").1,
    ] {
        assert!(verify_blob_batch(wire.as_slice(), &[blob], &|| false).is_err());
    }
    let error = verify_blob_batch(format!("{oid} blob 5\ndata\n").as_bytes(), &[blob], &|| {
        false
    })
    .unwrap_err();
    assert!(error.to_string().contains("expected size Some(4)"));
    assert!(error.to_string().contains("header bytes"));
    for length in 0..valid.len() {
        assert!(
            verify_blob_batch(&valid[..length], &[blob], &|| false).is_err(),
            "length={length}"
        );
    }
}

#[test]
fn cancellation_and_reader_failures_do_not_become_integrity_proof() {
    let (blob, bytes) = frame(&vec![3; 256 * 1024]);
    let calls = Cell::new(0);
    let result = verify_blob_batch(bytes.as_slice(), &[blob], &|| {
        calls.set(calls.get() + 1);
        calls.get() == 3
    });
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
    calls.set(0);
    let oid = gix_hash::ObjectId::Sha1(blob.oid).to_string();
    let error = copy_blob(bytes.as_slice(), &oid, blob.size, &mut io::sink(), &|| {
        calls.set(calls.get() + 1);
        calls.get() == 3
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    let (_, loose) = loose_frame(&vec![3; 256 * 1024]);
    calls.set(0);
    let error = copy_loose_blob(loose.as_slice(), &oid, blob.size, &mut io::sink(), &|| {
        calls.set(calls.get() + 1);
        calls.get() == 3
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    struct Denied;
    impl Read for Denied {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
        }
    }
    assert_eq!(
        verify_blob_batch(Denied, &[blob], &|| false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn small_blob_visitors_preserve_request_ordinals_and_verify_large_bodies() {
    let (small, first) = frame(b"one");
    let (large, second) = frame(&vec![0x80; 256 * 1024]);
    let ids = [small, large, small].map(|blob| gix_hash::ObjectId::Sha1(blob.oid).to_string());
    let wire = [first.clone(), second.clone(), first].concat();
    let mut visited = Vec::new();
    visit_small_blobs(
        wire.as_slice(),
        ids.iter().map(String::as_str),
        3,
        &|| false,
        |index, bytes| {
            visited.push((index, bytes.to_vec()));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(visited, [(0, b"one".to_vec()), (2, b"one".to_vec())]);

    let mut corrupt = second;
    let body = corrupt.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    corrupt[body] ^= 1;
    let error = visit_small_blobs(
        corrupt.as_slice(),
        [ids[1].as_str()],
        3,
        &|| false,
        |_, _| panic!("a large object is never a retained pointer candidate"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("checksum differs"));
}

#[test]
fn sha256_blob_identity_is_verified_without_enabling_native_sha256_transport() {
    let body = b"sha256 Git blob\0\xff";
    let mut hasher = sha2::Sha256::new();
    hasher.update(format!("blob {}\0", body.len()).as_bytes());
    hasher.update(body);
    let oid = format!("{:x}", hasher.finalize());
    let wire = [
        format!("{oid} blob {}\n", body.len()).into_bytes(),
        body.to_vec(),
        vec![b'\n'],
    ]
    .concat();
    let mut visited = Vec::new();
    visit_small_blobs(
        wire.as_slice(),
        [oid.as_str()],
        body.len(),
        &|| false,
        |_, bytes| {
            visited.extend_from_slice(bytes);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(visited, body);
    let mut copied = Vec::new();
    copy_blob(
        wire.as_slice(),
        &oid,
        body.len() as u64,
        &mut copied,
        &|| false,
    )
    .unwrap();
    assert_eq!(copied, body);
    let wrong = "0".repeat(64);
    assert!(
        visit_small_blobs(
            wire.as_slice(),
            [wrong.as_str()],
            body.len(),
            &|| false,
            |_, _| Ok(())
        )
        .is_err()
    );
}

#[test]
fn sha1_collision_errors_preserve_the_gitoxide_source_and_digest() {
    for input in [
        include_bytes!("../tests/fixtures/sha1-collisions/sha-mbles-1.bin").as_slice(),
        include_bytes!("../tests/fixtures/sha1-collisions/sha-mbles-2.bin").as_slice(),
    ] {
        for chunk_size in [1, 7, 63, 64, 65, 4096] {
            let mut original = gix_hash::hasher(gix_hash::Kind::Sha1);
            let mut batch = ObjectHasher::new(&"0".repeat(40)).unwrap();
            for bytes in input.chunks(chunk_size) {
                original.update(bytes);
                batch.update(bytes);
            }
            let gix_hash::hasher::Error::CollisionAttack { digest: expected } =
                original.try_finalize().unwrap_err();
            let error = batch.finalize().unwrap_err();
            let source = error
                .get_ref()
                .unwrap()
                .downcast_ref::<gix_hash::hasher::Error>()
                .unwrap();
            let gix_hash::hasher::Error::CollisionAttack { digest } = source;
            assert_eq!((error.kind(), *digest), (io::ErrorKind::Other, expected));
        }
    }
}
