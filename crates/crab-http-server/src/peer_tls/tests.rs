#![cfg(unix)]

use std::process::Command;

use axum::{Router, extract::ConnectInfo, routing::get};
use tempfile::TempDir;
use tokio::sync::oneshot;
use url::Url;

use super::*;

pub(crate) struct IdentityFiles {
    _directory: TempDir,
    certificate: std::path::PathBuf,
    private_key: std::path::PathBuf,
    other_key: std::path::PathBuf,
    ca: std::path::PathBuf,
}

impl IdentityFiles {
    pub(crate) fn generate() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let ca_key = directory.path().join("ca.key");
        let ca = directory.path().join("ca.crt");
        let private_key = directory.path().join("peer.key");
        let other_key = directory.path().join("other.key");
        let request = directory.path().join("peer.csr");
        let certificate = directory.path().join("peer.crt");
        let extensions = directory.path().join("peer.ext");
        run(Command::new("openssl")
            .args(["genpkey", "-algorithm", "ED25519", "-out"])
            .arg(&ca_key));
        run(Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-new",
                "-days",
                "1",
                "-subj",
                "/CN=Crab Test CA",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-key",
            ])
            .arg(&ca_key)
            .arg("-out")
            .arg(&ca));
        for key in [&private_key, &other_key] {
            run(Command::new("openssl")
                .args(["genpkey", "-algorithm", "ED25519", "-out"])
                .arg(key));
        }
        run(Command::new("openssl")
            .args(["req", "-new", "-subj", "/CN=localhost", "-key"])
            .arg(&private_key)
            .arg("-out")
            .arg(&request));
        std::fs::write(
            &extensions,
            "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth,clientAuth\nsubjectAltName=DNS:localhost\n",
        )
        .unwrap();
        run(Command::new("openssl")
            .args(["x509", "-req", "-days", "1", "-CAcreateserial", "-in"])
            .arg(&request)
            .arg("-CA")
            .arg(&ca)
            .arg("-CAkey")
            .arg(&ca_key)
            .arg("-extfile")
            .arg(&extensions)
            .arg("-out")
            .arg(&certificate));
        Self {
            _directory: directory,
            certificate,
            private_key,
            other_key,
            ca,
        }
    }

    pub(crate) fn config(&self, endpoint: Url) -> CellsConfig {
        CellsConfig {
            data_dir: self._directory.path().join("cells"),
            peer_advertise: endpoint,
            failure_zone: None,
            failure_host: None,
            peer_tls_server_name: None,
            peer_certificate: self.certificate.clone(),
            peer_private_key: self.private_key.clone(),
            peer_ca: self.ca.clone(),
        }
    }
}

#[test]
fn identity_requires_one_ca_trusted_matching_ed25519_key() {
    let files = IdentityFiles::generate();
    let mut config = files.config(Url::parse("https://localhost:8789").unwrap());
    let loaded = LoadedPeerTls::load(&config).unwrap();
    assert_eq!(
        loaded.signing_key().verifying_key().to_bytes(),
        certificate_public_key(&load_certificates(&files.certificate).unwrap()[0]).unwrap()
    );
    config.peer_private_key = files.other_key.clone();
    assert!(matches!(
        LoadedPeerTls::load(&config),
        Err(Error::Config(_))
    ));
}

#[test]
fn stable_tls_name_allows_a_node_specific_advertised_ip() {
    let files = IdentityFiles::generate();
    let mut config = files.config(Url::parse("https://10.42.3.17:8789").unwrap());
    assert!(LoadedPeerTls::load(&config).is_err());
    config.peer_tls_server_name = Some("localhost".into());
    LoadedPeerTls::load(&config).unwrap();
}

#[test]
fn fleet_digest_is_independent_of_ca_order_and_duplicates() {
    let first = IdentityFiles::generate();
    let second = IdentityFiles::generate();
    let first_ca = load_certificates(&first.ca).unwrap().remove(0);
    let second_ca = load_certificates(&second.ca).unwrap().remove(0);

    let ordered = fleet_digest(&[first_ca.clone(), second_ca.clone()]);
    let reordered = fleet_digest(&[second_ca, first_ca.clone(), first_ca]);

    assert_eq!(ordered, reordered);
}

#[tokio::test]
async fn listener_requires_mtls_and_exposes_the_verified_leaf_identity() {
    let files = IdentityFiles::generate();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config =
        files.config(Url::parse(&format!("https://127.0.0.1:{}", address.port())).unwrap());
    config.peer_tls_server_name = Some("localhost".into());
    let loaded = LoadedPeerTls::load(&config).unwrap();
    let certificate = loaded.certificate();
    let public_key = loaded.signing_key().verifying_key().to_bytes();
    let peer_client = loaded.client_identity();
    let expected = format!("{}:{}", encode(certificate.as_bytes()), encode(&public_key));
    let app = Router::new().route(
        "/identity",
        get(
            |ConnectInfo(identity): ConnectInfo<PeerTlsIdentity>| async move {
                format!(
                    "{}:{}",
                    encode(identity.certificate().as_bytes()),
                    encode(&identity.public_key())
                )
            },
        ),
    );
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(
            loaded.listener(listener),
            app.into_make_service_with_connect_info::<PeerTlsIdentity>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    let roots = reqwest::Certificate::from_pem_bundle(&std::fs::read(&files.ca).unwrap()).unwrap();
    let url = format!("https://127.0.0.1:{}/identity", address.port());
    let anonymous = client_builder(roots.clone()).build().unwrap();
    assert!(anonymous.get(&url).send().await.is_err());

    let client = peer_client.client(certificate, public_key).unwrap();
    assert_eq!(
        client.get(&url).send().await.unwrap().text().await.unwrap(),
        expected
    );
    let wrong_pin = peer_client
        .client(CellDigest::from_bytes([9; 32]), public_key)
        .unwrap();
    assert!(wrong_pin.get(&url).send().await.is_err());

    shutdown_tx.send(()).unwrap();
    server.await.unwrap().unwrap();
}

fn run(command: &mut Command) {
    assert!(command.output().unwrap().status.success());
}

fn encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn client_builder(certificates: Vec<reqwest::Certificate>) -> reqwest::ClientBuilder {
    certificates.into_iter().fold(
        reqwest::Client::builder().tls_built_in_root_certs(false),
        reqwest::ClientBuilder::add_root_certificate,
    )
}
