use std::fmt::Debug;
use std::time::SystemTime;

use crab_auth::token_cache::{CachedTokens, TokenIdentity};
use crab_auth::{AzureReadScope, AzureToken, CloudCredentials, CredentialResolution};

const SECRET: &str = "fixture-private-credential";

fn assert_redacted(value: &dyn Debug) {
    for rendered in [format!("{value:?}"), format!("{value:#?}")] {
        assert!(
            !rendered.contains(SECRET),
            "Debug exposed credential material"
        );
    }
}

#[test]
fn resolved_credentials_redact_secrets_including_nested_scopes() {
    let expires_at = SystemTime::UNIX_EPOCH;
    for credentials in [
        CloudCredentials::Aws {
            access_key_id: SECRET.into(),
            secret_access_key: SECRET.into(),
            session_token: Some(SECRET.into()),
            expires_at,
            region: "region".into(),
        },
        CloudCredentials::Gcp {
            access_token: SECRET.into(),
            expires_at,
        },
        CloudCredentials::Azure {
            account: "account".into(),
            token: AzureToken::Bearer(SECRET.into()),
            expires_at,
        },
        CloudCredentials::AzureScoped {
            account: "account".into(),
            read_scopes: vec![AzureReadScope {
                prefix: "read".into(),
                token: AzureToken::Sas(SECRET.into()),
            }],
            write_token: AzureToken::Sas(SECRET.into()),
            write_prefix: "write".into(),
            expires_at,
        },
    ] {
        assert_redacted(&CredentialResolution::new(credentials));
    }
}

#[test]
fn azure_tokens_redact_both_authorization_forms() {
    for token in [
        AzureToken::Bearer(SECRET.into()),
        AzureToken::Sas(SECRET.into()),
    ] {
        assert_redacted(&token);
    }
}

#[test]
fn cached_tokens_redact_all_tokens() {
    assert_redacted(&CachedTokens {
        id_token: SECRET.into(),
        access_token: Some(SECRET.into()),
        refresh_token: Some(SECRET.into()),
        identity: TokenIdentity {
            subject: "subject".into(),
            email: None,
            name: None,
        },
        issued_at: 0,
        expires_at: Some(60),
    });
}

#[test]
fn credential_response_redacts_untyped_provider_payload() {
    let response = crab_auth::parse_credential_response(
        &serde_json::json!({
            "provider": "aws",
            "credentials": {"secret_access_key": SECRET},
            "expires_at": "2026-01-01T00:00:00Z",
            "permissions": ["read"]
        })
        .to_string(),
    )
    .unwrap();

    assert_redacted(&response);
}

#[cfg(feature = "oidc-client")]
#[test]
fn oidc_tokens_redact_all_tokens() {
    assert_redacted(&crab_auth::OidcTokens {
        id_token: SECRET.into(),
        access_token: SECRET.into(),
        refresh_token: Some(SECRET.into()),
        expires_in: 60,
        token_type: "Bearer".into(),
    });
}

#[test]
fn protected_push_response_redacts_provider_credentials() {
    assert_redacted(&crab_auth::PushPrepareResponse {
        provider: "aws".into(),
        credentials: serde_json::json!({"secret_access_key": SECRET}),
        expires_at: "2026-01-01T00:00:00Z".into(),
        permissions: vec!["immutable-write".into()],
        push_id: "push".into(),
        upload_prefix: "repo/staging/push".into(),
    });
}
