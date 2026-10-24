use super::*;
use crate::cells::{Address, CellClient, application::Mutation};
use crate::test_support::server as fixtures;

fn create(subject: &str, handle: &str) -> CreateProfileInput {
    CreateProfileInput {
        submission_id: Uuid::now_v7(),
        identity: IdentityKey {
            issuer: "https://identity.test".into(),
            subject: subject.into(),
        },
        handle: handle.into(),
        name: subject.into(),
    }
}
async fn write<M: Mutation>(client: &CellClient, input: M::Input) -> M::Output {
    client
        .command::<M>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            input,
        )
        .await
        .unwrap()
        .output
}
fn saved(outcome: ProfileOutcome) -> ActorProfile {
    let ProfileOutcome::Saved(profile) = outcome else {
        panic!("profile was not saved: {outcome:?}")
    };
    profile
}
async fn revision(client: &CellClient) -> u64 {
    client
        .query::<GetDirectoryRevision>(Address::Directory, None, ())
        .await
        .unwrap()
        .output
}

#[tokio::test(flavor = "multi_thread")]
async fn qualified_identity_and_unique_handles_do_not_conflate_accounts_or_replay_payloads() {
    let server = fixtures::fixture().await;
    let client = server.cells().unwrap();
    client.initialize(Address::Directory).await.unwrap();
    let input = create("same-subject", "alice");
    let alice = saved(write::<CreateProfile>(&client, input.clone()).await);
    let mut other = input.clone();
    other.submission_id = Uuid::now_v7();
    other.identity.issuer = "https://different-identity.test".into();
    assert_eq!(
        write::<CreateProfile>(&client, other.clone()).await,
        ProfileOutcome::HandleTaken
    );
    other.handle = "other-alice".into();
    let different = saved(write::<CreateProfile>(&client, other).await);
    assert_ne!(alice.id, different.id);
    let mut changed = input.clone();
    changed.name = "Conflicting submission".into();
    assert_eq!(
        write::<CreateProfile>(&client, changed).await,
        ProfileOutcome::RequestConflict
    );
    assert_eq!(saved(write::<CreateProfile>(&client, input).await), alice);
    assert_eq!(revision(&client).await, 2);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_profile_edits_keep_id_and_draft_retries_cannot_overwrite_newer_versions() {
    let server = fixtures::fixture().await;
    let client = server.cells().unwrap();
    client.initialize(Address::Directory).await.unwrap();
    let input = create("alice", "alice");
    let initial = saved(write::<CreateProfile>(&client, input.clone()).await);
    let edit = EditProfileInput {
        identity: input.identity.clone(),
        version: 1,
        handle: "alice-renamed".into(),
        name: "Alice 🦀".into(),
    };
    let renamed = saved(write::<EditProfile>(&client, edit.clone()).await);
    assert_eq!(renamed.id, initial.id);
    assert_eq!(
        saved(write::<EditProfile>(&client, edit.clone()).await),
        renamed
    );
    assert_eq!(revision(&client).await, 2);
    let mut later = edit.clone();
    later.version = 2;
    later.name = "Newer name".into();
    let current = saved(write::<EditProfile>(&client, later).await);
    assert_eq!(
        write::<EditProfile>(&client, edit).await,
        ProfileOutcome::Conflict
    );
    assert_eq!(saved(write::<CreateProfile>(&client, input).await), current);
    assert_eq!(
        client
            .query::<GetActor>(Address::Directory, None, initial.id)
            .await
            .unwrap()
            .output,
        Some(current)
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_handle_claims_have_one_winner_without_duplicate_actors() {
    let server = fixtures::fixture().await;
    let client = server.cells().unwrap();
    client.initialize(Address::Directory).await.unwrap();
    let (alice, bob) = tokio::join!(
        write::<CreateProfile>(&client, create("alice", "shared")),
        write::<CreateProfile>(&client, create("bob", "shared"))
    );
    assert!(matches!(
        (alice, bob),
        (ProfileOutcome::Saved(_), ProfileOutcome::HandleTaken)
            | (ProfileOutcome::HandleTaken, ProfileOutcome::Saved(_))
    ));
    assert_eq!(revision(&client).await, 1);
    fixtures::close(&server).await;
}

#[test]
fn profile_validation_preserves_unicode_names_and_rejects_ambiguous_or_unbounded_handles() {
    assert!(validate_profile("alice-2", "阿丽絲 🦀").is_ok());
    for handle in [
        "",
        "Alice",
        "-alice",
        "alice-",
        "alice--bob",
        "é",
        "alice/bob",
    ] {
        assert!(validate_profile(handle, "Alice").is_err(), "{handle:?}");
    }
    for name in ["", " Alice", "Alice\n"] {
        assert!(validate_profile("alice", name).is_err(), "{name:?}");
    }
    assert!(validate_profile(&"a".repeat(40), "Alice").is_err());
    assert!(validate_profile("alice", &"🦀".repeat(101)).is_err());
}
