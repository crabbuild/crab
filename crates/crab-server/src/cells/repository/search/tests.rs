use super::*;

use crate::cells::repository::*;
use crate::test_support::server as fixtures;

const BODY: &str = "Fix the keyboard focus in the search menu";
async fn fixture(
    body: &str,
) -> (
    std::sync::Arc<crate::server::Server>,
    crate::cells::RepositoryCell,
) {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let author = RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: "alice".into(),
        name: "Alice Smith".into(),
    };
    for name in ["help wanted", "a,b", "bug"] {
        let result = cell
            .client
            .command::<CreateLabel>(
                &cell.target,
                crate::app::mutation_identity().unwrap(),
                CreateLabelInput {
                    submission_id: *uuid::Uuid::now_v7().as_bytes(),
                    author: author.clone(),
                    name: name.into(),
                    color: "abcdef".into(),
                    description: None,
                },
            )
            .await
            .unwrap()
            .output;
        assert!(matches!(result, CreateLabelOutcome::Created(_)));
    }
    let created = cell
        .client
        .command::<CreateIssue>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            CreateIssueInput {
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                author: author.clone(),
                title: "Improve accessibility".into(),
                body: body.into(),
            },
        )
        .await
        .unwrap()
        .output;
    assert!(matches!(created, CreateIssueOutcome::Created(_)));
    let assigned = cell
        .client
        .command::<UpdateIssue>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            UpdateIssueInput {
                state_reason: None,
                number: 1,
                actor: author,
                can_triage: true,
                version: 1,
                title: None,
                body: None,
                state: None,
                label_ids: Some(vec![1, 2]),
                assignees: None,
            },
        )
        .await
        .unwrap()
        .output;
    assert!(matches!(assigned, UpdateIssueOutcome::Updated(_)));
    (server, cell)
}
async fn matches(cell: &crate::cells::RepositoryCell, query: &str) -> bool {
    !cell
        .client
        .query::<ListDiscussions>(
            &cell.target,
            None,
            SearchInput {
                query: Some(query.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output
        .items
        .is_empty()
}

#[tokio::test(flavor = "multi_thread")]
async fn boolean_search_has_and_precedence_and_explicit_grouping() {
    let (server, cell) = fixture(BODY).await;
    for (query, expected) in [
        ("accessibility keyboard", true),
        ("accessibility missing", false),
        ("accessibility OR missing AND unrelated", true),
        ("(accessibility OR missing) AND unrelated", false),
        ("\"keyboard focus\"", true),
        ("\"focus keyboard\"", false),
        ("-missing AND accessibility", true),
        ("-keyboard", false),
        ("\"AND\"", false),
        ("(((((accessibility)))))", true),
    ] {
        assert_eq!(matches(&cell, query).await, expected, "{query}");
    }
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn labels_are_exact_case_insensitive_sets_with_comma_or_and_space_and() {
    let (server, cell) = fixture(BODY).await;
    for (query, expected) in [
        ("label:\"HELP WANTED\"", true),
        ("label:help", false),
        ("label:bug,\"help wanted\"", true),
        ("label:bug label:\"help wanted\"", false),
        ("label:\"a,b\"", true),
        ("-label:bug", true),
        ("-label:\"a,b\"", false),
        ("label:unknown", false),
        ("no:label", false),
        ("no:assignee", true),
    ] {
        assert_eq!(matches(&cell, query).await, expected, "{query}");
    }
    fixtures::close(&server).await;
}

#[test]
fn malformed_or_unsupported_queries_never_become_free_text() {
    for query in [
        "",
        " ",
        "trailing ",
        "\n",
        "\"unfinished",
        "label:",
        "label:\"\"",
        "label:\"bad\"quote",
        "label:,bug",
        "label:bug,",
        "is:unknown",
        "state:merged",
        "draft:yes",
        "no:project",
        "-no:label",
        "author:app/bot",
        "sort:updated",
        "-",
        "()",
        "(a",
        "a)",
        "OR a",
        "a OR",
        "a AND",
        "a AND OR b",
        "((((((a))))))",
        "\"bad\\escape\"",
    ] {
        assert!(Query::parse(Some(query)).is_err(), "{query:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn quoted_operators_colons_and_escaped_characters_are_literal() {
    let (server, cell) = fixture("AND https://example.test \"quoted\" C:\\code ÉCOLE 🦀船舶").await;
    for query in [
        r#""AND""#,
        r#""https://example.test""#,
        r#""\"quoted\"""#,
        r#""C:\\code""#,
        "école",
        "🦀船舶",
        "船",
    ] {
        assert!(matches(&cell, query).await, "{query}");
    }
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn native_issue_and_pull_search_share_labels_boolean_logic_and_readiness() {
    use crate::cells::repository::*;
    use crate::test_support::server as fixtures;
    use uuid::Uuid;
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let author = RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: "alice".into(),
        name: "Alice Smith".into(),
    };
    let label = cell
        .client
        .command::<CreateLabel>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            CreateLabelInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: author.clone(),
                name: "Help wanted".into(),
                color: "abcdef".into(),
                description: None,
            },
        )
        .await
        .unwrap()
        .output;
    let CreateLabelOutcome::Created(label) = label else {
        panic!("label not created")
    };
    for index in 0..3 {
        let title = if index == 2 {
            "Other work"
        } else {
            "Keyboard focus"
        };
        let issue = cell
            .client
            .command::<CreateIssue>(
                &cell.target,
                crate::app::mutation_identity().unwrap(),
                CreateIssueInput {
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: author.clone(),
                    title: title.into(),
                    body: "Accessibility".into(),
                },
            )
            .await
            .unwrap()
            .output;
        let CreateIssueOutcome::Created(issue) = issue else {
            panic!("issue not created")
        };
        if index == 0 {
            cell.client
                .command::<UpdateIssue>(
                    &cell.target,
                    crate::app::mutation_identity().unwrap(),
                    UpdateIssueInput {
                        state_reason: None,
                        number: issue.number,
                        actor: author.clone(),
                        can_triage: true,
                        version: issue.version,
                        title: None,
                        body: None,
                        state: None,
                        label_ids: Some(vec![label.number]),
                        assignees: None,
                    },
                )
                .await
                .unwrap();
        }
        let pull = cell
            .client
            .command::<CreatePull>(
                &cell.target,
                crate::app::mutation_identity().unwrap(),
                CreatePullInput {
                    draft: index == 1,
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: author.clone(),
                    title: title.into(),
                    body: "Accessibility".into(),
                    base_ref: "refs/heads/main".into(),
                    head_ref: format!("refs/heads/topic-{index}"),
                    base_oid: "a".repeat(40),
                    head_oid: format!("{:040x}", index + 1),
                },
            )
            .await
            .unwrap()
            .output;
        assert!(matches!(pull, CreatePullOutcome::Created(_)));
    }
    let issues = cell
        .client
        .query::<ListDiscussions>(
            &cell.target,
            None,
            SearchInput {
                before: None,
                limit: 1,
                query: Some(
                    "is:issue (label:\"help wanted\" OR no:label) keyboard -missing".into(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(
        issues
            .items
            .iter()
            .map(|item| match item {
                DiscussionSummary::Issue(issue) => issue.number,
                DiscussionSummary::Pull(pull) => pull.number,
            })
            .collect::<Vec<_>>(),
        vec![2]
    );
    let next = cell
        .client
        .query::<ListDiscussions>(
            &cell.target,
            None,
            SearchInput {
                before: issues.next.clone(),
                limit: 1,
                query: Some(
                    "is:issue (label:\"help wanted\" OR no:label) keyboard -missing".into(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(
        next.items
            .iter()
            .map(|item| match item {
                DiscussionSummary::Issue(issue) => issue.number,
                DiscussionSummary::Pull(pull) => pull.number,
            })
            .collect::<Vec<_>>(),
        vec![1]
    );
    let pulls = cell
        .client
        .query::<ListPulls>(
            &cell.target,
            None,
            SearchInput {
                before: None,
                limit: 30,
                query: Some("is:pr draft:true AND keyboard no:label".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(
        pulls
            .items
            .iter()
            .map(|item| item.number)
            .collect::<Vec<_>>(),
        vec![2]
    );
    let renamed = cell
        .client
        .command::<UpdateLabel>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            UpdateLabelInput {
                number: label.number,
                version: label.version,
                name: "Keyboard work".into(),
                color: "123456".into(),
                description: None,
            },
        )
        .await
        .unwrap()
        .output;
    let UpdateLabelOutcome::Updated(label) = renamed else {
        panic!("label not renamed")
    };
    let labelled = cell
        .client
        .query::<ListDiscussions>(
            &cell.target,
            None,
            SearchInput {
                query: Some("label:\"keyboard work\"".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(labelled.matching, 1);
    assert_eq!(labelled.labels[0].name, "Keyboard work");
    assert_eq!(labelled.labels[0].color, "123456");
    cell.client
        .command::<DeleteLabel>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            DeleteLabelInput {
                number: label.number,
                version: label.version,
            },
        )
        .await
        .unwrap();
    let deleted = cell
        .client
        .query::<ListDiscussions>(
            &cell.target,
            None,
            SearchInput {
                before: None,
                limit: 30,
                query: Some("label:\"keyboard work\"".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output;
    assert!(deleted.items.is_empty());
    let unlabelled = cell
        .client
        .query::<ListDiscussions>(
            &cell.target,
            None,
            SearchInput {
                before: None,
                limit: 30,
                query: Some("is:issue no:label".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(unlabelled.items.len(), 3);

    fixtures::close(&server).await;
}
