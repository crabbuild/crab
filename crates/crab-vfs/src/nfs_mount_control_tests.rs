#![expect(clippy::unwrap_used, reason = "lifecycle fixtures and assertions")]

use super::*;
use std::future::Future;

#[tokio::test]
async fn owner_cancellation_joins_control_cleanup_before_returning() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("control.sock");
    let endpoint = format!("unix:{}", socket.display());
    let (server_cancel, server) = tests::idle_server();
    let (_sender, receiver) = mpsc::channel(1);
    let mut session = tests::synthetic_session(root.path(), server, server_cancel, receiver);
    session.control_endpoint = Some(endpoint.clone());
    session.control_listener = Some(nfs_control::bind_server(&endpoint).await.unwrap());
    let cancel = CancellationToken::new();
    let shutdown = run_until_cancelled(session, cancel.clone());
    tokio::pin!(shutdown);
    std::future::poll_fn(|context| {
        assert!(shutdown.as_mut().poll(context).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    nfs_control::ping(&endpoint).await.unwrap();
    cancel.cancel();
    shutdown.await.unwrap();
    assert!(!socket.exists());
}

#[tokio::test]
async fn control_bind_failure_preserves_existing_source_before_mount() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("control.sock");
    std::fs::write(&socket, b"existing source").unwrap();
    let endpoint = format!("unix:{}", socket.display());
    let result = nfs_control::bind_server(&endpoint).await;
    assert!(
        matches!(result, Err(CrabError::Io(error)) if error.kind() == std::io::ErrorKind::AddrInUse)
    );
    assert_eq!(std::fs::read(socket).unwrap(), b"existing source");
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_shutdown_waits_for_a_contended_control_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let endpoint = format!("unix:{}", root.path().join("control.sock").display());
    let (server_cancel, server) = tests::idle_server();
    let (_sender, receiver) = mpsc::channel(1);
    let mut session = tests::synthetic_session(root.path(), server, server_cancel, receiver);
    session.control_endpoint = Some(endpoint.clone());
    session.control_listener = Some(nfs_control::bind_server(&endpoint).await.unwrap());
    let runtime = Arc::clone(session.runtime.as_ref().unwrap());
    let cancel = CancellationToken::new();
    let owner = run_until_cancelled(session, cancel.clone());
    tokio::pin!(owner);
    std::future::poll_fn(|context| {
        assert!(owner.as_mut().poll(context).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    nfs_control::ping(&endpoint).await.unwrap();
    // Session, listener state and observer remain after the ping client closes.
    tokio::time::timeout(Duration::from_secs(2), async {
        while Arc::strong_count(&runtime) != 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let locked_runtime = Arc::clone(&runtime);
    let (ready, entered) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = locked_runtime.lock().unwrap();
        ready.send(()).unwrap();
        released.recv().unwrap();
    });
    entered.await.unwrap();
    let requested = endpoint.clone();
    let request = tokio::spawn(async move { nfs_control::refresh(&requested).await });
    // Holder, accepted connection and admitted blocking snapshot add three refs.
    let admitted = tokio::time::timeout(Duration::from_secs(2), async {
        while Arc::strong_count(&runtime) < 6 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    cancel.cancel();
    let early = tokio::time::timeout(Duration::from_millis(30), &mut owner).await;
    let waited = early.is_err();
    let endpoint_retained = matches!(
        nfs_control::probe_endpoint_available(Some(&endpoint)),
        Err(CrabError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock
    );
    release.send(()).unwrap();
    holder.join().unwrap();
    if waited {
        owner.await.unwrap();
    } else {
        early.unwrap().unwrap();
    }
    let _ = request.await.unwrap();
    assert!(
        admitted.is_ok(),
        "the blocking control snapshot was not admitted"
    );
    assert!(waited, "teardown detached an admitted control snapshot");
    assert!(
        endpoint_retained,
        "the draining control owner lost its endpoint lease"
    );
    assert_eq!(Arc::strong_count(&runtime), 1);
    nfs_control::probe_endpoint_available(Some(&endpoint)).unwrap();
}

#[tokio::test]
async fn teardown_releases_mount_notifications_before_rpc_drain() {
    let root = tempfile::tempdir().unwrap();
    let (sender, receiver) = mpsc::channel(1);
    sender.send(true).await.unwrap();
    let server_cancel = CancellationToken::new();
    let stopping = server_cancel.clone();
    let server = tokio::spawn(async move {
        stopping.cancelled().await;
        // A second notification cannot fit until the lifecycle receiver closes.
        assert!(sender.send(false).await.is_err());
        Ok(())
    });
    let session = tests::synthetic_session(root.path(), server, server_cancel, receiver);
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    tokio::time::timeout(
        Duration::from_secs(2),
        run_until_cancelled(session, shutdown),
    )
    .await
    .unwrap()
    .unwrap();
}
