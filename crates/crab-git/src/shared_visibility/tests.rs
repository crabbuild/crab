use super::*;
use std::cell::Cell;
use std::io::Write;
use std::process::{Command, Stdio};

struct Repository(tempfile::TempDir);
impl Repository {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let output = Command::new("git")
            .args(["init", "--bare", "--object-format=sha1"])
            .arg(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        Self(directory)
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn object(&self, kind: &str, bytes: &[u8]) -> ObjectId {
        let mut child = Command::new("git")
            .arg("--git-dir")
            .arg(self.path())
            .args(["hash-object", "-w", "--literally", "--stdin", "-t", kind])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        ObjectId::from_hex(String::from_utf8(output.stdout).unwrap().trim().as_bytes()).unwrap()
    }
    fn tree(&self, entries: &[(&str, &str, ObjectId)]) -> ObjectId {
        let mut bytes = Vec::new();
        for (mode, name, oid) in entries {
            bytes.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            bytes.extend_from_slice(oid.as_bytes());
        }
        self.object("tree", &bytes)
    }
    fn commit(&self, tree: ObjectId, parents: &[ObjectId]) -> ObjectId {
        let mut bytes = format!("tree {tree}\n");
        for parent in parents {
            bytes.push_str(&format!("parent {parent}\n"));
        }
        bytes.push_str("author Tester <test@example.com> 1 +0000\ncommitter Tester <test@example.com> 1 +0000\n\ntest\n");
        self.object("commit", bytes.as_bytes())
    }
    fn tag(&self, target: ObjectId, kind: &str) -> ObjectId {
        self.object("tag", format!("object {target}\ntype {kind}\ntag test\ntagger Tester <test@example.com> 1 +0000\n\ntag\n").as_bytes())
    }
    fn native(&self, root: ObjectId) -> Vec<ObjectId> {
        let output = crate::raw_object_command(self.path())
            .args([
                "rev-list",
                "--objects",
                "--no-object-names",
                &root.to_string(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut oids = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| ObjectId::from_hex(line.as_bytes()).unwrap())
            .collect::<Vec<_>>();
        oids.sort_unstable();
        oids.dedup();
        oids
    }
}
fn selected(result: &SharedVisibility, index: usize) -> Vec<ObjectId> {
    result
        .objects
        .iter()
        .enumerate()
        .filter_map(|(position, oid)| {
            (result.closures[index].chunks[position / CHUNK_BITS][position % CHUNK_BITS / 8]
                & (1 << (position % 8))
                != 0)
                .then_some(*oid)
        })
        .collect()
}
fn fixture() -> (Repository, Vec<ObjectId>) {
    let repository = Repository::new();
    let blob = repository.object("blob", b"normal\n");
    let link = repository.object("blob", b"target");
    let missing = ObjectId::from_hex(&[b'f'; 40]).unwrap();
    let tree = repository.tree(&[
        ("100755", "exec", blob),
        ("100664", "old", blob),
        ("160000", "submodule", missing),
        ("120000", "symlink", link),
    ]);
    let first = repository.commit(tree, &[]);
    let other = repository.object("blob", b"other\n");
    let next_tree = repository.tree(&[("100644", "file", other)]);
    let second = repository.commit(next_tree, &[first]);
    let merged = repository.commit(tree, &[first, second]);
    let tag = repository.tag(first, "commit");
    let nested = repository.tag(tag, "tag");
    let tree_tag = repository.tag(tree, "tree");
    let blob_tag = repository.tag(blob, "blob");
    (
        repository,
        vec![
            first, second, merged, tag, nested, tree_tag, blob_tag, tree, blob, first,
        ],
    )
}
#[test]
fn exact_memberships_match_native_for_branches_merges_tags_and_noncommit_roots() {
    let (repository, roots) = fixture();
    let result = build(repository.path(), &roots, Limits::default(), || false).unwrap();
    for (index, root) in roots.iter().enumerate() {
        let actual = selected(&result, index);
        assert_eq!(actual, repository.native(*root));
        assert_eq!(actual.len(), result.closures[index].len());
    }
    assert!(Arc::ptr_eq(
        &result.closures[0].chunks[0],
        &result.closures[9].chunks[0]
    ));
    assert!(result.parsed_links > result.distinct_links);
    assert!(result.structural_bytes > 0);
}
#[test]
fn raw_physical_ancestry_ignores_replacement_graft_and_shallow_overrides() {
    let (repository, roots) = fixture();
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(repository.path())
        .args([
            "update-ref",
            &format!("refs/replace/{}", roots[1]),
            &roots[0].to_string(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    std::fs::write(repository.path().join("shallow"), format!("{}\n", roots[1])).unwrap();
    std::fs::write(
        repository.path().join("info/grafts"),
        format!("{}\n", roots[1]),
    )
    .unwrap();
    let result = build(repository.path(), &roots, Limits::default(), || false).unwrap();
    for (index, root) in roots.iter().enumerate() {
        assert_eq!(selected(&result, index), repository.native(*root));
    }
}
#[test]
fn duplicate_roots_across_words_and_padded_dictionary_chunks_remain_exact() {
    let (repository, roots) = fixture();
    let inputs = (0..137)
        .map(|index| roots[index % roots.len()])
        .collect::<Vec<_>>();
    let result = build(repository.path(), &inputs, Limits::default(), || false).unwrap();
    for (index, root) in inputs.iter().enumerate() {
        assert_eq!(selected(&result, index), repository.native(*root));
    }
    let last = result.objects.len();
    for closure in &result.closures {
        for position in last..CHUNK_BITS {
            assert_eq!(closure.chunks[0][position / 8] & (1 << (position % 8)), 0);
        }
    }
}
#[test]
fn limits_and_cancellation_fail_closed() {
    let (repository, roots) = fixture();
    let limits = Limits::default();
    for limits in [
        Limits {
            max_refs: 1,
            ..limits
        },
        Limits {
            max_objects: 1,
            ..limits
        },
        Limits {
            max_links: 0,
            ..limits
        },
        Limits {
            max_structural_bytes: 0,
            ..limits
        },
        Limits {
            max_matrix_bytes: 0,
            ..limits
        },
        Limits {
            max_memory_bytes: 0,
            ..limits
        },
        Limits {
            max_output_bytes: 0,
            ..limits
        },
        Limits {
            max_object_bytes: 1,
            ..limits
        },
    ] {
        assert!(
            build(repository.path(), &roots, limits, || false).is_err(),
            "{limits:?}"
        );
    }
    assert!(matches!(
        build(repository.path(), &roots, limits, || true),
        Err(Error::Cancelled)
    ));
    let checks = Cell::new(0);
    assert!(matches!(
        build(repository.path(), &roots, limits, || {
            checks.set(checks.get() + 1);
            checks.get() > roots.len() + 3
        }),
        Err(Error::Cancelled)
    ));
}
#[test]
fn missing_history_wrong_kinds_and_bad_structural_checksums_are_rejected() {
    let (repository, roots) = fixture();
    let missing = ObjectId::from_hex(&[b'e'; 40]).unwrap();
    let root = repository.commit(roots[7], &[missing]);
    assert!(
        matches!(build(repository.path(), &[root], Limits::default(), || false), Err(Error::Missing(oid)) if oid == missing)
    );
    let wrong_kind = repository.tag(roots[8], "commit");
    assert!(matches!(
        build(repository.path(), &[wrong_kind], Limits::default(), || {
            false
        }),
        Err(Error::Invalid { .. })
    ));
    let oid = roots[0].to_string();
    let path = repository
        .path()
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..]);
    let bytes = b"tree 0000000000000000000000000000000000000000\nauthor Tester <test@example.com> 1 +0000\ncommitter Tester <test@example.com> 1 +0000\n\nchanged\n";
    let mut compressed =
        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    write!(compressed, "commit {}\0", bytes.len()).unwrap();
    compressed.write_all(bytes).unwrap();
    #[cfg(windows)]
    {
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();
    }
    std::fs::remove_file(&path).unwrap();
    std::fs::write(path, compressed.finish().unwrap()).unwrap();
    assert!(build(repository.path(), &[roots[0]], Limits::default(), || false).is_err());
}
#[test]
fn cycles_and_long_propagation_can_never_succeed_silently() {
    let oid = ObjectId::from_hex(&[b'1'; 40]).unwrap();
    let cycle = [
        Node {
            oid,
            kind: None,
            links: vec![1].into_boxed_slice(),
        },
        Node {
            oid,
            kind: None,
            links: vec![0].into_boxed_slice(),
        },
    ];
    assert!(matches!(
        propagate(&cycle, 1, &mut [1, 0], &|| false),
        Err(Error::Cycle)
    ));
    let chain = (0..5000)
        .map(|index| Node {
            oid,
            kind: None,
            links: if index + 1 < 5000 {
                vec![index + 1].into_boxed_slice()
            } else {
                Box::new([])
            },
        })
        .collect::<Vec<_>>();
    let checks = Cell::new(0);
    assert!(matches!(
        propagate(&chain, 1, &mut vec![0; 5000], &|| {
            checks.set(checks.get() + 1);
            checks.get() > 6
        }),
        Err(Error::Cancelled)
    ));
}

#[test]
fn transpose_and_padding_match_native_across_multiple_dictionary_chunks() {
    use gix_object::Write as _;
    let repository = Repository::new();
    let odb = gix_odb::at(repository.path().join("objects")).unwrap();
    let blobs = (0..2051)
        .map(|index| {
            (
                format!("file-{index:04}"),
                odb.write_buf(Kind::Blob, format!("blob {index}\n").as_bytes())
                    .unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let even = blobs
        .iter()
        .step_by(2)
        .map(|(name, oid)| ("100644", name.as_str(), *oid))
        .collect::<Vec<_>>();
    let odd = blobs
        .iter()
        .skip(1)
        .step_by(2)
        .map(|(name, oid)| ("100644", name.as_str(), *oid))
        .collect::<Vec<_>>();
    let tree_a = repository.tree(&even);
    let tree_b = repository.tree(&odd);
    let first = repository.commit(tree_a, &[]);
    let second = repository.commit(tree_b, &[]);
    let merge = repository.commit(tree_a, &[first, second]);
    let roots = [first, second, merge, blobs[0].1];
    let result = build(repository.path(), &roots, Limits::default(), || false).unwrap();
    assert!(result.objects.len() > 2 * CHUNK_BITS);
    for (index, root) in roots.iter().enumerate() {
        let expected = repository.native(*root);
        assert_eq!(selected(&result, index), expected);
        assert_eq!(result.closures[index].len(), expected.len());
        let last = result.closures[index].chunks.last().unwrap();
        for position in result.objects.len() % CHUNK_BITS..CHUNK_BITS {
            assert_eq!(last[position / 8] & (1 << (position % 8)), 0);
        }
    }
}
