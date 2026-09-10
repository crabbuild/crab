use crab_sdk::storage::{AzureOptions, DirectStoreOptions, GcsOptions, S3Options};
use crab_sdk::{ClientBuilder, ErrorKind, RepositoryLocator};

#[test]
fn configuration_builders_need_no_provider_or_runtime() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<ClientBuilder>();
    for store in [
        DirectStoreOptions::filesystem(&std::env::temp_dir().join("sdk-unopened-store")).unwrap(),
        DirectStoreOptions::s3_from_env("sdk-bucket").unwrap(),
        DirectStoreOptions::s3(
            S3Options::new("sdk-bucket", "us-east-1", "access", "secret").unwrap(),
        ),
        DirectStoreOptions::gcs_from_env("sdk-bucket").unwrap(),
        DirectStoreOptions::gcs(GcsOptions::new("sdk-bucket", "private-token").unwrap()),
        DirectStoreOptions::azure_from_env("sdkaccount", "sdk-container").unwrap(),
        DirectStoreOptions::azure(AzureOptions::bearer("account", "container", "token").unwrap()),
        DirectStoreOptions::azure(AzureOptions::sas("account", "container", "sig=token").unwrap()),
    ] {
        let _configuration = ClientBuilder::default().direct_store(store);
    }
    assert_eq!(
        RepositoryLocator::new("team/repository")
            .unwrap()
            .prefix()
            .unwrap(),
        "team/repository"
    );
}

#[test]
fn azure_credentials_validate_and_redact_before_provider_construction() {
    for token in [
        "",
        "bad\rvalue",
        "bad\nvalue",
        "bad\0value",
        "bad\tvalue",
        "bad value",
        "bad\u{7f}",
        "é",
    ] {
        assert_eq!(
            AzureOptions::bearer("account", "container", token)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
    }
    for token in [
        "",
        "?",
        "? ",
        "https://account.example/container?sig=private",
    ] {
        assert_eq!(
            AzureOptions::sas("account", "container", token)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
    }
    for (account, container) in [("", "container"), ("account", "a/b")] {
        assert!(AzureOptions::bearer(account, container, "token").is_err());
        assert!(AzureOptions::sas(account, container, "sig=token").is_err());
    }
    for options in [
        AzureOptions::bearer("account", "container", "private-bearer").unwrap(),
        AzureOptions::sas("account", "container", "sig=private-signature").unwrap(),
    ] {
        assert_eq!(format!("{options:?}"), "AzureOptions { .. }");
    }
}

#[cfg(feature = "remote")]
#[test]
fn explicit_azure_endpoint_rejects_embedded_secrets() {
    for endpoint in [
        "file:///private-value",
        "https://user:private-value@example.test",
        "https://example.test?token=private-value",
        "https://example.test#private-value",
    ] {
        let options = AzureOptions::bearer("account", "container", "token")
            .unwrap()
            .with_endpoint(endpoint);
        let error = ClientBuilder::default()
            .direct_store(DirectStoreOptions::azure(options))
            .build()
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(!format!("{error:?}").contains("private-value"));
    }
}

#[test]
fn explicit_gcs_configuration_validates_and_redacts_credentials() {
    for (bucket, token) in [("gs://bucket", "private-token"), ("bucket", "")] {
        assert_eq!(
            GcsOptions::new(bucket, token).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }
    let options = GcsOptions::new("bucket", "private-token").unwrap();
    assert_eq!(format!("{options:?}"), "GcsOptions { .. }");
}

#[test]
fn explicit_s3_configuration_validates_and_redacts_credentials() {
    for fields in [
        ["s3://bucket", "region", "access", "secret"],
        ["bucket", "", "access", "secret"],
        ["bucket", "region", "", "secret"],
        ["bucket", "region", "access", ""],
    ] {
        assert_eq!(
            S3Options::new(fields[0], fields[1], fields[2], fields[3])
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
    }
    let options = S3Options::new("bucket", "region", "private-access", "private-secret")
        .unwrap()
        .with_session_token("private-session")
        .unwrap()
        .with_endpoint("https://private-endpoint.example");
    assert_eq!(format!("{options:?}"), "S3Options { .. }");
    assert_eq!(
        options.with_session_token("").unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
}

#[cfg(feature = "remote")]
#[test]
fn explicit_s3_endpoint_rejects_embedded_secrets() {
    for endpoint in [
        "file:///private-value",
        "https://user:private-value@example.test",
        "https://example.test?token=private-value",
        "https://example.test#private-value",
    ] {
        let options = S3Options::new("bucket", "us-east-1", "access", "secret")
            .unwrap()
            .with_endpoint(endpoint);
        let error = ClientBuilder::default()
            .direct_store(DirectStoreOptions::s3(options))
            .build()
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(!format!("{error:?}").contains("private-value"));
    }
}

#[test]
fn invalid_cloud_names_fail_before_building_a_client() {
    assert_eq!(
        DirectStoreOptions::s3_from_env("s3://bucket/repo")
            .err()
            .unwrap()
            .kind(),
        ErrorKind::InvalidInput
    );
}
