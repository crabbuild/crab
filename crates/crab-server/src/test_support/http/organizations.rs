use super::Harness;
use crate::{
    app,
    cells::{
        Address,
        directory::organizations::{Candidate, RecordCandidate},
        directory::{IdentityKey, RegisterAccounts},
        organization::{Bootstrap, BootstrapInput},
    },
};
use uuid::Uuid;

pub(crate) async fn set_private_forks(
    h: &Harness,
    organization: Uuid,
    cookie: &str,
    allowed: bool,
) {
    let path = format!("/api/organizations/{organization}/repository-policy");
    let mut policy = h.json(&path, cookie).await;
    policy["private_forks"] = serde_json::json!(allowed);
    h.write(reqwest::Method::PUT, &path, cookie, policy)
        .await
        .error_for_status()
        .unwrap();
}

pub(crate) async fn bootstrap(h: &Harness) -> (Uuid, Uuid, Uuid) {
    let cells = h.server.cells().unwrap();
    bootstrap_on(h, &cells).await
}

pub(crate) async fn bootstrap_on(
    h: &Harness,
    cells: &crate::cells::CellClient,
) -> (Uuid, Uuid, Uuid) {
    let issuer = h.provider.issuer.clone();
    let actors = cells
        .command::<RegisterAccounts>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            vec![
                IdentityKey {
                    issuer: issuer.clone(),
                    subject: "alice-id".into(),
                },
                IdentityKey {
                    issuer,
                    subject: "bob-id".into(),
                },
            ],
        )
        .await
        .unwrap()
        .output;
    let id = Uuid::now_v7();
    cells
        .command::<RecordCandidate>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            Candidate {
                actor: actors[0],
                organization: id,
            },
        )
        .await
        .unwrap();
    cells.initialize(Address::Organization(id)).await.unwrap();
    cells
        .command::<Bootstrap>(
            Address::Organization(id),
            app::mutation_identity().unwrap(),
            BootstrapInput {
                actor: actors[0],
                name: "Crab engineering".into(),
                description: "Organization authority".into(),
            },
        )
        .await
        .unwrap();
    (id, actors[0], actors[1])
}
