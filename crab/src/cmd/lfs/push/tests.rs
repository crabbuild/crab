use super::*;
use sha2::Digest;

mod all;

fn oid(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn git_oid(byte: u8) -> String {
    format!("{byte:02x}").repeat(20)
}

#[test]
fn pre_push_revisions_preserve_updates_and_skip_deletes() {
    let input = format!(
        "refs/heads/main {} refs/heads/main {}\n(delete) {} refs/heads/old {}\n",
        git_oid(1),
        git_oid(2),
        "0".repeat(40),
        git_oid(3),
    );

    let updates = read_pre_push(input.as_bytes(), 4096).unwrap();
    let (local, remote) = pre_push_revisions(&updates);

    assert_eq!(local, vec![git_oid(1)]);
    assert_eq!(remote, vec![git_oid(2)]);
}

#[test]
fn pre_push_revisions_use_oids_for_tags_and_differently_named_destinations() {
    let input = format!(
        "HEAD~ {} refs/heads/release {}\nrefs/tags/v1 {} refs/tags/published {}\n",
        git_oid(1),
        git_oid(2),
        git_oid(3),
        git_oid(0),
    );
    let updates = read_pre_push(input.as_bytes(), 4096).unwrap();
    let (local, remote) = pre_push_revisions(&updates);
    assert_eq!(
        (local, remote),
        (vec![git_oid(1), git_oid(3)], vec![git_oid(2)])
    );
}

#[test]
fn missing_remote_manifest_is_an_empty_base_tip_set() {
    let store =
        crate::storage::Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()));
    let context = crate::cmd::lfs::store_setup::LfsRemoteContext {
        store: std::sync::Arc::new(crab_lfs::LfsObjectStore::new(
            store.into(),
            "org/lfs-pre-push",
        )),
        local_lfs_dir: std::path::PathBuf::new(),
        config: crate::lfs::config::LfsConfig::default(),
        prefix: "org/lfs-pre-push".to_owned(),
    };

    assert!(load_remote_manifest_ref_tips(&context).unwrap().is_empty());
}

#[test]
fn resolve_push_args_accepts_legacy_single_object_id() {
    let options = LfsPushOptions {
        object_id: Some(Some(oid(1))),
        ..LfsPushOptions::default()
    };

    let resolved = resolve_push_args(&options).unwrap();
    assert_eq!(
        resolved,
        ResolvedPushArgs {
            remote: None,
            refs: Vec::new(),
            object_ids: vec![oid(1)],
        }
    );
}

#[test]
fn resolve_push_args_accepts_multiple_object_ids() {
    let options = LfsPushOptions {
        remote: Some(oid(2)),
        args: vec![oid(3)],
        object_id: Some(Some(oid(1))),
        ..LfsPushOptions::default()
    };

    let resolved = resolve_push_args(&options).unwrap();
    assert_eq!(resolved.remote, None);
    assert_eq!(resolved.object_ids, vec![oid(1), oid(2), oid(3)]);
}

#[test]
fn resolve_push_args_treats_non_oid_object_id_value_as_remote() {
    let options = LfsPushOptions {
        remote: Some(oid(4)),
        object_id: Some(Some("origin".to_owned())),
        ..LfsPushOptions::default()
    };

    let resolved = resolve_push_args(&options).unwrap();
    assert_eq!(resolved.remote.as_deref(), Some("origin"));
    assert_eq!(resolved.object_ids, vec![oid(4)]);
}

#[test]
fn resolve_push_args_keeps_ref_operands() {
    let options = LfsPushOptions {
        remote: Some("origin".to_owned()),
        args: vec!["main".to_owned(), "release".to_owned()],
        ..LfsPushOptions::default()
    };

    let resolved = resolve_push_args(&options).unwrap();
    assert_eq!(resolved.remote.as_deref(), Some("origin"));
    assert_eq!(resolved.refs, vec!["main", "release"]);
}

#[test]
fn resolve_push_args_keeps_object_id_remote_operand() {
    let options = LfsPushOptions {
        remote: Some("origin".to_owned()),
        args: vec![oid(2)],
        object_id: Some(Some(oid(1))),
        ..LfsPushOptions::default()
    };

    let resolved = resolve_push_args(&options).unwrap();

    assert_eq!(resolved.remote.as_deref(), Some("origin"));
    assert_eq!(resolved.object_ids, vec![oid(1), oid(2)]);
}

#[test]
fn malformed_object_id_operands_are_not_silently_dropped() {
    let options = LfsPushOptions {
        object_id: Some(Some("origin".to_owned())),
        remote: Some(oid(1)),
        args: vec!["not-an-oid".to_owned()],
        ..LfsPushOptions::default()
    };
    assert!(matches!(
        resolve_push_args(&options),
        Err(CrabError::Configuration { .. })
    ));
}

#[test]
fn ambiguous_object_id_remotes_are_rejected() {
    let options = LfsPushOptions {
        object_id: Some(Some("origin".to_owned())),
        remote: Some("other".to_owned()),
        args: vec![oid(1)],
        ..LfsPushOptions::default()
    };
    assert!(matches!(
        resolve_push_args(&options),
        Err(CrabError::Configuration { .. })
    ));
}

#[test]
fn stdin_conflicts_are_rejected_without_reading_input() {
    for options in [
        LfsPushOptions {
            all: true,
            object_id: Some(None),
            ..LfsPushOptions::default()
        },
        LfsPushOptions {
            args: vec!["main".to_owned()],
            ..LfsPushOptions::default()
        },
        LfsPushOptions {
            object_id: Some(Some(oid(1))),
            ..LfsPushOptions::default()
        },
    ] {
        let options = LfsPushOptions {
            stdin: true,
            ..options
        };
        assert!(matches!(
            resolve_push_args(&options),
            Err(CrabError::Configuration { .. })
        ));
    }
}

#[test]
fn object_id_stdin_defers_selection_without_defaulting_to_refs() {
    let options = LfsPushOptions {
        remote: Some("origin".to_owned()),
        object_id: Some(None),
        stdin: true,
        ..LfsPushOptions::default()
    };
    assert_eq!(
        resolve_push_args(&options).unwrap(),
        ResolvedPushArgs {
            remote: Some("origin".to_owned()),
            refs: Vec::new(),
            object_ids: Vec::new(),
        }
    );
}

#[test]
fn object_id_validation_rejects_non_ascii_and_malformed_values() {
    for invalid in [
        "",
        "not-an-oid",
        "éééééééééé",
        &"a".repeat(63),
        &"a".repeat(65),
    ] {
        assert!(matches!(
            validate_object_ids(&[oid(1), invalid.to_owned()]),
            Err(CrabError::Configuration { .. })
        ));
    }
    assert!(validate_object_ids(&[oid(1), "AB".repeat(32)]).is_ok());
}

#[test]
fn object_id_pointers_read_cached_file_size() {
    let dir = tempfile::tempdir().unwrap();
    let content = b"object-id upload";
    let oid: [u8; 32] = sha2::Sha256::digest(content).into();
    let path = crate::lfs::cache::object_path(dir.path(), &oid);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();

    let pointers = object_id_pointers(dir.path(), &[hex_encode(&oid)]).unwrap();

    assert_eq!(pointers.len(), 1);
    assert_eq!(pointers[0].oid, oid);
    assert_eq!(pointers[0].size, content.len() as u64);
}

#[test]
fn object_id_pointers_reject_missing_cached_file() {
    let dir = tempfile::tempdir().unwrap();
    let oid = oid(1);

    let error = object_id_pointers(dir.path(), &[oid.clone()]).unwrap_err();

    assert!(matches!(error, CrabError::LfsObjectMissing { oid: found } if found == oid));
}
