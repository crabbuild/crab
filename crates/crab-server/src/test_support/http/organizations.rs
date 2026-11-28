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
    let handle = format!("crab-eng-{}", &id.simple().to_string()[..8]);
    cells
        .command::<crate::cells::directory::organizations::ClaimOrganizationHandle>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            crate::cells::directory::organizations::HandleClaimInput {
                organization: id,
                handle: handle.clone(),
            },
        )
        .await
        .unwrap();
    cells
        .command::<Bootstrap>(
            Address::Organization(id),
            app::mutation_identity().unwrap(),
            BootstrapInput {
                actor: actors[0],
                handle,
                name: "Crab engineering".into(),
                description: "Organization authority".into(),
                website: String::new(),
                email: String::new(),
                location: String::new(),
                avatar: None,
            },
        )
        .await
        .unwrap();
    (id, actors[0], actors[1])
}

pub(crate) async fn bootstrap_local(h: &Harness, handle: &str) -> (Uuid, Uuid) {
    let cells = h.server.cells().unwrap();
    let local = crate::auth::Principal::local().identity().unwrap();
    let actors = cells
        .command::<RegisterAccounts>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            vec![IdentityKey {
                issuer: local.issuer,
                subject: local.subject,
            }],
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
        .command::<crate::cells::directory::organizations::ClaimOrganizationHandle>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            crate::cells::directory::organizations::HandleClaimInput {
                organization: id,
                handle: handle.to_owned(),
            },
        )
        .await
        .unwrap();
    cells
        .command::<Bootstrap>(
            Address::Organization(id),
            app::mutation_identity().unwrap(),
            BootstrapInput {
                actor: actors[0],
                handle: handle.to_owned(),
                name: "Local organization".into(),
                description: String::new(),
                website: String::new(),
                email: String::new(),
                location: String::new(),
                avatar: None,
            },
        )
        .await
        .unwrap();
    (id, actors[0])
}
