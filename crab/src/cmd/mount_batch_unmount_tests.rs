#![expect(clippy::unwrap_used, reason = "control peer setup and assertions")]
#![expect(clippy::panic, reason = "test peer rejects unexpected operations")]

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Clone, Copy)]
enum PeerBehavior {
    Stable,
    RejectDiscovery,
    MissingMounts,
    Disappear,
    RejectUnmount,
    ConflictingOwner,
    RegisteredAlive,
    RegisteredChanged,
}

async fn qualify_batch_unmount(behavior: PeerBehavior, expected_error: Option<&str>) {
    let _env_lock = crate::test::git_repo::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let _home = set_test_home(tmp.path());
    let mountpoint = tmp.path().join("view");
    std::fs::create_dir(&mountpoint).unwrap();
    let mountpoint = normalize_unmount_path(&mountpoint).display().to_string();
    let socket = crate::vfs::ipc_client::default_socket_path().unwrap();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let registry_path = crate::vfs::mounts_registry::registry_path().unwrap();
    let mut child = if matches!(
        behavior,
        PeerBehavior::RegisteredAlive | PeerBehavior::RegisteredChanged
    ) {
        Some(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        )
    } else {
        None
    };
    let registered = if matches!(
        behavior,
        PeerBehavior::ConflictingOwner
            | PeerBehavior::RegisteredAlive
            | PeerBehavior::RegisteredChanged
    ) {
        let entry = crate::vfs::mounts_registry::MountEntry {
            mountpoint: mountpoint.clone(),
            source: "crab://bucket/registered".into(),
            git_ref: "refs/heads/main".into(),
            pid: if matches!(behavior, PeerBehavior::RegisteredAlive) {
                child.as_ref().unwrap().id()
            } else {
                4_000_000_000
            },
            start_time: "2026-10-25T00:00:00Z".into(),
            read_only: false,
            name: "registered".into(),
            backend: Some(
                if matches!(behavior, PeerBehavior::ConflictingOwner) {
                    "nfs"
                } else {
                    "fuse"
                }
                .into(),
            ),
            log_path: None,
            control_endpoint: None,
        };
        crate::vfs::mounts_registry::add_entry(&registry_path, entry.clone()).unwrap();
        Some(entry)
    } else {
        None
    };
    let mut expected_entries = registered.clone().into_iter().collect::<Vec<_>>();
    if matches!(behavior, PeerBehavior::RegisteredChanged) {
        expected_entries[0].pid = child.as_ref().unwrap().id();
        expected_entries[0].start_time = "2026-10-25T00:00:01Z".into();
    }
    let replacement = expected_entries.first().cloned();
    let peer_registry = registry_path.clone();
    let unmounted = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&unmounted);
    let server = tokio::spawn(async move {
        let mut lists = 0;
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let response = match request["op"].as_str().unwrap() {
                    "list" => {
                        lists += 1;
                        if matches!(behavior, PeerBehavior::RegisteredChanged) && lists == 1 {
                            crate::vfs::mounts_registry::add_entry(
                                &peer_registry,
                                replacement.clone().unwrap(),
                            )
                            .unwrap();
                        }
                        match behavior {
                            PeerBehavior::RejectDiscovery => {
                                serde_json::json!({"ok":false,"error":"discovery refused"})
                            }
                            PeerBehavior::MissingMounts => serde_json::json!({"ok":true}),
                            PeerBehavior::RegisteredAlive | PeerBehavior::RegisteredChanged => {
                                serde_json::json!({"ok":true,"mounts":[]})
                            }
                            PeerBehavior::Disappear if lists > 1 => {
                                serde_json::json!({"ok":true,"mounts":[]})
                            }
                            _ => {
                                serde_json::json!({"ok":true,"mounts":[{"mountpoint":mountpoint,"remote":"crab://bucket/repo","ref":"main","read_only":false}]})
                            }
                        }
                    }
                    "status" => {
                        serde_json::json!({"ok":true,"status":{"mountpoint":mountpoint,"remote":"crab://bucket/repo","ref":"main","read_only":false,"head_oid":"base","pid":123}})
                    }
                    "unmount" if matches!(behavior, PeerBehavior::RejectUnmount) => {
                        serde_json::json!({"ok":false,"error":"shutdown refused"})
                    }
                    "unmount" => {
                        observed.store(true, Ordering::SeqCst);
                        serde_json::json!({"ok":true})
                    }
                    other => panic!("unexpected operation {other}"),
                };
                let mut bytes = serde_json::to_vec(&response).unwrap();
                bytes.push(b'\n');
                lines.get_mut().get_mut().write_all(&bytes).await.unwrap();
            }
        }
    });
    let result = run_unmount_all_live_or_persisted().await;
    server.abort();
    let _ = server.await;
    let child_alive = child
        .as_mut()
        .map(|child| child.try_wait().unwrap().is_none());
    if let Some(mut child) = child {
        let _ = child.kill();
        child.wait().unwrap();
    }
    assert!(
        child_alive.unwrap_or(true),
        "batch cleanup terminated an unowned process"
    );
    assert_eq!(
        crate::vfs::mounts_registry::list_entries(&registry_path).unwrap(),
        expected_entries
    );
    match expected_error {
        Some(message) => {
            assert!(result.unwrap_err().to_string().contains(message));
            assert!(!unmounted.load(Ordering::SeqCst));
        }
        None => {
            result.unwrap();
            assert!(
                unmounted.load(Ordering::SeqCst),
                "batch unmount omitted the live coordinator owner"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_unmount_discovers_coordinator_only_mounts() {
    qualify_batch_unmount(PeerBehavior::Stable, None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_unmount_reports_rejected_discovery() {
    qualify_batch_unmount(PeerBehavior::RejectDiscovery, Some("discovery refused")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_unmount_reports_malformed_discovery() {
    qualify_batch_unmount(PeerBehavior::MissingMounts, Some("missing mounts")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_unmount_refuses_stale_cleanup_after_live_owner_loss() {
    qualify_batch_unmount(PeerBehavior::Disappear, Some("control disappeared")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_unmount_preserves_rejected_unmount_errors() {
    qualify_batch_unmount(PeerBehavior::RejectUnmount, Some("shutdown refused")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_unmount_rejects_conflicting_backend_owners() {
    qualify_batch_unmount(
        PeerBehavior::ConflictingOwner,
        Some("conflicting mount owners"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_unmount_preserves_live_registered_process_without_control() {
    qualify_batch_unmount(
        PeerBehavior::RegisteredAlive,
        Some("remains alive without control"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_unmount_preserves_registry_replacement_during_discovery() {
    qualify_batch_unmount(PeerBehavior::RegisteredChanged, Some("owner changed")).await;
}
