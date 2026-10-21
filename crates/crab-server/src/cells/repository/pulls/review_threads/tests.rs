use super::*;

fn author() -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://issuer.example".into(),
        subject: "reviewer".into(),
        name: "Reviewer".into(),
    }
}

fn oid(byte: char) -> String {
    std::iter::repeat_n(byte, 40).collect()
}

fn input() -> CreatePullReviewThreadInput {
    CreatePullReviewThreadInput {
        pull: 1,
        submission_id: [1; 16],
        author: author(),
        body: "Please clarify this branch".into(),
        suggested_text: Some("clarified branch".into()),
        base_oid: oid('1'),
        head_oid: oid('2'),
        path: vec![0xff, b'.', b't', b'x', b't'],
        old_blob_oid: Some(oid('3')),
        new_blob_oid: Some(oid('4')),
        side: PullReviewThreadSide::New,
        start_line: 3,
        end_line: 5,
    }
}

#[test]
fn validation_preserves_raw_paths_and_rejects_invalid_ranges_and_suggestions() {
    let valid = input();
    assert!(validate_create_thread(&valid).is_ok());

    let mut invalid = valid.clone();
    invalid.path = vec![0];
    assert!(validate_create_thread(&invalid).is_err());

    let mut invalid = valid.clone();
    invalid.start_line = 0;
    assert!(validate_create_thread(&invalid).is_err());

    let mut invalid = valid.clone();
    invalid.end_line = 203;
    assert!(validate_create_thread(&invalid).is_err());

    let mut invalid = valid.clone();
    invalid.suggested_text = Some("x".repeat(MAX_SUGGESTED_TEXT_BYTES + 1));
    assert!(validate_create_thread(&invalid).is_err());

    let mut invalid = valid;
    invalid.suggested_text = Some("bad\0suggestion".into());
    assert!(validate_create_thread(&invalid).is_err());

    let mut old_side = input();
    old_side.side = PullReviewThreadSide::Old;
    assert!(validate_create_thread(&old_side).is_err());
}

#[test]
fn list_validation_requires_bounded_pages_and_complete_comparisons() {
    let valid = PullReviewThreadListInput {
        pull: 1,
        before: Some(2),
        limit: 30,
        path: Some(vec![0xff, b'.', b't', b'x', b't']),
        resolved: Some(false),
        comparison_base_oid: Some(oid('1')),
        comparison_head_oid: Some(oid('2')),
        outdated: Some(true),
    };
    assert!(validate_thread_list(&valid).is_ok());

    let mut invalid = valid.clone();
    invalid.limit = 0;
    assert!(validate_thread_list(&invalid).is_err());
    invalid.limit = 51;
    assert!(validate_thread_list(&invalid).is_err());
    invalid.limit = 30;
    invalid.comparison_head_oid = None;
    assert!(validate_thread_list(&invalid).is_err());
    invalid.comparison_head_oid = Some(oid('2'));
    invalid.before = Some(0);
    assert!(validate_thread_list(&invalid).is_err());
}

#[test]
fn record_codec_round_trips_raw_path_and_resolved_metadata() {
    let input = input();
    let record = PullReviewThreadRecord {
        pull: input.pull,
        number: 7,
        author: input.author,
        body: input.body,
        suggested_text: input.suggested_text,
        base_oid: input.base_oid,
        head_oid: input.head_oid,
        path: input.path,
        old_blob_oid: input.old_blob_oid,
        new_blob_oid: input.new_blob_oid,
        side: input.side,
        start_line: input.start_line,
        end_line: input.end_line,
        resolved: true,
        resolved_by: Some(author()),
        resolved_at_ms: Some(42),
        version: 2,
        created_at_ms: 41,
        updated_at_ms: 42,
    };
    let bytes = serde_json::to_vec(&record).unwrap();
    let decoded: PullReviewThreadRecord = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded, record);
    assert_eq!(decoded.path, vec![0xff, b'.', b't', b'x', b't']);
}
