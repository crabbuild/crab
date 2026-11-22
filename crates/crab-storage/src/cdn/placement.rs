use reqwest::{Client, StatusCode};
use serde::Deserialize;
use std::time::Duration;

use object_store::aws::{AwsAuthorizer, AwsCredentialProvider};
use object_store::client::{HttpRequest, HttpRequestBody};
use object_store::gcp::GcpCredentialProvider;

use crate::{Result, StorageError};

use super::publication::Placement;

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

    pub(crate) async fn verify(&self, placement: &Placement, timeout: Duration) -> Result<()> {
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
                    let mut response = client
                        .get(url)
                        .query(&[("fields", "name,projectNumber,location")])
                        .bearer_auth(&credential.bearer)
                        .send()
                        .await
                        .map_err(request_error)?;
                    if response.status() != StatusCode::OK {
                        return Err(mismatch());
                    }
                    let mut body = Vec::new();
                    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
                        if chunk.len() > 4096 - body.len() {
                            return Err(mismatch());
                        }
                        body.extend_from_slice(&chunk);
                    }
                    verify_gcs_bucket(&body, bucket, placement)
                }
                NativeAuthority::Azure => Err(StorageError::Forbidden {
                    path: "public CDN needs independently authorized Azure placement at activation"
                        .into(),
                }),
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
}
