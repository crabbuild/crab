use super::*;
use crate::{cells::Address, test_support::server as fixtures};

#[tokio::test(flavor = "multi_thread")]
async fn indexed_handle_completion_and_private_resolution_follow_renames_without_name_matching() {
    let server = fixtures::fixture().await;
    let client = server.cells().unwrap();
    client.initialize(Address::Directory).await.unwrap();
    for index in 0..24 {
        client
            .command::<CreateProfile>(
                Address::Directory,
                crate::app::mutation_identity().unwrap(),
                CreateProfileInput {
                    submission_id: Uuid::now_v7(),
                    identity: IdentityKey {
                        issuer: "https://one.test".into(),
                        subject: format!("subject-{index}"),
                    },
                    handle: format!("alice-{index:02}"),
                    name: "Same public name".into(),
                },
            )
            .await
            .unwrap();
        if index == 19 {
            let exact_limit = client
                .query::<CompleteHandles>(Address::Directory, None, "alice".into())
                .await
                .unwrap()
                .output;
            assert_eq!(
                (exact_limit.items.len(), exact_limit.truncated),
                (20, false)
            );
        }
    }
    let first = client
        .query::<CompleteHandles>(Address::Directory, None, "alice".into())
        .await
        .unwrap();
    assert_eq!(first.output.items.len(), 20);
    assert!(first.output.truncated);
    let exact = client
        .query::<CompleteHandles>(Address::Directory, None, "alice-23".into())
        .await
        .unwrap();
    assert_eq!(exact.output.items.len(), 1);
    assert!(!exact.output.truncated);
    let resolved = client
        .query::<ResolveHandles>(
            Address::Directory,
            None,
            vec!["alice-23".into(), "unknown".into(), "alice-00".into()],
        )
        .await
        .unwrap();
    assert_eq!(resolved.output[0].as_ref().unwrap().subject, "subject-23");
    assert!(resolved.output[1].is_none());
    assert_eq!(resolved.output[2].as_ref().unwrap().subject, "subject-0");
    let identity = resolved.output[0].clone().unwrap();
    client
        .command::<EditProfile>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            EditProfileInput {
                identity,
                version: 1,
                handle: "renamed".into(),
                name: "Same public name".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        client
            .query::<CompleteHandles>(Address::Directory, None, "alice-23".into())
            .await
            .unwrap()
            .output
            .items
            .is_empty()
    );
    let renamed = client
        .query::<ResolveHandles>(
            Address::Directory,
            None,
            vec!["renamed".into(), "alice-23".into()],
        )
        .await
        .unwrap();
    assert_eq!(renamed.output[0], resolved.output[0]);
    assert!(renamed.output[1].is_none());
    assert!(
        client
            .query::<CompleteHandles>(Address::Directory, None, "same public".into())
            .await
            .is_err()
    );
    assert!(
        client
            .query::<ResolveHandles>(Address::Directory, None, vec!["alice-00".into(); 65])
            .await
            .is_err()
    );
    fixtures::close(&server).await;
}
