//! Shared protected-push protocol contracts.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::credential_response::credentials_from_response;
use crate::credentials::CloudCredentials;
use crate::error::{AuthError, Result};
use crab_coordination::write_coordinator::{CommitOutcome, PushTransactionState};

const PUSH_ID_HEX_LEN: usize = 32;
const ZERO_OID: &str = "0000000000000000000000000000000000000000";

/// One protected-push ref update authorized by Crab Auth.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PushRefUpdate {
    pub ref_name: String,
    pub old_oid: Option<String>,
    pub new_oid: String,
}

/// Response returned when Crab Auth prepares a protected push.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushPrepareResponse {
    pub provider: String,
    pub credentials: serde_json::Value,
    pub expires_at: String,
    pub permissions: Vec<String>,
    pub push_id: String,
    pub upload_prefix: String,
}

impl PushPrepareResponse {
    /// Converts provider-specific credential JSON into cloud credentials.
    pub fn cloud_credentials(&self, expires_at: SystemTime) -> Result<CloudCredentials> {
        credentials_from_response(&self.provider, &self.credentials, expires_at)
    }
}

/// Response returned after a protected push is finalized by Crab Auth.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PushFinalizeResponse {
    pub status: String,
    pub ref_updates: Vec<PushRefUpdate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator_epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writer_region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub commit_state: Option<PushTransactionState>,
}

impl PushFinalizeResponse {
    /// Builds the standard successful finalize response without coordinator metadata.
    #[must_use]
    pub fn updated(ref_updates: Vec<PushRefUpdate>) -> Self {
        Self {
            status: "updated".to_owned(),
            ref_updates,
            operation_id: None,
            coordinator_epoch: None,
            writer_region: None,
            manifest_generation: None,
            commit_state: None,
        }
    }

    /// Builds the standard successful finalize response from an optional coordinator outcome.
    #[must_use]
    pub fn updated_with_commit_outcome(
        ref_updates: Vec<PushRefUpdate>,
        outcome: Option<&CommitOutcome>,
    ) -> Self {
        let Some(outcome) = outcome else {
            return Self::updated(ref_updates);
        };

        Self {
            status: "updated".to_owned(),
            ref_updates,
            operation_id: Some(outcome.operation_id.clone()),
            coordinator_epoch: Some(outcome.coordinator_epoch),
            writer_region: Some(outcome.region.clone()),
            manifest_generation: Some(outcome.manifest_generation),
            commit_state: Some(outcome.state),
        }
    }
}

/// Validates a protected-push prepare response as a shared auth protocol contract.
pub fn validate_push_prepare_response(
    repo_prefix: &str,
    response: &PushPrepareResponse,
) -> Result<()> {
    validate_push_prepare_permissions(&response.permissions)?;
    validate_push_prepare_scope(repo_prefix, &response.push_id, &response.upload_prefix)
}

/// Validates protected-push ref updates as a shared auth protocol contract.
///
/// Requires at least one update, unique ref names, branch-only refs, valid
/// SHA-1 object IDs, no deletions, and no no-op mutations.
pub fn validate_push_ref_updates(updates: &[PushRefUpdate]) -> Result<()> {
    if updates.is_empty() {
        return Err(AuthError::InvalidProtectedPushRefUpdates(
            "no ref updates".to_owned(),
        ));
    }

    let mut seen = std::collections::BTreeSet::new();
    for update in updates {
        validate_push_ref_update(update)?;
        if !seen.insert(update.ref_name.as_str()) {
            return Err(AuthError::InvalidProtectedPushRefUpdates(format!(
                "duplicate ref update: {}",
                update.ref_name
            )));
        }
    }

    Ok(())
}

/// Validates a protected-push finalize response as a shared auth protocol contract.
pub fn validate_push_finalize_response(response: &PushFinalizeResponse) -> Result<()> {
    if response.status != "updated" {
        return invalid_finalize_response("unexpected status");
    }
    validate_push_ref_updates(&response.ref_updates)?;

    let metadata_fields = [
        response.operation_id.is_some(),
        response.coordinator_epoch.is_some(),
        response.writer_region.is_some(),
        response.manifest_generation.is_some(),
        response.commit_state.is_some(),
    ];
    if metadata_fields.iter().any(|field| *field) && !metadata_fields.iter().all(|field| *field) {
        return invalid_finalize_response("partial active-active commit metadata");
    }

    if let Some(operation_id) = response.operation_id.as_deref()
        && operation_id.trim().is_empty()
    {
        return invalid_finalize_response("empty active-active operation_id");
    }
    if let Some(writer_region) = response.writer_region.as_deref()
        && writer_region.trim().is_empty()
    {
        return invalid_finalize_response("empty active-active writer_region");
    }

    Ok(())
}

/// Validates one protected-push ref update as a shared auth protocol contract.
pub fn validate_push_ref_update(update: &PushRefUpdate) -> Result<()> {
    if !update.ref_name.starts_with("refs/heads/") {
        return invalid_ref_update("non-branch ref update");
    }

    use bstr::ByteSlice;
    gix_validate::reference::name(update.ref_name.as_bytes().as_bstr())
        .map_err(|_| invalid_ref_update_error("invalid ref_name"))?;

    if let Some(old) = update.old_oid.as_deref() {
        validate_sha1(old, "old_oid")?;
    }
    validate_sha1(&update.new_oid, "new_oid")?;

    if update.new_oid == ZERO_OID {
        return invalid_ref_update("ref deletion is not allowed");
    }

    let new_oid = update.new_oid.to_ascii_lowercase();
    if normalize_optional_oid(update.old_oid.as_deref()).as_deref() == Some(new_oid.as_str()) {
        return invalid_ref_update("no-op ref updates are not allowed");
    }

    Ok(())
}

fn validate_push_prepare_scope(prefix: &str, push_id: &str, upload_prefix: &str) -> Result<()> {
    if !is_valid_push_id(push_id) {
        return invalid_prepare_response("invalid push_id");
    }

    let repo_prefix = prefix.trim_matches('/');
    if repo_prefix.is_empty() {
        return invalid_prepare_response("protected push requires a non-empty repo prefix");
    }

    let expected = format!("{repo_prefix}/staging/{push_id}");
    if upload_prefix.trim_matches('/') != expected {
        return invalid_prepare_response("upload_prefix outside the protected staging prefix");
    }

    Ok(())
}

fn validate_push_prepare_permissions(permissions: &[String]) -> Result<()> {
    let mut has_immutable_write = false;
    for permission in permissions {
        match permission.trim().to_ascii_lowercase().as_str() {
            "read" => return invalid_prepare_response("read permission"),
            "immutable-write" => has_immutable_write = true,
            "write" => return invalid_prepare_response("canonical write permission"),
            other => {
                return invalid_prepare_response(format!("unsupported permission: {other}"));
            }
        }
    }
    if !has_immutable_write {
        return invalid_prepare_response("missing immutable-write permission");
    }
    Ok(())
}

fn is_valid_push_id(push_id: &str) -> bool {
    push_id.len() == PUSH_ID_HEX_LEN
        && push_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Normalizes an optional old object ID from protected-push ref updates.
#[must_use]
pub fn normalize_optional_oid(value: Option<&str>) -> Option<String> {
    let oid = value?.trim();
    if oid.is_empty() || oid == ZERO_OID {
        return None;
    }
    Some(oid.to_ascii_lowercase())
}

fn validate_sha1(value: &str, field: &str) -> Result<()> {
    if value.len() != 40 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        return invalid_ref_update(format!("invalid {field}: must be 40 hex characters"));
    }
    Ok(())
}

fn invalid_ref_update(message: impl Into<String>) -> Result<()> {
    Err(invalid_ref_update_error(message))
}

fn invalid_prepare_response(message: impl Into<String>) -> Result<()> {
    Err(AuthError::InvalidProtectedPushPrepareResponse(
        message.into(),
    ))
}

fn invalid_ref_update_error(message: impl Into<String>) -> AuthError {
    AuthError::InvalidProtectedPushRefUpdate(message.into())
}

fn invalid_finalize_response(message: impl Into<String>) -> Result<()> {
    Err(AuthError::InvalidProtectedPushFinalizeResponse(
        message.into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_ref_update_round_trips() {
        let update = PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: Some("0000000000000000000000000000000000000000".to_owned()),
            new_oid: "1111111111111111111111111111111111111111".to_owned(),
        };

        let json = serde_json::to_string(&update).unwrap();
        let decoded: PushRefUpdate = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, update);
    }

    #[test]
    fn push_ref_update_rejects_unknown_fields() {
        let json = r#"{
            "ref_name": "refs/heads/main",
            "old_oid": null,
            "new_oid": "1111111111111111111111111111111111111111",
            "extra": true
        }"#;

        let err = serde_json::from_str::<PushRefUpdate>(json).unwrap_err();

        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn push_prepare_response_rejects_unknown_fields() {
        let err = serde_json::from_str::<PushPrepareResponse>(
            r#"{
                "provider": "aws",
                "credentials": {
                    "access_key_id": "AKIA",
                    "secret_access_key": "secret",
                    "session_token": "token"
                },
                "expires_at": "2026-04-24T18:00:00Z",
                "permissions": ["immutable-write"],
                "push_id": "0123456789abcdef0123456789abcdef",
                "upload_prefix": "team/repo/staging/0123456789abcdef0123456789abcdef",
                "extra": true
            }"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn validate_push_prepare_response_accepts_exact_staging_prefix() {
        let response = valid_prepare_response();

        validate_push_prepare_response("/team/repo/", &response).unwrap();
    }

    #[test]
    fn validate_push_prepare_response_rejects_invalid_push_id() {
        let mut response = valid_prepare_response();
        response.push_id = "0123456789ABCDEF0123456789abcdef".to_owned();
        response.upload_prefix = "team/repo/staging/0123456789ABCDEF0123456789abcdef".to_owned();

        let err = validate_push_prepare_response("team/repo", &response).unwrap_err();

        assert!(matches!(
            err,
            AuthError::InvalidProtectedPushPrepareResponse(_)
        ));
        assert!(err.to_string().contains("invalid push_id"));
    }

    #[test]
    fn validate_push_prepare_response_rejects_empty_repo_prefix() {
        let mut response = valid_prepare_response();
        response.upload_prefix = "staging/0123456789abcdef0123456789abcdef".to_owned();

        let err = validate_push_prepare_response("", &response).unwrap_err();

        assert!(matches!(
            err,
            AuthError::InvalidProtectedPushPrepareResponse(_)
        ));
        assert!(err.to_string().contains("non-empty repo prefix"));
    }

    #[test]
    fn validate_push_prepare_response_rejects_mismatched_upload_prefix() {
        let mut response = valid_prepare_response();
        response.upload_prefix = "team/other/staging/0123456789abcdef0123456789abcdef".to_owned();

        let err = validate_push_prepare_response("team/repo", &response).unwrap_err();

        assert!(matches!(
            err,
            AuthError::InvalidProtectedPushPrepareResponse(_)
        ));
        assert!(err.to_string().contains("protected staging prefix"));
    }

    #[test]
    fn validate_push_prepare_response_rejects_read_with_immutable_write() {
        let mut response = valid_prepare_response();
        response.permissions = vec!["read".to_owned(), "immutable-write".to_owned()];

        let err = validate_push_prepare_response("team/repo", &response).unwrap_err();

        assert!(err.to_string().contains("read permission"));
    }

    #[test]
    fn validate_push_prepare_response_accepts_staging_only_immutable_write() {
        let mut response = valid_prepare_response();
        response.permissions = vec!["immutable-write".to_owned()];

        validate_push_prepare_response("team/repo", &response).unwrap();
    }

    #[test]
    fn validate_push_prepare_response_rejects_canonical_write() {
        let mut response = valid_prepare_response();
        response.permissions = vec!["write".to_owned(), "immutable-write".to_owned()];

        let err = validate_push_prepare_response("team/repo", &response).unwrap_err();

        assert!(matches!(
            err,
            AuthError::InvalidProtectedPushPrepareResponse(_)
        ));
        assert!(err.to_string().contains("canonical write"));
    }

    #[test]
    fn validate_push_prepare_response_requires_immutable_write() {
        let mut response = valid_prepare_response();
        response.permissions = Vec::new();

        let err = validate_push_prepare_response("team/repo", &response).unwrap_err();

        assert!(matches!(
            err,
            AuthError::InvalidProtectedPushPrepareResponse(_)
        ));
        assert!(err.to_string().contains("immutable-write"));
    }

    #[test]
    fn validate_push_prepare_response_rejects_unknown_permission() {
        let mut response = valid_prepare_response();
        response.permissions = vec!["immutable-write".to_owned(), "admin".to_owned()];

        let err = validate_push_prepare_response("team/repo", &response).unwrap_err();

        assert!(matches!(
            err,
            AuthError::InvalidProtectedPushPrepareResponse(_)
        ));
        assert!(err.to_string().contains("unsupported permission"));
    }

    #[test]
    fn push_prepare_response_cloud_credentials_uses_provider_payload() {
        let response = valid_prepare_response();
        let expires_at = SystemTime::UNIX_EPOCH;

        let credentials = response.cloud_credentials(expires_at).unwrap();

        match credentials {
            CloudCredentials::Aws {
                access_key_id,
                secret_access_key,
                session_token,
                expires_at: actual_expires_at,
                region,
            } => {
                assert_eq!(access_key_id, "AKIA");
                assert_eq!(secret_access_key, "secret");
                assert_eq!(session_token.as_deref(), Some("token"));
                assert_eq!(actual_expires_at, expires_at);
                assert_eq!(region, "us-east-1");
            }
            other => panic!("expected AWS credentials, got {other:?}"),
        }
    }

    #[test]
    fn push_finalize_response_rejects_unknown_fields() {
        let err = serde_json::from_str::<PushFinalizeResponse>(
            r#"{
                "status": "updated",
                "ref_updates": [{
                    "ref_name": "refs/heads/main",
                    "old_oid": null,
                    "new_oid": "1111111111111111111111111111111111111111"
                }],
                "extra": true
            }"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn validate_push_finalize_response_accepts_updated_branch_ref() {
        let response = valid_finalize_response();

        validate_push_finalize_response(&response).unwrap();
    }

    #[test]
    fn validate_push_finalize_response_accepts_active_active_metadata() {
        let mut response = valid_finalize_response();
        response.operation_id = Some("op-123".to_owned());
        response.coordinator_epoch = Some(7);
        response.writer_region = Some("us-west-2".to_owned());
        response.manifest_generation = Some(42);
        response.commit_state = Some(PushTransactionState::Materialized);

        validate_push_finalize_response(&response).unwrap();
    }

    #[test]
    fn push_finalize_response_updated_has_no_active_active_metadata() {
        let response = PushFinalizeResponse::updated(vec![valid_ref_update()]);

        assert_eq!(response.status, "updated");
        assert_eq!(response.operation_id, None);
        assert_eq!(response.coordinator_epoch, None);
        assert_eq!(response.writer_region, None);
        assert_eq!(response.manifest_generation, None);
        assert_eq!(response.commit_state, None);
        validate_push_finalize_response(&response).unwrap();
    }

    #[test]
    fn push_finalize_response_updated_with_commit_outcome_sets_metadata() {
        let outcome = CommitOutcome {
            operation_id: "op-123".to_owned(),
            coordinator_epoch: 7,
            writer: "writer-a".to_owned(),
            region: "us-west-2".to_owned(),
            manifest_generation: 42,
            state: PushTransactionState::Materialized,
        };

        let response = PushFinalizeResponse::updated_with_commit_outcome(
            vec![valid_ref_update()],
            Some(&outcome),
        );

        assert_eq!(response.operation_id.as_deref(), Some("op-123"));
        assert_eq!(response.coordinator_epoch, Some(7));
        assert_eq!(response.writer_region.as_deref(), Some("us-west-2"));
        assert_eq!(response.manifest_generation, Some(42));
        assert_eq!(
            response.commit_state,
            Some(PushTransactionState::Materialized)
        );
        validate_push_finalize_response(&response).unwrap();
    }

    #[test]
    fn validate_push_finalize_response_rejects_partial_active_active_metadata() {
        let mut response = valid_finalize_response();
        response.operation_id = Some("op-123".to_owned());

        let err = validate_push_finalize_response(&response).unwrap_err();

        assert!(err.to_string().contains("partial active-active"));
    }

    #[test]
    fn validate_push_finalize_response_rejects_unexpected_status() {
        let response = PushFinalizeResponse {
            status: "accepted".to_owned(),
            ..valid_finalize_response()
        };

        let err = validate_push_finalize_response(&response).unwrap_err();

        assert!(err.to_string().contains("unexpected status"));
    }

    #[test]
    fn validate_push_finalize_response_rejects_empty_ref_updates() {
        let mut response = valid_finalize_response();
        response.ref_updates = Vec::new();

        let err = validate_push_finalize_response(&response).unwrap_err();

        assert!(err.to_string().contains("no ref updates"));
    }

    #[test]
    fn validate_push_finalize_response_rejects_duplicate_ref_updates() {
        let mut response = valid_finalize_response();
        response.ref_updates.push(PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: Some(oid('2')),
            new_oid: oid('3'),
        });

        let err = validate_push_finalize_response(&response).unwrap_err();

        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn validate_push_ref_update_accepts_branch_update() {
        let update = PushRefUpdate {
            ref_name: "refs/heads/feature".to_owned(),
            old_oid: Some(oid('1')),
            new_oid: oid('2'),
        };

        validate_push_ref_update(&update).unwrap();
    }

    #[test]
    fn validate_push_ref_update_rejects_invalid_refs() {
        for ref_name in [
            "heads/feature",
            "refs/tags/v1.0",
            "refs/heads/",
            "refs/heads/.hidden",
            "refs/heads/main.lock",
            "refs/heads/main/",
            "refs/heads/main@{1}",
            "refs/heads/main~1",
            "refs/heads/main:other",
            "refs//heads/main",
        ] {
            let update = PushRefUpdate {
                ref_name: ref_name.to_owned(),
                old_oid: Some(oid('1')),
                new_oid: oid('2'),
            };

            assert!(
                validate_push_ref_update(&update).is_err(),
                "expected {ref_name} to be rejected"
            );
        }
    }

    #[test]
    fn validate_push_ref_update_rejects_invalid_oid() {
        let update = PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: Some(oid('1')),
            new_oid: "not-a-sha".to_owned(),
        };

        let err = validate_push_ref_update(&update).unwrap_err();

        assert!(err.to_string().contains("invalid new_oid"));
    }

    #[test]
    fn validate_push_ref_update_rejects_deletion() {
        let update = PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: Some(oid('1')),
            new_oid: ZERO_OID.to_owned(),
        };

        let err = validate_push_ref_update(&update).unwrap_err();

        assert!(err.to_string().contains("ref deletion"));
    }

    #[test]
    fn validate_push_ref_update_rejects_case_insensitive_noop() {
        let update = PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: Some(oid('a').to_ascii_uppercase()),
            new_oid: oid('a'),
        };

        let err = validate_push_ref_update(&update).unwrap_err();

        assert!(err.to_string().contains("no-op ref updates"));
    }

    #[test]
    fn validate_push_ref_updates_rejects_duplicates() {
        let updates = vec![
            PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some(oid('1')),
                new_oid: oid('2'),
            },
            PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some(oid('2')),
                new_oid: oid('3'),
            },
        ];

        let err = validate_push_ref_updates(&updates).unwrap_err();

        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn normalize_optional_oid_treats_empty_and_zero_as_none() {
        assert_eq!(normalize_optional_oid(None), None);
        assert_eq!(normalize_optional_oid(Some("")), None);
        assert_eq!(normalize_optional_oid(Some(ZERO_OID)), None);
        assert_eq!(normalize_optional_oid(Some(&oid('A'))), Some(oid('a')));
    }

    fn oid(ch: char) -> String {
        ch.to_string().repeat(40)
    }

    fn valid_finalize_response() -> PushFinalizeResponse {
        PushFinalizeResponse::updated(vec![valid_ref_update()])
    }

    fn valid_ref_update() -> PushRefUpdate {
        PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: Some(oid('1')),
            new_oid: oid('2'),
        }
    }

    fn valid_prepare_response() -> PushPrepareResponse {
        PushPrepareResponse {
            provider: "aws".to_owned(),
            credentials: serde_json::json!({
                "access_key_id": "AKIA",
                "secret_access_key": "secret",
                "session_token": "token"
            }),
            expires_at: "2026-04-24T18:00:00Z".to_owned(),
            permissions: vec!["immutable-write".to_owned()],
            push_id: "0123456789abcdef0123456789abcdef".to_owned(),
            upload_prefix: "team/repo/staging/0123456789abcdef0123456789abcdef".to_owned(),
        }
    }
}
