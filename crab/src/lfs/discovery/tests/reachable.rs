use super::*;

#[test]
fn reachable_parser_keeps_unnamed_objects_and_rejects_invalid_records() {
    for oid in [git_oid(3), "ab".repeat(32)] {
        for name in ["", " path with spaces\tand tabs"] {
            let record = format!("{oid}{name}");
            assert_eq!(
                parse_reachable_object(record.as_bytes(), &mut ()).unwrap(),
                Some((
                    oid.clone(),
                    name.strip_prefix(' ').unwrap_or(name).to_owned()
                ))
            );
        }
    }
    for record in [
        b"".as_slice(),
        b"not-an-oid path",
        b"path=filename",
        &[0xff],
    ] {
        assert!(parse_reachable_object(record, &mut ()).is_err());
    }
}

#[test]
fn reachable_scan_bounds_records_and_batches_without_retaining_the_graph() {
    let record = format!("{} asset.bin\n", git_oid(3));
    let bytes = std::mem::size_of::<(String, String)>() + 40 + "asset.bin".len();
    let read = |input: &[u8], budget, cancel: &CancellationToken| -> Result<Vec<usize>> {
        let mut sizes = Vec::new();
        read_discovery(
            input,
            b'\n',
            parse_reachable_object,
            cancel,
            budget,
            |batch| {
                sizes.push(batch.len());
                Ok(())
            },
        )?;
        Ok(sizes)
    };
    let cancel = CancellationToken::new();
    assert_eq!(
        read(record.repeat(100).as_bytes(), (bytes * 3) as u64, &cancel).unwrap(),
        [vec![3; 33], vec![1]].concat()
    );
    assert!(read(record.trim_end().as_bytes(), MAX_CAPTURE_BYTES, &cancel).is_err());
    assert!(read(record.as_bytes(), bytes as u64 - 1, &cancel).is_err());
    assert!(read(&vec![b'x'; 1024 * 1024 + 1], MAX_CAPTURE_BYTES, &cancel).is_err());
    cancel.cancel();
    assert!(matches!(
        read(record.as_bytes(), MAX_CAPTURE_BYTES, &cancel),
        Err(CrabError::Cancelled)
    ));
}

#[test]
fn all_fetch_includes_a_pointer_referenced_only_by_a_blob_tag() {
    let (dir, oid, pointer) = pointer_object_fixture();
    let output = git_command_in(dir.path(), GitObjectAccess::LocalOnly)
        .args(["update-ref", "refs/tags/blob", &oid])
        .output()
        .unwrap();
    assert!(output.status.success());
    for refs in [vec![], vec!["refs/tags/blob".to_owned()]] {
        let entries =
            collect_all_pointers_for_fetch_in(dir.path(), &refs, &CancellationToken::new())
                .unwrap();
        assert_eq!(
            entries.into_iter().map(|(_, p)| p).collect::<Vec<_>>(),
            [pointer.clone()]
        );
    }
}
