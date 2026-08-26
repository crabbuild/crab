//! Gateway startup, TLS configuration, and graceful shutdown.

use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;
use std::sync::Arc;

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
use tokio_rustls::server::TlsStream;
use tower::Layer;

use crab_storage::build_url_object_store;

use crate::auth::{AuthPolicy, TlsClientIdentity};
use crate::config::LfsServerConfig;
use crate::error::{LfsServerError, Result};
use crate::http::{AppState, build_router};

/// Controls optional startup work for tests and embedding applications.
#[derive(Debug, Clone, Copy, Default)]
pub struct ServerStartupOptions;

/// Runtime state prepared before binding a listener.
pub struct PreparedServer {
    /// Shared gateway application state.
    pub state: Arc<AppState>,
}

/// Builds application state without binding a listener.
pub fn prepare_server(
    config: LfsServerConfig,
    _options: ServerStartupOptions,
) -> Result<PreparedServer> {
    std::fs::create_dir_all(&config.spool_dir).map_err(|source| {
        LfsServerError::Config(format!(
            "failed to create upload spool directory {}: {source}",
            config.spool_dir.display()
        ))
    })?;
    let origin = build_url_object_store(&config.origin_url)
        .map_err(|source| LfsServerError::OriginConfig(source.to_string()))?;
    let policy = config
        .policy_path
        .as_deref()
        .map(AuthPolicy::from_file)
        .transpose()?;
    let max_object_bytes = usize::try_from(config.max_object_bytes).map_err(|_| {
        LfsServerError::Config("server.max_object_bytes does not fit in usize".to_owned())
    })?;
    let state = AppState {
        max_object_bytes,
        upload_permits: Arc::new(tokio::sync::Semaphore::new(config.max_uploads)),
        download_permits: Arc::new(tokio::sync::Semaphore::new(
            crate::http::MAX_CONCURRENT_REQUESTS,
        )),
        config: Arc::new(config),
        origin,
        policy,
    };
    Ok(PreparedServer {
        state: Arc::new(state),
    })
}

/// Runs the gateway until the process receives its shutdown signal.
pub async fn run_server(config: LfsServerConfig) -> Result<()> {
    let prepared = prepare_server(config, ServerStartupOptions)?;
    let listen_addr = prepared.state.config.listen_addr;
    let tls = prepared.state.config.tls.clone();
    let router = build_router(Arc::clone(&prepared.state));
    tracing::info!(%listen_addr, tls = tls.is_some(), "starting Git LFS server");

    if let Some(tls) = tls {
        serve_tls(router, listen_addr, &tls).await
    } else {
        serve_plain(router, listen_addr).await
    }
}

async fn serve_plain(router: Router, listen_addr: std::net::SocketAddr) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen_addr)
        .await
        .map_err(|source| {
            LfsServerError::Server(format!("failed to bind {listen_addr}: {source}"))
        })?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|source| LfsServerError::Server(format!("HTTP server failed: {source}")))
}

async fn serve_tls(
    router: Router,
    listen_addr: std::net::SocketAddr,
    tls: &crate::config::TlsConfig,
) -> Result<()> {
    let rustls_config = build_rustls_config(tls)?;
    let handle = axum_server::Handle::new();
    let shutdown_handle = handle.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        shutdown_handle.graceful_shutdown(None);
    });

    let result = if tls.client_ca_path.is_some() {
        let acceptor = TlsIdentityAcceptor::new(RustlsAcceptor::new(rustls_config));
        axum_server::bind(listen_addr)
            .acceptor(acceptor)
            .handle(handle)
            .serve(router.into_make_service())
            .await
    } else {
        axum_server::bind_rustls(listen_addr, rustls_config)
            .handle(handle)
            .serve(router.into_make_service())
            .await
    };
    result.map_err(|source| LfsServerError::Server(format!("TLS server failed: {source}")))
}

/// Builds the rustls configuration used by the gateway.
pub fn build_rustls_config(tls: &crate::config::TlsConfig) -> Result<RustlsConfig> {
    install_rustls_crypto_provider();
    let certs = load_certs(&tls.cert_path)?;
    let key = load_private_key(&tls.key_path)?;
    let builder = ServerConfig::builder();
    let config = if let Some(client_ca_path) = &tls.client_ca_path {
        let roots = load_client_ca_roots(client_ca_path)?;
        let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|source| {
                LfsServerError::Tls(format!(
                    "invalid client CA bundle {}: {source}",
                    client_ca_path.display()
                ))
            })?;
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };
    let mut config = config.with_single_cert(certs, key).map_err(|source| {
        LfsServerError::Tls(format!(
            "invalid TLS certificate {} or key {}: {source}",
            tls.cert_path.display(),
            tls.key_path.display()
        ))
    })?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(RustlsConfig::from_config(Arc::new(config)))
}

fn install_rustls_crypto_provider() {
    static PROVIDER: std::sync::Once = std::sync::Once::new();
    PROVIDER.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let file = File::open(path).map_err(|source| {
        LfsServerError::Tls(format!(
            "failed to open certificate {}: {source}",
            path.display()
        ))
    })?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|source| {
            LfsServerError::Tls(format!(
                "failed to read certificate {}: {source}",
                path.display()
            ))
        })?;
    if certs.is_empty() {
        return Err(LfsServerError::Tls(format!(
            "certificate {} contains no certificates",
            path.display()
        )));
    }
    Ok(certs)
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let file = File::open(path).map_err(|source| {
        LfsServerError::Tls(format!(
            "failed to open private key {}: {source}",
            path.display()
        ))
    })?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|source| {
            LfsServerError::Tls(format!(
                "failed to read private key {}: {source}",
                path.display()
            ))
        })?
        .ok_or_else(|| {
            LfsServerError::Tls(format!(
                "private key {} contains no private key",
                path.display()
            ))
        })
}

fn load_client_ca_roots(path: &Path) -> Result<RootCertStore> {
    let certs = load_certs(path)?;
    let mut roots = RootCertStore::empty();
    let (valid, invalid) = roots.add_parsable_certificates(certs);
    if valid == 0 {
        return Err(LfsServerError::Tls(format!(
            "client CA bundle {} contains no valid roots",
            path.display()
        )));
    }
    if invalid > 0 {
        tracing::warn!(path = %path.display(), valid, invalid, "ignored invalid client CA certificates");
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
            let identity = tls_client_identity(stream.get_ref().1.peer_certificates())?;
            let service = Extension(identity).layer(service);
            Ok((stream, service))
        })
    }
}

fn tls_client_identity(
    peer_certificates: Option<&[CertificateDer<'static>]>,
) -> io::Result<TlsClientIdentity> {
    let leaf = peer_certificates
        .and_then(|certificates| certificates.first())
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

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(source) => {
                tracing::error!(%source, "failed to register SIGTERM handler");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(source) = tokio::signal::ctrl_c().await {
            tracing::error!(%source, "failed to register ctrl-c handler");
        }
    }
}
