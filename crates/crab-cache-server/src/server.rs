//! HTTP server bootstrap: bind, optional TLS, graceful shutdown.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use axum::middleware::AddExtension;
use axum::{Extension, Router};
use axum_server::accept::Accept;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use futures_util::future::BoxFuture;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::Instant;
use tokio_rustls::server::TlsStream;
use tower::Layer;

use crate::auth::{AuthPolicy, TlsClientIdentity};
use crate::cache_store::CacheStore;
use crate::chunk_index::ChunkIndex;
use crate::config::{CacheServerConfig, TlsConfig};
use crate::db::{CACHE_DB_FILE, CacheDb};
use crate::error::CacheServiceError;
use crate::evictor::start_evictor_task;
use crate::metrics::CacheMetrics;
use crate::origin_client::OriginClient;
use crate::state::{AppState, DedupIndexRebuildStats, build_router};

/// Startup controls for constructing cache-service runtime state.
pub struct ServerStartupOptions {
    pub metrics: CacheMetrics,
    pub start_evictor: bool,
    pub run_startup_eviction: bool,
}

/// Runtime state prepared before binding the HTTP listener.
pub struct PreparedServer {
    pub state: Arc<AppState>,
    evictor_handle: Option<crate::evictor::EvictorHandle>,
}

impl PreparedServer {
    /// Stops background eviction and drains admitted request mutations.
    pub async fn shutdown(self) {
        if let Some(evictor_handle) = self.evictor_handle {
            evictor_handle.shutdown().await;
        }
        self.state.cache_store.shutdown_mutations().await;
    }
}

/// Start the cache service HTTP server.
///
/// Opens the cache metadata database (recovering from corruption),
/// rebuilds the chunk index from cached shards, runs eviction if over budget,
/// then builds the router with middleware, binds to `config.listen_addr`, and
/// serves until SIGTERM triggers graceful shutdown.
pub async fn run_server(config: CacheServerConfig) -> Result<(), CacheServiceError> {
    let shutdown = shutdown_signal(config.drain_timeout)?;
    let metrics = CacheMetrics::new().map_err(|e| {
        CacheServiceError::InternalError(
            format!("failed to install prometheus recorder: {e}").into(),
        )
    })?;

    let prepared = prepare_server(
        config,
        ServerStartupOptions {
            metrics,
            start_evictor: true,
            run_startup_eviction: true,
        },
    )?;

    let state = Arc::clone(&prepared.state);
    let drain_timeout = state.config.drain_timeout;
    let listen_addr = state.config.listen_addr;
    let tls = state.config.tls.clone();
    let router = build_router(state);

    tracing::info!(%listen_addr, tls = tls.is_some(), "starting cache server");

    let result = if let Some(tls) = tls {
        serve_tls(router, listen_addr, tls, drain_timeout, shutdown).await
    } else {
        serve_plain(router, listen_addr, shutdown).await
    };

    prepared.shutdown().await;

    result
}

/// Opens cache-service runtime dependencies without binding the listener.
///
/// Rejects invalid policy and origin configuration before initializing the cache
/// or starting maintenance. Await [`PreparedServer::shutdown`] after use.
pub fn prepare_server(
    config: CacheServerConfig,
    options: ServerStartupOptions,
) -> Result<PreparedServer, CacheServiceError> {
    let policy = match &config.policy_path {
        Some(path) => {
            tracing::info!(path = %path.display(), "loading authorization policy");
            Some(AuthPolicy::from_file(path)?)
        }
        None => {
            tracing::info!("no policy_path configured, running in open-access mode");
            None
        }
    };

    // Reject unusable origin configuration before cache recovery/eviction can
    // mutate disk state or a maintenance task needs completion ownership.
    let origin = OriginClient::from_url(&config.origin_url)?;

    std::fs::create_dir_all(&config.cache_root).map_err(|e| {
        CacheServiceError::InternalError(
            format!(
                "failed to create cache root {}: {e}",
                config.cache_root.display()
            )
            .into(),
        )
    })?;

    // Step 1: Open cache.sqlite for object metadata and the chunk index.
    let cache_db_path = config.cache_root.join(CACHE_DB_FILE);
    tracing::info!(path = %cache_db_path.display(), "opening cache database");
    let cache_db = CacheDb::open_or_create(&cache_db_path)?;

    // Step 2: Open CacheStore — computes current_bytes from the metadata table.
    let cache_store = CacheStore::open(
        config.cache_root.clone(),
        config.max_cache_bytes,
        cache_db.connect()?,
    )?;
    tracing::info!(
        current_bytes = cache_store.current_bytes(),
        max_bytes = config.max_cache_bytes,
        "cache store opened"
    );

    // Step 3: Open a second connection for chunk-index reads and writes.
    let chunk_index = ChunkIndex::open(cache_db.connect()?)?;

    // Step 4: Rebuild chunk index from cached shards on disk.
    let shard_dir = config.cache_root.join("shards");
    tracing::info!(dir = %shard_dir.display(), "rebuilding chunk index from cached shards");
    let dedup_index_rebuild = match chunk_index.rebuild_from_shards(&shard_dir) {
        Ok(n) => {
            tracing::info!(entries = n, "chunk index rebuilt");
            DedupIndexRebuildStats {
                status: "ok".to_string(),
                entries: n,
                error: None,
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "chunk index rebuild failed, starting with empty index");
            DedupIndexRebuildStats {
                status: "failed".to_string(),
                entries: 0,
                error: Some(e.to_string()),
            }
        }
    };

    // Step 5: Run eviction if current_bytes exceeds max_bytes.
    if options.run_startup_eviction && cache_store.current_bytes() > config.max_cache_bytes {
        tracing::info!(
            current_bytes = cache_store.current_bytes(),
            max_bytes = config.max_cache_bytes,
            "cache over budget, running startup eviction"
        );
        match cache_store.evict_to_budget(config.high_water_ratio, config.low_water_ratio) {
            Ok(stats) => tracing::info!(
                evicted_count = stats.evicted_count,
                evicted_bytes = stats.evicted_bytes,
                "startup eviction complete"
            ),
            Err(e) => tracing::warn!(error = %e, "startup eviction failed"),
        }
    }

    // Step 6: Wrap cache store in Arc and start background evictor.
    let cache_store = Arc::new(cache_store);
    let (evictor_handle, evictor_notify) = if options.start_evictor {
        let handle = start_evictor_task(
            Arc::clone(&cache_store),
            config.high_water_ratio,
            config.low_water_ratio,
            std::time::Duration::from_secs(60),
        );
        let notify = handle.notify_handle();
        (Some(handle), notify)
    } else {
        (None, Arc::new(tokio::sync::Notify::new()))
    };

    Ok(PreparedServer {
        state: Arc::new(AppState {
            cache_store,
            chunk_index,
            origin,
            metrics: options.metrics,
            config,
            policy,
            evictor_notify,
            origin_healthy: AtomicBool::new(true),
            origin_health_checked_at: tokio::sync::Mutex::new(
                Instant::now() - Duration::from_secs(10),
            ),
            cache_miss_locks: std::sync::Mutex::new(HashMap::new()),
            push_warming_body_permits: AppState::push_warming_body_permits(),
            dedup_index_rebuild,
            dedup_last_ingestion_error: tokio::sync::RwLock::new(None),
        }),
        evictor_handle,
    })
}

/// Serve over plain TCP with graceful shutdown on SIGTERM.
async fn serve_plain(
    router: Router,
    listen_addr: std::net::SocketAddr,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), CacheServiceError> {
    let listener = tokio::net::TcpListener::bind(listen_addr)
        .await
        .map_err(|error| CacheServiceError::InternalError(error.into()))?;

    tracing::info!(%listen_addr, "listening (plain HTTP)");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|error| CacheServiceError::InternalError(error.into()))
}

/// Serve over TLS via `axum_server` + rustls with graceful shutdown on SIGTERM.
async fn serve_tls(
    router: Router,
    listen_addr: std::net::SocketAddr,
    tls: TlsConfig,
    drain_timeout: std::time::Duration,
    signal: impl std::future::Future<Output = ()>,
) -> Result<(), CacheServiceError> {
    let native_mtls = tls.client_ca_path.is_some();
    let rustls_config = build_rustls_config(&tls)?;

    tracing::info!(%listen_addr, native_mtls, "listening (TLS)");

    let handle = axum_server::Handle::new();
    let server = async {
        if native_mtls {
            let acceptor = TlsIdentityAcceptor::new(RustlsAcceptor::new(rustls_config));
            axum_server::bind(listen_addr)
                .acceptor(acceptor)
                .handle(handle.clone())
                .serve(router.into_make_service())
                .await
        } else {
            axum_server::bind_rustls(listen_addr, rustls_config)
                .handle(handle.clone())
                .serve(router.into_make_service())
                .await
        }
    };
    let shutdown = async {
        signal.await;
        // The dependency's graceful notification is not sticky. Wait for bind
        // and poll serving first so an early signal cannot miss its waiter.
        let _ = handle.listening().await;
        handle.graceful_shutdown(Some(drain_timeout));
    };
    tokio::pin!(server);
    let result = tokio::select! {
        biased;
        result = &mut server => result,
        () = shutdown => server.await,
    };
    result.map_err(|error| CacheServiceError::InternalError(error.into()))
}

/// Builds the rustls server config used by startup and preflight validation.
pub fn build_rustls_config(tls: &TlsConfig) -> Result<RustlsConfig, CacheServiceError> {
    let mut config = rustls_server_config(tls)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(RustlsConfig::from_config(Arc::new(config)))
}

fn rustls_server_config(tls: &TlsConfig) -> Result<ServerConfig, CacheServiceError> {
    install_rustls_crypto_provider();

    let certs = load_certs(&tls.cert_path)?;
    let key = load_private_key(&tls.key_path)?;
    let builder = ServerConfig::builder();

    let config = if let Some(client_ca_path) = &tls.client_ca_path {
        let roots = load_client_ca_roots(client_ca_path)?;
        let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|e| {
                CacheServiceError::ConfigError(format!(
                    "invalid TLS client CA bundle {}: {e}",
                    client_ca_path.display()
                ))
            })?;
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };

    config.with_single_cert(certs, key).map_err(|e| {
        CacheServiceError::ConfigError(format!(
            "invalid TLS certificate {} or key {}: {e}",
            tls.cert_path.display(),
            tls.key_path.display()
        ))
    })
}

fn install_rustls_crypto_provider() {
    static RUSTLS_CRYPTO_PROVIDER: std::sync::Once = std::sync::Once::new();
    RUSTLS_CRYPTO_PROVIDER.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, CacheServiceError> {
    let file = File::open(path).map_err(|e| {
        CacheServiceError::ConfigError(format!(
            "failed to open TLS certificate {}: {e}",
            path.display()
        ))
    })?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            CacheServiceError::ConfigError(format!(
                "failed to read TLS certificate {}: {e}",
                path.display()
            ))
        })?;
    if certs.is_empty() {
        return Err(CacheServiceError::ConfigError(format!(
            "TLS certificate {} contains no certificates",
            path.display()
        )));
    }
    Ok(certs)
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, CacheServiceError> {
    let file = File::open(path).map_err(|e| {
        CacheServiceError::ConfigError(format!("failed to open TLS key {}: {e}", path.display()))
    })?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| {
            CacheServiceError::ConfigError(format!(
                "failed to read TLS key {}: {e}",
                path.display()
            ))
        })?
        .ok_or_else(|| {
            CacheServiceError::ConfigError(format!(
                "TLS key {} contains no private key",
                path.display()
            ))
        })
}

fn load_client_ca_roots(path: &Path) -> Result<RootCertStore, CacheServiceError> {
    let certs = load_certs(path)?;
    let mut roots = RootCertStore::empty();
    let (valid, invalid) = roots.add_parsable_certificates(certs);
    if valid == 0 {
        return Err(CacheServiceError::ConfigError(format!(
            "TLS client CA bundle {} contains no valid root certificates",
            path.display()
        )));
    }
    if invalid > 0 {
        tracing::warn!(
            path = %path.display(),
            valid,
            invalid,
            "ignored invalid client CA certificates"
        );
    }
    Ok(roots)
}

#[derive(Clone, Debug)]
struct TlsIdentityAcceptor {
    inner: RustlsAcceptor,
}

impl TlsIdentityAcceptor {
    fn new(inner: RustlsAcceptor) -> Self {
        Self { inner }
    }
}

impl<I, S> Accept<I, S> for TlsIdentityAcceptor
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Send + 'static,
{
    type Stream = TlsStream<I>;
    type Service = AddExtension<S, TlsClientIdentity>;
    type Future = BoxFuture<'static, io::Result<(Self::Stream, Self::Service)>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let acceptor = self.inner.clone();
        Box::pin(async move {
            let (stream, service) = acceptor.accept(stream, service).await?;
            let server_conn = stream.get_ref().1;
            let identity = tls_client_identity(server_conn.peer_certificates())?;
            let service = Extension(identity).layer(service);
            Ok((stream, service))
        })
    }
}

fn tls_client_identity(
    peer_certificates: Option<&[CertificateDer<'static>]>,
) -> io::Result<TlsClientIdentity> {
    let leaf = peer_certificates
        .and_then(|certs| certs.first())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "verified client certificate missing",
            )
        })?;
    let digest = Sha256::digest(leaf.as_ref());
    Ok(TlsClientIdentity {
        principal: format!("mtls-sha256:{digest:x}"),
    })
}

// Register before serving so registration errors follow normal startup cleanup.
fn shutdown_signal(
    drain_timeout: Duration,
) -> Result<impl std::future::Future<Output = ()>, CacheServiceError> {
    #[cfg(unix)]
    let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
    #[cfg(windows)]
    let signal = tokio::signal::windows::ctrl_c();
    let mut signal = signal.map_err(|error| CacheServiceError::InternalError(error.into()))?;
    Ok(async move {
        signal.recv().await;
        tracing::info!(
            drain_timeout_secs = drain_timeout.as_secs(),
            "shutdown signal received, draining in-flight requests"
        );
    })
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn invalid_origin_leaves_no_cache_state_or_background_tasks() {
        use crate::config::{AuthConfig, DedupScope, MutablePathMode};

        for start_evictor in [true, false] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let temp = tempfile::tempdir().unwrap();
            let cache_root = temp.path().join("cache");
            let config = CacheServerConfig {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                tls: None,
                auth: AuthConfig::Psk { key_hash: [0; 32] },
                origin_url: "not a url".into(),
                cache_root: cache_root.clone(),
                max_cache_bytes: 1024,
                dedup_scope: DedupScope::All,
                drain_timeout: Duration::from_secs(1),
                mutable_path_mode: MutablePathMode::Strict,
                high_water_ratio: 0.95,
                low_water_ratio: 0.90,
                policy_path: None,
            };
            runtime.block_on(async {
                let before = runtime.metrics().num_alive_tasks();
                let result = prepare_server(
                    config,
                    ServerStartupOptions {
                        metrics: CacheMetrics::stub(),
                        start_evictor,
                        run_startup_eviction: start_evictor,
                    },
                );
                assert!(matches!(result, Err(CacheServiceError::ConfigError(_))));
                // This dedicated runtime contains only preparation's tasks.
                // Rejection must precede both disk mutation and task ownership.
                assert_eq!(
                    (runtime.metrics().num_alive_tasks(), cache_root.exists()),
                    (before, false),
                    "failed preparation retained state (evictor enabled: {start_evictor})"
                );
            });
        }
    }

    #[cfg(unix)]
    #[test]
    fn tls_startup_owns_signal_lifetime() {
        let temp = tempfile::tempdir().unwrap();
        let cert_path = temp.path().join("cert.pem");
        let key_path = temp.path().join("key.pem");
        let generated = std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-keyout",
            ])
            .arg(&key_path)
            .arg("-out")
            .arg(&cert_path)
            .output()
            .unwrap();
        assert!(
            generated.status.success(),
            "openssl certificate generation failed"
        );
        for client_ca_path in [None, Some(cert_path.clone())] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let tls = TlsConfig {
                cert_path: cert_path.clone(),
                key_path: key_path.clone(),
                client_ca_path,
            };
            runtime.block_on(async {
                let before = runtime.metrics().num_alive_tasks();
                let result = serve_tls(
                    Router::new(),
                    listener.local_addr().unwrap(),
                    tls.clone(),
                    Duration::from_secs(1),
                    std::future::pending(),
                )
                .await;
                let error = result.err().unwrap();
                let source = std::error::Error::source(&error).unwrap();
                assert_eq!(
                    source.downcast_ref::<io::Error>().unwrap().kind(),
                    io::ErrorKind::AddrInUse
                );
                assert_eq!(
                    runtime.metrics().num_alive_tasks(),
                    before,
                    "failed TLS bind detached a signal waiter"
                );
                // A signal ready before the first serve poll must still reach
                // the dependency's listener, whose notification is not sticky.
                tokio::time::timeout(
                    Duration::from_secs(1),
                    serve_tls(
                        Router::new(),
                        "127.0.0.1:0".parse().unwrap(),
                        tls,
                        Duration::from_millis(100),
                        std::future::ready(()),
                    ),
                )
                .await
                .unwrap()
                .unwrap();
            });
        }
    }

    #[test]
    fn tls_client_identity_uses_leaf_sha256_fingerprint() {
        let leaf = vec![1_u8, 2, 3];
        let certs = vec![CertificateDer::from(leaf.clone())];
        let identity = tls_client_identity(Some(&certs)).unwrap();
        let expected = format!("mtls-sha256:{:x}", Sha256::digest(&leaf));

        assert_eq!(identity.principal, expected);
    }

    #[test]
    fn tls_client_identity_rejects_missing_leaf() {
        let err = tls_client_identity(None).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }
}
