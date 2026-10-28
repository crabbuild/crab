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
    let handle = bind_server(&endpoint)
        .await
        .unwrap()
        .spawn(state(root, cancel.clone()), cancel.clone());
    ping(&endpoint).await.unwrap();
    (endpoint, cancel, handle)
}

#[cfg(unix)]
#[tokio::test]
async fn prepared_control_listener_accepts_connections_before_serving() {
    let root = tempfile::tempdir().unwrap();
    let endpoint = format!("unix:{}", root.path().join("control.sock").display());
    let _listener = bind_server(&endpoint).await.unwrap();
    assert!(
        tokio::net::UnixStream::connect(unix_path_from_endpoint(&endpoint).unwrap())
            .await
            .is_ok()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_unstarted_control_releases_its_endpoint() {
    let root = tempfile::tempdir().unwrap();
    let endpoint = format!("unix:{}", root.path().join("control.sock").display());
    let listener = bind_server(&endpoint).await.unwrap();
    drop(listener);
    assert!(bind_server(&endpoint).await.is_ok());
}

#[tokio::test]
async fn prepared_tcp_control_listener_accepts_connections_before_serving() -> Result<()> {
    let listener = bind_server("tcp:127.0.0.1:0?token=owned-token")
        .await
        .unwrap();
    let addr = match &listener.listener {
        ControlListener::Tcp(socket, _) => socket.local_addr().unwrap(),
        #[cfg(unix)]
        ControlListener::Unix(..) => {
            return Err(std::io::Error::other("TCP fixture selected another transport").into());
        }
    };
    assert!(tokio::net::TcpStream::connect(addr).await.is_ok());
    Ok(())
}

#[tokio::test]
async fn dropping_unstarted_tcp_control_releases_its_port() -> Result<()> {
    let listener = bind_server("tcp:127.0.0.1:0?token=owned-token")
        .await
        .unwrap();
    let addr = match &listener.listener {
        ControlListener::Tcp(socket, _) => socket.local_addr().unwrap(),
        #[cfg(unix)]
        ControlListener::Unix(..) => {
            return Err(std::io::Error::other("TCP fixture selected another transport").into());
        }
    };
    drop(listener);
    assert!(tokio::net::TcpListener::bind(addr).await.is_ok());
    Ok(())
}

#[tokio::test]
async fn occupied_tcp_control_endpoint_is_rejected_before_serving() {
    let occupied = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let endpoint = format!("tcp:{}?token=owned-token", occupied.local_addr().unwrap());
    assert!(
        matches!(bind_server(&endpoint).await, Err(CrabError::Io(error)) if error.kind() == std::io::ErrorKind::AddrInUse)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn duplicate_control_bind_does_not_unlink_the_serving_owner() {
    let root = tempfile::tempdir().unwrap();
    let (endpoint, cancel, handle) = start_unix(root.path()).await;
    let rejected = bind_server(&endpoint).await;
    let reachable = ping(&endpoint).await.is_ok();
    cancel.cancel();
    handle.await.unwrap().unwrap();
    assert!(
        matches!(rejected, Err(CrabError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock)
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
    fs4::fs_std::FileExt::unlock(&lease.file).unwrap();
    let (endpoint, cancel, handle) = start_unix(root.path()).await;
    let unavailable = probe_endpoint_available(Some(&endpoint));
    cancel.cancel();
    handle.await.unwrap().unwrap();
    assert!(
        matches!(unavailable, Err(CrabError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert!(!path.exists());
    assert!(fs4::fs_std::FileExt::try_lock_exclusive(&lease.file).unwrap());
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
    let inherited = lease.file.try_clone().unwrap();
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
    let owner = bind_server(&endpoint)
        .await
        .unwrap()
        .spawn(state(root.path(), detach.clone()), transport.clone());
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

#[cfg(unix)]
struct CapturedControlLog(Arc<Mutex<Vec<u8>>>);

#[cfg(unix)]
impl std::io::Write for CapturedControlLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
#[tokio::test]
async fn listener_failure_reports_its_prepared_endpoint() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let writer = Arc::clone(&captured);
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(move || CapturedControlLog(Arc::clone(&writer)))
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("control");
    std::fs::create_dir(&directory).unwrap();
    let endpoint = format!("unix:{}", directory.join("owner.sock").display());
    let cancel = CancellationToken::new();
    let owner = bind_server(&endpoint)
        .await
        .unwrap()
        .spawn(state(root.path(), cancel.clone()), cancel.clone());
    ping(&endpoint).await.unwrap();

    // A real filesystem lookup failure exercises the server boundary after bind;
    // the moved socket and replacement remain owned by this private fixture.
    std::fs::rename(&directory, root.path().join("moved-control")).unwrap();
    std::fs::write(&directory, b"replacement").unwrap();
    cancel.cancel();
    let error = owner.await.unwrap().unwrap_err();
    assert!(
        matches!(error, CrabError::Io(source) if source.kind() == std::io::ErrorKind::NotADirectory)
    );
    let logs = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    let failure = logs
        .lines()
        .find(|line| line.contains("NFS control server stopped with error"))
        .unwrap();
    assert!(
        failure.contains(&endpoint),
        "failure diagnostics lost the serving endpoint: {failure}"
    );
}
