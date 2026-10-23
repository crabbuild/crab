use super::*;

#[test]
fn assignment_grammar_distinguishes_public_handles_private_actors_and_wildcards() {
    let id = Uuid::from_u128(42);
    for (source, names, mine) in [
        ("assignee:ALICE".into(), vec!["alice"], false),
        ("assignee:@me label:bug".into(), vec!["@me"], true),
        ("-assignee:@me".into(), vec!["@me"], false),
        (
            "assignee:@me OR assignee:alice".into(),
            vec!["@me", "alice"],
            false,
        ),
        (format!("assignee:actor/{id}"), vec![], false),
        ("assignee:*".into(), vec![], false),
        ("\"assignee:alice\"".into(), vec![], false),
    ] {
        let query = Query::parse(Some(&source)).unwrap();
        assert_eq!(
            (
                query.assignees().into_iter().collect::<Vec<_>>(),
                query.assigned_to_me()
            ),
            (
                names.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                mine
            ),
            "{source}"
        );
    }
    for source in [
        "assignee:",
        "assignee:a,b",
        "assignee:actor/bad",
        "assignee:actor/00000000-0000-0000-0000-000000000000",
        "assignee:@unknown",
    ] {
        assert!(Query::parse(Some(source)).is_err(), "{source}");
    }
    let query = Query::parse(Some("assignee:alice")).unwrap();
    assert!(query.validate_assignees(&[]).is_err());
    assert!(
        query
            .validate_assignees(&[Binding {
                name: "alice".into(),
                id: Some(Uuid::nil())
            }])
            .is_err()
    );
    assert!(
        query
            .validate_assignees(&[Binding {
                name: "alice".into(),
                id: None
            }])
            .is_ok()
    );
}

#[test]
fn assignment_choices_replace_global_predicates_without_rewriting_or_groups() {
    for (source, value, expected, selected) in [
        (
            "is:open assignee:ALICE",
            Filter::Person("alice"),
            "is:open assignee:alice",
            true,
        ),
        (
            "is:open assignee:alice -assignee:bob",
            Filter::Person("@me"),
            "is:open assignee:@me",
            false,
        ),
        (
            "(assignee:alice OR assignee:bob) label:bug",
            Filter::Missing,
            "(assignee:alice OR assignee:bob) label:bug no:assignee",
            false,
        ),
        ("no:assignee", Filter::Missing, "no:assignee", true),
        (
            "is:open",
            Filter::Person("none"),
            "is:open assignee:none",
            false,
        ),
        ("no:assignee", Filter::Person("*"), "assignee:*", false),
        ("assignee:@me", Filter::Clear, "is:issue OR is:pr", false),
        ("label:bug", Filter::Clear, "label:bug", true),
    ] {
        let choice = Query::parse(Some(source))
            .unwrap()
            .assignee_choice(value)
            .unwrap();
        assert_eq!(
            (choice.query.as_deref(), choice.selected),
            (Some(expected), selected),
            "{source}"
        );
    }
    assert!(
        Query::parse(Some(&"🦀".repeat(256)))
            .unwrap()
            .assignee_choice(Filter::Person("@me"))
            .unwrap()
            .query
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn historical_choice_catalog_is_bounded_deduplicated_and_tracks_last_assignment_removal() {
    use crate::{cells::repository::*, test_support::server as fixtures};
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let actor = RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: "author".into(),
        name: "Author".into(),
    };
    for number in 1..=22 {
        cell.client
            .command::<CreateIssue>(
                &cell.target,
                crate::app::mutation_identity().unwrap(),
                CreateIssueInput {
                    metadata: Default::default(),
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: actor.clone(),
                    title: format!("Work {number}"),
                    body: String::new(),
                },
            )
            .await
            .unwrap();
        let assignee_number = number.min(21);
        cell.client
            .command::<UpdateIssue>(
                &cell.target,
                crate::app::mutation_identity().unwrap(),
                UpdateIssueInput {
                    state_reason: None,
                    number,
                    actor: actor.clone(),
                    can_triage: true,
                    version: 1,
                    title: None,
                    body: None,
                    state: None,
                    label_ids: None,
                    assignees: Some(vec![crate::cells::repository::assignments::Assignee {
                        id: Uuid::from_u128(assignee_number.into()),
                        name: format!("Former person {assignee_number:02}"),
                    }]),
                },
            )
            .await
            .unwrap();
    }
    let choices = cell
        .client
        .query::<AssigneeOptions>(
            &cell.target,
            None,
            OptionInput {
                query: None,
                search: "FORMER".into(),
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!((choices.items.len(), choices.truncated), (20, true));
    let choices = cell
        .client
        .query::<AssigneeOptions>(
            &cell.target,
            None,
            OptionInput {
                query: None,
                search: "person 21".into(),
            },
        )
        .await
        .unwrap()
        .output;
    assert_eq!((choices.items.len(), choices.truncated), (1, false));
    for number in [21, 22] {
        cell.client
            .command::<UpdateIssue>(
                &cell.target,
                crate::app::mutation_identity().unwrap(),
                UpdateIssueInput {
                    state_reason: None,
                    number,
                    actor: actor.clone(),
                    can_triage: true,
                    version: 2,
                    title: None,
                    body: None,
                    state: None,
                    label_ids: None,
                    assignees: Some(vec![]),
                },
            )
            .await
            .unwrap();
    }
    let choices = cell
        .client
        .query::<AssigneeOptions>(
            &cell.target,
            None,
            OptionInput {
                query: None,
                search: "person 21".into(),
            },
        )
        .await
        .unwrap()
        .output;
    assert!(choices.items.is_empty());
    fixtures::close(&server).await;
}
