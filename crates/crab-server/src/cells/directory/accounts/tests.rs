use super::*;
use crate::{cells::Address, test_support::server as fixtures};

#[tokio::test(flavor = "multi_thread")]
async fn private_account_registration_is_qualified_idempotent_and_keeps_profile_identity() {
    let server = fixtures::fixture().await;
    let client = server.cells().unwrap();
    client.initialize(Address::Directory).await.unwrap();
    let first = IdentityKey {
        issuer: "https://one.test".into(),
        subject: "same-subject".into(),
    };
    let second = IdentityKey {
        issuer: "https://two.test".into(),
        subject: first.subject.clone(),
    };
    let input = vec![first.clone(), second, first.clone()];
    let registered = client
        .command::<RegisterAccounts>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            input.clone(),
        )
        .await
        .unwrap();
    assert_eq!(registered.output[0], registered.output[2]);
    assert_ne!(registered.output[0], registered.output[1]);
    assert!(
        client
            .query::<GetOwnProfile>(Address::Directory, None, first.clone())
            .await
            .unwrap()
            .output
            .is_none()
    );
    let repeated = client
        .command::<RegisterAccounts>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            input,
        )
        .await
        .unwrap();
    assert_eq!(repeated.output, registered.output);
    assert_eq!(repeated.receipt.revision, registered.receipt.revision);
    let created = client
        .command::<CreateProfile>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            CreateProfileInput {
                submission_id: Uuid::now_v7(),
                identity: first,
                handle: "registered".into(),
                name: "Registered account".into(),
            },
        )
        .await
        .unwrap();
    let ProfileOutcome::Saved(profile) = created.output else {
        panic!("profile not saved")
    };
    assert_eq!(profile.id, registered.output[0]);
    fixtures::close(&server).await;
}
