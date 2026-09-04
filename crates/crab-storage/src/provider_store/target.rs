use object_store::aws::{AmazonS3Builder, AmazonS3ConfigKey};
use object_store::azure::{AzureConfigKey, MicrosoftAzureBuilder};
use object_store::gcp::{GoogleCloudStorageBuilder, GoogleConfigKey};
use serde::Serialize;

use crate::error::{Result, StorageError};
use crate::identity::{StorageProviderKind, endpoint_identity};

fn digest(fields: &impl Serialize) -> Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new_derive_key("crab storage transport target v1");
    serde_json::to_writer(&mut hasher, fields).map_err(std::io::Error::other)?;
    Ok(*hasher.finalize().as_bytes())
}

fn flag(value: Option<String>) -> Result<bool> {
    // Match object_store's ConfigValue<bool> contract, including its aliases.
    match value.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("1" | "true" | "on" | "yes" | "y") => Ok(true),
        None | Some("0" | "false" | "off" | "no" | "n") => Ok(false),
        _ => Err(StorageError::InvalidStaticEnvTarget {
            target: "storage addressing".to_owned(),
            reason: "invalid boolean addressing option".to_owned(),
        }),
    }
}

pub(super) fn s3(builder: &AmazonS3Builder, bucket: &str) -> Result<[u8; 32]> {
    // object_store gives S3Endpoint precedence over Endpoint after all overrides.
    // Identity captures the same immutable builder, never a second env resolution.
    let endpoint = builder
        .get_config_value(&AmazonS3ConfigKey::S3Endpoint)
        .or_else(|| builder.get_config_value(&AmazonS3ConfigKey::Endpoint));
    let endpoint = endpoint.as_deref().map(endpoint_identity).transpose()?;
    let region = builder
        .get_config_value(&AmazonS3ConfigKey::Region)
        .unwrap_or_else(|| "us-east-1".to_owned());
    let virtual_hosted =
        flag(builder.get_config_value(&AmazonS3ConfigKey::VirtualHostedStyleRequest))?;
    let express = flag(builder.get_config_value(&AmazonS3ConfigKey::S3Express))?;
    let fields = (
        StorageProviderKind::S3,
        bucket,
        region,
        endpoint,
        virtual_hosted,
        express,
    );
    digest(&fields)
}

pub(super) fn gcs(
    builder: GoogleCloudStorageBuilder,
    bucket: &str,
) -> Result<(GoogleCloudStorageBuilder, [u8; 32])> {
    let mut base_url = builder.get_config_value(&GoogleConfigKey::BaseUrl);
    if base_url.is_none() {
        let key = match builder.get_config_value(&GoogleConfigKey::ServiceAccountKey) {
            Some(key) => Some(key),
            None => builder
                .get_config_value(&GoogleConfigKey::ServiceAccount)
                .map(std::fs::read_to_string)
                .transpose()?,
        };
        if let Some(key) = key {
            // Deserialize arbitrary JSON before validating its shape: serde's
            // typed data errors may echo a credential-bearing scalar value.
            let parsed: serde_json::Value =
                serde_json::from_str(&key).map_err(std::io::Error::other)?;
            base_url =
                match parsed.as_object().map(|fields| fields.get("gcs_base_url")) {
                    Some(None | Some(serde_json::Value::Null)) => None,
                    Some(Some(serde_json::Value::String(endpoint))) => Some(endpoint.clone()),
                    _ => return Err(StorageError::InvalidStaticEnvTarget {
                        target: "GCS service endpoint".to_owned(),
                        reason:
                            "expected a service-account object with an optional string gcs_base_url"
                                .to_owned(),
                    }),
                };
        }
    }
    // Pin the endpoint selected from service-account configuration before build.
    // Otherwise a credential-file replacement could make identity and transport
    // observe different targets. ADC does not override this object_store default.
    let base_url = base_url.unwrap_or_else(|| "https://storage.googleapis.com".to_owned());
    let endpoint = endpoint_identity(&base_url)?;
    let fields = (StorageProviderKind::Gcs, bucket, endpoint);
    Ok((builder.with_base_url(&base_url), digest(&fields)?))
}

pub(super) fn azure(builder: &MicrosoftAzureBuilder, bucket: &str) -> Result<[u8; 32]> {
    let emulator = flag(builder.get_config_value(&AzureConfigKey::UseEmulator))?;
    let fabric = flag(builder.get_config_value(&AzureConfigKey::UseFabricEndpoint))?;
    let account = builder.get_config_value(&AzureConfigKey::AccountName);
    let endpoint = if emulator {
        Some(
            std::env::var("AZURITE_BLOB_STORAGE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:10000".to_owned()),
        )
    } else {
        builder.get_config_value(&AzureConfigKey::Endpoint)
    };
    let endpoint = endpoint.as_deref().map(endpoint_identity).transpose()?;
    let fields = (
        StorageProviderKind::Azure,
        bucket,
        account,
        endpoint,
        emulator,
        fabric,
    );
    digest(&fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::{ClientOptions, ObjectStoreExt};
    use std::io::Write as _;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    fn s3_builder(endpoint: &str, credential: &str) -> AmazonS3Builder {
        AmazonS3Builder::new()
            .with_bucket_name("same-bucket")
            .with_region("us-east-1")
            .with_access_key_id(credential)
            .with_secret_access_key(credential)
            .with_token(credential)
            .with_endpoint(endpoint)
            .with_client_options(ClientOptions::new().with_allow_http(true))
    }

    #[test]
    fn s3_target_uses_effective_endpoint_and_ignores_credential_rotation() {
        let original = s3_builder("https://first.example", "old");
        let rotated = s3_builder("https://first.example", "new");
        let other = s3_builder("https://second.example", "old");
        let specific = other
            .clone()
            .with_config(AmazonS3ConfigKey::S3Endpoint, "https://first.example");
        let original = s3(&original, "same-bucket").unwrap();
        assert_eq!(original, s3(&rotated, "same-bucket").unwrap());
        assert_eq!(original, s3(&specific, "same-bucket").unwrap());
        assert_ne!(original, s3(&other, "same-bucket").unwrap());
    }

    #[test]
    fn azure_target_binds_account_endpoint_and_not_bearer_token() {
        let builder = MicrosoftAzureBuilder::new()
            .with_account("first-account")
            .with_container_name("same-container")
            .with_bearer_token_authorization("old");
        let original = azure(&builder, "same-container").unwrap();
        let rotated = builder.clone().with_bearer_token_authorization("new");
        assert_eq!(original, azure(&rotated, "same-container").unwrap());
        for changed in [
            builder.clone().with_account("second-account"),
            builder.with_endpoint("https://second.example".to_owned()),
        ] {
            assert_ne!(original, azure(&changed, "same-container").unwrap());
        }
    }

    #[test]
    fn gcs_endpoint_errors_do_not_echo_invalid_credential_values() {
        for key in [
            "\"private-token\"",
            "[\"private-token\"]",
            "{\"gcs_base_url\":[\"private-token\"]}",
            "{\"gcs_base_url\":\"https://private-token@store.example\"}",
        ] {
            let builder = GoogleCloudStorageBuilder::new()
                .with_config(GoogleConfigKey::ServiceAccountKey, key);
            let error = gcs(builder, "same-bucket").unwrap_err();
            assert!(!format!("{error:?}").contains("private-token"));
        }
    }

    async fn object_endpoint() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(10), async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    assert!(request.len() < 16 * 1024);
                    request.push(stream.read_u8().await.unwrap());
                }
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: \"same\"\r\nLast-Modified: Wed, 02 Sep 2026 00:00:00 GMT\r\nConnection: close\r\n\r\nbytes").await.unwrap();
            }).await.unwrap();
        });
        (endpoint, task)
    }

    #[tokio::test]
    async fn independent_endpoints_with_identical_objects_have_different_targets() {
        let (first, first_task) = object_endpoint().await;
        let (second, second_task) = object_endpoint().await;
        let mut proofs = Vec::new();
        for endpoint in [first, second] {
            let builder = s3_builder("http://127.0.0.1:1", "fixture")
                .with_config(AmazonS3ConfigKey::S3Endpoint, endpoint);
            let built =
                super::super::build_s3_object_store("same-bucket", builder, None, None, false)
                    .unwrap();
            let body = built
                .inner
                .get(&object_store::path::Path::from("manifest"))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            proofs.push((body, built.target_identity));
        }
        first_task.await.unwrap();
        second_task.await.unwrap();
        assert_eq!(proofs[0].0, proofs[1].0);
        assert_ne!(proofs[0].1, proofs[1].1);
    }

    #[tokio::test]
    async fn gcs_credential_file_rotation_cannot_redirect_captured_target() {
        let (endpoint, task) = object_endpoint().await;
        let mut credentials = tempfile::NamedTempFile::new().unwrap();
        let key = serde_json::json!({
            "private_key": "unused", "private_key_id": "fixture",
            "client_email": "fixture@example.invalid", "disable_oauth": true,
            "gcs_base_url": endpoint,
        });
        write!(credentials, "{key}").unwrap();
        let builder = GoogleCloudStorageBuilder::new()
            .with_bucket_name("same-bucket")
            .with_service_account_path(credentials.path().to_str().unwrap())
            .with_client_options(ClientOptions::new().with_allow_http(true));
        let (builder, captured) = gcs(builder, "same-bucket").unwrap();
        let mut replacement = key;
        replacement["gcs_base_url"] = "http://127.0.0.1:1".into();
        std::fs::write(credentials.path(), replacement.to_string()).unwrap();
        let store = builder.build().unwrap();
        let body = store
            .get(&object_store::path::Path::from("manifest"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        task.await.unwrap();
        let explicit = GoogleCloudStorageBuilder::new().with_base_url(&endpoint);
        assert_eq!(
            (body.as_ref(), captured),
            (b"bytes".as_slice(), gcs(explicit, "same-bucket").unwrap().1)
        );
    }
}
