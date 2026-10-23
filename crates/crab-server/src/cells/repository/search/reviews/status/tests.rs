use super::*;
use crate::cells::repository::search::index;
use crate::cells::{RepositoryCell, application::Mutation, repository::*};
use crate::test_support::server as fixtures;
use uuid::Uuid;

fn actor(name: &str) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: name.into(),
        name: name.into(),
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
async fn pull(cell: &RepositoryCell, number: u64) {
    assert!(matches!(
        write::<CreatePull>(
            cell,
            CreatePullInput {
                metadata: Default::default(),
                draft: false,
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor("Alice"),
                title: format!("Review {number}"),
                body: String::new(),
                base_ref: "refs/heads/main".into(),
                head_ref: format!("refs/heads/topic-{number}"),
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40)
            }
        )
        .await,
        CreatePullOutcome::Created(_)
    ));
}
async fn review(
    cell: &RepositoryCell,
    pull: u64,
    who: &str,
    state: ReviewState,
    oid: &str,
) -> PullReviewRecord {
    let CreatePullReviewOutcome::Created(record) = write::<CreatePullReview>(
        cell,
        CreatePullReviewInput {
            pull,
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor(who),
            body: "Review feedback".into(),
            state,
            commit_oid: oid.into(),
        },
    )
    .await
    else {
        panic!("review rejected")
    };
    record
}
async fn refs(cell: &RepositoryCell, generation: u64, heads: &[(u64, &str)]) -> String {
    use crate::cells::repository::projection::*;
    let token = format!("{generation:064x}");
    let source = SourceIdentity {
        source_token: token.clone(),
        manifest_generation: generation,
        manifest_etag: format!("etag-{generation}"),
        journal_state_digest: String::new(),
        pack_index_hash: String::new(),
        git_validation_digest: String::new(),
        commit_graph_hash: None,
        path_state_hash: None,
        head_ref: b"refs/heads/main".to_vec(),
    };
    let source = serde_json::to_vec(&source).unwrap();
    let started = write::<ApplyProjectionBatch>(
        cell,
        ProjectionBatch {
            operation: OP_BEGIN,
            epoch_id: 0,
            source: source.clone(),
            payload: vec![],
        },
    )
    .await;
    let epoch_id = started.epoch_id.unwrap();
    let rows: Vec<_> = heads
        .iter()
        .map(|(number, oid)| RefRow {
            name: format!("refs/heads/topic-{number}").into_bytes(),
            target_oid: gix_hash::ObjectId::from_hex(oid.as_bytes())
                .unwrap()
                .as_bytes()
                .to_vec(),
            peeled_oid: None,
        })
        .collect();
    write::<ApplyProjectionBatch>(
        cell,
        ProjectionBatch {
            operation: OP_REFS,
            epoch_id,
            source: source.clone(),
            payload: serde_json::to_vec(&rows).unwrap(),
        },
    )
    .await;
    write::<ApplyProjectionBatch>(
        cell,
        ProjectionBatch {
            operation: OP_PROMOTE,
            epoch_id,
            source,
            payload: vec![],
        },
    )
    .await;
    token
}
fn input(query: &str, token: Option<&str>, approvals: u8) -> SearchInput {
    SearchInput {
        query: Some(query.into()),
        review_context: Query::parse(Some(query))
            .unwrap()
            .needs_review_source()
            .then(|| Context {
                source_token: token.map(str::to_owned),
                configured_rules: vec![BranchProtectionRecord {
                    branch: "main".into(),
                    required_approvals: approvals,
                    required_checks: vec![],
                }],
            }),
        ..Default::default()
    }
}
async fn matches(cell: &RepositoryCell, query: &str, token: &str, approvals: u8) -> u64 {
    read::<ListDiscussions>(cell, input(query, Some(token), approvals))
        .await
        .matching
}

#[test]
fn menu_choices_preserve_boolean_scopes_and_replace_global_review_filters() {
    let query = Query::parse(Some("is:open -reviewed-by:@me label:Bug")).unwrap();
    let menu = query.review_menu();
    assert!(menu.items[5].choice.selected);
    assert_eq!(
        menu.items[2].choice.query.as_deref(),
        Some("is:open label:Bug review:approved")
    );
    let query = Query::parse(Some("(review:approved OR label:bug) is:open")).unwrap();
    assert_eq!(
        query.review_menu().items[0].choice.query.as_deref(),
        Some("(review:approved OR label:bug) is:open review:none")
    );
    for invalid in ["review:unknown", "review:", "-review:pending"] {
        assert!(Query::parse(Some(invalid)).is_err(), "{invalid}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn review_status_counts_follow_current_decisions_and_preserve_mixed_boolean_queries() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    for number in 1..=8 {
        pull(&cell, number).await;
    }
    let b = "b".repeat(40);
    let c = "c".repeat(40);
    review(&cell, 2, "Bob", ReviewState::Commented, &b).await;
    review(&cell, 3, "Bob", ReviewState::Approved, &b).await;
    review(&cell, 4, "Bob", ReviewState::ChangesRequested, &b).await;
    review(&cell, 5, "Bob", ReviewState::Approved, &b).await;
    review(&cell, 5, "Carol", ReviewState::ChangesRequested, &b).await;
    review(&cell, 6, "Bob", ReviewState::Approved, &b).await;
    review(&cell, 6, "Carol", ReviewState::Approved, &b).await;
    let old = review(&cell, 7, "Bob", ReviewState::Approved, &b).await;
    review(&cell, 8, "Bob", ReviewState::ChangesRequested, &b).await;
    let heads: Vec<_> = (1..=8)
        .map(|number| (number, if number >= 7 { c.as_str() } else { b.as_str() }))
        .collect();
    let token = refs(&cell, 1, &heads).await;
    write::<CreateIssue>(
        &cell,
        CreateIssueInput {
            metadata: Default::default(),
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor("Alice"),
            title: "Unrelated issue".into(),
            body: String::new(),
        },
    )
    .await;
    for (query, expected) in [
        ("review:none", 2),
        ("review:approved", 2),
        ("review:changes_requested", 3),
        ("review:required", 3),
        ("-review:none", 7),
        ("is:issue OR review:none", 3),
        ("(review:approved OR review:changes_requested) -is:issue", 5),
    ] {
        assert_eq!(matches(&cell, query, &token, 1).await, expected, "{query}");
    }
    assert_eq!(matches(&cell, "review:approved", &token, 2).await, 1);
    write::<ReplaceBranchProtections>(
        &cell,
        ReplaceBranchProtectionsInput {
            expected_version: 0,
            rules: vec![],
        },
    )
    .await;
    assert_eq!(matches(&cell, "review:required", &token, 2).await, 0);
    assert_eq!(matches(&cell, "review:approved", &token, 2).await, 2);
    let outcome = write::<review_dismissals::DismissReview>(
        &cell,
        review_dismissals::Input {
            submission_id: *Uuid::now_v7().as_bytes(),
            key: PullChildKey {
                pull: 7,
                number: old.number,
            },
            version: old.version,
            actor: actor("Alice"),
            body: "Superseded".into(),
            can_write: true,
            can_admin: true,
            configured_protected: true,
        },
    )
    .await;
    assert_eq!(outcome, review_dismissals::Outcome::Dismissed);
    assert_eq!(matches(&cell, "review:none", &token, 2).await, 3);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn source_changes_invalidate_cursors_and_rollups_retain_the_filtered_snapshot() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let b = "b".repeat(40);
    let c = "c".repeat(40);
    for number in 1..=3 {
        pull(&cell, number).await;
        review(&cell, number, "Bob", ReviewState::Approved, &b).await;
    }
    let first = refs(&cell, 1, &[(1, &b), (2, &b), (3, &b)]).await;
    let mut search = input("review:approved", Some(&first), 1);
    search.limit = 1;
    let page = read::<ListDiscussions>(&cell, search.clone()).await;
    assert_eq!(
        (page.items.len(), page.matching, page.counts.open),
        (1, 3, 3)
    );
    let selected = read::<GetPull>(&cell, 3).await.unwrap();
    let rollups = read::<rollups::GetRollups>(
        &cell,
        rollups::Input {
            heads: vec![rollups::Head {
                number: 3,
                version: selected.version,
                oid: Some(c.clone()),
            }],
            source_token: Some(first.clone()),
        },
    )
    .await
    .unwrap();
    assert_eq!(rollups[0].head_oid.as_deref(), Some(b.as_str()));
    // Protection edits do not touch discussion rows, but change the filter's
    // result set. Their effective rules must invalidate the paging cursor too.
    write::<ReplaceBranchProtections>(
        &cell,
        ReplaceBranchProtectionsInput {
            expected_version: 0,
            rules: vec![BranchProtectionRecord {
                branch: "main".into(),
                required_approvals: 2,
                required_checks: vec![],
            }],
        },
    )
    .await;
    let mut changed_rules = search.clone();
    changed_rules.before = page.next.clone();
    assert!(matches!(
        cell.client
            .query::<ListDiscussions>(&cell.target, None, changed_rules)
            .await
            .err()
            .unwrap(),
        Error::StaleSearch
    ));
    write::<ReplaceBranchProtections>(
        &cell,
        ReplaceBranchProtectionsInput {
            expected_version: 1,
            rules: vec![BranchProtectionRecord {
                branch: "main".into(),
                required_approvals: 1,
                required_checks: vec![],
            }],
        },
    )
    .await;
    let second = refs(&cell, 2, &[(1, &b), (2, &b), (3, &c)]).await;
    let error = cell
        .client
        .query::<ListDiscussions>(&cell.target, None, search.clone())
        .await
        .err()
        .unwrap();
    assert!(matches!(error, Error::StaleSearch));
    search.review_context.as_mut().unwrap().source_token = Some(second.clone());
    search.before = page.next;
    assert!(matches!(
        cell.client
            .query::<ListDiscussions>(&cell.target, None, search)
            .await
            .err()
            .unwrap(),
        Error::StaleSearch
    ));
    assert_eq!(matches(&cell, "review:approved", &second, 1).await, 2);
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_repository_needs_no_git_source_and_comment_only_reviews_are_unreviewed() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    assert!(read::<GetSourceState>(&cell, ()).await.is_none());
    assert_eq!(
        read::<ListDiscussions>(&cell, input("review:approved", None, 1))
            .await
            .matching,
        0
    );
    pull(&cell, 1).await;
    review(&cell, 1, "Bob", ReviewState::Commented, &"b".repeat(40)).await;
    assert_eq!(
        read::<ListDiscussions>(&cell, input("review:none", None, 1))
            .await
            .matching,
        1
    );
    assert!(matches!(
        cell.client
            .query::<ListDiscussions>(&cell.target, None, input("review:approved", None, 1))
            .await
            .err()
            .unwrap(),
        Error::StaleSearch
    ));
    fixtures::close(&server).await;
}

pub(in crate::cells::repository) struct PreviousFormat;
impl Mutation for PreviousFormat {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 65_514;
    type Input = ();
    type Output = ();
    fn execute(
        context: &mut Transaction<'_>,
        (): (),
    ) -> Result<crate::cells::application::MutationResult<()>> {
        let key = "discussion-search/v1/state";
        let mut state = context.document::<serde_json::Value>(key)?.unwrap();
        state["format"] = serde_json::json!(11);
        context.put_document(key, &state)?;
        context.install_schema("DROP TABLE discussion_index_pull_sources;")?;
        Ok(crate::cells::application::MutationResult::Success(()))
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn merged_source_heads_survive_branch_deletion_and_explicit_bounded_preparation() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    pull(&cell, 1).await;
    let c = "c".repeat(40);
    review(&cell, 1, "Bob", ReviewState::Approved, &c).await;
    let current = read::<GetPull>(&cell, 1).await.unwrap();
    let merge = PullMerge {
        request_id: *Uuid::now_v7().as_bytes(),
        author: actor("Alice"),
        method: MergeMethod::MergeCommit,
        pull_version: current.version,
        base_oid: "a".repeat(40),
        head_oid: c.clone(),
        commit_oid: "d".repeat(40),
        message: "Merge".into(),
        created_at_ms: 1,
    };
    for transition in [PullMergeTransition::Begin, PullMergeTransition::Complete] {
        assert!(matches!(
            write::<TransitionPullMerge>(
                &cell,
                TransitionPullMergeInput {
                    pull: 1,
                    merge: merge.clone(),
                    transition
                }
            )
            .await,
            TransitionPullMergeOutcome::Applied(_)
        ));
    }
    let token = refs(&cell, 2, &[]).await;
    assert_eq!(
        matches(&cell, "is:merged review:approved", &token, 1).await,
        1
    );
    let before = read::<GetPull>(&cell, 1).await;
    write::<PreviousFormat>(&cell, ()).await;
    assert!(matches!(
        cell.client
            .query::<ListDiscussions>(
                &cell.target,
                None,
                input("review:approved", Some(&token), 1)
            )
            .await
            .err()
            .unwrap(),
        Error::IndexUnavailable
    ));
    let status = read::<index::GetSearchIndexStatus>(&cell, ()).await;
    let index::SearchIndexStatus::Outdated {
        generation: expected,
        ..
    } = status
    else {
        panic!("expected previous format")
    };
    let generation = Uuid::now_v7();
    write::<index::RebuildIndex>(
        &cell,
        index::RebuildSearchInput::Begin {
            expected: Some(expected),
            generation,
        },
    )
    .await;
    loop {
        let status = read::<index::GetSearchIndexStatus>(&cell, ()).await;
        let index::SearchIndexStatus::Building { progress, .. } = status else {
            break;
        };
        write::<index::RebuildIndex>(
            &cell,
            index::RebuildSearchInput::Step {
                generation,
                version: progress.version,
            },
        )
        .await;
    }
    assert_eq!(read::<GetPull>(&cell, 1).await, before);
    assert_eq!(
        matches(&cell, "is:merged review:approved", &token, 1).await,
        1
    );
    let record = before.unwrap();
    let rollup = read::<rollups::GetRollups>(
        &cell,
        rollups::Input {
            heads: vec![rollups::Head {
                number: 1,
                version: record.version,
                oid: None,
            }],
            source_token: Some(token),
        },
    )
    .await
    .unwrap();
    assert_eq!(rollup[0].head_oid.as_deref(), Some(c.as_str()));
    fixtures::close(&server).await;
}
