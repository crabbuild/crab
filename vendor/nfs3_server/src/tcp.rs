use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::{debug, info};

use crate::context::RPCContext;
use crate::rpcwire::{SocketMessageHandler, write_fragment};
use crate::transaction_tracker::TransactionTracker;
use crate::units::KIBIBYTE;
use crate::vfs::adapters::ReadOnlyAdapter;
use crate::vfs::{NfsFileSystem, NfsReadFileSystem};

/// A NFS Tcp Connection Handler
pub struct NFSTcpListener<T: NfsFileSystem + 'static> {
    listener: TcpListener,
    port: u16,
    arcfs: Arc<T>,
    mount_signal: Option<mpsc::Sender<bool>>,
    export_name: Arc<String>,
    transaction_tracker: Arc<TransactionTracker>,
    file_handle_converter: crate::vfs::handle::FileHandleConverter,
}

#[must_use]
pub fn generate_host_ip(hostnum: u16) -> String {
    format!(
        "127.88.{}.{}",
        ((hostnum >> 8) & 0xFF) as u8,
        (hostnum & 0xFF) as u8
    )
}

/// processes an established socket
pub(crate) async fn process_socket<IO, T>(
    mut socket: IO,
    context: RPCContext<T>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), anyhow::Error>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
    T: NfsFileSystem + 'static,
{
    let (mut handler, mut input, mut replies) = SocketMessageHandler::new(context);
    let (stop_parser, mut parser_stop) = watch::channel(false);
    let mut parser = tokio::spawn(async move {
        let result = loop {
            tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut parser_stop) => break Ok(()),
                result = handler.read() => {
                    if let Err(error) = result { break Err(error); }
                }
            }
        };
        let drained = handler.drain().await;
        result.and(drained)
    });
    let mut parser_joined = false;
    let mut buf = vec![0u8; 128 * KIBIBYTE as usize].into_boxed_slice();
    let result = loop {
        // Fair selection prevents a continuously readable peer from starving replies.
        tokio::select! {
            _ = wait_for_shutdown(&mut shutdown) => break Ok(()),
            result = &mut parser => {
                parser_joined = true;
                break result.map_err(anyhow::Error::from).and_then(std::convert::identity);
            }
            result = socket.read(&mut buf) => {
                match result {
                    Ok(0) => break Ok(()),
                    Ok(n) => {
                        let sent = tokio::select! {
                            biased;
                            _ = wait_for_shutdown(&mut shutdown) => break Ok(()),
                            result = input.write_all(&buf[..n]) => result,
                        };
                        if let Err(error) = sent { break Err(error.into()); }
                    }
                    Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => break Err(error.into()),
                }
            }
            reply = replies.recv() => {
                match reply {
                    Some(Err(error)) => break Err(error),
                    Some(Ok(message)) => {
                        let written = tokio::select! {
                            biased;
                            _ = wait_for_shutdown(&mut shutdown) => break Ok(()),
                            result = write_fragment(&mut socket, message) => result,
                        };
                        if let Err(error) = written { break Err(error); }
                    }
                    None => break Err(anyhow::anyhow!("Unexpected socket context termination")),
                }
            }
        }
    };
    // Transport closure must not detach admitted filesystem operations.
    let _ = stop_parser.send(true);
    let drained = if parser_joined {
        Ok(())
    } else {
        parser
            .await
            .map_err(anyhow::Error::from)
            .and_then(std::convert::identity)
    };
    result.and(drained)
}

async fn wait_for_shutdown(stopping: &mut watch::Receiver<bool>) {
    // Discard the watch borrow before dispatch; nested I/O waits share this
    // receiver, and a dropped sender must also stop parser admission.
    let _ = stopping.wait_for(|stopped| *stopped).await;
}

pub trait NFSTcp: Send + Sync {
    /// Gets the true listening port. Useful if the bound port number is 0
    fn get_listen_port(&self) -> u16;

    /// Gets the true listening IP. Useful on windows when the IP may be random
    fn get_listen_ip(&self) -> IpAddr;

    /// Sets a mount listener. A "true" signal will be sent on a mount
    /// and a "false" will be sent on an unmount
    fn set_mount_listener(&mut self, signal: mpsc::Sender<bool>);

    /// Loops forever and never returns handling all incoming connections.
    fn handle_forever(&self) -> impl Future<Output = io::Result<()>> + Send;
}

impl<RO> NFSTcpListener<ReadOnlyAdapter<RO>>
where
    RO: NfsReadFileSystem + 'static,
{
    /// Create a new `NFSTcpListener` with a read-only file system.
    ///
    /// It binds to a ipstr of the form [ip address]:port. For instance,
    /// "127.0.0.1:12000". `fs` is an instance of an implementation
    /// of [`NfsReadFileSystem`].
    pub async fn bind_ro(ipstr: &str, fs: RO) -> io::Result<Self> {
        Self::bind(ipstr, ReadOnlyAdapter::new(fs)).await
    }
}

impl<T: NfsFileSystem + 'static> NFSTcpListener<T> {
    /// Create a new `NFSTcpListener`.
    ///
    /// It binds to a ipstr of the form [ip address]:port. For instance,
    /// "127.0.0.1:12000". `fs` is an instance of an implementation
    /// of [`NfsFileSystem`].
    pub async fn bind(ipstr: &str, fs: T) -> io::Result<Self> {
        Self::bind_inner(ipstr, fs, None).await
    }

    /// Create a new `NFSTcpListener` with a specific generation number.
    ///
    /// The generation number is embedded in file handles and used to detect
    /// stale handles from previous server instances. When multiple NFS server
    /// instances share the same generation number, file handles remain valid
    /// across all instances (e.g. behind a load balancer).
    pub async fn bind_with_generation(
        ipstr: &str,
        fs: T,
        generation_number: u64,
    ) -> io::Result<Self> {
        Self::bind_inner(ipstr, fs, Some(generation_number)).await
    }

    async fn bind_inner(ipstr: &str, fs: T, generation_number: Option<u64>) -> io::Result<Self> {
        let (ip, port) = ipstr.split_once(':').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "IP Address must be of form ip:port",
            )
        })?;
        let port = port.parse::<u16>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "Port not in range 0..=65535",
            )
        })?;

        let arcfs: Arc<T> = Arc::new(fs);

        if ip == "auto" {
            let mut num_tries_left = 32;

            for try_ip in 1u16.. {
                let ip = generate_host_ip(try_ip);

                let result = Self::bind_internal(&ip, port, arcfs.clone(), generation_number).await;

                match result {
                    Err(_) => {
                        if num_tries_left == 0 {
                            return result;
                        }
                        num_tries_left -= 1;
                    }
                    Ok(_) => {
                        return result;
                    }
                }
            }
            unreachable!(); // Does not detect automatically that loop above never terminates.
        } else {
            // Otherwise, try this.
            Self::bind_internal(ip, port, arcfs, generation_number).await
        }
    }

    async fn bind_internal(
        ip: &str,
        port: u16,
        arcfs: Arc<T>,
        generation_number: Option<u64>,
    ) -> io::Result<Self> {
        let ipstr = format!("{ip}:{port}");
        let listener = TcpListener::bind(&ipstr).await?;
        info!("Listening on {:?}", &ipstr);

        let port = match listener.local_addr().expect("failed to get local address") {
            SocketAddr::V4(s) => s.port(),
            SocketAddr::V6(s) => s.port(),
        };

        let file_handle_converter = generation_number.map_or_else(
            crate::vfs::handle::FileHandleConverter::new,
            crate::vfs::handle::FileHandleConverter::with_generation_number,
        );

        Ok(Self {
            listener,
            port,
            arcfs,
            mount_signal: None,
            export_name: Arc::from("/".to_string()),
            transaction_tracker: Self::new_transaction_tracker(),
            file_handle_converter,
        })
    }

    fn new_transaction_tracker() -> Arc<TransactionTracker> {
        const TRANSACTION_LIFETIME: Duration = Duration::from_secs(60);
        const MAX_ACTIVE_TRANSACTIONS: u16 = 256;
        const TRANSACTION_TRIM_THRESHOLD: usize = 2048;

        Arc::new(TransactionTracker::new(
            TRANSACTION_LIFETIME,
            MAX_ACTIVE_TRANSACTIONS,
            TRANSACTION_TRIM_THRESHOLD,
        ))
    }

    /// Sets an optional NFS export name.
    ///
    /// - `export_name`: The desired export name without slashes.
    ///
    /// Example: Name `foo` results in the export path `/foo`.
    /// Default path is `/` if not set.
    pub fn with_export_name<S: AsRef<str>>(&mut self, export_name: S) {
        self.export_name = Arc::new(format!(
            "/{}",
            export_name
                .as_ref()
                .trim_end_matches('/')
                .trim_start_matches('/')
        ));
    }
}

impl<T: NfsFileSystem + 'static> NFSTcp for NFSTcpListener<T> {
    /// Gets the true listening port. Useful if the bound port number is 0
    fn get_listen_port(&self) -> u16 {
        let addr = self
            .listener
            .local_addr()
            .expect("failed to get local address");
        addr.port()
    }

    fn get_listen_ip(&self) -> IpAddr {
        let addr = self
            .listener
            .local_addr()
            .expect("failed to get local address");
        addr.ip()
    }

    /// Sets a mount listener. A "true" signal will be sent on a mount
    /// and a "false" will be sent on an unmount
    fn set_mount_listener(&mut self, signal: mpsc::Sender<bool>) {
        self.mount_signal = Some(signal);
    }

    /// Loops forever and never returns handling all incoming connections.
    async fn handle_forever(&self) -> io::Result<()> {
        self.handle_until_cancelled(std::future::pending()).await
    }
}

impl<T: NfsFileSystem + 'static> NFSTcpListener<T> {
    /// Serve until shutdown, then stop transport admission and join accepted RPC work.
    ///
    /// Await this method to completion; dropping its future cannot await task drain.
    pub async fn handle_until_cancelled(
        &self,
        shutdown: impl Future<Output = ()> + Send,
    ) -> io::Result<()> {
        tokio::pin!(shutdown);
        let (stop, stopping) = watch::channel(false);
        let mut connections = JoinSet::new();
        let mut cleanup = tokio::time::interval(Duration::from_secs(10));
        let mut result = loop {
            tokio::select! {
                biased;
                () = &mut shutdown => break Ok(()),
                Some(joined) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(error) = joined { break Err(io::Error::other(error)); }
                }
                _ = cleanup.tick() => self.transaction_tracker.cleanup(std::time::Instant::now()),
                accepted = self.listener.accept() => {
                    let (socket, peer) = match accepted {
                        Ok(accepted) => accepted,
                        Err(error) => break Err(error),
                    };
                    let context = RPCContext {
                        local_port: self.port,
                        client_addr: peer.to_string(),
                        auth: nfs3_types::rpc::auth_unix::default(),
                        vfs: self.arcfs.clone(),
                        mount_signal: self.mount_signal.clone(),
                        export_name: self.export_name.clone(),
                        transaction_tracker: self.transaction_tracker.clone(),
                        file_handle_converter: self.file_handle_converter,
                    };
                    let shutdown = stopping.clone();
                    connections.spawn(async move {
                        let _ = socket.set_nodelay(true);
                        if let Err(error) = process_socket(socket, context, shutdown).await {
                            debug!(%error, "NFS connection ended");
                        }
                    });
                }
            }
        };
        let _ = stop.send(true);
        while let Some(joined) = connections.join_next().await {
            if let Err(error) = joined {
                if result.is_ok() {
                    result = Err(io::Error::other(error));
                }
            }
        }
        result
    }
}
