use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn project_sources_show_live_issues_pulls_links_and_native_search() {
    use crate::cells::repository::collaboration::links::{Pair, Selection, SetLink};
    use crate::cells::repository::{CreatePull, CreatePullInput, UpdateIssue, UpdateIssueInput};
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell, 1).await;
    let id = Uuid::now_v7();
    command(&cell, created(id)).await;
    cell.client
        .command::<CreatePull>(
            &cell.target,
            identity(),
            CreatePullInput {
                metadata: Default::default(),
                draft: false,
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor(),
                title: "Implementation PR".into(),
                body: String::new(),
                base_ref: "refs/heads/main".into(),
                head_ref: "refs/heads/feature".into(),
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();
    cell.client
        .command::<SetLink>(
            &cell.target,
            identity(),
            Selection {
                pair: Pair { pull: 1, issue: 1 },
                active: true,
                version: 0,
                actor: actor(),
            },
        )
        .await
        .unwrap();
    command(
        &cell,
        Command::Item {
            project: id,
            version: 1,
            item: entry(1),
        },
    )
    .await;
    let pull = Item {
        subject: Some(Subject {
            kind: SubjectKind::Pull,
            number: 1,
        }),
        ..entry(1)
    };
    command(
        &cell,
        Command::Item {
            project: id,
            version: 2,
            item: pull,
        },
    )
    .await;
    cell.client
        .command::<UpdateIssue>(
            &cell.target,
            identity(),
            UpdateIssueInput {
                number: 1,
                version: 1,
                actor: actor(),
                can_triage: true,
                title: Some("Current title".into()),
                body: None,
                state: Some(1),
                state_reason: None,
                label_ids: None,
                assignees: Some(vec![crate::cells::repository::assignments::Assignee {
                    id: Uuid::from_u128(99),
                    name: "Current assignee".into(),
                }]),
            },
        )
        .await
        .unwrap();
    let items = page(&cell, id).await;
    assert_eq!(items.items[0].assignees[0].name, "Current assignee");
    assert_eq!(items.items[0].work.as_ref().unwrap().title, "Current title");
    assert_eq!(items.items[0].links[0].title, "Implementation PR");
    let matches = cell
        .client
        .query::<Candidates>(
            &cell.target,
            None,
            CandidateQuery {
                q: "Current".into(),
                kind: Some(SubjectKind::Issue),
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(matches[0].title, "Current title");
    let exact = cell
        .client
        .query::<Candidates>(
            &cell.target,
            None,
            CandidateQuery {
                q: "#1".into(),
                kind: None,
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(exact.len(), 2);
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_edits_have_one_winner_and_rejected_submissions_remain_terminal() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let id = Uuid::now_v7();
    command(&cell, created(id)).await;
    let results = futures_util::future::join_all((0..8).map(|i| {
        command(
            &cell,
            Command::Item {
                project: id,
                version: 1,
                item: Item {
                    subject: None,
                    title: format!("Task {i}"),
                    body: String::new(),
                    ..entry(1)
                },
            },
        )
    }))
    .await;
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Outcome::Applied { .. }))
            .count(),
        1
    );
    let request = Request {
        request_id: Uuid::now_v7(),
        actor: actor(),
        can_write: false,
        command: created(Uuid::now_v7()),
        cancel: false,
    };
    let first = cell
        .client
        .command::<Change>(&cell.target, identity(), request.clone())
        .await
        .unwrap()
        .output;
    let retry = Request {
        can_write: true,
        ..request
    };
    assert_eq!(
        cell.client
            .command::<Change>(&cell.target, identity(), retry)
            .await
            .unwrap()
            .output,
        first
    );
    fixtures::close(&server).await;
}

pub(crate) struct Historical;
impl Mutation for Historical {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 59000;
    type Input = ();
    type Output = ();
    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<MutationResult<()>> {
        for table in [
            "repository_issue_parents",
            "repository_project_items",
            "repository_projects",
        ] {
            context.query(&format!("DROP TABLE {table}"), &[])?;
        }
        context.delete_document("projects/v1/prepared")?;
        Ok(MutationResult::Success(()))
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn existing_cells_require_explicit_preparation_without_rewriting_issues() {
    use crate::cells::repository::collaboration::lifecycle::{
        LifecycleStatus, PrepareLifecycleInput,
    };
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell, 1).await;
    cell.client
        .command::<Historical>(&cell.target, identity(), ())
        .await
        .unwrap();
    assert!(
        cell.client
            .query::<ListProjects>(
                &cell.target,
                None,
                ProjectQuery {
                    after: None,
                    archived: false,
                    templates: false
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        cell.client
            .query::<preparation::GetState>(&cell.target, None, ())
            .await
            .unwrap()
            .output,
        LifecycleStatus::Outdated
    );
    cell.client
        .command::<preparation::Prepare>(
            &cell.target,
            identity(),
            PrepareLifecycleInput { version: 0 },
        )
        .await
        .unwrap();
    assert_eq!(
        cell.client
            .query::<crate::cells::repository::GetIssue>(&cell.target, None, 1)
            .await
            .unwrap()
            .output
            .unwrap()
            .title,
        "Issue 0"
    );
    let id = Uuid::now_v7();
    assert!(matches!(
        command(&cell, created(id)).await,
        Outcome::Applied { .. }
    ));
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn native_restart_retains_projects_items_parent_edges_and_submission_decisions() {
    use cellar::sdk::{NodeOutcome, ShutdownMode};
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let repository = Uuid::now_v7();
    let started = crate::cells::runtime::test_node(directory.path(), address).await;
    started.client.initialize(repository).await.unwrap();
    let cell = started.client.repository(repository).unwrap();
    seed(&cell, 2).await;
    let id = Uuid::now_v7();
    command(&cell, created(id)).await;
    command(
        &cell,
        Command::Item {
            project: id,
            version: 1,
            item: entry(2),
        },
    )
    .await;
    let request = Request {
        request_id: Uuid::now_v7(),
        actor: actor(),
        can_write: true,
        command: Command::Parent {
            child: 2,
            parent: Some(1),
            version: 0,
        },
        cancel: false,
    };
    let committed = cell
        .client
        .command::<Change>(&cell.target, identity(), request.clone())
        .await
        .unwrap()
        .output;
    let NodeOutcome::Stopped(report) = started.node.shutdown(ShutdownMode::Preserve).await.unwrap()
    else {
        panic!("native node did not drain")
    };
    assert!(report.cells_drained && !report.drain_timed_out);
    let restored = crate::cells::runtime::test_node(directory.path(), address).await;
    let cell = restored.client.repository(repository).unwrap();
    assert_eq!(
        cell.client
            .command::<Change>(&cell.target, identity(), request)
            .await
            .unwrap()
            .output,
        committed
    );
    assert_eq!(
        page(&cell, id).await.items[0]
            .parent
            .as_ref()
            .unwrap()
            .subject
            .number,
        1
    );
    let NodeOutcome::Stopped(report) = restored
        .node
        .shutdown(ShutdownMode::Preserve)
        .await
        .unwrap()
    else {
        panic!("restored node did not drain")
    };
    assert!(report.cells_drained && !report.drain_timed_out);
}
