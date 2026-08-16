use std::time::SystemTime;

use crab_auth::{
    CloudCredentials, PushFinalizeResponse, PushPrepareResponse, parse_credential_response,
    validate_push_finalize_response, validate_push_prepare_response,
};
use crab_git::{CrabUrl, RepositoryUrl};
use serde::Deserialize;

const FIXTURE_ROOT: &str = "fixtures/released-v1.0.14";

#[derive(Debug, Deserialize)]
struct ContractManifest {
    release: String,
    commit: String,
    direct_url: String,
    bucket: String,
    repo_prefix: String,
    endpoints: Vec<String>,
}

#[test]
fn released_direct_repository_url_contract_is_frozen() {
    let manifest: ContractManifest = fixture("manifest.json");

    assert_eq!(manifest.release, "v1.0.14");
    assert_eq!(manifest.commit, "a253548a41ec8744d5a60af7048644cc57c8e6fe");
    assert_eq!(
        manifest.endpoints,
        ["/v1/credentials", "/v1/push/prepare", "/v1/push/finalize"]
    );

    let crab_url = CrabUrl::parse(&manifest.direct_url).unwrap();
    assert_eq!(crab_url.bucket, manifest.bucket);
    assert_eq!(crab_url.repo_path, manifest.repo_prefix);

    let repository_url = RepositoryUrl::parse(&manifest.direct_url).unwrap();
    assert_eq!(repository_url.bucket, manifest.bucket);
    assert_eq!(repository_url.repo_prefix, manifest.repo_prefix);
}

#[test]
fn released_s3_credential_contract_is_frozen() {
    let body = fixture_text("credentials-s3.json");
    let response = parse_credential_response(&body).unwrap();

    assert_eq!(response.provider, "s3");
    assert_eq!(response.permissions, ["read"]);
    assert_eq!(response.expires_at, "2026-07-21T20:11:09Z");
    assert!(response.storage_scope.is_none());
    assert!(matches!(
        response.cloud_credentials(SystemTime::UNIX_EPOCH).unwrap(),
        CloudCredentials::Aws {
            session_token: None,
            region,
            ..
        } if region == "us-east-1"
    ));
}

#[test]
fn released_protected_push_contract_is_frozen() {
    let prepare: PushPrepareResponse = fixture("push-prepare-s3.json");
    validate_push_prepare_response("ml/models", &prepare).unwrap();
    assert_eq!(prepare.provider, "s3");
    assert_eq!(prepare.permissions, ["immutable-write"]);

    let finalize: PushFinalizeResponse = fixture("push-finalize.json");
    validate_push_finalize_response(&finalize).unwrap();
    assert_eq!(finalize.status, "updated");
    assert_eq!(finalize.ref_updates.len(), 1);
}

#[test]
fn released_error_envelope_contract_is_frozen() {
    let invalid_repo: serde_json::Value = fixture("error-invalid-repo-url.json");
    let forbidden: serde_json::Value = fixture("error-forbidden.json");

    assert_eq!(
        invalid_repo
            .pointer("/detail/error")
            .and_then(|v| v.as_str()),
        Some("invalid_repo_url")
    );
    assert_eq!(
        forbidden.pointer("/detail/error").and_then(|v| v.as_str()),
        Some("forbidden")
    );
    assert!(
        invalid_repo
            .pointer("/detail/message")
            .is_some_and(serde_json::Value::is_string)
    );
    assert!(
        forbidden
            .pointer("/detail/message")
            .is_some_and(serde_json::Value::is_string)
    );
}

fn fixture<T>(name: &str) -> T
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(&fixture_text(name)).unwrap()
}

fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join(FIXTURE_ROOT)
            .join(name),
    )
    .unwrap()
}
