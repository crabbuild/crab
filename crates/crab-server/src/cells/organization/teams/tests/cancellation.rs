use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_and_change_serialize_to_one_permanent_outcome() {
    let f = fixture().await;
    let client = &f.server.cells().unwrap();
    for index in 0..8 {
        let input = request(
            f.bob,
            Command::Create {
                name: format!("Concurrent {index}"),
                description: String::new(),
                visibility: Visibility::Secret,
            },
        );
        let (applied, cancelled) = tokio::join!(
            write::<Change>(client, f.organization, input.clone()),
            write::<Cancel>(client, f.organization, input.clone()),
        );
        assert_eq!(applied, cancelled);
        assert!(matches!(
            applied,
            Outcome::Applied { .. } | Outcome::Cancelled { .. }
        ));
        assert_eq!(
            write::<Change>(client, f.organization, input.clone()).await,
            applied
        );
        let visible = client
            .query::<Get>(
                Address::Organization(f.organization),
                None,
                Key {
                    actor: f.bob,
                    team: input.request_id,
                },
            )
            .await;
        assert_eq!(visible.is_ok(), matches!(applied, Outcome::Applied { .. }));
        let mut changed = input.clone();
        changed.command = Command::Create {
            name: "Changed payload".into(),
            description: String::new(),
            visibility: Visibility::Closed,
        };
        assert_eq!(
            write::<Cancel>(client, f.organization, changed).await,
            Outcome::SubmissionConflict
        );
    }
    fixtures::close(&f.server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_fences_late_writes_and_preserves_actor_ownership_after_removal() {
    let f = fixture().await;
    let client = &f.server.cells().unwrap();
    let input = request(
        f.bob,
        Command::Create {
            name: "Cancelled draft".into(),
            description: String::new(),
            visibility: Visibility::Secret,
        },
    );
    let cancelled = Outcome::Cancelled {
        id: input.request_id,
    };
    assert_eq!(
        write::<Cancel>(client, f.organization, input.clone()).await,
        cancelled
    );
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
    assert_eq!(
        write::<Change>(client, f.organization, input.clone()).await,
        cancelled
    );
    let mut other = input.clone();
    other.actor = f.owner;
    assert_eq!(
        write::<Cancel>(client, f.organization, other).await,
        Outcome::SubmissionConflict
    );
    let never_member = Uuid::now_v7();
    assert!(matches!(
        client
            .command::<Cancel>(
                Address::Organization(f.organization),
                crate::app::mutation_identity().unwrap(),
                request(never_member, input.command.clone())
            )
            .await
            .err()
            .unwrap(),
        Error::AccessDenied
    ));
    join(client, f.organization, f.owner, f.bob, 2).await;
    assert_eq!(
        write::<Change>(client, f.organization, input).await,
        cancelled
    );
    let listing = listed(&f, f.owner, "").await;
    assert!(matches!(listing.page,Page::Page { items,.. } if items.is_empty()));
    fixtures::close(&f.server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_rejections_fence_delayed_transports_after_mutable_preconditions_change() {
    let f = fixture().await;
    let client = &f.server.cells().unwrap();
    let existing = create(&f, f.owner, "Reserved", Visibility::Secret).await;
    let original = request(
        f.bob,
        Command::Create {
            name: "Reserved".into(),
            description: String::new(),
            visibility: Visibility::Secret,
        },
    );
    let before = read::<GetOrganization>(client, f.organization, Caller { actor: f.owner }).await;
    assert_eq!(
        write::<Change>(client, f.organization, original.clone()).await,
        Outcome::NameTaken
    );
    assert_eq!(
        read::<GetOrganization>(client, f.organization, Caller { actor: f.owner }).await,
        before
    );
    write::<Change>(
        client,
        f.organization,
        request(
            f.owner,
            Command::Delete {
                team: existing,
                version: 1,
            },
        ),
    )
    .await;
    assert_eq!(
        write::<Change>(client, f.organization, original.clone()).await,
        Outcome::NameTaken
    );
    assert!(matches!(listed(&f,f.owner,"").await.page,Page::Page {items,..} if items.is_empty()));
    let team = create(&f, f.owner, "Reviewers", Visibility::Closed).await;
    let person = Uuid::now_v7();
    let rejected = request(
        f.owner,
        Command::Member {
            team,
            member: person,
            version: 0,
            role: Some(Role::Maintainer),
        },
    );
    assert_eq!(
        write::<Change>(client, f.organization, rejected.clone()).await,
        Outcome::NotMember
    );
    join(client, f.organization, f.owner, person, 0).await;
    assert_eq!(
        write::<Change>(client, f.organization, rejected).await,
        Outcome::NotMember
    );
    assert_eq!(
        read::<GetMembership>(
            client,
            f.organization,
            MemberKey {
                actor: f.owner,
                team,
                member: person
            }
        )
        .await
        .role,
        None
    );
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
    assert_eq!(
        write::<Cancel>(client, f.organization, original).await,
        Outcome::NameTaken
    );
    fixtures::close(&f.server).await;
}
