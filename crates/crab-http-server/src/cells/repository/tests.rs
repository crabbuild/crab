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

#[test]
fn repository_codec_v1_pins_label_fixtures() {
    let author = RepositoryAuthor {
        issuer: "i".into(),
        subject: "s".into(),
        name: "n".into(),
    };
    let label = LabelRecord {
        number: 9,
        name: "bug".into(),
        color: "123abc".into(),
        description: Some("x".into()),
        version: 2,
        created_at_ms: 3,
        updated_at_ms: 4,
    };
    assert_fixture(
        &CreateLabelInput {
            submission_id: [3; 16],
            author,
            name: "bug".into(),
            color: "123abc".into(),
            description: Some("x".into()),
        },
        "000000100303030303030303030303030303030300000001690000000173000000016e0000000362756700000006313233616263010000000178",
    );
    assert_fixture(
        &CreateLabelOutcome::Created(label.clone()),
        "0100000000000000090000000362756700000006313233616263010000000178000000000000000200000000000000030000000000000004",
    );
    assert_fixture(
        &UpdateLabelInput {
            number: 9,
            version: 2,
            name: "bug".into(),
            color: "123abc".into(),
            description: Some("x".into()),
        },
        "000000000000000900000000000000020000000362756700000006313233616263010000000178",
    );
    assert_fixture(
        &LabelCatalog {
            labels: vec![label],
        },
        "0000000100000000000000090000000362756700000006313233616263010000000178000000000000000200000000000000030000000000000004",
    );
    assert_fixture(
        &DeleteLabelInput {
            number: 9,
            version: 2,
        },
        "00000000000000090000000000000002",
    );
    assert_fixture(&DeleteLabelOutcome::Deleted, "01");
}

#[test]
fn repository_codec_v1_pins_commit_status_fixtures() {
    let record = CommitStatusRecord {
        number: 9,
        submission_id: [4; 16],
        author: RepositoryAuthor {
            issuer: "i".into(),
            subject: "s".into(),
            name: "n".into(),
        },
        oid: "0123456789abcdef0123456789abcdef01234567".into(),
        context: "ci/test".into(),
        state: 3,
        description: Some("passed".into()),
        target_url: Some("https://ci.example.test/4".into()),
        created_at_ms: 5,
    };
    let input = CreateCommitStatusInput {
        submission_id: record.submission_id,
        author: record.author.clone(),
        oid: record.oid.clone(),
        context: record.context.clone(),
        state: record.state,
        description: record.description.clone(),
        target_url: record.target_url.clone(),
    };
    assert_fixture(
        &input,
        "000000100404040404040404040404040404040400000001690000000173000000016e00000028303132333435363738396162636465663031323334353637383961626364656630313233343536370000000763692f74657374030100000006706173736564010000001968747470733a2f2f63692e6578616d706c652e746573742f34",
    );
    assert_fixture(
        &CreateCommitStatusOutcome::Created(Box::new(record.clone())),
        "010000000000000009000000100404040404040404040404040404040400000001690000000173000000016e00000028303132333435363738396162636465663031323334353637383961626364656630313233343536370000000763692f74657374030100000006706173736564010000001968747470733a2f2f63692e6578616d706c652e746573742f340000000000000005",
    );
    assert_fixture(
        &CommitStatusSubmissionKey {
            oid: record.oid.clone(),
            submission_id: record.submission_id,
        },
        "00000028303132333435363738396162636465663031323334353637383961626364656630313233343536370000001004040404040404040404040404040404",
    );
    assert_fixture(
        &CommitStatusCatalog {
            statuses: vec![record],
        },
        "000000010000000000000009000000100404040404040404040404040404040400000001690000000173000000016e00000028303132333435363738396162636465663031323334353637383961626364656630313233343536370000000763692f74657374030100000006706173736564010000001968747470733a2f2f63692e6578616d706c652e746573742f340000000000000005",
    );
    assert_fixture(&CreateCommitStatusOutcome::RequestConflict, "02");
    assert_fixture(&CreateCommitStatusOutcome::ContextLimit, "03");
    assert_fixture(&CreateCommitStatusOutcome::SubmissionLimit, "04");
}

#[test]
fn repository_codec_v1_pins_check_fixtures() {
    let author = RepositoryAuthor {
        issuer: "i".into(),
        subject: "s".into(),
        name: "n".into(),
    };
    let output = CheckOutputRecord {
        title: "t".into(),
        summary: "m".into(),
        text: None,
        steps: vec![],
        annotations: vec![],
    };
    let report = CheckReportInput {
        status: 0,
        conclusion: None,
        details_url: Some("https://ci.test/5".into()),
        output: output.clone(),
    };
    let create = CreateCheckRunInput {
        submission_id: [5; 16],
        author: author.clone(),
        oid: "0123456789abcdef0123456789abcdef01234567".into(),
        name: "ci/test".into(),
        report: report.clone(),
    };
    let detail = CheckRunDetail {
        run: CheckRunRecord {
            number: 9,
            create_submission_id: [5; 16],
            author: author.clone(),
            oid: create.oid.clone(),
            name: create.name.clone(),
            status: 0,
            conclusion: None,
            details_url: report.details_url.clone(),
            output_title: output.title.clone(),
            version: 1,
            started_at_ms: None,
            completed_at_ms: None,
            created_at_ms: 3,
            updated_at_ms: 3,
        },
        output,
    };
    let update = UpdateCheckRunInput {
        submission_id: [6; 16],
        actor: author,
        oid: create.oid.clone(),
        number: 9,
        version: 1,
        report,
    };
    assert_fixture(
        &create,
        "000000100505050505050505050505050505050500000001690000000173000000016e00000028303132333435363738396162636465663031323334353637383961626364656630313233343536370000000763692f746573740000010000001168747470733a2f2f63692e746573742f350000000174000000016d000000000000000000",
    );
    assert_fixture(
        &CreateCheckRunOutcome::Created(Box::new(detail.clone())),
        "010000000000000009000000100505050505050505050505050505050500000001690000000173000000016e00000028303132333435363738396162636465663031323334353637383961626364656630313233343536370000000763692f746573740000010000001168747470733a2f2f63692e746573742f35000000017400000000000000010000000000000000000300000000000000030000000174000000016d000000000000000000",
    );
    assert_fixture(
        &update,
        "000000100606060606060606060606060606060600000001690000000173000000016e0000002830313233343536373839616263646566303132333435363738396162636465663031323334353637000000000000000900000000000000010000010000001168747470733a2f2f63692e746573742f350000000174000000016d000000000000000000",
    );
    assert_fixture(
        &UpdateCheckRunOutcome::Updated(Box::new(detail.clone())),
        "010000000000000009000000100505050505050505050505050505050500000001690000000173000000016e00000028303132333435363738396162636465663031323334353637383961626364656630313233343536370000000763692f746573740000010000001168747470733a2f2f63692e746573742f35000000017400000000000000010000000000000000000300000000000000030000000174000000016d000000000000000000",
    );
    assert_fixture(
        &CheckRunPage {
            runs: vec![detail.run],
            next: Some(8),
        },
        "000000010000000000000009000000100505050505050505050505050505050500000001690000000173000000016e00000028303132333435363738396162636465663031323334353637383961626364656630313233343536370000000763692f746573740000010000001168747470733a2f2f63692e746573742f3500000001740000000000000001000000000000000000030000000000000003010000000000000008",
    );
}

#[test]
fn repository_codec_v1_pins_settings_fixtures() {
    let settings = BranchProtectionSettings {
        version: 1,
        rules: vec![BranchProtectionRecord {
            branch: "main".into(),
            required_approvals: 2,
            required_checks: vec!["ci".into()],
        }],
    };
    assert_fixture(
        &settings,
        "000000000000000100000001000000046d61696e0200000001000000026369",
    );
    assert_fixture(
        &ReplaceBranchProtectionsInput {
            expected_version: 0,
            rules: settings.rules.clone(),
        },
        "000000000000000000000001000000046d61696e0200000001000000026369",
    );
    assert_fixture(
        &ReplaceBranchProtectionsOutcome::Updated(settings),
        "01000000000000000100000001000000046d61696e0200000001000000026369",
    );
    assert_fixture(&ReplaceBranchProtectionsOutcome::Conflict, "02");
    let lifecycle = RepositoryLifecycleRecord {
        version: 1,
        archived: true,
    };
    assert_fixture(&lifecycle, "000000000000000101");
    assert_fixture(
        &ReplaceRepositoryLifecycleInput {
            expected_version: 0,
            archived: true,
        },
        "000000000000000001",
    );
    assert_fixture(
        &ReplaceRepositoryLifecycleOutcome::Updated(lifecycle),
        "01000000000000000101",
    );
    assert_fixture(&ReplaceRepositoryLifecycleOutcome::Conflict, "02");
    assert_fixture(&ReplaceRepositoryLifecycleOutcome::Unchanged, "03");
}

#[test]
fn repository_codec_v1_pins_pull_fixtures() {
    assert_fixture(
        &CreatePullInput {
            submission_id: [7; 16],
            author: RepositoryAuthor {
                issuer: "i".into(),
                subject: "s".into(),
                name: "n".into(),
            },
            title: "t".into(),
            body: "b".into(),
            base_ref: "refs/heads/main".into(),
            base_oid: "0123456789abcdef0123456789abcdef01234567".into(),
            head_ref: "refs/heads/feature".into(),
            head_oid: "89abcdef0123456789abcdef0123456789abcdef".into(),
        },
        "000001247b227375626d697373696f6e5f6964223a5b372c372c372c372c372c372c372c372c372c372c372c372c372c372c372c375d2c22617574686f72223a7b22697373756572223a2269222c227375626a656374223a2273222c226e616d65223a226e227d2c227469746c65223a2274222c22626f6479223a2262222c22626173655f726566223a22726566732f68656164732f6d61696e222c22626173655f6f6964223a2230313233343536373839616263646566303132333435363738396162636465663031323334353637222c22686561645f726566223a22726566732f68656164732f66656174757265222c22686561645f6f6964223a2238396162636465663031323334353637383961626364656630313233343536373839616263646566227d",
    );
    assert_fixture(
        &CreatePullOutcome::RequestConflict,
        "000000112252657175657374436f6e666c69637422",
    );
}

#[test]
fn maximum_pull_decisions_fit_the_registered_output_bound() {
    let author = RepositoryAuthor {
        issuer: "🦀".repeat(512),
        subject: "🦀".repeat(512),
        name: "🦀".repeat(160),
    };
    let pull = PullRecord {
        number: 1,
        create_submission_id: [8; 16],
        author: author.clone(),
        title: "t".repeat(256),
        body: "\u{1}".repeat(64 * 1024),
        state: PullState::Open,
        base_ref: "refs/heads/main".into(),
        base_oid: "0123456789abcdef0123456789abcdef01234567".into(),
        head_ref: "refs/heads/feature".into(),
        head_oid: "89abcdef0123456789abcdef0123456789abcdef".into(),
        label_ids: (1..=20).collect(),
        assignee_subjects: (0..10).map(|index| format!("s{index}")).collect(),
        merge_pending: None,
        merge: None,
        review_decisions: (1..=96)
            .map(|review| pulls::PullReviewDecision {
                review,
                author: author.clone(),
                state: ReviewState::Approved,
                commit_oid: "89abcdef0123456789abcdef0123456789abcdef".into(),
            })
            .collect(),
        version: 1,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    let mut encoder = BoundedEncoder::new(1024 * 1024).unwrap();
    pull.encode(&mut encoder).unwrap();
    assert!(encoder.finish().len() <= 1024 * 1024);
}

#[test]
fn maximum_pull_child_fits_the_registered_record_bound() {
    let comment = PullCommentRecord {
        pull: 1,
        number: 1,
        author: RepositoryAuthor {
            issuer: "🦀".repeat(512),
            subject: "🦀".repeat(512),
            name: "🦀".repeat(160),
        },
        body: "\u{1}".repeat(64 * 1024),
        version: 1,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    let mut encoder = BoundedEncoder::new(512 * 1024).unwrap();
    comment.encode(&mut encoder).unwrap();
    assert!(encoder.finish().len() <= 512 * 1024);
}

#[test]
fn maximum_utf8_label_catalog_fits_its_registered_output_bound() {
    let label = LabelRecord {
        number: 1,
        name: "🦀".repeat(50),
        color: "123abc".into(),
        description: Some("🦀".repeat(100)),
        version: 1,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    let catalog = LabelCatalog {
        labels: vec![label; 500],
    };
    let mut encoder = BoundedEncoder::new(384 * 1024).unwrap();
    catalog.encode(&mut encoder).unwrap();
    assert!(encoder.finish().len() <= 384 * 1024);
}

#[test]
fn maximum_utf8_status_catalog_fits_its_registered_output_bound() {
    let target_prefix = "https://example.test/";
    let status = CommitStatusRecord {
        number: 1,
        submission_id: [1; 16],
        author: RepositoryAuthor {
            issuer: "🦀".repeat(512),
            subject: "🦀".repeat(512),
            name: "🦀".repeat(160),
        },
        oid: "0123456789abcdef0123456789abcdef01234567".into(),
        context: "🦀".repeat(100),
        state: 3,
        description: Some("🦀".repeat(140)),
        target_url: Some(format!(
            "{target_prefix}{}",
            "a".repeat(2_048 - target_prefix.len())
        )),
        created_at_ms: 1,
    };
    let catalog = CommitStatusCatalog {
        statuses: vec![status; 128],
    };
    let mut encoder = BoundedEncoder::new(1024 * 1024).unwrap();
    catalog.encode(&mut encoder).unwrap();
    assert!(encoder.finish().len() <= 1024 * 1024);
}

#[test]
fn maximum_check_results_fit_their_registered_output_bounds() {
    let target_prefix = "https://example.test/";
    let run = CheckRunRecord {
        number: 1,
        create_submission_id: [1; 16],
        author: RepositoryAuthor {
            issuer: "🦀".repeat(512),
            subject: "🦀".repeat(512),
            name: "🦀".repeat(160),
        },
        oid: "0123456789abcdef0123456789abcdef01234567".into(),
        name: "🦀".repeat(100),
        status: 1,
        conclusion: None,
        details_url: Some(format!(
            "{target_prefix}{}",
            "a".repeat(2_048 - target_prefix.len())
        )),
        output_title: "🦀".repeat(200),
        version: 1,
        started_at_ms: Some(1),
        completed_at_ms: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    let page = CheckRunPage {
        runs: vec![run.clone(); 100],
        next: None,
    };
    let mut page_encoder = BoundedEncoder::new(1024 * 1024).unwrap();
    page.encode(&mut page_encoder).unwrap();
    assert!(page_encoder.finish().len() <= 1024 * 1024);

    let detail = CheckRunDetail {
        run,
        output: CheckOutputRecord {
            title: "🦀".repeat(200),
            summary: "s".repeat(32 * 1024),
            text: Some("t".repeat(64 * 1024)),
            steps: (0..10)
                .map(|index| CheckStepRecord {
                    name: format!("step-{index}"),
                    status: 1,
                    conclusion: None,
                    log: Some("l".repeat(8 * 1024)),
                })
                .collect(),
            annotations: vec![],
        },
    };
    let mut detail_encoder = BoundedEncoder::new(256 * 1024).unwrap();
    detail.encode(&mut detail_encoder).unwrap();
    assert!(detail_encoder.finish().len() <= 256 * 1024);
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
