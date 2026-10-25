#![expect(clippy::unwrap_used, reason = "RPC lifecycle fixtures and assertions")]

use super::*;
use nfs3_server::vfs::{FileHandleU64, NextResult, NfsReadFileSystem, ReadDirPlusIterator};
use nfs3_types::nfs3::{fattr3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, specdata3};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;

#[derive(Default)]
struct ReadGate {
    started: Notify,
    release: Notify,
    finished: AtomicBool,
}

struct GatedFs {
    gate: Arc<ReadGate>,
    size: u32,
}

struct NoEntries;

impl ReadDirPlusIterator<FileHandleU64> for NoEntries {
    async fn next(&mut self) -> NextResult<nfs3_server::vfs::DirEntryPlus<FileHandleU64>> {
        NextResult::Eof
    }
}

impl NfsReadFileSystem for GatedFs {
    type Handle = FileHandleU64;

    fn root_dir(&self) -> Self::Handle {
        1.into()
    }

    async fn lookup(
        &self,
        dirid: &Self::Handle,
        filename: &filename3<'_>,
    ) -> std::result::Result<Self::Handle, nfsstat3> {
        if *dirid == 1 && filename.0.as_ref() == b"data" {
            Ok(2.into())
        } else {
            Err(nfsstat3::NFS3ERR_NOENT)
        }
    }

    async fn getattr(&self, id: &Self::Handle) -> std::result::Result<fattr3, nfsstat3> {
        Ok(fattr3 {
            type_: if *id == 1 {
                ftype3::NF3DIR
            } else {
                ftype3::NF3REG
            },
            mode: 0o755,
            nlink: 1,
            uid: 0,
            gid: 0,
            size: u64::from(self.size),
            used: u64::from(self.size),
            rdev: specdata3::default(),
            fsid: 0,
            fileid: id.as_u64(),
            atime: nfstime3::default(),
            mtime: nfstime3::default(),
            ctime: nfstime3::default(),
        })
    }

    async fn read(
        &self,
        _id: &Self::Handle,
        offset: u64,
        count: u32,
    ) -> std::result::Result<(Vec<u8>, bool), nfsstat3> {
        self.gate.started.notify_one();
        self.gate.release.notified().await;
        let len = u64::from(count).min(u64::from(self.size).saturating_sub(offset));
        self.gate.finished.store(true, Ordering::Release);
        Ok((
            vec![0x5a; usize::try_from(len).unwrap()],
            offset + len >= u64::from(self.size),
        ))
    }

    async fn readdirplus(
        &self,
        _dirid: &Self::Handle,
        _cookie: u64,
    ) -> std::result::Result<impl ReadDirPlusIterator<Self::Handle>, nfsstat3> {
        Ok(NoEntries)
    }

    async fn readlink(&self, _id: &Self::Handle) -> std::result::Result<nfspath3<'_>, nfsstat3> {
        Err(nfsstat3::NFS3ERR_INVAL)
    }
}

async fn start(
    gate: &Arc<ReadGate>,
    size: u32,
    cancel: CancellationToken,
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let mut listener = NFSTcpListener::bind_ro(
        "127.0.0.1:0",
        GatedFs {
            gate: Arc::clone(gate),
            size,
        },
    )
    .await
    .unwrap();
    listener.with_export_name("crab");
    let address = (listener.get_listen_ip(), listener.get_listen_port()).into();
    let owner = tokio::spawn(async move {
        listener
            .handle_until_cancelled(cancel.cancelled_owned())
            .await
    });
    (address, owner)
}

fn opaque(bytes: &[u8]) -> Vec<u8> {
    let mut result = u32::try_from(bytes.len()).unwrap().to_be_bytes().to_vec();
    result.extend_from_slice(bytes);
    result.resize(4 + bytes.len().next_multiple_of(4), 0);
    result
}

async fn send(stream: &mut TcpStream, xid: u32, program: u32, procedure: u32, args: &[u8]) {
    let mut body = [xid, 0, 2, program, 3, procedure, 0, 0, 0, 0]
        .into_iter()
        .flat_map(u32::to_be_bytes)
        .collect::<Vec<_>>();
    body.extend_from_slice(args);
    stream
        .write_u32(0x8000_0000 | u32::try_from(body.len()).unwrap())
        .await
        .unwrap();
    stream.write_all(&body).await.unwrap();
}

async fn reply(stream: &mut TcpStream, xid: u32) -> Vec<u8> {
    let header = stream.read_u32().await.unwrap();
    assert_ne!(header & 0x8000_0000, 0);
    let mut bytes = vec![0; usize::try_from(header & 0x7fff_ffff).unwrap()];
    stream.read_exact(&mut bytes).await.unwrap();
    let expected = [xid, 1, 0, 0, 0, 0]
        .into_iter()
        .flat_map(u32::to_be_bytes)
        .collect::<Vec<_>>();
    assert_eq!(&bytes[..24], expected);
    bytes
}

async fn file_handle(stream: &mut TcpStream) -> Vec<u8> {
    send(stream, 1, nfs3_types::mount::PROGRAM, 1, &opaque(b"/crab")).await;
    let mounted = reply(stream, 1).await;
    let root = decoded_handle(&mounted);
    let mut lookup = opaque(&root);
    lookup.extend(opaque(b"data"));
    send(stream, 2, nfs3_types::nfs3::PROGRAM, 3, &lookup).await;
    decoded_handle(&reply(stream, 2).await)
}

fn decoded_handle(bytes: &[u8]) -> Vec<u8> {
    assert_eq!(&bytes[24..28], &[0; 4]);
    let len = usize::try_from(u32::from_be_bytes(bytes[28..32].try_into().unwrap())).unwrap();
    bytes[32..32 + len].to_vec()
}

async fn request_read(stream: &mut TcpStream, count: u32) {
    let mut args = opaque(&file_handle(stream).await);
    args.extend(0_u64.to_be_bytes());
    args.extend(count.to_be_bytes());
    send(stream, 3, nfs3_types::nfs3::PROGRAM, 6, &args).await;
}

#[tokio::test]
async fn graceful_data_shutdown_closes_idle_and_partial_rpc_connections() {
    for partial in [false, true] {
        let gate = Arc::new(ReadGate::default());
        let cancel = CancellationToken::new();
        let (address, owner) = start(&gate, 1, cancel.clone()).await;
        let mut stream = TcpStream::connect(address).await.unwrap();
        send(&mut stream, 1, nfs3_types::nfs3::PROGRAM, 0, &[]).await;
        reply(&mut stream, 1).await;
        if partial {
            stream.write_all(&[0x80, 0, 0, 40, 0, 0]).await.unwrap();
        }
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), owner)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // Closing with an unread partial record may reset TCP (RFC 9293 §3.6.1).
        let closed = stream.read(&mut [0]).await;
        assert!(
            matches!(closed, Ok(0))
                || partial
                    && matches!(closed, Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset)
        );
        assert_eq!(Arc::strong_count(&gate), 1);
    }
}

#[tokio::test]
async fn graceful_data_shutdown_drains_admitted_reads_after_client_disconnect() {
    for disconnect in [false, true] {
        let gate = Arc::new(ReadGate::default());
        let cancel = CancellationToken::new();
        let (address, mut owner) = start(&gate, 1, cancel.clone()).await;
        let mut stream = TcpStream::connect(address).await.unwrap();
        request_read(&mut stream, 1).await;
        tokio::time::timeout(Duration::from_secs(2), gate.started.notified())
            .await
            .unwrap();
        let stream = (!disconnect).then_some(stream);
        cancel.cancel();
        let early = tokio::time::timeout(Duration::from_millis(30), &mut owner).await;
        let waited = early.is_err();
        gate.release.notify_one();
        if waited {
            tokio::time::timeout(Duration::from_secs(2), owner)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        } else {
            early.unwrap().unwrap().unwrap();
        }
        assert!(waited, "data owner detached an admitted read");
        assert!(gate.finished.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(&gate), 1);
        drop(stream);
    }
}

#[tokio::test]
async fn graceful_data_shutdown_interrupts_a_blocked_reply() {
    let gate = Arc::new(ReadGate::default());
    let cancel = CancellationToken::new();
    let (address, owner) = start(&gate, 8 * 1024 * 1024, cancel.clone()).await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    request_read(&mut stream, 8 * 1024 * 1024).await;
    tokio::time::timeout(Duration::from_secs(2), gate.started.notified())
        .await
        .unwrap();
    gate.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), stream.read_u32())
        .await
        .unwrap()
        .unwrap();
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), owner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(Arc::strong_count(&gate), 1);
}

#[tokio::test]
async fn graceful_data_shutdown_before_first_poll_releases_the_adapter() {
    let gate = Arc::new(ReadGate::default());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (_, owner) = start(&gate, 1, cancel).await;
    tokio::time::timeout(Duration::from_secs(2), owner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(Arc::strong_count(&gate), 1);
}

#[tokio::test]
async fn mounted_owner_releases_engine_with_an_idle_rpc_client() {
    let root = tempfile::tempdir().unwrap();
    let (server_cancel, placeholder) = tests::idle_server();
    let (_sender, receiver) = mpsc::channel(1);
    let mut session = tests::synthetic_session(root.path(), placeholder, server_cancel, receiver);
    session.server_cancel.cancel();
    (&mut session.server_handle).await.unwrap().unwrap();
    let resolver = Arc::clone(
        &session
            .runtime
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .output
            .resolver,
    );
    let adapter = CrabNfsFs::new(
        resolver,
        Arc::clone(&session.engine),
        root.path().to_str().unwrap(),
        true,
        None,
    );
    let listener = NFSTcpListener::bind("127.0.0.1:0", adapter).await.unwrap();
    let address = (listener.get_listen_ip(), listener.get_listen_port());
    session.server_cancel = CancellationToken::new();
    let server_stop = session.server_cancel.clone();
    session.server_handle = tokio::spawn(async move {
        listener
            .handle_until_cancelled(server_stop.cancelled_owned())
            .await
            .map_err(CrabError::Io)
    });
    let engine = Arc::downgrade(&session.engine);
    let mut stream = TcpStream::connect(address).await.unwrap();
    send(&mut stream, 1, nfs3_types::nfs3::PROGRAM, 0, &[]).await;
    reply(&mut stream, 1).await;
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    tokio::time::timeout(
        Duration::from_secs(2),
        run_until_cancelled(session, shutdown),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(engine.strong_count(), 0);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), stream.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn owner_syncs_unstable_writes_completed_during_rpc_drain() {
    use nfs3_server::vfs::NfsFileSystem;
    use nfs3_types::nfs3::stable_how;

    let root = tempfile::tempdir().unwrap();
    let (server_cancel, placeholder) = tests::idle_server();
    let (_sender, receiver) = mpsc::channel(1);
    let mut session = tests::synthetic_session(root.path(), placeholder, server_cancel, receiver);
    session.server_cancel.cancel();
    (&mut session.server_handle).await.unwrap().unwrap();
    let (snapshot, hydration) = {
        let runtime = session.runtime.as_ref().unwrap().lock().unwrap();
        (
            Arc::clone(&runtime.output.snapshot),
            Arc::clone(&runtime.output.hydration),
        )
    };
    snapshot
        .publish_generation("abc123", "refs/heads/main", &[])
        .unwrap();
    let generation = snapshot.current_generation().unwrap().unwrap();
    let overlay = Arc::new(
        crate::overlay::OverlayStore::open(
            &root.path().join("overlay.db"),
            &root.path().join("upper"),
        )
        .unwrap(),
    );
    let lookup: Arc<dyn crate::resolver::OverlayLookup> = overlay.clone();
    let writer: Arc<dyn crate::engine::OverlayWriter> = overlay;
    let resolver = Arc::new(
        FuseResolver::new(
            Arc::clone(&snapshot),
            Some(lookup),
            generation,
            0,
            Some(Arc::clone(&hydration)),
            None,
        )
        .unwrap(),
    );
    let engine = Arc::new(VfsEngine::new(
        Arc::clone(&resolver),
        Some(writer),
        hydration,
        None,
        Some(snapshot),
    ));
    engine.create("late", 0o644).await.unwrap();
    let fs = CrabNfsFs::new(
        resolver,
        Arc::clone(&engine),
        root.path().to_str().unwrap(),
        false,
        None,
    );
    let id = fs
        .lookup(&fs.root_dir(), &filename3::from(b"late".as_slice()))
        .await
        .unwrap();
    let journal = fs.write_journal();
    session.engine = Arc::clone(&engine);
    session.write_journal = Arc::clone(&journal);
    session.server_cancel = CancellationToken::new();
    let stopping = session.server_cancel.clone();
    session.server_handle = tokio::spawn(async move {
        // An admitted callback can finish after transport shutdown starts.
        stopping.cancelled().await;
        let (_, stability) = fs
            .write(&id, 0, b"late unstable bytes", stable_how::UNSTABLE)
            .await
            .unwrap();
        assert_eq!(stability, stable_how::UNSTABLE);
        Ok(())
    });
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    run_until_cancelled(session, shutdown).await.unwrap();
    assert_eq!(journal.snapshot().pending_paths, 0);
    assert_eq!(journal.snapshot().sync_successes, 1);
    assert_eq!(
        &engine.read("late", 0, 64).await.unwrap()[..],
        b"late unstable bytes"
    );
}
