use super::{management_body, management_request};
use crab_server::{Config, LifecycleStatus, Result};

pub(super) enum Scope {
    Issue,
    Pull,
    Review,
    Project,
}

pub(super) async fn prepare(
    config: &Config,
    owner: &str,
    name: &str,
    scope: Scope,
) -> Result<Vec<u8>> {
    let owner = url::form_urlencoded::byte_serialize(owner.as_bytes()).collect::<String>();
    let name = url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>();
    let (state, submissions): (&str, &[&str]) = match scope {
        Scope::Issue => ("issue-state", &["issue", "bulk", "pins"]),
        Scope::Pull => ("pull-state", &["pull", "bulk"]),
        Scope::Review => ("review-state", &[]),
        Scope::Project => ("project-state", &[]),
    };
    let path = format!("/internal/repositories/{owner}/{name}/{state}");
    let mut body = frames(config, &path).await?;
    for kind in submissions {
        let path = format!("/internal/repositories/{owner}/{name}/discussion-submissions/{kind}");
        body = frames(config, &path).await?;
    }
    Ok(body)
}
async fn frames(config: &Config, path: &str) -> Result<Vec<u8>> {
    let mut body = management_body(config, path).await?;
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
            path,
            Some(&serde_json::json!({"version":version})),
        )
        .await?
        .bytes()
        .await
        .map_err(|source| crab_server::Error::Healthcheck { source })?
        .to_vec();
    }
}
