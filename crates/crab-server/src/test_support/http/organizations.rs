use super::Harness;
use crate::{
    app,
    cells::{
        Address,
        directory::{IdentityKey, RegisterAccounts},
        organization::{Bootstrap, BootstrapInput},
    },
};
use uuid::Uuid;

pub(crate) async fn bootstrap(h: &Harness) -> (Uuid, Uuid, Uuid) {
    let cells = h.server.cells().unwrap();
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
