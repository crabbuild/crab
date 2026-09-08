use std::fmt::Debug;

use crab_storage::provider_store::{AzureAuthorization, ObjectStoreCredentials};

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
fn object_store_credentials_redact_each_provider_secret() {
    for credentials in [
        ObjectStoreCredentials::Aws {
            access_key_id: SECRET.into(),
            secret_access_key: SECRET.into(),
            session_token: Some(SECRET.into()),
            region: "region".into(),
        },
        ObjectStoreCredentials::Gcp {
            access_token: SECRET.into(),
        },
        ObjectStoreCredentials::Azure {
            account: "account".into(),
            token: AzureAuthorization::Bearer(SECRET.into()),
        },
        ObjectStoreCredentials::Azure {
            account: "account".into(),
            token: AzureAuthorization::Sas(SECRET.into()),
        },
    ] {
        assert_redacted(&credentials);
    }
}

#[test]
fn azure_authorization_redacts_both_authorization_forms() {
    for token in [
        AzureAuthorization::Bearer(SECRET.into()),
        AzureAuthorization::Sas(SECRET.into()),
    ] {
        assert_redacted(&token);
    }
}
