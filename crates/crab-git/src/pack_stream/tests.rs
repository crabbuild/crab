use super::*;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

fn limits(cache: usize) -> StreamLimits {
    StreamLimits {
        max_pack_bytes: 64 * 1024 * 1024,
        max_objects: 100_000,
        max_object_bytes: 8 * 1024 * 1024,
        max_inflated_bytes: 256 * 1024 * 1024,
        max_delta_depth: 64,
        max_cache_bytes: cache,
    }
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
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// A history whose repeated edits produce long delta chains, an empty blob, and a tag.
fn history(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"], b"");
    std::fs::write(dir.join("empty"), b"").unwrap();
    let mut contents = String::new();
    for revision in 0..40 {
        for line in 0..200 {
            contents.push_str(&format!("revision {revision} line {line}\n"));
        }
        std::fs::write(dir.join("file"), &contents).unwrap();
        std::fs::create_dir_all(dir.join(format!("dir/{}", revision % 5))).unwrap();
        std::fs::write(
            dir.join(format!("dir/{}/note", revision % 5)),
            format!("{contents}{revision}"),
        )
        .unwrap();
        git(dir, &["add", "-A"], b"");
        git(
            dir,
            &["commit", "-qm", &format!("revision {revision}")],
            b"",
        );
    }
    git(dir, &["tag", "-a", "-m", "release", "v1"], b"");
}

fn pack(dir: &Path, offset_deltas: bool) -> Vec<u8> {
    let mut args = vec![
        "pack-objects",
        "--stdout",
        "--all",
        "--depth=50",
        "--window=50",
    ];
    if offset_deltas {
        args.push("--delta-base-offset");
    }
    git(dir, &args, b"")
}

struct Memory<'a>(&'a [u8], u64);

impl EntrySource for Memory<'_> {
    fn read(&mut self, start: u64, end: u64) -> std::result::Result<Vec<u8>, BoxError> {
        self.1 += 1;
        Ok(self.0[start as usize..end as usize].to_vec())
    }
}

fn scan(bytes: &[u8], chunk: usize) -> Result<PackTable> {
    let mut scanner = PackScanner::new(limits(0));
    for part in bytes.chunks(chunk) {
        scanner.feed(part)?;
    }
    scanner.finish()
}

fn resolve(
    bytes: &[u8],
    table: &PackTable,
    cache: usize,
    chunk: usize,
) -> (IndexedPack, Vec<(ObjectId, Kind)>, u64) {
    let mut objects = Vec::new();
    let mut sink = |object: ResolvedObject<'_>| {
        assert_eq!(object_id(object.kind, object.data), object.oid);
        objects.push((object.oid, object.kind));
        Ok(())
    };
    let mut source = Memory(bytes, 0);
    let mut resolver = PackResolver::new(table, limits(cache), || false).unwrap();
    for part in bytes.chunks(chunk) {
        resolver.feed(part, &mut source, &mut sink).unwrap();
    }
    let indexed = resolver.finish(&mut source, &mut sink).unwrap();
    (indexed, objects, source.1)
}

fn native_index(dir: &Path, bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let pack = dir.join("native.pack");
    std::fs::write(&pack, bytes).unwrap();
    git(
        dir,
        &["index-pack", "--rev-index", pack.to_str().unwrap()],
        b"",
    );
    let index = std::fs::read(pack.with_extension("idx")).unwrap();
    let reverse = std::fs::read(pack.with_extension("rev")).unwrap();
    std::fs::remove_file(pack.with_extension("idx")).unwrap();
    std::fs::remove_file(pack.with_extension("rev")).unwrap();
    (index, reverse)
}

#[test]
fn streamed_sidecars_match_native_git_for_offset_and_reference_deltas() {
    let temp = tempfile::tempdir().unwrap();
    history(temp.path());
    for offset_deltas in [true, false] {
        let bytes = pack(temp.path(), offset_deltas);
        let table = scan(&bytes, 4096).unwrap();
        assert!(
            table
                .entries()
                .iter()
                .any(|entry| !matches!(entry.kind, EntryKind::Base(_))),
            "fixture must contain deltas"
        );
        let (indexed, objects, _) = resolve(&bytes, &table, 64 * 1024 * 1024, 4096);
        let (index, reverse) = native_index(temp.path(), &bytes);
        assert_eq!(indexed.index, index, "offset deltas: {offset_deltas}");
        assert_eq!(
            indexed.reverse_index, reverse,
            "offset deltas: {offset_deltas}"
        );
        crate::pack_locator::validate_pack_kind_metadata(
            &indexed.kinds,
            indexed.checksum,
            u64::from(indexed.object_count),
        )
        .unwrap();
        assert_eq!(objects.len(), table.entries().len());
        assert!(indexed.max_depth > 1);
    }
}

#[test]
fn evicted_bases_are_reread_without_changing_the_index() {
    let temp = tempfile::tempdir().unwrap();
    history(temp.path());
    let bytes = pack(temp.path(), true);
    let table = scan(&bytes, 1 << 20).unwrap();
    let (complete, _, unconstrained_reads) = resolve(&bytes, &table, 64 * 1024 * 1024, 1 << 20);
    let (starved, _, starved_reads) = resolve(&bytes, &table, 0, 1 << 20);
    assert_eq!(unconstrained_reads, 0);
    assert!(starved_reads > 0);
    assert_eq!(starved.reread_bases, starved_reads);
    assert_eq!(starved.index, complete.index);
    assert_eq!(starved.kinds, complete.kinds);
}

#[test]
fn windowed_rereads_coalesce_nearby_entries_into_one_fetch() {
    let temp = tempfile::tempdir().unwrap();
    history(temp.path());
    let bytes = pack(temp.path(), true);
    let table = scan(&bytes, 1 << 20).unwrap();
    let (complete, _, entry_reads) = resolve(&bytes, &table, 0, 1 << 20);
    let mut fetches = 0u64;
    let size = bytes.len() as u64;
    let mut source = WindowedSource::new(
        |start: u64, end: u64| {
            fetches += 1;
            Ok(bytes[start as usize..end as usize].to_vec())
        },
        size,
        64 * 1024,
        4,
    );
    let mut sink = |_: ResolvedObject<'_>| Ok(());
    let mut resolver = PackResolver::new(&table, limits(0), || false).unwrap();
    for part in bytes.chunks(1 << 20) {
        resolver.feed(part, &mut source, &mut sink).unwrap();
    }
    let windowed = resolver.finish(&mut source, &mut sink).unwrap();
    drop(source);
    assert_eq!(windowed.index, complete.index);
    assert!(fetches > 0 && fetches < entry_reads);
}

#[test]
fn byte_at_a_time_feeding_matches_bulk_feeding() {
    let temp = tempfile::tempdir().unwrap();
    history(temp.path());
    let bytes = pack(temp.path(), true);
    let bulk = scan(&bytes, bytes.len()).unwrap();
    let single = scan(&bytes, 1).unwrap();
    assert_eq!(bulk.entries().len(), single.entries().len());
    assert_eq!(bulk.checksum(), single.checksum());
    let (left, _, _) = resolve(&bytes, &bulk, 1 << 20, 1);
    let (right, _, _) = resolve(&bytes, &bulk, 1 << 20, bytes.len());
    assert_eq!(left.index, right.index);
}

#[test]
fn corrupt_truncated_and_padded_packs_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    history(temp.path());
    let bytes = pack(temp.path(), true);
    let mut flipped = bytes.clone();
    let middle = flipped.len() / 2;
    flipped[middle] ^= 0x40;
    assert!(scan(&flipped, 4096).is_err());
    assert!(scan(&bytes[..bytes.len() - 1], 4096).is_err());
    let mut padded = bytes.clone();
    padded.push(0);
    assert!(scan(&padded, 4096).is_err());
}

#[test]
fn second_pass_rejects_bytes_that_differ_from_the_scan() {
    let temp = tempfile::tempdir().unwrap();
    history(temp.path());
    let bytes = pack(temp.path(), true);
    let table = scan(&bytes, 4096).unwrap();
    let mut changed = bytes.clone();
    let entry = table.entries()[table.entries().len() / 2];
    changed[entry.data_offset as usize] ^= 0x01;
    let mut source = Memory(&changed, 0);
    let mut resolver = PackResolver::new(&table, limits(1 << 20), || false).unwrap();
    let mut sink = |_: ResolvedObject<'_>| Ok(());
    let result = changed
        .chunks(4096)
        .try_for_each(|part| resolver.feed(part, &mut source, &mut sink));
    assert!(matches!(
        result,
        Err(PackStreamError::Invalid(_) | PackStreamError::Inflate(_))
    ));
}

#[test]
fn limits_bound_object_count_and_pack_size() {
    let temp = tempfile::tempdir().unwrap();
    history(temp.path());
    let bytes = pack(temp.path(), true);
    let mut scanner = PackScanner::new(StreamLimits {
        max_objects: 3,
        ..limits(0)
    });
    assert!(matches!(
        scanner.feed(&bytes),
        Err(PackStreamError::Limit("object count"))
    ));
    let mut scanner = PackScanner::new(StreamLimits {
        max_pack_bytes: 100,
        ..limits(0)
    });
    assert!(matches!(
        scanner.feed(&bytes),
        Err(PackStreamError::Limit("pack bytes"))
    ));
}
