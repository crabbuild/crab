use super::*;
use std::cell::Cell;
use std::io::Write;
use std::process::{Command, Stdio};

struct Repo(tempfile::TempDir);

impl Repo {
    fn new() -> Self {
        let repo = Self(tempfile::tempdir().unwrap());
        repo.git(&["init", "--bare", "--quiet"], b"");
        repo
    }

    fn git(&self, args: &[&str], input: &[u8]) -> String {
        let mut child = Command::new("git")
            .args(args)
            .current_dir(self.0.path())
            .env("GIT_DIR", self.0.path())
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env("GIT_AUTHOR_NAME", "Scan Test")
            .env("GIT_AUTHOR_EMAIL", "scan@example.invalid")
            .env("GIT_COMMITTER_NAME", "Scan Test")
            .env("GIT_COMMITTER_EMAIL", "scan@example.invalid")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn blob(&self, body: &[u8]) -> String {
        self.git(&["hash-object", "-w", "--stdin"], body)
    }

    fn tree(&self, blob: &str) -> String {
        self.git(
            &["mktree"],
            format!("100644 blob {blob}\tdata\n").as_bytes(),
        )
    }

    fn commit(&self, tree: &str, parent: Option<&str>) -> String {
        let mut args = vec![
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            tree,
            "-m",
            "scan",
        ];
        if let Some(parent) = parent {
            args.extend(["-p", parent]);
        }
        self.git(&args, b"")
    }

    fn scan(&self, targets: &[&str], limits: PointerScanLimits) -> Result<Vec<PointerBlob>> {
        let refs = targets
            .iter()
            .enumerate()
            .map(|(i, oid)| (format!("refs/heads/{i}"), (*oid).to_owned()))
            .collect::<Vec<_>>();
        scan_pointers(self.0.path(), &refs, limits, &|| false).map(|scan| scan.pointers)
    }

    fn object_path(&self, oid: &str) -> std::path::PathBuf {
        self.0
            .path()
            .join("objects")
            .join(&oid[..2])
            .join(&oid[2..])
    }
}

fn limits() -> PointerScanLimits {
    PointerScanLimits {
        objects: 100,
        lookups: 1000,
        allocation_bytes: 1024 * 1024,
    }
}

fn pointer() -> crab_types::pointer::Pointer {
    crab_types::pointer::Pointer {
        file_hash: [0x31; 32],
        size: 8192,
        shard_hint: None,
    }
}

#[test]
fn scans_history_and_all_tag_target_kinds_without_peel_hints() {
    let repo = Repo::new();
    let pointer = pointer();
    let blob = repo.blob(&pointer.serialize());
    let tree = repo.tree(&blob);
    let commit = repo.commit(&tree, None);
    let plain = repo.blob(b"no pointer\n");
    let later = repo.commit(&repo.tree(&plain), Some(&commit));
    let mut targets = vec![blob.clone(), tree.clone(), later.clone()];
    for (target, kind) in [(&blob, "blob"), (&tree, "tree"), (&later, "commit")] {
        let body = format!(
            "object {target}\ntype {kind}\ntag snapshot\ntagger Scan <scan@example.invalid> 1 +0000\n\nsnapshot\n"
        );
        let tag = repo.git(&["mktag"], body.as_bytes());
        let body = format!(
            "object {tag}\ntype tag\ntag nested\ntagger Scan <scan@example.invalid> 1 +0000\n\nnested\n"
        );
        targets.push(repo.git(&["mktag"], body.as_bytes()));
    }
    let refs = targets.iter().map(String::as_str).collect::<Vec<_>>();
    let scanned = repo.scan(&refs, limits()).unwrap();
    assert_eq!(
        scanned,
        vec![PointerBlob {
            oid: super::super::oid_to_bytes(
                &gix_hash::ObjectId::from_hex(blob.as_bytes()).unwrap()
            ),
            file_hash: pointer.file_hash,
            size: pointer.size,
        }]
    );
    for (i, oid) in targets.iter().enumerate() {
        repo.git(&["update-ref", &format!("refs/tags/{i}"), oid], b"");
    }
    repo.git(&["repack", "-adq"], b"");
    assert_eq!(repo.scan(&refs, limits()).unwrap(), scanned);
}

#[test]
fn missing_corrupt_and_wrong_kind_blobs_fail_shared_and_pointer_walks() {
    for damage in ["missing", "corrupt", "checksum", "kind"] {
        let repo = Repo::new();
        let blob = repo.blob(&pointer().serialize());
        let tree = repo.tree(&blob);
        let commit = repo.commit(&tree, None);
        let path = repo.object_path(&blob);
        // Git writes read-only loose files. Replace only this fixture's object
        // instead of relying on platform-specific writable Git file modes.
        std::fs::remove_file(&path).unwrap();
        match damage {
            "missing" => {}
            "corrupt" => std::fs::write(path, b"not a loose Git object").unwrap(),
            "checksum" => {
                let other = repo.blob(b"different object\n");
                std::fs::copy(repo.object_path(&other), path).unwrap();
            }
            "kind" => {
                std::fs::copy(repo.object_path(&tree), path).unwrap();
            }
            _ => unreachable!(),
        }
        let refs = [("refs/heads/main".to_owned(), commit)];
        assert!(
            super::super::walk_reachable(repo.0.path(), &refs).is_err(),
            "{damage}"
        );
        assert!(
            scan_pointers(repo.0.path(), &refs, limits(), &|| false).is_err(),
            "{damage}"
        );
    }
}

#[test]
fn distinct_objects_and_repeated_work_have_separate_fail_closed_bounds() {
    let repo = Repo::new();
    let blob = repo.blob(&pointer().serialize());
    let tree = repo.tree(&blob);
    let commit = repo.commit(&tree, None);
    let mut budget = limits();
    budget.objects = 3;
    assert_eq!(repo.scan(&[&commit, &commit], budget).unwrap().len(), 1);
    budget.objects = 2;
    assert!(matches!(
        repo.scan(&[&commit], budget),
        Err(WalkError::LimitExceeded { .. })
    ));
    budget = limits();
    budget.lookups = 5;
    assert!(matches!(
        repo.scan(&[&commit], budget),
        Err(WalkError::LookupLimitExceeded { maximum: 5 })
    ));
}

#[test]
fn cancellation_before_open_and_during_history_never_returns_partial_pointers() {
    assert!(matches!(
        scan_pointers(Path::new("absent"), &[], limits(), &|| true),
        Err(WalkError::Cancelled)
    ));
    let repo = Repo::new();
    let blob = repo.blob(&pointer().serialize());
    let tree = repo.tree(&blob);
    let old = repo.commit(&tree, None);
    let current = repo.commit(&tree, Some(&old));
    let refs = [("refs/heads/main".into(), current)];
    let full_scan_reads = Cell::new(0);
    scan_pointers(repo.0.path(), &refs, limits(), &|| {
        full_scan_reads.set(full_scan_reads.get() + 1);
        false
    })
    .unwrap();
    for stop_after in [2, full_scan_reads.get() / 2, full_scan_reads.get() - 1] {
        let reads = Cell::new(0);
        let cancelled = || {
            reads.set(reads.get() + 1);
            reads.get() > stop_after
        };
        let result = scan_pointers(repo.0.path(), &refs, limits(), &cancelled);
        assert!(
            matches!(result, Err(WalkError::Cancelled)),
            "stop_after={stop_after}: {result:?}"
        );
    }
}

#[test]
fn allocation_limit_applies_to_metadata_without_loading_large_normal_blobs() {
    let repo = Repo::new();
    let small = repo.blob(&pointer().serialize());
    let large = repo.blob(&vec![0x42; 4096]);
    let tree = repo.git(
        &["mktree"],
        format!("100644 blob {large}\tlarge\n100644 blob {small}\tsmall\n").as_bytes(),
    );
    let commit = repo.commit(&tree, None);
    let mut budget = limits();
    budget.allocation_bytes = 1024;
    assert_eq!(repo.scan(&[&commit, &large], budget).unwrap().len(), 1);
    let message = "x".repeat(4096);
    let oversized = repo.git(&["commit-tree", &tree, "-m", &message], b"");
    assert!(repo.scan(&[&oversized], budget).is_err());
}

#[test]
fn oversized_headers_leave_explicit_byte_verification_work() {
    let repo = Repo::new();
    let blob = repo.blob(&pointer().serialize());
    let tree = repo.tree(&blob);
    let commit = repo.commit(&tree, None);
    let large = repo.blob(&vec![0x80; 4096]);
    let path = repo.object_path(&blob);
    std::fs::remove_file(&path).unwrap();
    std::fs::copy(repo.object_path(&large), &path).unwrap();
    let refs = [("refs/heads/main".into(), commit)];
    let scan = scan_pointers(repo.0.path(), &refs, limits(), &|| false).unwrap();
    assert!(scan.pointers.is_empty());
    assert_eq!(
        scan.unchecked_blobs,
        vec![BlobHeader {
            oid: super::super::oid_to_bytes(
                &gix_hash::ObjectId::from_hex(blob.as_bytes()).unwrap()
            ),
            size: 4096,
        }]
    );
}

#[test]
fn large_blob_inventory_is_bounded_during_root_preflight() {
    let repo = Repo::new();
    let a = repo.blob(&[1; 4096]);
    let b = repo.blob(&[2; 4096]);
    let refs = [("refs/tags/a".into(), a), ("refs/tags/b".into(), b)];
    let mut budget = limits();
    budget.objects = 1;
    assert!(matches!(
        scan_pointers(repo.0.path(), &refs, budget, &|| false),
        Err(WalkError::LimitExceeded {
            actual: 2,
            maximum: 1
        })
    ));
}
