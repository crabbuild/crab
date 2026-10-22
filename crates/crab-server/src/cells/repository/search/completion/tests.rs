use super::*;

fn input(query: &str, cursor: usize) -> CompletionInput {
    CompletionInput {
        query: query.into(),
        cursor: cursor.try_into().unwrap(),
    }
}

fn complete(input: &CompletionInput) -> Suggestions {
    let label = LabelRecord {
        number: 1,
        name: "Help wanted".into(),
        color: "abcdef".into(),
        description: Some("Good first contribution".into()),
        version: 1,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    let (start, end, cursor) = replacement(input).unwrap();
    suggestions(input, start, end, cursor, &[label], &[]).unwrap()
}

#[test]
fn completion_replaces_the_whole_current_token_and_preserves_surrounding_query() {
    let source = "🦀 AND label:he OR keyboard";
    let cursor = "🦀 AND label:he".encode_utf16().count();
    let result = complete(&input(source, cursor));
    assert_eq!(
        (result.start, result.end, result.items[0].value.as_str()),
        (7, 15, "label:\"Help wanted\"")
    );
    assert!(Query::parse(Some("🦀 AND label:\"Help wanted\" OR keyboard")).is_ok());
}

#[test]
fn partial_quoted_labels_comma_lists_and_exclusions_are_completed() {
    for (query, expected) in [
        ("label:\"Help w", "label:\"Help wanted\""),
        ("-label:wan", "-label:\"Help wanted\""),
        ("label:bug,he", "label:bug,\"Help wanted\""),
        ("label:\"bug,fix\",he", "label:\"bug,fix\",\"Help wanted\""),
    ] {
        let result = complete(&input(query, query.encode_utf16().count()));
        assert_eq!(result.items[0].value, expected);
    }
}

#[test]
fn completion_rejects_invalid_utf16_cursors_and_control_characters() {
    for value in [input("🦀", 1), input("a", 2), input("a\n", 2)] {
        assert!(replacement(&value).is_err());
    }
}

#[test]
fn suggestions_discover_both_discussion_kinds_and_report_syntax_errors() {
    let result = complete(&input("", 0));
    for expected in ["is:issue", "is:pr", "draft:true", "is:merged"] {
        assert!(
            result.items.iter().any(|item| item.value == expected),
            "{expected}"
        );
    }
    let result = complete(&input("label:\"Help", 11));
    assert!(result.error.is_some());
    assert_eq!(result.items[0].value, "label:\"Help wanted\"");
}

#[test]
fn label_prefix_decodes_escaped_quotes_without_losing_literal_characters() {
    for (source, expected) in [
        (r#""say \""#, "say \""),
        (r#""path\\to"#, "path\\to"),
        (r#"bug,"say \"he"#, "say \"he"),
    ] {
        assert_eq!(label_prefix(source).1, expected);
    }
}

#[test]
fn count_suggestions_preserve_exclusions_and_produce_valid_numeric_queries() {
    for qualifier in ["comments", "reactions", "interactions"] {
        let query = format!("-{qualifier}:");
        let result = complete(&input(&query, query.len()));
        assert!(result.error.is_some());
        assert_eq!(result.items.len(), 3);
        for item in result.items {
            assert!(item.value.starts_with(&query));
            assert!(Query::parse(Some(&item.value)).is_ok());
        }
        let query = format!("{qualifier}:>0");
        assert!(complete(&input(&query, query.len())).error.is_none());
    }
}

#[test]
fn commenter_completion_uses_the_shared_people_path_and_keeps_utf16_ranges() {
    let query = "🦀 -commenter:@";
    let input = input(query, query.encode_utf16().count());
    let mut result = complete(&input);
    assert_eq!(result.items[0].value, "-commenter:@me");
    assert_eq!(
        person_prefix(&input).unwrap(),
        Some(("commenter".into(), "@".into()))
    );
    append_people(
        &input,
        &mut result,
        "commenter",
        [("public-alice".into(), "Alice".into())].into_iter(),
    )
    .unwrap();
    assert_eq!((result.start, result.end), (3, 15));
    assert_eq!(result.items[1].value, "-commenter:public-alice");
}
