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
            author: author.clone(),
            title: "t".into(),
            body: "b".into(),
        },
        "00000001690000000173000000016e00000001740000000162",
    );
    assert_fixture(
        &issue,
        "000000000000000900000001690000000173000000016e0000000174000000016200000000000000000200000000000000030000000000000004",
    );
    assert_fixture(
        &CreateCommentInput {
            issue: 7,
            author,
            body: "b".into(),
        },
        "000000000000000700000001690000000173000000016e0000000162",
    );
    assert_fixture(
        &CreateCommentOutcome::Created(comment.clone()),
        "010000000000000007000000000000000900000001690000000173000000016e0000000162000000000000000200000000000000030000000000000004",
    );
    assert_fixture(&7_u64, "0000000000000007");
    assert_fixture(
        &Some(issue),
        "01000000000000000900000001690000000173000000016e0000000174000000016200000000000000000200000000000000030000000000000004",
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

fn assert_fixture<T: WireValue + PartialEq + std::fmt::Debug>(value: &T, fixture: &str) {
    let bytes = decode_hex(fixture);
    let mut encoder = BoundedEncoder::new(80 * 1024).unwrap();
    value.encode(&mut encoder).unwrap();
    assert_eq!(encoder.finish(), bytes);

    let mut decoder = BoundedDecoder::new(&bytes, 80 * 1024).unwrap();
    assert_eq!(&T::decode(&mut decoder).unwrap(), value);
    decoder.finish().unwrap();
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
