use super::preparation::Step;
use super::*;
use crate::cells::organization::{
    Bootstrap, BootstrapInput, ChangeMember, ChangeOutcome, DecideInvitation, Decision,
    GetOrganization, Invite, InviteInput, MemberChange,
};
use crate::cells::{Address, CellClient};
use crate::test_support::server as fixtures;

struct Fixture {
    server: std::sync::Arc<crate::server::Server>,
    organization: Uuid,
    owner: Uuid,
    bob: Uuid,
}
async fn write<M: Mutation>(client: &CellClient, id: Uuid, input: M::Input) -> M::Output {
    client
        .command::<M>(
            Address::Organization(id),
            crate::app::mutation_identity().unwrap(),
            input,
        )
        .await
        .unwrap()
        .output
}
async fn read<R: Read>(client: &CellClient, id: Uuid, input: R::Input) -> R::Output {
    client
        .query::<R>(Address::Organization(id), None, input)
        .await
        .unwrap()
        .output
}
async fn join(client: &CellClient, id: Uuid, owner: Uuid, person: Uuid, version: u64) {
    let ChangeOutcome::Applied(offer) = write::<Invite>(
        client,
        id,
        InviteInput {
            actor: owner,
            recipient: person,
            role: OrganizationRole::Member,
            version,
        },
    )
    .await
    else {
        panic!("invitation failed")
    };
    assert!(matches!(
        write::<DecideInvitation>(
            client,
            id,
            Decision {
                actor: person,
                version: offer.version,
                accept: true
            }
        )
        .await,
        ChangeOutcome::Applied(_)
    ));
}
async fn fixture() -> Fixture {
    let server = fixtures::fixture().await;
    let client = &server.cells().unwrap();
    let organization = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let bob = Uuid::now_v7();
    client
        .initialize(Address::Organization(organization))
        .await
        .unwrap();
    write::<Bootstrap>(
        client,
        organization,
        BootstrapInput {
            actor: owner,
            name: "Engineering".into(),
            description: String::new(),
        },
    )
    .await;
    join(client, organization, owner, bob, 0).await;
    Fixture {
        server,
        organization,
        owner,
        bob,
    }
}
fn request(actor: Uuid, command: Command) -> Request {
    Request {
        actor,
        request_id: Uuid::now_v7(),
        command,
    }
}
async fn create(f: &Fixture, person: Uuid, name: &str, visibility: Visibility) -> Uuid {
    let input = request(
        person,
        Command::Create {
            name: name.into(),
            description: String::new(),
            visibility,
        },
    );
    let id = input.request_id;
    assert_eq!(
        write::<Change>(&f.server.cells().unwrap(), f.organization, input).await,
        Outcome::Applied { id }
    );
    id
}
async fn own(f: &Fixture, actor: Uuid, id: Uuid) -> View {
    read::<Get>(
        &f.server.cells().unwrap(),
        f.organization,
        Key { actor, team: id },
    )
    .await
    .unwrap()
}
async fn change_member(
    f: &Fixture,
    who: Uuid,
    id: Uuid,
    person: Uuid,
    version: u64,
    role: Option<Role>,
) -> Request {
    let input = request(
        who,
        Command::Member {
            team: id,
            member: person,
            version,
            role,
        },
    );
    assert_eq!(
        write::<Change>(&f.server.cells().unwrap(), f.organization, input.clone()).await,
        Outcome::Applied { id }
    );
    input
}
async fn listed(f: &Fixture, actor: Uuid, search: &str) -> reads::Listing {
    read::<List>(
        &f.server.cells().unwrap(),
        f.organization,
        ListInput {
            actor,
            search: search.into(),
            after: None,
            limit: 50,
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_visibility_maintainer_roles_and_creation_policy_are_native_authority() {
    let f = fixture().await;
    let client = &f.server.cells().unwrap();
    let id = create(&f, f.bob, "Reviewers", Visibility::Secret).await;
    assert_eq!(
        (
            own(&f, f.bob, id).await.role,
            own(&f, f.owner, id).await.can_manage
        ),
        (Some(Role::Maintainer), true)
    );
    let carol = Uuid::now_v7();
    join(client, f.organization, f.owner, carol, 0).await;
    assert!(matches!(
        client
            .query::<Get>(
                Address::Organization(f.organization),
                None,
                Key {
                    actor: carol,
                    team: id
                }
            )
            .await
            .err()
            .unwrap(),
        Error::AccessDenied
    ));
    let reads::Page::Page { items, .. } = listed(&f, carol, "").await.page else {
        panic!("changed page")
    };
    assert!(items.is_empty());
    let edit = request(
        f.bob,
        Command::Edit {
            team: id,
            version: 1,
            name: "Reviewers".into(),
            description: "Review code".into(),
            visibility: Visibility::Closed,
        },
    );
    write::<Change>(client, f.organization, edit).await;
    assert!(!own(&f, carol, id).await.can_manage);
    change_member(&f, f.bob, id, carol, 0, Some(Role::Member)).await;
    let denied = request(
        carol,
        Command::Delete {
            team: id,
            version: 3,
        },
    );
    assert!(matches!(
        client
            .command::<Change>(
                Address::Organization(f.organization),
                crate::app::mutation_identity().unwrap(),
                denied
            )
            .await
            .err()
            .unwrap(),
        Error::AccessDenied
    ));
    change_member(&f, f.bob, id, carol, 1, Some(Role::Maintainer)).await;
    assert!(own(&f, carol, id).await.can_manage);
    write::<Change>(
        client,
        f.organization,
        request(
            f.owner,
            Command::Policy {
                version: 1,
                member_creation: false,
            },
        ),
    )
    .await;
    let denied = request(
        f.bob,
        Command::Create {
            name: "Another".into(),
            description: String::new(),
            visibility: Visibility::Closed,
        },
    );
    assert!(matches!(
        client
            .command::<Change>(
                Address::Organization(f.organization),
                crate::app::mutation_identity().unwrap(),
                denied
            )
            .await
            .err()
            .unwrap(),
        Error::AccessDenied
    ));
    assert_eq!(
        read::<GetPolicy>(client, f.organization, Caller { actor: f.owner }).await,
        Policy {
            member_creation: false,
            version: 2
        }
    );
    create(&f, f.owner, "Owner team", Visibility::Secret).await;
    fixtures::close(&f.server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn organization_rejoining_does_not_restore_team_grants_and_own_acknowledgements_recover() {
    let f = fixture().await;
    let client = &f.server.cells().unwrap();
    let id = create(&f, f.owner, "Private reviewers", Visibility::Secret).await;
    change_member(&f, f.owner, id, f.bob, 0, Some(Role::Maintainer)).await;
    let original = request(
        f.bob,
        Command::Edit {
            team: id,
            version: 2,
            name: "Private reviewers".into(),
            description: "Changed".into(),
            visibility: Visibility::Secret,
        },
    );
    let transport = crate::app::mutation_identity().unwrap();
    client
        .command::<Change>(
            Address::Organization(f.organization),
            transport,
            original.clone(),
        )
        .await
        .unwrap();
    write::<ChangeMember>(
        client,
        f.organization,
        MemberChange {
            actor: f.owner,
            member: f.bob,
            version: 1,
            role: None,
        },
    )
    .await;
    assert!(matches!(
        client
            .query::<Get>(
                Address::Organization(f.organization),
                None,
                Key {
                    actor: f.bob,
                    team: id
                }
            )
            .await
            .err()
            .unwrap(),
        Error::AccessDenied
    ));
    let expected = Outcome::Applied { id };
    assert_eq!(
        client
            .command::<Change>(
                Address::Organization(f.organization),
                transport,
                original.clone()
            )
            .await
            .unwrap()
            .output,
        expected
    );
    assert_eq!(
        write::<Change>(client, f.organization, original).await,
        expected
    );
    join(client, f.organization, f.owner, f.bob, 2).await;
    assert!(matches!(
        client
            .query::<Get>(
                Address::Organization(f.organization),
                None,
                Key {
                    actor: f.bob,
                    team: id
                }
            )
            .await
            .err()
            .unwrap(),
        Error::AccessDenied
    ));
    assert_eq!(
        read::<GetMembership>(
            client,
            f.organization,
            MemberKey {
                actor: f.owner,
                team: id,
                member: f.bob
            }
        )
        .await
        .role,
        None
    );
    change_member(&f, f.owner, id, f.bob, 1, Some(Role::Member)).await;
    write::<ChangeMember>(
        client,
        f.organization,
        MemberChange {
            actor: f.owner,
            member: f.bob,
            version: 3,
            role: Some(OrganizationRole::Owner),
        },
    )
    .await;
    assert!(own(&f, f.bob, id).await.can_manage);
    write::<ChangeMember>(
        client,
        f.organization,
        MemberChange {
            actor: f.owner,
            member: f.bob,
            version: 4,
            role: Some(OrganizationRole::Member),
        },
    )
    .await;
    let current = own(&f, f.bob, id).await;
    assert_eq!(
        (current.role, current.can_manage),
        (Some(Role::Member), false)
    );
    fixtures::close(&f.server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn permanent_creation_and_deletion_do_not_duplicate_or_rebind_name_reuse() {
    let f = fixture().await;
    let client = &f.server.cells().unwrap();
    let original = request(
        f.bob,
        Command::Create {
            name: "Platform".into(),
            description: String::new(),
            visibility: Visibility::Secret,
        },
    );
    let id = original.request_id;
    write::<Change>(client, f.organization, original.clone()).await;
    let duplicate = request(
        f.owner,
        Command::Create {
            name: "PLATFORM".into(),
            description: String::new(),
            visibility: Visibility::Closed,
        },
    );
    assert_eq!(
        write::<Change>(client, f.organization, duplicate).await,
        Outcome::NameTaken
    );
    write::<Change>(
        client,
        f.organization,
        request(
            f.bob,
            Command::Edit {
                team: id,
                version: 1,
                name: "Renamed".into(),
                description: String::new(),
                visibility: Visibility::Secret,
            },
        ),
    )
    .await;
    assert_eq!(
        write::<Change>(client, f.organization, original.clone()).await,
        Outcome::Applied { id }
    );
    let deletion = request(
        f.bob,
        Command::Delete {
            team: id,
            version: 2,
        },
    );
    write::<Change>(client, f.organization, deletion.clone()).await;
    let replacement = create(&f, f.owner, "Platform", Visibility::Secret).await;
    assert_ne!(id, replacement);
    assert_eq!(
        write::<Change>(client, f.organization, deletion).await,
        Outcome::Applied { id }
    );
    assert_eq!(
        write::<Change>(client, f.organization, original.clone()).await,
        Outcome::Applied { id }
    );
    assert!(matches!(
        client
            .query::<Get>(
                Address::Organization(f.organization),
                None,
                Key {
                    actor: f.bob,
                    team: replacement
                }
            )
            .await
            .err()
            .unwrap(),
        Error::AccessDenied
    ));
    let changed = Request {
        command: Command::Create {
            name: "Different".into(),
            description: String::new(),
            visibility: Visibility::Secret,
        },
        ..original
    };
    assert_eq!(
        write::<Change>(client, f.organization, changed).await,
        Outcome::SubmissionConflict
    );
    fixtures::close(&f.server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn visible_search_and_scoped_cursors_cover_all_native_pages() {
    let f = fixture().await;
    let client = &f.server.cells().unwrap();
    let alpha = create(&f, f.owner, "Alpha", Visibility::Closed).await;
    create(&f, f.owner, "Beta secret", Visibility::Secret).await;
    create(&f, f.owner, "Zulu", Visibility::Closed).await;
    let listing = read::<List>(
        client,
        f.organization,
        ListInput {
            actor: f.bob,
            search: String::new(),
            after: None,
            limit: 1,
        },
    )
    .await;
    let reads::Page::Page { items, next, .. } = listing.page else {
        panic!("changed page")
    };
    assert_eq!(items[0].view.team.id, alpha);
    let next = next.unwrap();
    let reads::Page::Page { items, .. } = read::<List>(
        client,
        f.organization,
        ListInput {
            actor: f.bob,
            search: String::new(),
            after: Some(next.clone()),
            limit: 1,
        },
    )
    .await
    .page
    else {
        panic!("changed page")
    };
    assert_eq!(items[0].view.team.name, "Zulu");
    let listing = listed(&f, f.owner, "BETA").await;
    let reads::Page::Page { items, .. } = listing.page else {
        panic!("changed page")
    };
    assert_eq!(items.len(), 1);
    change_member(&f, f.owner, alpha, f.bob, 0, Some(Role::Member)).await;
    assert!(matches!(
        read::<List>(
            client,
            f.organization,
            ListInput {
                actor: f.bob,
                search: String::new(),
                after: Some(next),
                limit: 1
            }
        )
        .await
        .page,
        reads::Page::Changed
    ));
    let page = read::<Members>(
        client,
        f.organization,
        MemberInput {
            actor: f.owner,
            team: alpha,
            after: None,
            limit: 1,
        },
    )
    .await;
    let reads::Page::Page { next, .. } = page else {
        panic!("changed page")
    };
    assert!(matches!(
        read::<Candidates>(
            client,
            f.organization,
            MemberInput {
                actor: f.owner,
                team: alpha,
                after: next,
                limit: 1
            }
        )
        .await,
        reads::Page::Changed
    ));
    fixtures::close(&f.server).await;
}

mod cancellation;
mod preparation;
pub(in crate::cells::organization) use preparation::{FailedStep, WithoutTeams};
