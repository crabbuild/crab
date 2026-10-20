use crate::test_support::tls::IdentityFiles;

use axum::{Router, extract::ConnectInfo, routing::get};
use tokio::sync::oneshot;
use url::Url;

use super::*;

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

#[tokio::test]
async fn listener_requires_mtls_and_exposes_the_verified_leaf_identity() {
    let files = IdentityFiles::generate();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config =
        files.config(Url::parse(&format!("https://127.0.0.1:{}", address.port())).unwrap());
    config.peer_tls_server_name = Some("localhost".into());
    let loaded = LoadedPeerTls::load(&config).unwrap();
    let certificate = sha256_digest(load_certificates(&files.certificate).unwrap()[0].as_ref());
    let public_key = loaded.signing_key().verifying_key().to_bytes();
    let peer_client = loaded.client_identity();
    let expected = format!("{}:{}", encode(&certificate), encode(&public_key));
    let app = Router::new().route(
        "/identity",
        get(
            |ConnectInfo(identity): ConnectInfo<PeerTlsIdentity>| async move {
                format!(
                    "{}:{}",
                    encode(&identity.certificate()),
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

    let client = peer_client.client().unwrap();
    assert_eq!(
        client.get(&url).send().await.unwrap().text().await.unwrap(),
        expected
    );
    shutdown_tx.send(()).unwrap();
    server.await.unwrap().unwrap();
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
