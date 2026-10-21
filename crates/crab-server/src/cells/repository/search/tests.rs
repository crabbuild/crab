use super::*;

fn document() -> Document<'static> {
    Document {
        kind: SubjectKind::Issue,
        state: SubjectState::Open,
        draft: None,
        title: "Improve accessibility",
        body: "Fix the keyboard focus in the search menu",
        author_name: "Alice Smith",
        label_ids: &[1, 2],
        assignees: &[],
    }
}

#[test]
fn boolean_search_has_and_precedence_and_explicit_grouping() {
    for (query, expected) in [
        ("accessibility keyboard", true),
        ("accessibility missing", false),
        ("accessibility OR missing AND unrelated", true),
        ("(accessibility OR missing) AND unrelated", false),
        ("\"keyboard focus\"", true),
        ("\"focus keyboard\"", false),
        ("Alice AND Smith", true),
        ("-missing AND accessibility", true),
        ("-keyboard", false),
        ("\"AND\"", false),
        ("(((((accessibility)))))", true),
    ] {
        assert_eq!(
            Query::parse(Some(query)).unwrap().matches(&document(), &[]),
            expected,
            "{query}"
        );
    }
}

#[test]
fn labels_are_exact_case_insensitive_sets_with_comma_or_and_space_and() {
    let labels = vec![
        (1, "help wanted".into()),
        (2, "a,b".into()),
        (3, "bug".into()),
    ];
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
        assert_eq!(
            Query::parse(Some(query))
                .unwrap()
                .matches(&document(), &labels),
            expected,
            "{query}"
        );
    }
}

#[test]
fn pull_state_and_readiness_are_distinct_predicates() {
    let mut item = document();
    item.kind = SubjectKind::Pull;
    item.state = SubjectState::Merged;
    item.draft = Some(false);
    for (query, expected) in [
        ("is:closed", true),
        ("is:merged", true),
        ("is:unmerged", false),
        ("is:open", false),
        ("is:pr", true),
        ("is:issue", false),
        ("draft:true", false),
        ("draft:false", true),
        ("is:closed -is:merged", false),
    ] {
        assert_eq!(
            Query::parse(Some(query)).unwrap().matches(&item, &[]),
            expected,
            "{query}"
        );
    }
    item.state = SubjectState::Open;
    item.draft = Some(true);
    assert!(
        Query::parse(Some("is:open draft:true"))
            .unwrap()
            .matches(&item, &[])
    );
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
        "author:alice",
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

#[test]
fn quoted_operators_colons_and_escaped_characters_are_literal() {
    let mut item = document();
    item.body = "AND https://example.test \"quoted\" C:\\code";
    for query in [
        r#""AND""#,
        r#""https://example.test""#,
        r#""\"quoted\"""#,
        r#""C:\\code""#,
    ] {
        assert!(
            Query::parse(Some(query)).unwrap().matches(&item, &[]),
            "{query}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn native_issue_and_pull_search_share_labels_boolean_logic_and_readiness() {
    use crate::cells::repository::*;
    use crate::test_support::server as fixtures;
    use uuid::Uuid;
    let server = fixtures::fixture().await;
    let cell = server
        .repository_cells()
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
                        number: issue.number,
                        actor: author.clone(),
                        can_manage_metadata: true,
                        version: issue.version,
                        title: None,
                        body: None,
                        state: None,
                        label_ids: Some(vec![label.number]),
                        assignee_subjects: None,
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
        .query::<ListIssues>(
            &cell.target,
            None,
            ListIssuesInput {
                before: None,
                limit: 1,
                state: 2,
                query: Some("(label:\"help wanted\" OR no:label) keyboard -missing".into()),
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(
        issues
            .items
            .iter()
            .map(|item| item.number)
            .collect::<Vec<_>>(),
        vec![2]
    );
    let next = cell
        .client
        .query::<ListIssues>(
            &cell.target,
            None,
            ListIssuesInput {
                before: issues.next,
                limit: 1,
                state: 2,
                query: Some("label:\"help wanted\" keyboard".into()),
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(
        next.items
            .iter()
            .map(|item| item.number)
            .collect::<Vec<_>>(),
        vec![1]
    );
    let pulls = cell
        .client
        .query::<ListPulls>(
            &cell.target,
            None,
            PullListInput {
                before: None,
                limit: 30,
                state: 2,
                query: Some("is:pr draft:true AND keyboard no:label".into()),
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
        .query::<ListIssues>(
            &cell.target,
            None,
            ListIssuesInput {
                before: None,
                limit: 30,
                state: 2,
                query: Some("label:\"help wanted\"".into()),
            },
        )
        .await
        .unwrap()
        .output;
    assert!(deleted.items.is_empty());
    let unlabelled = cell
        .client
        .query::<ListIssues>(
            &cell.target,
            None,
            ListIssuesInput {
                before: None,
                limit: 30,
                state: 2,
                query: Some("no:label".into()),
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(unlabelled.items.len(), 3);

    fixtures::close(&server).await;
}
