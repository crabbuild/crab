use super::{commands::*, reads::*, *};
use crate::cells::{Address, CellClient, application::MutationIdentity, runtime};
use cellar::sdk::{NodeOutcome, ShutdownMode};

async fn stop(node: cellar::sdk::Node) {
    let NodeOutcome::Stopped(report) = node.shutdown(ShutdownMode::Preserve).await.unwrap() else {
        panic!("native node did not stop");
    };
    assert!(
        report.cells_drained
            && report.ingress_drained
            && report.application_work_drained
            && !report.drain_timed_out
    );
}
fn identity() -> MutationIdentity {
    crate::app::mutation_identity().unwrap()
}
async fn write<C: Mutation>(client: &CellClient, id: Uuid, input: C::Input) -> C::Output {
    client
        .command::<C>(Address::Organization(id), identity(), input)
        .await
        .unwrap()
        .output
}
async fn bootstrap(client: &CellClient, id: Uuid, owner: Uuid) -> Organization {
    client.initialize(Address::Organization(id)).await.unwrap();
    let ChangeOutcome::Applied(record) = write::<Bootstrap>(
        client,
        id,
        BootstrapInput {
            actor: owner,
            name: "Native organization".into(),
            description: "Private authority".into(),
        },
    )
    .await
    else {
        panic!("bootstrap failed");
    };
    record
}
async fn join(
    client: &CellClient,
    id: Uuid,
    owner: Uuid,
    person: Uuid,
    role: Role,
    invitation_version: u64,
) {
    let ChangeOutcome::Applied(offer) = write::<Invite>(
        client,
        id,
        InviteInput {
            actor: owner,
            recipient: person,
            version: invitation_version,
            role,
        },
    )
    .await
    else {
        panic!("invitation failed");
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
async fn list(client: &CellClient, id: Uuid, actor: Uuid) -> Vec<Member> {
    let Page::Page { items, .. } = client
        .query::<ListMembers>(
            Address::Organization(id),
            None,
            PageQuery {
                actor,
                after: None,
                limit: 50,
            },
        )
        .await
        .unwrap()
        .output
    else {
        panic!("page changed");
    };
    items
}
#[tokio::test(flavor = "multi_thread")]
async fn native_class_and_receipts_cannot_be_retargeted_and_initialization_grants_nothing() {
    let local = tempfile::tempdir().unwrap();
    let started = runtime::test_node(local.path(), "127.0.0.1:8788".parse().unwrap()).await;
    let client = &started.client;
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    assert!(matches!(
        client
            .query::<GetOrganization>(Address::Organization(id), None, Caller { actor: owner })
            .await,
        Err(Error::NotInitialized)
    ));
    client.initialize(Address::Organization(id)).await.unwrap();
    assert!(matches!(
        client
            .query::<GetOrganization>(Address::Organization(id), None, Caller { actor: owner })
            .await,
        Err(Error::AccessDenied)
    ));
    let org = bootstrap(client, id, owner).await;
    assert_eq!(org.id, id);
    client.initialize(Address::Repository(id)).await.unwrap();
    assert!(matches!(
        client
            .query::<GetOrganization>(Address::Repository(id), None, Caller { actor: owner })
            .await,
        Err(Error::Invalid(_))
    ));
    let read = client
        .query::<GetOrganization>(Address::Organization(id), None, Caller { actor: owner })
        .await
        .unwrap();
    assert!(matches!(
        client
            .query::<crate::cells::repository::GetRepositoryLifecycle>(
                Address::Repository(id),
                Some(read.receipt),
                ()
            )
            .await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        Address::parse(&Address::Organization(id).path()).unwrap(),
        Address::Organization(id)
    );
    assert!(Address::parse("/organizations/00000000-0000-0000-0000-000000000000").is_err());
    stop(started.node).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn current_membership_fences_privileged_commands_reads_and_recorded_replays() {
    let local = tempfile::tempdir().unwrap();
    let started = runtime::test_node(local.path(), "127.0.0.1:8788".parse().unwrap()).await;
    let client = &started.client;
    let id = Uuid::now_v7();
    let alice = Uuid::now_v7();
    let bob = Uuid::now_v7();
    let charlie = Uuid::now_v7();
    bootstrap(client, id, alice).await;
    join(client, id, alice, bob, Role::Owner, 0).await;
    join(client, id, alice, charlie, Role::Member, 0).await;
    let key = identity();
    let input = MemberChange {
        actor: bob,
        member: charlie,
        version: 1,
        role: Some(Role::Owner),
    };
    let applied = client
        .command::<ChangeMember>(Address::Organization(id), key, input.clone())
        .await
        .unwrap();
    write::<ChangeMember>(
        client,
        id,
        MemberChange {
            actor: alice,
            member: bob,
            version: 1,
            role: Some(Role::Member),
        },
    )
    .await;
    assert!(
        client
            .query::<GetOrganization>(Address::Organization(id), None, Caller { actor: bob })
            .await
            .is_ok()
    );
    assert!(matches!(
        client
            .command::<ChangeMember>(Address::Organization(id), key, input.clone())
            .await,
        Err(Error::AccessDenied)
    ));
    assert!(matches!(
        client
            .command::<Edit>(
                Address::Organization(id),
                identity(),
                EditInput {
                    actor: bob,
                    version: 1,
                    name: "Unauthorized edit".into(),
                    description: String::new()
                }
            )
            .await,
        Err(Error::AccessDenied)
    ));
    write::<ChangeMember>(
        client,
        id,
        MemberChange {
            actor: alice,
            member: bob,
            version: 2,
            role: Some(Role::Owner),
        },
    )
    .await;
    assert_eq!(
        client
            .command::<ChangeMember>(Address::Organization(id), key, input.clone())
            .await
            .unwrap(),
        applied
    );
    write::<ChangeMember>(
        client,
        id,
        MemberChange {
            actor: alice,
            member: bob,
            version: 3,
            role: None,
        },
    )
    .await;
    assert!(matches!(
        client
            .query::<GetOrganization>(Address::Organization(id), None, Caller { actor: bob })
            .await,
        Err(Error::AccessDenied)
    ));
    assert!(matches!(
        client
            .command::<ChangeMember>(Address::Organization(id), key, input)
            .await,
        Err(Error::AccessDenied)
    ));
    assert_eq!(list(client, id, alice).await.len(), 2);
    stop(started.node).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_owner_demotion_cannot_remove_the_last_owner() {
    let local = tempfile::tempdir().unwrap();
    let started = runtime::test_node(local.path(), "127.0.0.1:8788".parse().unwrap()).await;
    let client = &started.client;
    let id = Uuid::now_v7();
    let alice = Uuid::now_v7();
    let bob = Uuid::now_v7();
    bootstrap(client, id, alice).await;
    join(client, id, alice, bob, Role::Owner, 0).await;
    let (a, b) = tokio::join!(
        write::<ChangeMember>(
            client,
            id,
            MemberChange {
                actor: alice,
                member: alice,
                version: 1,
                role: Some(Role::Member)
            }
        ),
        write::<ChangeMember>(
            client,
            id,
            MemberChange {
                actor: bob,
                member: bob,
                version: 1,
                role: Some(Role::Member)
            }
        )
    );
    assert_eq!(
        usize::from(matches!(a, ChangeOutcome::Applied(_)))
            + usize::from(matches!(b, ChangeOutcome::Applied(_))),
        1
    );
    assert!(matches!(a, ChangeOutcome::LastOwner) || matches!(b, ChangeOutcome::LastOwner));
    assert_eq!(
        list(client, id, alice)
            .await
            .iter()
            .filter(|m| m.role == Role::Owner)
            .count(),
        1
    );
    stop(started.node).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn removed_membership_versions_cannot_overwrite_a_later_grant() {
    let local = tempfile::tempdir().unwrap();
    let started = runtime::test_node(local.path(), "127.0.0.1:8788".parse().unwrap()).await;
    let client = &started.client;
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let person = Uuid::now_v7();
    bootstrap(client, id, owner).await;
    join(client, id, owner, person, Role::Member, 0).await;
    write::<ChangeMember>(
        client,
        id,
        MemberChange {
            actor: owner,
            member: person,
            version: 1,
            role: None,
        },
    )
    .await;
    join(client, id, owner, person, Role::Owner, 2).await;
    assert_eq!(
        write::<ChangeMember>(
            client,
            id,
            MemberChange {
                actor: owner,
                member: person,
                version: 1,
                role: Some(Role::Member)
            }
        )
        .await,
        ChangeOutcome::Conflict
    );
    let current = client
        .query::<GetMember>(
            Address::Organization(id),
            None,
            MemberKey {
                actor: owner,
                member: person,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(
        (current.version, current.role, current.active),
        (3, Role::Owner, true)
    );
    stop(started.node).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn member_pages_are_bounded_and_bound_to_the_organization_revision() {
    let local = tempfile::tempdir().unwrap();
    let started = runtime::test_node(local.path(), "127.0.0.1:8788".parse().unwrap()).await;
    let client = &started.client;
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    bootstrap(client, id, owner).await;
    for _ in 0..7 {
        join(client, id, owner, Uuid::now_v7(), Role::Member, 0).await;
    }
    let mut cursor = None;
    let mut all = Vec::new();
    let mut first = None;
    loop {
        let Page::Page { items, next, .. } = client
            .query::<ListMembers>(
                Address::Organization(id),
                None,
                PageQuery {
                    actor: owner,
                    after: cursor,
                    limit: 3,
                },
            )
            .await
            .unwrap()
            .output
        else {
            panic!("page changed");
        };
        assert!(items.len() <= 3);
        all.extend(items);
        if first.is_none() {
            first = next.clone();
        }
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(all, list(client, id, owner).await);
    assert_eq!(all.len(), 8);
    join(client, id, owner, Uuid::now_v7(), Role::Member, 0).await;
    assert_eq!(
        client
            .query::<ListMembers>(
                Address::Organization(id),
                None,
                PageQuery {
                    actor: owner,
                    after: first,
                    limit: 3
                }
            )
            .await
            .unwrap()
            .output,
        Page::Changed
    );
    stop(started.node).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn native_organization_grants_and_command_receipts_survive_restart() {
    let local = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:8788".parse().unwrap();
    let started = runtime::test_node(local.path(), address).await;
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    bootstrap(&started.client, id, owner).await;
    let person = Uuid::now_v7();
    join(&started.client, id, owner, person, Role::Member, 0).await;
    let key = identity();
    let input = MemberChange {
        actor: owner,
        member: person,
        version: 1,
        role: Some(Role::Owner),
    };
    let created = started
        .client
        .command::<ChangeMember>(Address::Organization(id), key, input.clone())
        .await
        .unwrap();
    stop(started.node).await;
    let restored = runtime::test_node(local.path(), address).await;
    assert_eq!(
        restored
            .client
            .command::<ChangeMember>(Address::Organization(id), key, input)
            .await
            .unwrap(),
        created
    );
    assert_eq!(list(&restored.client, id, owner).await.len(), 2);
    stop(restored.node).await;
}

mod invitations;
