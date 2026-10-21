use super::*;
use crate::cells::{RepositoryCell, application::Mutation, repository::*};
use crate::test_support::server as fixtures;
use uuid::Uuid;

fn query(value: &str) -> Query {
    Query::parse((!value.is_empty()).then_some(value)).unwrap()
}
#[test]
fn toggling_global_labels_preserves_other_filters_and_comma_or_members() {
    for (source, name, exclude, expected) in [
        (
            "is:open label:bug label:\"Needs work\"",
            "bug",
            false,
            "is:open label:\"Needs work\"",
        ),
        (
            "is:open label:bug,help",
            "BUG",
            false,
            "is:open label:\"help\"",
        ),
        (
            "-label:bug,help",
            "bug",
            false,
            "-label:\"help\" label:\"bug\"",
        ),
        ("label:bug", "bug", true, "-label:\"bug\""),
        ("-label:bug", "bug", true, "is:issue OR is:pr"),
        (
            "no:label milestone:\"Version one\"",
            "Needs work",
            false,
            "milestone:\"Version one\" label:\"Needs work\"",
        ),
        (
            "(label:bug OR label:help) is:closed",
            "help",
            false,
            "(label:bug OR label:help) is:closed label:\"help\"",
        ),
        (
            "\"label:bug\" -label:help",
            "bug",
            false,
            "\"label:bug\" -label:help label:\"bug\"",
        ),
    ] {
        assert_eq!(
            change(&query(source), Filter::Label { name, exclude })
                .unwrap()
                .as_deref(),
            Some(expected),
            "{source}"
        );
    }
    assert_eq!(
        selection(&query("label:bug,help -label:other"), "HELP"),
        (true, false)
    );
    assert_eq!(
        selection(&query("label:bug OR label:help"), "help"),
        (false, false)
    );
    assert_eq!(selection(&query("-label:bug,help"), "help"), (false, true));
    assert_eq!(
        change(&query("label:bug -label:help"), Filter::Missing)
            .unwrap()
            .as_deref(),
        Some("no:label")
    );
    assert_eq!(
        change(&query("no:label"), Filter::Missing)
            .unwrap()
            .as_deref(),
        Some("is:issue OR is:pr")
    );
    assert_eq!(
        change(
            &query("is:open (label:bug OR label:help) -label:other"),
            Filter::Clear
        )
        .unwrap()
        .as_deref(),
        Some("is:open (label:bug OR label:help)")
    );
}

#[test]
fn quoted_label_names_and_query_bounds_never_corrupt_destinations() {
    let name = "A,b \"c\" \\ 🦀";
    let added = change(
        &query("is:open"),
        Filter::Label {
            name,
            exclude: false,
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(selection(&query(&added), name), (true, false));
    assert_eq!(
        change(
            &query(&added),
            Filter::Label {
                name,
                exclude: false
            }
        )
        .unwrap()
        .as_deref(),
        Some("is:open")
    );
    assert!(
        change(
            &query(&"x".repeat(256)),
            Filter::Label {
                name: "Bug",
                exclude: false
            }
        )
        .unwrap()
        .is_none()
    );
}

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
async fn options(cell: &RepositoryCell, source: &str, search: &str) -> Options {
    cell.client
        .query::<LabelOptions>(
            cell.target,
            None,
            OptionInput {
                query: (!source.is_empty()).then(|| source.into()),
                search: search.into(),
            },
        )
        .await
        .unwrap()
        .output
}
async fn count(cell: &RepositoryCell, query: Option<String>) -> u64 {
    cell.client
        .query::<ListDiscussions>(
            cell.target,
            None,
            index::SearchInput {
                query,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output
        .matching
}

#[tokio::test(flavor = "multi_thread")]
async fn native_label_choices_search_the_whole_catalog_and_follow_assignments_renames_and_deletion()
{
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    for index in 0..24 {
        assert!(matches!(
            write::<CreateLabel>(
                &cell,
                CreateLabelInput {
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: author(),
                    name: format!("Label {index:02}"),
                    color: "abcdef".into(),
                    description: Some("Useful context".into())
                }
            )
            .await,
            CreateLabelOutcome::Created(_)
        ));
    }
    for (number, labels) in [(1, vec![1]), (2, vec![1, 24]), (3, vec![])] {
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
        write::<UpdateIssue>(
            &cell,
            UpdateIssueInput {
                number,
                actor: author(),
                can_manage_metadata: true,
                version: 1,
                title: None,
                body: None,
                state: None,
                label_ids: Some(labels),
                assignee_subjects: None,
            },
        )
        .await;
    }
    let all = options(&cell, "is:open", "label").await;
    assert_eq!(all.items.len(), 20);
    assert!(all.truncated);
    let one = options(&cell, "is:open", "label 23").await;
    assert_eq!(one.items[0].number, 24);
    assert_eq!(one.items[0].description.as_deref(), Some("Useful context"));
    assert!(!one.truncated);
    assert_eq!(count(&cell, one.items[0].choice.query.clone()).await, 1);
    let two = options(&cell, "is:open label:\"Label 23\"", "label 00").await;
    assert_eq!(count(&cell, two.items[0].choice.query.clone()).await, 1);
    assert_eq!(count(&cell, two.missing.query.clone()).await, 1);
    let selected = options(&cell, "is:open label:\"Label 00\"", "label 00").await;
    assert!(selected.items[0].choice.selected);
    assert_eq!(
        count(&cell, selected.items[0].choice.query.clone()).await,
        3
    );
    assert_eq!(
        count(&cell, selected.items[0].exclude_query.clone()).await,
        1
    );
    write::<UpdateLabel>(
        &cell,
        UpdateLabelInput {
            number: 24,
            version: 1,
            name: "New name".into(),
            color: "ff0000".into(),
            description: None,
        },
    )
    .await;
    assert!(options(&cell, "", "label 23").await.items.is_empty());
    let renamed = options(&cell, "label:\"New name\"", "new").await;
    assert!(renamed.items[0].choice.selected);
    assert_eq!(renamed.items[0].color, "ff0000");
    assert_eq!(count(&cell, renamed.items[0].choice.query.clone()).await, 3);
    assert!(matches!(
        write::<DeleteLabel>(
            &cell,
            DeleteLabelInput {
                number: 24,
                version: 2
            }
        )
        .await,
        DeleteLabelOutcome::Deleted
    ));
    assert!(options(&cell, "", "new").await.items.is_empty());
    assert_eq!(count(&cell, Some("no:label".into())).await, 1);
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
    write::<UpdatePull>(
        &cell,
        UpdatePullInput {
            number: 1,
            actor: author(),
            can_manage: true,
            version: 1,
            title: None,
            body: None,
            state: None,
            label_ids: Some(vec![1]),
            assignee_subjects: None,
        },
    )
    .await;
    let pull_choice = options(&cell, "is:pr is:open", "Label 00").await;
    assert_eq!(
        count(&cell, pull_choice.items[0].choice.query.clone()).await,
        1
    );
    let pulls = cell
        .client
        .query::<ListPulls>(
            cell.target,
            None,
            index::SearchInput {
                query: pull_choice.items[0].choice.query.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(pulls.matching, 1);
    fixtures::close(&server).await;
}
