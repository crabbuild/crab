#![expect(clippy::unwrap_used, reason = "owner fixtures and assertions")]

use super::*;
use crate::test::git_repo::GIT_DIR_MUTEX;
use crate::vfs::mounts_registry;

struct CurrentDirGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    original: PathBuf,
}

impl CurrentDirGuard {
    fn set(path: &Path) -> Self {
        let lock = GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self {
            _lock: lock,
            original,
        }
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.original).unwrap();
    }
}

fn opts(repo: &str, mountpoint: PathBuf) -> NewMountOpts {
    NewMountOpts {
        repo: repo.to_owned(),
        mountpoint,
        backend: MountBackend::Nfs,
        git_ref: None,
        foreground: true,
        read_only: false,
        no_refresh: false,
        allow_nested: false,
        name: None,
        cancel: tokio_util::sync::CancellationToken::new(),
    }
}

#[test]
fn serving_registration_retains_canonical_source_after_it_disappears() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let mountpoint = tmp.path().join("view");
    let control_endpoint = Some("tcp:127.0.0.1:50000?token=parent-secret".to_owned());
    std::fs::create_dir_all(&mountpoint).unwrap();
    let _cwd = CurrentDirGuard::set(tmp.path());
    let registry_path = tmp.path().join("mounts.json");

    let source = crate::vfs::source::MountSource::canonical_input("./repo").unwrap();
    let canonical_repo = std::fs::canonicalize(&repo).unwrap();
    // The CLI normalizes before helper startup; registry publication must
    // retain that identity even if the source disappears while starting.
    std::fs::remove_dir_all(&repo).unwrap();
    register_nfs_mount(
        &registry_path,
        &opts(&source, mountpoint.clone()),
        &mountpoint,
        control_endpoint.clone(),
    )
    .unwrap();

    let entries = mounts_registry::read_entries(&registry_path).unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].source, canonical_repo.display().to_string());
    assert_eq!(entries[0].name, "repo");
    assert_eq!(entries[0].backend.as_deref(), Some("nfs"));
    assert_eq!(entries[0].pid, std::process::id());
    assert_eq!(entries[0].log_path, None);
    assert_eq!(entries[0].control_endpoint, control_endpoint);
}

#[test]
fn serving_registration_preserves_remote_source() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_path = tmp.path().join("mounts.json");
    let mountpoint = tmp.path().join("view");
    let control_endpoint = Some("tcp:127.0.0.1:50001?token=parent-secret".to_owned());
    std::fs::create_dir_all(&mountpoint).unwrap();

    register_nfs_mount(
        &registry_path,
        &opts("crab://bucket/repo", mountpoint.clone()),
        &mountpoint,
        control_endpoint.clone(),
    )
    .unwrap();

    let entries = mounts_registry::read_entries(&registry_path).unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].source, "crab://bucket/repo");
    assert_eq!(entries[0].name, "repo");
    assert_eq!(entries[0].backend.as_deref(), Some("nfs"));
    assert_eq!(entries[0].pid, std::process::id());
    assert_eq!(entries[0].log_path, None);
    assert_eq!(entries[0].control_endpoint, control_endpoint);
}

#[test]
fn launcher_waits_for_the_serving_registration() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("mounts.json");
    assert!(
        !attach_nfs_helper_log(&registry, Path::new("/view"), 123, None, Path::new("/log"))
            .unwrap()
    );
    assert!(!registry.exists());
}

#[test]
fn launcher_log_attachment_preserves_the_serving_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("mounts.json");
    let mountpoint = tmp.path().join("view");
    let endpoint = Some("unix:/owned.sock".to_owned());
    for read_only in [false, true] {
        let mut selected = opts("crab://bucket/repo", mountpoint.clone());
        selected.read_only = read_only;
        selected.git_ref = Some("topic".into());
        selected.name = Some("chosen name".into());
        let mut owner =
            register_nfs_mount(&registry, &selected, &mountpoint, endpoint.clone()).unwrap();
        let log = tmp.path().join("helper.log");
        assert!(
            attach_nfs_helper_log(&registry, &mountpoint, owner.pid, endpoint.as_deref(), &log,)
                .unwrap()
        );
        owner.log_path = Some(log.display().to_string());
        assert_eq!(
            mounts_registry::list_entries(&registry).unwrap(),
            vec![owner]
        );
    }
}

#[test]
fn launcher_cannot_claim_another_serving_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("mounts.json");
    let mountpoint = tmp.path().join("view");
    let endpoint = Some("unix:/owned.sock".to_owned());
    let owner = register_nfs_mount(
        &registry,
        &opts("crab://bucket/repo", mountpoint.clone()),
        &mountpoint,
        endpoint.clone(),
    )
    .unwrap();
    for field in ["pid", "endpoint", "backend"] {
        let mut replacement = owner.clone();
        match field {
            "pid" => replacement.pid += 1,
            "endpoint" => replacement.control_endpoint = Some("unix:/replacement.sock".into()),
            _ => replacement.backend = Some("fuse".into()),
        }
        mounts_registry::add_entry(&registry, replacement.clone()).unwrap();
        assert!(
            !attach_nfs_helper_log(
                &registry,
                &mountpoint,
                owner.pid,
                endpoint.as_deref(),
                Path::new("/log"),
            )
            .unwrap(),
            "{field}"
        );
        assert_eq!(
            mounts_registry::list_entries(&registry).unwrap(),
            vec![replacement]
        );
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn failed_startup_gracefully_stops_and_reaps_its_child() {
    use std::io::BufRead;
    use std::process::{Command, Stdio};

    let mut child = Command::new("sh")
        .args(["-c", "trap 'exit 0' INT; printf 'ready\\n'; read line"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "ready\n");
    stop_failed_nfs_helper(&mut child, None).await.unwrap();
    assert!(child.try_wait().unwrap().unwrap().success());
}
