use std::{
    fs::File,
    future::Future,
    io::{self, BufReader},
    net::SocketAddr,
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::extract::connect_info::Connected;
use crab_cell_runtime::Digest as CellDigest;
use ed25519_dalek::{SigningKey, pkcs8::DecodePrivateKey};
use futures_util::{StreamExt, stream::FuturesUnordered};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig,
    SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
    server::WebPkiClientVerifier,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use x509_cert::{Certificate, der::Decode, spki::ObjectIdentifier};

use crate::PeerHttpClientFactory;

/// An invalid pinned peer TLS identity, certificate, or listener setup.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// A required TLS file or identity property is invalid.
    #[error("{0}")]
    Config(&'static str),
    /// A certificate or private-key file could not be read.
    #[error("peer TLS file I/O failed")]
    Io(#[from] std::io::Error),
    /// Certificate validation or TLS client construction failed.
    #[error("private Cell TLS setup failed: {context}")]
    Setup {
        context: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

type Result<T> = std::result::Result<T, TlsError>;

const ED25519_OID: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.112");
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PENDING_HANDSHAKES: usize = 128;
const FLEET_DIGEST_DOMAIN: &[u8] = b"crab.peer-ca.v1\0";

/// Loaded private peer identity and its verified mTLS server configuration.
pub struct LoadedPeerTls {
    config: Arc<ServerConfig>,
    certificates: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
    roots: Arc<RootCertStore>,
    server_name: ServerName<'static>,
    signing_key: SigningKey,
    certificate: CellDigest,
    fleet: CellDigest,
}

impl LoadedPeerTls {
    /// Loads a CA-trusted Ed25519 identity without exposing key bytes.
    ///
    /// The leaf must be valid for client and server authentication under the
    /// given CA and name; a missing, mismatched, or invalid input returns an error.
    pub fn load(
        certificate_path: &Path,
        private_key_path: &Path,
        ca_path: &Path,
        server_name: &str,
    ) -> Result<Self> {
        install_crypto_provider();
        let certificates = load_certificates(certificate_path)?;
        let private_key = load_private_key(private_key_path)?;
        let signing_key =
            SigningKey::from_pkcs8_der(private_key.secret_der()).map_err(|source| {
                TlsError::Setup {
                    context: "peer private key is not Ed25519 PKCS#8",
                    source: Box::new(source),
                }
            })?;
        let leaf_key = certificate_public_key(&certificates[0])?;
        if signing_key.verifying_key().to_bytes() != leaf_key {
            return Err(TlsError::Config(
                "Cell peer certificate and private key do not match",
            ));
        }

        let authorities = load_certificates(ca_path)?;
        let roots = Arc::new(root_store(&authorities)?);
        let server_name =
            ServerName::try_from(server_name.to_owned()).map_err(|source| TlsError::Setup {
                context: "Cell peer TLS server name is invalid",
                source: Box::new(source),
            })?;
        verify_own_certificate(&certificates, Arc::clone(&roots), server_name.clone())?;
        let client_verifier = WebPkiClientVerifier::builder(Arc::clone(&roots))
            .build()
            .map_err(|source| TlsError::Setup {
                context: "Cell peer CA cannot verify clients",
                source: Box::new(source),
            })?;
        let mut config = ServerConfig::builder()
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(certificates.clone(), private_key.clone_key())
            .map_err(|source| TlsError::Setup {
                context: "Cell peer certificate or private key is invalid",
                source: Box::new(source),
            })?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let certificate = sha256_digest(certificates[0].as_ref());

        Ok(Self {
            config: Arc::new(config),
            certificates,
            private_key,
            roots,
            server_name,
            signing_key,
            certificate,
            fleet: fleet_digest(&authorities),
        })
    }

    /// Wraps a TCP listener with mutual TLS and verified peer connect info.
    pub fn listener(&self, listener: tokio::net::TcpListener) -> PeerTlsListener {
        PeerTlsListener {
            listener,
            acceptor: TlsAcceptor::from(Arc::clone(&self.config)),
            handshakes: FuturesUnordered::new(),
        }
    }

    /// Returns the key that signs this node's advertisement and peer requests.
    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }

    /// Returns the digest pinned by the node advertisement.
    pub const fn certificate(&self) -> CellDigest {
        self.certificate
    }

    /// Returns the digest of the trusted CA set used to scope the fleet.
    pub const fn fleet(&self) -> CellDigest {
        self.fleet
    }

    /// Builds the outbound identity for owner-resolving peer requests.
    pub fn client_identity(&self) -> PeerTlsClient {
        PeerTlsClient {
            certificates: self.certificates.clone(),
            private_key: self.private_key.clone_key(),
            roots: Arc::clone(&self.roots),
            server_name: self.server_name.clone(),
        }
    }
}

/// Fleet-authenticated client identity that pins every request to one enrolled leaf.
pub struct PeerTlsClient {
    certificates: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
    roots: Arc<RootCertStore>,
    server_name: ServerName<'static>,
}

impl Clone for PeerTlsClient {
    fn clone(&self) -> Self {
        Self {
            certificates: self.certificates.clone(),
            private_key: self.private_key.clone_key(),
            roots: Arc::clone(&self.roots),
            server_name: self.server_name.clone(),
        }
    }
}

impl PeerTlsClient {
    /// Builds a client pinned to the enrolled server certificate and key.
    ///
    /// Invalid trust configuration or client identity returns an error.
    pub fn client(&self, certificate: CellDigest, public_key: [u8; 32]) -> Result<reqwest::Client> {
        let verifier = WebPkiServerVerifier::builder(Arc::clone(&self.roots))
            .build()
            .map_err(|source| TlsError::Setup {
                context: "Cell peer CA cannot verify servers",
                source: Box::new(source),
            })?;
        let verifier = Arc::new(PinnedServerVerifier {
            verifier,
            certificate,
            public_key,
            server_name: self.server_name.clone(),
        });
        let mut config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(self.certificates.clone(), self.private_key.clone_key())
            .map_err(|source| TlsError::Setup {
                context: "Cell peer client identity is invalid",
                source: Box::new(source),
            })?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(8)
            .use_preconfigured_tls(config)
            .build()
            .map_err(|source| TlsError::Setup {
                context: "Cell peer HTTP client initialization failed",
                source: Box::new(source),
            })
    }
}

impl PeerHttpClientFactory for PeerTlsClient {
    fn client(
        &self,
        certificate: CellDigest,
        public_key: [u8; 32],
    ) -> crab_cell_runtime::Result<reqwest::Client> {
        PeerTlsClient::client(self, certificate, public_key).map_err(|source| {
            crab_cell_runtime::Error::PeerTransport {
                context: "peer mTLS client initialization failed",
                source: Box::new(source),
            }
        })
    }
}

#[derive(Debug)]
struct PinnedServerVerifier {
    verifier: Arc<WebPkiServerVerifier>,
    certificate: CellDigest,
    public_key: [u8; 32],
    server_name: ServerName<'static>,
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let verified = self.verifier.verify_server_cert(
            end_entity,
            intermediates,
            &self.server_name,
            ocsp_response,
            now,
        )?;
        let key = certificate_public_key(end_entity).map_err(|_| pin_error())?;
        if sha256_digest(end_entity.as_ref()) != self.certificate || key != self.public_key {
            return Err(pin_error());
        }
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.verifier
            .verify_tls12_signature(message, certificate, signature)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.verifier
            .verify_tls13_signature(message, certificate, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.verifier.supported_verify_schemes()
    }
}

const fn pin_error() -> rustls::Error {
    rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
}

/// Identity extracted only after rustls validates the complete client chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerTlsIdentity {
    certificate: CellDigest,
    public_key: [u8; 32],
}

impl PeerTlsIdentity {
    /// Returns the verified client's leaf certificate digest.
    pub const fn certificate(&self) -> CellDigest {
        self.certificate
    }

    /// Returns the verified client's Ed25519 leaf public key.
    pub const fn public_key(&self) -> [u8; 32] {
        self.public_key
    }
}

/// Axum listener that accepts only CA-trusted mutual TLS peers.
pub struct PeerTlsListener {
    listener: tokio::net::TcpListener,
    acceptor: TlsAcceptor,
    handshakes: FuturesUnordered<Handshake>,
}

type Handshake = Pin<Box<dyn Future<Output = Option<(PeerTlsStream, SocketAddr)>> + Send>>;

impl axum::serve::Listener for PeerTlsListener {
    type Io = PeerTlsStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                ready = self.handshakes.next(), if !self.handshakes.is_empty() => {
                    if let Some(Some(accepted)) = ready {
                        return accepted;
                    }
                }
                accepted = self.listener.accept(), if self.handshakes.len() < MAX_PENDING_HANDSHAKES => {
                    match accepted {
                        Ok((stream, address)) => {
                            let acceptor = self.acceptor.clone();
                            self.handshakes.push(Box::pin(handshake(acceptor, stream, address)));
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, "private listener accept failed");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

async fn handshake(
    acceptor: TlsAcceptor,
    stream: tokio::net::TcpStream,
    address: SocketAddr,
) -> Option<(PeerTlsStream, SocketAddr)> {
    let stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            tracing::debug!(error = %error, "private mTLS handshake rejected");
            return None;
        }
        Err(_) => {
            tracing::debug!("private mTLS handshake timed out");
            return None;
        }
    };
    let identity = match tls_identity(stream.get_ref().1.peer_certificates()) {
        Ok(identity) => identity,
        Err(error) => {
            tracing::warn!(error = %error, "verified private peer identity is invalid");
            return None;
        }
    };
    Some((PeerTlsStream { stream, identity }, address))
}

/// Verified TLS stream retaining the connecting peer's leaf identity.
pub struct PeerTlsStream {
    stream: TlsStream<tokio::net::TcpStream>,
    identity: PeerTlsIdentity,
}

impl AsyncRead for PeerTlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for PeerTlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

impl Connected<axum::serve::IncomingStream<'_, PeerTlsListener>> for PeerTlsIdentity {
    fn connect_info(stream: axum::serve::IncomingStream<'_, PeerTlsListener>) -> Self {
        stream.io().identity.clone()
    }
}

fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let file = File::open(path)?;
    let certificates = rustls_pemfile::certs(&mut BufReader::new(file))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if certificates.is_empty() {
        return Err(TlsError::Config("Cell peer PEM contains no certificates"));
    }
    Ok(certificates)
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    rustls_pemfile::private_key(&mut BufReader::new(File::open(path)?))?
        .ok_or(TlsError::Config("Cell peer PEM contains no private key"))
}

fn root_store(authorities: &[CertificateDer<'static>]) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    for authority in authorities {
        roots
            .add(authority.clone())
            .map_err(|source| TlsError::Setup {
                context: "Cell peer CA certificate is invalid",
                source: Box::new(source),
            })?;
    }
    Ok(roots)
}

fn verify_own_certificate(
    certificates: &[CertificateDer<'static>],
    roots: Arc<RootCertStore>,
    server_name: ServerName<'static>,
) -> Result<()> {
    let leaf = &certificates[0];
    let intermediates = &certificates[1..];
    let client = WebPkiClientVerifier::builder(Arc::clone(&roots))
        .build()
        .map_err(|source| TlsError::Setup {
            context: "Cell peer CA cannot verify clients",
            source: Box::new(source),
        })?;
    client
        .verify_client_cert(leaf, intermediates, UnixTime::now())
        .map_err(|source| TlsError::Setup {
            context: "Cell peer certificate is not valid for client authentication",
            source: Box::new(source),
        })?;

    WebPkiServerVerifier::builder(roots)
        .build()
        .map_err(|source| TlsError::Setup {
            context: "Cell peer CA cannot verify servers",
            source: Box::new(source),
        })?
        .verify_server_cert(leaf, intermediates, &server_name, &[], UnixTime::now())
        .map_err(|source| TlsError::Setup {
            context: "Cell peer certificate is not valid for its advertised endpoint",
            source: Box::new(source),
        })?;
    Ok(())
}

fn tls_identity(certificates: Option<&[CertificateDer<'static>]>) -> Result<PeerTlsIdentity> {
    let leaf = certificates
        .and_then(|certificates| certificates.first())
        .ok_or(TlsError::Config(
            "verified Cell peer certificate is missing",
        ))?;
    Ok(PeerTlsIdentity {
        certificate: sha256_digest(leaf.as_ref()),
        public_key: certificate_public_key(leaf)?,
    })
}

fn certificate_public_key(certificate: &CertificateDer<'_>) -> Result<[u8; 32]> {
    let certificate =
        Certificate::from_der(certificate.as_ref()).map_err(|source| TlsError::Setup {
            context: "Cell peer certificate is malformed",
            source: Box::new(source),
        })?;
    let key = &certificate.tbs_certificate.subject_public_key_info;
    if key.algorithm.oid != ED25519_OID || key.algorithm.parameters.is_some() {
        return Err(TlsError::Config("Cell peer certificate must use Ed25519"));
    }
    key.subject_public_key
        .as_bytes()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(TlsError::Config(
            "Cell peer certificate has an invalid Ed25519 public key",
        ))
}

fn sha256_digest(bytes: &[u8]) -> CellDigest {
    CellDigest::from_bytes(Sha256::digest(bytes).into())
}

fn fleet_digest(authorities: &[CertificateDer<'static>]) -> CellDigest {
    let mut authorities = authorities
        .iter()
        .map(|certificate| certificate.as_ref())
        .collect::<Vec<_>>();
    authorities.sort_unstable();
    authorities.dedup();
    let mut digest = Sha256::new();
    digest.update(FLEET_DIGEST_DOMAIN);
    for authority in authorities {
        digest.update((authority.len() as u64).to_be_bytes());
        digest.update(authority);
    }
    CellDigest::from_bytes(digest.finalize().into())
}

fn install_crypto_provider() {
    static PROVIDER: std::sync::Once = std::sync::Once::new();
    PROVIDER.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_digest_is_independent_of_ca_order_and_duplicates() {
        let first = CertificateDer::from(vec![1, 2, 3]);
        let second = CertificateDer::from(vec![4, 5, 6]);
        let ordered = fleet_digest(&[first.clone(), second.clone()]);
        let reordered = fleet_digest(&[second, first.clone(), first]);
        assert_eq!(ordered, reordered);
    }
}
