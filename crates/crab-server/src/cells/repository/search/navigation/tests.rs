use super::*;

#[test]
fn state_scope_accounts_for_negation_boolean_groups_and_literal_qualifiers() {
    for (query, expected) in [
        ("is:open -label:bug", Some(StateSelection::Open)),
        ("is:open state:open", Some(StateSelection::Open)),
        (
            "(is:closed label:bug) keyword",
            Some(StateSelection::Closed),
        ),
        ("-is:closed", None),
        ("is:merged", None),
        ("-is:open", None),
        ("is:open OR is:closed", None),
        ("is:open OR label:bug", None),
        ("is:closed (is:open OR label:bug)", None),
        ("is:open is:closed", None),
        ("label:bug", None),
        ("\"is:open\"", None),
        ("(is:open label:bug) OR (is:closed label:feature)", None),
    ] {
        let (_, navigation) = Query::parse(Some(query)).unwrap().state_navigation();
        let selected = match navigation {
            StateNavigation::State { selected, .. } => Some(selected),
            StateNavigation::Results => None,
        };
        assert_eq!(selected, expected, "{query}");
    }
}

#[test]
fn maximum_queries_disable_unrepresentable_navigation_without_truncating_filters() {
    let value = "x".repeat(248);
    let query = format!("is:open {value}");
    let (base, navigation) = Query::parse(Some(&query)).unwrap().state_navigation();
    assert_eq!(source(&base, 0), value);
    let StateNavigation::State { open, closed, .. } = navigation else {
        panic!("missing state facet")
    };
    assert_eq!(open, Some(format!("{value} is:open")));
    assert!(closed.is_none());
}

#[test]
fn state_changes_preserve_non_state_atom_spelling_and_quoted_literals() {
    let query = Query::parse(Some(
        r#"is:closed (label:"Help Wanted" OR "AND") -"is:open""#,
    ))
    .unwrap();
    let (_, navigation) = query.state_navigation();
    let StateNavigation::State { open, .. } = navigation else {
        panic!("missing state facet")
    };
    assert_eq!(
        open.as_deref(),
        Some(r#"(label:"Help Wanted" OR "AND") -"is:open" is:open"#)
    );
}
