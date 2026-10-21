use super::*;
use crate::cells::{
    RepositoryCell,
    application::Mutation,
    repository::{
        collaboration::{Decision, Subject, SubjectKind, milestones::*},
        *,
    },
};
use crate::test_support::server as fixtures;
use uuid::Uuid;

fn author() -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: "alice".into(),
        name: "Alice".into(),
    }
}
async fn write<M: Mutation>(cell: &RepositoryCell, input: M::Input) -> M::Output {
    cell.client
        .command::<M>(cell.target, crate::app::mutation_identity().unwrap(), input)
        .await
        .unwrap()
        .output
}
async fn page(cell: &RepositoryCell, query: &str) -> index::Page<DiscussionSummary> {
    cell.client
        .query::<ListDiscussions>(
            cell.target,
            None,
            index::SearchInput {
                query: Some(query.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output
}
fn applied<T>(value: Decision<T>) -> T {
    match value {
        Decision::Applied(value) => value,
        _ => panic!("milestone command failed"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn milestone_queries_share_assignments_counts_renames_and_rebuilds_for_both_discussion_kinds()
{
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let milestone = applied(
        write::<CreateMilestone>(
            &cell,
            CreateMilestoneInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: author(),
                title: "Release 🦀, \"one\"".into(),
                description: String::new(),
                due_on: None,
            },
        )
        .await,
    );
    for _ in 0..3 {
        write::<CreateIssue>(
            &cell,
            CreateIssueInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: author(),
                title: "Issue".into(),
                body: String::new(),
            },
        )
        .await;
    }
    write::<CreatePull>(
        &cell,
        CreatePullInput {
            submission_id: *Uuid::now_v7().as_bytes(),
            author: author(),
            draft: false,
            title: "Pull".into(),
            body: String::new(),
            base_ref: "refs/heads/main".into(),
            head_ref: "refs/heads/feature".into(),
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
        },
    )
    .await;
    for kind in [SubjectKind::Issue, SubjectKind::Pull] {
        applied(
            write::<AssignMilestone>(
                &cell,
                AssignMilestoneInput {
                    subject: Subject { kind, number: 1 },
                    milestone: Some(milestone.number),
                    version: 0,
                    actor: author(),
                },
            )
            .await,
        );
    }
    let query = format!(
        "milestone:{}",
        serde_json::to_string(&milestone.title.to_uppercase()).unwrap()
    );
    let matched = page(&cell, &query).await;
    assert_eq!((matched.matching, matched.counts.open), (2, 2));
    assert_eq!(page(&cell, "no:milestone").await.matching, 2);
    assert_eq!(page(&cell, &format!("-{query}")).await.matching, 2);
    assert_eq!(
        page(&cell, &format!("({query} OR no:milestone) is:issue"))
            .await
            .matching,
        3
    );
    let before = cell
        .client
        .query::<ListDiscussions>(
            cell.target,
            None,
            index::SearchInput {
                query: Some(query.clone()),
                limit: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output
        .next
        .unwrap();
    let current = cell
        .client
        .query::<GetMilestone>(cell.target, None, milestone.number)
        .await
        .unwrap()
        .output
        .unwrap();
    let renamed = applied(
        write::<EditMilestone>(
            &cell,
            EditMilestoneInput {
                number: milestone.number,
                version: current.version,
                actor: author(),
                title: "Release two".into(),
                description: String::new(),
                due_on: None,
                state: Some(MilestoneState::Closed),
            },
        )
        .await,
    );
    assert_eq!(page(&cell, &query).await.matching, 0);
    assert_eq!(page(&cell, "milestone:\"release TWO\"").await.matching, 2);
    assert!(matches!(
        cell.client
            .query::<ListDiscussions>(
                cell.target,
                None,
                index::SearchInput {
                    query: Some(query),
                    before: Some(before),
                    limit: 1,
                    ..Default::default()
                }
            )
            .await,
        Err(Error::StaleSearch)
    ));
    let options = cell
        .client
        .query::<MilestoneOptions>(
            cell.target,
            None,
            OptionInput {
                query: Some("is:open milestone:\"release two\"".into()),
                search: "two".into(),
                state: Some(MilestoneState::Closed),
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(options.items.len(), 1);
    assert!(options.items[0].choice.selected);
    let completion = cell
        .client
        .query::<super::super::completion::CompleteSearch>(
            cell.target,
            None,
            super::super::completion::CompletionInput {
                query: "-milestone:\"release t".into(),
                cursor: 21,
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(completion.items[0].value, "-milestone:\"Release two\"");
    let assignment = applied(
        write::<AssignMilestone>(
            &cell,
            AssignMilestoneInput {
                subject: Subject {
                    kind: SubjectKind::Issue,
                    number: 1,
                },
                milestone: None,
                version: 1,
                actor: author(),
            },
        )
        .await,
    );
    assert_eq!(page(&cell, "no:milestone").await.matching, 3);
    assert_eq!(page(&cell, "milestone:\"release two\"").await.matching, 1);
    let original = serde_json::to_value(
        cell.client
            .query::<GetMilestone>(cell.target, None, renamed.number)
            .await
            .unwrap()
            .output
            .unwrap(),
    )
    .unwrap();
    let generation = Uuid::now_v7();
    let status = cell
        .client
        .query::<index::GetSearchIndexStatus>(cell.target, None, ())
        .await
        .unwrap()
        .output;
    let index::SearchIndexStatus::Ready {
        generation: old, ..
    } = status
    else {
        panic!("index not ready")
    };
    write::<index::RebuildIndex>(
        &cell,
        index::RebuildSearchInput::Begin {
            expected: Some(old),
            generation,
        },
    )
    .await;
    for step in 0..30 {
        let state = cell
            .client
            .query::<index::GetSearchIndexStatus>(cell.target, None, ())
            .await
            .unwrap()
            .output;
        let index::SearchIndexStatus::Building { progress, .. } = state else {
            break;
        };
        assert!(step < 29);
        write::<index::RebuildIndex>(
            &cell,
            index::RebuildSearchInput::Step {
                generation,
                version: progress.version,
            },
        )
        .await;
    }
    assert_eq!(page(&cell, "milestone:\"release two\"").await.matching, 1);
    let restored = cell
        .client
        .query::<GetMilestone>(cell.target, None, renamed.number)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(serde_json::to_value(restored).unwrap(), original);
    let restored_assignment = cell
        .client
        .query::<GetAssignment>(
            cell.target,
            None,
            Subject {
                kind: SubjectKind::Issue,
                number: 1,
            },
        )
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(restored_assignment, assignment);
    fixtures::close(&server).await;
}

#[test]
fn milestone_destinations_preserve_boolean_terms_quotes_and_unrestricted_state() {
    for (query, expected) in [
        (
            "label:\"Needs Work\" is:closed milestone:old",
            "label:\"Needs Work\" is:closed milestone:\"New name\"",
        ),
        (
            "(milestone:old OR label:Bug) -milestone:other",
            "(milestone:old OR label:Bug) milestone:\"New name\"",
        ),
        ("no:milestone", "milestone:\"New name\""),
    ] {
        let query = Query::parse(Some(query)).unwrap();
        assert_eq!(
            choice(&query, Filter::Title("New name"))
                .unwrap()
                .query
                .as_deref(),
            Some(expected)
        );
    }
    let query = Query::parse(Some("milestone:\"a,b\\\"c\\\\d\"")).unwrap();
    assert!(choice(&query, Filter::Title("A,b\"c\\d")).unwrap().selected);
    assert_eq!(
        choice(&query, Filter::Clear).unwrap().query.as_deref(),
        Some("is:issue OR is:pr")
    );
    assert!(Query::parse(Some("-no:milestone")).is_err());
    let query = Query::parse(Some(&"x".repeat(256))).unwrap();
    assert!(choice(&query, Filter::Missing).unwrap().query.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn options_search_the_complete_catalog_with_bounded_output_and_exact_matches_first() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    for number in 0..24 {
        applied(
            write::<CreateMilestone>(
                &cell,
                CreateMilestoneInput {
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: author(),
                    title: format!("Release {number:02}"),
                    description: String::new(),
                    due_on: None,
                },
            )
            .await,
        );
    }
    let all = cell
        .client
        .query::<MilestoneOptions>(
            cell.target,
            None,
            OptionInput {
                query: None,
                search: "release".into(),
                state: None,
            },
        )
        .await
        .unwrap()
        .output;
    assert!(all.truncated);
    assert_eq!(all.items.len(), 20);
    let exact = cell
        .client
        .query::<MilestoneOptions>(
            cell.target,
            None,
            OptionInput {
                query: None,
                search: "release 23".into(),
                state: None,
            },
        )
        .await
        .unwrap()
        .output;
    assert!(!exact.truncated);
    assert_eq!(exact.items[0].milestone.title, "Release 23");
    assert_eq!(
        exact.items[0].choice.query.as_deref(),
        Some("milestone:\"Release 23\"")
    );
    fixtures::close(&server).await;
}
