use super::*;
use crate::cells::repository::search::index::{
    GetSearchIndexStatus, RebuildIndex, RebuildSearchInput, SearchIndexStatus,
};
use crate::{cells::RepositoryCell, test_support::server as fixtures};
use uuid::Uuid;

fn actor(subject: &str) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: subject.into(),
        name: subject.into(),
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
async fn seed(cell: &RepositoryCell) {
    write::<CreatePull>(
        cell,
        CreatePullInput {
            metadata: Default::default(),
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("author"),
            draft: false,
            title: "Current head summaries".into(),
            body: String::new(),
            base_ref: "refs/heads/main".into(),
            head_ref: "refs/heads/topic".into(),
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
        },
    )
    .await;
}
async fn version(cell: &RepositoryCell) -> u64 {
    cell.client
        .query::<GetPull>(&cell.target, None, 1)
        .await
        .unwrap()
        .output
        .unwrap()
        .version
}
async fn page(cell: &RepositoryCell, oid: Option<&str>) -> Rollup {
    cell.client
        .query::<GetRollups>(
            &cell.target,
            None,
            Input {
                heads: vec![Head {
                    number: 1,
                    version: version(cell).await,
                    oid: oid.map(str::to_owned),
                }],
                source_token: None,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap()
        .pop()
        .unwrap()
}
fn report(status: u8, conclusion: Option<u8>) -> CheckReportInput {
    CheckReportInput {
        status,
        conclusion,
        details_url: None,
        output: CheckOutputRecord {
            title: "CI".into(),
            summary: "CI result".into(),
            text: None,
            steps: vec![],
            annotations: vec![],
        },
    }
}
async fn run(
    cell: &RepositoryCell,
    name: &str,
    status: u8,
    conclusion: Option<u8>,
) -> CheckRunDetail {
    match write::<CreateCheckRun>(
        cell,
        CreateCheckRunInput {
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("ci"),
            oid: "b".repeat(40),
            name: name.into(),
            report: report(status, conclusion),
        },
    )
    .await
    {
        CreateCheckRunOutcome::Created(run) => *run,
        _ => panic!("create check failed"),
    }
}
async fn request(cell: &RepositoryCell, who: &str, requested: bool) {
    let outcome = write::<review_requests::SetReviewRequest>(
        cell,
        review_requests::Selection {
            pull: 1,
            version: version(cell).await,
            actor: actor("author"),
            can_manage: true,
            reviewer: actor(who),
            requested,
        },
    )
    .await;
    assert!(matches!(outcome, review_requests::Outcome::Applied(_)));
}
async fn review(cell: &RepositoryCell, who: &str, state: ReviewState) {
    let outcome = write::<CreatePullReview>(
        cell,
        CreatePullReviewInput {
            pull: 1,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor(who),
            body: "Review context".into(),
            state,
            commit_oid: "b".repeat(40),
        },
    )
    .await;
    assert!(matches!(outcome, CreatePullReviewOutcome::Created(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn rollups_combine_latest_status_contexts_and_latest_named_run_versions_without_outputs() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    for (context, state) in [("lint", 1), ("LINT", 3), ("deploy", 2)] {
        write::<CreateCommitStatus>(
            &cell,
            CreateCommitStatusInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor("ci"),
                oid: "b".repeat(40),
                context: context.into(),
                state,
                description: None,
                target_url: None,
            },
        )
        .await;
    }
    let older = run(&cell, "unit", 1, None).await;
    run(&cell, "UNIT", 2, Some(5)).await;
    write::<UpdateCheckRun>(
        &cell,
        UpdateCheckRunInput {
            submission_id: *Uuid::now_v7().as_bytes(),
            actor: actor("ci"),
            oid: "b".repeat(40),
            number: older.run.number,
            version: 1,
            report: report(2, Some(2)),
        },
    )
    .await;
    for conclusion in [0, 1, 2, 3, 4, 6] {
        run(&cell, &format!("result-{conclusion}"), 2, Some(conclusion)).await;
    }
    assert_eq!(
        page(&cell, Some(&"b".repeat(40))).await.checks.unwrap(),
        Checks {
            success: 2,
            neutral: 1,
            skipped: 1,
            pending: 1,
            failed: 4
        }
    );
    assert_eq!(
        page(&cell, Some(&"c".repeat(40))).await.checks.unwrap(),
        Checks::default()
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn decisions_and_request_counters_follow_fulfillment_cancellation_and_source_head_changes() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    request(&cell, "bob", true).await;
    request(&cell, "carol", true).await;
    review(&cell, "bob", ReviewState::Approved).await;
    review(&cell, "bob", ReviewState::Commented).await;
    request(&cell, "carol", false).await;
    review(&cell, "dan", ReviewState::ChangesRequested).await;
    request(&cell, "erin", true).await;
    assert_eq!(
        page(&cell, Some(&"b".repeat(40))).await.reviews.unwrap(),
        Reviews {
            approved: 1,
            changes_requested: 1,
            requested: 1,
            outdated: 0
        }
    );
    assert_eq!(
        page(&cell, Some(&"c".repeat(40))).await.reviews.unwrap(),
        Reviews {
            approved: 0,
            changes_requested: 1,
            requested: 1,
            outdated: 2
        }
    );
    let missing = page(&cell, None).await;
    assert!(missing.head_oid.is_none() && missing.reviews.is_none() && missing.checks.is_none());
    let stale = cell
        .client
        .query::<GetRollups>(
            &cell.target,
            None,
            Input {
                heads: vec![Head {
                    number: 1,
                    version: 1,
                    oid: Some("b".repeat(40)),
                }],
                source_token: None,
            },
        )
        .await
        .unwrap()
        .output;
    assert!(stale.is_none());
    fixtures::close(&server).await;
}

pub(in crate::cells::repository) struct PreviousFormat;
impl Mutation for PreviousFormat {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 65_518;
    type Input = ();
    type Output = ();
    fn execute(context: &mut Transaction<'_>, (): ()) -> crate::cells::Result<MutationResult<()>> {
        context.install_schema("DROP TRIGGER discussion_rollup_reviewer_insert; DROP TRIGGER discussion_rollup_reviewer_update; DROP TRIGGER discussion_rollup_reviewer_delete; DROP TABLE discussion_pull_rollups;")?;
        let mut state: serde_json::Value = context.document("discussion-search/v1/state")?.unwrap();
        state["format"] = serde_json::json!(10);
        context.put_document("discussion-search/v1/state", &state)?;
        Ok(MutationResult::Success(()))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn format_ten_preparation_rebuilds_request_counts_and_is_repeatable_without_source_edits() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    for name in ["bob", "carol", "dan"] {
        request(&cell, name, true).await;
    }
    review(&cell, "bob", ReviewState::Approved).await;
    let expected = page(&cell, Some(&"b".repeat(40))).await.reviews;
    let before = cell
        .client
        .query::<GetPull>(&cell.target, None, 1)
        .await
        .unwrap()
        .output;
    write::<PreviousFormat>(&cell, ()).await;
    for _ in 0..2 {
        let status = cell
            .client
            .query::<GetSearchIndexStatus>(&cell.target, None, ())
            .await
            .unwrap()
            .output;
        let old = match status {
            SearchIndexStatus::Outdated { generation, .. }
            | SearchIndexStatus::Ready { generation, .. } => generation,
            _ => panic!("Unexpected rebuild status"),
        };
        let generation = Uuid::now_v7();
        write::<RebuildIndex>(
            &cell,
            RebuildSearchInput::Begin {
                expected: Some(old),
                generation,
            },
        )
        .await;
        loop {
            let status = cell
                .client
                .query::<GetSearchIndexStatus>(&cell.target, None, ())
                .await
                .unwrap()
                .output;
            let SearchIndexStatus::Building { progress, .. } = status else {
                break;
            };
            assert!(
                cell.client
                    .query::<GetRollups>(
                        &cell.target,
                        None,
                        Input {
                            heads: vec![Head {
                                number: 1,
                                version: before.as_ref().unwrap().version,
                                oid: Some("b".repeat(40))
                            }],
                            source_token: None
                        }
                    )
                    .await
                    .is_err()
            );
            write::<RebuildIndex>(
                &cell,
                RebuildSearchInput::Step {
                    generation,
                    version: progress.version,
                },
            )
            .await;
        }
        assert_eq!(page(&cell, Some(&"b".repeat(40))).await.reviews, expected);
    }
    assert_eq!(
        cell.client
            .query::<GetPull>(&cell.target, None, 1)
            .await
            .unwrap()
            .output,
        before
    );
    fixtures::close(&server).await;
}
