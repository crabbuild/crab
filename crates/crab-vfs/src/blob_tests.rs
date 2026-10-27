use super::*;
use crate::test_support::{create_git_repo_with_blob, pack_git_blobs, write_git_blob};

async fn block_native_reads(
    reader: &Arc<OdbReader>,
) -> (std::sync::mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
    let (release, wait) = std::sync::mpsc::channel();
    let entered = Arc::new(tokio::sync::Notify::new());
    let notify = Arc::clone(&entered);
    let reader = Arc::clone(reader);
    let task = tokio::task::spawn_blocking(move || {
        let _native = reader.odb.lock().unwrap();
        notify.notify_one();
        // Dropping release also unblocks the owner when a test assertion fails.
        let _ = wait.recv();
    });
    entered.notified().await;
    (release, task)
}

async fn wait_for_native_read(reader: &OdbReader, before: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while reader
            .native_reads_started
            .load(std::sync::atomic::Ordering::Acquire)
            == before
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_blob_read_keeps_admission_until_blocking_work_finishes() {
    let (_root, git_dir, oid) = create_git_repo_with_blob(b"verified after cancellation");
    let reader = Arc::new(OdbReader::new(&git_dir).unwrap());
    let (release, native_owner) = block_native_reads(&reader).await;
    let active = Arc::clone(&reader);
    let before = reader
        .native_reads_started
        .load(std::sync::atomic::Ordering::Acquire);
    let read = tokio::spawn(async move { active.read_blob_async(&oid).await });
    wait_for_native_read(&reader, before).await;
    read.abort();
    let _ = read.await;
    assert!(
        reader.fills.try_lock().is_err(),
        "cancelled waiter released a running fill's admission"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), reader.shutdown())
            .await
            .is_err()
    );
    release.send(()).unwrap();
    native_owner.await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), reader.shutdown())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_closes_cold_blob_admission_and_preserves_cached_bytes() {
    let (_root, git_dir, warm) = create_git_repo_with_blob(b"verified cached bytes");
    let cold = write_git_blob(&git_dir, b"not yet decoded");
    let reader = Arc::new(OdbReader::new(&git_dir).unwrap());
    reader.read_blob(&warm).unwrap();
    reader.shutdown().await;
    assert!(
        reader.read_blob_async(&warm).await.unwrap().as_ref() == b"verified cached bytes"
            && matches!(
                reader.read_blob_async(&cold).await,
                Err(CrabError::Cancelled)
            )
            && matches!(reader.read_blob(&cold), Err(CrabError::Cancelled))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn warm_blob_read_does_not_wait_for_unrelated_blocked_fill() {
    let (_root, git_dir, warm) = create_git_repo_with_blob(b"warm bytes");
    let cold = write_git_blob(&git_dir, b"cold bytes");
    let reader = Arc::new(OdbReader::new(&git_dir).unwrap());
    reader.read_blob(&warm).unwrap();
    let (release, native_owner) = block_native_reads(&reader).await;
    let active = Arc::clone(&reader);
    let before = reader
        .native_reads_started
        .load(std::sync::atomic::Ordering::Acquire);
    let read = tokio::spawn(async move { active.read_blob_async(&cold).await });
    wait_for_native_read(&reader, before).await;
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_blob_async(&warm),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(bytes.as_ref(), b"warm bytes");
    release.send(()).unwrap();
    native_owner.await.unwrap();
    read.await.unwrap().unwrap();
}

#[test]
fn odb_reader_promisor_uses_raw_oid_despite_replace_refs() {
    let (_root, git_dir, seed) = create_git_repo_with_blob(b"seed");
    let reader = OdbReader::new(&git_dir).unwrap();
    reader.read_blob(&seed).unwrap();
    let oid = write_git_blob(&git_dir, b"original bytes");
    let other = write_git_blob(&git_dir, b"replacement bytes");
    pack_git_blobs(&git_dir, &[&oid, &other]);
    let replace = format!("refs/replace/{oid}");
    for args in [vec!["prune-packed"], vec!["update-ref", &replace, &other]] {
        assert!(
            std::process::Command::new("git")
                .arg("--git-dir")
                .arg(&git_dir)
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_COMMON_DIR")
                .status()
                .unwrap()
                .success()
        );
    }
    let overridden = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&git_dir)
        .args(["cat-file", "blob", &oid])
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(overridden.status.success());
    assert_eq!(overridden.stdout, b"replacement bytes");
    assert_eq!(reader.read_blob(&oid).unwrap().as_ref(), b"original bytes");
}

#[test]
fn odb_reader_read_blob_returns_correct_content() {
    let content = b"hello from the ODB";
    let (_dir, git_dir, oid_hex) = create_git_repo_with_blob(content);

    let reader = OdbReader::new(&git_dir).unwrap();
    let data = reader.read_blob(&oid_hex).unwrap();
    assert_eq!(&data[..], content);
}

#[test]
fn odb_reader_preserves_loose_and_delta_packed_bodies() {
    use rand::{RngCore, SeedableRng};

    for packed in [false, true] {
        let mut content = vec![0_u8; 256 * 1024];
        rand::rngs::StdRng::seed_from_u64(17).fill_bytes(&mut content);
        let mut changed = content.clone();
        changed[content.len() / 2] ^= 1;
        let (_root, git_dir, oid) = create_git_repo_with_blob(&content);
        let oids = [oid, write_git_blob(&git_dir, &changed)];
        if packed {
            pack_git_blobs(&git_dir, &[&oids[0], &oids[1]]);
            for oid in &oids {
                let loose = git_dir.join("objects").join(&oid[..2]).join(&oid[2..]);
                std::fs::remove_file(loose).unwrap();
            }
            let index = std::fs::read_dir(git_dir.join("objects/pack"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.extension().is_some_and(|ext| ext == "idx"))
                .unwrap();
            let verification = std::process::Command::new("git")
                .args(["verify-pack", "-v"])
                .arg(index)
                .output()
                .unwrap();
            assert!(verification.status.success());
            let records = String::from_utf8(verification.stdout).unwrap();
            assert!(
                records.lines().any(|line| {
                    let fields = line.split_whitespace().collect::<Vec<_>>();
                    fields.len() == 7 && oids.iter().any(|oid| oid == fields[0])
                }),
                "fixture must contain a delta object"
            );
        }
        let reader = OdbReader::new(&git_dir).unwrap();
        if packed {
            assert!(
                oids.iter().any(|oid| {
                    let mut scratch = Vec::new();
                    let length = reader
                        .odb
                        .lock()
                        .unwrap()
                        .try_find(&parse_oid(oid).unwrap(), &mut scratch)
                        .unwrap()
                        .unwrap()
                        .data
                        .len();
                    eprintln!(
                        "odb_decoder_body_bytes={length} capacity_bytes={}",
                        scratch.capacity()
                    );
                    scratch.capacity() > length
                }),
                "fixture must exercise spare delta scratch capacity"
            );
        }
        for (oid, expected) in oids.iter().zip([&content, &changed]) {
            let actual = reader
                .read_native(&parse_oid(oid).unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(actual.as_ref(), expected, "packed={packed}, oid={oid}");
            assert_eq!(
                actual.try_into_mut().unwrap().capacity(),
                expected.len(),
                "decoded bytes must not retain delta scratch capacity"
            );
        }
    }
}

#[test]
fn odb_reader_serves_from_cache_on_second_read() {
    let content = b"read me twice";
    let (_dir, git_dir, oid_hex) = create_git_repo_with_blob(content);

    let reader = OdbReader::new(&git_dir).unwrap();

    // First read — from ODB.
    let first = reader.read_blob(&oid_hex).unwrap();
    assert_eq!(&first[..], content);

    // Second read — from cache (same result).
    let second = reader.read_blob(&oid_hex).unwrap();
    assert_eq!(&second[..], content);
    assert_eq!(first, second);
}

#[test]
fn odb_reader_ignores_unverified_disk_blob_bytes() {
    let content = b"verified Git blob";
    let (root, git_dir, oid) = create_git_repo_with_blob(content);
    let cache = root.path().join("blob_cache");
    let reader = OdbReader::new(&git_dir).unwrap();
    reader.read_blob(&oid).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join(&oid), vec![b'x'; content.len()]).unwrap();
    assert_eq!(reader.read_blob(&oid).unwrap().as_ref(), content);
    drop(reader);
    let reopened = OdbReader::new(&git_dir).unwrap();
    assert_eq!(reopened.read_blob(&oid).unwrap().as_ref(), content);
}

#[test]
fn odb_reader_rejects_misnamed_loose_blob() {
    let (_root, git_dir, oid) = create_git_repo_with_blob(b"trusted original");
    let other = write_git_blob(&git_dir, b"corrupt original");
    let path = |id: &str| git_dir.join("objects").join(&id[..2]).join(&id[2..]);
    std::fs::remove_file(path(&oid)).unwrap();
    std::fs::copy(path(&other), path(&oid)).unwrap();
    // gix's lookup decodes bytes; its separate checksum API owns identity.
    use gix_object::Find as _;
    let native = gix_odb::at(git_dir.join("objects")).unwrap();
    let id = gix_hash::ObjectId::from_hex(oid.as_bytes()).unwrap();
    let mut decoded = Vec::new();
    assert_eq!(
        native.try_find(&id, &mut decoded).unwrap().unwrap().data,
        b"corrupt original"
    );
    let reader = OdbReader::new(&git_dir).unwrap();
    let error = reader.read_blob(&oid).unwrap_err();
    assert!(matches!(error, CrabError::GitObject { source, .. }
        if source.downcast_ref::<gix_object::data::verify::Error>().is_some()));
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(target_os = "linux")]
#[ignore = "requires an isolated process for /proc/self/io byte accounting"]
async fn odb_warm_ranges_do_not_reload_entire_blob() {
    let content = vec![42; 4 * 1024 * 1024];
    let (_root, git_dir, oid) = create_git_repo_with_blob(&content);
    let reader = Arc::new(OdbReader::new(&git_dir).unwrap());
    reader.read_blob(&oid).unwrap();
    let rchar = || {
        std::fs::read_to_string("/proc/self/io")
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("rchar: "))
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    let before = rchar();
    let started = std::time::Instant::now();
    for offset in (0..content.len()).step_by(4096) {
        let bytes = reader
            .read_blob_range(&oid, offset as u64, 4096)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), &content[offset..offset + 4096]);
        std::hint::black_box(bytes);
    }
    let elapsed = started.elapsed();
    let read_bytes = rchar() - before;
    eprintln!(
        "odb_range_probe logical_bytes={} userspace_read_bytes={read_bytes} elapsed_us={}",
        content.len(),
        elapsed.as_micros()
    );
    assert!(
        read_bytes < 16 * 1024 * 1024,
        "warm 4 KiB reads reloaded {read_bytes} bytes for a 4 MiB blob"
    );
}

#[test]
fn odb_reader_promisor_fallback_ignores_new_pack_slot_pressure() {
    let (_dir, git_dir, initial_oid) = create_git_repo_with_blob(b"initial");
    let reader = OdbReader::new(&git_dir).unwrap();

    assert_eq!(&reader.read_blob(&initial_oid).unwrap()[..], b"initial");

    let mut last = None;
    for index in 0..33 {
        let content = format!("promisor blob {index}");
        let oid = write_git_blob(&git_dir, content.as_bytes());
        pack_git_blobs(&git_dir, &[&oid]);
        last = Some((oid, content));
    }
    assert!(
        std::process::Command::new("git")
            .arg("prune-packed")
            .env("GIT_DIR", &git_dir)
            .status()
            .unwrap()
            .success(),
        "git prune-packed failed"
    );

    let (oid, content) = last.unwrap();
    assert_eq!(&reader.read_blob(&oid).unwrap()[..], content.as_bytes());
}

#[tokio::test(flavor = "multi_thread")]
async fn odb_reader_read_blob_range_full() {
    let content = b"abcdefghij";
    let (_dir, git_dir, oid_hex) = create_git_repo_with_blob(content);

    let reader = Arc::new(OdbReader::new(&git_dir).unwrap());
    let data = reader.read_blob_range(&oid_hex, 0, 100).await.unwrap();
    assert_eq!(&data[..], content);
}

#[tokio::test(flavor = "multi_thread")]
async fn odb_reader_read_blob_range_with_offset() {
    let content = b"abcdefghij";
    let (_dir, git_dir, oid_hex) = create_git_repo_with_blob(content);

    let reader = Arc::new(OdbReader::new(&git_dir).unwrap());
    let data = reader.read_blob_range(&oid_hex, 3, 4).await.unwrap();
    assert_eq!(&data[..], b"defg");
}

#[tokio::test(flavor = "multi_thread")]
async fn odb_reader_read_blob_range_past_eof() {
    let content = b"short";
    let (_dir, git_dir, oid_hex) = create_git_repo_with_blob(content);

    let reader = Arc::new(OdbReader::new(&git_dir).unwrap());
    let data = reader.read_blob_range(&oid_hex, 100, 10).await.unwrap();
    assert!(data.is_empty());
}

#[test]
fn odb_reader_missing_oid_returns_not_found() {
    let (_dir, git_dir, _oid_hex) = create_git_repo_with_blob(b"x");

    let reader = OdbReader::new(&git_dir).unwrap();
    let err = reader
        .read_blob("0000000000000000000000000000000000000000")
        .unwrap_err();
    assert!(matches!(err, CrabError::NotFound { .. }));
}

#[test]
fn odb_reader_invalid_oid_returns_error() {
    let (_dir, git_dir, _oid_hex) = create_git_repo_with_blob(b"x");

    let reader = OdbReader::new(&git_dir).unwrap();
    let err = reader.read_blob("not-a-valid-hex-oid").unwrap_err();
    assert!(matches!(err, CrabError::GitObject { .. }));
}

#[test]
fn odb_reader_missing_objects_dir_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let fake_git_dir = dir.path().join("nonexistent.git");

    let err = OdbReader::new(&fake_git_dir).unwrap_err();
    assert!(matches!(err, CrabError::Io(_)));
}
