use reqwest::{Client, StatusCode, header};
use serde::Deserialize;
use std::time::Duration;

use object_store::aws::{AwsAuthorizer, AwsCredentialProvider};
use object_store::client::{HttpRequest, HttpRequestBody};
use object_store::gcp::GcpCredentialProvider;

use crate::{Result, StorageError};

use super::publication::Placement;

/// Provides a sensitive authorization header for Azure Resource Manager requests.
#[async_trait::async_trait]
pub trait AzureManagementAuthorization: Send + Sync {
    /// Resolve the operator's management-plane identity when a receipt is activated.
    async fn authorization(&self) -> Result<header::HeaderValue>;
}

/// Canonical CDN origin and its selected native credential authority.
#[derive(Clone)]
pub struct NativeCdnOrigin {
    pub(crate) address: String,
    pub(crate) authority: NativeAuthority,
}

#[derive(Clone)]
pub(crate) enum NativeAuthority {
    S3 {
        region: String,
        credentials: AwsCredentialProvider,
    },
    Gcs(GcpCredentialProvider),
    Azure,
    #[cfg(any(test, feature = "test-support"))]
    Fixture,
}

impl NativeCdnOrigin {
    /// Return the canonical provider origin without credentials.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn fixture(address: String) -> Self {
        Self {
            address,
            authority: NativeAuthority::Fixture,
        }
    }

    pub(crate) async fn verify(
        &self,
        placement: &Placement,
        timeout: Duration,
        azure_authority: Option<&dyn AzureManagementAuthorization>,
    ) -> Result<()> {
        #[cfg(any(test, feature = "test-support"))]
        if matches!(self.authority, NativeAuthority::Fixture) {
            return Ok(());
        }
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(timeout)
            .build()
            .map_err(request_error)?;
        let proof = async {
            match &self.authority {
                NativeAuthority::S3 {
                    region,
                    credentials,
                } => {
                    let bucket = self.address.strip_prefix("s3://").ok_or_else(mismatch)?;
                    if region != &placement.location {
                        return Err(mismatch());
                    }
                    let owner = placement.account_or_project().ok_or_else(mismatch)?;
                    let credential = credentials
                        .get_credential()
                        .await
                        .map_err(|source| StorageError::ObjectStore { source })?;
                    let url = format!("https://s3.{region}.amazonaws.com/{bucket}");
                    let request = signed_head(&url, region, owner, &credential)?;
                    let response = client
                        .head(url)
                        .headers(request.into_parts().0.headers)
                        .send()
                        .await
                        .map_err(request_error)?;
                    verify_s3_response(response.status(), response.headers(), region)
                }
                NativeAuthority::Gcs(credentials) => {
                    let bucket = self.address.strip_prefix("gs://").ok_or_else(mismatch)?;
                    let credential = credentials
                        .get_credential()
                        .await
                        .map_err(|source| StorageError::ObjectStore { source })?;
                    let url = format!("https://storage.googleapis.com/storage/v1/b/{bucket}");
                    let response = client
                        .get(url)
                        .query(&[("fields", "name,projectNumber,location")])
                        .bearer_auth(&credential.bearer)
                        .send()
                        .await
                        .map_err(request_error)?;
                    let body = bounded_response(response, 4096).await?;
                    verify_gcs_bucket(&body, bucket, placement)
                }
                NativeAuthority::Azure => {
                    let authority = azure_authority.ok_or_else(|| StorageError::Forbidden {
                        path:
                            "Azure Resource Manager identity is required for public CDN placement"
                                .into(),
                    })?;
                    let mut authorization = authority.authorization().await?;
                    authorization.set_sensitive(true);
                    verify_azure_placement(&client, &self.address, placement, authorization).await
                }
                #[cfg(any(test, feature = "test-support"))]
                NativeAuthority::Fixture => Ok(()),
            }
        };
        tokio::time::timeout(timeout, proof)
            .await
            .map_err(|_| StorageError::Forbidden {
                path: "native public CDN placement could not be verified before the deadline"
                    .into(),
            })?
    }
}

async fn bounded_response(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response.status() != StatusCode::OK {
        return Err(mismatch());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if chunk.len() > limit - body.len() {
            return Err(mismatch());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn verify_azure_placement(
    client: &Client,
    address: &str,
    placement: &Placement,
    authorization: header::HeaderValue,
) -> Result<()> {
    let get = |resource: &str| {
        let url = format!("https://management.azure.com{resource}");
        client
            .get(url)
            .query(&[("api-version", "2025-08-01")])
            .header(header::AUTHORIZATION, authorization.clone())
    };
    let (account_id, _) = placement
        .resource
        .split_once("/blobservices/")
        .ok_or_else(mismatch)?;
    let read = |request: reqwest::RequestBuilder| async move {
        let response = request.send().await.map_err(request_error)?;
        bounded_response(response, 1024 * 1024).await
    };
    let (account, container) =
        tokio::try_join!(read(get(account_id)), read(get(&placement.resource)))?;
    verify_azure_resources(&account, &container, address, placement)
}

fn verify_azure_resources(
    account: &[u8],
    container: &[u8],
    address: &str,
    placement: &Placement,
) -> Result<()> {
    #[derive(Deserialize)]
    struct ResourceId {
        id: String,
    }
    #[derive(Deserialize)]
    struct Account {
        id: String,
        properties: AccountProperties,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct AccountProperties {
        primary_location: String,
        primary_endpoints: PrimaryEndpoints,
    }
    #[derive(Deserialize)]
    struct PrimaryEndpoints {
        blob: String,
    }

    let account: Account = serde_json::from_slice(account).map_err(|_| mismatch())?;
    let container: ResourceId = serde_json::from_slice(container).map_err(|_| mismatch())?;
    let (account_id, _) = placement
        .resource
        .split_once("/blobservices/")
        .ok_or_else(mismatch)?;
    let (blob_endpoint, _) = address.rsplit_once('/').ok_or_else(mismatch)?;
    if account.id.eq_ignore_ascii_case(account_id)
        && container.id.eq_ignore_ascii_case(&placement.resource)
        && account.properties.primary_location == placement.location
        && account
            .properties
            .primary_endpoints
            .blob
            .trim_end_matches('/')
            == blob_endpoint
    {
        return Ok(());
    }
    Err(mismatch())
}

fn signed_head(
    url: &str,
    region: &str,
    owner: &str,
    credential: &object_store::aws::AwsCredential,
) -> Result<HttpRequest> {
    let mut request = hyper::Request::builder()
        .method("HEAD")
        .uri(url)
        .header("x-amz-expected-bucket-owner", owner)
        .body(HttpRequestBody::empty())
        .map_err(|_| mismatch())?;
    AwsAuthorizer::new(credential, "s3", region)
        .try_authorize(&mut request, None)
        .map_err(|source| StorageError::ObjectStore { source })?;
    Ok(request)
}

fn verify_s3_response(
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    region: &str,
) -> Result<()> {
    if status == StatusCode::OK
        && headers
            .get("x-amz-bucket-region")
            .and_then(|value| value.to_str().ok())
            == Some(region)
    {
        return Ok(());
    }
    Err(mismatch())
}

fn verify_gcs_bucket(bytes: &[u8], bucket: &str, placement: &Placement) -> Result<()> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Bucket {
        name: String,
        project_number: String,
        location: String,
    }
    let observed: Bucket = serde_json::from_slice(bytes).map_err(|_| mismatch())?;
    if observed.name == bucket
        && Some(observed.project_number.as_str()) == placement.account_or_project()
        && observed.location == placement.location
    {
        return Ok(());
    }
    Err(mismatch())
}

fn mismatch() -> StorageError {
    StorageError::Forbidden {
        path: "native origin authority does not match the public CDN placement approval".into(),
    }
}

fn request_error(source: reqwest::Error) -> StorageError {
    StorageError::ObjectStore {
        source: object_store::Error::Generic {
            store: "native CDN placement request",
            source: Box::new(source.without_url()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderMap;

    #[test]
    fn s3_native_proof_signs_expected_owner_and_requires_authenticated_region() {
        let credential = object_store::aws::AwsCredential {
            key_id: "fixture".into(),
            secret_key: "fixture-key".into(),
            token: Some("fixture-token".into()),
        };
        let request = signed_head(
            "https://s3.us-west-2.amazonaws.com/fixture",
            "us-west-2",
            "123456789012",
            &credential,
        )
        .unwrap();
        assert_eq!(request.method(), "HEAD");
        assert_eq!(
            request.headers()["x-amz-expected-bucket-owner"],
            "123456789012"
        );
        let authorization = request.headers()["authorization"].to_str().unwrap();
        assert!(authorization.contains("/us-west-2/s3/aws4_request"));
        assert!(authorization.contains("x-amz-expected-bucket-owner"));

        let mut headers = HeaderMap::new();
        headers.insert("x-amz-bucket-region", "us-west-2".parse().unwrap());
        assert!(verify_s3_response(StatusCode::OK, &headers, "us-west-2").is_ok());
        assert!(verify_s3_response(StatusCode::FORBIDDEN, &headers, "us-west-2").is_err());
        assert!(verify_s3_response(StatusCode::OK, &headers, "us-east-1").is_err());
    }

    #[test]
    fn gcs_native_proof_matches_bucket_project_and_location() {
        let expected: Placement =
            serde_json::from_str(r#"{"resource":"projects/1234/buckets/fixture","location":"US"}"#)
                .unwrap();
        for (document, accepted) in [
            (
                r#"{"name":"fixture","projectNumber":"1234","location":"US"}"#,
                true,
            ),
            (
                r#"{"name":"other","projectNumber":"1234","location":"US"}"#,
                false,
            ),
            (
                r#"{"name":"fixture","projectNumber":"5678","location":"US"}"#,
                false,
            ),
            (
                r#"{"name":"fixture","projectNumber":"1234","location":"EU"}"#,
                false,
            ),
            (r#"{"name":"fixture","location":"US"}"#, false),
        ] {
            assert_eq!(
                verify_gcs_bucket(document.as_bytes(), "fixture", &expected).is_ok(),
                accepted
            );
        }
    }

    #[test]
    fn azure_native_proof_binds_arm_ids_primary_location_and_blob_endpoint() {
        let account_id = "/subscriptions/11111111-2222-3333-4444-555555555555/resourcegroups/fixture/providers/microsoft.storage/storageaccounts/fixtureacct";
        let expected = Placement {
            resource: format!("{account_id}/blobservices/default/containers/repo"),
            location: "westus2".into(),
        };
        let address = "https://fixtureacct.blob.core.windows.net/repo";
        let account = serde_json::json!({
            "id": account_id.to_ascii_uppercase(),
            "properties": {
                "primaryLocation": "westus2",
                "primaryEndpoints": {"blob": "https://fixtureacct.blob.core.windows.net/"}
            }
        });
        let container = serde_json::json!({"id": expected.resource.to_ascii_uppercase()});
        let verify = |account: &serde_json::Value, container: &serde_json::Value| {
            verify_azure_resources(
                &serde_json::to_vec(account).unwrap(),
                &serde_json::to_vec(container).unwrap(),
                address,
                &expected,
            )
        };
        assert!(verify(&account, &container).is_ok());

        for (path, value) in [
            (
                "/id",
                account_id.replace("resourcegroups/fixture", "resourcegroups/other"),
            ),
            ("/properties/primaryLocation", "eastus".into()),
            (
                "/properties/primaryEndpoints/blob",
                "https://otheracct.blob.core.windows.net/".into(),
            ),
        ] {
            let mut wrong = account.clone();
            *wrong.pointer_mut(path).unwrap() = value.into();
            assert!(verify(&wrong, &container).is_err(), "{path}");
        }
        let sibling = serde_json::json!({"id": format!("{account_id}/blobservices/default/containers/sibling")});
        assert!(verify(&account, &sibling).is_err());
    }

    #[tokio::test]
    async fn azure_placement_requires_the_injected_management_identity() {
        struct Denied;

        #[async_trait::async_trait]
        impl AzureManagementAuthorization for Denied {
            async fn authorization(&self) -> Result<header::HeaderValue> {
                Err(StorageError::AuthFailed {
                    path: "fixture ARM authorization denied".into(),
                })
            }
        }

        let origin = NativeCdnOrigin {
            address: "https://fixtureacct.blob.core.windows.net/repo".into(),
            authority: NativeAuthority::Azure,
        };
        let placement = Placement {
            resource: "/subscriptions/11111111-2222-3333-4444-555555555555/resourcegroups/fixture/providers/microsoft.storage/storageaccounts/fixtureacct/blobservices/default/containers/repo".into(),
            location: "westus2".into(),
        };
        let deadline = Duration::from_secs(1);
        assert!(matches!(
            origin.verify(&placement, deadline, None).await,
            Err(StorageError::Forbidden { .. })
        ));
        assert!(
            matches!(origin.verify(&placement, deadline, Some(&Denied)).await,
                         Err(StorageError::AuthFailed { path }) if path == "fixture ARM authorization denied")
        );
    }
}
