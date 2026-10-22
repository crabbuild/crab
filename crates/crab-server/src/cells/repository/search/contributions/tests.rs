use super::*;
use crate::{
    cells::repository::{
        search::{Query, index, people::Binding},
        *,
    },
    cells::{
        RepositoryCell,
        application::{Mutation, MutationResult},
    },
    test_support::server as fixtures,
};
use uuid::Uuid;

fn actor(issuer: &str, subject: &str) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: issuer.into(),
        subject: subject.into(),
        name: "Recorded name".into(),
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
async fn search(
    cell: &RepositoryCell,
    source: &str,
    actor: &RepositoryAuthor,
) -> index::Page<DiscussionSummary> {
    let query = Query::parse(Some(source)).unwrap();
    let people = query
        .people()
        .into_iter()
        .map(|name| Binding {
            name,
            identity: Some(super::super::people::identity(&actor.issuer, &actor.subject).unwrap()),
        })
        .collect();
    cell.client
        .query::<ListDiscussions>(
            &cell.target,
            None,
            SearchInput {
                query: Some(source.into()),
                people,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output
}
fn subjects(page: &index::Page<DiscussionSummary>) -> Vec<(u8, u64)> {
    let mut items = page
        .items
        .iter()
        .map(|item| match item {
            DiscussionSummary::Issue(issue) => (0, issue.number),
            DiscussionSummary::Pull(pull) => (1, pull.number),
        })
        .collect::<Vec<_>>();
    items.sort_unstable();
    items
}

#[test]
fn people_bindings_deduplicate_all_relationships_and_preserve_boolean_identity() {
    let query = Query::parse(Some(
        "author:ALICE (commenter:alice OR reviewed-by:bob) -commenter:@me",
    ))
    .unwrap();
    assert_eq!(
        query.people().into_iter().collect::<Vec<_>>(),
        ["@me", "alice", "bob"]
    );
    let bindings = [
        Binding {
            name: "@me".into(),
            identity: Some([1; 32]),
        },
        Binding {
            name: "alice".into(),
            identity: Some([2; 32]),
        },
        Binding {
            name: "bob".into(),
            identity: None,
        },
    ];
    assert!(query.validate_people(&bindings).is_ok());
    assert!(query.validate_people(&bindings[1..]).is_err());
    let mut reversed = bindings.clone();
    reversed.reverse();
    assert!(query.validate_people(&reversed).is_err());
    for source in [
        "commenter:",
        "commenter:*",
        "commenter:@alice",
        "commenter:team/name",
        "commenter:a,b",
    ] {
        assert!(Query::parse(Some(source)).is_err(), "{source}");
    }
    assert!(
        Query::parse(Some("\"commenter:alice\""))
            .unwrap()
            .people()
            .is_empty()
    );
}

// Exercise source-deletion triggers through the same native SQL transaction;
// no public delete API or production test operation is introduced.
pub(in crate::cells::repository) struct DeleteSource;
impl Mutation for DeleteSource {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 65_529;
    type Input = Target;
    type Output = ();
    fn execute(context: &mut Transaction<'_>, target: Target) -> Result<MutationResult<()>> {
        match target {
            Target::IssueComment { issue, number } => {
                context.query(
                    "DELETE FROM repository_issue_comments WHERE issue_number = ? AND number = ?",
                    &[integer(issue)?, integer(number)?],
                )?;
            }
            Target::PullThread { pull, number } => {
                context.query("DELETE FROM repository_pull_review_threads WHERE pull_number = ? AND number = ?", &[integer(pull)?, integer(number)?])?;
            }
            Target::Issue { number } => {
                context.query(
                    "DELETE FROM repository_issues WHERE number = ?",
                    &[integer(number)?],
                )?;
            }
            Target::Pull { number } => {
                context.query(
                    "DELETE FROM repository_pulls WHERE number = ?",
                    &[integer(number)?],
                )?;
            }
            _ => return Err(Error::Invalid("Unsupported deletion fixture target")),
        }
        Ok(MutationResult::Success(()))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn qualified_commenters_cover_all_sources_survive_rebuild_and_follow_source_deletion() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let owner = actor("https://a.test", "owner");
    let issue_commenter = actor("https://a.test", "bob");
    let pull_commenter = actor("https://b.test", "bob");
    let reviewer = actor("https://a.test", "reviewer");
    let thread_author = actor("https://a.test", "thread");
    let reply_author = actor("https://a.test", "reply");
    for number in 1..=2 {
        assert!(matches!(
            write::<CreateIssue>(
                &cell,
                CreateIssueInput {
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: owner.clone(),
                    title: format!("Issue {number}"),
                    body: "Description".into(),
                }
            )
            .await,
            CreateIssueOutcome::Created(_)
        ));
        assert!(matches!(
            write::<CreatePull>(
                &cell,
                CreatePullInput {
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: owner.clone(),
                    draft: false,
                    title: format!("Pull {number}"),
                    body: String::new(),
                    base_ref: "refs/heads/main".into(),
                    head_ref: format!("refs/heads/feature-{number}"),
                    base_oid: "a".repeat(40),
                    head_oid: "b".repeat(40),
                }
            )
            .await,
            CreatePullOutcome::Created(_)
        ));
    }
    for i in 0..18 {
        let number = if i < 9 { 1 } else { 2 };
        let comment = CreateCommentInput {
            submission_id: *Uuid::now_v7().as_bytes(),
            issue: number,
            author: issue_commenter.clone(),
            body: "Issue comment".into(),
        };
        assert!(matches!(
            write::<CreateComment>(&cell, comment.clone()).await,
            CreateCommentOutcome::Created(_)
        ));
        assert!(matches!(
            write::<CreateComment>(&cell, comment).await,
            CreateCommentOutcome::Created(_)
        ));
        assert!(matches!(
            write::<CreatePullComment>(
                &cell,
                CreatePullCommentInput {
                    submission_id: *Uuid::now_v7().as_bytes(),
                    pull: number,
                    author: pull_commenter.clone(),
                    body: "PR conversation".into(),
                }
            )
            .await,
            CreatePullCommentOutcome::Created(_)
        ));
        assert!(matches!(
            write::<CreatePullReview>(
                &cell,
                CreatePullReviewInput {
                    submission_id: *Uuid::now_v7().as_bytes(),
                    pull: number,
                    author: reviewer.clone(),
                    body: String::new(),
                    state: ReviewState::Approved,
                    commit_oid: "b".repeat(40),
                }
            )
            .await,
            CreatePullReviewOutcome::Created(_)
        ));
        let CreatePullReviewThreadOutcome::Created(thread) = write::<CreatePullReviewThread>(
            &cell,
            CreatePullReviewThreadInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                pull: number,
                author: thread_author.clone(),
                body: "Inline comment".into(),
                suggested_text: None,
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
                path: b"file.rs".to_vec(),
                old_blob_oid: None,
                new_blob_oid: Some("c".repeat(40)),
                side: PullReviewThreadSide::New,
                start_line: 1,
                end_line: 1,
            },
        )
        .await
        else {
            panic!("expected thread")
        };
        assert!(matches!(
            write::<CreatePullReviewReply>(
                &cell,
                CreatePullReviewReplyInput {
                    submission_id: *Uuid::now_v7().as_bytes(),
                    pull: number,
                    thread: thread.number,
                    author: reply_author.clone(),
                    body: "Inline reply".into(),
                }
            )
            .await,
            CreatePullReviewReplyOutcome::Created(_)
        ));
    }
    for (person, expected) in [
        (&owner, vec![]),
        (&issue_commenter, vec![(0, 1), (0, 2)]),
        (&pull_commenter, vec![(1, 1), (1, 2)]),
        (&reviewer, vec![(1, 1), (1, 2)]),
        (&thread_author, vec![(1, 1), (1, 2)]),
        (&reply_author, vec![(1, 1), (1, 2)]),
    ] {
        let page = search(&cell, "commenter:@me", person).await;
        assert_eq!(subjects(&page), expected);
        assert_eq!(page.matching, expected.len() as u64);
    }
    assert_eq!(
        subjects(&search(&cell, "commenter:@me OR is:issue", &reviewer).await),
        [(0, 1), (0, 2), (1, 1), (1, 2)]
    );
    assert_eq!(
        subjects(&search(&cell, "-commenter:@me is:pr", &issue_commenter).await),
        [(1, 1), (1, 2)]
    );
    // Editing content refreshes disposable index documents without losing any
    // source participation. An empty approval still counts as a submission.
    assert!(matches!(
        write::<UpdateIssue>(
            &cell,
            UpdateIssueInput {
                state_reason: None,
                number: 1,
                actor: owner.clone(),
                can_triage: true,
                version: 1,
                title: Some("Edited".into()),
                body: None,
                state: None,
                label_ids: None,
                assignees: None,
            }
        )
        .await,
        UpdateIssueOutcome::Updated(_)
    ));
    let before = cell
        .client
        .query::<GetIssue>(&cell.target, None, 1)
        .await
        .unwrap()
        .output;
    let index::SearchIndexStatus::Ready {
        generation: previous,
        ..
    } = cell
        .client
        .query::<index::GetSearchIndexStatus>(&cell.target, None, ())
        .await
        .unwrap()
        .output
    else {
        panic!("expected ready index")
    };
    let generation = Uuid::now_v7();
    write::<index::RebuildIndex>(
        &cell,
        index::RebuildSearchInput::Begin {
            expected: Some(previous),
            generation,
        },
    )
    .await;
    let mut source_frames = 0;
    loop {
        let status = cell
            .client
            .query::<index::GetSearchIndexStatus>(&cell.target, None, ())
            .await
            .unwrap()
            .output;
        let index::SearchIndexStatus::Building { progress, .. } = status else {
            break;
        };
        if progress.phase == index::SearchIndexPhase::Commenters {
            source_frames += 1;
        }
        let input = index::RebuildSearchInput::Step {
            generation,
            version: progress.version,
        };
        let result = write::<index::RebuildIndex>(&cell, input.clone()).await;
        let replay = write::<index::RebuildIndex>(&cell, input).await;
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::to_value(replay).unwrap()
        );
    }
    assert_eq!(source_frames, 15);
    assert_eq!(
        cell.client
            .query::<GetIssue>(&cell.target, None, 1)
            .await
            .unwrap()
            .output,
        before
    );
    for person in [
        &issue_commenter,
        &pull_commenter,
        &reviewer,
        &thread_author,
        &reply_author,
    ] {
        assert_eq!(search(&cell, "commenter:@me", person).await.matching, 2);
    }
    write::<DeleteSource>(
        &cell,
        Target::IssueComment {
            issue: 1,
            number: 1,
        },
    )
    .await;
    assert_eq!(
        search(&cell, "commenter:@me", &issue_commenter)
            .await
            .matching,
        2
    );
    for number in 2..=9 {
        write::<DeleteSource>(&cell, Target::IssueComment { issue: 1, number }).await;
    }
    assert_eq!(
        subjects(&search(&cell, "commenter:@me", &issue_commenter).await),
        [(0, 2)]
    );
    assert_eq!(
        subjects(&search(&cell, "\"inline reply\" in:comments", &reply_author).await),
        [(1, 1), (1, 2)]
    );
    // Thread deletion cascades through replies and both source projections.
    for number in 1..=9 {
        write::<DeleteSource>(&cell, Target::PullThread { pull: 1, number }).await;
    }
    for person in [&thread_author, &reply_author] {
        assert_eq!(
            subjects(&search(&cell, "commenter:@me", person).await),
            [(1, 2)]
        );
    }
    assert_eq!(
        subjects(&search(&cell, "\"inline reply\" in:comments", &reply_author).await),
        [(1, 2)]
    );
    write::<DeleteSource>(&cell, Target::Pull { number: 2 }).await;
    for person in [&thread_author, &reply_author] {
        assert_eq!(search(&cell, "commenter:@me", person).await.matching, 0);
    }
    assert_eq!(
        subjects(&search(&cell, "commenter:@me", &reviewer).await),
        [(1, 1)]
    );
    assert_eq!(
        subjects(&search(&cell, "commenter:@me", &pull_commenter).await),
        [(1, 1)]
    );
    write::<DeleteSource>(&cell, Target::Issue { number: 2 }).await;
    assert_eq!(
        search(&cell, "commenter:@me", &issue_commenter)
            .await
            .matching,
        0
    );
    assert_eq!(
        search(&cell, "\"inline reply\" in:comments", &reply_author)
            .await
            .matching,
        0
    );
    assert!(matches!(
        write::<CreateIssue>(
            &cell,
            CreateIssueInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: owner.clone(),
                title: "New discussion".into(),
                body: String::new(),
            }
        )
        .await,
        CreateIssueOutcome::Created(_)
    ));
    assert!(matches!(
        write::<CreateComment>(
            &cell,
            CreateCommentInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                issue: 3,
                author: owner.clone(),
                body: "Rowreuseneedle".into(),
            }
        )
        .await,
        CreateCommentOutcome::Created(_)
    ));
    assert_eq!(
        subjects(&search(&cell, "rowreuseneedle in:comments", &owner).await),
        [(0, 3)]
    );
    assert_eq!(
        search(&cell, "\"inline comment\" in:comments", &owner)
            .await
            .matching,
        0
    );
    fixtures::close(&server).await;
}
