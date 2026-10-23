use super::*;
use crate::{cells::RepositoryCell, test_support::server as fixtures};
use uuid::Uuid;

fn actor(issuer: &str, subject: &str) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: issuer.into(),
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
            author: actor("one", "author"),
            draft: false,
            title: "Reviewer decisions".into(),
            body: String::new(),
            base_ref: "refs/heads/main".into(),
            head_ref: "refs/heads/topic".into(),
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
        },
    )
    .await;
}
async fn page(cell: &RepositoryCell, after: Option<String>, limit: u8) -> Page {
    cell.client
        .query::<ListReviewers>(
            &cell.target,
            None,
            Query {
                pull: 1,
                after,
                limit,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap()
}
async fn request(cell: &RepositoryCell, author: &RepositoryAuthor, requested: bool) {
    let pull = cell
        .client
        .query::<GetPull>(&cell.target, None, 1)
        .await
        .unwrap()
        .output
        .unwrap();
    let outcome = write::<review_requests::SetReviewRequest>(
        cell,
        review_requests::Selection {
            pull: 1,
            version: pull.version,
            actor: actor("one", "author"),
            can_manage: true,
            reviewer: author.clone(),
            requested,
        },
    )
    .await;
    assert!(matches!(outcome, review_requests::Outcome::Applied(_)));
}
fn review(author: &RepositoryAuthor, state: ReviewState) -> CreatePullReviewInput {
    CreatePullReviewInput {
        pull: 1,
        submission_id: *Uuid::now_v7().as_bytes(),
        author: author.clone(),
        body: "Feedback".into(),
        state,
        commit_oid: "b".repeat(40),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn participant_rows_keep_latest_decisions_and_comments_without_invitations() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    let bob = actor("one", "bob");
    let other = actor("two", "bob");
    request(&cell, &bob, true).await;
    write::<CreatePullReview>(&cell, review(&bob, ReviewState::Approved)).await;
    let comment = review(&bob, ReviewState::Commented);
    write::<CreatePullReview>(&cell, comment.clone()).await;
    write::<CreatePullReview>(&cell, review(&other, ReviewState::Commented)).await;
    let rows = page(&cell, None, 50).await.items;
    assert_eq!(rows.len(), 2);
    let current = rows.iter().find(|row| row.author == bob).unwrap();
    assert_eq!(
        (
            current.review.as_ref().unwrap().number,
            current.decision.as_ref().unwrap().number
        ),
        (2, 1)
    );
    assert_eq!(
        current.decision.as_ref().unwrap().state,
        ReviewState::Approved
    );
    let uninvited = rows.iter().find(|row| row.author == other).unwrap();
    assert!(uninvited.request.is_none() && uninvited.decision.is_none());
    request(&cell, &bob, true).await;
    write::<CreatePullReview>(&cell, comment).await;
    let rows = page(&cell, None, 50).await.items;
    assert_eq!(
        rows.iter()
            .find(|row| row.author == bob)
            .unwrap()
            .request
            .as_ref()
            .unwrap()
            .state,
        review_requests::State::Requested
    );
    write::<CreatePullReview>(&cell, review(&bob, ReviewState::ChangesRequested)).await;
    request(&cell, &bob, false).await;
    let rows = page(&cell, None, 50).await.items;
    let current = rows.iter().find(|row| row.author == bob).unwrap();
    assert_eq!(
        current.decision.as_ref().unwrap().state,
        ReviewState::ChangesRequested
    );
    assert_eq!(current.review.as_ref().unwrap().number, 4);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn participant_pages_seek_exclusively_and_cancelled_invitations_do_not_fill_pages() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    for i in 0..17 {
        let author = actor("one", &format!("reviewer-{i}"));
        request(&cell, &author, true).await;
        if i % 2 == 0 {
            request(&cell, &author, false).await;
        }
    }
    let all = page(&cell, None, 50).await;
    assert_eq!(all.items.len(), 8);
    let mut after = None;
    let mut items = Vec::new();
    loop {
        let next = page(&cell, after, 3).await;
        items.extend(next.items);
        after = next.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(items, all.items);
    for (after, limit) in [(None, 0), (None, 51), (Some("invalid".into()), 1)] {
        assert!(
            cell.client
                .query::<ListReviewers>(
                    &cell.target,
                    None,
                    Query {
                        pull: 1,
                        after,
                        limit
                    }
                )
                .await
                .is_err()
        );
    }
    fixtures::close(&server).await;
}

pub(in crate::cells::repository) struct PreviousIndex;
impl Mutation for PreviousIndex {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 65_528;
    type Input = ();
    type Output = ();
    fn execute(context: &mut Transaction<'_>, _: ()) -> crate::cells::Result<MutationResult<()>> {
        let rows=context.query("SELECT pull,identity,reviewed,requested FROM discussion_index_reviewers ORDER BY pull,identity LIMIT 20",&[])?;
        context.install_schema("DROP TABLE discussion_index_reviewers; CREATE TABLE discussion_index_reviewers (pull INTEGER NOT NULL REFERENCES repository_pulls(number) ON DELETE CASCADE, identity BLOB NOT NULL CHECK(length(identity)=32), reviewed INTEGER NOT NULL CHECK(reviewed IN (0,1)), requested INTEGER NOT NULL CHECK(requested IN (0,1)), PRIMARY KEY(pull,identity)) STRICT, WITHOUT ROWID;")?;
        for row in rows.rows {
            context.query(
                "INSERT INTO discussion_index_reviewers VALUES (?,?,?,?)",
                &row,
            )?;
        }
        let mut state: serde_json::Value = context.document("discussion-search/v1/state")?.unwrap();
        state["format"] = serde_json::json!(8);
        context.put_document("discussion-search/v1/state", &state)?;
        Ok(MutationResult::Success(()))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_bounded_rebuild_reconstructs_latest_reviews_from_format_eight() {
    use search::index::{
        GetSearchIndexStatus, RebuildIndex, RebuildSearchInput, SearchIndexPhase, SearchIndexStatus,
    };
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    let bob = actor("one", "bob");
    request(&cell, &bob, true).await;
    for _ in 0..17 {
        write::<CreatePullReview>(&cell, review(&bob, ReviewState::Commented)).await;
    }
    request(&cell, &bob, true).await;
    let before = page(&cell, None, 50).await;
    let pull_before = cell
        .client
        .query::<GetPull>(&cell.target, None, 1)
        .await
        .unwrap()
        .output;
    write::<PreviousIndex>(&cell, ()).await;
    let observed = cell
        .client
        .query::<GetSearchIndexStatus>(&cell.target, None, ())
        .await
        .unwrap()
        .output;
    let SearchIndexStatus::Outdated {
        generation: old,
        format: 8,
    } = observed
    else {
        panic!("expected format-eight preparation");
    };
    assert!(
        cell.client
            .query::<ListReviewers>(
                &cell.target,
                None,
                Query {
                    pull: 1,
                    after: None,
                    limit: 50
                }
            )
            .await
            .is_err()
    );
    let generation = Uuid::now_v7();
    write::<RebuildIndex>(
        &cell,
        RebuildSearchInput::Begin {
            expected: Some(old),
            generation,
        },
    )
    .await;
    let mut review_steps = 0;
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
        if progress.phase == SearchIndexPhase::Reviews {
            review_steps += 1;
        }
        write::<RebuildIndex>(
            &cell,
            RebuildSearchInput::Step {
                generation,
                version: progress.version,
            },
        )
        .await;
    }
    assert!(review_steps >= 3);
    let after = page(&cell, None, 50).await;
    assert_eq!(after.items, before.items);
    assert_eq!(after.version, before.version);
    assert_eq!(
        cell.client
            .query::<GetPull>(&cell.target, None, 1)
            .await
            .unwrap()
            .output,
        pull_before
    );
    fixtures::close(&server).await;
}
