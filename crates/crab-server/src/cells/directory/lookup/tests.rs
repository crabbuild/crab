use super::*;
use crate::{
    cells::{
        Address,
        directory::{
            CreateProfile, CreateProfileInput, EditProfile, EditProfileInput, ProfileOutcome,
        },
    },
    test_support::server as fixtures,
};

#[tokio::test(flavor = "multi_thread")]
async fn lookup_preserves_input_order_qualified_identity_and_current_profile_without_writes() {
    let server = fixtures::fixture().await;
    let client = server.cells().unwrap();
    client.initialize(Address::Directory).await.unwrap();
    let mut actors = Vec::new();
    let mut keys = Vec::new();
    for (issuer, handle) in [
        ("https://one.test", "alice"),
        ("https://two.test", "different-alice"),
    ] {
        let identity = IdentityKey {
            issuer: issuer.into(),
            subject: "same-subject".into(),
        };
        let saved = client
            .command::<CreateProfile>(
                Address::Directory,
                crate::app::mutation_identity().unwrap(),
                CreateProfileInput {
                    submission_id: Uuid::now_v7(),
                    identity: identity.clone(),
                    handle: handle.into(),
                    name: "Chosen name".into(),
                },
            )
            .await
            .unwrap();
        let ProfileOutcome::Saved(actor) = saved.output else {
            panic!("profile not created");
        };
        actors.push(actor);
        keys.push(identity);
    }
    // Stored repository identities and SQLite length bounds count characters,
    // so non-ASCII provider subjects must not fail a valid native lookup.
    let unicode = IdentityKey {
        issuer: "https://one.test".into(),
        subject: "🦀".repeat(512),
    };
    let missing_unicode = client
        .query::<LookupActors>(Address::Directory, None, vec![unicode])
        .await
        .unwrap();
    assert_eq!(missing_unicode.output, vec![None]);
    let missing = IdentityKey {
        issuer: "https://one.test".into(),
        subject: "not-enrolled".into(),
    };
    let result = client
        .query::<LookupActors>(
            Address::Directory,
            None,
            vec![keys[1].clone(), missing, keys[0].clone(), keys[1].clone()],
        )
        .await
        .unwrap();
    assert_eq!(
        result.output,
        vec![
            Some(actors[1].clone().into()),
            None,
            Some(actors[0].clone().into()),
            Some(actors[1].clone().into())
        ]
    );
    assert_ne!(actors[0].id, actors[1].id);
    let revision = client
        .query::<GetDirectoryRevision>(Address::Directory, None, ())
        .await
        .unwrap();
    assert_eq!(result.receipt, revision.receipt);
    let edited = client
        .command::<EditProfile>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            EditProfileInput {
                identity: keys[0].clone(),
                version: 1,
                handle: "renamed-alice".into(),
                name: "New name".into(),
            },
        )
        .await
        .unwrap();
    let ProfileOutcome::Saved(edited) = edited.output else {
        panic!("profile not edited");
    };
    let current = client
        .query::<LookupActors>(Address::Directory, None, vec![keys[0].clone()])
        .await
        .unwrap();
    assert_eq!(current.output, vec![Some(edited.clone().into())]);
    assert_eq!(edited.id, actors[0].id);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lookup_bounds_private_inputs_and_requires_directory_preparation() {
    let server = fixtures::fixture().await;
    let client = server.cells().unwrap();
    let identity = IdentityKey {
        issuer: "https://one.test".into(),
        subject: "alice".into(),
    };
    assert!(matches!(
        client
            .query::<LookupActors>(Address::Directory, None, vec![identity.clone()])
            .await,
        Err(Error::NotInitialized)
    ));
    client.initialize(Address::Directory).await.unwrap();
    for input in [
        vec![identity; 101],
        vec![IdentityKey {
            issuer: String::new(),
            subject: "alice".into(),
        }],
    ] {
        assert!(matches!(
            client
                .query::<LookupActors>(Address::Directory, None, input)
                .await,
            Err(Error::Invalid(_))
        ));
    }
    fixtures::close(&server).await;
}
