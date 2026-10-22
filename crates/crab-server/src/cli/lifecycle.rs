use super::{management_body, management_request};
use crab_server::{Config, LifecycleStatus, Result};

pub(super) async fn prepare(
    config: &Config,
    owner: &str,
    name: &str,
    state: &str,
) -> Result<Vec<u8>> {
    let owner = url::form_urlencoded::byte_serialize(owner.as_bytes()).collect::<String>();
    let name = url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>();
    let path = format!("/internal/repositories/{owner}/{name}/{state}");
    let mut body = management_body(config, &path).await?;
    loop {
        let status: LifecycleStatus = serde_json::from_slice(&body)?;
        let version = match status {
            LifecycleStatus::Ready => return Ok(body),
            LifecycleStatus::Outdated => 0,
            LifecycleStatus::Migrating { version, copied } => {
                eprintln!("Prepared {copied} records, frame {version}");
                version
            }
        };
        body = management_request(
            config,
            reqwest::Method::POST,
            &path,
            Some(&serde_json::json!({"version":version})),
        )
        .await?
        .bytes()
        .await
        .map_err(|source| crab_server::Error::Healthcheck { source })?
        .to_vec();
    }
}
