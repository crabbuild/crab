use super::*;

#[test]
fn local_calendar_days_and_offset_timestamps_preserve_search_boundaries() {
    let (first, last) = period("2024-02-29").unwrap();
    assert_eq!(last - first, 86_399_999);
    assert_eq!(
        period("2024-02-28T19:00:00-05:00").unwrap(),
        (first, first + 999)
    );
    assert_eq!(
        period("2024-02-29T00:00:00.123Z").unwrap(),
        (first + 123, first + 123)
    );
    assert!(
        matches!(Filter::parse("created", ">2024-02-29").unwrap().bounds, Bounds::AtLeast(value) if value == last + 1)
    );
    assert!(
        matches!(Filter::parse("updated", "<2024-02-29").unwrap().bounds, Bounds::AtMost(value) if value == first - 1)
    );
    assert!(
        matches!(Filter::parse("closed", "2024-02-29..*").unwrap().bounds, Bounds::AtLeast(value) if value == first)
    );
    assert!(
        matches!(Filter::parse("merged", "*..2024-02-29").unwrap().bounds, Bounds::AtMost(value) if value == last)
    );
}

#[test]
fn date_grammar_rejects_unqualified_times_and_invalid_ranges() {
    for value in [
        "2023-02-29",
        "2024-13-01",
        "2024-02",
        "20240229",
        "2024-02-29T10:00:00",
        "2024-02-29X10:00:00Z",
        "2024-02-29T10:00:00.0001Z",
        "2024-02-29T10:00:00.0000000001Z",
        "*..*",
        "2024-03-01..2024-02-29",
        "2024-02-29..",
        "..2024-02-29",
        ">2024-02-29..2024-03-01",
    ] {
        let query = format!("created:{value}");
        assert!(super::super::Query::parse(Some(&query)).is_err(), "{query}");
    }
}
