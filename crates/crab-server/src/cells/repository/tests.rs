use super::*;

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
    assert!(serde_json::to_vec(&pull).unwrap().len() <= 1024 * 1024);
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
    assert!(serde_json::to_vec(&comment).unwrap().len() <= 512 * 1024);
}

#[test]
fn maximum_release_fits_the_registered_output_bound() {
    let author = RepositoryAuthor {
        issuer: "🦀".repeat(512),
        subject: "🦀".repeat(512),
        name: "🦀".repeat(160),
    };
    let release = ReleaseRecord {
        number: 1,
        create_submission_id: [9; 16],
        author: author.clone(),
        tag_name: "v1.0.0".into(),
        tag_oid: Some("0123456789abcdef0123456789abcdef01234567".into()),
        target_oid: "0123456789abcdef0123456789abcdef01234567".into(),
        title: "🦀".repeat(256),
        body: "\u{1}".repeat(64 * 1024),
        prerelease: false,
        draft: false,
        publication_pending: None,
        version: 97,
        created_at_ms: 1,
        published_at_ms: Some(1),
        updated_at_ms: 97,
        deleted: false,
        assets: (0..96)
            .map(|index| ReleaseAssetRecord {
                request_id: [index; 16],
                name: format!("asset-{index:03}"),
                content_type: "application/octet-stream".into(),
                size: 512 * 1024 * 1024,
                digest: "a".repeat(64),
                uploader: author.clone(),
                created_at_ms: 1,
            })
            .collect(),
    };
    assert!(serde_json::to_vec(&release).unwrap().len() <= 1024 * 1024);
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
    assert!(serde_json::to_vec(&catalog).unwrap().len() <= 384 * 1024);
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
    assert!(serde_json::to_vec(&catalog).unwrap().len() <= 1024 * 1024);
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
    assert!(serde_json::to_vec(&page).unwrap().len() <= 1024 * 1024);

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
    assert!(serde_json::to_vec(&detail).unwrap().len() <= 256 * 1024);
}

#[test]
fn label_ids_retain_the_persisted_big_endian_format() {
    assert_eq!(
        encode_label_ids(&[1, 20]).unwrap(),
        vec![0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 20]
    );
}

#[test]
fn assignees_retain_the_persisted_length_prefixed_format() {
    let stored = vec![0, 0, 0, 1, 0, 0, 0, 5, b'a', b'l', b'i', b'c', b'e'];
    assert_eq!(decode_assignees(&stored).unwrap(), vec!["alice".to_owned()]);
    assert_eq!(encode_assignees(&["alice".to_owned()]).unwrap(), stored);
}

#[test]
fn check_output_reopens_the_existing_record_format() {
    let fixture = "0000000174000000016d000000000000000000";
    let bytes = (0..fixture.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&fixture[index..index + 2], 16).unwrap())
        .collect::<Vec<_>>();
    let mut decoder = RecordDecoder::new(&bytes, 256 * 1024).unwrap();
    let output = CheckOutputRecord::decode(&mut decoder).unwrap();
    decoder.finish().unwrap();
    let expected = CheckOutputRecord {
        title: "t".into(),
        summary: "m".into(),
        text: None,
        steps: vec![],
        annotations: vec![],
    };
    assert_eq!(output, expected);
    let mut encoder = RecordEncoder::new(256 * 1024).unwrap();
    output.encode(&mut encoder).unwrap();
    assert_eq!(encoder.finish(), bytes);
}

#[test]
fn branch_protections_reopen_the_existing_record_format() {
    let fixture = "00000001000000046d61696e0200000001000000026369";
    let bytes = (0..fixture.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&fixture[index..index + 2], 16).unwrap())
        .collect::<Vec<_>>();
    let mut decoder = RecordDecoder::new(&bytes, 64 * 1024).unwrap();
    assert_eq!(decoder.read_count().unwrap(), 1);
    let rule = BranchProtectionRecord::decode(&mut decoder).unwrap();
    decoder.finish().unwrap();
    assert_eq!(
        rule,
        BranchProtectionRecord {
            branch: "main".into(),
            required_approvals: 2,
            required_checks: vec!["ci".into()]
        }
    );
}
