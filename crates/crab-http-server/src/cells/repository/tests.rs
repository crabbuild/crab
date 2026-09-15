use super::*;

#[test]
fn repository_codec_v1_has_stable_command_and_query_fixtures() {
    let author = RepositoryAuthor {
        issuer: "i".into(),
        subject: "s".into(),
        name: "n".into(),
    };
    let issue = IssueRecord {
        number: 9,
        author: author.clone(),
        title: "t".into(),
        body: "b".into(),
        state: 0,
        label_ids: vec![],
        assignee_subjects: vec![],
        version: 2,
        created_at_ms: 3,
        updated_at_ms: 4,
    };
    let comment = CommentRecord {
        issue: 7,
        number: 9,
        author: author.clone(),
        body: "b".into(),
        version: 2,
        created_at_ms: 3,
        updated_at_ms: 4,
    };
    assert_fixture(
        &CreateIssueInput {
            submission_id: [1; 16],
            author: author.clone(),
            title: "t".into(),
            body: "b".into(),
        },
        "000000100101010101010101010101010101010100000001690000000173000000016e00000001740000000162",
    );
    assert_fixture(
        &CreateIssueOutcome::Created(Box::new(issue.clone())),
        "01000000000000000900000001690000000173000000016e00000001740000000162000000000000000000000000000000000200000000000000030000000000000004",
    );
    assert_fixture(
        &CreateCommentInput {
            submission_id: [2; 16],
            issue: 7,
            author,
            body: "b".into(),
        },
        "0000001002020202020202020202020202020202000000000000000700000001690000000173000000016e0000000162",
    );
    assert_fixture(
        &CreateCommentOutcome::Created(comment.clone()),
        "010000000000000007000000000000000900000001690000000173000000016e0000000162000000000000000200000000000000030000000000000004",
    );
    assert_fixture(&CreateIssueOutcome::RequestConflict, "02");
    assert_fixture(&CreateCommentOutcome::RequestConflict, "03");
    assert_fixture(&7_u64, "0000000000000007");
    assert_fixture(
        &Some(issue),
        "01000000000000000900000001690000000173000000016e00000001740000000162000000000000000000000000000000000200000000000000030000000000000004",
    );
    assert_fixture(
        &CommentKey {
            issue: 7,
            number: 9,
        },
        "00000000000000070000000000000009",
    );
    assert_fixture(
        &Some(comment),
        "010000000000000007000000000000000900000001690000000173000000016e0000000162000000000000000200000000000000030000000000000004",
    );
}

#[test]
fn repository_codec_v1_pins_list_and_update_fixtures() {
    let author = RepositoryAuthor {
        issuer: "i".into(),
        subject: "s".into(),
        name: "n".into(),
    };
    let issue = IssueRecord {
        number: 9,
        author: author.clone(),
        title: "t".into(),
        body: "b".into(),
        state: 1,
        label_ids: vec![5],
        assignee_subjects: vec!["s".into()],
        version: 2,
        created_at_ms: 3,
        updated_at_ms: 4,
    };
    let comment = CommentRecord {
        issue: 7,
        number: 9,
        author: author.clone(),
        body: "b".into(),
        version: 2,
        created_at_ms: 3,
        updated_at_ms: 4,
    };
    assert_fixture(
        &UpdateIssueInput {
            number: 9,
            actor: author.clone(),
            can_manage_metadata: true,
            version: 2,
            title: Some("u".into()),
            body: None,
            state: Some(1),
            label_ids: Some(vec![5]),
            assignee_subjects: Some(vec!["s".into()]),
        },
        "000000000000000900000001690000000173000000016e0100000000000000020100000001750001010100000001000000000000000501000000010000000173",
    );
    assert_fixture(
        &UpdateIssueOutcome::Updated(Box::new(issue.clone())),
        "01000000000000000900000001690000000173000000016e0000000174000000016201000000010000000000000005000000010000000173000000000000000200000000000000030000000000000004",
    );
    assert_fixture(
        &UpdateCommentInput {
            key: CommentKey {
                issue: 7,
                number: 9,
            },
            actor: author,
            version: 2,
            body: "u".into(),
        },
        "0000000000000007000000000000000900000001690000000173000000016e00000000000000020000000175",
    );
    assert_fixture(
        &UpdateCommentOutcome::Updated(comment.clone()),
        "010000000000000007000000000000000900000001690000000173000000016e0000000162000000000000000200000000000000030000000000000004",
    );
    assert_fixture(
        &ListIssuesInput {
            before: Some(9),
            limit: 30,
            state: 2,
            query: Some("q".into()),
        },
        "0100000000000000091e02010000000171",
    );
    assert_fixture(
        &IssuePage {
            items: vec![issue.into()],
            next: Some(8),
        },
        "00000001000000000000000900000001690000000173000000016e000000017401000000010000000000000005000000010000000173000000000000000200000000000000030000000000000004010000000000000008",
    );
    assert_fixture(
        &ListCommentsInput {
            issue: 7,
            before: Some(9),
            limit: 30,
        },
        "00000000000000070100000000000000091e",
    );
    assert_fixture(
        &CommentPage::Found {
            items: vec![comment],
            next: Some(8),
        },
        "01000000010000000000000007000000000000000900000001690000000173000000016e0000000162000000000000000200000000000000030000000000000004010000000000000008",
    );
}

fn assert_fixture<T: WireValue + PartialEq + std::fmt::Debug>(value: &T, fixture: &str) {
    let bytes = decode_hex(fixture);
    let mut encoder = BoundedEncoder::new(80 * 1024).unwrap();
    value.encode(&mut encoder).unwrap();
    let encoded = encoder.finish();
    assert_eq!(encoded_hex(&encoded), fixture);
    assert_eq!(encoded, bytes);

    let mut decoder = BoundedDecoder::new(&bytes, 80 * 1024).unwrap();
    assert_eq!(&T::decode(&mut decoder).unwrap(), value);
    decoder.finish().unwrap();
}

fn encoded_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = nibble(pair[0]);
            let low = nibble(pair[1]);
            (high << 4) | low
        })
        .collect()
}

fn nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("invalid test fixture"),
    }
}
