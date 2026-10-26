#![expect(clippy::unwrap_used, reason = "connection fixtures and assertions")]

use super::*;
use tokio::io::AsyncBufReadExt;

fn state(root: &Path, shutdown: CancellationToken) -> NfsControlState {
    NfsControlState {
        shutdown,
        mountpoint: root.join("view"),
        read_only: true,
        read_leases: ReadLeasePool::new(4, 4096),
        directory_pages: NfsDirectoryPageCache::new(4, 4096),
        write_journal: Arc::new(NfsWriteJournal::new()),
        protocol_stats: Arc::new(NfsProtocolStats::new()),
        engine: None,
        runtime: None,
        lifecycle: NfsMountLifecycleStatus::default(),
    }
}

#[cfg(unix)]
async fn start_unix(
    root: &Path,
) -> (
    String,
    CancellationToken,
    tokio::task::JoinHandle<Result<()>>,
) {
    let endpoint = format!("unix:{}", root.join("control.sock").display());
    let cancel = CancellationToken::new();
    let handle = spawn_server(
        Some(endpoint.clone()),
        state(root, cancel.clone()),
        cancel.clone(),
    )
    .unwrap();
    for _ in 0..40 {
        if ping(&endpoint).await.is_ok() {
            return (endpoint, cancel, handle);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    ping(&endpoint).await.unwrap();
    (endpoint, cancel, handle)
}

#[cfg(unix)]
#[tokio::test]
async fn duplicate_control_bind_does_not_unlink_the_serving_owner() {
    let root = tempfile::tempdir().unwrap();
    let (endpoint, cancel, handle) = start_unix(root.path()).await;
    let second_cancel = CancellationToken::new();
    let mut second = spawn_server(
        Some(endpoint.clone()),
        state(root.path(), second_cancel.clone()),
        second_cancel.clone(),
    )
    .unwrap();
    let rejected = tokio::time::timeout(Duration::from_secs(1), &mut second).await;
    let reachable = ping(&endpoint).await.is_ok();
    second_cancel.cancel();
    if rejected.is_err() {
        second.await.unwrap().unwrap();
    }
    cancel.cancel();
    handle.await.unwrap().unwrap();
    assert!(
        matches!(rejected, Ok(Ok(Err(CrabError::Io(error)))) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert!(reachable);
}

#[cfg(unix)]
#[tokio::test]
async fn control_cleanup_preserves_a_replacement_file() {
    for socket_replacement in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, cancel, handle) = start_unix(root.path()).await;
        let path = unix_path_from_endpoint(&endpoint).unwrap();
        std::fs::remove_file(&path).unwrap();
        let replacement = if socket_replacement {
            Some(std::os::unix::net::UnixListener::bind(&path).unwrap())
        } else {
            std::fs::write(&path, b"replacement source").unwrap();
            None
        };
        cancel.cancel();
        handle.await.unwrap().unwrap();
        if socket_replacement {
            assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());
        } else {
            assert_eq!(std::fs::read(path).unwrap(), b"replacement source");
        }
        drop(replacement);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn endpoint_lease_keeps_one_inode_across_socket_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("control.sock");
    let lease = lock_unix_endpoint(&path).unwrap();
    fs4::fs_std::FileExt::unlock(&lease.0).unwrap();
    let (endpoint, cancel, handle) = start_unix(root.path()).await;
    let unavailable = probe_endpoint_available(Some(&endpoint));
    cancel.cancel();
    handle.await.unwrap().unwrap();
    assert!(
        matches!(unavailable, Err(CrabError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert!(!path.exists());
    assert!(fs4::fs_std::FileExt::try_lock_exclusive(&lease.0).unwrap());
    // A stale descriptor must still exclude the next owner. Recreating the
    // lease file during cleanup would let the new owner bypass this lock.
    assert!(
        matches!(lock_unix_endpoint(&path), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    drop(lease);
    probe_endpoint_available(Some(&endpoint)).unwrap();
}

#[cfg(unix)]
#[test]
fn endpoint_lease_release_does_not_wait_for_inherited_descriptors() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("control.sock");
    let lease = lock_unix_endpoint(&path).unwrap();
    // dup and fork retain the same open-file description and flock ownership.
    let inherited = lease.0.try_clone().unwrap();
    assert!(matches!(
        lock_unix_endpoint(&path),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    drop(lease);
    let next = lock_unix_endpoint(&path).unwrap();
    drop(inherited);
    assert!(matches!(
        lock_unix_endpoint(&path),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    drop(next);
    let endpoint = format!("unix:{}", path.display());
    probe_endpoint_available(Some(&endpoint)).unwrap();
}

#[tokio::test]
async fn tcp_shutdown_joins_idle_authenticated_connections() {
    let root = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let state = state(root.path(), cancel.clone());
    let leases = Arc::clone(&state.read_leases);
    let handle = tokio::spawn(run_tcp_listener(
        listener,
        "owned-token".into(),
        state,
        cancel.clone(),
    ));
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"{\"token\":\"owned-token\",\"op\":\"ping\"}\n")
        .await
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(
        serde_json::from_str::<NfsControlResponse>(&line)
            .unwrap()
            .ok
    );
    cancel.cancel();
    handle.await.unwrap().unwrap();
    line.clear();
    assert_eq!(reader.read_line(&mut line).await.unwrap(), 0);
    assert_eq!(Arc::strong_count(&leases), 1);
}

#[tokio::test]
async fn stalled_control_reply_has_a_write_deadline() {
    let (mut writer, _reader) = tokio::io::duplex(1);
    let response = NfsControlResponse::err("x".repeat(1024));
    let result = tokio::time::timeout(
        REQUEST_TIMEOUT + Duration::from_secs(1),
        write_response(&mut writer, &response),
    )
    .await;
    assert!(matches!(result, Ok(Err(error)) if error.kind() == std::io::ErrorKind::TimedOut));
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_request_keeps_control_live_until_native_detachment() {
    let root = tempfile::tempdir().unwrap();
    let endpoint = format!("unix:{}", root.path().join("control.sock").display());
    let detach = CancellationToken::new();
    let transport = CancellationToken::new();
    let owner = spawn_server(
        Some(endpoint.clone()),
        state(root.path(), detach.clone()),
        transport.clone(),
    )
    .unwrap();
    for _ in 0..40 {
        if ping(&endpoint).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    shutdown(&endpoint).await.unwrap();
    assert!(detach.is_cancelled());
    assert!(!transport.is_cancelled());
    ping(&endpoint).await.unwrap();
    // Repeated shutdown must acknowledge the same draining owner.
    shutdown(&endpoint).await.unwrap();
    transport.cancel();
    owner.await.unwrap().unwrap();
}
