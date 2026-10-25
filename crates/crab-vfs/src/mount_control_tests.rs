#![expect(clippy::unwrap_used, reason = "control fixture setup and assertions")]
#![cfg_attr(
    unix,
    expect(clippy::panic, reason = "test peer rejects unexpected operations")
)]

use super::*;

#[cfg(feature = "nfs")]
use std::sync::Arc;

#[cfg(feature = "nfs")]
use tokio_util::sync::CancellationToken;

struct HomeGuard {
    original: Option<std::ffi::OsString>,
}

impl HomeGuard {
    fn set(home: &Path) -> Self {
        let original = std::env::var_os("HOME");
        // SAFETY: these tests update HOME before starting any worker
        // threads and restore it before returning to the harness.
        unsafe {
            std::env::set_var("HOME", home);
        }
        Self { original }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        // SAFETY: restores the process environment for this single-threaded
        // test scope.
        unsafe {
            if let Some(original) = &self.original {
                std::env::set_var("HOME", original);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }
}

#[test]
fn backend_names_match_registry_values() {
    assert_eq!(MountControlBackend::Nfs.as_str(), "nfs");
    assert_eq!(MountControlBackend::Fuse.as_str(), "fuse");
}

#[test]
fn normalize_preserves_missing_paths() {
    let path = Path::new("/tmp/crab-missing-mount-control-test");
    assert_eq!(normalize_mountpoint(path), path);
}

#[test]
fn persisted_list_reads_registry_entries() {
    let _env_lock = crate::test_support::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(tmp.path());
    let mountpoint = tmp.path().join("view");
    let registry_path = crate::mounts_registry::registry_path().unwrap();
    crate::mounts_registry::add_entry(
        &registry_path,
        crate::mounts_registry::MountEntry {
            mountpoint: mountpoint.display().to_string(),
            source: "crab://bucket/repo".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            pid: 123,
            start_time: "2026-01-01T00:00:00Z".to_owned(),
            read_only: true,
            name: "repo".to_owned(),
            backend: Some("nfs".to_owned()),
            log_path: Some("/tmp/crab-nfs.log".to_owned()),
            control_endpoint: Some("unix:/tmp/crab-nfs.sock".to_owned()),
        },
    )
    .unwrap();

    let entries = persisted_list().unwrap();

    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.name, "repo");
    assert_eq!(entry.backend.as_deref(), Some("nfs"));
    assert_eq!(entry.mountpoint, mountpoint.display().to_string());
    assert_eq!(entry.source, "crab://bucket/repo");
    assert_eq!(entry.head_ref, "refs/heads/main");
    assert!(entry.read_only);
    assert_eq!(entry.pid, Some(123));
    assert_eq!(entry.start_time.as_deref(), Some("2026-01-01T00:00:00Z"));
    assert_eq!(entry.log_path.as_deref(), Some("/tmp/crab-nfs.log"));
    assert_eq!(
        entry.control_endpoint.as_deref(),
        Some("unix:/tmp/crab-nfs.sock")
    );
    assert!(!entry.live);
}

#[test]
fn list_keeps_registry_entries_when_live_probe_fails() {
    let _env_lock = crate::test_support::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(tmp.path());
    let mountpoint = tmp.path().join("view");
    let registry_path = crate::mounts_registry::registry_path().unwrap();
    crate::mounts_registry::add_entry(
        &registry_path,
        crate::mounts_registry::MountEntry {
            mountpoint: mountpoint.display().to_string(),
            source: "crab://bucket/repo".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            pid: 123,
            start_time: "2026-01-01T00:00:00Z".to_owned(),
            read_only: true,
            name: "repo".to_owned(),
            backend: Some("nfs".to_owned()),
            log_path: None,
            control_endpoint: Some("unix:/tmp/crab-nfs-missing.sock".to_owned()),
        },
    )
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let entries = runtime.block_on(list()).unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].mountpoint, mountpoint.display().to_string());
    assert!(!entries[0].live);
}

#[test]
fn upsert_list_entry_preserves_registry_metadata() {
    let mut entries = vec![MountControlListEntry {
        name: "repo".to_owned(),
        backend: Some("nfs".to_owned()),
        mountpoint: "/mnt/repo".to_owned(),
        source: "crab://bucket/repo".to_owned(),
        head_ref: "refs/heads/main".to_owned(),
        read_only: true,
        pid: Some(123),
        start_time: Some("2026-01-01T00:00:00Z".to_owned()),
        log_path: Some("/tmp/crab-nfs.log".to_owned()),
        control_endpoint: Some("unix:/tmp/crab-nfs.sock".to_owned()),
        live: false,
    }];

    upsert_list_entry(
        &mut entries,
        MountControlListEntry {
            name: "ignored".to_owned(),
            backend: Some("fuse".to_owned()),
            mountpoint: "/mnt/repo".to_owned(),
            source: "crab://bucket/repo-live".to_owned(),
            head_ref: "refs/heads/dev".to_owned(),
            read_only: false,
            pid: Some(456),
            start_time: None,
            log_path: None,
            control_endpoint: None,
            live: true,
        },
    );

    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.name, "repo");
    assert_eq!(entry.backend.as_deref(), Some("fuse"));
    assert_eq!(entry.source, "crab://bucket/repo-live");
    assert_eq!(entry.head_ref, "refs/heads/dev");
    assert!(!entry.read_only);
    assert_eq!(entry.pid, Some(456));
    assert_eq!(entry.start_time.as_deref(), Some("2026-01-01T00:00:00Z"));
    assert_eq!(entry.log_path.as_deref(), Some("/tmp/crab-nfs.log"));
    assert_eq!(
        entry.control_endpoint.as_deref(),
        Some("unix:/tmp/crab-nfs.sock")
    );
    assert!(entry.live);
}

#[cfg(feature = "nfs")]
fn nfs_control_state(
    mountpoint: &Path,
    read_only: bool,
    shutdown: CancellationToken,
) -> crate::nfs_control::NfsControlState {
    crate::nfs_control::NfsControlState {
        shutdown,
        mountpoint: mountpoint.to_path_buf(),
        read_only,
        read_leases: crate::read_lease_pool::ReadLeasePool::new(4, 4096),
        directory_pages: crate::nfs::NfsDirectoryPageCache::new(4, 4096),
        write_journal: Arc::new(crate::nfs::NfsWriteJournal::new()),
        protocol_stats: Arc::new(crate::nfs::NfsProtocolStats::new()),
        engine: None,
        runtime: None,
        lifecycle: crate::nfs_control::NfsMountLifecycleStatus {
            server_bind_ms: 3,
            native_mount_ms: 5,
            startup_ms: 8,
        },
    }
}

#[cfg(feature = "nfs")]
fn free_tcp_control_endpoint() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("tcp:{addr}?token=mount-control-test")
}

#[cfg(feature = "nfs")]
fn add_nfs_mount_entry(
    registry_path: &Path,
    mountpoint: &Path,
    endpoint: &str,
) -> crate::mounts_registry::MountEntry {
    let entry = crate::mounts_registry::MountEntry {
        mountpoint: mountpoint.display().to_string(),
        source: "crab://bucket/repo".to_owned(),
        git_ref: "refs/heads/main".to_owned(),
        pid: 123,
        start_time: "2026-01-01T00:00:00Z".to_owned(),
        read_only: true,
        name: "repo".to_owned(),
        backend: Some("nfs".to_owned()),
        log_path: Some("/tmp/crab-nfs.log".to_owned()),
        control_endpoint: Some(endpoint.to_owned()),
    };
    crate::mounts_registry::add_entry(registry_path, entry.clone()).unwrap();
    entry
}

#[cfg(feature = "nfs")]
async fn wait_for_nfs_status(path: &Path) -> MountControlStatus {
    for _ in 0..20 {
        if let Ok(Some(status)) = status(path).await {
            return status;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    status(path).await.unwrap().unwrap()
}

#[test]
#[cfg(feature = "nfs")]
fn nfs_control_status_and_shutdown_route_through_registry_endpoint() {
    let _env_lock = crate::test_support::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(tmp.path());
    let raw_mountpoint = tmp.path().join("view");
    std::fs::create_dir_all(&raw_mountpoint).unwrap();
    let mountpoint = normalize_mountpoint(&raw_mountpoint);
    let registry_path = crate::mounts_registry::registry_path().unwrap();
    let endpoint = free_tcp_control_endpoint();
    add_nfs_mount_entry(&registry_path, &mountpoint, &endpoint);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let cancel = CancellationToken::new();
        let source = tmp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "seed",
            ],
        ] {
            let result = std::process::Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_COMMON_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        let config = crate::pipeline::PipelineConfig {
            source: source.display().to_string(),
            git_dir: source.join(".git"),
            git_metadata: crate::pipeline::GitMetadataOwnership::Borrowed,
            ref_name: Some("refs/heads/main".into()),
            read_only: false,
            cache_dir: tmp.path().join("actual-live-cache"),
            cancel_token: CancellationToken::new(),
        };
        let output = crate::pipeline::MountPipelineBuilder::new(config.clone())
            .execute()
            .unwrap();
        let engine = Arc::clone(&output.engine);
        let mut state = nfs_control_state(&mountpoint, false, cancel.clone());
        state.engine = Some(Arc::clone(&engine));
        state.runtime = Some(Arc::new(std::sync::Mutex::new(
            crate::nfs_control::NfsMountRuntime {
                output,
                config: config.clone(),
            },
        )));
        let handle =
            crate::nfs_control::spawn_server(Some(endpoint.clone()), state, cancel.clone())
                .unwrap();

        let status = wait_for_nfs_status(&mountpoint).await;

        assert_eq!(status.backend, MountControlBackend::Nfs);
        assert_eq!(status.mountpoint, mountpoint);
        assert_eq!(status.source.as_deref(), Some("crab://bucket/repo"));
        assert_eq!(status.head_ref.as_deref(), Some("refs/heads/main"));
        assert!(!status.read_only);
        assert_eq!(status.pid, Some(std::process::id()));
        assert_eq!(
            status.nfs_runtime.unwrap().lifecycle,
            crate::nfs_control::NfsMountLifecycleStatus {
                server_bind_ms: 3,
                native_mount_ms: 5,
                startup_ms: 8,
            }
        );

        let shutdown = shutdown(&mountpoint).await.unwrap().unwrap();

        let (context, cache_dir) = match shutdown {
            MountControlShutdown::Nfs { context, cache_dir } => (context, cache_dir),
            #[cfg(unix)]
            MountControlShutdown::Fuse { .. } => panic!("expected NFS shutdown"),
        };
        assert_eq!(context.mountpoint, mountpoint);
        assert_eq!(context.registry_path, registry_path);
        assert_eq!(context.entry.pid, 123);
        assert_eq!(cache_dir, config.cache_dir);
        assert!(cancel.is_cancelled());
        handle.await.unwrap().unwrap();
        engine.shutdown_background().await;
    });
}

#[test]
#[cfg(feature = "nfs")]
fn nfs_control_list_refreshes_live_registry_entries() {
    let _env_lock = crate::test_support::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(tmp.path());
    let raw_mountpoint = tmp.path().join("view");
    std::fs::create_dir_all(&raw_mountpoint).unwrap();
    let mountpoint = normalize_mountpoint(&raw_mountpoint);
    let registry_path = crate::mounts_registry::registry_path().unwrap();
    let endpoint = free_tcp_control_endpoint();
    add_nfs_mount_entry(&registry_path, &mountpoint, &endpoint);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let cancel = CancellationToken::new();
        let handle = crate::nfs_control::spawn_server(
            Some(endpoint.clone()),
            nfs_control_state(&mountpoint, false, cancel.clone()),
            cancel.clone(),
        )
        .unwrap();
        wait_for_nfs_status(&mountpoint).await;

        let entries = list().await.unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mountpoint, mountpoint.display().to_string());
        assert_eq!(entries[0].backend.as_deref(), Some("nfs"));
        assert_eq!(entries[0].pid, Some(std::process::id()));
        assert!(!entries[0].read_only);
        assert!(entries[0].live);

        crate::nfs_control::shutdown(&endpoint).await.unwrap();
        handle.await.unwrap().unwrap();
        assert!(cancel.is_cancelled());
    });
}

#[test]
#[cfg(feature = "nfs")]
fn nfs_control_refresh_and_switch_route_to_helper_endpoint() {
    let _env_lock = crate::test_support::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(tmp.path());
    let raw_mountpoint = tmp.path().join("view");
    std::fs::create_dir_all(&raw_mountpoint).unwrap();
    let mountpoint = normalize_mountpoint(&raw_mountpoint);
    let registry_path = crate::mounts_registry::registry_path().unwrap();
    let endpoint = free_tcp_control_endpoint();
    add_nfs_mount_entry(&registry_path, &mountpoint, &endpoint);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let cancel = CancellationToken::new();
        let handle = crate::nfs_control::spawn_server(
            Some(endpoint.clone()),
            nfs_control_state(&mountpoint, true, cancel.clone()),
            cancel.clone(),
        )
        .unwrap();
        wait_for_nfs_status(&mountpoint).await;

        let refresh_error = refresh(&mountpoint).await.unwrap_err().to_string();
        let switch_error = switch_ref(&mountpoint, "refs/heads/dev")
            .await
            .unwrap_err()
            .to_string();

        assert!(refresh_error.contains("NFS control refresh unavailable"));
        assert!(switch_error.contains("NFS control switch unavailable"));
        assert!(!cancel.is_cancelled());

        let shutdown_error = shutdown(&mountpoint).await.unwrap_err();
        assert!(
            shutdown_error
                .to_string()
                .contains("missing live cache identity")
        );
        assert!(cancel.is_cancelled());
        handle.await.unwrap().unwrap();
    });
}

#[test]
#[cfg(feature = "nfs")]
fn nfs_context_uses_registry_backend_and_endpoint() {
    let _env_lock = crate::test_support::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(tmp.path());
    let mountpoint = tmp.path().join("view");
    let registry_path = crate::mounts_registry::registry_path().unwrap();
    crate::mounts_registry::add_entry(
        &registry_path,
        crate::mounts_registry::MountEntry {
            mountpoint: mountpoint.display().to_string(),
            source: "crab://bucket/repo".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            pid: 123,
            start_time: "2026-01-01T00:00:00Z".to_owned(),
            read_only: true,
            name: "repo".to_owned(),
            backend: Some("nfs".to_owned()),
            log_path: Some("/tmp/crab-nfs.log".to_owned()),
            control_endpoint: Some("unix:/tmp/crab-nfs.sock".to_owned()),
        },
    )
    .unwrap();

    let context = nfs_context(&mountpoint).unwrap().unwrap();

    assert_eq!(context.mountpoint, mountpoint);
    assert_eq!(context.endpoint, "unix:/tmp/crab-nfs.sock");
    assert_eq!(context.entry.pid, 123);
}

#[test]
#[cfg(feature = "nfs")]
fn nfs_context_ignores_non_nfs_registry_entries() {
    let _env_lock = crate::test_support::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(tmp.path());
    let mountpoint = tmp.path().join("view");
    let registry_path = crate::mounts_registry::registry_path().unwrap();
    crate::mounts_registry::add_entry(
        &registry_path,
        crate::mounts_registry::MountEntry {
            mountpoint: mountpoint.display().to_string(),
            source: "crab://bucket/repo".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            pid: 123,
            start_time: "2026-01-01T00:00:00Z".to_owned(),
            read_only: true,
            name: "repo".to_owned(),
            backend: Some("fuse".to_owned()),
            log_path: None,
            control_endpoint: Some("unix:/tmp/crab-nfs.sock".to_owned()),
        },
    )
    .unwrap();

    assert!(nfs_context(&mountpoint).unwrap().is_none());
}
#[cfg(unix)]
#[test]
fn unix_controls_reach_fuse_without_linking_the_server() {
    let _env_lock = crate::test_support::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(directory.path());
    let mountpoint = directory.path().join("view");
    std::fs::create_dir(&mountpoint).unwrap();
    // The CLI advertises canonical mount paths, including macOS temp aliases.
    let mountpoint = normalize_mountpoint(&mountpoint);
    let socket = directory.path().join(".crab/mounts/daemon.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        let path = mountpoint.display().to_string();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut lines = BufReader::new(stream).lines();
                while let Some(line) = lines.next_line().await.unwrap() {
                    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                    let response = match request["op"].as_str().unwrap() {
                        "list" => serde_json::json!({"ok":true,"mounts":[{"mountpoint":path,"remote":"crab://bucket/repo","ref":"main","read_only":false}]}),
                        "status" => serde_json::json!({"ok":true,"status":{"mountpoint":path,"remote":"crab://bucket/repo","ref":"main","read_only":false,"head_oid":"base","pid":123}}),
                        "refresh" | "switch_ref" => serde_json::json!({"ok":true,"head_oid":"next"}),
                        "commit_overlay" if request["message"] == "reject" => serde_json::json!({"ok":false,"error":"publication refused"}),
                        "commit_overlay" => serde_json::json!({"ok":true,"overlay_commit_result":{"transaction_id":"commit","commit_oid":"next","pushed":true,"overlay_cleaned":true,"diff":{"changes":[],"estimated_upload_bytes":0,"deletion_count":0}}}),
                        "export_overlay" if request["destination"].as_str().unwrap().ends_with("/reject") => serde_json::json!({"ok":false,"error":"export refused"}),
                        "reset_overlay" | "diff_overlay" | "export_overlay" => serde_json::json!({"ok":true,"overlay_diff":{"changes":[],"estimated_upload_bytes":0,"deletion_count":0}}),
                        "unmount" => serde_json::json!({"ok":true}),
                        other => panic!("unexpected request {other}"),
                    };
                    let mut encoded = serde_json::to_vec(&response).unwrap();
                    encoded.push(b'\n');
                    lines.get_mut().get_mut().write_all(&encoded).await.unwrap();
                }
            }
        });
        let live = status(&mountpoint).await.unwrap().unwrap();
        assert_eq!(live.backend, MountControlBackend::Fuse);
        assert!(list().await.unwrap().iter().any(|entry| entry.mountpoint == mountpoint.display().to_string() && entry.live));
        assert_eq!(refresh(&mountpoint).await.unwrap().unwrap().head_oid.as_deref(), Some("next"));
        assert_eq!(switch_ref(&mountpoint, "other").await.unwrap().unwrap().head_oid.as_deref(), Some("next"));
        assert!(commit(&mountpoint, "save", true).await.unwrap().unwrap().pushed);
        let rejected = commit(&mountpoint, "reject", true).await.unwrap_err();
        assert!(rejected.to_string().contains("publication refused"));
        assert!(inspect_overlay(&mountpoint).await.unwrap().is_some());
        assert!(export_overlay(&mountpoint, Path::new("review")).await.unwrap().is_some());
        assert!(export_overlay(&mountpoint, Path::new("reject")).await.unwrap_err().to_string().contains("export refused"));
        assert!(reset_overlay(&mountpoint).await.unwrap().is_some());
        assert!(matches!(shutdown(&mountpoint).await.unwrap().unwrap(), MountControlShutdown::Fuse { .. }));
        server.abort();
        let _ = server.await;
    });
}

#[cfg(feature = "nfs")]
#[test]
fn inspection_uses_live_nfs_mode_when_registry_is_stale() {
    let _env_lock = crate::test_support::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let _home = HomeGuard::set(tmp.path());
    let mountpoint = tmp.path().join("view");
    std::fs::create_dir(&mountpoint).unwrap();
    let mountpoint = normalize_mountpoint(&mountpoint);
    let registry_path = crate::mounts_registry::registry_path().unwrap();
    let endpoint = free_tcp_control_endpoint();
    let mut entry = add_nfs_mount_entry(&registry_path, &mountpoint, &endpoint);
    entry.read_only = false;
    crate::mounts_registry::add_entry(&registry_path, entry).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let cancel = CancellationToken::new();
        let server = crate::nfs_control::spawn_server(
            Some(endpoint.clone()),
            nfs_control_state(&mountpoint, true, cancel.clone()),
            cancel,
        )
        .unwrap();
        wait_for_nfs_status(&mountpoint).await;
        let error = inspect_overlay(&mountpoint).await.unwrap_err();
        let export_error = export_overlay(&mountpoint, &tmp.path().join("review"))
            .await
            .unwrap_err();
        crate::nfs_control::shutdown(&endpoint).await.unwrap();
        server.await.unwrap().unwrap();
        assert!(error.to_string().contains("read-only mount"));
        assert!(export_error.to_string().contains("read-only mount"));
    });
}

#[cfg(unix)]
#[test]
fn listing_rejects_incomplete_coordinator_discovery() {
    let _env_lock = crate::test_support::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (response, expected) in [
        (
            serde_json::json!({"ok":false,"error":"discovery refused"}),
            "discovery refused",
        ),
        (serde_json::json!({"ok":true}), "missing mounts"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let socket = crate::ipc_client::default_socket_path().unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let listener = tokio::net::UnixListener::bind(socket).unwrap();
            let peer = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut lines = BufReader::new(stream).lines();
                let request: serde_json::Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(request["op"], "list");
                let mut bytes = serde_json::to_vec(&response).unwrap();
                bytes.push(b'\n');
                lines.get_mut().get_mut().write_all(&bytes).await.unwrap();
            });
            let result = list().await;
            peer.await.unwrap();
            assert!(result.unwrap_err().to_string().contains(expected));
        });
    }
}
