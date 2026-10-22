use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn invitation_versions_fence_cancellation_edits_retries_and_reacceptance() {
    let local = tempfile::tempdir().unwrap();
    let started = runtime::test_node(local.path(), "127.0.0.1:8788".parse().unwrap()).await;
    let client = &started.client;
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let recipient = Uuid::now_v7();
    bootstrap(client, id, owner).await;
    let input = InviteInput {
        actor: owner,
        recipient,
        version: 0,
        role: Role::Owner,
    };
    let ChangeOutcome::Applied(first) = write::<Invite>(client, id, input.clone()).await else {
        panic!("offer missing")
    };
    assert_eq!(
        first.expires_at_ms - first.created_at_ms,
        7 * 24 * 60 * 60 * 1000
    );
    assert_eq!(list(client, id, owner).await.len(), 1);
    let own = client
        .query::<GetInvitation>(
            Address::Organization(id),
            None,
            MemberKey {
                actor: recipient,
                member: recipient,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(own.invitation, Some(first.clone()));
    let outsider = Uuid::now_v7();
    assert!(matches!(
        client
            .query::<GetInvitation>(
                Address::Organization(id),
                None,
                MemberKey {
                    actor: outsider,
                    member: recipient
                }
            )
            .await,
        Err(Error::AccessDenied)
    ));
    assert_eq!(
        write::<DecideInvitation>(
            client,
            id,
            Decision {
                actor: outsider,
                version: 1,
                accept: true
            }
        )
        .await,
        ChangeOutcome::NotFound
    );
    let ChangeOutcome::Applied(edited) = write::<Invite>(
        client,
        id,
        InviteInput {
            actor: owner,
            recipient,
            version: 1,
            role: Role::Member,
        },
    )
    .await
    else {
        panic!("edit missing")
    };
    assert_eq!(
        (edited.version, edited.created_at_ms, edited.expires_at_ms),
        (2, first.created_at_ms, first.expires_at_ms)
    );
    assert_eq!(
        write::<DecideInvitation>(
            client,
            id,
            Decision {
                actor: recipient,
                version: 1,
                accept: true
            }
        )
        .await,
        ChangeOutcome::Conflict
    );
    assert!(matches!(
        write::<CancelInvitation>(
            client,
            id,
            InvitationKey {
                actor: owner,
                recipient,
                version: 2
            }
        )
        .await,
        ChangeOutcome::Applied(_)
    ));
    assert_eq!(
        write::<DecideInvitation>(
            client,
            id,
            Decision {
                actor: recipient,
                version: 2,
                accept: true
            }
        )
        .await,
        ChangeOutcome::Conflict
    );
    let ChangeOutcome::Applied(renewed) = write::<Invite>(
        client,
        id,
        InviteInput {
            actor: owner,
            recipient,
            version: 3,
            role: Role::Member,
        },
    )
    .await
    else {
        panic!("renewal missing")
    };
    assert_eq!(renewed.version, 4);
    assert!(matches!(
        write::<DecideInvitation>(
            client,
            id,
            Decision {
                actor: recipient,
                version: 4,
                accept: false
            }
        )
        .await,
        ChangeOutcome::Applied(_)
    ));
    assert_eq!(list(client, id, owner).await.len(), 1);
    join(client, id, owner, recipient, Role::Member, 5).await;
    write::<ChangeMember>(
        client,
        id,
        MemberChange {
            actor: owner,
            member: recipient,
            version: 1,
            role: None,
        },
    )
    .await;
    assert_eq!(
        write::<DecideInvitation>(
            client,
            id,
            Decision {
                actor: recipient,
                version: 6,
                accept: true
            }
        )
        .await,
        ChangeOutcome::Conflict
    );
    assert!(matches!(
        client
            .query::<GetOrganization>(Address::Organization(id), None, Caller { actor: recipient })
            .await,
        Err(Error::AccessDenied)
    ));
    stop(started.node).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_invitation_acceptance_and_cancellation_have_one_atomic_winner() {
    let local = tempfile::tempdir().unwrap();
    let started = runtime::test_node(local.path(), "127.0.0.1:8788".parse().unwrap()).await;
    let client = &started.client;
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let recipient = Uuid::now_v7();
    bootstrap(client, id, owner).await;
    write::<Invite>(
        client,
        id,
        InviteInput {
            actor: owner,
            recipient,
            version: 0,
            role: Role::Member,
        },
    )
    .await;
    let (accepted, cancelled) = tokio::join!(
        write::<DecideInvitation>(
            client,
            id,
            Decision {
                actor: recipient,
                version: 1,
                accept: true
            }
        ),
        write::<CancelInvitation>(
            client,
            id,
            InvitationKey {
                actor: owner,
                recipient,
                version: 1
            }
        )
    );
    assert_eq!(
        usize::from(matches!(accepted, ChangeOutcome::Applied(_)))
            + usize::from(matches!(cancelled, ChangeOutcome::Applied(_))),
        1
    );
    let view = client
        .query::<GetInvitation>(
            Address::Organization(id),
            None,
            MemberKey {
                actor: owner,
                member: recipient,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    let accepted = view.invitation.unwrap().state == InvitationState::Accepted;
    assert_eq!(
        list(client, id, owner).await.len(),
        if accepted { 2 } else { 1 }
    );
    let events = client
        .query::<ListActivity>(
            Address::Organization(id),
            None,
            ActivityQuery {
                actor: owner,
                before: None,
                limit: 50,
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(
        events
            .items
            .iter()
            .map(|event| event.revision)
            .collect::<Vec<_>>(),
        vec![3, 2, 1]
    );
    stop(started.node).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_and_noop_commands_do_not_append_audit_or_change_revision() {
    let local = tempfile::tempdir().unwrap();
    let started = runtime::test_node(local.path(), "127.0.0.1:8788".parse().unwrap()).await;
    let client = &started.client;
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let original = bootstrap(client, id, owner).await;
    assert!(matches!(
        write::<Edit>(
            client,
            id,
            EditInput {
                actor: owner,
                version: original.version,
                name: original.name.clone(),
                description: original.description.clone()
            }
        )
        .await,
        ChangeOutcome::Applied(_)
    ));
    assert_eq!(
        write::<Edit>(
            client,
            id,
            EditInput {
                actor: owner,
                version: 0,
                name: "Stale".into(),
                description: String::new()
            }
        )
        .await,
        ChangeOutcome::Conflict
    );
    assert_eq!(
        write::<ChangeMember>(
            client,
            id,
            MemberChange {
                actor: owner,
                member: owner,
                version: 1,
                role: None
            }
        )
        .await,
        ChangeOutcome::LastOwner
    );
    let result = client
        .command::<Edit>(
            Address::Organization(id),
            identity(),
            EditInput {
                actor: owner,
                version: 1,
                name: "   ".into(),
                description: String::new(),
            },
        )
        .await;
    assert!(matches!(result, Err(Error::Invalid(_))));
    let current = client
        .query::<GetOrganization>(Address::Organization(id), None, Caller { actor: owner })
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(current.organization, original);
    let events = client
        .query::<ListActivity>(
            Address::Organization(id),
            None,
            ActivityQuery {
                actor: owner,
                before: None,
                limit: 1,
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(
        (
            events.items.len(),
            events.next,
            events.items[0].action.clone()
        ),
        (1, None, Action::Created)
    );
    stop(started.node).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invitation_pages_are_bounded_and_cannot_reuse_a_member_cursor() {
    let local = tempfile::tempdir().unwrap();
    let started = runtime::test_node(local.path(), "127.0.0.1:8788".parse().unwrap()).await;
    let client = &started.client;
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    bootstrap(client, id, owner).await;
    let mut expected = Vec::new();
    for _ in 0..8 {
        let recipient = Uuid::now_v7();
        expected.push(recipient);
        write::<Invite>(
            client,
            id,
            InviteInput {
                actor: owner,
                recipient,
                version: 0,
                role: Role::Member,
            },
        )
        .await;
    }
    let mut cursor = None;
    let mut all = Vec::new();
    let mut first = None;
    loop {
        let Page::Page { items, next, .. } = client
            .query::<ListInvitations>(
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
            panic!("page changed")
        };
        assert!(items.len() <= 3);
        all.extend(items.into_iter().map(|value| value.recipient));
        if first.is_none() {
            first = next.clone();
        }
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    expected.sort_unstable();
    assert_eq!(all, expected);
    let mut cross_collection = first.clone().unwrap();
    cross_collection.collection = Collection::Members;
    assert_eq!(
        client
            .query::<ListInvitations>(
                Address::Organization(id),
                None,
                PageQuery {
                    actor: owner,
                    after: Some(cross_collection),
                    limit: 3
                }
            )
            .await
            .unwrap()
            .output,
        Page::Changed
    );
    assert_eq!(
        client
            .query::<ListMembers>(
                Address::Organization(id),
                None,
                PageQuery {
                    actor: owner,
                    after: first.clone(),
                    limit: 3
                }
            )
            .await
            .unwrap()
            .output,
        Page::Changed
    );
    write::<CancelInvitation>(
        client,
        id,
        InvitationKey {
            actor: owner,
            recipient: all[0],
            version: 1,
        },
    )
    .await;
    assert_eq!(
        client
            .query::<ListInvitations>(
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
