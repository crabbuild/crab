use super::reads::ItemPage;
use super::*;
use crate::{
    cells::{MutationIdentity, RepositoryCell},
    test_support::server as fixtures,
};
fn actor() -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: "alice".into(),
        name: "Alice".into(),
    }
}
fn identity() -> MutationIdentity {
    crate::app::mutation_identity().unwrap()
}
async fn command(cell: &RepositoryCell, command: Command) -> Outcome {
    cell.client
        .command::<Change>(
            &cell.target,
            identity(),
            Request {
                request_id: Uuid::now_v7(),
                actor: actor(),
                can_write: true,
                command,
                cancel: false,
            },
        )
        .await
        .unwrap()
        .output
}
fn created(id: Uuid) -> Command {
    Command::Create {
        id,
        name: "Release plan".into(),
        template: "kanban".into(),
    }
}
async fn seed(cell: &RepositoryCell, n: u64) {
    for i in 0..n {
        cell.client
            .command::<super::super::CreateIssue>(
                &cell.target,
                identity(),
                super::super::CreateIssueInput {
                    metadata: Default::default(),
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: actor(),
                    title: format!("Issue {i}"),
                    body: String::new(),
                },
            )
            .await
            .unwrap();
    }
}
fn entry(number: u64) -> Item {
    Item {
        id: Uuid::now_v7(),
        subject: Some(Subject {
            kind: SubjectKind::Issue,
            number,
        }),
        title: String::new(),
        body: String::new(),
        status: "Todo".into(),
        priority: 2,
        start: Some("2026-10-01".into()),
        end: Some("2026-10-31".into()),
        values: BTreeMap::new(),
    }
}
async fn page(cell: &RepositoryCell, project: Uuid) -> ItemPage {
    cell.client
        .query::<Items>(
            &cell.target,
            None,
            ItemQuery {
                project,
                after: None,
                revision: None,
                query: String::new(),
                status: None,
                hide_closed: false,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap()
}
#[tokio::test(flavor = "multi_thread")]
async fn templates_create_real_views_and_copied_definitions_without_copying_membership() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    for template in templates() {
        assert!(template.definition.valid());
        let id = Uuid::now_v7();
        assert!(matches!(
            command(
                &cell,
                Command::Create {
                    id,
                    name: template.definition.name,
                    template: template.id
                }
            )
            .await,
            Outcome::Applied { version: 1, .. }
        ));
        assert_eq!(page(&cell, id).await.project.definition.views.len(), 3);
    }
    let source = Uuid::now_v7();
    command(&cell, created(source)).await;
    seed(&cell, 1).await;
    command(
        &cell,
        Command::Item {
            project: source,
            version: 1,
            item: entry(1),
        },
    )
    .await;
    let id = Uuid::now_v7();
    assert!(matches!(
        command(
            &cell,
            Command::Copy {
                id,
                source,
                version: 2,
                name: "Next release".into()
            }
        )
        .await,
        Outcome::Applied { .. }
    ));
    assert!(page(&cell, id).await.items.is_empty());
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn project_retries_cancel_conflicts_and_field_validation_preserve_canonical_items() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let id = Uuid::now_v7();
    seed(&cell, 1).await;
    let request = Request {
        request_id: Uuid::now_v7(),
        actor: actor(),
        can_write: true,
        command: created(id),
        cancel: false,
    };
    let result = cell
        .client
        .command::<Change>(&cell.target, identity(), request.clone())
        .await
        .unwrap()
        .output;
    assert_eq!(
        result,
        cell.client
            .command::<Change>(&cell.target, identity(), request.clone())
            .await
            .unwrap()
            .output
    );
    let mut changed = request.clone();
    changed.command = created(Uuid::now_v7());
    assert_eq!(
        cell.client
            .command::<Change>(&cell.target, identity(), changed)
            .await
            .unwrap()
            .output,
        Outcome::RequestConflict
    );
    let item = entry(1);
    assert!(matches!(
        command(
            &cell,
            Command::Item {
                project: id,
                version: 1,
                item: item.clone()
            }
        )
        .await,
        Outcome::Applied { version: 2, .. }
    ));
    assert!(matches!(
        command(
            &cell,
            Command::Item {
                project: id,
                version: 1,
                item: item.clone()
            }
        )
        .await,
        Outcome::Rejected { .. }
    ));
    let mut duplicate = item.clone();
    duplicate.id = Uuid::now_v7();
    assert!(matches!(
        command(
            &cell,
            Command::Item {
                project: id,
                version: 2,
                item: duplicate
            }
        )
        .await,
        Outcome::Rejected { .. }
    ));
    let mut bad = item.clone();
    bad.end = Some("2026-02-30".into());
    assert!(matches!(
        command(
            &cell,
            Command::Item {
                project: id,
                version: 2,
                item: bad
            }
        )
        .await,
        Outcome::Rejected { .. }
    ));
    let mut cancelled = request.clone();
    cancelled.request_id = Uuid::now_v7();
    cancelled.command = Command::Remove {
        project: id,
        version: 2,
        item: item.id,
    };
    cancelled.cancel = true;
    assert_eq!(
        cell.client
            .command::<Change>(&cell.target, identity(), cancelled.clone())
            .await
            .unwrap()
            .output,
        Outcome::Cancelled
    );
    cancelled.cancel = false;
    assert_eq!(
        cell.client
            .command::<Change>(&cell.target, identity(), cancelled)
            .await
            .unwrap()
            .output,
        Outcome::Cancelled
    );
    assert_eq!(page(&cell, id).await.items.len(), 1);
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn hierarchy_reparents_atomically_rejects_cycles_and_counts_live_child_state() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell, 3).await;
    for (child, parent) in [(2, 1), (3, 2)] {
        assert!(matches!(
            command(
                &cell,
                Command::Parent {
                    child,
                    parent: Some(parent),
                    version: 0
                }
            )
            .await,
            Outcome::Applied { .. }
        ));
    }
    assert!(matches!(
        command(
            &cell,
            Command::Parent {
                child: 1,
                parent: Some(3),
                version: 0
            }
        )
        .await,
        Outcome::Rejected { .. }
    ));
    assert!(matches!(
        command(
            &cell,
            Command::Parent {
                child: 3,
                parent: Some(1),
                version: 1
            }
        )
        .await,
        Outcome::Applied { version: 2, .. }
    ));
    let tree = cell
        .client
        .query::<hierarchy::GetHierarchy>(
            &cell.target,
            None,
            hierarchy::Query {
                issue: 1,
                after: None,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(
        tree.progress,
        hierarchy::Progress {
            total: 2,
            closed: 0
        }
    );
    assert!(matches!(
        command(
            &cell,
            Command::Parent {
                child: 3,
                parent: None,
                version: 1
            }
        )
        .await,
        Outcome::Rejected { .. }
    ));
    let issue = Subject {
        kind: SubjectKind::Issue,
        number: 3,
    };
    cell.client
        .command::<super::super::UpdateIssue>(
            &cell.target,
            identity(),
            super::super::UpdateIssueInput {
                state_reason: None,
                number: issue.number,
                version: 1,
                actor: actor(),
                can_triage: true,
                title: None,
                body: None,
                state: Some(1),
                label_ids: None,
                assignees: None,
            },
        )
        .await
        .unwrap();
    let tree = cell
        .client
        .query::<hierarchy::GetHierarchy>(
            &cell.target,
            None,
            hierarchy::Query {
                issue: 1,
                after: None,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(
        tree.progress,
        hierarchy::Progress {
            total: 2,
            closed: 1
        }
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn issue_creation_conversion_and_parent_assignment_share_one_transaction() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell, 1).await;
    let id = Uuid::now_v7();
    command(&cell, created(id)).await;
    let draft = Item {
        subject: None,
        title: "Child issue".into(),
        body: "Plan".into(),
        ..entry(1)
    };
    command(
        &cell,
        Command::Item {
            project: id,
            version: 1,
            item: draft.clone(),
        },
    )
    .await;
    assert!(matches!(
        command(
            &cell,
            Command::CreateIssue {
                project: id,
                version: 2,
                item: draft.clone(),
                parent: Some(999)
            }
        )
        .await,
        Outcome::Rejected { .. }
    ));
    assert!(
        cell.client
            .query::<super::super::GetIssue>(&cell.target, None, 2)
            .await
            .unwrap()
            .output
            .is_none()
    );
    assert!(matches!(
        command(
            &cell,
            Command::CreateIssue {
                project: id,
                version: 2,
                item: draft,
                parent: Some(1)
            }
        )
        .await,
        Outcome::Applied { version: 3, .. }
    ));
    let items = page(&cell, id).await;
    assert_eq!(items.items.len(), 1);
    assert_eq!(items.items[0].work.as_ref().unwrap().subject.number, 2);
    assert_eq!(items.items[0].parent.as_ref().unwrap().subject.number, 1);
    assert!(matches!(
        command(
            &cell,
            Command::SubIssue {
                parent: 1,
                title: "Another child".into(),
                body: String::new()
            }
        )
        .await,
        Outcome::Applied { project: None, .. }
    ));
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn pages_count_all_matching_work_and_reject_stale_cursors() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let id = Uuid::now_v7();
    command(&cell, created(id)).await;
    for i in 0..51 {
        let draft = Item {
            subject: None,
            title: format!("Task {i}"),
            body: String::new(),
            ..entry(1)
        };
        assert!(matches!(
            command(
                &cell,
                Command::Item {
                    project: id,
                    version: i + 1,
                    item: draft
                }
            )
            .await,
            Outcome::Applied { .. }
        ));
    }
    let first = page(&cell, id).await;
    assert_eq!((first.total, first.items.len()), (51, 50));
    let query = ItemQuery {
        project: id,
        after: first.next,
        revision: Some(first.revision),
        query: String::new(),
        status: None,
        hide_closed: false,
    };
    let next = cell
        .client
        .query::<Items>(&cell.target, None, query.clone())
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(next.items.len(), 1);
    assert_eq!(next.total, 51);
    let draft = Item {
        subject: None,
        title: "Extra".into(),
        body: String::new(),
        ..entry(1)
    };
    command(
        &cell,
        Command::Item {
            project: id,
            version: 52,
            item: draft,
        },
    )
    .await;
    assert!(
        cell.client
            .query::<Items>(&cell.target, None, query)
            .await
            .is_err()
    );
    fixtures::close(&server).await;
}

mod fields;
mod integration;
pub(crate) use integration::Historical;
