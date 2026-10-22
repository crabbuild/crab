use super::*;
use crate::{cells::RepositoryCell, test_support::server as fixtures};

fn actor() -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: "alice".into(),
        name: "Alice".into(),
    }
}
async fn write<M: Mutation>(cell: &RepositoryCell, input: M::Input) -> M::Output {
    cell.client
        .command::<M>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            input,
        )
        .await
        .unwrap()
        .output
}
async fn read<R: Read>(cell: &RepositoryCell, input: R::Input) -> R::Output {
    cell.client
        .query::<R>(&cell.target, None, input)
        .await
        .unwrap()
        .output
}
async fn seed(cell: &RepositoryCell) {
    write::<CreateIssue>(
        cell,
        CreateIssueInput {
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor(),
            title: "Issue".into(),
            body: "Description".into(),
        },
    )
    .await;
    write::<CreatePull>(
        cell,
        CreatePullInput {
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor(),
            draft: false,
            title: "Pull".into(),
            body: "Description".into(),
            base_ref: "refs/heads/main".into(),
            head_ref: "refs/heads/feature".into(),
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
        },
    )
    .await;
}
async fn selected(cell: &RepositoryCell, version: u64) -> Vec<Selected> {
    let Outcome::Applied(items) = read::<Select>(
        cell,
        [SubjectKind::Issue, SubjectKind::Pull]
            .into_iter()
            .map(|kind| Versioned {
                subject: Subject { kind, number: 1 },
                version,
            })
            .collect(),
    )
    .await
    else {
        panic!("selection rejected")
    };
    items
}
fn input(items: Vec<Selected>, action: Action) -> Input {
    Input {
        submission_id: *Uuid::now_v7().as_bytes(),
        actor: actor(),
        can_triage: true,
        items,
        action,
        eligible: vec![],
    }
}
fn applied(result: Outcome<u16>) -> u16 {
    match result {
        Outcome::Applied(n) => n,
        other => panic!("bulk rejected: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn label_deltas_ignore_retired_assignments_on_both_discussion_kinds() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    for name in ["retired", "current"] {
        write::<CreateLabel>(
            &cell,
            CreateLabelInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor(),
                name: name.into(),
                color: "abcdef".into(),
                description: None,
            },
        )
        .await;
    }
    let change = input(
        selected(&cell, 1).await,
        Action::Labels {
            add: vec![1],
            remove: vec![],
        },
    );
    applied(write::<Edit>(&cell, change).await);
    write::<DeleteLabel>(
        &cell,
        DeleteLabelInput {
            number: 1,
            version: 1,
        },
    )
    .await;
    let change = input(
        selected(&cell, 2).await,
        Action::Labels {
            add: vec![2],
            remove: vec![],
        },
    );
    assert_eq!(applied(write::<Edit>(&cell, change).await), 2);
    assert_eq!(read::<GetIssue>(&cell, 1).await.unwrap().label_ids, [2]);
    assert_eq!(read::<GetPull>(&cell, 1).await.unwrap().label_ids, [2]);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mixed_state_edits_rollback_on_stale_selection_and_recover_after_archive() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    let change = input(
        selected(&cell, 1).await,
        Action::State {
            state: State::Closed,
            reason: None,
        },
    );
    let mut stale = change.clone();
    stale.items[1].item.version = 2;
    assert!(matches!(
        write::<Edit>(&cell, stale).await,
        Outcome::Rejected(Failure::Conflict)
    ));
    assert_eq!(read::<GetIssue>(&cell, 1).await.unwrap().version, 1);
    assert_eq!(applied(write::<Edit>(&cell, change.clone()).await), 2);
    let mut altered = change.clone();
    altered.action = Action::State {
        state: State::Open,
        reason: None,
    };
    assert!(matches!(
        write::<Edit>(&cell, altered).await,
        Outcome::Rejected(Failure::RequestConflict)
    ));
    write::<ReplaceRepositoryLifecycle>(
        &cell,
        ReplaceRepositoryLifecycleInput {
            expected_version: 0,
            archived: true,
        },
    )
    .await;
    let mut replay = change.clone();
    replay.actor.name = "Renamed".into();
    replay.can_triage = false;
    assert_eq!(applied(write::<Edit>(&cell, replay).await), 2);
    let mut fresh = change;
    fresh.submission_id = *Uuid::now_v7().as_bytes();
    assert!(matches!(
        write::<Edit>(&cell, fresh).await,
        Outcome::Rejected(Failure::Archived)
    ));
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn metadata_deltas_preserve_other_values_and_milestone_versions_fence_changes() {
    use super::super::Decision;
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    for name in ["first", "second"] {
        write::<CreateLabel>(
            &cell,
            CreateLabelInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor(),
                name: name.into(),
                color: "abcdef".into(),
                description: None,
            },
        )
        .await;
    }
    for (version, add, remove) in [
        (1, vec![1], vec![]),
        (2, vec![2], vec![]),
        (3, vec![], vec![1]),
    ] {
        let change = input(
            selected(&cell, version).await,
            Action::Labels { add, remove },
        );
        assert_eq!(applied(write::<Edit>(&cell, change).await), 2);
    }
    assert_eq!(read::<GetIssue>(&cell, 1).await.unwrap().label_ids, [2]);
    assert_eq!(read::<GetPull>(&cell, 1).await.unwrap().label_ids, [2]);
    let person = Assignee {
        id: Uuid::now_v7(),
        name: "Person".into(),
    };
    let mut assign = input(
        selected(&cell, 4).await,
        Action::Assignees {
            add: vec![person.id],
            remove: vec![],
        },
    );
    assert!(matches!(
        write::<Edit>(&cell, assign.clone()).await,
        Outcome::Rejected(Failure::InvalidAssignees)
    ));
    assign.eligible = vec![person.clone()];
    assert_eq!(applied(write::<Edit>(&cell, assign.clone()).await), 2);
    assign.eligible.clear();
    assign.can_triage = false;
    assert_eq!(applied(write::<Edit>(&cell, assign).await), 2);
    let Decision::Applied(milestone) = write::<milestones::CreateMilestone>(
        &cell,
        milestones::CreateMilestoneInput {
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor(),
            title: "Release".into(),
            description: "".into(),
            due_on: None,
        },
    )
    .await
    else {
        panic!("milestone rejected")
    };
    let change = input(
        selected(&cell, 5).await,
        Action::Milestone {
            number: Some(milestone.number),
        },
    );
    write::<milestones::AssignMilestone>(
        &cell,
        milestones::AssignMilestoneInput {
            subject: change.items[1].item.subject,
            milestone: Some(milestone.number),
            version: 0,
            actor: actor(),
        },
    )
    .await;
    assert!(matches!(
        write::<Edit>(&cell, change).await,
        Outcome::Rejected(Failure::Conflict)
    ));
    assert!(
        read::<milestones::GetAssignment>(
            &cell,
            Subject {
                kind: SubjectKind::Issue,
                number: 1
            }
        )
        .await
        .unwrap()
        .milestone
        .is_none()
    );
    let change = input(
        selected(&cell, 5).await,
        Action::Milestone {
            number: Some(milestone.number),
        },
    );
    assert_eq!(applied(write::<Edit>(&cell, change).await), 2);
    let progress = read::<milestones::GetMilestone>(&cell, milestone.number)
        .await
        .unwrap();
    assert_eq!((progress.open_count, progress.closed_count), (2, 0));
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn selection_bounds_duplicates_roles_and_overlapping_deltas_reject_without_effects() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    let mut change = input(
        selected(&cell, 1).await,
        Action::State {
            state: State::Closed,
            reason: None,
        },
    );
    change.can_triage = false;
    assert!(matches!(
        write::<Edit>(&cell, change.clone()).await,
        Outcome::Rejected(Failure::Forbidden)
    ));
    change.can_triage = true;
    change.items.push(change.items[0].clone());
    assert!(matches!(
        write::<Edit>(&cell, change).await,
        Outcome::Rejected(Failure::Invalid)
    ));
    let change = input(
        selected(&cell, 1).await,
        Action::Labels {
            add: vec![1],
            remove: vec![1],
        },
    );
    assert!(matches!(
        write::<Edit>(&cell, change).await,
        Outcome::Rejected(Failure::Invalid)
    ));
    assert_eq!(read::<GetIssue>(&cell, 1).await.unwrap().state, 0);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn maximum_page_selection_edits_large_records_and_bounds_the_next_item() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    for number in 1..=MAX_ITEMS {
        write::<CreateIssue>(
            &cell,
            CreateIssueInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor(),
                title: format!("Issue {number}"),
                body: "content ".repeat(8000),
            },
        )
        .await;
    }
    let targets: Vec<_> = (1..=MAX_ITEMS)
        .map(|number| Versioned {
            subject: Subject {
                kind: SubjectKind::Issue,
                number: number as u64,
            },
            version: 1,
        })
        .collect();
    let Outcome::Applied(items) = read::<Select>(&cell, targets.clone()).await else {
        panic!("maximum selection rejected")
    };
    let change = input(
        items,
        Action::State {
            state: State::Closed,
            reason: Some(Reason::NotPlanned),
        },
    );
    assert_eq!(
        applied(write::<Edit>(&cell, change).await),
        MAX_ITEMS as u16
    );
    let mut oversized = targets;
    oversized.push(Versioned {
        subject: Subject {
            kind: SubjectKind::Issue,
            number: 101,
        },
        version: 1,
    });
    assert!(matches!(
        read::<Select>(&cell, oversized).await,
        Outcome::Rejected(Failure::Invalid)
    ));
    let search = read::<search::results::ListDiscussions>(
        &cell,
        search::index::SearchInput {
            query: Some("reason:\"not planned\"".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(search.matching, MAX_ITEMS as u64);
    fixtures::close(&server).await;
}
