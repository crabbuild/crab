use base64::{Engine as _, engine::general_purpose::STANDARD};
use crab_types::pointer_read::{Request, Response};

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

fn command(root: &Path, binary: &str, args: &[&str]) -> Command {
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("CRAB_CACHE_DIR", root.join("cache"));
    command
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = command(root, "git", args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn fixture() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "--initial-branch=main"]);
    let driver = format!(
        "'{}' filter-process",
        env!("CARGO_BIN_EXE_crab").replace('\'', "'\\''")
    );
    for kind in ["crab", "lfs"] {
        git(
            root,
            &["config", &format!("filter.{kind}.process"), &driver],
        );
        git(
            root,
            &["config", &format!("filter.{kind}.required"), "true"],
        );
    }
    std::fs::write(
        root.join(".gitattributes"),
        b"*.bin filter=crab -text\n*.lfs filter=lfs -text\n",
    )
    .unwrap();
    directory
}

struct Reader {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}

impl Reader {
    fn open(root: &Path, pointer: &[u8]) -> Self {
        let mut reader = Self::spawn(root);
        reader.send(Request::Open {
            pointer_b64: STANDARD.encode(pointer),
            dehydrate_path: None,
        });
        assert!(matches!(reader.next(), Response::Ready { .. }));
        reader
    }

    fn spawn(root: &Path) -> Self {
        let mut child = command(root, env!("CARGO_BIN_EXE_crab"), &["read-pointer"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
        }
    }

    fn send(&mut self, request: Request) {
        let mut bytes = serde_json::to_vec(&request).unwrap();
        bytes.push(b'\n');
        self.input.as_mut().unwrap().write_all(&bytes).unwrap();
    }

    fn next(&mut self) -> Response {
        let mut line = String::new();
        assert!(self.output.read_line(&mut line).unwrap() > 0);
        serde_json::from_str(&line).unwrap()
    }

    fn range(&mut self, offset: u64, length: u32) -> Vec<u8> {
        self.send(Request::Range { offset, length });
        match self.next() {
            Response::Data { bytes_b64, .. } => STANDARD.decode(bytes_b64).unwrap(),
            other => panic!("range failed: {other:?}"),
        }
    }

    fn close(mut self) {
        self.send(Request::Close);
        self.input.take();
        assert!(matches!(self.next(), Response::Closed));
        self.assert_exit();
    }

    fn assert_exit(&mut self) {
        let status = self.child.wait().unwrap();
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(status.success(), "{stderr}");
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn content() -> Vec<u8> {
    (0..2 * 1024 * 1024_usize)
        .map(|i| ((i.wrapping_mul(73) ^ (i >> 8)) & 255) as u8)
        .collect()
}

#[test]
fn close_before_ready_drains_reader_waiting_for_a_staging_writer() {
    let directory = fixture();
    let root = directory.path();
    std::fs::write(root.join("file.bin"), content()).unwrap();
    git(root, &["add", "file.bin"]);
    let pointer = git(root, &["show", ":file.bin"]);
    let context = crab::git::worktree::WorktreeContext::resolve_from_path(root).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    for close_command in [true, false] {
        let writer = runtime
            .block_on(crab_staging::StagingArea::open(
                context.shared_staging_dir(),
            ))
            .unwrap();
        let mut reader = Reader::spawn(root);
        reader.send(Request::Open {
            pointer_b64: STANDARD.encode(&pointer),
            dehydrate_path: None,
        });
        if close_command {
            reader.send(Request::Close);
        }
        reader.input.take();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let response = reader.next();
            reader.assert_exit();
            let _ = send.send(response);
        });
        let result = receive.recv_timeout(std::time::Duration::from_secs(1));
        runtime.block_on(writer.close()).unwrap();
        worker.join().unwrap();
        assert!(matches!(result.unwrap(), Response::Closed));
    }
}

#[test]
fn hydrate_reads_the_same_delegated_git_clean_lease_as_native_ranges() {
    let directory = fixture();
    let root = directory.path();
    let original = content();
    let path = "model[1].bin";
    std::fs::write(root.join(path), &original).unwrap();
    git(root, &["add", "--", path]);
    let pointer = git(root, &["show", &format!(":{path}")]);
    std::fs::write(root.join(path), &pointer).unwrap();
    let mut reader = Reader::open(root, &pointer);
    assert_eq!(reader.range(0, 16), original[..16]);
    reader.close();
    let result = command(
        root,
        env!("CARGO_BIN_EXE_crab"),
        &["hydrate", "--all", "--json"],
    )
    .output()
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    assert_eq!(std::fs::read(root.join(path)).unwrap(), original);
    assert_eq!(git(root, &["show", &format!(":{path}")]), pointer);
}

#[test]
fn hydrate_reports_selected_staging_corruption_without_replacing_the_pointer() {
    let directory = fixture();
    let root = directory.path();
    let path = "model.bin";
    std::fs::write(root.join(path), content()).unwrap();
    git(root, &["add", "--", path]);
    let indexed = git(root, &["show", &format!(":{path}")]);
    let mut pointer = crab_types::pointer::Pointer::parse(&indexed).unwrap();
    pointer.size += 1;
    let invalid = pointer.serialize();
    std::fs::write(root.join(path), &invalid).unwrap();
    let result = command(
        root,
        env!("CARGO_BIN_EXE_crab"),
        &["hydrate", path, "--json"],
    )
    .output()
    .unwrap();
    let response: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(!result.status.success());
    assert_eq!(response["error"]["code"], "CRAB-E0080");
    assert_eq!(std::fs::read(root.join(path)).unwrap(), invalid);
    assert_eq!(git(root, &["show", &format!(":{path}")]), indexed);
}

#[test]
fn staged_crab_and_lfs_ranges_are_byte_identical_without_hydrating_worktree() {
    let directory = fixture();
    let root = directory.path();
    let original = content();
    for path in ["file.bin", "file.lfs"] {
        std::fs::write(root.join(path), &original).unwrap();
        git(root, &["add", "--", path]);
        let pointer = git(root, &["show", &format!(":{path}")]);
        assert!(!matches!(
            crab_git::classify(&pointer),
            crab_git::PointerKind::NotAPointer
        ));
        std::fs::write(root.join(path), &pointer).unwrap();
        let mut reader = Reader::open(root, &pointer);
        for (offset, length) in [
            (0, 8),
            (65530, 65547),
            (original.len() as u64 - 9, 100),
            (u64::MAX, 100),
        ] {
            let bytes = reader.range(offset, length);
            let start = offset.min(original.len() as u64) as usize;
            let end = start.saturating_add(length as usize).min(original.len());
            assert_eq!(bytes, original[start..end]);
        }
        if path.ends_with(".bin") {
            let mut start = 0;
            let mut offset = 0;
            loop {
                reader.send(Request::Chunks { start, limit: 3 });
                let Response::Chunks { page } = reader.next() else {
                    panic!("expected chunk metadata");
                };
                assert_eq!(page.xorb_count, None);
                for chunk in &page.chunks {
                    assert_eq!(chunk.offset, offset);
                    offset += chunk.length;
                    let bytes = &original[chunk.offset as usize..offset as usize];
                    assert_eq!(chunk.hash, crab_xet::hash::compute_data_hash(bytes).hex());
                }
                let next = start + page.chunks.len() as u64;
                let Some(cursor) = page.next else {
                    assert_eq!(next, page.chunk_count);
                    break;
                };
                assert!(cursor > start);
                start = cursor;
            }
            assert_eq!(offset, original.len() as u64);
        } else {
            reader.send(Request::Chunks { start: 0, limit: 3 });
            assert!(matches!(reader.next(), Response::Failed { .. }));
        }
        assert_eq!(reader.range(0, 8), original[..8]);
        reader.close();
        assert_eq!(std::fs::read(root.join(path)).unwrap(), pointer);
    }
}

#[test]
fn corrupt_lfs_payload_never_returns_a_successful_partial_preview() {
    let directory = fixture();
    let root = directory.path();
    std::fs::write(root.join("file.lfs"), content()).unwrap();
    git(root, &["add", "file.lfs"]);
    let pointer = git(root, &["show", ":file.lfs"]);
    let pointer_data = crab_git::LfsPointer::parse(&pointer).unwrap();
    let storage = crab_lfs::local::storage_from_git(&root.join(".git")).unwrap();
    let object = crab_lfs::local::object_path(&storage, &pointer_data.oid);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(object)
        .unwrap();
    file.write_all(b"corrupt").unwrap();
    file.sync_all().unwrap();
    let mut reader = Reader::open(root, &pointer);
    reader.send(Request::Range {
        offset: 1024 * 1024,
        length: 8,
    });
    assert!(matches!(reader.next(), Response::Failed { .. }));
    reader.close();
}

#[test]
fn captured_configuration_cannot_be_redirected_between_ranges() {
    let directory = fixture();
    let root = directory.path();
    let pointer = crab_types::pointer::Pointer {
        file_hash: [1; 32],
        size: 32,
        shard_hint: None,
    }
    .serialize();
    let mut reader = Reader::open(root, &pointer);
    std::fs::write(
        root.join("crab.toml"),
        "version = 1\n[remote]\nurl = 'crab://changed.invalid/repository'\n",
    )
    .unwrap();
    reader.send(Request::Range {
        offset: 0,
        length: 8,
    });
    let Response::Failed { message, .. } = reader.next() else {
        panic!("missing source must fail");
    };
    assert!(
        message.contains("no content remote configured"),
        "{message}"
    );
    reader.close();
}

#[test]
fn cancelling_a_range_drains_it_before_the_next_range_and_eof_closes_the_source() {
    let directory = fixture();
    let root = directory.path();
    let original = content();
    std::fs::write(root.join("file.lfs"), &original).unwrap();
    git(root, &["add", "file.lfs"]);
    let pointer = git(root, &["show", ":file.lfs"]);
    let mut reader = Reader::open(root, &pointer);
    reader.send(Request::Range {
        offset: 0,
        length: 1024 * 1024,
    });
    reader.send(Request::Cancel);
    assert!(matches!(
        reader.next(),
        Response::Data { .. } | Response::Cancelled
    ));
    assert_eq!(reader.range(65530, 32), original[65530..65562]);
    reader.input.take();
    assert!(matches!(reader.next(), Response::Closed));
    reader.assert_exit();
}

#[test]
fn pointer_session_retains_unpublished_content_across_staging_retirement() {
    let directory = fixture();
    let root = directory.path();
    let original = content();
    std::fs::write(root.join("file.bin"), &original).unwrap();
    git(root, &["add", "file.bin"]);
    let bytes = git(root, &["show", ":file.bin"]);
    let pointer = crab_types::pointer::Pointer::parse(&bytes).unwrap();
    let mut reader = Reader::open(root, &bytes);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let context = crab::git::worktree::WorktreeContext::resolve_from_path(root).unwrap();
    {
        let staging = std::sync::Arc::new(
            runtime
                .block_on(crab_staging::StagingAreaReadOnly::open(
                    context.shared_staging_dir(),
                ))
                .unwrap(),
        );
        let mut recipes = staging
            .external_recipes_for_files(&[pointer.file_hash.into()])
            .unwrap();
        let recipe = recipes.remove(&pointer.file_hash.into()).unwrap().unwrap();
        let push = crab_staging::StagingPushSnapshot::begin(
            staging.clone(),
            uuid::Uuid::new_v4().to_string(),
        )
        .unwrap();
        push.capture(&[recipe]).unwrap();
        push.commit().unwrap();
    }
    {
        let staging = runtime
            .block_on(crab_staging::StagingArea::open(
                context.shared_staging_dir(),
            ))
            .unwrap();
        staging.clean().unwrap();
        runtime.block_on(staging.compact()).unwrap();
        runtime.block_on(staging.close()).unwrap();
    }
    assert_eq!(reader.range(65530, 32), original[65530..65562]);
    reader.close();
    let staging = runtime
        .block_on(crab_staging::StagingArea::open(
            context.shared_staging_dir(),
        ))
        .unwrap();
    staging.clean().unwrap();
    assert!(
        !context
            .shared_staging_dir()
            .read_dir()
            .unwrap()
            .any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pin-")
            })
    );
    runtime.block_on(staging.close()).unwrap();
}
