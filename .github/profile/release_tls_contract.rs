use object_store::{
    Certificate, ClientOptions, ObjectStoreExt, RetryConfig, aws::AmazonS3Builder, path::Path,
};
use std::time::Duration;

fn options() -> ClientOptions {
    ClientOptions::new()
        .with_connect_timeout(Duration::from_secs(3))
        .with_timeout(Duration::from_secs(3))
}

fn store(
    endpoint: &str,
    options: ClientOptions,
) -> object_store::Result<object_store::aws::AmazonS3> {
    AmazonS3Builder::new()
        .with_bucket_name("qualification")
        .with_region("us-east-1")
        .with_access_key_id("qualification")
        .with_secret_access_key("qualification")
        .with_endpoint(endpoint)
        .with_client_options(options)
        .with_retry(RetryConfig {
            max_retries: 0,
            ..Default::default()
        })
        .build()
}

#[test]
fn explicit_tls_trust_and_constructor_failure() {
    let ca = std::fs::read(std::env::var("CERT_CA").unwrap()).unwrap();
    let endpoint = std::env::var("CERT_ENDPOINT").unwrap();
    let certificate = Certificate::from_pem(&ca).unwrap();
    let options = || options().with_no_system_certificates(true);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let trusted = store(
            &endpoint,
            options().with_root_certificate(certificate.clone()),
        )
        .unwrap();
        assert_eq!(
            trusted
                .get(&Path::from("probe"))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .as_ref(),
            b"tls-probe"
        );
        let untrusted = store(&endpoint, options()).unwrap();
        let error = untrusted.get(&Path::from("probe")).await.unwrap_err();
        assert!(format!("{error:?}").contains("UnknownIssuer"), "{error:?}");
        let wrong = store(
            &endpoint.replace("localhost", "127.0.0.1"),
            options().with_root_certificate(certificate),
        )
        .unwrap();
        let error = wrong.get(&Path::from("probe")).await.unwrap_err();
        assert!(
            format!("{error:?}").contains("NotValidForName"),
            "{error:?}"
        );
        let malformed = Certificate::from_pem(
            b"-----BEGIN CERTIFICATE-----\n%%%%%%%%\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        assert!(store(&endpoint, options().with_root_certificate(malformed)).is_err());
    });
}

#[test]
#[cfg(target_os = "linux")]
fn native_tls_reloads_roots_and_rejects_invalid_trust() {
    let ca = std::fs::read(std::env::var("CERT_CA").unwrap()).unwrap();
    let other = std::fs::read(std::env::var("CERT_OTHER_CA").unwrap()).unwrap();
    let path = std::env::var("SSL_CERT_FILE").unwrap();
    let endpoint = std::env::var("CERT_ENDPOINT").unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let trusted = store(&endpoint, options()).unwrap();
        assert_eq!(
            trusted
                .get(&Path::from("probe"))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .as_ref(),
            b"tls-probe"
        );
        std::fs::write(&path, &other).unwrap();
        let replaced = store(&endpoint, options()).unwrap();
        let error = replaced.get(&Path::from("probe")).await.unwrap_err();
        assert!(format!("{error:?}").contains("UnknownIssuer"), "{error:?}");
        std::fs::write(&path, &ca).unwrap();
        let restored = store(&endpoint, options()).unwrap();
        assert_eq!(
            restored
                .get(&Path::from("probe"))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .as_ref(),
            b"tls-probe"
        );
        let wrong = store(&endpoint.replace("localhost", "127.0.0.1"), options()).unwrap();
        let error = wrong.get(&Path::from("probe")).await.unwrap_err();
        assert!(
            format!("{error:?}").contains("NotValidForName"),
            "{error:?}"
        );
        std::fs::write(
            &path,
            b"-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        assert!(
            store(&endpoint, options()).is_err(),
            "an empty native root store must fail construction"
        );
    });
}
