use super::super::people::Binding;
use super::*;
use crate::{
    cells::{RepositoryCell, repository::*},
    test_support::server as fixtures,
};

#[test]
fn author_grammar_and_global_self_selection_keep_boolean_meaning() {
    for (source, names, mine) in [
        ("author:ALICE", vec!["alice"], false),
        ("author:@me label:bug", vec!["@me"], true),
        ("-author:@me", vec!["@me"], false),
        ("author:@me OR author:alice", vec!["@me", "alice"], false),
        (
            "(author:alice OR author:bob) author:@me",
            vec!["@me", "alice", "bob"],
            true,
        ),
        ("\"author:alice\"", vec![], false),
    ] {
        let query = Query::parse(Some(source)).unwrap();
        assert_eq!(
            query.people().into_iter().collect::<Vec<_>>(),
            names,
            "{source}"
        );
        assert_eq!(query.authored_by_me(), mine, "{source}");
    }
    for source in [
        "author:",
        "author:a,b",
        "author:\"Display Name\"",
        "author:app/bot",
        "author:@unknown",
    ] {
        assert!(Query::parse(Some(source)).is_err(), "{source}");
    }
    assert_ne!(
        super::super::people::identity("ab", "c").unwrap(),
        super::super::people::identity("a", "bc").unwrap()
    );
}

async fn page(
    cell: &RepositoryCell,
    query: &str,
    bindings: Vec<Binding>,
    before: Option<String>,
    limit: u8,
) -> crate::cells::Result<crate::cells::repository::search::index::Page<DiscussionSummary>> {
    cell.client
        .query::<ListDiscussions>(
            cell.target,
            None,
            SearchInput {
                query: Some(query.into()),
                people: bindings,
                before,
                limit,
                ..Default::default()
            },
        )
        .await
        .map(|observed| observed.output)
}

#[tokio::test(flavor = "multi_thread")]
async fn qualified_author_projection_filters_both_kinds_and_binds_cursors_to_resolved_identity() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    let mut actors = Vec::new();
    for (issuer, subject) in [
        ("https://one.test", "a"),
        ("https://one.test", "b"),
        ("https://two.test", "a"),
    ] {
        let author = RepositoryAuthor {
            issuer: issuer.into(),
            subject: subject.into(),
            name: "Same display name".into(),
        };
        cell.client
            .command::<CreateIssue>(
                cell.target,
                crate::app::mutation_identity().unwrap(),
                CreateIssueInput {
                    metadata: Default::default(),
                    submission_id: *uuid::Uuid::now_v7().as_bytes(),
                    author: author.clone(),
                    title: "Authorship".into(),
                    body: String::new(),
                },
            )
            .await
            .unwrap();
        actors.push(author);
    }
    cell.client
        .command::<CreatePull>(
            cell.target,
            crate::app::mutation_identity().unwrap(),
            CreatePullInput {
                metadata: Default::default(),
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                author: actors[0].clone(),
                draft: false,
                title: "Authored pull".into(),
                body: String::new(),
                base_ref: "refs/heads/main".into(),
                head_ref: "refs/heads/feature".into(),
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();
    let key = |index: usize| {
        Some(super::super::people::identity(&actors[index].issuer, &actors[index].subject).unwrap())
    };
    let binding = |name: &str, index| Binding {
        name: name.into(),
        identity: key(index),
    };
    let first = page(
        &cell,
        "is:open author:alice",
        vec![binding("alice", 0)],
        None,
        1,
    )
    .await
    .unwrap();
    assert_eq!(first.matching, 2);
    assert_eq!(first.counts.open, 2);
    let next = first.next.unwrap();
    assert_eq!(
        page(
            &cell,
            "is:open author:alice",
            vec![binding("alice", 0)],
            Some(next.clone()),
            1
        )
        .await
        .unwrap()
        .items
        .len(),
        1
    );
    assert!(matches!(
        page(
            &cell,
            "is:open author:alice",
            vec![binding("alice", 1)],
            Some(next),
            1
        )
        .await,
        Err(crate::cells::Error::StaleSearch)
    ));
    assert_eq!(
        page(
            &cell,
            "author:alice OR author:other",
            vec![binding("alice", 0), binding("other", 2)],
            None,
            50
        )
        .await
        .unwrap()
        .matching,
        3
    );
    assert_eq!(
        page(&cell, "-author:alice", vec![binding("alice", 0)], None, 50)
            .await
            .unwrap()
            .matching,
        2
    );
    assert_eq!(
        page(
            &cell,
            "author:unknown",
            vec![Binding {
                name: "unknown".into(),
                identity: None
            }],
            None,
            50
        )
        .await
        .unwrap()
        .matching,
        0
    );
    assert!(
        page(&cell, "author:@me", vec![binding("@me", 0)], None, 50)
            .await
            .unwrap()
            .authored_by_me
    );
    assert!(page(&cell, "author:alice", vec![], None, 50).await.is_err());
    assert!(
        page(
            &cell,
            "author:@me",
            vec![Binding {
                name: "@me".into(),
                identity: None
            }],
            None,
            50
        )
        .await
        .is_err()
    );
    fixtures::close(&server).await;
}

#[test]
fn author_choices_replace_global_terms_and_preserve_conditional_authors() {
    for (source, handle, expected, selected) in [
        (
            "is:open author:ALICE",
            Some("alice"),
            "is:open author:alice",
            true,
        ),
        (
            "is:open author:alice -author:bob",
            Some("@me"),
            "is:open author:@me",
            false,
        ),
        (
            "(author:alice OR author:bob) label:bug",
            Some("carol"),
            "(author:alice OR author:bob) label:bug author:carol",
            false,
        ),
        ("author:@me", None, "is:issue OR is:pr", false),
        ("label:bug", None, "label:bug", true),
        ("-author:alice", Some("alice"), "author:alice", false),
    ] {
        let choice = Query::parse(Some(source))
            .unwrap()
            .author_choice(handle)
            .unwrap();
        assert_eq!(
            (choice.query.as_deref(), choice.selected),
            (Some(expected), selected),
            "{source}"
        );
    }
    let maximum = Query::parse(Some(&"🦀".repeat(256))).unwrap();
    assert!(
        maximum
            .author_choice(Some("alice"))
            .unwrap()
            .query
            .is_none()
    );
    assert!(maximum.author_choice(Some("not a handle")).is_err());
}
