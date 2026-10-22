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
async fn read(cell: &RepositoryCell) -> Page {
    cell.client
        .query::<GetPins>(&cell.target, None, ())
        .await
        .unwrap()
        .output
}
async fn seed(cell: &RepositoryCell) {
    for number in 1..=4 {
        write::<CreateIssue>(
            cell,
            CreateIssueInput {
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                author: actor(),
                title: format!("Issue {number}"),
                body: "Body".into(),
            },
        )
        .await;
    }
}
fn input(version: u64, numbers: Vec<u64>) -> Input {
    Input {
        submission_id: *uuid::Uuid::now_v7().as_bytes(),
        actor: actor(),
        can_write: true,
        version,
        numbers,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pin_order_versions_and_replay_preserve_later_changes_without_duplicate_activity() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    let original = input(0, vec![1]);
    assert!(matches!(
        write::<SetPins>(&cell, original.clone()).await,
        Outcome::Applied(1)
    ));
    assert!(matches!(
        write::<SetPins>(&cell, input(1, vec![1, 2, 3])).await,
        Outcome::Applied(2)
    ));
    assert!(matches!(
        write::<SetPins>(&cell, input(1, vec![3, 2, 1])).await,
        Outcome::Conflict
    ));
    assert!(matches!(
        write::<SetPins>(&cell, input(2, vec![3, 2, 1])).await,
        Outcome::Applied(3)
    ));
    write::<ReplaceRepositoryLifecycle>(
        &cell,
        ReplaceRepositoryLifecycleInput {
            expected_version: 0,
            archived: true,
        },
    )
    .await;
    let mut retry = original.clone();
    retry.actor.name = "Renamed".into();
    retry.can_write = false;
    assert!(matches!(
        write::<SetPins>(&cell, retry).await,
        Outcome::Applied(1)
    ));
    let mut altered = original;
    altered.numbers = vec![2];
    assert!(matches!(
        write::<SetPins>(&cell, altered).await,
        Outcome::RequestConflict
    ));
    let page = read(&cell).await;
    assert_eq!(
        (
            page.version,
            page.items
                .iter()
                .map(|item| item.number)
                .collect::<Vec<_>>()
        ),
        (3, vec![3, 2, 1])
    );
    let events = cell
        .client
        .query::<activity::ListActivity>(
            &cell.target,
            None,
            activity::Query {
                subject: None,
                before: None,
                limit: 50,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(
        events
            .items
            .iter()
            .filter(|event| matches!(event.action, activity::Action::Pinned))
            .count(),
        3
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pins_enforce_write_access_unique_existing_issues_and_the_three_item_limit() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    let mut denied = input(0, vec![1]);
    denied.can_write = false;
    assert!(matches!(
        write::<SetPins>(&cell, denied).await,
        Outcome::Forbidden
    ));
    assert!(matches!(
        write::<SetPins>(&cell, input(0, vec![1, 1])).await,
        Outcome::Invalid
    ));
    assert!(matches!(
        write::<SetPins>(&cell, input(0, vec![1, 99])).await,
        Outcome::NotFound
    ));
    assert!(read(&cell).await.items.is_empty());
    assert!(matches!(
        write::<SetPins>(&cell, input(0, vec![1, 2, 3])).await,
        Outcome::Applied(1)
    ));
    assert!(matches!(
        write::<SetPins>(&cell, input(1, vec![1, 2, 3, 4])).await,
        Outcome::Limit
    ));
    assert_eq!(read(&cell).await.items.len(), 3);
    assert!(matches!(
        write::<SetPins>(&cell, input(1, vec![2, 3, 4])).await,
        Outcome::Applied(2)
    ));
    let original = cell
        .client
        .query::<GetIssue>(&cell.target, None, 1)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(original.version, 1);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_pin_sets_compare_one_collection_version() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    let (a, b) = tokio::join!(
        write::<SetPins>(&cell, input(0, vec![1, 2])),
        write::<SetPins>(&cell, input(0, vec![3, 4]))
    );
    assert_eq!(
        [a, b]
            .iter()
            .filter(|value| matches!(value, Outcome::Applied(1)))
            .count(),
        1
    );
    assert_eq!(read(&cell).await.items.len(), 2);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pinned_cards_read_current_titles_close_reasons_and_indexed_comment_counts() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    write::<SetPins>(&cell, input(0, vec![1])).await;
    write::<UpdateIssue>(
        &cell,
        UpdateIssueInput {
            number: 1,
            actor: actor(),
            can_triage: true,
            version: 1,
            title: Some("Renamed title".into()),
            body: None,
            state: Some(1),
            state_reason: Some(lifecycle::Reason::NotPlanned),
            label_ids: None,
            assignees: None,
        },
    )
    .await;
    write::<CreateComment>(
        &cell,
        CreateCommentInput {
            submission_id: *uuid::Uuid::now_v7().as_bytes(),
            issue: 1,
            author: actor(),
            body: "Context".into(),
        },
    )
    .await;
    let page = read(&cell).await;
    let item = &page.items[0];
    assert_eq!(
        (
            item.title.as_str(),
            item.state,
            item.state_reason,
            item.comments
        ),
        ("Renamed title", 1, Some(lifecycle::Reason::NotPlanned), 1)
    );
    fixtures::close(&server).await;
}
