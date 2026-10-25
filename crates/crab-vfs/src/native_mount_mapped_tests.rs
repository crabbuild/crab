use super::*;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy)]
enum Transition {
    Refresh,
    Switch,
    Automatic,
    Inspect,
}

async fn qualify_mapped_transition(fuse: bool, transition_kind: Transition) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(&source, &["config", "user.name", "VFS audit"]);
    git(&source, &["config", "user.email", "audit@example.invalid"]);
    std::fs::write(source.join("mapped.txt"), vec![b'A'; 4096]).unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "base"]);
    let mut config = PipelineConfig {
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
    let pause = output.engine.pause_next_writeback();
    let path = mountpoint.join("mapped.txt");
    // A real client owns the mapping. In-process mappings/descriptors are
    // inherited by the server's Git children, whose exec/close can wait on the
    // deliberately paused WRITE before the controller is even invoked.
    let mut writer = tokio::process::Command::new("python3")
        .args([
            "-c",
            r"
import ctypes, mmap, os, sys
fd = os.open(sys.argv[1], os.O_RDWR)
mapping = mmap.mmap(fd, 4096)
mapping[0] = ord('B')
sys.stdout.buffer.write(b'1')
sys.stdout.buffer.flush()
libc = ctypes.CDLL(None, use_errno=True)
if libc.sync_file_range(fd, ctypes.c_longlong(0), ctypes.c_longlong(4096), 2):
    raise OSError(ctypes.get_errno(), 'sync_file_range')
sys.stdin.buffer.read(1)
os.fsync(fd)
sys.stdout.buffer.write(b'2')
sys.stdout.buffer.flush()
sys.stdin.buffer.read(1)
mapping.close()
os.close(fd)
",
        ])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut release = writer.stdin.take().unwrap();
    let mut ready = writer.stdout.take().unwrap();
    ready.read_u8().await.unwrap();
    let entered = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        pause.state.entered.notified(),
    )
    .await;
    if entered.is_err() {
        pause.state.resume.add_permits(1);
        release.write_u8(1).await.unwrap();
        drop(release);
        let writer_result = writer.wait().await;
        mounted.stop().await;
        output.engine.shutdown_background().await;
        assert!(
            entered.is_ok(),
            "client WRITE did not reach admission: {writer_result:?}"
        );
        return;
    }
    if matches!(transition_kind, Transition::Switch) {
        git(&source, &["switch", "-c", "next"]);
    }
    std::fs::write(source.join("mapped.txt"), vec![b'Z'; 4096]).unwrap();
    git(&source, &["add", "mapped.txt"]);
    git(&source, &["commit", "-m", "diverging base"]);
    let resolver = output.resolver.clone();
    let generation = resolver.generation();
    let mut transition = tokio::spawn(async move {
        let updated = match transition_kind {
            Transition::Switch => crate::mount_runtime::switch_mount_runtime(
                &mut output,
                &mut config,
                &mountpoint,
                "next",
            )
            .await
            .map(|_| ()),
            Transition::Refresh => {
                crate::mount_runtime::refresh_mount_runtime(&mut output, &config, &mountpoint)
                    .await
                    .map(|_| ())
            }
            Transition::Inspect => crate::mount_runtime::inspect_overlay_runtime(
                Arc::clone(&output.engine),
                output.overlay.clone().unwrap(),
                mountpoint,
            )
            .await
            .map(|_| ()),
            Transition::Automatic => {
                let refresh = crate::pipeline::spawn_refresh_loop(
                    &output,
                    &config,
                    &mountpoint,
                    std::time::Duration::from_secs(30),
                )
                .unwrap();
                let advanced = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while output.resolver.generation() == generation {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                })
                .await;
                config.cancel_token.cancel();
                refresh.await.unwrap();
                advanced.unwrap();
                Ok(())
            }
        };
        (updated, output)
    });
    let control_started = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        pause.state.control_started.notified(),
    )
    .await;
    let progressed = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        tokio::select! {
            completed = &mut transition => Some(completed),
            () = async {
                while resolver.generation() == generation {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            } => None,
        }
    })
    .await;
    let held_generation = resolver.generation();
    let waited_for_write = progressed.is_err();
    pause.state.resume.add_permits(1);
    let (updated, output) = match progressed {
        Ok(Some(completed)) => completed.unwrap(),
        _ => transition.await.unwrap(),
    };
    // Confirm completion without dropping the mapping: late unmap writeback
    // must not mask lost server-side ownership after a base transition.
    release.write_u8(1).await.unwrap();
    let synchronized =
        tokio::time::timeout(std::time::Duration::from_secs(10), ready.read_u8()).await;
    let visible = output.engine.read("mapped.txt", 0, 4096).await;
    release.write_u8(1).await.unwrap();
    let writer_result = writer.wait().await.unwrap();
    mounted.stop().await;
    output.engine.shutdown_background().await;
    assert!(
        control_started.is_ok(),
        "controller did not enter admission"
    );
    assert!(writer_result.success());
    assert!(
        matches!(synchronized, Ok(Ok(b'2'))),
        "client fsync did not complete: {synchronized:?}"
    );
    updated.unwrap();
    let mut expected = vec![b'A'; 4096];
    expected[0] = b'B';
    let visible = visible.unwrap();
    assert!(
        waited_for_write
            && held_generation == generation
            && visible.as_ref() == expected.as_slice(),
        "mapped transition: waited={waited_for_write}, generation={generation}, held_generation={held_generation}, actual prefix {:?}, expected prefix {:?}",
        &visible[..visible.len().min(8)],
        &expected[..8]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn fuse_mapped_refresh_without_msync() {
    qualify_mapped_transition(true, Transition::Refresh).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn nfs_mapped_refresh_without_msync() {
    qualify_mapped_transition(false, Transition::Refresh).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn fuse_mapped_switch_without_msync() {
    qualify_mapped_transition(true, Transition::Switch).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn nfs_mapped_switch_without_msync() {
    qualify_mapped_transition(false, Transition::Switch).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn fuse_mapped_automatic_refresh_without_msync() {
    qualify_mapped_transition(true, Transition::Automatic).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn nfs_mapped_automatic_refresh_without_msync() {
    qualify_mapped_transition(false, Transition::Automatic).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn fuse_mapped_inspection_without_msync() {
    qualify_mapped_transition(true, Transition::Inspect).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated Linux environment with native FUSE/NFS mount privileges"]
async fn nfs_mapped_inspection_without_msync() {
    qualify_mapped_transition(false, Transition::Inspect).await;
}
