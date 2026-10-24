use super::{management_body, management_request};
use crab_server::{Config, OrganizationTeamState, Result};
use uuid::Uuid;

pub(super) async fn prepare(config: &Config, organization: Uuid, actor: Uuid) -> Result<Vec<u8>> {
    let path = format!("/internal/organizations/{organization}/teams");
    let mut body = management_body(config, &format!("{path}?actor={actor}")).await?;
    loop {
        let status: OrganizationTeamState = serde_json::from_slice(&body)?;
        let step = match status {
            OrganizationTeamState::Ready => return Ok(body),
            OrganizationTeamState::Missing => serde_json::json!({"action":"begin"}),
            OrganizationTeamState::Building {
                processed, version, ..
            } => {
                eprintln!("Prepared {processed} organization members, frame {version}");
                serde_json::json!({"action":"step","version":version})
            }
        };
        body = management_request(
            config,
            reqwest::Method::POST,
            &path,
            Some(&serde_json::json!({"actor":actor,"step":step})),
        )
        .await?
        .bytes()
        .await
        .map_err(|source| crab_server::Error::Healthcheck { source })?
        .to_vec();
    }
}
