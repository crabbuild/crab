use crab_sdk::{ErrorKind, GitPath, HashAlgorithm, ObjectId, Revision};

#[test]
fn object_ids_preserve_sha1_and_reject_other_formats() {
    let hex = "0123456789ABCDEF0123456789ABCDEF01234567";
    let oid = ObjectId::from_hex(hex).unwrap();
    assert_eq!(
        (oid.algorithm(), oid.to_string()),
        (HashAlgorithm::Sha1, hex.to_lowercase())
    );
    for (input, kind) in [
        ("1".repeat(64), ErrorKind::UnsupportedCapability),
        ("1".repeat(39), ErrorKind::InvalidInput),
        ("x".repeat(40), ErrorKind::InvalidInput),
        (String::new(), ErrorKind::InvalidInput),
    ] {
        assert_eq!(ObjectId::from_hex(&input).unwrap_err().kind(), kind);
    }
}

#[test]
fn git_paths_preserve_non_utf8_without_allowing_parent_traversal() {
    let bytes = b"directory/\xff.bin".as_slice();
    assert_eq!(GitPath::new(bytes).unwrap().as_bytes(), bytes);
    for invalid in [
        b"".as_slice(),
        b"/a",
        b"a/",
        b"a//b",
        b"..",
        b"a/../b",
        b"a\0b",
    ] {
        assert_eq!(
            GitPath::new(invalid).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }
}

#[test]
fn root_is_explicit_and_revision_names_are_unambiguous() {
    assert!(GitPath::root().is_root());
    assert_ne!(
        Revision::branch("release").unwrap(),
        Revision::tag("release").unwrap()
    );
    for invalid in [
        "",
        "bad..name",
        "bad.lock",
        "bad/name.lock",
        "/leading",
        "trailing/",
        "a@{b",
    ] {
        assert!(Revision::branch(invalid).is_err(), "{invalid}");
        assert!(Revision::tag(invalid).is_err(), "{invalid}");
    }
    assert!(Revision::branch("HEAD").is_err());
}
