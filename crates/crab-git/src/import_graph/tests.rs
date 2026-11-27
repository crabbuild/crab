use super::*;
use crate::pack_stream::{EntrySource, PackResolver, PackScanner, ResolvedObject, StreamLimits};
use crate::shared_visibility::CHUNK_BYTES;
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

fn git(dir: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
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
    assert!(output.status.success(), "{args:?}");
    output.stdout
}

fn repository(dir: &Path) -> Vec<(String, ObjectId)> {
    git(dir, &["init", "-q", "-b", "main"], b"");
    for revision in 0..6 {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib"), format!("version {revision}\n")).unwrap();
        git(dir, &["add", "-A"], b"");
        git(dir, &["commit", "-qm", &format!("r{revision}")], b"");
        if revision == 2 {
            git(dir, &["tag", "-a", "-m", "early", "v0"], b"");
            git(dir, &["branch", "side"], b"");
        }
    }
    git(dir, &["tag", "light"], b"");
    String::from_utf8(git(
        dir,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
        b"",
    ))
    .unwrap()
    .lines()
    .map(|line| {
        let (oid, name) = line.split_once(' ').unwrap();
        (name.to_owned(), ObjectId::from_hex(oid.as_bytes()).unwrap())
    })
    .collect()
}

struct Memory(Vec<u8>);

impl EntrySource for Memory {
    fn read(
        &mut self,
        start: u64,
        end: u64,
    ) -> std::result::Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.0[start as usize..end as usize].to_vec())
    }
}

fn graph(bytes: Vec<u8>) -> std::result::Result<CompleteGraph, Box<dyn std::error::Error>> {
    let limits = StreamLimits {
        max_pack_bytes: 1 << 30,
        max_objects: 100_000,
        max_object_bytes: 1 << 24,
        max_inflated_bytes: 1 << 30,
        max_delta_depth: 64,
        max_cache_bytes: 1 << 24,
    };
    let mut scanner = PackScanner::new(limits);
    scanner.feed(&bytes)?;
    let table = scanner.finish()?;
    let mut graph = ImportGraph::new(GraphLimits {
        max_objects: 100_000,
        max_links: 1_000_000,
    });
    let mut sink = |object: ResolvedObject<'_>| {
        graph
            .add(object.oid, object.kind, object.data)
            .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
    };
    let mut source = Memory(bytes.clone());
    let mut resolver = PackResolver::new(&table, limits, || false)?;
    resolver.feed(&bytes, &mut source, &mut sink)?;
    resolver.finish(&mut source, &mut sink)?;
    Ok(graph.finish()?)
}

#[test]
fn shared_closures_match_native_reachability_for_every_ref() {
    let temp = tempfile::tempdir().unwrap();
    let refs = repository(temp.path());
    let bytes = git(
        temp.path(),
        &["pack-objects", "--stdout", "--all", "--delta-base-offset"],
        b"",
    );
    let complete = graph(bytes).unwrap();
    let tag = refs
        .iter()
        .find(|(name, _)| name == "refs/tags/v0")
        .unwrap()
        .1;
    assert_eq!(complete.kind(&tag), Some(Kind::Tag));
    let peeled = complete.peel(tag).unwrap();
    assert_eq!(complete.kind(&peeled), Some(Kind::Commit));
    let roots = refs.iter().map(|(_, oid)| *oid).collect::<Vec<_>>();
    let visibility = complete
        .visibility(&roots, shared_visibility::Limits::default(), || false)
        .unwrap();
    for ((name, oid), closure) in refs.iter().zip(visibility.closures()) {
        let native = String::from_utf8(git(
            temp.path(),
            &[
                "rev-list",
                "--objects",
                "--no-object-names",
                &oid.to_string(),
            ],
            b"",
        ))
        .unwrap()
        .lines()
        .map(|line| ObjectId::from_hex(line.as_bytes()).unwrap())
        .chain(
            // rev-list lists a tag's target closure but not the tag object itself.
            (name == "refs/tags/v0").then_some(*oid),
        )
        .collect::<BTreeSet<_>>();
        let members = visibility
            .objects()
            .iter()
            .enumerate()
            .filter(|(position, _)| {
                closure.chunks()[position / (CHUNK_BYTES * 8)][(position % (CHUNK_BYTES * 8)) / 8]
                    & (1 << (position % 8))
                    != 0
            })
            .map(|(_, oid)| *oid)
            .collect::<BTreeSet<_>>();
        assert_eq!(members, native, "{name}");
        assert_eq!(closure.len(), native.len(), "{name}");
    }
}

#[test]
fn a_pack_missing_a_linked_object_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let refs = repository(temp.path());
    let head = refs
        .iter()
        .find(|(name, _)| name == "refs/heads/main")
        .unwrap()
        .1;
    // Omit the first commit's root tree while keeping everything else.
    let all =
        String::from_utf8(git(temp.path(), &["rev-list", "--objects", "--all"], b"")).unwrap();
    let root_commit = String::from_utf8(git(
        temp.path(),
        &["rev-list", "--max-parents=0", &head.to_string()],
        b"",
    ))
    .unwrap();
    let root_tree = String::from_utf8(git(
        temp.path(),
        &["rev-parse", &format!("{}^{{tree}}", root_commit.trim())],
        b"",
    ))
    .unwrap();
    let kept = all
        .lines()
        .filter_map(|line| line.split(' ').next())
        .filter(|oid| *oid != root_tree.trim())
        .collect::<Vec<_>>()
        .join("\n");
    let bytes = git(
        temp.path(),
        &["pack-objects", "--stdout"],
        format!("{kept}\n").as_bytes(),
    );
    let error = graph(bytes).err().unwrap();
    assert!(matches!(
        error.downcast_ref::<ImportGraphError>(),
        Some(ImportGraphError::Missing { .. })
    ));
}

#[test]
fn first_parent_linearity_detects_merge_commits() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    git(dir, &["init", "-q", "-b", "main"], b"");
    std::fs::write(dir.join("a"), "a\n").unwrap();
    git(dir, &["add", "-A"], b"");
    git(dir, &["commit", "-qm", "base"], b"");
    git(dir, &["checkout", "-qb", "feature"], b"");
    std::fs::write(dir.join("b"), "b\n").unwrap();
    git(dir, &["add", "-A"], b"");
    git(dir, &["commit", "-qm", "feature"], b"");
    git(dir, &["checkout", "-q", "main"], b"");
    std::fs::write(dir.join("c"), "c\n").unwrap();
    git(dir, &["add", "-A"], b"");
    git(dir, &["commit", "-qm", "main"], b"");
    git(
        dir,
        &["merge", "-q", "--no-ff", "-m", "merge", "feature"],
        b"",
    );
    let tip = |name: &str| {
        ObjectId::from_hex(
            String::from_utf8(git(dir, &["rev-parse", name], b""))
                .unwrap()
                .trim()
                .as_bytes(),
        )
        .unwrap()
    };
    let complete = graph(git(dir, &["pack-objects", "--stdout", "--all"], b"")).unwrap();
    assert!(!complete.is_linear(tip("main")).unwrap());
    assert!(complete.is_linear(tip("feature")).unwrap());
}
