#![expect(clippy::unwrap_used, reason = "native test setup and assertions")]

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;

use crate::pipeline::{MountPipelineBuilder, PipelineConfig};
use tokio_util::sync::CancellationToken;

fn git(source: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(source)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct NativeMount {
    fuse: Option<fuser::BackgroundSession>,
    nfs: Option<tokio::task::JoinHandle<crate::Result<()>>>,
    cancel: CancellationToken,
    path: std::path::PathBuf,
}

struct Mapping {
    address: *mut libc::c_void,
    len: usize,
    writable: bool,
}

impl Mapping {
    fn shared(file: &std::fs::File, writable: bool) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        let len = 4096;
        let protection = libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 };
        // SAFETY: the fixture file covers the mapped page; mmap retains its
        // own kernel file reference until the matching munmap in Drop.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                protection,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            address,
            len,
            writable,
        })
    }

    fn byte(&self, offset: usize) -> u8 {
        assert!(offset < self.len);
        // SAFETY: the mapping is readable and the checked offset is in bounds.
        // Volatile reads observe changes made through another descriptor.
        unsafe { self.address.cast::<u8>().add(offset).read_volatile() }
    }

    fn set_byte(&mut self, offset: usize, value: u8) {
        assert!(self.writable && offset < self.len);
        // SAFETY: this live mapping is writable and the offset is in bounds.
        unsafe { self.address.cast::<u8>().add(offset).write_volatile(value) };
    }

    fn sync(&self) -> std::io::Result<()> {
        // SAFETY: address and length describe the live mapping owned by self.
        if unsafe { libc::msync(self.address, self.len, libc::MS_SYNC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: self exclusively owns this mapping and unmaps it once.
        unsafe { libc::munmap(self.address, self.len) };
    }
}

impl NativeMount {
    async fn start(
        fuse: bool,
        config: &PipelineConfig,
        output: &crate::pipeline::PipelineOutput,
        path: &Path,
    ) -> Self {
        let mut mounted = Self {
            fuse: None,
            nfs: None,
            cancel: CancellationToken::new(),
            path: path.to_owned(),
        };
        if fuse {
            let session = crate::mount::mount(
                &crate::mount::MountConfig {
                    mountpoint: path.to_owned(),
                    git_dir: config.git_dir.to_string_lossy().into_owned(),
                    write_pid: false,
                    crab_dir: config.cache_dir.join("state"),
                    read_only: config.read_only,
                },
                output.resolver.clone(),
                output.engine.clone(),
                tokio::runtime::Handle::current(),
            )
            .unwrap();
            mounted.fuse = Some(session.session.spawn().unwrap());
        } else {
            let session = crate::nfs_mount::mount(
                &crate::nfs_mount::NfsMountConfig {
                    mountpoint: path.to_owned(),
                    git_dir: config.git_dir.to_string_lossy().into_owned(),
                    exclusive_verifiers_path: config.cache_dir.join("verifiers.json"),
                    read_only: config.read_only,
                    auto_refresh_interval: None,
                    control_endpoint_override: None,
                },
                output.resolver.clone(),
                output.engine.clone(),
                None,
            )
            .await
            .unwrap();
            mounted.nfs = Some(tokio::spawn(crate::nfs_mount::run_until_cancelled(
                session,
                mounted.cancel.clone(),
            )));
        }
        mounted
    }

    async fn stop(self) {
        if let Some(session) = self.fuse {
            let path = self.path;
            tokio::task::spawn_blocking(move || {
                crate::mount::unmount_background_session(session, &path)
            })
            .await
            .unwrap()
            .unwrap();
        }
        self.cancel.cancel();
        if let Some(task) = self.nfs {
            task.await.unwrap().unwrap();
        }
    }
}

async fn qualify(fuse: bool, read_only: bool) {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(&source, &["config", "user.email", "audit@localhost"]);
    git(&source, &["config", "user.name", "VFS audit"]);
    let expected = bytes::Bytes::from(
        (0..1024 * 1024)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>(),
    );
    let stored =
        crate::test_support::StoredPointer::new(&temp.path().join("shared"), expected.clone())
            .await;
    std::fs::write(source.join("base.txt"), b"ordinary Git data").unwrap();
    std::fs::write(source.join("model.bin"), stored.pointer.serialize()).unwrap();
    std::fs::write(
        source.join("unlinked-model.bin"),
        stored.pointer.serialize(),
    )
    .unwrap();
    std::fs::create_dir(source.join("dir")).unwrap();
    std::fs::write(source.join("dir/nested.txt"), b"nested").unwrap();
    std::os::unix::fs::symlink("base.txt", source.join("link")).unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "seed"]);
    git(&source, &["checkout", "-b", "other"]);
    std::fs::write(source.join("other.txt"), b"selected branch").unwrap();
    git(&source, &["add", "other.txt"]);
    git(&source, &["commit", "-m", "other branch"]);
    git(&source, &["checkout", "main"]);
    std::fs::write(source.join("base.txt"), b"staged source change").unwrap();
    git(&source, &["add", "base.txt"]);
    let source_state = (
        std::fs::read(source.join(".git/index")).unwrap(),
        std::fs::read(source.join(".git/HEAD")).unwrap(),
        std::fs::read(source.join("base.txt")).unwrap(),
    );
    let config = PipelineConfig {
        source: source.to_string_lossy().into_owned(),
        git_dir: source.join(".git"),
        git_metadata: crate::pipeline::GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/other".into()),
        read_only,
        cache_dir: temp.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let output = MountPipelineBuilder::new(config.clone())
        .with_read_context(stored.context)
        .execute()
        .unwrap();
    let mountpoint = temp.path().join("mount");
    std::fs::create_dir(&mountpoint).unwrap();
    let start = Instant::now();
    let mounted = NativeMount::start(fuse, &config, &output, &mountpoint).await;
    let startup = start.elapsed();
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        assert_eq!(std::fs::read(mountpoint.join("other.txt"))?, b"selected branch");
        assert_eq!(std::fs::read(mountpoint.join("base.txt"))?, b"ordinary Git data");
        assert_eq!(std::fs::read(mountpoint.join("dir/nested.txt"))?, b"nested");
        assert_eq!(std::fs::read(mountpoint.join("link"))?, b"ordinary Git data");
        assert!(std::fs::read_to_string(mountpoint.join(".git"))?.starts_with("gitdir: "));
        assert_eq!(std::fs::metadata(mountpoint.join("model.bin"))?.len(), expected.len() as u64);
        let mut model = std::fs::File::open(mountpoint.join("model.bin"))?;
        model.seek(SeekFrom::Start(17))?;
        let start = Instant::now();
        let mut range = vec![0; 4096];
        model.read_exact(&mut range)?;
        assert_eq!(&range, &expected[17..4113]);
        let cold = start.elapsed();
        let start = Instant::now();
        model.seek(SeekFrom::Start(0))?;
        let mut full = Vec::new();
        model.read_to_end(&mut full)?;
        assert_eq!(full, expected.as_ref());
        let warm = start.elapsed();
        let names = std::fs::read_dir(&mountpoint)?.collect::<std::io::Result<Vec<_>>>()?;
        assert!(names.len() >= 5);
        eprintln!("native_audit fuse={fuse} read_only={read_only} startup_ms={} cold_4k_us={} warm_1m_us={}", startup.as_millis(), cold.as_micros(), warm.as_micros());
        if read_only {
            assert!(std::fs::write(mountpoint.join("forbidden"), b"x").is_err());
            return Ok(());
        }
        {
            use std::os::unix::fs::FileExt;
            let file = std::fs::OpenOptions::new().read(true).write(true)
                .open(mountpoint.join("model.bin"))?;
            let mapped = Mapping::shared(&model, false)?;
            let original = mapped.byte(0);
            file.write_all_at(&[original ^ 0xff], 0)?;
            file.sync_all()?;
            assert_eq!(mapped.byte(0), original ^ 0xff);
            file.write_all_at(&[original], 0)?;
            file.sync_all()?;
            drop(mapped);
            let mut mapped = Mapping::shared(&file, true)?;
            let original = mapped.byte(1);
            mapped.set_byte(1, original ^ 0xff);
            mapped.sync()?;
            let mut observed = [0];
            file.read_exact_at(&mut observed, 1)?;
            assert_eq!(observed[0], original ^ 0xff);
            mapped.set_byte(1, original);
            mapped.sync()?;
        }
        eprintln!("native_mmap fuse={fuse} shared_read_write_coherent=true");
        let mut ordinary = std::fs::OpenOptions::new()
            .read(true).write(true).open(mountpoint.join("base.txt"))?;
        ordinary.seek(SeekFrom::Start(3))?;
        ordinary.write_all(b"!")?;
        ordinary.sync_all()?;
        assert_eq!(std::fs::read(mountpoint.join("base.txt"))?, b"ord!nary Git data");
        eprintln!("native_blob_cow fuse={fuse} verified=true");
        drop(ordinary);
        std::fs::create_dir(mountpoint.join("newdir"))?;
        let original = mountpoint.join("newdir/created.txt");
        let renamed = mountpoint.join("newdir/renamed.txt");
        let mut open = std::fs::OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&original)?;
        open.write_all(b"written through mounted filesystem")?;
        open.sync_all()?;
        assert_eq!(std::fs::read(&original)?, b"written through mounted filesystem");
        std::fs::rename(&original, &renamed)?;
        assert_eq!(std::fs::read(&renamed)?, b"written through mounted filesystem");
        open.seek(SeekFrom::Start(0))?;
        let mut after = Vec::new();
        open.read_to_end(&mut after).map_err(|error| std::io::Error::new(error.kind(), format!("read through open descriptor after rename: {error}")))?;
        assert_eq!(after, b"written through mounted filesystem");
        open.write_all(b"!")?;
        open.sync_all()?;
        drop(open);
        std::fs::remove_file(&renamed)?;
        std::fs::remove_dir(mountpoint.join("newdir"))?;

        let named = mountpoint.join("dir/nested.txt");
        let mut reader = std::fs::File::open(&named)?;
        let mut writer = std::fs::OpenOptions::new().read(true).write(true).open(&named)?;
        writer.seek(SeekFrom::Start(1))?;
        writer.write_all(b"Z")?;
        let mut bytes = [0; 6];
        reader.read_exact(&mut bytes)?;
        assert_eq!(&bytes, b"nZsted");
        writer.set_len(3)?;
        std::fs::remove_file(&named)?;
        std::fs::write(&named, b"replacement")?;
        writer.seek(SeekFrom::Start(0))?;
        writer.write_all(b"old")?;
        writer.sync_all()?;
        reader.seek(SeekFrom::Start(0))?;
        let mut old = Vec::new();
        reader.read_to_end(&mut old)?;
        assert_eq!(old, b"old");
        assert_eq!(writer.metadata()?.len(), 3);
        assert_eq!(std::fs::read(&named)?, b"replacement");

        let victim = mountpoint.join("victim.txt");
        let replacement = mountpoint.join("replacement.txt");
        std::fs::write(&victim, b"victim")?;
        let mut held = std::fs::OpenOptions::new().read(true).write(true).open(&victim)?;
        std::fs::write(&replacement, b"new inode")?;
        std::fs::rename(&replacement, &victim)?;
        let mut old = Vec::new();
        held.read_to_end(&mut old)?;
        assert_eq!(old, b"victim");
        held.write_all(b"!")?;
        held.sync_all()?;
        assert_eq!(std::fs::read(&victim)?, b"new inode");

        if fuse {
            use std::os::unix::ffi::OsStrExt;
            let from = mountpoint.join("no-replace-source");
            let to = mountpoint.join("no-replace-target");
            std::fs::write(&from, b"source")?;
            std::fs::write(&to, b"target")?;
            let from_c = std::ffi::CString::new(from.as_os_str().as_bytes()).unwrap();
            let to_c = std::ffi::CString::new(to.as_os_str().as_bytes()).unwrap();
            // SAFETY: both C strings remain valid for the synchronous syscall.
            let result = unsafe {
                libc::renameat2(libc::AT_FDCWD, from_c.as_ptr(), libc::AT_FDCWD,
                    to_c.as_ptr(), libc::RENAME_NOREPLACE)
            };
            assert_eq!((result, std::io::Error::last_os_error().raw_os_error()),
                (-1, Some(libc::EEXIST)), "no-replace rename overwrote an existing name");
            assert_eq!(std::fs::read(&to)?, b"target");
            // SAFETY: both C strings remain valid for the synchronous syscall.
            let exchange = unsafe {
                libc::renameat2(libc::AT_FDCWD, from_c.as_ptr(), libc::AT_FDCWD,
                    to_c.as_ptr(), libc::RENAME_EXCHANGE)
            };
            assert_eq!((exchange, std::io::Error::last_os_error().raw_os_error()),
                (-1, Some(libc::EINVAL)), "unsupported exchange must leave both names intact");
            assert_eq!(std::fs::read(&from)?, b"source");
            assert_eq!(std::fs::read(&to)?, b"target");
        }

        let mut pointer_writer = std::fs::OpenOptions::new().read(true).write(true)
            .open(mountpoint.join("model.bin"))?;
        pointer_writer.write_all(b"M")?;
        pointer_writer.sync_all()?;
        model.seek(SeekFrom::Start(0))?;
        let mut first = [0; 8];
        model.read_exact(&mut first)?;
        assert_eq!(&first[1..], &expected[1..8]);
        assert_eq!(first[0], b'M');

        let unnamed = mountpoint.join("unlinked-model.bin");
        let mut unlinked = std::fs::OpenOptions::new().read(true).write(true).open(&unnamed)?;
        std::fs::remove_file(&unnamed)?;
        unlinked.write_all(b"ghost")?;
        unlinked.sync_all()?;
        unlinked.seek(SeekFrom::Start(0))?;
        let mut first = [0; 8];
        unlinked.read_exact(&mut first)?;
        assert_eq!(&first[..5], b"ghost");
        assert_eq!(&first[5..], &expected[5..8]);
        assert!(!unnamed.exists());
        eprintln!("native_open_lifetime fuse={fuse} rename_unlink_replacement_cow=true");
        Ok(())
    }).await;
    mounted.stop().await;
    output.engine.shutdown_background().await;
    assert_eq!(
        (
            std::fs::read(source.join(".git/index")).unwrap(),
            std::fs::read(source.join(".git/HEAD")).unwrap(),
            std::fs::read(source.join("base.txt")).unwrap(),
        ),
        source_state,
        "mount changed source staging or HEAD"
    );
    let result = result.unwrap();
    assert!(result.is_ok(), "native operations failed: {result:?}");
}

async fn qualify_mapped_commit(fuse: bool) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(&source, &["config", "user.name", "VFS audit"]);
    git(&source, &["config", "user.email", "audit@example.invalid"]);
    std::fs::write(source.join("mapped.txt"), vec![b'A'; 4096]).unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "base"]);
    let config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: source.join(".git"),
        git_metadata: crate::pipeline::GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: false,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let mut output = MountPipelineBuilder::new(config.clone()).execute().unwrap();
    let mountpoint = root.path().join("mount");
    std::fs::create_dir(&mountpoint).unwrap();
    let mounted = NativeMount::start(fuse, &config, &output, &mountpoint).await;
    let path = mountpoint.join("mapped.txt");
    let (closed, release) = tokio::sync::oneshot::channel::<()>();
    let (ready, dirty) = tokio::sync::oneshot::channel();
    let writer = tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let mut mapping = Mapping::shared(&file, true).unwrap();
        mapping.set_byte(0, b'B');
        ready.send(()).unwrap();
        let _ = release.blocking_recv();
    });
    dirty.await.unwrap();
    let engine = output.engine.clone();
    let reset = crate::mount_runtime::begin_mount_update(&engine, &mountpoint)
        .await
        .unwrap();
    let result = crate::mount_runtime::commit_mount_runtime(
        &mut output,
        &config,
        &reset,
        "mapped edit without msync".into(),
        false,
    )
    .await;
    drop(reset);
    drop(closed);
    writer.await.unwrap();
    mounted.stop().await;
    output.engine.shutdown_background().await;
    let result = result.unwrap();
    let committed = std::process::Command::new("git")
        .arg("-C")
        .arg(source)
        .args(["show", "main:mapped.txt"])
        .output()
        .unwrap();
    assert_eq!(
        (
            result.commit_oid.is_some(),
            committed.stdout.first().copied()
        ),
        (true, Some(b'B'))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn fuse_mapped_commit_without_msync() {
    qualify_mapped_commit(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn nfs_mapped_commit_without_msync() {
    qualify_mapped_commit(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn fuse_readonly() {
    qualify(true, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn fuse_writable() {
    qualify(true, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn nfs_readonly() {
    qualify(false, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn nfs_writable() {
    qualify(false, false).await;
}

async fn committed_pointer_remount(fuse: bool) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(&source, &["config", "user.email", "audit@localhost"]);
    git(&source, &["config", "user.name", "VFS audit"]);
    std::fs::write(source.join(".gitattributes"), b"*.bin filter=crab\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "attributes"]);
    let mut config = PipelineConfig {
        source: source.to_string_lossy().into_owned(),
        git_dir: source.join(".git"),
        git_metadata: crate::pipeline::GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: false,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let mut output = MountPipelineBuilder::new(config.clone()).execute().unwrap();
    let path = root.path().join("mount");
    std::fs::create_dir(&path).unwrap();
    let expected = bytes::Bytes::from(
        (0..8 * 1024 * 1024)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>(),
    );
    let mounted = NativeMount::start(fuse, &config, &output, &path).await;
    let file = path.join("new.bin");
    let written = expected.clone();
    let write_result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mut file = std::fs::File::create(file)?;
        file.write_all(&written)?;
        file.sync_all()
    })
    .await;
    mounted.stop().await;
    write_result.unwrap().unwrap();
    let engine = output.engine.clone();
    let reset = engine.begin_overlay_reset().await;
    crate::mount_runtime::commit_mount_runtime(
        &mut output,
        &config,
        &reset,
        "native unpushed pointer".into(),
        false,
    )
    .await
    .unwrap();
    drop(reset);
    output.engine.shutdown_background().await;
    drop(output);
    // A fresh mount and service exclude client/server warm content from proof.
    config.read_only = true;
    let output = MountPipelineBuilder::new(config.clone()).execute().unwrap();
    let mounted = NativeMount::start(fuse, &config, &output, &path).await;
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let actual = std::fs::read(path.join("new.bin"))?;
        assert_eq!(blake3::hash(&actual), blake3::hash(&expected));
        Ok(())
    })
    .await;
    mounted.stop().await;
    output.engine.shutdown_background().await;
    result.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn fuse_committed_pointer_remount() {
    committed_pointer_remount(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn nfs_committed_pointer_remount() {
    committed_pointer_remount(false).await;
}

#[path = "native_mount_mapped_tests.rs"]
mod mapped;
