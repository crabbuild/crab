use super::*;
use std::cell::Cell;

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
