use crab_ltx::CrabError;
use crab_storage::{ObjectStoreCredentials, Store};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct RustfsTarget {
    pub store: Store,
    pub repository_prefix: String,
}

pub fn rustfs_target(workload: &str) -> crab_ltx::Result<RustfsTarget> {
    let bucket = required_environment("CRAB_LTX_TEST_BUCKET")?;
    let endpoint = required_environment("CRAB_LTX_TEST_ENDPOINT")?;
    let access_key_id = required_environment("AWS_ACCESS_KEY_ID")?;
    let secret_access_key = required_environment("AWS_SECRET_ACCESS_KEY")?;
    let allow_http = match endpoint.split_once("://").map(|(scheme, _)| scheme) {
        Some("http") => true,
        Some("https") => false,
        _ => {
            return Err(CrabError::InvalidState(
                "RustFS endpoint must use http or https",
            ));
        }
    };
    let store = crab_storage::build_explicit_store(
        &bucket,
        ObjectStoreCredentials::Aws {
            access_key_id,
            secret_access_key,
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&endpoint),
        allow_http,
    )?;
    let run = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CrabError::InvalidState("system clock is before the Unix epoch"))?
        .as_millis();
    Ok(RustfsTarget {
        store,
        repository_prefix: format!("crab-ltx-examples/{workload}/{run}-{}", std::process::id()),
    })
}

fn required_environment(name: &str) -> crab_ltx::Result<String> {
    std::env::var(name).map_err(|_| CrabError::InvalidState("missing RustFS test environment"))
}
